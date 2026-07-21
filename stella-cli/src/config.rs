//! Configuration: provider/model resolution, BYOK credential lookup.
//!
//! Resolution order: CLI flag -> env var ->
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
    /// Which wire adapter serves this provider. `build_provider_parts`
    /// (agent.rs) dispatches on this — never on a hard-coded id match — so
    /// config-defined providers (settings.json) reach the right adapter too.
    pub dialect: Dialect,
    /// Whether this provider's models are curated in the catalog seed.
    /// `true` for the built-in rows (an unknown slug is a hard, named error
    /// — the anti-phantom-slug check exists to catch drift in OUR seed
    /// data); `false` for `local` and settings.json-defined providers,
    /// whose models are whatever the user's endpoint actually serves.
    pub seeded: bool,
}

/// The wire dialect a provider speaks — which `stella_model` adapter is
/// constructed for it. Serialized form is the settings.json `dialect` field
/// (kebab-case, e.g. `"openai-compatible"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Dialect {
    /// OpenAI Chat Completions shape (`stella_model::zai::ZaiProvider`,
    /// re-identified per provider) — Z.ai, xAI, DeepSeek, OpenRouter,
    /// local endpoints, and the default for config-defined providers.
    OpenaiCompatible,
    /// OpenAI Responses API (`stella_model::openai::OpenAiProvider`).
    OpenaiResponses,
    /// Anthropic Messages API (`stella_model::anthropic::AnthropicProvider`).
    Anthropic,
    /// Gemini generateContent (`stella_model::gemini::GeminiProvider`).
    Gemini,
    /// Vertex generateContent with project/location addressing. Built-in
    /// only: it needs `VERTEX_PROJECT_ID`/`VERTEX_LOCATION` resolution that
    /// a settings.json entry has no way to express.
    Vertex,
    /// Bedrock Converse with SigV4. Built-in only, same reasoning.
    Bedrock,
}

impl Dialect {
    /// Human-readable label for `stella config` / `stella models`.
    pub fn label(self) -> &'static str {
        match self {
            Dialect::OpenaiCompatible => "OpenAI-compatible",
            Dialect::OpenaiResponses => "OpenAI Responses",
            Dialect::Anthropic => "Anthropic Messages",
            Dialect::Gemini | Dialect::Vertex => "Gemini generateContent",
            Dialect::Bedrock => "Bedrock Converse",
        }
    }
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
        dialect: Dialect::OpenaiCompatible,
        seeded: true,
    },
    ProviderConfig {
        id: "anthropic",
        env_var: "ANTHROPIC_API_KEY",
        env_var_aliases: &[],
        display_name: "Anthropic (Claude)",
        default_model: "claude-fable-5",
        base_url: "https://api.anthropic.com",
        dialect: Dialect::Anthropic,
        seeded: true,
    },
    ProviderConfig {
        id: "openai",
        env_var: "OPENAI_API_KEY",
        env_var_aliases: &[],
        display_name: "OpenAI (GPT)",
        default_model: "gpt-5.5",
        base_url: "https://api.openai.com/v1",
        dialect: Dialect::OpenaiResponses,
        seeded: true,
    },
    ProviderConfig {
        id: "xai",
        env_var: "XAI_API_KEY",
        env_var_aliases: &[],
        display_name: "xAI (Grok)",
        default_model: "grok-4",
        base_url: "https://api.x.ai/v1",
        dialect: Dialect::OpenaiCompatible,
        seeded: true,
    },
    ProviderConfig {
        id: "deepseek",
        env_var: "DEEPSEEK_API_KEY",
        env_var_aliases: &[],
        display_name: "DeepSeek",
        default_model: "deepseek-chat",
        base_url: "https://api.deepseek.com/v1",
        dialect: Dialect::OpenaiCompatible,
        seeded: true,
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
        dialect: Dialect::Gemini,
        seeded: true,
    },
    ProviderConfig {
        id: "openrouter",
        env_var: "OPENROUTER_API_KEY",
        env_var_aliases: &[],
        display_name: "OpenRouter",
        default_model: "openrouter/auto",
        base_url: "https://openrouter.ai/api/v1",
        dialect: Dialect::OpenaiCompatible,
        // Unseeded on purpose, like `local`: OpenRouter fronts hundreds of
        // `vendor/model` slugs that change weekly — a curated seed can only
        // veto real models (`anthropic/claude-…` was a hard error here). A
        // typo'd slug fails fast with OpenRouter's own named 400/404, and
        // cost metering doesn't need list prices: the adapter requests the
        // gateway's usage accounting and takes the reported per-call cost.
        seeded: false,
    },
    // Vertex and Bedrock are appended LAST so auto-detection (the no-`--model`
    // path picks the first provider with a resolvable credential) never
    // prefers them over an explicitly-configured provider — AWS_ACCESS_KEY_ID
    // in particular is commonly present in a shell for unrelated reasons.
    // Both speak a native, non-OpenAI wire shape, so `build_provider`
    // (agent.rs) routes them to their own adapters rather than the generic
    // Chat Completions client.
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
        dialect: Dialect::Vertex,
        seeded: true,
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
        dialect: Dialect::Bedrock,
        seeded: true,
    },
];

/// The `local` pseudo-provider: any OpenAI-compatible endpoint the user
/// points `--base-url` at (Ollama, vLLM, LM Studio, llama.cpp server —
///). Not in [`PROVIDERS`]: it is never auto-detected
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
    dialect: Dialect::OpenaiCompatible,
    seeded: false,
};

/// Leak a string into a `&'static str`. `ProviderConfig` is `&'static str`
/// throughout (it is almost always one of the static [`PROVIDERS`] rows);
/// settings-defined providers are synthesized ONCE at startup, so leaking
/// their handful of strings trades a few bytes for keeping every downstream
/// consumer of `ProviderConfig` untouched.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

/// The env var a config-defined provider reads its credential from when the
/// entry doesn't name one: `<ID>_API_KEY`, uppercased, with anything outside
/// `[A-Za-z0-9]` folded to `_` (`my-gateway` → `MY_GATEWAY_API_KEY`).
pub(crate) fn derived_env_var(id: &str) -> String {
    let mut var: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    var.push_str("_API_KEY");
    var
}

/// A built-in provider with any settings.json override applied: display
/// name, base URL, and default model swap in place; `api_key_env` becomes
/// the primary credential var (the original demotes to an alias, so plain
/// `ANTHROPIC_API_KEY` keeps working). `dialect` on a built-in override is
/// ignored — a built-in's dialect is fixed by its adapter — and the catalog
/// check stays on (`seeded` is untouched).
pub(crate) fn effective_builtin(
    provider: &ProviderConfig,
    settings: &crate::settings::Settings,
) -> ProviderConfig {
    let Some(entry) = settings.providers.get(provider.id) else {
        return provider.clone();
    };
    let mut effective = provider.clone();
    if let Some(name) = &entry.name {
        effective.display_name = leak(name);
    }
    if let Some(base_url) = &entry.base_url {
        effective.base_url = leak(base_url);
    }
    if let Some(default_model) = &entry.default_model {
        effective.default_model = leak(default_model);
    }
    if let Some(api_key_env) = &entry.api_key_env {
        let mut aliases = vec![provider.env_var];
        aliases.extend_from_slice(provider.env_var_aliases);
        effective.env_var = leak(api_key_env);
        effective.env_var_aliases = Box::leak(aliases.into_boxed_slice());
    }
    effective
}

/// Synthesize a [`ProviderConfig`] for a settings.json entry whose id is NOT
/// a built-in provider — the "define a brand-new provider" half of issue
/// #44. `base_url` is required (there is no default to fall back to);
/// `dialect` defaults to OpenAI-compatible; the model catalog check is off
/// (`seeded: false`) because the user's endpoint, not our seed data, is the
/// authority on which models exist.
pub(crate) fn custom_provider(
    id: &str,
    entry: &crate::settings::ProviderSettings,
) -> Result<ProviderConfig, String> {
    if id.is_empty() || id.contains('/') || id.chars().any(char::is_whitespace) {
        return Err(format!(
            "settings.json: `{id}` is not a valid provider id (no slashes or whitespace)"
        ));
    }
    if id == LOCAL_PROVIDER.id {
        return Err(
            "settings.json: `local` is reserved for --model local/<model> --base-url <url> \
             and cannot be redefined"
                .to_string(),
        );
    }
    let dialect = entry.dialect.unwrap_or(Dialect::OpenaiCompatible);
    if matches!(dialect, Dialect::Vertex | Dialect::Bedrock) {
        return Err(format!(
            "settings.json: provider `{id}` requests the `{}` dialect, which is reserved for \
             the built-in provider (it needs credentials a settings entry cannot express)",
            if dialect == Dialect::Vertex {
                "vertex"
            } else {
                "bedrock"
            }
        ));
    }
    let base_url = entry.base_url.as_deref().ok_or_else(|| {
        format!("settings.json: provider `{id}` is not built-in, so it must declare `base_url`")
    })?;
    Ok(ProviderConfig {
        id: leak(id),
        env_var: leak(
            entry
                .api_key_env
                .clone()
                .unwrap_or_else(|| derived_env_var(id))
                .as_str(),
        ),
        env_var_aliases: &[],
        display_name: leak(entry.name.as_deref().unwrap_or(id)),
        default_model: leak(entry.default_model.as_deref().unwrap_or("")),
        base_url: leak(base_url),
        dialect,
        seeded: false,
    })
}

/// Every selectable provider id, for error messages: built-ins, `local`,
/// then config-defined ids.
fn all_provider_ids(settings: &crate::settings::Settings) -> Vec<&str> {
    let mut ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
    ids.push(LOCAL_PROVIDER.id);
    ids.extend(
        settings
            .providers
            .keys()
            .map(String::as_str)
            .filter(|id| !PROVIDERS.iter().any(|p| p.id == *id) && *id != LOCAL_PROVIDER.id),
    );
    ids
}

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
    /// Lifecycle hooks merged from the settings scope chain (see
    /// `Settings::load` for the project-scope trust boundary). `None` means
    /// no hooks configured anywhere — the engine runs the pre-hooks path.
    pub hooks: Option<stella_core::hooks::Hooks>,
    /// The merged `agent_engine_config` from the settings scope chain —
    /// per-agent models/prompts/effort/params and the auto modes. `None`
    /// means the section is absent everywhere; consumers treat that as
    /// all-defaults (`crate::engine_config` resolves it per run).
    pub engine_settings: Option<crate::settings::AgentEngineConfig>,
    /// Whether the `bash` tool is enabled (`tools.bash` in the settings
    /// scope chain; absent = OFF). Threaded into every `ToolRegistry`
    /// construction via `agent::registry_options` — the default tool
    /// surface has no shell.
    pub tools_bash: bool,
    /// Whether the web tool family is enabled (`tools.web`; absent = OFF).
    /// Same threading as `tools_bash` — the default tool surface has no
    /// network egress.
    pub tools_web: bool,
    /// Monotonic authority computed while loading the scope chain. Runtime
    /// adapters consume this instead of reinterpreting trust environment
    /// variables or repository settings independently.
    pub authority: crate::settings::AuthorityPolicy,
    /// Which step in the credential chain produced `api_key` — `stella
    /// config` displays this next to the redacted key preview so the
    /// command can never disagree with what actually resolved it. `None`
    /// only for the `local` provider's placeholder bearer token when no
    /// real credential (flag/env) was involved — not a resolved secret, so
    /// there is no source to report.
    pub credential_source: Option<stella_model::credential::CredentialSource>,
}

impl Config {
    /// The base URL requests actually go to: `--base-url` when given,
    /// otherwise the provider's default. (Vertex and Bedrock build
    /// region/project-scoped URLs in their adapters and only consume the
    /// override half of this.)
    ///
    /// For Z.ai, when the `ZAI_GLM_CODING_PLAN=1` environment variable is set,
    /// the coding plan endpoint (`https://api.z.ai/api/coding/paas/v4`) is used
    /// instead of the standard endpoint (`https://api.z.ai/api/paas/v4`).
    pub fn effective_base_url(&self) -> &str {
        if let Some(override_url) = &self.base_url_override {
            return override_url;
        }
        // Check for ZAI_GLM_CODING_PLAN env var for Zai provider
        if self.provider.id == "zai" && std::env::var("ZAI_GLM_CODING_PLAN").as_deref() == Ok("1") {
            return "https://api.z.ai/api/coding/paas/v4";
        }
        self.provider.base_url
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
        let settings = crate::settings::Settings::load(&workspace_root)?;
        Self::load_with_settings(
            model_override,
            api_key_override,
            base_url_override,
            &settings,
            workspace_root,
        )
    }

    /// [`Config::load`] over an already-merged [`Settings`] value — the
    /// seam tests use to exercise provider resolution without touching
    /// `$HOME`, `/etc`, or the real scope chain.
    fn load_with_settings(
        model_override: Option<&str>,
        api_key_override: Option<&str>,
        base_url_override: Option<&str>,
        settings: &crate::settings::Settings,
        workspace_root: std::path::PathBuf,
    ) -> Result<Self, String> {
        // The default agent's configured model sits BETWEEN the --model
        // flag and auto-detect: an explicit flag always wins, but a
        // settings-configured default beats "first provider with a key".
        // The spec string is synthesized in --model's own grammar
        // (`provider/slug`) so the whole existing resolution path applies
        // unchanged. Pin-vs-spec precedence lives in `model_spec_for` — the
        // one resolver every engine agent goes through — so a stale pin
        // can't swallow a qualified flat spec into a phantom slug here
        // while the judge/triage wiring resolves the same settings sanely.
        let engine_default: Option<String> = settings.agent_engine_config.as_ref().and_then(|e| {
            use crate::settings::EngineAgentKind;
            let ids = all_provider_ids(settings);
            let is_provider = |id: &str| ids.contains(&id);
            match crate::engine_config::model_spec_for(e, EngineAgentKind::Default, &is_provider) {
                Some(spec) if !spec.model.is_empty() => {
                    Some(format!("{}/{}", spec.provider, spec.model))
                }
                // A pin with no model anywhere is left to the normal chain
                // — the provider's own default_model already answers there.
                Some(_) => None,
                // No setting, or an unparseable string — pass the raw
                // value through so `resolve_provider_config` keeps its
                // named error for typos.
                None => e.model_for(EngineAgentKind::Default).map(str::to_string),
            }
        });
        let model_override = model_override.or(engine_default.as_deref());

        let mut cfg = Self::resolve_provider_config(
            model_override,
            api_key_override,
            base_url_override,
            settings,
            workspace_root,
        )?;
        cfg.hooks = if crate::enterprise_telemetry::process_free_authority_active() {
            None
        } else {
            settings.hooks.clone()
        };
        cfg.engine_settings = settings.agent_engine_config.clone();
        cfg.authority = settings.authority_policy;
        cfg.tools_bash = settings.bash_tool_enabled() && cfg.authority.bash_allowed;
        cfg.tools_web = settings.web_tools_enabled() && cfg.authority.web_allowed;
        Ok(cfg)
    }

    /// The provider-resolution body of [`Config::load_with_settings`] —
    /// everything except the hooks stamp, so its many early returns stay
    /// exactly as they were.
    fn resolve_provider_config(
        model_override: Option<&str>,
        api_key_override: Option<&str>,
        base_url_override: Option<&str>,
        settings: &crate::settings::Settings,
        workspace_root: std::path::PathBuf,
    ) -> Result<Self, String> {
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
                    // Just a model slug — find which provider has it:
                    // built-in defaults first, then config-defined ones.
                    if let Some(provider) = PROVIDERS.iter().find(|p| p.default_model == model_spec)
                    {
                        let provider = effective_builtin(provider, settings);
                        let settings_key = settings
                            .providers
                            .get(provider.id)
                            .and_then(|e| e.api_key.clone());
                        return Self::resolve(
                            &provider,
                            model_spec.to_string(),
                            api_key_override,
                            base_url_override,
                            settings_key.as_deref(),
                            &mut credentials_file,
                            &workspace_root,
                            true,
                        );
                    }
                    if let Some((id, entry)) = settings.providers.iter().find(|(id, e)| {
                        !PROVIDERS.iter().any(|p| p.id == id.as_str())
                            && e.default_model.as_deref() == Some(model_spec)
                    }) {
                        let provider = custom_provider(id, entry)?;
                        return Self::resolve(
                            &provider,
                            model_spec.to_string(),
                            api_key_override,
                            base_url_override,
                            entry.api_key.as_deref(),
                            &mut credentials_file,
                            &workspace_root,
                            true,
                        );
                    }
                    return Err(format!(
                        "model `{model_spec}` not recognized — use provider/model_id format (e.g. zai/glm-5.2)\navailable providers: {}",
                        all_provider_ids(settings).join(", ")
                    ));
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
                // Track WHERE the local key came from (if anywhere real) so
                // `stella config` reports it accurately — the "local"
                // fallback below is a placeholder bearer token, not a
                // resolved credential, so it gets no source.
                let (api_key, credential_source) = if let Some(flag) = api_key_override {
                    (
                        flag.to_string(),
                        Some(stella_model::credential::CredentialSource::CliFlag),
                    )
                } else if let Some(env_value) = env::var(LOCAL_PROVIDER.env_var)
                    .ok()
                    .filter(|v| !v.is_empty())
                {
                    (
                        env_value,
                        Some(stella_model::credential::CredentialSource::EnvVar),
                    )
                } else {
                    // Most local servers ignore auth; OpenAI-compatible
                    // clients still send *something* as the bearer token.
                    ("local".to_string(), None)
                };
                return Ok(Self {
                    provider: LOCAL_PROVIDER.clone(),
                    model_id,
                    api_key: ApiKey::new(api_key),
                    workspace_root,
                    base_url_override: Some(base_url),
                    hooks: None,
                    engine_settings: None,
                    tools_bash: false,
                    tools_web: false,
                    authority: crate::settings::AuthorityPolicy::default(),
                    credential_source,
                });
            }

            if let Some(provider) = PROVIDERS.iter().find(|p| p.id == provider_id) {
                let provider = effective_builtin(provider, settings);
                let settings_key = settings
                    .providers
                    .get(provider_id)
                    .and_then(|e| e.api_key.clone());
                return Self::resolve(
                    &provider,
                    model_id,
                    api_key_override,
                    base_url_override,
                    settings_key.as_deref(),
                    &mut credentials_file,
                    &workspace_root,
                    true,
                );
            }

            // Not built-in: a settings.json entry can define it outright
            // (issue #44 — Together, Fireworks, a private gateway, …).
            if let Some(entry) = settings.providers.get(provider_id) {
                let provider = custom_provider(provider_id, entry)?;
                return Self::resolve(
                    &provider,
                    model_id,
                    api_key_override,
                    base_url_override,
                    entry.api_key.as_deref(),
                    &mut credentials_file,
                    &workspace_root,
                    true,
                );
            }

            return Err(format!(
                "unknown provider `{provider_id}` — available: {}\n(define new providers in \
                 settings.json under `providers.<id>` with a `base_url`)",
                all_provider_ids(settings).join(", ")
            ));
        }

        // A bare `--api-key` with no `--model` is ambiguous: the key doesn't
        // say which provider it belongs to, and threading it into detection
        // would make the FIRST provider (zai) always "resolve" and get built
        // with a key meant for someone else. Require an explicit provider.
        if api_key_override.is_some() {
            return Err("--api-key needs an explicit --model provider/model_id \
                        (a bare key doesn't say which provider it is for), e.g. \
                        stella --model anthropic/claude-fable-5 --api-key <key>"
                .to_string());
        }

        // No --model: pick the first provider with a resolvable credential
        // (env var/aliases, credentials file, or a settings.json `api_key`
        // — never prompts here, since prompting needs a specific provider
        // in mind and the user hasn't named one). Built-ins keep their
        // preference order; config-defined providers follow, alphabetically
        // (`--model <id>/<model>` pins one regardless). `api_key_override`
        // is `None` on this path (guarded above), so detection reflects
        // only real ambient credentials.
        for provider in PROVIDERS {
            let provider = effective_builtin(provider, settings);
            let settings_key = settings
                .providers
                .get(provider.id)
                .and_then(|e| e.api_key.clone());
            if resolve_provider_key(
                &provider,
                api_key_override,
                settings_key.as_deref(),
                &credentials_file,
                false,
            )
            .is_ok()
            {
                let default_model = provider.default_model.to_string();
                return Self::resolve(
                    &provider,
                    default_model,
                    api_key_override,
                    base_url_override,
                    settings_key.as_deref(),
                    &mut credentials_file,
                    &workspace_root,
                    false,
                );
            }
        }
        for (id, entry) in &settings.providers {
            if PROVIDERS.iter().any(|p| p.id == id.as_str()) || id == LOCAL_PROVIDER.id {
                continue;
            }
            // Auto-detection needs a model to pick; an entry without
            // `default_model` is reachable only via --model <id>/<model>.
            let Some(default_model) = entry.default_model.clone() else {
                continue;
            };
            let provider = custom_provider(id, entry)?;
            if resolve_provider_key(
                &provider,
                api_key_override,
                entry.api_key.as_deref(),
                &credentials_file,
                false,
            )
            .is_ok()
            {
                return Self::resolve(
                    &provider,
                    default_model,
                    api_key_override,
                    base_url_override,
                    entry.api_key.as_deref(),
                    &mut credentials_file,
                    &workspace_root,
                    false,
                );
            }
        }

        Err(format!(
            "no API key found. Set one of: {}\n\nExample: export ZAI_API_KEY=your_key_here\n\
             (or put it in a project .env / .env.local, add it to \
             ~/.config/stella/credentials.toml, or pass --model provider/model to be prompted \
             interactively)",
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
        settings_key: Option<&str>,
        credentials_file: &mut CredentialsFile,
        workspace_root: &std::path::Path,
        interactive: bool,
    ) -> Result<Self, String> {
        let (key, source) = resolve_provider_key(
            provider,
            api_key_override,
            settings_key,
            credentials_file,
            interactive,
        )
        .map_err(|e| {
            format!(
                "could not resolve a credential for {}: {e}",
                provider.display_name
            )
        })?;
        // "Interactive prompt on first use" implies
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
            // Hooks, the agent-engine settings, and the tool switches ride
            // the settings chain, not credential resolution —
            // `load_with_settings` stamps them after the provider resolves.
            hooks: None,
            engine_settings: None,
            tools_bash: false,
            tools_web: false,
            authority: crate::settings::AuthorityPolicy::default(),
            credential_source: Some(source),
        })
    }

    /// Print the provider/model table for an interactive session. The listing
    /// depends only on `PROVIDERS` and the ambient environment, never on
    /// `self`, so it delegates to the static [`Config::print_available_models`]
    /// — one renderer backs both the `/models` REPL command and the top-level
    /// `stella models` subcommand, and they can never drift apart. The REPL
    /// call site has no startup `Loaded` record handy, so its source labels
    /// degrade to the generic `env:VAR` form rather than a specific dotenv
    /// filename — see `credential_status::label_for`.
    pub fn print_models(&self) {
        Self::print_available_models(None);
    }

    /// `loaded_env` is the startup dotenv-load record (main's
    /// `env_files::maybe_load` result) — pass `Some` so the API Key line can
    /// name the exact `.env*` file a key came from, and so the "Env files"
    /// line (always shown, unconditionally — unlike `STELLA_ENV_DEBUG`) can
    /// list which files/names were loaded.
    pub fn print_config(&self, loaded_env: Option<&crate::env_files::Loaded>) {
        println!(
            "{}\n",
            "Stella — Current Configuration".bright_cyan().bold()
        );
        println!(
            "  Provider:   {}",
            self.provider.display_name.bright_magenta()
        );
        println!(
            "  Model:      {}/{}",
            self.provider.id.bright_magenta(),
            self.model_id.bright_white()
        );
        let source = self
            .credential_source
            .map(|s| crate::credential_status::label_for(&self.provider, s, loaded_env))
            .unwrap_or_else(|| "n/a (local placeholder)".to_string());
        println!(
            "  API Key:    {} {}",
            self.api_key.redacted_preview().dimmed(),
            format!("({source})").dimmed()
        );
        println!("  Base URL:   {}", self.effective_base_url().dimmed());
        println!("  Workspace:  {}", self.workspace_root.display());
        println!("  Dialect:    {}", self.provider.dialect.label());
        if let Some(summary) = loaded_env.and_then(crate::credential_status::env_files_summary) {
            println!("  Env files:  {}", summary.dimmed());
        }
    }
}

impl Config {
    /// The credentials file to check provider status against, degrading to
    /// an empty in-memory one (rather than aborting the whole listing) on a
    /// read/parse failure — the listing commands must still show the
    /// built-ins even when `~/.config/stella/credentials.toml` is malformed.
    /// Returns the degradation warning line, if any, alongside the file.
    fn credentials_file_for_listing() -> (CredentialsFile, Option<String>) {
        match CredentialsFile::load_default() {
            Ok(f) => (f, None),
            Err(e) => (
                CredentialsFile::empty(),
                Some(format!(
                    "~/.config/stella/credentials.toml could not be read: {e}"
                )),
            ),
        }
    }

    /// The provider/model table as plain text (no ANSI): the built-in
    /// providers with their key status, then any config-defined providers
    /// from `.stella/settings.json` — the same two sections the ANSI
    /// `print_available_models` renders, so `/models` in the deck lists
    /// exactly what `stella models` does. The Command Deck renders this into
    /// the transcript; stdout printing would corrupt the alternate screen, so
    /// the deck needs a string, not a print. `loaded_env`: see
    /// [`Config::print_config`]'s doc — `None` at call sites that don't have
    /// the startup dotenv record handy (the deck, the REPL fallback).
    pub fn available_models_plain(loaded_env: Option<&crate::env_files::Loaded>) -> String {
        // Surface a settings load/parse failure rather than silently reporting
        // built-in defaults (which would hide a malformed config and wrong key
        // status), then continue with defaults so the listing still renders.
        let (settings, load_error) = match env::current_dir()
            .map_err(|e| e.to_string())
            .and_then(|ws| crate::settings::Settings::load(&ws))
        {
            Ok(s) => (s, None),
            Err(e) => (crate::settings::Settings::default(), Some(e)),
        };
        let (credentials_file, credentials_error) = Self::credentials_file_for_listing();
        let mut lines = vec!["Available providers & models:".to_string()];
        if let Some(e) = &load_error {
            lines.push(format!("  ! settings could not be read: {e}"));
        }
        if let Some(e) = &credentials_error {
            lines.push(format!("  ! {e}"));
        }
        for p in PROVIDERS {
            let p = effective_builtin(p, &settings);
            let settings_key = crate::credential_status::settings_literal_key(&p, &settings);
            let status = crate::credential_status::status_for(
                &p,
                settings_key.as_deref(),
                &credentials_file,
                loaded_env,
            );
            lines.push(format!(
                "  {} {}/{}  {}{}",
                if status.configured { "✓" } else { "✗" },
                p.id,
                p.default_model,
                p.display_name,
                status
                    .source_label
                    .map(|s| format!("  [{s}]"))
                    .unwrap_or_default(),
            ));
        }
        // Config-defined (non-built-in) providers, mirroring the ANSI table.
        let mut printed_header = false;
        for (id, entry) in &settings.providers {
            if PROVIDERS.iter().any(|p| p.id == id.as_str()) || id == LOCAL_PROVIDER.id {
                continue;
            }
            // A malformed provider entry surfaces as a warning line rather than
            // silently vanishing — the ANSI renderer warns here too, and a
            // deck user needs to see that their settings.json is broken.
            let p = match custom_provider(id, entry) {
                Ok(p) => p,
                Err(e) => {
                    lines.push(format!("  ! provider `{id}` is misconfigured: {e}"));
                    continue;
                }
            };
            if !printed_header {
                lines.push("Config-defined providers (settings.json):".to_string());
                printed_header = true;
            }
            let settings_key = entry.api_key.clone();
            let status = crate::credential_status::status_for(
                &p,
                settings_key.as_deref(),
                &credentials_file,
                loaded_env,
            );
            lines.push(format!(
                "  {} {}/{}  {}{}",
                if status.configured { "✓" } else { "✗" },
                p.id,
                if p.default_model.is_empty() {
                    "<model>"
                } else {
                    p.default_model
                },
                p.display_name,
                status
                    .source_label
                    .map(|s| format!("  [{s}]"))
                    .unwrap_or_default(),
            ));
        }
        lines.push("Pin one with --model provider/model_id on the next launch.".to_string());
        lines.join("\n")
    }

    /// Print all available providers/models without needing a resolved
    /// config: the built-in table (with any settings.json overrides
    /// applied), then the config-defined providers. A malformed settings or
    /// credentials file degrades to a warning here — a listing command
    /// should still list the built-ins. `loaded_env`: see
    /// [`Config::print_config`]'s doc.
    pub fn print_available_models(loaded_env: Option<&crate::env_files::Loaded>) {
        let settings = match env::current_dir()
            .map_err(|e| e.to_string())
            .and_then(|ws| crate::settings::Settings::load(&ws))
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  {} {e}", "warning:".yellow());
                crate::settings::Settings::default()
            }
        };
        let (credentials_file, credentials_error) = Self::credentials_file_for_listing();
        if let Some(e) = &credentials_error {
            eprintln!("  {} {e}", "warning:".yellow());
        }
        println!(
            "{}\n",
            "Stella — Available Providers & Models".bright_cyan().bold()
        );
        let key_status = |status: &crate::credential_status::CredentialStatus| {
            if status.configured {
                "✓ configured".green()
            } else {
                "✗ no key".dimmed()
            }
        };
        let source_suffix = |status: &crate::credential_status::CredentialStatus| {
            status
                .source_label
                .as_ref()
                .map(|s| format!(" ({s})").dimmed().to_string())
                .unwrap_or_default()
        };
        for p in PROVIDERS {
            let p = effective_builtin(p, &settings);
            let settings_key = crate::credential_status::settings_literal_key(&p, &settings);
            let status = crate::credential_status::status_for(
                &p,
                settings_key.as_deref(),
                &credentials_file,
                loaded_env,
            );
            println!(
                "  {} {}/{}  {}  [{}]{}",
                key_status(&status),
                p.id.bright_magenta(),
                p.default_model.bright_white(),
                p.display_name,
                p.base_url.dimmed(),
                source_suffix(&status),
            );
        }
        let mut printed_header = false;
        for (id, entry) in &settings.providers {
            if PROVIDERS.iter().any(|p| p.id == id.as_str()) || id == LOCAL_PROVIDER.id {
                continue;
            }
            let p = match custom_provider(id, entry) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("  {} {e}", "warning:".yellow());
                    continue;
                }
            };
            if !printed_header {
                println!("\n  {}", "Config-defined providers (settings.json):".bold());
                printed_header = true;
            }
            let settings_key = entry.api_key.clone();
            let status = crate::credential_status::status_for(
                &p,
                settings_key.as_deref(),
                &credentials_file,
                loaded_env,
            );
            println!(
                "  {} {}/{}  {}  [{}] ({}){}",
                key_status(&status),
                p.id.bright_magenta(),
                if p.default_model.is_empty() {
                    "<model>"
                } else {
                    p.default_model
                }
                .bright_white(),
                p.display_name,
                p.base_url.dimmed(),
                p.dialect.label().dimmed(),
                source_suffix(&status),
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

/// The provider-aware credential chain: CLI flag -> primary env var ->
/// alias env vars -> settings.json `api_key` -> credentials file ->
/// interactive prompt. Wraps `ApiKey::resolve` (which owns everything
/// except aliases and the settings literal) so alias env vars slot in at
/// exactly env-var precedence — after the primary var, before the settings
/// literal. A settings `api_key` outranks the credentials file because it
/// is explicit, scope-merged configuration; the credentials file is the
/// store interactive prompts write into. The returned `CredentialSource`
/// distinguishes the two file-shaped stores (`SettingsJson` for the
/// settings.json literal, `ConfigFile` for a real credentials.toml hit) so
/// callers displaying the source (`stella models`/`stella config`/`stella
/// auth list`) never conflate them. This is the SAME resolution `Config`
/// itself uses — `discover_configured_providers` and the "is this provider
/// configured" status shown by `stella models`/`stella config` both call
/// through here, so status can never disagree with what `stella run` would
/// actually do.
pub(crate) fn resolve_provider_key(
    provider: &ProviderConfig,
    api_key_override: Option<&str>,
    settings_key: Option<&str>,
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
        // var, or a settings.json literal, still outranks the file.
        Ok((key, source)) => {
            for alias in provider.env_var_aliases {
                if let Ok(alias_key) = ApiKey::from_env(alias) {
                    return Ok((alias_key, CredentialSource::EnvVar));
                }
            }
            if let Some(settings_key) = settings_key.filter(|k| !k.is_empty()) {
                return Ok((ApiKey::new(settings_key), CredentialSource::SettingsJson));
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
            if let Some(settings_key) = settings_key.filter(|k| !k.is_empty()) {
                return Ok((ApiKey::new(settings_key), CredentialSource::SettingsJson));
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

/// A provider whose BYOK credential currently resolves, paired with the
/// resolved key. Produced by [`discover_configured_providers`] and consumed
/// by the goal loop's role Router: the `config` supplies the id/family/model
/// for a `stella_core::router::ProviderProfile`, the `api_key` builds the
/// concrete judge adapter when this provider is routed as judge. `api_key`
/// is an [`ApiKey`] (H3) so the derived `Debug` never leaks the secret.
#[derive(Debug, Clone)]
pub struct ConfiguredProvider {
    pub config: ProviderConfig,
    pub api_key: ApiKey,
}

/// Enumerate every provider in [`PROVIDERS`] whose credential currently
/// resolves, in preference order, pairing each with its resolved key. Uses
/// the SAME credential chain [`Config::load`] uses ([`resolve_provider_key`],
/// non-interactively — env var / alias / credentials file, never a prompt),
/// so a provider is "configured" here iff `Config` could have auto-selected
/// it. Never fails: an unreadable credentials file degrades to whatever the
/// environment alone provides.
///
/// The goal loop calls this to build a role Router that can pick a
/// cross-family JUDGE; with one configured family
/// it returns a single entry and the judge stays the worker provider.
pub fn discover_configured_providers() -> Vec<ConfiguredProvider> {
    // A corrupt/unreadable credentials file must not break judge routing —
    // degrade to env-only discovery via an empty in-memory file (an empty
    // path reads as "no file"). If even that fails, discover nothing: the
    // goal loop then simply keeps the worker as judge.
    let Ok(credentials_file) = CredentialsFile::load_default()
        .or_else(|_| CredentialsFile::load(std::path::PathBuf::new()))
    else {
        return Vec::new();
    };
    // Same degradation posture for settings: judge routing is best-effort,
    // so an unreadable settings.json costs the config-defined providers,
    // never the built-ins. (`Config::load` is where a bad file is loud.)
    let settings = env::current_dir()
        .ok()
        .and_then(|ws| crate::settings::Settings::load(&ws).ok())
        .unwrap_or_default();

    let mut configured: Vec<ConfiguredProvider> = PROVIDERS
        .iter()
        .filter_map(|provider| {
            let provider = effective_builtin(provider, &settings);
            let settings_key = settings
                .providers
                .get(provider.id)
                .and_then(|e| e.api_key.clone());
            resolve_provider_key(
                &provider,
                None,
                settings_key.as_deref(),
                &credentials_file,
                false,
            )
            .ok()
            .map(|(api_key, _source)| ConfiguredProvider {
                config: provider,
                api_key,
            })
        })
        .collect();
    for (id, entry) in &settings.providers {
        if PROVIDERS.iter().any(|p| p.id == id.as_str()) || id == LOCAL_PROVIDER.id {
            continue;
        }
        // The judge router needs a model to route to — an entry without
        // `default_model` can't serve as a judge.
        if entry.default_model.as_deref().unwrap_or("").is_empty() {
            continue;
        }
        let Ok(provider) = custom_provider(id, entry) else {
            continue;
        };
        if let Ok((api_key, _)) = resolve_provider_key(
            &provider,
            None,
            entry.api_key.as_deref(),
            &credentials_file,
            false,
        ) {
            configured.push(ConfiguredProvider {
                config: provider,
                api_key,
            });
        }
    }
    configured
}

#[cfg(test)]
mod tests;
