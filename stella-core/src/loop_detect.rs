//! Loop detection — pure synchronous analysis of recent tool calls and
//! the results they produced: plain synchronous functions over owned
//! data, easy to property-test, run by the step-driver alongside
//! compaction and budget eviction.
//!
//! A flat iteration cap alone burns the *entire* step budget before
//! giving up, even when the model got stuck after three steps. This
//! module gives the step-driver (`driver.rs`, which every CLI path
//! drives) a real, typed verdict it can act on early: steer or abort with
//! a clear reason instead of grinding to the cap.
//!
//! Two failure modes are detected, matching real agent stuck-loop
//! signatures:
//!
//! 1. **Exact repeat** — the same tool called with byte-identical input,
//!    over and over (`read_file` on the same path, `bash` re-running the
//!    same failing command).
//! 2. **Short cycle** — a fixed sequence of 2 to [`MAX_CYCLE_PERIOD`]
//!    distinct calls repeating with no other call interleaved
//!    (`read_file` → `edit_file` that keeps getting rejected →
//!    `read_file` again; or the period-3 read → failing edit → failing
//!    test grind). Invisible to exact-repeat detection because no single
//!    call repeats consecutively.
//!
//! **Progress is part of the loop definition.** A repeat or cycle only
//! counts when the *outputs* are byte-identical too: identical input with
//! identical output means the model gained no new information, which is
//! the actual pathology. Identical input with *changing* output is
//! legitimate work — polling a running process, re-reading a file another
//! call just modified, re-running a test whose failures are shrinking —
//! and must never be flagged. That is why the detector consumes
//! [`CallRecord`]s (call + result) rather than bare calls.
//!
//! Calls are compared by **tool name + input + output**, deliberately
//! ignoring `ToolCall::call_id`. `ToolCall` derives `PartialEq` over *all*
//! fields including `call_id`, which providers assign fresh per call — two
//! semantically identical calls almost never share a `call_id`, so using
//! derived equality here would silently never fire. `same_record` below is
//! the one place that distinction is made.

use stella_protocol::{ToolCall, ToolOutput};

/// Longest trailing cycle period the short-cycle detector considers.
/// Real stuck signatures observed so far are periods 2 and 3 (the
/// read → failing edit → failing test grind); 4 adds headroom without
/// scanning for long "cycles" that are really just varied work.
const MAX_CYCLE_PERIOD: usize = 4;

/// One tool call paired with the output it produced — the unit the
/// detector inspects. `output` is `None` while unresolved (the call never
/// ran, or its result message is gone from the window); an unresolved
/// output never matches anything, because progress cannot be ruled out
/// without seeing the result.
#[derive(Debug, Clone, PartialEq)]
pub struct CallRecord {
    pub call: ToolCall,
    pub output: Option<ToolOutput>,
}

/// Threshold configuration for [`detect_loop`]. `Default` gives sensible
/// starting values; callers (the step-driver) may tune per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopDetectionConfig {
    /// Consecutive identical (name + input + output) calls required to
    /// flag an exact-repeat loop. `0` or `1` disable exact-repeat
    /// detection — a single call can't be "repeated" by definition.
    pub exact_repeat_threshold: usize,
    /// Full cycles (of any period `2..=`[`MAX_CYCLE_PERIOD`]) required to
    /// flag a short-cycle loop. `0` disables short-cycle detection.
    pub short_cycle_repeats: usize,
}

impl Default for LoopDetectionConfig {
    /// Three consecutive identical calls, or three full cycles — enough to
    /// rule out coincidence without flagging a legitimately-repeated
    /// read-then-fix-then-verify pattern (which changes some output every
    /// pass and so never matches anyway). These thresholds are the PRIMARY
    /// stuck-turn defense and fire orders of magnitude before the
    /// step-driver's belt-and-suspenders backstop
    /// (`EngineConfig::max_steps`, 200 by default), so a stuck turn costs
    /// a handful of wasted calls, never a whole cap's worth.
    fn default() -> Self {
        Self {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 3,
        }
    }
}

/// Verdict returned by [`detect_loop`]. Never a bare bool — matching this
/// crate's convention of typed, inspectable outputs (`ToolOutput` in
/// `stella_protocol::tool` is never a bare string; `CompactionReport` in
/// `compaction.rs` is a named struct).
#[derive(Debug, Clone, PartialEq)]
pub enum LoopVerdict {
    /// No loop detected in the inspected window. The default/healthy
    /// verdict: empty history, history shorter than every configured
    /// threshold, genuinely varied history, and identical calls whose
    /// outputs kept changing (visible progress) all return this.
    NoLoop,
    /// The same tool call (name + byte-identical input) was made `count`
    /// times consecutively at the end of the inspected history, every time
    /// producing byte-identical output, at or above
    /// `LoopDetectionConfig::exact_repeat_threshold`.
    ExactRepeat {
        tool: String,
        input: serde_json::Value,
        count: usize,
    },
    /// A fixed sequence of `pattern.len()` calls (2 to
    /// [`MAX_CYCLE_PERIOD`], in cycle order — oldest position first)
    /// repeated with no other call interleaved and byte-identical outputs
    /// at every position, for `repeats` full cycles at the end of the
    /// inspected history, at or above
    /// `LoopDetectionConfig::short_cycle_repeats`.
    ShortCycle {
        pattern: Vec<ToolCall>,
        repeats: usize,
    },
}

impl LoopVerdict {
    /// `true` for any detected loop variant; `false` for `NoLoop`.
    pub fn is_loop(&self) -> bool {
        !matches!(self, LoopVerdict::NoLoop)
    }

    /// A human-readable evidence string for the driver to surface when it
    /// steers or aborts. `None` for `NoLoop`.
    pub fn evidence(&self) -> Option<String> {
        match self {
            LoopVerdict::NoLoop => None,
            LoopVerdict::ExactRepeat { tool, input, count } => Some(format!(
                "the same `{tool}` call with identical arguments repeated {count} times \
                 consecutively, producing byte-identical output every time (input: {input})"
            )),
            LoopVerdict::ShortCycle { pattern, repeats } => {
                let names: Vec<String> = pattern.iter().map(|c| format!("`{}`", c.name)).collect();
                Some(format!(
                    "calls cycled through {} for {repeats} cycles with byte-identical \
                     outputs and no progress",
                    names.join(" → ")
                ))
            }
        }
    }
}

/// Two records are "the same" for loop-detection purposes iff the tool
/// name, the JSON input, AND the produced output all match exactly.
/// Comparing name alone would false-positive on legitimate repeated calls
/// to the same tool with different arguments (`read_file` on two
/// different paths); comparing name + input alone would false-positive on
/// legitimate polling (`read_output` on the same handle returning new
/// bytes every call). An unresolved output (`None`) matches nothing —
/// including another `None` — because a loop can only be *proven* by two
/// observed identical outputs.
fn same_record(a: &CallRecord, b: &CallRecord) -> bool {
    a.call.name == b.call.name
        && a.call.input == b.call.input
        && match (&a.output, &b.output) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
}

/// Inspect the tail of recent tool calls for a non-progress loop.
/// `records` should be the recent window of [`CallRecord`]s in
/// chronological order (oldest first, most recent last) — the caller
/// decides how much history to hand in; a few dozen calls is plenty since
/// both checks only look at the trailing run.
///
/// Checks, in order:
/// 1. **Exact repeat** (see the module docs). Checked first: an exact
///    repeat of length `>= 2 * short_cycle_repeats` would otherwise also
///    satisfy a degenerate "cycle" of one call repeating against itself,
///    so exact-repeat takes precedence and the caller never has to
///    disentangle two overlapping classifications of the same evidence.
/// 2. **Short cycle** (see the module docs), shortest period first so the
///    tightest description of the evidence wins (a period-2 loop is never
///    reported as the period-4 loop it also technically is).
///
/// Never panics on any input — empty history, a single call, history
/// shorter than every threshold, and a zeroed-out `config` (which disables
/// both checks) all return `NoLoop` rather than indexing out of bounds.
pub fn detect_loop(records: &[CallRecord], config: LoopDetectionConfig) -> LoopVerdict {
    if let Some(verdict) = detect_exact_repeat(records, config.exact_repeat_threshold) {
        return verdict;
    }
    if let Some(verdict) = detect_short_cycle(records, config.short_cycle_repeats) {
        return verdict;
    }
    LoopVerdict::NoLoop
}

/// Count the trailing run of records identical (by [`same_record`]) to the
/// last record; report `ExactRepeat` if that run is `>= threshold`.
/// `threshold < 2` and empty `records` both return `None` (no detection).
fn detect_exact_repeat(records: &[CallRecord], threshold: usize) -> Option<LoopVerdict> {
    if threshold < 2 {
        return None;
    }
    let last = records.last()?;
    let count = records
        .iter()
        .rev()
        .take_while(|record| same_record(record, last))
        .count();
    if count >= threshold {
        Some(LoopVerdict::ExactRepeat {
            tool: last.call.name.clone(),
            input: last.call.input.clone(),
            count,
        })
    } else {
        None
    }
}

/// For each period `2..=`[`MAX_CYCLE_PERIOD`] (shortest first), count how
/// far the trailing history repeats its last `period` records; report
/// `ShortCycle` if any period spans `>= repeats_threshold` full cycles.
/// `repeats_threshold == 0`, history too short for every period, and a
/// candidate pattern that is itself an exact repeat (one record against
/// itself, not distinct calls) all return `None`.
fn detect_short_cycle(records: &[CallRecord], repeats_threshold: usize) -> Option<LoopVerdict> {
    if repeats_threshold == 0 {
        return None;
    }
    for period in 2..=MAX_CYCLE_PERIOD {
        // Reaching the threshold takes `period * repeats_threshold`
        // records; longer periods need strictly more, so stop entirely.
        if records.len() < period * repeats_threshold {
            break;
        }
        let pattern = &records[records.len() - period..];
        // A run of one record repeating against itself is exact-repeat's
        // territory, not a genuine cycle of distinct calls.
        if pattern
            .iter()
            .all(|record| same_record(record, &pattern[0]))
        {
            continue;
        }

        // Walk backward from the end: the record at reverse-offset `o`
        // must match the pattern position `period - 1 - (o % period)`
        // (the pattern itself is stored oldest-first). Count how many
        // records in a row satisfy that alternation.
        let mut matched = 0usize;
        for (offset, record) in records.iter().rev().enumerate() {
            let expected = &pattern[period - 1 - (offset % period)];
            if same_record(record, expected) {
                matched += 1;
            } else {
                break;
            }
        }

        let repeats = matched / period;
        if repeats >= repeats_threshold {
            return Some(LoopVerdict::ShortCycle {
                pattern: pattern.iter().map(|record| record.call.clone()).collect(),
                repeats,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn record(name: &str, input: serde_json::Value, output: Option<ToolOutput>) -> CallRecord {
        // Distinct `call_id` per invocation, on purpose — providers never
        // reuse call ids, and the detector must not depend on them.
        static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        CallRecord {
            call: ToolCall {
                call_id: format!("call_{id}"),
                name: name.into(),
                input,
            },
            output,
        }
    }

    fn call(name: &str, input: serde_json::Value, output: &str) -> CallRecord {
        record(
            name,
            input,
            Some(ToolOutput::Ok {
                content: output.into(),
            }),
        )
    }

    /// Re-reading an unchanged file: same input, same output — the classic
    /// no-progress ingredient.
    fn read(path: &str) -> CallRecord {
        call(
            "read_file",
            serde_json::json!({ "path": path }),
            "fn main() {}",
        )
    }

    /// An edit that keeps failing the same way: same input, same error.
    fn edit(path: &str) -> CallRecord {
        record(
            "edit_file",
            serde_json::json!({ "path": path, "old": "x", "new": "y" }),
            Some(ToolOutput::Error {
                message: "old text not found".into(),
            }),
        )
    }

    #[test]
    fn empty_history_is_never_a_loop() {
        let verdict = detect_loop(&[], LoopDetectionConfig::default());
        assert_eq!(verdict, LoopVerdict::NoLoop);
        assert!(!verdict.is_loop());
    }

    #[test]
    fn single_call_is_never_a_loop() {
        let records = vec![read("a.rs")];
        assert_eq!(
            detect_loop(&records, LoopDetectionConfig::default()),
            LoopVerdict::NoLoop
        );
    }

    #[test]
    fn history_shorter_than_exact_repeat_threshold_is_not_a_loop() {
        // Two identical calls, but the threshold requires three.
        let records = vec![read("a.rs"), read("a.rs")];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 100, // disable short-cycle for this test
        };
        assert_eq!(detect_loop(&records, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn exact_repeat_at_threshold_is_detected() {
        let records = vec![read("a.rs"), read("a.rs"), read("a.rs")];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 100,
        };
        let verdict = detect_loop(&records, config);
        assert_eq!(
            verdict,
            LoopVerdict::ExactRepeat {
                tool: "read_file".into(),
                input: serde_json::json!({ "path": "a.rs" }),
                count: 3,
            }
        );
        assert!(verdict.is_loop());
    }

    #[test]
    fn exact_repeat_above_threshold_reports_full_count() {
        // Five in a row, threshold 3 — the full count is reported, not
        // capped at the threshold.
        let records = vec![read("a.rs"); 5];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 100,
        };
        match detect_loop(&records, config) {
            LoopVerdict::ExactRepeat { count, .. } => assert_eq!(count, 5),
            other => panic!("expected ExactRepeat, got {other:?}"),
        }
    }

    #[test]
    fn identical_input_with_changing_output_is_not_a_loop() {
        // Polling a still-running process: `read_output` on the same
        // handle, no cursor field — the input is byte-identical every
        // time, but each poll returns new bytes. Visible progress, never a
        // loop, however long it goes on.
        let records: Vec<CallRecord> = (0..10)
            .map(|i| {
                call(
                    "read_output",
                    serde_json::json!({ "handle": "proc-5" }),
                    &format!("[{i}s] compiling stella-core..."),
                )
            })
            .collect();
        assert_eq!(
            detect_loop(&records, LoopDetectionConfig::default()),
            LoopVerdict::NoLoop
        );
    }

    #[test]
    fn identical_input_with_unresolved_output_is_not_a_loop() {
        // No result observed (the result message is gone from the window,
        // or the call never ran): progress cannot be ruled out, so
        // repetition alone is not evidence.
        let records: Vec<CallRecord> = (0..5)
            .map(|_| record("read_file", serde_json::json!({ "path": "a.rs" }), None))
            .collect();
        assert_eq!(
            detect_loop(&records, LoopDetectionConfig::default()),
            LoopVerdict::NoLoop
        );
    }

    #[test]
    fn different_arguments_to_the_same_tool_is_not_a_loop() {
        // Same tool name every time, but a different path each call — must
        // compare full input, not just tool name.
        let records = vec![read("a.rs"), read("b.rs"), read("c.rs"), read("d.rs")];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 100,
        };
        assert_eq!(detect_loop(&records, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn call_id_is_ignored_when_comparing_calls() {
        // `record()` assigns a fresh call_id every time; ToolCall's derived
        // PartialEq would see these as all-different. The detector must
        // still catch the repeat.
        let records: Vec<CallRecord> = (0..3).map(|_| read("a.rs")).collect();
        let ids: std::collections::HashSet<_> =
            records.iter().map(|r| r.call.call_id.clone()).collect();
        assert_eq!(ids.len(), 3, "test fixture sanity: call_ids must differ");

        let config = LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 100,
        };
        assert!(detect_loop(&records, config).is_loop());
    }

    #[test]
    fn short_cycle_below_threshold_is_not_a_loop() {
        // Only two full A-B cycles; threshold requires three.
        let records = vec![read("a.rs"), edit("a.rs"), read("a.rs"), edit("a.rs")];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 100, // disable exact-repeat for this test
            short_cycle_repeats: 3,
        };
        assert_eq!(detect_loop(&records, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn short_cycle_at_threshold_is_detected() {
        // read, edit, read, edit, read, edit — 3 full cycles, the "read
        // file / edit rejected or no-op / read again" failure mode.
        let records = vec![
            read("a.rs"),
            edit("a.rs"),
            read("a.rs"),
            edit("a.rs"),
            read("a.rs"),
            edit("a.rs"),
        ];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 100,
            short_cycle_repeats: 3,
        };
        let verdict = detect_loop(&records, config);
        match &verdict {
            LoopVerdict::ShortCycle { pattern, repeats } => {
                assert_eq!(*repeats, 3);
                let names: Vec<&str> = pattern.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, ["read_file", "edit_file"]);
            }
            other => panic!("expected ShortCycle, got {other:?}"),
        }
        assert!(verdict.is_loop());
    }

    #[test]
    fn short_cycle_above_threshold_reports_full_repeat_count() {
        let mut records = Vec::new();
        for _ in 0..5 {
            records.push(read("a.rs"));
            records.push(edit("a.rs"));
        }
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 100,
            short_cycle_repeats: 3,
        };
        match detect_loop(&records, config) {
            LoopVerdict::ShortCycle { repeats, .. } => assert_eq!(repeats, 5),
            other => panic!("expected ShortCycle, got {other:?}"),
        }
    }

    #[test]
    fn two_distinct_calls_alternating_with_changing_outputs_is_not_a_loop() {
        // The "correct" polling alternation: read_output / bash sleep,
        // read_output / bash sleep — identical inputs at every position,
        // but each poll returns new bytes. Progress, not a cycle.
        let mut records = Vec::new();
        for i in 0..6 {
            records.push(call(
                "read_output",
                serde_json::json!({ "handle": "proc-5" }),
                &format!("[{i}s] compiling..."),
            ));
            records.push(call("bash", serde_json::json!({ "cmd": "sleep 5" }), ""));
        }
        assert_eq!(
            detect_loop(&records, LoopDetectionConfig::default()),
            LoopVerdict::NoLoop
        );
    }

    #[test]
    fn period_three_cycle_with_identical_outputs_is_detected() {
        // The most common real stuck signature: read → failing edit →
        // failing test, over and over, nothing changing.
        let cycle = || {
            vec![
                read("a.rs"),
                edit("a.rs"),
                call(
                    "bash",
                    serde_json::json!({ "cmd": "cargo test" }),
                    "2 failed",
                ),
            ]
        };
        let records: Vec<CallRecord> = (0..3).flat_map(|_| cycle()).collect();
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 100,
            short_cycle_repeats: 3,
        };
        let verdict = detect_loop(&records, config);
        match &verdict {
            LoopVerdict::ShortCycle { pattern, repeats } => {
                assert_eq!(*repeats, 3);
                let names: Vec<&str> = pattern.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, ["read_file", "edit_file", "bash"]);
            }
            other => panic!("expected a period-3 ShortCycle, got {other:?}"),
        }
    }

    #[test]
    fn period_three_cycle_with_differing_outputs_is_not_a_loop() {
        // The same A, B, C shape — but the test output improves every
        // cycle (2 failed → 1 failed → 0 failed). That is a productive
        // fix loop, not a stuck one.
        let cycle = |failures: usize| {
            vec![
                read("a.rs"),
                edit("a.rs"),
                call(
                    "bash",
                    serde_json::json!({ "cmd": "cargo test" }),
                    &format!("{failures} failed"),
                ),
            ]
        };
        let records: Vec<CallRecord> = (0..3).rev().flat_map(cycle).collect();
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 100,
            short_cycle_repeats: 2,
        };
        assert_eq!(detect_loop(&records, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn period_four_cycle_with_identical_outputs_is_detected() {
        let cycle = || {
            vec![
                read("a.rs"),
                read("b.rs"),
                edit("a.rs"),
                call(
                    "bash",
                    serde_json::json!({ "cmd": "cargo test" }),
                    "2 failed",
                ),
            ]
        };
        let records: Vec<CallRecord> = (0..2).flat_map(|_| cycle()).collect();
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 100,
            short_cycle_repeats: 2,
        };
        match detect_loop(&records, config) {
            LoopVerdict::ShortCycle { pattern, repeats } => {
                assert_eq!(repeats, 2);
                assert_eq!(pattern.len(), 4);
            }
            other => panic!("expected a period-4 ShortCycle, got {other:?}"),
        }
    }

    #[test]
    fn period_five_cycle_is_beyond_the_detector() {
        // Five distinct calls repeating: outside MAX_CYCLE_PERIOD, left to
        // the step cap. Pins the k <= 4 bound.
        let cycle = || {
            vec![
                read("a.rs"),
                read("b.rs"),
                read("c.rs"),
                edit("a.rs"),
                call(
                    "bash",
                    serde_json::json!({ "cmd": "cargo test" }),
                    "2 failed",
                ),
            ]
        };
        let records: Vec<CallRecord> = (0..3).flat_map(|_| cycle()).collect();
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 100,
            short_cycle_repeats: 2,
        };
        assert_eq!(detect_loop(&records, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn identical_calls_repeated_are_not_misreported_as_a_short_cycle() {
        // Six identical calls: exact-repeat detection is disabled here so
        // we can prove detect_short_cycle's own all-same guard holds at
        // every period — this must stay NoLoop, not a degenerate
        // ShortCycle{X, X}.
        let records = vec![read("a.rs"); 6];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 0, // disabled
            short_cycle_repeats: 1,
        };
        assert_eq!(detect_loop(&records, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn drift_attributed_edit_recovery_is_not_a_loop() {
        // #331: when `edit_file` attributes a match failure to an
        // out-of-band file change, the error embeds the CURRENT content —
        // so as long as the file keeps changing, the outputs differ and the
        // legitimate read→edit-retry recovery is progress by construction.
        // Only when the file stops changing (and the model keeps replaying
        // the same failing edit verbatim) do outputs repeat byte-identically
        // and detection rightly fires. Pins the contract that the drift echo
        // is what keeps recovery progress-eligible.
        let drift_fail = |content: &str| {
            record(
                "edit_file",
                serde_json::json!({ "path": "a.rs", "old": "x", "new": "y" }),
                Some(ToolOutput::Error {
                    message: format!("old_string not found — the file CHANGED; current: {content}"),
                }),
            )
        };
        let read_v =
            |content: &str| call("read_file", serde_json::json!({ "path": "a.rs" }), content);
        let records = vec![
            read_v("v1"),
            drift_fail("v2"),
            read_v("v2"),
            drift_fail("v3"),
            read_v("v3"),
            drift_fail("v4"),
        ];
        assert_eq!(
            detect_loop(&records, LoopDetectionConfig::default()),
            LoopVerdict::NoLoop
        );
    }

    #[test]
    fn healthy_varied_sequence_is_not_a_loop() {
        // A realistic productive trajectory: no false positives.
        let records = vec![
            read("src/lib.rs"),
            call(
                "grep",
                serde_json::json!({ "pattern": "fn run_turn" }),
                "src/agent.rs:42",
            ),
            read("src/agent.rs"),
            edit("src/agent.rs"),
            call(
                "bash",
                serde_json::json!({ "cmd": "cargo test -p stella-cli" }),
                "2 failed",
            ),
            read("src/agent.rs"),
            edit("src/agent.rs"),
            call(
                "bash",
                serde_json::json!({ "cmd": "cargo test -p stella-cli -- --nocapture" }),
                "1 failed",
            ),
        ];
        assert_eq!(
            detect_loop(&records, LoopDetectionConfig::default()),
            LoopVerdict::NoLoop
        );
    }

    #[test]
    fn exact_repeat_takes_precedence_over_a_would_be_cycle_read() {
        // The trailing three calls are an exact repeat; that a short-cycle
        // check might also find something (given a different config) is
        // irrelevant — exact-repeat is checked first and wins.
        let records = vec![edit("a.rs"), read("a.rs"), read("a.rs"), read("a.rs")];
        let config = LoopDetectionConfig::default(); // 3/3
        match detect_loop(&records, config) {
            LoopVerdict::ExactRepeat { count, tool, .. } => {
                assert_eq!(count, 3);
                assert_eq!(tool, "read_file");
            }
            other => panic!("expected ExactRepeat to win, got {other:?}"),
        }
    }

    #[test]
    fn zero_or_one_exact_repeat_threshold_disables_that_check() {
        let records = vec![read("a.rs"); 10];
        for threshold in [0, 1] {
            let config = LoopDetectionConfig {
                exact_repeat_threshold: threshold,
                short_cycle_repeats: 0, // also disabled, so overall NoLoop
            };
            assert_eq!(
                detect_loop(&records, config),
                LoopVerdict::NoLoop,
                "threshold {threshold} should disable exact-repeat detection"
            );
        }
    }

    #[test]
    fn zero_short_cycle_repeats_disables_that_check() {
        let records = vec![
            read("a.rs"),
            edit("a.rs"),
            read("a.rs"),
            edit("a.rs"),
            read("a.rs"),
            edit("a.rs"),
        ];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 0,
            short_cycle_repeats: 0,
        };
        assert_eq!(detect_loop(&records, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn evidence_is_none_for_no_loop() {
        assert_eq!(LoopVerdict::NoLoop.evidence(), None);
    }

    #[test]
    fn evidence_describes_exact_repeat() {
        let verdict = LoopVerdict::ExactRepeat {
            tool: "read_file".into(),
            input: serde_json::json!({ "path": "a.rs" }),
            count: 4,
        };
        let evidence = verdict.evidence().expect("loop verdict has evidence");
        assert!(evidence.contains("read_file"));
        assert!(evidence.contains('4'));
    }

    #[test]
    fn evidence_describes_short_cycle() {
        let verdict = LoopVerdict::ShortCycle {
            pattern: vec![read("a.rs").call, edit("a.rs").call],
            repeats: 3,
        };
        let evidence = verdict.evidence().expect("loop verdict has evidence");
        assert!(evidence.contains("read_file"));
        assert!(evidence.contains("edit_file"));
        assert!(evidence.contains('3'));
    }

    /// Small, deliberately overlapping alphabet of names/inputs/outputs so
    /// property-test runs actually exercise repeats and cycles instead of
    /// almost always generating trivially-varied (and thus trivially
    /// NoLoop) sequences. Includes unresolved (`None`) outputs.
    fn arb_call_record() -> impl Strategy<Value = CallRecord> {
        (0..3usize, 0..2usize, 0..3usize).prop_map(|(name_idx, input_idx, output_idx)| {
            let names = ["read_file", "edit_file", "bash"];
            let inputs = [
                serde_json::json!({ "path": "a.rs" }),
                serde_json::json!({ "path": "b.rs" }),
            ];
            let outputs = [
                Some(ToolOutput::Ok {
                    content: "ok".into(),
                }),
                Some(ToolOutput::Error {
                    message: "boom".into(),
                }),
                None,
            ];
            record(
                names[name_idx],
                inputs[input_idx].clone(),
                outputs[output_idx].clone(),
            )
        })
    }

    proptest! {
        /// Property: `detect_loop` never panics or indexes out of bounds,
        /// for any history length and any threshold configuration
        /// (including the degenerate `0` thresholds) — required by the
        /// "runs on live untrusted model output" quality bar.
        #[test]
        fn detect_loop_never_panics(
            records in proptest::collection::vec(arb_call_record(), 0..16),
            exact_repeat_threshold in 0usize..8,
            short_cycle_repeats in 0usize..8,
        ) {
            let config = LoopDetectionConfig { exact_repeat_threshold, short_cycle_repeats };
            let verdict = detect_loop(&records, config);
            // Whatever the verdict, `is_loop`/`evidence` must not panic either.
            let _ = verdict.is_loop();
            let _ = verdict.evidence();
        }

        /// Property: history shorter than both thresholds is always
        /// `NoLoop` — there's no way for either check to have enough
        /// evidence (the shortest cycle period is 2).
        #[test]
        fn short_history_is_always_no_loop(
            records in proptest::collection::vec(arb_call_record(), 0..12),
            exact_repeat_threshold in 2usize..8,
            short_cycle_repeats in 1usize..8,
        ) {
            if records.len() < exact_repeat_threshold && records.len() < 2 * short_cycle_repeats {
                let config = LoopDetectionConfig { exact_repeat_threshold, short_cycle_repeats };
                prop_assert_eq!(detect_loop(&records, config), LoopVerdict::NoLoop);
            }
        }

        /// Property: when every output in the history is unique, nothing
        /// is ever flagged (at meaningful thresholds — a threshold below 2
        /// is disabled or degenerate). Unique outputs = every call
        /// produced new information = progress by definition; this is the
        /// class-level guarantee that legitimate polling can never trip
        /// the detector, whatever the inputs look like.
        #[test]
        fn unique_outputs_are_never_a_loop(
            records in proptest::collection::vec(arb_call_record(), 0..16),
            exact_repeat_threshold in 2usize..8,
            short_cycle_repeats in 2usize..8,
        ) {
            let records: Vec<CallRecord> = records
                .into_iter()
                .enumerate()
                .map(|(i, mut record)| {
                    record.output = Some(ToolOutput::Ok { content: format!("output {i}") });
                    record
                })
                .collect();
            let config = LoopDetectionConfig { exact_repeat_threshold, short_cycle_repeats };
            prop_assert_eq!(detect_loop(&records, config), LoopVerdict::NoLoop);
        }
    }
}
