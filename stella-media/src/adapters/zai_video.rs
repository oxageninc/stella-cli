//! Z.ai CogVideoX video adapter. Video is async:
//! `POST /videos/generations` submits and returns a job id; `GET
//! /async-result/{id}` polls until the job succeeds (with a downloadable
//! video URL) or fails.
//!
//! Truthfulness (L-V3): `poll_video` reconciles **live**. A `404` on the poll
//! endpoint — a job the provider has purged or never had — is returned as
//! `MediaJobState::Failed` (gone), a definite terminal state, *not* an error
//! and *not* a stale "running". This is what lets a resumed job after a
//! process restart report the truth rather than a cached handle.
//!
//! The wire shapes below are modeled on Z.ai's async video API; the recorded
//! fixtures pin this contract and the runtime-skipped live smoke validates it
//! against the real endpoint on a keyed release run (L-V4).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stella_protocol::{MediaJobState, MediaKind};

use crate::credential::ApiKey;
use crate::error::MediaError;
use crate::http::{classify_http_error, download_bytes, parse_retry_after_ms};
use crate::provider::{
    ImageRequest, MediaArtifact, MediaCapabilities, MediaJob, MediaJobStatus, MediaProvider,
    VideoRequest,
};

const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/paas/v4";
/// Default CogVideoX slug used when the caller doesn't pin one.
pub const DEFAULT_MODEL: &str = "cogvideox-3";
/// Documented default rate — catalog data once wired.
const DEFAULT_VIDEO_USD_PER_SECOND: f64 = 0.20;

/// A CogVideoX video provider.
pub struct ZaiVideoProvider {
    client: reqwest::Client,
    api_key: ApiKey,
    base_url: String,
    model: String,
}

impl ZaiVideoProvider {
    pub fn new(api_key: ApiKey, model: impl Into<String>) -> Self {
        Self {
            client: crate::http::client(),
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[derive(Serialize)]
struct VideoGenRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    /// Requested clip length in seconds. Load-bearing: the cost gate prices
    /// the job by this duration, so it must actually reach the provider —
    /// otherwise the approved estimate is for a duration the wire never
    /// carried, and the provider bills its own default length.
    duration: u32,
}

#[derive(Deserialize)]
struct VideoSubmitResponse {
    // The submit response also carries an initial `task_status`; we only need
    // the job id here (the first `poll_video` reports the live status).
    id: String,
}

#[derive(Deserialize)]
struct VideoPollResponse {
    #[serde(default)]
    task_status: Option<String>,
    #[serde(default)]
    video_result: Vec<VideoResultItem>,
}

#[derive(Deserialize)]
struct VideoResultItem {
    #[serde(default)]
    url: Option<String>,
}

#[async_trait]
impl MediaProvider for ZaiVideoProvider {
    fn id(&self) -> &str {
        "zai"
    }

    fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            provider_id: "zai".to_string(),
            image: false,
            video: true,
            image_edit: false,
            sizes: Vec::new(),
            image_usd_each: None,
            video_usd_per_second: Some(DEFAULT_VIDEO_USD_PER_SECOND),
        }
    }

    async fn generate_image(&self, _req: ImageRequest) -> Result<MediaArtifact, MediaError> {
        Err(MediaError::CapabilityUnavailable {
            capability: "image".into(),
            enabling_keys: "ZAI_API_KEY (via the CogView adapter)".into(),
        })
    }

    async fn generate_video(&self, req: VideoRequest) -> Result<MediaJob, MediaError> {
        let body = VideoGenRequest {
            model: &self.model,
            prompt: &req.prompt,
            duration: req.duration_secs,
        };
        let response = self
            .client
            .post(format!("{}/videos/generations", self.base_url))
            .bearer_auth(self.api_key.reveal())
            .json(&body)
            .send()
            .await
            .map_err(|e| MediaError::Transport(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after_ms = parse_retry_after_ms(response.headers());
            let text = response.text().await.unwrap_or_default();
            return Err(classify_http_error("zai", status, retry_after_ms, &text));
        }

        let parsed: VideoSubmitResponse = response
            .json()
            .await
            .map_err(|e| MediaError::Malformed(format!("zai video submit: {e}")))?;

        let estimated_cost_usd = self
            .capabilities()
            .estimate_video(req.duration_secs)
            .map(|e| e.estimated_usd)
            .unwrap_or(0.0);

        Ok(MediaJob {
            artifact_id: artifact_id_for(&parsed.id),
            provider_id: "zai".to_string(),
            provider_job_id: parsed.id,
            kind: MediaKind::Video,
            model: self.model.clone(),
            estimated_cost_usd,
            submitted_at: now_unix_secs(),
            label: req.label,
        })
    }

    async fn poll_video(&self, job: &MediaJob) -> Result<MediaJobStatus, MediaError> {
        let response = self
            .client
            .get(format!(
                "{}/async-result/{}",
                self.base_url, job.provider_job_id
            ))
            .bearer_auth(self.api_key.reveal())
            .send()
            .await
            .map_err(|e| MediaError::Transport(e.to_string()))?;

        // L-V3: a job the provider says is gone is *gone*, reported as a
        // terminal Failed state — never a cached "running".
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(MediaJobStatus {
                state: MediaJobState::Failed {
                    reason: format!(
                        "provider has no record of job `{}` (expired or purged)",
                        job.provider_job_id
                    ),
                },
                progress: None,
                artifact: None,
            });
        }
        if !response.status().is_success() {
            let status = response.status();
            let retry_after_ms = parse_retry_after_ms(response.headers());
            let text = response.text().await.unwrap_or_default();
            return Err(classify_http_error("zai", status, retry_after_ms, &text));
        }

        let parsed: VideoPollResponse = response
            .json()
            .await
            .map_err(|e| MediaError::Malformed(format!("zai video poll: {e}")))?;

        let status_str = parsed.task_status.unwrap_or_default().to_ascii_uppercase();
        match status_str.as_str() {
            "SUCCESS" => {
                let url = parsed
                    .video_result
                    .into_iter()
                    .find_map(|item| item.url)
                    .ok_or_else(|| {
                        MediaError::Malformed("zai video succeeded without a result url".into())
                    })?;
                let bytes = download_bytes(&self.client, &url, "zai").await?;
                let artifact = MediaArtifact {
                    kind: MediaKind::Video,
                    bytes,
                    extension: "mp4".to_string(),
                    label: job.label.clone(),
                    model: job.model.clone(),
                    cost_usd: job.estimated_cost_usd,
                };
                Ok(MediaJobStatus {
                    state: MediaJobState::Succeeded,
                    progress: Some(1.0),
                    artifact: Some(artifact),
                })
            }
            "FAIL" | "FAILED" | "ERROR" => Ok(MediaJobStatus {
                state: MediaJobState::Failed {
                    reason: "provider reported the video job failed".into(),
                },
                progress: None,
                artifact: None,
            }),
            "QUEUING" | "QUEUED" | "PENDING" => Ok(MediaJobStatus {
                state: MediaJobState::Queued,
                progress: None,
                artifact: None,
            }),
            // PROCESSING/RUNNING and any unknown-but-non-terminal status.
            _ => Ok(MediaJobStatus {
                state: MediaJobState::Running,
                progress: None,
                artifact: None,
            }),
        }
    }
}

/// Deterministically derive our artifact id from the provider's job id, so a
/// resume after a restart reconstructs the same identity for events and the
/// final file.
fn artifact_id_for(provider_job_id: &str) -> String {
    let digest = Sha256::digest(provider_job_id.as_bytes());
    let mut hex = String::with_capacity(12);
    for byte in &digest[..6] {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("med_{hex}")
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(uri: &str) -> ZaiVideoProvider {
        ZaiVideoProvider::new(ApiKey::new("sk-test-zai"), DEFAULT_MODEL).with_base_url(uri)
    }

    fn running_job(id: &str) -> MediaJob {
        MediaJob {
            artifact_id: artifact_id_for(id),
            provider_id: "zai".into(),
            provider_job_id: id.into(),
            kind: MediaKind::Video,
            model: DEFAULT_MODEL.into(),
            estimated_cost_usd: 2.0,
            submitted_at: 1_700_000_000,
            label: "teaser".into(),
        }
    }

    #[tokio::test]
    async fn submit_returns_a_persistable_job_handle() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/videos/generations"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "id": "job-42", "task_status": "PROCESSING" }),
                ),
            )
            .mount(&server)
            .await;

        let job = provider(&server.uri())
            .generate_video(VideoRequest::new("10s product teaser", 10))
            .await
            .unwrap();
        assert_eq!(job.provider_job_id, "job-42");
        assert_eq!(job.kind, MediaKind::Video);
        assert!(job.estimated_cost_usd > 0.0);
        assert_eq!(job.artifact_id, artifact_id_for("job-42"));
    }

    /// The cost gate prices the job by `duration_secs`, so that duration must
    /// actually reach the provider — otherwise the approved estimate is for a
    /// length the wire request never carried.
    #[tokio::test]
    async fn the_requested_duration_reaches_the_provider() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/videos/generations"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "id": "job-7", "task_status": "PROCESSING" }),
                ),
            )
            .mount(&server)
            .await;

        provider(&server.uri())
            .generate_video(VideoRequest::new("a 4s ember loop", 4))
            .await
            .unwrap();

        let requests = server.received_requests().await.expect("recorded requests");
        let body = String::from_utf8_lossy(&requests[0].body);
        assert!(
            body.contains("\"duration\":4"),
            "the wire request must carry the requested duration, got: {body}"
        );
    }

    #[tokio::test]
    async fn poll_success_downloads_the_video() {
        let server = MockServer::start().await;
        let video_url = format!("{}/vids/out.mp4", server.uri());
        Mock::given(method("GET"))
            .and(path("/async-result/job-42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "task_status": "SUCCESS",
                "video_result": [ { "url": video_url } ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/vids/out.mp4"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"mp4-bytes".to_vec(), "video/mp4"),
            )
            .mount(&server)
            .await;

        let status = provider(&server.uri())
            .poll_video(&running_job("job-42"))
            .await
            .unwrap();
        assert_eq!(status.state, MediaJobState::Succeeded);
        let art = status.artifact.expect("succeeded job carries an artifact");
        assert_eq!(art.bytes, b"mp4-bytes");
        assert_eq!(art.extension, "mp4");
    }

    #[tokio::test]
    async fn poll_processing_is_running() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/async-result/job-42"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "task_status": "PROCESSING" })),
            )
            .mount(&server)
            .await;
        let status = provider(&server.uri())
            .poll_video(&running_job("job-42"))
            .await
            .unwrap();
        assert_eq!(status.state, MediaJobState::Running);
        assert!(!status.is_terminal());
    }

    #[tokio::test]
    async fn poll_fail_is_failed_with_reason() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/async-result/job-42"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "task_status": "FAIL" })),
            )
            .mount(&server)
            .await;
        let status = provider(&server.uri())
            .poll_video(&running_job("job-42"))
            .await
            .unwrap();
        assert!(matches!(status.state, MediaJobState::Failed { .. }));
    }

    #[tokio::test]
    async fn poll_404_reports_the_job_gone_not_running_lv3() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/async-result/job-ghost"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let status = provider(&server.uri())
            .poll_video(&running_job("job-ghost"))
            .await
            .unwrap();
        assert!(status.is_terminal());
        match &status.state {
            MediaJobState::Failed { reason } => {
                assert!(reason.contains("job-ghost"), "{reason}");
                assert!(
                    reason.contains("expired") || reason.contains("no record"),
                    "{reason}"
                );
            }
            other => panic!("expected Failed(gone), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submit_401_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/videos/generations"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;
        let err = provider(&server.uri())
            .generate_video(VideoRequest::new("x", 5))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::Auth(_)));
    }

    #[test]
    fn capabilities_report_video_only() {
        let caps = provider("http://unused").capabilities();
        assert!(caps.video);
        assert!(!caps.image);
        assert!(caps.video_usd_per_second.is_some());
    }

    // Live smoke (L-V4): video actually spends, so it is gated on BOTH the key
    // and an explicit OXAGEN_MEDIA_LIVE=1 opt-in; otherwise it no-ops.
    #[tokio::test]
    async fn live_smoke_submit_and_poll_once() {
        if std::env::var("OXAGEN_MEDIA_LIVE").is_err() {
            return;
        }
        let key = match ApiKey::from_env("ZAI_API_KEY") {
            Ok(k) => k,
            Err(_) => return,
        };
        let provider = ZaiVideoProvider::new(key, DEFAULT_MODEL);
        let job = provider
            .generate_video(VideoRequest::new("a 4s abstract ember loop", 4))
            .await
            .expect("live CogVideoX submit");
        let status = provider.poll_video(&job).await.expect("live poll");
        // Right after submit it is almost certainly not yet terminal; we only
        // assert the call succeeded and returned a well-formed state.
        assert!(matches!(
            status.state,
            MediaJobState::Queued | MediaJobState::Running | MediaJobState::Succeeded
        ));
    }
}
