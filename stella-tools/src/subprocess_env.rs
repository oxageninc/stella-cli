//! Environment policy for subprocesses that can execute model- or
//! repository-controlled code.
//!
//! Stella needs provider credentials in its own process, but tools do not.
//! Passing the full inherited environment to a shell, project script, hook,
//! or long-running server lets ordinary repository code print or exfiltrate
//! the credential that pays for the agent. Apply [`scrub_sensitive_env`] as
//! the final environment mutation before every such spawn.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::sync::{OnceLock, RwLock};

/// Credential-bearing AWS variables whose names do not all use one of the
/// generic secret suffixes below. Region variables are intentionally absent:
/// `AWS_REGION` is task configuration, not a credential.
const AWS_CREDENTIAL_ENV_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_SECURITY_TOKEN",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
];

/// Other credential-source variables that provide a child access to tokens
/// without putting the token directly in the variable value.
const CREDENTIAL_SOURCE_ENV_VARS: &[&str] = &[
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AZURE_FEDERATED_TOKEN_FILE",
];

/// Exact environment authentication channels documented by GitHub CLI.
/// Trusted `gh` call sites may preserve these while still removing every
/// model-provider, cloud, database, and unrelated repository secret.
pub const GITHUB_CLI_AUTH_ENV_VARS: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

static REGISTERED_CREDENTIAL_ENV_VARS: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

/// Register exact credential environment names learned from trusted provider
/// configuration.
///
/// Custom providers may use a name such as `CORP_AUTH` that no suffix rule can
/// infer. Registration is monotonic for the process lifetime: once a name has
/// carried a model credential, arbitrary descendants must never inherit it.
pub fn register_sensitive_env_names<I, S>(names: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let registry = REGISTERED_CREDENTIAL_ENV_VARS.get_or_init(|| RwLock::new(HashSet::new()));
    let mut registry = registry
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.extend(names.into_iter().filter_map(|name| {
        let upper = name.as_ref().to_string_lossy().to_ascii_uppercase();
        (!upper.is_empty()).then_some(upper)
    }));
}

/// Whether an environment variable name can carry a credential.
///
/// Matching is ASCII case-insensitive for parity with Windows environment
/// semantics. The suffix policy covers built-in and custom Stella providers
/// (`OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, `VERTEX_ACCESS_TOKEN`, ...)
/// plus common repository credentials such as `GITHUB_TOKEN`. Exact AWS and
/// credential-source names cover the standard chains that do not follow a
/// generic suffix.
pub fn is_sensitive_env_name(name: &OsStr) -> bool {
    let upper = name.to_string_lossy().to_ascii_uppercase();
    matches!(upper.as_str(), "API_KEY" | "TOKEN" | "PASSWORD" | "SECRET")
        || ["_API_KEY", "_TOKEN", "_PASSWORD", "_SECRET"]
            .iter()
            .any(|suffix| upper.ends_with(suffix))
        || AWS_CREDENTIAL_ENV_VARS.contains(&upper.as_str())
        || CREDENTIAL_SOURCE_ENV_VARS.contains(&upper.as_str())
        || REGISTERED_CREDENTIAL_ENV_VARS
            .get()
            .is_some_and(|registry| {
                registry
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains(&upper)
            })
}

fn allowlisted_sensitive_env_is_safe_to_preserve(
    name: &OsStr,
    preserved_names: &[&str],
    registered_as_model_credential: bool,
) -> bool {
    let candidate = name.to_string_lossy();
    preserved_names
        .iter()
        .any(|allowed| candidate.eq_ignore_ascii_case(allowed))
        && !registered_as_model_credential
}

fn is_registered_model_credential_name(name: &OsStr) -> bool {
    let upper = name.to_string_lossy().to_ascii_uppercase();
    REGISTERED_CREDENTIAL_ENV_VARS
        .get()
        .is_some_and(|registry| {
            registry
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&upper)
        })
}

/// Remove credentials from a Tokio command while preserving ordinary task
/// environment such as `PATH`, locale, build flags, and provider-neutral
/// configuration.
///
/// Both inherited variables and explicit command overrides are inspected.
/// The latter matters for custom tool manifests: a repository must not be
/// able to re-introduce `OPENROUTER_API_KEY` through its `[env]` table after
/// the inherited copy was removed.
pub fn scrub_sensitive_env(command: &mut tokio::process::Command) {
    scrub_sensitive_std_env(command.as_std_mut());
}

/// Remove credentials except an exact, integration-owned allowlist.
///
/// This is only for a trusted executable whose documented authentication
/// channel is itself an environment variable (for example `gh`). Arbitrary
/// shells, hooks, test commands, repository tools, and model-selected
/// executables must use [`scrub_sensitive_env`] with no exceptions.
pub fn scrub_sensitive_env_except(command: &mut tokio::process::Command, preserved_names: &[&str]) {
    scrub_sensitive_std_env_except(command.as_std_mut(), preserved_names);
}

/// Synchronous-command counterpart used by the few repository helpers that
/// cannot use Tokio.
pub fn scrub_sensitive_std_env(command: &mut std::process::Command) {
    scrub_sensitive_std_env_except(command, &[]);
}

/// Synchronous counterpart of [`scrub_sensitive_env_except`].
pub fn scrub_sensitive_std_env_except(
    command: &mut std::process::Command,
    preserved_names: &[&str],
) {
    // A provider credential name learned from trusted settings always wins
    // over an integration allowlist collision. For example, if a custom
    // provider is configured to use `GH_TOKEN`, `gh` authentication must fail
    // closed rather than inheriting the model-spend credential.
    let is_preserved = |name: &OsStr| {
        allowlisted_sensitive_env_is_safe_to_preserve(
            name,
            preserved_names,
            is_registered_model_credential_name(name),
        )
    };
    let mut names: Vec<OsString> = std::env::vars_os()
        .filter_map(|(name, _)| {
            (is_sensitive_env_name(&name) && !is_preserved(&name)).then_some(name)
        })
        .collect();

    // `Command::env` entries are not necessarily present in Stella's own
    // environment. Snapshot them before calling `env_remove`, which mutates
    // the iterator returned by `get_envs`.
    names.extend(
        command
            .get_envs()
            .filter(|(name, value)| {
                value.is_some() && is_sensitive_env_name(name) && !is_preserved(name)
            })
            .map(|(name, _)| name.to_os_string())
            .collect::<Vec<_>>(),
    );
    names.extend(
        AWS_CREDENTIAL_ENV_VARS
            .iter()
            .filter(|name| {
                !preserved_names
                    .iter()
                    .any(|allowed| name.eq_ignore_ascii_case(allowed))
            })
            .map(OsString::from),
    );
    names.extend(
        CREDENTIAL_SOURCE_ENV_VARS
            .iter()
            .filter(|name| {
                !preserved_names
                    .iter()
                    .any(|allowed| name.eq_ignore_ascii_case(allowed))
            })
            .map(OsString::from),
    );

    for name in names {
        command.env_remove(name);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Probe that reports only the word `leaked`, never a credential value.
    pub const PROBE_COMMAND: &str = "printf '%s|%s|%s|%s|%s' \
        \"${OPENROUTER_API_KEY:+leaked}\" \
        \"${GITHUB_TOKEN:+leaked}\" \
        \"${AWS_SECRET_ACCESS_KEY:+leaked}\" \
        \"${STELLA_TEST_BENIGN-unset}\" \
        \"${PATH:+present}\"";

    /// Serializes tests that temporarily modify the process environment and
    /// restores the caller's original values even if an assertion panics.
    pub struct InheritedCredentialFixture {
        previous: Vec<(&'static str, Option<OsString>)>,
        _lock: MutexGuard<'static, ()>,
    }

    impl InheritedCredentialFixture {
        pub fn install() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
            let values = [
                ("OPENROUTER_API_KEY", "test-openrouter-secret"),
                ("GITHUB_TOKEN", "test-github-secret"),
                ("AWS_SECRET_ACCESS_KEY", "test-aws-secret"),
                ("STELLA_TEST_BENIGN", "visible"),
            ];
            let previous = values
                .iter()
                .map(|(name, _)| (*name, std::env::var_os(name)))
                .collect();
            for (name, value) in values {
                // SAFETY: all credential-inheritance tests use ENV_LOCK and
                // no production thread is running in this test process.
                unsafe { std::env::set_var(name, value) };
            }
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for InheritedCredentialFixture {
        fn drop(&mut self) {
            for (name, value) in &self.previous {
                // SAFETY: the fixture still owns ENV_LOCK during restoration.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    pub fn assert_scrubbed(output: &str) {
        assert_eq!(
            output, "|||visible|present",
            "credential reached child or benign environment was removed: {output}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_model_credential_outranks_integration_allowlist() {
        let name = OsStr::new("GH_TOKEN");
        assert!(allowlisted_sensitive_env_is_safe_to_preserve(
            name,
            GITHUB_CLI_AUTH_ENV_VARS,
            false,
        ));
        assert!(!allowlisted_sensitive_env_is_safe_to_preserve(
            name,
            GITHUB_CLI_AUTH_ENV_VARS,
            true,
        ));
    }

    #[test]
    fn classifies_provider_repo_and_aws_credentials_without_overmatching() {
        for secret in [
            "OPENROUTER_API_KEY",
            "ANTHROPIC_API_KEY",
            "GITHUB_TOKEN",
            "DATABASE_PASSWORD",
            "STELLA_LINEAR_CLIENT_SECRET",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(
                is_sensitive_env_name(OsStr::new(secret)),
                "{secret} must be classified as sensitive"
            );
        }
        for benign in [
            "PATH",
            "HOME",
            "STELLA_TEST_BENIGN",
            "AWS_REGION",
            "CARGO_TARGET_DIR",
            "TOKENIZERS_PARALLELISM",
        ] {
            assert!(
                !is_sensitive_env_name(OsStr::new(benign)),
                "{benign} must remain available to task subprocesses"
            );
        }
    }

    #[tokio::test]
    async fn removes_explicit_secret_overrides_but_keeps_benign_overrides() {
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-c",
                "printf '%s|%s' \"${CUSTOM_API_KEY-unset}\" \"${STELLA_TEST_BENIGN-unset}\"",
            ])
            .env("CUSTOM_API_KEY", "must-not-leak")
            .env("STELLA_TEST_BENIGN", "kept");
        scrub_sensitive_env(&mut command);
        let output = command.output().await.expect("spawn shell");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "unset|kept");
    }

    #[tokio::test]
    async fn trusted_integration_can_preserve_only_its_exact_auth_names() {
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-c",
                "printf '%s|%s|%s|%s' \"${OPENROUTER_API_KEY-unset}\" \"${GITHUB_TOKEN-unset}\" \"${GH_TOKEN-unset}\" \"${AWS_SECRET_ACCESS_KEY-unset}\"",
            ])
            .env("OPENROUTER_API_KEY", "provider-secret")
            .env("GITHUB_TOKEN", "github-secret")
            .env("GH_TOKEN", "gh-secret")
            .env("AWS_SECRET_ACCESS_KEY", "cloud-secret");
        scrub_sensitive_env_except(&mut command, &["GH_TOKEN", "GITHUB_TOKEN"]);

        let output = command.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "unset|github-secret|gh-secret|unset"
        );
    }

    #[test]
    fn registered_custom_provider_name_is_sensitive_without_a_secret_suffix() {
        assert!(!is_sensitive_env_name(OsStr::new("STELLA_TEST_CORP_AUTH")));
        register_sensitive_env_names(["STELLA_TEST_CORP_AUTH"]);
        assert!(is_sensitive_env_name(OsStr::new("STELLA_TEST_CORP_AUTH")));
    }
}
