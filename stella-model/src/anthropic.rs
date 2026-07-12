//! Anthropic adapter — Messages API, SSE streaming, native tool-use. One of
//! the two Phase 0 spikes (`03-plan.md` step 3): retires raw-SSE-parsing
//! risk against a second, structurally different dialect from Z.ai's
//! OpenAI-compatible one (`anthropic-tools` vs. `openai-json`,
//! `07-model-matrix.md` §4).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use stella_protocol::{
    CompletionMessage, CompletionRequest, CompletionResult, CompletionUsage, MessageRole,
    ProviderError, ToolCall,
};

use crate::catalog::{Catalog, Pricing};
use crate::credential::ApiKey;
use crate::http;
use crate::provider::Provider;
use crate::sse::SseDecoder;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: ApiKey,
    base_url: String,
    model: String,
    /// List pricing for `model`, resolved from the catalog at construction so
    /// `cost_usd` is computed on the real request path (see `zai.rs`).
    pricing: Option<Pricing>,
}

impl AnthropicProvider {
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

    /// Override the base URL — used by conformance tests against a mock
    /// server, and by anyone routing through a private proxy.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

// ── Wire types (Anthropic Messages API) ─────────────────────────────────

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: Option<&'a str>,
    messages: Vec<AnthropicMessage>,
    stream: bool,
    /// Sampling temperature, forwarded from `CompletionRequest.temperature`.
    /// Omitted when `None` so Anthropic applies its own default — dropping it
    /// unconditionally (the prior bug) meant a caller-set temperature was
    /// silently ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicToolSchema>,
}

#[derive(Serialize)]
struct AnthropicToolSchema {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: &'static str,
    content: Vec<AnthropicContentBlock>,
}

#[derive(Serialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

/// Streamed SSE payloads from the Messages API's `content_block_delta`
/// events. Anthropic's stream sends several event *types*
/// (`message_start`, `content_block_start`, `content_block_delta`,
/// `message_delta`, `message_stop`); Phase 0 only needs to aggregate text
/// deltas and the final usage block.
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicMessageStart },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        #[serde(default)]
        index: usize,
        content_block: AnthropicStartBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        #[serde(default)]
        index: usize,
        delta: AnthropicDelta,
    },
    #[serde(rename = "message_delta")]
    MessageDelta { usage: Option<AnthropicUsage> },
    /// A mid-stream error event. The Messages API can send
    /// `event: error` / `data: {"type":"error","error":{...}}` after already
    /// streaming content — modeled explicitly so it aborts the turn with a
    /// typed error instead of falling into `Other` and being swallowed,
    /// returning truncated text as a bogus success.
    #[serde(rename = "error")]
    Error { error: AnthropicStreamError },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug, Default)]
struct AnthropicStreamError {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
}

/// Map an Anthropic in-stream error to a typed `ProviderError`. Anthropic
/// documents `overloaded_error` and `api_error` as transient server-side
/// conditions (retryable `Transport`); `rate_limit_error` is `RateLimited`;
/// everything else (`invalid_request_error`, `authentication_error`,
/// `permission_error`, `not_found_error`, …) is `Terminal`.
fn classify_anthropic_stream_error(err: &AnthropicStreamError) -> ProviderError {
    let detail = if err.message.is_empty() {
        format!("Anthropic stream error ({})", err.kind)
    } else {
        format!("Anthropic stream error: {}", err.message)
    };
    match err.kind.as_str() {
        "overloaded_error" | "api_error" | "timeout_error" => ProviderError::Transport(detail),
        "rate_limit_error" => ProviderError::RateLimited {
            message: detail,
            retry_after_ms: None,
        },
        _ => ProviderError::Terminal(detail),
    }
}

#[derive(Deserialize, Debug, Default)]
struct AnthropicMessageStart {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicStartBlock {
    ToolUse {
        id: String,
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    /// Tokens served from the prompt cache. Anthropic reports these
    /// *separately* from `input_tokens` (they are NOT already folded in, as
    /// they are for OpenAI), so the adapter must add them back to keep the
    /// normalized `cached_input_tokens` a subset of `input_tokens` and bill
    /// them at the cheaper cache rate rather than dropping them.
    #[serde(default)]
    cache_read_input_tokens: u64,
}

fn to_anthropic_messages(
    messages: &[CompletionMessage],
) -> (Option<String>, Vec<AnthropicMessage>) {
    let mut system = None;
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            MessageRole::System => {
                system = Some(message.content.clone());
            }
            MessageRole::User => out.push(AnthropicMessage {
                role: "user",
                content: vec![AnthropicContentBlock::Text {
                    text: message.content.clone(),
                }],
            }),
            MessageRole::Assistant => {
                let mut content = Vec::new();
                if !message.content.is_empty() {
                    content.push(AnthropicContentBlock::Text {
                        text: message.content.clone(),
                    });
                }
                for call in &message.tool_calls {
                    content.push(AnthropicContentBlock::ToolUse {
                        id: call.call_id.clone(),
                        name: call.name.clone(),
                        input: call.input.clone(),
                    });
                }
                if content.is_empty() {
                    content.push(AnthropicContentBlock::Text {
                        text: String::new(),
                    });
                }
                out.push(AnthropicMessage {
                    role: "assistant",
                    content,
                });
            }
            // Anthropic dialect: tool results are content blocks inside a
            // `user` message, each keyed by `tool_use_id`.
            MessageRole::Tool => {
                let content: Vec<AnthropicContentBlock> = message
                    .tool_results
                    .iter()
                    .map(|result| {
                        let (text, is_error) = match &result.output {
                            stella_protocol::ToolOutput::Ok { content } => (content.clone(), false),
                            stella_protocol::ToolOutput::Error { message } => {
                                (message.clone(), true)
                            }
                        };
                        AnthropicContentBlock::ToolResult {
                            tool_use_id: result.call_id.clone(),
                            content: text,
                            is_error,
                        }
                    })
                    .collect();
                if !content.is_empty() {
                    out.push(AnthropicMessage {
                        role: "user",
                        content,
                    });
                }
            }
        }
    }
    (system, out)
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        let (system, messages) = to_anthropic_messages(&req.messages);
        let body = AnthropicRequest {
            model: &self.model,
            max_tokens: req.max_output_tokens.unwrap_or(4096),
            system: system.as_deref(),
            messages,
            stream: true,
            temperature: req.temperature,
            tools: req
                .tools
                .iter()
                .map(|tool| AnthropicToolSchema {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
                .collect(),
        };

        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", self.api_key.reveal())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ProviderError::Auth("Anthropic rejected the API key".into()));
        }
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited {
                message: "Anthropic rate limit".into(),
                retry_after_ms: None,
            });
        }
        // 5xx (incl. 529 overloaded, which Anthropic uses for load shedding)
        // is transient — map to a retryable Transport error.
        if response.status().is_server_error() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Transport(format!(
                "Anthropic HTTP {status}: {text}"
            )));
        }
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Terminal(format!(
                "Anthropic HTTP {status}: {text}"
            )));
        }

        let (text, tool_calls, usage) = aggregate_anthropic_stream(response).await?;
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

/// Accumulator for one in-progress `tool_use` block, keyed by its stream
/// index until the block completes.
#[derive(Default)]
struct ToolUseAccumulator {
    id: String,
    name: String,
    input_json: String,
}

async fn aggregate_anthropic_stream(
    response: reqwest::Response,
) -> Result<(String, Vec<ToolCall>, CompletionUsage), ProviderError> {
    use std::collections::BTreeMap;

    let mut decoder = SseDecoder::new();
    let mut text = String::new();
    let mut usage = CompletionUsage::default();
    let mut tool_uses: BTreeMap<usize, ToolUseAccumulator> = BTreeMap::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = http::next_with_timeout(&mut stream, http::STREAM_IDLE_TIMEOUT).await? {
        decoder
            .push_bytes(&chunk)
            .map_err(|e| ProviderError::Malformed(e.to_string()))?;
        for event in decoder.poll() {
            if event.data.trim() == "[DONE]" || event.data.is_empty() {
                continue;
            }
            let parsed: Result<AnthropicStreamEvent, _> = serde_json::from_str(&event.data);
            match parsed {
                Ok(AnthropicStreamEvent::Error { error }) => {
                    // A mid-stream error aborts the turn with a typed error —
                    // never a truncated Ok with the text seen so far.
                    return Err(classify_anthropic_stream_error(&error));
                }
                Ok(AnthropicStreamEvent::MessageStart { message }) => {
                    if let Some(u) = message.usage {
                        usage.input_tokens = u.input_tokens + u.cache_read_input_tokens;
                        usage.cached_input_tokens = u.cache_read_input_tokens;
                    }
                }
                Ok(AnthropicStreamEvent::ContentBlockStart {
                    index,
                    content_block: AnthropicStartBlock::ToolUse { id, name },
                }) => {
                    tool_uses.insert(
                        index,
                        ToolUseAccumulator {
                            id,
                            name,
                            input_json: String::new(),
                        },
                    );
                }
                Ok(AnthropicStreamEvent::ContentBlockDelta { index, delta }) => match delta {
                    AnthropicDelta::TextDelta { text: delta } => text.push_str(&delta),
                    AnthropicDelta::InputJsonDelta { partial_json } => {
                        if let Some(acc) = tool_uses.get_mut(&index) {
                            acc.input_json.push_str(&partial_json);
                        }
                    }
                    AnthropicDelta::Other => {}
                },
                Ok(AnthropicStreamEvent::MessageDelta { usage: Some(u) }) => {
                    if u.input_tokens > 0 {
                        usage.input_tokens = u.input_tokens + u.cache_read_input_tokens;
                    }
                    if u.cache_read_input_tokens > 0 {
                        usage.cached_input_tokens = u.cache_read_input_tokens;
                    }
                    usage.output_tokens = u.output_tokens;
                }
                Ok(_) => {}
                Err(_) => {
                    // Unrecognized event shape (e.g. ping/ack events with no
                    // `type` we model) — tolerated, never fatal to the turn.
                }
            }
        }
    }

    let tool_calls = tool_uses
        .into_values()
        .map(|acc| {
            let input = if acc.input_json.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&acc.input_json).unwrap_or(serde_json::Value::Null)
            };
            ToolCall {
                call_id: acc.id,
                name: acc.name,
                input,
            }
        })
        .collect();

    Ok((text, tool_calls, usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::MessageRole;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn to_anthropic_messages_hoists_system_and_maps_roles() {
        let messages = vec![
            CompletionMessage::system("You are a coding agent."),
            CompletionMessage::user("Fix the bug."),
        ];
        let (system, mapped) = to_anthropic_messages(&messages);
        assert_eq!(system, Some("You are a coding agent.".to_string()));
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].role, "user");
    }

    #[tokio::test]
    async fn complete_streams_and_aggregates_text_deltas_from_a_mock_server() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"lo!\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":12,\"output_tokens\":2}}\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-test-anthropic"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test-anthropic"), "claude-fable-5")
            .with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![
                CompletionMessage::system("system"),
                CompletionMessage::user("say hello"),
            ],
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
        assert_eq!(result.usage.input_tokens, 12);
        assert_eq!(result.usage.output_tokens, 2);
        assert_eq!(result.model, "claude-fable-5");
    }

    #[tokio::test]
    async fn complete_reassembles_a_streamed_tool_use_block() {
        let server = MockServer::start().await;
        // Anthropic streams tool input as `input_json_delta` fragments after
        // a `content_block_start` announces the tool_use block.
        let sse_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":40,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"src/lib.rs\\\"}\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":0,\"output_tokens\":15}}\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("read src/lib.rs")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![stella_protocol::tool::ToolSchema {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: false,
            }],
        };

        let result = provider.complete(req).await.expect("should succeed");
        assert_eq!(result.tool_calls.len(), 1);
        let call = &result.tool_calls[0];
        assert_eq!(call.call_id, "toolu_1");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.input, serde_json::json!({"path": "src/lib.rs"}));
        assert_eq!(result.usage.input_tokens, 40);
        assert_eq!(result.usage.output_tokens, 15);
    }

    #[test]
    fn to_anthropic_messages_frames_tool_results_as_user_blocks() {
        use stella_protocol::{ToolCall, ToolOutput, ToolResult};
        let messages = vec![
            CompletionMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    call_id: "toolu_9".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                }],
                tool_results: vec![],
            },
            CompletionMessage {
                role: MessageRole::Tool,
                content: String::new(),
                tool_calls: vec![],
                tool_results: vec![ToolResult {
                    call_id: "toolu_9".into(),
                    output: ToolOutput::Error {
                        message: "command failed".into(),
                    },
                }],
            },
        ];
        let (_, mapped) = to_anthropic_messages(&messages);
        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[0].role, "assistant");
        assert_eq!(mapped[1].role, "user");
        // The tool result block must carry the id and the error flag.
        match &mapped[1].content[0] {
            AnthropicContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_9");
                assert!(is_error);
            }
            other => panic!("expected tool_result block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn complete_maps_401_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("bad-key"), "claude-fable-5")
            .with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage {
                role: MessageRole::User,
                content: "hi".into(),
                tool_calls: vec![],
                tool_results: vec![],
            }],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        assert!(matches!(err, ProviderError::Auth(_)));
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn complete_computes_nonzero_cost_from_catalog_pricing() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":0,\"output_tokens\":500}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let result = provider.complete(req).await.expect("should succeed");
        let expected = Catalog::seed()
            .resolve("claude-fable-5")
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
    async fn complete_forwards_temperature_to_the_request_body() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        );
        // The mock only matches if the serialized body carries the
        // temperature — proving the adapter no longer drops it.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_string_contains("\"temperature\":0.3"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: Some(0.3),
            effort: None,
            tools: vec![],
        };

        let result = provider.complete(req).await.expect("should succeed");
        assert_eq!(result.text, "ok");
    }

    #[tokio::test]
    async fn complete_maps_5xx_to_retryable_transport() {
        let server = MockServer::start().await;
        // 529 is Anthropic's "overloaded" load-shedding status.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        assert!(matches!(err, ProviderError::Transport(_)));
        assert!(err.is_retryable(), "5xx/529 must be retryable");
    }

    #[tokio::test]
    async fn complete_returns_err_on_mid_stream_error_event_not_truncated_ok() {
        let server = MockServer::start().await;
        // Text arrives, then an `error` event: the turn must fail, not return
        // the partial "Hel".
        let sse_body = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"server overloaded\"}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        // overloaded_error ⇒ retryable Transport.
        assert!(matches!(err, ProviderError::Transport(_)));
        assert!(err.is_retryable());
    }
}
