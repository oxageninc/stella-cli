//! `stella auth` — manage BYOK provider keys stored in
//! `~/.stella/credentials.toml`. A small, flat store for a handful of
//! keys, not a config language (`CredentialsFile`'s own doc intent) —
//! `set`/`remove`/`list` is the whole surface. Every command here masks the
//! secret value in its own output; the only place a plaintext key is ever
//! written is the 0600 credentials file itself.
//!
//! Every subcommand splits into a pure core (operates on an already-loaded
//! `&mut CredentialsFile`, returns the lines to print — never touches the
//! real filesystem) and a thin `run_*` wrapper that loads/saves the real
//! `~/.stella/credentials.toml`. This is what lets the core be unit
//! tested against a temp-path `CredentialsFile` instead of ever touching a
//! developer's actual credentials file.

use colored::Colorize as _;
use stella_model::credential::{ApiKey, CredentialsFile};

use crate::credential_status;
use crate::settings::Settings;

/// Entry point for `stella auth <cmd>`.
pub fn run(cmd: &crate::AuthCmd) -> Result<(), String> {
    match cmd {
        crate::AuthCmd::Set {
            provider,
            key,
            stdin,
        } => run_set(provider, key.as_deref(), *stdin),
        crate::AuthCmd::Remove { provider } => run_remove(provider),
        crate::AuthCmd::List => run_list(),
    }
}

fn credentials_path_display() -> String {
    CredentialsFile::default_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.stella/credentials.toml".to_string())
}

fn load_file() -> Result<CredentialsFile, String> {
    CredentialsFile::load_default()
        .map_err(|e| format!("could not read {}: {e}", credentials_path_display()))
}

/// Reject ids that would corrupt lookups elsewhere — `config::custom_provider`
/// enforces the same rule for settings.json-defined providers. Otherwise
/// permissive about WHICH id: a key can legitimately be stored ahead of the
/// provider being declared in settings.json, or under one of the built-ins.
fn validate_provider_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.contains('/') || id.chars().any(char::is_whitespace) {
        return Err(format!(
            "`{id}` is not a valid provider id (no slashes or whitespace)"
        ));
    }
    Ok(())
}

/// The key value for `set`, in priority order: `--key` (warned), `--stdin`
/// (one line, trimmed), or an interactive masked prompt (rpassword) —
/// mirroring `stella_model::credential`'s own interactive-prompt idiom and
/// `connect_cmd`'s `prompt_secret`.
fn read_key_value(provider: &str, key: Option<&str>, use_stdin: bool) -> Result<String, String> {
    if let Some(k) = key {
        eprintln!(
            "  {} --key exposes the credential in shell history and `ps` output — prefer \
             `stella auth set {provider} --stdin` or the interactive masked prompt (omit \
             both --key and --stdin)",
            "⚠".yellow()
        );
        if k.is_empty() {
            return Err("--key value is empty".to_string());
        }
        return Ok(k.to_string());
    }
    if use_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .map_err(|e| format!("could not read key from stdin: {e}"))?;
        let trimmed = buf.trim().to_string();
        if trimmed.is_empty() {
            return Err("empty key read from stdin".to_string());
        }
        return Ok(trimmed);
    }
    let value = rpassword::prompt_password(format!("  API key for `{provider}`: "))
        .map_err(|e| format!("cannot read secret from terminal: {e}"))?;
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        return Err("empty input — no credential entered".to_string());
    }
    Ok(trimmed)
}

/// Pure core of `stella auth set`: validates the id, resolves the key value,
/// and stores it into `file` (caller saves). Returns the masked confirmation
/// line — never the raw value.
fn set_in(
    file: &mut CredentialsFile,
    path_display: &str,
    provider: &str,
    key: Option<&str>,
    use_stdin: bool,
) -> Result<String, String> {
    validate_provider_id(provider)?;
    let value = read_key_value(provider, key, use_stdin)?;
    let api_key = ApiKey::new(value);
    file.set(provider, api_key.reveal());
    Ok(format!(
        "stored key for `{provider}` in {path_display} ({})",
        api_key.redacted_preview()
    ))
}

fn run_set(provider: &str, key: Option<&str>, use_stdin: bool) -> Result<(), String> {
    let mut file = load_file()?;
    let path_display = credentials_path_display();
    let message = set_in(&mut file, &path_display, provider, key, use_stdin)?;
    file.save()
        .map_err(|e| format!("failed to save {path_display}: {e}"))?;
    println!("  {} {}", "◆".yellow(), message);
    Ok(())
}

/// Pure core of `stella auth remove`: removes `provider` from `file` if
/// present. Returns `(message, removed)` — the caller only re-saves the
/// file when `removed` is true.
fn remove_from(file: &mut CredentialsFile, path_display: &str, provider: &str) -> (String, bool) {
    if file.remove(provider) {
        (
            format!("removed key for `{provider}` from {path_display}"),
            true,
        )
    } else {
        (
            format!("no stored key for `{provider}` in {path_display} — nothing to remove"),
            false,
        )
    }
}

fn run_remove(provider: &str) -> Result<(), String> {
    let mut file = load_file()?;
    let path_display = credentials_path_display();
    let (message, removed) = remove_from(&mut file, &path_display, provider);
    if removed {
        file.save()
            .map_err(|e| format!("failed to save {path_display}: {e}"))?;
        println!("  {} {}", "◆".yellow(), message);
    } else {
        println!("  {} {}", "·".green(), message);
    }
    Ok(())
}

/// Pure core of `stella auth list`: one display line per stored provider —
/// redacted preview, the `credentials.toml` label, and (when something else
/// currently outranks the stored key in the real resolution chain — an env
/// var, a settings.json literal) a "shadowed by …" note, so this list stays
/// consistent with `stella models`/`stella config` rather than a static
/// echo of the file's own contents. Never includes a raw secret value.
fn list_lines(file: &CredentialsFile, settings: &Settings) -> Vec<String> {
    file.provider_ids()
        .filter_map(|id| {
            let value = file.get(id)?;
            let preview = ApiKey::new(value).redacted_preview();
            let provider = credential_status::provider_config_for(id, settings);
            let settings_key = credential_status::settings_literal_key(&provider, settings);
            let status =
                credential_status::status_for(&provider, settings_key.as_deref(), file, None);
            let note = match status.source_label {
                Some(label) if label != "credentials.toml" => {
                    format!("  (shadowed by {label})")
                }
                _ => String::new(),
            };
            Some(format!("{id}  {preview}  credentials.toml{note}"))
        })
        .collect()
}

fn run_list() -> Result<(), String> {
    let file = load_file()?;
    crate::tui::section_header("Stored credentials");
    let lines = {
        // Best-effort: a malformed settings.json costs the provider-config
        // lookup its overrides, never the listing itself.
        let settings = std::env::current_dir()
            .ok()
            .and_then(|ws| Settings::load(&ws).ok())
            .unwrap_or_default();
        list_lines(&file, &settings)
    };
    if lines.is_empty() {
        println!(
            "  {}",
            "none — run `stella auth set <provider>` to store one.".dimmed()
        );
        return Ok(());
    }
    for line in lines {
        println!("  {} {line}", "◆".yellow());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_credentials_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "stella-test-auth-cmd-{name}-{}.toml",
            std::process::id()
        ))
    }

    #[test]
    fn validate_provider_id_rejects_empty_slash_and_whitespace() {
        assert!(validate_provider_id("zai").is_ok());
        assert!(validate_provider_id("my-gateway").is_ok());
        assert!(validate_provider_id("").is_err());
        assert!(validate_provider_id("zai/glm").is_err());
        assert!(validate_provider_id("has space").is_err());
    }

    // set: witness for masked stdout + 0600 perms on disk

    #[test]
    fn set_in_stores_the_key_but_never_echoes_it_in_the_confirmation_message() {
        let path = temp_credentials_path("set-masked");
        let _ = std::fs::remove_file(&path);
        let mut file = CredentialsFile::load(&path).unwrap();

        let message = set_in(
            &mut file,
            "credentials.toml",
            "zai",
            Some("sk-super-secret-value-1234"),
            false,
        )
        .unwrap();

        assert!(
            !message.contains("sk-super-secret-value-1234"),
            "confirmation message must never contain the raw secret: {message}"
        );
        assert!(message.contains("zai"));
        assert_eq!(file.get("zai"), Some("sk-super-secret-value-1234"));

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn set_then_save_writes_the_real_file_0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_credentials_path("set-perms");
        let _ = std::fs::remove_file(&path);
        let mut file = CredentialsFile::load(&path).unwrap();

        set_in(
            &mut file,
            "credentials.toml",
            "zai",
            Some("sk-value"),
            false,
        )
        .unwrap();
        file.save().unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "stella auth set must leave a 0600 file"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_in_rejects_an_invalid_provider_id_before_touching_the_file() {
        let mut file = CredentialsFile::load(temp_credentials_path("set-invalid")).unwrap();
        let err = set_in(
            &mut file,
            "credentials.toml",
            "zai/glm",
            Some("sk-value"),
            false,
        )
        .unwrap_err();
        assert!(err.contains("not a valid provider id"));
        assert_eq!(file.get("zai/glm"), None);
    }

    // remove

    #[test]
    fn remove_from_reports_success_and_actually_drops_the_entry() {
        let mut file = CredentialsFile::load(temp_credentials_path("remove-hit")).unwrap();
        file.set("zai", "sk-value");

        let (message, removed) = remove_from(&mut file, "credentials.toml", "zai");
        assert!(removed);
        assert!(message.contains("removed"));
        assert_eq!(file.get("zai"), None);
    }

    #[test]
    fn remove_from_reports_absence_without_erroring() {
        let mut file = CredentialsFile::load(temp_credentials_path("remove-miss")).unwrap();
        let (message, removed) = remove_from(&mut file, "credentials.toml", "nonexistent");
        assert!(!removed, "removing an absent entry must not claim success");
        assert!(message.contains("nothing to remove"));
    }

    // list: masked, and consistent with the #249 source labels

    #[test]
    fn list_lines_shows_a_redacted_preview_never_the_raw_value() {
        let mut file = CredentialsFile::load(temp_credentials_path("list-masked")).unwrap();
        file.set("zai", "sk-super-secret-value-1234");
        let settings = Settings::default();

        let lines = list_lines(&file, &settings);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("sk-super-secret-value-1234"));
        assert!(lines[0].contains("zai"));
        assert!(lines[0].contains("credentials.toml"));
    }

    #[test]
    fn list_lines_notes_when_an_env_var_shadows_the_stored_key() {
        let _env = crate::test_env::lock();
        // `provider_config_for` derives `<ID>_API_KEY` (uppercased,
        // punctuation folded to `_`) for an id that matches no built-in or
        // settings.json entry — this id's derived var is
        // `STELLA_TEST_AUTH_LIST_SHADOW_API_KEY` (see
        // `config::derived_env_var`'s own test for the exact rule).
        let provider_id = "stella-test-auth-list-shadow";
        let env_var = "STELLA_TEST_AUTH_LIST_SHADOW_API_KEY";
        // SAFETY: guarded by test_env's lock; a unique var name unused
        // elsewhere.
        unsafe {
            std::env::set_var(env_var, "sk-from-env");
        }
        let mut file = CredentialsFile::load(temp_credentials_path("list-shadow")).unwrap();
        file.set(provider_id, "sk-from-credentials-file");
        let settings = Settings::default();

        let lines = list_lines(&file, &settings);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains(&format!("shadowed by env:{env_var}")),
            "expected a shadow note naming the winning env var, got: {}",
            lines[0]
        );

        unsafe {
            std::env::remove_var(env_var);
        }
    }

    #[test]
    fn list_lines_is_empty_when_nothing_stored() {
        let file = CredentialsFile::load(temp_credentials_path("list-empty")).unwrap();
        assert!(list_lines(&file, &Settings::default()).is_empty());
    }
}
