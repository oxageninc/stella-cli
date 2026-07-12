//! Configuration: provider/model resolution, BYOK credential lookup.
//!
//! Resolution order per 01-product-spec.md §4: CLI flag -> env var ->
//! `~/.config/stella/credentials.toml` -> interactive prompt on first use.
//! The full chain lives in `stella_model::credential::ApiKey::resolve`; this
//! module's job is picking WHICH provider (from `--model`, or the first one
//! with a resolvable credential) and then running that chain for it.

use std::env;

use colored::Colorize;
use stella_model::credential::{ApiKey, CredentialError, CredentialsFile};

/// One provider's config: id, env var name, display name, default model.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub id: &'static str,
    pub env_var: &'static str,
    /// Alternate env var names accepted for this provider's credential,
    /// tried after `env_var` and before the credentials file (spec §2:
    /// `GEMINI_API_KEY` alias `GOOGLE_API_KEY`).
    pub env_var_aliases: &'static [&'static str],
    pub display_name: &'static str,
    pub default_model: &'static str,
    pub base_url: &'static str,
}

/// All supported providers, in preference order. Order matters twice over:
/// auto-detection picks the first row with a resolvable credential, which is
/// why Bedrock (keyed on the generic `AWS_ACCESS_KEY_ID` that plenty of
/// non-Bedrock users have exported) sits last — it must only ever be
/// auto-picked when nothing else is configured; `--model bedrock/…` pins it
/// explicitly regardless.
pub static PROVIDERS: &[ProviderConfig] = &[
    ProviderConfig {
        id: "zai",
        env_var: "ZAI_API_KEY",
        env_var_aliases: &[],
        display_name: "Z.ai (GLM 5.2)",
        default_model: "glm-5.2",
        base_url: "https://api.z.ai/api/paas/v4",
    },
    ProviderConfig {
        id: "anthropic",
        env_var: "ANTHROPIC_API_KEY",
        env_var_aliases: &[],
        display_name: "Anthropic (Claude)",
        default_model: "claude-fable-5",
        base_url: "https://api.anthropic.com",
    },
    ProviderConfig {
        id: "openai",
        env_var: "OPENAI_API_KEY",
        env_var_aliases: &[],
        display_name: "OpenAI (GPT)",
        default_model: "gpt-5.5",
        base_url: "https://api.openai.com/v1",
    },
    ProviderConfig {
        id: "xai",
        env_var: "XAI_API_KEY",
        env_var_aliases: &[],
        display_name: "xAI (Grok)",
        default_model: "grok-4",
        base_url: "https://api.x.ai/v1",
    },
    ProviderConfig {
        id: "deepseek",
        env_var: "DEEPSEEK_API_KEY",
        env_var_aliases: &[],
        display_name: "DeepSeek",
        default_model: "deepseek-chat",
        base_url: "https://api.deepseek.com/v1",
    },
    ProviderConfig {
        id: "gemini",
        env_var: "GEMINI_API_KEY",
        // Spec §2: "GEMINI_API_KEY (alias GOOGLE_API_KEY)" — the name most
        // Google tooling exports.
        env_var_aliases: &["GOOGLE_API_KEY"],
        display_name: "Google Gemini",
        default_model: "gemini-3-pro",
        // Gemini's native generateContent surface
        // (`stella_model::gemini::GeminiProvider`). This row previously
        // pointed at Google's OpenAI-compatibility shim
        // (`…/v1beta/openai`) served by the generic Chat Completions
        // adapter as a stand-in until the native adapter existed.
        base_url: "https://generativelanguage.googleapis.com/v1beta",
    },
    ProviderConfig {
        id: "openrouter",
        env_var: "OPENROUTER_API_KEY",
        env_var_aliases: &[],
        display_name: "OpenRouter",
        default_model: "auto",
        base_url: "https://openrouter.ai/api/v1",
    },
    ProviderConfig {
        id: "vertex",
        // Deliberately Vertex-specific (not a generic Google var) so
        // auto-detection is an explicit opt-in; documented as
        // `export VERTEX_ACCESS_TOKEN=$(gcloud auth print-access-token)`.
        // Also requires VERTEX_PROJECT_ID (or GOOGLE_CLOUD_PROJECT) and
        // honors VERTEX_LOCATION — resolved in `build_provider`.
        env_var: "VERTEX_ACCESS_TOKEN",
        env_var_aliases: &[],
        display_name: "Google Vertex AI",
        default_model: "gemini-3-pro",
        // Display anchor: the real endpoint is project/location-scoped and
        // built per request by the adapter.
        base_url: "https://aiplatform.googleapis.com",
    },
    ProviderConfig {
        id: "bedrock",
        // The standard AWS chain vars; AWS_SECRET_ACCESS_KEY (and optional
        // AWS_SESSION_TOKEN / AWS_REGION) are resolved in `build_provider`.
        // Last in preference order on purpose — see the doc comment above.
        env_var: "AWS_ACCESS_KEY_ID",
        env_var_aliases: &[],
        display_name: "Amazon Bedrock",
        default_model: "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        // Display anchor: the real host is region-scoped
        // (`bedrock-runtime.<AWS_REGION>.amazonaws.com`), built per request
        // by the adapter.
        base_url: "https://bedrock-runtime.<AWS_REGION>.amazonaws.com",
    },
    // Vertex and Bedrock are appended LAST so auto-detection (the no-`--model`
    // path picks the first provider with a resolvable credential) never
    // prefers them over an explicitly-configured provider — AWS_ACCESS_KEY_ID
    // in particular is commonly present in a shell for unrelated reasons.
    // Both speak a native, non-OpenAI wire shape, so `openai_compatible` is
    // false and `build_provider` (agent.rs) routes them to their own adapters.
    ProviderConfig {
        id: "vertex",
        env_var: "VERTEX_ACCESS_TOKEN",
        display_name: "Google Vertex AI",
        default_model: "gemini-3-pro",
        // Native generateContent, project/location-scoped. The VertexProvider
        // adapter builds its own addressing from VERTEX_PROJECT_ID /
        // VERTEX_LOCATION, so this base_url is shown in `stella models` for
        // reference only — build_provider does not pass it to the adapter.
        base_url: "https://aiplatform.googleapis.com",
        openai_compatible: false,
    },
    ProviderConfig {
        id: "bedrock",
        env_var: "AWS_ACCESS_KEY_ID",
        display_name: "Amazon Bedrock",
        default_model: "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        // Region-templated at request time by the BedrockProvider adapter
        // (bedrock-runtime.<AWS_REGION>.amazonaws.com); shown here for
        // reference only.
        base_url: "https://bedrock-runtime.us-east-1.amazonaws.com",
        openai_compatible: false,
    },
];

/// The `local` pseudo-provider: any OpenAI-compatible endpoint the user
/// points `--base-url` at (Ollama, vLLM, LM Studio, llama.cpp server —
/// `07-model-matrix.md` §2). Not in [`PROVIDERS`]: it is never auto-detected
/// (there is no ambient signal a local server exists), has no default model
/// (the server's models are whatever the user pulled), and its API key is
/// optional (`LOCAL_API_KEY`, defaulting to a placeholder — most local
/// servers ignore auth entirely).
pub static LOCAL_PROVIDER: ProviderConfig = ProviderConfig {
    id: "local",
    env_var: "LOCAL_API_KEY",
    env_var_aliases: &[],
    display_name: "Local (OpenAI-compatible)",
    default_model: "",
    base_url: "",
};

/// Resolved configuration: which provider, which model, which API key.
///
/// `api_key` is an [`ApiKey`] (not a raw `String`) so the whole `Config`'s
/// derived `Debug` can never leak the secret into logs, panics, or traces
/// (H3) — `ApiKey`'s `Debug` prints `<redacted>`. Read the raw value only
/// where a wire call genuinely needs it, via `reveal()`.
#[derive(Debug, Clone)]
pub struct Config {
    pub provider: ProviderConfig,
    pub model_id: String,
    pub api_key: ApiKey,
    pub workspace_root: std::path::PathBuf,
    /// `--base-url`: required for the `local` provider (it IS the server
    /// address), an optional proxy/override for every other provider.
    pub base_url_override: Option<String>,
}

impl Config {
    /// The base URL requests actually go to: `--base-url` when given,
    /// otherwise the provider's default. (Vertex and Bedrock build
    /// region/project-scoped URLs in their adapters and only consume the
    /// override half of this.)
    pub fn effective_base_url(&self) -> &str {
        self.base_url_override
            .as_deref()
            .unwrap_or(self.provider.base_url)
    }

    /// Load config: resolve provider from `--model` flag or the first one
    /// with a resolvable credential, then run the full chain (CLI flag ->
    /// env var (+aliases) -> credentials file -> interactive prompt) for it.
    /// `api_key_override` is `--api-key`, threaded straight into the chain's
    /// first (highest-precedence) step; `base_url_override` is `--base-url`.
    /// Errors if no key is found at all.
    pub fn load(
        model_override: Option<&str>,
        api_key_override: Option<&str>,
        base_url_override: Option<&str>,
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
                        base_url_override,
                        &mut credentials_file,
                        &workspace_root,
                        true,
                    );
                }
            };

            // `local/<model>`: any OpenAI-compatible endpoint the user
            // points --base-url at. Never auto-detected, key optional —
            // see LOCAL_PROVIDER's doc comment.
            if provider_id == LOCAL_PROVIDER.id {
                let base_url = base_url_override.map(str::to_string).ok_or_else(|| {
                    "the local provider needs --base-url (e.g. stella --model local/llama3.3 \
                     --base-url http://localhost:11434/v1)"
                        .to_string()
                })?;
                let api_key = api_key_override
                    .map(str::to_string)
                    .or_else(|| {
                        env::var(LOCAL_PROVIDER.env_var)
                            .ok()
                            .filter(|v| !v.is_empty())
                    })
                    // Most local servers ignore auth; OpenAI-compatible
                    // clients still send *something* as the bearer token.
                    .unwrap_or_else(|| "local".to_string());
                return Ok(Self {
                    provider: LOCAL_PROVIDER.clone(),
                    model_id,
                    api_key: ApiKey::new(api_key),
                    workspace_root,
                    base_url_override: Some(base_url),
                });
            }

            let provider = PROVIDERS
                .iter()
                .find(|p| p.id == provider_id)
                .ok_or_else(|| {
                    format!("unknown provider `{provider_id}` — available: {}", {
                        let mut v: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
                        v.push(LOCAL_PROVIDER.id);
                        v.join(", ")
                    })
                })?;

            return Self::resolve(
                provider,
                model_id,
                api_key_override,
                base_url_override,
                &mut credentials_file,
                &workspace_root,
                true,
            );
        }

        // No --model: pick the first provider with a resolvable credential
        // (env var/aliases or credentials file — never prompts here, since
        // prompting needs a specific provider in mind and the user hasn't
        // named one).
        for provider in PROVIDERS {
            if resolve_provider_key(provider, api_key_override, &credentials_file, false).is_ok() {
                return Self::resolve(
                    provider,
                    provider.default_model.to_string(),
                    api_key_override,
                    base_url_override,
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
        base_url_override: Option<&str>,
        credentials_file: &mut CredentialsFile,
        workspace_root: &std::path::Path,
        interactive: bool,
    ) -> Result<Self, String> {
        let (key, source) =
            resolve_provider_key(provider, api_key_override, credentials_file, interactive)
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
        // `reveal()` is used only here, where the plaintext secret genuinely
        // must be written to the credentials file — never stored back as a
        // bare `String` on `Config`.
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
            base_url_override: base_url_override.map(str::to_string),
        })
    }

    pub fn print_models(&self) {
        println!(
            "{}\n",
            "Stella — Available Providers & Models".cyan().bold()
        );
        for p in PROVIDERS {
            let has_key = std::iter::once(&p.env_var)
                .chain(p.env_var_aliases)
                .any(|var| env::var(var).map(|v| !v.is_empty()).unwrap_or(false));
            let key_status = if has_key {
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
        println!(
            "  Local endpoints (Ollama, vLLM, LM Studio): stella --model local/<model> \
             --base-url http://localhost:11434/v1"
        );
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
        println!("  Base URL:   {}", self.effective_base_url().dimmed());
        println!("  Workspace:  {}", self.workspace_root.display());
        println!(
            "  Dialect:    {}",
            match self.provider.id {
                "openai" => "OpenAI Responses",
                "anthropic" => "Anthropic Messages",
                "gemini" | "vertex" => "Gemini generateContent",
                "bedrock" => "Bedrock Converse",
                _ => "OpenAI-compatible",
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
            let has_key = std::iter::once(&p.env_var)
                .chain(p.env_var_aliases)
                .any(|var| env::var(var).map(|v| !v.is_empty()).unwrap_or(false));
            let key_status = if has_key {
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
        println!(
            "  Local endpoints (Ollama, vLLM, LM Studio): stella --model local/<model> \
             --base-url http://localhost:11434/v1"
        );
    }
}

fn model_id_override(slug: &str) -> String {
    slug.to_string()
}

/// The provider-aware credential chain: CLI flag -> primary env var ->
/// alias env vars -> credentials file -> interactive prompt. Wraps
/// `ApiKey::resolve` (which owns everything except aliases) so alias env
/// vars slot in at exactly env-var precedence — after the primary var,
/// before the credentials file.
fn resolve_provider_key(
    provider: &ProviderConfig,
    api_key_override: Option<&str>,
    credentials_file: &CredentialsFile,
    interactive: bool,
) -> Result<(ApiKey, stella_model::credential::CredentialSource), CredentialError> {
    use stella_model::credential::CredentialSource;

    let first_pass = ApiKey::resolve(
        provider.id,
        provider.env_var,
        api_key_override,
        Some(credentials_file),
        false,
    );
    match first_pass {
        // Flag or primary env var hit: nothing outranks those.
        Ok((key, source @ (CredentialSource::CliFlag | CredentialSource::EnvVar))) => {
            Ok((key, source))
        }
        // The chain fell through to the credentials file — but an alias env
        // var still outranks the file.
        Ok((key, source)) => {
            for alias in provider.env_var_aliases {
                if let Ok(alias_key) = ApiKey::from_env(alias) {
                    return Ok((alias_key, CredentialSource::EnvVar));
                }
            }
            Ok((key, source))
        }
        Err(CredentialError::NotFound { .. }) => {
            for alias in provider.env_var_aliases {
                match ApiKey::from_env(alias) {
                    Ok(key) => return Ok((key, CredentialSource::EnvVar)),
                    // An explicitly-set-but-empty alias is a user mistake
                    // worth surfacing, same posture as the primary var.
                    Err(err @ CredentialError::Empty { .. }) => return Err(err),
                    Err(_) => {}
                }
            }
            // Nothing anywhere — rerun the full chain with the caller's
            // interactivity so the prompt step can fire when allowed.
            ApiKey::resolve(
                provider.id,
                provider.env_var,
                api_key_override,
                Some(credentials_file),
                interactive,
            )
        }
        Err(other) => Err(other),
    }
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
    /// Uses the provider-scoped resolver, same as `build_provider`, so a
    /// default that only exists under a *different* provider's row still
    /// fails here.
    #[test]
    fn every_provider_default_model_resolves_against_the_catalog_seed() {
        let catalog = stella_model::catalog::Catalog::seed();
        for provider in PROVIDERS {
            catalog
                .resolve_for(provider.id, provider.default_model)
                .unwrap_or_else(|e| {
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
        ids.push(LOCAL_PROVIDER.id);
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate provider id in PROVIDERS");
    }

    #[test]
    fn alias_env_var_resolves_when_the_primary_is_unset() {
        // Synthetic provider with unique var names so parallel tests can't
        // race on shared env state (the convention credential.rs's own
        // tests follow).
        let provider = ProviderConfig {
            id: "alias-test",
            env_var: "STELLA_TEST_ALIAS_PRIMARY_KEY",
            env_var_aliases: &["STELLA_TEST_ALIAS_SECONDARY_KEY"],
            display_name: "Alias Test",
            default_model: "m",
            base_url: "",
        };
        // SAFETY: test-only env mutation, unique var names per test.
        unsafe {
            std::env::remove_var("STELLA_TEST_ALIAS_PRIMARY_KEY");
            std::env::set_var("STELLA_TEST_ALIAS_SECONDARY_KEY", "sk-from-alias");
        }
        let file = CredentialsFile::load(std::env::temp_dir().join(format!(
            "stella-test-alias-credentials-{}.toml",
            std::process::id()
        )))
        .unwrap();

        let (key, source) = resolve_provider_key(&provider, None, &file, false).unwrap();
        assert_eq!(key.reveal(), "sk-from-alias");
        assert_eq!(source, stella_model::credential::CredentialSource::EnvVar);

        unsafe {
            std::env::remove_var("STELLA_TEST_ALIAS_SECONDARY_KEY");
        }
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
            base_url_override: None,
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains(secret), "Config Debug leaked the key: {dbg}");
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn local_provider_requires_base_url_and_defaults_its_key() {
        let err = Config::load(Some("local/llama3.3"), None, None).unwrap_err();
        assert!(err.contains("--base-url"), "{err}");

        let cfg = Config::load(
            Some("local/llama3.3"),
            None,
            Some("http://localhost:11434/v1"),
        )
        .expect("local provider with --base-url should resolve");
        assert_eq!(cfg.provider.id, "local");
        assert_eq!(cfg.model_id, "llama3.3");
        assert_eq!(cfg.effective_base_url(), "http://localhost:11434/v1");
        // No LOCAL_API_KEY set: the placeholder key is used (local servers
        // generally ignore auth, but the OpenAI-compatible client always
        // sends a bearer token).
        assert_eq!(cfg.api_key.reveal(), "local");
    }
}
