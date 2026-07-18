//! `HostError` — the one typed error the host runtime raises
//! ( "fail loud"). Everything a fan-out or a single
//! provider exchange can go wrong with is a named variant here; nothing in
//! the hot path panics. `ocp-host` owns its own error type rather than
//! borrowing `stella`'s so the crate stays industry-facing and dependency-
//! light (depends only on `ocp-types` + transport
//! crates).

use ocp_types::DataFlow;

/// Anything the host runtime can surface while talking to a provider.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The provider speaks an incompatible protocol family
    ///. Reported the instant the handshake ack
    /// arrives — never a hang (task deliverable 1).
    #[error(
        "protocol version mismatch: host speaks {host}, provider {provider} speaks {provider_version}"
    )]
    VersionMismatch {
        host: String,
        provider: String,
        provider_version: String,
    },

    /// A line/body could not be encoded to or decoded from the wire envelope
    ///. A malformed provider message is a
    /// clean error, never a host crash (task deliverable 5).
    #[error("wire encode/decode error: {0}")]
    Wire(String),

    /// The underlying transport (stdio pipe or HTTP) failed.
    #[error("transport error talking to provider {id}: {message}")]
    Transport { id: String, message: String },

    /// The provider's child process closed its stream mid-exchange — it
    /// crashed. Isolated to this provider; never poisons a `query_all`
    /// (task deliverable 5).
    #[error("provider {id} crashed or closed its stream mid-exchange")]
    ProviderCrashed { id: String },

    /// The provider took longer than the host's per-provider budget.
    #[error("provider {id} timed out after {timeout_ms}ms")]
    Timeout { id: String, timeout_ms: u64 },

    /// The provider reported an error over the wire (an `error` envelope).
    #[error("provider {id} reported an error: {message}")]
    Provider { id: String, message: String },

    /// The provider declares `egress` and has no recorded consent, so the
    /// host refuses to transmit a query to it (
    /// §3.5 — a host MUST NOT auto-enable egress providers). The query
    /// payload never left the host.
    #[error(
        "provider {id} declares egress and requires one-time consent naming what leaves before it can be queried"
    )]
    ConsentRequired { id: String, data_flow: DataFlow },

    /// A message of the wrong kind arrived where the protocol expected a
    /// specific envelope (e.g. a `frames` reply to a `query`).
    #[error("expected a `{expected}` envelope from provider {id}, got `{got}`")]
    UnexpectedEnvelope {
        id: String,
        expected: String,
        got: String,
    },

    /// No provider is registered under the given id.
    #[error("no provider registered with id `{0}`")]
    UnknownProvider(String),

    /// Spawning the provider child process failed.
    #[error("failed to spawn provider process: {0}")]
    Spawn(String),
}
