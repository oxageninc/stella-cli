//! Gemini direct adapter — Google's native `generativelanguage.googleapis.com`
//! generateContent API (`gemini-functions` dialect),
//! replacing the OpenAI-compatibility shim `stella-cli` previously pointed
//! `ZaiProvider` at. The shim works for plain chat but is not the wire shape
//! Gemini needs, and drops everything Gemini-specific: thinking level,
//! thought-signature round-trips (required for Gemini 3 function calling),
//! cached-token accounting, and the native media endpoints (Imagen/Veo)
//! that later phases hang off this same adapter family.
//!
//! The wire types and stream aggregation here are shared with
//! `vertex.rs` — Vertex AI speaks the identical `generateContent` response
//! shape behind different auth (OAuth bearer vs. API key) and a
//! project/location-scoped URL, so the two adapters differ only in those
//! seams ("casual Gemini use → direct adapter",
//! Vertex is the enterprise path).

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
use crate::provider::Provider;
use crate::sse::SseDecoder;

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Separator between the synthesized call ordinal and a Gemini thought
/// signature inside a `ToolCall::call_id`. Gemini's wire has no call ids at
/// all — calls correlate to responses by function *name* and order — but the
/// engine's internal protocol requires one, so this adapter mints
/// `call_0`, `call_1`, …. Gemini 3 additionally attaches a `thoughtSignature`
/// to streamed `functionCall` parts and *requires* it to be echoed back on
/// the matching part in conversation history (omitting it degrades or
/// rejects the next turn). `CompletionMessage`/`ToolCall` have no slot for a
/// provider-private blob, so the signature rides inside the minted call id
/// after this separator (`call_0#<sig>`) and is split back out when history
/// is re-framed for the wire. `#` cannot appear in the base64 signature, so
/// the split is unambiguous.
const SIGNATURE_SEPARATOR: char = '#';

pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: ApiKey,
    base_url: String,
    model: String,
    /// List pricing for `model`, resolved from the catalog at construction so
    /// `cost_usd` is computed on the real request path (see `zai.rs`). `None`
    /// only if the slug is absent from the catalog — the same posture as the
    /// other adapters, never a silent hard-coded zero.
    pricing: Option<Pricing>,
}

impl GeminiProvider {
    /// Build an adapter for `model` (a catalog-resolved slug, e.g.
    /// `gemini-3-pro` — never a literal chosen at the call site).
    pub fn new(api_key: ApiKey, model: impl Into<String>) -> Self {
        let model = model.into();
        // Scope the lookup to the `gemini` provider: `gemini-3-pro` is seeded
        // under both `gemini` and `vertex`, so a bare `resolve` would be
        // ambiguous (see `Catalog::resolve_for`).
        let catalog = Catalog::current();
        let pricing = catalog
            .resolve_for("gemini", &model)
            .ok()
            .map(|e| e.pricing);
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

// ── Wire types (Gemini generateContent API) ──────────────────────────────
//
// `pub(crate)` where `vertex.rs` shares them: the request/response envelope
// is identical between the two Google surfaces; only auth and URL differ.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system_instruction: Option<GeminiSystemInstruction>,
    pub(crate) contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tools: Vec<GeminiToolDecls>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Serialize)]
pub(crate) struct GeminiSystemInstruction {
    pub(crate) parts: Vec<GeminiTextPart>,
}

#[derive(Serialize)]
pub(crate) struct GeminiTextPart {
    pub(crate) text: String,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiContent {
    pub(crate) role: &'static str,
    pub(crate) parts: Vec<GeminiOutboundPart>,
}

/// One outbound content part. Exactly one of the value fields is set per
/// part (the wire treats them as a union); `thought_signature` may accompany
/// a `function_call` part when replaying Gemini 3 history — see
/// [`SIGNATURE_SEPARATOR`].
#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiOutboundPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
    /// A user attachment's payload (`inlineData` on the wire) — Gemini
    /// ingests images, PDFs, audio, and video through this one field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) inline_data: Option<GeminiInlineData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) function_response: Option<GeminiFunctionResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thought_signature: Option<String>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiInlineData {
    pub(crate) mime_type: String,
    pub(crate) data: String,
}

/// Gemini's generateContent ingests every kind Stella models — images,
/// PDFs, audio, and video — as `inlineData` parts.
const GEMINI_CAPS: crate::attachment::DialectCaps = crate::attachment::DialectCaps {
    images: true,
    pdfs: true,
    audio: true,
    video: true,
};

/// Map a user message's attachments to outbound parts (media before text).
fn attachment_parts(message: &CompletionMessage) -> Vec<GeminiOutboundPart> {
    crate::attachment::wire_parts(&message.attachments, GEMINI_CAPS)
        .into_iter()
        .map(|part| match part {
            crate::attachment::WirePart::Text { text } => GeminiOutboundPart {
                text: Some(text),
                ..Default::default()
            },
            crate::attachment::WirePart::Image { media_type, base64 }
            | crate::attachment::WirePart::Audio { media_type, base64 }
            | crate::attachment::WirePart::Video { media_type, base64 } => GeminiOutboundPart {
                inline_data: Some(GeminiInlineData {
                    mime_type: media_type,
                    data: base64,
                }),
                ..Default::default()
            },
            crate::attachment::WirePart::Pdf { base64, .. } => GeminiOutboundPart {
                inline_data: Some(GeminiInlineData {
                    mime_type: "application/pdf".into(),
                    data: base64,
                }),
                ..Default::default()
            },
        })
        .collect()
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct GeminiFunctionCall {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) args: Value,
}

#[derive(Serialize, Debug)]
pub(crate) struct GeminiFunctionResponse {
    pub(crate) name: String,
    /// The wire requires a JSON *object*, not a bare string — tool output is
    /// wrapped as `{"output": …}` / `{"error": …}` per Google's own
    /// function-calling convention.
    pub(crate) response: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiToolDecls {
    pub(crate) function_declarations: Vec<GeminiFunctionDecl>,
}

#[derive(Serialize)]
pub(crate) struct GeminiFunctionDecl {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    /// Sampling overrides from `CompletionRequest.params` — the struct-level
    /// `rename_all` gives them the REST API's camelCase names (`topP`,
    /// `topK`, `frequencyPenalty`, `presencePenalty`), matching
    /// `max_output_tokens`/`maxOutputTokens` above. Each is skipped when
    /// `None` so a request without overrides serializes byte-identical to
    /// before (the prompt-cache contract).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking_config: Option<GeminiThinkingConfig>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiThinkingConfig {
    pub(crate) thinking_level: &'static str,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiStreamChunk {
    #[serde(default)]
    pub(crate) candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    pub(crate) usage_metadata: Option<GeminiUsageMetadata>,
    /// An in-band error frame (`data: {"error": {...}}`). Google can emit one
    /// mid-stream after a 200 status; without modelling it the frame would
    /// deserialize into an otherwise-empty chunk and be silently dropped,
    /// ending the turn as a truncated success.
    #[serde(default)]
    pub(crate) error: Option<GoogleApiError>,
}

/// The `error` object Google returns both in a non-2xx body and in a
/// mid-stream SSE error frame. Only the fields this adapter classifies on are
/// modelled; unknown fields are ignored.
#[derive(Deserialize, Debug)]
pub(crate) struct GoogleApiError {
    #[serde(default)]
    pub(crate) code: u16,
    #[serde(default)]
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) status: String,
}

impl GoogleApiError {
    /// Classify a mid-stream error frame. 5xx / `UNAVAILABLE` / `INTERNAL` are
    /// transient (retryable `Transport`); `RESOURCE_EXHAUSTED` / 429 map to
    /// `RateLimited`; everything else is `Terminal` — the same posture as
    /// [`classify_google_error`] for non-2xx statuses.
    fn into_provider_error(self, label: &str) -> ProviderError {
        let msg = format!("{label} stream error [{}]: {}", self.status, self.message);
        if self.code >= 500 || self.status == "UNAVAILABLE" || self.status == "INTERNAL" {
            ProviderError::Transport(msg)
        } else if self.code == 429 || self.status == "RESOURCE_EXHAUSTED" {
            ProviderError::RateLimited {
                message: msg,
                retry_after_ms: None,
            }
        } else {
            ProviderError::Terminal(msg)
        }
    }
}

#[derive(Deserialize, Debug)]
pub(crate) struct GeminiCandidate {
    #[serde(default)]
    pub(crate) content: Option<GeminiCandidateContent>,
    /// Why generation ended (`STOP`, `MAX_TOKENS`, `SAFETY`, …) — reported
    /// on the final chunk's candidate; absent on interim chunks.
    #[serde(default, rename = "finishReason")]
    pub(crate) finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct GeminiCandidateContent {
    #[serde(default)]
    pub(crate) parts: Vec<GeminiInboundPart>,
}

/// One streamed content part. `thought: true` marks a thought-summary text
/// part (model reasoning surfaced for display) — never aggregated into the
/// user-visible answer text.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiInboundPart {
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) thought: bool,
    #[serde(default)]
    pub(crate) function_call: Option<GeminiFunctionCall>,
    #[serde(default)]
    pub(crate) thought_signature: Option<String>,
}

/// Cumulative usage — each chunk reports the running totals, so the last
/// assignment wins. `candidates_token_count` excludes thinking tokens; the
/// engine's one `output_tokens` figure includes them (they are billed
/// output), per the "normalization lives in the adapter" rule
///
#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiUsageMetadata {
    #[serde(default)]
    pub(crate) prompt_token_count: u64,
    #[serde(default)]
    pub(crate) candidates_token_count: u64,
    #[serde(default)]
    pub(crate) thoughts_token_count: u64,
    #[serde(default)]
    pub(crate) cached_content_token_count: u64,
}

/// Map the engine's one `ReasoningEffort` enum to Gemini's
/// `thinkingConfig.thinkingLevel`. Audited against the vendor docs (2026-07):
/// `thinkingLevel`'s accepted set is model-dependent — `gemini-3-pro` documents
/// `low`/`high`, while some `gemini-3.x` flash models added `minimal`/`medium`.
/// The adapter maps to the portable `low`/`high` pair that every current Gemini
/// model accepts, so a request never 400s on a level the routed model lacks —
/// the same "never send a value the model rejects" collapse posture as
/// `openai.rs::map_reasoning_effort`. (`effort_levels` offers only low/high for
/// the Gemini dialect, so this collapse is unreachable from the picker anyway.)
pub(crate) fn map_thinking_level(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium
        | ReasoningEffort::High
        | ReasoningEffort::Xhigh
        | ReasoningEffort::Max => "high",
    }
}

/// Split a minted call id back into `(wire ordinal, thought signature)` —
/// the inverse of what [`aggregate_gemini_stream`] minted.
fn split_call_id(call_id: &str) -> (&str, Option<&str>) {
    match call_id.split_once(SIGNATURE_SEPARATOR) {
        Some((ordinal, signature)) if !signature.is_empty() => (ordinal, Some(signature)),
        _ => (call_id, None),
    }
}

/// Translate the engine's one message list into Gemini `contents` +
/// `systemInstruction`. Dialect rules (`gemini-functions`):
/// - system turns hoist into `systemInstruction` (joined, like `openai.rs`)
/// - assistant → `role: "model"`, its tool calls become `functionCall`
///   parts (with any Gemini 3 thought signature restored — see
///   [`SIGNATURE_SEPARATOR`])
/// - tool results → a `role: "user"` content whose parts are
///   `functionResponse` objects. Gemini correlates a response to its call by
///   function *name* (there are no wire call ids), so the name is recovered
///   from the assistant `tool_calls` earlier in this same message list that
///   minted the `call_id`.
pub(crate) fn to_gemini_request_parts(
    messages: &[CompletionMessage],
) -> (Option<GeminiSystemInstruction>, Vec<GeminiContent>) {
    let mut system: Vec<String> = Vec::new();
    let mut contents: Vec<GeminiContent> = Vec::new();
    // call_id -> function name, harvested from assistant turns in order so
    // a later Tool message can name the call it answers.
    let mut call_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for message in messages {
        match message.role {
            MessageRole::System => system.push(message.content.clone()),
            MessageRole::User => {
                let mut parts = attachment_parts(message);
                if !message.content.is_empty() || parts.is_empty() {
                    parts.push(GeminiOutboundPart {
                        text: Some(message.content.clone()),
                        ..Default::default()
                    });
                }
                contents.push(GeminiContent {
                    role: "user",
                    parts,
                });
            }
            MessageRole::Assistant => {
                let mut parts = Vec::new();
                if !message.content.is_empty() {
                    parts.push(GeminiOutboundPart {
                        text: Some(message.content.clone()),
                        ..Default::default()
                    });
                }
                for call in &message.tool_calls {
                    call_names.insert(call.call_id.clone(), call.name.clone());
                    let (_, signature) = split_call_id(&call.call_id);
                    parts.push(GeminiOutboundPart {
                        function_call: Some(GeminiFunctionCall {
                            name: call.name.clone(),
                            args: call.input.clone(),
                        }),
                        thought_signature: signature.map(str::to_string),
                        ..Default::default()
                    });
                }
                if !parts.is_empty() {
                    contents.push(GeminiContent {
                        role: "model",
                        parts,
                    });
                }
            }
            MessageRole::Tool => {
                let parts: Vec<GeminiOutboundPart> = message
                    .tool_results
                    .iter()
                    .map(|result| {
                        let name = call_names
                            .get(&result.call_id)
                            .cloned()
                            .unwrap_or_else(|| result.call_id.clone());
                        let response = match &result.output {
                            stella_protocol::ToolOutput::Ok { content } => {
                                serde_json::json!({ "output": content })
                            }
                            stella_protocol::ToolOutput::Error { message } => {
                                serde_json::json!({ "error": message })
                            }
                        };
                        GeminiOutboundPart {
                            function_response: Some(GeminiFunctionResponse { name, response }),
                            ..Default::default()
                        }
                    })
                    .collect();
                if !parts.is_empty() {
                    contents.push(GeminiContent {
                        role: "user",
                        parts,
                    });
                }
            }
        }
    }

    let system_instruction = if system.is_empty() {
        None
    } else {
        Some(GeminiSystemInstruction {
            parts: vec![GeminiTextPart {
                text: system.join("\n\n"),
            }],
        })
    };
    (system_instruction, contents)
}

pub(crate) fn to_gemini_tools(tools: &[stella_protocol::tool::ToolSchema]) -> Vec<GeminiToolDecls> {
    if tools.is_empty() {
        return Vec::new();
    }
    vec![GeminiToolDecls {
        function_declarations: tools
            .iter()
            .map(|tool| GeminiFunctionDecl {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            })
            .collect(),
    }]
}

pub(crate) fn build_generation_config(req: &CompletionRequest) -> Option<GeminiGenerationConfig> {
    let params = req.params.unwrap_or_default();
    // The early-out considers EVERY field that could set a key below: a
    // request with no overrides at all must omit `generationConfig` entirely
    // (byte-stable with the pre-params wire), but any single override —
    // including the new params and the reasoning switch — must produce it.
    // Only the params this dialect actually forwards count; a request
    // carrying nothing but verbosity/service_tier (which Gemini can't
    // express) must not degrade to an empty `"generationConfig": {}`.
    if req.max_output_tokens.is_none()
        && req.temperature.is_none()
        && req.effort.is_none()
        && req.reasoning.is_none()
        && params.top_p.is_none()
        && params.top_k.is_none()
        && params.seed.is_none()
        && params.frequency_penalty.is_none()
        && params.presence_penalty.is_none()
    {
        return None;
    }
    // Thinking level: an explicit `reasoning: Some(false)` maps to "low" —
    // Gemini 3 cannot fully disable thinking (there is no "off"), so the
    // lowest level is the closest honest expression of "suppress thinking".
    // It wins over a pinned effort for the same reason an explicit off wins
    // in the other adapters. A bare `Some(true)` with no effort picks
    // "high" (Gemini 3 has no medium tier — see [`map_thinking_level`]);
    // otherwise a pinned effort maps as it always has, and (None, None)
    // sends no thinkingConfig (provider default).
    let thinking_level = match (req.reasoning, req.effort) {
        (Some(false), _) => Some("low"),
        (_, Some(effort)) => Some(map_thinking_level(effort)),
        (Some(true), None) => Some("high"),
        (None, None) => None,
    };
    Some(GeminiGenerationConfig {
        max_output_tokens: req.max_output_tokens,
        temperature: req.temperature,
        top_p: params.top_p,
        top_k: params.top_k,
        seed: params.seed,
        frequency_penalty: params.frequency_penalty,
        presence_penalty: params.presence_penalty,
        thinking_config: thinking_level
            .map(|thinking_level| GeminiThinkingConfig { thinking_level }),
    })
}

/// Classify a non-success generateContent status. Shared with `vertex.rs`
/// (identical error envelope); `label` names the actual surface in the
/// message so a Vertex failure never reads as a Gemini one.
pub(crate) async fn classify_google_error(
    label: &str,
    response: reqwest::Response,
    model: &str,
) -> ProviderError {
    let status = response.status();
    let retry_after_ms = crate::http::parse_retry_after_ms(response.headers());
    let body = response.text().await.unwrap_or_default();

    // Vendor pre-check ahead of the shared ladder: Google reports an invalid
    // API key as HTTP 400 with reason API_KEY_INVALID, not a 401 — surface
    // it as the auth failure it is so the user is told to fix the key rather
    // than shown a generic terminal error (and so the step-driver never
    // retries it).
    if status == reqwest::StatusCode::BAD_REQUEST && body.contains("API_KEY_INVALID") {
        return ProviderError::Auth(format!("{label} rejected the API key: {body}"));
    }
    crate::http::classify_http_status(label, status, retry_after_ms, &body, model)
}

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> &str {
        "gemini"
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
            .post(format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                self.base_url, self.model
            ))
            .header("x-goog-api-key", self.api_key.reveal())
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !response.status().is_success() {
            return Err(classify_google_error("Gemini", response, &self.model).await);
        }

        let (text, tool_calls, usage, finish_reason) =
            aggregate_gemini_stream("Gemini", response).await?;
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

/// Aggregate a `streamGenerateContent?alt=sse` response. Unlike the OpenAI
/// dialects there is no fragment reassembly: each `functionCall` part
/// arrives whole (args already a JSON object), and text parts are plain
/// deltas. Thought-summary parts (`thought: true`) are excluded from the
/// answer text; a part's `thoughtSignature` is preserved by riding inside
/// the minted call id (see [`SIGNATURE_SEPARATOR`]).
pub(crate) async fn aggregate_gemini_stream(
    label: &str,
    response: reqwest::Response,
) -> Result<(String, Vec<ToolCall>, CompletionUsage, Option<FinishReason>), ProviderError> {
    let mut decoder = SseDecoder::new();
    let mut text = String::new();
    let mut usage = CompletionUsage::default();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    // Reported on the final chunk's candidate; last assignment wins.
    let mut finish_raw: Option<String> = None;
    let mut stream = response.bytes_stream();

    // `next_with_timeout` bounds each read by `STREAM_IDLE_TIMEOUT` (a silent
    // stream surfaces as a retryable Transport error, not an unbounded hang)
    // and `push_bytes` reassembles multi-byte UTF-8 characters split across
    // chunk boundaries — decoding each chunk in isolation would spuriously
    // abort a CJK/emoji stream with `Malformed`.
    while let Some(chunk) = http::next_with_timeout(&mut stream, http::STREAM_IDLE_TIMEOUT).await? {
        decoder
            .push_bytes(&chunk)
            .map_err(|e| ProviderError::Malformed(e.to_string()))?;
        for event in decoder.poll() {
            let data = event.data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let parsed: GeminiStreamChunk = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue, // tolerate keep-alive/ping frames
            };
            if let Some(err) = parsed.error {
                return Err(err.into_provider_error(label));
            }
            if let Some(u) = parsed.usage_metadata {
                usage.input_tokens = u.prompt_token_count;
                usage.output_tokens = u.candidates_token_count + u.thoughts_token_count;
                usage.cached_input_tokens = u.cached_content_token_count;
            }
            for candidate in parsed.candidates {
                if let Some(reason) = candidate.finish_reason {
                    finish_raw = Some(reason);
                }
                let Some(content) = candidate.content else {
                    continue;
                };
                for part in content.parts {
                    if let Some(call) = part.function_call {
                        let ordinal = tool_calls.len();
                        let call_id = match &part.thought_signature {
                            Some(sig) => format!("call_{ordinal}{SIGNATURE_SEPARATOR}{sig}"),
                            None => format!("call_{ordinal}"),
                        };
                        // A no-argument call omits `args` on the wire, which
                        // deserializes to `Value::Null` (the field is
                        // `#[serde(default)]`). That is an empty object, not
                        // the malformed-call sentinel — a downstream tool
                        // deserializing its input as an object must not be
                        // handed `null`, and `driver.rs::execute_with_repair`
                        // must not mistake a valid no-arg call for broken JSON.
                        let input = if call.args.is_null() {
                            serde_json::json!({})
                        } else {
                            call.args
                        };
                        tool_calls.push(ToolCall {
                            call_id,
                            name: call.name,
                            input,
                        });
                    } else if let Some(t) = part.text
                        && !part.thought
                    {
                        text.push_str(&t);
                    }
                }
            }
        }
    }

    let finish_reason = map_gemini_finish_reason(finish_raw.as_deref(), !tool_calls.is_empty());
    Ok((text, tool_calls, usage, finish_reason))
}

/// Normalize Gemini's `finishReason` vocabulary onto the provider-neutral
/// [`FinishReason`]. Gemini reports `STOP` even for function-calling turns,
/// so tool presence disambiguates. Unknown values stay `None` per the
/// `CompletionResult` contract.
fn map_gemini_finish_reason(raw: Option<&str>, has_tool_calls: bool) -> Option<FinishReason> {
    match raw? {
        "STOP" if has_tool_calls => Some(FinishReason::ToolCalls),
        "STOP" => Some(FinishReason::Stop),
        "MAX_TOKENS" => Some(FinishReason::Length),
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => {
            Some(FinishReason::ContentFilter)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::tool::ToolSchema;
    use stella_protocol::{ToolOutput, ToolResult};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn user_attachments_map_to_inline_data_parts_of_every_kind() {
        use stella_protocol::{Attachment, AttachmentSource};
        let att = |name: &str, mime: &str, b64: &str| Attachment {
            name: name.into(),
            media_type: mime.into(),
            byte_len: 3,
            source: AttachmentSource::Data { base64: b64.into() },
        };
        let messages = vec![CompletionMessage::user_with_attachments(
            "look",
            vec![
                att("a.png", "image/png", "aW1n"),
                att("b.pdf", "application/pdf", "cGRm"),
                att("c.mp3", "audio/mpeg", "YXVk"),
                att("d.mp4", "video/mp4", "dmlk"),
            ],
        )];
        let (_, contents) = to_gemini_request_parts(&messages);
        assert_eq!(contents.len(), 1);
        let json = serde_json::to_value(&contents[0]).unwrap();
        let parts = json["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 5, "{json}");
        for (idx, mime) in [
            (0, "image/png"),
            (1, "application/pdf"),
            (2, "audio/mpeg"),
            (3, "video/mp4"),
        ] {
            assert_eq!(parts[idx]["inlineData"]["mimeType"], mime, "{json}");
            assert!(parts[idx]["inlineData"]["data"].is_string());
        }
        assert_eq!(parts[4]["text"], "look");
    }

    #[test]
    fn to_gemini_request_parts_hoists_system_and_maps_roles() {
        let messages = vec![
            CompletionMessage::system("You are a coding agent."),
            CompletionMessage::user("Fix the bug."),
        ];
        let (system, contents) = to_gemini_request_parts(&messages);
        assert_eq!(
            system.unwrap().parts[0].text,
            "You are a coding agent.".to_string()
        );
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
    }

    #[test]
    fn tool_results_become_function_response_parts_named_via_the_calling_turn() {
        // Gemini has no wire call ids: a functionResponse names the function
        // it answers. The adapter must recover that name from the assistant
        // turn that minted the call_id.
        let messages = vec![
            CompletionMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    call_id: "call_0".into(),
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
                    call_id: "call_0".into(),
                    output: ToolOutput::Ok {
                        content: "fn main(){}".into(),
                    },
                }],
                attachments: Vec::new(),
            },
        ];
        let (_, contents) = to_gemini_request_parts(&messages);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].role, "model");
        let call = contents[0].parts[0].function_call.as_ref().unwrap();
        assert_eq!(call.name, "read_file");
        assert_eq!(contents[1].role, "user");
        let response = contents[1].parts[0].function_response.as_ref().unwrap();
        assert_eq!(response.name, "read_file");
        assert_eq!(
            response.response,
            serde_json::json!({"output": "fn main(){}"})
        );
    }

    #[test]
    fn error_results_are_framed_as_error_objects() {
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
        let (_, contents) = to_gemini_request_parts(&messages);
        let response = contents[0].parts[0].function_response.as_ref().unwrap();
        assert_eq!(
            response.response,
            serde_json::json!({"error": "no such file"})
        );
    }

    #[test]
    fn thought_signatures_round_trip_through_the_minted_call_id() {
        // Gemini 3 attaches a thoughtSignature to a functionCall part and
        // requires it echoed back on the matching history part. The engine's
        // ToolCall has no slot for it, so it rides inside the minted call_id
        // — this test pins the full mint → history-replay round trip.
        let messages = vec![CompletionMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                call_id: "call_0#c2lnbmF0dXJl".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
            tool_results: vec![],
            attachments: Vec::new(),
        }];
        let (_, contents) = to_gemini_request_parts(&messages);
        let part = &contents[0].parts[0];
        assert_eq!(part.thought_signature.as_deref(), Some("c2lnbmF0dXJl"));
        let call = part.function_call.as_ref().unwrap();
        assert_eq!(call.name, "bash");
    }

    #[test]
    fn thinking_level_maps_low_directly_and_everything_else_to_high() {
        assert_eq!(map_thinking_level(ReasoningEffort::Low), "low");
        assert_eq!(map_thinking_level(ReasoningEffort::Medium), "high");
        assert_eq!(map_thinking_level(ReasoningEffort::High), "high");
        assert_eq!(map_thinking_level(ReasoningEffort::Xhigh), "high");
        assert_eq!(map_thinking_level(ReasoningEffort::Max), "high");
    }

    #[tokio::test]
    async fn complete_streams_and_aggregates_text_excluding_thought_parts() {
        let server = MockServer::start().await;
        let sse_body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"planning...\",\"thought\":true}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo!\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":3,\"thoughtsTokenCount\":5,\"cachedContentTokenCount\":2}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/models/gemini-3-pro:streamGenerateContent"))
            .and(query_param("alt", "sse"))
            .and(header("x-goog-api-key", "test-gemini-key"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = GeminiProvider::new(ApiKey::new("test-gemini-key"), "gemini-3-pro")
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
        assert_eq!(result.usage.input_tokens, 10);
        // candidates (3) + thoughts (5): thinking tokens are billed output.
        assert_eq!(result.usage.output_tokens, 8);
        assert_eq!(result.usage.cached_input_tokens, 2);
        assert_eq!(result.model, "gemini-3-pro");
    }

    #[tokio::test]
    async fn complete_mints_call_ids_and_captures_thought_signatures() {
        let server = MockServer::start().await;
        // Two parallel functionCall parts in one turn — Gemini sends args as
        // complete JSON objects (no fragment reassembly) and a Gemini 3
        // thoughtSignature on the first part of the group.
        let sse_body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[",
            "{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"src/lib.rs\"}},\"thoughtSignature\":\"c2ln\"},",
            "{\"functionCall\":{\"name\":\"bash\",\"args\":{\"command\":\"ls\"}}}",
            "]}}],\"usageMetadata\":{\"promptTokenCount\":20,\"candidatesTokenCount\":12}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/models/gemini-3-pro:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = GeminiProvider::new(ApiKey::new("test-key"), "gemini-3-pro")
            .with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("read src/lib.rs and list files")],
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

        let result = provider.complete(req).await.expect("should succeed");
        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(result.tool_calls[0].call_id, "call_0#c2ln");
        assert_eq!(result.tool_calls[0].name, "read_file");
        assert_eq!(
            result.tool_calls[0].input,
            serde_json::json!({"path": "src/lib.rs"})
        );
        assert_eq!(result.tool_calls[1].call_id, "call_1");
        assert_eq!(result.tool_calls[1].name, "bash");
    }

    #[tokio::test]
    async fn complete_normalizes_a_no_arg_call_to_an_empty_object_not_null() {
        let server = MockServer::start().await;
        // A no-argument Gemini/Vertex tool call omits `args` on the wire, so
        // the `#[serde(default)]` field deserializes to `Value::Null`. It must
        // surface as an empty object, not null — `Value::Null` is the
        // malformed-call sentinel `driver.rs::execute_with_repair` checks, so a
        // valid no-arg call reported as null would be wrongly "repaired".
        let sse_body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[",
            "{\"functionCall\":{\"name\":\"now\"}}",
            "]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2}}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/models/gemini-3-pro:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = GeminiProvider::new(ApiKey::new("test-key"), "gemini-3-pro")
            .with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("what time is it")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![ToolSchema {
                name: "now".into(),
                description: "Current time".into(),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: false,
            }],
            reasoning: None,
            params: None,
        };

        let result = provider.complete(req).await.expect("should succeed");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "now");
        assert_eq!(result.tool_calls[0].input, serde_json::json!({}));
        assert!(!result.tool_calls[0].input.is_null());
    }

    #[tokio::test]
    async fn complete_maps_403_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let provider =
            GeminiProvider::new(ApiKey::new("bad-key"), "gemini-3-pro").with_base_url(server.uri());

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
        assert!(matches!(err, ProviderError::Auth(_)));
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn complete_maps_400_api_key_invalid_to_auth_not_terminal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "{\"error\":{\"code\":400,\"status\":\"INVALID_ARGUMENT\",\"details\":[{\"reason\":\"API_KEY_INVALID\"}]}}",
            ))
            .mount(&server)
            .await;

        let provider =
            GeminiProvider::new(ApiKey::new("bad-key"), "gemini-3-pro").with_base_url(server.uri());

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
        assert!(matches!(err, ProviderError::Auth(_)));
    }

    #[tokio::test]
    async fn complete_maps_429_to_rate_limited_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "3")
                    .set_body_string("quota exceeded"),
            )
            .mount(&server)
            .await;

        let provider =
            GeminiProvider::new(ApiKey::new("key"), "gemini-3-pro").with_base_url(server.uri());

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
        assert!(err.is_retryable());
        match err {
            ProviderError::RateLimited { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(3000));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    /// A bare request for config-shape tests; overrides are patched in.
    fn bare_request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        }
    }

    #[test]
    fn generation_config_forwards_params_in_camel_case() {
        use stella_protocol::GenerationParams;
        let req = CompletionRequest {
            params: Some(GenerationParams {
                top_p: Some(0.9),
                top_k: Some(40),
                frequency_penalty: Some(0.5),
                presence_penalty: Some(0.25),
                seed: Some(7),
                // No generateContent slot — silently dropped.
                repetition_penalty: Some(1.1),
                verbosity: Some(stella_protocol::Verbosity::High),
                service_tier: Some(stella_protocol::ServiceTier::Priority),
            }),
            ..bare_request()
        };
        let config = build_generation_config(&req).expect("params force a config");
        // Assert on the serialized string (the exact bytes reqwest sends),
        // not a `to_value` round-trip — `to_value` widens f32 through f64
        // and would perturb 0.9 into 0.90000003….
        let body = serde_json::to_string(&config).unwrap();
        // The REST API is camelCase — snake_case keys here would be silently
        // ignored by the server, which is why the casing is pinned.
        assert!(body.contains("\"topP\":0.9"), "{body}");
        assert!(body.contains("\"topK\":40"), "{body}");
        assert!(body.contains("\"seed\":7"), "{body}");
        assert!(body.contains("\"frequencyPenalty\":0.5"), "{body}");
        assert!(body.contains("\"presencePenalty\":0.25"), "{body}");
        for dropped in ["repetition", "verbosity", "serviceTier", "top_p"] {
            assert!(!body.contains(dropped), "`{dropped}` leaked into: {body}");
        }
    }

    /// The byte-stability contract: no overrides at all — including a params
    /// struct carrying only fields this dialect can't express — must omit
    /// `generationConfig` entirely, exactly the pre-params wire shape.
    #[test]
    fn generation_config_is_omitted_without_any_forwardable_override() {
        use stella_protocol::GenerationParams;
        assert!(build_generation_config(&bare_request()).is_none());
        let unforwardable = CompletionRequest {
            params: Some(GenerationParams {
                verbosity: Some(stella_protocol::Verbosity::Low),
                service_tier: Some(stella_protocol::ServiceTier::Flex),
                ..Default::default()
            }),
            ..bare_request()
        };
        assert!(
            build_generation_config(&unforwardable).is_none(),
            "verbosity/service_tier alone must not produce an empty config"
        );
    }

    /// `reasoning: Some(false)` → thinkingLevel "low": Gemini 3 cannot fully
    /// disable thinking, so the lowest level is the closest honest mapping —
    /// and the explicit off wins over a pinned effort. A bare `Some(true)`
    /// picks "high" (there is no medium tier).
    #[test]
    fn reasoning_switch_maps_to_thinking_levels() {
        let off = CompletionRequest {
            reasoning: Some(false),
            effort: Some(ReasoningEffort::Max),
            ..bare_request()
        };
        let config = build_generation_config(&off).expect("reasoning forces a config");
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["thinkingConfig"]["thinkingLevel"], "low", "{json}");

        let on = CompletionRequest {
            reasoning: Some(true),
            ..bare_request()
        };
        let config = build_generation_config(&on).expect("reasoning forces a config");
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["thinkingConfig"]["thinkingLevel"], "high", "{json}");

        // A pinned effort with no reasoning switch maps as it always has.
        let effort_only = CompletionRequest {
            effort: Some(ReasoningEffort::Low),
            ..bare_request()
        };
        let config = build_generation_config(&effort_only).expect("effort forces a config");
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["thinkingConfig"]["thinkingLevel"], "low", "{json}");
    }

    /// End-to-end: the config rides the request body to the wire.
    #[tokio::test]
    async fn complete_sends_generation_config_params_on_the_wire() {
        use stella_protocol::GenerationParams;
        let server = MockServer::start().await;
        let sse_body = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]}}]}\n\n";
        Mock::given(method("POST"))
            .and(path("/models/gemini-3-pro:streamGenerateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = GeminiProvider::new(ApiKey::new("test-key"), "gemini-3-pro")
            .with_base_url(server.uri());
        provider
            .complete(CompletionRequest {
                params: Some(GenerationParams {
                    top_p: Some(0.9),
                    top_k: Some(40),
                    ..Default::default()
                }),
                reasoning: Some(false),
                ..bare_request()
            })
            .await
            .expect("should succeed");

        let requests = server.received_requests().await.expect("recorded requests");
        let body = String::from_utf8_lossy(&requests[0].body);
        assert!(body.contains("\"topP\":0.9"), "{body}");
        assert!(body.contains("\"topK\":40"), "{body}");
        assert!(
            body.contains("\"thinkingConfig\":{\"thinkingLevel\":\"low\"}"),
            "{body}"
        );
    }
}
