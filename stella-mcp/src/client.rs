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

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use stella_protocol::ToolOutput;
use tokio::sync::Mutex;

use crate::config::{McpServerConfig, McpTransport};
use crate::error::McpError;
use crate::http::HttpTransport;
use crate::protocol::{
    CallToolParams, CallToolResult, ContentBlock, Implementation, InitializeParams,
    InitializeResult, ListToolsParams, ListToolsResult, PREFERRED_PROTOCOL_VERSION,
    SUPPORTED_PROTOCOL_VERSIONS, is_supported_version,
};
use crate::stdio::StdioTransport;
use crate::toolset::DEFAULT_CALL_TIMEOUT;
use crate::transport::Transport;

/// Reconnect backoff floor: the first re-attempt after the *second* straight
/// failure waits this long, doubling each further failure.
const RECONNECT_BASE: Duration = Duration::from_secs(1);
/// Reconnect backoff ceiling — the delay never grows past this, so a
/// long-dead server is still probed roughly twice a minute forever.
const RECONNECT_CAP: Duration = Duration::from_secs(30);

/// Rebuilds a fresh (pre-handshake) transport for a server whose connection
/// dropped. Boxed so a [`McpClient`] can carry it without naming the concrete
/// spawn future; `None` means "no auto-reconnect" (the shape tests build via
/// [`McpClient::new`], which keeps the historical dead-stays-dead behavior).
type Reconnector = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<Box<dyn Transport>, McpError>> + Send>>
        + Send
        + Sync,
>;

/// Health of one server's connection, as surfaced to the CLI/TUI/telemetry so
/// a mid-session drop is a *visible, non-fatal* diagnostic rather than a
/// silent degradation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// The transport is connected and the last call succeeded.
    Live,
    /// A reconnect attempt is in flight right now.
    Reconnecting,
    /// The connection is down; the next call will retry once the backoff
    /// window (see [`ServerHealth::retry_in`]) elapses.
    Down,
}

/// A point-in-time snapshot of a server's connection health.
#[derive(Debug, Clone)]
pub struct ServerHealth {
    /// The server name (tool namespace segment).
    pub name: String,
    /// Live / Reconnecting / Down.
    pub state: HealthState,
    /// How many calls in a row have failed (0 when healthy).
    pub consecutive_failures: u32,
    /// The last failure's model-safe message, if any.
    pub last_error: Option<String>,
    /// When `Down`, roughly how long until the next reconnect is allowed.
    pub retry_in: Option<Duration>,
}

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
///
/// The transport lives behind a [`Mutex`] so that `call_tool(&self)` can
/// *replace* a dropped connection with a freshly-spawned one (auto-reconnect)
/// without a `&mut` at the call site. Everything set once at handshake time
/// (`name`, `tools`, `negotiated_version`, `server_info`) stays a plain field:
/// reconnect restores connectivity to the *same* advertised tool set, it does
/// not re-negotiate the surface mid-session.
pub struct McpClient {
    name: String,
    /// The live transport + reconnect bookkeeping. `None` transport = down.
    conn: Mutex<Connection>,
    /// How to rebuild a dropped transport. `None` = no auto-reconnect.
    reconnect: Option<Reconnector>,
    /// Per-call timeout (also the reconnect/handshake bound). Set at build
    /// time by [`McpToolSet`]; a timed-out call is treated as a drop.
    call_timeout: Duration,
    negotiated_version: String,
    server_info: Option<Implementation>,
    tools: Vec<McpToolInfo>,
}

/// The mutable half of a client: the current transport (`None` once it has
/// been torn down) and its rolling health.
struct Connection {
    transport: Option<Box<dyn Transport>>,
    health: Health,
}

/// Rolling connection health + the backoff clock.
struct Health {
    state: HealthState,
    consecutive_failures: u32,
    last_error: Option<String>,
    /// Earliest instant a reconnect may be attempted (set while `Down`).
    next_retry_at: Option<Instant>,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            state: HealthState::Live,
            consecutive_failures: 0,
            last_error: None,
            next_retry_at: None,
        }
    }
}

impl Connection {
    /// A call (or reconnect) succeeded: back to fully healthy.
    fn mark_healthy(&mut self) {
        self.health.state = HealthState::Live;
        self.health.consecutive_failures = 0;
        self.health.last_error = None;
        self.health.next_retry_at = None;
    }

    /// A call (or reconnect) failed: drop the transport and arm the backoff
    /// clock so the next reconnect waits an increasing, capped interval.
    fn tear_down(&mut self, err: &McpError) {
        self.transport = None;
        self.health.consecutive_failures = self.health.consecutive_failures.saturating_add(1);
        self.health.last_error = Some(err.user_message());
        self.health.state = HealthState::Down;
        self.health.next_retry_at =
            Some(Instant::now() + backoff_delay(self.health.consecutive_failures));
    }

    /// How long until a reconnect is allowed (`Some(0)` = now), or `None` when
    /// the connection is not currently down.
    fn retry_in(&self) -> Option<Duration> {
        self.health
            .next_retry_at
            .map(|at| at.saturating_duration_since(Instant::now()))
    }
}

/// The outcome of one bounded transport request, classified so the caller can
/// react: a fast *drop* self-heals in-call, a *timeout* defers reconnect to
/// the next call, a *protocol* error is passed straight through (reconnecting
/// would not help — the server answered, it just answered badly).
enum RequestOutcome {
    Ok(Value),
    Dropped(McpError),
    Timeout(McpError),
    Protocol(McpError),
}

/// Bounded exponential backoff. The first failure retries immediately (so a
/// single blip self-heals within the turn); each further consecutive failure
/// doubles the wait from [`RECONNECT_BASE`], capped at [`RECONNECT_CAP`].
fn backoff_delay(consecutive_failures: u32) -> Duration {
    if consecutive_failures <= 1 {
        return Duration::ZERO;
    }
    // failures=2 -> 2^0·base, =3 -> 2^1·base, … clamp the exponent so the
    // shift can never overflow (the cap dominates long before this bites).
    let exp = (consecutive_failures - 2).min(20);
    let secs = RECONNECT_BASE.as_secs().saturating_mul(1u64 << exp);
    Duration::from_secs(secs).min(RECONNECT_CAP)
}

/// Whether an error means the *connection* died (spawn/pipe/stream failure or
/// a closed transport) — the only errors a reconnect can fix.
fn is_connection_death(err: &McpError) -> bool {
    matches!(err, McpError::Transport(_) | McpError::Closed(_))
}

impl McpClient {
    /// Wrap a transport. Does **not** perform the handshake — call
    /// [`McpClient::initialize`] before using the client. (Splitting
    /// construction from the handshake is what lets tests drive a client over
    /// an in-memory transport.)
    pub fn new(name: impl Into<String>, transport: Box<dyn Transport>) -> Self {
        Self {
            name: name.into(),
            conn: Mutex::new(Connection {
                transport: Some(transport),
                health: Health::default(),
            }),
            reconnect: None,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            negotiated_version: String::new(),
            server_info: None,
            tools: Vec::new(),
        }
    }

    /// Override the per-call timeout (default [`DEFAULT_CALL_TIMEOUT`]). Set by
    /// [`McpToolSet`] when the set is assembled so every server shares one
    /// bound; a call that exceeds it is treated as a drop and schedules a
    /// reconnect.
    pub fn set_call_timeout(&mut self, timeout: Duration) {
        self.call_timeout = timeout;
    }

    /// Build the transport for `config`, then run the full handshake +
    /// tool discovery. `timeout` bounds each underlying request.
    pub async fn connect(config: &McpServerConfig, timeout: Duration) -> Result<Self, McpError> {
        let transport = build_transport(config, timeout).await?;
        let mut client = McpClient::new(&config.name, transport);
        client.call_timeout = timeout;
        client.initialize().await?;
        // Retain a reconnector so a mid-session drop can be respawned from the
        // same config (with the same scrubbed environment for stdio servers).
        let cfg = config.clone();
        client.reconnect = Some(Arc::new(move || {
            let cfg = cfg.clone();
            Box::pin(async move { build_transport(&cfg, timeout).await })
        }));
        Ok(client)
    }

    /// Run `initialize` → negotiate the version → `notifications/initialized`
    /// → `tools/list` (all pages). On success the client is ready for
    /// [`McpClient::call_tool`].
    pub async fn initialize(&mut self) -> Result<(), McpError> {
        let name = self.name.clone();
        let handshake = {
            let conn = self.conn.get_mut();
            let transport = conn.transport.as_deref().ok_or_else(|| {
                McpError::Closed(format!("client `{name}` has no transport to initialize"))
            })?;
            run_handshake(&name, transport).await?
        };
        self.negotiated_version = handshake.negotiated_version;
        self.server_info = handshake.server_info;
        self.tools = handshake.tools;
        self.conn.get_mut().mark_healthy();
        Ok(())
    }

    /// Call `tool` with `arguments`, returning the mapped [`ToolOutput`].
    ///
    /// Resilience: the call is bounded by [`McpClient::set_call_timeout`]. If
    /// the connection has *dropped*, one transparent reconnect + retry is
    /// attempted so a single blip self-heals within the turn; if it *hangs*
    /// (timeout) or the reconnect is still backing off, a clear, server-named
    /// error is returned as data — the agent's turn is never aborted.
    pub async fn call_tool(&self, tool: &str, arguments: Value) -> Result<ToolOutput, McpError> {
        let params = CallToolParams {
            name: tool.to_string(),
            arguments: if arguments.is_null() {
                Value::Object(serde_json::Map::new())
            } else {
                arguments
            },
        };
        let raw_params = to_value(&params)?;

        let mut conn = self.conn.lock().await;

        // Already down from an earlier failure: try to reconnect first (a
        // fast, model-visible error if the backoff window has not elapsed).
        if conn.transport.is_none() {
            self.reconnect_locked(&mut conn).await?;
        }

        match self
            .request_once(&conn, "tools/call", raw_params.clone())
            .await
        {
            RequestOutcome::Ok(raw) => {
                conn.mark_healthy();
                decode_call_result(tool, raw)
            }
            // A hung call already burned the whole timeout; retrying now would
            // double the wait. Tear down (arming reconnect for the next call)
            // and surface the timeout as model-visible data.
            RequestOutcome::Timeout(err) => {
                conn.tear_down(&err);
                Err(err)
            }
            // The connection died fast. Attempt exactly one transparent
            // reconnect + retry so a single blip self-heals inside this turn.
            RequestOutcome::Dropped(err) => {
                conn.tear_down(&err);
                if self.reconnect_locked(&mut conn).await.is_err() {
                    return Err(err);
                }
                match self.request_once(&conn, "tools/call", raw_params).await {
                    RequestOutcome::Ok(raw) => {
                        conn.mark_healthy();
                        decode_call_result(tool, raw)
                    }
                    RequestOutcome::Timeout(e2) | RequestOutcome::Dropped(e2) => {
                        conn.tear_down(&e2);
                        Err(e2)
                    }
                    RequestOutcome::Protocol(e2) => Err(e2),
                }
            }
            // A JSON-RPC / decode error: the server answered, just badly.
            // Reconnecting would not help, so pass it straight through.
            RequestOutcome::Protocol(err) => Err(err),
        }
    }

    /// One bounded request on the *current* transport, classified into a
    /// [`RequestOutcome`]. A timeout gets its own variant so the caller can
    /// tell "hung" from "dropped" and react differently.
    async fn request_once(&self, conn: &Connection, method: &str, params: Value) -> RequestOutcome {
        let Some(transport) = conn.transport.as_deref() else {
            return RequestOutcome::Dropped(McpError::Closed(format!(
                "server `{}` transport is closed",
                self.name
            )));
        };
        match tokio::time::timeout(self.call_timeout, transport.request(method, params)).await {
            Ok(Ok(raw)) => RequestOutcome::Ok(raw),
            Ok(Err(err)) if is_connection_death(&err) => RequestOutcome::Dropped(err),
            Ok(Err(err)) => RequestOutcome::Protocol(err),
            Err(_) => RequestOutcome::Timeout(McpError::Transport(format!(
                "server `{}` timed out after {}ms calling `{method}`",
                self.name,
                self.call_timeout.as_millis()
            ))),
        }
    }

    /// Rebuild a dropped transport, honoring the backoff clock. Fails fast
    /// (without spawning) when the backoff window has not elapsed, when there
    /// is no reconnector (test clients built via [`McpClient::new`]), or when
    /// the fresh handshake fails — each path arms the clock for the next try.
    async fn reconnect_locked(&self, conn: &mut Connection) -> Result<(), McpError> {
        if let Some(at) = conn.health.next_retry_at {
            let now = Instant::now();
            if now < at {
                return Err(McpError::Closed(format!(
                    "server `{}` is down; next reconnect attempt in {:.1}s ({})",
                    self.name,
                    (at - now).as_secs_f64(),
                    conn.health
                        .last_error
                        .as_deref()
                        .unwrap_or("connection lost"),
                )));
            }
        }
        let Some(reconnect) = self.reconnect.as_ref() else {
            return Err(McpError::Closed(format!(
                "server `{}` connection is closed and has no reconnect source",
                self.name
            )));
        };

        conn.health.state = HealthState::Reconnecting;
        let transport = match reconnect().await {
            Ok(transport) => transport,
            Err(err) => {
                conn.tear_down(&err);
                return Err(err);
            }
        };
        // The fresh transport must pass the full handshake before we trust it;
        // we keep the *original* advertised tool set (reconnect restores
        // connectivity, it does not re-negotiate the surface mid-session).
        if let Err(err) = run_handshake(&self.name, transport.as_ref()).await {
            conn.tear_down(&err);
            return Err(err);
        }
        conn.transport = Some(transport);
        conn.mark_healthy();
        Ok(())
    }

    /// Orderly shutdown of the underlying transport (best-effort; an
    /// already-down connection is a no-op).
    pub async fn close(&self) -> Result<(), McpError> {
        let conn = self.conn.lock().await;
        match conn.transport.as_deref() {
            Some(transport) => transport.close().await,
            None => Ok(()),
        }
    }

    /// A point-in-time snapshot of this server's connection health, so the
    /// CLI/TUI/telemetry can render a clear, non-fatal diagnostic when a
    /// server drops and while it is reconnecting.
    pub async fn health(&self) -> ServerHealth {
        let conn = self.conn.lock().await;
        ServerHealth {
            name: self.name.clone(),
            state: conn.health.state,
            consecutive_failures: conn.health.consecutive_failures,
            last_error: conn.health.last_error.clone(),
            retry_in: conn.retry_in(),
        }
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

#[cfg(test)]
impl McpClient {
    /// Attach a reconnect factory for tests, so reconnect can be exercised
    /// over in-memory transports without spawning a real process. Each call to
    /// `f` must yield a *fresh* pre-handshake transport.
    pub(crate) fn with_test_reconnector<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Box<dyn Transport>, McpError>> + Send + 'static,
    {
        self.reconnect = Some(Arc::new(move || Box::pin(f())));
        self
    }
}

/// The negotiated result of a handshake, produced by [`run_handshake`] and
/// used by both the initial [`McpClient::initialize`] and each reconnect.
struct Handshake {
    negotiated_version: String,
    server_info: Option<Implementation>,
    tools: Vec<McpToolInfo>,
}

/// Build a fresh (pre-handshake) transport for `config`. Shared by the first
/// [`McpClient::connect`] and every later reconnect so both spawn the child
/// with the identical (scrubbed, for stdio) environment.
async fn build_transport(
    config: &McpServerConfig,
    timeout: Duration,
) -> Result<Box<dyn Transport>, McpError> {
    Ok(match &config.transport {
        McpTransport::Stdio { cmd, args, env } => {
            Box::new(StdioTransport::spawn(&config.name, cmd, args, env).await?)
        }
        McpTransport::Http { url, headers } => {
            Box::new(HttpTransport::new(&config.name, url, headers, timeout)?)
        }
    })
}

/// Run `initialize` → negotiate the version → `notifications/initialized` →
/// `tools/list` (all pages) over a fresh transport, returning the negotiated
/// surface. Free-standing so both the first handshake and reconnect share one
/// implementation.
async fn run_handshake(name: &str, transport: &dyn Transport) -> Result<Handshake, McpError> {
    let params = InitializeParams {
        protocol_version: PREFERRED_PROTOCOL_VERSION.to_string(),
        capabilities: serde_json::json!({}),
        client_info: Implementation {
            name: CLIENT_NAME.to_string(),
            version: CLIENT_VERSION.to_string(),
        },
    };
    let raw = transport.request("initialize", to_value(&params)?).await?;
    let result: InitializeResult = serde_json::from_value(raw)
        .map_err(|e| McpError::Protocol(format!("could not decode initialize result: {e}")))?;

    // Negotiate: accept the server's version if we can speak it; a server that
    // omits the field is read leniently as agreeing to our offer.
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

    // Complete the handshake, then discover tools.
    transport
        .notify("notifications/initialized", Value::Null)
        .await?;
    let tools = fetch_all_tools(name, transport).await?;
    Ok(Handshake {
        negotiated_version: version,
        server_info: result.server_info,
        tools,
    })
}

/// Drive `tools/list` to exhaustion over `transport`, following `nextCursor`.
async fn fetch_all_tools(
    name: &str,
    transport: &dyn Transport,
) -> Result<Vec<McpToolInfo>, McpError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_TOOL_PAGES {
        let params = ListToolsParams {
            cursor: cursor.clone(),
        };
        let raw = transport.request("tools/list", to_value(&params)?).await?;
        let page: ListToolsResult = serde_json::from_value(raw)
            .map_err(|e| McpError::Protocol(format!("could not decode tools/list result: {e}")))?;
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
        "server `{name}` exceeded {MAX_TOOL_PAGES} tools/list pages — cursor never terminated"
    )))
}

/// Map a raw `tools/call` result value into the engine's [`ToolOutput`].
fn decode_call_result(tool: &str, raw: Value) -> Result<ToolOutput, McpError> {
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

    /// Build a fresh, fully-healthy scripted transport (handshake + one queued
    /// `tools/call` success) — the shape a reconnect factory yields.
    fn healthy_transport() -> Box<dyn Transport> {
        let t = ScriptedTransport::new();
        t.push_ok(
            "initialize",
            serde_json::json!({ "protocolVersion": PREFERRED_PROTOCOL_VERSION }),
        );
        t.push_ok(
            "tools/list",
            serde_json::json!({ "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] }),
        );
        t.push_ok(
            "tools/call",
            serde_json::json!({ "content": [{ "type": "text", "text": "healed" }] }),
        );
        Box::new(t)
    }

    #[tokio::test]
    async fn a_dropped_connection_transparently_reconnects_and_retries() {
        // Initial transport: handshake succeeds, then the very first tools/call
        // drops the connection (child exited).
        let initial = ScriptedTransport::new();
        initial.push_ok(
            "initialize",
            serde_json::json!({ "protocolVersion": PREFERRED_PROTOCOL_VERSION }),
        );
        initial.push_ok(
            "tools/list",
            serde_json::json!({ "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] }),
        );
        initial.push_err("tools/call", McpError::Closed("child exited".into()));

        let mut client = McpClient::new("srv", Box::new(initial))
            .with_test_reconnector(|| async { Ok(healthy_transport()) });
        client.initialize().await.unwrap();

        // The dropped call self-heals: reconnect + retry happen inside the one
        // call, so the model sees success, not an error.
        let out = client
            .call_tool("echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            out,
            ToolOutput::Ok {
                content: "healed".into()
            }
        );

        let health = client.health().await;
        assert_eq!(health.state, HealthState::Live);
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.last_error.is_none());
    }

    #[tokio::test]
    async fn repeated_failures_back_off_and_fail_fast_without_blocking() {
        let initial = ScriptedTransport::new();
        initial.push_ok(
            "initialize",
            serde_json::json!({ "protocolVersion": PREFERRED_PROTOCOL_VERSION }),
        );
        initial.push_ok(
            "tools/list",
            serde_json::json!({ "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] }),
        );
        initial.push_err("tools/call", McpError::Closed("child exited".into()));

        // Reconnect always fails — the server is genuinely dead.
        let mut client = McpClient::new("srv", Box::new(initial)).with_test_reconnector(|| async {
            Err(McpError::Transport("connection refused".into()))
        });
        client.initialize().await.unwrap();

        // First call drops, then the in-call reconnect also fails: an error.
        assert!(
            client
                .call_tool("echo", serde_json::json!({}))
                .await
                .is_err()
        );

        // The backoff clock is now armed, so the next call fails *fast* — it
        // never spawns or blocks the turn — and names the wait.
        let err = client
            .call_tool("echo", serde_json::json!({}))
            .await
            .unwrap_err();
        let msg = err.user_message();
        assert!(msg.contains("is down"), "fast-fail names the state: {msg}");
        assert!(
            msg.contains("next reconnect attempt"),
            "hints at the backoff wait: {msg}"
        );

        let health = client.health().await;
        assert_eq!(health.state, HealthState::Down);
        assert!(health.consecutive_failures >= 2);
        assert!(health.last_error.is_some());
    }

    #[tokio::test]
    async fn a_hung_call_times_out_and_arms_reconnect_without_retrying_in_call() {
        // A transport that answers the handshake but hangs forever on any call.
        struct Hang;
        #[async_trait::async_trait]
        impl Transport for Hang {
            async fn request(&self, method: &str, _params: Value) -> Result<Value, McpError> {
                match method {
                    "initialize" => {
                        Ok(serde_json::json!({ "protocolVersion": PREFERRED_PROTOCOL_VERSION }))
                    }
                    "tools/list" => Ok(serde_json::json!({
                        "tools": [{ "name": "slow", "inputSchema": { "type": "object" } }]
                    })),
                    _ => {
                        std::future::pending::<()>().await;
                        unreachable!()
                    }
                }
            }
            async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
                Ok(())
            }
            async fn close(&self) -> Result<(), McpError> {
                Ok(())
            }
        }

        let mut client = McpClient::new("slow", Box::new(Hang));
        client.initialize().await.unwrap();
        client.set_call_timeout(Duration::from_millis(30));

        // The hung call times out (non-fatal, server-named) and does NOT
        // double-wait on an in-call retry — it just arms reconnect.
        let err = client
            .call_tool("slow", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            err.user_message().contains("timed out"),
            "{}",
            err.user_message()
        );

        let health = client.health().await;
        assert_eq!(health.state, HealthState::Down);
        assert_eq!(health.consecutive_failures, 1);
    }

    #[test]
    fn backoff_is_zero_on_first_failure_then_doubles_to_the_cap() {
        assert_eq!(backoff_delay(0), Duration::ZERO);
        assert_eq!(backoff_delay(1), Duration::ZERO);
        assert_eq!(backoff_delay(2), Duration::from_secs(1));
        assert_eq!(backoff_delay(3), Duration::from_secs(2));
        assert_eq!(backoff_delay(4), Duration::from_secs(4));
        // Grows exponentially but never past the cap, even at absurd counts.
        assert_eq!(backoff_delay(50), RECONNECT_CAP);
        assert_eq!(backoff_delay(u32::MAX), RECONNECT_CAP);
    }
}
