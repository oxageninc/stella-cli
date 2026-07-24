//! The event vocabulary — plain enum variants flowing from `stella-core` to
//! whichever renderer (TUI or the JSON serializer) is listening.
//! `--output-format stream-json` is a `serde_json` serialization of this
//! exact enum, one line per event: a stable, versioned machine interface.
//!
//! The vocabulary is additive-only: later variants are appended as the
//! context/media/fleet crates land, never a breaking rename.

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolOutput};

/// A named point in the turn's data flow
/// Exactly one stage
/// vocabulary exists in this workspace — never duplicated per-crate (the
/// TS-era `StageKind` duplication this structurally forbids, L-E1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Triage,
    ContextRecall,
    Plan,
    ScopeReview,
    /// Witness authoring: before the worker executes, an independent model
    /// (the judge's resolution, never the worker's transcript) writes the
    /// witness test — a test that FAILS on the current code and will pass
    /// once the goal is met — arming the deterministic flip oracle (L-E11).
    /// The witness is visible to the worker (iterating against a failing
    /// test is where convergence comes from); integrity comes from tamper
    /// exclusion at verify time, not from hiding the test.
    Witness,
    Execute,
    Verify,
    Judge,
    /// Post-turn self-reflection: the agent reviews its own performance on
    /// the completed turn and records improvement memories into the context
    /// plane, tagged with the workspace's inferred domains, for recall on
    /// future relevant turns.
    Reflect,
    ContextWrite,
    Complete,
}

/// Budget enforcement mode: `off` (no metering),
/// `observed` (meter + warn), `enforced` (hard stop with a clean turn
/// abort — never a mid-tool kill).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetMode {
    Off,
    Observed,
    Enforced,
}

/// Which budget limit a [`AgentEvent::BudgetDenied`] tripped — mirrors
/// `stella-core::budget::BudgetAxis` (kept separate so `stella-protocol`
/// never depends on `stella-core`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScope {
    Turn,
    Session,
}

/// What kind of policy-plane decision a [`AgentEvent::PolicyDecision`]
/// records (receipts spec §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    /// A blocking policy chain evaluated a tool call or side effect.
    Evaluated,
    /// A policy denied the call/side effect.
    Blocked,
    /// A policy deferred the call to human approval.
    ApprovalRequested,
    /// A payload-hygiene detector flagged secret-shaped content.
    SecretDetected,
}

/// Concrete purpose of one provider call. This is more precise than the
/// router's tier role: repair and guidance calls must remain distinguishable
/// in the paid-call ledger even when they share a provider/model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallRole {
    /// Legacy events written before call-role attribution existed.
    #[default]
    Unknown,
    Triage,
    Plan,
    PlanRepair,
    WitnessAuthor,
    WitnessRepair,
    Worker,
    DistressGuidance,
    Judge,
    AgentAuthor,
    SkillAuthor,
    DomainInference,
    Reflection,
    Summarization,
}

/// Content-free reason a provider attempt cannot contribute a truthful usage
/// envelope. Error bodies and prompts are deliberately unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageIncompleteReason {
    ProviderError,
    Timeout,
    /// The caller dropped the turn (hard cancel) while a paid provider
    /// attempt was still in flight — the call may have real server-side
    /// cost whose usage is unknowable. Emitted by the engine's drop guard,
    /// which is armed only for exactly that window (a call that settles
    /// normally reports through its ordinary `StepUsage` envelope instead).
    Cancelled,
}

/// The semantic kind of one context block — one durable, individually
/// attributable unit that can enter the model's prompt. Finer-grained than a
/// `CompletionMessage`: a tool message holding several results decomposes into
/// one `ToolResult` block per `call_id`. See the session-telemetry-receipts
/// spec (`docs/design/session-telemetry-receipts-spec.md`, §4). Forward-compat:
/// an unknown kind read from a newer emitter deserializes to [`BlockKind::Other`]
/// rather than failing the whole event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// Message index 0 — the stable system prefix, never compacted.
    SystemPrefix,
    /// The user's task text.
    UserGoal,
    /// A context frame injected by recall (memory/graph/file).
    RecalledFrame,
    /// Model prose.
    AssistantText,
    /// An assistant tool-call request.
    ToolCall,
    /// A tool output (Ok or Error).
    ToolResult,
    /// A mid-turn injected user (steering) message.
    Steered,
    /// An overflow-summarizer replacement span.
    Summary,
    /// A multimodal attachment (image/doc/audio/video).
    Attachment,
    /// A kind this reader does not recognize (written by a newer emitter).
    #[default]
    #[serde(other)]
    Other,
}

/// A block's cache position relative to the provider's prompt-cache
/// breakpoints. Stella keeps the system prefix byte-stable and places volatile
/// recall after it (L-E8), so a block's zone is computable from its position at
/// manifest time. A structural hint at emission; reconciled against reported
/// usage by cache attribution (spec §7). Forward-compat via [`CacheZone::Other`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheZone {
    /// At/before the system-block breakpoint — should cache-hit every step.
    StablePrefix,
    /// Before the conversation-tail breakpoint — cacheable across steps.
    #[default]
    Cacheable,
    /// After the last breakpoint — recomputed every step by construction.
    Volatile,
    /// A zone this reader does not recognize (written by a newer emitter).
    #[serde(other)]
    Other,
}

/// One event in the turn's stream. Every stage boundary emits an event;
/// nothing user-visible is derived from internal state that isn't also in
/// this stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Stage {
        name: StageKind,
    },
    Text {
        delta: String,
    },
    /// One in-order fragment of the answer text, emitted live while the
    /// model call streams. Strictly a best-effort preview: the step's
    /// following `Text` event carries the full text and is authoritative —
    /// consumers must REPLACE any accumulated deltas with it, never merge
    /// (a retried model call re-streams its deltas from the start, so the
    /// accumulation can be garbled; there is no reset marker). Additive to
    /// the stream-json wire contract: consumers must tolerate `text_delta`
    /// lines appearing between events, and persistence layers may drop them
    /// (the `Text` event is the durable record).
    TextDelta {
        text: String,
    },
    Reasoning {
        delta: String,
    },
    ToolStart {
        call: ToolCall,
    },
    ToolResult {
        call_id: String,
        output: ToolOutput,
        duration_ms: u64,
        /// True when this result was produced by speculative execution: the
        /// call was read-only and began executing while the model was still
        /// streaming the rest of its response, so `duration_ms` (the real
        /// execution time) overlapped the model call instead of following
        /// it. `serde(default)` so streams recorded before this field parse.
        #[serde(default)]
        speculated: bool,
    },
    /// A speculatively-executed read-only call (`stella-core::speculation`)
    /// whose result never reached the transcript: its stream attempt failed
    /// and the pool was dropped, or the committed call diverged from what
    /// was announced so the pooled result was rejected at harvest. The
    /// tool's real I/O still ran — this is the event-log's record of that
    /// work, so call counts reconcile with what actually executed rather
    /// than silently diverging. `reason` is a short stable token
    /// (`"attempt_failed"`, `"harvest_mismatch"`). Additive to the wire
    /// contract: consumers recorded before speculation existed never see it.
    SpeculationDiscarded {
        call_id: String,
        name: String,
        reason: String,
    },
    Retry {
        attempt: u32,
        reason: String,
    },
    /// A user message queued mid-turn was injected at a step boundary
    /// (`stella-core` steering) — the transcript's record that the model
    /// was steered, and when.
    Steered {
        text: String,
    },
    /// Loop detection fired (receipts spec §6.3, #364 gap 3): the typed
    /// twin of the prose steer/abort, so receipts can parse the decision
    /// instead of string-matching an `Error` prefix. Emitted on BOTH
    /// outcomes — the first detection steers (`aborted: false`) and a
    /// detection that persists past the warning aborts (`aborted: true`).
    /// Additive to the wire contract: older consumers never see it.
    LoopDetected {
        turn_instance: u32,
        /// `"exact_repeat"` | `"short_cycle"` — mirrors
        /// `stella-core::loop_detect::LoopVerdict` (kept as a string here so
        /// `stella-protocol` never depends on `stella-core`).
        kind: String,
        /// Tool names of the repeated signature, in cycle order (one entry
        /// for an exact repeat).
        pattern: Vec<String>,
        /// Consecutive identical calls (exact repeat) or full cycles (short
        /// cycle) observed.
        repeats: usize,
        /// The human-readable evidence — same text the paired
        /// `Steered`/`Error` carries.
        evidence: String,
        /// `false`: first detection, the turn was steered and continues.
        /// `true`: detection persisted after the warning, the turn aborted.
        aborted: bool,
    },
    /// An enforced budget stopped the turn (receipts spec §6.3, #364 gap
    /// 3): the typed twin of the prose "budget exceeded" `Error`. Only ever
    /// emitted in `BudgetMode::Enforced` — observed mode warns without
    /// denying.
    BudgetDenied {
        /// Which limit tripped.
        scope: BudgetScope,
        spent_usd: f64,
        limit_usd: f64,
        mode: BudgetMode,
    },
    /// A model call failed terminally after exhausting its retries
    /// (receipts spec §6.3, #364 gap 3). The per-attempt reasons were
    /// previously lost on the failure path — `Retry` events only flush for
    /// steps that COMMIT — so this is the durable record of the doomed
    /// attempts. Emitted just before the paired `Error`.
    RetriesExhausted {
        turn_instance: u32,
        /// Total dispatched attempts that failed (the initial call plus
        /// every retry). Equals `reasons.len()`.
        attempts: u32,
        /// Per-attempt failure reasons, oldest first.
        reasons: Vec<String>,
    },
    /// One decision from the extension/policy audit plane bridged into the
    /// event stream (receipts spec §6.4, #364 gap 6). The `HookBus` audit
    /// events (`policy.evaluated`/`policy.blocked`, `approval.requested`,
    /// `secret.detected`) were process-ephemeral — hosts map them onto this
    /// variant so the journal carries the policy plane too. Content-free by
    /// design: `subject` names the tool/capability/path, NEVER a secret
    /// value or file contents.
    PolicyDecision {
        kind: PolicyKind,
        /// The tool name, capability, or workspace-relative path the
        /// decision was about.
        subject: String,
        /// Short outcome token — e.g. `"allow"`, `"deny"`, `"modify"`, a
        /// detector's kind list — never content.
        outcome: String,
    },
    /// A compaction pass ran (`stella-core::compaction`). Fields mirror
    /// `CompactionReport` — kept as a flat struct here (not a re-exported
    /// type) so `stella-protocol` never depends on `stella-core` (dependency
    /// direction: core depends on protocol, never the reverse).
    Compaction {
        before_tokens: u64,
        after_tokens: u64,
        evicted: usize,
        deduped: usize,
        /// Older results of a repeated identical call, stubbed as stale.
        /// `serde(default)` so journals written before these fields parse.
        #[serde(default)]
        superseded: usize,
        /// Large old outputs middle-out truncated instead of dropped whole.
        #[serde(default)]
        aged: usize,
        /// Messages replaced by a model-written history summary — the
        /// overflow fallback when eviction alone cannot reach budget.
        #[serde(default)]
        summarized: usize,
        /// The `block_id`s each pass stubbed (spec §6.2) — identities, not just
        /// counts, so the receipt records *which* blocks left context and a
        /// later pass can prove a block was evicted before it was ever cited or
        /// referenced (the wasted-carry signal). For the pure passes each vec's
        /// length equals its count field (`summarized_blocks` is the documented
        /// exception). `serde(default)` — absent on pre-identity journals.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        evicted_blocks: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        deduped_blocks: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        superseded_blocks: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        aged_blocks: Vec<String>,
        /// The `block_id`s of the tool-result blocks folded into an
        /// overflow-summary splice (spec §6.2). Unlike the pure passes — which
        /// stub tool-result blocks one-for-one, so their vec length equals the
        /// count — the summary replaces a whole message span whose `summarized`
        /// count also covers user/assistant text carrying no block identity;
        /// this vector is the identity-bearing (tool-result) subset that left
        /// context, so `summarized_blocks.len()` may be less than `summarized`.
        /// `serde(default)` — absent on pre-identity journals.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        summarized_blocks: Vec<String>,
        /// The budget this pass actually compared against — the raw compaction
        /// budget divided by the model's calibration factor — and that factor.
        /// The event's `before/after_tokens` are raw estimates; these are the
        /// numbers the eviction loop's stopping condition used, so the receipt
        /// lines up with the decision (#364 item 1). `0` on pre-receipt journals.
        #[serde(default)]
        effective_budget_tokens: u64,
        #[serde(default)]
        calibration_factor: f64,
    },
    /// Emitted after every provider/media call that spends money
    /// The TUI HUD renders spend live from this
    /// stream; nothing user-visible about spend is derived from state that
    /// isn't also in this event.
    BudgetTick {
        spent_usd: f64,
        limit_usd: Option<f64>,
        mode: BudgetMode,
        /// Session-scoped spend at this tick — `spent_usd`/`limit_usd` are
        /// turn-scoped, so a HUD cannot otherwise reconstruct session state
        /// (or see a session-axis breach) from this stream. `None` when the
        /// emitter does not track a session axis, and on events serialized
        /// before these fields existed (hence `serde(default)`, so older
        /// streams still parse).
        #[serde(default)]
        session_spent_usd: Option<f64>,
        /// The configured per-session limit, when one is set. `None` mirrors
        /// `session_spent_usd`.
        #[serde(default)]
        session_limit_usd: Option<f64>,
    },
    /// One committed model call — the metering record. Emitted exactly once
    /// per step that lands, carrying the normalized usage envelope plus
    /// everything a metering/billing pipeline needs to price and audit the
    /// call; aggregate a turn by summing its `StepUsage` events.
    StepUsage {
        step: usize,
        /// Exact call purpose. Missing legacy values deserialize as
        /// [`ModelCallRole::Unknown`].
        #[serde(default)]
        role: ModelCallRole,
        /// Provider which actually served this call, never the session's
        /// configured default. Empty only on legacy events.
        #[serde(default)]
        provider: String,
        /// Authoritative model output for calls that do not emit a separate
        /// [`AgentEvent::Text`] (pipeline management and compaction calls).
        /// Execute calls leave this `None`, avoiding duplicate transcript
        /// text while keeping older event consumers compatible.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_text: Option<String>,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        /// Tokens written to the provider's prompt cache by this call
        /// (`CompletionUsage::cache_write_tokens`). Reported separately from
        /// `input_tokens`, never a subset of it. `0` when the provider does
        /// not report cache writes (the OpenAI-compatible dialects) — hence
        /// `serde(default)`, so streams serialized before this field existed
        /// still parse.
        #[serde(default)]
        cache_write_tokens: u64,
        /// The engine's RAW (uncalibrated) pre-call estimate of the input it
        /// sent — paired with `input_tokens` this is one drift sample, the
        /// feedback that calibrates future estimates per model
        /// (`stella-core::estimator::Calibration`). Raw by contract:
        /// consumers rebuild the correction from these pairs, and a
        /// corrected estimate here would compound the correction on every
        /// round trip. `0` means no estimate was taken (pre-drift emitters —
        /// hence `serde(default)`, so old streams still parse).
        #[serde(default)]
        estimated_input_tokens: u64,
        cost_usd: f64,
        duration_ms: u64,
        retries: u32,
        tool_calls: usize,
        /// Whether the provider supplied a truthful usage envelope. Missing
        /// legacy values fail closed to `false`.
        #[serde(default)]
        complete: bool,
    },
    /// A provider call failed or timed out after dispatch, so local accounting
    /// cannot prove that no billable work occurred. Content-free by design.
    UsageIncomplete {
        role: ModelCallRole,
        provider: String,
        model: String,
        reason: UsageIncompleteReason,
        duration_ms: u64,
        /// Number of retries completed before the failure, when known.
        retries: Option<u32>,
    },
    /// A judge model's assessment of a goal-driven loop after one working
    /// round. `met == true` ends the loop; `met == false` feeds `reasoning`
    /// back to the worker as course-correction. `cost_usd` is the judge
    /// call's own spend.
    GoalVerdict {
        round: usize,
        met: bool,
        reasoning: String,
        cost_usd: f64,
    },
    /// A provider's circuit breaker opened and the router fell back to the
    /// next configured provider of the same role's tier. Never silent
    /// (L-M7) — no mid-turn family switch happens without this event.
    ProviderFallback {
        from: String,
        to: String,
        reason: String,
    },
    /// A file was read/created/modified/deleted by the agent, carrying the
    /// diff so the TUI's files-touched panel renders per-edit diffs without a
    /// second data path (L-T5: in TS, the `onFileEdit` callback had to be
    /// patched into two pipeline switches — here there is one emission
    /// point by construction). Reads carry no diff; consumers that only care
    /// about mutations (the pipeline's zero-diff guard, inline transcript
    /// diffs) filter on the kind.
    FileChange {
        path: String,
        kind: FileChangeKind,
        diff: Option<String>,
    },
    /// Context recall completed: which frames
    /// reached the prompt, from which providers, at what token cost. Every
    /// frame carries a human `citation_label`, never a raw id (L-C4).
    ContextRecall {
        frames: Vec<ContextFrameRef>,
        provider_mix: Vec<ProviderShare>,
        tokens: u32,
    },
    /// Context write-back completed: episode summaries, fact upserts,
    /// supersession (bi-temporal,
    /// close-not-delete per L-C3).
    ContextWrite {
        provider: String,
        upserts: u32,
        superseded: u32,
    },
    /// A context block first became eligible to enter the prompt (spec §4).
    /// The birth record that makes the per-step manifest an index over the fold.
    ///
    /// Digest, not bytes, for the kinds the journal already carries whole
    /// (`ToolResult`, `ToolCall`/`ToolStart`, assistant `Text`): those preimages
    /// are resolved from the originating event at reconstruction time, never
    /// re-stored. For the two kinds the fold does NOT carry — the system prefix
    /// and the assembled user/recall message — `content` carries the bytes so
    /// the step is reconstructable (spec §5.3). That content is **local-only**:
    /// it is stripped by the content-free enterprise export projection and never
    /// leaves the local journal. So "content-free" means content-free *on
    /// export*, and content-free *on the wire for journal-resolvable kinds* —
    /// gap-kind blocks deliberately carry their bytes locally. Additive:
    /// consumers recorded before receipts existed simply never see this event.
    BlockRegistered {
        /// `blk_<24 hex of sha256(kind \0 content)>`. Byte-identical blocks
        /// share an id, so dedup/supersession become identities not counts.
        block_id: String,
        kind: BlockKind,
        origin: BlockOrigin,
        /// Estimated tokens at birth (the engine's estimator).
        token_cost: u32,
        /// `"sha256:<full hex>"` — verifies the preimage on reconstruction.
        content_digest: String,
        /// Human label for recall frames / memory nodes, when the block has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        citation_label: Option<String>,
        /// Local-only preimage for gap kinds the journal cannot resolve (the
        /// system prefix, the assembled user/recall message). `None` for
        /// journal-resolvable kinds (tool I/O, assistant text) — those never
        /// carry bytes here. Redacted by the content-free export projection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    /// The ordered receipt of exactly what the model saw on one step (spec §5):
    /// the block sequence sent, in wire order, plus the budget the compaction
    /// pass actually compared against this step. Emitted immediately before the
    /// step's model call commits. Content-free (block ids + small ints); the
    /// preimages are resolved from the fold at inspection time. This is the
    /// record that makes any past step reconstructable and auditable.
    StepManifest {
        /// Monotonic per session — groups the steps of one `run_turn`.
        turn_instance: u32,
        step: usize,
        role: ModelCallRole,
        provider: String,
        model: String,
        /// Blocks in wire order; index 0 is the system prefix.
        blocks: Vec<ManifestEntry>,
        /// The budget the compaction pass actually compared against THIS step —
        /// the raw budget divided by the model's calibration factor. Evented so
        /// the receipt's numbers line up with the decision that was made (the
        /// `Compaction` event's raw before/after do not, on their own — #364).
        effective_budget_tokens: u64,
        /// The per-model calibration factor applied to the raw budget.
        calibration_factor: f64,
        /// Sum of block token costs, pre-call (the engine's raw estimate).
        estimated_input_tokens: u64,
    },
    /// A verification verdict — from the deterministic ladder (flip oracle,
    /// touched-tests-green) or the model judge (L-E11: deterministic-first;
    /// model judges handle only inconclusive evidence).
    JudgeVerdict {
        passed: bool,
        evidence: JudgeEvidence,
    },
    /// Interactive gate before large plans execute (L-E5): the pipeline
    /// pauses on this event and waits for approval above configured
    /// thresholds; headless requires a flag to bypass.
    ScopeReview {
        proposal: ScopeProposal,
    },
    /// The agent asked the user a multiple-choice question (the `ask_user`
    /// tool). BINDING renderer contract: present the structured `options`
    /// AND always exactly one additional free-text option — the user can
    /// always answer in their own words, on every question, without the
    /// model having to list that affordance itself. The answer returns as
    /// the tool call's ordinary `ToolResult`; there is no separate answer
    /// event. Headless runs fail this tool with a named error instead of
    /// hanging on input that will never arrive.
    AskUser {
        /// Correlates the eventual answer (the ToolResult's `call_id`)
        /// back to this question.
        id: String,
        question: String,
        options: Vec<String>,
    },
    /// A media generation job changed state. Video
    /// jobs are async and long-lived; this event is how the TUI shows
    /// progress without polling shared state (L-T1).
    MediaProgress {
        artifact_id: String,
        kind: MediaKind,
        state: MediaJobState,
    },
    /// A media artifact landed under `.stella/artifacts/` with a manifest
    /// row.
    MediaComplete {
        artifact: MediaArtifactRef,
    },
    /// A commit landed (fleet ledger / pipeline execute stage).
    Commit {
        sha: String,
        message: String,
    },
    /// A pull request was opened or changed status (fleet PR/CI monitor).
    /// `number` and `ci` ride `serde(default)` so streams recorded before
    /// they existed still parse (additive-only wire contract).
    Pr {
        url: String,
        status: PrStatus,
        /// The PR number (e.g. 183 for `…/pull/183`). `None` on streams
        /// recorded before the field existed or when the monitor could not
        /// parse one from the URL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<u64>,
        /// The head commit's aggregate CI verdict, when observed. Absent
        /// means "not polled yet", never "passing".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ci: Option<CiStatus>,
    },
    /// The turn's task board changed (an agent called one of the `task_*`
    /// tools). Carries the FULL board snapshot, not a delta — the render
    /// fold stays pure and any single event reconstructs the checklist,
    /// which is what makes dead-session replay show the board as it was.
    TaskUpdate {
        tasks: Vec<TaskItem>,
    },
    Error {
        message: String,
        retryable: bool,
    },
    Complete {
        model: String,
        cost_usd: f64,
    },
}

impl AgentEvent {
    /// The stable discriminant tag for this event — identical to the string
    /// `serde` writes as the `"type"` field on the stream-json wire (this enum
    /// is `#[serde(tag = "type", rename_all = "snake_case")]`). Allocation-free,
    /// so logs, metrics, and tests can name an event without serializing it.
    ///
    /// This match is deliberately **exhaustive, with no wildcard arm**: it is
    /// the cheap compile-time guard for the additive-only `AgentEvent`
    /// vocabulary. Adding a variant fails `cargo build -p stella-protocol` (and
    /// `-p stella-core`, which compiles this crate) with `E0004` right here — a
    /// scoped per-crate build, not only a full `--workspace` build discovered
    /// post-merge (#455). When that fires, add the arm AND propagate the new
    /// variant to every downstream matcher:
    ///
    /// **Compile-enforced** — also exhaustive, so they will not build until you
    /// add an arm; but each break surfaces one crate at a time (CI stops at the
    /// first failing crate), which is exactly how #415's variant reached `main`
    /// before breaking `stella-pipeline` (#421) then `stella-tui` (#422):
    ///   - `stella-pipeline` `replay::event_signature`
    ///   - `stella-tui` `model::Model::apply`
    ///   - `stella-tui` `textline::event_line`
    ///   - `stella-tui` `deck::trace_of`
    ///
    /// **Silent** — wildcard / `matches!` arms the compiler CANNOT catch, so a
    /// new variant falls through to a default and is wrong only at runtime.
    /// These are the real trap; audit them by hand:
    ///   - `stella-pipeline` `replay::structural_diff` volatile keep-set: add
    ///     the variant if it is a run-to-run artifact absent from older golden
    ///     streams, or it will shift every aligned position of the diff.
    ///   - `stella-tui` `deck::event_intensity` and `deck::status_from_event`:
    ///     give the variant an intensity / agent status if it should register
    ///     on the fleet deck.
    ///
    /// The same duty applies to the other exhaustively-matched cross-crate
    /// enums this pattern warns about (`ToolOutput`, `BudgetOutcome`).
    pub fn type_tag(&self) -> &'static str {
        match self {
            AgentEvent::Stage { .. } => "stage",
            AgentEvent::Text { .. } => "text",
            AgentEvent::TextDelta { .. } => "text_delta",
            AgentEvent::Reasoning { .. } => "reasoning",
            AgentEvent::ToolStart { .. } => "tool_start",
            AgentEvent::ToolResult { .. } => "tool_result",
            AgentEvent::SpeculationDiscarded { .. } => "speculation_discarded",
            AgentEvent::Retry { .. } => "retry",
            AgentEvent::Steered { .. } => "steered",
            AgentEvent::LoopDetected { .. } => "loop_detected",
            AgentEvent::BudgetDenied { .. } => "budget_denied",
            AgentEvent::RetriesExhausted { .. } => "retries_exhausted",
            AgentEvent::PolicyDecision { .. } => "policy_decision",
            AgentEvent::Compaction { .. } => "compaction",
            AgentEvent::BudgetTick { .. } => "budget_tick",
            AgentEvent::StepUsage { .. } => "step_usage",
            AgentEvent::UsageIncomplete { .. } => "usage_incomplete",
            AgentEvent::GoalVerdict { .. } => "goal_verdict",
            AgentEvent::ProviderFallback { .. } => "provider_fallback",
            AgentEvent::FileChange { .. } => "file_change",
            AgentEvent::ContextRecall { .. } => "context_recall",
            AgentEvent::ContextWrite { .. } => "context_write",
            AgentEvent::BlockRegistered { .. } => "block_registered",
            AgentEvent::StepManifest { .. } => "step_manifest",
            AgentEvent::JudgeVerdict { .. } => "judge_verdict",
            AgentEvent::ScopeReview { .. } => "scope_review",
            AgentEvent::AskUser { .. } => "ask_user",
            AgentEvent::MediaProgress { .. } => "media_progress",
            AgentEvent::MediaComplete { .. } => "media_complete",
            AgentEvent::Commit { .. } => "commit",
            AgentEvent::Pr { .. } => "pr",
            AgentEvent::TaskUpdate { .. } => "task_update",
            AgentEvent::Error { .. } => "error",
            AgentEvent::Complete { .. } => "complete",
        }
    }
}

/// What happened to a file in a `FileChange` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    /// Content was successfully read — no mutation, never a diff. Rides the
    /// same event so the files-touched panel sees reads without a second
    /// data path.
    Read,
    Created,
    Modified,
    Deleted,
}

impl FileChangeKind {
    /// Whether this kind mutated the file — what the pipeline's zero-diff
    /// guard and inline transcript diffs key on. Reads are observability,
    /// not change.
    pub fn is_mutation(self) -> bool {
        !matches!(self, FileChangeKind::Read)
    }
}

/// A context frame as cited in a `ContextRecall` event. `citation_label`
/// is mandatory and human-readable; the raw `id` (when the frame is
/// materialized at all) belongs only in inspectable detail views, never as
/// the primary identifier (L-C4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextFrameRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub citation_label: String,
    /// The CGP provider leg that returned the frame. Empty only when reading
    /// a stream recorded before provider provenance was added.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// The original source named by the frame's provenance chain. This is
    /// deliberately distinct from [`Self::provider`]: a host adapter may be
    /// `workspace-memory` while the record source remains `stella-context`.
    pub source: String,
    /// The protocol frame kind (`symbol`, `memory`, `graph`, ...).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// Canonical source URI when the frame supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// The most-derived provenance method, when declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub token_cost: u32,
    /// The registry id (`blk_…`) of this frame as a context block, when
    /// receipts are enabled. Joins the frame to its manifest membership and,
    /// for memory frames, to the write→citation loop (spec §5.3, §9). Absent on
    /// streams recorded before receipts existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    /// `"sha256:<hex>"` of the exact injected text. The digest rides the wire;
    /// the content itself is journaled only locally (never exported), closing
    /// the recall-content gap G1 without widening the content-free export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<String>,
}

/// Where a context block came from — the provenance stamped once at a block's
/// birth (spec §4). The join hub: a `RecalledFrame` carries the `memory_id` it
/// was recalled from; a `ToolResult`/`ToolCall` carries its `call_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockOrigin {
    /// Monotonic per session — the `run_turn` that produced the block.
    pub turn_instance: u32,
    /// The step within that turn.
    pub step: usize,
    /// Tool-call correlation id, for `ToolCall`/`ToolResult` blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// The `nod_…` memory node id, for a `RecalledFrame` that is a memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
}

/// One block's membership in a step's manifest (spec §5): its id, its cache
/// zone at that step, its estimated token cost, and how long it has been
/// resident. Residency × cost is what makes cost-of-carry a real number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub block_id: String,
    /// Cache position class relative to the last stable breakpoint.
    #[serde(default)]
    pub cache_zone: CacheZone,
    pub token_cost: u32,
    /// The step this block first entered a manifest — drives cost-of-carry.
    pub resident_since_step: usize,
    /// Which `CompletionMessage` this block belonged to, by position in the
    /// sent sequence. Event-granular blocks (a tool message's several results,
    /// an assistant message's text + calls) share one `message_index`, so
    /// reconstruction regroups them back into the exact message boundaries
    /// rather than inferring them from kinds (spec §5.1). `0` on manifests
    /// recorded before reconstruction existed.
    #[serde(default)]
    pub message_index: usize,
}

/// One provider's share of a recall's frame mix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderShare {
    pub provider: String,
    pub frames: u32,
}

/// Evidence backing a `JudgeVerdict`. `deterministic` distinguishes the
/// flip-oracle/tests ladder from a model judge's opinion — the two are
/// never conflated (L-E11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeEvidence {
    pub summary: String,
    /// `true` when the verdict came from the deterministic ladder (a
    /// fail→pass flip of the same normalized test command, touched-tests
    /// green, diff budget) rather than a model judge.
    pub deterministic: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

/// What a `ScopeReview` gate presents for approval before a large plan
/// executes (L-E5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeProposal {
    pub summary: String,
    pub steps: Vec<String>,
    pub estimated_files: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

/// Which kind of media artifact a job produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Svg,
    Video,
}

/// Lifecycle of an async media job. `Failed` carries the reason inline —
/// a failed job must never be distinguishable only by the absence of a
/// success event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MediaJobState {
    Queued,
    Running,
    Succeeded,
    Failed { reason: String },
}

/// A completed media artifact: id + kind + where it landed on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaArtifactRef {
    pub id: String,
    pub kind: MediaKind,
    /// Path under `.stella/artifacts/` (the generation tools may never
    /// write outside it).
    pub path: String,
    /// Human label for citation/display.
    pub label: String,
}

/// A pull request's status as observed by the fleet monitor. Reconciled
/// against the live source before rendering, never served from cache
/// alone (L-V3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrStatus {
    Draft,
    Open,
    Merged,
    Closed,
}

/// Aggregate CI verdict for a PR's head commit, as observed by the
/// fleet monitor (`gh pr checks`). Reconciled against the live source
/// before rendering, never served from cache alone (L-V3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiStatus {
    /// Checks exist but none have started reporting.
    Pending,
    /// At least one check is still running and none have failed.
    Running,
    Passing,
    Failing,
}

/// One entry on the turn's task board (the `task_*` tools). The board is
/// session-scoped working state — what the agent has planned, is doing,
/// and has finished — mirrored to the store for cross-session findability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskItem {
    /// Stable per-session ordinal id ("1", "2", …) — what `task_complete`
    /// / `task_cancel` / `task_assign` reference.
    pub id: String,
    /// Imperative title ("Fix the auth redirect loop").
    pub subject: String,
    /// What needs to be done, if the creator elaborated beyond the subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: TaskStatus,
    /// Which agent lane owns the task: `None` until claimed, `Some("lead")`
    /// for the lead, or the sub-agent lane id once `task_assign` spawned a
    /// dedicated worker for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// Lifecycle of a `TaskItem`. Terminal states are `Completed` and
/// `Cancelled`; a cancelled task keeps its row (the board is an audit
/// surface, not just a scheduler).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TaskStatus {
    /// Whether the task can still change state. Terminal tasks reject
    /// further transitions (enforced by the board logic in `stella-core`).
    pub fn is_open(self) -> bool {
        matches!(self, TaskStatus::Pending | TaskStatus::InProgress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_roundtrips_with_type_tag() {
        let event = AgentEvent::ToolStart {
            call: ToolCall {
                call_id: "call_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "src/main.rs" }),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"tool_start\""), "{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::ToolStart { call } => assert_eq!(call.name, "read_file"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn text_delta_roundtrips_with_its_own_type_tag() {
        // `text_delta` is additive on the wire: a distinct tag from `text`,
        // so a pre-delta consumer that skips unknown lines keeps parsing the
        // authoritative `text` events unchanged.
        let event = AgentEvent::TextDelta { text: "Hel".into() };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"text_delta\""), "{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::TextDelta { text } => assert_eq!(text, "Hel"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn tool_result_roundtrips_and_streams_without_speculated_still_parse() {
        // Round-trip with the flag set.
        let event = AgentEvent::ToolResult {
            call_id: "call_1".into(),
            output: ToolOutput::Ok {
                content: "x".into(),
            },
            duration_ms: 42,
            speculated: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::ToolResult { speculated, .. } => assert!(speculated),
            other => panic!("unexpected variant: {other:?}"),
        }

        // A stream recorded BEFORE the field existed must still parse, with
        // the safe default (not speculated).
        let old = r#"{"type":"tool_result","call_id":"c","output":{"ok":{"content":""}},"duration_ms":1}"#;
        match serde_json::from_str::<AgentEvent>(old) {
            Ok(AgentEvent::ToolResult { speculated, .. }) => {
                assert!(!speculated, "missing field must default to false")
            }
            other => panic!("old stream must parse: {other:?}"),
        }
    }

    #[test]
    fn speculation_discarded_roundtrips_and_names_the_reason() {
        let event = AgentEvent::SpeculationDiscarded {
            call_id: "c1".into(),
            name: "read_file".into(),
            reason: "attempt_failed".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"type\":\"speculation_discarded\""),
            "{json}"
        );
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::SpeculationDiscarded {
                call_id,
                name,
                reason,
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(name, "read_file");
                assert_eq!(reason, "attempt_failed");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn budget_tick_roundtrips_with_session_axis() {
        let event = AgentEvent::BudgetTick {
            spent_usd: 0.42,
            limit_usd: Some(2.5),
            mode: BudgetMode::Enforced,
            session_spent_usd: Some(1.75),
            session_limit_usd: Some(10.0),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::BudgetTick {
                spent_usd,
                limit_usd,
                mode,
                session_spent_usd,
                session_limit_usd,
            } => {
                assert_eq!(spent_usd, 0.42);
                assert_eq!(limit_usd, Some(2.5));
                assert_eq!(mode, BudgetMode::Enforced);
                assert_eq!(session_spent_usd, Some(1.75));
                assert_eq!(session_limit_usd, Some(10.0));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn budget_tick_without_session_fields_parses_with_none() {
        // A stream recorded BEFORE the session axis existed must still parse,
        // with both new fields defaulting to `None` (not `0.0`, which would
        // read as a real "spent nothing").
        let old = r#"{"type":"budget_tick","spent_usd":0.42,"limit_usd":2.5,"mode":"enforced"}"#;
        match serde_json::from_str::<AgentEvent>(old) {
            Ok(AgentEvent::BudgetTick {
                session_spent_usd,
                session_limit_usd,
                ..
            }) => {
                assert_eq!(session_spent_usd, None);
                assert_eq!(session_limit_usd, None);
            }
            other => panic!("old stream must parse: {other:?}"),
        }
    }

    #[test]
    fn compaction_event_carries_counts_and_block_identities() {
        let event = AgentEvent::Compaction {
            before_tokens: 10_000,
            after_tokens: 4_000,
            evicted: 2,
            deduped: 1,
            superseded: 1,
            aged: 1,
            summarized: 3,
            evicted_blocks: vec!["blk_ev1".into(), "blk_ev2".into()],
            deduped_blocks: vec!["blk_dd1".into()],
            superseded_blocks: vec!["blk_sup".into()],
            aged_blocks: vec!["blk_age".into()],
            // Fewer identities than the `summarized` count: the summary folded
            // three messages but only two were identity-bearing tool results.
            summarized_blocks: vec!["blk_sum1".into(), "blk_sum2".into()],
            effective_budget_tokens: 136_363,
            calibration_factor: 1.1,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"compaction\""), "{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::Compaction {
                before_tokens,
                after_tokens,
                evicted_blocks,
                summarized,
                summarized_blocks,
                effective_budget_tokens,
                ..
            } => {
                assert!(after_tokens < before_tokens);
                // Identities, not just counts — which blocks left context.
                assert_eq!(evicted_blocks, vec!["blk_ev1", "blk_ev2"]);
                // The summary names its folded tool-result blocks, and the vec
                // may be shorter than the message count it replaced.
                assert_eq!(summarized_blocks, vec!["blk_sum1", "blk_sum2"]);
                assert!(summarized_blocks.len() < summarized);
                assert_eq!(effective_budget_tokens, 136_363);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn legacy_compaction_without_identities_still_parses() {
        // A journal written before §6.2 has counts but no identity fields; it
        // must still deserialize (additive contract), with empty identity vecs.
        let old =
            r#"{"type":"compaction","before_tokens":9,"after_tokens":4,"evicted":1,"deduped":0}"#;
        match serde_json::from_str::<AgentEvent>(old).unwrap() {
            AgentEvent::Compaction {
                evicted,
                evicted_blocks,
                effective_budget_tokens,
                ..
            } => {
                assert_eq!(evicted, 1);
                assert!(evicted_blocks.is_empty());
                assert_eq!(effective_budget_tokens, 0);
            }
            other => panic!("old compaction must parse: {other:?}"),
        }
    }

    #[test]
    fn provider_fallback_is_never_silent_it_names_both_ends() {
        let event = AgentEvent::ProviderFallback {
            from: "zai".into(),
            to: "anthropic".into(),
            reason: "circuit breaker open after 3 consecutive transport failures".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"from\":\"zai\""), "{json}");
        assert!(json.contains("\"to\":\"anthropic\""), "{json}");
    }

    #[test]
    fn file_change_carries_the_diff_on_the_single_event_path() {
        let event = AgentEvent::FileChange {
            path: "src/lib.rs".into(),
            kind: FileChangeKind::Modified,
            diff: Some("@@ -1 +1 @@\n-old\n+new".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"file_change\""), "{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::FileChange { kind, diff, .. } => {
                assert_eq!(kind, FileChangeKind::Modified);
                assert!(diff.is_some());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn read_kind_serializes_and_is_the_only_non_mutation() {
        assert_eq!(
            serde_json::to_string(&FileChangeKind::Read).unwrap(),
            "\"read\""
        );
        let back: FileChangeKind = serde_json::from_str("\"read\"").unwrap();
        assert_eq!(back, FileChangeKind::Read);
        assert!(!FileChangeKind::Read.is_mutation());
        for kind in [
            FileChangeKind::Created,
            FileChangeKind::Modified,
            FileChangeKind::Deleted,
        ] {
            assert!(kind.is_mutation(), "{kind:?} is a mutation");
        }
    }

    #[test]
    fn context_recall_frames_always_carry_a_citation_label() {
        let event = AgentEvent::ContextRecall {
            frames: vec![ContextFrameRef {
                id: None, // not-yet-materialized frames carry no id (L-C4)
                citation_label: "engine step-driver (driver.rs)".into(),
                provider: "code-graph".into(),
                source: "code-graph".into(),
                kind: "symbol".into(),
                uri: Some("file:///repo/stella-core/src/driver.rs".into()),
                method: Some("tree-sitter/symbol-extract".into()),
                token_cost: 120,
                block_id: None,
                content_digest: None,
            }],
            provider_mix: vec![ProviderShare {
                provider: "code-graph".into(),
                frames: 1,
            }],
            tokens: 120,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("citation_label"), "{json}");
        assert!(
            !json.contains("\"id\""),
            "absent id must be omitted: {json}"
        );
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::ContextRecall { frames, .. } => {
                let frame = &frames[0];
                assert_eq!(frame.provider, "code-graph");
                assert_eq!(frame.source, "code-graph");
                assert_eq!(frame.kind, "symbol");
                assert_eq!(
                    frame.uri.as_deref(),
                    Some("file:///repo/stella-core/src/driver.rs")
                );
                assert_eq!(frame.method.as_deref(), Some("tree-sitter/symbol-extract"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn context_recall_from_a_pre_provenance_stream_still_parses() {
        let legacy = r#"{"type":"context_recall","frames":[{"citation_label":"driver.rs","source":"code-graph","token_cost":12}],"provider_mix":[{"provider":"code-graph","frames":1}],"tokens":12}"#;
        match serde_json::from_str::<AgentEvent>(legacy) {
            Ok(AgentEvent::ContextRecall { frames, .. }) => {
                let frame = &frames[0];
                assert!(frame.provider.is_empty());
                assert_eq!(frame.source, "code-graph");
                assert!(frame.kind.is_empty());
                assert_eq!(frame.uri, None);
                assert_eq!(frame.method, None);
            }
            other => panic!("old stream must parse: {other:?}"),
        }
    }

    #[test]
    fn media_job_failure_carries_its_reason_inline() {
        let state = MediaJobState::Failed {
            reason: "provider rejected the prompt".into(),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: MediaJobState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn judge_verdict_distinguishes_deterministic_from_model_evidence() {
        let event = AgentEvent::JudgeVerdict {
            passed: true,
            evidence: JudgeEvidence {
                summary: "flip oracle: fail→pass on `cargo test -p x`".into(),
                deterministic: true,
                evidence_refs: vec!["trace:t1#verify".into()],
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::JudgeVerdict { evidence, .. } => assert!(evidence.deterministic),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn ask_user_roundtrips_and_carries_structured_options() {
        let event = AgentEvent::AskUser {
            id: "call_q1".into(),
            question: "Which database should the migration target?".into(),
            options: vec!["local (5433)".into(), "staging".into()],
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"ask_user\""), "{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::AskUser { options, .. } => assert_eq!(options.len(), 2),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn scope_review_and_pr_events_roundtrip() {
        for event in [
            AgentEvent::ScopeReview {
                proposal: ScopeProposal {
                    summary: "refactor the auth module".into(),
                    steps: vec!["step 1".into(), "step 2".into()],
                    estimated_files: 12,
                    estimated_cost_usd: Some(1.25),
                },
            },
            AgentEvent::Pr {
                url: "https://github.com/x/y/pull/1".into(),
                status: PrStatus::Open,
                number: Some(1),
                ci: Some(CiStatus::Running),
            },
            AgentEvent::Commit {
                sha: "abc123".into(),
                message: "feat: x".into(),
            },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            let _back: AgentEvent = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn pr_event_from_a_pre_ci_stream_still_parses() {
        // Backward compatibility: a `pr` line serialized before `number`
        // and `ci` existed must deserialize with both absent — absent ci
        // means "not polled yet", never "passing".
        let legacy = r#"{"type":"pr","url":"https://github.com/x/y/pull/183","status":"open"}"#;
        match serde_json::from_str::<AgentEvent>(legacy) {
            Ok(AgentEvent::Pr { number, ci, .. }) => {
                assert_eq!(number, None);
                assert_eq!(ci, None);
            }
            other => panic!("old stream must parse: {other:?}"),
        }
    }

    #[test]
    fn task_update_roundtrips_a_full_board_snapshot() {
        let event = AgentEvent::TaskUpdate {
            tasks: vec![
                TaskItem {
                    id: "1".into(),
                    subject: "Map the auth module".into(),
                    description: None,
                    status: TaskStatus::Completed,
                    owner: Some("lead".into()),
                },
                TaskItem {
                    id: "2".into(),
                    subject: "Fix the redirect loop".into(),
                    description: Some("token refresh races the redirect".into()),
                    status: TaskStatus::InProgress,
                    owner: Some("sub:2".into()),
                },
                TaskItem {
                    id: "3".into(),
                    subject: "Add a witness test".into(),
                    description: None,
                    status: TaskStatus::Pending,
                    owner: None,
                },
            ],
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"task_update\""), "{json}");
        // Absent optionals are omitted, not serialized as null.
        assert!(!json.contains("null"), "{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::TaskUpdate { tasks } => {
                assert_eq!(tasks.len(), 3);
                assert_eq!(tasks[1].status, TaskStatus::InProgress);
                assert_eq!(tasks[1].owner.as_deref(), Some("sub:2"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn task_status_open_vs_terminal() {
        assert!(TaskStatus::Pending.is_open());
        assert!(TaskStatus::InProgress.is_open());
        assert!(!TaskStatus::Completed.is_open());
        assert!(!TaskStatus::Cancelled.is_open());
    }

    #[test]
    fn stream_json_is_one_line_per_event() {
        let events = [
            AgentEvent::Stage {
                name: StageKind::Triage,
            },
            AgentEvent::Text { delta: "hi".into() },
            AgentEvent::Complete {
                model: "glm-5.2".into(),
                cost_usd: 0.001,
            },
        ];
        let lines: Vec<String> = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(!line.contains('\n'));
        }
    }

    #[test]
    fn step_usage_roundtrips_as_a_complete_metering_record() {
        let event = AgentEvent::StepUsage {
            step: 3,
            role: ModelCallRole::Plan,
            provider: "zai".into(),
            output_text: Some(r#"["inspect", "patch"]"#.into()),
            model: "glm-5.2".into(),
            input_tokens: 12_000,
            output_tokens: 450,
            cached_input_tokens: 9_000,
            cache_write_tokens: 2_500,
            estimated_input_tokens: 11_200,
            cost_usd: 0.0042,
            duration_ms: 1_830,
            retries: 1,
            tool_calls: 4,
            complete: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"step_usage\""), "{json}");
        assert!(json.contains("\"role\":\"plan\""), "{json}");
        assert!(json.contains("\"cache_write_tokens\":2500"), "{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::StepUsage {
                step,
                role,
                output_text,
                cached_input_tokens,
                cache_write_tokens,
                estimated_input_tokens,
                retries,
                tool_calls,
                ..
            } => {
                assert_eq!(step, 3);
                assert_eq!(role, ModelCallRole::Plan);
                assert_eq!(output_text.as_deref(), Some(r#"["inspect", "patch"]"#));
                assert_eq!(cached_input_tokens, 9_000);
                assert_eq!(cache_write_tokens, 2_500);
                assert_eq!(estimated_input_tokens, 11_200);
                assert_eq!(retries, 1);
                assert_eq!(tool_calls, 4);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn step_usage_from_a_pre_drift_stream_still_parses() {
        // Backward compatibility: a `step_usage` line serialized before
        // `estimated_input_tokens` existed must deserialize with the field
        // defaulting to 0 ("no estimate was taken") — the stream-json wire
        // format is versioned by being additive-only.
        let legacy = r#"{"type":"step_usage","step":3,"model":"glm-5.2","input_tokens":12000,
            "output_tokens":450,"cached_input_tokens":9000,"cost_usd":0.0042,
            "duration_ms":1830,"retries":1,"tool_calls":4}"#;
        let back: AgentEvent = serde_json::from_str(legacy).unwrap();
        match back {
            AgentEvent::StepUsage {
                role,
                output_text,
                estimated_input_tokens,
                input_tokens,
                ..
            } => {
                assert_eq!(estimated_input_tokens, 0);
                assert_eq!(role, ModelCallRole::Unknown);
                assert_eq!(output_text, None);
                assert_eq!(input_tokens, 12_000);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn step_usage_from_a_pre_cache_write_stream_still_parses() {
        // Backward compatibility: a `step_usage` line serialized before
        // `cache_write_tokens` existed (but after `estimated_input_tokens`)
        // must deserialize with the field defaulting to 0 ("provider
        // reported no cache writes") — the additive-only wire contract.
        let legacy = r#"{"type":"step_usage","step":3,"model":"glm-5.2","input_tokens":12000,
            "output_tokens":450,"cached_input_tokens":9000,"estimated_input_tokens":11200,
            "cost_usd":0.0042,"duration_ms":1830,"retries":1,"tool_calls":4}"#;
        let back: AgentEvent = serde_json::from_str(legacy).unwrap();
        match back {
            AgentEvent::StepUsage {
                cache_write_tokens,
                cached_input_tokens,
                estimated_input_tokens,
                ..
            } => {
                assert_eq!(cache_write_tokens, 0);
                assert_eq!(cached_input_tokens, 9_000);
                assert_eq!(estimated_input_tokens, 11_200);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn goal_verdict_roundtrips_both_outcomes() {
        for met in [true, false] {
            let event = AgentEvent::GoalVerdict {
                round: 2,
                met,
                reasoning: "tests now pass".into(),
                cost_usd: 0.001,
            };
            let json = serde_json::to_string(&event).unwrap();
            assert!(json.contains("\"type\":\"goal_verdict\""), "{json}");
            let back: AgentEvent = serde_json::from_str(&json).unwrap();
            match back {
                AgentEvent::GoalVerdict { met: b, round, .. } => {
                    assert_eq!(b, met);
                    assert_eq!(round, 2);
                }
                other => panic!("unexpected variant: {other:?}"),
            }
        }
    }

    #[test]
    fn step_usage_preserves_call_identity_and_completeness() {
        let json = r#"{"type":"step_usage","step":3,"role":"plan_repair","provider":"anthropic","model":"claude-sonnet-4-5","input_tokens":12000,"output_tokens":300,"cached_input_tokens":9000,"cache_write_tokens":12,"estimated_input_tokens":11000,"cost_usd":0.09,"duration_ms":1400,"retries":1,"tool_calls":0,"complete":true}"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        let roundtrip = serde_json::to_value(event).unwrap();
        assert_eq!(roundtrip["role"], "plan_repair");
        assert_eq!(roundtrip["provider"], "anthropic");
        assert_eq!(roundtrip["complete"], true);
    }

    #[test]
    fn legacy_step_usage_without_completeness_fails_closed() {
        let legacy = r#"{"type":"step_usage","step":1,"model":"old","input_tokens":10,"output_tokens":2,"cached_input_tokens":0,"cost_usd":0.01,"duration_ms":10,"retries":0,"tool_calls":0}"#;
        let event: AgentEvent = serde_json::from_str(legacy).unwrap();
        let roundtrip = serde_json::to_value(event).unwrap();
        assert_eq!(roundtrip["complete"], false);
    }

    #[test]
    fn usage_incomplete_is_a_closed_content_free_signal() {
        let json = r#"{"type":"usage_incomplete","role":"judge","provider":"anthropic","model":"claude-sonnet-4-5","reason":"timeout","duration_ms":2500,"retries":null}"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        let roundtrip = serde_json::to_value(event).unwrap();
        assert_eq!(roundtrip["type"], "usage_incomplete");
        assert_eq!(roundtrip["reason"], "timeout");
        assert_eq!(roundtrip.as_object().unwrap().len(), 7);
    }

    #[test]
    fn block_registered_carries_bytes_only_for_gap_kinds() {
        // Journal-resolvable kinds (tool I/O, assistant text) carry NO content —
        // their preimage is resolved from the originating event, never re-stored.
        let tool = AgentEvent::BlockRegistered {
            block_id: "blk_0123456789abcdef01234567".into(),
            kind: BlockKind::ToolResult,
            origin: BlockOrigin {
                turn_instance: 2,
                step: 5,
                call_id: Some("call_9".into()),
                memory_id: None,
            },
            token_cost: 480,
            content_digest: "sha256:deadbeef".into(),
            citation_label: None,
            content: None,
        };
        let value = serde_json::to_value(&tool).unwrap();
        assert_eq!(value["type"], "block_registered");
        assert_eq!(value["kind"], "tool_result");
        assert_eq!(value["origin"]["call_id"], "call_9");
        assert!(
            value.get("content").is_none() && value.get("output").is_none(),
            "a journal-resolvable block must not carry payload bytes: {value}"
        );

        // Gap kinds the journal cannot resolve (the system prefix, the assembled
        // user message) DO carry their bytes — local-only, stripped on export —
        // so the step stays reconstructable (spec §5.3).
        let system = AgentEvent::BlockRegistered {
            block_id: "blk_sys0000000000000000000".into(),
            kind: BlockKind::SystemPrefix,
            origin: BlockOrigin {
                turn_instance: 0,
                step: 0,
                call_id: None,
                memory_id: None,
            },
            token_cost: 300,
            content_digest: "sha256:beef".into(),
            citation_label: None,
            content: Some("you are a careful engineer".into()),
        };
        let value = serde_json::to_value(&system).unwrap();
        assert_eq!(value["content"], "you are a careful engineer");

        let back: AgentEvent = serde_json::from_str(&value.to_string()).unwrap();
        match back {
            AgentEvent::BlockRegistered { kind, content, .. } => {
                assert_eq!(kind, BlockKind::SystemPrefix);
                assert_eq!(content.as_deref(), Some("you are a careful engineer"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn step_manifest_preserves_block_order_and_the_effective_budget() {
        let event = AgentEvent::StepManifest {
            turn_instance: 1,
            step: 3,
            role: ModelCallRole::Worker,
            provider: "anthropic".into(),
            model: "claude-opus".into(),
            blocks: vec![
                ManifestEntry {
                    block_id: "blk_sys".into(),
                    cache_zone: CacheZone::StablePrefix,
                    token_cost: 1200,
                    resident_since_step: 0,
                    message_index: 0,
                },
                ManifestEntry {
                    block_id: "blk_tail".into(),
                    cache_zone: CacheZone::Volatile,
                    token_cost: 90,
                    resident_since_step: 3,
                    message_index: 3,
                },
            ],
            effective_budget_tokens: 136_363,
            calibration_factor: 1.1,
            estimated_input_tokens: 1290,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "step_manifest");
        // Order is load-bearing — the manifest IS the wire sequence.
        assert_eq!(value["blocks"][0]["block_id"], "blk_sys");
        assert_eq!(value["blocks"][1]["block_id"], "blk_tail");
        assert_eq!(value["effective_budget_tokens"], 136_363);
        let back: AgentEvent = serde_json::from_str(&value.to_string()).unwrap();
        match back {
            AgentEvent::StepManifest { blocks, .. } => {
                assert_eq!(blocks.len(), 2);
                assert_eq!(blocks[0].cache_zone, CacheZone::StablePrefix);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn context_frame_ref_without_receipt_fields_still_parses() {
        // A recall frame recorded before receipts existed carries no block_id
        // or content_digest — it must still deserialize (additive contract).
        let old = r#"{"citation_label":"auth module","source":"stella-context","token_cost":42}"#;
        let frame: ContextFrameRef = serde_json::from_str(old).unwrap();
        assert_eq!(frame.citation_label, "auth module");
        assert!(frame.block_id.is_none());
        assert!(frame.content_digest.is_none());
    }

    #[test]
    fn unknown_block_kind_degrades_to_other_not_a_parse_error() {
        // A newer emitter may name a block kind this reader has never heard of.
        // The additive contract requires it to degrade, not reject the event.
        let kind: BlockKind = serde_json::from_str("\"some_future_kind\"").unwrap();
        assert_eq!(kind, BlockKind::Other);
        let zone: CacheZone = serde_json::from_str("\"some_future_zone\"").unwrap();
        assert_eq!(zone, CacheZone::Other);
    }

    #[test]
    fn type_tag_matches_the_serde_type_wire_tag() {
        // `type_tag()` must return exactly the `"type"` string serde writes,
        // since both come from the same `snake_case` variant name. The match's
        // exhaustiveness is compiler-enforced (a new variant cannot escape a
        // tag), so this only pins that the hand-written strings are correct —
        // cross-checked against serde for a representative sample, weighted to
        // the recently added variants most prone to a copy-paste tag.
        let sample = vec![
            AgentEvent::Stage {
                name: StageKind::Triage,
            },
            AgentEvent::Text { delta: "hi".into() },
            AgentEvent::TextDelta { text: "h".into() },
            AgentEvent::Reasoning { delta: "r".into() },
            AgentEvent::SpeculationDiscarded {
                call_id: "c".into(),
                name: "n".into(),
                reason: "attempt_failed".into(),
            },
            AgentEvent::Retry {
                attempt: 1,
                reason: "x".into(),
            },
            AgentEvent::Steered { text: "s".into() },
            AgentEvent::LoopDetected {
                turn_instance: 1,
                kind: "exact_repeat".into(),
                pattern: vec!["read".into()],
                repeats: 2,
                evidence: "e".into(),
                aborted: false,
            },
            AgentEvent::BudgetDenied {
                scope: BudgetScope::Turn,
                spent_usd: 1.0,
                limit_usd: 0.5,
                mode: BudgetMode::Enforced,
            },
            AgentEvent::RetriesExhausted {
                turn_instance: 1,
                attempts: 3,
                reasons: vec!["t".into()],
            },
            AgentEvent::PolicyDecision {
                kind: PolicyKind::Blocked,
                subject: "write_file".into(),
                outcome: "deny".into(),
            },
            AgentEvent::BudgetTick {
                spent_usd: 0.1,
                limit_usd: None,
                mode: BudgetMode::Off,
                session_spent_usd: None,
                session_limit_usd: None,
            },
            AgentEvent::UsageIncomplete {
                role: ModelCallRole::Worker,
                provider: "z".into(),
                model: "m".into(),
                reason: UsageIncompleteReason::Timeout,
                duration_ms: 1,
                retries: None,
            },
            AgentEvent::GoalVerdict {
                round: 1,
                met: true,
                reasoning: "ok".into(),
                cost_usd: 0.0,
            },
            AgentEvent::ProviderFallback {
                from: "a".into(),
                to: "b".into(),
                reason: "r".into(),
            },
            AgentEvent::Commit {
                sha: "abc".into(),
                message: "m".into(),
            },
            AgentEvent::Pr {
                url: "u".into(),
                status: PrStatus::Open,
                number: None,
                ci: None,
            },
            AgentEvent::Error {
                message: "m".into(),
                retryable: false,
            },
            AgentEvent::Complete {
                model: "m".into(),
                cost_usd: 0.0,
            },
        ];
        for event in &sample {
            let value = serde_json::to_value(event).unwrap();
            let wire = value
                .get("type")
                .and_then(|tag| tag.as_str())
                .unwrap_or_else(|| panic!("event has no string `type` tag: {event:?}"));
            assert_eq!(
                event.type_tag(),
                wire,
                "type_tag disagrees with the serde wire tag for {event:?}"
            );
        }
        // Pin two exact tags so a wholesale serde-rename change is caught too.
        assert_eq!(
            AgentEvent::TextDelta {
                text: String::new()
            }
            .type_tag(),
            "text_delta"
        );
        assert_eq!(
            AgentEvent::SpeculationDiscarded {
                call_id: String::new(),
                name: String::new(),
                reason: String::new(),
            }
            .type_tag(),
            "speculation_discarded"
        );
    }
}
