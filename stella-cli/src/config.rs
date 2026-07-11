//! Configuration: provider/model resolution, BYOK credential lookup.
//!
//! Resolution order per 01-product-spec.md §4: CLI flag -> env var ->
//! `~/.config/stella/credentials.toml` -> interactive prompt on first use.
//! The full chain lives in `stella_model::credential::ApiKey::resolve`; this
//! module's job is picking WHICH provider (from `--model`, or the first one
//! with a resolvable credential) and then running that chain for it.

use std::env;

use colored::Colorize;
use stella_model::credential::{ApiKey, CredentialsFile};

/// One provider's config: id, env var name, display name, default model.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub id: &'static str,
    pub env_var: &'static str,
    pub display_name: &'static str,
    pub default_model: &'static str,
    pub base_url: &'static str,
    /// Whether this provider speaks the OpenAI-*compatible* chat/completions
    /// dialect (true for Z.ai, xAI, DeepSeek, Gemini's OpenAI-compat shim,
    /// OpenRouter, any generic OpenAI-compatible gateway). False for
    /// Anthropic (Messages API). Real OpenAI (`id == "openai"`) is `true`
    /// here too for backwards compatibility with anything still matching on
    /// this flag, but `build_provider` (`agent.rs`) special-cases
    /// `id == "openai"` ahead of this flag and routes it through the real
    /// Responses API adapter (`stella_model::openai`) instead — OpenAI's own
    /// API is not the same wire shape as the "OpenAI-compatible" gateways
    /// this flag is actually describing.
    pub openai_compatible: bool,
}

/// All supported providers, in preference order.
pub static PROVIDERS: &[ProviderConfig] = &[
    ProviderConfig {
        id: "zai",
        env_var: "ZAI_API_KEY",
        display_name: "Z.ai (GLM 5.2)",
        default_model: "glm-5.2",
        base_url: "https://api.z.ai/api/paas/v4",
        openai_compatible: true,
    },
    ProviderConfig {
        id: "anthropic",
        env_var: "ANTHROPIC_API_KEY",
        display_name: "Anthropic (Claude)",
        default_model: "claude-fable-5",
        base_url: "https://api.anthropic.com",
        openai_compatible: false,
    },
    ProviderConfig {
        id: "openai",
        env_var: "OPENAI_API_KEY",
        display_name: "OpenAI (GPT)",
        default_model: "gpt-5.5",
        base_url: "https://api.openai.com/v1",
        openai_compatible: true,
    },
    ProviderConfig {
        id: "xai",
        env_var: "XAI_API_KEY",
        display_name: "xAI (Grok)",
        default_model: "grok-4",
        base_url: "https://api.x.ai/v1",
        openai_compatible: true,
    },
    ProviderConfig {
        id: "deepseek",
        env_var: "DEEPSEEK_API_KEY",
        display_name: "DeepSeek",
        default_model: "deepseek-chat",
        base_url: "https://api.deepseek.com/v1",
        openai_compatible: true,
    },
    ProviderConfig {
        id: "gemini",
        env_var: "GEMINI_API_KEY",
        display_name: "Google Gemini",
        default_model: "gemini-3-pro",
        // NOTE: this is Google's OpenAI-compatibility shim
        // (`/v1beta/openai/...`), not Gemini's native `generateContent`
        // wire shape — the two are NOT interchangeable and the base URL
        // must include the `/openai` segment or every request 404s. A
        // native "Gemini direct" adapter (07-model-matrix.md §2: thinking
        // support, Imagen/Veo, native multimodal) is real follow-up work,
        // deferred because it can't be verified without a live
        // GEMINI_API_KEY in this environment (same reasoning as Bedrock/
        // Vertex/local-GGUF — see the Phase 2 PR description).
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        openai_compatible: true,
    },
    ProviderConfig {
        id: "openrouter",
        env_var: "OPENROUTER_API_KEY",
        display_name: "OpenRouter",
        default_model: "auto",
        base_url: "https://openrouter.ai/api/v1",
        openai_compatible: true,
    },
];

/// Resolved configuration: which provider, which model, which API key.
///
/// `api_key` is an [`ApiKey`] (not a raw `String`) so it inherits the
/// redaction newtype's guarantees: `Config`'s derived `Debug` prints the key
/// as `<redacted>`, and the only ways to see the value are the deliberate
/// `reveal()` (auth headers) and `redacted_preview()` (partial display).
#[derive(Debug, Clone)]
pub struct Config {
    pub provider: ProviderConfig,
    pub model_id: String,
    pub api_key: ApiKey,
    pub workspace_root: std::path::PathBuf,
}

impl Config {
    /// Load config: resolve provider from `--model` flag or the first one
    /// with a resolvable credential, then run the full chain (CLI flag ->
    /// env var -> credentials file -> interactive prompt) for it.
    /// `api_key_override` is `--api-key`, threaded straight into the chain's
    /// first (highest-precedence) step. Errors if no key is found at all.
    pub fn load(
        model_override: Option<&str>,
        api_key_override: Option<&str>,
    ) -> Result<Self, String> {
        let workspace_root =
            env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;
        let mut credentials_file = CredentialsFile::load_default().map_err(|e| {
            format!("~/.config/stella/credentials.toml exists but could not be read: {e}")
        })?;

        // If --model provider/model_id was given, resolve that provider —
        // interactively prompting if nothing else resolves it, since the
        // user has told us unambiguously which provider they want.
        if let Some(model_spec) = model_override {
            let (provider_id, model_id) = match model_spec.split_once('/') {
                Some((p, m)) => (p, m.to_string()),
                None => {
                    // Just a model slug — find which provider has it.
                    let provider = PROVIDERS
                        .iter()
                        .find(|p| p.default_model == model_spec)
                        .ok_or_else(|| {
                            format!(
                                "model `{model_spec}` not recognized — use provider/model_id format (e.g. zai/glm-5.2)\navailable providers: {}",
                                { let v: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect(); v.join(", ") }
                            )
                        })?;
                    return Self::resolve(
                        provider,
                        model_id_override(model_spec),
                        api_key_override,
                        &mut credentials_file,
                        &workspace_root,
                        true,
                    );
                }
            };

            let provider = PROVIDERS
                .iter()
                .find(|p| p.id == provider_id)
                .ok_or_else(|| {
                    format!("unknown provider `{provider_id}` — available: {}", {
                        let v: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
                        v.join(", ")
                    })
                })?;

            return Self::resolve(
                provider,
                model_id,
                api_key_override,
                &mut credentials_file,
                &workspace_root,
                true,
            );
        }

        // No --model: pick the first provider with a resolvable credential
        // (env var or credentials file — never prompts here, since prompting
        // needs a specific provider in mind and the user hasn't named one).
        for provider in PROVIDERS {
            if ApiKey::resolve(
                provider.id,
                provider.env_var,
                api_key_override,
                Some(&credentials_file),
                false,
            )
            .is_ok()
            {
                return Self::resolve(
                    provider,
                    provider.default_model.to_string(),
                    api_key_override,
                    &mut credentials_file,
                    &workspace_root,
                    false,
                );
            }
        }

        Err(format!(
            "no API key found. Set one of: {}\n\nExample: export ZAI_API_KEY=your_key_here\n\
             (or add it to ~/.config/stella/credentials.toml, or pass --model provider/model \
             to be prompted interactively)",
            PROVIDERS
                .iter()
                .map(|p| format!("{} ({})", p.env_var, p.display_name))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve(
        provider: &ProviderConfig,
        model_id: String,
        api_key_override: Option<&str>,
        credentials_file: &mut CredentialsFile,
        workspace_root: &std::path::Path,
        interactive: bool,
    ) -> Result<Self, String> {
        let (key, source) = ApiKey::resolve(
            provider.id,
            provider.env_var,
            api_key_override,
            Some(credentials_file),
            interactive,
        )
        .map_err(|e| {
            format!(
                "could not resolve a credential for {}: {e}",
                provider.display_name
            )
        })?;

        // "Interactive prompt on first use" (01-product-spec.md §4) implies
        // exactly that — first use. Persist so next invocation resolves via
        // the config-file step instead of prompting again. Best-effort: a
        // save failure (e.g. read-only home dir) shouldn't fail the command
        // the user actually asked for, just warn so it isn't silent.
        if source == stella_model::credential::CredentialSource::Interactive {
            credentials_file.set(provider.id, key.reveal());
            if let Err(e) = credentials_file.save() {
                eprintln!(
                    "  {} could not save the credential to ~/.config/stella/credentials.toml \
                     ({e}) — you'll be prompted again next time",
                    "warning:".yellow()
                );
            }
        }

        Ok(Self {
            provider: provider.clone(),
            model_id,
            api_key: key,
            workspace_root: workspace_root.to_path_buf(),
        })
    }

    pub fn print_models(&self) {
        println!(
            "{}\n",
            "Stella — Available Providers & Models".cyan().bold()
        );
        for p in PROVIDERS {
            let key_status = if env::var(p.env_var).map(|v| !v.is_empty()).unwrap_or(false) {
                "✓ configured".green()
            } else {
                "✗ no key".dimmed()
            };
            println!(
                "  {} {}/{}  {}  [{}]",
                key_status,
                p.id.bright_blue(),
                p.default_model.bright_white(),
                p.display_name,
                p.base_url.dimmed(),
            );
        }
        println!("\n  Use --model provider/model_id to pin a specific model.");
        println!("  Example: stella --model zai/glm-5.2 run 'fix the failing test'");
    }

    pub fn print_config(&self) {
        println!("{}\n", "Stella — Current Configuration".cyan().bold());
        println!("  Provider:   {}", self.provider.display_name.bright_blue());
        println!(
            "  Model:      {}/{}",
            self.provider.id.bright_blue(),
            self.model_id.bright_white()
        );
        println!("  API Key:    {}", self.api_key.redacted_preview().dimmed());
        println!("  Base URL:   {}", self.provider.base_url.dimmed());
        println!("  Workspace:  {}", self.workspace_root.display());
        println!(
            "  Dialect:    {}",
            if self.provider.id == "openai" {
                "OpenAI Responses"
            } else if self.provider.openai_compatible {
                "OpenAI-compatible"
            } else {
                "Anthropic Messages"
            }
        );
    }
}

impl Config {
    /// Print all available providers/models without needing a resolved config.
    pub fn print_available_models() {
        println!(
            "{}\n",
            "Stella — Available Providers & Models".cyan().bold()
        );
        for p in PROVIDERS {
            let key_status = if env::var(p.env_var).map(|v| !v.is_empty()).unwrap_or(false) {
                "✓ configured".green()
            } else {
                "✗ no key".dimmed()
            };
            println!(
                "  {} {}/{}  {}  [{}]",
                key_status,
                p.id.bright_blue(),
                p.default_model.bright_white(),
                p.display_name,
                p.base_url.dimmed(),
            );
        }
        println!("\n  Use --model provider/model_id to pin a specific model.");
        println!("  Example: stella --model zai/glm-5.2 run 'fix the failing test'");
    }
}

fn model_id_override(slug: &str) -> String {
    slug.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the bug fixed alongside this test: every
    /// provider's `default_model` here must resolve against
    /// `stella_model::catalog::Catalog::seed()`, or `build_provider`'s
    /// catalog check (`agent.rs`) would hard-error on first use of a
    /// provider whose default was never added to the seed — exactly what
    /// happened for 5 of these 7 rows before the catalog was completed.
    #[test]
    fn every_provider_default_model_resolves_against_the_catalog_seed() {
        let catalog = stella_model::catalog::Catalog::seed();
        for provider in PROVIDERS {
            catalog.resolve(provider.default_model).unwrap_or_else(|e| {
                panic!(
                    "provider `{}`'s default_model `{}` is not in the catalog seed: {e}",
                    provider.id, provider.default_model
                )
            });
        }
    }

    #[test]
    fn provider_ids_are_unique() {
        let mut ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate provider id in PROVIDERS");
    }

    #[test]
    fn config_debug_never_leaks_the_api_key() {
        // H3: with `api_key: ApiKey`, the whole Config's derived Debug must
        // redact the secret — no `{:?}` (logs, panics, traces) can leak it.
        let secret = "sk-super-secret-do-not-log-XYZ";
        let cfg = Config {
            provider: PROVIDERS[0].clone(),
            model_id: "glm-5.2".to_string(),
            api_key: ApiKey::new(secret),
            workspace_root: std::path::PathBuf::from("/tmp/ws"),
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains(secret), "Config Debug leaked the key: {dbg}");
        assert!(dbg.contains("redacted"));
    }
}
