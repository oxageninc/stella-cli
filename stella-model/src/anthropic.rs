//! Anthropic adapter — Messages API, SSE streaming, native tool-use.
//! Retires raw-SSE-parsing risk against a second, structurally different
//! dialect from Z.ai's OpenAI-compatible one (`anthropic-tools` vs.
//! `openai-json`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use stella_protocol::{
    CompletionMessage, CompletionRequest, CompletionResult, CompletionUsage, FinishReason,
    MessageRole, ProviderError, ToolCall,
};

use crate::catalog::{Catalog, Pricing};
use crate::credential::ApiKey;
use crate::http;
use crate::provider::{Provider, ToolCallObserver};
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
        let catalog = Catalog::current();
        let pricing = catalog.resolve(&model).ok().map(|e| e.pricing);
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
    /// System prompt as a content-block array rather than a bare string, so
    /// the block can carry the `cache_control` breakpoint that caches the
    /// tools+system prefix tier (prompt caching is opt-in per request on the
    /// Messages API).
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<AnthropicSystemBlock<'a>>>,
    messages: Vec<AnthropicMessage>,
    stream: bool,
    /// Sampling temperature, forwarded from `CompletionRequest.temperature`.
    /// Omitted when `None` so Anthropic applies its own default — dropping it
    /// unconditionally (the prior bug) meant a caller-set temperature was
    /// silently ignored. Also omitted whenever `thinking` is set: the
    /// Messages API rejects any temperature != 1 alongside extended thinking,
    /// and the engine's default (0.0) would fail every thinking turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// Sampling overrides from `CompletionRequest.params`, skipped when
    /// `None` so a request without overrides serializes byte-identical to
    /// before (the prompt-cache contract). Only the subset the Messages API
    /// speaks — the rest of `GenerationParams` has no slot here and is
    /// silently dropped per the never-fail contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    /// Extended thinking (`{"type":"enabled","budget_tokens":N}`), set only
    /// for `CompletionRequest.reasoning == Some(true)`. `Some(false)` and
    /// `None` both omit the block — thinking is opt-in per request on this
    /// API, so "off" and "provider default" are the same wire shape (which
    /// keeps the pre-field bytes stable).
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicToolSchema>,
}

/// The Messages API's extended-thinking switch. `budget_tokens` is a hard
/// cap on thinking output and must satisfy `1024 <= budget < max_tokens` —
/// [`thinking_budget_tokens`] maps the engine's effort tiers and the caller
/// in `complete_inner` clamps against the request's actual `max_tokens`.
#[derive(Serialize)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u32,
}

/// Map the engine's effort tiers onto thinking budgets. Anthropic has no
/// named levels — the budget IS the level — so the tiers are spaced roughly
/// geometrically from the API's 1024-token floor up to a Max that still
/// leaves headroom under typical output caps. `None` defaults to Medium, the
/// same middle-tier default posture as `openai.rs` ("effort":"medium").
fn thinking_budget_tokens(effort: Option<stella_protocol::ReasoningEffort>) -> u32 {
    use stella_protocol::ReasoningEffort::*;
    match effort {
        Some(Low) => 2_048,
        None | Some(Medium) => 8_192,
        Some(High) => 16_384,
        Some(Xhigh) => 32_768,
        Some(Max) => 49_152,
    }
}

/// The opt-in prompt-cache marker (`{"type": "ephemeral"}`, default 5-minute
/// TTL). The pipeline keeps the system prefix byte-stable and rides volatile
/// recall after it (L-E8) precisely so these breakpoints hit; this is the
/// wire half that actually turns the cache on. Reads bill at ~0.1x the input
/// rate, writes at ~1.25x — break-even after two requests, and the agent
/// loop replays its prefix every turn.
#[derive(Serialize, Clone, Copy, Debug)]
struct AnthropicCacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

const EPHEMERAL_CACHE: AnthropicCacheControl = AnthropicCacheControl { kind: "ephemeral" };

/// Stamp the conversation-tail cache breakpoint: `cache_control` on the
/// LAST content block of the final message, so each agent-loop turn reads
/// the prefix written by the previous turn instead of re-paying the whole
/// replayed history at the full input rate. Pairs with the system-block
/// marker (two of the four allowed breakpoints). Block-level is the only
/// placement the Messages API accepts — a top-level `cache_control`
/// request field is an unknown parameter the API rejects with a 400.
fn stamp_tail_cache_breakpoint(messages: &mut [AnthropicMessage]) {
    let Some(block) = messages.last_mut().and_then(|m| m.content.last_mut()) else {
        return;
    };
    match block {
        AnthropicContentBlock::Text { cache_control, .. }
        | AnthropicContentBlock::ToolResult { cache_control, .. } => {
            *cache_control = Some(EPHEMERAL_CACHE);
        }
        // A media or tool_use tail is not a request shape the loop produces
        // (a user message's text follows its attachments; requests end on a
        // user or tool_result turn) — the system-block breakpoint still
        // caches the tools+system tier.
        AnthropicContentBlock::Image { .. }
        | AnthropicContentBlock::Document { .. }
        | AnthropicContentBlock::ToolUse { .. } => {}
    }
}

#[derive(Serialize)]
struct AnthropicSystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
    cache_control: AnthropicCacheControl,
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
        /// Set only on the final block of the last message — the
        /// conversation-tail cache breakpoint ([`stamp_tail_cache_breakpoint`]).
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
    /// A user-attached image (`{"type":"image","source":{...}}`).
    Image { source: AnthropicMediaSource },
    /// A user-attached PDF (`{"type":"document","source":{...}}`).
    Document { source: AnthropicMediaSource },
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
        /// Same conversation-tail breakpoint slot as `Text::cache_control`.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<AnthropicCacheControl>,
    },
}

/// The base64 payload envelope shared by image and document blocks.
#[derive(Serialize, Debug)]
struct AnthropicMediaSource {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: String,
    data: String,
}

impl AnthropicMediaSource {
    fn base64(media_type: impl Into<String>, data: String) -> Self {
        Self {
            kind: "base64",
            media_type: media_type.into(),
            data,
        }
    }
}

/// The Messages API ingests images and PDFs natively; audio, video, and
/// arbitrary binaries degrade to descriptive text notes.
const ANTHROPIC_CAPS: crate::attachment::DialectCaps = crate::attachment::DialectCaps {
    images: true,
    pdfs: true,
    audio: false,
    video: false,
};

/// Map a user message's attachments to Anthropic content blocks. Media
/// blocks precede text (the documented preferred ordering for vision).
fn attachment_blocks(message: &CompletionMessage) -> Vec<AnthropicContentBlock> {
    crate::attachment::wire_parts(&message.attachments, ANTHROPIC_CAPS)
        .into_iter()
        .map(|part| match part {
            crate::attachment::WirePart::Image { media_type, base64 } => {
                AnthropicContentBlock::Image {
                    source: AnthropicMediaSource::base64(media_type, base64),
                }
            }
            crate::attachment::WirePart::Pdf { base64, .. } => AnthropicContentBlock::Document {
                source: AnthropicMediaSource::base64("application/pdf", base64),
            },
            crate::attachment::WirePart::Text { text } => AnthropicContentBlock::Text {
                text,
                cache_control: None,
            },
            // Audio/video are switched off in ANTHROPIC_CAPS, so wire_parts
            // has already degraded them to Text notes.
            crate::attachment::WirePart::Audio { .. }
            | crate::attachment::WirePart::Video { .. } => {
                unreachable!("caps exclude audio/video")
            }
        })
        .collect()
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
    /// A content block finished streaming. For a `tool_use` block this is
    /// the earliest moment its complete input is known — the hook that lets
    /// [`aggregate_anthropic_stream`] announce the call to a
    /// [`ToolCallObserver`] while the rest of the message still streams.
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        #[serde(default)]
        index: usize,
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
    /// Tokens WRITTEN to the prompt cache by this call — also reported
    /// separately from `input_tokens`. Surfaced as the normalized
    /// `cache_write_tokens` (telemetry, `stella stats`) but deliberately NOT
    /// folded into `input_tokens`: Anthropic bills cache writes at a premium
    /// rate the catalog does not carry yet, so folding them in would
    /// misprice them as plain input (see `Pricing::cost_usd`).
    #[serde(default)]
    cache_creation_input_tokens: u64,
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
            // The Anthropic API rejects a text content block whose text is
            // empty or whitespace-only with a 400 — and because the whole
            // conversation is replayed on every turn, one such block bricks
            // the session permanently (every retry re-sends it). So a text
            // block is emitted only when it carries non-whitespace content,
            // and a message that ends up with zero blocks is dropped rather
            // than padded with an empty block. Attachment blocks (images,
            // documents, inlined files) precede the typed text.
            MessageRole::User => {
                let mut content = attachment_blocks(message);
                if !message.content.trim().is_empty() {
                    content.push(AnthropicContentBlock::Text {
                        text: message.content.clone(),
                        cache_control: None,
                    });
                }
                if !content.is_empty() {
                    out.push(AnthropicMessage {
                        role: "user",
                        content,
                    });
                }
            }
            MessageRole::Assistant => {
                let mut content = Vec::new();
                if !message.content.trim().is_empty() {
                    content.push(AnthropicContentBlock::Text {
                        text: message.content.clone(),
                        cache_control: None,
                    });
                }
                for call in &message.tool_calls {
                    content.push(AnthropicContentBlock::ToolUse {
                        id: call.call_id.clone(),
                        name: call.name.clone(),
                        input: call.input.clone(),
                    });
                }
                // A content-less assistant turn (no text, no tool calls) is
                // dropped, not sent as an empty text block: it carries no
                // information and there is no tool_use to orphan.
                if !content.is_empty() {
                    out.push(AnthropicMessage {
                        role: "assistant",
                        content,
                    });
                }
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
                            cache_control: None,
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
        self.complete_inner(req, None).await
    }

    async fn complete_observed(
        &self,
        req: CompletionRequest,
        observer: &dyn ToolCallObserver,
    ) -> Result<CompletionResult, ProviderError> {
        self.complete_inner(req, Some(observer)).await
    }
}

impl AnthropicProvider {
    /// The one request/stream/aggregate body behind both `complete` and
    /// `complete_observed` — the observer is threaded down to the stream
    /// aggregator, which announces each tool call at its
    /// `content_block_stop`.
    async fn complete_inner(
        &self,
        req: CompletionRequest,
        observer: Option<&dyn ToolCallObserver>,
    ) -> Result<CompletionResult, ProviderError> {
        let (system, mut messages) = to_anthropic_messages(&req.messages);
        stamp_tail_cache_breakpoint(&mut messages);
        // Thinking budget, resolved before max_tokens because the two are
        // coupled: the API requires budget < max_tokens, and this adapter's
        // 4096-token max_tokens default would leave no room for any budget
        // above Low. So when thinking is on and the CALLER didn't set an
        // output cap, the default floor rises to budget + 8192 (thinking
        // spends from the same output allowance as the answer — a budget
        // that consumes the whole cap yields a truncated or empty turn).
        // A caller-set cap is honored as-is and the budget clamps to it:
        // at most max_tokens - 1024, never below the API's 1024 floor — and
        // a cap at or below the floor leaves NO legal budget (the API
        // requires 1024 <= budget < max_tokens), so thinking is omitted
        // entirely rather than sent as a request the API rejects with 400.
        let thinking_budget =
            (req.reasoning == Some(true)).then(|| thinking_budget_tokens(req.effort));
        let max_tokens = match (req.max_output_tokens, thinking_budget) {
            (Some(cap), _) => cap,
            (None, Some(budget)) => budget + 8_192,
            (None, None) => 4096,
        };
        let thinking = thinking_budget.and_then(|budget| {
            (max_tokens > 1024).then(|| AnthropicThinking {
                kind: "enabled",
                budget_tokens: budget.min(max_tokens - 1024).max(1024),
            })
        });
        let params = req.params.unwrap_or_default();
        let body = AnthropicRequest {
            model: &self.model,
            max_tokens,
            system: system.as_deref().map(|text| {
                vec![AnthropicSystemBlock {
                    kind: "text",
                    text,
                    cache_control: EPHEMERAL_CACHE,
                }]
            }),
            messages,
            stream: true,
            // The API rejects temperature != 1 with thinking enabled; rather
            // than special-casing 1.0, omit the field entirely and let the
            // API apply its own thinking-compatible default.
            temperature: if thinking.is_some() {
                None
            } else {
                req.temperature
            },
            top_p: params.top_p,
            top_k: params.top_k,
            thinking,
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

        let (text, tool_calls, usage, stop_reason) =
            aggregate_anthropic_stream(response, observer).await?;
        let cost_usd = self.pricing.map(|p| p.cost_usd(&usage)).unwrap_or(0.0);
        let finish_reason = map_stop_reason(stop_reason.as_deref());
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

/// Accumulator for one in-progress `tool_use` block, keyed by its stream
/// index until the block completes.
#[derive(Default)]
struct ToolUseAccumulator {
    id: String,
    name: String,
    input_json: String,
}

/// Normalize the Messages API's `stop_reason` vocabulary onto the
/// provider-neutral [`FinishReason`] — the driver's truncation diagnostics
/// (`driver.rs`) only fire when `Length` actually reaches it. Unknown or
/// unreported reasons stay `None` per the `CompletionResult` contract.
fn map_stop_reason(stop_reason: Option<&str>) -> Option<FinishReason> {
    match stop_reason? {
        "end_turn" | "stop_sequence" | "pause_turn" => Some(FinishReason::Stop),
        "max_tokens" => Some(FinishReason::Length),
        "tool_use" => Some(FinishReason::ToolCalls),
        "refusal" => Some(FinishReason::ContentFilter),
        _ => None,
    }
}

async fn aggregate_anthropic_stream(
    response: reqwest::Response,
    observer: Option<&dyn ToolCallObserver>,
) -> Result<(String, Vec<ToolCall>, CompletionUsage, Option<String>), ProviderError> {
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
                        usage.cache_write_tokens = u.cache_creation_input_tokens;
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
                Ok(AnthropicStreamEvent::ContentBlockStop { index }) => {
                    // The earliest moment a tool call is complete. Announce
                    // it to the observer ONLY when its input already parses —
                    // a block whose JSON is broken or truncated must go
                    // through the end-of-stream repair/truncation logic
                    // below, never reach speculative execution. The
                    // accumulator stays in the map: the final assembly below
                    // remains the single source of truth, and it re-parses
                    // the same bytes, so an announced call and its committed
                    // twin are structurally identical.
                    if let (Some(observer), Some(acc)) = (observer, tool_uses.get(&index)) {
                        let input = if acc.input_json.is_empty() {
                            Some(serde_json::json!({}))
                        } else {
                            serde_json::from_str(&acc.input_json).ok()
                        };
                        if let Some(input) = input
                            && !acc.id.is_empty()
                        {
                            observer.tool_call_streamed(&ToolCall {
                                call_id: acc.id.clone(),
                                name: acc.name.clone(),
                                input,
                            });
                        }
                    }
                }
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
                        if u.cache_creation_input_tokens > 0 {
                            usage.cache_write_tokens = u.cache_creation_input_tokens;
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

    Ok((text, tool_calls, usage, stop_reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::MessageRole;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn user_attachments_map_to_image_and_document_blocks_before_text() {
        use stella_protocol::{Attachment, AttachmentSource};
        let attachments = vec![
            Attachment {
                name: "shot.png".into(),
                media_type: "image/png".into(),
                byte_len: 3,
                source: AttachmentSource::Data {
                    base64: "aW1n".into(), // "img"
                },
            },
            Attachment {
                name: "spec.pdf".into(),
                media_type: "application/pdf".into(),
                byte_len: 3,
                source: AttachmentSource::Data {
                    base64: "cGRm".into(), // "pdf"
                },
            },
            Attachment {
                name: "clip.mp4".into(),
                media_type: "video/mp4".into(),
                byte_len: 3,
                source: AttachmentSource::Data {
                    base64: "dmlk".into(), // "vid"
                },
            },
        ];
        let messages = vec![CompletionMessage::user_with_attachments(
            "what do you see?",
            attachments,
        )];
        let (_, mapped) = to_anthropic_messages(&messages);
        assert_eq!(mapped.len(), 1);
        let json = serde_json::to_value(&mapped[0]).unwrap();
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 4, "{json}");
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["type"], "base64");
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[0]["source"]["data"], "aW1n");
        assert_eq!(blocks[1]["type"], "document");
        assert_eq!(blocks[1]["source"]["media_type"], "application/pdf");
        // Video is not natively ingestible on this dialect: degrade note.
        assert_eq!(blocks[2]["type"], "text");
        let note = blocks[2]["text"].as_str().unwrap();
        assert!(note.contains("clip.mp4"), "{note}");
        // The typed text comes after the media blocks.
        assert_eq!(blocks[3]["type"], "text");
        assert_eq!(blocks[3]["text"], "what do you see?");
    }

    #[test]
    fn attachment_only_user_message_survives_without_text() {
        use stella_protocol::{Attachment, AttachmentSource};
        let messages = vec![CompletionMessage::user_with_attachments(
            "",
            vec![Attachment {
                name: "shot.png".into(),
                media_type: "image/png".into(),
                byte_len: 3,
                source: AttachmentSource::Data {
                    base64: "aW1n".into(),
                },
            }],
        )];
        let (_, mapped) = to_anthropic_messages(&messages);
        assert_eq!(mapped.len(), 1, "attachment-only message must not drop");
        let json = serde_json::to_value(&mapped[0]).unwrap();
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "image");
    }

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
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
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
                reasoning: None,
                params: None,
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
                reasoning: None,
                params: None,
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
                reasoning: None,
                params: None,
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

    /// The Anthropic API rejects empty / whitespace-only text blocks with a
    /// 400, and since history is replayed every turn, one such block bricks
    /// the session forever. `to_anthropic_messages` must never emit one, and
    /// a message that would carry only such a block is dropped rather than
    /// padded.
    #[test]
    fn empty_and_whitespace_text_blocks_are_never_emitted() {
        use stella_protocol::ToolCall;
        let messages = vec![
            CompletionMessage::system("sys"),
            CompletionMessage::user("real question"),
            // Assistant turn with only whitespace text plus a tool call:
            // the whitespace text must be dropped, the tool_use kept.
            CompletionMessage {
                role: MessageRole::Assistant,
                content: "   \n\t ".into(),
                tool_calls: vec![ToolCall {
                    call_id: "toolu_1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "a"}),
                }],
                tool_results: vec![],
                attachments: Vec::new(),
            },
            // A fully content-less assistant turn — must vanish entirely,
            // NOT become an empty text block.
            CompletionMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_calls: vec![],
                tool_results: vec![],
                attachments: Vec::new(),
            },
            // An empty user turn — dropped, never an empty text block.
            CompletionMessage {
                role: MessageRole::User,
                content: "  ".into(),
                tool_calls: vec![],
                tool_results: vec![],
                attachments: Vec::new(),
            },
        ];
        let (_, mapped) = to_anthropic_messages(&messages);

        // Not one emitted text block is empty or whitespace-only.
        for m in &mapped {
            for block in &m.content {
                if let AnthropicContentBlock::Text { text, .. } = block {
                    assert!(
                        !text.trim().is_empty(),
                        "emitted an empty/whitespace text block: {text:?}"
                    );
                }
            }
        }
        // The whitespace-plus-tool assistant turn kept exactly its tool_use.
        let assistant = mapped
            .iter()
            .find(|m| m.role == "assistant")
            .expect("the tool-calling assistant turn survives");
        assert_eq!(assistant.content.len(), 1);
        assert!(matches!(
            assistant.content[0],
            AnthropicContentBlock::ToolUse { .. }
        ));
        // The content-less assistant turn and the empty user turn are gone.
        assert_eq!(
            mapped.iter().filter(|m| m.role == "assistant").count(),
            1,
            "the content-less assistant turn is dropped, not padded"
        );
        // Only the real user question survives among user-role messages.
        let user_texts: Vec<&AnthropicMessage> =
            mapped.iter().filter(|m| m.role == "user").collect();
        assert_eq!(user_texts.len(), 1);
    }

    /// Prompt caching is opt-in per request: the serialized body must carry
    /// both breakpoints — one on the system block (tools+system tier), one
    /// on the final content block of the last message (conversation tail) —
    /// or the API silently caches nothing and every turn re-pays the full
    /// replayed prefix. And it must carry them at BLOCK level only: a
    /// top-level `cache_control` request field is an unknown parameter the
    /// live API rejects with a 400, killing every Anthropic call.
    #[test]
    fn request_serializes_both_cache_breakpoints() {
        let mut messages = vec![
            AnthropicMessage {
                role: "user",
                content: vec![AnthropicContentBlock::Text {
                    text: "earlier turn".into(),
                    cache_control: None,
                }],
            },
            AnthropicMessage {
                role: "user",
                content: vec![AnthropicContentBlock::Text {
                    text: "hi".into(),
                    cache_control: None,
                }],
            },
        ];
        stamp_tail_cache_breakpoint(&mut messages);
        let body = AnthropicRequest {
            model: "claude-fable-5",
            max_tokens: 64,
            system: Some(vec![AnthropicSystemBlock {
                kind: "text",
                text: "You are a coding agent.",
                cache_control: EPHEMERAL_CACHE,
            }]),
            messages,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            thinking: None,
            tools: vec![],
        };
        let v = serde_json::to_value(&body).expect("request serializes");
        assert_eq!(v["system"][0]["type"], "text");
        assert_eq!(v["system"][0]["cache_control"]["type"], "ephemeral");
        // Tail breakpoint on the LAST block of the LAST message only.
        assert_eq!(
            v["messages"][1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(
            v["messages"][0]["content"][0]
                .get("cache_control")
                .is_none(),
            "only the tail block carries a breakpoint"
        );
        assert!(
            v.get("cache_control").is_none(),
            "top-level cache_control is not a Messages API parameter"
        );
        // The optional params/thinking fields must vanish when unset — the
        // byte-stability contract for requests without overrides.
        let body_json = serde_json::to_string(&v).unwrap();
        for key in ["top_p", "top_k", "thinking"] {
            assert!(!body_json.contains(key), "unexpected `{key}`: {body_json}");
        }
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
            reasoning: None,
            params: None,
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

    /// Cache accounting: `message_start` reports `cache_read_input_tokens`
    /// and `cache_creation_input_tokens` separately from `input_tokens`.
    /// Reads fold into `input_tokens` (subset invariant) and surface as
    /// `cached_input_tokens`; writes surface as `cache_write_tokens` WITHOUT
    /// folding in — the catalog has no cache-write rate, so pricing them as
    /// plain input would be wrong in the other direction (issue #97).
    #[tokio::test]
    async fn complete_reports_cache_write_tokens_from_the_usage_envelope() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":0,\"cache_read_input_tokens\":900,\"cache_creation_input_tokens\":650}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
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
            reasoning: None,
            params: None,
        };

        let result = provider.complete(req).await.expect("should succeed");
        // Reads folded in: 100 fresh + 900 read-from-cache.
        assert_eq!(result.usage.input_tokens, 1_000);
        assert_eq!(result.usage.cached_input_tokens, 900);
        // Writes surfaced, not folded: input_tokens stays 1_000.
        assert_eq!(result.usage.cache_write_tokens, 650);
        assert_eq!(result.usage.output_tokens, 7);
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
            reasoning: None,
            params: None,
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

    /// Shared with the zai tests in spirit: records every announcement so a
    /// test can compare them against the final `CompletionResult`.
    struct RecordingObserver(std::sync::Mutex<Vec<stella_protocol::ToolCall>>);

    impl ToolCallObserver for RecordingObserver {
        fn tool_call_streamed(&self, call: &stella_protocol::ToolCall) {
            self.0.lock().unwrap().push(call.clone());
        }
    }

    #[tokio::test]
    async fn complete_observed_announces_a_tool_call_at_its_block_stop() {
        let server = MockServer::start().await;
        // The tool_use block CLOSES (content_block_stop) while the message
        // continues with a text block — the exact window speculation exists
        // to exploit.
        let sse_body = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"reading it now\"}}\n\n",
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
            tools: vec![],
            reasoning: None,
            params: None,
        };

        let observer = RecordingObserver(std::sync::Mutex::new(Vec::new()));
        let result = provider
            .complete_observed(req, &observer)
            .await
            .expect("should succeed");

        let announced = observer.0.lock().unwrap();
        assert_eq!(announced.len(), 1, "exactly one announcement per block");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(
            announced[0], result.tool_calls[0],
            "an announced call must be identical to its committed twin — \
             harvest matches by exact equality"
        );
        assert_eq!(
            announced[0].input,
            serde_json::json!({"path": "src/lib.rs"})
        );
    }

    #[tokio::test]
    async fn complete_observed_never_announces_a_block_whose_json_is_broken() {
        let server = MockServer::start().await;
        // The block closes but its accumulated JSON does not parse — the
        // end-of-stream repair path owns it; speculation must never see it.
        let sse_body = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": not json,}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("go")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        };

        let observer = RecordingObserver(std::sync::Mutex::new(Vec::new()));
        let result = provider
            .complete_observed(req, &observer)
            .await
            .expect("broken JSON on a finished block is the repair sentinel, not an error");
        assert!(
            observer.0.lock().unwrap().is_empty(),
            "unparseable input must not be announced"
        );
        // The committed call still carries the Null repair sentinel.
        assert_eq!(result.tool_calls.len(), 1);
        assert!(result.tool_calls[0].input.is_null());
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
                attachments: Vec::new(),
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
                attachments: Vec::new(),
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
                attachments: Vec::new(),
            }],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
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
                attachments: Vec::new(),
            }],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
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
                attachments: Vec::new(),
            }],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
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
                cache_write_tokens: 0,
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
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
        };

        let err = provider.complete(req).await.unwrap_err();
        // overloaded_error ⇒ retryable Transport.
        assert!(matches!(err, ProviderError::Transport(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn thinking_budgets_map_effort_tiers_and_default_to_medium() {
        use stella_protocol::ReasoningEffort::*;
        assert_eq!(thinking_budget_tokens(Some(Low)), 2_048);
        assert_eq!(thinking_budget_tokens(Some(Medium)), 8_192);
        assert_eq!(thinking_budget_tokens(None), 8_192, "None defaults Medium");
        assert_eq!(thinking_budget_tokens(Some(High)), 16_384);
        assert_eq!(thinking_budget_tokens(Some(Xhigh)), 32_768);
        assert_eq!(thinking_budget_tokens(Some(Max)), 49_152);
    }

    /// Minimal happy-path SSE body for tests that only inspect the request.
    const OK_SSE: &str = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    );

    async fn mock_ok(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(OK_SSE, "text/event-stream"))
            .mount(server)
            .await;
    }

    async fn first_request_body(server: &MockServer) -> String {
        let requests = server.received_requests().await.expect("recorded requests");
        String::from_utf8_lossy(&requests[0].body).into_owned()
    }

    #[tokio::test]
    async fn generation_params_forward_top_p_and_top_k_and_drop_the_rest() {
        use stella_protocol::GenerationParams;
        let server = MockServer::start().await;
        mock_ok(&server).await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        provider
            .complete(CompletionRequest {
                messages: vec![CompletionMessage::user("hi")],
                max_output_tokens: None,
                temperature: None,
                effort: None,
                tools: vec![],
                reasoning: None,
                params: Some(GenerationParams {
                    top_p: Some(0.9),
                    top_k: Some(40),
                    // No Messages API slot — silently dropped, never a 400.
                    frequency_penalty: Some(0.5),
                    presence_penalty: Some(0.25),
                    repetition_penalty: Some(1.1),
                    seed: Some(7),
                    verbosity: None,
                    service_tier: None,
                }),
            })
            .await
            .expect("should succeed");

        let body = first_request_body(&server).await;
        assert!(body.contains("\"top_p\":0.9"), "{body}");
        assert!(body.contains("\"top_k\":40"), "{body}");
        for dropped in [
            "frequency_penalty",
            "presence_penalty",
            "repetition_penalty",
            "seed",
        ] {
            assert!(!body.contains(dropped), "`{dropped}` leaked into: {body}");
        }
    }

    /// `reasoning: Some(true)` with no caller output cap: the budget maps
    /// from effort, the defaulted max_tokens rises to budget + 8192 so
    /// thinking can't consume the whole output allowance, and temperature is
    /// omitted entirely (the API rejects temperature != 1 with thinking).
    #[tokio::test]
    async fn reasoning_true_enables_thinking_raises_max_tokens_and_omits_temperature() {
        let server = MockServer::start().await;
        mock_ok(&server).await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        provider
            .complete(CompletionRequest {
                messages: vec![CompletionMessage::user("think hard")],
                max_output_tokens: None,
                temperature: Some(0.0), // the engine default that would 400
                effort: Some(stella_protocol::ReasoningEffort::Max),
                tools: vec![],
                reasoning: Some(true),
                params: None,
            })
            .await
            .expect("should succeed");

        let body = first_request_body(&server).await;
        assert!(
            body.contains("\"thinking\":{\"type\":\"enabled\",\"budget_tokens\":49152}"),
            "{body}"
        );
        assert!(
            body.contains("\"max_tokens\":57344"),
            "49152 + 8192: {body}"
        );
        assert!(!body.contains("temperature"), "{body}");
    }

    /// A caller-set max_tokens is honored, and the budget clamps under it:
    /// at most max_tokens - 1024 (the API requires budget < max_tokens).
    #[tokio::test]
    async fn thinking_budget_clamps_under_a_caller_set_max_tokens() {
        let server = MockServer::start().await;
        mock_ok(&server).await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        provider
            .complete(CompletionRequest {
                messages: vec![CompletionMessage::user("think")],
                max_output_tokens: Some(4096),
                temperature: None,
                effort: None, // Medium default: 8192, which exceeds the cap
                tools: vec![],
                reasoning: Some(true),
                params: None,
            })
            .await
            .expect("should succeed");

        let body = first_request_body(&server).await;
        assert!(body.contains("\"max_tokens\":4096"), "{body}");
        assert!(
            body.contains("\"budget_tokens\":3072"),
            "4096 - 1024: {body}"
        );
    }

    /// `Some(false)` and `None` are the same wire shape: no thinking block
    /// (thinking is opt-in on this API), temperature forwarded as always —
    /// i.e. exactly today's request bytes.
    #[tokio::test]
    async fn reasoning_false_sends_no_thinking_block_and_keeps_temperature() {
        let server = MockServer::start().await;
        mock_ok(&server).await;

        let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
            .with_base_url(server.uri());
        provider
            .complete(CompletionRequest {
                messages: vec![CompletionMessage::user("hi")],
                max_output_tokens: None,
                temperature: Some(0.3),
                effort: Some(stella_protocol::ReasoningEffort::High),
                tools: vec![],
                reasoning: Some(false),
                params: None,
            })
            .await
            .expect("should succeed");

        let body = first_request_body(&server).await;
        assert!(!body.contains("thinking"), "{body}");
        assert!(!body.contains("budget_tokens"), "{body}");
        assert!(body.contains("\"temperature\":0.3"), "{body}");
        assert!(body.contains("\"max_tokens\":4096"), "default cap: {body}");
    }
}
