//! The model-completion request/response envelope — the same shape for
//! every provider adapter (Z.ai, Anthropic, OpenAI, Gemini, xAI, Bedrock,
//! Vertex, OpenRouter, local). Mirrors the TS runtime's
//! `apps/cli/src/runtime/types.ts` `CompletionRequest`/`CompletionResult`,
//! port map.

use serde::{Deserialize, Serialize};

use crate::attachment::Attachment;
use crate::tool::{ToolCall, ToolResult};

/// Who authored one message in the conversation. Tool results are
/// represented as a `Tool` message carrying the `tool_call_id` they answer,
/// so every dialect adapter has one place to translate role framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Reasoning effort forwarded to models with a thinking/extended-reasoning
/// mode. One enum, mapped per-adapter to the provider's own parameter name
/// ("reasoning_param").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// Response-detail level for providers with a verbosity parameter (OpenAI's
/// `text.verbosity`). Adapters whose wire has no equivalent ignore it — the
/// same never-fail contract as [`ReasoningEffort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verbosity {
    Low,
    Medium,
    High,
}

/// Provider service tier: `Priority` routes to faster paid-tier capacity,
/// `Flex` to cheaper capacity with slower response times. Only applied by
/// providers that support tiered service; others use their default tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Auto,
    Default,
    Flex,
    Priority,
}

/// Optional sampling/routing parameter overrides riding a
/// [`CompletionRequest`]. Every field is independently optional —
/// "include" semantics: `None` leaves the provider's own default in place,
/// `Some` puts the value on the wire. Each adapter forwards the subset its
/// dialect supports and silently drops the rest (a param the provider
/// can't express must never fail the request).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationParams {
    /// Nucleus sampling: cumulative-probability cutoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Limit sampling to the k highest-probability tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Penalize tokens by their frequency in the text so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    /// Penalize tokens that have appeared at all in the text so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    /// Multiplicative repetition penalty (>1 discourages, <1 encourages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repetition_penalty: Option<f32>,
    /// Random seed for deterministic outputs, where supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

/// One chat message handed to a provider, including any tool calls the
/// assistant made or tool results being reported back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionMessage {
    pub role: MessageRole,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
    /// Multimodal inputs (images, documents, audio, video) accompanying a
    /// user message. `serde(default)` + skip-when-empty so envelopes
    /// serialized before this field existed still parse and text-only
    /// messages serialize byte-for-byte as they always have (the prompt-cache
    /// stability contract).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

impl CompletionMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            attachments: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            attachments: Vec::new(),
        }
    }

    /// A user message carrying multimodal attachments alongside its text.
    pub fn user_with_attachments(content: impl Into<String>, attachments: Vec<Attachment>) -> Self {
        Self {
            attachments,
            ..Self::user(content)
        }
    }

    /// An assistant text message with no tool calls — e.g. a final answer
    /// replayed into a transcript for post-turn reflection.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            attachments: Vec::new(),
        }
    }
}

/// A completion request — the same shape regardless of which provider
/// adapter ultimately serves it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub messages: Vec<CompletionMessage>,
    /// Upper bound on generated tokens. `None` uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature. `None` uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Reasoning effort for models that support a thinking mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// Whether the model's thinking/extended-reasoning mode is enabled at
    /// all. `Some(true)` asks the adapter to turn thinking on (at
    /// `effort`'s level, or the adapter's default level when `effort` is
    /// `None`); `Some(false)` asks it to suppress thinking; `None` keeps
    /// the provider's default behavior (exactly the pre-field wire shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// Optional sampling/routing overrides ([`GenerationParams`]) — each
    /// adapter forwards the subset its dialect supports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<GenerationParams>,
    /// Tool schemas the model may call, in the engine's one internal shape
    /// (`crate::tool::ToolSchema`); each adapter translates to its own
    /// dialect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<crate::tool::ToolSchema>,
}

/// Token accounting for a single completion, normalized across providers
/// into one envelope (the AI-SDK-v7
/// usage-shape-breakage lesson: normalization lives in the adapter, not the
/// caller).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletionUsage {
    /// The adapter observed the provider's authoritative usage-bearing
    /// terminal response. This is explicit because a legitimate call can
    /// report all zero counters, while a missing usage frame can accompany
    /// non-empty streamed text. Legacy envelopes fail closed.
    #[serde(default)]
    pub reported: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Tokens WRITTEN to the provider's prompt cache by this call
    /// (Anthropic `cache_creation_input_tokens`, Bedrock
    /// `cacheWriteInputTokens`). Unlike `cached_input_tokens` this is NOT a
    /// subset of `input_tokens` — providers report writes separately, and
    /// folding them into `input_tokens` would change cost accounting
    /// (`Pricing::cost_usd` carries no cache-write rate). 0 for providers
    /// that never report cache writes (the OpenAI-compatible dialects).
    /// `serde(default)` so envelopes serialized before this field existed
    /// still parse.
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl CompletionUsage {
    /// A reported terminal envelope whose counters are all legitimately zero.
    pub fn reported_zero() -> Self {
        Self {
            reported: true,
            ..Self::default()
        }
    }

    /// Whether the adapter proved this accounting envelope came from the
    /// provider's authoritative terminal usage frame.
    pub fn is_complete(&self) -> bool {
        self.reported
    }
}

/// Why the model stopped generating, normalized across providers. Lets the
/// engine tell a natural stop from a truncation (`Length`) so an empty or
/// cut-off turn is surfaced to the user instead of being recorded as a clean
/// completion (the "turn ends with no feedback" defect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural end of the response.
    Stop,
    /// Output was cut off at the token limit (OpenAI-compatible `length`).
    Length,
    /// The model stopped in order to make tool calls.
    ToolCalls,
    /// A provider content filter halted generation.
    ContentFilter,
}

/// The result of a completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResult {
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    pub usage: CompletionUsage,
    /// Concrete model id/slug that produced the result, resolved from the
    /// catalog — never a literal at the call site.
    pub model: String,
    /// Estimated provider cost in USD (0 for on-device/local).
    pub cost_usd: f64,
    /// Why generation stopped, when the adapter can determine it. `None` when
    /// the provider doesn't report it. `serde(default)` so envelopes
    /// serialized before this field existed still parse.
    #[serde(default)]
    pub finish_reason: Option<FinishReason>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_request_roundtrips_through_json() {
        let req = CompletionRequest {
            messages: vec![
                CompletionMessage::system("You are a coding agent."),
                CompletionMessage::user("Fix the failing test."),
            ],
            max_output_tokens: Some(4096),
            temperature: Some(0.2),
            effort: Some(ReasoningEffort::High),
            tools: vec![],
            reasoning: None,
            params: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: CompletionRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.messages.len(), 2);
        assert_eq!(back.effort, Some(ReasoningEffort::High));
        assert_eq!(back.max_output_tokens, Some(4096));
    }

    #[test]
    fn completion_result_roundtrips_and_defaults_empty_tool_calls() {
        let result = CompletionResult {
            text: "done".into(),
            tool_calls: vec![],
            usage: CompletionUsage {
                reported: true,
                input_tokens: 100,
                output_tokens: 20,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
            },
            model: "glm-5.2".into(),
            cost_usd: 0.0012,
            finish_reason: None,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(
            !json.contains("tool_calls"),
            "empty tool_calls must be omitted: {json}"
        );
        let back: CompletionResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.model, "glm-5.2");
        assert_eq!(back.usage.input_tokens, 100);
    }

    #[test]
    fn completion_usage_roundtrips_cache_write_tokens() {
        let usage = CompletionUsage {
            reported: true,
            input_tokens: 1_000,
            output_tokens: 50,
            cached_input_tokens: 400,
            cache_write_tokens: 600,
        };
        let json = serde_json::to_string(&usage).expect("serialize");
        assert!(json.contains("\"cache_write_tokens\":600"), "{json}");
        let back: CompletionUsage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, usage);
    }

    #[test]
    fn completion_usage_without_cache_write_tokens_still_parses() {
        // Backward compatibility: a usage envelope serialized before
        // `cache_write_tokens` existed must deserialize with the field
        // defaulting to 0 — the `serde(default)` contract.
        let legacy = r#"{"input_tokens":100,"output_tokens":20,"cached_input_tokens":30}"#;
        let back: CompletionUsage = serde_json::from_str(legacy).expect("deserialize");
        assert_eq!(back.cached_input_tokens, 30);
        assert_eq!(back.cache_write_tokens, 0);
    }

    #[test]
    fn completion_usage_completeness_is_explicit_not_inferred_from_token_values() {
        let reported_zero = CompletionUsage::reported_zero();
        assert!(reported_zero.is_complete());

        let unreported_nonzero = CompletionUsage {
            input_tokens: 10,
            output_tokens: 1,
            ..CompletionUsage::default()
        };
        assert!(!unreported_nonzero.is_complete());
    }

    #[test]
    fn message_role_serializes_snake_case() {
        let json = serde_json::to_string(&MessageRole::Tool).unwrap();
        assert_eq!(json, "\"tool\"");
    }
}
