//! The step-driver: `Engine::run_turn`. One
//! model call per step, message accumulation, `AgentEvent` emission at
//! every boundary, retry+backoff, compaction, tool-output budget checks,
//! loop detection, and (a first, structural cut of) malformed-call repair —
//! wiring together every other module in this crate.
//!
//! `Engine` drives through `&dyn Provider` (`stella_protocol`) and
//! `&dyn ToolExecutor` (`crate::ports`) — no adapter-specific code, no
//! filesystem call, lives here. Everything
//! *inside* one step (compaction, loop detection, budget evaluation) is the
//! plain synchronous logic from the other modules in this crate; `run_turn`
//! is the one place that sequences them against real I/O.
//!
//! # Deferred-flush events (L-E10)
//!
//! [`crate::retry::retry_with_backoff`] already implements the contract:
//! on success it returns the *full* retry history (so a step that failed
//! twice then succeeded still reports two `Retry` events — the attempts
//! were real, they just didn't fail the step); on failure it returns only
//! the terminal error. `run_turn` emits events straight from that outcome,
//! so a step that never commits emits nothing about its doomed attempts —
//! there is nothing extra to build here, the discipline is inherited.
//!
//! # Retry never re-executes a tool call
//!
//! [`crate::retry::retry_with_backoff`] wraps *only* the model call
//! (`Provider::complete`). Tool execution happens exactly once, after a
//! model call has already succeeded and returned tool calls to run — it is
//! never inside the retried closure. A retried step therefore structurally
//! cannot re-execute a non-idempotent tool call; see the property test
//! `retry_never_re_executes_a_tool_call` below, which proves it by
//! counting real executions against a flaky scripted provider.
//!
//! # Budget is checked between steps, never mid-tool
//!
//! Per [`crate::budget`]'s module contract, `run_turn` only consults
//! [`crate::budget::BudgetGuard::evaluate`]/`record_spend` immediately
//! after a model call completes and before the next one (or before
//! executing this step's tool calls) — an `AbortTurn` outcome ends the turn
//! cleanly, it never interrupts a tool already in flight.
//!
//! # Malformed-call repair
//!
//! Every existing adapter's stream aggregator falls back to
//! `serde_json::Value::Null` when a tool call's streamed argument JSON
//! doesn't parse (`stella-model/src/{zai,anthropic}.rs`). `run_turn`
//! recognizes that sentinel structurally: rather than handing `Null` to a
//! tool that expects an object, it short-circuits to a named
//! `ToolOutput::Error` telling the model its own JSON was malformed, so the
//! model can retry with corrected syntax on the next step. This is a real,
//! if first-cut, repair — dialect-specific tuning (
//! §4.2: "malformed-call repair tuned to the failure shapes GLM actually
//! produces") is a documented follow-up, not faked here.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use futures_util::StreamExt;
use stella_protocol::{
    AgentEvent, CompletionMessage, CompletionRequest, FinishReason, MessageRole, Provider,
    ProviderError, ReasoningEffort, StageKind, ToolCall, ToolOutput, ToolResult,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::budget::{BudgetGuard, BudgetOutcome};
use crate::compaction::compact;
use crate::estimator::{CalibrationMap, estimate_conversation_tokens};
use crate::hooks::{HookPayload, HookRunner, Hooks, run_hooks};
use crate::loop_detect::{LoopDetectionConfig, detect_loop};
use crate::ports::ToolExecutor;
use crate::retry::{RetryOutcome, RetryPolicy, Sleeper, retry_with_backoff};

/// Everything about a turn's execution that isn't the provider/tools
/// themselves: prompt shape, retry/compaction/loop tuning, and hard
/// backstops. `Default` gives sensible starting values for `stella-cli`.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub effort: Option<ReasoningEffort>,
    pub retry_policy: RetryPolicy,
    pub loop_detection: LoopDetectionConfig,
    /// Compaction fires once the estimated conversation size exceeds this
    /// many tokens (`crate::estimator`). When calibration is attached
    /// ([`Engine::with_calibration`]) the comparison uses the
    /// drift-corrected estimate, so this budget is honored in the model's
    /// own observed tokens rather than raw heuristic tokens.
    pub compaction_budget_tokens: u64,
    /// Hard backstop on step count, independent of loop detection — belt
    /// and suspenders, never the *primary* stuck-loop defense (that's
    /// `crate::loop_detect`).
    pub max_steps: usize,
    /// Working directory reported to lifecycle hooks (`crate::hooks`) as the
    /// `cwd` of every [`HookPayload`]. Kept here — rather than sniffed via
    /// `std::env::current_dir()` inside the engine — so `stella-core`
    /// performs no I/O of its own: the caller
    /// (which already knows the workspace root) supplies the real path, and
    /// the `"."` default keeps hook-free turns unaffected. Only read when
    /// hooks are actually configured.
    pub cwd: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            // 16k, not 8k: reasoning models (e.g. glm-5.2) can spend their whole
            // output budget on chain-of-thought and get cut off before emitting
            // any answer. 16k gives the answer room to land after reasoning and
            // is within every seeded catalog model's output ceiling. Per-model
            // caps in the catalog are the eventual refinement.
            max_output_tokens: Some(16384),
            temperature: Some(0.0),
            effort: None,
            retry_policy: RetryPolicy::standard(),
            loop_detection: LoopDetectionConfig::default(),
            compaction_budget_tokens: 150_000,
            max_steps: 200,
            cwd: ".".to_string(),
        }
    }
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    /// The model produced a final text response with no further tool
    /// calls.
    Completed { text: String, cost_usd: f64 },
    /// The turn ended before completion: budget enforced, a loop was
    /// detected, retries were exhausted, or the step cap was hit. Always a
    /// *clean* abort — never mid-tool (see module docs).
    Aborted { reason: String },
}

/// The two references a turn needs to fire lifecycle hooks: the parsed
/// workspace [`Hooks`] config and the [`HookRunner`] execution port that
/// actually spawns the commands (the real process I/O `stella-core` never
/// performs — see `crate::hooks`). Bundled so the engine carries a single
/// `Option`: `None` means hooks are entirely off and the turn path is
/// byte-for-byte the same as before this seam existed. `Copy` because both
/// fields are shared references.
#[derive(Clone, Copy)]
struct HooksHandle<'a> {
    hooks: &'a Hooks,
    runner: &'a dyn HookRunner,
}

/// The step-driver. Holds no conversation state of its own — `run_turn`
/// takes the message history by `&mut` reference so callers (one-shot CLI,
/// REPL, fleet worker) own persistence and can inspect history after an
/// aborted turn.
pub struct Engine<'a> {
    pub(crate) provider: &'a dyn Provider,
    pub(crate) tools: &'a dyn ToolExecutor,
    pub(crate) sleeper: &'a dyn Sleeper,
    pub(crate) config: EngineConfig,
    /// Lifecycle hooks, off by default. Attached via [`Engine::with_hooks`]
    /// so `with_sleeper` keeps its existing signature. When `None`,
    /// no hook is ever consulted and the turn path adds zero work.
    hooks: Option<HooksHandle<'a>>,
    /// Token-drift calibration (`crate::estimator::CalibrationMap`), off by
    /// default. Attached via [`Engine::with_calibration`]; the caller owns
    /// the map across turns (and seeds it from persisted telemetry at
    /// session start), the engine feeds it every committed step's
    /// (estimated, actual) pair and reads the correction back into the
    /// compaction decision. When `None` the turn path is exactly the
    /// uncalibrated engine.
    pub(crate) calibration: Option<&'a CalibrationMap>,
}

/// Upper bound on tool calls from one step executing concurrently. Tools
/// are I/O-bound (process spawns, file reads), so this caps descriptor and
/// process pressure, not CPU.
const MAX_CONCURRENT_TOOL_CALLS: usize = 8;

/// One committed model call plus the step-scoped context the phases after
/// it consume: the pre-call raw token estimate (drift feedback + telemetry
/// — raw, never calibrated, see [`Engine::run_model_call`]), the read-only
/// tool set for dispatch scheduling, and the retry/duration figures for
/// the `StepUsage` metering record.
struct CommittedStep {
    result: CompletionResultAlias,
    /// Names of tools whose schemas declare `read_only`, snapshotted from
    /// the same `schemas()` call the request itself was built from.
    read_only_tools: HashSet<String>,
    estimated_input_tokens: u64,
    retries: u32,
    duration_ms: u64,
}

impl<'a> Engine<'a> {
    /// Construct an engine with an injected [`Sleeper`]. This is the only
    /// constructor — `stella-core` exports the port, never a production
    /// impl, so the caller wires a real sleeper (the CLI's tokio-backed
    /// one) and tests wire a no-op to run retries with zero real
    /// wall-clock delay.
    pub fn with_sleeper(
        provider: &'a dyn Provider,
        tools: &'a dyn ToolExecutor,
        config: EngineConfig,
        sleeper: &'a dyn Sleeper,
    ) -> Self {
        Self {
            provider,
            tools,
            sleeper,
            config,
            hooks: None,
            calibration: None,
        }
    }

    /// Attach lifecycle hooks (`crate::hooks`) to an engine, opt-in. Kept a
    /// builder so [`Engine::with_sleeper`] retains its signature and every
    /// existing call site is unchanged — an engine
    /// built without this is exactly the pre-hooks engine. Takes both the
    /// parsed [`Hooks`] config and the [`HookRunner`] that executes the
    /// commands, because [`crate::hooks::run_hooks`] needs the port to run
    /// anything (the config alone spawns nothing).
    pub fn with_hooks(mut self, hooks: &'a Hooks, runner: &'a dyn HookRunner) -> Self {
        self.hooks = Some(HooksHandle { hooks, runner });
        self
    }

    /// Attach token-drift calibration, opt-in and by reference for the same
    /// reason `run_turn` borrows `messages`: the caller (CLI session, REPL,
    /// fleet worker) owns state that outlives any single turn — engines are
    /// constructed per turn, calibration accumulates per session. An engine
    /// built without this estimates exactly as before.
    pub fn with_calibration(mut self, calibration: &'a CalibrationMap) -> Self {
        self.calibration = Some(calibration);
        self
    }

    /// Fire `SessionStart` hooks once and return any stdout they produced —
    /// the additional system-prompt context described in `crate::hooks`.
    ///
    /// This is deliberately NOT called from [`Engine::run_turn`].
    /// `SessionStart` is a session-level event ("runs once before the
    /// turn"), but `run_turn` is per-turn and a REPL or fleet worker calls
    /// it many times per session — firing it inside would re-run session
    /// setup on every turn. Prompt assembly is the caller's concern anyway
    /// (`run_turn` takes history by `&mut` and never owns the system
    /// prompt), so the caller invokes this once, before the first turn, and
    /// folds the returned context into the system message it builds.
    /// Returns `None` when no hooks are attached or the hooks printed
    /// nothing.
    pub async fn run_session_start_hooks(&self) -> Option<String> {
        let handle = self.hooks?;
        let outcome = run_hooks(
            handle.runner,
            Some(handle.hooks),
            &HookPayload::session_start(self.config.cwd.clone()),
        )
        .await;
        if outcome.output.is_empty() {
            None
        } else {
            Some(outcome.output)
        }
    }

    /// Drive one turn to completion or a clean abort, appending every
    /// message to `messages` and streaming an `AgentEvent` for every
    /// boundary over `events`. `budget` is `&mut` because spend
    /// accumulates across the turn (and, via `BudgetGuard::begin_turn`,
    /// across turns in the same session — the caller decides when to reset
    /// it, `run_turn` only reads and records).
    ///
    /// Every step is the same fixed phase sequence, one sub-method per
    /// phase: compaction, loop detection, the between-steps budget check,
    /// the model call (with retry+backoff), bookkeeping for the committed
    /// call, then dispatch — complete the turn or execute its tool calls.
    pub async fn run_turn(
        &self,
        messages: &mut Vec<CompletionMessage>,
        budget: &mut BudgetGuard,
        events: &UnboundedSender<AgentEvent>,
    ) -> TurnOutcome {
        let _ = events.send(AgentEvent::Stage {
            name: StageKind::Execute,
        });

        let mut total_cost_usd = 0.0f64;
        // The model string of the last committed step, for reading the
        // per-model drift correction. `None` until the first result lands —
        // `CalibrationMap::factor` then falls back to the session's single
        // seeded entry.
        let mut calibration_model: Option<String> = None;

        for step in 0..self.config.max_steps {
            self.run_compaction_pass(messages, calibration_model.as_deref(), events);

            if let Some(aborted) = self.check_loop_detection(messages, events) {
                return aborted;
            }
            if let Some(aborted) = check_budget(budget, events) {
                return aborted;
            }

            let committed = match self.run_model_call(messages, events).await {
                Ok(committed) => committed,
                Err(aborted) => return aborted,
            };
            calibration_model = Some(committed.result.model.clone());
            total_cost_usd += committed.result.cost_usd;

            if let Some(aborted) =
                self.handle_committed_result(step, &committed, budget, messages, events)
            {
                return aborted;
            }

            if let Some(completed) = self
                .dispatch_completion(committed, total_cost_usd, messages, events)
                .await
            {
                return completed;
            }
        }

        let reason = format!(
            "reached the step cap ({}) without completing — this is the belt-and-suspenders \
             backstop; loop detection should normally catch a stuck turn first",
            self.config.max_steps
        );
        let _ = events.send(AgentEvent::Error {
            message: reason.clone(),
            retryable: false,
        });
        TurnOutcome::Aborted { reason }
    }

    /// Compaction, before every model call, per the running estimate
    /// (L-E3 dedup+evict, stable system prefix — the system message is
    /// index 0 and `compact()` never touches it).
    ///
    /// Drift correction enters here: `compact` compares the RAW estimate
    /// against the budget it is given, so dividing the configured budget
    /// by the correction factor is exactly comparing the CALIBRATED
    /// estimate (raw × factor) against the configured budget — including
    /// the eviction loop's stopping condition — without threading a factor
    /// through compaction's incremental bookkeeping. A factor > 1 (we
    /// under-estimate this model's tokenizer) shrinks the effective budget
    /// and compacts earlier; the factor's clamp (`crate::estimator`)
    /// bounds how far either way a noisy sample can move this.
    fn run_compaction_pass(
        &self,
        messages: &mut [CompletionMessage],
        calibration_model: Option<&str>,
        events: &UnboundedSender<AgentEvent>,
    ) {
        let compaction_budget = match self.calibration {
            Some(calibration) => {
                (self.config.compaction_budget_tokens as f64
                    / calibration.factor(calibration_model)) as u64
            }
            None => self.config.compaction_budget_tokens,
        };
        if let Some(report) = compact(messages, compaction_budget) {
            let _ = events.send(AgentEvent::Compaction {
                before_tokens: report.before_tokens,
                after_tokens: report.after_tokens,
                evicted: report.evicted,
                deduped: report.deduped,
            });
        }
    }

    /// Loop detection, before spending a model call on a step that's
    /// already stuck. `Some` is the turn's clean abort.
    fn check_loop_detection(
        &self,
        messages: &[CompletionMessage],
        events: &UnboundedSender<AgentEvent>,
    ) -> Option<TurnOutcome> {
        let recent_calls = recent_tool_calls(messages);
        let verdict = detect_loop(&recent_calls, self.config.loop_detection);
        if !verdict.is_loop() {
            return None;
        }
        let reason = verdict
            .evidence()
            .unwrap_or_else(|| "loop detected".to_string());
        let _ = events.send(AgentEvent::Error {
            message: reason.clone(),
            retryable: false,
        });
        Some(TurnOutcome::Aborted {
            reason: format!("stuck-loop detected: {reason}"),
        })
    }

    /// One model call with retry+backoff (`crate::retry`). On commit,
    /// flushes the step's deferred `Retry` events (module docs, L-E10) and
    /// returns the result bundled with the request-time snapshots the
    /// later phases consume; on exhausted retries, emits the terminal
    /// error and returns the turn's clean abort.
    ///
    /// The estimate captured here is the raw (uncalibrated) estimate of
    /// exactly what this step sends — recorded against the provider's
    /// reported usage by [`Engine::handle_committed_result`]. Raw, not
    /// calibrated: the drift ratio is actual/raw, and recording a
    /// corrected estimate would compound corrections on every feedback
    /// pass.
    async fn run_model_call(
        &self,
        messages: &[CompletionMessage],
        events: &UnboundedSender<AgentEvent>,
    ) -> Result<CommittedStep, TurnOutcome> {
        let tools_schema = self.tools.schemas();
        let read_only_tools: HashSet<String> = tools_schema
            .iter()
            .filter(|s| s.read_only)
            .map(|s| s.name.clone())
            .collect();
        let estimated_input_tokens = estimate_conversation_tokens(messages);
        let messages_snapshot = messages.to_vec();
        let req_config = &self.config;
        let attempt: RetryAttemptFn = Box::new(move || {
            let req = CompletionRequest {
                messages: messages_snapshot.clone(),
                max_output_tokens: req_config.max_output_tokens,
                temperature: req_config.temperature,
                effort: req_config.effort,
                tools: tools_schema.clone(),
            };
            Box::pin(self.provider.complete(req))
        });

        let call_started = std::time::Instant::now();
        let outcome = retry_with_backoff(&self.config.retry_policy, self.sleeper, attempt).await;
        let call_duration_ms = call_started.elapsed().as_millis() as u64;

        let RetryOutcome {
            value: result,
            retries,
            ..
        } = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let message = error.to_string();
                let _ = events.send(AgentEvent::Error {
                    message: message.clone(),
                    retryable: error.is_retryable(),
                });
                return Err(TurnOutcome::Aborted {
                    reason: format!("model call failed: {message}"),
                });
            }
        };

        // Deferred-flush: these `Retry` events only reach the wire now
        // that the step has actually committed (see module docs).
        for attempt in &retries {
            let _ = events.send(AgentEvent::Retry {
                attempt: attempt.attempt,
                reason: attempt.reason.clone(),
            });
        }

        Ok(CommittedStep {
            result,
            read_only_tools,
            estimated_input_tokens,
            retries: retries.len() as u32,
            duration_ms: call_duration_ms,
        })
    }

    /// Bookkeeping for the call that just committed: drift feedback into
    /// the attached calibration, exactly one `StepUsage` metering record
    /// per landed step, and budget accounting. `Some` is the turn's clean
    /// abort — this call's spend pushed the turn over an enforced limit —
    /// issued only after delivering what was already paid for (see body);
    /// never a mid-tool kill.
    fn handle_committed_result(
        &self,
        step: usize,
        committed: &CommittedStep,
        budget: &mut BudgetGuard,
        messages: &mut Vec<CompletionMessage>,
        events: &UnboundedSender<AgentEvent>,
    ) -> Option<TurnOutcome> {
        let result = &committed.result;

        // Drift feedback: the provider's reported input tokens (total,
        // cached included — cached tokens were still real prompt tokens)
        // against the raw estimate, keyed by the model that actually
        // served the call. `record` ignores zero-sided pairs, so a
        // provider omitting usage never poisons the state.
        if let Some(calibration) = self.calibration {
            calibration.record(
                &result.model,
                committed.estimated_input_tokens,
                result.usage.input_tokens,
            );
        }

        let _ = events.send(AgentEvent::StepUsage {
            step,
            model: result.model.clone(),
            input_tokens: result.usage.input_tokens,
            output_tokens: result.usage.output_tokens,
            cached_input_tokens: result.usage.cached_input_tokens,
            cache_write_tokens: result.usage.cache_write_tokens,
            estimated_input_tokens: committed.estimated_input_tokens,
            cost_usd: result.cost_usd,
            duration_ms: committed.duration_ms,
            retries: committed.retries,
            tool_calls: result.tool_calls.len(),
        });

        let outcome = budget.record_spend(result.cost_usd);
        let _ = events.send(AgentEvent::BudgetTick {
            spent_usd: budget.spent_usd(),
            limit_usd: budget.turn_limit_usd(),
            mode: budget.mode(),
        });
        let BudgetOutcome::AbortTurn {
            spent_usd,
            limit_usd,
            ..
        } = outcome
        else {
            return None;
        };

        // The call that just landed is the one that pushed spend over the
        // limit — it already committed (its result is real, its cost
        // already happened), so deliver what was paid for: emit its text
        // and append it to history, THEN abort before dispatching
        // anything further (its tool calls, if any, never run — recorded
        // so the transcript shows what was cut). Still not a mid-tool
        // kill.
        if !result.text.is_empty() {
            let _ = events.send(AgentEvent::Text {
                delta: result.text.clone(),
            });
        }
        messages.push(CompletionMessage {
            role: MessageRole::Assistant,
            content: result.text.clone(),
            tool_calls: result.tool_calls.clone(),
            tool_results: Vec::new(),
        });
        // The assistant message above may carry `tool_calls` that never
        // ran (we abort before dispatching them). A recorded `tool_use`
        // with no matching `tool_result` is a broken history: when a
        // REPL caller reuses this `messages` vec, the next turn's first
        // provider call is hard-rejected ("tool_use must be followed by
        // tool_result"). Close the pairing with a synthetic error
        // result per un-run call so resumption stays valid.
        if !result.tool_calls.is_empty() {
            let tool_results = result
                .tool_calls
                .iter()
                .map(|call| ToolResult {
                    call_id: call.call_id.clone(),
                    output: ToolOutput::Error {
                        message: "not executed — turn aborted on budget".to_string(),
                    },
                })
                .collect();
            messages.push(CompletionMessage {
                role: MessageRole::Tool,
                content: String::new(),
                tool_calls: Vec::new(),
                tool_results,
            });
        }
        let reason = format!(
            "budget exceeded after this call: spent ${spent_usd:.4} against a ${limit_usd:.2} limit"
        );
        let _ = events.send(AgentEvent::Error {
            message: reason.clone(),
            retryable: false,
        });
        Some(TurnOutcome::Aborted { reason })
    }

    /// Deliver a committed step's result: emit its text, then either
    /// finish the turn (no tool calls — `Some(Completed)`) or record the
    /// assistant message, execute its tool calls, record their results,
    /// and return `None` so the loop takes another step. Consumes the
    /// step: the result's text moves into the `Completed` outcome.
    async fn dispatch_completion(
        &self,
        committed: CommittedStep,
        total_cost_usd: f64,
        messages: &mut Vec<CompletionMessage>,
        events: &UnboundedSender<AgentEvent>,
    ) -> Option<TurnOutcome> {
        let CommittedStep {
            result,
            read_only_tools,
            ..
        } = committed;

        if !result.text.is_empty() {
            let _ = events.send(AgentEvent::Text {
                delta: result.text.clone(),
            });
        }

        if result.tool_calls.is_empty() {
            // A turn that produced neither a tool call NOR any visible text is
            // never a real completion: the model was cut off at its output
            // limit (usually mid-reasoning) or returned nothing at all.
            // Recording it as `Completed` shows the user a silent, blank turn
            // (the "turn ends with no feedback" defect). Surface why and abort
            // cleanly so the caller can retry instead of swallowing it.
            if result.text.trim().is_empty() {
                let reason = match result.finish_reason {
                    Some(FinishReason::Length) => format!(
                        "The model reached its output-token limit ({} tokens) before producing \
                         any visible response — its budget was likely spent on reasoning. Retry, \
                         raise the output cap, or run /compact to shrink the context.",
                        result.usage.output_tokens
                    ),
                    _ => format!(
                        "The model returned an empty response this turn — no text and no tool \
                         call ({} output tokens). Retry, or switch to a different model.",
                        result.usage.output_tokens
                    ),
                };
                let _ = events.send(AgentEvent::Error {
                    message: reason.clone(),
                    retryable: true,
                });
                return Some(TurnOutcome::Aborted { reason });
            }
            // A non-empty answer that was still truncated at the limit: keep the
            // partial answer (already emitted above) but tell the user it was
            // cut off, so a mid-thought stop is never mistaken for a full one.
            if result.finish_reason == Some(FinishReason::Length) {
                let _ = events.send(AgentEvent::Text {
                    delta: format!(
                        "\n\n⚠ Response was truncated at the output-token limit ({} tokens); \
                         ask to continue if it was cut off mid-thought.",
                        result.usage.output_tokens
                    ),
                });
            }
            messages.push(CompletionMessage {
                role: MessageRole::Assistant,
                content: result.text.clone(),
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
            });
            let _ = events.send(AgentEvent::Stage {
                name: StageKind::Complete,
            });
            let _ = events.send(AgentEvent::Complete {
                model: result.model.clone(),
                cost_usd: total_cost_usd,
            });
            return Some(TurnOutcome::Completed {
                text: result.text,
                cost_usd: total_cost_usd,
            });
        }

        messages.push(CompletionMessage {
            role: MessageRole::Assistant,
            content: result.text.clone(),
            tool_calls: result.tool_calls.clone(),
            tool_results: Vec::new(),
        });

        let tool_results = self
            .execute_tool_calls(&result.tool_calls, &read_only_tools, events)
            .await;

        messages.push(CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: Vec::new(),
            tool_results,
        });

        None
    }

    /// Execute one step's tool calls, preserving sequential semantics for
    /// anything that can mutate: consecutive read-only calls (per
    /// `ToolSchema::read_only`) form a group executed concurrently (capped
    /// at [`MAX_CONCURRENT_TOOL_CALLS`]); every mutating call is its own
    /// barrier, executed alone, in call order. So `[read, read, edit,
    /// read]` runs the two reads in parallel, then the edit alone, then the
    /// final read — an observer of any *mutable* state cannot distinguish
    /// this schedule from fully-sequential execution, while the common
    /// "read five files" step gets real concurrency.
    ///
    /// `ToolStart` fires when a call actually starts; `ToolResult` fires as
    /// each call completes (so results from one parallel group may
    /// interleave — consumers correlate by `call_id`, which the TUI already
    /// does). The returned `Vec<ToolResult>` is always in original call
    /// order, so message history is deterministic regardless of completion
    /// order.
    async fn execute_tool_calls(
        &self,
        calls: &[ToolCall],
        read_only_tools: &HashSet<String>,
        events: &UnboundedSender<AgentEvent>,
    ) -> Vec<ToolResult> {
        let mut indexed: Vec<(usize, ToolResult)> = Vec::with_capacity(calls.len());
        let mut i = 0;
        while i < calls.len() {
            let group_end = if read_only_tools.contains(&calls[i].name) {
                let mut end = i + 1;
                while end < calls.len() && read_only_tools.contains(&calls[end].name) {
                    end += 1;
                }
                end
            } else {
                i + 1
            };

            // Plain copy for the closures: borrowing the loop variable
            // itself would conflict with advancing it below (E0506).
            let group_start = i;
            let group_futures =
                calls[group_start..group_end]
                    .iter()
                    .enumerate()
                    .map(|(offset, call)| {
                        let _ = events.send(AgentEvent::ToolStart { call: call.clone() });
                        let index = group_start + offset;
                        async move {
                            let started = std::time::Instant::now();
                            let output = self.execute_with_repair(call).await;
                            (index, call, output, started.elapsed().as_millis() as u64)
                        }
                    });
            let mut in_flight = futures_util::stream::iter(group_futures)
                .buffer_unordered(MAX_CONCURRENT_TOOL_CALLS);
            while let Some((index, call, output, duration_ms)) = in_flight.next().await {
                let _ = events.send(AgentEvent::ToolResult {
                    call_id: call.call_id.clone(),
                    output: output.clone(),
                    duration_ms,
                });
                indexed.push((
                    index,
                    ToolResult {
                        call_id: call.call_id.clone(),
                        output,
                    },
                ));
            }
            drop(in_flight);

            i = group_end;
        }
        indexed.sort_by_key(|(index, _)| *index);
        indexed.into_iter().map(|(_, result)| result).collect()
    }

    /// Execute one tool call, first checking for the malformed-input
    /// sentinel every adapter's stream aggregator falls back to (see module
    /// docs) rather than handing a tool `Null` and getting back a confusing
    /// tool-specific error.
    ///
    /// The malformed-input check comes *before* any hook fires: a `Null`
    /// call is the model's own broken JSON, structurally short-circuited —
    /// it never reaches the executor, so it is not a real tool invocation
    /// and no `PreToolUse`/`PostToolUse` hook is fired for it. When no hooks
    /// are attached this is exactly the previous body:
    /// `self.tools.execute(...)`.
    async fn execute_with_repair(&self, call: &ToolCall) -> ToolOutput {
        if call.input.is_null() {
            return ToolOutput::Error {
                message: format!(
                    "malformed tool call: `{}`'s arguments were not valid JSON (the model's \
                     streamed output didn't parse) — retry this call with well-formed JSON \
                     arguments",
                    call.name
                ),
            };
        }
        match self.hooks {
            None => self.tools.execute(&call.name, &call.input).await,
            Some(handle) => self.execute_with_hooks(handle, call).await,
        }
    }

    /// Wrap a single (well-formed) executor invocation in its `PreToolUse` /
    /// `PostToolUse` hooks. Only reached when hooks are attached.
    ///
    /// `PreToolUse` fires first: if it blocks (a hook exited non-zero, or
    /// failed to even run — per `crate::hooks`'s contract), the tool is NOT
    /// executed and the model instead sees a `ToolOutput::Error` naming the
    /// block, exactly as the engine surfaces every other tool failure as
    /// model-visible data rather than an engine error. Otherwise the tool
    /// runs and `PostToolUse` fires as a pure observation — its outcome is
    /// discarded (it can never block or alter the result), so a failing
    /// post-hook cannot abort the turn.
    async fn execute_with_hooks(&self, handle: HooksHandle<'a>, call: &ToolCall) -> ToolOutput {
        let pre = run_hooks(
            handle.runner,
            Some(handle.hooks),
            &HookPayload::pre_tool_use(self.config.cwd.clone(), &call.name, call.input.clone()),
        )
        .await;
        if pre.blocked {
            let message = match pre.reason {
                Some(reason) => format!(
                    "tool `{}` was blocked by a PreToolUse hook: {reason}",
                    call.name
                ),
                None => format!("tool `{}` was blocked by a PreToolUse hook", call.name),
            };
            return ToolOutput::Error { message };
        }

        let output = self.tools.execute(&call.name, &call.input).await;

        let result_str = match &output {
            ToolOutput::Ok { content } => content.clone(),
            ToolOutput::Error { message } => message.clone(),
        };
        // Observation only — the outcome is intentionally ignored so a
        // non-zero PostToolUse exit never blocks or rewrites the result.
        let _ = run_hooks(
            handle.runner,
            Some(handle.hooks),
            &HookPayload::post_tool_use(
                self.config.cwd.clone(),
                &call.name,
                call.input.clone(),
                result_str,
            ),
        )
        .await;

        output
    }
}

/// The boxed-future shape `retry_with_backoff` needs from its `attempt_fn`
/// — named here purely to keep the call site in `run_turn` readable.
type RetryAttemptFn<'a> = Box<
    dyn FnMut() -> Pin<Box<dyn Future<Output = Result<CompletionResultAlias, ProviderError>> + 'a>>
        + 'a,
>;
type CompletionResultAlias = stella_protocol::CompletionResult;

/// The between-steps budget check (never mid-tool — see module docs).
/// `Some` is the turn's clean abort. A free function like
/// [`recent_tool_calls`] because it reads no engine state; the wording
/// differs from the post-call abort in `Engine::handle_committed_result`
/// because here nothing new was spent.
fn check_budget(budget: &BudgetGuard, events: &UnboundedSender<AgentEvent>) -> Option<TurnOutcome> {
    let BudgetOutcome::AbortTurn {
        spent_usd,
        limit_usd,
        ..
    } = budget.evaluate()
    else {
        return None;
    };
    let reason = format!("budget exceeded: spent ${spent_usd:.4} against a ${limit_usd:.2} limit");
    let _ = events.send(AgentEvent::Error {
        message: reason.clone(),
        retryable: false,
    });
    Some(TurnOutcome::Aborted { reason })
}

/// Flatten the tool calls of the CURRENT turn — assistant messages after
/// the last user message — in chronological order, for
/// `crate::loop_detect::detect_loop`. Windowing at the user boundary
/// matters: identical calls across turns are the user re-asking a
/// question, not a stuck loop (a REPL session asking the same thing three
/// times would otherwise trip the exact-repeat detector), and it keeps
/// this per-step scan O(turn) instead of O(entire history).
fn recent_tool_calls(messages: &[CompletionMessage]) -> Vec<ToolCall> {
    let turn_start = messages
        .iter()
        .rposition(|m| m.role == MessageRole::User)
        .map(|i| i + 1)
        .unwrap_or(0);
    messages[turn_start..]
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .flat_map(|m| m.tool_calls.iter().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
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
            ProviderError::UnknownModel { slug } => {
                ProviderError::UnknownModel { slug: slug.clone() }
            }
            ProviderError::Malformed(m) => ProviderError::Malformed(m.clone()),
            ProviderError::Cancelled => ProviderError::Cancelled,
            ProviderError::Terminal(m) => ProviderError::Terminal(m.clone()),
        }
    }

    fn text_result(text: &str) -> CompletionResultAlias {
        CompletionResultAlias {
            text: text.into(),
            tool_calls: vec![],
            usage: CompletionUsage::default(),
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
            usage: CompletionUsage::default(),
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

    fn empty_result(finish_reason: Option<FinishReason>) -> CompletionResultAlias {
        CompletionResultAlias {
            text: String::new(),
            tool_calls: vec![],
            usage: CompletionUsage {
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
        // Every call returns the identical tool call — well past the
        // default exact-repeat threshold (3) — so loop detection must
        // abort long before EngineConfig::default()'s 200-step cap.
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
            TurnOutcome::Aborted { reason } => assert!(reason.contains("stuck-loop")),
            other => panic!("expected a stuck-loop abort, got {other:?}"),
        }
        // Well under the 200-step cap — loop detection caught it early.
        assert!(tool_calls.load(Ordering::SeqCst) < 10);

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
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
            TurnOutcome::Aborted { reason } => assert!(reason.contains("budget")),
            other => panic!("expected a budget abort, got {other:?}"),
        }
        let events = drain_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::BudgetTick { .. }))
        );
    }

    /// Exit criterion ( Phase 2): "synthetic 200-step turn
    /// (scripted provider incl. 429s, stream drop, context pressure)
    /// survives across three dialects (GLM 5.2, Anthropic, OpenAI
    /// shapes)". "Dialect" at this layer (`stella-core`, which never
    /// touches HTTP/SSE — that's `stella-model`'s job, tested there) means
    /// varying provider *behavior*: call-id conventions, injected 429s
    /// (`RateLimited`), injected transport drops, and steadily growing tool
    /// output that forces repeated compaction — the shapes a real
    /// GLM/Anthropic/OpenAI backend can actually produce at this seam.
    async fn run_synthetic_survival_turn(
        dialect: &str,
        id_style: fn(u32) -> String,
    ) -> TurnOutcome {
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
                usage: CompletionUsage::default(),
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
            usage: CompletionUsage::default(),
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
                // Step 0 commits only after one retryable failure — its
                // StepUsage must say retries: 1.
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
                input_tokens: 4_000,
                output_tokens: 50,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
            };
            // Vary each call's input: `tool_call_result` reuses one command,
            // and three byte-identical bash calls are exactly what
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
}
