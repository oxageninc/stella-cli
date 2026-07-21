//! Z.ai adapter — OpenAI-compatible chat completions + SSE streaming, GLM
//! 5.2's tool-call dialect (`openai-json`: an accumulating `tool_calls`
//! array keyed by index, arguments streamed as string fragments). This is
//! the *other* Phase 0 spike ( step 3) and the default suite
//! — it must work first, not last.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stella_protocol::{
    CompletionMessage, CompletionRequest, CompletionResult, CompletionUsage, FinishReason,
    MessageRole, ProviderError, ReasoningEffort, ToolCall,
};

use crate::catalog::{Catalog, Pricing};
use crate::credential::ApiKey;
use crate::http;
use crate::provider::{Provider, ToolCallObserver};
use crate::sse::SseDecoder;

/// International endpoint. `open.bigmodel.cn` (mainland) is the same wire
/// shape behind a different base URL — `with_base_url` covers both without
/// a second adapter ( note).
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
    /// `cost_usd` is computed on the real request path. `None` when the slug
    /// isn't in the catalog — for seeded providers `build_provider`
    /// (`agent.rs`) rejects that case up front; for gateway providers with
    /// free-form slugs (OpenRouter) it is legitimately `None` and the cost
    /// comes from the gateway's own usage accounting instead.
    pricing: Option<Pricing>,
    id: String,
    label: String,
    /// Extra headers sent with every request — OpenRouter's app-attribution
    /// pair (`HTTP-Referer`, `X-Title`). Empty for every other provider.
    extra_headers: Vec<(&'static str, String)>,
    /// Ask the gateway to report the request's actual cost in the final
    /// usage frame (OpenRouter `usage: {"include": true}`). When the frame
    /// carries a cost, it overrides catalog list pricing — the gateway
    /// routed the call, only it knows what the call cost.
    usage_accounting: bool,
}

impl ZaiProvider {
    pub fn new(api_key: ApiKey, model: impl Into<String>) -> Self {
        let model = model.into();
        let catalog = Catalog::current();
        let pricing = catalog.resolve(&model).ok().map(|e| e.pricing);
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
            extra_headers: Vec::new(),
            usage_accounting: false,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// OpenRouter app attribution: `HTTP-Referer` names the app's site,
    /// `X-Title` its display name (shown on openrouter.ai rankings and in
    /// the user's activity feed). Harmless on any other OpenAI-compatible
    /// endpoint, but only the OpenRouter build path sets it.
    pub fn with_attribution(
        mut self,
        referer: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        self.extra_headers.push(("HTTP-Referer", referer.into()));
        self.extra_headers.push(("X-Title", title.into()));
        self
    }

    /// Request per-call cost reporting in the stream's final usage frame
    /// (OpenRouter `usage: {"include": true}`). A reported cost overrides
    /// catalog list pricing in `CompletionResult::cost_usd`, which is what
    /// makes budget metering real for a gateway whose routed models (and
    /// therefore prices) are not in our seed catalog.
    pub fn with_usage_accounting(mut self) -> Self {
        self.usage_accounting = true;
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
        // Re-resolve pricing scoped to the real provider id: the same bare
        // slug can exist on more than one provider, and a provider-scoped
        // catalog row (e.g. an xAI or DeepSeek row from `stella models
        // refresh`) is authoritative over whatever the unscoped constructor
        // lookup found — or didn't find — under the default `zai` identity.
        let catalog = Catalog::current();
        if let Ok(entry) = catalog.resolve_for(&self.id, &self.model) {
            self.pricing = Some(entry.pricing);
        }
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
    /// Sampling overrides from `CompletionRequest.params`, each skipped when
    /// `None` so a request without overrides serializes byte-identical to
    /// what this adapter has always sent (the prompt-cache contract).
    /// `top_p`/`frequency_penalty`/`presence_penalty`/`seed` are standard
    /// Chat Completions parameters; `top_k` and `repetition_penalty` are
    /// non-standard but accepted by OpenRouter and local servers — they are
    /// forwarded whenever `Some` because the user explicitly opted in, and an
    /// honest provider error beats silently dropping an explicit setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repetition_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    /// GLM's native thinking switch (`{"type": "enabled"|"disabled"}`) —
    /// only sent when this adapter is serving Z.ai itself AND the caller set
    /// `CompletionRequest.reasoning`; other Chat Completions servers don't
    /// speak this field and `None` keeps their wire untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ZaiThinking>,
    /// OpenRouter's normalized reasoning control — only sent when this
    /// adapter is re-identified as OpenRouter, which translates it to
    /// whatever the routed upstream vendor calls it. See
    /// [`openrouter_reasoning`] for the effort/enabled shape rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenRouterReasoning>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ZaiToolSchema>,
    /// OpenRouter usage accounting (`{"include": true}`) — omitted entirely
    /// unless the provider was built `with_usage_accounting`, so the shared
    /// adapter never sends a field other Chat Completions servers might
    /// reject.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<ZaiUsageInclude>,
    /// OpenRouter's request-root automatic prompt-caching switch
    /// (`{"type": "ephemeral"}`): the gateway places a cache breakpoint at
    /// the last cacheable block and advances it as the conversation grows.
    /// Anthropic models routed through OpenRouter get NO prompt caching
    /// without it — their cache is explicit opt-in, unlike the implicit
    /// OpenAI/Gemini/DeepSeek caches — so every agent turn re-billed the
    /// full growing prefix at the uncached input rate and pinned the
    /// cache-hit stat at zero. Providers with implicit caches ignore the
    /// field (OpenRouter normalizes it per upstream), so it is sent on
    /// every OpenRouter request; no other Chat Completions server speaks
    /// it, so any other identity keeps it off the wire.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<ZaiCacheControl>,
}

/// GLM's request-level thinking object: `{"type": "enabled"}` /
/// `{"type": "disabled"}`. Z.ai's default depends on the model, so the field
/// is only sent when the caller expressed an explicit preference —
/// `CompletionRequest.reasoning == None` keeps the provider default AND the
/// pre-field wire bytes.
#[derive(Serialize)]
struct ZaiThinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

/// OpenRouter's `reasoning` object. Exactly one field is set per request:
/// `effort` when the caller pinned a level, `enabled` for a bare on/off —
/// sending both would be redundant (effort implies enabled) and `enabled:
/// false` with an effort would be contradictory.
#[derive(Serialize)]
struct OpenRouterReasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
}

/// Map the engine's one `ReasoningEffort` enum to OpenRouter's
/// `reasoning.effort`, which only accepts `low`/`medium`/`high`. Same
/// collapse posture as `openai.rs::map_reasoning_effort`: never drop the
/// hint, never panic on a variant the gateway doesn't model.
fn map_openrouter_effort(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High | ReasoningEffort::Xhigh | ReasoningEffort::Max => "high",
    }
}

/// Build OpenRouter's `reasoning` object from the request's reasoning/effort
/// pair. Rules: an explicit off (`reasoning == Some(false)`) always wins and
/// sends `{"enabled": false}` — a pinned effort must not resurrect thinking
/// the caller suppressed; otherwise a pinned effort sends
/// `{"effort": …}` (implicitly enabling); a bare `Some(true)` with no effort
/// sends `{"enabled": true}` and lets the gateway pick the level; and
/// neither set keeps the field off the wire entirely (provider default,
/// byte-stable with the pre-field body).
fn openrouter_reasoning(
    reasoning: Option<bool>,
    effort: Option<ReasoningEffort>,
) -> Option<OpenRouterReasoning> {
    match (reasoning, effort) {
        (Some(false), _) => Some(OpenRouterReasoning {
            effort: None,
            enabled: Some(false),
        }),
        (_, Some(effort)) => Some(OpenRouterReasoning {
            effort: Some(map_openrouter_effort(effort)),
            enabled: None,
        }),
        (Some(true), None) => Some(OpenRouterReasoning {
            effort: None,
            enabled: Some(true),
        }),
        (None, None) => None,
    }
}

#[derive(Serialize)]
struct ZaiUsageInclude {
    include: bool,
}

/// OpenRouter's cache-control object, `{"type": "ephemeral"}` — the 5-minute
/// default TTL. The 1-hour variant costs 2x input on writes (vs 1.25x) and
/// only pays off for sessions idle between turns, which an agent loop never
/// is.
#[derive(Serialize)]
struct ZaiCacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct ZaiMessage {
    role: &'static str,
    content: ZaiContent,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ZaiOutboundToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// Message content in the OpenAI-compatible chat dialect: a plain string for
/// text-only turns (byte-stable with what this adapter has always sent —
/// the prompt-cache contract), or a part array when a user turn carries
/// attachments.
#[derive(Serialize, Debug, PartialEq)]
#[serde(untagged)]
enum ZaiContent {
    Text(String),
    Parts(Vec<ZaiContentPart>),
}

impl ZaiContent {
    /// The plain-text form, for assertions and logging; a parts array is not
    /// plain text and yields `""`.
    #[cfg(test)]
    fn as_text(&self) -> &str {
        match self {
            ZaiContent::Text(text) => text,
            ZaiContent::Parts(_) => "",
        }
    }
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ZaiContentPart {
    Text { text: String },
    ImageUrl { image_url: ZaiImageUrl },
}

#[derive(Serialize, Debug, PartialEq)]
struct ZaiImageUrl {
    /// A `data:` URI carrying the base64 payload.
    url: String,
}

/// GLM vision models ingest images via `image_url` parts; PDFs, audio, and
/// video degrade to descriptive text notes in this dialect.
const ZAI_CAPS: crate::attachment::DialectCaps = crate::attachment::DialectCaps {
    images: true,
    pdfs: false,
    audio: false,
    video: false,
};

/// Content for a user turn: a plain string when there are no attachments,
/// otherwise a parts array with media before text.
fn user_content(message: &CompletionMessage) -> ZaiContent {
    if message.attachments.is_empty() {
        return ZaiContent::Text(message.content.clone());
    }
    let mut parts: Vec<ZaiContentPart> =
        crate::attachment::wire_parts(&message.attachments, ZAI_CAPS)
            .into_iter()
            .map(|part| match part {
                crate::attachment::WirePart::Image { media_type, base64 } => {
                    ZaiContentPart::ImageUrl {
                        image_url: ZaiImageUrl {
                            url: format!("data:{media_type};base64,{base64}"),
                        },
                    }
                }
                crate::attachment::WirePart::Text { text } => ZaiContentPart::Text { text },
                crate::attachment::WirePart::Pdf { .. }
                | crate::attachment::WirePart::Audio { .. }
                | crate::attachment::WirePart::Video { .. } => {
                    unreachable!("caps exclude pdf/audio/video")
                }
            })
            .collect();
    if !message.content.is_empty() {
        parts.push(ZaiContentPart::Text {
            text: message.content.clone(),
        });
    }
    ZaiContent::Parts(parts)
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

/// An OpenAI-compatible in-band error object. We classify only on
/// `type`/`message` text — the gateways don't share a stable machine
/// code, so matching on human-readable text is the only reliable signal.
#[derive(Deserialize, Debug, Default)]
struct ZaiStreamError {
    #[serde(default)]
    message: String,
    #[serde(default, rename = "type")]
    kind: String,
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
    /// GLM streams chain-of-thought under `reasoning_content`, separate from
    /// the answer in `content`. Without deserializing it, a turn that spends
    /// its whole output budget reasoning (and is cut off before emitting any
    /// `content`) looks empty — the adapter returns no text and no tool call,
    /// and the driver used to record that as a clean completion (the "turn
    /// ends with no feedback" defect). Captured so it can be surfaced.
    #[serde(default)]
    reasoning_content: Option<String>,
    /// OpenRouter's normalized name for the same thing: reasoning models
    /// routed through the gateway stream chain-of-thought under `reasoning`,
    /// whatever the upstream vendor calls it. Folded into the same buffer as
    /// `reasoning_content` so a reasoning-only turn is visible regardless of
    /// which endpoint served it.
    #[serde(default)]
    reasoning: Option<String>,
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
    /// OpenAI-compatible cache detail: prompt tokens served from Z.ai's
    /// implicit (server-side, no opt-in) prompt cache. Unlike Anthropic's
    /// envelope these are already folded into `prompt_tokens`, so they map
    /// straight onto the normalized subset invariant (cached ⊆ input) and
    /// bill at the catalog's cheaper cached rate instead of full input.
    #[serde(default)]
    prompt_tokens_details: Option<ZaiPromptTokensDetails>,
    /// OpenRouter usage accounting: the request's actual cost in USD
    /// credits, present only when the request opted in (`usage.include`).
    /// Authoritative over catalog list pricing — the gateway routed the
    /// call, only it knows which upstream (at which price) served it.
    #[serde(default)]
    cost: Option<f64>,
    /// DeepSeek's native cache-hit field: the platform reports prompt-cache
    /// hits as TOP-LEVEL `prompt_cache_hit_tokens` (paired with
    /// `prompt_cache_miss_tokens`; hits + misses = `prompt_tokens`) and does
    /// NOT send an OpenAI-style `prompt_tokens_details` object. Absent on
    /// every other endpoint this adapter serves, where `serde(default)`
    /// keeps it 0.
    #[serde(default)]
    prompt_cache_hit_tokens: u64,
}

#[derive(Deserialize, Debug, Default)]
struct ZaiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
    /// OpenRouter-only sibling of `cached_tokens`: tokens WRITTEN to the
    /// upstream's prompt cache by this call (Anthropic bills them at 1.25x
    /// input). Absent on every other OpenAI-compatible endpoint, where
    /// `serde(default)` keeps it 0 — matching the normalized envelope's
    /// "0 when never reported" contract.
    #[serde(default)]
    cache_write_tokens: u64,
}

fn to_zai_messages(messages: &[CompletionMessage]) -> Vec<ZaiMessage> {
    let mut out = Vec::new();
    for message in messages {
        match message.role {
            MessageRole::System => out.push(ZaiMessage {
                role: "system",
                content: ZaiContent::Text(message.content.clone()),
                tool_calls: Vec::new(),
                tool_call_id: None,
            }),
            MessageRole::User => out.push(ZaiMessage {
                role: "user",
                content: user_content(message),
                tool_calls: Vec::new(),
                tool_call_id: None,
            }),
            MessageRole::Assistant => out.push(ZaiMessage {
                role: "assistant",
                content: ZaiContent::Text(message.content.clone()),
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
                        content: ZaiContent::Text(content),
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

impl ZaiProvider {
    /// The one request/stream/aggregate body behind both `complete` and
    /// `complete_observed` — the observer is threaded down to the stream
    /// aggregator, which announces each tool call as soon as the next one
    /// starts streaming (the OpenAI dialect's only completion boundary).
    async fn complete_inner(
        &self,
        req: CompletionRequest,
        observer: Option<&dyn ToolCallObserver>,
    ) -> Result<CompletionResult, ProviderError> {
        // "Include" semantics: every override is `None` unless the caller
        // set it, and `None` never reaches the wire — a request without
        // params serializes byte-identical to the pre-params body.
        let params = req.params.unwrap_or_default();
        let body = ZaiRequest {
            model: &self.model,
            messages: to_zai_messages(&req.messages),
            stream: true,
            max_tokens: req.max_output_tokens,
            temperature: req.temperature,
            top_p: params.top_p,
            top_k: params.top_k,
            frequency_penalty: params.frequency_penalty,
            presence_penalty: params.presence_penalty,
            repetition_penalty: params.repetition_penalty,
            seed: params.seed,
            // Reasoning routes by identity: only Z.ai itself speaks GLM's
            // `thinking` object and only OpenRouter speaks the normalized
            // `reasoning` object. Any other identity behind this shared
            // adapter (xAI, DeepSeek, local, settings-defined) gets neither —
            // there is no portable Chat Completions reasoning field, and an
            // unknown key would risk a hard 400 on servers the user never
            // opted into experimenting with.
            thinking: (self.id == "zai")
                .then(|| {
                    req.reasoning.map(|enabled| ZaiThinking {
                        kind: if enabled { "enabled" } else { "disabled" },
                    })
                })
                .flatten(),
            reasoning: (self.id == "openrouter")
                .then(|| openrouter_reasoning(req.reasoning, req.effort))
                .flatten(),
            tools: to_zai_tools(&req.tools),
            usage: self
                .usage_accounting
                .then_some(ZaiUsageInclude { include: true }),
            cache_control: (self.id == "openrouter")
                .then_some(ZaiCacheControl { kind: "ephemeral" }),
        };

        let mut request = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(self.api_key.reveal());
        for (name, value) in &self.extra_headers {
            request = request.header(*name, value);
        }
        let response = request
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
                &self.model,
            ));
        }

        let (text, tool_calls, usage, finish_reason, reported_cost_usd) =
            aggregate_zai_stream(response, &self.label, observer).await?;
        // A gateway-reported cost (OpenRouter usage accounting) is
        // authoritative; catalog list pricing is the estimate for providers
        // that don't report one.
        let cost_usd = reported_cost_usd
            .unwrap_or_else(|| self.pricing.map(|p| p.cost_usd(&usage)).unwrap_or(0.0));
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

/// Accumulator for one in-progress streamed tool call, keyed by the
/// provider's `index` field until it's complete.
#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
    /// Whether this call was already announced to a [`ToolCallObserver`].
    /// OpenAI-style tool calls stream sequentially by index, so a call is
    /// complete the moment a HIGHER index appears — that boundary announces
    /// it exactly once. The stream's last call has no such boundary and is
    /// simply never announced (the completion returns immediately after, so
    /// there is nothing to overlap with anyway).
    announced: bool,
}

/// Announce every un-announced accumulator below `next_index` to the
/// observer. Only calls whose arguments already parse are announced — a
/// call the end-of-stream assembly would hand the `Null` repair sentinel
/// must never reach speculative execution. Announced calls re-parse the
/// same bytes at final assembly, so an announced call and its committed
/// twin are structurally identical.
fn announce_completed_below(
    observer: &dyn ToolCallObserver,
    tool_calls: &mut BTreeMap<usize, ToolCallAccumulator>,
    next_index: usize,
) {
    for (_, acc) in tool_calls.range_mut(..next_index) {
        if acc.announced {
            continue;
        }
        acc.announced = true;
        if acc.id.is_empty() {
            continue;
        }
        let trimmed = acc.arguments.trim();
        let input = if trimmed.is_empty() {
            Some(Value::Object(serde_json::Map::new()))
        } else {
            serde_json::from_str(trimmed).ok()
        };
        if let Some(input) = input {
            observer.tool_call_streamed(&ToolCall {
                call_id: acc.id.clone(),
                name: acc.name.clone(),
                input,
            });
        }
    }
}

async fn aggregate_zai_stream(
    response: reqwest::Response,
    label: &str,
    observer: Option<&dyn ToolCallObserver>,
) -> Result<
    (
        String,
        Vec<ToolCall>,
        CompletionUsage,
        Option<FinishReason>,
        Option<f64>,
    ),
    ProviderError,
> {
    let mut decoder = SseDecoder::new();
    let mut text = String::new();
    // Chain-of-thought streamed under `reasoning_content`, kept separate from
    // the answer. Used only as a fallback when `content` never arrives, so a
    // reasoning-only turn is visible instead of blank.
    let mut reasoning = String::new();
    let mut usage = CompletionUsage::default();
    let mut tool_calls: BTreeMap<usize, ToolCallAccumulator> = BTreeMap::new();
    // Set once any choice reports `finish_reason: "length"` — the output was
    // cut off at the token limit, so a tool call whose argument JSON didn't
    // finish streaming is truncated, not merely malformed.
    let mut truncated_at_token_limit = false;
    // Gateway-reported per-call cost (OpenRouter usage accounting), from the
    // final usage frame. `None` when the endpoint doesn't report one.
    let mut reported_cost_usd: Option<f64> = None;
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
                let details = u.prompt_tokens_details.unwrap_or_default();
                // Two wire spellings for the same fact: the OpenAI-compatible
                // details object, or DeepSeek's native top-level field — take
                // whichever the server spoke (never both on one endpoint).
                usage.cached_input_tokens = if details.cached_tokens > 0 {
                    details.cached_tokens
                } else {
                    u.prompt_cache_hit_tokens
                };
                usage.cache_write_tokens = details.cache_write_tokens;
                if u.cost.is_some() {
                    reported_cost_usd = u.cost;
                }
            }
            for choice in parsed.choices {
                if choice.finish_reason.as_deref() == Some("length") {
                    truncated_at_token_limit = true;
                }
                if let Some(content) = choice.delta.content {
                    // Only `content` (the user-visible answer) is announced —
                    // `reasoning_content`/`reasoning` stay observer-silent,
                    // including when the reasoning-only fallback below ends up
                    // supplying the final text (the deltas are best-effort).
                    if let Some(observer) = observer {
                        observer.text_delta(&content);
                    }
                    text.push_str(&content);
                }
                if let Some(rc) = choice.delta.reasoning_content {
                    reasoning.push_str(&rc);
                }
                if let Some(r) = choice.delta.reasoning {
                    reasoning.push_str(&r);
                }
                for tc_delta in choice.delta.tool_calls {
                    // A delta for index N proves every lower index finished
                    // streaming (the dialect emits calls sequentially) —
                    // the moment those calls can be announced for
                    // speculative execution.
                    if let Some(observer) = observer {
                        announce_completed_below(observer, &mut tool_calls, tc_delta.index);
                    }
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

    // Reasoning-only fallback: if the model emitted no answer `content` but did
    // stream chain-of-thought, surface the reasoning as the text so the turn is
    // never blank. Normal turns keep `content` as the answer and ignore it.
    if text.trim().is_empty() && !reasoning.trim().is_empty() {
        text = reasoning;
    }

    let finish_reason = if truncated_at_token_limit {
        Some(FinishReason::Length)
    } else if !calls.is_empty() {
        Some(FinishReason::ToolCalls)
    } else {
        Some(FinishReason::Stop)
    };

    Ok((text, calls, usage, finish_reason, reported_cost_usd))
}

#[cfg(test)]
mod tests;
