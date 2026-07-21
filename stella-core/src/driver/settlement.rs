use stella_protocol::AgentEvent;
use tokio::sync::mpsc::UnboundedSender;

use super::TurnOutcome;
use crate::budget::{BudgetGuard, BudgetOutcome};

pub(super) fn record_settled_cost(
    budget: &mut BudgetGuard,
    cost_usd: f64,
    events: &UnboundedSender<AgentEvent>,
) -> BudgetOutcome {
    let outcome = budget.record_spend(cost_usd);
    let _ = events.send(AgentEvent::BudgetTick {
        spent_usd: budget.spent_usd(),
        limit_usd: budget.turn_limit_usd(),
        mode: budget.mode(),
    });
    outcome
}

/// The between-steps budget check (never mid-tool — see module docs).
pub(super) fn check_budget(
    budget: &BudgetGuard,
    total_cost_usd: f64,
    events: &UnboundedSender<AgentEvent>,
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
