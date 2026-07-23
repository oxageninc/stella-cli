//! The step-driver: `Engine::run_turn`. One
//! model call per step, message accumulation, `AgentEvent` emission at
//! every boundary, retry+backoff, compaction, tool-output budget checks,
//! loop detection, and (a first, structural cut of) malformed-call repair —
//! wiring together every other module in this crate.
//!
//! `Engine` drives through `&dyn Provider` (`stella_protocol`) and
//! `&dyn ToolExecutor` (`crate::ports`) — no adapter-specific code and no
//! direct filesystem access live here. Everything
//! *inside* one step (compaction, loop detection, budget evaluation) is the
//! plain synchronous logic from the other modules in this crate; `run_turn`
//! is the one place that sequences them against real I/O.
//!
//! # Deferred-flush events (L-E10)
//!
//! [`crate::retry::retry_with_backoff_observed`] returns committed retry
//! history while synchronously exposing each failed provider attempt to the
//! accounting path. Ordinary retry narration stays deferred until success;
//! content-free `UsageIncomplete` envelopes are durable immediately because
//! a later successful attempt cannot recover the failed call's usage. A
//! caller-side hard cancel that drops the turn while an attempt is still in
//! flight emits one `Cancelled` envelope from a drop guard
//! ([`CancelUsageGuard`]) armed for exactly that window.
//!
//! # Retry never re-executes a mutating tool call
//!
//! [`crate::retry::retry_with_backoff_observed`] wraps the model call
//! (`Provider::complete_observed`) together with that attempt's speculation
//! pump (`crate::speculation`): read-only calls announced by the stream may
//! execute inside a failed attempt and execute again when the retry
//! re-announces them — discarded work, safe precisely because they are
//! read-only. MUTATING tool execution happens exactly once, after a model
//! call has already succeeded and returned tool calls to run — it is never
//! inside the retried closure. A retried step therefore structurally
//! cannot re-execute a non-idempotent (mutating) tool call; see the
//! property test `retry_never_re_executes_a_tool_call` below, which proves
//! it by counting real executions against a flaky scripted provider.
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

use crate::budget::{BudgetGuard, BudgetOutcome};
use crate::compaction::compact;
use crate::estimator::{CalibrationMap, estimate_conversation_tokens};
use crate::event_sender::EventSender;
use crate::hooks::{HookPayload, HookRunner, Hooks, run_hooks};
use crate::loop_detect::{LoopDetectionConfig, detect_loop};
use crate::ports::ToolExecutor;
use crate::retry::{RetryOutcome, RetryPolicy, Sleeper, retry_with_backoff_observed};
use crate::speculation::{SpeculationGate, SpeculationPool, SpeculativeResult};
use crate::{AccountedCall, AccountedCallError, run_accounted_call};
use tokio::sync::mpsc::UnboundedSender;

mod settlement;
use settlement::{check_budget, record_settled_cost};

/// Everything about a turn's execution that isn't the provider/tools
/// themselves: prompt shape, retry/compaction/loop tuning, and hard
/// backstops. `Default` gives sensible starting values for `stella-cli`.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub effort: Option<ReasoningEffort>,
    /// Thinking-mode enable/disable forwarded to every completion —
    /// `CompletionRequest::reasoning` semantics (`None` = provider default).
    pub reasoning: Option<bool>,
    /// Sampling/routing overrides forwarded to every completion —
    /// `CompletionRequest::params` semantics (each adapter forwards the
    /// subset its dialect supports).
    pub params: Option<stella_protocol::GenerationParams>,
    pub retry_policy: RetryPolicy,
    pub loop_detection: LoopDetectionConfig,
    /// Compaction fires once the estimated conversation size exceeds this
    /// many tokens (`crate::estimator`). When calibration is attached
    /// ([`Engine::with_calibration`]) the comparison uses the
    /// drift-corrected estimate, so this budget is honored in the model's
    /// own observed tokens rather than raw heuristic tokens.
    pub compaction_budget_tokens: u64,
    /// When eviction/dedup/aging alone cannot reach the compaction budget
    /// (the oversized content is protected user/assistant text, or already
    /// stubbed), replace the oldest span of the conversation with a
    /// model-written summary instead of letting the next call overflow the
    /// provider's context window. Costs one cheap completion, metered into
    /// the same [`BudgetGuard`] as every other call.
    pub summarize_overflow: bool,
    /// Messages at the conversation tail the summarizer never touches —
    /// the recent work the model is actively reasoning over.
    pub summarize_keep_recent: usize,
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
            reasoning: None,
            params: None,
            retry_policy: RetryPolicy::standard(),
            loop_detection: LoopDetectionConfig::default(),
            compaction_budget_tokens: 150_000,
            summarize_overflow: true,
            summarize_keep_recent: 8,
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
    Aborted { reason: String, cost_usd: f64 },
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
    call_role: stella_protocol::ModelCallRole,
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
    /// Boundary pause gate ([`crate::ports::TurnGate`]), off by default.
    /// Attached via [`Engine::with_gate`]; consulted once per step, before
    /// any model call — a paused turn parks at that safe boundary and
    /// spends nothing until resumed. `None` adds zero work.
    gate: Option<&'a dyn crate::ports::TurnGate>,
    /// Step-boundary steering ([`crate::ports::TurnSteering`]), off by
    /// default. Attached via [`Engine::with_steering`]; drained once per
    /// step at the same boundary as the pause gate — queued user messages
    /// become the model's next observation, and a latched soft stop ends
    /// the turn keeping every completed step. `None` adds zero work.
    steering: Option<&'a dyn crate::ports::TurnSteering>,
}

/// Upper bound on tool calls from one step executing concurrently. Tools
/// are I/O-bound (process spawns, file reads), so this caps descriptor and
/// process pressure, not CPU.
const MAX_CONCURRENT_TOOL_CALLS: usize = 8;

/// One committed model call plus the step-scoped context the phases after
/// it consume: the pre-call raw token estimate (drift feedback + telemetry
/// — raw, never calibrated, see [`Engine::run_model_call`]) and the
/// read-only tool set for dispatch scheduling. The step's `StepUsage`
/// metering record (retry and duration figures included) was already
/// emitted by [`Engine::run_model_call`] at the no-await settlement
/// boundary — it is deliberately NOT carried here.
struct CommittedStep {
    result: CompletionResultAlias,
    budget_outcome: BudgetOutcome,
    /// Names of tools whose schemas declare `read_only`, snapshotted from
    /// the same `schemas()` call the request itself was built from.
    read_only_tools: HashSet<String>,
    /// Read-only calls executed speculatively while THIS committed
    /// attempt's response was still streaming (`crate::speculation`).
    /// Dispatch harvests matching entries instead of re-executing; a failed
    /// attempt's pool never gets here — it is dropped with the attempt.
    speculation: SpeculationPool,
    estimated_input_tokens: u64,
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
            call_role: stella_protocol::ModelCallRole::Worker,
            hooks: None,
            calibration: None,
            gate: None,
            steering: None,
        }
    }

    /// Attribute this engine's provider calls to a concrete pipeline role.
    /// Ordinary execution defaults to [`ModelCallRole::Worker`].
    pub fn with_call_role(mut self, role: stella_protocol::ModelCallRole) -> Self {
        self.call_role = role;
        self
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

    /// Attach a boundary pause gate — Pause/Resume at step granularity,
    /// never mid-tool.
    pub fn with_gate(mut self, gate: &'a dyn crate::ports::TurnGate) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Attach step-boundary steering — mid-turn user messages and the soft
    /// stop, at step granularity, never mid-tool.
    pub fn with_steering(mut self, steering: &'a dyn crate::ports::TurnSteering) -> Self {
        self.steering = Some(steering);
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
        let events = EventSender::new(events.clone());
        self.run_turn_with_sender(messages, budget, &events).await
    }

    /// [`Self::run_turn`] with a caller-supplied ordered event boundary.
    /// Existing callers use an ordinary Tokio sender; benchmark callers use
    /// this form so append+flush completes synchronously before a paid-call
    /// producer can advance to another request.
    pub async fn run_turn_with_sender(
        &self,
        messages: &mut Vec<CompletionMessage>,
        budget: &mut BudgetGuard,
        events: &EventSender,
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
            // Pause parks HERE — after the previous step fully settled and
            // before any new model call, mirroring the budget-abort
            // boundary. Resuming continues the very same turn.
            if let Some(gate) = self.gate {
                gate.wait_if_paused().await;
            }
            // Steering rides the same safe boundary as the pause gate:
            // queued user messages land BEFORE compaction (so the pass sees
            // them) and before the model call (so it answers them this
            // step). Drain precedes the soft-stop check deliberately — a
            // steer typed just before Esc is preserved in history for the
            // next turn instead of evaporating with the per-turn tap.
            if let Some(steering) = self.steering {
                for text in steering.drain_steering() {
                    let _ = events.send(AgentEvent::Steered { text: text.clone() });
                    messages.push(CompletionMessage::user(text));
                }
                if steering.soft_stop_requested() {
                    // A user choice, not a failure: no Error event, and the
                    // caller keeps every completed step (unlike the hard
                    // cancel, which drops the future and truncates).
                    return TurnOutcome::Aborted {
                        reason: SOFT_STOP_REASON.to_string(),
                        cost_usd: total_cost_usd,
                    };
                }
            }
            if let Some(aborted) = check_budget(budget, total_cost_usd, events) {
                return aborted;
            }
            total_cost_usd += self
                .run_compaction_pass(messages, calibration_model.as_deref(), budget, events)
                .await;

            if let Some(aborted) = self.check_loop_detection(messages, total_cost_usd, events) {
                return aborted;
            }
            if let Some(aborted) = check_budget(budget, total_cost_usd, events) {
                return aborted;
            }

            let committed = match self.run_model_call(step, messages, budget, events).await {
                Ok(committed) => committed,
                Err(reason) => {
                    return TurnOutcome::Aborted {
                        reason,
                        cost_usd: total_cost_usd,
                    };
                }
            };
            calibration_model = Some(committed.result.model.clone());
            total_cost_usd += committed.result.cost_usd;

            if let Some(aborted) =
                self.handle_committed_result(step, &committed, total_cost_usd, messages, events)
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
        TurnOutcome::Aborted {
            reason,
            cost_usd: total_cost_usd,
        }
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
    ///
    /// Returns the summarizer's spend (0.0 on the overwhelmingly common
    /// no-summarization path) so `run_turn` folds it into the turn total.
    async fn run_compaction_pass(
        &self,
        messages: &mut Vec<CompletionMessage>,
        calibration_model: Option<&str>,
        budget: &mut BudgetGuard,
        events: &EventSender,
    ) -> f64 {
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
                superseded: report.superseded,
                aged: report.aged,
                summarized: 0,
            });
        }
        // Overflow fallback: still over budget after every pure pass means
        // the weight is in PROTECTED content (user/assistant text, the
        // latest tool result) — without this, the next provider call
        // eventually hard-fails on context overflow.
        if self.config.summarize_overflow
            && crate::estimator::estimate_conversation_tokens(messages) > compaction_budget
        {
            return self.summarize_overflow_span(messages, budget, events).await;
        }
        0.0
    }

    /// Replace the oldest viable span with a model-written summary. Failures
    /// leave the conversation untouched. Returns the summarizer call's spend.
    async fn summarize_overflow_span(
        &self,
        messages: &mut Vec<CompletionMessage>,
        budget: &mut BudgetGuard,
        events: &EventSender,
    ) -> f64 {
        let before_tokens = crate::estimator::estimate_conversation_tokens(messages);
        // Span start: after the system prompt and the FIRST user message —
        // the task statement anchors every later step and must survive
        // verbatim. A Tool message can't open the kept tail either side of
        // the span (its assistant partner would be summarized away and the
        // provider rejects orphaned tool results), so both bounds walk off
        // Tool messages.
        let first_user = messages
            .iter()
            .position(|m| m.role == MessageRole::User)
            .unwrap_or(0);
        let mut start = first_user + 1;
        while start < messages.len() && messages[start].role == MessageRole::Tool {
            start += 1;
        }
        let mut end = messages
            .len()
            .saturating_sub(self.config.summarize_keep_recent);
        // `end == messages.len()` (keep_recent 0) has no kept tail to walk
        // off — indexing it would be out of bounds, not a Tool message.
        while end > start && end < messages.len() && messages[end].role == MessageRole::Tool {
            end -= 1;
        }
        // A tiny span isn't worth a model call — and this guard is also the
        // convergence backstop: once a summary message occupies the span,
        // the next over-budget step finds nothing left to replace and skips
        // instead of summarizing its own summary every step.
        if end <= start || end - start < 4 {
            return 0.0;
        }
        let rendered = crate::summarize::render_span_for_summary(&messages[start..end]);
        let request = CompletionRequest {
            messages: vec![
                CompletionMessage::system(crate::summarize::SUMMARIZE_SYSTEM),
                CompletionMessage::user(&rendered),
            ],
            max_output_tokens: Some(1_200),
            temperature: Some(0.0),
            effort: Some(ReasoningEffort::Low),
            tools: vec![],
            reasoning: None,
            params: None,
        };
        let estimated_input_tokens = estimate_conversation_tokens(&request.messages);
        let result = match run_accounted_call(
            AccountedCall {
                provider: self.provider,
                role: stella_protocol::ModelCallRole::Summarization,
                model_hint: "unknown".into(),
                request,
                retry_policy: RetryPolicy::deterministic(),
                timeout: None,
                estimated_input_tokens,
            },
            budget,
            events,
            self.sleeper,
        )
        .await
        {
            Ok(result) => result,
            Err(AccountedCallError::Budget { result, .. }) => return result.cost_usd,
            Err(AccountedCallError::Provider(_) | AccountedCallError::Timeout) => return 0.0,
        };
        let cost_usd = result.cost_usd;
        if result.text.trim().is_empty() {
            return cost_usd;
        }
        let replaced = end - start;
        let summary = CompletionMessage::user(format!(
            "{SUMMARY_MARKER_PREFIX} to fit context — full detail was compacted away; \
             re-read files or re-run tools for specifics]\n\n{}",
            result.text.trim()
        ));
        messages.splice(start..end, std::iter::once(summary));
        let _ = events.send(AgentEvent::Compaction {
            before_tokens,
            after_tokens: crate::estimator::estimate_conversation_tokens(messages),
            evicted: 0,
            deduped: 0,
            superseded: 0,
            aged: 0,
            summarized: replaced,
        });
        cost_usd
    }

    /// Loop detection, before spending a model call on a step that's
    /// already stuck. `Some` is the turn's clean abort.
    fn check_loop_detection(
        &self,
        messages: &[CompletionMessage],
        total_cost_usd: f64,
        events: &EventSender,
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
            cost_usd: total_cost_usd,
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
        step: usize,
        messages: &[CompletionMessage],
        budget: &mut BudgetGuard,
        events: &EventSender,
    ) -> Result<CommittedStep, String> {
        let tools_schema = self.tools.schemas();
        let read_only_tools: HashSet<String> = tools_schema
            .iter()
            .filter(|s| s.read_only)
            .map(|s| s.name.clone())
            .collect();
        let estimated_input_tokens = estimate_conversation_tokens(messages);
        let messages_snapshot = messages.to_vec();
        let req_config = &self.config;
        let speculation_read_only = read_only_tools.clone();
        // The gate forwards answer fragments as `TextDelta` previews. Deliberately NOT rolled back
        // on a failed attempt: a retry's deltas re-stream from the start
        // with no reset marker — the eventual `Text` event is authoritative
        // and consumers replace the preview with it (protocol docs).
        let delta_events = events.clone();
        // Each attempt runs the provider call and the speculation pump
        // concurrently: the pump executes read-only calls the moment the
        // adapter announces them (`crate::speculation`), so their wall-clock
        // overlaps the stream instead of following it. The gate (and with
        // it the channel's send half) drops when the provider call resolves,
        // which is what lets the pump finish draining. A failed attempt
        // drops its pool with the attempt — read-only work is safe to
        // waste — and the retry builds a fresh channel and pool.
        let attempt: RetryAttemptFn = Box::new(move || {
            let req = CompletionRequest {
                messages: messages_snapshot.clone(),
                max_output_tokens: req_config.max_output_tokens,
                temperature: req_config.temperature,
                effort: req_config.effort,
                reasoning: req_config.reasoning,
                params: req_config.params,
                tools: tools_schema.clone(),
            };
            let read_only = speculation_read_only.clone();
            let delta_tx = delta_events.clone();
            Box::pin(async move {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let mut pump: SpeculationFuture<'_> = Box::pin(self.pump_speculations(rx));
                let mut complete = Box::pin(async move {
                    let gate = SpeculationGate::new(read_only, tx, delta_tx);
                    self.provider.complete_observed(req, &gate).await
                    // `gate` (and its sender) drop here → the pump's
                    // stream ends once in-flight executions drain.
                });
                let result = tokio::select! {
                    result = &mut complete => result,
                    _ = &mut pump => unreachable!("the gate keeps the speculation channel open"),
                };
                drop(complete);
                result.map(|result| (result, pump))
            })
        });

        let call_started = std::time::Instant::now();
        // Armed for exactly the interval where a paid attempt may be in
        // flight: a caller-side hard cancel that drops this future mid-await
        // still leaves one content-free `Cancelled` envelope behind.
        // Disarmed on BOTH normal exits — a success reports through its
        // `StepUsage`, and a terminal failure's attempts already reported
        // through the per-attempt observer below.
        let mut cancel_guard = CancelUsageGuard {
            events: events.clone(),
            role: self.call_role,
            provider: self.provider.id().to_string(),
            started: call_started,
            armed: true,
        };
        let incomplete_events = events.clone();
        let RetryOutcome {
            value: (result, speculation_future),
            retries,
            ..
        } = match retry_with_backoff_observed(
            &self.config.retry_policy,
            self.sleeper,
            attempt,
            // Per-attempt duration (retry.rs times each dispatch
            // individually): the failed call's own latency, never
            // cumulative across earlier attempts or backoff sleeps.
            |attempt, _error, attempt_duration| {
                let _ = incomplete_events.send(AgentEvent::UsageIncomplete {
                    role: self.call_role,
                    provider: self.provider.id().to_string(),
                    model: "unknown".into(),
                    reason: stella_protocol::UsageIncompleteReason::ProviderError,
                    duration_ms: attempt_duration.as_millis() as u64,
                    retries: Some(attempt.saturating_sub(1)),
                });
            },
        )
        .await
        {
            Ok(outcome) => {
                cancel_guard.disarm();
                outcome
            }
            Err(error) => {
                cancel_guard.disarm();
                let message = error.to_string();
                let _ = events.send(AgentEvent::Error {
                    message: message.clone(),
                    retryable: error.is_retryable(),
                });
                return Err(format!("model call failed: {message}"));
            }
        };
        let budget_outcome = record_settled_cost(budget, result.cost_usd, events);
        let call_duration_ms = call_started.elapsed().as_millis() as u64;

        // Deferred-flush: these `Retry` events only reach the wire now
        // that the step has actually committed (see module docs).
        for attempt in &retries {
            let _ = events.send(AgentEvent::Retry {
                attempt: attempt.attempt,
                reason: attempt.reason.clone(),
            });
        }

        // Cost and usage settle at one no-await boundary. Speculative tool
        // work may still be draining; cancellation in that interval must not
        // preserve spend while losing its per-call accounting envelope.
        let _ = events.send(AgentEvent::StepUsage {
            step,
            role: self.call_role,
            provider: self.provider.id().to_string(),
            // The engine's own step already streams its answer as a `Text`
            // event; duplicating it here would double the transcript.
            output_text: None,
            model: result.model.clone(),
            input_tokens: result.usage.input_tokens,
            output_tokens: result.usage.output_tokens,
            cached_input_tokens: result.usage.cached_input_tokens,
            cache_write_tokens: result.usage.cache_write_tokens,
            estimated_input_tokens,
            cost_usd: result.cost_usd,
            duration_ms: call_duration_ms,
            retries: retries.len() as u32,
            tool_calls: result.tool_calls.len(),
            complete: result.usage.is_complete(),
        });
        let speculation = speculation_future.await;

        Ok(CommittedStep {
            result,
            budget_outcome,
            read_only_tools,
            speculation,
            estimated_input_tokens,
        })
    }

    /// Receive announced calls from the [`SpeculationGate`] and execute them
    /// concurrently (same cap as dispatch) while the model call streams,
    /// collecting outputs into the attempt's [`SpeculationPool`]. Runs until
    /// the gate drops the send half AND every in-flight execution finishes —
    /// speculated calls are exactly the calls dispatch would run first, so
    /// draining them is never wasted time on the committed path.
    async fn pump_speculations(
        &self,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<ToolCall>,
    ) -> SpeculationPool {
        let announced = futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx));
        let mut in_flight = announced
            .map(|call| async move {
                let started = std::time::Instant::now();
                let output = self.execute_with_repair(&call, None).await;
                (call, output, started.elapsed().as_millis() as u64)
            })
            .buffer_unordered(MAX_CONCURRENT_TOOL_CALLS);

        let mut pool = SpeculationPool::new();
        while let Some((call, output, duration_ms)) = in_flight.next().await {
            pool.insert(
                call.call_id.clone(),
                SpeculativeResult {
                    name: call.name,
                    input: call.input,
                    output,
                    duration_ms,
                },
            );
        }
        pool
    }

    /// Bookkeeping for the call that just committed: drift feedback into
    /// the attached calibration. Its cost was settled — and its single
    /// `StepUsage` metering record emitted — synchronously at the
    /// provider-success boundary in [`Engine::run_model_call`], before this
    /// method can be reached; the carried outcome decides whether `Some` is
    /// the turn's clean abort. That abort is issued only after delivering
    /// what was already paid for (see body), never as a mid-tool kill.
    fn handle_committed_result(
        &self,
        _step: usize,
        committed: &CommittedStep,
        total_cost_usd: f64,
        messages: &mut Vec<CompletionMessage>,
        events: &EventSender,
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

        let BudgetOutcome::AbortTurn {
            spent_usd,
            limit_usd,
            ..
        } = committed.budget_outcome
        else {
            return None;
        };

        // The call that just landed is the one that pushed spend over the
        // limit — it already committed (its result is real, its cost
        // already happened), so deliver what was paid for: emit its text
        // and append it to history, THEN abort before dispatching
        // anything further (its tool calls, if any, never run — recorded
        // so the transcript shows what was cut). Still not a mid-tool
        // kill. Trimmed guard: whitespace-only text is not a deliverable
        // answer and must not stream a blank `Text` event.
        if !result.text.trim().is_empty() {
            let _ = events.send(AgentEvent::Text {
                delta: result.text.clone(),
            });
        }
        messages.push(CompletionMessage {
            role: MessageRole::Assistant,
            content: result.text.clone(),
            tool_calls: result.tool_calls.clone(),
            tool_results: Vec::new(),
            attachments: Vec::new(),
        });
        // The assistant message above may carry `tool_calls` that never
        // ran (we abort before dispatching them). A recorded `tool_use`
        // with no matching `tool_result` is a broken history: when a
        // REPL caller reuses this `messages` vec, the next turn's first
        // provider call is hard-rejected ("tool_use must be followed by
        // tool_result"). Close the pairing with a synthetic error
        // result per un-run call so resumption stays valid.
        if !result.tool_calls.is_empty() {
            let tool_results: Vec<ToolResult> = result
                .tool_calls
                .iter()
                .map(|call| ToolResult {
                    call_id: call.call_id.clone(),
                    output: ToolOutput::Error {
                        message: "not executed — turn aborted on budget".to_string(),
                    },
                })
                .collect();
            // Mirror the synthetic results onto the event stream: this
            // step's `StepUsage` already reported `tool_calls: N`, and a
            // transcript reconstructed from events must resolve every
            // announced call the same way `messages` does. No `ToolStart`
            // — these calls never ran.
            for tool_result in &tool_results {
                let _ = events.send(AgentEvent::ToolResult {
                    call_id: tool_result.call_id.clone(),
                    output: tool_result.output.clone(),
                    duration_ms: 0,
                    speculated: false,
                });
            }
            messages.push(CompletionMessage {
                role: MessageRole::Tool,
                content: String::new(),
                tool_calls: Vec::new(),
                tool_results,
                attachments: Vec::new(),
            });
        }
        let reason = format!(
            "budget exceeded after this call: spent ${spent_usd:.4} against a ${limit_usd:.2} limit"
        );
        let _ = events.send(AgentEvent::Error {
            message: reason.clone(),
            retryable: false,
        });
        Some(TurnOutcome::Aborted {
            reason,
            cost_usd: total_cost_usd,
        })
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
        events: &EventSender,
    ) -> Option<TurnOutcome> {
        let CommittedStep {
            result,
            read_only_tools,
            speculation,
            ..
        } = committed;

        // Trimmed guard, matching the empty-turn check below: a
        // whitespace-only response must not stream a blank `Text` event and
        // then abort as "no text" — events and history stay consistent.
        if !result.text.trim().is_empty() {
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
                return Some(TurnOutcome::Aborted {
                    reason,
                    cost_usd: total_cost_usd,
                });
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
                attachments: Vec::new(),
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
            attachments: Vec::new(),
        });

        let tool_results = self
            .execute_tool_calls(&result.tool_calls, &read_only_tools, speculation, events)
            .await;

        messages.push(CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: Vec::new(),
            tool_results,
            attachments: Vec::new(),
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
    ///
    /// `speculation` holds this step's speculatively-executed read-only
    /// calls (`crate::speculation`). A call is *harvested* — its recorded
    /// output delivered without re-executing — only when the pool entry
    /// matches the committed call exactly (id, name, AND input); any
    /// mismatch falls through to normal execution and the stale entry is
    /// discarded. Harvested calls emit `ToolStart` immediately followed by
    /// `ToolResult { speculated: true }` carrying the real (overlapped)
    /// execution duration.
    async fn execute_tool_calls(
        &self,
        calls: &[ToolCall],
        read_only_tools: &HashSet<String>,
        mut speculation: SpeculationPool,
        events: &EventSender,
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
            let speculation = &mut speculation;
            let group_futures =
                calls[group_start..group_end]
                    .iter()
                    .enumerate()
                    .map(|(offset, call)| {
                        let _ = events.send(AgentEvent::ToolStart { call: call.clone() });
                        let index = group_start + offset;
                        let harvested = speculation
                            .remove(&call.call_id)
                            .filter(|s| s.name == call.name && s.input == call.input);
                        async move {
                            match harvested {
                                Some(s) => (index, call, s.output, s.duration_ms, true),
                                None => {
                                    let started = std::time::Instant::now();
                                    let output = self.execute_with_repair(call, Some(events)).await;
                                    let duration_ms = started.elapsed().as_millis() as u64;
                                    (index, call, output, duration_ms, false)
                                }
                            }
                        }
                    });
            let mut in_flight = futures_util::stream::iter(group_futures)
                .buffer_unordered(MAX_CONCURRENT_TOOL_CALLS);
            while let Some((index, call, output, duration_ms, speculated)) = in_flight.next().await
            {
                let _ = events.send(AgentEvent::ToolResult {
                    call_id: call.call_id.clone(),
                    output: output.clone(),
                    duration_ms,
                    speculated,
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
    ///
    /// `events` carries non-blocking hook diagnostics (a hook that failed to
    /// spawn, or exited non-zero on an event that cannot block) to the turn
    /// stream as one non-fatal `Error` per call. `None` on the speculative
    /// path: speculation emits no events until harvest, and a failed
    /// attempt's hook noise must not reach the wire with it.
    async fn execute_with_repair(
        &self,
        call: &ToolCall,
        events: Option<&EventSender>,
    ) -> ToolOutput {
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
            Some(handle) => self.execute_with_hooks(handle, call, events).await,
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
    /// runs and `PostToolUse` fires as a pure observation — its outcome can
    /// never block or alter the result, so a failing post-hook cannot abort
    /// the turn. Non-blocking failures from either phase (spawn failures,
    /// non-zero exits on events that cannot block) are no longer discarded:
    /// they surface as one non-fatal `Error { retryable: true }` on the
    /// turn stream when an event channel is present.
    async fn execute_with_hooks(
        &self,
        handle: HooksHandle<'a>,
        call: &ToolCall,
        events: Option<&EventSender>,
    ) -> ToolOutput {
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
        let mut diagnostics = pre.diagnostics;

        let output = self.tools.execute(&call.name, &call.input).await;

        let result_str = match &output {
            ToolOutput::Ok { content } => content.clone(),
            ToolOutput::Error { message } => message.clone(),
        };
        // Observation only — a non-zero PostToolUse exit never blocks or
        // rewrites the result; its failures ride `diagnostics` instead.
        let post = run_hooks(
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
        diagnostics.extend(post.diagnostics);

        if !diagnostics.is_empty()
            && let Some(events) = events
        {
            let _ = events.send(AgentEvent::Error {
                message: format!(
                    "hook problem(s) around tool `{}` (non-blocking): {}",
                    call.name,
                    diagnostics.join("; ")
                ),
                retryable: true,
            });
        }

        output
    }
}

/// The boxed-future shape `retry_with_backoff` needs from its `attempt_fn`
/// — named here purely to keep the call site in `run_turn` readable. Each
/// attempt yields the completion AND its still-live speculation future as
/// one value. The caller settles the billed completion synchronously before
/// awaiting that future, closing the cancellation window without moving the
/// mutable budget ledger into concurrent work.
type RetryAttemptFn<'a> = Box<
    dyn FnMut() -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            (CompletionResultAlias, SpeculationFuture<'a>),
                            ProviderError,
                        >,
                    > + 'a,
            >,
        > + 'a,
>;
type CompletionResultAlias = stella_protocol::CompletionResult;
type SpeculationFuture<'a> = Pin<Box<dyn Future<Output = SpeculationPool> + 'a>>;

/// Drop guard for the paid-call window ([`Engine::run_model_call`]): armed
/// before the retried provider dispatch, disarmed on both normal exits. It
/// fires only when the turn future is dropped mid-await — the caller-side
/// hard cancel — leaving one content-free `Cancelled` usage envelope so a
/// possibly-billed in-flight call never vanishes from the accounting
/// stream. Content-free by construction, same privacy rule as every other
/// `UsageIncomplete` envelope: no request or response body is representable.
struct CancelUsageGuard {
    events: EventSender,
    role: stella_protocol::ModelCallRole,
    provider: String,
    started: std::time::Instant,
    armed: bool,
}

impl CancelUsageGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelUsageGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.events.send(AgentEvent::UsageIncomplete {
            role: self.role,
            provider: self.provider.clone(),
            model: "unknown".into(),
            reason: stella_protocol::UsageIncompleteReason::Cancelled,
            duration_ms: self.started.elapsed().as_millis() as u64,
            retries: None,
        });
    }
}

/// Prefix of the overflow summarizer's marker message
/// ([`Engine::summarize_overflow_span`]). Shared with [`recent_tool_calls`]:
/// the marker is User-role on the wire, but it is NOT a real user turn and
/// must not act as a loop-detection window boundary.
const SUMMARY_MARKER_PREFIX: &str = "[earlier history summarized";

/// Flatten the tool calls of the CURRENT turn — assistant messages after
/// the last user message — in chronological order, for
/// `crate::loop_detect::detect_loop`. Windowing at the user boundary
/// matters: identical calls across turns are the user re-asking a
/// question, not a stuck loop (a REPL session asking the same thing three
/// times would otherwise trip the exact-repeat detector), and it keeps
/// this per-step scan O(turn) instead of O(entire history). The overflow
/// summary is also User-role but is not a real user turn — treating it as
/// a boundary would truncate the loop window on every summarization pass
/// and let a stuck loop outrun detection, so it is skipped when locating
/// the boundary.
fn recent_tool_calls(messages: &[CompletionMessage]) -> Vec<ToolCall> {
    let turn_start = messages
        .iter()
        .rposition(|m| m.role == MessageRole::User && !m.content.starts_with(SUMMARY_MARKER_PREFIX))
        .map(|i| i + 1)
        .unwrap_or(0);
    messages[turn_start..]
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .flat_map(|m| m.tool_calls.iter().cloned())
        .collect()
}

/// The [`TurnOutcome::Aborted`] reason of a user-requested soft stop —
/// callers match on this to render "stopped" rather than "failed", and to
/// keep (never truncate) the turn's completed work.
pub const SOFT_STOP_REASON: &str = "stopped at step boundary by user — completed steps kept";

#[cfg(test)]
mod tests;
