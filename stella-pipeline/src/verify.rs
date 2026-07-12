//! Deterministic-first verification (L-E11): the design that stops model
//! judges from rubber-stamping plausible-but-unverified work. Three pure
//! pieces live here — the flip-oracle state machine, the evidence ladder, and
//! the judge-response parsing + heuristic fallback. The async parts (running
//! the test command, calling the judge model) live in [`crate::pipeline`];
//! everything in this module is a synchronous function over owned data.
//!
//! # The flip oracle ([`FlipOracle`])
//!
//! Only a **fail→pass flip of the same normalized test command** counts as
//! verification. A test that never failed proves nothing; a pass on a
//! *different* command proves nothing. The oracle is a `none → failing →
//! flipped` state machine keyed on a normalized command string: it locks onto
//! the first command it sees *fail*, and only a later *pass of that same
//! normalized command* moves it to flipped. This structurally excludes the
//! "it passed, ship it" false positive.
//!
//! # The evidence ladder ([`ladder_decision`])
//!
//! With the flip result plus touched-tests status and diff size, the ladder
//! decides — *before any model judge runs*:
//! - **submit fast** (judge skipped) when flip + touched-tests-green + diff
//!   within budget all hold;
//! - **revise** on a clear failure (touched tests red);
//! - **escalate to the model judge** only on genuinely inconclusive evidence.
//!
//! Linters and typecheckers are deliberately **excluded** from the flip
//! oracle (L-E11): only a real test command's fail→pass counts. The pipeline
//! never feeds a lint/typecheck command to [`FlipOracle::observe`].

use stella_protocol::JudgeEvidence;

/// The flip oracle's state. `None` = no failing observation yet; `Failing` =
/// the tracked command has been seen failing; `Flipped` = the tracked command
/// was seen failing and then passing. The invariant the whole design rests on:
/// **`Flipped` is reachable only by passing through `Failing` for the same
/// normalized command** — proven by [`tests::flip_requires_a_prior_failing_observation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlipState {
    #[default]
    None,
    Failing,
    Flipped,
}

/// What one [`FlipOracle::observe`] call did — surfaced so the pipeline can
/// tell "this evidence advanced the oracle" from "this was a different
/// command, ignored" from "a pass with nothing to prove".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveOutcome {
    /// The observation changed or reinforced the tracked command's state.
    Advanced,
    /// A pass observed before any failure — proves nothing, no state change.
    NoEvidence,
    /// A different normalized command than the one being tracked — ignored.
    Ignored,
}

/// The deterministic flip oracle (L-E11). Construct empty; feed it
/// `(command, passed)` observations. It locks onto the first command it sees
/// *fail* and thereafter only reasons about that one normalized command.
///
/// Keyed on the *normalized* command ([`normalize_command`]) so incidental
/// whitespace differences between two runs of the same command don't look
/// like two different commands — but token/flag reordering is intentionally
/// NOT normalized away, because reordering can change a command's meaning
/// (a pass on `cargo test -p a` must never be credited to a failure of
/// `cargo test -p b`).
#[derive(Debug, Clone, Default)]
pub struct FlipOracle {
    /// The normalized command the oracle locked onto (set on first failure).
    tracked: Option<String>,
    state: FlipState,
}

impl FlipOracle {
    /// A fresh oracle in the `None` state, tracking no command yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The oracle's current state.
    pub fn state(&self) -> FlipState {
        self.state
    }

    /// Whether the oracle has observed a genuine fail→pass flip of the same
    /// normalized command. This is the *only* deterministic "verified" signal
    /// the ladder trusts.
    pub fn is_flipped(&self) -> bool {
        matches!(self.state, FlipState::Flipped)
    }

    /// The normalized command the oracle is tracking, if it has locked onto
    /// one (i.e. once it has seen a first failure).
    pub fn tracked_command(&self) -> Option<&str> {
        self.tracked.as_deref()
    }

    /// Observe one run of a test `command` with its `passed` result. Returns
    /// what the observation did. Transition table:
    ///
    /// | state      | observation                          | next       |
    /// |------------|--------------------------------------|------------|
    /// | None       | pass (any cmd)                       | None (NoEvidence) |
    /// | None       | fail (cmd C)                        | Failing, tracked=C |
    /// | Failing/Flipped | different cmd than tracked      | unchanged (Ignored) |
    /// | Failing    | fail (tracked cmd)                  | Failing    |
    /// | Failing    | pass (tracked cmd)                  | Flipped    |
    /// | Flipped    | pass (tracked cmd)                  | Flipped    |
    /// | Flipped    | fail (tracked cmd)                  | Failing (honest regression) |
    ///
    /// The honest `Flipped → Failing` regression edge keeps the oracle
    /// truthful if a "fixed" test starts failing again on re-run; it never
    /// violates the core invariant (reaching `Flipped` still required a prior
    /// `Failing` of the same command).
    pub fn observe(&mut self, command: &str, passed: bool) -> ObserveOutcome {
        let norm = normalize_command(command);
        match &self.tracked {
            None => {
                if passed {
                    // A pass with no prior failure proves nothing — do not
                    // even lock the command (L-E11).
                    ObserveOutcome::NoEvidence
                } else {
                    self.tracked = Some(norm);
                    self.state = FlipState::Failing;
                    ObserveOutcome::Advanced
                }
            }
            Some(tracked) => {
                if *tracked != norm {
                    return ObserveOutcome::Ignored;
                }
                self.state = match (self.state, passed) {
                    (FlipState::Failing, true) | (FlipState::Flipped, true) => FlipState::Flipped,
                    (FlipState::Failing, false) | (FlipState::Flipped, false) => FlipState::Failing,
                    // `None` with a tracked command is unreachable (they are
                    // set together), but stay total rather than panic.
                    (FlipState::None, true) => FlipState::None,
                    (FlipState::None, false) => FlipState::Failing,
                };
                ObserveOutcome::Advanced
            }
        }
    }
}

/// Normalize a test command for the flip oracle's identity check: trim, and
/// collapse every run of ASCII whitespace to a single space. This makes
/// `"cargo   test  -p x"` and `"cargo test -p x"` the same tracked command
/// while leaving token order — which can be semantically load-bearing —
/// untouched.
pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The three ways the evidence ladder can resolve a turn *before* spending a
/// model-judge call (L-E11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LadderDecision {
    /// Deterministic pass: flip achieved + touched-tests-green + diff within
    /// budget. Submit fast; the model judge is SKIPPED and a deterministic
    /// `JudgeVerdict { passed: true }` is emitted.
    SubmitFast,
    /// Clear failure (touched tests are red): feed the evidence back into a
    /// revision turn. No judge call — the failure is already deterministic.
    Revise,
    /// Inconclusive: no flip evidence, or diff over budget, or tests couldn't
    /// be run. Escalate to the model judge (a different model than the
    /// worker).
    ModelJudge,
}

/// The evidence gathered after execution, over which [`ladder_decision`]
/// reasons. All fields are owned plain data — the ladder is a pure function.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderInputs {
    /// The flip oracle reached `Flipped` for its tracked test command.
    pub flip_achieved: bool,
    /// Whether the touched tests passed after execution. `None` when no test
    /// command was available/run — an *inconclusive* signal, not a pass.
    pub touched_tests_passed: Option<bool>,
    /// Lines changed by the turn (from the diff command).
    pub diff_lines: u32,
    /// The diff-size budget; a diff at or under this is "small enough" to
    /// trust deterministic evidence without a judge.
    pub diff_budget: u32,
}

/// The evidence ladder (L-E11). Decides submit/revise/escalate from
/// deterministic evidence alone. Ordering of the checks matters:
///
/// 1. **Touched tests red → `Revise`.** A red test is a clear, deterministic
///    failure; never spend a judge call to "confirm" it.
/// 2. **Flip + green + within budget → `SubmitFast`.** The full deterministic
///    pass: judge skipped.
/// 3. **Otherwise → `ModelJudge`.** Genuinely inconclusive: no flip, or the
///    diff is over budget (large change deserves a second opinion even with
///    green tests), or tests couldn't be run.
pub fn ladder_decision(inputs: &LadderInputs) -> LadderDecision {
    // 1. A red touched-test is a deterministic failure — revise, no judge.
    if inputs.touched_tests_passed == Some(false) {
        return LadderDecision::Revise;
    }
    // 2. Full deterministic pass — submit fast, judge skipped.
    if inputs.flip_achieved
        && inputs.touched_tests_passed == Some(true)
        && inputs.diff_lines <= inputs.diff_budget
    {
        return LadderDecision::SubmitFast;
    }
    // 3. Inconclusive — escalate to the model judge.
    LadderDecision::ModelJudge
}

/// Build the deterministic `JudgeEvidence` for a `SubmitFast` verdict — the
/// evidence attached to the emitted `JudgeVerdict { passed: true,
/// evidence: { deterministic: true, .. } }`.
pub fn deterministic_pass_evidence(tracked_cmd: Option<&str>, diff_lines: u32) -> JudgeEvidence {
    let summary = match tracked_cmd {
        Some(cmd) => format!(
            "flip oracle: fail→pass of `{cmd}`; touched tests green; diff {diff_lines} lines within budget"
        ),
        None => format!(
            "touched tests green; diff {diff_lines} lines within budget (no flip command tracked)"
        ),
    };
    JudgeEvidence {
        summary,
        deterministic: true,
        evidence_refs: Vec::new(),
    }
}

/// Build the deterministic `JudgeEvidence` for a `Revise` verdict (touched
/// tests red) — a `passed: false`, `deterministic: true` verdict.
pub fn deterministic_fail_evidence(tail: &str) -> JudgeEvidence {
    JudgeEvidence {
        summary: format!("touched tests failed after execution: {}", tail.trim()),
        deterministic: true,
        evidence_refs: Vec::new(),
    }
}

/// A model judge's parsed verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeVerdict {
    pub passed: bool,
    pub reasoning: String,
}

/// Parse a Role::Judge model response into a verdict. The judge prompt (see
/// [`judge_prompt`]) asks for a leading `PASS` or `FAIL` token; this scans
/// token-by-token (case-insensitive) for the first of either, and treats the
/// remainder as reasoning. Returns `None` when neither token appears — the
/// signal the caller uses to invoke the [`heuristic_fallback`] verdict rather
/// than trusting an unparseable judge response.
pub fn parse_judge_response(text: &str) -> Option<JudgeVerdict> {
    let lower = text.to_ascii_lowercase();
    for raw in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        match raw {
            "pass" | "passed" | "approve" | "approved" | "yes" => {
                return Some(JudgeVerdict {
                    passed: true,
                    reasoning: text.trim().to_string(),
                });
            }
            "fail" | "failed" | "reject" | "rejected" | "no" => {
                return Some(JudgeVerdict {
                    passed: false,
                    reasoning: text.trim().to_string(),
                });
            }
            _ => {}
        }
    }
    None
}

/// The conservative heuristic verdict used when the *judge model call itself*
/// fails or its response is unparseable (L-E11: "a heuristic fallback verdict
/// if the judge call itself fails"). It never fabricates confidence: it
/// passes only when the touched tests were observed green, and otherwise
/// fails (so an unverifiable turn is revised rather than shipped). A judge
/// outage therefore degrades to "trust green tests, distrust everything
/// else", never to a blanket pass.
pub fn heuristic_fallback(inputs: &LadderInputs) -> JudgeVerdict {
    let passed = inputs.touched_tests_passed == Some(true);
    let reasoning = if passed {
        "judge unavailable; heuristic fallback passed on green touched tests".to_string()
    } else {
        "judge unavailable; heuristic fallback failed (touched tests not confirmed green)"
            .to_string()
    };
    JudgeVerdict { passed, reasoning }
}

/// Convert a model/heuristic [`JudgeVerdict`] into the `JudgeEvidence` for the
/// emitted `JudgeVerdict` event, marked `deterministic: false` (it is a
/// model/heuristic opinion, never conflated with the deterministic ladder —
/// L-E11).
pub fn model_verdict_evidence(verdict: &JudgeVerdict) -> JudgeEvidence {
    JudgeEvidence {
        summary: verdict.reasoning.clone(),
        deterministic: false,
        evidence_refs: Vec::new(),
    }
}

/// The prompt handed to the Role::Judge model on inconclusive evidence. Asks
/// for a leading `PASS`/`FAIL` token plus a one-line reason. The judge sees
/// the goal, the diff, and the deterministic evidence gathered so far — never
/// the worker's full transcript (judge ≠ worker, L-E11).
pub fn judge_prompt(goal: &str, diff: &str, evidence_summary: &str) -> String {
    format!(
        "You are an independent code reviewer judging whether a change accomplishes its goal. \
         Answer with `PASS` or `FAIL` on the first line, then one line of reasoning.\n\n\
         ## Goal\n{goal}\n\n\
         ## Deterministic evidence gathered\n{evidence_summary}\n\n\
         ## Diff\n{diff}\n\n\
         Verdict:"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ---- FlipOracle transitions ----------------------------------------

    #[test]
    fn a_pass_with_no_prior_failure_proves_nothing() {
        let mut oracle = FlipOracle::new();
        assert_eq!(
            oracle.observe("cargo test -p x", true),
            ObserveOutcome::NoEvidence
        );
        assert_eq!(oracle.state(), FlipState::None);
        assert!(!oracle.is_flipped());
        // It didn't even lock the command.
        assert_eq!(oracle.tracked_command(), None);
    }

    #[test]
    fn fail_then_pass_of_the_same_command_flips() {
        let mut oracle = FlipOracle::new();
        assert_eq!(
            oracle.observe("cargo test -p x", false),
            ObserveOutcome::Advanced
        );
        assert_eq!(oracle.state(), FlipState::Failing);
        assert_eq!(
            oracle.observe("cargo test -p x", true),
            ObserveOutcome::Advanced
        );
        assert!(oracle.is_flipped());
    }

    #[test]
    fn whitespace_differences_are_the_same_tracked_command() {
        let mut oracle = FlipOracle::new();
        oracle.observe("cargo   test  -p   x", false);
        // Re-run with normalized whitespace — must be recognized as the same.
        let out = oracle.observe("cargo test -p x", true);
        assert_eq!(out, ObserveOutcome::Advanced);
        assert!(oracle.is_flipped());
    }

    #[test]
    fn a_pass_of_a_different_command_never_flips() {
        let mut oracle = FlipOracle::new();
        oracle.observe("cargo test -p a", false);
        // A pass of a DIFFERENT command must be ignored — proving a's failure
        // fixed is not established by b passing.
        assert_eq!(
            oracle.observe("cargo test -p b", true),
            ObserveOutcome::Ignored
        );
        assert!(!oracle.is_flipped());
        assert_eq!(oracle.state(), FlipState::Failing);
    }

    #[test]
    fn flipped_regresses_honestly_if_the_command_fails_again() {
        let mut oracle = FlipOracle::new();
        oracle.observe("t", false);
        oracle.observe("t", true);
        assert!(oracle.is_flipped());
        // A later failure of the same command honestly moves back to Failing.
        oracle.observe("t", false);
        assert_eq!(oracle.state(), FlipState::Failing);
        assert!(!oracle.is_flipped());
    }

    #[test]
    fn from_none_a_single_observation_can_never_reach_flipped() {
        // The core invariant, in the small: you cannot go None → Flipped in
        // one step; Failing is mandatory.
        for passed in [true, false] {
            let mut oracle = FlipOracle::new();
            oracle.observe("t", passed);
            assert_ne!(
                oracle.state(),
                FlipState::Flipped,
                "one observation from None must never be Flipped"
            );
        }
    }

    // ---- normalize_command ---------------------------------------------

    #[test]
    fn normalize_collapses_whitespace_and_trims_but_keeps_order() {
        assert_eq!(normalize_command("  a   b\tc \n"), "a b c");
        // Order is preserved (not sorted) — reordering could change meaning.
        assert_ne!(normalize_command("a b"), normalize_command("b a"));
    }

    // ---- ladder_decision ------------------------------------------------

    #[test]
    fn red_touched_tests_revise_without_a_judge() {
        let decision = ladder_decision(&LadderInputs {
            flip_achieved: false,
            touched_tests_passed: Some(false),
            diff_lines: 3,
            diff_budget: 100,
        });
        assert_eq!(decision, LadderDecision::Revise);
    }

    #[test]
    fn full_deterministic_pass_submits_fast() {
        let decision = ladder_decision(&LadderInputs {
            flip_achieved: true,
            touched_tests_passed: Some(true),
            diff_lines: 40,
            diff_budget: 100,
        });
        assert_eq!(decision, LadderDecision::SubmitFast);
    }

    #[test]
    fn flip_and_green_but_over_diff_budget_escalates_to_judge() {
        let decision = ladder_decision(&LadderInputs {
            flip_achieved: true,
            touched_tests_passed: Some(true),
            diff_lines: 500,
            diff_budget: 100,
        });
        assert_eq!(
            decision,
            LadderDecision::ModelJudge,
            "a large diff deserves a second opinion even with green tests"
        );
    }

    #[test]
    fn no_flip_evidence_escalates_to_judge_not_a_false_pass() {
        // Tests green but never flipped (they always passed) → inconclusive.
        let decision = ladder_decision(&LadderInputs {
            flip_achieved: false,
            touched_tests_passed: Some(true),
            diff_lines: 5,
            diff_budget: 100,
        });
        assert_eq!(decision, LadderDecision::ModelJudge);
    }

    #[test]
    fn tests_indeterminate_escalates_to_judge() {
        let decision = ladder_decision(&LadderInputs {
            flip_achieved: false,
            touched_tests_passed: None,
            diff_lines: 5,
            diff_budget: 100,
        });
        assert_eq!(decision, LadderDecision::ModelJudge);
    }

    // ---- judge parsing + fallback --------------------------------------

    #[test]
    fn parses_pass_and_fail_verdicts() {
        assert_eq!(
            parse_judge_response("PASS — looks correct").map(|v| v.passed),
            Some(true)
        );
        assert_eq!(
            parse_judge_response("FAIL: missing edge case").map(|v| v.passed),
            Some(false)
        );
        assert_eq!(
            parse_judge_response("Verdict: approved").map(|v| v.passed),
            Some(true)
        );
    }

    #[test]
    fn unparseable_judge_response_is_none() {
        assert_eq!(parse_judge_response("hmm, hard to say"), None);
        assert_eq!(parse_judge_response(""), None);
    }

    #[test]
    fn heuristic_fallback_passes_only_on_confirmed_green_tests() {
        let green = heuristic_fallback(&LadderInputs {
            flip_achieved: false,
            touched_tests_passed: Some(true),
            diff_lines: 0,
            diff_budget: 100,
        });
        assert!(green.passed);

        for tests in [Some(false), None] {
            let v = heuristic_fallback(&LadderInputs {
                flip_achieved: true, // even a flip doesn't rescue an unconfirmed suite
                touched_tests_passed: tests,
                diff_lines: 0,
                diff_budget: 100,
            });
            assert!(!v.passed, "unconfirmed tests must fall back to FAIL");
        }
    }

    #[test]
    fn evidence_builders_tag_determinism_correctly() {
        assert!(deterministic_pass_evidence(Some("cargo test"), 10).deterministic);
        assert!(deterministic_fail_evidence("boom").deterministic);
        let model = model_verdict_evidence(&JudgeVerdict {
            passed: true,
            reasoning: "looks fine".into(),
        });
        assert!(
            !model.deterministic,
            "model verdicts are never deterministic"
        );
    }

    #[test]
    fn judge_prompt_carries_goal_diff_and_evidence_but_asks_for_pass_fail() {
        let p = judge_prompt("fix the bug", "@@ -1 +1 @@\n-x\n+y", "no flip; tests green");
        assert!(p.contains("fix the bug"));
        assert!(p.contains("+y"));
        assert!(p.contains("no flip; tests green"));
        assert!(p.contains("PASS"));
        assert!(p.contains("FAIL"));
    }

    // ---- The binding property (L-E11) -----------------------------------

    /// A reference oracle: replay a sequence of observations and independently
    /// compute whether a genuine flip occurred (a failure of some command
    /// followed later by a pass of that *same normalized* command, with no
    /// intervening failure of it right before the pass). This mirrors the
    /// state machine's intent so we can cross-check `FlipOracle` against it.
    fn reference_flipped(observations: &[(String, bool)]) -> bool {
        // Track the first command that fails (the oracle locks onto it).
        let mut tracked: Option<String> = None;
        let mut state = FlipState::None;
        for (cmd, passed) in observations {
            let norm = normalize_command(cmd);
            match &tracked {
                None => {
                    if !passed {
                        tracked = Some(norm);
                        state = FlipState::Failing;
                    }
                }
                Some(t) if *t == norm => {
                    state = if *passed {
                        FlipState::Flipped
                    } else {
                        FlipState::Failing
                    };
                }
                _ => {}
            }
        }
        matches!(state, FlipState::Flipped)
    }

    proptest! {
        /// The binding invariant (L-E11): the oracle reports `Flipped` **only**
        /// when the observation sequence contains a failing observation of the
        /// tracked command strictly before the pass that flipped it. We prove
        /// it two ways at once: (a) the live oracle agrees with an independent
        /// reference computation, and (b) whenever the oracle is flipped, the
        /// tracked command was observed failing at least once earlier in the
        /// sequence.
        #[test]
        fn flip_requires_a_prior_failing_observation(
            // A small alphabet of commands so collisions (same command re-run)
            // actually happen; random pass/fail outcomes.
            seq in prop::collection::vec(
                ((0u8..4).prop_map(|n| format!("cargo test -p crate{n}")), any::<bool>()),
                0..40,
            )
        ) {
            let mut oracle = FlipOracle::new();
            for (cmd, passed) in &seq {
                oracle.observe(cmd, *passed);
            }

            // (a) Agreement with the independent reference.
            prop_assert_eq!(oracle.is_flipped(), reference_flipped(&seq));

            // (b) If flipped, the tracked command was seen failing earlier,
            //     and a pass of it came after that failure.
            if oracle.is_flipped() {
                let tracked = oracle.tracked_command().expect("flipped implies a tracked command");
                let norm_tracked = normalize_command(tracked);
                let mut saw_fail = false;
                let mut fail_before_pass = false;
                for (cmd, passed) in &seq {
                    if normalize_command(cmd) != norm_tracked {
                        continue;
                    }
                    if !passed {
                        saw_fail = true;
                    } else if saw_fail {
                        fail_before_pass = true;
                    }
                }
                prop_assert!(saw_fail, "flipped without ever observing the tracked command fail");
                prop_assert!(
                    fail_before_pass,
                    "flipped without a fail strictly before the flipping pass"
                );
            }
        }

        /// The oracle can never jump straight from `None` to `Flipped`: the
        /// state after processing a prefix is `Flipped` only if the prefix
        /// already contained a failure of the tracked command.
        #[test]
        fn never_none_to_flipped_in_one_step(
            passed in any::<bool>(),
        ) {
            let mut oracle = FlipOracle::new();
            oracle.observe("cargo test", passed);
            prop_assert_ne!(oracle.state(), FlipState::Flipped);
        }
    }
}
