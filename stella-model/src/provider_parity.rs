//! Provider feature-parity matrix — the structural guard against
//! per-provider gotchas.
//!
//! Born from a real defect: Anthropic models routed through OpenRouter ran
//! with NO prompt caching (CACHE 0% across a $2+ session) because
//! Anthropic's cache is explicit opt-in while most providers' caches are
//! implicit — a per-provider divergence nothing enforced. DeepSeek had the
//! sibling defect the same week: its native cache-hit field was silently
//! dropped because it spells the telemetry differently. This module makes
//! that class of gap structural instead of tribal: every provider id the
//! CLI can construct declares a row on each axis here, and tests fail when a
//! row is missing (`stella-cli` config tests), duplicated, or names a witness
//! test that no longer exists in the adapter sources.
//!
//! Two axes are guarded today, both born from the same shape of silent
//! per-provider divergence:
//! - [`CachePosture`] — how a provider's prompt cache is engaged and observed.
//! - [`ReasoningPosture`] — how a provider's reasoning/thinking budget is
//!   controlled on the wire. The reasoning axis has the sibling defect the
//!   cache axis had: only Z.ai (`thinking`) and OpenRouter (`reasoning`)
//!   honored a reasoning preference on the shared chat-completions adapter,
//!   so a pinned `effort` was *silently dropped* for xAI, DeepSeek, and local
//!   — the exact "nothing enforces the omission stays deliberate" gap.
//!
//! **The law for new providers:** adding a provider id means adding a row on
//! every axis here in the same PR, and a `Controllable`/`OptIn`/`Implicit`
//! row must name a witness test proving the posture on the wire — the opt-in
//! marker is sent, the hit telemetry is parsed into `CompletionUsage`, or the
//! reasoning control reaches the request body. The no-control variants
//! (`NotApplicable`, `Unsupported`, `FixedOn`, `FixedOff`) are allowed only
//! with a note a reviewer can check. The same pattern applies to any future
//! per-provider feature divergence (attachment dialects, tool schemas): when
//! one provider needs something the others don't, record the axis as a
//! matrix, don't leave it as adapter folklore.

/// How a provider's prompt cache is engaged and observed.
#[derive(Debug)]
pub enum CachePosture {
    /// The adapter must SEND an explicit opt-in marker or the provider
    /// caches nothing (Anthropic `cache_control`, Bedrock `cachePoint`,
    /// OpenRouter's request-root `cache_control` for Claude routes).
    OptIn {
        /// The wire mechanism, for humans reading the matrix.
        mechanism: &'static str,
        /// Name of the test function that proves the marker reaches the
        /// wire; checked for existence by this module's tests.
        witness: &'static str,
    },
    /// The provider caches implicitly — nothing to send, but the adapter
    /// must PARSE the provider's hit telemetry or cached tokens bill at the
    /// full input rate and the cache stat pins at zero.
    Implicit {
        /// Where the hits are reported on this provider's usage envelope.
        telemetry: &'static str,
        /// Name of the test function that proves the telemetry lands in
        /// `CompletionUsage`; checked for existence by this module's tests.
        witness: &'static str,
    },
    /// No billable prompt cache exists to opt into or meter.
    NotApplicable { reason: &'static str },
}

/// One row per provider id constructible by the CLI (`config.rs
/// PROVIDERS` + `LOCAL_PROVIDER`; the completeness check lives in
/// `stella-cli`'s config tests, which see both lists). Settings-defined
/// custom providers inherit the shared OpenAI-compatible adapter and its
/// `prompt_tokens_details` parsing — they need no row of their own.
pub static CACHE_POSTURE: &[(&str, CachePosture)] = &[
    (
        "anthropic",
        CachePosture::OptIn {
            mechanism: "Messages API cache_control breakpoints (system + conversation tail)",
            witness: "request_serializes_both_cache_breakpoints",
        },
    ),
    (
        "bedrock",
        CachePosture::OptIn {
            mechanism: "Converse cachePoint blocks, gated to supporting model families",
            witness: "complete_sends_cache_points_for_claude_models",
        },
    ),
    (
        "openrouter",
        CachePosture::OptIn {
            mechanism: "request-root cache_control {type: ephemeral} — required for Claude \
                        routes, ignored by implicit-cache upstreams — plus a session-stable \
                        top-level session_id that pins every turn of a session to the same \
                        upstream provider + cache shard (sticky routing)",
            witness: "openrouter_identity_sends_root_level_cache_control",
        },
    ),
    (
        "openai",
        CachePosture::Implicit {
            telemetry: "input_tokens_details.cached_tokens, plus session-stable \
                        prompt_cache_key cache-shard routing",
            witness: "complete_sends_a_session_stable_prompt_cache_key",
        },
    ),
    (
        "gemini",
        CachePosture::Implicit {
            telemetry: "usageMetadata.cachedContentTokenCount",
            witness: "complete_streams_and_aggregates_text_excluding_thought_parts",
        },
    ),
    (
        "vertex",
        CachePosture::Implicit {
            telemetry: "usageMetadata.cachedContentTokenCount (shared gemini aggregator)",
            witness: "complete_sends_a_bearer_token_to_the_project_scoped_path",
        },
    ),
    (
        "zai",
        CachePosture::Implicit {
            telemetry: "prompt_tokens_details.cached_tokens",
            witness: "complete_surfaces_cached_tokens_and_bills_them_at_the_cached_rate",
        },
    ),
    (
        "xai",
        CachePosture::Implicit {
            telemetry: "prompt_tokens_details.cached_tokens (shared chat-completions parse path)",
            witness: "complete_surfaces_cached_tokens_and_bills_them_at_the_cached_rate",
        },
    ),
    (
        "deepseek",
        CachePosture::Implicit {
            telemetry: "top-level prompt_cache_hit_tokens — DeepSeek's native spelling; \
                        it sends no prompt_tokens_details object",
            witness: "deepseek_native_cache_hit_tokens_surface_as_cached_input",
        },
    ),
    (
        "local",
        CachePosture::NotApplicable {
            reason: "local servers prefix-cache for latency, not price — there is no \
                     billed cache to opt into; OpenAI-shape cached_tokens still parse \
                     via the shared adapter when a server reports them",
        },
    ),
];

/// The declared cache posture for `provider_id`, or `None` for an id the
/// matrix doesn't know — which the `stella-cli` completeness test turns
/// into a hard failure for any seeded provider.
pub fn cache_posture(provider_id: &str) -> Option<&'static CachePosture> {
    CACHE_POSTURE
        .iter()
        .find(|(id, _)| *id == provider_id)
        .map(|(_, posture)| posture)
}

/// How a provider's reasoning / thinking budget is controlled from a
/// completion request. The sibling of [`CachePosture`]: reasoning had the
/// same per-provider divergence and no guard — a pinned `effort` reached
/// only Z.ai and OpenRouter and was silently dropped everywhere else.
#[derive(Debug)]
pub enum ReasoningPosture {
    /// The adapter translates the engine's `effort`/`reasoning` preference
    /// into this provider's native reasoning control and puts it on the wire
    /// (Anthropic thinking budget, OpenRouter `reasoning`, OpenAI/xAI
    /// `reasoning[_]effort`, Gemini `thinkingLevel`, GLM `thinking`).
    Controllable {
        /// The wire mechanism, for humans reading the matrix.
        mechanism: &'static str,
        /// Name of the test function that proves the control reaches the
        /// request body; checked for existence by this module's tests.
        witness: &'static str,
    },
    /// The provider always reasons and the depth cannot be pinned from the
    /// request. Declared for taxonomy completeness — no id in the current
    /// fleet is classified here (DeepSeek's always-on reasoner is filed under
    /// [`ReasoningPosture::Unsupported`] instead, so a dropped effort still
    /// surfaces a notice), but a future reasoning-only model with no dial
    /// belongs here rather than pretending it is controllable.
    FixedOn { note: &'static str },
    /// The provider has no reasoning mode at all for the models this id
    /// serves. Declared for taxonomy completeness (see [`FixedOn`]).
    ///
    /// [`FixedOn`]: ReasoningPosture::FixedOn
    FixedOff { note: &'static str },
    /// The shared adapter deliberately drops `effort`/`reasoning` for this id:
    /// there is no portable Chat Completions reasoning field and an unknown
    /// key risks a hard 400. Honest degradation, not a silent one — a pinned
    /// effort against an `Unsupported` provider surfaces a one-line transcript
    /// notice (`stella-cli` boot chrome) rather than vanishing.
    Unsupported { note: &'static str },
}

/// One reasoning row per provider id constructible by the CLI — same
/// completeness contract as [`CACHE_POSTURE`] (enforced by `stella-cli`'s
/// config tests, which see both `PROVIDERS` and `LOCAL_PROVIDER`).
/// Settings-defined custom providers inherit the shared OpenAI-compatible
/// adapter, which sends no reasoning field, so they behave as `Unsupported`
/// and need no row of their own.
pub static REASONING_POSTURE: &[(&str, ReasoningPosture)] = &[
    (
        "anthropic",
        ReasoningPosture::Controllable {
            mechanism: "extended-thinking budget (thinking.budget_tokens) + output_config.effort \
                        — all five effort tiers map to distinct budgets",
            witness: "reasoning_true_enables_thinking_raises_max_tokens_and_omits_temperature",
        },
    ),
    (
        "bedrock",
        ReasoningPosture::Unsupported {
            note: "Claude-on-Bedrock supports extended thinking via \
                   additionalModelRequestFields.reasoning_config, but the Converse adapter has no \
                   passthrough for it yet — a pinned effort is dropped (surfaced as a boot \
                   notice), not sent",
        },
    ),
    (
        "openrouter",
        ReasoningPosture::Controllable {
            mechanism: "normalized reasoning object ({effort} / {enabled}), translated by the \
                        gateway to whatever the routed upstream vendor calls it",
            witness: "openrouter_identity_maps_reasoning_to_the_gateway_object",
        },
    ),
    (
        "openai",
        ReasoningPosture::Controllable {
            mechanism: "Responses API reasoning.effort (low/medium/high; finer model-dependent \
                        tiers — minimal/xhigh/max — collapse to the universally-safe high)",
            witness: "reasoning_true_without_effort_defaults_to_medium",
        },
    ),
    (
        "gemini",
        ReasoningPosture::Controllable {
            mechanism: "thinkingConfig.thinkingLevel (low/high; medium/minimal exist only on some \
                        Gemini 3.x models, so the adapter maps to the portable low/high pair)",
            witness: "complete_sends_generation_config_params_on_the_wire",
        },
    ),
    (
        "vertex",
        ReasoningPosture::Controllable {
            mechanism: "thinkingConfig.thinkingLevel via the shared gemini generation-config \
                        builder",
            witness: "complete_sends_shared_generation_config_params_on_the_wire",
        },
    ),
    (
        "zai",
        ReasoningPosture::Controllable {
            mechanism: "GLM thinking object ({type: enabled|disabled}) — an on/off switch, so \
                        `reasoning` is honored but `effort` has no dial (effort_levels returns \
                        empty for zai)",
            witness: "zai_identity_maps_reasoning_to_glm_thinking_object",
        },
    ),
    (
        "xai",
        ReasoningPosture::Controllable {
            mechanism: "chat-completions top-level reasoning_effort (low/medium/high), gated to \
                        the xai identity on the shared adapter — and, within xai, skipped for the \
                        original grok-4, which reasons but 400s on the param (retiring 2026-08-15)",
            witness: "xai_identity_maps_effort_to_reasoning_effort",
        },
    ),
    (
        "deepseek",
        ReasoningPosture::Unsupported {
            note: "deepseek-reasoner reasons unconditionally and DeepSeek's chat-completions API \
                   exposes no request-level effort control; a pinned effort is dropped (surfaced \
                   as a boot notice) — the model reasons at its own fixed depth",
        },
    ),
    (
        "local",
        ReasoningPosture::Unsupported {
            note: "local OpenAI-compatible servers have no portable reasoning field; a pinned \
                   effort is dropped rather than guessed at — an unknown key risks a 400 on a \
                   server the user never opted into experimenting with",
        },
    ),
];

/// The declared reasoning posture for `provider_id`, or `None` for an id the
/// matrix doesn't know — which the `stella-cli` completeness test turns into a
/// hard failure for any seeded provider.
pub fn reasoning_posture(provider_id: &str) -> Option<&'static ReasoningPosture> {
    REASONING_POSTURE
        .iter()
        .find(|(id, _)| *id == provider_id)
        .map(|(_, posture)| posture)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter source files every witness (cache or reasoning) must live
    /// in. `include_str!` embeds them at compile time so a renamed/deleted
    /// witness fails the build's tests, not production.
    fn adapter_sources() -> [&'static str; 6] {
        [
            include_str!("anthropic/tests.rs"),
            include_str!("bedrock.rs"),
            include_str!("openai.rs"),
            include_str!("gemini.rs"),
            include_str!("vertex.rs"),
            include_str!("zai/tests.rs"),
        ]
    }

    /// Every witness named in the cache matrix must exist as a test function
    /// in the adapter sources — a row whose proof rotted (test renamed or
    /// deleted) fails here, not as a production surprise.
    #[test]
    fn every_witness_test_exists_in_the_adapter_sources() {
        let sources = adapter_sources();
        for (id, posture) in CACHE_POSTURE {
            let witness = match posture {
                CachePosture::OptIn { witness, .. } | CachePosture::Implicit { witness, .. } => {
                    witness
                }
                CachePosture::NotApplicable { .. } => continue,
            };
            let needle = format!("fn {witness}(");
            assert!(
                sources.iter().any(|source| source.contains(&needle)),
                "cache-posture witness for `{id}` not found in adapter sources: {witness}"
            );
        }
    }

    /// The reasoning-axis sibling: every `Controllable` row must name a test
    /// that exists in the adapter sources, proving the reasoning control
    /// reaches the wire. The no-control variants carry a note, not a witness.
    #[test]
    fn every_reasoning_witness_test_exists_in_the_adapter_sources() {
        let sources = adapter_sources();
        for (id, posture) in REASONING_POSTURE {
            let witness = match posture {
                ReasoningPosture::Controllable { witness, .. } => witness,
                ReasoningPosture::FixedOn { .. }
                | ReasoningPosture::FixedOff { .. }
                | ReasoningPosture::Unsupported { .. } => continue,
            };
            let needle = format!("fn {witness}(");
            assert!(
                sources.iter().any(|source| source.contains(&needle)),
                "reasoning-posture witness for `{id}` not found in adapter sources: {witness}"
            );
        }
    }

    #[test]
    fn provider_ids_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for (id, _) in CACHE_POSTURE {
            assert!(seen.insert(id), "duplicate cache-posture row for `{id}`");
        }
    }

    #[test]
    fn reasoning_provider_ids_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for (id, _) in REASONING_POSTURE {
            assert!(
                seen.insert(id),
                "duplicate reasoning-posture row for `{id}`"
            );
        }
    }

    /// Both axes must cover exactly the same set of provider ids — a provider
    /// present on one axis but not the other is a matrix hole.
    #[test]
    fn both_axes_cover_the_same_provider_ids() {
        let cache: std::collections::BTreeSet<_> =
            CACHE_POSTURE.iter().map(|(id, _)| *id).collect();
        let reasoning: std::collections::BTreeSet<_> =
            REASONING_POSTURE.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            cache, reasoning,
            "cache and reasoning matrices cover different provider ids"
        );
    }

    #[test]
    fn lookup_finds_every_row_and_rejects_unknown_ids() {
        for (id, _) in CACHE_POSTURE {
            assert!(cache_posture(id).is_some());
        }
        assert!(cache_posture("no-such-provider").is_none());
        for (id, _) in REASONING_POSTURE {
            assert!(reasoning_posture(id).is_some());
        }
        assert!(reasoning_posture("no-such-provider").is_none());
    }
}
