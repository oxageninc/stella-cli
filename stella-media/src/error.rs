//! Typed media errors (`02-architecture.md` §1.5: fail loud, recover
//! gracefully — never `panic!` in the hot path). This mirrors
//! `stella_protocol::ProviderError`'s category shape (transport / rate-limit /
//! auth / malformed / cancelled / terminal) with its own type because
//! `stella-media` may not depend on `stella-model` (parallel-workstream
//! isolation, see the crate-level doc). Two categories are added beyond the
//! chat provider's set, both required by `08-multimodal.md`:
//!
//! * [`MediaError::ContentPolicy`] — a provider refusal (`08-multimodal.md`
//!   §7 lists content-policy refusals as a failure shape the fixtures must
//!   cover); terminal, never retried.
//! * [`MediaError::CostDenied`] — the video cost gate (`08-multimodal.md` §6)
//!   said no; terminal.

use thiserror::Error;

/// Every fallible operation in this crate returns a [`MediaError`]. The
/// retry classification ([`MediaError::is_retryable`]) matches
/// `ProviderError`'s: transport and rate-limit are retryable, everything
/// else (auth, malformed, content-policy, cost-denied, terminal,
/// cancellation) is not.
#[derive(Error, Debug)]
pub enum MediaError {
    /// A network/transport failure reaching the provider. Retryable.
    #[error("media transport error: {0}")]
    Transport(String),

    /// The provider throttled the request. Retryable, with an optional
    /// server-advised delay parsed from `Retry-After`.
    #[error("media provider rate limited (retry after {retry_after_ms:?}ms): {message}")]
    RateLimited {
        message: String,
        retry_after_ms: Option<u64>,
    },

    /// The API key was rejected (401/403-auth). Terminal — retrying with the
    /// same key cannot succeed.
    #[error("media provider auth error: {0}")]
    Auth(String),

    /// The provider refused the prompt on safety/content-policy grounds
    /// (`08-multimodal.md` §7). Terminal.
    #[error("media provider refused the request (content policy): {0}")]
    ContentPolicy(String),

    /// A capability was requested that no configured key can serve
    /// (`08-multimodal.md` §2: "fails loudly, naming which keys would enable
    /// it"). Terminal.
    #[error(
        "media capability `{capability}` is unavailable with the configured providers; \
         set one of these to enable it: {enabling_keys}"
    )]
    CapabilityUnavailable {
        capability: String,
        enabling_keys: String,
    },

    /// The provider's response could not be decoded into the expected shape
    /// (missing field, bad base64, non-JSON body). Terminal.
    #[error("media provider returned a malformed response: {0}")]
    Malformed(String),

    /// The request was cancelled (e.g. Ctrl-C draining the turn). Never
    /// retried.
    #[error("media request cancelled")]
    Cancelled,

    /// The cost gate denied a video job above the confirmation threshold
    /// (`08-multimodal.md` §6). Terminal.
    #[error(
        "cost gate denied the job (estimated ${estimated_usd:.4}, threshold ${threshold_usd:.4})"
    )]
    CostDenied {
        estimated_usd: f64,
        threshold_usd: f64,
    },

    /// The artifact store could not read/write under `.stella/artifacts/`
    /// (I/O, or a rejected path). Terminal.
    #[error("artifact store error: {0}")]
    Artifact(String),

    /// A non-retryable provider failure not covered above (4xx other than
    /// auth/rate-limit, 5xx that exhausted retries upstream).
    #[error("terminal media provider error: {0}")]
    Terminal(String),
}

impl MediaError {
    /// Whether a caller's retry loop should retry this with backoff.
    /// Transport and rate-limit only — everything else is terminal
    /// (matches `ProviderError::is_retryable`, `02-architecture.md` §5).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            MediaError::Transport(_) | MediaError::RateLimited { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_and_rate_limit_are_retryable_nothing_else_is() {
        assert!(MediaError::Transport("reset".into()).is_retryable());
        assert!(
            MediaError::RateLimited {
                message: "slow down".into(),
                retry_after_ms: Some(1000),
            }
            .is_retryable()
        );
        assert!(!MediaError::Auth("bad key".into()).is_retryable());
        assert!(!MediaError::ContentPolicy("refused".into()).is_retryable());
        assert!(!MediaError::Malformed("no data".into()).is_retryable());
        assert!(!MediaError::Cancelled.is_retryable());
        assert!(
            !MediaError::CostDenied {
                estimated_usd: 0.4,
                threshold_usd: 0.0,
            }
            .is_retryable()
        );
        assert!(!MediaError::Terminal("500".into()).is_retryable());
    }

    #[test]
    fn cost_denied_names_the_numbers_in_its_message() {
        let msg = MediaError::CostDenied {
            estimated_usd: 0.42,
            threshold_usd: 0.10,
        }
        .to_string();
        assert!(msg.contains("0.42"), "{msg}");
        assert!(msg.contains("0.10"), "{msg}");
    }

    #[test]
    fn capability_unavailable_names_the_enabling_keys() {
        let msg = MediaError::CapabilityUnavailable {
            capability: "video".into(),
            enabling_keys: "ZAI_API_KEY".into(),
        }
        .to_string();
        assert!(msg.contains("video"), "{msg}");
        assert!(msg.contains("ZAI_API_KEY"), "{msg}");
    }
}
