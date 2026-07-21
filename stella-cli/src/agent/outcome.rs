use stella_context::EpisodeOutcome;
use stella_pipeline::PipelineStatus;

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
