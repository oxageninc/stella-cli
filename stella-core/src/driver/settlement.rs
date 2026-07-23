use stella_protocol::AgentEvent;

use super::TurnOutcome;
use crate::budget::{BudgetAxis, BudgetGuard, BudgetOutcome};
use crate::event_sender::EventSender;

pub(super) fn record_settled_cost(
    budget: &mut BudgetGuard,
    cost_usd: f64,
    events: &EventSender,
) -> BudgetOutcome {
    let outcome = budget.record_spend(cost_usd);
    let _ = events.send(AgentEvent::BudgetTick {
        spent_usd: budget.spent_usd(),
        limit_usd: budget.turn_limit_usd(),
        mode: budget.mode(),
    });
    // Observed-mode breaches surface here, once per settled call — NOT in
    // `check_budget`, which runs twice per step and would spam the same
    // warning at every boundary. `BudgetTick` alone cannot carry this: it
    // reports only turn-scoped numbers, so a session-axis breach would
    // otherwise be invisible to every event-stream consumer
    // (`BudgetOutcome::Warn`'s contract is that the driver surfaces it).
    if let BudgetOutcome::Warn {
        axis,
        spent_usd,
        limit_usd,
    } = outcome
    {
        let axis = match axis {
            BudgetAxis::Turn => "turn",
            BudgetAxis::Session => "session",
        };
        let _ = events.send(AgentEvent::Error {
            message: format!(
                "budget warning: spent ${spent_usd:.4} against a ${limit_usd:.2} observed {axis} \
                 limit; continuing"
            ),
            retryable: true,
        });
    }
    outcome
}

/// The between-steps budget check (never mid-tool — see module docs).
pub(super) fn check_budget(
    budget: &BudgetGuard,
    total_cost_usd: f64,
    events: &EventSender,
) -> Option<TurnOutcome> {
    let BudgetOutcome::AbortTurn {
        spent_usd,
        limit_usd,
        ..
    } = budget.evaluate()
    else {
        return None;
    };
    let reason = format!("budget exceeded: spent ${spent_usd:.4} against a ${limit_usd:.2} limit");
    let _ = events.send(AgentEvent::Error {
        message: reason.clone(),
        retryable: false,
    });
    Some(TurnOutcome::Aborted {
        reason,
        cost_usd: total_cost_usd,
    })
}
