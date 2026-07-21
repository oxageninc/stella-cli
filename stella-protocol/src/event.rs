//! The event vocabulary — plain enum variants flowing from `stella-core` to
//! whichever renderer (TUI or the JSON serializer) is listening.
//! `--output-format stream-json` is a `serde_json` serialization of this
//! exact enum, one line per event: a stable, versioned machine interface
//!
//!
//! This is deliberately a *subset* at Phase 0 (only what a bare
//! provider-streaming spike needs); later phases append variants as the
//! context/media/fleet crates land — additive only, never a breaking
//! rename, once this ships past Phase 0.

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
    },
    /// Emitted after every provider/media call that spends money
    /// The TUI HUD renders spend live from this
    /// stream; nothing user-visible about spend is derived from state that
    /// isn't also in this event.
    BudgetTick {
        spent_usd: f64,
        limit_usd: Option<f64>,
        mode: BudgetMode,
    },
    /// One committed model call — the metering record. Emitted exactly once
    /// per step that lands, carrying the normalized usage envelope plus
    /// everything a metering/billing pipeline needs to price and audit the
    /// call; aggregate a turn by summing its `StepUsage` events.
    StepUsage {
        step: usize,
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
    /// The OCP provider leg that returned the frame. Empty only when reading
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
    fn budget_tick_roundtrips_with_optional_limit() {
        let event = AgentEvent::BudgetTick {
            spent_usd: 0.42,
            limit_usd: Some(2.5),
            mode: BudgetMode::Enforced,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::BudgetTick {
                spent_usd,
                limit_usd,
                mode,
            } => {
                assert_eq!(spent_usd, 0.42);
                assert_eq!(limit_usd, Some(2.5));
                assert_eq!(mode, BudgetMode::Enforced);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn compaction_event_carries_counts() {
        let event = AgentEvent::Compaction {
            before_tokens: 10_000,
            after_tokens: 4_000,
            evicted: 3,
            deduped: 2,
            superseded: 1,
            aged: 1,
            summarized: 0,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"compaction\""), "{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::Compaction {
                before_tokens,
                after_tokens,
                ..
            } => {
                assert!(after_tokens < before_tokens);
            }
            other => panic!("unexpected variant: {other:?}"),
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
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"step_usage\""), "{json}");
        assert!(json.contains("\"cache_write_tokens\":2500"), "{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::StepUsage {
                step,
                cached_input_tokens,
                cache_write_tokens,
                estimated_input_tokens,
                retries,
                tool_calls,
                ..
            } => {
                assert_eq!(step, 3);
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
                estimated_input_tokens,
                input_tokens,
                ..
            } => {
                assert_eq!(estimated_input_tokens, 0);
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
}
