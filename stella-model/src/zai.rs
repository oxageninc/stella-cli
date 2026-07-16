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
///
/// When the `ZAI_GLM_CODING_PLAN` environment variable is set to `1`, the
/// coding plan endpoint (`/api/coding/paas/v4`) is used instead of the
/// standard endpoint (`/api/paas/v4`).
const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

/// GLM Coding Plan endpoint. Activated when `ZAI_GLM_CODING_PLAN=1` is set.
const CODING_PLAN_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

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
    id: String,
    label: String,
}

impl ZaiProvider {
    pub fn new(api_key: ApiKey, model: impl Into<String>) -> Self {
        let model = model.into();
        let pricing = Catalog::seed().resolve(&model).ok().map(|e| e.pricing);
        // Use the coding plan endpoint when ZAI_GLM_CODING_PLAN=1 is set
        let base_url = if std::env::var("ZAI_GLM_CODING_PLAN").as_deref() == Ok("1") {
            CODING_PLAN_BASE_URL
        } else {
            DEFAULT_BASE_URL
        };
        Self {
            client: http::client(),
            api_key,
            base_url: base_url.to_string(),
            model,
            pricing,
            id: "zai".to_string(),
            label: "Z.ai".to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Re-identify this adapter for another OpenAI-*compatible* provider it
    /// is serving (xAI, DeepSeek, OpenRouter, a local endpoint): `id` is
    /// what `Provider::id()` reports and `label` is the human name used in
    /// error messages. Without this, every gateway routed through the
    /// shared Chat Completions adapter misreported itself as Z.ai — an
    /// xAI 401 read "Z.ai rejected the API key", pointing the user at the
    /// wrong credential.
    pub fn with_identity(mut self, id: impl Into<String>, label: impl Into<String>) -> Self {
        self.id = id.into();
        self.label = label.into();
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
/// a genuine failure is never retried into an infinite loop. `label` names
/// the concrete provider (Z.ai / xAI / DeepSeek / …) so the surfaced message
/// points at the right credential and endpoint.
fn classify_zai_stream_error(err: &ZaiStreamError, label: &str) -> ProviderError {
    let haystack = format!("{} {}", err.kind, err.message).to_lowercase();
    let detail = if err.message.is_empty() {
        format!("{label} stream error ({})", err.kind)
    } else {
        format!("{label} stream error: {}", err.message)
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

/// The `error` object Z.ai returns in a *non-streamed* HTTP error body:
/// `{"error":{"code":"1113","message":"Insufficient balance ..."}}`. Distinct
/// from [`ZaiStreamError`] (the in-band SSE frame) — this is the top-level
/// envelope on a plain JSON 4xx response.
#[derive(Deserialize, Debug)]
struct ZaiErrorEnvelope {
    error: ZaiErrorBody,
}

#[derive(Deserialize, Debug, Default)]
struct ZaiErrorBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    #[allow(dead_code)]
    code: Option<Value>,
}

/// Classify an HTTP 429 by its body. Z.ai returns 429 both for real rate
/// limiting AND for account-balance/quota exhaustion (code 1113,
/// "Insufficient balance or no resource package. Please recharge") — the
/// latter is **terminal**: no amount of backoff refills an empty account, so
/// retrying it just delays a clear error and, worse, reports it as a rate
/// limit. Billing/quota text ⇒ [`ProviderError::Terminal`]; anything else ⇒
/// retryable [`ProviderError::RateLimited`], honoring a `Retry-After` hint
/// when the server sent one. The provider's own message is always carried
/// through so the real reason is visible instead of a hard-coded string.
fn classify_zai_429(body: &str, retry_after_ms: Option<u64>) -> ProviderError {
    let detail = serde_json::from_str::<ZaiErrorEnvelope>(body)
        .ok()
        .map(|e| e.error.message)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| body.trim().to_string());
    let haystack = detail.to_lowercase();
    let message = if detail.is_empty() {
        "Z.ai HTTP 429".to_string()
    } else {
        format!("Z.ai HTTP 429: {detail}")
    };
    // Balance/quota exhaustion — never self-clears, so terminal not retryable.
    if haystack.contains("balance")
        || haystack.contains("recharge")
        || haystack.contains("resource package")
        || haystack.contains("insufficient")
        || haystack.contains("quota")
        || haystack.contains("arrears")
    {
        ProviderError::Terminal(message)
    } else {
        ProviderError::RateLimited {
            message,
            retry_after_ms,
        }
    }
}

#[derive(Deserialize, Debug)]
struct ZaiStreamChoice {
    #[serde(default)]
    delta: ZaiStreamDelta,
    /// Why generation ended for this choice. `"length"` is the OpenAI-
    /// compatible signal that the output was cut off at `max_tokens` — the
    /// truncation marker a partially-streamed tool call needs so it can be
    /// reported rather than silently nulled.
    #[serde(default)]
    finish_reason: Option<String>,
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
        &self.id
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

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Vendor pre-check ahead of the shared ladder — Z.ai overloads
            // HTTP 429: besides genuine throttling it also returns 429 for
            // BILLING problems — an account with no credit answers with
            // `{"error":{"code":"1113","message":"Insufficient balance or no
            // resource package. Please recharge."}}`. Blindly mapping every
            // 429 to a retryable `RateLimited` both mislabels that as a rate
            // limit (the user sees "provider rate limited" on their very
            // first call, which can't be a real throttle) AND burns three
            // pointless retries on a condition backoff will never clear.
            // Read the body and classify: a real throttle stays retryable, a
            // billing/quota failure is terminal, and either way Z.ai's own
            // message is surfaced instead of a hard-coded string.
            let retry_after_ms = http::parse_retry_after_ms(response.headers());
            let body = response.text().await.unwrap_or_default();
            return Err(classify_zai_429(&body, retry_after_ms));
        }
        if !response.status().is_success() {
            let status = response.status();
            let retry_after_ms = http::parse_retry_after_ms(response.headers());
            let body = response.text().await.unwrap_or_default();
            return Err(http::classify_http_status(
                &self.label,
                status,
                retry_after_ms,
                &body,
            ));
        }

        let (text, tool_calls, usage) = aggregate_zai_stream(response, &self.label).await?;
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
    label: &str,
) -> Result<(String, Vec<ToolCall>, CompletionUsage), ProviderError> {
    let mut decoder = SseDecoder::new();
    let mut text = String::new();
    let mut usage = CompletionUsage::default();
    let mut tool_calls: BTreeMap<usize, ToolCallAccumulator> = BTreeMap::new();
    // Set once any choice reports `finish_reason: "length"` — the output was
    // cut off at the token limit, so a tool call whose argument JSON didn't
    // finish streaming is truncated, not merely malformed.
    let mut truncated_at_token_limit = false;
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
                return Err(classify_zai_stream_error(err, label));
            }
            if let Some(u) = parsed.usage {
                usage.input_tokens = u.prompt_tokens;
                usage.output_tokens = u.completion_tokens;
            }
            for choice in parsed.choices {
                if choice.finish_reason.as_deref() == Some("length") {
                    truncated_at_token_limit = true;
                }
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

    // OpenAI-style tool calls stream sequentially by index, so when the
    // stream reports `finish_reason: "length"` only the highest-index call
    // can be the one the token limit cut. Pinning truncation there keeps the
    // blame on the right call — an earlier call whose JSON is broken is the
    // model's own malformed output and still gets the repair sentinel below.
    let truncated_index = if truncated_at_token_limit {
        tool_calls.keys().next_back().copied()
    } else {
        None
    };

    let mut calls = Vec::with_capacity(tool_calls.len());
    for (index, acc) in tool_calls {
        let truncated = Some(index) == truncated_index;
        let trimmed = acc.arguments.trim();
        let input = if trimmed.is_empty() {
            if truncated {
                // The limit landed after the call's id/name but before any
                // argument fragment: executing it with `{}` would fail on
                // missing parameters and re-enter the same unwinnable
                // retry-retruncate loop as a mid-payload cut.
                return Err(http::truncated_tool_input_error(
                    label,
                    &acc.name,
                    "",
                    "finish_reason=length",
                ));
            }
            // A no-argument tool call arrives as `arguments: ""`; that is an
            // empty object, not null — a downstream tool deserializing its
            // input as an object must not be handed `null`.
            Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_str(trimmed) {
                Ok(value) => value,
                // The stream stopped at the token limit MID-arguments: the
                // JSON is truncated, not the model's own broken syntax.
                // Terminal and turn-aborting — mirroring openai.rs's
                // `response.incomplete` handling — because retrying the
                // identical request re-truncates identically (the reported
                // "stuck-loop" defect).
                Err(_) if truncated => {
                    return Err(http::truncated_tool_input_error(
                        label,
                        &acc.name,
                        trimmed,
                        "finish_reason=length",
                    ));
                }
                // A *non-empty* body that fails to parse without being the
                // truncated call is the model's own broken JSON (GLM emits
                // these): fall back to the `Value::Null` sentinel
                // `driver.rs::execute_with_repair` checks for, so the repair
                // loop — documented as tuned to GLM's failure shapes — can
                // ask the model to retry. Mirrors anthropic.rs.
                Err(_) => Value::Null,
            }
        };
        calls.push(ToolCall {
            call_id: acc.id,
            name: acc.name,
            input,
        });
    }

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
    async fn complete_falls_back_to_null_when_streamed_tool_arguments_never_parse() {
        let server = MockServer::start().await;
        // GLM streams a tool call whose argument fragments never form valid
        // JSON. The adapter must fall back to `Value::Null` — the exact
        // sentinel `driver.rs::execute_with_repair` checks for — so the repair
        // loop (tuned to GLM's failure shapes) can ask the model to retry,
        // rather than hard-erroring the whole turn before any `ToolCall` is
        // produced.
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_7\",\"function\":{\"name\":\"bash\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{not valid json\"}}]}}]}\n\n",
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

        let result = provider
            .complete(req)
            .await
            .expect("malformed args must not error the turn — they become the repair sentinel");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "bash");
        assert_eq!(result.tool_calls[0].input, Value::Null);
    }

    #[tokio::test]
    async fn complete_surfaces_a_tool_call_truncated_at_the_token_limit_not_null() {
        let server = MockServer::start().await;
        // GLM streams a large tool call, but the output is cut off at the token
        // limit MID-arguments: the final chunk carries `finish_reason: "length"`
        // and the accumulated `arguments` is an unterminated JSON fragment.
        // Unlike the malformed-but-complete case (which becomes the repair
        // sentinel), a truncation is a terminal, actionable failure that must
        // surface with a raw snippet — never a silent null the repair loop
        // spins on forever (the reported "stuck-loop").
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write_file\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"README.md\\\",\\\"content\\\":\\\"# Title\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
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
            messages: vec![CompletionMessage::user("write the readme")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![ToolSchema {
                name: "write_file".into(),
                description: "Write a file".into(),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: false,
            }],
        };

        let err = provider.complete(req).await.unwrap_err();
        assert!(
            matches!(err, ProviderError::Terminal(_)),
            "a truncated tool call must be terminal, got {err:?}"
        );
        assert!(!err.is_retryable(), "re-issuing the request re-truncates");
        let msg = err.to_string();
        assert!(msg.contains("write_file"), "names the tool: {msg}");
        assert!(
            msg.contains("finish_reason=length"),
            "names the cause: {msg}"
        );
        assert!(msg.contains("README.md"), "carries a raw snippet: {msg}");
    }

    /// With parallel tool calls and a `finish_reason: "length"` cut, the
    /// truncation error must blame the call that was actually cut (the
    /// highest index — the one still streaming) — an earlier call whose
    /// complete-but-broken JSON is GLM's own output must not be misreported
    /// as truncated.
    #[tokio::test]
    async fn truncation_is_blamed_on_the_last_call_not_an_earlier_broken_one() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{not valid json\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"README\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
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
            !msg.contains("bash"),
            "must not blame the earlier repairable call: {msg}"
        );
    }

    /// `finish_reason: "length"` landing after a call's id/name but before
    /// any argument fragment must surface the terminal truncation error —
    /// never a silent `{}` that executes with missing parameters.
    #[tokio::test]
    async fn a_tool_call_with_no_arguments_cut_at_the_token_limit_is_terminal() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write_file\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
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
    }

    #[tokio::test]
    async fn complete_maps_a_real_throttle_429_to_retryable_rate_limited() {
        let server = MockServer::start().await;
        // A genuine throttle body (no billing/quota keywords) stays a
        // retryable RateLimited — the normal, correct 429 path.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429).set_body_string(
                    r#"{"error":{"code":"1302","message":"API rate limit reached"}}"#,
                ),
            )
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
    async fn complete_maps_insufficient_balance_429_to_terminal_not_rate_limited() {
        let server = MockServer::start().await;
        // The exact body a live, empty-balance Z.ai account returns for its
        // very first request. HTTP 429, but NOT a rate limit — a billing
        // failure that no retry can clear. It must classify as terminal, be
        // NON-retryable, and surface Z.ai's own message rather than a
        // hard-coded "rate limit" string.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string(
                r#"{"error":{"code":"1113","message":"Insufficient balance or no resource package. Please recharge."}}"#,
            ))
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
        assert!(
            matches!(err, ProviderError::Terminal(_)),
            "insufficient-balance 429 must be terminal, got {err:?}"
        );
        assert!(
            !err.is_retryable(),
            "a billing failure must never be retried"
        );
        assert!(
            err.to_string().contains("Insufficient balance"),
            "the real Z.ai message must be surfaced, got: {err}"
        );
    }

    #[tokio::test]
    async fn complete_honors_retry_after_header_on_a_throttle_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "2")
                    .set_body_string("too many requests"),
            )
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
        match err {
            ProviderError::RateLimited { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(2_000), "Retry-After: 2s → 2000ms");
            }
            other => panic!("expected RateLimited with a retry hint, got {other:?}"),
        }
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
                cache_write_tokens: 0,
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
