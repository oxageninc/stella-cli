//! The event vocabulary — plain enum variants flowing from `stella-core` to
//! whichever renderer (TUI or the JSON serializer) is listening.
//! `--output-format stream-json` is a `serde_json` serialization of this
//! exact enum, one line per event: a stable, versioned machine interface
//! (`docs/specs/oxagen-rust-cli/02-architecture.md` §4).
//!
//! This is deliberately a *subset* at Phase 0 (only what a bare
//! provider-streaming spike needs); later phases append variants as the
//! context/media/fleet crates land — additive only, never a breaking
//! rename, once this ships past Phase 0.

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolOutput};

/// A named point in the turn's data flow
/// (`docs/specs/oxagen-rust-cli/02-architecture.md` §5). Exactly one stage
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
    /// per step that lands (a step whose retries all fail emits `Error`,
    /// never a `StepUsage`), carrying the normalized usage envelope from
    /// `CompletionUsage` plus everything a metering/billing pipeline needs
    /// to price and audit the call without reconstructing state:
    /// aggregate a turn by summing its `StepUsage` events.
    /// `duration_ms` is wall-clock for the committed call *including* any
    /// retry backoff that preceded it (`retries` says how many).
    StepUsage {
        step: usize,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        cost_usd: f64,
        duration_ms: u64,
        retries: u32,
        tool_calls: usize,
    },
    /// A judge model's assessment of a goal-driven loop
    /// (`stella-core::goal`) after one working round. `met == true` ends
    /// the loop; `met == false` feeds `reasoning` back to the worker as
    /// course-correction. `cost_usd` is the judge call's own spend (already
    /// recorded against the budget when this event fires).
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
    Error {
        message: String,
        retryable: bool,
    },
    Complete {
        model: String,
        cost_usd: f64,
    },
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
    fn step_usage_roundtrips_as_a_complete_metering_record() {
        let event = AgentEvent::StepUsage {
            step: 3,
            model: "glm-5.2".into(),
            input_tokens: 12_000,
            output_tokens: 450,
            cached_input_tokens: 9_000,
            cost_usd: 0.0042,
            duration_ms: 1_830,
            retries: 1,
            tool_calls: 4,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"step_usage\""), "{json}");
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        match back {
            AgentEvent::StepUsage {
                step,
                input_tokens,
                cached_input_tokens,
                retries,
                tool_calls,
                ..
            } => {
                assert_eq!(step, 3);
                assert_eq!(input_tokens, 12_000);
                assert_eq!(cached_input_tokens, 9_000);
                assert_eq!(retries, 1);
                assert_eq!(tool_calls, 4);
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
                AgentEvent::GoalVerdict {
                    met: back_met,
                    round,
                    ..
                } => {
                    assert_eq!(back_met, met);
                    assert_eq!(round, 2);
                }
                other => panic!("unexpected variant: {other:?}"),
            }
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
}
