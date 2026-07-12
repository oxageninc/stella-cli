//! Z.ai CogView image adapter (`08-multimodal.md` §2 — Z.ai is the default
//! family). Same `api.z.ai/api/paas/v4` base-URL convention as the chat
//! adapter (`stella-model::zai`). CogView's `POST /images/generations` returns
//! a URL (or inline base64) for the generated image; this adapter downloads
//! the URL to bytes so the caller gets a self-contained
//! [`MediaArtifact`](crate::provider::MediaArtifact), never a URL that could
//! expire.
//!
//! Error classification (auth / rate-limit / content-policy / terminal)
//! routes through [`crate::http::classify_http_error`], mirroring
//! `ProviderError`'s categories with this crate's [`MediaError`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use stella_protocol::MediaKind;

use crate::credential::ApiKey;
use crate::error::MediaError;
use crate::http::{classify_http_error, download_bytes, parse_retry_after_ms};
use crate::provider::{
    ImageRequest, ImageSize, MediaArtifact, MediaCapabilities, MediaJob, MediaJobStatus,
    MediaProvider, VideoRequest,
};

/// Z.ai international base URL (`api.z.ai`); `open.bigmodel.cn` is the same
/// wire shape behind `with_base_url`.
const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/paas/v4";
/// Default CogView slug used when the caller doesn't pin one.
pub const DEFAULT_MODEL: &str = "cogview-4";
/// Documented default rate — real pricing comes from the catalog once wired
/// (`08-multimodal.md` §2: costs are catalog data, never truly hard-coded).
const DEFAULT_IMAGE_USD_EACH: f64 = 0.06;

/// A CogView image provider.
pub struct ZaiImageProvider {
    client: reqwest::Client,
    api_key: ApiKey,
    base_url: String,
    model: String,
}

impl ZaiImageProvider {
    pub fn new(api_key: ApiKey, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
        }
    }

    /// Override the base URL — used by fixtures and private proxies.
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
}

#[derive(Deserialize)]
struct ImageGenResponse {
    #[serde(default)]
    data: Vec<ImageDatum>,
}

#[derive(Deserialize)]
struct ImageDatum {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    b64_json: Option<String>,
}

#[async_trait]
impl MediaProvider for ZaiImageProvider {
    fn id(&self) -> &str {
        "zai"
    }

    fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            provider_id: "zai".to_string(),
            image: true,
            video: false,
            image_edit: false,
            sizes: vec![
                ImageSize::square(1024),
                ImageSize::new(768, 1344),
                ImageSize::new(1344, 768),
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
            return Err(classify_http_error("zai", status, retry_after_ms, &text));
        }

        let parsed: ImageGenResponse = response
            .json()
            .await
            .map_err(|e| MediaError::Malformed(format!("zai image response: {e}")))?;
        let datum = parsed
            .data
            .into_iter()
            .next()
            .ok_or_else(|| MediaError::Malformed("zai image response had no data".into()))?;

        let bytes = decode_image(&self.client, datum).await?;
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
        Err(MediaError::CapabilityUnavailable {
            capability: "video".into(),
            enabling_keys: "ZAI_API_KEY (via the CogVideoX adapter)".into(),
        })
    }

    async fn poll_video(&self, _job: &MediaJob) -> Result<MediaJobStatus, MediaError> {
        Err(MediaError::CapabilityUnavailable {
            capability: "video".into(),
            enabling_keys: "ZAI_API_KEY (via the CogVideoX adapter)".into(),
        })
    }
}

/// Resolve one image datum to bytes: prefer inline base64, else download the
/// URL. A datum with neither is a malformed response.
async fn decode_image(client: &reqwest::Client, datum: ImageDatum) -> Result<Vec<u8>, MediaError> {
    if let Some(b64) = datum.b64_json {
        use base64::Engine as _;
        return base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| MediaError::Malformed(format!("zai image base64: {e}")));
    }
    if let Some(url) = datum.url {
        return download_bytes(client, &url, "zai").await;
    }
    Err(MediaError::Malformed(
        "zai image datum had neither url nor b64_json".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(uri: &str) -> ZaiImageProvider {
        ZaiImageProvider::new(ApiKey::new("sk-test-zai"), DEFAULT_MODEL).with_base_url(uri)
    }

    #[tokio::test]
    async fn generate_image_downloads_the_returned_url() {
        let server = MockServer::start().await;
        let img_url = format!("{}/img/out.png", server.uri());
        let gen_body = serde_json::json!({ "data": [ { "url": img_url } ] });
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .and(header("authorization", "Bearer sk-test-zai"))
            .respond_with(ResponseTemplate::new(200).set_body_json(gen_body))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/img/out.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(b"\x89PNG-real-bytes".to_vec(), "image/png"),
            )
            .mount(&server)
            .await;

        let art = provider(&server.uri())
            .generate_image(ImageRequest::new("a wordmark", ImageSize::square(1024)))
            .await
            .unwrap();
        assert_eq!(art.kind, MediaKind::Image);
        assert_eq!(art.bytes, b"\x89PNG-real-bytes");
        assert_eq!(art.extension, "png");
        assert!(art.cost_usd > 0.0);
    }

    #[tokio::test]
    async fn generate_image_decodes_inline_base64() {
        let server = MockServer::start().await;
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"inline-png");
        let gen_body = serde_json::json!({ "data": [ { "b64_json": b64 } ] });
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(gen_body))
            .mount(&server)
            .await;

        let art = provider(&server.uri())
            .generate_image(ImageRequest::new("logo", ImageSize::square(1024)))
            .await
            .unwrap();
        assert_eq!(art.bytes, b"inline-png");
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
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn rate_limited_is_retryable_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "2")
                    .set_body_string("slow down"),
            )
            .mount(&server)
            .await;
        let err = provider(&server.uri())
            .generate_image(ImageRequest::new("x", ImageSize::square(1024)))
            .await
            .unwrap_err();
        match err {
            MediaError::RateLimited { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(2000));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn content_policy_refusal_is_classified() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("{\"error\":\"prompt blocked by our safety policy\"}"),
            )
            .mount(&server)
            .await;
        let err = provider(&server.uri())
            .generate_image(ImageRequest::new("x", ImageSize::square(1024)))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::ContentPolicy(_)));
    }

    #[tokio::test]
    async fn empty_data_is_malformed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
            .mount(&server)
            .await;
        let err = provider(&server.uri())
            .generate_image(ImageRequest::new("x", ImageSize::square(1024)))
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::Malformed(_)));
    }

    #[test]
    fn capabilities_report_image_only() {
        let caps = provider("http://unused").capabilities();
        assert!(caps.image);
        assert!(!caps.video);
        assert!(caps.image_usd_each.is_some());
    }

    // Live smoke (L-V4): runs only when a real key AND OXAGEN_MEDIA_LIVE=1 are
    // present, so CI never spends. Otherwise it no-ops (runtime-skip).
    #[tokio::test]
    async fn live_smoke_generate_image() {
        if std::env::var("OXAGEN_MEDIA_LIVE").is_err() {
            return; // opt-in only
        }
        let key = match ApiKey::from_env("ZAI_API_KEY") {
            Ok(k) => k,
            Err(_) => return, // no key → skip
        };
        let provider = ZaiImageProvider::new(key, DEFAULT_MODEL);
        let art = provider
            .generate_image(ImageRequest::new(
                "a minimal ember-orange hexagon on charcoal",
                ImageSize::square(1024),
            ))
            .await
            .expect("live CogView image");
        assert!(!art.bytes.is_empty());
    }
}
