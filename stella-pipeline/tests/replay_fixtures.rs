//! Golden-trajectory replay harness, driven against synthetic fixtures
//! (`03-plan.md` Phase 4 item 3). These fixtures are **synthetic** streams
//! that exercise the harness's invariants and its structural differ; recording
//! real TS-engine trajectories on fixed tasks and replaying the Rust stack
//! against them is the documented next step (see `replay.rs`'s module doc) —
//! deliberately not faked here.

use std::fs;
use std::path::PathBuf;

use stella_pipeline::replay::{parse_jsonl, streams_equivalent, structural_diff, validate_stream};

fn fixture(name: &str) -> String {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "fixtures", name]
        .iter()
        .collect();
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading fixture {path:?}: {e}"))
}

#[test]
fn a_well_formed_trajectory_passes_every_invariant() {
    let events = parse_jsonl(&fixture("single_task_flip.jsonl")).expect("fixture parses");
    let violations = validate_stream(&events);
    assert!(
        violations.is_empty(),
        "expected a clean stream, got violations: {violations:?}"
    );
}

#[test]
fn two_runs_of_the_same_flow_are_structurally_equivalent() {
    // Same staged flow, different providers/costs/ids/summaries — the golden
    // replay must treat them as equivalent (kinds + order, volatile fields
    // ignored).
    let run = parse_jsonl(&fixture("single_task_flip.jsonl")).unwrap();
    let reference = parse_jsonl(&fixture("single_task_flip_reference.jsonl")).unwrap();
    let diff = structural_diff(&run, &reference);
    assert!(
        streams_equivalent(&run, &reference),
        "structurally-equal runs must not diff; got: {diff:?}"
    );
}

#[test]
fn a_judge_escalation_diverges_from_a_deterministic_pass() {
    // The deterministic-flip path skips the judge; the escalation path has a
    // `judge` stage and a non-deterministic verdict — they must diverge.
    let deterministic = parse_jsonl(&fixture("single_task_flip.jsonl")).unwrap();
    let escalation = parse_jsonl(&fixture("judge_escalation.jsonl")).unwrap();
    let diff = structural_diff(&deterministic, &escalation);
    assert!(
        !diff.is_empty(),
        "a submit-fast run and a judge-escalation run must be structurally distinct"
    );
    // And the escalation stream is itself well-formed.
    assert!(validate_stream(&escalation).is_empty());
}

#[test]
fn a_torn_writer_tail_is_tolerated_not_fatal() {
    // A crashed writer left a partial final line; the reader keeps the clean
    // prefix (L-T1).
    let events = parse_jsonl(&fixture("torn_tail.jsonl")).expect("torn tail must not fail parsing");
    assert_eq!(events.len(), 3, "the partial final line is dropped");
    // The recovered prefix is still a legal (if unterminated) stage sequence.
    assert!(validate_stream(&events).is_empty());
}
