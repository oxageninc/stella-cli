//! OpenAI adapter — the Responses API (`POST /responses`), not the
//! Chat Completions API. This is the third of the three provider dialects
//! Phase 2's exit criterion requires (`03-plan.md` step 1,
//! `07-model-matrix.md` §2: "Responses API (streaming, tool-use, reasoning
//! effort)"). Before this file existed, `stella-cli` routed `OPENAI_API_KEY`
//! through `zai::ZaiProvider` pointed at a different base URL — that works
//! only because `/v1/chat/completions` happens to also exist on OpenAI's
//! account, but it is not the wire shape the spec calls for and it is not a
//! structurally distinct dialect from Z.ai's, which defeated the point of
//! having three dialects in the exit criterion. The Responses API is
//! genuinely different: an `input` *items* array instead of a flat
//! `messages` array, an `output` items array instead of `choices`, and
//! `function_call`/`function_call_output` items instead of an accumulating
//! `tool_calls` delta array — see the wire types below.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stella_protocol::{
    CompletionMessage, CompletionRequest, CompletionResult, CompletionUsage, MessageRole,
    ProviderError, ReasoningEffort, ToolCall,
};

use crate::catalog::{Catalog, Pricing};
use crate::credential::ApiKey;
use crate::http;
use crate::provider::Provider;
use crate::sse::SseDecoder;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: ApiKey,
    base_url: String,
    model: String,
    /// List pricing for `model`, resolved from the catalog at construction so
    /// `cost_usd` is computed on the real request path (see `zai.rs`).
    pricing: Option<Pricing>,
}

impl OpenAiProvider {
    /// Build an adapter for `model` (a catalog-resolved slug, e.g.
    /// `gpt-5.5` — never a literal chosen at the call site).
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

// ── Wire types (OpenAI Responses API) ────────────────────────────────────

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    input: Vec<OpenAiInputItem>,
    /// The Responses API's dedicated system/developer-prompt field. We pick
    /// this over framing the system prompt as an `input` item with
    /// `role: "system"` — both are accepted, but `instructions` is the
    /// documented, stable mechanism specifically for "the model's
    /// persistent behavior" and keeps the system prompt out of the item
    /// array we're otherwise using purely for conversation turns and tool
    /// I/O, which is easier to reason about when building `input` from our
    /// one internal message list.
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAiToolSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenAiReasoning>,
}

#[derive(Serialize)]
struct OpenAiReasoning {
    effort: &'static str,
}

/// The Responses API's function-tool shape: flat (`name`/`description`/
/// `parameters` at the top level), unlike Chat Completions' nested
/// `{"type":"function","function":{...}}` wrapper that `zai.rs` speaks.
#[derive(Serialize)]
struct OpenAiToolSchema {
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
    description: String,
    parameters: Value,
}

/// One item in the Responses API's `input` array. This replaces the flat
/// `messages` array every other adapter here uses — text turns are
/// `message` items, an assistant's tool call is its own `function_call`
/// item, and a tool result is its own `function_call_output` item
/// correlated back by `call_id` (`07-model-matrix.md` §4).
#[derive(Serialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiInputItem {
    Message {
        role: &'static str,
        content: Vec<OpenAiContentPart>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Serialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiContentPart {
    InputText { text: String },
    OutputText { text: String },
}

/// Streamed SSE payloads from the Responses API. Unlike Chat Completions'
/// single chunk shape, this dialect sends many named event *types*
/// (`response.created`, `response.output_item.added`,
/// `response.output_text.delta`, `response.function_call_arguments.delta`,
/// `response.completed`, …). We model only what we aggregate and tolerate
/// everything else via `#[serde(other)]`, matching `zai.rs`'s "tolerate
/// keep-alive/ping frames" posture — a new event type OpenAI adds later
/// must never turn into a hard failure of the turn.
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum OpenAiStreamEvent {
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        #[serde(default)]
        output_index: usize,
        item: OpenAiOutputItem,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        #[serde(default)]
        output_index: usize,
        delta: String,
    },
    #[serde(rename = "response.completed")]
    Completed { response: OpenAiResponseObject },
    /// The response terminated in failure. The `response.error` object
    /// carries the code/message — modeled explicitly so it aborts the turn
    /// instead of falling into `Other` and returning truncated text as a
    /// bogus success.
    #[serde(rename = "response.failed")]
    Failed { response: OpenAiResponseObject },
    /// The response stopped before completing (e.g. `max_output_tokens`,
    /// `content_filter`). Returning the partial text as success would be a
    /// silent truncation, so this is surfaced as a terminal error.
    #[serde(rename = "response.incomplete")]
    Incomplete { response: OpenAiResponseObject },
    /// A top-level stream error frame (`event: error`), distinct from a
    /// `response.failed` wrapper.
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        message: Option<String>,
    },
    #[serde(other)]
    Other,
}

/// The item announced by `response.output_item.added`. We only need to act
/// on `function_call` items (to learn the `call_id`/`name` before argument
/// deltas start arriving); `message` items and anything else are ignored —
/// their text arrives via `response.output_text.delta` regardless of which
/// item it belongs to, which is all Phase 2's single-turn aggregation needs.
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiOutputItem {
    FunctionCall {
        call_id: String,
        #[serde(default)]
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug, Default)]
struct OpenAiResponseObject {
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    /// Present on `response.failed`.
    #[serde(default)]
    error: Option<OpenAiResponseError>,
    /// Present on `response.incomplete`.
    #[serde(default)]
    incomplete_details: Option<OpenAiIncompleteDetails>,
}

#[derive(Deserialize, Debug, Default)]
struct OpenAiResponseError {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct OpenAiIncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

/// Classify an OpenAI Responses-API error (from a `response.failed` error
/// object or a top-level `error` frame) into a typed `ProviderError`.
/// Server-side/overload/timeout conditions are **retryable** `Transport`; an
/// explicit rate limit is `RateLimited`; everything else is `Terminal`.
fn classify_openai_stream_error(code: Option<&str>, message: &str) -> ProviderError {
    let haystack = format!("{} {}", code.unwrap_or(""), message).to_lowercase();
    let detail = match code {
        Some(c) if !c.is_empty() && !message.is_empty() => {
            format!("OpenAI stream error [{c}]: {message}")
        }
        Some(c) if !c.is_empty() => format!("OpenAI stream error [{c}]"),
        _ if !message.is_empty() => format!("OpenAI stream error: {message}"),
        _ => "OpenAI stream error".to_string(),
    };
    if haystack.contains("server_error")
        || haystack.contains("overloaded")
        || haystack.contains("unavailable")
        || haystack.contains("timeout")
    {
        ProviderError::Transport(detail)
    } else if haystack.contains("rate_limit")
        || (haystack.contains("rate") && haystack.contains("limit"))
    {
        ProviderError::RateLimited {
            message: detail,
            retry_after_ms: None,
        }
    } else {
        ProviderError::Terminal(detail)
    }
}

#[derive(Deserialize, Debug, Default)]
struct OpenAiUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<OpenAiInputTokensDetails>,
}

#[derive(Deserialize, Debug, Default)]
struct OpenAiInputTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

/// Map the engine's one `ReasoningEffort` enum to the Responses API's
/// `reasoning.effort` parameter, which only documents `low`/`medium`/`high`.
/// `Xhigh` and `Max` have no native slot — rather than silently dropping the
/// hint or panicking on an enum variant OpenAI's API doesn't model, both
/// collapse to `"high"`, the closest tier the API actually accepts.
fn map_reasoning_effort(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High | ReasoningEffort::Xhigh | ReasoningEffort::Max => "high",
    }
}

fn to_openai_input(messages: &[CompletionMessage]) -> (Option<String>, Vec<OpenAiInputItem>) {
    let mut instructions: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            MessageRole::System => instructions.push(message.content.clone()),
            MessageRole::User => out.push(OpenAiInputItem::Message {
                role: "user",
                content: vec![OpenAiContentPart::InputText {
                    text: message.content.clone(),
                }],
            }),
            MessageRole::Assistant => {
                if !message.content.is_empty() {
                    out.push(OpenAiInputItem::Message {
                        role: "assistant",
                        content: vec![OpenAiContentPart::OutputText {
                            text: message.content.clone(),
                        }],
                    });
                }
                for call in &message.tool_calls {
                    out.push(OpenAiInputItem::FunctionCall {
                        call_id: call.call_id.clone(),
                        name: call.name.clone(),
                        arguments: call.input.to_string(),
                    });
                }
            }
            // Responses API dialect: each tool result is its own
            // `function_call_output` item, correlated back to the call
            // solely by `call_id` — there is no wrapping "tool message".
            MessageRole::Tool => {
                for result in &message.tool_results {
                    let output = match &result.output {
                        stella_protocol::ToolOutput::Ok { content } => content.clone(),
                        stella_protocol::ToolOutput::Error { message } => {
                            format!("ERROR: {message}")
                        }
                    };
                    out.push(OpenAiInputItem::FunctionCallOutput {
                        call_id: result.call_id.clone(),
                        output,
                    });
                }
            }
        }
    }
    let instructions = if instructions.is_empty() {
        None
    } else {
        Some(instructions.join("\n\n"))
    };
    (instructions, out)
}

fn to_openai_tools(tools: &[stella_protocol::tool::ToolSchema]) -> Vec<OpenAiToolSchema> {
    tools
        .iter()
        .map(|tool| OpenAiToolSchema {
            kind: "function",
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
        })
        .collect()
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        let (instructions, input) = to_openai_input(&req.messages);
        let body = OpenAiRequest {
            model: &self.model,
            input,
            instructions: instructions.as_deref(),
            stream: true,
            max_output_tokens: req.max_output_tokens,
            temperature: req.temperature,
            tools: to_openai_tools(&req.tools),
            reasoning: req.effort.map(|effort| OpenAiReasoning {
                effort: map_reasoning_effort(effort),
            }),
        };

        let response = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(self.api_key.reveal())
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after_ms = http::parse_retry_after_ms(response.headers());
            let body = response.text().await.unwrap_or_default();
            return Err(http::classify_http_status(
                "OpenAI",
                status,
                retry_after_ms,
                &body,
            ));
        }

        let (text, tool_calls, usage) = aggregate_openai_stream(response).await?;
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

/// Accumulator for one in-progress `function_call` item, keyed by the
/// stream's `output_index` until it completes.
#[derive(Default)]
struct ToolCallAccumulator {
    call_id: String,
    name: String,
    arguments: String,
}

async fn aggregate_openai_stream(
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
            if data.is_empty() {
                continue;
            }
            let parsed: OpenAiStreamEvent = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue, // tolerate event shapes we don't model
            };
            match parsed {
                OpenAiStreamEvent::OutputItemAdded {
                    output_index,
                    item: OpenAiOutputItem::FunctionCall { call_id, name },
                } => {
                    let acc = tool_calls.entry(output_index).or_default();
                    acc.call_id = call_id;
                    acc.name = name;
                }
                OpenAiStreamEvent::OutputItemAdded { .. } => {}
                OpenAiStreamEvent::OutputTextDelta { delta } => text.push_str(&delta),
                OpenAiStreamEvent::FunctionCallArgumentsDelta {
                    output_index,
                    delta,
                } => {
                    tool_calls
                        .entry(output_index)
                        .or_default()
                        .arguments
                        .push_str(&delta);
                }
                OpenAiStreamEvent::Completed { response } => {
                    if let Some(u) = response.usage {
                        usage.input_tokens = u.input_tokens;
                        usage.output_tokens = u.output_tokens;
                        usage.cached_input_tokens =
                            u.input_tokens_details.map(|d| d.cached_tokens).unwrap_or(0);
                    }
                }
                // A mid-stream failure/incompletion/error aborts the turn with
                // a typed error — never a truncated Ok with the text so far.
                OpenAiStreamEvent::Failed { response } => {
                    let (code, message) = response
                        .error
                        .map(|e| (e.code, e.message.unwrap_or_default()))
                        .unwrap_or((None, String::new()));
                    return Err(classify_openai_stream_error(code.as_deref(), &message));
                }
                OpenAiStreamEvent::Incomplete { response } => {
                    let reason = response
                        .incomplete_details
                        .and_then(|d| d.reason)
                        .unwrap_or_else(|| "unspecified".to_string());
                    return Err(ProviderError::Terminal(format!(
                        "OpenAI response incomplete: {reason}"
                    )));
                }
                OpenAiStreamEvent::Error { code, message } => {
                    return Err(classify_openai_stream_error(
                        code.as_deref(),
                        message.as_deref().unwrap_or_default(),
                    ));
                }
                OpenAiStreamEvent::Other => {}
            }
        }
    }

    let tool_calls = tool_calls
        .into_values()
        .map(|acc| {
            let input = if acc.arguments.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&acc.arguments).unwrap_or(Value::Null)
            };
            ToolCall {
                call_id: acc.call_id,
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
    use stella_protocol::tool::ToolSchema;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn to_openai_input_hoists_system_into_instructions_and_maps_user() {
        let messages = vec![
            CompletionMessage::system("You are a coding agent."),
            CompletionMessage::user("Fix the bug."),
        ];
        let (instructions, mapped) = to_openai_input(&messages);
        assert_eq!(instructions, Some("You are a coding agent.".to_string()));
        assert_eq!(mapped.len(), 1);
        match &mapped[0] {
            OpenAiInputItem::Message { role, .. } => assert_eq!(*role, "user"),
            other => panic!("expected a message item, got {other:?}"),
        }
    }

    #[test]
    fn to_openai_input_frames_assistant_tool_calls_and_results_by_call_id() {
        use stella_protocol::{ToolOutput, ToolResult};
        let messages = vec![
            CompletionMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
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
        let (_, mapped) = to_openai_input(&messages);
        assert_eq!(mapped.len(), 2);
        match &mapped[0] {
            OpenAiInputItem::FunctionCall { call_id, name, .. } => {
                assert_eq!(call_id, "call_9");
                assert_eq!(name, "read_file");
            }
            other => panic!("expected a function_call item, got {other:?}"),
        }
        match &mapped[1] {
            OpenAiInputItem::FunctionCallOutput { call_id, output } => {
                assert_eq!(call_id, "call_9");
                assert_eq!(output, "fn main(){}");
            }
            other => panic!("expected a function_call_output item, got {other:?}"),
        }
    }

    #[test]
    fn to_openai_input_marks_error_results_loudly() {
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
        let (_, mapped) = to_openai_input(&messages);
        assert_eq!(mapped.len(), 1);
        match &mapped[0] {
            OpenAiInputItem::FunctionCallOutput { output, .. } => {
                assert!(output.starts_with("ERROR:"))
            }
            other => panic!("expected a function_call_output item, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_effort_maps_low_directly_and_unsupported_tiers_to_high() {
        assert_eq!(map_reasoning_effort(ReasoningEffort::Low), "low");
        assert_eq!(map_reasoning_effort(ReasoningEffort::Medium), "medium");
        assert_eq!(map_reasoning_effort(ReasoningEffort::High), "high");
        assert_eq!(map_reasoning_effort(ReasoningEffort::Xhigh), "high");
        assert_eq!(map_reasoning_effort(ReasoningEffort::Max), "high");
    }

    #[tokio::test]
    async fn complete_streams_and_aggregates_text_deltas_from_a_mock_server() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo!\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":4}}}}\n\n",
        );

        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", "Bearer sk-test-openai"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = OpenAiProvider::new(ApiKey::new("sk-test-openai"), "gpt-5.5")
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
        assert_eq!(result.usage.cached_input_tokens, 4);
        assert_eq!(result.model, "gpt-5.5");
    }

    #[tokio::test]
    async fn complete_reassembles_a_streamed_tool_call_split_across_many_chunks() {
        let server = MockServer::start().await;
        // The Responses API announces the function_call item once (with its
        // call_id and name) via `response.output_item.added`, then streams
        // `arguments` as string fragments across several
        // `response.function_call_arguments.delta` events keyed by
        // `output_index` — the exact dialect quirk this test proves the
        // adapter handles, mirroring `zai.rs`'s equivalent test for the
        // OpenAI-compatible dialect's own (structurally different) fragment
        // shape.
        let sse_body = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"path\\\":\"}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\\\"src/lib.rs\\\"}\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":40,\"output_tokens\":15}}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(ApiKey::new("sk-test"), "gpt-5.5").with_base_url(server.uri());

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

        let result = provider.complete(req).await.expect("should succeed");
        assert_eq!(result.tool_calls.len(), 1);
        let call = &result.tool_calls[0];
        assert_eq!(call.call_id, "call_1");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.input, serde_json::json!({"path": "src/lib.rs"}));
        assert_eq!(result.usage.input_tokens, 40);
        assert_eq!(result.usage.output_tokens, 15);
    }

    #[tokio::test]
    async fn complete_falls_back_to_null_when_streamed_arguments_never_parse() {
        let server = MockServer::start().await;
        // Arguments arrive but never form valid JSON (e.g. a dropped
        // fragment) — the adapter must fall back to `Value::Null`, the exact
        // sentinel `stella-core`'s `driver.rs::execute_with_repair` checks
        // for, rather than executing the tool with garbage input.
        let sse_body = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"bash\",\"arguments\":\"\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{not valid json\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(ApiKey::new("sk-test"), "gpt-5.5").with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("run ls")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![ToolSchema {
                name: "bash".into(),
                description: "Run a command".into(),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: false,
            }],
        };

        let result = provider.complete(req).await.expect("should succeed");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].input, Value::Null);
    }

    #[tokio::test]
    async fn complete_maps_401_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(ApiKey::new("bad-key"), "gpt-5.5").with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
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
        // A permission-denied key is a credential failure, not a generic
        // terminal error. Regression for the drift where only 401 was mapped
        // to Auth here while sibling adapters mapped 401|403.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(ApiKey::new("limited-key"), "gpt-5.5").with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
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
    async fn complete_maps_429_to_rate_limited_with_retry_after_and_it_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "2")
                    .set_body_string("rate limited"),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(ApiKey::new("sk-test"), "gpt-5.5").with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        assert!(err.is_retryable());
        match err {
            ProviderError::RateLimited { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(2000));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn complete_computes_nonzero_cost_from_catalog_pricing() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":500,\"input_tokens_details\":{\"cached_tokens\":200}}}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(ApiKey::new("sk-test"), "gpt-5.5").with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let result = provider.complete(req).await.expect("should succeed");
        // Cached input is billed at its own rate — assert against the catalog
        // computation so the wiring (and the cached-token split) is proven.
        let expected = Catalog::seed()
            .resolve("gpt-5.5")
            .unwrap()
            .pricing
            .cost_usd(&CompletionUsage {
                input_tokens: 1000,
                output_tokens: 500,
                cached_input_tokens: 200,
            });
        assert!(result.cost_usd > 0.0, "cost must be non-zero");
        assert_eq!(result.cost_usd, expected);
    }

    #[tokio::test]
    async fn complete_maps_5xx_to_retryable_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(ApiKey::new("sk-test"), "gpt-5.5").with_base_url(server.uri());
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
    async fn complete_returns_err_on_response_failed_not_truncated_ok() {
        let server = MockServer::start().await;
        // Text arrives, then `response.failed`: the turn must error, not
        // return the partial "Hel".
        let sse_body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"upstream failure\"}}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(ApiKey::new("sk-test"), "gpt-5.5").with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        // server_error ⇒ retryable Transport.
        assert!(matches!(err, ProviderError::Transport(_)));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn complete_returns_err_on_response_incomplete_not_truncated_ok() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "event: response.incomplete\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::new(ApiKey::new("sk-test"), "gpt-5.5").with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        match err {
            ProviderError::Terminal(msg) => assert!(msg.contains("max_output_tokens"), "{msg}"),
            other => panic!("expected Terminal incomplete error, got {other:?}"),
        }
    }
}
