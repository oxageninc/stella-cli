//! Scope review (L-E5): above configurable thresholds, a plan is presented to
//! the user for approval before it executes. The *decision* — does this plan
//! cross a threshold — is a pure function ([`needs_scope_review`]); the
//! interactive gate (emit the event, await the [`crate::ports::ApprovalGate`])
//! lives in [`crate::pipeline`].
//!
//! The gate exists to restore trust for large work without slowing small
//! tasks: a two-step, one-file plan sails straight through; a
//! twelve-step, thirty-file plan pauses for a plan card. Headless runs must
//! opt into a bypass explicitly (`PipelineConfig`), never auto-approve
//! silently.

use stella_protocol::ScopeProposal;

use crate::plan::PlanStep;

/// The thresholds above which a plan triggers interactive scope review
/// (L-E5). Any one axis crossing its threshold is enough — the axes are
/// OR-ed, since a plan can be large in steps, files, or cost independently.
/// `None` on an axis disables that axis (never triggers on it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScopeThresholds {
    /// Trigger when the plan has strictly more than this many steps.
    pub max_steps: Option<usize>,
    /// Trigger when estimated files touched strictly exceeds this.
    pub max_files: Option<u32>,
    /// Trigger when estimated cost (USD) strictly exceeds this.
    pub max_cost_usd: Option<f64>,
}

impl Default for ScopeThresholds {
    /// Defaults tuned to let ordinary work through and gate genuinely large
    /// plans: more than 5 steps, more than 8 files, or more than $1.00
    /// estimated. These mirror the TS CLI's scope-review defaults (PR #661).
    fn default() -> Self {
        Self {
            max_steps: Some(5),
            max_files: Some(8),
            max_cost_usd: Some(1.00),
        }
    }
}

/// A coarse estimate of a plan's blast radius, used both to decide whether
/// scope review fires and to populate the [`ScopeProposal`] the user sees.
/// Computed by the pipeline from the plan (and any planner-provided hints);
/// kept a plain owned struct so the threshold decision is pure.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ScopeEstimate {
    pub estimated_files: u32,
    pub estimated_cost_usd: Option<f64>,
}

/// Whether this plan must go through interactive scope review, given the
/// thresholds. Pure and total: OR across the three axes, each skipped when
/// its threshold is `None`. Strict `>` so a plan exactly at a threshold
/// passes without a gate (the threshold is the largest size that *doesn't*
/// prompt).
pub fn needs_scope_review(
    plan: &[PlanStep],
    estimate: &ScopeEstimate,
    thresholds: &ScopeThresholds,
) -> bool {
    if let Some(max) = thresholds.max_steps
        && plan.len() > max
    {
        return true;
    }
    if let Some(max) = thresholds.max_files
        && estimate.estimated_files > max
    {
        return true;
    }
    if let (Some(max), Some(cost)) = (thresholds.max_cost_usd, estimate.estimated_cost_usd)
        && cost > max
    {
        return true;
    }
    false
}

/// Build the [`ScopeProposal`] surfaced in the `ScopeReview` event from the
/// plan and estimate. The summary names the plan's size so the card is
/// self-describing even before the steps render.
pub fn build_proposal(goal: &str, plan: &[PlanStep], estimate: &ScopeEstimate) -> ScopeProposal {
    let steps: Vec<String> = plan.iter().map(|s| s.description.clone()).collect();
    let summary = format!(
        "{} step{} to accomplish: {}",
        steps.len(),
        if steps.len() == 1 { "" } else { "s" },
        goal.trim()
    );
    ScopeProposal {
        summary,
        steps,
        estimated_files: estimate.estimated_files,
        estimated_cost_usd: estimate.estimated_cost_usd,
    }
}

/// Apply a `Trim { keep_steps }` decision to a plan: keep exactly the steps at
/// the given indices, in the order the user listed them, ignoring any
/// out-of-range index. Returns the trimmed plan. An empty result is possible
/// (the user kept nothing) — the caller treats that as an abort.
pub fn apply_trim(plan: &[PlanStep], keep_steps: &[usize]) -> Vec<PlanStep> {
    keep_steps
        .iter()
        .filter_map(|&i| plan.get(i).cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_of(n: usize) -> Vec<PlanStep> {
        (0..n).map(|i| PlanStep::new(format!("step {i}"))).collect()
    }

    #[test]
    fn small_plan_under_all_thresholds_does_not_trigger() {
        let plan = plan_of(2);
        let estimate = ScopeEstimate {
            estimated_files: 1,
            estimated_cost_usd: Some(0.05),
        };
        assert!(!needs_scope_review(
            &plan,
            &estimate,
            &ScopeThresholds::default()
        ));
    }

    #[test]
    fn exceeding_step_threshold_triggers() {
        let plan = plan_of(6); // default max_steps is 5
        assert!(needs_scope_review(
            &plan,
            &ScopeEstimate::default(),
            &ScopeThresholds::default()
        ));
    }

    #[test]
    fn exactly_at_threshold_does_not_trigger() {
        let plan = plan_of(5); // == max_steps, strict > means no trigger
        assert!(!needs_scope_review(
            &plan,
            &ScopeEstimate::default(),
            &ScopeThresholds::default()
        ));
    }

    #[test]
    fn exceeding_file_estimate_triggers_even_with_a_tiny_plan() {
        let plan = plan_of(1);
        let estimate = ScopeEstimate {
            estimated_files: 20, // default max_files is 8
            estimated_cost_usd: None,
        };
        assert!(needs_scope_review(
            &plan,
            &estimate,
            &ScopeThresholds::default()
        ));
    }

    #[test]
    fn exceeding_cost_estimate_triggers() {
        let plan = plan_of(1);
        let estimate = ScopeEstimate {
            estimated_files: 1,
            estimated_cost_usd: Some(5.0), // default max_cost is 1.00
        };
        assert!(needs_scope_review(
            &plan,
            &estimate,
            &ScopeThresholds::default()
        ));
    }

    #[test]
    fn none_axis_thresholds_never_trigger_on_that_axis() {
        let thresholds = ScopeThresholds {
            max_steps: None,
            max_files: None,
            max_cost_usd: None,
        };
        let plan = plan_of(1000);
        let estimate = ScopeEstimate {
            estimated_files: 9999,
            estimated_cost_usd: Some(9999.0),
        };
        assert!(!needs_scope_review(&plan, &estimate, &thresholds));
    }

    #[test]
    fn cost_axis_needs_both_a_threshold_and_an_estimate() {
        // Threshold set but no estimate → can't trigger on cost.
        let plan = plan_of(1);
        let estimate = ScopeEstimate {
            estimated_files: 1,
            estimated_cost_usd: None,
        };
        assert!(!needs_scope_review(
            &plan,
            &estimate,
            &ScopeThresholds::default()
        ));
    }

    #[test]
    fn proposal_describes_the_plan() {
        let plan = plan_of(3);
        let estimate = ScopeEstimate {
            estimated_files: 4,
            estimated_cost_usd: Some(0.5),
        };
        let proposal = build_proposal("refactor auth", &plan, &estimate);
        assert_eq!(proposal.steps.len(), 3);
        assert_eq!(proposal.estimated_files, 4);
        assert_eq!(proposal.estimated_cost_usd, Some(0.5));
        assert!(proposal.summary.contains("3 steps"));
        assert!(proposal.summary.contains("refactor auth"));
    }

    #[test]
    fn proposal_summary_is_singular_for_one_step() {
        let proposal = build_proposal("x", &plan_of(1), &ScopeEstimate::default());
        assert!(proposal.summary.contains("1 step to"));
    }

    #[test]
    fn trim_keeps_selected_steps_in_user_order_ignoring_out_of_range() {
        let plan = plan_of(4); // steps 0,1,2,3
        let trimmed = apply_trim(&plan, &[2, 0, 99]);
        assert_eq!(
            trimmed,
            vec![PlanStep::new("step 2"), PlanStep::new("step 0")]
        );
    }

    #[test]
    fn trim_to_nothing_yields_empty() {
        let plan = plan_of(3);
        assert!(apply_trim(&plan, &[]).is_empty());
        assert!(apply_trim(&plan, &[99]).is_empty());
    }
}
