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
    /// Extended thinking, set only for `CompletionRequest.reasoning ==
    /// Some(true)`. Its wire shape depends on the model generation — adaptive
    /// (`{"type":"adaptive"}`) on current models, `{"type":"enabled",
    /// "budget_tokens":N}` on legacy ones — see [`AnthropicThinking`] and
    /// [`uses_adaptive_thinking`]. `Some(false)`/`None` omit it (thinking is
    /// opt-in per request), keeping the pre-field bytes stable.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    /// Output controls (`{"effort":"low|…|max"}`). On current models this is
    /// the depth/spend knob that replaced `thinking.budget_tokens`; omitted on
    /// legacy models, which reject the field. See [`AnthropicOutputConfig`].
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<AnthropicOutputConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicToolSchema>,
}

/// The Messages API's thinking switch, in one of two wire shapes chosen by the
/// model generation:
///   * `{"type":"adaptive"}` — current models (Claude 4.6+, the 5-family). The
///     model picks its own depth; [`AnthropicOutputConfig`]'s `effort` tunes
///     it. Sending `budget_tokens` here is an HTTP 400.
///   * `{"type":"enabled","budget_tokens":N}` — legacy models (≤ 4.5), where
///     `N` must satisfy `1024 <= N < max_tokens` (see [`thinking_budget_tokens`]).
#[derive(Serialize)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    kind: &'static str,
    /// Present only on the legacy `enabled` shape; omitted (and rejected) on
    /// the adaptive shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<u32>,
}

impl AnthropicThinking {
    /// `{"type":"adaptive"}` — the current-model shape (no budget field).
    const fn adaptive() -> Self {
        Self {
            kind: "adaptive",
            budget_tokens: None,
        }
    }

    /// `{"type":"enabled","budget_tokens":N}` — the legacy-model shape.
    const fn enabled(budget_tokens: u32) -> Self {
        Self {
            kind: "enabled",
            budget_tokens: Some(budget_tokens),
        }
    }
}

/// The Messages API's `output_config` object. Only `effort` is modeled — the
/// GA depth/spend control on current models (no beta header), which replaces
/// the legacy per-request thinking budget. Rejected by legacy models.
#[derive(Serialize)]
struct AnthropicOutputConfig {
    effort: &'static str,
}

/// Whether `model` speaks the current adaptive-thinking wire shape
/// (`thinking:{type:"adaptive"}` + `output_config.effort`, with `temperature`/
/// `top_p`/`top_k` rejected) rather than the legacy
/// `{type:"enabled",budget_tokens}` shape (which accepts sampling).
///
/// Claude 4.6+ and the "5" family (Fable 5, Mythos 5, Sonnet 5, …) **require**
/// the adaptive shape and answer a stray `budget_tokens` — or any sampling
/// parameter — with an HTTP 400. That 400 is exactly the failure this classifier
/// exists to prevent. The 4.5-and-older generations still use `budget_tokens`.
///
/// The legacy set is closed and shrinking; the modern set is open and growing.
/// So we **denylist** the known legacy generations and default everything else
/// — including models released after this code was written — to the modern
/// shape. An allowlist would silently 400 the next launch (fable-6, opus-5),
/// which is precisely how this bug reached production.
fn uses_adaptive_thinking(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    // Version markers unique to the ≤ 4.5 / 3.x / 2.x generations. `-4-5`
    // cleanly separates 4.5 (opus/sonnet/haiku) from 4.6+/…-8; `-4-2025`
    // catches dated 4.0 snapshots (`claude-*-4-20250514`), which `-4-0` misses.
    const LEGACY_MARKERS: &[&str] = &["-4-5", "-4-1", "-4-0", "-4-2025", "claude-3", "claude-2"];
    !LEGACY_MARKERS.iter().any(|marker| m.contains(marker))
}

/// Map the engine's effort tiers onto thinking budgets, for the **legacy**
/// `budget_tokens` shape only. Anthropic's older models had no named levels —
/// the budget IS the level — so the tiers are spaced roughly geometrically from
/// the API's 1024-token floor up to a Max that still leaves headroom under
/// typical output caps. `None` defaults to Medium, the same middle-tier default
/// posture as `openai.rs` ("effort":"medium"). Current models use
/// [`map_effort`] instead.
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

/// Map the engine's effort tiers onto the Messages API's `output_config.effort`
/// levels for the current adaptive shape. The vocabularies line up 1:1, so this
/// is a direct rename (unlike the legacy [`thinking_budget_tokens`] mapping).
fn map_effort(effort: stella_protocol::ReasoningEffort) -> &'static str {
    use stella_protocol::ReasoningEffort::*;
    match effort {
        Low => "low",
        Medium => "medium",
        High => "high",
        Xhigh => "xhigh",
        Max => "max",
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
/// marker (two of the four allowed breakpoints). Block-level is the
/// placement this adapter sends (never a top-level `cache_control` request
/// field, which Anthropic's documented behavior for unknown parameters
/// says would 400) — see `stella-model/tests/live_smoke.rs`'s
/// `anthropic_smoke` (#274) for the live-wire status of that claim: as of
/// 2026-07-21 it's still unverified end-to-end (the one available
/// credential hit an account-billing 400 before the request shape was ever
/// evaluated), so treat "top-level would 400" as an inherited assumption
/// from PR #221, not a confirmed fact, until that smoke test reports a
/// clean pass.
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
        let reasoning_on = req.reasoning == Some(true);
        let params = req.params.unwrap_or_default();

        // Two thinking dialects, chosen by model generation. Current models
        // (Claude 4.6+, the 5-family) take `thinking:{type:"adaptive"}` plus
        // `output_config.effort`, and REJECT `budget_tokens` and every sampling
        // parameter with a 400 — the failure this fix repairs. Legacy models
        // (≤ 4.5) keep the old `{type:"enabled",budget_tokens}` shape and accept
        // sampling. `uses_adaptive_thinking` denylists the closed legacy set and
        // defaults everything else (incl. future launches) to the modern shape.
        let (max_tokens, thinking, output_config, temperature, top_p, top_k) =
            if uses_adaptive_thinking(&self.model) {
                // Adaptive thinking spends from the SAME output allowance as the
                // answer, so an un-capped reasoning turn needs far more headroom
                // than the old 4096 no-thinking default — 4096 would truncate a
                // max-effort judge mid-verdict (returning empty text with
                // stop_reason=max_tokens). We already stream, so a high ceiling
                // costs nothing; a caller-set cap is still honored as-is.
                let max_tokens =
                    req.max_output_tokens
                        .unwrap_or(if reasoning_on { 32_000 } else { 4096 });
                (
                    max_tokens,
                    reasoning_on.then(AnthropicThinking::adaptive),
                    // Effort is a GA control independent of thinking; forward it
                    // whenever the caller pinned one, defaulting to the API's own
                    // (high) when unset.
                    req.effort.map(|effort| AnthropicOutputConfig {
                        effort: map_effort(effort),
                    }),
                    // Sampling params 400 on these models — never send them.
                    None,
                    None,
                    None,
                )
            } else {
                // Legacy shape: budget_tokens is coupled to max_tokens (the API
                // requires budget < max_tokens), and the 4096 default would leave
                // no room for any budget above Low — so when thinking is on and
                // the caller set no cap, the floor rises to budget + 8192. A
                // caller-set cap is honored and the budget clamps to it: at most
                // max_tokens - 1024, never below the 1024 floor; a cap at or below
                // the floor leaves NO legal budget, so thinking is omitted rather
                // than sent as a 400.
                let thinking_budget = reasoning_on.then(|| thinking_budget_tokens(req.effort));
                let max_tokens = match (req.max_output_tokens, thinking_budget) {
                    (Some(cap), _) => cap,
                    (None, Some(budget)) => budget + 8_192,
                    (None, None) => 4096,
                };
                let thinking = thinking_budget.and_then(|budget| {
                    (max_tokens > 1024).then(|| {
                        AnthropicThinking::enabled(budget.min(max_tokens - 1024).max(1024))
                    })
                });
                // The API rejects temperature != 1 with thinking enabled; omit it
                // entirely (rather than special-casing 1.0) and let the API apply
                // its own thinking-compatible default.
                let temperature = if thinking.is_some() {
                    None
                } else {
                    req.temperature
                };
                (
                    max_tokens,
                    thinking,
                    None,
                    temperature,
                    params.top_p,
                    params.top_k,
                )
            };

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
            temperature,
            top_p,
            top_k,
            thinking,
            output_config,
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
                &self.model,
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
                    // Only user-visible answer text is announced — thinking
                    // deltas (`thinking_delta`) deserialize as `Other` and
                    // never reach the observer.
                    AnthropicDelta::TextDelta { text: delta } => {
                        if let Some(observer) = observer {
                            observer.text_delta(&delta);
                        }
                        text.push_str(&delta);
                    }
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
                        usage.reported = true;
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
mod tests;
