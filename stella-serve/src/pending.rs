//! The in-flight reverse-request registry.
//!
//! When the engine raises a reverse-RPC request (a tool call or a model
//! completion), it registers a one-shot reply channel here keyed by a fresh
//! `request_id`, then emits the request frame and awaits the receiver. When the
//! host POSTs the matching result, the serve layer looks the id up here and
//! fires the sender — unblocking the parked engine step.
//!
//! The registry is shared (`Arc<Mutex<..>>`) between the engine-driving thread
//! (which registers) and the inbound HTTP handlers (which resolve). The one-shot
//! senders bridge the two Tokio runtimes: the receiver is awaited on the session
//! thread's current-thread runtime, the sender fired from the server runtime —
//! `tokio::sync::oneshot` is runtime-agnostic, so the wake crosses cleanly. This
//! is the fleet's `!Send`-future bridge pattern (`stella-cli::fleet_cmd`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use stella_protocol::{CompletionResult, ProviderError, ToolOutput};
use tokio::sync::oneshot;

use crate::error::ServeError;

/// A registered reply channel, discriminated by the kind of request it answers.
enum PendingReply {
    Tool(oneshot::Sender<ToolOutput>),
    Provider(oneshot::Sender<Result<CompletionResult, ProviderError>>),
}

/// A cloneable handle to the shared registry. Cloning shares the same map.
#[derive(Clone, Default)]
pub struct Pending {
    inner: Arc<Mutex<HashMap<String, PendingReply>>>,
}

impl Pending {
    /// Register a tool reply channel under `id`. Called by the engine thread
    /// before it emits the request frame, so the entry always exists by the
    /// time the host can answer.
    pub(crate) fn register_tool(&self, id: String, reply: oneshot::Sender<ToolOutput>) {
        self.lock().insert(id, PendingReply::Tool(reply));
    }

    /// Register a provider reply channel under `id`.
    pub(crate) fn register_provider(
        &self,
        id: String,
        reply: oneshot::Sender<Result<CompletionResult, ProviderError>>,
    ) {
        self.lock().insert(id, PendingReply::Provider(reply));
    }

    /// Resolve a tool request with the host-supplied output. Errors if `id` is
    /// unknown or names a provider request.
    pub fn resolve_tool(&self, id: &str, output: ToolOutput) -> Result<(), ServeError> {
        match self.take(id)? {
            PendingReply::Tool(tx) => {
                // A receive-side drop (the engine step was cancelled) is not an
                // error to the host: the request is simply gone.
                let _ = tx.send(output);
                Ok(())
            }
            other => {
                self.reinsert(id, other);
                Err(ServeError::RequestKindMismatch(id.to_string(), "tool"))
            }
        }
    }

    /// Resolve a provider request with the host-supplied result. Errors if `id`
    /// is unknown or names a tool request.
    pub fn resolve_provider(
        &self,
        id: &str,
        result: Result<CompletionResult, ProviderError>,
    ) -> Result<(), ServeError> {
        match self.take(id)? {
            PendingReply::Provider(tx) => {
                let _ = tx.send(result);
                Ok(())
            }
            other => {
                self.reinsert(id, other);
                Err(ServeError::RequestKindMismatch(id.to_string(), "provider"))
            }
        }
    }

    /// Drop every in-flight request. Their receivers observe a channel close,
    /// which each port impl maps to a clean error for the engine — used when a
    /// session is torn down or its host disconnects.
    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    fn take(&self, id: &str) -> Result<PendingReply, ServeError> {
        self.lock()
            .remove(id)
            .ok_or_else(|| ServeError::UnknownRequest(id.to_string()))
    }

    fn reinsert(&self, id: &str, reply: PendingReply) {
        self.lock().insert(id.to_string(), reply);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingReply>> {
        // The registry guards only map insert/remove — no await is held across
        // the lock, so poisoning can only follow a panic while mutating the map
        // itself; recover the guard rather than propagate the poison.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}
