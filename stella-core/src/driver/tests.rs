use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::CompletionUsage;
use stella_protocol::ToolSchema;
use stella_protocol::event::BudgetMode;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;

use super::*;
use crate::hooks::{HookAction, HookExecError, HookExecResult, HookMatcher};
use crate::retry::Sleeper;

/// A `Sleeper` that records but never actually waits.
#[derive(Default)]
struct NoopSleeper;
#[async_trait]
impl Sleeper for NoopSleeper {
    async fn sleep(&self, _duration_ms: u64) {}
}

/// A `ToolExecutor` that always succeeds and counts real invocations —
/// the counter is what `retry_never_re_executes_a_tool_call` asserts
/// against.
struct CountingTools {
    calls: Arc<AtomicU32>,
}
#[async_trait]
impl ToolExecutor for CountingTools {
    fn schemas(&self) -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "bash".into(),
            description: "run a command".into(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: false,
        }]
    }
    async fn execute(&self, _name: &str, _input: &Value) -> ToolOutput {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ToolOutput::Ok {
            content: "ok".into(),
        }
    }
}

/// A scripted `Provider`: pops one `Result` per call from a queue,
/// looping the last entry once exhausted. Used both for the flaky-retry
/// property test and the synthetic multi-dialect survival test.
struct ScriptedProvider {
    id: String,
    script: TokioMutex<Vec<Result<CompletionResultAlias, ProviderError>>>,
    calls: Arc<AtomicU32>,
}
#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        &self.id
    }
    async fn complete(
        &self,
        _req: CompletionRequest,
    ) -> Result<CompletionResultAlias, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut script = self.script.lock().await;
        if script.len() > 1 {
            script.remove(0)
        } else {
            clone_result(&script[0])
        }
    }
}

fn clone_result(
    r: &Result<CompletionResultAlias, ProviderError>,
) -> Result<CompletionResultAlias, ProviderError> {
    match r {
        Ok(v) => Ok(v.clone()),
        Err(e) => Err(clone_provider_error(e)),
    }
}

fn clone_provider_error(e: &ProviderError) -> ProviderError {
    match e {
        ProviderError::Transport(m) => ProviderError::Transport(m.clone()),
        ProviderError::RateLimited {
            message,
            retry_after_ms,
        } => ProviderError::RateLimited {
            message: message.clone(),
            retry_after_ms: *retry_after_ms,
        },
        ProviderError::Auth(m) => ProviderError::Auth(m.clone()),
        ProviderError::UnknownModel { slug } => ProviderError::UnknownModel { slug: slug.clone() },
        ProviderError::Malformed(m) => ProviderError::Malformed(m.clone()),
        ProviderError::Cancelled => ProviderError::Cancelled,
        ProviderError::Terminal(m) => ProviderError::Terminal(m.clone()),
    }
}

fn text_result(text: &str) -> CompletionResultAlias {
    CompletionResultAlias {
        text: text.into(),
        tool_calls: vec![],
        usage: CompletionUsage::reported_zero(),
        model: "scripted".into(),
        cost_usd: 0.0001,
        finish_reason: None,
    }
}

fn tool_call_result(call_id: &str, name: &str) -> CompletionResultAlias {
    CompletionResultAlias {
        text: String::new(),
        tool_calls: vec![ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            input: serde_json::json!({"cmd": "echo hi"}),
        }],
        usage: CompletionUsage::reported_zero(),
        model: "scripted".into(),
        cost_usd: 0.0001,
        finish_reason: None,
    }
}

fn drain_events(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

/// A provider that announces one tool call mid-"stream" via the
/// observer, then — when `wait_for_execution` — refuses to finish its
/// response until the tool has actually run. Returning at all therefore
/// PROVES the speculative execution overlapped the model call. The
/// second step completes the turn with plain text.
struct SpeculatingProvider {
    announce: ToolCall,
    /// The call the committed result carries — usually identical to
    /// `announce`; different in the divergence test.
    commit: ToolCall,
    executed: Arc<tokio::sync::Notify>,
    wait_for_execution: bool,
    step: AtomicU32,
}
#[async_trait]
impl Provider for SpeculatingProvider {
    fn id(&self) -> &str {
        "speculating"
    }
    async fn complete(
        &self,
        _req: CompletionRequest,
    ) -> Result<CompletionResultAlias, ProviderError> {
        unreachable!("the engine must drive complete_observed, never bare complete")
    }
    async fn complete_observed(
        &self,
        _req: CompletionRequest,
        observer: &dyn stella_protocol::ToolCallObserver,
    ) -> Result<CompletionResultAlias, ProviderError> {
        if self.step.fetch_add(1, Ordering::SeqCst) == 0 {
            observer.tool_call_streamed(&self.announce);
            if self.wait_for_execution {
                // "Keep streaming" until the speculated call has run.
                self.executed.notified().await;
            }
            Ok(CompletionResultAlias {
                text: String::new(),
                tool_calls: vec![self.commit.clone()],
                usage: CompletionUsage::reported_zero(),
                model: "speculating".into(),
                cost_usd: 0.0001,
                finish_reason: None,
            })
        } else {
            Ok(text_result("done"))
        }
    }
}

/// A read-only counting executor that signals each execution — the
/// other half of the overlap proof.
struct NotifyingReadTools {
    calls: Arc<AtomicU32>,
    executed: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl ToolExecutor for NotifyingReadTools {
    fn schemas(&self) -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "read_file".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: true,
        }]
    }
    async fn execute(&self, _name: &str, _input: &Value) -> ToolOutput {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.executed.notify_one();
        ToolOutput::Ok {
            content: "contents".into(),
        }
    }
}

fn read_call(input: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: "read_file".into(),
        input,
    }
}

async fn run_speculation_turn(
    provider: &SpeculatingProvider,
    tools: &dyn ToolExecutor,
) -> (TurnOutcome, Vec<AgentEvent>) {
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(provider, tools, EngineConfig::default(), &sleeper);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut messages = vec![CompletionMessage::user("read a.rs")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.run_turn(&mut messages, &mut budget, &tx),
    )
    .await
    .expect("a hung turn means speculation deadlocked the provider/pump join");
    (outcome, drain_events(&mut rx))
}

#[tokio::test]
async fn read_only_calls_execute_during_the_stream_and_are_harvested_not_rerun() {
    let executed = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicU32::new(0));
    let input = serde_json::json!({"path": "a.rs"});
    let provider = SpeculatingProvider {
        announce: read_call(input.clone()),
        commit: read_call(input),
        executed: executed.clone(),
        // The provider cannot finish its response until the tool ran —
        // completing the turn at all proves the overlap.
        wait_for_execution: true,
        step: AtomicU32::new(0),
    };
    let tools = NotifyingReadTools {
        calls: calls.clone(),
        executed,
    };

    let (outcome, events) = run_speculation_turn(&provider, &tools).await;
    assert!(
        matches!(outcome, TurnOutcome::Completed { ref text, .. } if text == "done"),
        "turn must complete: {outcome:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the speculated execution must be harvested, never re-run at dispatch"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { call_id, speculated: true, .. } if call_id == "c1"
        )),
        "the harvested result must be marked speculated: {events:?}"
    );
}

#[tokio::test]
async fn a_divergent_committed_call_is_re_executed_not_harvested() {
    let executed = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicU32::new(0));
    let provider = SpeculatingProvider {
        // Announced input differs from what the final result carries —
        // the harvest's exact-equality check must reject the pool entry.
        announce: read_call(serde_json::json!({"path": "a.rs"})),
        commit: read_call(serde_json::json!({"path": "b.rs"})),
        executed: executed.clone(),
        wait_for_execution: true,
        step: AtomicU32::new(0),
    };
    let tools = NotifyingReadTools {
        calls: calls.clone(),
        executed,
    };

    let (outcome, events) = run_speculation_turn(&provider, &tools).await;
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "divergence must fall back to a real execution of the committed call"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { call_id, speculated: false, .. } if call_id == "c1"
        )),
        "a re-executed result must NOT claim speculation: {events:?}"
    );
}

#[tokio::test]
async fn a_divergent_committed_call_emits_a_harvest_mismatch_discard() {
    // The announced read (a.rs) is speculated and runs real I/O; the
    // committed call (b.rs) diverges, so the pooled result is rejected at
    // harvest. That discarded execution must leave a trace on the wire so
    // event-log consumers can reconcile call counts (#370).
    let executed = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicU32::new(0));
    let provider = SpeculatingProvider {
        announce: read_call(serde_json::json!({"path": "a.rs"})),
        commit: read_call(serde_json::json!({"path": "b.rs"})),
        executed: executed.clone(),
        wait_for_execution: true,
        step: AtomicU32::new(0),
    };
    let tools = NotifyingReadTools {
        calls: calls.clone(),
        executed,
    };

    let (outcome, events) = run_speculation_turn(&provider, &tools).await;
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SpeculationDiscarded { call_id, name, reason }
                if call_id == "c1" && name == "read_file" && reason == "harvest_mismatch"
        )),
        "a rejected divergent pool entry must emit SpeculationDiscarded(harvest_mismatch): {events:?}"
    );
}

/// #460: a step that speculates a read-only call and then trips an ENFORCED
/// budget aborts the turn before `dispatch_completion` — the only place a
/// committed step's pool is harvested or discarded — ever runs. The read that
/// already executed would drop silently on the abort unwind; it must instead
/// emit `SpeculationDiscarded(budget_abort)` so #370's accounting holds on the
/// abort path too. Witness: this event is absent before the fix.
#[tokio::test]
async fn budget_abort_after_speculation_discards_the_pool() {
    let executed = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicU32::new(0));
    // announce == commit: on a non-aborting turn this read is *harvested*,
    // never a harvest_mismatch — so a discard here can only be the budget path.
    let provider = SpeculatingProvider {
        announce: read_call(serde_json::json!({"path": "a.rs"})),
        commit: read_call(serde_json::json!({"path": "a.rs"})),
        executed: executed.clone(),
        wait_for_execution: true,
        step: AtomicU32::new(0),
    };
    let tools = NotifyingReadTools {
        calls: calls.clone(),
        executed,
    };

    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut messages = vec![CompletionMessage::user("read a.rs")];
    // The step-0 cost ($0.0001) crosses the enforced $0.00005 turn limit, so
    // `handle_committed_result` returns `AbortTurn` before the pool is harvested.
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, Some(0.00005), None);
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.run_turn(&mut messages, &mut budget, &tx),
    )
    .await
    .expect("a hung turn means speculation deadlocked the provider/pump join");
    let events = drain_events(&mut rx);

    assert!(
        matches!(outcome, TurnOutcome::Aborted { .. }),
        "the enforced budget aborts the turn: {outcome:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the read really ran — its I/O is exactly what the discard must account for"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SpeculationDiscarded { call_id, name, reason }
                if call_id == "c1" && name == "read_file" && reason == "budget_abort"
        )),
        "the pool dropped on the budget-abort unwind must emit \
         SpeculationDiscarded(budget_abort): {events:?}"
    );
}

/// Announces `announce` during its stream, FAILS its first attempt with a
/// retryable transport error (dropping that attempt's speculation pool),
/// then on the retry commits `commit`; a final step returns plain text. On
/// the failed attempt it waits (bounded by `first_attempt_wait_ms`) for the
/// speculative execution to notify: when the call IS speculated the wait
/// returns as soon as the tool ran, so the dropped attempt deterministically
/// executed (and fired any hooks) before failing; when it is NOT speculated
/// nothing notifies and the wait simply times out and the attempt fails.
struct FlakySpeculatingProvider {
    announce: ToolCall,
    commit: ToolCall,
    executed: Arc<tokio::sync::Notify>,
    first_attempt_wait_ms: u64,
    invocation: AtomicU32,
}
#[async_trait]
impl Provider for FlakySpeculatingProvider {
    fn id(&self) -> &str {
        "flaky-speculating"
    }
    async fn complete(
        &self,
        _req: CompletionRequest,
    ) -> Result<CompletionResultAlias, ProviderError> {
        unreachable!("the engine must drive complete_observed, never bare complete")
    }
    async fn complete_observed(
        &self,
        _req: CompletionRequest,
        observer: &dyn stella_protocol::ToolCallObserver,
    ) -> Result<CompletionResultAlias, ProviderError> {
        match self.invocation.fetch_add(1, Ordering::SeqCst) {
            0 => {
                observer.tool_call_streamed(&self.announce);
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(self.first_attempt_wait_ms),
                    self.executed.notified(),
                )
                .await;
                Err(ProviderError::Transport("blip".into()))
            }
            1 => {
                observer.tool_call_streamed(&self.announce);
                Ok(CompletionResultAlias {
                    text: String::new(),
                    tool_calls: vec![self.commit.clone()],
                    usage: CompletionUsage::reported_zero(),
                    model: "flaky-speculating".into(),
                    cost_usd: 0.0001,
                    finish_reason: None,
                })
            }
            _ => Ok(text_result("done")),
        }
    }
}

#[tokio::test]
async fn a_failed_attempts_speculative_pool_emits_discarded_events() {
    // No hooks: the read IS speculated, so the failed first attempt runs it
    // for real (the wait returns the moment it does) and then drops the
    // pool. That completed-but-dropped execution must be reported (#370).
    let executed = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicU32::new(0));
    let input = serde_json::json!({"path": "a.rs"});
    let provider = FlakySpeculatingProvider {
        announce: read_call(input.clone()),
        commit: read_call(input),
        executed: executed.clone(),
        first_attempt_wait_ms: 2_000,
        invocation: AtomicU32::new(0),
    };
    let tools = NotifyingReadTools {
        calls: calls.clone(),
        executed,
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![CompletionMessage::user("read a.rs")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.run_turn(&mut messages, &mut budget, &tx),
    )
    .await
    .expect("a hung turn means the pump/provider join deadlocked");
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let events = drain_events(&mut rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SpeculationDiscarded { call_id, name, reason }
                if call_id == "c1" && name == "read_file" && reason == "attempt_failed"
        )),
        "the dropped attempt's completed read must emit SpeculationDiscarded(attempt_failed): {events:?}"
    );
}

#[tokio::test]
async fn a_hooked_read_fires_its_hook_once_never_for_a_dropped_speculative_attempt() {
    // A read-only tool with a configured PreToolUse hook. The first stream
    // attempt announces it and then fails; the retry commits it. The hook
    // must fire exactly ONCE — on the committed dispatch path — never for
    // the dropped attempt. Regression: before the fix the read was
    // speculated on the doomed attempt too, firing the hook a second time
    // for a call that never reached the transcript (#370).
    let executed = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicU32::new(0));
    let input = serde_json::json!({"path": "a.rs"});
    let provider = FlakySpeculatingProvider {
        announce: read_call(input.clone()),
        commit: read_call(input),
        executed: executed.clone(),
        // Pre-fix the dropped attempt speculates the read and fires the hook
        // before this returns; post-fix the hooked read is never speculated,
        // so nothing notifies and this bounded wait just times out.
        first_attempt_wait_ms: 300,
        invocation: AtomicU32::new(0),
    };
    let tools = NotifyingReadTools {
        calls: calls.clone(),
        executed,
    };
    let sleeper = NoopSleeper;
    let payloads = Arc::new(TokioMutex::new(Vec::new()));
    // exit 0: non-blocking, so the tool runs and the hook is a pure
    // observation — what matters is how many times it is invoked.
    let runner = RecordingHookRunner {
        exit_code: 0,
        stdout: String::new(),
        stderr: String::new(),
        payloads: payloads.clone(),
    };
    let hooks = Hooks {
        pre_tool_use: Some(vec![HookMatcher {
            matcher: Some("read_file".into()),
            hooks: vec![HookAction::new("audit-log")],
        }]),
        ..Hooks::default()
    };
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper)
        .with_hooks(&hooks, &runner);
    let mut messages = vec![CompletionMessage::user("read a.rs")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.run_turn(&mut messages, &mut budget, &tx),
    )
    .await
    .expect("a hung turn means the pump/provider join deadlocked");
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let pre_fires = payloads
        .lock()
        .await
        .iter()
        .filter(|p| p.contains("\"event\":\"PreToolUse\""))
        .count();
    assert_eq!(
        pre_fires, 1,
        "PreToolUse must fire once per committed call, never for a dropped speculative attempt"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the hooked read must execute once, on the committed dispatch path"
    );
}

/// A provider that streams its answer through the observer before
/// committing it — the adapter side of token-level streaming.
struct StreamingTextProvider;
#[async_trait]
impl Provider for StreamingTextProvider {
    fn id(&self) -> &str {
        "streaming"
    }
    async fn complete(
        &self,
        _req: CompletionRequest,
    ) -> Result<CompletionResultAlias, ProviderError> {
        unreachable!("the engine must drive complete_observed, never bare complete")
    }
    async fn complete_observed(
        &self,
        _req: CompletionRequest,
        observer: &dyn stella_protocol::ToolCallObserver,
    ) -> Result<CompletionResultAlias, ProviderError> {
        observer.text_delta("Hel");
        observer.text_delta("lo!");
        Ok(text_result("Hello!"))
    }
}

#[tokio::test]
async fn text_deltas_precede_the_authoritative_text_and_concatenate_to_it() {
    let provider = StreamingTextProvider;
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut messages = vec![CompletionMessage::user("say hello")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(
        matches!(outcome, TurnOutcome::Completed { ref text, .. } if text == "Hello!"),
        "turn must complete: {outcome:?}"
    );

    let events = drain_events(&mut rx);
    let text_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Text { .. }))
        .expect("the authoritative Text event lands");
    let deltas: Vec<(usize, &str)> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            AgentEvent::TextDelta { text } => Some((i, text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 2, "both fragments stream live: {events:?}");
    assert!(
        deltas.iter().all(|(i, _)| *i < text_idx),
        "every delta precedes the authoritative Text: {events:?}"
    );
    let concatenated: String = deltas.iter().map(|(_, t)| *t).collect();
    match &events[text_idx] {
        AgentEvent::Text { delta } => assert_eq!(
            &concatenated, delta,
            "on a clean run the preview equals the committed text"
        ),
        other => unreachable!("{other:?}"),
    }
}

#[tokio::test]
async fn mutating_calls_are_never_speculated() {
    let executed = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicU32::new(0));
    let mutating = ToolCall {
        call_id: "c1".into(),
        name: "bash".into(),
        input: serde_json::json!({"cmd": "echo hi"}),
    };
    let provider = SpeculatingProvider {
        announce: mutating.clone(),
        commit: mutating,
        executed,
        // Must NOT wait: a mutating announcement is fenced, so nothing
        // would ever signal — waiting would (correctly) deadlock.
        wait_for_execution: false,
        step: AtomicU32::new(0),
    };
    let tools = CountingTools {
        calls: calls.clone(),
    };

    let (outcome, events) = run_speculation_turn(&provider, &tools).await;
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "exactly one execution, at dispatch — never during the stream"
    );
    assert!(
        events.iter().all(|e| !matches!(
            e,
            AgentEvent::ToolResult {
                speculated: true,
                ..
            }
        )),
        "no result of a mutating call may be speculated: {events:?}"
    );
}

#[tokio::test]
async fn simple_turn_with_no_tool_calls_completes() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("hello!"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert_eq!(
        outcome,
        TurnOutcome::Completed {
            text: "hello!".into(),
            cost_usd: 0.0001
        }
    );

    let events = drain_events(&mut rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Complete { .. }))
    );
}

/// ~900 chars of protected assistant text — compaction's pure passes
/// only touch tool outputs, so weight parked here can ONLY be reclaimed
/// by the summarization fallback.
fn big_assistant_text(tag: &str) -> CompletionMessage {
    CompletionMessage {
        role: MessageRole::Assistant,
        content: format!("{tag}: {}", "analysis ".repeat(100)),
        tool_calls: vec![],
        tool_results: vec![],
        attachments: Vec::new(),
    }
}

fn overflow_config() -> EngineConfig {
    EngineConfig {
        compaction_budget_tokens: 500,
        summarize_keep_recent: 2,
        ..EngineConfig::default()
    }
}

/// Every tool result's call_id must be answered by a PRECEDING
/// assistant tool_call — the provider-side pairing invariant that a
/// careless span cut would break.
fn assert_tool_pairing(messages: &[CompletionMessage]) {
    let mut seen_calls: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for message in messages {
        for call in &message.tool_calls {
            seen_calls.insert(call.call_id.as_str());
        }
        for result in &message.tool_results {
            assert!(
                seen_calls.contains(result.call_id.as_str()),
                "orphaned tool result `{}` after summarization",
                result.call_id
            );
        }
    }
}

/// A scripted [`crate::ports::TurnSteering`]: hands out its queue on
/// the first drain, and (optionally) latches the soft stop once
/// `stop_after_drains` boundaries have passed.
struct TestSteering {
    queue: std::sync::Mutex<Vec<String>>,
    stop_after_drains: Option<u32>,
    drains: AtomicU32,
}
impl crate::ports::TurnSteering for TestSteering {
    fn drain_steering(&self) -> Vec<String> {
        self.drains.fetch_add(1, Ordering::SeqCst);
        std::mem::take(&mut *self.queue.lock().unwrap())
    }
    fn soft_stop_requested(&self) -> bool {
        match self.stop_after_drains {
            Some(n) => self.drains.load(Ordering::SeqCst) > n,
            None => false,
        }
    }
}

#[tokio::test]
async fn steered_messages_inject_before_the_next_model_call() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("answered both"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let steering = TestSteering {
        queue: std::sync::Mutex::new(vec!["also check the tests".into()]),
        stop_after_drains: None,
        drains: AtomicU32::new(0),
    };
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper)
        .with_steering(&steering);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    let steer_idx = messages
        .iter()
        .position(|m| m.role == MessageRole::User && m.content == "also check the tests")
        .expect("steered text must enter the conversation as a user message");
    let reply_idx = messages
        .iter()
        .position(|m| m.role == MessageRole::Assistant)
        .expect("assistant reply");
    assert!(
        steer_idx < reply_idx,
        "steer must precede the model call that answers it"
    );
    let events = drain_events(&mut rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { text } if text == "also check the tests")),
        "steering must be visible in the event stream: {events:?}"
    );
}

#[tokio::test]
async fn soft_stop_ends_the_turn_keeping_completed_steps() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(tool_call_result("c1", "bash")),
            Ok(text_result("never reached")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let provider_calls = provider.calls.clone();
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    // Stop latches after the first boundary: step 0 runs fully (model
    // call + tool), step 1's boundary honors the stop.
    let steering = TestSteering {
        queue: std::sync::Mutex::new(vec![]),
        stop_after_drains: Some(1),
        drains: AtomicU32::new(0),
    };
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper)
        .with_steering(&steering);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
    ];
    let before_len = messages.len();
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    match outcome {
        TurnOutcome::Aborted { reason, .. } => assert_eq!(reason, SOFT_STOP_REASON),
        other => panic!("expected soft-stop abort, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1, "one step ran");
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1, "its tool ran");
    assert!(
        messages.len() > before_len,
        "completed work must be KEPT — soft stop never truncates"
    );
}

#[tokio::test]
async fn overflow_of_protected_content_is_summarized_and_metered() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(text_result("SUMMARY: earlier steps established the plan")),
            Ok(text_result("done!")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
    ];
    for i in 0..6 {
        messages.push(big_assistant_text(&format!("t{i}")));
    }
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    match outcome {
        TurnOutcome::Completed { text, cost_usd } => {
            assert_eq!(text, "done!");
            // Turn cost folds the summarizer's call in (0.0001 each).
            assert!(
                (cost_usd - 0.0002).abs() < 1e-9,
                "summarizer spend missing from turn cost: {cost_usd}"
            );
        }
        other => panic!("expected completion, got {other:?}"),
    }
    let markers = messages
        .iter()
        .filter(|m| m.content.starts_with("[earlier history summarized"))
        .count();
    assert_eq!(markers, 1, "exactly one summary message");
    assert_eq!(
        messages[1].content, "the task",
        "the task statement must survive verbatim"
    );
    assert!(
        budget.spent_usd() >= 0.0001,
        "summarizer spend must be metered into the guard"
    );
    let events = drain_events(&mut rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Compaction { summarized, .. } if *summarized > 0
        )),
        "summarization must be reported: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::BudgetTick { .. })),
        "summarizer spend must tick the budget stream"
    );
}

#[tokio::test]
async fn summarization_disabled_leaves_history_untouched() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("done!"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let calls = provider.calls.clone();
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let config = EngineConfig {
        summarize_overflow: false,
        ..overflow_config()
    };
    let engine = Engine::with_sleeper(&provider, &tools, config, &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
    ];
    for i in 0..6 {
        messages.push(big_assistant_text(&format!("t{i}")));
    }
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no summarizer call");
    assert!(
        !messages
            .iter()
            .any(|m| m.content.starts_with("[earlier history summarized")),
    );
}

#[tokio::test]
async fn summarizer_failure_is_non_fatal_and_leaves_history() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Err(ProviderError::Terminal("summarizer down".into())),
            Ok(text_result("done!")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
    ];
    for i in 0..6 {
        messages.push(big_assistant_text(&format!("t{i}")));
    }
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "a summarizer outage must never fail the turn: {outcome:?}"
    );
    assert!(
        !messages
            .iter()
            .any(|m| m.content.starts_with("[earlier history summarized")),
        "failed summarization must leave the conversation untouched"
    );
}

#[tokio::test]
async fn summarization_never_orphans_tool_results_at_the_span_edge() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(text_result("SUMMARY of the early exploration")),
            Ok(text_result("done!")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);
    // The naive span end (len - keep_recent) lands ON the tool-result
    // message; the summarizer must walk back so the assistant call and
    // its result stay together in the kept tail.
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
    ];
    for i in 0..4 {
        messages.push(big_assistant_text(&format!("t{i}")));
    }
    messages.push(CompletionMessage {
        role: MessageRole::Assistant,
        content: String::new(),
        tool_calls: vec![ToolCall {
            call_id: "edge".into(),
            name: "bash".into(),
            input: serde_json::json!({"cmd": "ls"}),
        }],
        tool_results: vec![],
        attachments: Vec::new(),
    });
    messages.push(CompletionMessage {
        role: MessageRole::Tool,
        content: String::new(),
        tool_calls: vec![],
        tool_results: vec![stella_protocol::ToolResult {
            call_id: "edge".into(),
            output: ToolOutput::Ok {
                content: "small".into(),
            },
        }],
        attachments: Vec::new(),
    });
    messages.push(big_assistant_text("tail"));
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert!(
        messages
            .iter()
            .any(|m| m.content.starts_with("[earlier history summarized")),
        "summarization should have fired"
    );
    assert_tool_pairing(&messages);
}

fn empty_result(finish_reason: Option<FinishReason>) -> CompletionResultAlias {
    CompletionResultAlias {
        text: String::new(),
        tool_calls: vec![],
        usage: CompletionUsage {
            reported: true,
            input_tokens: 100,
            output_tokens: 8192,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
        },
        model: "scripted".into(),
        cost_usd: 0.05,
        finish_reason,
    }
}

#[tokio::test]
async fn empty_completion_aborts_with_a_visible_message_not_a_silent_success() {
    // A turn that yields no text AND no tool calls — e.g. the model spent
    // its whole output budget on reasoning and was cut off at
    // finish_reason "length" — must never be recorded as a clean
    // completion. It must surface why and abort. Regression for the
    // "turn ends with no feedback, feature never built" defect.
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(empty_result(Some(FinishReason::Length)))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("build the feature"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(
        matches!(outcome, TurnOutcome::Aborted { .. }),
        "an empty completion must abort, not complete: {outcome:?}"
    );

    let events = drain_events(&mut rx);
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "the user must see an error explaining the empty turn"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Complete { .. })),
        "an empty turn must NOT emit a Complete success marker"
    );
}

#[tokio::test]
async fn tool_calls_execute_and_feed_back_into_history() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(tool_call_result("call_1", "bash")),
            Ok(text_result("done")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert_eq!(
        outcome,
        TurnOutcome::Completed {
            text: "done".into(),
            cost_usd: 0.0002
        }
    );
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);

    let events = drain_events(&mut rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { .. }))
    );
}

#[tokio::test]
async fn retry_never_re_executes_a_tool_call() {
    // Property: a step's tool call is executed exactly once, even when
    // the model call surrounding it needed retries elsewhere in the
    // turn. Script: transient failures, then a tool call, then success
    // — the tool must be counted exactly once, never per retry.
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Err(ProviderError::Transport("blip".into())),
            Err(ProviderError::Transport("blip again".into())),
            Ok(tool_call_result("call_1", "bash")),
            Ok(text_result("done")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert_eq!(
        outcome,
        TurnOutcome::Completed {
            text: "done".into(),
            cost_usd: 0.0002
        }
    );
    assert_eq!(
        tool_calls.load(Ordering::SeqCst),
        1,
        "the tool call must execute exactly once, never once per model-call retry"
    );

    // And the doomed early attempts produced no per-attempt wire event
    // beyond the two `Retry` entries for the step that actually
    // committed (L-E10 — see module docs).
    let events = drain_events(&mut rx);
    let retry_events = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Retry { .. }))
        .count();
    assert_eq!(retry_events, 2);
}

#[tokio::test]
async fn malformed_tool_call_input_is_repaired_not_executed_blindly() {
    let mut malformed_call = tool_call_result("call_1", "bash");
    malformed_call.tool_calls[0].input = Value::Null;
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(malformed_call), Ok(text_result("done"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let _ = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert_eq!(
        tool_calls.load(Ordering::SeqCst),
        0,
        "a malformed (Null-input) call must never reach the real tool executor"
    );
    // The synthesized error result must be visible in history so the
    // model sees it and can retry with valid JSON.
    let tool_message = messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("a tool message was appended");
    match &tool_message.tool_results[0].output {
        ToolOutput::Error { message } => assert!(message.contains("malformed")),
        other => panic!("expected a malformed-call error, got {other:?}"),
    }
}

#[tokio::test]
async fn stuck_loop_aborts_the_turn_cleanly_before_the_step_cap() {
    // Every call returns the identical tool call and the tool answers with
    // identical output — well past the default exact-repeat threshold (3)
    // — so loop detection must end the turn long before
    // EngineConfig::default()'s 200-step cap.
    let repeated = tool_call_result("call_1", "bash");
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(repeated)]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    match outcome {
        TurnOutcome::Aborted { reason, .. } => assert!(reason.contains("stuck-loop")),
        other => panic!("expected a stuck-loop abort, got {other:?}"),
    }
    // Well under the 200-step cap — loop detection caught it early.
    assert!(tool_calls.load(Ordering::SeqCst) < 10);

    let events = drain_events(&mut rx);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
}

#[tokio::test]
async fn stuck_loop_steers_once_then_aborts_on_re_detection() {
    // The exact steer-then-abort sequencing: three identical no-progress
    // calls earn a steering warning, the model ignores it with a fourth
    // identical call, and only THAT detection aborts. The count also
    // proves the injected warning (a User-role message) did not reset the
    // detection window — a reset would demand three more calls, not one.
    let repeated = tool_call_result("call_1", "bash");
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(repeated)]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(matches!(outcome, TurnOutcome::Aborted { .. }));
    assert_eq!(
        tool_calls.load(Ordering::SeqCst),
        4,
        "three calls to detect and steer, ONE more no-progress call to abort"
    );

    let events = drain_events(&mut rx);
    let steers: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Steered { .. }))
        .collect();
    assert_eq!(steers.len(), 1, "exactly one steering warning per turn");
    assert!(matches!(
        steers[0],
        AgentEvent::Steered { text } if text.contains("looping")
    ));
    // Typed decisions (receipts spec §6.3): each detection also lands as a
    // parseable LoopDetected — the steer with `aborted: false`, the kill
    // with `aborted: true` — so receipts never string-match Error prefixes.
    let detections: Vec<(&str, &Vec<String>, bool)> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::LoopDetected {
                kind,
                pattern,
                aborted,
                ..
            } => Some((kind.as_str(), pattern, *aborted)),
            _ => None,
        })
        .collect();
    assert_eq!(
        detections.len(),
        2,
        "one typed event per detection (steer + abort): {detections:?}"
    );
    assert_eq!(detections[0].0, "exact_repeat");
    assert_eq!(detections[0].1, &vec!["bash".to_string()]);
    assert!(!detections[0].2, "first detection steers");
    assert!(detections[1].2, "second detection aborts");
    // The warning precedes the abort's Error event — steer first, abort
    // second.
    let steer_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }));
    let error_pos = events.iter().position(|e| {
        matches!(
            e,
            AgentEvent::Error {
                retryable: false,
                ..
            }
        )
    });
    assert!(
        steer_pos < error_pos,
        "steer at {steer_pos:?}, abort at {error_pos:?}"
    );
    // The warning is in history for the model (and the next turn) to see.
    assert!(
        messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.content.starts_with(LOOP_STEER_PREFIX)),
        "the steering warning must ride the conversation itself"
    );
}

/// A tool whose output changes every invocation — a `read_output`-style
/// poll of a still-running process.
struct PollingTools {
    calls: Arc<AtomicU32>,
}
#[async_trait]
impl ToolExecutor for PollingTools {
    fn schemas(&self) -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "bash".into(),
            description: "run a command".into(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: false,
        }]
    }
    async fn execute(&self, _name: &str, _input: &Value) -> ToolOutput {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        ToolOutput::Ok {
            content: format!("[{n}s] still running..."),
        }
    }
}

#[tokio::test]
async fn identical_polls_with_changing_output_complete_without_abort() {
    // Six byte-identical calls (same name, same input, no cursor field) —
    // but every poll returns new output. That is visible progress, not a
    // loop: the turn must run to completion with no steering and no abort.
    let mut script: Vec<Result<CompletionResultAlias, ProviderError>> = (0..6)
        .map(|_| Ok(tool_call_result("call_1", "bash")))
        .collect();
    script.push(Ok(text_result("build finished")));
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(script),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = PollingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("run the build and wait for it"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    match outcome {
        TurnOutcome::Completed { text, .. } => assert_eq!(text, "build finished"),
        other => panic!("polling with changing output must complete, got {other:?}"),
    }
    assert_eq!(tool_calls.load(Ordering::SeqCst), 6, "every poll ran");
    let events = drain_events(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "progress must not even draw a steering warning"
    );
}

#[tokio::test]
async fn period_three_cycle_with_no_progress_steers_then_aborts() {
    // The common real stuck signature: read → failing edit → failing test,
    // with byte-identical outputs every cycle — invisible to exact-repeat
    // (no consecutive repeat) and to a two-call-only cycle detector.
    let cycle_call = |i: usize| {
        let (name, input) = match i % 3 {
            0 => ("read_file", serde_json::json!({"path": "a.rs"})),
            1 => (
                "edit_file",
                serde_json::json!({"path": "a.rs", "old": "x", "new": "y"}),
            ),
            _ => ("bash", serde_json::json!({"cmd": "cargo test"})),
        };
        Ok(CompletionResultAlias {
            text: String::new(),
            tool_calls: vec![ToolCall {
                call_id: format!("call_{i}"),
                name: name.into(),
                input,
            }],
            usage: CompletionUsage::reported_zero(),
            model: "scripted".into(),
            cost_usd: 0.0001,
            finish_reason: None,
        })
    };
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new((0..12).map(cycle_call).collect()),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    let config = EngineConfig {
        loop_detection: LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 2,
        },
        ..EngineConfig::default()
    };
    let engine = Engine::with_sleeper(&provider, &tools, config, &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("fix the test"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    match outcome {
        TurnOutcome::Aborted { reason, .. } => {
            assert!(reason.contains("stuck-loop"), "unexpected reason: {reason}")
        }
        other => panic!("expected a stuck-loop abort, got {other:?}"),
    }
    // Two full cycles (6 calls) to detect and steer, one more no-progress
    // call to abort — never the 12-entry script (let alone the step cap).
    assert_eq!(tool_calls.load(Ordering::SeqCst), 7);
    let events = drain_events(&mut rx);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Steered { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn enforced_budget_aborts_the_turn_cleanly_between_steps() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(tool_call_result("call_1", "bash"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    // Budget of $0.00005 is below a single $0.0001 call's cost, so the
    // very first call's spend trips enforced mode.
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, Some(0.00005), None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    match outcome {
        TurnOutcome::Aborted { reason, cost_usd } => {
            assert!(reason.contains("budget"));
            assert!(
                (cost_usd - 0.0001).abs() < 1e-9,
                "the abort must retain the settled over-cap call: {cost_usd}"
            );
        }
        other => panic!("expected a budget abort, got {other:?}"),
    }
    let events = drain_events(&mut rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::BudgetTick { .. }))
    );
    // Typed decision (receipts spec §6.3): the denial is parseable, not
    // just a prose Error. Scope is the axis that tripped; mode is
    // Enforced by construction (only enforced budgets abort).
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::BudgetDenied {
                scope: stella_protocol::BudgetScope::Turn,
                mode: BudgetMode::Enforced,
                spent_usd,
                limit_usd,
            } if *spent_usd > *limit_usd
        )),
        "expected a typed BudgetDenied: {events:?}"
    );
}

/// Exit criterion: "synthetic 200-step turn
/// (scripted provider incl. 429s, stream drop, context pressure)
/// survives across three dialects (GLM 5.2, Anthropic, OpenAI
/// shapes)". "Dialect" at this layer (`stella-core`, which never
/// touches HTTP/SSE — that's `stella-model`'s job, tested there) means
/// varying provider *behavior*: call-id conventions, injected 429s
/// (`RateLimited`), injected transport drops, and steadily growing tool
/// output that forces repeated compaction — the shapes a real
/// GLM/Anthropic/OpenAI backend can actually produce at this seam.
async fn run_synthetic_survival_turn(dialect: &str, id_style: fn(u32) -> String) -> TurnOutcome {
    const STEPS: u32 = 200;
    let mut script: Vec<Result<CompletionResultAlias, ProviderError>> = Vec::new();
    for i in 0..STEPS {
        match i % 10 {
            // A 429 that must be retried, not fatal.
            3 => script.push(Err(ProviderError::RateLimited {
                message: format!("{dialect} rate limited"),
                retry_after_ms: Some(1),
            })),
            // A transport-level "stream drop" — also retried.
            7 => script.push(Err(ProviderError::Transport(format!(
                "{dialect} stream drop"
            )))),
            _ => {}
        }
        // Growing tool output simulates context pressure — compaction
        // must keep the turn alive rather than the provider choking on
        // an ever-larger prompt.
        let big_output_call_id = id_style(i);
        script.push(Ok(CompletionResultAlias {
            text: String::new(),
            tool_calls: vec![ToolCall {
                call_id: big_output_call_id,
                name: "bash".into(),
                input: serde_json::json!({"cmd": format!("step {i}")}),
            }],
            usage: CompletionUsage::reported_zero(),
            model: format!("{dialect}-model"),
            cost_usd: 0.00001,
            finish_reason: None,
        }));
    }
    script.push(Ok(text_result(&format!("{dialect} turn complete"))));

    let provider = ScriptedProvider {
        id: dialect.into(),
        script: TokioMutex::new(script),
        calls: Arc::new(AtomicU32::new(0)),
    };
    // A tool executor returning a constant 600-char output — the context
    // pressure half of the exit criterion.
    struct GrowingTools;
    #[async_trait]
    impl ToolExecutor for GrowingTools {
        fn schemas(&self) -> Vec<ToolSchema> {
            vec![ToolSchema {
                name: "bash".into(),
                description: "run a command".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: false,
            }]
        }
        async fn execute(&self, _name: &str, _input: &Value) -> ToolOutput {
            ToolOutput::Ok {
                content: "x".repeat(600), // consistently "large" per compaction's threshold
            }
        }
    }
    let tools = GrowingTools;
    let sleeper = NoopSleeper;
    let config = EngineConfig {
        // Keep the retry backoff floor at 0 so 200 steps with injected
        // 429s/drops still runs near-instantly under NoopSleeper.
        retry_policy: RetryPolicy::new(3, 0, 0),
        // A tight-ish compaction budget so the growing tool output
        // actually forces multiple compaction passes over 200 steps.
        compaction_budget_tokens: 4_000,
        // 200 tool-call steps plus the final text response is 201 model
        // calls — one more than EngineConfig::default()'s own step cap
        // (200), which exists as an *independent* backstop above loop
        // detection, not a ceiling this test should be fighting.
        max_steps: STEPS as usize + 1,
        ..EngineConfig::default()
    };
    let engine = Engine::with_sleeper(&provider, &tools, config, &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("run the long task"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Observed, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    engine.run_turn(&mut messages, &mut budget, &tx).await
}

#[tokio::test]
async fn synthetic_200_step_turn_survives_glm_shape() {
    let outcome = run_synthetic_survival_turn("glm", |i| format!("call_{i}")).await;
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "GLM-shaped turn must survive 200 steps with injected 429s/drops/context pressure, got {outcome:?}"
    );
}

#[tokio::test]
async fn synthetic_200_step_turn_survives_anthropic_shape() {
    // Anthropic's tool_use ids are its own `toolu_...` convention —
    // varying the id shape alone is enough to prove the driver never
    // assumes anything about call-id format.
    let outcome = run_synthetic_survival_turn("anthropic", |i| format!("toolu_{i:08x}")).await;
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "Anthropic-shaped turn must survive 200 steps, got {outcome:?}"
    );
}

#[tokio::test]
async fn synthetic_200_step_turn_survives_openai_shape() {
    let outcome = run_synthetic_survival_turn("openai", |i| format!("call_{i:016x}")).await;
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "OpenAI-shaped turn must survive 200 steps, got {outcome:?}"
    );
}

// ---- Parallel tool execution ------------------------------------------

fn read_only_schema(name: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: "read".into(),
        input_schema: serde_json::json!({"type": "object"}),
        read_only: true,
    }
}

fn multi_call_result(calls: &[(&str, &str)]) -> CompletionResultAlias {
    CompletionResultAlias {
        text: String::new(),
        tool_calls: calls
            .iter()
            .map(|(id, name)| ToolCall {
                call_id: (*id).into(),
                name: (*name).into(),
                input: serde_json::json!({"which": *id}),
            })
            .collect(),
        usage: CompletionUsage::reported_zero(),
        model: "scripted".into(),
        cost_usd: 0.0001,
        finish_reason: None,
    }
}

/// Read-only tools that rendezvous on a barrier: the step completes
/// ONLY if both calls are in flight at the same time. Sequential
/// execution deadlocks here — the timeout below converts that into a
/// named failure.
struct BarrierTools {
    barrier: tokio::sync::Barrier,
}
#[async_trait]
impl ToolExecutor for BarrierTools {
    fn schemas(&self) -> Vec<ToolSchema> {
        vec![read_only_schema("read_file")]
    }
    async fn execute(&self, _name: &str, _input: &Value) -> ToolOutput {
        self.barrier.wait().await;
        ToolOutput::Ok {
            content: "read".into(),
        }
    }
}

#[tokio::test]
async fn read_only_calls_in_one_step_execute_concurrently() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(multi_call_result(&[
                ("call_1", "read_file"),
                ("call_2", "read_file"),
            ])),
            Ok(text_result("done")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = BarrierTools {
        barrier: tokio::sync::Barrier::new(2),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("read two files"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.run_turn(&mut messages, &mut budget, &tx),
    )
    .await
    .expect(
        "two read-only calls in one step must run concurrently — a sequential \
             executor deadlocks on the barrier",
    );
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
}

/// Tools that log start/end order. The two read-only `read_file` calls
/// run a two-phase Notify handshake (not a wall-clock sleep — a loaded
/// CI runner can stall the "fast" path past any sleep): call_1 announces
/// its start and then waits for call_2 to end; call_2 refuses to end
/// until call_1 has started. Each call blocks on the other, so BOTH
/// sequential orders deadlock (caught by the test's timeout) and only
/// genuinely overlapping execution completes. Mutating `edit_file`
/// records that it saw a quiet world (no read in flight).
struct RecordingTools {
    log: Arc<TokioMutex<Vec<String>>>,
    read1_started: tokio::sync::Notify,
    read2_done: tokio::sync::Notify,
}
#[async_trait]
impl ToolExecutor for RecordingTools {
    fn schemas(&self) -> Vec<ToolSchema> {
        vec![
            read_only_schema("read_file"),
            ToolSchema {
                name: "edit_file".into(),
                description: "edit".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: false,
            },
        ]
    }
    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        let which = input.get("which").and_then(|v| v.as_str()).unwrap_or("?");
        self.log.lock().await.push(format!("start:{name}:{which}"));
        if name == "read_file" && which == "call_1" {
            // Phase 1: tell call_2 we started, then wait for it to end.
            // A sequential executor running call_1 first parks here
            // forever (call_2 never runs) — caught by the outer timeout.
            self.read1_started.notify_one();
            self.read2_done.notified().await;
        }
        if name == "read_file" && which == "call_2" {
            // Phase 2: refuse to end until call_1 has started (Notify
            // stores the permit if call_1 got there first). A sequential
            // executor running call_2 first parks here forever — so
            // neither serial order can sneak past the overlap assert.
            self.read1_started.notified().await;
        }
        self.log.lock().await.push(format!("end:{name}:{which}"));
        if name == "read_file" && which == "call_2" {
            self.read2_done.notify_one();
        }
        ToolOutput::Ok {
            content: "done".into(),
        }
    }
}

#[tokio::test]
async fn mutating_calls_are_barriers_and_history_keeps_call_order() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(multi_call_result(&[
                ("call_1", "read_file"),
                ("call_2", "read_file"),
                ("call_3", "edit_file"),
                ("call_4", "read_file"),
            ])),
            Ok(text_result("done")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let log = Arc::new(TokioMutex::new(Vec::new()));
    let tools = RecordingTools {
        log: log.clone(),
        read1_started: tokio::sync::Notify::new(),
        read2_done: tokio::sync::Notify::new(),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("work"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.run_turn(&mut messages, &mut budget, &tx),
    )
    .await
    .expect("reads must overlap — a sequential executor deadlocks on the handshake");
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));

    // Sequencing: the mutating call is a barrier — it must start only
    // after BOTH reads ended, and the trailing read only after it ended.
    let log = log.lock().await.clone();
    let position = |entry: &str| {
        log.iter()
            .position(|l| l == entry)
            .unwrap_or_else(|| panic!("missing `{entry}` in {log:?}"))
    };
    assert!(position("start:edit_file:call_3") > position("end:read_file:call_1"));
    assert!(position("start:edit_file:call_3") > position("end:read_file:call_2"));
    assert!(position("start:read_file:call_4") > position("end:edit_file:call_3"));

    // Real concurrency inside the read group: the slow first read ends
    // AFTER the fast second read (sequential execution in either order
    // deadlocks on the handshake and never reaches this assert).
    assert!(
        position("end:read_file:call_2") < position("end:read_file:call_1"),
        "reads did not overlap — executed sequentially? log: {log:?}"
    );

    // History: the Tool message's results are in original call order
    // even though completion order inverted.
    let tool_message = messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("a Tool message must be recorded");
    let ids: Vec<&str> = tool_message
        .tool_results
        .iter()
        .map(|r| r.call_id.as_str())
        .collect();
    assert_eq!(ids, vec!["call_1", "call_2", "call_3", "call_4"]);

    // Events: ToolResult for the fast read arrives before the slow one
    // (completion order), and consumers correlate by call_id.
    let mut result_order = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ToolResult { call_id, .. } = event {
            result_order.push(call_id);
        }
    }
    let pos_1 = result_order.iter().position(|id| id == "call_1").unwrap();
    let pos_2 = result_order.iter().position(|id| id == "call_2").unwrap();
    assert!(
        pos_2 < pos_1,
        "expected call_2 to complete first: {result_order:?}"
    );
}

// ---- StepUsage telemetry ----------------------------------------------

#[tokio::test]
async fn every_committed_step_emits_exactly_one_step_usage_record() {
    let with_usage = |text: &str, calls: &[(&str, &str)]| {
        let mut result = if calls.is_empty() {
            text_result(text)
        } else {
            multi_call_result(calls)
        };
        result.usage = CompletionUsage {
            reported: true,
            input_tokens: 1000,
            output_tokens: 50,
            cached_input_tokens: 800,
            cache_write_tokens: 120,
        };
        result
    };
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            // Step 0 commits after a retry, so StepUsage must say retries: 1.
            Err(ProviderError::RateLimited {
                message: "429".into(),
                retry_after_ms: Some(1),
            }),
            Ok(with_usage("", &[("call_1", "bash")])),
            Ok(with_usage("done", &[])),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("do work"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Observed, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));

    let mut usages = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::StepUsage {
            step,
            input_tokens,
            cached_input_tokens,
            cache_write_tokens,
            retries,
            tool_calls,
            cost_usd,
            ..
        } = event
        {
            usages.push((
                step,
                input_tokens,
                cached_input_tokens,
                cache_write_tokens,
                retries,
                tool_calls,
                cost_usd,
            ));
        }
    }
    // Two committed model calls → exactly two metering records; the
    // 429'd attempt shows up as retries: 1 on step 0, never as its own
    // record. Cache writes flow through from the provider's usage
    // envelope — never re-derived, never dropped to 0.
    assert_eq!(
        usages.len(),
        2,
        "one StepUsage per committed step: {usages:?}"
    );
    assert_eq!(usages[0], (0, 1000, 800, 120, 1, 1, 0.0001));
    assert_eq!(usages[1], (1, 1000, 800, 120, 0, 0, 0.0001));
}

// ---- Token-drift calibration -------------------------------------------

use crate::estimator::CalibrationMap;

/// A conversation with one old, evictable tool output — the shape
/// compaction acts on (the LAST tool message is protected).
fn compactable_history() -> Vec<CompletionMessage> {
    let tool_msg = |call_id: &str, content: String| CompletionMessage {
        role: MessageRole::Tool,
        content: String::new(),
        tool_calls: vec![],
        tool_results: vec![ToolResult {
            call_id: call_id.into(),
            output: ToolOutput::Ok { content },
        }],
        attachments: Vec::new(),
    };
    let assistant_with_call = |call_id: &str| CompletionMessage {
        role: MessageRole::Assistant,
        content: String::new(),
        tool_calls: vec![ToolCall {
            call_id: call_id.into(),
            name: "bash".into(),
            input: serde_json::json!({"cmd": call_id}),
        }],
        tool_results: vec![],
        attachments: Vec::new(),
    };
    vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("do things"),
        assistant_with_call("c1"),
        tool_msg("c1", "old ".repeat(1000)),
        assistant_with_call("c2"),
        tool_msg("c2", "new ".repeat(1000)),
    ]
}

/// Witness for the feedback loop's read side: the SAME conversation
/// under the SAME configured budget compacts only when calibration says
/// the raw estimate runs low against this model's tokenizer — the
/// compaction decision demonstrably consumes the calibrated estimate,
/// not the raw one.
#[tokio::test]
async fn calibrated_estimate_changes_the_compaction_decision() {
    let run = |calibrate: bool| async move {
        let provider = ScriptedProvider {
            id: "scripted".into(),
            script: TokioMutex::new(vec![Ok(text_result("done"))]),
            calls: Arc::new(AtomicU32::new(0)),
        };
        let tools = CountingTools {
            calls: Arc::new(AtomicU32::new(0)),
        };
        let sleeper = NoopSleeper;
        let mut messages = compactable_history();
        // A budget the RAW estimate just fits under: uncalibrated, no
        // compaction can fire.
        let raw = crate::estimator::estimate_conversation_tokens(&messages);
        let config = EngineConfig {
            compaction_budget_tokens: raw + 10,
            ..EngineConfig::default()
        };
        // Observed drift: this model's tokenizer reports 2× the char
        // heuristic (three samples — the minimum for the factor to
        // apply). Calibrated, the same conversation reads as ~2×(budget)
        // and must compact.
        let calibration = CalibrationMap::new();
        if calibrate {
            calibration.seed("scripted", &[(1000, 2000), (1000, 2000), (1000, 2000)]);
        }
        let engine = Engine::with_sleeper(&provider, &tools, config, &sleeper)
            .with_calibration(&calibration);
        let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
        assert!(matches!(outcome, TurnOutcome::Completed { .. }));
        drain_events(&mut rx)
            .iter()
            .any(|e| matches!(e, AgentEvent::Compaction { .. }))
    };

    assert!(
        !run(false).await,
        "uncalibrated, the conversation fits the budget — no compaction"
    );
    assert!(
        run(true).await,
        "with observed 2× drift the calibrated estimate exceeds the budget — \
             compaction must fire before the model call"
    );
}

/// Witness for the feedback loop's write side: every committed step
/// records its (estimated, actual) pair into the attached calibration —
/// keyed by the model that served it — and emits the raw estimate on
/// `StepUsage` for persistence.
#[tokio::test]
async fn each_committed_step_feeds_the_calibration_and_reports_its_estimate() {
    let with_real_usage = |result: CompletionResultAlias| {
        let mut result = result;
        result.usage = CompletionUsage {
            reported: true,
            input_tokens: 4_000,
            output_tokens: 50,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
        };
        // Vary each input: three byte-identical bash calls are exactly what
        // `loop_detect` exists to abort — this test is about the
        // calibration feed, not the loop breaker.
        if let Some(call) = result.tool_calls.first_mut() {
            call.input = serde_json::json!({ "cmd": format!("echo {}", call.call_id) });
        }
        result
    };
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(with_real_usage(tool_call_result("call_1", "bash"))),
            Ok(with_real_usage(tool_call_result("call_2", "bash"))),
            Ok(with_real_usage(tool_call_result("call_3", "bash"))),
            Ok(with_real_usage(text_result("done"))),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let calibration = CalibrationMap::new();
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper)
        .with_calibration(&calibration);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));

    // Every StepUsage carries the raw pre-call estimate (> 0: the
    // conversation is never empty).
    let estimates: Vec<u64> = drain_events(&mut rx)
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StepUsage {
                estimated_input_tokens,
                ..
            } => Some(*estimated_input_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(estimates.len(), 4);
    assert!(estimates.iter().all(|&e| e > 0), "{estimates:?}");

    // Four samples of a model reporting far more tokens than the tiny
    // history estimates: the correction engaged (past min-samples),
    // pushed up, and stayed inside its clamp — a noisy run can shift
    // budgeting by at most 2× in either direction.
    let factor = calibration.factor(Some("scripted"));
    assert!(
        factor > 1.0 && factor <= 2.0,
        "factor must be engaged and bounded, got {factor}"
    );
    assert_eq!(
        calibration.factor(Some("some-other-model")),
        1.0,
        "drift is keyed by the model that served the call"
    );
}

// ---- Lifecycle hooks wired into the turn path -------------------------

/// A no-I/O [`HookRunner`] test double: returns a fixed exit code +
/// stdout/stderr for every command and records the JSON payload of each
/// call, so a test can assert which lifecycle event fired and what it
/// carried — the same fake-runner discipline as `hooks.rs`'s own tests,
/// but here driven end-to-end through `run_turn`.
struct RecordingHookRunner {
    exit_code: i32,
    stdout: String,
    stderr: String,
    payloads: Arc<TokioMutex<Vec<String>>>,
}
#[async_trait]
impl HookRunner for RecordingHookRunner {
    async fn run(
        &self,
        _action: &HookAction,
        payload_json: &str,
        _cwd: &str,
    ) -> Result<HookExecResult, HookExecError> {
        self.payloads.lock().await.push(payload_json.to_string());
        Ok(HookExecResult {
            exit_code: self.exit_code,
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone(),
        })
    }
}

#[tokio::test]
async fn pre_tool_use_hook_nonzero_exit_blocks_the_tool_and_model_sees_it() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(tool_call_result("call_1", "bash")),
            Ok(text_result("done")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    let payloads = Arc::new(TokioMutex::new(Vec::new()));
    let runner = RecordingHookRunner {
        exit_code: 1,
        stdout: String::new(),
        stderr: "blocked by policy".into(),
        payloads: payloads.clone(),
    };
    let hooks = Hooks {
        pre_tool_use: Some(vec![HookMatcher {
            matcher: Some("*".into()),
            hooks: vec![HookAction::new("exit 1")],
        }]),
        ..Hooks::default()
    };
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper)
        .with_hooks(&hooks, &runner);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));

    // A blocking PreToolUse hook (non-zero exit) must keep the tool from
    // ever reaching the executor.
    assert_eq!(
        tool_calls.load(Ordering::SeqCst),
        0,
        "a PreToolUse hook that exits non-zero must block the tool from executing"
    );
    // ...and the model must see the block as a tool-result error, so it
    // can react — never an engine error.
    let tool_message = messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("a tool message was appended");
    match &tool_message.tool_results[0].output {
        ToolOutput::Error { message } => {
            assert!(message.contains("blocked by a PreToolUse hook"));
            assert!(
                message.contains("blocked by policy"),
                "the hook's own reason must be surfaced to the model: {message}"
            );
        }
        other => panic!("expected a hook-blocked error, got {other:?}"),
    }
    // Only PreToolUse fired — a blocked tool never runs, so no
    // PostToolUse observation follows.
    let payloads = payloads.lock().await.clone();
    assert_eq!(payloads.len(), 1);
    assert!(payloads[0].contains("\"event\":\"PreToolUse\""));
}

#[tokio::test]
async fn post_tool_use_hook_runs_after_the_tool_and_never_blocks() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(tool_call_result("call_1", "bash")),
            Ok(text_result("done")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    let payloads = Arc::new(TokioMutex::new(Vec::new()));
    // Exit 3 (non-zero) proves a *failing* PostToolUse hook is still a
    // pure observation — it can neither block nor abort the turn.
    let runner = RecordingHookRunner {
        exit_code: 3,
        stdout: String::new(),
        stderr: String::new(),
        payloads: payloads.clone(),
    };
    let hooks = Hooks {
        post_tool_use: Some(vec![HookMatcher {
            matcher: Some("*".into()),
            hooks: vec![HookAction::new("exit 3")],
        }]),
        ..Hooks::default()
    };
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper)
        .with_hooks(&hooks, &runner);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert_eq!(
        outcome,
        TurnOutcome::Completed {
            text: "done".into(),
            cost_usd: 0.0002
        }
    );
    // The tool ran — PostToolUse never gates execution.
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    // Exactly one PostToolUse hook fired, and it ran AFTER the tool: it
    // carries the tool's own result ("ok" from CountingTools).
    let payloads = payloads.lock().await.clone();
    assert_eq!(payloads.len(), 1);
    assert!(payloads[0].contains("\"event\":\"PostToolUse\""));
    assert!(
        payloads[0].contains("\"toolResult\":\"ok\""),
        "PostToolUse must fire after the tool and carry its result: {}",
        payloads[0]
    );
}

#[tokio::test]
async fn non_blocking_hook_failure_surfaces_as_one_retryable_error_event() {
    // A PostToolUse hook exiting non-zero stays non-blocking (pinned by the
    // test above) but must no longer vanish: the dispatch path forwards the
    // hook diagnostics as exactly one non-fatal Error event per tool call
    // (issue #373, item 6).
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(tool_call_result("call_1", "bash")),
            Ok(text_result("done")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let payloads = Arc::new(TokioMutex::new(Vec::new()));
    let runner = RecordingHookRunner {
        exit_code: 3,
        stdout: String::new(),
        stderr: "post hook broke".into(),
        payloads: payloads.clone(),
    };
    let hooks = Hooks {
        post_tool_use: Some(vec![HookMatcher {
            matcher: Some("*".into()),
            hooks: vec![HookAction::new("exit 3")],
        }]),
        ..Hooks::default()
    };
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper)
        .with_hooks(&hooks, &runner);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "a failing post-hook must never abort the turn: {outcome:?}"
    );
    let events = drain_events(&mut rx);
    let hook_errors: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::Error { message, retryable: true } if message.contains("hook problem")
            )
        })
        .collect();
    assert_eq!(
        hook_errors.len(),
        1,
        "exactly one non-fatal diagnostic event per affected call: {events:?}"
    );
    assert!(
        matches!(
            hook_errors[0],
            AgentEvent::Error { message, .. }
                if message.contains("bash") && message.contains("exited 3")
        ),
        "the event must name the tool and the failure: {hook_errors:?}"
    );
}

#[tokio::test]
async fn no_hooks_configured_leaves_the_turn_path_unchanged() {
    // With no hooks attached the tool executes normally and the turn
    // completes exactly as it did before the hooks seam existed — the
    // `None` branch is `self.tools.execute(...)` verbatim.
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(tool_call_result("call_1", "bash")),
            Ok(text_result("done")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    // Built WITHOUT `with_hooks` — `hooks` stays `None`.
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert_eq!(
        outcome,
        TurnOutcome::Completed {
            text: "done".into(),
            cost_usd: 0.0002
        }
    );
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    // The recorded result is the tool's own output, never a hook block.
    let tool_message = messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("a tool message was appended");
    assert_eq!(
        tool_message.tool_results[0].output,
        ToolOutput::Ok {
            content: "ok".into()
        }
    );
}

#[tokio::test]
async fn session_start_hooks_run_via_the_helper_not_per_turn() {
    // SessionStart is exposed as an explicit once-per-session helper
    // (Engine::run_session_start_hooks); run_turn must never fire it, so
    // a REPL calling run_turn repeatedly does not re-run session setup.
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("hi there"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let payloads = Arc::new(TokioMutex::new(Vec::new()));
    let runner = RecordingHookRunner {
        exit_code: 0,
        stdout: "on-call: alice".into(),
        stderr: String::new(),
        payloads: payloads.clone(),
    };
    let hooks = Hooks {
        session_start: Some(vec![HookMatcher {
            matcher: None,
            hooks: vec![HookAction::new("echo on-call: alice")],
        }]),
        ..Hooks::default()
    };
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper)
        .with_hooks(&hooks, &runner);

    // The helper fires SessionStart once and returns its stdout as the
    // additional system-prompt context.
    let context = engine.run_session_start_hooks().await;
    assert_eq!(context.as_deref(), Some("on-call: alice"));

    // A full turn must NOT fire SessionStart a second time.
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));

    let payloads = payloads.lock().await.clone();
    assert_eq!(
        payloads.len(),
        1,
        "run_turn must not fire SessionStart — only the helper does"
    );
    assert!(payloads[0].contains("\"event\":\"SessionStart\""));
}

#[test]
fn read_tally_footer_does_not_blind_loop_detection() {
    // `read_file` appends "read K\u{d7} this session" — different bytes on
    // EVERY read of an unchanged file. Comparison must see through the
    // footer, or the module-doc thrash (read → failing edit → read, and a
    // bare reread spiral) can never satisfy the byte-identical-output
    // requirement and detection is structurally blind for read_file.
    let read_call = |id: &str| CompletionMessage {
        role: MessageRole::Assistant,
        content: String::new(),
        tool_calls: vec![ToolCall {
            call_id: id.into(),
            name: "read_file".into(),
            input: serde_json::json!({ "path": "a.rs" }),
        }],
        tool_results: Vec::new(),
        attachments: Vec::new(),
    };
    let read_result = |id: &str, body: &str, count: usize| CompletionMessage {
        role: MessageRole::Tool,
        content: String::new(),
        tool_calls: Vec::new(),
        tool_results: vec![stella_protocol::ToolResult {
            call_id: id.into(),
            output: ToolOutput::Ok {
                content: format!(
                    "     1\tfn {body}() {{}}\n\n(1/1 lines shown \u{b7} read {count}\u{d7} this session)"
                ),
            },
        }],
        attachments: Vec::new(),
    };

    // Unchanged file reread four times: only the tally differs → a loop.
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("go"),
    ];
    for count in 1..=4usize {
        let id = format!("c{count}");
        messages.push(read_call(&id));
        messages.push(read_result(&id, "same", count));
    }
    let verdict = detect_loop(
        &recent_call_records(&messages),
        LoopDetectionConfig::default(),
    );
    assert!(
        verdict.is_loop(),
        "identical rereads must be a loop despite the tally footer: {verdict:?}"
    );

    // Content that genuinely changes between reads stays progress.
    let mut changing = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("go"),
    ];
    for count in 1..=4usize {
        let id = format!("d{count}");
        changing.push(read_call(&id));
        changing.push(read_result(&id, &format!("v{count}"), count));
    }
    assert_eq!(
        detect_loop(
            &recent_call_records(&changing),
            LoopDetectionConfig::default()
        ),
        crate::loop_detect::LoopVerdict::NoLoop
    );
}

mod audit_fixes;
mod task4;
mod usage_completeness;
