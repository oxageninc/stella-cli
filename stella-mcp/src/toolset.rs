//! [`McpToolSet`]: the bridge that makes external MCP servers' tools
//! indistinguishable from native tools to the engine. It implements
//! `stella_core::ports::ToolExecutor` — the same port `stella-tools`'
//! `ToolRegistry` implements — so `stella-core::Engine` drives MCP tools and
//! built-in tools through one seam, exactly as `driver.rs` consumes the trait.
//!
//! # Namespacing
//!
//! Every MCP tool is advertised as `mcp__<server>__<tool>` so tools from
//! different servers (and from the native set) never collide. Server names
//! are required to be non-empty and free of the `__` separator; a server whose
//! name violates that is skipped and recorded in
//! [`McpToolSet::failed_servers`]. With that guarantee the prefix uniquely
//! identifies the server and the remainder is the raw tool name, so routing is
//! unambiguous.
//!
//! # Composition & fall-through
//!
//! [`McpToolSet::wrapping`] layers the set over an inner native
//! `ToolExecutor`. `execute` routes an `mcp__…` name to its server; any other
//! name falls through to the native executor. An `mcp__…` name that matches no
//! connected server's tool is a model-visible error, never a panic — matching
//! `ToolRegistry`'s contract.
//!
//! # Resilience
//!
//! Every MCP call is bounded by a per-call timeout ([`McpClient`] owns it, so
//! a hang is observable rather than merely cancelled). A dead, hung, or
//! erroring server yields a `ToolOutput::Error` that *names the server*; it
//! never poisons sibling servers or the native tools, and it is never an `Err`
//! out of `execute` (tool failures are model-visible data).
//!
//! **Auto-reconnect (bounded backoff).** A server that drops mid-session is
//! not written off: the next call transparently respawns it (a single blip
//! self-heals within the turn), and repeated failures back off on an
//! increasing, capped interval so a long-dead server degrades gracefully
//! instead of aborting the agent. Per-server state is surfaced by
//! [`McpToolSet::health`] for a non-fatal CLI/TUI/telemetry diagnostic.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use stella_core::mcp_usage::{McpUsageLedger, McpUsageRecord, push_usage};
use stella_core::ports::ToolExecutor;
use stella_protocol::{ToolOutput, ToolSchema};

use crate::client::{McpClient, ServerHealth};
use crate::config::McpServerConfig;

/// A session-scoped set of server names disabled by the operator. Shared with
/// the CLI/TUI so a toggle takes effect on the next model call — the engine
/// re-reads [`McpToolSet::schemas`] each call, so a disabled server's tools
/// simply disappear from the advertised set (and any stray call errors).
pub type DisabledServers = Arc<Mutex<HashSet<String>>>;

/// The tool-namespace prefix.
const NS_PREFIX: &str = "mcp__";
/// The separator between the `mcp__`, server, and tool segments.
const NS_SEP: &str = "__";
/// Default per-call (and per-connect) timeout when the caller does not set one.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// A set of connected MCP servers exposed to the engine as one
/// `ToolExecutor`, optionally composed over an inner native executor.
pub struct McpToolSet {
    clients: Vec<McpClient>,
    /// Namespaced tool name -> (client index, raw tool name).
    routes: HashMap<String, (usize, String)>,
    /// Servers that could not be connected or had invalid names: `(name,
    /// reason)`. They advertise no tools but never block the rest.
    failed: Vec<(String, String)>,
    native: Option<Arc<dyn ToolExecutor>>,
    /// Where each successful MCP call is recorded (server, tool, reason, time)
    /// for the `mcp_usage` telemetry table. `None` = no telemetry (a no-op).
    usage: Option<McpUsageLedger>,
    /// Server names disabled for this session. A disabled server's tools are
    /// hidden from `schemas()` and its calls error, without disconnecting it.
    disabled: Option<DisabledServers>,
}

impl McpToolSet {
    /// Connect every server in `configs`. Connection is best-effort and
    /// isolated: a server that fails to connect (bad spawn, handshake error,
    /// timeout, duplicate/invalid name) is recorded in
    /// [`McpToolSet::failed_servers`] and contributes nothing — it never
    /// blocks the others. `timeout` bounds both each connect and (until
    /// overridden with [`McpToolSet::with_call_timeout`]) each later call.
    pub async fn connect(configs: &[McpServerConfig], timeout: Duration) -> Self {
        let mut clients = Vec::new();
        let mut failed = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for config in configs {
            if !is_namespaceable(&config.name) {
                failed.push((
                    config.name.clone(),
                    "server name is empty or contains the reserved `__` separator".into(),
                ));
                continue;
            }
            if !seen.insert(config.name.clone()) {
                failed.push((config.name.clone(), "duplicate server name".into()));
                continue;
            }
            match tokio::time::timeout(timeout, McpClient::connect(config, timeout)).await {
                Ok(Ok(client)) => clients.push(client),
                Ok(Err(err)) => failed.push((config.name.clone(), err.user_message())),
                Err(_) => failed.push((
                    config.name.clone(),
                    format!("connect timed out after {}ms", timeout.as_millis()),
                )),
            }
        }

        let mut set = Self {
            clients,
            routes: HashMap::new(),
            failed,
            native: None,
            usage: None,
            disabled: None,
        };
        set.rebuild_routes();
        set
    }

    /// Build from already-connected clients. Invalid or duplicate server names
    /// are dropped into [`McpToolSet::failed_servers`]. Uses
    /// [`DEFAULT_CALL_TIMEOUT`] until [`McpToolSet::with_call_timeout`].
    pub fn from_clients(clients: Vec<McpClient>) -> Self {
        let mut kept = Vec::new();
        let mut failed = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for client in clients {
            let name = client.name().to_string();
            if !is_namespaceable(&name) {
                failed.push((
                    name,
                    "server name is empty or contains the reserved `__` separator".into(),
                ));
                continue;
            }
            if !seen.insert(name.clone()) {
                failed.push((name, "duplicate server name".into()));
                continue;
            }
            kept.push(client);
        }

        let mut set = Self {
            clients: kept,
            routes: HashMap::new(),
            failed,
            native: None,
            usage: None,
            disabled: None,
        };
        set.rebuild_routes();
        set
    }

    /// Compose over an inner native `ToolExecutor`. Any non-`mcp__` tool name
    /// falls through to it.
    pub fn wrapping(mut self, native: Arc<dyn ToolExecutor>) -> Self {
        self.native = Some(native);
        self
    }

    /// Record every successful MCP call into `ledger` (server, tool, reason,
    /// call time) for the `mcp_usage` telemetry table. Without this the set
    /// still works — telemetry is simply not collected.
    pub fn with_usage_ledger(mut self, ledger: McpUsageLedger) -> Self {
        self.usage = Some(ledger);
        self
    }

    /// Consult `disabled` (a shared, session-scoped set of server names) so a
    /// disabled server's tools are hidden and its calls error, live, without a
    /// reconnect. The set is shared with the CLI/TUI, which toggles it.
    pub fn with_disabled_servers(mut self, disabled: DisabledServers) -> Self {
        self.disabled = Some(disabled);
        self
    }

    /// Whether `server` is currently disabled for this session.
    fn is_disabled(&self, server: &str) -> bool {
        self.disabled.as_ref().is_some_and(|set| {
            set.lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(server)
        })
    }

    /// Override the per-call timeout (default [`DEFAULT_CALL_TIMEOUT`]),
    /// propagating it to every connected server so the client owns the bound
    /// (and can treat a hang as a reconnect trigger).
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        for client in &mut self.clients {
            client.set_call_timeout(timeout);
        }
        self
    }

    /// Per-server connection health, for a non-fatal CLI/TUI/telemetry
    /// diagnostic (which servers are live, reconnecting, or backing off).
    pub async fn health(&self) -> Vec<ServerHealth> {
        let mut out = Vec::with_capacity(self.clients.len());
        for client in &self.clients {
            out.push(client.health().await);
        }
        out
    }

    /// Servers that were not connected, as `(name, reason)`.
    pub fn failed_servers(&self) -> &[(String, String)] {
        &self.failed
    }

    /// How many servers are live.
    pub fn connected_count(&self) -> usize {
        self.clients.len()
    }

    /// The live servers' names, in connect order — the counterpart of
    /// [`McpToolSet::failed_servers`] for a caller's connect-outcome report.
    pub fn connected_names(&self) -> Vec<&str> {
        self.clients.iter().map(|c| c.name()).collect()
    }

    /// Close every connected server's transport in order (best-effort).
    pub async fn close_all(&self) {
        for client in &self.clients {
            let _ = client.close().await;
        }
    }

    /// (Re)build the routing map from the current clients. Server names are
    /// already validated unique + namespaceable, so no two entries collide.
    fn rebuild_routes(&mut self) {
        self.routes.clear();
        for (idx, client) in self.clients.iter().enumerate() {
            for tool in client.tools() {
                let namespaced = namespaced_name(client.name(), &tool.name);
                self.routes.insert(namespaced, (idx, tool.name.clone()));
            }
        }
    }

    /// Route one MCP call, mapping every failure mode to a server-named
    /// `ToolOutput::Error`. The per-call timeout and auto-reconnect now live in
    /// [`McpClient::call_tool`] (so a hang can trigger a reconnect); this layer
    /// only names the server and turns an `Err` into model-visible data.
    async fn execute_mcp(&self, client: &McpClient, raw_tool: &str, input: &Value) -> ToolOutput {
        match client.call_tool(raw_tool, input.clone()).await {
            Ok(output) => {
                // Record the successful call for the `mcp_usage` telemetry
                // table. `reason` is best-effort: external MCP tools rarely
                // carry one, so it is usually empty.
                if let Some(ledger) = &self.usage {
                    let reason = input
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    push_usage(ledger, McpUsageRecord::now(client.name(), raw_tool, reason));
                }
                output
            }
            Err(err) => ToolOutput::Error {
                message: format!(
                    "mcp server `{}` failed calling `{raw_tool}`: {}",
                    client.name(),
                    err.user_message()
                ),
            },
        }
    }
}

#[async_trait]
impl ToolExecutor for McpToolSet {
    fn schemas(&self) -> Vec<ToolSchema> {
        let mut schemas = Vec::new();
        // Native tools first — they are the base layer the MCP set augments.
        if let Some(native) = &self.native {
            schemas.extend(native.schemas());
        }
        for (idx, client) in self.clients.iter().enumerate() {
            // A disabled server advertises nothing this session — the engine
            // re-reads schemas each model call, so the model stops seeing its
            // tools the moment it is toggled off.
            if self.is_disabled(client.name()) {
                continue;
            }
            for tool in client.tools() {
                let namespaced = namespaced_name(client.name(), &tool.name);
                // Only advertise tools that actually route back to this client
                // (defends against any skipped/collided entry).
                if self.routes.get(&namespaced).map(|(i, _)| *i) == Some(idx) {
                    schemas.push(ToolSchema {
                        name: namespaced,
                        description: tool.description.clone(),
                        input_schema: tool.input_schema.clone(),
                        // External MCP tools are unknown — treat as mutating,
                        // the safe direction (never auto-parallelized).
                        read_only: false,
                    });
                }
            }
        }
        schemas
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        if let Some((idx, raw_tool)) = self.routes.get(name) {
            let client = &self.clients[*idx];
            if self.is_disabled(client.name()) {
                return ToolOutput::Error {
                    message: format!(
                        "mcp server `{}` is disabled for this session — tool `{name}` unavailable",
                        client.name()
                    ),
                };
            }
            return self.execute_mcp(client, raw_tool, input).await;
        }
        // A namespaced name we don't recognize is an MCP miss, not a native
        // tool — never fall through to native for it.
        if name.starts_with(NS_PREFIX) {
            return ToolOutput::Error {
                message: format!(
                    "unknown MCP tool `{name}` — not advertised by any connected server"
                ),
            };
        }
        match &self.native {
            Some(native) => native.execute(name, input).await,
            None => ToolOutput::Error {
                message: format!("unknown tool `{name}` (no native tools configured)"),
            },
        }
    }
}

/// Compose the namespaced tool name for a server/tool pair.
fn namespaced_name(server: &str, tool: &str) -> String {
    format!("{NS_PREFIX}{server}{NS_SEP}{tool}")
}

/// A server name may be used as a namespace segment only if it is non-empty
/// and does not contain the `__` separator (which would make the prefix
/// ambiguous).
fn is_namespaceable(name: &str) -> bool {
    !name.is_empty() && !name.contains(NS_SEP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::McpError;
    use crate::protocol::PREFERRED_PROTOCOL_VERSION;
    use crate::transport::Transport;
    use crate::transport::testkit::ScriptedTransport;

    /// A fake native executor advertising one tool, `bash`.
    struct FakeNative;
    #[async_trait]
    impl ToolExecutor for FakeNative {
        fn schemas(&self) -> Vec<ToolSchema> {
            vec![ToolSchema {
                name: "bash".into(),
                description: "run a command".into(),
                input_schema: serde_json::json!({ "type": "object" }),
                read_only: false,
            }]
        }
        async fn execute(&self, name: &str, _input: &Value) -> ToolOutput {
            ToolOutput::Ok {
                content: format!("native ran {name}"),
            }
        }
    }

    /// A transport that never answers `tools/call` — used to prove the
    /// per-call timeout fires and names the server.
    struct HangingTransport;
    #[async_trait]
    impl Transport for HangingTransport {
        async fn request(&self, method: &str, _params: Value) -> Result<Value, McpError> {
            if method == "tools/call" {
                // Never resolves within the test's short call timeout.
                std::future::pending::<()>().await;
                unreachable!()
            }
            // Handshake methods answer instantly.
            match method {
                "initialize" => {
                    Ok(serde_json::json!({ "protocolVersion": PREFERRED_PROTOCOL_VERSION }))
                }
                "tools/list" => Ok(serde_json::json!({
                    "tools": [{ "name": "slow", "inputSchema": { "type": "object" } }]
                })),
                _ => Ok(Value::Null),
            }
        }
        async fn notify(&self, _method: &str, _params: Value) -> Result<(), McpError> {
            Ok(())
        }
        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    async fn connected_client(name: &str, tool: &str) -> McpClient {
        let transport = ScriptedTransport::new();
        transport.push_ok(
            "initialize",
            serde_json::json!({ "protocolVersion": PREFERRED_PROTOCOL_VERSION }),
        );
        transport.push_ok(
            "tools/list",
            serde_json::json!({ "tools": [{ "name": tool, "inputSchema": { "type": "object" } }] }),
        );
        // Pre-queue a successful call for the routing test.
        transport.push_ok(
            "tools/call",
            serde_json::json!({ "content": [{ "type": "text", "text": "mcp ran" }] }),
        );
        let mut client = McpClient::new(name, Box::new(transport));
        client.initialize().await.unwrap();
        client
    }

    #[tokio::test]
    async fn connected_names_lists_live_servers_in_order() {
        let a = connected_client("files", "read").await;
        let b = connected_client("search", "grep").await;
        let set = McpToolSet::from_clients(vec![a, b]);
        assert_eq!(set.connected_names(), vec!["files", "search"]);
        assert!(set.failed_servers().is_empty());
    }

    #[tokio::test]
    async fn schemas_namespace_mcp_tools_and_include_native() {
        let client = connected_client("files", "read").await;
        let set = McpToolSet::from_clients(vec![client]).wrapping(Arc::new(FakeNative));

        let names: HashSet<String> = set.schemas().into_iter().map(|s| s.name).collect();
        assert!(names.contains("mcp__files__read"), "namespaced MCP tool");
        assert!(names.contains("bash"), "native tool passes through");
        assert_eq!(names.len(), 2);
    }

    #[tokio::test]
    async fn execute_routes_by_prefix_and_falls_through_to_native() {
        let client = connected_client("files", "read").await;
        let set = McpToolSet::from_clients(vec![client]).wrapping(Arc::new(FakeNative));

        // Routes to the MCP server.
        let mcp = set
            .execute("mcp__files__read", &serde_json::json!({ "path": "x" }))
            .await;
        assert_eq!(
            mcp,
            ToolOutput::Ok {
                content: "mcp ran".into()
            }
        );

        // Falls through to native.
        let native = set.execute("bash", &serde_json::json!({})).await;
        assert_eq!(
            native,
            ToolOutput::Ok {
                content: "native ran bash".into()
            }
        );
    }

    #[tokio::test]
    async fn unknown_mcp_tool_is_an_error_not_a_fallthrough() {
        let client = connected_client("files", "read").await;
        let set = McpToolSet::from_clients(vec![client]).wrapping(Arc::new(FakeNative));
        let out = set.execute("mcp__files__missing", &Value::Null).await;
        match out {
            ToolOutput::Error { message } => assert!(message.contains("unknown MCP tool")),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_native_tool_errors_when_no_native_configured() {
        let client = connected_client("files", "read").await;
        let set = McpToolSet::from_clients(vec![client]);
        let out = set.execute("bash", &Value::Null).await;
        assert!(out.is_error());
    }

    #[tokio::test]
    async fn a_hung_server_times_out_naming_the_server_without_poisoning_native() {
        let mut client = McpClient::new("slowsrv", Box::new(HangingTransport));
        client.initialize().await.unwrap();
        let set = McpToolSet::from_clients(vec![client])
            .wrapping(Arc::new(FakeNative))
            .with_call_timeout(Duration::from_millis(50));

        // The hung MCP call times out with a server-named error…
        let hung = set.execute("mcp__slowsrv__slow", &Value::Null).await;
        match hung {
            ToolOutput::Error { message } => {
                assert!(message.contains("slowsrv"), "names the server: {message}");
                assert!(message.contains("timed out"));
            }
            other => panic!("expected a timeout error, got {other:?}"),
        }

        // …and the native tool still works — the dead server didn't poison it.
        let native = set.execute("bash", &Value::Null).await;
        assert_eq!(
            native,
            ToolOutput::Ok {
                content: "native ran bash".into()
            }
        );
    }

    #[tokio::test]
    async fn duplicate_and_invalid_server_names_are_recorded_not_advertised() {
        let a = connected_client("dup", "x").await;
        let b = connected_client("dup", "y").await;
        let bad = connected_client("has__sep", "z").await;
        let set = McpToolSet::from_clients(vec![a, b, bad]);

        assert_eq!(set.connected_count(), 1, "only the first `dup` is kept");
        let reasons: Vec<&str> = set
            .failed_servers()
            .iter()
            .map(|(_, r)| r.as_str())
            .collect();
        assert!(reasons.iter().any(|r| r.contains("duplicate")));
        assert!(reasons.iter().any(|r| r.contains("reserved")));
    }

    #[tokio::test]
    async fn usage_ledger_records_a_successful_call_with_server_tool_and_reason() {
        let client = connected_client("files", "read").await;
        let ledger: McpUsageLedger = Arc::default();
        let set = McpToolSet::from_clients(vec![client]).with_usage_ledger(ledger.clone());

        let out = set
            .execute(
                "mcp__files__read",
                &serde_json::json!({ "reason": "inspect config" }),
            )
            .await;
        assert!(!out.is_error());

        let drained = stella_core::mcp_usage::drain_usage(&ledger);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].server, "files");
        assert_eq!(drained[0].tool, "read");
        assert_eq!(drained[0].reason, "inspect config");
        assert!(drained[0].called_at_ms > 0);
    }

    #[tokio::test]
    async fn disabled_server_is_hidden_from_schemas_and_errors_on_execute() {
        let client = connected_client("files", "read").await;
        let disabled: DisabledServers = Arc::new(Mutex::new(HashSet::new()));
        disabled.lock().unwrap().insert("files".to_string());
        let set = McpToolSet::from_clients(vec![client]).with_disabled_servers(disabled.clone());

        // Hidden from the advertised schema while disabled.
        assert!(
            set.schemas().iter().all(|s| s.name != "mcp__files__read"),
            "disabled server's tool must not be advertised"
        );
        // And a direct call errors, naming the disabled server.
        match set.execute("mcp__files__read", &Value::Null).await {
            ToolOutput::Error { message } => assert!(message.contains("disabled")),
            other => panic!("expected a disabled error, got {other:?}"),
        }

        // Re-enabling (clearing the set) makes the tool visible again — live,
        // no reconnect.
        disabled.lock().unwrap().clear();
        assert!(
            set.schemas().iter().any(|s| s.name == "mcp__files__read"),
            "re-enabled server's tool must reappear"
        );
    }
}
