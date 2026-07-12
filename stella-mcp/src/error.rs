//! Typed errors for the MCP client (`02-architecture.md` §1.5 — fail loud,
//! never `panic!` in the hot path). Every fallible operation in this crate
//! returns [`McpError`]; the boundary that faces `stella-core` — the engine's
//! [`crate::McpToolSet`] — maps each of these onto a model-visible
//! `ToolOutput::Error` so a broken server is *data*, never an engine crash.

use thiserror::Error;

/// Everything that can go wrong talking to an MCP server.
#[derive(Debug, Error)]
pub enum McpError {
    /// Transport-level failure: the child process could not be spawned, the
    /// HTTP request failed, stdin/stdout broke, or the stream ended
    /// unexpectedly.
    #[error("transport error: {0}")]
    Transport(String),

    /// The server sent a JSON-RPC error object in response to a request.
    /// `code`/`message` are the server's own; `data` is passed through
    /// verbatim for diagnostics.
    #[error("json-rpc error {code}: {message}")]
    JsonRpc {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },

    /// A response arrived but could not be decoded into the shape the
    /// protocol requires (bad JSON, a `result` that isn't the expected
    /// object, a JSON-RPC message missing both `result` and `error`).
    #[error("malformed protocol message: {0}")]
    Protocol(String),

    /// The server negotiated a protocol revision this client cannot speak.
    #[error("unsupported MCP protocol version {offered:?} (this client speaks {supported:?})")]
    UnsupportedProtocol {
        offered: String,
        supported: Vec<String>,
    },

    /// The operation exceeded its deadline.
    #[error("timed out after {0}ms")]
    Timeout(u64),

    /// The transport is closed — the child exited, or `close()` was already
    /// called. A subsequent request cannot be served.
    #[error("connection closed: {0}")]
    Closed(String),

    /// Configuration could not be parsed (bad TOML, unknown transport shape).
    #[error("invalid MCP configuration: {0}")]
    Config(String),
}

impl McpError {
    /// A one-line, credential-free summary safe to surface to the model as a
    /// `ToolOutput::Error` message. (No `McpError` variant ever carries a
    /// secret — credentials are never logged nor forwarded, `02-architecture.md`
    /// §8 — so this is just the `Display` form, named for intent.)
    pub fn user_message(&self) -> String {
        self.to_string()
    }
}
