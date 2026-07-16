//! The event vocabulary — plain enum variants flowing from `stella-core` to
//! whichever renderer (TUI or the JSON serializer) is listening.
//! `--output-format stream-json` is a `serde_json` serialization of this
//! exact enum, one line per event: a stable, versioned machine interface
//! (`docs/specs/stella-rust-cli/02-architecture.md` §4).
//!
//! This is deliberately a *subset* at Phase 0 (only what a bare
//! provider-streaming spike needs); later phases append variants as the
//! context/media/fleet crates land — additive only, never a breaking
//! rename, once this ships past Phase 0.

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolOutput};

/// A named point in the turn's data flow
/// (`docs/specs/stella-rust-cli/02-architecture.md` §5). Exactly one stage
/// vocabulary exists in this workspace — never duplicated per-crate (the
/// TS-era `StageKind` duplication this structurally forbids, L-E1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Triage,
    ContextRecall,
    Plan,
    ScopeReview,
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

/// Budget enforcement mode (`07-model-matrix.md` §6): `off` (no metering),
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
    },
    Retry {
        attempt: u32,
        reason: String,
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
    },
    /// Emitted after every provider/media call that spends money
    /// (`07-model-matrix.md` §6). The TUI HUD renders spend live from this
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
    /// A file was created/modified/deleted by the agent, carrying the diff
    /// so the TUI's files-touched panel renders per-edit diffs without a
    /// second data path (L-T5: in TS, the `onFileEdit` callback had to be
    /// patched into two pipeline switches — here there is one emission
    /// point by construction).
    FileChange {
        path: String,
        kind: FileChangeKind,
        diff: Option<String>,
    },
    /// Context recall completed (`06-context-protocol.md`): which frames
    /// reached the prompt, from which providers, at what token cost. Every
    /// frame carries a human `citation_label`, never a raw id (L-C4).
    ContextRecall {
        frames: Vec<ContextFrameRef>,
        provider_mix: Vec<ProviderShare>,
        tokens: u32,
    },
    /// Context write-back completed: episode summaries, fact upserts,
    /// supersession (`06-context-protocol.md` §2.2 — bi-temporal,
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
    /// A media generation job changed state (`08-multimodal.md`). Video
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
    Pr {
        url: String,
        status: PrStatus,
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
    Created,
    Modified,
    Deleted,
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
    /// Which provider produced the frame (e.g. `"code-graph"`, `"memory"`,
    /// `"git-history"`, or an external OCP provider id).
    pub source: String,
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

/// Which kind of media artifact a job produces (`08-multimodal.md`).
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
    /// write outside it — `02-architecture.md` §8).
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
    fn context_recall_frames_always_carry_a_citation_label() {
        let event = AgentEvent::ContextRecall {
            frames: vec![ContextFrameRef {
                id: None, // not-yet-materialized frames carry no id (L-C4)
                citation_label: "engine step-driver (driver.rs)".into(),
                source: "code-graph".into(),
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
