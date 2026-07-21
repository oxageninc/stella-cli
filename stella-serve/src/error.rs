//! Typed errors for the serve layer. Library code never panics on host input
//! or wire data — a malformed or stale request is a named error, not a crash.

use thiserror::Error;

/// A failure at the serve boundary (session lifecycle, reverse-RPC resolution).
#[derive(Debug, Error)]
pub enum ServeError {
    /// A reverse-RPC result arrived for a `request_id` with no in-flight
    /// request — the host answered twice, answered after cancellation, or
    /// used a stale id.
    #[error("no in-flight request with id `{0}` (already resolved, or unknown)")]
    UnknownRequest(String),

    /// A reverse-RPC result arrived for a real in-flight request, but of the
    /// wrong kind (e.g. a provider result posted to a tool request id).
    #[error("request `{0}` is not a {1} request")]
    RequestKindMismatch(String, &'static str),

    /// The engine-driving thread ended before the turn produced an outcome —
    /// it panicked, or its runtime failed to build.
    #[error("engine session thread ended without an outcome: {0}")]
    SessionThreadFailed(String),

    /// Could not construct the per-session Tokio runtime.
    #[error("failed to build the session runtime: {0}")]
    RuntimeBuild(String),
}
