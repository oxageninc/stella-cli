//! The goal loop (`/goal`, `stella goal`): keep running working turns
//! until an independent judge model assesses the goal as met.
//!
//! Structure per round: one full [`Engine::run_turn`] (the worker doing
//! real work), then one judge call assessing the transcript against the
//! goal. `met == true` ends the loop; `met == false` feeds the judge's
//! feedback back to the worker as the next user message, so every round
//! starts with concrete course-correction rather than a bare "try again".
//!
//! # Bounded by construction
//!
//! "Don't stop until the judge says the goal is met" still terminates on
//! three backstops, each with a named reason in [`GoalOutcome::Unmet`]:
//! the round cap ([`GoalConfig::max_rounds`]), the budget (judge spend is
//! recorded against the same [`BudgetGuard`] as worker spend — a goal loop
//! cannot out-spend its budget by hiding cost in assessments), and a turn
//! abort (loop detection, retry exhaustion, step cap — anything that ends
//! a turn uncleanly ends the goal loop too, never silently retried).
//!
//! # Graceful degradation
//!
//! A judge that fails its call after retries ends the loop as `Unmet`
//! with the provider error in the reason — the loop never spins unjudged
//! work while the judge is down, and never fabricates a verdict. A judge
//! that answers but with unparseable output is treated as "not met" with
//! generic feedback (one bad judge response wastes a round, not the run;
//! the round cap bounds the damage).
//!
//! # Telemetry
//!
//! Every judge call emits `Stage(Judge)` → `GoalVerdict` (with the judge
//! call's own `cost_usd`) → `BudgetTick`, so a metering consumer sees
//! judge spend with the same fidelity as worker spend, and the verdict
//! stream reconstructs the goal loop's arc without any out-of-band state.

use serde::Deserialize;
use stella_protocol::{AgentEvent, CompletionMessage, MessageRole, Provider, StageKind};
use tokio::sync::mpsc::UnboundedSender;

use crate::budget::BudgetGuard;
use crate::driver::{Engine, EngineConfig, TurnOutcome};
use crate::ports::ReadOnlyTools;

/// Tuning for [`Engine::run_goal`]. `Default` is sized for interactive
/// use: enough rounds to converge on a real goal, small enough that a
/// judge stuck on "not met" can't run away with the session.
#[derive(Debug, Clone)]
pub struct GoalConfig {
    /// Hard cap on working rounds (worker turn + judge assessment pairs).
    pub max_rounds: usize,
    /// Output-token cap for judge calls — a verdict is small by design.
    pub judge_max_output_tokens: Option<u32>,
    /// Cap on transcript characters shown to the judge, tail-biased (the
    /// most recent work is what decides "met"), and always on a char
    /// boundary.
    pub judge_transcript_chars: usize,
}

impl Default for GoalConfig {
    fn default() -> Self {
        Self {
            max_rounds: 8,
            judge_max_output_tokens: Some(1024),
            judge_transcript_chars: 24_000,
        }
    }
}

/// How a goal loop ended.
#[derive(Debug, Clone, PartialEq)]
pub enum GoalOutcome {
    /// The judge assessed the goal as met.
    Met {
        rounds: usize,
        /// The judge's reasoning for the final, passing verdict.
        verdict: String,
        /// Total spend across all rounds — worker turns and judge calls.
        cost_usd: f64,
    },
    /// The loop ended without a passing verdict: round cap, budget, a turn
    /// abort, or an unreachable judge. Never silent — `reason` names which.
    Unmet {
        rounds: usize,
        reason: String,
        cost_usd: f64,
    },
}

/// What the judge model must return, as strict JSON. `reasoning` explains
/// the verdict; `feedback` (only meaningful when `met == false`) is
/// actionable course-correction handed to the worker verbatim.
#[derive(Debug, Deserialize)]
struct JudgeVerdict {
    met: bool,
    #[serde(default)]
    reasoning: String,
    #[serde(default)]
    feedback: String,
}

const JUDGE_SYSTEM_PROMPT: &str = "You are an impartial judge assessing whether a coding agent \
     has fully met a stated goal. Judge from EVIDENCE, never from claims: use your read-only \
     tools (read_file, grep, glob, explorations, ci_status, search_issues) to verify the work \
     directly whenever the transcript alone is not conclusive — read the changed files, check \
     the tests exist, inspect CI. Claimed success without supporting evidence is NOT met. The \
     strongest completion evidence is a `verify_done` tool result reading WITNESS CONFIRMED \
     (the change's test fails on the previous code and passes on the new code); a merely \
     green test suite is weak evidence, since it cannot distinguish real work from vacuous \
     tests or unwired code. If you need something only the worker can provide (a trace, a \
     screenshot, a system log, an explanation), set met:false and put the request in \
     feedback — the worker acts on it next round. When decided, end your reply with ONLY a \
     JSON object, no prose after it:\n\
     {\"met\": true|false, \"reasoning\": \"why, in one or two sentences\", \
     \"feedback\": \"if not met: the single most useful next action or evidence request\"}";

impl Engine<'_> {
    /// Drive working turns until `judge` assesses `goal` as met, or a
    /// backstop ends the loop (see module docs). Appends to `messages`
    /// like [`Engine::run_turn`] does, so the caller's conversation
    /// history contains the full goal arc — including judge feedback
    /// messages — afterward.
    pub async fn run_goal(
        &self,
        judge: &dyn Provider,
        goal: &str,
        messages: &mut Vec<CompletionMessage>,
        budget: &mut BudgetGuard,
        events: &UnboundedSender<AgentEvent>,
        goal_config: &GoalConfig,
    ) -> GoalOutcome {
        let mut total_cost_usd = 0.0f64;

        messages.push(CompletionMessage::user(format!(
            "GOAL: {goal}\n\nWork toward this goal. An independent judge will assess the \
             result after each working round from the transcript evidence; keep your work \
             verifiable (run tests, show outputs)."
        )));

        for round in 1..=goal_config.max_rounds {
            budget.begin_turn();
            match self.run_turn(messages, budget, events).await {
                TurnOutcome::Completed { cost_usd, .. } => total_cost_usd += cost_usd,
                TurnOutcome::Aborted { reason } => {
                    return GoalOutcome::Unmet {
                        rounds: round,
                        reason: format!("working turn aborted: {reason}"),
                        cost_usd: total_cost_usd,
                    };
                }
            }

            let _ = events.send(AgentEvent::Stage {
                name: StageKind::Judge,
            });
            let (verdict, judge_cost) = match self
                .assess(judge, goal, messages, budget, events, goal_config)
                .await
            {
                Ok(pair) => pair,
                Err(reason) => {
                    // Judge unreachable (or its turn hit the budget) after
                    // retries: stop rather than loop unjudged (module docs,
                    // graceful degradation). Spend was already recorded by
                    // the judge turn itself.
                    return GoalOutcome::Unmet {
                        rounds: round,
                        reason: format!("judge unavailable: {reason}"),
                        cost_usd: total_cost_usd,
                    };
                }
            };

            // Judge spend was metered inside its own engine turn
            // (StepUsage + BudgetTick already emitted); the verdict event
            // carries the assessment's total for the goal-loop arc.
            total_cost_usd += judge_cost;
            let _ = events.send(AgentEvent::GoalVerdict {
                round,
                met: verdict.met,
                reasoning: verdict.reasoning.clone(),
                cost_usd: judge_cost,
            });

            if verdict.met {
                return GoalOutcome::Met {
                    rounds: round,
                    verdict: verdict.reasoning,
                    cost_usd: total_cost_usd,
                };
            }

            let feedback = if verdict.feedback.trim().is_empty() {
                verdict.reasoning.clone()
            } else {
                verdict.feedback.clone()
            };
            messages.push(CompletionMessage::user(format!(
                "The judge assessed the goal as NOT yet met.\nJudge feedback: {feedback}\n\n\
                 Continue working toward the goal: {goal}"
            )));
        }

        GoalOutcome::Unmet {
            rounds: goal_config.max_rounds,
            reason: format!(
                "round cap ({}) reached without a passing verdict",
                goal_config.max_rounds
            ),
            cost_usd: total_cost_usd,
        }
    }

    /// One judge assessment, run as a bounded tool-using engine turn: the
    /// judge sees the goal + transcript tail and may gather its own
    /// evidence through a [`ReadOnlyTools`] view of the SAME tool registry
    /// the worker used (read files, grep, check CI) — structurally unable
    /// to mutate the workspace it is judging. Spend flows through the same
    /// `budget` as worker turns. `Err` carries the abort reason (provider
    /// failure or budget) after retries were exhausted.
    async fn assess(
        &self,
        judge: &dyn Provider,
        goal: &str,
        messages: &[CompletionMessage],
        budget: &mut BudgetGuard,
        events: &UnboundedSender<AgentEvent>,
        goal_config: &GoalConfig,
    ) -> Result<(JudgeVerdict, f64), String> {
        let transcript = render_transcript_tail(messages, goal_config.judge_transcript_chars);
        let mut judge_messages = vec![
            CompletionMessage::system(JUDGE_SYSTEM_PROMPT),
            CompletionMessage::user(format!(
                "GOAL:\n{goal}\n\nAGENT TRANSCRIPT (most recent last):\n{transcript}\n\n\
                 Has the goal been fully met? Verify with your tools where the transcript \
                 is not conclusive."
            )),
        ];
        let read_only = ReadOnlyTools::new(self.tools);
        let mut judge_engine = Engine::with_sleeper(
            judge,
            &read_only,
            EngineConfig {
                max_output_tokens: goal_config.judge_max_output_tokens,
                temperature: Some(0.0),
                // A verdict needs a handful of evidence lookups, not a
                // work session.
                max_steps: 8,
                ..self.config.clone()
            },
            self.sleeper,
        );
        // Share the session's drift calibration: the map is keyed per model
        // (`crate::estimator::CalibrationMap`), so a cross-family judge
        // learns its own model's drift without ever blending into the
        // worker's.
        judge_engine.calibration = self.calibration;

        match judge_engine
            .run_turn(&mut judge_messages, budget, events)
            .await
        {
            TurnOutcome::Completed { text, cost_usd } => {
                let verdict = parse_verdict(&text).unwrap_or_else(|| JudgeVerdict {
                    met: false,
                    reasoning: "judge response was not parseable JSON — treated as not met".into(),
                    feedback: format!(
                        "Continue working toward the goal; the previous assessment was \
                         unreadable (judge said: {})",
                        truncate_chars(&text, 500)
                    ),
                });
                Ok((verdict, cost_usd))
            }
            TurnOutcome::Aborted { reason } => Err(reason),
        }
    }
}

/// Extract the verdict from judge output that may wrap its JSON in prose or
/// a code fence: parse the outermost `{ … }` span.
fn parse_verdict(text: &str) -> Option<JudgeVerdict> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}

/// Render the conversation for the judge: role-labeled, tool activity
/// summarized, tail-biased to `max_chars` on message boundaries (the judge
/// needs the most recent evidence, and a truncated JSON blob mid-message
/// would read as agent malfunction).
fn render_transcript_tail(messages: &[CompletionMessage], max_chars: usize) -> String {
    let mut rendered: Vec<String> = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == MessageRole::System {
            continue;
        }
        let mut block = String::new();
        let label = match message.role {
            MessageRole::User => "USER",
            MessageRole::Assistant => "ASSISTANT",
            MessageRole::Tool => "TOOL RESULTS",
            MessageRole::System => unreachable!("filtered above"),
        };
        block.push_str(label);
        block.push_str(": ");
        if !message.content.is_empty() {
            block.push_str(&message.content);
        }
        for call in &message.tool_calls {
            block.push_str(&format!(
                "\n  [called {}({})]",
                call.name,
                truncate_chars(&call.input.to_string(), 200)
            ));
        }
        for result in &message.tool_results {
            let (status, body) = match &result.output {
                stella_protocol::ToolOutput::Ok { content } => ("ok", content),
                stella_protocol::ToolOutput::Error { message } => ("error", message),
            };
            block.push_str(&format!(
                "\n  [{} {}: {}]",
                result.call_id,
                status,
                truncate_chars(body, 400)
            ));
        }
        rendered.push(block);
    }

    // Take whole blocks from the end until the budget is spent.
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for block in rendered.iter().rev() {
        let cost = block.chars().count() + 2;
        if used + cost > max_chars && !kept.is_empty() {
            break;
        }
        used += cost;
        kept.push(block);
        if used > max_chars {
            break;
        }
    }
    kept.reverse();
    kept.join("\n\n")
}

/// Truncate to `max` characters on a char boundary, appending `…` when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use async_trait::async_trait;
    use serde_json::Value;
    use stella_protocol::event::BudgetMode;
    use stella_protocol::{
        CompletionRequest, CompletionResult, CompletionUsage, ProviderError, ToolOutput, ToolSchema,
    };
    use tokio::sync::mpsc;

    use super::*;
    use crate::driver::EngineConfig;
    use crate::ports::ToolExecutor;
    use crate::retry::Sleeper;

    /// Sleeper that never really sleeps — goal tests run instantly.
    struct NoSleep;
    #[async_trait]
    impl Sleeper for NoSleep {
        async fn sleep(&self, _duration_ms: u64) {}
    }

    /// A provider that returns a fixed sequence of results, then errors.
    struct ScriptedProvider {
        script: Mutex<Vec<Result<CompletionResult, ProviderError>>>,
        calls: AtomicU32,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Result<CompletionResult, ProviderError>>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResult, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                return Err(ProviderError::Transport("script exhausted".into()));
            }
            script.remove(0)
        }
    }

    struct NoTools;
    #[async_trait]
    impl ToolExecutor for NoTools {
        fn schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        async fn execute(&self, _name: &str, _input: &Value) -> ToolOutput {
            ToolOutput::Ok {
                content: String::new(),
            }
        }
    }

    fn text_result(text: &str, cost: f64) -> CompletionResult {
        CompletionResult {
            text: text.into(),
            tool_calls: vec![],
            usage: CompletionUsage::default(),
            model: "scripted".into(),
            cost_usd: cost,
        }
    }

    fn collect_events(
        mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    ) -> impl FnOnce() -> Vec<AgentEvent> {
        move || {
            let mut events = Vec::new();
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
            events
        }
    }

    #[tokio::test]
    async fn goal_loop_iterates_until_the_judge_passes_it() {
        // Worker completes a turn each round; judge fails round 1 with
        // feedback, passes round 2.
        let worker = ScriptedProvider::new(vec![
            Ok(text_result("attempt one", 0.01)),
            Ok(text_result("attempt two", 0.01)),
        ]);
        let judge = ScriptedProvider::new(vec![
            Ok(text_result(
                r#"{"met": false, "reasoning": "tests not run", "feedback": "run the test suite"}"#,
                0.001,
            )),
            Ok(text_result(
                r#"{"met": true, "reasoning": "all evidence present", "feedback": ""}"#,
                0.001,
            )),
        ]);
        let tools = NoTools;
        let engine = Engine::with_sleeper(&worker, &tools, EngineConfig::default(), &NoSleep);
        let mut messages = vec![CompletionMessage::system("sys")];
        let mut budget = BudgetGuard::new(BudgetMode::Observed, None, None);
        let (tx, rx) = mpsc::unbounded_channel();
        let drain = collect_events(rx);

        let outcome = engine
            .run_goal(
                &judge,
                "make the tests pass",
                &mut messages,
                &mut budget,
                &tx,
                &GoalConfig::default(),
            )
            .await;
        drop(tx);

        match outcome {
            GoalOutcome::Met {
                rounds,
                verdict,
                cost_usd,
            } => {
                assert_eq!(rounds, 2);
                assert_eq!(verdict, "all evidence present");
                // 2 worker turns + 2 judge calls.
                assert!((cost_usd - 0.022).abs() < 1e-9, "cost was {cost_usd}");
            }
            other => panic!("expected Met, got {other:?}"),
        }

        // The judge's feedback reached the worker as a user message.
        let feedback_message = messages
            .iter()
            .find(|m| m.role == MessageRole::User && m.content.contains("run the test suite"))
            .expect("judge feedback must be fed back into the conversation");
        assert!(feedback_message.content.contains("NOT yet met"));

        // Verdict events tell the whole arc, and judge spend is metered.
        let events = drain();
        let verdicts: Vec<(usize, bool)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::GoalVerdict { round, met, .. } => Some((*round, *met)),
                _ => None,
            })
            .collect();
        assert_eq!(verdicts, vec![(1, false), (2, true)]);
        assert!((budget.session_spent_usd() - 0.022).abs() < 1e-9);
    }

    #[tokio::test]
    async fn round_cap_ends_an_unconvinced_goal_loop() {
        let worker = ScriptedProvider::new(vec![
            Ok(text_result("try", 0.0)),
            Ok(text_result("try", 0.0)),
        ]);
        let judge = ScriptedProvider::new(vec![
            Ok(text_result(
                r#"{"met": false, "reasoning": "no", "feedback": "more"}"#,
                0.0,
            )),
            Ok(text_result(
                r#"{"met": false, "reasoning": "no", "feedback": "more"}"#,
                0.0,
            )),
        ]);
        let tools = NoTools;
        let engine = Engine::with_sleeper(&worker, &tools, EngineConfig::default(), &NoSleep);
        let mut messages = vec![CompletionMessage::system("sys")];
        let mut budget = BudgetGuard::new(BudgetMode::Observed, None, None);
        let (tx, _rx) = mpsc::unbounded_channel();

        let config = GoalConfig {
            max_rounds: 2,
            ..GoalConfig::default()
        };
        let outcome = engine
            .run_goal(
                &judge,
                "impossible",
                &mut messages,
                &mut budget,
                &tx,
                &config,
            )
            .await;

        match outcome {
            GoalOutcome::Unmet { rounds, reason, .. } => {
                assert_eq!(rounds, 2);
                assert!(reason.contains("round cap"), "{reason}");
            }
            other => panic!("expected Unmet, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unparseable_judge_output_wastes_a_round_not_the_run() {
        let worker = ScriptedProvider::new(vec![
            Ok(text_result("try", 0.0)),
            Ok(text_result("try again", 0.0)),
        ]);
        let judge = ScriptedProvider::new(vec![
            Ok(text_result("I think it looks pretty good!", 0.0)),
            Ok(text_result(
                r#"{"met": true, "reasoning": "done", "feedback": ""}"#,
                0.0,
            )),
        ]);
        let tools = NoTools;
        let engine = Engine::with_sleeper(&worker, &tools, EngineConfig::default(), &NoSleep);
        let mut messages = vec![CompletionMessage::system("sys")];
        let mut budget = BudgetGuard::new(BudgetMode::Observed, None, None);
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = engine
            .run_goal(
                &judge,
                "goal",
                &mut messages,
                &mut budget,
                &tx,
                &GoalConfig::default(),
            )
            .await;

        match outcome {
            GoalOutcome::Met { rounds, .. } => assert_eq!(rounds, 2),
            other => panic!("expected Met after recovering, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn judge_failure_ends_the_loop_with_a_named_reason() {
        let worker = ScriptedProvider::new(vec![Ok(text_result("try", 0.0))]);
        // Auth errors are non-retryable, so the judge fails immediately.
        let judge = ScriptedProvider::new(vec![Err(ProviderError::Auth("bad key".into()))]);
        let tools = NoTools;
        let engine = Engine::with_sleeper(&worker, &tools, EngineConfig::default(), &NoSleep);
        let mut messages = vec![CompletionMessage::system("sys")];
        let mut budget = BudgetGuard::new(BudgetMode::Observed, None, None);
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = engine
            .run_goal(
                &judge,
                "goal",
                &mut messages,
                &mut budget,
                &tx,
                &GoalConfig::default(),
            )
            .await;

        match outcome {
            GoalOutcome::Unmet { rounds, reason, .. } => {
                assert_eq!(rounds, 1);
                assert!(reason.contains("judge unavailable"), "{reason}");
            }
            other => panic!("expected Unmet, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn aborted_working_turn_ends_the_goal_loop() {
        // Worker's provider errors non-retryably → run_turn aborts.
        let worker = ScriptedProvider::new(vec![Err(ProviderError::Auth("expired".into()))]);
        let judge = ScriptedProvider::new(vec![]);
        let tools = NoTools;
        let engine = Engine::with_sleeper(&worker, &tools, EngineConfig::default(), &NoSleep);
        let mut messages = vec![CompletionMessage::system("sys")];
        let mut budget = BudgetGuard::new(BudgetMode::Observed, None, None);
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = engine
            .run_goal(
                &judge,
                "goal",
                &mut messages,
                &mut budget,
                &tx,
                &GoalConfig::default(),
            )
            .await;

        match outcome {
            GoalOutcome::Unmet { reason, .. } => {
                assert!(reason.contains("working turn aborted"), "{reason}");
                // The judge was never consulted about an aborted turn.
                assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
            }
            other => panic!("expected Unmet, got {other:?}"),
        }
    }

    #[test]
    fn parse_verdict_tolerates_prose_and_fences() {
        let fenced = "Here is my assessment:\n```json\n{\"met\": true, \"reasoning\": \"ok\"}\n```";
        let verdict = parse_verdict(fenced).expect("fenced JSON must parse");
        assert!(verdict.met);
        assert!(parse_verdict("no json here at all").is_none());
        assert!(parse_verdict("} backwards {").is_none());
    }

    #[test]
    fn transcript_tail_keeps_whole_recent_messages() {
        let messages = vec![
            CompletionMessage::system("sys"),
            CompletionMessage::user("first message that is quite old"),
            CompletionMessage::user("second message, most recent"),
        ];
        let tail = render_transcript_tail(&messages, 40);
        assert!(tail.contains("second message"));
        assert!(!tail.contains("first message"), "tail was: {tail}");
        // And a generous budget keeps everything, system prompt excluded.
        let full = render_transcript_tail(&messages, 10_000);
        assert!(full.contains("first message"));
        assert!(!full.contains("sys"));
    }

    #[test]
    fn truncate_chars_respects_multibyte_boundaries() {
        let s = "héllo wörld";
        let cut = truncate_chars(s, 4);
        assert_eq!(cut, "héll…");
        assert_eq!(truncate_chars("short", 10), "short");
    }
}
