//! Golden-trajectory replay harness. Two jobs,
//! both pure: **validate** that an `AgentEvent` stream obeys the protocol's
//! structural invariants, and **structurally diff** two streams (kinds + order,
//! ignoring volatile fields like durations and exact costs) so a Rust-stack
//! trajectory can be asserted equivalent to a reference one.
//!
//! # Reference trajectories are a documented follow-up
//!
//! The fixtures under `tests/fixtures/` are **synthetic** streams that exercise
//! the invariants and the differ. *Recording real TS-engine trajectories* on
//! fixed tasks and checking the Rust stack against them is the next step — it is
//! deliberately not faked here. This module is the machinery those recordings
//! will be validated with once they exist.
//!
//! # Torn tails (L-T1)
//!
//! A crashed writer must never poison a reader: [`parse_jsonl`] tolerates a
//! single unparseable *final* line (a torn tail) by dropping it, while a
//! malformed *interior* line is a real error. Envelope evolution is
//! additive-only, so parsing is forward-tolerant by construction (serde
//! ignores unknown fields on the structs that opt in; unknown *variants* are
//! the one thing that can't be tolerated and surface as an interior error).

use stella_protocol::{AgentEvent, StageKind};

/// A structural invariant an event stream violated, with a human-readable
/// reason. Returned as a list so a single validation pass reports every
/// problem, not just the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamViolation {
    /// Index of the offending event in the stream (or the index the problem
    /// is attributed to — e.g. an unmatched `ToolStart`).
    pub index: usize,
    pub reason: String,
}

/// Validate a stream against the protocol's structural invariants
/// :
///
/// 1. **Legal stage ordering** — consecutive `Stage` events move forward in
///    the canonical order or take a known revise back-edge (Verify/Judge →
///    Execute); no other backward jump is legal.
/// 2. **Tool pairing** — every `ToolStart` has a later matching `ToolResult`
///    (same `call_id`), and no `ToolResult` appears without a prior
///    `ToolStart`.
/// 3. **Terminal `Complete`** — at most one `Complete`, and if present it is
///    the last event.
/// 4. **Monotonic budget** — `BudgetTick.spent_usd` never decreases.
///
/// Returns every violation found (empty vec = the stream is well-formed).
pub fn validate_stream(events: &[AgentEvent]) -> Vec<StreamViolation> {
    let mut violations = Vec::new();
    validate_stage_ordering(events, &mut violations);
    validate_tool_pairing(events, &mut violations);
    validate_terminal(events, &mut violations);
    validate_budget_monotonic(events, &mut violations);
    violations
}

/// Canonical rank of a stage in the one-turn data flow (
/// §5). Forward motion is any non-decreasing rank; the only legal backward
/// motion is the revise/best-of-N loop back to Execute.
fn stage_rank(stage: StageKind) -> u8 {
    match stage {
        StageKind::Triage => 0,
        StageKind::ContextRecall => 1,
        StageKind::Plan => 2,
        StageKind::ScopeReview => 3,
        // Witness authoring precedes execution: the failing witness test is
        // written before the worker starts (L-E11 front half).
        StageKind::Witness => 4,
        StageKind::Execute => 5,
        StageKind::Verify => 6,
        StageKind::Judge => 7,
        // Reflect is post-verdict self-reflection, before context write-back.
        StageKind::Reflect => 8,
        StageKind::ContextWrite => 9,
        StageKind::Complete => 10,
    }
}

/// Whether a transition between two consecutive `Stage` events is legal: a
/// forward (or same-rank) move, or the revise back-edge from Verify/Judge to
/// Execute (the revision loop and best-of-N re-execute the work).
pub fn stage_transition_legal(from: StageKind, to: StageKind) -> bool {
    if stage_rank(to) >= stage_rank(from) {
        return true;
    }
    matches!(
        (from, to),
        (StageKind::Verify, StageKind::Execute) | (StageKind::Judge, StageKind::Execute)
    )
}

fn validate_stage_ordering(events: &[AgentEvent], out: &mut Vec<StreamViolation>) {
    let mut last_stage: Option<StageKind> = None;
    for (i, event) in events.iter().enumerate() {
        if let AgentEvent::Stage { name } = event {
            if let Some(prev) = last_stage
                && !stage_transition_legal(prev, *name)
            {
                out.push(StreamViolation {
                    index: i,
                    reason: format!("illegal stage transition {prev:?} -> {name:?}"),
                });
            }
            last_stage = Some(*name);
        }
    }
}

fn validate_tool_pairing(events: &[AgentEvent], out: &mut Vec<StreamViolation>) {
    // Open ToolStarts keyed by call_id → the index they started at.
    let mut open: Vec<(String, usize)> = Vec::new();
    for (i, event) in events.iter().enumerate() {
        match event {
            AgentEvent::ToolStart { call } => open.push((call.call_id.clone(), i)),
            // `AskUser` is the `ask_user` tool's question; its answer returns
            // as an ordinary `ToolResult` keyed by this `id`, so it opens a
            // pending call exactly like a `ToolStart`.
            AgentEvent::AskUser { id, .. } => open.push((id.clone(), i)),
            AgentEvent::ToolResult { call_id, .. } => {
                if let Some(pos) = open.iter().position(|(id, _)| id == call_id) {
                    open.remove(pos);
                } else {
                    out.push(StreamViolation {
                        index: i,
                        reason: format!("tool_result for `{call_id}` with no preceding tool_start"),
                    });
                }
            }
            _ => {}
        }
    }
    for (call_id, start_index) in open {
        out.push(StreamViolation {
            index: start_index,
            reason: format!("tool_start for `{call_id}` never matched by a tool_result"),
        });
    }
}

fn validate_terminal(events: &[AgentEvent], out: &mut Vec<StreamViolation>) {
    let complete_indices: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, AgentEvent::Complete { .. }))
        .map(|(i, _)| i)
        .collect();
    if complete_indices.len() > 1 {
        for &i in &complete_indices[1..] {
            out.push(StreamViolation {
                index: i,
                reason: "more than one Complete event; a stream terminates once".to_string(),
            });
        }
    }
    if let Some(&first) = complete_indices.first()
        && first != events.len() - 1
    {
        out.push(StreamViolation {
            index: first,
            reason: "Complete is not the last event; nothing may follow it".to_string(),
        });
    }
}

fn validate_budget_monotonic(events: &[AgentEvent], out: &mut Vec<StreamViolation>) {
    let mut last_spent: Option<f64> = None;
    for (i, event) in events.iter().enumerate() {
        if let AgentEvent::BudgetTick { spent_usd, .. } = event {
            if let Some(prev) = last_spent
                && *spent_usd + f64::EPSILON < prev
            {
                out.push(StreamViolation {
                    index: i,
                    reason: format!(
                        "budget spent went backwards: {spent_usd:.6} < previous {prev:.6}"
                    ),
                });
            }
            last_spent = Some(*spent_usd);
        }
    }
}

/// A stable identifier for one event, capturing its kind and the fields that
/// matter for *structural* equivalence, while dropping volatile fields
/// (durations, exact costs/spend, free-text deltas). Two streams that agree on
/// their sequence of signatures are structurally equivalent even if they took
/// different wall-clock time or cost slightly different amounts.
///
/// Deliberately keeps identity-bearing fields (a tool's `name`, a stage's
/// `name`, a verdict's `passed`) and drops magnitude-bearing ones — the
/// distinction the golden-replay comparison rests on.
pub fn event_signature(event: &AgentEvent) -> String {
    match event {
        AgentEvent::Stage { name } => format!("stage:{name:?}"),
        // Text/Reasoning deltas are volatile content — only their presence
        // and kind are structural.
        AgentEvent::Text { .. } => "text".to_string(),
        // Streaming previews have no structural identity at all — even their
        // COUNT varies run to run with chunk boundaries — so
        // [`structural_diff`] excludes them before comparing; the signature
        // exists only to keep this function total.
        AgentEvent::TextDelta { .. } => "text_delta".to_string(),
        AgentEvent::Reasoning { .. } => "reasoning".to_string(),
        AgentEvent::ToolStart { call } => format!("tool_start:{}", call.name),
        // A tool_result's structural identity is that it answered a call and
        // whether it errored — not its duration or output body.
        AgentEvent::ToolResult { output, .. } => {
            format!("tool_result:error={}", output.is_error())
        }
        AgentEvent::Retry { .. } => "retry".to_string(),
        AgentEvent::Compaction { .. } => "compaction".to_string(),
        // Steering text is user-authored free text; only its occurrence is
        // structural (same posture as budget ticks).
        AgentEvent::Steered { .. } => "steered".to_string(),
        // Budget ticks vary in magnitude every run; only their occurrence is
        // structural.
        AgentEvent::BudgetTick { mode, .. } => format!("budget_tick:{mode:?}"),
        AgentEvent::ProviderFallback { from, to, .. } => {
            format!("provider_fallback:{from}->{to}")
        }
        AgentEvent::FileChange { kind, .. } => format!("file_change:{kind:?}"),
        AgentEvent::ContextRecall { .. } => "context_recall".to_string(),
        AgentEvent::ContextWrite { .. } => "context_write".to_string(),
        AgentEvent::MediaProgress { kind, .. } => format!("media_progress:{kind:?}"),
        AgentEvent::MediaComplete { .. } => "media_complete".to_string(),
        AgentEvent::JudgeVerdict { passed, evidence } => {
            format!(
                "judge_verdict:passed={},deterministic={}",
                passed, evidence.deterministic
            )
        }
        AgentEvent::ScopeReview { .. } => "scope_review".to_string(),
        // The question text is volatile; the number of structured options is
        // the structural part (the free-text option is always implied).
        AgentEvent::AskUser { options, .. } => format!("ask_user:options={}", options.len()),
        AgentEvent::Commit { .. } => "commit".to_string(),
        AgentEvent::Pr { status, .. } => format!("pr:{status:?}"),
        // Step usage is pure magnitude (tokens/cost/duration) — only its
        // occurrence is structural, like a budget tick.
        AgentEvent::StepUsage { .. } => "step_usage".to_string(),
        AgentEvent::UsageIncomplete { reason, .. } => {
            format!("usage_incomplete:{reason:?}")
        }
        // A goal verdict's structural identity is whether the goal was met
        // (mirrors `judge_verdict`); the reasoning text and cost are volatile.
        AgentEvent::GoalVerdict { met, .. } => format!("goal_verdict:met={met}"),
        AgentEvent::Error { retryable, .. } => format!("error:retryable={retryable}"),
        AgentEvent::Complete { .. } => "complete".to_string(),
        // Task subjects/descriptions are volatile content; the board's shape
        // (how many tasks, how many resolved) is the structural part.
        AgentEvent::TaskUpdate { tasks } => {
            let done = tasks.iter().filter(|t| !t.status.is_open()).count();
            format!("task_update:tasks={},resolved={done}", tasks.len())
        }
        // Context receipts are additive observability (spec §4/§5), excluded
        // from the structural comparison below just like TextDelta — a golden
        // stream recorded before receipts existed has none, so they must not
        // shift the aligned positions. The signatures exist only to keep this
        // function total; they capture occurrence + shape, never volatile ids.
        AgentEvent::BlockRegistered { kind, .. } => format!("block_registered:{kind:?}"),
        AgentEvent::StepManifest { blocks, .. } => {
            format!("step_manifest:blocks={}", blocks.len())
        }
    }
}

/// One positional difference between two structurally-compared streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDiff {
    /// Position in the sequence where the streams differ.
    pub index: usize,
    /// The left stream's signature at this position, `None` if the left ran
    /// out (the right is longer).
    pub left: Option<String>,
    /// The right stream's signature at this position, `None` if the right ran
    /// out (the left is longer).
    pub right: Option<String>,
}

/// Structurally diff two event streams by comparing their [`event_signature`]
/// sequences positionally (kinds + order, volatile fields ignored). Returns a
/// diff entry at every position where the signatures differ, plus one entry
/// per trailing event when the streams differ in length. An empty result
/// means the two streams are structurally equivalent.
///
/// Positional (not longest-common-subsequence) by design: golden replay
/// compares a run against a reference produced by the *same staged flow*, so
/// an aligned position-by-position comparison is the intended semantics —
/// a spurious insertion should surface as a divergence, not be quietly
/// realigned away.
pub fn structural_diff(left: &[AgentEvent], right: &[AgentEvent]) -> Vec<StreamDiff> {
    // `TextDelta` previews are excluded before the positional walk: the same
    // answer streams in different chunkings run to run, so even their count
    // is volatile — the authoritative `Text` event that follows them is the
    // structural record. Diff indices therefore address the delta-free
    // sequence.
    // Context receipts (BlockRegistered/StepManifest) join TextDelta in the
    // exclusion set: they are additive observability a pre-receipt golden
    // stream does not carry, so keeping them would shift every later position.
    let keep = |e: &&AgentEvent| {
        !matches!(
            e,
            AgentEvent::TextDelta { .. }
                | AgentEvent::BlockRegistered { .. }
                | AgentEvent::StepManifest { .. }
        )
    };
    let left: Vec<&AgentEvent> = left.iter().filter(keep).collect();
    let right: Vec<&AgentEvent> = right.iter().filter(keep).collect();
    let mut diffs = Vec::new();
    let max_len = left.len().max(right.len());
    for i in 0..max_len {
        let l = left.get(i).copied().map(event_signature);
        let r = right.get(i).copied().map(event_signature);
        if l != r {
            diffs.push(StreamDiff {
                index: i,
                left: l,
                right: r,
            });
        }
    }
    diffs
}

/// Whether two streams are structurally equivalent (no diffs).
pub fn streams_equivalent(left: &[AgentEvent], right: &[AgentEvent]) -> bool {
    structural_diff(left, right).is_empty()
}

/// An error parsing an event-stream JSONL document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsonlError {
    /// A non-final line failed to parse as an `AgentEvent`. Interior
    /// corruption is fatal — only a torn *tail* is tolerated (L-T1).
    #[error("malformed event on line {line} (1-indexed): {message}")]
    MalformedLine { line: usize, message: String },
}

/// Parse an event-stream JSONL document (one `AgentEvent` per line) into a
/// vector of events, tolerating a single torn final line (L-T1): if the *last*
/// non-empty line fails to parse — the signature of a writer that crashed
/// mid-line — it is dropped rather than failing the whole parse. A malformed
/// *interior* line is a [`JsonlError::MalformedLine`].
pub fn parse_jsonl(input: &str) -> Result<Vec<AgentEvent>, JsonlError> {
    // Collect (1-indexed line number, content) for every non-blank line.
    let lines: Vec<(usize, &str)> = input
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim()))
        .filter(|(_, l)| !l.is_empty())
        .collect();

    let mut events = Vec::with_capacity(lines.len());
    let last_index = lines.len().saturating_sub(1);
    for (pos, (line_no, content)) in lines.iter().enumerate() {
        match serde_json::from_str::<AgentEvent>(content) {
            Ok(event) => events.push(event),
            Err(err) => {
                if pos == last_index {
                    // Torn tail: a crashed writer left a partial final line.
                    // Drop it and return what parsed cleanly (L-T1).
                    break;
                }
                return Err(JsonlError::MalformedLine {
                    line: *line_no,
                    message: err.to_string(),
                });
            }
        }
    }
    Ok(events)
}

/// Serialize a stream to JSONL (one event per line) — the inverse of
/// [`parse_jsonl`], used to write fixtures and to emit `--output-format
/// stream-json`. Never fails: every `AgentEvent` is serde-serializable by
/// construction.
pub fn to_jsonl(events: &[AgentEvent]) -> String {
    let mut out = String::new();
    for event in events {
        // `AgentEvent` always serializes (no non-string map keys, no
        // non-finite floats introduced by this crate); `expect` documents that
        // invariant rather than hiding a real fallible path.
        let line = serde_json::to_string(event).expect("AgentEvent is always serializable");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::event::BudgetMode;
    use stella_protocol::{JudgeEvidence, ToolCall, ToolOutput};

    fn stage(name: StageKind) -> AgentEvent {
        AgentEvent::Stage { name }
    }
    fn tool_start(id: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolStart {
            call: ToolCall {
                call_id: id.into(),
                name: name.into(),
                input: serde_json::json!({}),
            },
        }
    }
    fn tool_result(id: &str, err: bool) -> AgentEvent {
        AgentEvent::ToolResult {
            call_id: id.into(),
            output: if err {
                ToolOutput::Error {
                    message: "boom".into(),
                }
            } else {
                ToolOutput::Ok {
                    content: "ok".into(),
                }
            },
            duration_ms: 12,
            speculated: false,
        }
    }
    fn budget(spent: f64) -> AgentEvent {
        AgentEvent::BudgetTick {
            spent_usd: spent,
            limit_usd: None,
            mode: BudgetMode::Observed,
        }
    }
    fn complete() -> AgentEvent {
        AgentEvent::Complete {
            model: "glm-5.2".into(),
            cost_usd: 0.01,
        }
    }
    fn judge(passed: bool, deterministic: bool) -> AgentEvent {
        AgentEvent::JudgeVerdict {
            passed,
            evidence: JudgeEvidence {
                summary: "x".into(),
                deterministic,
                evidence_refs: vec![],
            },
        }
    }

    // stage ordering

    #[test]
    fn canonical_forward_ordering_is_legal() {
        let events = [
            stage(StageKind::Triage),
            stage(StageKind::ContextRecall),
            stage(StageKind::Plan),
            stage(StageKind::Execute),
            stage(StageKind::Verify),
            stage(StageKind::Complete),
            complete(),
        ];
        assert!(validate_stream(&events).is_empty());
    }

    #[test]
    fn the_revise_back_edge_is_legal() {
        assert!(stage_transition_legal(
            StageKind::Verify,
            StageKind::Execute
        ));
        assert!(stage_transition_legal(StageKind::Judge, StageKind::Execute));
        // Witness authoring precedes execution (forward move), and the revise
        // back-edges land AFTER it — re-execution never re-authors.
        assert!(stage_transition_legal(
            StageKind::Witness,
            StageKind::Execute
        ));
        assert!(stage_transition_legal(
            StageKind::ScopeReview,
            StageKind::Witness
        ));
        assert!(!stage_transition_legal(
            StageKind::Execute,
            StageKind::Witness
        ));
        assert!(!stage_transition_legal(
            StageKind::Verify,
            StageKind::Witness
        ));
        // But you cannot jump backward to planning.
        assert!(!stage_transition_legal(StageKind::Execute, StageKind::Plan));
    }

    #[test]
    fn an_illegal_backward_stage_jump_is_flagged() {
        let events = [stage(StageKind::Execute), stage(StageKind::Triage)];
        let v = validate_stream(&events);
        assert_eq!(v.len(), 1);
        assert!(v[0].reason.contains("illegal stage transition"));
    }

    // tool pairing

    #[test]
    fn matched_tool_calls_pass() {
        let events = [tool_start("c1", "read_file"), tool_result("c1", false)];
        assert!(validate_stream(&events).is_empty());
    }

    #[test]
    fn an_unmatched_tool_start_is_flagged() {
        let events = [tool_start("c1", "read_file")];
        let v = validate_stream(&events);
        assert_eq!(v.len(), 1);
        assert!(v[0].reason.contains("never matched"));
    }

    #[test]
    fn a_dangling_tool_result_is_flagged() {
        let events = [tool_result("c9", false)];
        let v = validate_stream(&events);
        assert_eq!(v.len(), 1);
        assert!(v[0].reason.contains("no preceding tool_start"));
    }

    // terminal

    #[test]
    fn two_completes_are_flagged() {
        let events = [complete(), complete()];
        let v = validate_stream(&events);
        // one for "more than one Complete", one for "not the last"
        assert!(
            v.iter()
                .any(|x| x.reason.contains("more than one Complete"))
        );
    }

    #[test]
    fn complete_not_last_is_flagged() {
        let events = [complete(), stage(StageKind::Execute)];
        let v = validate_stream(&events);
        assert!(v.iter().any(|x| x.reason.contains("not the last event")));
    }

    // budget monotonic

    #[test]
    fn monotonic_budget_passes_and_regression_is_flagged() {
        assert!(validate_stream(&[budget(0.1), budget(0.2), budget(0.2)]).is_empty());
        let v = validate_stream(&[budget(0.5), budget(0.2)]);
        assert_eq!(v.len(), 1);
        assert!(v[0].reason.contains("backwards"));
    }

    // structural diff

    #[test]
    fn identical_kind_streams_are_equivalent_despite_volatile_fields() {
        let a = [
            tool_start("c1", "read_file"),
            tool_result("c1", false),
            budget(0.1),
        ];
        // Same kinds/names, different call_ids, durations, spend — still
        // structurally equivalent.
        let b = [
            tool_start("c2", "read_file"),
            tool_result("c2", false),
            budget(0.9),
        ];
        assert!(streams_equivalent(&a, &b));
        assert!(structural_diff(&a, &b).is_empty());
    }

    #[test]
    fn a_different_tool_name_diverges() {
        let a = [tool_start("c1", "read_file")];
        let b = [tool_start("c1", "write_file")];
        let diff = structural_diff(&a, &b);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].index, 0);
        assert_eq!(diff[0].left.as_deref(), Some("tool_start:read_file"));
        assert_eq!(diff[0].right.as_deref(), Some("tool_start:write_file"));
    }

    #[test]
    fn a_judge_verdict_flip_diverges() {
        assert!(!streams_equivalent(
            &[judge(true, true)],
            &[judge(false, true)]
        ));
        assert!(!streams_equivalent(
            &[judge(true, true)],
            &[judge(true, false)]
        ));
    }

    #[test]
    fn length_mismatch_reports_trailing_events() {
        let a = [stage(StageKind::Execute)];
        let b = [stage(StageKind::Execute), complete()];
        let diff = structural_diff(&a, &b);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].index, 1);
        assert_eq!(diff[0].left, None);
        assert_eq!(diff[0].right.as_deref(), Some("complete"));
    }

    // JSONL round-trip + torn tail

    #[test]
    fn jsonl_round_trips() {
        let events = vec![
            stage(StageKind::Triage),
            tool_start("c1", "read_file"),
            tool_result("c1", false),
            complete(),
        ];
        let jsonl = to_jsonl(&events);
        let parsed = parse_jsonl(&jsonl).unwrap();
        assert_eq!(parsed.len(), 4);
        assert!(streams_equivalent(&events, &parsed));
    }

    #[test]
    fn parse_jsonl_tolerates_a_torn_final_line() {
        let mut jsonl = to_jsonl(&[stage(StageKind::Execute), complete()]);
        // Simulate a crashed writer: append a partial final line.
        jsonl.push_str("{\"type\":\"tool_start\",\"call\":{\"call_id\":\"c1\",\"na");
        let parsed = parse_jsonl(&jsonl).unwrap();
        assert_eq!(parsed.len(), 2, "torn tail dropped, clean prefix kept");
    }

    #[test]
    fn parse_jsonl_rejects_a_malformed_interior_line() {
        let mut jsonl = String::new();
        jsonl.push_str(&serde_json::to_string(&stage(StageKind::Execute)).unwrap());
        jsonl.push('\n');
        jsonl.push_str("{ not valid json }\n");
        jsonl.push_str(&serde_json::to_string(&complete()).unwrap());
        jsonl.push('\n');
        match parse_jsonl(&jsonl) {
            Err(JsonlError::MalformedLine { line, .. }) => assert_eq!(line, 2),
            other => panic!("expected an interior MalformedLine error, got {other:?}"),
        }
    }

    #[test]
    fn parse_jsonl_ignores_blank_lines() {
        let jsonl = format!(
            "\n{}\n\n{}\n",
            serde_json::to_string(&stage(StageKind::Execute)).unwrap(),
            serde_json::to_string(&complete()).unwrap()
        );
        let parsed = parse_jsonl(&jsonl).unwrap();
        assert_eq!(parsed.len(), 2);
    }
}
