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
    MessageDelta {
        /// Carries `stop_reason` — `"max_tokens"` when the model was cut off
        /// at the output-token limit. Tracked so a tool call whose argument
        /// JSON was truncated mid-stream surfaces an actionable error instead
        /// of a silent `Null` (see [`crate::http::truncated_tool_input_error`]).
        #[serde(default)]
        delta: AnthropicMessageDeltaBody,
        usage: Option<AnthropicUsage>,
    },
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

/// The `delta` object of a `message_delta` event. Only `stop_reason` is
/// modeled — it is how the Messages API signals *why* generation ended, and
/// `"max_tokens"` specifically means the output was cut off at the token
/// limit (potentially mid-tool-call).
#[derive(Deserialize, Debug, Default)]
struct AnthropicMessageDeltaBody {
    #[serde(default)]
    stop_reason: Option<String>,
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

        if !response.status().is_success() {
            let status = response.status();
            let retry_after_ms = http::parse_retry_after_ms(response.headers());
            let body = response.text().await.unwrap_or_default();
            return Err(http::classify_http_status(
                "Anthropic",
                status,
                retry_after_ms,
                &body,
            ));
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
    // Why generation ended, from the `message_delta` event. `"max_tokens"`
    // means the stream was cut off at the output-token limit — the signal a
    // truncated tool-call payload needs to be reported as such rather than
    // silently nulled.
    let mut stop_reason: Option<String> = None;
    // The highest content-block index that started. Blocks stream
    // sequentially, so only this block can have been cut off by the token
    // limit — a later block starting proves every earlier one closed.
    let mut last_block_index: Option<usize> = None;
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
                    content_block,
                }) => {
                    last_block_index = Some(index);
                    if let AnthropicStartBlock::ToolUse { id, name } = content_block {
                        tool_uses.insert(
                            index,
                            ToolUseAccumulator {
                                id,
                                name,
                                input_json: String::new(),
                            },
                        );
                    }
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
                Ok(AnthropicStreamEvent::MessageDelta { delta, usage: u }) => {
                    if let Some(reason) = delta.stop_reason {
                        stop_reason = Some(reason);
                    }
                    if let Some(u) = u {
                        if u.input_tokens > 0 {
                            usage.input_tokens = u.input_tokens + u.cache_read_input_tokens;
                        }
                        if u.cache_read_input_tokens > 0 {
                            usage.cached_input_tokens = u.cache_read_input_tokens;
                        }
                        usage.output_tokens = u.output_tokens;
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    // Unrecognized event shape (e.g. ping/ack events with no
                    // `type` we model) — tolerated, never fatal to the turn.
                }
            }
        }
    }

    // The one content block the token limit could have cut: the last block
    // started, and only when the stream actually stopped at `max_tokens`.
    // Pinning truncation to that block keeps the blame on the call that was
    // cut — an *earlier* call whose JSON is broken is the model's own
    // malformed output and still gets the repair sentinel below.
    let truncated_index = if stop_reason.as_deref() == Some("max_tokens") {
        last_block_index
    } else {
        None
    };

    let mut tool_calls = Vec::with_capacity(tool_uses.len());
    for (index, acc) in tool_uses {
        let truncated = Some(index) == truncated_index;
        let input = if acc.input_json.is_empty() {
            if truncated {
                // The limit landed after this call's `content_block_start`
                // but before its first `input_json_delta`: executing it with
                // `{}` would fail on missing parameters and re-enter the same
                // unwinnable retry-retruncate loop as a mid-payload cut.
                return Err(http::truncated_tool_input_error(
                    "Anthropic",
                    &acc.name,
                    "",
                    "stop_reason=max_tokens",
                ));
            }
            // A no-argument tool call arrives with no `input_json_delta` at
            // all: that is an empty object, never null.
            serde_json::json!({})
        } else {
            match serde_json::from_str(&acc.input_json) {
                Ok(value) => value,
                // The fragments were concatenated byte-exactly (the SSE
                // decoder's own tests prove arbitrary chunk boundaries
                // reassemble losslessly), so an unparseable buffer on the
                // block the token limit cut means the arguments never
                // finished streaming. Terminal and turn-aborting — mirroring
                // openai.rs's `response.incomplete` handling — because
                // retrying the identical request re-truncates identically:
                // the old silent `Null` here sent the driver's repair loop
                // into exactly that "stuck-loop".
                Err(_) if truncated => {
                    return Err(http::truncated_tool_input_error(
                        "Anthropic",
                        &acc.name,
                        &acc.input_json,
                        "stop_reason=max_tokens",
                    ));
                }
                // Broken JSON on a block that *finished* is the model's own
                // malformed output: fall back to the `Value::Null` sentinel
                // `driver.rs::execute_with_repair` consumes (the documented
                // adapter contract), so the repair loop asks the model to
                // re-emit just this call instead of aborting the turn.
                Err(_) => serde_json::Value::Null,
            }
        };
        tool_calls.push(ToolCall {
            call_id: acc.id,
            name: acc.name,
            input,
        });
    }

    Ok((text, tool_calls, usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::MessageRole;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The reported failure's happy path: a multi-kilobyte `write_file` tool
    /// call whose argument JSON is split across HUNDREDS of `input_json_delta`
    /// fragments (and thus many SSE events / network chunks) must reassemble
    /// to the exact JSON object — never a truncated buffer or `null`. Proves
    /// the assembly itself is sound for large payloads; the null only ever
    /// came from genuine truncation, handled by the test below.
    #[tokio::test]
    async fn complete_reassembles_a_large_multi_fragment_tool_call() {
        let server = MockServer::start().await;
        // A realistic large `write_file` content field: several KB, with the
        // newlines, quotes and backslashes a real README carries.
        let mut content = String::new();
        for i in 0..200 {
            content.push_str(&format!(
                "## Section {i}\n\nSome \"quoted\" text with a backslash \\ and a tab\there.\n\n"
            ));
        }
        let full_input = serde_json::json!({"path": "README.md", "content": content});
        let full_input_json = serde_json::to_string(&full_input).unwrap();

        // Fragment the JSON at arbitrary byte-ish boundaries (chars), exactly
        // as Anthropic streams `partial_json` — including splits mid-escape.
        let chars: Vec<char> = full_input_json.chars().collect();
        let mut body = String::from(
            "event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":40,\"output_tokens\":0}}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"write_file\"}}\n\n",
        );
        for piece in chars.chunks(29) {
            let frag: String = piece.iter().collect();
            let escaped = serde_json::to_string(&frag).unwrap();
            body.push_str("event: content_block_delta\n");
            body.push_str(&format!(
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{escaped}}}}}\n\n"
            ));
        }
        body.push_str(
            "event: message_delta\n\
             data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":0,\"output_tokens\":15}}\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("write the readme")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![stella_protocol::tool::ToolSchema {
                name: "write_file".into(),
                description: "Write a file".into(),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: false,
            }],
        };

        let result = provider.complete(req).await.expect("should succeed");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(
            result.tool_calls[0].input, full_input,
            "large multi-fragment tool input must reassemble to the exact JSON, not null"
        );
    }

    /// The reported bug's failure path: the model starts a large `write_file`
    /// tool call but the stream stops at the output-token limit MID-JSON —
    /// `partial_json` is an unterminated fragment and `message_delta` carries
    /// `stop_reason: "max_tokens"`. The adapter must NOT silently emit a
    /// tool call with `null` input (which the driver's repair loop can never
    /// fix, producing the observed "stuck-loop"); it must surface a clear,
    /// non-retryable Terminal error naming the truncation and carrying a raw
    /// snippet of what was accumulated.
    #[tokio::test]
    async fn complete_surfaces_a_truncated_tool_call_instead_of_silent_null() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"write_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"README.md\\\",\\\"content\\\":\\\"# Title\\\\nlots of\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":8192}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("write the readme")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![stella_protocol::tool::ToolSchema {
                name: "write_file".into(),
                description: "Write a file".into(),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: false,
            }],
        };

        let err = provider.complete(req).await.unwrap_err();
        // A truncated-at-max_tokens tool call is a terminal, non-retryable
        // failure — retrying with the same output budget re-truncates.
        assert!(
            matches!(err, ProviderError::Terminal(_)),
            "expected Terminal, got {err:?}"
        );
        assert!(!err.is_retryable());
        let msg = err.to_string();
        assert!(msg.contains("write_file"), "names the tool: {msg}");
        assert!(msg.contains("max_tokens"), "names the cause: {msg}");
        // The raw accumulated snippet must be surfaced, never dropped to null.
        assert!(msg.contains("README.md"), "carries a raw snippet: {msg}");
    }

    /// Broken JSON on a call that *finished* streaming (the stream did not
    /// stop at `max_tokens`) is the model's own malformed output — the
    /// adapter must keep the `Value::Null` repair sentinel the driver's
    /// documented repair loop consumes, not abort the turn.
    #[tokio::test]
    async fn malformed_but_complete_tool_json_falls_back_to_the_null_repair_sentinel() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"edit_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": not json,}\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":40}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        let result = provider
            .complete(CompletionRequest {
                messages: vec![CompletionMessage::user("edit the file")],
                max_output_tokens: None,
                temperature: None,
                effort: None,
                tools: vec![],
            })
            .await
            .expect("a repairable malformed call must not abort the turn");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].input, serde_json::Value::Null);
    }

    /// `max_tokens` landing between a tool call's `content_block_start` and
    /// its first `input_json_delta` must surface the same terminal truncation
    /// error as a mid-payload cut — never a silent `{}` that executes with
    /// missing parameters and re-enters the retry-retruncate loop.
    #[tokio::test]
    async fn a_tool_call_with_no_arguments_cut_at_max_tokens_is_terminal() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"write_file\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":8192}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        let err = provider
            .complete(CompletionRequest {
                messages: vec![CompletionMessage::user("write the readme")],
                max_output_tokens: None,
                temperature: None,
                effort: None,
                tools: vec![],
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Terminal(_)),
            "expected Terminal, got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("write_file"), "names the tool: {msg}");
        assert!(msg.contains("max_tokens"), "names the cause: {msg}");
    }

    /// With parallel tool calls, the truncation error must blame the call the
    /// token limit actually cut (the last block started) — an earlier call
    /// whose complete-but-broken JSON is the model's own output must not be
    /// misreported as truncated.
    #[tokio::test]
    async fn truncation_is_blamed_on_the_call_that_was_cut_not_an_earlier_broken_one() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"edit_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": broken,}\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"write_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"README\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":8192}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        let err = provider
            .complete(CompletionRequest {
                messages: vec![CompletionMessage::user("do both")],
                max_output_tokens: None,
                temperature: None,
                effort: None,
                tools: vec![],
            })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("write_file"),
            "blames the truncated call: {msg}"
        );
        assert!(
            !msg.contains("edit_file"),
            "must not blame the earlier repairable call: {msg}"
        );
    }

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
    async fn complete_maps_403_to_auth_error() {
        // A permission-denied key (403 permission_error — e.g. a key without
        // access to the requested model) is a credential failure the user
        // must fix, not a generic terminal error. Regression for the drift
        // where only 401 was mapped to Auth here while sibling adapters
        // mapped 401|403.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(403).set_body_string("permission_error"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("limited-key"), "claude-fable-5")
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
    async fn complete_honors_retry_after_header_on_a_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "3")
                    .set_body_string("rate limited"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("test-key"), "claude-fable-5")
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
        match err {
            ProviderError::RateLimited { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(3_000), "Retry-After: 3s → 3000ms");
            }
            other => panic!("expected RateLimited with a retry hint, got {other:?}"),
        }
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
