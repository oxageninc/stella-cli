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
//! CLI can construct declares a [`CachePosture`] row here, and tests fail
//! when a row is missing (`stella-cli` config tests), duplicated, or names
//! a witness test that no longer exists in the adapter sources.
//!
//! **The law for new providers:** adding a provider id means adding a row
//! here in the same PR, and the row must name a witness test proving the
//! posture on the wire — the opt-in marker is sent, or the provider's hit
//! telemetry is parsed into `CompletionUsage`. `NotApplicable` is allowed
//! only with a reason a reviewer can check. The same pattern applies to any
//! future per-provider feature divergence (reasoning controls, attachment
//! dialects): when one provider needs something the others don't, record
//! the axis as a matrix, don't leave it as adapter folklore.

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
                        routes, ignored by implicit-cache upstreams",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every witness named in the matrix must exist as a test function in
    /// the adapter sources — a row whose proof rotted (test renamed or
    /// deleted) fails here, not as a production surprise.
    #[test]
    fn every_witness_test_exists_in_the_adapter_sources() {
        let sources = [
            include_str!("anthropic/tests.rs"),
            include_str!("bedrock.rs"),
            include_str!("openai.rs"),
            include_str!("gemini.rs"),
            include_str!("vertex.rs"),
            include_str!("zai/tests.rs"),
        ];
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

    #[test]
    fn provider_ids_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for (id, _) in CACHE_POSTURE {
            assert!(seen.insert(id), "duplicate cache-posture row for `{id}`");
        }
    }

    #[test]
    fn lookup_finds_every_row_and_rejects_unknown_ids() {
        for (id, _) in CACHE_POSTURE {
            assert!(cache_posture(id).is_some());
        }
        assert!(cache_posture("no-such-provider").is_none());
    }
}
