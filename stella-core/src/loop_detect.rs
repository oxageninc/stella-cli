//! Loop detection — pure synchronous analysis of recent tool calls:
//! plain synchronous functions over owned data, easy to property-test,
//! run by the step-driver alongside compaction and budget eviction.
//!
//! A flat iteration cap alone burns the *entire* step budget before
//! giving up, even when the model got stuck after three steps. This
//! module gives the step-driver (`driver.rs`, which every CLI path
//! drives) a real, typed verdict it can act on early: abort with a clear
//! reason instead of grinding to the cap.
//!
//! Two failure modes are detected, matching real agent stuck-loop
//! signatures:
//!
//! 1. **Exact repeat** — the same tool called with byte-identical input,
//!    over and over (`read_file` on the same path, `bash` re-running the
//!    same failing command).
//! 2. **Short cycle** — exactly two distinct calls alternating with no
//!    progress (`read_file`, `edit_file` that gets rejected or no-ops,
//!    `read_file` again, ...). This is the "same edit keeps failing"
//!    pattern, invisible to exact-repeat detection because no single call
//!    repeats consecutively.
//!
//! Both checks compare calls by **tool name + input**, deliberately
//! ignoring `ToolCall::call_id`. `ToolCall` derives `PartialEq` over *all*
//! fields including `call_id`, which providers assign fresh per call — two
//! semantically identical calls almost never share a `call_id`, so using
//! derived equality here would silently never fire. `same_call` below is
//! the one place that distinction is made.

use stella_protocol::ToolCall;

/// Threshold configuration for [`detect_loop`]. `Default` gives sensible
/// starting values; callers (the step-driver) may tune per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopDetectionConfig {
    /// Consecutive identical (name + input) calls required to flag an
    /// exact-repeat loop. `0` or `1` disable exact-repeat detection — a
    /// single call can't be "repeated" by definition.
    pub exact_repeat_threshold: usize,
    /// Full A→B alternation cycles required to flag a short-cycle loop.
    /// `0` disables short-cycle detection.
    pub short_cycle_repeats: usize,
}

impl Default for LoopDetectionConfig {
    /// Three consecutive identical calls, or three full A-B cycles (12
    /// calls of the 20-iteration cap in the interim CLI loop) — enough to
    /// rule out coincidence without flagging a legitimately-repeated
    /// read-then-fix-then-verify pattern.
    fn default() -> Self {
        Self {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 3,
        }
    }
}

/// The two distinct calls that make up a detected short cycle, in
/// alternation order: history reads `..., a, b, a, b, a, b` (most recent
/// call last).
#[derive(Debug, Clone, PartialEq)]
pub struct CyclePair {
    pub a: ToolCall,
    pub b: ToolCall,
}

/// Verdict returned by [`detect_loop`]. Never a bare bool — matching this
/// crate's convention of typed, inspectable outputs (`ToolOutput` in
/// `stella_protocol::tool` is never a bare string; `CompactionReport` in
/// `compaction.rs` is a named struct).
#[derive(Debug, Clone, PartialEq)]
pub enum LoopVerdict {
    /// No loop detected in the inspected window. The default/healthy
    /// verdict: empty history, history shorter than every configured
    /// threshold, and genuinely varied history all return this.
    NoLoop,
    /// The same tool call (name + byte-identical input) was made `count`
    /// times consecutively at the end of the inspected history, at or
    /// above `LoopDetectionConfig::exact_repeat_threshold`.
    ExactRepeat {
        tool: String,
        input: serde_json::Value,
        count: usize,
    },
    /// Two distinct calls alternated with no other call interleaved, for
    /// `repeats` full A→B cycles at the end of the inspected history, at
    /// or above `LoopDetectionConfig::short_cycle_repeats`.
    ShortCycle { pattern: CyclePair, repeats: usize },
}

impl LoopVerdict {
    /// `true` for any detected loop variant; `false` for `NoLoop`.
    pub fn is_loop(&self) -> bool {
        !matches!(self, LoopVerdict::NoLoop)
    }

    /// A human-readable evidence string for the driver to surface when it
    /// aborts ( Phase 2 step 4: "the driver can intervene").
    /// `None` for `NoLoop`.
    pub fn evidence(&self) -> Option<String> {
        match self {
            LoopVerdict::NoLoop => None,
            LoopVerdict::ExactRepeat { tool, input, count } => Some(format!(
                "the same `{tool}` call with identical arguments repeated {count} times \
                 consecutively (input: {input})"
            )),
            LoopVerdict::ShortCycle { pattern, repeats } => Some(format!(
                "calls alternated between `{}` and `{}` for {repeats} cycles with no progress",
                pattern.a.name, pattern.b.name
            )),
        }
    }
}

/// Two calls are "the same" for loop-detection purposes iff both the tool
/// name and the JSON input match exactly. Comparing name alone would
/// false-positive on legitimate repeated calls to the same tool with
/// different arguments (e.g. `read_file` on two different paths); this is
/// the full-input comparison the loop detector requires.
fn same_call(a: &ToolCall, b: &ToolCall) -> bool {
    a.name == b.name && a.input == b.input
}

/// Inspect the tail of recent tool calls for a non-progress loop. `calls`
/// should be the recent window of `ToolCall`s in chronological order
/// (oldest first, most recent last) — the caller decides how much history
/// to hand in; a few dozen calls is plenty since both checks only look at
/// the trailing run.
///
/// Checks, in order:
/// 1. **Exact repeat** (see the module docs). Checked first: an exact
///    repeat of length `>= 2 * short_cycle_repeats` would otherwise also
///    satisfy a degenerate "cycle" of one call repeating against itself,
///    so exact-repeat takes precedence and the caller never has to
///    disentangle two overlapping classifications of the same evidence.
/// 2. **Short cycle** (see the module docs).
///
/// Never panics on any input — empty history, a single call, history
/// shorter than every threshold, and a zeroed-out `config` (which disables
/// both checks) all return `NoLoop` rather than indexing out of bounds.
pub fn detect_loop(calls: &[ToolCall], config: LoopDetectionConfig) -> LoopVerdict {
    if let Some(verdict) = detect_exact_repeat(calls, config.exact_repeat_threshold) {
        return verdict;
    }
    if let Some(verdict) = detect_short_cycle(calls, config.short_cycle_repeats) {
        return verdict;
    }
    LoopVerdict::NoLoop
}

/// Count the trailing run of calls identical (by [`same_call`]) to the
/// last call; report `ExactRepeat` if that run is `>= threshold`.
/// `threshold < 2` and empty `calls` both return `None` (no detection).
fn detect_exact_repeat(calls: &[ToolCall], threshold: usize) -> Option<LoopVerdict> {
    if threshold < 2 {
        return None;
    }
    let last = calls.last()?;
    let count = calls
        .iter()
        .rev()
        .take_while(|call| same_call(call, last))
        .count();
    if count >= threshold {
        Some(LoopVerdict::ExactRepeat {
            tool: last.name.clone(),
            input: last.input.clone(),
            count,
        })
    } else {
        None
    }
}

/// Count the trailing alternation between the last two calls (`a`, `b`,
/// distinct by [`same_call`]); report `ShortCycle` if that alternation
/// spans `>= repeats_threshold` full cycles. `repeats_threshold == 0`,
/// fewer than 2 calls, and a trailing pair that is itself an exact repeat
/// (not two *distinct* calls) all return `None`.
fn detect_short_cycle(calls: &[ToolCall], repeats_threshold: usize) -> Option<LoopVerdict> {
    if repeats_threshold == 0 || calls.len() < 2 {
        return None;
    }
    let last = calls.last()?;
    let prev = &calls[calls.len() - 2];
    if same_call(last, prev) {
        // A run of one call repeating against itself is exact-repeat's
        // territory, not a genuine 2-distinct-call cycle.
        return None;
    }

    // Walk backward from the end, alternating the expected call: the most
    // recent call (offset 0, 2, 4, ...) must match `last`; the one before
    // it (offset 1, 3, 5, ...) must match `prev`. Count how many calls in
    // a row satisfy that alternation.
    let mut matched = 0usize;
    for (offset, call) in calls.iter().rev().enumerate() {
        let expected = if offset % 2 == 0 { last } else { prev };
        if same_call(call, expected) {
            matched += 1;
        } else {
            break;
        }
    }

    let repeats = matched / 2;
    if repeats >= repeats_threshold {
        Some(LoopVerdict::ShortCycle {
            pattern: CyclePair {
                a: prev.clone(),
                b: last.clone(),
            },
            repeats,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn call(name: &str, input: serde_json::Value) -> ToolCall {
        // Distinct `call_id` per invocation, on purpose — providers never
        // reuse call ids, and the detector must not depend on them.
        static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ToolCall {
            call_id: format!("call_{id}"),
            name: name.into(),
            input,
        }
    }

    fn read(path: &str) -> ToolCall {
        call("read_file", serde_json::json!({ "path": path }))
    }

    fn edit(path: &str) -> ToolCall {
        call(
            "edit_file",
            serde_json::json!({ "path": path, "old": "x", "new": "y" }),
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
        let calls = vec![read("a.rs")];
        assert_eq!(
            detect_loop(&calls, LoopDetectionConfig::default()),
            LoopVerdict::NoLoop
        );
    }

    #[test]
    fn history_shorter_than_exact_repeat_threshold_is_not_a_loop() {
        // Two identical calls, but the threshold requires three.
        let calls = vec![read("a.rs"), read("a.rs")];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 100, // disable short-cycle for this test
        };
        assert_eq!(detect_loop(&calls, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn exact_repeat_at_threshold_is_detected() {
        let calls = vec![read("a.rs"), read("a.rs"), read("a.rs")];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 100,
        };
        let verdict = detect_loop(&calls, config);
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
        let calls = vec![read("a.rs"); 5];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 100,
        };
        match detect_loop(&calls, config) {
            LoopVerdict::ExactRepeat { count, .. } => assert_eq!(count, 5),
            other => panic!("expected ExactRepeat, got {other:?}"),
        }
    }

    #[test]
    fn different_arguments_to_the_same_tool_is_not_a_loop() {
        // Same tool name every time, but a different path each call — must
        // compare full input, not just tool name.
        let calls = vec![read("a.rs"), read("b.rs"), read("c.rs"), read("d.rs")];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 100,
        };
        assert_eq!(detect_loop(&calls, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn call_id_is_ignored_when_comparing_calls() {
        // `call()` assigns a fresh call_id every time; ToolCall's derived
        // PartialEq would see these as all-different. The detector must
        // still catch the repeat.
        let calls: Vec<ToolCall> = (0..3).map(|_| read("a.rs")).collect();
        let ids: std::collections::HashSet<_> = calls.iter().map(|c| c.call_id.clone()).collect();
        assert_eq!(ids.len(), 3, "test fixture sanity: call_ids must differ");

        let config = LoopDetectionConfig {
            exact_repeat_threshold: 3,
            short_cycle_repeats: 100,
        };
        assert!(detect_loop(&calls, config).is_loop());
    }

    #[test]
    fn short_cycle_below_threshold_is_not_a_loop() {
        // Only two full A-B cycles; threshold requires three.
        let calls = vec![read("a.rs"), edit("a.rs"), read("a.rs"), edit("a.rs")];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 100, // disable exact-repeat for this test
            short_cycle_repeats: 3,
        };
        assert_eq!(detect_loop(&calls, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn short_cycle_at_threshold_is_detected() {
        // read, edit, read, edit, read, edit — 3 full cycles, the "read
        // file / edit rejected or no-op / read again" failure mode.
        let calls = vec![
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
        let verdict = detect_loop(&calls, config);
        match &verdict {
            LoopVerdict::ShortCycle { pattern, repeats } => {
                assert_eq!(*repeats, 3);
                assert_eq!(pattern.a.name, "read_file");
                assert_eq!(pattern.b.name, "edit_file");
            }
            other => panic!("expected ShortCycle, got {other:?}"),
        }
        assert!(verdict.is_loop());
    }

    #[test]
    fn short_cycle_above_threshold_reports_full_repeat_count() {
        let mut calls = Vec::new();
        for _ in 0..5 {
            calls.push(read("a.rs"));
            calls.push(edit("a.rs"));
        }
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 100,
            short_cycle_repeats: 3,
        };
        match detect_loop(&calls, config) {
            LoopVerdict::ShortCycle { repeats, .. } => assert_eq!(repeats, 5),
            other => panic!("expected ShortCycle, got {other:?}"),
        }
    }

    #[test]
    fn alternating_three_distinct_calls_is_not_a_short_cycle() {
        // A, B, C, A, B, C — a 3-cycle, not the 2-distinct-call pattern
        // this detector targets.
        let calls = vec![
            read("a.rs"),
            edit("a.rs"),
            call("bash", serde_json::json!({ "cmd": "test" })),
            read("a.rs"),
            edit("a.rs"),
            call("bash", serde_json::json!({ "cmd": "test" })),
        ];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 100,
            short_cycle_repeats: 2,
        };
        assert_eq!(detect_loop(&calls, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn identical_calls_repeated_are_not_misreported_as_a_short_cycle() {
        // Six identical calls: exact-repeat detection is disabled here so
        // we can prove detect_short_cycle's own a==b guard holds — this
        // must stay NoLoop, not a degenerate ShortCycle{a: X, b: X}.
        let calls = vec![read("a.rs"); 6];
        let config = LoopDetectionConfig {
            exact_repeat_threshold: 0, // disabled
            short_cycle_repeats: 1,
        };
        assert_eq!(detect_loop(&calls, config), LoopVerdict::NoLoop);
    }

    #[test]
    fn healthy_varied_sequence_is_not_a_loop() {
        // A realistic productive trajectory: no false positives.
        let calls = vec![
            read("src/lib.rs"),
            call("grep", serde_json::json!({ "pattern": "fn run_turn" })),
            read("src/agent.rs"),
            edit("src/agent.rs"),
            call(
                "bash",
                serde_json::json!({ "cmd": "cargo test -p stella-cli" }),
            ),
            read("src/agent.rs"),
            edit("src/agent.rs"),
            call(
                "bash",
                serde_json::json!({ "cmd": "cargo test -p stella-cli -- --nocapture" }),
            ),
        ];
        assert_eq!(
            detect_loop(&calls, LoopDetectionConfig::default()),
            LoopVerdict::NoLoop
        );
    }

    #[test]
    fn exact_repeat_takes_precedence_over_a_would_be_cycle_read() {
        // The trailing three calls are an exact repeat; that a short-cycle
        // check might also find something (given a different config) is
        // irrelevant — exact-repeat is checked first and wins.
        let calls = vec![edit("a.rs"), read("a.rs"), read("a.rs"), read("a.rs")];
        let config = LoopDetectionConfig::default(); // 3/3
        match detect_loop(&calls, config) {
            LoopVerdict::ExactRepeat { count, tool, .. } => {
                assert_eq!(count, 3);
                assert_eq!(tool, "read_file");
            }
            other => panic!("expected ExactRepeat to win, got {other:?}"),
        }
    }

    #[test]
    fn zero_or_one_exact_repeat_threshold_disables_that_check() {
        let calls = vec![read("a.rs"); 10];
        for threshold in [0, 1] {
            let config = LoopDetectionConfig {
                exact_repeat_threshold: threshold,
                short_cycle_repeats: 0, // also disabled, so overall NoLoop
            };
            assert_eq!(
                detect_loop(&calls, config),
                LoopVerdict::NoLoop,
                "threshold {threshold} should disable exact-repeat detection"
            );
        }
    }

    #[test]
    fn zero_short_cycle_repeats_disables_that_check() {
        let calls = vec![
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
        assert_eq!(detect_loop(&calls, config), LoopVerdict::NoLoop);
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
            pattern: CyclePair {
                a: read("a.rs"),
                b: edit("a.rs"),
            },
            repeats: 3,
        };
        let evidence = verdict.evidence().expect("loop verdict has evidence");
        assert!(evidence.contains("read_file"));
        assert!(evidence.contains("edit_file"));
        assert!(evidence.contains('3'));
    }

    /// Small, deliberately overlapping alphabet of names/inputs so
    /// property-test runs actually exercise repeats and cycles instead of
    /// almost always generating trivially-varied (and thus trivially
    /// NoLoop) sequences.
    fn arb_tool_call() -> impl Strategy<Value = ToolCall> {
        (0..3usize, 0..2usize).prop_map(|(name_idx, input_idx)| {
            let names = ["read_file", "edit_file", "bash"];
            let inputs = [
                serde_json::json!({ "path": "a.rs" }),
                serde_json::json!({ "path": "b.rs" }),
            ];
            call(names[name_idx], inputs[input_idx].clone())
        })
    }

    proptest! {
        /// Property: `detect_loop` never panics or indexes out of bounds,
        /// for any history length and any threshold configuration
        /// (including the degenerate `0` thresholds) — required by the
        /// "runs on live untrusted model output" quality bar.
        #[test]
        fn detect_loop_never_panics(
            calls in proptest::collection::vec(arb_tool_call(), 0..16),
            exact_repeat_threshold in 0usize..8,
            short_cycle_repeats in 0usize..8,
        ) {
            let config = LoopDetectionConfig { exact_repeat_threshold, short_cycle_repeats };
            let verdict = detect_loop(&calls, config);
            // Whatever the verdict, `is_loop`/`evidence` must not panic either.
            let _ = verdict.is_loop();
            let _ = verdict.evidence();
        }

        /// Property: history shorter than both thresholds is always
        /// `NoLoop` — there's no way for either check to have enough
        /// evidence.
        #[test]
        fn short_history_is_always_no_loop(
            calls in proptest::collection::vec(arb_tool_call(), 0..12),
            exact_repeat_threshold in 2usize..8,
            short_cycle_repeats in 1usize..8,
        ) {
            if calls.len() < exact_repeat_threshold && calls.len() < 2 * short_cycle_repeats {
                let config = LoopDetectionConfig { exact_repeat_threshold, short_cycle_repeats };
                prop_assert_eq!(detect_loop(&calls, config), LoopVerdict::NoLoop);
            }
        }
    }
}
