//! Vertex AI adapter — Google's enterprise surface for the same
//! `generateContent` wire shape `gemini.rs` speaks
//! ("Vertex | ADC | generateContent | catalog-driven … Enterprise path;
//! casual Gemini use → direct adapter"). The request/response envelope,
//! tool-call dialect (`gemini-functions`), and stream aggregation are all
//! shared with `gemini.rs`; what differs is auth and addressing:
//!
//! - **Auth**: an OAuth2 bearer token, not an API key. The full ADC chain
//!   (service-account JWT signing, metadata-server exchange) is deliberately
//!   out of scope for this first cut — the adapter takes a ready token,
//!   which `stella-cli` resolves from `VERTEX_ACCESS_TOKEN` (documented as
//!   `export VERTEX_ACCESS_TOKEN=$(gcloud auth print-access-token)`), the
//!   same "ready credential in, provider-native acquisition later" posture
//!   the credential chain doc already records for Bedrock/Vertex.
//! - **Addressing**: requests are project- and location-scoped:
//!   `{base}/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:streamGenerateContent`,
//!   where the base host is `aiplatform.googleapis.com` for the `global`
//!   location and `{location}-aiplatform.googleapis.com` for a pinned
//!   region.

use async_trait::async_trait;
use stella_protocol::{CompletionRequest, CompletionResult, ProviderError};

use crate::catalog::{Catalog, Pricing};
use crate::credential::ApiKey;
use crate::gemini::{
    GeminiRequest, aggregate_gemini_stream, build_generation_config, classify_google_error,
    to_gemini_request_parts, to_gemini_tools,
};
use crate::http;
use crate::provider::Provider;

pub struct VertexProvider {
    client: reqwest::Client,
    access_token: ApiKey,
    model: String,
    project: String,
    location: String,
    base_url_override: Option<String>,
    /// List pricing for `model`, resolved from the catalog at construction so
    /// `cost_usd` is computed on the real request path — never a hard-coded
    /// zero (which would silently disable budget enforcement for Vertex).
    pricing: Option<Pricing>,
}

impl VertexProvider {
    /// Build an adapter for `model` in `project`/`location`. `location`
    /// `"global"` selects the global endpoint; anything else selects the
    /// matching regional host.
    pub fn new(
        access_token: ApiKey,
        model: impl Into<String>,
        project: impl Into<String>,
        location: impl Into<String>,
    ) -> Self {
        let model = model.into();
        let catalog = Catalog::current();
        let pricing = catalog
            .resolve_for("vertex", &model)
            .ok()
            .map(|e| e.pricing);
        Self {
            client: http::client(),
            access_token,
            model,
            project: project.into(),
            location: location.into(),
            base_url_override: None,
            pricing,
        }
    }

    /// Override the scheme+host — used by conformance tests against a mock
    /// server, and by anyone routing through a private proxy. The
    /// project/location path structure is preserved either way.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url_override = Some(base_url.into());
        self
    }

    fn endpoint(&self) -> String {
        let base = match &self.base_url_override {
            Some(base) => base.clone(),
            None if self.location == "global" => "https://aiplatform.googleapis.com".to_string(),
            None => format!("https://{}-aiplatform.googleapis.com", self.location),
        };
        format!(
            "{base}/v1/projects/{}/locations/{}/publishers/google/models/{}:streamGenerateContent?alt=sse",
            self.project, self.location, self.model
        )
    }
}

#[async_trait]
impl Provider for VertexProvider {
    fn id(&self) -> &str {
        "vertex"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        let (system_instruction, contents) = to_gemini_request_parts(&req.messages);
        let body = GeminiRequest {
            system_instruction,
            contents,
            tools: to_gemini_tools(&req.tools),
            generation_config: build_generation_config(&req),
        };

        let response = self
            .client
            .post(self.endpoint())
            .bearer_auth(self.access_token.reveal())
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !response.status().is_success() {
            return Err(classify_google_error("Vertex AI", response, &self.model).await);
        }

        let (text, tool_calls, usage, finish_reason) =
            aggregate_gemini_stream("Vertex AI", response).await?;
        let cost_usd = self.pricing.map(|p| p.cost_usd(&usage)).unwrap_or(0.0);
        Ok(CompletionResult {
            text,
            tool_calls,
            usage,
            model: self.model.clone(),
            cost_usd,
            finish_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::CompletionMessage;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn endpoint_uses_the_global_host_for_the_global_location() {
        let provider =
            VertexProvider::new(ApiKey::new("token"), "gemini-3-pro", "my-project", "global");
        assert_eq!(
            provider.endpoint(),
            "https://aiplatform.googleapis.com/v1/projects/my-project/locations/global/publishers/google/models/gemini-3-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn endpoint_uses_the_regional_host_for_a_pinned_region() {
        let provider = VertexProvider::new(
            ApiKey::new("token"),
            "gemini-3-pro",
            "my-project",
            "us-central1",
        );
        assert_eq!(
            provider.endpoint(),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-3-pro:streamGenerateContent?alt=sse"
        );
    }

    #[tokio::test]
    async fn complete_sends_a_bearer_token_to_the_project_scoped_path() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi from Vertex\"}]}}],",
            "\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":4,\"cachedContentTokenCount\":3}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path(
                "/v1/projects/my-project/locations/global/publishers/google/models/gemini-3-pro:streamGenerateContent",
            ))
            .and(query_param("alt", "sse"))
            .and(header("authorization", "Bearer ya29.test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = VertexProvider::new(
            ApiKey::new("ya29.test-token"),
            "gemini-3-pro",
            "my-project",
            "global",
        )
        .with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        };

        let result = provider
            .complete(req)
            .await
            .expect("completion should succeed");
        assert_eq!(result.text, "Hi from Vertex");
        assert_eq!(result.usage.input_tokens, 7);
        assert_eq!(result.usage.output_tokens, 4);
        // Vertex shares the gemini aggregator: implicit-cache hits reported
        // as `cachedContentTokenCount` must reach the normalized envelope
        // here too, or Vertex runs bill cached tokens at the full rate.
        assert_eq!(result.usage.cached_input_tokens, 3);
    }

    #[tokio::test]
    async fn complete_maps_401_to_auth_error_naming_vertex_not_gemini() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthenticated"))
            .mount(&server)
            .await;

        let provider = VertexProvider::new(
            ApiKey::new("expired-token"),
            "gemini-3-pro",
            "my-project",
            "global",
        )
        .with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        };

        let err = provider.complete(req).await.unwrap_err();
        match &err {
            ProviderError::Auth(message) => {
                assert!(message.contains("Vertex AI"), "{message}");
            }
            other => panic!("expected Auth, got {other:?}"),
        }
        assert!(!err.is_retryable());
    }

    /// Vertex shares `build_generation_config` with `gemini.rs`, so the
    /// params/reasoning mapping (and its byte-stability early-out) is proven
    /// there; this pins that the shared config actually rides the Vertex
    /// request body — a regression guard against the two adapters drifting
    /// onto separate builders.
    #[tokio::test]
    async fn complete_sends_shared_generation_config_params_on_the_wire() {
        use stella_protocol::GenerationParams;
        let server = MockServer::start().await;
        let sse_body = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]}}]}\n\n";
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = VertexProvider::new(
            ApiKey::new("ya29.test-token"),
            "gemini-3-pro",
            "my-project",
            "global",
        )
        .with_base_url(server.uri());
        provider
            .complete(CompletionRequest {
                messages: vec![CompletionMessage::user("hi")],
                max_output_tokens: None,
                temperature: None,
                effort: None,
                tools: vec![],
                reasoning: Some(true),
                params: Some(GenerationParams {
                    top_p: Some(0.9),
                    top_k: Some(40),
                    seed: Some(7),
                    ..Default::default()
                }),
            })
            .await
            .expect("should succeed");

        let requests = server.received_requests().await.expect("recorded requests");
        let body = String::from_utf8_lossy(&requests[0].body);
        assert!(body.contains("\"topP\":0.9"), "{body}");
        assert!(body.contains("\"topK\":40"), "{body}");
        assert!(body.contains("\"seed\":7"), "{body}");
        assert!(
            body.contains("\"thinkingConfig\":{\"thinkingLevel\":\"high\"}"),
            "{body}"
        );
    }
}
