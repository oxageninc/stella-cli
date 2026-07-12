//! OpenAI gpt-image adapter (`08-multimodal.md` §2). `POST /images/generations`
//! with `response_format` defaulting to base64: gpt-image-1 returns the image
//! inline as `b64_json`, which this adapter decodes to bytes (no second
//! download round trip, unlike Z.ai's URL response).
//!
//! Same `OPENAI_API_KEY` bearer-auth and `api.openai.com/v1` base-URL
//! convention as `stella-model::openai`. Error classification routes through
//! the shared [`crate::http`] policy.

use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use stella_protocol::MediaKind;

use crate::credential::ApiKey;
use crate::error::MediaError;
use crate::http::{classify_http_error, parse_retry_after_ms};
use crate::provider::{
    ImageRequest, ImageSize, MediaArtifact, MediaCapabilities, MediaJob, MediaJobStatus,
    MediaProvider, VideoRequest,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Default gpt-image slug used when the caller doesn't pin one.
pub const DEFAULT_MODEL: &str = "gpt-image-1";
/// Documented default rate — catalog data once wired (`08-multimodal.md` §2).
const DEFAULT_IMAGE_USD_EACH: f64 = 0.04;

/// A gpt-image provider.
pub struct OpenAiImageProvider {
    client: reqwest::Client,
    api_key: ApiKey,
    base_url: String,
    model: String,
}

impl OpenAiImageProvider {
    pub fn new(api_key: ApiKey, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
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
struct ImageGenRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    size: String,
    n: u32,
}

#[derive(Deserialize)]
struct ImageGenResponse {
    #[serde(default)]
    data: Vec<ImageDatum>,
}

#[derive(Deserialize)]
struct ImageDatum {
    #[serde(default)]
    b64_json: Option<String>,
}

#[async_trait]
impl MediaProvider for OpenAiImageProvider {
    fn id(&self) -> &str {
        "openai"
    }

    fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            provider_id: "openai".to_string(),
            image: true,
            video: false,
            image_edit: true,
            sizes: vec![
                ImageSize::square(1024),
                ImageSize::new(1024, 1536),
                ImageSize::new(1536, 1024),
            ],
            image_usd_each: Some(DEFAULT_IMAGE_USD_EACH),
            video_usd_per_second: None,
        }
    }

    async fn generate_image(&self, req: ImageRequest) -> Result<MediaArtifact, MediaError> {
        let body = ImageGenRequest {
            model: &self.model,
            prompt: &req.prompt,
            size: req.size.to_string(),
            n: 1,
        };
        let response = self
            .client
            .post(format!("{}/images/generations", self.base_url))
            .bearer_auth(self.api_key.reveal())
            .json(&body)
            .send()
            .await
            .map_err(|e| MediaError::Transport(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after_ms = parse_retry_after_ms(response.headers());
            let text = response.text().await.unwrap_or_default();
            return Err(classify_http_error("openai", status, retry_after_ms, &text));
        }

        let parsed: ImageGenResponse = response
            .json()
            .await
            .map_err(|e| MediaError::Malformed(format!("openai image response: {e}")))?;
        let b64 = parsed
            .data
            .into_iter()
            .next()
            .and_then(|d| d.b64_json)
            .ok_or_else(|| MediaError::Malformed("openai image response had no b64_json".into()))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| MediaError::Malformed(format!("openai image base64: {e}")))?;

        let cost_usd = self
            .capabilities()
            .estimate_image(1, req.size)
            .map(|e| e.estimated_usd)
            .unwrap_or(0.0);

        Ok(MediaArtifact {
            kind: MediaKind::Image,
            bytes,
            extension: "png".to_string(),
            label: req.label,
            model: self.model.clone(),
            cost_usd,
        })
    }

    async fn generate_video(&self, _req: VideoRequest) -> Result<MediaJob, MediaError> {
        // Sora is entitlement-gated and not wired here (`08-multimodal.md` §2).
        Err(MediaError::CapabilityUnavailable {
            capability: "video".into(),
            enabling_keys: "a video-capable provider (e.g. ZAI_API_KEY for CogVideoX)".into(),
        })
    }

    async fn poll_video(&self, _job: &MediaJob) -> Result<MediaJobStatus, MediaError> {
        Err(MediaError::CapabilityUnavailable {
            capability: "video".into(),
            enabling_keys: "a video-capable provider (e.g. ZAI_API_KEY for CogVideoX)".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(uri: &str) -> OpenAiImageProvider {
        OpenAiImageProvider::new(ApiKey::new("sk-test-openai"), DEFAULT_MODEL).with_base_url(uri)
    }

    #[tokio::test]
    async fn generate_image_decodes_b64_json() {
        let server = MockServer::start().await;
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"png-real-bytes");
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .and(header("authorization", "Bearer sk-test-openai"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": [ { "b64_json": b64 } ] })),
            )
            .mount(&server)
            .await;

        let art = provider(&server.uri())
            .generate_image(ImageRequest::new("a logo", ImageSize::square(1024)))
            .await
            .unwrap();
        assert_eq!(art.bytes, b"png-real-bytes");
        assert_eq!(art.kind, MediaKind::Image);
        assert!(art.cost_usd > 0.0);
    }

    #[tokio::test]
    async fn unauthorized_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;
        let err = provider(&server.uri())
            .generate_image(ImageRequest::new("x", ImageSize::square(1024)))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::Auth(_)));
    }

    #[tokio::test]
    async fn content_policy_refusal_is_classified() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "{\"error\":{\"message\":\"Your request was rejected by our safety system\"}}",
            ))
            .mount(&server)
            .await;
        let err = provider(&server.uri())
            .generate_image(ImageRequest::new("x", ImageSize::square(1024)))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::ContentPolicy(_)));
    }

    #[tokio::test]
    async fn missing_b64_is_malformed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [ { } ] })),
            )
            .mount(&server)
            .await;
        let err = provider(&server.uri())
            .generate_image(ImageRequest::new("x", ImageSize::square(1024)))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::Malformed(_)));
    }

    #[tokio::test]
    async fn video_is_unavailable_on_the_image_adapter() {
        let err = provider("http://unused")
            .generate_video(VideoRequest::new("x", 5))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::CapabilityUnavailable { .. }));
    }

    // Live smoke (L-V4): gated on the key and an explicit opt-in.
    #[tokio::test]
    async fn live_smoke_generate_image() {
        if std::env::var("OXAGEN_MEDIA_LIVE").is_err() {
            return;
        }
        let key = match ApiKey::from_env("OPENAI_API_KEY") {
            Ok(k) => k,
            Err(_) => return,
        };
        let provider = OpenAiImageProvider::new(key, DEFAULT_MODEL);
        let art = provider
            .generate_image(ImageRequest::new(
                "a minimal ember-orange hexagon on charcoal",
                ImageSize::square(1024),
            ))
            .await
            .expect("live gpt-image");
        assert!(!art.bytes.is_empty());
    }
}
