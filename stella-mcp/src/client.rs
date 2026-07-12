//! [`McpClient`]: the MCP protocol state machine over a [`Transport`]. It
//! runs the handshake (`initialize` → version negotiation →
//! `notifications/initialized`), discovers tools (`tools/list` with cursor
//! pagination), and invokes them (`tools/call`), translating MCP content
//! arrays into the engine's [`ToolOutput`].
//!
//! # Version negotiation
//!
//! The client offers [`PREFERRED_PROTOCOL_VERSION`] in `initialize` and
//! accepts whatever revision the server names back *if* it is one this client
//! can speak ([`SUPPORTED_PROTOCOL_VERSIONS`]); the negotiated version is
//! recorded ([`McpClient::negotiated_version`]). A server that names a
//! revision outside that set is a hard [`McpError::UnsupportedProtocol`] — a
//! client that guessed at an unknown wire format would be worse than one that
//! failed loudly (`02-architecture.md` §1.5).
//!
//! # Content mapping
//!
//! A `tools/call` result is a `content` array. `text` blocks are concatenated
//! (newline-joined); every non-text block is summarized as a compact
//! placeholder — `[image]`, `[audio]`, `[resource: <uri>]` — so a tool that
//! returns an image never floods the model context with base64, but the model
//! still learns *what* came back. `isError: true` maps to
//! [`ToolOutput::Error`]; a JSON-RPC error (a rejected request, distinct from
//! a tool that ran and failed) propagates as [`McpError::JsonRpc`].
//!
//! # Server-initiated traffic
//!
//! v1 is a pure client: it drives `initialize`/`tools/*` and ignores any
//! server-initiated request or notification (sampling, roots, progress). The
//! transport drops those silently.

use std::time::Duration;

use serde_json::Value;
use stella_protocol::ToolOutput;

use crate::config::{McpServerConfig, McpTransport};
use crate::error::McpError;
use crate::http::HttpTransport;
use crate::protocol::{
    CallToolParams, CallToolResult, ContentBlock, Implementation, InitializeParams,
    InitializeResult, ListToolsParams, ListToolsResult, PREFERRED_PROTOCOL_VERSION,
    SUPPORTED_PROTOCOL_VERSIONS, is_supported_version,
};
use crate::stdio::StdioTransport;
use crate::transport::Transport;

/// The `clientInfo.name` advertised in `initialize`.
const CLIENT_NAME: &str = "stella";
/// The `clientInfo.version` advertised in `initialize`.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// A hard cap on `tools/list` pages, defending against a server that returns
/// a non-advancing cursor forever.
const MAX_TOOL_PAGES: usize = 1000;

/// One tool advertised by a server, in the client's own shape (the raw,
/// un-namespaced name plus its input schema).
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A connected MCP client for a single server.
pub struct McpClient {
    name: String,
    transport: Box<dyn Transport>,
    negotiated_version: String,
    server_info: Option<Implementation>,
    tools: Vec<McpToolInfo>,
}

impl McpClient {
    /// Wrap a transport. Does **not** perform the handshake — call
    /// [`McpClient::initialize`] before using the client. (Splitting
    /// construction from the handshake is what lets tests drive a client over
    /// an in-memory transport.)
    pub fn new(name: impl Into<String>, transport: Box<dyn Transport>) -> Self {
        Self {
            name: name.into(),
            transport,
            negotiated_version: String::new(),
            server_info: None,
            tools: Vec::new(),
        }
    }

    /// Build the transport for `config`, then run the full handshake +
    /// tool discovery. `timeout` bounds each underlying request.
    pub async fn connect(config: &McpServerConfig, timeout: Duration) -> Result<Self, McpError> {
        let transport: Box<dyn Transport> = match &config.transport {
            McpTransport::Stdio { cmd, args, env } => {
                Box::new(StdioTransport::spawn(&config.name, cmd, args, env).await?)
            }
            McpTransport::Http { url, headers } => {
                Box::new(HttpTransport::new(&config.name, url, headers, timeout)?)
            }
        };
        let mut client = McpClient::new(&config.name, transport);
        client.initialize().await?;
        Ok(client)
    }

    /// Run `initialize` → negotiate the version → `notifications/initialized`
    /// → `tools/list` (all pages). On success the client is ready for
    /// [`McpClient::call_tool`].
    pub async fn initialize(&mut self) -> Result<(), McpError> {
        let params = InitializeParams {
            protocol_version: PREFERRED_PROTOCOL_VERSION.to_string(),
            capabilities: serde_json::json!({}),
            client_info: Implementation {
                name: CLIENT_NAME.to_string(),
                version: CLIENT_VERSION.to_string(),
            },
        };
        let raw = self
            .transport
            .request("initialize", to_value(&params)?)
            .await?;
        let result: InitializeResult = serde_json::from_value(raw)
            .map_err(|e| McpError::Protocol(format!("could not decode initialize result: {e}")))?;

        // Negotiate: accept the server's version if we can speak it; a server
        // that omits the field is read leniently as agreeing to our offer.
        let version = if result.protocol_version.is_empty() {
            PREFERRED_PROTOCOL_VERSION.to_string()
        } else {
            result.protocol_version
        };
        if !is_supported_version(&version) {
            return Err(McpError::UnsupportedProtocol {
                offered: version,
                supported: SUPPORTED_PROTOCOL_VERSIONS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            });
        }
        self.negotiated_version = version;
        self.server_info = result.server_info;

        // Complete the handshake, then discover tools.
        self.transport
            .notify("notifications/initialized", Value::Null)
            .await?;
        self.tools = self.fetch_all_tools().await?;
        Ok(())
    }

    /// Drive `tools/list` to exhaustion, following `nextCursor`.
    async fn fetch_all_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = ListToolsParams {
                cursor: cursor.clone(),
            };
            let raw = self
                .transport
                .request("tools/list", to_value(&params)?)
                .await?;
            let page: ListToolsResult = serde_json::from_value(raw).map_err(|e| {
                McpError::Protocol(format!("could not decode tools/list result: {e}"))
            })?;
            for tool in page.tools {
                tools.push(McpToolInfo {
                    description: tool.description.unwrap_or_default(),
                    name: tool.name,
                    input_schema: normalize_schema(tool.input_schema),
                });
            }
            match page.next_cursor {
                Some(next) if !next.is_empty() => cursor = Some(next),
                _ => return Ok(tools),
            }
        }
        Err(McpError::Protocol(format!(
            "server `{}` exceeded {MAX_TOOL_PAGES} tools/list pages — cursor never terminated",
            self.name
        )))
    }

    /// Call `tool` with `arguments`, returning the mapped [`ToolOutput`].
    pub async fn call_tool(&self, tool: &str, arguments: Value) -> Result<ToolOutput, McpError> {
        let params = CallToolParams {
            name: tool.to_string(),
            arguments: if arguments.is_null() {
                Value::Object(serde_json::Map::new())
            } else {
                arguments
            },
        };
        let raw = self
            .transport
            .request("tools/call", to_value(&params)?)
            .await?;
        let result: CallToolResult = serde_json::from_value(raw)
            .map_err(|e| McpError::Protocol(format!("could not decode tools/call result: {e}")))?;

        let rendered = render_content(&result.content);
        if result.is_error {
            Ok(ToolOutput::Error {
                message: if rendered.is_empty() {
                    format!("tool `{tool}` reported an error with no detail")
                } else {
                    rendered
                },
            })
        } else {
            Ok(ToolOutput::Ok { content: rendered })
        }
    }

    /// Orderly shutdown of the underlying transport.
    pub async fn close(&self) -> Result<(), McpError> {
        self.transport.close().await
    }

    /// The configured server name (the namespace segment for its tools).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The negotiated protocol revision (empty until `initialize` succeeds).
    pub fn negotiated_version(&self) -> &str {
        &self.negotiated_version
    }

    /// The server's advertised implementation info, if it sent any.
    pub fn server_info(&self) -> Option<&Implementation> {
        self.server_info.as_ref()
    }

    /// The tools discovered during `initialize` (raw, un-namespaced).
    pub fn tools(&self) -> &[McpToolInfo] {
        &self.tools
    }
}

/// Serialize a params struct, mapping any (unexpected) failure to a protocol
/// error rather than panicking.
fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, McpError> {
    serde_json::to_value(value).map_err(|e| McpError::Protocol(e.to_string()))
}

/// A tool with a null/missing input schema still needs *a* schema; default to
/// the permissive empty-object schema so the model always sees valid JSON
/// Schema.
fn normalize_schema(schema: Value) -> Value {
    if schema.is_null() {
        serde_json::json!({ "type": "object" })
    } else {
        schema
    }
}

/// Concatenate a `tools/call` content array into a single model-visible
/// string: text verbatim, everything else a compact placeholder.
pub fn render_content(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        let piece = match block.kind.as_str() {
            "text" => block.text.clone().unwrap_or_default(),
            "image" => "[image]".to_string(),
            "audio" => "[audio]".to_string(),
            "resource" => match block.resource.as_ref().and_then(|r| r.uri.clone()) {
                Some(uri) => format!("[resource: {uri}]"),
                None => "[resource]".to_string(),
            },
            "resource_link" => match &block.uri {
                Some(uri) => format!("[resource: {uri}]"),
                None => "[resource]".to_string(),
            },
            // A block with no `type` is malformed; fall back to any text it
            // carried, else a generic marker.
            "" => block
                .text
                .clone()
                .unwrap_or_else(|| "[unknown]".to_string()),
            // An unknown but named type: summarize by its name.
            other => format!("[{other}]"),
        };
        if !piece.is_empty() {
            parts.push(piece);
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ResourceContents;
    use crate::transport::testkit::ScriptedTransport;

    fn block(kind: &str) -> ContentBlock {
        ContentBlock {
            kind: kind.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn render_concatenates_text_and_summarizes_the_rest() {
        let blocks = vec![
            ContentBlock {
                kind: "text".into(),
                text: Some("first".into()),
                ..Default::default()
            },
            block("image"),
            ContentBlock {
                kind: "text".into(),
                text: Some("second".into()),
                ..Default::default()
            },
            ContentBlock {
                kind: "resource".into(),
                resource: Some(ResourceContents {
                    uri: Some("file:///a.txt".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ContentBlock {
                kind: "resource_link".into(),
                uri: Some("https://x/y".into()),
                ..Default::default()
            },
            block("audio"),
            block("brand_new_kind"),
        ];
        let rendered = render_content(&blocks);
        assert_eq!(
            rendered,
            "first\n[image]\nsecond\n[resource: file:///a.txt]\n[resource: https://x/y]\n[audio]\n[brand_new_kind]"
        );
    }

    #[test]
    fn empty_content_renders_empty() {
        assert_eq!(render_content(&[]), "");
    }

    #[tokio::test]
    async fn initialize_negotiates_version_and_lists_tools() {
        let transport = ScriptedTransport::new();
        transport.push_ok(
            "initialize",
            serde_json::json!({
                "protocolVersion": PREFERRED_PROTOCOL_VERSION,
                "serverInfo": { "name": "fixture", "version": "1.0" }
            }),
        );
        transport.push_ok(
            "tools/list",
            serde_json::json!({ "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] }),
        );
        let mut client = McpClient::new("srv", Box::new(transport));
        client.initialize().await.unwrap();

        assert_eq!(client.negotiated_version(), PREFERRED_PROTOCOL_VERSION);
        assert_eq!(client.server_info().unwrap().name, "fixture");
        assert_eq!(client.tools().len(), 1);
        assert_eq!(client.tools()[0].name, "echo");
    }

    #[tokio::test]
    async fn initialize_accepts_an_older_counter_offer() {
        let transport = ScriptedTransport::new();
        transport.push_ok(
            "initialize",
            serde_json::json!({ "protocolVersion": "2024-11-05" }),
        );
        transport.push_ok("tools/list", serde_json::json!({ "tools": [] }));
        let mut client = McpClient::new("srv", Box::new(transport));
        client.initialize().await.unwrap();
        assert_eq!(client.negotiated_version(), "2024-11-05");
    }

    #[tokio::test]
    async fn initialize_rejects_an_unspeakable_version() {
        let transport = ScriptedTransport::new();
        transport.push_ok(
            "initialize",
            serde_json::json!({ "protocolVersion": "1999-01-01" }),
        );
        let mut client = McpClient::new("srv", Box::new(transport));
        let err = client.initialize().await.unwrap_err();
        assert!(matches!(err, McpError::UnsupportedProtocol { .. }));
    }

    #[tokio::test]
    async fn tools_list_follows_pagination_cursors() {
        let transport = ScriptedTransport::new();
        transport.push_ok(
            "initialize",
            serde_json::json!({ "protocolVersion": PREFERRED_PROTOCOL_VERSION }),
        );
        transport.push_ok(
            "tools/list",
            serde_json::json!({ "tools": [{ "name": "a" }], "nextCursor": "p2" }),
        );
        transport.push_ok(
            "tools/list",
            serde_json::json!({ "tools": [{ "name": "b" }, { "name": "c" }] }),
        );
        let mut client = McpClient::new("srv", Box::new(transport));
        client.initialize().await.unwrap();
        let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn initialized_notification_is_sent() {
        let transport = ScriptedTransport::new();
        transport.push_ok(
            "initialize",
            serde_json::json!({ "protocolVersion": PREFERRED_PROTOCOL_VERSION }),
        );
        transport.push_ok("tools/list", serde_json::json!({ "tools": [] }));
        let notes = transport.notifications_handle();
        let mut client = McpClient::new("srv", Box::new(transport));
        client.initialize().await.unwrap();
        let sent = notes.lock().unwrap();
        assert!(sent.iter().any(|(m, _)| m == "notifications/initialized"));
    }

    #[tokio::test]
    async fn call_tool_maps_ok_and_is_error() {
        let transport = ScriptedTransport::new();
        transport.push_ok(
            "tools/call",
            serde_json::json!({ "content": [{ "type": "text", "text": "done" }] }),
        );
        transport.push_ok(
            "tools/call",
            serde_json::json!({ "content": [{ "type": "text", "text": "boom" }], "isError": true }),
        );
        let client = McpClient::new("srv", Box::new(transport));

        let ok = client.call_tool("t", serde_json::json!({})).await.unwrap();
        assert_eq!(
            ok,
            ToolOutput::Ok {
                content: "done".into()
            }
        );

        let err = client.call_tool("t", serde_json::json!({})).await.unwrap();
        assert_eq!(
            err,
            ToolOutput::Error {
                message: "boom".into()
            }
        );
    }

    #[tokio::test]
    async fn call_tool_propagates_a_jsonrpc_error() {
        let transport = ScriptedTransport::new();
        transport.push_err(
            "tools/call",
            McpError::JsonRpc {
                code: -32602,
                message: "unknown tool".into(),
                data: None,
            },
        );
        let client = McpClient::new("srv", Box::new(transport));
        let err = client.call_tool("t", Value::Null).await.unwrap_err();
        assert!(matches!(err, McpError::JsonRpc { code: -32602, .. }));
    }
}
