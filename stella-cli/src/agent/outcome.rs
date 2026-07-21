use stella_context::EpisodeOutcome;
use stella_pipeline::{PipelineOutcome, PipelineRunError, PipelineStatus};

pub(crate) fn settled_cost_since(start_usd: f64, current_usd: f64) -> f64 {
    (current_usd - start_usd).max(0.0)
}

pub(crate) fn pipeline_execution_closeout(
    result: &Result<PipelineOutcome, PipelineRunError>,
) -> (&'static str, f64) {
    match result {
        Ok(outcome) => (
            pipeline_status_label(&outcome.status),
            outcome.total_cost_usd,
        ),
        Err(error) => ("error", error.total_cost_usd),
    }
}

pub(super) fn pipeline_status_label(status: &PipelineStatus) -> &'static str {
    match status {
        PipelineStatus::Completed => "completed",
        PipelineStatus::VerificationFailed { .. } => "verification_failed",
        PipelineStatus::Aborted { .. } => "aborted",
    }
}

pub(super) fn pipeline_failure_reason(status: &PipelineStatus) -> Option<String> {
    match status {
        PipelineStatus::Completed => None,
        PipelineStatus::VerificationFailed { verdict } => {
            Some(format!("verification failed: {}", verdict.summary))
        }
        PipelineStatus::Aborted { reason } => Some(reason.clone()),
    }
}

pub(super) fn pipeline_episode_outcome(status: &PipelineStatus) -> EpisodeOutcome {
    match status {
        PipelineStatus::Completed => EpisodeOutcome::Success,
        PipelineStatus::VerificationFailed { .. } => EpisodeOutcome::Failure,
        PipelineStatus::Aborted { .. } => EpisodeOutcome::Aborted,
    }
}

pub(super) fn pipeline_status_result(status: &PipelineStatus) -> Result<(), String> {
    match status {
        PipelineStatus::Completed => Ok(()),
        PipelineStatus::VerificationFailed { verdict } => {
            Err(format!("verification failed: {}", verdict.summary))
        }
        PipelineStatus::Aborted { reason } => Err(reason.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_pipeline::{PipelineError, PipelineRunError};

    #[test]
    fn hard_pipeline_error_closeout_retains_prior_stage_cost() {
        let result = Err(PipelineRunError {
            cause: PipelineError::ScopeReviewRequiredHeadless,
            total_cost_usd: 0.42,
        });

        assert_eq!(pipeline_execution_closeout(&result), ("error", 0.42));
    }

    #[test]
    fn cancellation_after_settled_spend_persists_the_dispatch_delta() {
        assert!((settled_cost_since(1.25, 1.75) - 0.50).abs() < 1e-9);
    }

    #[test]
    fn cancellation_before_spend_persists_zero() {
        assert_eq!(settled_cost_since(1.25, 1.25), 0.0);
    }
}
