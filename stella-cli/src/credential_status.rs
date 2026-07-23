//! Shared "is this provider configured, and where did the key come from"
//! logic for `stella models`, `stella config`, and `stella auth list`.
//!
//! All three surfaces must report the exact same verdict, because they are
//! all describing the SAME resolution chain `stella run` actually uses
//! (`config::resolve_provider_key`, run non-interactively so a listing
//! command never blocks on a prompt). Before this module existed, `stella
//! models`/`stella config` hand-rolled a narrower check that only looked at
//! env vars and a settings.json literal — silently ignoring
//! `~/.stella/credentials.toml`, so a provider keyed ONLY there
//! showed as unconfigured even though `stella run` resolved it fine. Routing
//! every status check through `resolve_provider_key` closes that gap for
//! good: the display can no longer disagree with real behavior because it
//! is asking the real resolver, not reimplementing a piece of it.

use crate::config::{self, ProviderConfig};
use crate::env_files::Loaded;
use crate::settings::Settings;
use stella_model::credential::{CredentialSource, CredentialsFile};

/// One provider's resolved credential status for display.
pub struct CredentialStatus {
    pub configured: bool,
    /// A short human label for WHERE the key came from — `env:VAR_NAME`,
    /// a loaded dotenv filename (`.env.local`), `credentials.toml`, or
    /// `settings.json`. `None` iff `configured` is `false`.
    pub source_label: Option<String>,
}

/// The settings.json literal `api_key` configured for `provider.id`, if
/// any — the lookup `config.rs` performs inline at each of its resolution
/// call sites, centralized here so every caller of [`status_for`] shares it.
pub fn settings_literal_key(provider: &ProviderConfig, settings: &Settings) -> Option<String> {
    settings
        .providers
        .get(provider.id)
        .and_then(|e| e.api_key.clone())
}

/// Resolve `provider`'s credential status via the exact same chain
/// `Config::load` uses ([`config::resolve_provider_key`], non-interactively
/// — no prompt), then attach a display label for the winning source.
/// `loaded_env` is the startup dotenv-load record (`env_files::maybe_load`'s
/// result); pass `None` when it isn't available in the caller's context —
/// the label degrades to the generic `env:VAR_NAME` form instead of the
/// specific loaded filename.
pub fn status_for(
    provider: &ProviderConfig,
    settings_key: Option<&str>,
    credentials_file: &CredentialsFile,
    loaded_env: Option<&Loaded>,
) -> CredentialStatus {
    match config::resolve_provider_key(provider, None, settings_key, credentials_file, false) {
        Ok((_, source)) => CredentialStatus {
            configured: true,
            source_label: Some(label_for(provider, source, loaded_env)),
        },
        Err(_) => CredentialStatus {
            configured: false,
            source_label: None,
        },
    }
}

/// The [`ProviderConfig`] to resolve `id` against for display purposes: a
/// built-in (with any settings.json override applied), a settings-defined
/// custom provider, or — for an id that matches neither (e.g. a key stored
/// via `stella auth set` ahead of the provider being declared in
/// settings.json) — a minimal synthesized one using the same
/// `<ID>_API_KEY` convention `config::custom_provider` falls back to.
pub fn provider_config_for(id: &str, settings: &Settings) -> ProviderConfig {
    if let Some(p) = config::PROVIDERS.iter().find(|p| p.id == id) {
        return config::effective_builtin(p, settings);
    }
    if let Some(entry) = settings.providers.get(id)
        && let Ok(p) = config::custom_provider(id, entry)
    {
        return p;
    }
    ProviderConfig {
        id: Box::leak(id.to_string().into_boxed_str()),
        env_var: Box::leak(config::derived_env_var(id).into_boxed_str()),
        env_var_aliases: &[],
        display_name: Box::leak(id.to_string().into_boxed_str()),
        default_model: "",
        base_url: "",
        dialect: config::Dialect::OpenaiCompatible,
        seeded: false,
    }
}

/// A one-line summary of which project `.env*` files contributed which
/// variable NAMES (never values) — the same information `STELLA_ENV_DEBUG`
/// prints to stderr, but surfaced unconditionally in `stella config` rather
/// than gated behind that flag. `None` when nothing was loaded.
pub fn env_files_summary(loaded: &Loaded) -> Option<String> {
    if loaded.names.is_empty() {
        return None;
    }
    let files = loaded
        .files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("{files} ({})", loaded.names.join(", ")))
}

/// The display label for a resolved [`CredentialSource`]: which env var
/// actually matched (and, if it was loaded from a project dotenv file, that
/// file's name instead of the generic `env:` form), or which store.
pub fn label_for(
    provider: &ProviderConfig,
    source: CredentialSource,
    loaded_env: Option<&Loaded>,
) -> String {
    match source {
        CredentialSource::CliFlag => "--api-key flag".to_string(),
        CredentialSource::EnvVar => {
            let var = std::iter::once(&provider.env_var)
                .chain(provider.env_var_aliases)
                .find(|v| std::env::var(*v).map(|s| !s.is_empty()).unwrap_or(false));
            let Some(var) = var else {
                // Unreachable in practice (resolve_provider_key only
                // returns EnvVar when a var actually matched) — degrade to
                // the primary var name rather than panic.
                return format!("env:{}", provider.env_var);
            };
            match loaded_env.and_then(|l| l.file_for(var)) {
                Some(path) => file_label(path),
                None => format!("env:{var}"),
            }
        }
        CredentialSource::ConfigFile => "credentials.toml".to_string(),
        CredentialSource::SettingsJson => "settings.json".to_string(),
        // Just prompted — and `Config::resolve` persists a successful
        // interactive prompt to credentials.toml before this label is ever
        // shown, so that's where the key now actually lives.
        CredentialSource::Interactive => "credentials.toml (just entered)".to_string(),
    }
}

fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Dialect;
    use std::path::PathBuf;

    fn test_provider(id: &'static str, env_var: &'static str) -> ProviderConfig {
        ProviderConfig {
            id,
            env_var,
            env_var_aliases: &[],
            display_name: "Test Provider",
            default_model: "m",
            base_url: "https://x.example/v1",
            dialect: Dialect::OpenaiCompatible,
            seeded: false,
        }
    }

    fn temp_credentials_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "stella-test-credstatus-{name}-{}.toml",
            std::process::id()
        ))
    }

    // status_for: the actual #249 root-cause witness
    //
    // Before this module existed, `stella models`/`stella config` never
    // consulted the credentials file at all when deciding "configured or
    // not" — only env vars and a settings.json literal. This is the exact
    // scenario that silently showed `✗ no key` for a provider keyed only
    // via credentials.toml.

    #[test]
    fn status_for_reports_configured_via_credentials_toml_alone() {
        let _env = crate::test_env::lock();
        // SAFETY: guarded by test_env's lock; unique var name, never used
        // by any other test.
        unsafe {
            std::env::remove_var("STELLA_TEST_CREDSTATUS_TOML_ONLY_KEY");
        }
        let provider = test_provider(
            "credstatus-toml-only",
            "STELLA_TEST_CREDSTATUS_TOML_ONLY_KEY",
        );
        let mut file = CredentialsFile::load(temp_credentials_path("toml-only")).unwrap();
        file.set("credstatus-toml-only", "sk-from-credentials-file");

        let status = status_for(&provider, None, &file, None);
        assert!(
            status.configured,
            "a provider keyed only via credentials.toml must show as configured"
        );
        assert_eq!(status.source_label.as_deref(), Some("credentials.toml"));
    }

    #[test]
    fn status_for_reports_not_configured_when_nothing_resolves() {
        let _env = crate::test_env::lock();
        unsafe {
            std::env::remove_var("STELLA_TEST_CREDSTATUS_UNSET_KEY");
        }
        let provider = test_provider("credstatus-unset", "STELLA_TEST_CREDSTATUS_UNSET_KEY");
        let file = CredentialsFile::load(temp_credentials_path("unset")).unwrap();

        let status = status_for(&provider, None, &file, None);
        assert!(!status.configured);
        assert_eq!(status.source_label, None);
    }

    // label_for: source distinction + .env file attribution

    #[test]
    fn label_for_settings_json_is_distinct_from_credentials_toml() {
        let provider = test_provider("label-test-a", "STELLA_TEST_LABEL_A_KEY");
        assert_eq!(
            label_for(&provider, CredentialSource::SettingsJson, None),
            "settings.json"
        );
        assert_eq!(
            label_for(&provider, CredentialSource::ConfigFile, None),
            "credentials.toml"
        );
    }

    #[test]
    fn label_for_env_var_names_the_loaded_dotenv_file_when_known() {
        let _env = crate::test_env::lock();
        let provider = test_provider("label-test-b", "STELLA_TEST_LABEL_B_KEY");
        unsafe {
            std::env::set_var("STELLA_TEST_LABEL_B_KEY", "sk-value");
        }
        let local_file = PathBuf::from("/proj/.env.local");
        let loaded = Loaded {
            files: vec![local_file.clone()],
            names: vec!["STELLA_TEST_LABEL_B_KEY".to_string()],
            name_files: [("STELLA_TEST_LABEL_B_KEY".to_string(), local_file)]
                .into_iter()
                .collect(),
        };
        assert_eq!(
            label_for(&provider, CredentialSource::EnvVar, Some(&loaded)),
            ".env.local"
        );
        unsafe {
            std::env::remove_var("STELLA_TEST_LABEL_B_KEY");
        }
    }

    #[test]
    fn label_for_env_var_falls_back_to_generic_form_without_loaded_env() {
        let _env = crate::test_env::lock();
        let provider = test_provider("label-test-c", "STELLA_TEST_LABEL_C_KEY");
        unsafe {
            std::env::set_var("STELLA_TEST_LABEL_C_KEY", "sk-value");
        }
        assert_eq!(
            label_for(&provider, CredentialSource::EnvVar, None),
            "env:STELLA_TEST_LABEL_C_KEY"
        );
        unsafe {
            std::env::remove_var("STELLA_TEST_LABEL_C_KEY");
        }
    }

    #[test]
    fn env_files_summary_lists_files_and_names_never_values() {
        let loaded = Loaded {
            files: vec![
                PathBuf::from("/proj/.env.local"),
                PathBuf::from("/proj/.env"),
            ],
            names: vec!["OPENROUTER_API_KEY".to_string(), "FOO".to_string()],
            name_files: Default::default(),
        };
        let summary = env_files_summary(&loaded).unwrap();
        assert!(summary.contains(".env.local"));
        assert!(summary.contains(".env"));
        assert!(summary.contains("OPENROUTER_API_KEY"));
        assert!(summary.contains("FOO"));
    }

    #[test]
    fn env_files_summary_is_none_when_nothing_loaded() {
        assert!(env_files_summary(&Loaded::default()).is_none());
    }

    // provider_config_for: unknown-id fallback for `stella auth list`

    #[test]
    fn provider_config_for_unknown_id_derives_the_conventional_env_var() {
        let settings = Settings::default();
        let p = provider_config_for("my-custom-gateway", &settings);
        assert_eq!(p.id, "my-custom-gateway");
        assert_eq!(p.env_var, "MY_CUSTOM_GATEWAY_API_KEY");
    }

    #[test]
    fn provider_config_for_known_builtin_uses_its_real_env_var() {
        let settings = Settings::default();
        let p = provider_config_for("anthropic", &settings);
        assert_eq!(p.env_var, "ANTHROPIC_API_KEY");
    }
}
