//! MCP wire types: JSON-RPC 2.0 envelopes plus the handful of MCP methods
//! this client speaks (`initialize`, `notifications/initialized`,
//! `tools/list`, `tools/call`). Protocol revision `2025-06-18`
//! (<https://modelcontextprotocol.io>).
//!
//! This crate is a *client of a public protocol*, so every inbound type is
//! deliberately permissive: `#[serde(default)]` on optional fields and
//! **never** `deny_unknown_fields`. A server that adds a field, or speaks a
//! newer minor revision, must not break us (`02-architecture.md` §1.4, §7).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::McpError;

/// The only JSON-RPC version this client emits or expects.
pub const JSONRPC_VERSION: &str = "2.0";

/// The protocol revision offered first in `initialize`.
pub const PREFERRED_PROTOCOL_VERSION: &str = "2025-06-18";

/// Revisions this client can speak. If a server counter-offers any of these
/// (typically an older one), we accept and record it; anything else is a
/// hard [`crate::McpError::UnsupportedProtocol`].
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Is `version` one this client can speak?
pub fn is_supported_version(version: &str) -> bool {
    SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelopes
// ---------------------------------------------------------------------------

/// An outbound JSON-RPC request (has an `id`, expects a response).
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    /// Omitted entirely when null so `params`-less methods stay clean.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            method: method.into(),
            params,
        }
    }
}

/// An outbound JSON-RPC notification (no `id`, no response expected).
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            method: method.into(),
            params,
        }
    }
}

/// An inbound JSON-RPC message. One decoder handles all three cases a server
/// can send back over the same stream: a success response (`result`), an
/// error response (`error`), or a server-initiated request/notification
/// (`method` present) which this client acknowledges but does not act on in
/// v1 (documented in [`crate::client`]).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct JsonRpcMessage {
    /// Correlates a response to its request. Absent on server notifications.
    /// Kept as a `Value` (number *or* string) because JSON-RPC only requires
    /// the id be echoed *identically*, not that it be a number.
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcErrorObject>,
    /// Present only on a server-initiated request/notification.
    #[serde(default)]
    pub method: Option<String>,
}

impl JsonRpcMessage {
    /// Coerce the echoed id back to the `u64` this client assigned. Accepts a
    /// number or a numeric string (a tolerant reading of servers that
    /// stringify ids); anything else yields `None` and the message is treated
    /// as uncorrelated.
    pub fn correlated_id(&self) -> Option<u64> {
        match self.id.as_ref()? {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Collapse a correlated response into either its `result`, its `error`
    /// mapped to [`McpError::JsonRpc`], or a [`McpError::Protocol`] violation
    /// when it carries neither. Both transports funnel responses through here
    /// so error mapping is identical on stdio and HTTP.
    pub fn into_result(self) -> Result<Value, McpError> {
        if let Some(error) = self.error {
            return Err(McpError::JsonRpc {
                code: error.code,
                message: error.message,
                data: error.data,
            });
        }
        match self.result {
            Some(value) => Ok(value),
            None => Err(McpError::Protocol(
                "JSON-RPC response carried neither `result` nor `error`".into(),
            )),
        }
    }
}

/// The `error` member of a JSON-RPC error response.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

// ---------------------------------------------------------------------------
// MCP method payloads
// ---------------------------------------------------------------------------

/// Name + version of a protocol participant (`clientInfo` / `serverInfo`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Implementation {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

/// `initialize` params. Capabilities are an open object; v1 advertises none.
#[derive(Debug, Clone, Serialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: Value,
    #[serde(rename = "clientInfo")]
    pub client_info: Implementation,
}

/// `initialize` result. Only the negotiated version and (optional) server
/// info matter to this client; `capabilities` is captured but untyped.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(rename = "serverInfo", default)]
    pub server_info: Option<Implementation>,
}

/// `tools/list` params — a lone optional pagination cursor.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListToolsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// `tools/list` result: one page of tools plus an optional continuation
/// cursor. The client loops until `next_cursor` is absent.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<Tool>,
    #[serde(rename = "nextCursor", default)]
    pub next_cursor: Option<String>,
}

/// A single tool as advertised by the server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The tool's JSON Schema for its input. Passed through verbatim into the
    /// engine's `ToolSchema` — no second schema language.
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// `tools/call` params.
#[derive(Debug, Clone, Serialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// `tools/call` result: a content array plus a tool-level error flag.
/// `is_error: true` maps to `ToolOutput::Error` (a tool that ran but failed),
/// distinct from a JSON-RPC error (a request the server rejected).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// One item in a tool result's `content` array. Modeled as a flat, permissive
/// struct rather than a tagged enum so an unknown `type` never fails
/// deserialization — it just renders as a `[<type>]` placeholder.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Present on `text` blocks.
    #[serde(default)]
    pub text: Option<String>,
    /// Base64 payload on `image`/`audio` blocks — never inlined into the
    /// model-visible string, only summarized.
    #[serde(default)]
    pub data: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    /// Present on `resource_link` blocks.
    #[serde(default)]
    pub uri: Option<String>,
    /// Present on embedded `resource` blocks.
    #[serde(default)]
    pub resource: Option<ResourceContents>,
}

/// The `resource` member of an embedded-resource content block.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResourceContents {
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_omits_null_params() {
        let req = JsonRpcRequest::new(7, "tools/list", Value::Null);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 7);
        assert_eq!(json["method"], "tools/list");
        assert!(json.get("params").is_none(), "null params must be omitted");
    }

    #[test]
    fn request_keeps_object_params() {
        let req = JsonRpcRequest::new(1, "tools/call", serde_json::json!({"name": "x"}));
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["params"]["name"], "x");
    }

    #[test]
    fn message_decodes_success_error_and_notification() {
        let ok: JsonRpcMessage =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        assert_eq!(ok.correlated_id(), Some(1));
        assert!(ok.result.is_some() && ok.error.is_none());

        let err: JsonRpcMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"bad"}}"#,
        )
        .unwrap();
        assert_eq!(err.correlated_id(), Some(2));
        assert_eq!(err.error.as_ref().unwrap().code, -32602);

        let note: JsonRpcMessage =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#).unwrap();
        assert_eq!(note.correlated_id(), None);
        assert_eq!(note.method.as_deref(), Some("notifications/progress"));
    }

    #[test]
    fn correlated_id_accepts_numeric_string() {
        let msg: JsonRpcMessage =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"42","result":{}}"#).unwrap();
        assert_eq!(msg.correlated_id(), Some(42));
    }

    #[test]
    fn unknown_fields_are_tolerated_everywhere() {
        // A server on a newer minor revision adds fields we've never seen.
        let init: InitializeResult = serde_json::from_str(
            r#"{"protocolVersion":"2025-06-18","serverInfo":{"name":"s","version":"9","extra":1},"brandNewCapability":{"nested":true}}"#,
        )
        .unwrap();
        assert_eq!(init.protocol_version, "2025-06-18");
        assert_eq!(init.server_info.unwrap().name, "s");

        let tool: Tool = serde_json::from_str(
            r#"{"name":"t","title":"human title we ignore","inputSchema":{"type":"object"}}"#,
        )
        .unwrap();
        assert_eq!(tool.name, "t");
    }

    #[test]
    fn list_result_paginates() {
        let page: ListToolsResult =
            serde_json::from_str(r#"{"tools":[{"name":"a"}],"nextCursor":"c2"}"#).unwrap();
        assert_eq!(page.tools.len(), 1);
        assert_eq!(page.next_cursor.as_deref(), Some("c2"));
    }

    #[test]
    fn call_result_defaults_is_error_false() {
        let res: CallToolResult =
            serde_json::from_str(r#"{"content":[{"type":"text","text":"hi"}]}"#).unwrap();
        assert!(!res.is_error);
        assert_eq!(res.content[0].text.as_deref(), Some("hi"));
    }

    #[test]
    fn supported_version_gate() {
        assert!(is_supported_version("2025-06-18"));
        assert!(is_supported_version("2024-11-05"));
        assert!(!is_supported_version("1999-01-01"));
    }
}
