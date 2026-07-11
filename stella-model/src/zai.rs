//! Z.ai adapter — OpenAI-compatible chat completions + SSE streaming, GLM
//! 5.2's tool-call dialect (`openai-json`: an accumulating `tool_calls`
//! array keyed by index, arguments streamed as string fragments). This is
//! the *other* Phase 0 spike (`03-plan.md` step 3) and the default suite
//! per `07-model-matrix.md` — it must work first, not last.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stella_protocol::{
    CompletionMessage, CompletionRequest, CompletionResult, CompletionUsage, MessageRole,
    ProviderError, ToolCall,
};

use crate::catalog::{Catalog, Pricing};
use crate::credential::ApiKey;
use crate::http;
use crate::provider::Provider;
use crate::sse::SseDecoder;

/// International endpoint. `open.bigmodel.cn` (mainland) is the same wire
/// shape behind a different base URL — `with_base_url` covers both without
/// a second adapter (`07-model-matrix.md` §2 note).
const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

pub struct ZaiProvider {
    client: reqwest::Client,
    api_key: ApiKey,
    base_url: String,
    model: String,
    /// List pricing for `model`, resolved from the catalog at construction so
    /// `cost_usd` is computed on the real request path. `None` only if the
    /// slug isn't in the catalog — `build_provider` (`agent.rs`) rejects that
    /// case up front, so in practice this is always `Some` for a live call.
    pricing: Option<Pricing>,
}

impl ZaiProvider {
    pub fn new(api_key: ApiKey, model: impl Into<String>) -> Self {
        let model = model.into();
        let pricing = Catalog::seed().resolve(&model).ok().map(|e| e.pricing);
        Self {
            client: http::client(),
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            model,
            pricing,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

// ── Wire types (OpenAI-compatible chat/completions) ─────────────────────

#[derive(Serialize)]
struct ZaiRequest<'a> {
    model: &'a str,
    messages: Vec<ZaiMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ZaiToolSchema>,
}

#[derive(Serialize)]
struct ZaiMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ZaiOutboundToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// An assistant-authored tool call echoed back in conversation history
/// (OpenAI-compatible dialect requires the assistant message to carry the
/// calls its tool results answer).
#[derive(Serialize)]
struct ZaiOutboundToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: ZaiOutboundFunction,
}

#[derive(Serialize)]
struct ZaiOutboundFunction {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct ZaiToolSchema {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ZaiFunctionSchema,
}

#[derive(Serialize)]
struct ZaiFunctionSchema {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Deserialize, Debug)]
struct ZaiStreamChunk {
    #[serde(default)]
    choices: Vec<ZaiStreamChoice>,
    #[serde(default)]
    usage: Option<ZaiUsage>,
    /// An in-band error frame. The OpenAI-compatible gateways can emit
    /// `data: {"error":{...}}` mid-stream after already sending some content
    /// deltas — without this field it deserialized into an all-default,
    /// empty `ZaiStreamChunk` and was silently swallowed, returning the
    /// truncated text so far as a bogus success.
    #[serde(default)]
    error: Option<ZaiStreamError>,
}

/// An OpenAI-compatible in-band error object. `code` is `Value` because
/// gateways disagree on its type (string on some, integer HTTP status on
/// others) — we only classify on `type`/`message` text, so its exact type
/// doesn't matter.
#[derive(Deserialize, Debug, Default)]
struct ZaiStreamError {
    #[serde(default)]
    message: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    #[allow(dead_code)]
    code: Option<Value>,
}

/// Classify an in-band OpenAI-compatible error frame into a typed
/// `ProviderError`. Transient/server-side conditions (overloaded, 5xx,
/// unavailable, timeout) are **retryable** `Transport`; an explicit rate
/// limit is `RateLimited`; everything else is `Terminal`. The gateways don't
/// share a stable machine code, so this matches on the human-readable
/// `type`/`message` text — deliberately conservative: unknown ⇒ terminal, so
/// a genuine failure is never retried into an infinite loop.
fn classify_zai_stream_error(err: &ZaiStreamError) -> ProviderError {
    let haystack = format!("{} {}", err.kind, err.message).to_lowercase();
    let detail = if err.message.is_empty() {
        format!("Z.ai stream error ({})", err.kind)
    } else {
        format!("Z.ai stream error: {}", err.message)
    };
    if haystack.contains("overload")
        || haystack.contains("server_error")
        || haystack.contains("unavailable")
        || haystack.contains("timeout")
        || haystack.contains("500")
        || haystack.contains("502")
        || haystack.contains("503")
        || haystack.contains("529")
    {
        ProviderError::Transport(detail)
    } else if haystack.contains("rate") && haystack.contains("limit") {
        ProviderError::RateLimited {
            message: detail,
            retry_after_ms: None,
        }
    } else {
        ProviderError::Terminal(detail)
    }
}

#[derive(Deserialize, Debug)]
struct ZaiStreamChoice {
    #[serde(default)]
    delta: ZaiStreamDelta,
}

#[derive(Deserialize, Debug, Default)]
struct ZaiStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ZaiStreamToolCallDelta>,
}

/// GLM 5.2's streamed tool-call shape: fragments keyed by `index`, with
/// `function.arguments` arriving as a partial JSON string across many
/// chunks — the exact dialect quirk this adapter exists to prove out.
#[derive(Deserialize, Debug)]
struct ZaiStreamToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ZaiStreamFunctionDelta>,
}

#[derive(Deserialize, Debug, Default)]
struct ZaiStreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct ZaiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

fn to_zai_messages(messages: &[CompletionMessage]) -> Vec<ZaiMessage> {
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            MessageRole::System => out.push(ZaiMessage {
                role: "system",
                content: message.content.clone(),
                tool_calls: Vec::new(),
                tool_call_id: None,
            }),
            MessageRole::User => out.push(ZaiMessage {
                role: "user",
                content: message.content.clone(),
                tool_calls: Vec::new(),
                tool_call_id: None,
            }),
            MessageRole::Assistant => out.push(ZaiMessage {
                role: "assistant",
                content: message.content.clone(),
                tool_calls: message
                    .tool_calls
                    .iter()
                    .map(|call| ZaiOutboundToolCall {
                        id: call.call_id.clone(),
                        kind: "function",
                        function: ZaiOutboundFunction {
                            name: call.name.clone(),
                            arguments: call.input.to_string(),
                        },
                    })
                    .collect(),
                tool_call_id: None,
            }),
            // OpenAI-compatible dialect: one `role: "tool"` message per
            // result, each carrying the `tool_call_id` it answers.
            MessageRole::Tool => {
                for result in &message.tool_results {
                    let content = match &result.output {
                        stella_protocol::ToolOutput::Ok { content } => content.clone(),
                        stella_protocol::ToolOutput::Error { message } => {
                            format!("ERROR: {message}")
                        }
                    };
                    out.push(ZaiMessage {
                        role: "tool",
                        content,
                        tool_calls: Vec::new(),
                        tool_call_id: Some(result.call_id.clone()),
                    });
                }
            }
        }
    }
    out
}

fn to_zai_tools(tools: &[stella_protocol::tool::ToolSchema]) -> Vec<ZaiToolSchema> {
    tools
        .iter()
        .map(|tool| ZaiToolSchema {
            kind: "function",
            function: ZaiFunctionSchema {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            },
        })
        .collect()
}

#[async_trait]
impl Provider for ZaiProvider {
    fn id(&self) -> &str {
        "zai"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        let body = ZaiRequest {
            model: &self.model,
            messages: to_zai_messages(&req.messages),
            stream: true,
            max_tokens: req.max_output_tokens,
            temperature: req.temperature,
            tools: to_zai_tools(&req.tools),
        };

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(self.api_key.reveal())
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ProviderError::Auth("Z.ai rejected the API key".into()));
        }
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited {
                message: "Z.ai rate limit".into(),
                retry_after_ms: None,
            });
        }
        // 5xx (incl. 529 overloaded) is a transient server-side failure — map
        // to a retryable Transport error, not the terminal bucket below.
        if response.status().is_server_error() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Transport(format!(
                "Z.ai HTTP {status}: {text}"
            )));
        }
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Terminal(format!(
                "Z.ai HTTP {status}: {text}"
            )));
        }

        let (text, tool_calls, usage) = aggregate_zai_stream(response).await?;
        let cost_usd = self.pricing.map(|p| p.cost_usd(&usage)).unwrap_or(0.0);
        Ok(CompletionResult {
            text,
            tool_calls,
            usage,
            model: self.model.clone(),
            cost_usd,
        })
    }
}

/// Accumulator for one in-progress streamed tool call, keyed by the
/// provider's `index` field until it's complete.
#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

async fn aggregate_zai_stream(
    response: reqwest::Response,
) -> Result<(String, Vec<ToolCall>, CompletionUsage), ProviderError> {
    let mut decoder = SseDecoder::new();
    let mut text = String::new();
    let mut usage = CompletionUsage::default();
    let mut tool_calls: BTreeMap<usize, ToolCallAccumulator> = BTreeMap::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = http::next_with_timeout(&mut stream, http::STREAM_IDLE_TIMEOUT).await? {
        decoder
            .push_bytes(&chunk)
            .map_err(|e| ProviderError::Malformed(e.to_string()))?;
        for event in decoder.poll() {
            let data = event.data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let parsed: ZaiStreamChunk = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue, // tolerate keep-alive/ping frames
            };
            // A mid-stream error frame aborts the turn with a typed error —
            // never a truncated Ok with the partial text seen so far.
            if let Some(err) = &parsed.error {
                return Err(classify_zai_stream_error(err));
            }
            if let Some(u) = parsed.usage {
                usage.input_tokens = u.prompt_tokens;
                usage.output_tokens = u.completion_tokens;
            }
            for choice in parsed.choices {
                if let Some(content) = choice.delta.content {
                    text.push_str(&content);
                }
                for tc_delta in choice.delta.tool_calls {
                    let acc = tool_calls.entry(tc_delta.index).or_default();
                    if let Some(id) = tc_delta.id {
                        acc.id = id;
                    }
                    if let Some(function) = tc_delta.function {
                        if let Some(name) = function.name {
                            acc.name.push_str(&name);
                        }
                        if let Some(args) = function.arguments {
                            acc.arguments.push_str(&args);
                        }
                    }
                }
            }
        }
    }

    let calls = tool_calls
        .into_values()
        .map(|acc| {
            let input = serde_json::from_str(&acc.arguments).unwrap_or(Value::Null);
            ToolCall {
                call_id: acc.id,
                name: acc.name,
                input,
            }
        })
        .collect();

    Ok((text, calls, usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::tool::ToolSchema;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn to_zai_messages_maps_all_roles() {
        let messages = vec![
            CompletionMessage::system("sys"),
            CompletionMessage::user("hi"),
        ];
        let mapped = to_zai_messages(&messages);
        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[0].role, "system");
        assert_eq!(mapped[1].role, "user");
    }

    #[test]
    fn to_zai_messages_frames_tool_results_with_call_ids() {
        use stella_protocol::{ToolOutput, ToolResult};
        let messages = vec![
            CompletionMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_calls: vec![stella_protocol::ToolCall {
                    call_id: "call_9".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                }],
                tool_results: vec![],
            },
            CompletionMessage {
                role: MessageRole::Tool,
                content: String::new(),
                tool_calls: vec![],
                tool_results: vec![ToolResult {
                    call_id: "call_9".into(),
                    output: ToolOutput::Ok {
                        content: "fn main(){}".into(),
                    },
                }],
            },
        ];
        let mapped = to_zai_messages(&messages);
        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[0].role, "assistant");
        assert_eq!(mapped[0].tool_calls.len(), 1);
        assert_eq!(mapped[1].role, "tool");
        assert_eq!(mapped[1].tool_call_id.as_deref(), Some("call_9"));
        assert_eq!(mapped[1].content, "fn main(){}");
    }

    #[test]
    fn to_zai_messages_marks_error_results_loudly() {
        use stella_protocol::{ToolOutput, ToolResult};
        let messages = vec![CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: "call_1".into(),
                output: ToolOutput::Error {
                    message: "no such file".into(),
                },
            }],
        }];
        let mapped = to_zai_messages(&messages);
        assert_eq!(mapped.len(), 1);
        assert!(mapped[0].content.starts_with("ERROR:"));
    }

    #[tokio::test]
    async fn complete_aggregates_text_deltas_from_a_mock_server() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo!\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("say hello")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let result = provider
            .complete(req)
            .await
            .expect("completion should succeed");
        assert_eq!(result.text, "Hello!");
        assert_eq!(result.usage.input_tokens, 8);
        assert_eq!(result.usage.output_tokens, 3);
    }

    #[tokio::test]
    async fn complete_reassembles_a_streamed_tool_call_split_across_many_chunks() {
        let server = MockServer::start().await;
        // GLM 5.2 streams tool_calls as index-keyed fragments; arguments
        // arrive as partial JSON string pieces across several chunks —
        // exactly the dialect quirk this test proves the adapter handles.
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"file\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"src/lib.rs\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("read src/lib.rs")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![ToolSchema {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: false,
            }],
        };

        let result = provider
            .complete(req)
            .await
            .expect("completion should succeed");
        assert_eq!(result.tool_calls.len(), 1);
        let call = &result.tool_calls[0];
        assert_eq!(call.call_id, "call_1");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.input, serde_json::json!({"path": "src/lib.rs"}));
    }

    #[tokio::test]
    async fn complete_maps_429_to_rate_limited_and_it_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        assert!(matches!(err, ProviderError::RateLimited { .. }));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn complete_computes_nonzero_cost_from_catalog_pricing() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":500}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let result = provider.complete(req).await.expect("should succeed");
        // Budget metering is no longer a no-op: cost is derived from the
        // catalog's glm-5.2 pricing and the streamed usage, not hard-coded 0.
        let expected = Catalog::seed()
            .resolve("glm-5.2")
            .unwrap()
            .pricing
            .cost_usd(&CompletionUsage {
                input_tokens: 1000,
                output_tokens: 500,
                cached_input_tokens: 0,
            });
        assert!(result.cost_usd > 0.0, "cost must be non-zero");
        assert_eq!(result.cost_usd, expected);
    }

    #[tokio::test]
    async fn complete_maps_5xx_to_retryable_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("service unavailable"))
            .mount(&server)
            .await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        assert!(matches!(err, ProviderError::Transport(_)));
        assert!(err.is_retryable(), "5xx must be retryable");
    }

    #[tokio::test]
    async fn complete_returns_err_on_mid_stream_error_frame_not_truncated_ok() {
        let server = MockServer::start().await;
        // Some text arrives, THEN an in-band error frame: the turn must fail
        // rather than return the partial "Hel" as a success.
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"error\":{\"type\":\"server_error\",\"message\":\"upstream overloaded\"}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        // server_error / overloaded ⇒ retryable Transport.
        assert!(matches!(err, ProviderError::Transport(_)));
        assert!(err.is_retryable());
    }
}
