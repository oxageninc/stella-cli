//! `settings.json` — declarative provider configuration (issue #44).
//!
//! Three scopes, merged per provider id, per field, in ascending precedence:
//!
//! 1. user:        `~/.config/stella/settings.json`
//! 2. org-managed: `/Library/Application Support/stella/settings.json` on
//!    macOS, `/etc/stella/settings.json` elsewhere (override the path with
//!    `STELLA_MANAGED_SETTINGS` — also how tests point at a fixture)
//! 3. project:     `<workspace>/.stella/settings.json`
//!
//! An entry whose id matches a built-in provider OVERRIDES that provider's
//! defaults (display name, base URL, default model, credential source). An
//! entry with a new id DEFINES a whole new provider — `base_url` becomes
//! required and `dialect` picks the wire adapter (`config.rs` synthesizes
//! the `ProviderConfig`). A malformed file is a hard, named error rather
//! than a silent skip: a typo that quietly reverted someone to a built-in
//! endpoint would be far worse than a loud parse failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::Dialect;

/// One `providers.<id>` entry. Every field is optional at the schema level;
/// which ones are *required* depends on whether the id names a built-in
/// (override: any subset is fine) or defines a new provider (`base_url`
/// must be present). `config.rs` enforces that split.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ProviderSettings {
    /// Optional restatement of the map key (the issue's examples carry it);
    /// when present it must match the key, so a copy-paste of one entry
    /// under a new key can't silently configure the wrong provider.
    pub id: Option<String>,
    /// Display name (`ProviderConfig::display_name`).
    pub name: Option<String>,
    pub base_url: Option<String>,
    /// A literal credential. Sits below env vars and above the interactive
    /// prompt in the chain, mirroring the credentials file. Prefer
    /// `api_key_env` for anything long-lived — settings.json is often
    /// committed, credentials should not be.
    pub api_key: Option<String>,
    /// Name of an environment variable to read the credential from.
    pub api_key_env: Option<String>,
    pub default_model: Option<String>,
    /// Wire dialect for config-defined providers. Defaults to
    /// `openai-compatible`; ignored for built-in overrides (a built-in's
    /// dialect is fixed by its adapter).
    pub dialect: Option<Dialect>,
}

impl ProviderSettings {
    /// Overlay `other` (higher precedence) onto `self`, field by field, so
    /// e.g. an org-managed base URL and a user-scope api_key_env compose
    /// instead of the whole entry being replaced wholesale.
    fn overlay(&mut self, other: &ProviderSettings) {
        macro_rules! take {
            ($field:ident) => {
                if other.$field.is_some() {
                    self.$field = other.$field.clone();
                }
            };
        }
        take!(id);
        take!(name);
        take!(base_url);
        take!(api_key);
        take!(api_key_env);
        take!(default_model);
        take!(dialect);
    }
}

/// The merged view of every settings.json in scope.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderSettings>,
}

impl Settings {
    /// Load and merge the standard scope chain for `workspace_root`.
    /// Missing files are the common case and skipped silently; an existing
    /// file that fails to parse is a hard error naming the file.
    pub fn load(workspace_root: &Path) -> Result<Self, String> {
        let mut paths: Vec<PathBuf> = Vec::new();
        // Ascending precedence: user, org-managed, project.
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            paths.push(home.join(".config").join("stella").join("settings.json"));
        }
        paths.push(managed_settings_path());
        paths.push(workspace_root.join(".stella").join("settings.json"));
        Self::load_from(&paths)
    }

    /// Merge the files at `paths`, later paths taking precedence. Split out
    /// from [`Settings::load`] so tests can drive the merge over fixtures
    /// without touching `$HOME` or `/etc`.
    pub fn load_from(paths: &[PathBuf]) -> Result<Self, String> {
        let mut merged = Settings::default();
        for path in paths {
            let contents = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
            };
            let scope: Settings = serde_json::from_str(&contents)
                .map_err(|e| format!("invalid settings file {}: {e}", path.display()))?;
            for (id, entry) in &scope.providers {
                if let Some(stated) = &entry.id
                    && stated != id
                {
                    return Err(format!(
                        "settings file {}: providers.{id} declares id `{stated}` — the \
                         entry's id must match its key",
                        path.display()
                    ));
                }
                merged
                    .providers
                    .entry(id.clone())
                    .or_default()
                    .overlay(entry);
            }
        }
        Ok(merged)
    }
}

/// The org-managed scope path. `STELLA_MANAGED_SETTINGS` overrides the
/// platform default so fleets can mount it anywhere (and tests can point at
/// a fixture instead of `/etc`).
fn managed_settings_path() -> PathBuf {
    if let Some(p) = std::env::var_os("STELLA_MANAGED_SETTINGS") {
        return PathBuf::from(p);
    }
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/stella/settings.json")
    } else {
        PathBuf::from("/etc/stella/settings.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, json: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn missing_files_merge_to_empty_settings() {
        let settings = Settings::load_from(&[PathBuf::from("/nonexistent/settings.json")]).unwrap();
        assert!(settings.providers.is_empty());
    }

    #[test]
    fn later_scopes_overlay_earlier_ones_field_by_field() {
        let dir = tempfile::tempdir().unwrap();
        let user = write(
            dir.path(),
            "user.json",
            r#"{"providers": {"together": {
                "base_url": "https://user.example/v1",
                "api_key_env": "TOGETHER_KEY",
                "default_model": "user-model"
            }}}"#,
        );
        let project = write(
            dir.path(),
            "project.json",
            r#"{"providers": {"together": {
                "base_url": "https://project.example/v1",
                "dialect": "openai-compatible"
            }}}"#,
        );
        let merged = Settings::load_from(&[user, project]).unwrap();
        let entry = &merged.providers["together"];
        // Project wins where it speaks…
        assert_eq!(
            entry.base_url.as_deref(),
            Some("https://project.example/v1")
        );
        assert_eq!(entry.dialect, Some(Dialect::OpenaiCompatible));
        // …and user-scope fields it left unset survive.
        assert_eq!(entry.api_key_env.as_deref(), Some("TOGETHER_KEY"));
        assert_eq!(entry.default_model.as_deref(), Some("user-model"));
    }

    #[test]
    fn a_parse_error_is_a_hard_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(dir.path(), "bad.json", "{ not json");
        let err = Settings::load_from(std::slice::from_ref(&bad)).unwrap_err();
        assert!(err.contains(&bad.display().to_string()), "{err}");
    }

    #[test]
    fn a_mismatched_inner_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(
            dir.path(),
            "mismatch.json",
            r#"{"providers": {"together": {"id": "fireworks"}}}"#,
        );
        let err = Settings::load_from(&[bad]).unwrap_err();
        assert!(err.contains("must match its key"), "{err}");
    }

    #[test]
    fn unknown_dialects_are_rejected_with_the_valid_set() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(
            dir.path(),
            "dialect.json",
            r#"{"providers": {"x": {"dialect": "smoke-signals"}}}"#,
        );
        let err = Settings::load_from(&[bad]).unwrap_err();
        assert!(err.contains("invalid settings file"), "{err}");
    }
}
