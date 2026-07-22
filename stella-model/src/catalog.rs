//! Model catalog. Binding rule:
//! **a slug not present
//! in the catalog is a hard, immediate, named error, never a silent
//! fallback** (the TS-era phantom `glm-5.2-turbo` slug and gateway
//! slug-drift lessons, L-M1/L-M2). The seed below covers every provider
//! `stella-cli/src/config.rs`'s `PROVIDERS` table can select — it is
//! the compile-time floor, always accepted. `stella models refresh` pulls
//! the live master list (models.dev) into the on-disk catalog
//! (`stella-store`'s model cards), and `stella-cli` installs the merged
//! result here via [`Catalog::install_runtime`] so every consumer —
//! adapters resolving pricing, the deck's model picker, the engine config —
//! sees one catalog through [`Catalog::current`].

use std::sync::{Arc, OnceLock, RwLock};

use stella_protocol::{CompletionUsage, ProviderError};

/// Per-model list pricing in USD per million tokens.
/// Seed values below are day-0 offline approximations of each
/// provider's published list price; `stella models refresh` overlays them
/// with live data (the latest model-card version's pricing configuration).
/// Cached input is billed at its own, cheaper rate — cached tokens are a
/// *subset* of `input_tokens` in the normalized [`CompletionUsage`]
/// envelope, so cost accounting must not double-charge them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pricing {
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
    pub cached_input_usd_per_mtok: f64,
}

impl Pricing {
    /// Estimated USD cost for one completion's normalized usage. Non-cached
    /// input (`input_tokens - cached_input_tokens`) is billed at the input
    /// rate, the cached remainder at the cached rate, and output at the
    /// output rate. Never panics and never goes negative — a provider that
    /// reports more cached than total input (shouldn't happen, but is not
    /// worth aborting a turn over) saturates to zero non-cached input.
    ///
    /// `cache_write_tokens` is deliberately NOT priced: the catalog carries
    /// no cache-write rate yet (providers bill writes at a premium over
    /// input — e.g. Anthropic 1.25x), and those tokens are reported outside
    /// `input_tokens`, so today they contribute $0 here. Adding a
    /// `cache_write_usd_per_mtok` column is the staged follow-up to
    /// issue #97 (the on-disk model-card versions already record the rate).
    pub fn cost_usd(&self, usage: &CompletionUsage) -> f64 {
        let cached = usage.cached_input_tokens.min(usage.input_tokens);
        let uncached_input = usage.input_tokens - cached;
        const PER_MTOK: f64 = 1_000_000.0;
        (uncached_input as f64 / PER_MTOK) * self.input_usd_per_mtok
            + (cached as f64 / PER_MTOK) * self.cached_input_usd_per_mtok
            + (usage.output_tokens as f64 / PER_MTOK) * self.output_usd_per_mtok
    }
}

/// Which tool-call dialect a model's provider speaks
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDialect {
    AnthropicTools,
    OpenaiJson,
    /// OpenAI's own Responses API (`stella_model::openai::OpenAiProvider`).
    /// Structurally distinct from `OpenaiJson` (Chat Completions and every
    /// OpenAI-*compatible* gateway: Z.ai, xAI, DeepSeek, OpenRouter, local)
    /// despite the name overlap: item-based `input`/`output` arrays with
    /// `function_call`/`function_call_output` items, not a `messages` array
    /// with an accumulating `tool_calls` delta array. Real OpenAI models
    /// (the `gpt-5.5` row below) get this variant now that the real
    /// adapter exists; `OpenaiJson` stays the dialect name for everything
    /// that actually speaks the Chat Completions wire shape.
    OpenaiResponses,
    /// Google's native `generateContent` dialect
    /// (`stella_model::gemini::GeminiProvider` and
    /// `stella_model::vertex::VertexProvider` — identical wire shape,
    /// different auth/addressing): `functionCall`/`functionResponse` parts
    /// correlated by function *name* (no wire call ids), args arriving as
    /// complete JSON objects rather than streamed string fragments, and
    /// Gemini 3 thought signatures riding on call parts
    /// ( `gemini-functions`).
    GeminiFunctions,
    /// Amazon Bedrock's Converse dialect
    /// (`stella_model::bedrock::BedrockProvider`): `toolUse`/`toolResult`
    /// content blocks correlated by `toolUseId`, tool results framed on a
    /// user-role message with an explicit `status` field for failures.
    BedrockConverse,
}

/// One catalog row — provider-native slug, verified against the provider's
/// own `/models` endpoint (seed data below is the day-0 offline fallback;
/// refreshed rows carry the latest model-card version's pricing).
///
/// Fields are owned `String`s (not `&'static str`): rows come from two
/// sources now — the compile-time seed and the on-disk model-card catalog
/// installed at startup — and only one of those can borrow from the binary.
///
/// `Eq` is intentionally *not* derived: [`Pricing`] carries `f64` fields, and
/// exact float equality is not a meaningful identity for a catalog row (rows
/// are keyed by `(provider, id)`, deduped by the seed test). `PartialEq` is
/// kept for tests that compare whole entries.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub id: String,
    pub provider: String,
    pub family: String,
    pub context_window: u32,
    pub tool_dialect: ToolDialect,
    /// List pricing used to compute `CompletionResult::cost_usd` on the real
    /// request path — each adapter resolves its own row in its constructor.
    pub pricing: Pricing,
    /// Whether this model supports reasoning / extended thinking. `None` is
    /// "unknown": effort settings pass through and the provider stays the
    /// authority. `Some(false)` is a hard "no" from catalog data — the
    /// effort picker hides its levels and the request path drops
    /// effort/reasoning instead of sending a parameter the API rejects.
    pub supports_reasoning: Option<bool>,
}

impl CatalogEntry {
    /// A catalog row without the field-by-field ceremony — the seed table
    /// below, the runtime-catalog assembly in `stella-cli`, and tests all
    /// build entries through this. Capabilities default to unknown; chain
    /// [`CatalogEntry::with_reasoning`] where the data exists.
    pub fn new(
        id: &str,
        provider: &str,
        family: &str,
        context_window: u32,
        tool_dialect: ToolDialect,
        pricing: Pricing,
    ) -> Self {
        Self {
            id: id.to_string(),
            provider: provider.to_string(),
            family: family.to_string(),
            context_window,
            tool_dialect,
            pricing,
            supports_reasoning: None,
        }
    }

    /// Set the reasoning capability (builder-style, so the many existing
    /// `new` call sites stay untouched).
    pub fn with_reasoning(mut self, supports_reasoning: Option<bool>) -> Self {
        self.supports_reasoning = supports_reasoning;
        self
    }
}

/// The process-wide catalog installed by `stella-cli` at startup (seed rows
/// merged with the on-disk model-card catalog). `None` until installed;
/// [`Catalog::current`] falls back to the seed so library consumers and
/// tests behave identically without an install.
static RUNTIME_CATALOG: RwLock<Option<Arc<Catalog>>> = RwLock::new(None);

/// The seed, built once — [`Catalog::current`]'s fallback must not
/// reallocate the table on every adapter construction.
fn seed_arc() -> &'static Arc<Catalog> {
    static SEED: OnceLock<Arc<Catalog>> = OnceLock::new();
    SEED.get_or_init(|| Arc::new(Catalog::seed()))
}

/// The model catalog. Curated, versioned data — not code that
/// call sites reach past. `Catalog::resolve` is the only sanctioned way to
/// turn a user-supplied slug into a usable model reference.
pub struct Catalog {
    entries: Vec<CatalogEntry>,
}

impl Catalog {
    /// The in-binary seed: one row per provider `config.rs::PROVIDERS` can
    /// select, keyed to that table's `default_model`. `stella models
    /// refresh` grows the *runtime* catalog with live master-list data; the
    /// seed stays the offline floor.
    pub fn seed() -> Self {
        Self {
            entries: vec![
                CatalogEntry::new(
                    "glm-5.2",
                    "zai",
                    "glm",
                    200_000,
                    ToolDialect::OpenaiJson,
                    Pricing {
                        input_usd_per_mtok: 0.60,
                        output_usd_per_mtok: 2.20,
                        cached_input_usd_per_mtok: 0.11,
                    },
                )
                .with_reasoning(Some(true)),
                CatalogEntry::new(
                    "claude-fable-5",
                    "anthropic",
                    "claude",
                    200_000,
                    ToolDialect::AnthropicTools,
                    Pricing {
                        input_usd_per_mtok: 3.00,
                        output_usd_per_mtok: 15.00,
                        cached_input_usd_per_mtok: 0.30,
                    },
                )
                .with_reasoning(Some(true)),
                // Real adapter now exists (stella_model::openai) — this row
                // used to be OpenaiJson, which was wrong: OpenAI was never
                // routed through Chat Completions, only through the generic
                // ZaiProvider pointed at OpenAI's base URL as a stand-in
                // until the Responses API adapter landed.
                CatalogEntry::new(
                    "gpt-5.5",
                    "openai",
                    "gpt",
                    400_000,
                    ToolDialect::OpenaiResponses,
                    Pricing {
                        input_usd_per_mtok: 1.25,
                        output_usd_per_mtok: 10.00,
                        cached_input_usd_per_mtok: 0.125,
                    },
                )
                .with_reasoning(Some(true)),
                CatalogEntry::new(
                    "grok-4",
                    "xai",
                    "grok",
                    256_000,
                    ToolDialect::OpenaiJson,
                    Pricing {
                        input_usd_per_mtok: 3.00,
                        output_usd_per_mtok: 15.00,
                        cached_input_usd_per_mtok: 0.75,
                    },
                )
                .with_reasoning(Some(true)),
                // The non-thinking chat model (`deepseek-reasoner` is the
                // reasoning one) — the seed's one honest `Some(false)`.
                CatalogEntry::new(
                    "deepseek-chat",
                    "deepseek",
                    "deepseek",
                    128_000,
                    ToolDialect::OpenaiJson,
                    Pricing {
                        input_usd_per_mtok: 0.27,
                        output_usd_per_mtok: 1.10,
                        cached_input_usd_per_mtok: 0.07,
                    },
                )
                .with_reasoning(Some(false)),
                // The native Gemini-direct adapter (stella_model::gemini) —
                // this row used to be OpenaiJson while requests routed
                // through Google's OpenAI-compatibility shim as a stand-in.
                CatalogEntry::new(
                    "gemini-3-pro",
                    "gemini",
                    "gemini",
                    1_000_000,
                    ToolDialect::GeminiFunctions,
                    Pricing {
                        input_usd_per_mtok: 1.25,
                        output_usd_per_mtok: 10.00,
                        cached_input_usd_per_mtok: 0.31,
                    },
                )
                .with_reasoning(Some(true)),
                // The same Google model surfaced through Vertex AI — one
                // model genuinely existing on two providers is why
                // uniqueness (and `resolve_for`) is keyed on
                // (provider, id), not id alone. Same list price as the
                // Gemini-direct row above.
                CatalogEntry::new(
                    "gemini-3-pro",
                    "vertex",
                    "gemini",
                    1_000_000,
                    ToolDialect::GeminiFunctions,
                    Pricing {
                        input_usd_per_mtok: 1.25,
                        output_usd_per_mtok: 10.00,
                        cached_input_usd_per_mtok: 0.31,
                    },
                )
                .with_reasoning(Some(true)),
                // A cross-region inference profile, not a bare model id —
                // Bedrock rejects on-demand invocation of newer Anthropic
                // models without one. Priced as Claude Sonnet 4.5 (Bedrock
                // on-demand list price).
                CatalogEntry::new(
                    "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                    "bedrock",
                    "claude",
                    200_000,
                    ToolDialect::BedrockConverse,
                    Pricing {
                        input_usd_per_mtok: 3.00,
                        output_usd_per_mtok: 15.00,
                        cached_input_usd_per_mtok: 0.30,
                    },
                )
                .with_reasoning(Some(true)),
                // OpenRouter's fully-qualified slug for its own meta-router.
                // The gateway's model ids are ALL `vendor/model` — a bare
                // `auto` is not a model there, so this row must carry the
                // wire-true id (it is sent verbatim as the request's
                // `model`). A real, provider-native catalog entry, not our
                // internal `Option<ModelRef>` "auto" sentinel (L-M3 is
                // about OUR resolver never using a string for "no pin";
                // this is a third party's own product feature we pass
                // through verbatim).
                //
                // OpenRouter's `auto` meta-model routes to whichever
                // underlying model it picks, so the effective price varies
                // per request and the gateway reports it back on its own
                // usage/generation endpoint — we cannot know it from the
                // slug alone. Left at zero deliberately: a wrong fixed
                // estimate is worse than a zero the metering layer can flag
                // as "gateway-priced, reconcile from the provider's usage
                // record."
                CatalogEntry::new(
                    "openrouter/auto",
                    "openrouter",
                    "openrouter",
                    128_000,
                    ToolDialect::OpenaiJson,
                    Pricing {
                        input_usd_per_mtok: 0.0,
                        output_usd_per_mtok: 0.0,
                        cached_input_usd_per_mtok: 0.0,
                    },
                ),
            ],
        }
    }

    /// A catalog over explicit rows — how `stella-cli` assembles the runtime
    /// catalog (seed rows first, then the on-disk model-card rows, so seed
    /// lookups keep their exact pre-refresh results).
    pub fn with_entries(entries: Vec<CatalogEntry>) -> Self {
        Self { entries }
    }

    /// Install the process-wide catalog [`Catalog::current`] serves.
    /// Idempotent and replaceable — the last install wins (a mid-session
    /// `stella models refresh` re-installs with the new rows).
    pub fn install_runtime(catalog: Catalog) {
        let mut slot = RUNTIME_CATALOG.write().unwrap_or_else(|e| e.into_inner());
        *slot = Some(Arc::new(catalog));
    }

    /// The catalog every consumer resolves against: the installed runtime
    /// catalog when `stella-cli` has loaded one, otherwise the seed. Library
    /// users (and tests) that never install see exactly the seed.
    pub fn current() -> Arc<Catalog> {
        let slot = RUNTIME_CATALOG.read().unwrap_or_else(|e| e.into_inner());
        slot.clone().unwrap_or_else(|| Arc::clone(seed_arc()))
    }

    /// Resolve a slug against the catalog. Returns `ProviderError::UnknownModel`
    /// (never a fallback to a default model) when the slug isn't present —
    /// the loud, named error the spec requires. When the same slug exists on
    /// several providers (e.g. `gemini-3-pro` on both `gemini` and
    /// `vertex`), the first row wins; use [`Catalog::resolve_for`] when the
    /// provider is known.
    pub fn resolve(&self, slug: &str) -> Result<&CatalogEntry, ProviderError> {
        self.entries
            .iter()
            .find(|entry| entry.id == slug)
            .ok_or_else(|| ProviderError::UnknownModel {
                slug: slug.to_string(),
            })
    }

    /// Resolve a slug for a specific provider — the form `build_provider`
    /// uses, since the same model genuinely exists on more than one
    /// provider (Gemini on `gemini` and `vertex`; most things on
    /// `openrouter`) and a slug that exists on provider A must still be a
    /// hard error when requested from provider B.
    pub fn resolve_for(&self, provider: &str, slug: &str) -> Result<&CatalogEntry, ProviderError> {
        self.entries
            .iter()
            .find(|entry| entry.provider == provider && entry.id == slug)
            .ok_or_else(|| ProviderError::UnknownModel {
                slug: format!("{provider}/{slug}"),
            })
    }

    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_known_slug_succeeds() {
        let catalog = Catalog::seed();
        let entry = catalog.resolve("glm-5.2").expect("glm-5.2 is seeded");
        assert_eq!(entry.provider, "zai");
        assert_eq!(entry.tool_dialect, ToolDialect::OpenaiJson);
    }

    #[test]
    fn resolve_unknown_slug_is_a_named_hard_error_never_a_fallback() {
        let catalog = Catalog::seed();
        let err = catalog.resolve("glm-5.2-turbo").unwrap_err();
        match err {
            ProviderError::UnknownModel { slug } => assert_eq!(slug, "glm-5.2-turbo"),
            other => panic!("expected UnknownModel, got {other:?}"),
        }
    }

    #[test]
    fn seed_catalog_has_no_duplicate_provider_id_pairs() {
        // Keyed on (provider, id), not id alone: the same model genuinely
        // exists on more than one provider (gemini-3-pro on gemini and
        // vertex), which is exactly why resolve_for exists.
        let catalog = Catalog::seed();
        let mut pairs: Vec<(&str, &str)> = catalog
            .entries()
            .iter()
            .map(|e| (e.provider.as_str(), e.id.as_str()))
            .collect();
        let before = pairs.len();
        pairs.sort_unstable();
        pairs.dedup();
        assert_eq!(
            pairs.len(),
            before,
            "catalog seed must not contain duplicate (provider, slug) pairs"
        );
    }

    #[test]
    fn resolve_for_scopes_the_slug_to_the_named_provider() {
        let catalog = Catalog::seed();
        let entry = catalog
            .resolve_for("vertex", "gemini-3-pro")
            .expect("vertex row is seeded");
        assert_eq!(entry.provider, "vertex");
        assert_eq!(entry.tool_dialect, ToolDialect::GeminiFunctions);

        // A slug that exists — but on a different provider — is still a
        // hard error for the provider actually requested.
        let err = catalog.resolve_for("bedrock", "gemini-3-pro").unwrap_err();
        match err {
            ProviderError::UnknownModel { slug } => {
                assert_eq!(slug, "bedrock/gemini-3-pro")
            }
            other => panic!("expected UnknownModel, got {other:?}"),
        }
    }

    #[test]
    fn install_runtime_extends_current_without_disturbing_seed_rows() {
        // The runtime catalog is a process-global; this test installs a
        // strict SUPERSET of the seed so any concurrently-running test that
        // resolves seed rows through `current()` sees identical results
        // regardless of test ordering.
        let mut entries = Catalog::seed().entries.clone();
        entries.push(CatalogEntry::new(
            "test-only-model",
            "anthropic",
            "claude",
            100_000,
            ToolDialect::AnthropicTools,
            Pricing {
                input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 2.0,
                cached_input_usd_per_mtok: 0.1,
            },
        ));
        Catalog::install_runtime(Catalog::with_entries(entries));

        let current = Catalog::current();
        // Seed row unchanged, new row visible.
        assert_eq!(
            current.resolve_for("zai", "glm-5.2").unwrap().pricing,
            Catalog::seed().resolve("glm-5.2").unwrap().pricing,
        );
        assert!(current.resolve_for("anthropic", "test-only-model").is_ok());
    }

    #[test]
    fn pricing_bills_cached_input_at_its_own_rate_and_never_double_charges() {
        let pricing = Pricing {
            input_usd_per_mtok: 3.00,
            output_usd_per_mtok: 15.00,
            cached_input_usd_per_mtok: 0.30,
        };
        // 1M input tokens of which 400k are cached, plus 200k output:
        //   uncached input = 600k @ $3/M    = 1.80
        //   cached input   = 400k @ $0.30/M = 0.12
        //   output         = 200k @ $15/M   = 3.00
        //                                     ------
        //                                      4.92
        let usage = CompletionUsage {
            reported: true,
            input_tokens: 1_000_000,
            output_tokens: 200_000,
            cached_input_tokens: 400_000,
            cache_write_tokens: 0,
        };
        assert!((pricing.cost_usd(&usage) - 4.92).abs() < 1e-9);
    }

    #[test]
    fn pricing_saturates_when_cached_exceeds_reported_input() {
        // Defensive: a provider reporting more cached than total input must
        // never produce a negative uncached-input charge.
        let pricing = Pricing {
            input_usd_per_mtok: 3.00,
            output_usd_per_mtok: 15.00,
            cached_input_usd_per_mtok: 0.30,
        };
        let usage = CompletionUsage {
            reported: true,
            input_tokens: 100,
            output_tokens: 0,
            cached_input_tokens: 1_000,
            cache_write_tokens: 0,
        };
        // All 100 input tokens billed as cached (clamped), never negative.
        let expected = (100.0 / 1_000_000.0) * 0.30;
        assert!((pricing.cost_usd(&usage) - expected).abs() < 1e-12);
        assert!(pricing.cost_usd(&usage) >= 0.0);
    }

    #[test]
    fn every_priced_provider_default_has_nonzero_input_and_output_pricing() {
        // OpenRouter `auto` is deliberately zero (gateway-priced); every
        // other seeded model must carry a real, positive list price so
        // `cost_usd` is never a silent no-op on the real request path.
        let catalog = Catalog::seed();
        for entry in catalog.entries() {
            if entry.id == "openrouter/auto" {
                continue;
            }
            assert!(
                entry.pricing.input_usd_per_mtok > 0.0 && entry.pricing.output_usd_per_mtok > 0.0,
                "model `{}` has zero pricing — budget metering would be a no-op",
                entry.id
            );
        }
    }

    #[test]
    fn seed_covers_every_provider_stella_cli_can_select() {
        // stella-cli/src/config.rs::PROVIDERS lists these providers; this
        // test doesn't import that crate (stella-cli depends on
        // stella-model, not the reverse) but pins the provider id set here
        // so the two can't silently drift apart again — the actual
        // cross-check lives in stella-cli's own test suite (config::tests).
        let catalog = Catalog::seed();
        for provider in [
            "zai",
            "anthropic",
            "openai",
            "xai",
            "deepseek",
            "gemini",
            "openrouter",
            "vertex",
            "bedrock",
        ] {
            assert!(
                catalog.entries().iter().any(|e| e.provider == provider),
                "no catalog entry for provider `{provider}`"
            );
        }
    }
}
