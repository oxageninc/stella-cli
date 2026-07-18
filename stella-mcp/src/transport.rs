//! The transport seam. A [`Transport`] carries JSON-RPC requests and
//! notifications to one server and returns raw `result` values; it owns id
//! assignment and request/response correlation. [`crate::client::McpClient`]
//! sits on top and knows the MCP *methods*; the transport knows only the
//! *framing* (newline-delimited stdio, or POST-with-maybe-SSE HTTP).
//!
//! Keeping this a trait is what lets the unit tests drive
//! [`crate::McpToolSet`] with an in-memory canned transport (no process, no
//! socket) while production wires [`crate::stdio::StdioTransport`] /
//! [`crate::http::HttpTransport`].

use async_trait::async_trait;
use serde_json::Value;

use crate::error::McpError;

/// A live connection to a single MCP server.
///
/// Implementations must be cancel-safe: if the future returned by `request`
/// is dropped (e.g. a caller-side timeout fired), the transport may leave the
/// in-flight id uncorrelated but must not corrupt subsequent requests.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send a JSON-RPC request and await the server's `result`. A JSON-RPC
    /// `error` response becomes [`McpError::JsonRpc`]; a dead connection
    /// becomes [`McpError::Closed`] or [`McpError::Transport`].
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError>;

    /// Send a JSON-RPC notification (no id, no response). Best-effort: a
    /// closed connection is still an error, but there is nothing to await.
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError>;

    /// Orderly shutdown. For stdio: close stdin, wait briefly, then kill the
    /// child. For HTTP: a no-op (there is no persistent connection to tear
    /// down). Idempotent.
    async fn close(&self) -> Result<(), McpError>;
}

/// In-memory test double: a [`Transport`] whose responses are scripted
/// per-method. Lets `client` and `toolset` unit tests exercise the full
/// protocol state machine with no process and no socket.
#[cfg(test)]
pub(crate) mod testkit {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::Value;

    use super::Transport;
    use crate::error::McpError;

    type Queues = Arc<Mutex<HashMap<String, VecDeque<Result<Value, McpError>>>>>;
    type Log = Arc<Mutex<Vec<(String, Value)>>>;

    /// Enqueue responses with [`ScriptedTransport::push_ok`] /
    /// [`ScriptedTransport::push_err`]; each `request(method, …)` pops the
    /// next queued response for that method. Notifications and requests are
    /// recorded for assertions.
    pub(crate) struct ScriptedTransport {
        responses: Queues,
        requests: Log,
        notifications: Log,
    }

    impl ScriptedTransport {
        pub(crate) fn new() -> Self {
            Self {
                responses: Arc::new(Mutex::new(HashMap::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
                notifications: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub(crate) fn push_ok(&self, method: &str, value: Value) {
            self.responses
                .lock()
                .unwrap()
                .entry(method.to_string())
                .or_default()
                .push_back(Ok(value));
        }

        pub(crate) fn push_err(&self, method: &str, err: McpError) {
            self.responses
                .lock()
                .unwrap()
                .entry(method.to_string())
                .or_default()
                .push_back(Err(err));
        }

        pub(crate) fn notifications_handle(&self) -> Log {
            self.notifications.clone()
        }
    }

    #[async_trait]
    impl Transport for ScriptedTransport {
        async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
            self.requests
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            let popped = self
                .responses
                .lock()
                .unwrap()
                .get_mut(method)
                .and_then(|queue| queue.pop_front());
            popped.unwrap_or_else(|| {
                Err(McpError::Transport(format!(
                    "scripted transport has no more `{method}` responses queued"
                )))
            })
        }

        async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
            self.notifications
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            Ok(())
        }

        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }
}
