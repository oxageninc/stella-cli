//! Emit-shape helpers: turn media job transitions into `stella_protocol`
//! event *values* (`08-multimodal.md`; event vocabulary in
//! `02-architecture.md` §4). These return `AgentEvent`s as plain data — this
//! crate has no channel dependency, so the caller owns how they reach the
//! renderer. Centralizing the mapping means a `MediaJob` and a
//! `MediaJobStatus` translate to `MediaProgress`/`MediaComplete` one way, not
//! per call site.

use stella_protocol::{AgentEvent, MediaArtifactRef, MediaJobState};

use crate::provider::{MediaJob, MediaJobStatus};

/// Build a `MediaProgress` event for `job` transitioning to `state`. The
/// `artifact_id` and `kind` come from the job so events and the eventual
/// artifact share one identity across a resume (`08-multimodal.md` §6).
pub fn media_progress(job: &MediaJob, state: MediaJobState) -> AgentEvent {
    AgentEvent::MediaProgress {
        artifact_id: job.artifact_id.clone(),
        kind: job.kind,
        state,
    }
}

/// Build a `MediaProgress` event straight from a poll result, cloning the
/// job's live state — the common path in a poll loop.
pub fn progress_from_status(job: &MediaJob, status: &MediaJobStatus) -> AgentEvent {
    media_progress(job, status.state.clone())
}

/// Build the `MediaComplete` event announcing a persisted artifact
/// (`08-multimodal.md` §3 — the artifact landed under `.stella/artifacts/`).
pub fn media_complete(artifact: MediaArtifactRef) -> AgentEvent {
    AgentEvent::MediaComplete { artifact }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::MediaKind;

    fn job() -> MediaJob {
        MediaJob {
            artifact_id: "med_xyz".into(),
            provider_id: "zai".into(),
            provider_job_id: "job-1".into(),
            kind: MediaKind::Video,
            model: "cogvideox".into(),
            estimated_cost_usd: 2.0,
            submitted_at: 1_700_000_000,
            label: "teaser".into(),
        }
    }

    #[test]
    fn media_progress_carries_job_identity_and_state() {
        let event = media_progress(&job(), MediaJobState::Running);
        match event {
            AgentEvent::MediaProgress {
                artifact_id,
                kind,
                state,
            } => {
                assert_eq!(artifact_id, "med_xyz");
                assert_eq!(kind, MediaKind::Video);
                assert_eq!(state, MediaJobState::Running);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn progress_from_status_reflects_the_live_state() {
        let status = MediaJobStatus {
            state: MediaJobState::Failed {
                reason: "gone".into(),
            },
            progress: None,
            artifact: None,
        };
        let event = progress_from_status(&job(), &status);
        match event {
            AgentEvent::MediaProgress { state, .. } => {
                assert!(matches!(state, MediaJobState::Failed { .. }));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn media_complete_wraps_the_artifact_ref() {
        let art_ref = MediaArtifactRef {
            id: "med_xyz".into(),
            kind: MediaKind::Video,
            path: "med_xyz.mp4".into(),
            label: "teaser".into(),
        };
        let event = media_complete(art_ref.clone());
        match event {
            AgentEvent::MediaComplete { artifact } => assert_eq!(artifact, art_ref),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn events_serialize_to_the_protocol_wire_shape() {
        // Guard the stream-json contract: the helper's events serialize with
        // the protocol's type tags.
        let progress = media_progress(&job(), MediaJobState::Queued);
        let json = serde_json::to_string(&progress).unwrap();
        assert!(json.contains("\"type\":\"media_progress\""), "{json}");

        let complete = media_complete(MediaArtifactRef {
            id: "med_1".into(),
            kind: MediaKind::Image,
            path: "med_1.png".into(),
            label: "logo".into(),
        });
        let json = serde_json::to_string(&complete).unwrap();
        assert!(json.contains("\"type\":\"media_complete\""), "{json}");
    }
}
