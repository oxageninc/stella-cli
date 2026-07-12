//! A redacted API-key holder for the media adapters' BYOK keys.
//!
//! This is a deliberately minimal, self-contained copy of
//! `stella-model`'s `ApiKey` (same redacted-`Debug`, `reveal`-only contract).
//! `stella-media` may not depend on `stella-model` (parallel-workstream
//! isolation — see the crate-level doc), so the secret type is duplicated
//! here rather than imported. The security contract is identical to
//! `02-architecture.md` §8: the value is never `Display`ed, its `Debug` is
//! redacted, and the only accessor is [`ApiKey::reveal`], used solely to
//! build an `Authorization` header.
//!
//! Full credential *resolution* (CLI flag → env → file → interactive prompt)
//! stays in `stella-model`/CLI glue; this crate only needs to hold a key it
//! was handed and read it from an env var for the live-smoke tests.

use std::fmt;

use thiserror::Error;

/// Failure resolving a media provider's key from the environment.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum CredentialError {
    /// The env var is not set.
    #[error("no credential found for `{env_var}` — set the environment variable")]
    NotFound { env_var: String },
    /// The env var is set but empty (a user mistake worth surfacing
    /// distinctly from "unset").
    #[error("credential for `{env_var}` is empty")]
    Empty { env_var: String },
}

/// A secret API key. No `Display`; a redacted `Debug`; the value is only
/// reachable through [`ApiKey::reveal`]. A stray `println!`/log line can
/// therefore never leak it.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wrap a key value handed in by the caller (CLI glue, tests).
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Resolve from a single environment variable — the narrow primitive the
    /// live-smoke tests use to decide whether a real key is present.
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

    /// The one sanctioned way to read the secret — for building an auth
    /// header, nothing else.
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ApiKey").field(&"<redacted>").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_secret_value() {
        let key = ApiKey::new("sk-super-secret-value");
        let debug = format!("{key:?}");
        assert!(!debug.contains("sk-super-secret-value"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn from_env_missing_var_is_not_found() {
        let err = ApiKey::from_env("OXAGEN_MEDIA_TEST_DEFINITELY_UNSET_12345").unwrap_err();
        assert_eq!(
            err,
            CredentialError::NotFound {
                env_var: "OXAGEN_MEDIA_TEST_DEFINITELY_UNSET_12345".into()
            }
        );
    }

    #[test]
    fn from_env_present_var_resolves_and_reveals() {
        // SAFETY: test-only env mutation with a unique var name.
        unsafe {
            std::env::set_var("OXAGEN_MEDIA_TEST_KEY", "sk-test-value");
        }
        let key = ApiKey::from_env("OXAGEN_MEDIA_TEST_KEY").unwrap();
        assert_eq!(key.reveal(), "sk-test-value");
        unsafe {
            std::env::remove_var("OXAGEN_MEDIA_TEST_KEY");
        }
    }

    #[test]
    fn from_env_empty_var_is_reported_as_empty() {
        unsafe {
            std::env::set_var("OXAGEN_MEDIA_TEST_EMPTY_KEY", "");
        }
        let err = ApiKey::from_env("OXAGEN_MEDIA_TEST_EMPTY_KEY").unwrap_err();
        assert_eq!(
            err,
            CredentialError::Empty {
                env_var: "OXAGEN_MEDIA_TEST_EMPTY_KEY".into()
            }
        );
        unsafe {
            std::env::remove_var("OXAGEN_MEDIA_TEST_EMPTY_KEY");
        }
    }
}
