//! Credential resolution. Never `Display`/`Debug`-leaks the secret value —
//! credentials are never logged, never in trace JSONL
//!
//!
//! Resolution order: CLI flag -> env var ->
//! provider-native config (`~/.config/stella/credentials.toml` here; the AWS
//! profile file and Google ADC file remain deferred — the Bedrock/Vertex
//! adapters take ready credentials from env vars for now, see their module
//! docs) -> interactive prompt on first use, which never silently fails
//! with an opaque provider error.

use std::collections::BTreeMap;
use std::fmt;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum CredentialError {
    #[error(
        "no credential found for `{env_var}` — set the environment variable, add it to \
         ~/.config/stella/credentials.toml, or run interactively to be prompted"
    )]
    NotFound { env_var: String },
    #[error("credential for `{env_var}` is empty")]
    Empty { env_var: String },
    #[error("failed to read credentials file {path}: {message}")]
    FileRead { path: String, message: String },
    #[error("failed to parse credentials file {path}: {message}")]
    FileParse { path: String, message: String },
    #[error("failed to write credentials file {path}: {message}")]
    FileWrite { path: String, message: String },
    #[error("interactive prompt failed: {0}")]
    PromptFailed(String),
}

/// Which step in the resolution chain produced an [`ApiKey`]. Exists so
/// callers (and tests) can assert precedence without inspecting the secret
/// itself, and so a successful interactive prompt can be told to persist —
/// only `Interactive` results get written back to the credentials file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    CliFlag,
    EnvVar,
    ConfigFile,
    Interactive,
}

/// A secret API key. Deliberately has no `Display` and a redacted `Debug`
/// so a stray `println!`/`tracing::info!` can never leak it.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Resolve from an environment variable only. Kept as the narrow,
    /// single-step primitive the full chain (`resolve`) and simpler
    /// call sites build on.
    pub fn from_env(env_var: &str) -> Result<Self, CredentialError> {
        match std::env::var(env_var) {
            Ok(value) if !value.is_empty() => Ok(Self(value)),
            Ok(_) => Err(CredentialError::Empty {
                env_var: env_var.to_string(),
            }),
            Err(_) => Err(CredentialError::NotFound {
                env_var: env_var.to_string(),
            }),
        }
    }

    /// The full resolution chain: CLI flag -> env
    /// var -> `credentials_file` -> interactive prompt. `provider_id` keys
    /// both the credentials-file lookup and, on a successful interactive
    /// prompt, what gets written back so the user is only ever prompted
    /// once. `interactive` gates the last step — headless/non-interactive
    /// callers (CI, `--output-format stream-json`) should pass `false` and
    /// get a clean [`CredentialError::NotFound`] instead of hanging on a
    /// read from a stdin that isn't there.
    pub fn resolve(
        provider_id: &str,
        env_var: &str,
        cli_flag: Option<&str>,
        credentials_file: Option<&CredentialsFile>,
        interactive: bool,
    ) -> Result<(Self, CredentialSource), CredentialError> {
        if let Some(flag_value) = cli_flag
            && !flag_value.is_empty()
        {
            return Ok((Self(flag_value.to_string()), CredentialSource::CliFlag));
        }

        match Self::from_env(env_var) {
            Ok(key) => return Ok((key, CredentialSource::EnvVar)),
            Err(CredentialError::Empty { env_var }) => {
                return Err(CredentialError::Empty { env_var });
            }
            Err(CredentialError::NotFound { .. }) => {} // fall through
            Err(other) => return Err(other),
        }

        if let Some(file) = credentials_file
            && let Some(value) = file.get(provider_id)
        {
            return Ok((Self(value.to_string()), CredentialSource::ConfigFile));
        }

        if interactive && std::io::stdin().is_terminal() {
            let value = prompt_for_key(provider_id, env_var)?;
            return Ok((Self(value), CredentialSource::Interactive));
        }

        Err(CredentialError::NotFound {
            env_var: env_var.to_string(),
        })
    }

    /// The only sanctioned way to read the secret value — for building an
    /// auth header, nothing else.
    pub fn reveal(&self) -> &str {
        &self.0
    }

    /// A non-reversible preview for human display (e.g. `stella config`):
    /// a few leading and trailing characters with the middle elided, so a
    /// user can eyeball *which* key is active without the full secret hitting
    /// the terminal. Char-boundary-safe (never panics on multi-byte input)
    /// and never panics on short keys — a key too short to partially reveal
    /// without effectively exposing it is shown fully masked instead. This is
    /// the safe replacement for the old `&self.api_key[..8]` byte-slice, which
    /// panicked both on keys shorter than 8 bytes and on non-ASCII boundaries.
    pub fn redacted_preview(&self) -> String {
        const HEAD: usize = 6;
        const TAIL: usize = 4;
        let chars: Vec<char> = self.0.chars().collect();
        // Require enough length that head+tail don't overlap and the elided
        // middle actually hides something; otherwise mask entirely.
        if chars.len() <= HEAD + TAIL + 2 {
            return "•".repeat(chars.len().min(8));
        }
        let head: String = chars[..HEAD].iter().collect();
        let tail: String = chars[chars.len() - TAIL..].iter().collect();
        format!("{head}…{tail}")
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ApiKey").field(&"<redacted>").finish()
    }
}

/// Prompt the user on stdin for a provider's API key, masking input (never
/// echoed) since the terminal itself isn't a redaction boundary. Called
/// only when `resolve` has exhausted the flag/env/file steps and the caller
/// opted into interactive mode on an actual TTY.
fn prompt_for_key(provider_id: &str, env_var: &str) -> Result<String, CredentialError> {
    let value = rpassword::prompt_password(format!(
        "No {env_var} found for `{provider_id}`. Enter it now (saved to \
         ~/.config/stella/credentials.toml for next time; input hidden): "
    ))
    .map_err(|e| CredentialError::PromptFailed(e.to_string()))?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(CredentialError::PromptFailed(
            "empty input — no credential entered".into(),
        ));
    }
    Ok(trimmed.to_string())
}

/// `~/.config/stella/credentials.toml` — optional provider keys for users
/// who prefer file storage over env vars. Written
/// with `0600` permissions on Unix (owner read/write only) since it holds
/// secrets in plaintext, same threat model as `~/.ssh/config`.
///
/// Shape: `[credentials]` table, `provider_id = "key"` per row — flat and
/// small on purpose; this is a handful of BYOK keys, not a config language.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct CredentialsFileData {
    #[serde(default)]
    credentials: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct CredentialsFile {
    path: PathBuf,
    data: CredentialsFileData,
}

impl CredentialsFile {
    /// The default path: `~/.config/stella/credentials.toml`. Returns `None`
    /// if the platform has no resolvable home directory (never panics —
    /// callers treat "no credentials file available" as just another
    /// resolution step that falls through).
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        Some(home.join(".config").join("stella").join("credentials.toml"))
    }

    /// Load from `path`. A missing file is not an error — it's the common
    /// case for anyone using env vars — and yields an empty file ready to
    /// be populated by a later interactive prompt + `save`.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, CredentialError> {
        let path = path.into();
        let data = match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).map_err(|e| CredentialError::FileParse {
                path: path.display().to_string(),
                message: e.to_string(),
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CredentialsFileData::default(),
            Err(e) => {
                return Err(CredentialError::FileRead {
                    path: path.display().to_string(),
                    message: e.to_string(),
                });
            }
        };
        Ok(Self { path, data })
    }

    /// Load from the default path (`default_path`), or an empty in-memory
    /// file when no home directory is resolvable — never fails outright,
    /// since a credentials file is always optional.
    pub fn load_default() -> Result<Self, CredentialError> {
        match Self::default_path() {
            Some(path) => Self::load(path),
            None => Ok(Self {
                path: PathBuf::new(),
                data: CredentialsFileData::default(),
            }),
        }
    }

    /// The key stored for `provider_id`, if any.
    pub fn get(&self, provider_id: &str) -> Option<&str> {
        self.data.credentials.get(provider_id).map(String::as_str)
    }

    /// Set (or replace) `provider_id`'s key in memory. Call `save` to
    /// persist — kept separate so a caller can batch multiple sets into one
    /// write.
    pub fn set(&mut self, provider_id: &str, value: impl Into<String>) {
        self.data
            .credentials
            .insert(provider_id.to_string(), value.into());
    }

    /// Write the current in-memory state to disk, creating parent
    /// directories as needed. The write is **atomic** and the secret file is
    /// created with owner-only (`0600`) permissions **from birth** on Unix:
    /// contents go to a sibling temp file opened `0600`, which is then
    /// `rename`d over the target (an atomic filesystem operation on the same
    /// directory). This closes the prior TOCTOU window where the file existed
    /// world-readable (`0644`) between `write` and the follow-up `chmod`, and
    /// guarantees no reader ever sees a half-written credentials file.
    /// (Best-effort on non-Unix — Windows ACLs are a different mechanism,
    /// out of scope here.)
    pub fn save(&self) -> Result<(), CredentialError> {
        if self.path.as_os_str().is_empty() {
            return Err(CredentialError::FileWrite {
                path: "<none>".into(),
                message: "no resolvable home directory — cannot persist credentials".into(),
            });
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CredentialError::FileWrite {
                path: self.path.display().to_string(),
                message: e.to_string(),
            })?;
        }
        let contents =
            toml::to_string_pretty(&self.data).map_err(|e| CredentialError::FileWrite {
                path: self.path.display().to_string(),
                message: e.to_string(),
            })?;

        // Temp file in the SAME directory so the final `rename` is atomic
        // (a cross-filesystem rename would fall back to copy+delete and lose
        // atomicity). Unique per-process suffix avoids clobbering a temp file
        // a parallel writer is using.
        let tmp_path = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        write_secret_file(&tmp_path, contents.as_bytes()).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp_path);
        })?;
        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            CredentialError::FileWrite {
                path: self.path.display().to_string(),
                message: e.to_string(),
            }
        })?;
        Ok(())
    }
}

fn write_err(path: &Path, e: std::io::Error) -> CredentialError {
    CredentialError::FileWrite {
        path: path.display().to_string(),
        message: e.to_string(),
    }
}

/// Write `bytes` to `path`, creating the file with `0600` permissions from
/// the moment it exists on Unix (via `OpenOptions::mode`, applied at
/// creation — never a create-then-chmod race). `0600` has no group/other
/// bits, so it is unaffected by any reasonable `umask`.
#[cfg(unix)]
fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), CredentialError> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| write_err(path, e))?;
    // Belt-and-suspenders: if the temp path already existed (e.g. a crashed
    // prior run), `mode` on `open` does not reset an existing file's perms,
    // so force them here before writing any secret bytes.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| write_err(path, e))?;
    file.write_all(bytes).map_err(|e| write_err(path, e))?;
    file.flush().map_err(|e| write_err(path, e))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), CredentialError> {
    std::fs::write(path, bytes).map_err(|e| write_err(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_credentials_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "stella-test-credentials-{name}-{}.toml",
            std::process::id()
        ))
    }

    // ---- ApiKey::from_env (unchanged narrow primitive) ---------------

    #[test]
    fn debug_never_prints_the_secret_value() {
        let key = ApiKey::new("sk-super-secret-value");
        let debug = format!("{key:?}");
        assert!(!debug.contains("sk-super-secret-value"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn redacted_preview_masks_short_keys_without_panicking_or_revealing() {
        // The old `&key[..8]` panicked on keys shorter than 8 bytes; the
        // preview must never panic and must not echo a short secret verbatim.
        for short in ["", "a", "short", "sk-1234"] {
            let preview = ApiKey::new(short).redacted_preview();
            assert!(
                !preview.contains(short) || short.is_empty(),
                "short key `{short}` leaked in preview `{preview}`"
            );
            assert!(
                !preview
                    .chars()
                    .any(|c| c.is_ascii_alphanumeric() && short.contains(c))
            );
        }
    }

    #[test]
    fn redacted_preview_handles_non_ascii_keys_without_panicking() {
        // The old byte-slice `&key[..8]` panicked on a non-char boundary;
        // a multi-byte key must be previewed safely.
        let preview = ApiKey::new("kéy-with-mültibyte-çharacters-1234").redacted_preview();
        assert!(preview.contains('…'));
        assert!(!preview.contains("mültibyte"));
    }

    #[test]
    fn redacted_preview_shows_head_and_tail_but_elides_the_middle() {
        let preview = ApiKey::new("sk-abcdefghijklmnopqrstuvwxyz-9876").redacted_preview();
        assert!(preview.starts_with("sk-abc"), "{preview}");
        assert!(preview.ends_with("9876"), "{preview}");
        assert!(preview.contains('…'));
        assert!(
            !preview.contains("ghijkl"),
            "middle must be elided: {preview}"
        );
    }

    #[test]
    fn from_env_missing_var_is_not_found() {
        let err = ApiKey::from_env("STELLA_TEST_DEFINITELY_UNSET_VAR_12345").unwrap_err();
        assert_eq!(
            err,
            CredentialError::NotFound {
                env_var: "STELLA_TEST_DEFINITELY_UNSET_VAR_12345".into()
            }
        );
    }

    #[test]
    fn from_env_present_var_resolves_and_reveals() {
        // SAFETY: test-only env mutation, unique var name per test.
        unsafe {
            std::env::set_var("STELLA_TEST_CREDENTIAL_VAR", "sk-test-value");
        }
        let key = ApiKey::from_env("STELLA_TEST_CREDENTIAL_VAR").unwrap();
        assert_eq!(key.reveal(), "sk-test-value");
        unsafe {
            std::env::remove_var("STELLA_TEST_CREDENTIAL_VAR");
        }
    }

    #[test]
    fn from_env_empty_var_is_reported_as_empty_not_not_found() {
        unsafe {
            std::env::set_var("STELLA_TEST_EMPTY_CREDENTIAL_VAR", "");
        }
        let err = ApiKey::from_env("STELLA_TEST_EMPTY_CREDENTIAL_VAR").unwrap_err();
        assert_eq!(
            err,
            CredentialError::Empty {
                env_var: "STELLA_TEST_EMPTY_CREDENTIAL_VAR".into()
            }
        );
        unsafe {
            std::env::remove_var("STELLA_TEST_EMPTY_CREDENTIAL_VAR");
        }
    }

    // ---- CredentialsFile -------------------------------------------------

    #[test]
    fn missing_file_loads_as_empty_not_an_error() {
        let path = temp_credentials_path("missing");
        let _ = std::fs::remove_file(&path);
        let file = CredentialsFile::load(&path).expect("missing file is not an error");
        assert_eq!(file.get("zai"), None);
    }

    #[test]
    fn set_then_save_then_load_round_trips() {
        let path = temp_credentials_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let mut file = CredentialsFile::load(&path).unwrap();
        file.set("zai", "sk-zai-secret");
        file.set("anthropic", "sk-ant-secret");
        file.save().expect("save should succeed");

        let reloaded = CredentialsFile::load(&path).unwrap();
        assert_eq!(reloaded.get("zai"), Some("sk-zai-secret"));
        assert_eq!(reloaded.get("anthropic"), Some("sk-ant-secret"));
        assert_eq!(reloaded.get("openai"), None);

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_read_write_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_credentials_path("perms");
        let _ = std::fs::remove_file(&path);

        let mut file = CredentialsFile::load(&path).unwrap();
        file.set("zai", "sk-secret");
        file.save().unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_existing_credentials_stays_0600_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_credentials_path("overwrite");
        let _ = std::fs::remove_file(&path);

        // First save creates the file.
        let mut file = CredentialsFile::load(&path).unwrap();
        file.set("zai", "sk-first");
        file.save().unwrap();

        // Second save must atomically replace it, keep 0600 (from the temp
        // file's birth mode, carried through the rename), and update content.
        file.set("zai", "sk-second-longer-value");
        file.save().unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "overwrite must preserve 0600");

        let reloaded = CredentialsFile::load(&path).unwrap();
        assert_eq!(reloaded.get("zai"), Some("sk-second-longer-value"));

        // No `.tmp.<pid>` sibling left behind after a successful rename.
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        assert!(
            !tmp.exists(),
            "temp file must be renamed away, not left on disk"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_toml_is_a_named_parse_error() {
        let path = temp_credentials_path("malformed");
        std::fs::write(&path, "this is not valid toml {{{").unwrap();
        let err = CredentialsFile::load(&path).unwrap_err();
        assert!(matches!(err, CredentialError::FileParse { .. }));
        let _ = std::fs::remove_file(&path);
    }

    // ---- ApiKey::resolve precedence (per-provider-per-source) --------
    //
    // Each test below exercises the chain for a distinct provider id/env
    // var pair so tests can run in parallel without racing on shared env
    // state.

    #[test]
    fn resolve_cli_flag_wins_over_everything_else() {
        unsafe {
            std::env::set_var("STELLA_TEST_R1_KEY", "from-env");
        }
        let mut file = CredentialsFile::load(temp_credentials_path("r1")).unwrap();
        file.set("r1", "from-file");

        let (key, source) = ApiKey::resolve(
            "r1",
            "STELLA_TEST_R1_KEY",
            Some("from-flag"),
            Some(&file),
            false,
        )
        .unwrap();
        assert_eq!(key.reveal(), "from-flag");
        assert_eq!(source, CredentialSource::CliFlag);

        unsafe {
            std::env::remove_var("STELLA_TEST_R1_KEY");
        }
    }

    #[test]
    fn resolve_env_var_wins_over_config_file() {
        unsafe {
            std::env::set_var("STELLA_TEST_R2_KEY", "from-env");
        }
        let mut file = CredentialsFile::load(temp_credentials_path("r2")).unwrap();
        file.set("r2", "from-file");

        let (key, source) =
            ApiKey::resolve("r2", "STELLA_TEST_R2_KEY", None, Some(&file), false).unwrap();
        assert_eq!(key.reveal(), "from-env");
        assert_eq!(source, CredentialSource::EnvVar);

        unsafe {
            std::env::remove_var("STELLA_TEST_R2_KEY");
        }
    }

    #[test]
    fn resolve_falls_through_to_config_file_when_no_flag_or_env() {
        unsafe {
            std::env::remove_var("STELLA_TEST_R3_KEY");
        }
        let mut file = CredentialsFile::load(temp_credentials_path("r3")).unwrap();
        file.set("r3", "from-file");

        let (key, source) =
            ApiKey::resolve("r3", "STELLA_TEST_R3_KEY", None, Some(&file), false).unwrap();
        assert_eq!(key.reveal(), "from-file");
        assert_eq!(source, CredentialSource::ConfigFile);
    }

    #[test]
    fn resolve_with_nothing_configured_and_non_interactive_is_a_named_not_found() {
        unsafe {
            std::env::remove_var("STELLA_TEST_R4_KEY");
        }
        let err = ApiKey::resolve("r4", "STELLA_TEST_R4_KEY", None, None, false).unwrap_err();
        assert_eq!(
            err,
            CredentialError::NotFound {
                env_var: "STELLA_TEST_R4_KEY".into()
            }
        );
    }

    #[test]
    fn resolve_never_prompts_when_interactive_is_true_but_stdin_is_not_a_terminal() {
        // Test harnesses never run with a real TTY on stdin, so this also
        // guards against the suite hanging on a read that would otherwise
        // block forever waiting for input that will never arrive.
        unsafe {
            std::env::remove_var("STELLA_TEST_R5_KEY");
        }
        let err = ApiKey::resolve("r5", "STELLA_TEST_R5_KEY", None, None, true).unwrap_err();
        assert_eq!(
            err,
            CredentialError::NotFound {
                env_var: "STELLA_TEST_R5_KEY".into()
            }
        );
    }

    #[test]
    fn resolve_empty_env_var_is_reported_as_empty_not_a_silent_fall_through() {
        // An explicitly-set-but-empty env var is a user mistake worth
        // surfacing distinctly, not something that should silently fall
        // through to the config file as if the var were merely unset.
        unsafe {
            std::env::set_var("STELLA_TEST_R6_KEY", "");
        }
        let err = ApiKey::resolve("r6", "STELLA_TEST_R6_KEY", None, None, false).unwrap_err();
        assert_eq!(
            err,
            CredentialError::Empty {
                env_var: "STELLA_TEST_R6_KEY".into()
            }
        );
        unsafe {
            std::env::remove_var("STELLA_TEST_R6_KEY");
        }
    }
}
