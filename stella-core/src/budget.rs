//! USD spend metering — the *money* meter. This is
//! a distinct concern from `crate::compaction`'s *token*-budget eviction:
//! compaction manages context-window pressure, `budget` manages dollars
//! spent against a turn and/or session cap. Ported from the TS per-turn
//! budget (PR #625), now generalized to also cover a session scope and
//! future media spend.
//!
//! # The mid-tool-kill lesson
//!
//! Per, `enforced` mode is "a hard stop with a
//! clean turn abort — never a mid-tool kill." This module cannot enforce
//! *when* the caller checks the outcome — that discipline belongs to the
//! `driver.rs` step-driver, which only consults
//! [`BudgetGuard::record_spend`]'s (or [`BudgetGuard::evaluate`]'s) return
//! value **between steps**, after a model/media call has fully completed and
//! before the next one is dispatched — never while a tool call is
//! in-flight. Killing a tool mid-execution to enforce a budget was a TS-era
//! defect: it leaves the workspace and the model's view of it inconsistent.
//! [`BudgetOutcome::AbortTurn`] is a *recommendation* the driver acts on at
//! that safe boundary; this module has zero I/O and cannot itself abort
//! anything.
//!
//! # Money vs. tokens
//!
//! [`BudgetGuard::record_spend`] takes a bare `cost_usd: f64` and does not
//! care what produced it — a text completion, an image job, or a video job
//! all settle through the same call ("media counts").
//! `stella-media` does not exist yet; no special-casing is needed here for
//! that to fall out for free later.
//!
//! # Precision
//!
//! Spend accumulates via plain `f64` addition. This guard is an in-memory
//! running total for gating and HUD display (`BudgetTick`,
//!) — it is not the billing system of record (that
//! lives in adapter-reported usage plus, for the platform's own metered
//! products, ClickHouse/Stripe). Summation drift across a session-length
//! number of calls is negligible at USD-cent granularity; callers needing
//! exact reconciliation should do so against the authoritative usage log,
//! not this guard.

use stella_protocol::BudgetMode;

/// Which scope a [`BudgetOutcome`] is reporting against. A guard configured
/// with both a turn and a session limit checks them independently — the
/// turn axis first (the more local, more frequently-reset scope), then the
/// session axis — so a caller under its turn limit but over its session
/// limit still gets a session-scoped outcome rather than a false "all
/// clear."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetAxis {
    /// Spend since the last [`BudgetGuard::begin_turn`] call.
    Turn,
    /// Spend since the guard was constructed (accumulates across turns).
    Session,
}

/// The result of a spend check, computed after [`BudgetGuard::record_spend`]
/// or on demand via [`BudgetGuard::evaluate`].
///
/// See the module-level docs for the mid-tool-kill contract: `AbortTurn` is
/// a signal for the driver to consult **between steps**, never a directive
/// this module enforces itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BudgetOutcome {
    /// Under all configured limits (or metering is off, or no limit is
    /// configured on any axis). Safe to start the next step.
    Continue,
    /// Over a configured limit in `observed` mode: keep going, but the
    /// driver should surface a warning (e.g. in the TUI HUD). The guard
    /// keeps accepting further `record_spend` calls — `observed` never
    /// blocks.
    Warn {
        axis: BudgetAxis,
        spent_usd: f64,
        limit_usd: f64,
    },
    /// Over a configured limit in `enforced` mode: the driver must not
    /// dispatch another step this turn. Per the module contract, this must
    /// only be checked at a step boundary — never used to interrupt a tool
    /// already in flight.
    AbortTurn {
        axis: BudgetAxis,
        spent_usd: f64,
        limit_usd: f64,
    },
}

/// Tracks USD spend against an optional per-turn limit and an optional
/// per-session limit, and reports a [`BudgetOutcome`] after every recorded
/// cost.
///
/// A `None` limit on an axis means that axis never triggers, regardless of
/// mode. A session accumulates spend across multiple turns; call
/// [`begin_turn`](Self::begin_turn) when a new turn starts to reset the
/// turn-scoped counter while the session-scoped total keeps accumulating.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetGuard {
    mode: BudgetMode,
    turn_limit_usd: Option<f64>,
    session_limit_usd: Option<f64>,
    turn_spent_usd: f64,
    session_spent_usd: f64,
}

impl BudgetGuard {
    /// Construct a new guard. `mode` governs whether breaches ever produce a
    /// [`BudgetOutcome`] other than `Continue` (`Off` never does); the two
    /// limits are independent and either or both may be `None`.
    pub fn new(
        mode: BudgetMode,
        turn_limit_usd: Option<f64>,
        session_limit_usd: Option<f64>,
    ) -> Self {
        Self {
            mode,
            turn_limit_usd,
            session_limit_usd,
            turn_spent_usd: 0.0,
            session_spent_usd: 0.0,
        }
    }

    /// Record one call's cost (a text completion, an image job, a video
    /// job — this guard is generic over the source, see module docs) and
    /// return the resulting [`BudgetOutcome`]. Always accumulates on both
    /// the turn and session axes, in every mode, so `spent_usd`/
    /// `session_spent_usd` remain a truthful running total even when
    /// `mode` is `Off` — only *gating* is mode-dependent, accounting never
    /// is.
    pub fn record_spend(&mut self, cost_usd: f64) -> BudgetOutcome {
        self.turn_spent_usd += cost_usd;
        self.session_spent_usd += cost_usd;
        self.evaluate()
    }

    /// Evaluate the current spend against the configured limits without
    /// recording anything new. This is the "safe boundary" check the future
    /// `driver.rs` calls between steps — see module docs.
    pub fn evaluate(&self) -> BudgetOutcome {
        self.check_axis(BudgetAxis::Turn, self.turn_spent_usd, self.turn_limit_usd)
            .or_else(|| {
                self.check_axis(
                    BudgetAxis::Session,
                    self.session_spent_usd,
                    self.session_limit_usd,
                )
            })
            .unwrap_or(BudgetOutcome::Continue)
    }

    fn check_axis(
        &self,
        axis: BudgetAxis,
        spent_usd: f64,
        limit_usd: Option<f64>,
    ) -> Option<BudgetOutcome> {
        let limit_usd = limit_usd?;
        if spent_usd <= limit_usd {
            return None;
        }
        match self.mode {
            BudgetMode::Off => None,
            BudgetMode::Observed => Some(BudgetOutcome::Warn {
                axis,
                spent_usd,
                limit_usd,
            }),
            BudgetMode::Enforced => Some(BudgetOutcome::AbortTurn {
                axis,
                spent_usd,
                limit_usd,
            }),
        }
    }

    /// Reset the turn-scoped counter to zero at the start of a new turn.
    /// The session-scoped total is untouched — a session accumulates across
    /// every turn it contains.
    pub fn begin_turn(&mut self) {
        self.turn_spent_usd = 0.0;
    }

    /// Current spend for the active turn, in USD.
    pub fn spent_usd(&self) -> f64 {
        self.turn_spent_usd
    }

    /// Current spend for the whole session, in USD (accumulates across
    /// turns; see [`begin_turn`](Self::begin_turn)).
    pub fn session_spent_usd(&self) -> f64 {
        self.session_spent_usd
    }

    /// The configured per-turn limit, if any.
    pub fn turn_limit_usd(&self) -> Option<f64> {
        self.turn_limit_usd
    }

    /// The configured per-session limit, if any.
    pub fn session_limit_usd(&self) -> Option<f64> {
        self.session_limit_usd
    }

    /// The mode this guard was constructed with.
    pub fn mode(&self) -> BudgetMode {
        self.mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spend_is_monotonically_non_decreasing_and_exact() {
        // Property: turn_spent_usd/session_spent_usd never go down as calls
        // accumulate, and the running total equals the exact sum of every
        // recorded cost_usd (within float tolerance — see module docs on
        // precision).
        let mut guard = BudgetGuard::new(BudgetMode::Observed, None, None);
        let costs = [0.001, 0.02, 0.0003, 1.5, 0.0, 0.25, 0.0001];

        let mut running_total = 0.0f64;
        let mut previous_turn = 0.0f64;
        let mut previous_session = 0.0f64;
        for cost in costs {
            guard.record_spend(cost);
            running_total += cost;

            assert!(
                guard.spent_usd() >= previous_turn,
                "turn spend must never decrease"
            );
            assert!(
                guard.session_spent_usd() >= previous_session,
                "session spend must never decrease"
            );
            previous_turn = guard.spent_usd();
            previous_session = guard.session_spent_usd();
        }

        assert!(
            (guard.spent_usd() - running_total).abs() < 1e-9,
            "turn total {} must equal sum of recorded costs {}",
            guard.spent_usd(),
            running_total
        );
        assert!(
            (guard.session_spent_usd() - running_total).abs() < 1e-9,
            "session total {} must equal sum of recorded costs {}",
            guard.session_spent_usd(),
            running_total
        );
    }

    #[test]
    fn off_mode_never_warns_or_aborts_even_wildly_over_limit() {
        let mut guard = BudgetGuard::new(BudgetMode::Off, Some(1.0), Some(1.0));
        for _ in 0..10 {
            let outcome = guard.record_spend(1000.0);
            assert_eq!(outcome, BudgetOutcome::Continue);
        }
        // Off still meters (accounting is not mode-dependent), just never gates.
        assert!(guard.spent_usd() > 1.0);
    }

    #[test]
    fn observed_mode_warns_over_limit_but_never_blocks_further_recording() {
        let mut guard = BudgetGuard::new(BudgetMode::Observed, Some(1.0), None);
        assert_eq!(guard.record_spend(0.5), BudgetOutcome::Continue);

        let outcome = guard.record_spend(0.6); // now 1.1, over the 1.0 turn limit
        match outcome {
            BudgetOutcome::Warn {
                axis: BudgetAxis::Turn,
                spent_usd,
                limit_usd,
            } => {
                assert!((spent_usd - 1.1).abs() < 1e-9);
                assert_eq!(limit_usd, 1.0);
            }
            other => panic!("expected Warn, got {other:?}"),
        }

        // Observed never blocks — further recording keeps working.
        let outcome2 = guard.record_spend(5.0);
        assert!(matches!(outcome2, BudgetOutcome::Warn { .. }));
        assert!((guard.spent_usd() - 6.1).abs() < 1e-9);
    }

    #[test]
    fn enforced_mode_continues_under_limit_and_aborts_once_exceeded() {
        let mut guard = BudgetGuard::new(BudgetMode::Enforced, Some(2.0), None);
        assert_eq!(guard.record_spend(1.0), BudgetOutcome::Continue);
        assert_eq!(guard.record_spend(0.9), BudgetOutcome::Continue); // 1.9, still under

        let outcome = guard.record_spend(0.2); // 2.1, over
        match outcome {
            BudgetOutcome::AbortTurn {
                axis: BudgetAxis::Turn,
                spent_usd,
                limit_usd,
            } => {
                assert!((spent_usd - 2.1).abs() < 1e-9);
                assert_eq!(limit_usd, 2.0);
            }
            other => panic!("expected AbortTurn, got {other:?}"),
        }
    }

    #[test]
    fn none_limit_never_triggers_regardless_of_mode() {
        for mode in [BudgetMode::Off, BudgetMode::Observed, BudgetMode::Enforced] {
            let mut guard = BudgetGuard::new(mode, None, None);
            let outcome = guard.record_spend(1_000_000.0);
            assert_eq!(
                outcome,
                BudgetOutcome::Continue,
                "mode {mode:?} with no configured limits must never trigger"
            );
        }
    }

    #[test]
    fn turn_and_session_limits_are_checked_independently() {
        // Under the turn limit but over the session limit must still
        // trigger, and it must report the SESSION axis's numbers, not the
        // turn axis's.
        let mut guard = BudgetGuard::new(BudgetMode::Enforced, Some(10.0), Some(2.0));
        guard.record_spend(1.0); // turn=1.0 (under 10.0), session=1.0 (under 2.0)
        let outcome = guard.record_spend(1.5); // turn=2.5 (under 10.0), session=2.5 (over 2.0)

        match outcome {
            BudgetOutcome::AbortTurn {
                axis: BudgetAxis::Session,
                spent_usd,
                limit_usd,
            } => {
                assert!((spent_usd - 2.5).abs() < 1e-9);
                assert_eq!(limit_usd, 2.0);
            }
            other => panic!("expected a session-axis AbortTurn, got {other:?}"),
        }
        assert!(guard.spent_usd() < guard.turn_limit_usd().unwrap());
    }

    #[test]
    fn turn_axis_is_reported_when_only_the_turn_limit_is_breached() {
        let mut guard = BudgetGuard::new(BudgetMode::Enforced, Some(1.0), Some(100.0));
        let outcome = guard.record_spend(1.5); // turn over, session nowhere near its limit

        match outcome {
            BudgetOutcome::AbortTurn {
                axis: BudgetAxis::Turn,
                spent_usd,
                limit_usd,
            } => {
                assert!((spent_usd - 1.5).abs() < 1e-9);
                assert_eq!(limit_usd, 1.0);
            }
            other => panic!("expected a turn-axis AbortTurn, got {other:?}"),
        }
    }

    #[test]
    fn begin_turn_resets_turn_spend_but_not_session_spend() {
        let mut guard = BudgetGuard::new(BudgetMode::Observed, Some(5.0), None);
        guard.record_spend(3.0);
        assert!((guard.spent_usd() - 3.0).abs() < 1e-9);
        assert!((guard.session_spent_usd() - 3.0).abs() < 1e-9);

        guard.begin_turn();
        assert_eq!(guard.spent_usd(), 0.0);
        assert!(
            (guard.session_spent_usd() - 3.0).abs() < 1e-9,
            "session total must survive a turn reset"
        );

        guard.record_spend(1.0);
        assert!((guard.spent_usd() - 1.0).abs() < 1e-9);
        assert!(
            (guard.session_spent_usd() - 4.0).abs() < 1e-9,
            "session total accumulates across turns"
        );
    }

    #[test]
    fn evaluate_reports_without_recording_anything() {
        let mut guard = BudgetGuard::new(BudgetMode::Enforced, Some(1.0), None);
        guard.record_spend(2.0);
        assert!(matches!(guard.evaluate(), BudgetOutcome::AbortTurn { .. }));
        // Calling evaluate() repeatedly must not change the running total.
        let before = guard.spent_usd();
        let _ = guard.evaluate();
        let _ = guard.evaluate();
        assert_eq!(guard.spent_usd(), before);
    }

    #[test]
    fn exactly_at_the_limit_is_not_a_breach() {
        let mut guard = BudgetGuard::new(BudgetMode::Enforced, Some(1.0), None);
        let outcome = guard.record_spend(1.0);
        assert_eq!(
            outcome,
            BudgetOutcome::Continue,
            "spend == limit must not abort"
        );
    }
}
