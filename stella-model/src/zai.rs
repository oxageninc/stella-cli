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
}

#[derive(Deserialize, Debug, Default)]
struct ZaiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
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
                usage.cached_input_tokens = u
                    .prompt_tokens_details
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0);
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
mod tests {
    use super::*;
    use stella_protocol::tool::ToolSchema;
    use wiremock::matchers::{body_string_contains, header, method, path};
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
                attachments: Vec::new(),
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
                attachments: Vec::new(),
            },
        ];
        let mapped = to_zai_messages(&messages);
        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[0].role, "assistant");
        assert_eq!(mapped[0].tool_calls.len(), 1);
        assert_eq!(mapped[1].role, "tool");
        assert_eq!(mapped[1].tool_call_id.as_deref(), Some("call_9"));
        assert_eq!(mapped[1].content.as_text(), "fn main(){}");
    }

    #[test]
    fn user_attachments_widen_content_to_parts_with_data_uri_images() {
        use stella_protocol::{Attachment, AttachmentSource};
        let att = |name: &str, mime: &str, b64: &str| Attachment {
            name: name.into(),
            media_type: mime.into(),
            byte_len: 3,
            source: AttachmentSource::Data { base64: b64.into() },
        };
        let messages = vec![
            CompletionMessage::user("plain"),
            CompletionMessage::user_with_attachments(
                "look",
                vec![
                    att("a.png", "image/png", "aW1n"),
                    att("b.pdf", "application/pdf", "cGRm"),
                ],
            ),
        ];
        let mapped = to_zai_messages(&messages);
        // A text-only user turn stays a plain string — byte-stable with the
        // pre-attachment wire format.
        let plain = serde_json::to_value(&mapped[0]).unwrap();
        assert_eq!(plain["content"], "plain");
        let multi = serde_json::to_value(&mapped[1]).unwrap();
        let parts = multi["content"].as_array().unwrap();
        assert_eq!(parts.len(), 3, "{multi}");
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,aW1n");
        // PDF degrades to a note on this dialect.
        assert_eq!(parts[1]["type"], "text");
        assert!(parts[1]["text"].as_str().unwrap().contains("b.pdf"));
        assert_eq!(parts[2]["type"], "text");
        assert_eq!(parts[2]["text"], "look");
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
            attachments: Vec::new(),
        }];
        let mapped = to_zai_messages(&messages);
        assert_eq!(mapped.len(), 1);
        assert!(mapped[0].content.as_text().starts_with("ERROR:"));
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
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
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

    /// Records announcements for comparison with the committed result.
    struct RecordingObserver {
        calls: std::sync::Mutex<Vec<ToolCall>>,
        deltas: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingObserver {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                deltas: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ToolCallObserver for RecordingObserver {
        fn tool_call_streamed(&self, call: &ToolCall) {
            self.calls.lock().unwrap().push(call.clone());
        }
        fn text_delta(&self, delta: &str) {
            self.deltas.lock().unwrap().push(delta.to_string());
        }
    }

    #[tokio::test]
    async fn complete_observed_streams_content_deltas_in_order_never_reasoning() {
        let server = MockServer::start().await;
        // Answer `content` interleaved with `reasoning_content`: the observer
        // must see exactly the visible fragments, in stream order — the
        // chain-of-thought never streams to it.
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"let me think\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"more thought\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo!\"}}]}\n\n",
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
            reasoning: None,
            params: None,
        };

        let observer = RecordingObserver::new();
        let result = provider
            .complete_observed(req, &observer)
            .await
            .expect("should succeed");

        let deltas = observer.deltas.lock().unwrap();
        assert_eq!(
            *deltas,
            vec!["Hel".to_string(), "lo!".to_string()],
            "answer fragments only, in order — reasoning excluded"
        );
        assert_eq!(
            result.text, "Hello!",
            "the committed text is the announced deltas' concatenation"
        );
    }

    #[tokio::test]
    async fn complete_observed_announces_a_call_when_the_next_index_starts() {
        let server = MockServer::start().await;
        // Two sequential tool calls: index 0 is complete the moment index 1
        // appears — the only mid-stream completion boundary the OpenAI
        // dialect offers. Index 1 (the last call) has no boundary and is
        // never announced.
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"b.rs\\\"}\"}}]}}]}\n\n",
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
            messages: vec![CompletionMessage::user("read both")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        };

        let observer = RecordingObserver::new();
        let result = provider
            .complete_observed(req, &observer)
            .await
            .expect("completion should succeed");

        let announced = observer.calls.lock().unwrap();
        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(
            announced.len(),
            1,
            "only index 0 has a completion boundary mid-stream"
        );
        assert_eq!(
            announced[0], result.tool_calls[0],
            "an announced call must be identical to its committed twin"
        );
    }

    #[tokio::test]
    async fn usage_accounting_sends_the_include_field_and_takes_the_reported_cost() {
        let server = MockServer::start().await;
        // OpenRouter's final usage frame carries the routed call's actual
        // cost; for an unseeded slug (no catalog pricing) that report is the
        // ONLY real price. The request must opt in via `usage.include`.
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"cost\":0.0123}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains("\"usage\":{\"include\":true}"))
            .and(header("HTTP-Referer", "https://stella.oxagen.sh"))
            .and(header("X-Title", "Stella"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = ZaiProvider::new(ApiKey::new("sk-or-test"), "anthropic/claude-sonnet-4.5")
            .with_base_url(server.uri())
            .with_identity("openrouter", "OpenRouter")
            .with_attribution("https://stella.oxagen.sh", "Stella")
            .with_usage_accounting();
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        };

        // The mock's matchers make a missing usage field / missing headers a
        // 404 — reaching a successful result proves the request shape.
        let result = provider.complete(req).await.expect("should succeed");
        assert!(
            (result.cost_usd - 0.0123).abs() < 1e-12,
            "gateway-reported cost is authoritative, got {}",
            result.cost_usd
        );
        assert_eq!(result.usage.input_tokens, 100);
    }

    #[tokio::test]
    async fn openrouter_reasoning_deltas_surface_like_reasoning_content() {
        let server = MockServer::start().await;
        // OpenRouter normalizes chain-of-thought to `delta.reasoning`. A
        // reasoning-only turn must surface it, same as GLM's
        // `reasoning_content`, instead of returning a blank turn.
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"thinking \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"hard\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = ZaiProvider::new(ApiKey::new("sk-or-test"), "openrouter/auto")
            .with_base_url(server.uri())
            .with_identity("openrouter", "OpenRouter");
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
        assert_eq!(result.text, "thinking hard");
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
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
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

    /// Z.ai's implicit prompt cache reports hits via the OpenAI-compatible
    /// `prompt_tokens_details.cached_tokens` field. Dropping it (the prior
    /// behavior) overbilled cached tokens at the full input rate and pinned
    /// the cache-hit stat at zero for every Z.ai run.
    #[tokio::test]
    async fn complete_surfaces_cached_tokens_and_bills_them_at_the_cached_rate() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":500,\"prompt_tokens_details\":{\"cached_tokens\":200}}}\n\n",
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
            reasoning: None,
            params: None,
        };

        let result = provider.complete(req).await.expect("should succeed");
        assert_eq!(result.usage.cached_input_tokens, 200);
        let expected = Catalog::seed()
            .resolve("glm-5.2")
            .unwrap()
            .pricing
            .cost_usd(&CompletionUsage {
                input_tokens: 1000,
                output_tokens: 500,
                cached_input_tokens: 200,
                cache_write_tokens: 0,
            });
        assert_eq!(result.cost_usd, expected, "200 cached tokens bill cheaper");
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
            reasoning: None,
            params: None,
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
            reasoning: None,
            params: None,
        };

        let err = provider.complete(req).await.unwrap_err();
        // server_error / overloaded ⇒ retryable Transport.
        assert!(matches!(err, ProviderError::Transport(_)));
        assert!(err.is_retryable());
    }

    /// Minimal happy-path SSE body for tests that only inspect the request.
    const OK_SSE: &str =
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";

    async fn mock_ok(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(OK_SSE, "text/event-stream"))
            .mount(server)
            .await;
    }

    async fn first_request_body(server: &MockServer) -> String {
        let requests = server.received_requests().await.expect("recorded requests");
        String::from_utf8_lossy(&requests[0].body).into_owned()
    }

    #[tokio::test]
    async fn generation_params_land_in_the_wire_body_including_nonstandard_fields() {
        use stella_protocol::GenerationParams;
        let server = MockServer::start().await;
        mock_ok(&server).await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
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
                    frequency_penalty: Some(0.5),
                    presence_penalty: Some(0.25),
                    repetition_penalty: Some(1.1),
                    seed: Some(7),
                    // Not part of this dialect — must never reach the wire.
                    verbosity: Some(stella_protocol::Verbosity::High),
                    service_tier: Some(stella_protocol::ServiceTier::Priority),
                }),
            })
            .await
            .expect("should succeed");

        let body = first_request_body(&server).await;
        assert!(body.contains("\"top_p\":0.9"), "{body}");
        // top_k / repetition_penalty are non-standard but the user opted in.
        assert!(body.contains("\"top_k\":40"), "{body}");
        assert!(body.contains("\"frequency_penalty\":0.5"), "{body}");
        assert!(body.contains("\"presence_penalty\":0.25"), "{body}");
        assert!(body.contains("\"repetition_penalty\":1.1"), "{body}");
        assert!(body.contains("\"seed\":7"), "{body}");
        // verbosity/service_tier have no Chat Completions slot: dropped.
        assert!(!body.contains("verbosity"), "{body}");
        assert!(!body.contains("service_tier"), "{body}");
    }

    /// The prompt-cache stability contract: a request without params or a
    /// reasoning preference must serialize with none of the new keys — the
    /// body is byte-identical to what this adapter sent before they existed.
    #[tokio::test]
    async fn absent_params_and_reasoning_add_no_keys_to_the_body() {
        let server = MockServer::start().await;
        mock_ok(&server).await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
        provider
            .complete(CompletionRequest {
                messages: vec![CompletionMessage::user("hi")],
                max_output_tokens: None,
                temperature: None,
                effort: None,
                tools: vec![],
                reasoning: None,
                params: None,
            })
            .await
            .expect("should succeed");

        let body = first_request_body(&server).await;
        for key in [
            "top_p",
            "top_k",
            "frequency_penalty",
            "presence_penalty",
            "repetition_penalty",
            "seed",
            "thinking",
            "reasoning",
        ] {
            assert!(!body.contains(key), "unexpected `{key}` in: {body}");
        }
    }

    #[tokio::test]
    async fn zai_identity_maps_reasoning_to_glm_thinking_object() {
        let server = MockServer::start().await;
        mock_ok(&server).await;

        let provider =
            ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
        let req = |reasoning| CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning,
            params: None,
        };
        provider.complete(req(Some(true))).await.expect("on");
        provider.complete(req(Some(false))).await.expect("off");

        let requests = server.received_requests().await.expect("recorded requests");
        let on = String::from_utf8_lossy(&requests[0].body);
        let off = String::from_utf8_lossy(&requests[1].body);
        assert!(on.contains("\"thinking\":{\"type\":\"enabled\"}"), "{on}");
        assert!(
            off.contains("\"thinking\":{\"type\":\"disabled\"}"),
            "{off}"
        );
    }

    #[tokio::test]
    async fn openrouter_identity_maps_reasoning_to_the_gateway_object() {
        let server = MockServer::start().await;
        mock_ok(&server).await;

        let provider = ZaiProvider::new(ApiKey::new("sk-or-test"), "openrouter/auto")
            .with_base_url(server.uri())
            .with_identity("openrouter", "OpenRouter");
        let req = |reasoning, effort| CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort,
            tools: vec![],
            reasoning,
            params: None,
        };
        // Pinned effort (reasoning not suppressed) → {"effort": …}.
        provider
            .complete(req(None, Some(ReasoningEffort::Xhigh)))
            .await
            .expect("effort");
        // Explicit off wins even over a pinned effort → {"enabled": false}.
        provider
            .complete(req(Some(false), Some(ReasoningEffort::High)))
            .await
            .expect("off");
        // Bare on with no effort → {"enabled": true}.
        provider.complete(req(Some(true), None)).await.expect("on");

        let requests = server.received_requests().await.expect("recorded requests");
        let bodies: Vec<String> = requests
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect();
        // Xhigh collapses to "high", the top tier OpenRouter models.
        assert!(
            bodies[0].contains("\"reasoning\":{\"effort\":\"high\"}"),
            "{}",
            bodies[0]
        );
        assert!(
            bodies[1].contains("\"reasoning\":{\"enabled\":false}"),
            "{}",
            bodies[1]
        );
        assert!(
            bodies[2].contains("\"reasoning\":{\"enabled\":true}"),
            "{}",
            bodies[2]
        );
        // GLM's thinking object is Z.ai-only; OpenRouter never sees it.
        assert!(!bodies[0].contains("thinking"), "{}", bodies[0]);
    }

    /// Identities other than Z.ai and OpenRouter (xAI, DeepSeek, local, …)
    /// have no known reasoning field on this dialect: the hint is ignored
    /// rather than guessed at — an unknown key risks a hard 400.
    #[tokio::test]
    async fn other_identities_ignore_reasoning_entirely() {
        let server = MockServer::start().await;
        mock_ok(&server).await;

        let provider = ZaiProvider::new(ApiKey::new("xai-test"), "grok-4")
            .with_base_url(server.uri())
            .with_identity("xai", "xAI");
        provider
            .complete(CompletionRequest {
                messages: vec![CompletionMessage::user("hi")],
                max_output_tokens: None,
                temperature: None,
                effort: Some(ReasoningEffort::High),
                tools: vec![],
                reasoning: Some(true),
                params: None,
            })
            .await
            .expect("should succeed");

        let body = first_request_body(&server).await;
        assert!(!body.contains("thinking"), "{body}");
        assert!(!body.contains("reasoning"), "{body}");
    }
}
