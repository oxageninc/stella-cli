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
use crate::oauth::OAuthManager;

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
    /// Connected server names opted into the Best-of-N candidate allowlist
    /// (`.stella/mcp.toml`'s `candidate_safe = true`, issue #248 Phase 1).
    /// Populated from the configs at connect time; see
    /// [`McpToolSet::for_candidates`].
    candidate_safe: HashSet<String>,
}

impl McpToolSet {
    /// Connect every server in `configs`. Connection is best-effort and
    /// isolated: a server that fails to connect (bad spawn, handshake error,
    /// timeout, duplicate/invalid name) is recorded in
    /// [`McpToolSet::failed_servers`] and contributes nothing — it never
    /// blocks the others. `timeout` bounds both each connect and (until
    /// overridden with [`McpToolSet::with_call_timeout`]) each later call.
    pub async fn connect(configs: &[McpServerConfig], timeout: Duration) -> Self {
        Self::connect_with_auth(configs, timeout, None).await
    }

    /// [`McpToolSet::connect`], with an optional [`OAuthManager`] that gives
    /// every HTTP server a lazy OAuth bearer source (see [`crate::oauth`]).
    /// Hosts that support `stella mcp login` pass one; everything else is
    /// unchanged.
    pub async fn connect_with_auth(
        configs: &[McpServerConfig],
        timeout: Duration,
        auth: Option<Arc<OAuthManager>>,
    ) -> Self {
        let mut clients = Vec::new();
        let mut failed = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let candidate_safe: HashSet<String> = configs
            .iter()
            .filter(|c| c.candidate_safe)
            .map(|c| c.name.clone())
            .collect();

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
            match tokio::time::timeout(
                timeout,
                McpClient::connect_with_auth(config, timeout, auth.clone()),
            )
            .await
            {
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
            candidate_safe,
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
            candidate_safe: HashSet::new(),
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

    /// Flag `names` as Best-of-N candidate-safe (issue #248 Phase 1) —
    /// [`McpToolSet::connect_with_auth`] does this from `.stella/mcp.toml`
    /// automatically; this is for [`McpToolSet::from_clients`] callers (tests,
    /// or any future non-config-driven construction) that need to set it
    /// explicitly.
    pub fn with_candidate_safe_servers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.candidate_safe = names.into_iter().map(Into::into).collect();
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

    /// Whether `namespaced` (an `mcp__server__tool` name) routes to a
    /// currently-connected, non-disabled server flagged `candidate_safe` —
    /// the Best-of-N allowlist check (issue #248 Phase 1). A name that
    /// doesn't route to any server (including every plain native tool name,
    /// which never matches an `mcp__…` route) is `false`.
    pub fn is_candidate_safe_tool(&self, namespaced: &str) -> bool {
        self.routes.get(namespaced).is_some_and(|(idx, _)| {
            let name = self.clients[*idx].name();
            self.candidate_safe.contains(name) && !self.is_disabled(name)
        })
    }

    /// Connected, non-disabled server names flagged `candidate_safe`, in
    /// connect order — the counterpart of [`McpToolSet::connected_names`] for
    /// a Best-of-N status/diagnostic surface.
    pub fn candidate_safe_server_names(&self) -> Vec<&str> {
        self.clients
            .iter()
            .map(|c| c.name())
            .filter(|name| self.candidate_safe.contains(*name) && !self.is_disabled(name))
            .collect()
    }

    /// Build a Best-of-N candidate's tool surface (issue #248 Phase 1):
    /// `native` (the candidate's own snapshot-rooted registry + custom
    /// tools) composed with a READ-ONLY, filtered view over `self` — only
    /// tools from `candidate_safe`-flagged servers are advertised or
    /// callable; the same connected clients answer every candidate's calls,
    /// no per-candidate subprocess. Requires `Arc<Self>` (not `&self`) so the
    /// returned view can outlive the session's own borrow of the set — see
    /// `stella-cli/src/candidate_ws.rs`'s module doc for the full rationale
    /// and why every OTHER server (and `ask_user`) stays withheld.
    pub fn for_candidates(self: &Arc<Self>, native: Arc<dyn ToolExecutor>) -> CandidateMcpView {
        CandidateMcpView {
            inner: Arc::clone(self),
            native,
        }
    }

    /// The orchestrator's Best-of-N pre-fetch (issue #248 Phase 1): call
    /// every connected, non-disabled, `candidate_safe`-flagged server's
    /// ZERO-required-input tools ONCE, concatenating their output as shared
    /// context every candidate can start from — the common "candidates all
    /// need the same DB schema" case, at the cost of one round trip instead
    /// of N. A tool whose schema requires input is skipped outright: this
    /// never synthesizes an argument, it only ever calls a tool with a
    /// genuinely empty `{}`. `None` when nothing is safe to call, or every
    /// call errored or came back empty — a prefetch miss, never a panic.
    pub async fn prefetch_candidate_context(&self) -> Option<String> {
        let mut sections = Vec::new();
        for schema in self.schemas() {
            if !self.is_candidate_safe_tool(&schema.name)
                || !accepts_empty_input(&schema.input_schema)
            {
                continue;
            }
            let empty = Value::Object(serde_json::Map::new());
            if let ToolOutput::Ok { content } = self.execute(&schema.name, &empty).await
                && !content.trim().is_empty()
            {
                sections.push(format!("### {}\n{}", schema.name, content.trim()));
            }
        }
        (!sections.is_empty()).then(|| sections.join("\n\n"))
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

/// A Best-of-N candidate's tool surface (issue #248 Phase 1): built by
/// [`McpToolSet::for_candidates`]. `execute`/`schemas` route an `mcp__…` name
/// through `inner` ONLY when [`McpToolSet::is_candidate_safe_tool`] allows
/// it; every other name — including a `mcp__…` miss on a withheld server —
/// falls to `native` or a loud, model-visible error, never to `inner`'s own
/// (real-tree-rooted) native layer, which this view never advertises or
/// calls.
pub struct CandidateMcpView {
    inner: Arc<McpToolSet>,
    native: Arc<dyn ToolExecutor>,
}

#[async_trait]
impl ToolExecutor for CandidateMcpView {
    fn schemas(&self) -> Vec<ToolSchema> {
        let mut schemas = self.native.schemas();
        // `inner.schemas()` also carries `inner`'s own native layer (the
        // SESSION's real-tree tools) if it wraps one — but those never match
        // an `mcp__…` route, so `is_candidate_safe_tool` (route lookup only)
        // naturally excludes them here without special-casing.
        schemas.extend(
            self.inner
                .schemas()
                .into_iter()
                .filter(|s| self.inner.is_candidate_safe_tool(&s.name)),
        );
        schemas
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        if name.starts_with(NS_PREFIX) {
            return if self.inner.is_candidate_safe_tool(name) {
                self.inner.execute(name, input).await
            } else {
                ToolOutput::Error {
                    message: format!(
                        "mcp tool `{name}` is withheld from Best-of-N candidates — its \
                         server is not marked `candidate_safe` in .stella/mcp.toml"
                    ),
                }
            };
        }
        self.native.execute(name, input).await
    }
}

/// Compose the namespaced tool name for a server/tool pair.
fn namespaced_name(server: &str, tool: &str) -> String {
    format!("{NS_PREFIX}{server}{NS_SEP}{tool}")
}

/// Whether `schema` can be called with an empty `{}` — no `required` array,
/// or an empty one. Used ONLY to decide whether [`McpToolSet::prefetch_candidate_context`]
/// may call a tool blind; never to synthesize a value for a required field.
fn accepts_empty_input(schema: &Value) -> bool {
    schema
        .get("required")
        .and_then(Value::as_array)
        .is_none_or(|required| required.is_empty())
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
        connected_client_with_schema(name, tool, serde_json::json!({ "type": "object" })).await
    }

    /// Like [`connected_client`], with a caller-chosen `inputSchema` — for
    /// proving [`McpToolSet::prefetch_candidate_context`]'s zero-required-input
    /// filter against a tool that DOES require one.
    async fn connected_client_with_schema(
        name: &str,
        tool: &str,
        input_schema: Value,
    ) -> McpClient {
        let transport = ScriptedTransport::new();
        transport.push_ok(
            "initialize",
            serde_json::json!({ "protocolVersion": PREFERRED_PROTOCOL_VERSION }),
        );
        transport.push_ok(
            "tools/list",
            serde_json::json!({ "tools": [{ "name": tool, "inputSchema": input_schema }] }),
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

    // ---- for_candidates / CandidateMcpView (issue #248 Phase 1) ----------

    #[tokio::test]
    async fn candidate_view_advertises_only_allowlisted_mcp_tools_plus_native() {
        let docs = connected_client("docs", "search").await;
        let fs = connected_client("fs", "write").await;
        let set = Arc::new(
            McpToolSet::from_clients(vec![docs, fs]).with_candidate_safe_servers(["docs"]),
        );
        let view = set.for_candidates(Arc::new(FakeNative));

        let names: HashSet<String> = view.schemas().into_iter().map(|s| s.name).collect();
        assert!(names.contains("mcp__docs__search"), "allowlisted server");
        assert!(names.contains("bash"), "candidate's own native tool");
        assert!(
            !names.contains("mcp__fs__write"),
            "non-candidate_safe server must not be advertised: {names:?}"
        );
        assert_eq!(names.len(), 2);
    }

    #[tokio::test]
    async fn candidate_view_executes_an_allowlisted_tool_and_falls_through_to_native() {
        let docs = connected_client("docs", "search").await;
        let set =
            Arc::new(McpToolSet::from_clients(vec![docs]).with_candidate_safe_servers(["docs"]));
        let view = set.for_candidates(Arc::new(FakeNative));

        let mcp_out = view.execute("mcp__docs__search", &Value::Null).await;
        assert_eq!(
            mcp_out,
            ToolOutput::Ok {
                content: "mcp ran".into()
            }
        );
        let native_out = view.execute("bash", &Value::Null).await;
        assert_eq!(
            native_out,
            ToolOutput::Ok {
                content: "native ran bash".into()
            }
        );
    }

    #[tokio::test]
    async fn candidate_view_denies_a_non_allowlisted_mcp_tool_with_a_named_error() {
        let fs = connected_client("fs", "write").await;
        // No `with_candidate_safe_servers` call — nothing is allowlisted.
        let set = Arc::new(McpToolSet::from_clients(vec![fs]));
        let view = set.for_candidates(Arc::new(FakeNative));

        match view.execute("mcp__fs__write", &Value::Null).await {
            ToolOutput::Error { message } => {
                assert!(message.contains("mcp__fs__write"), "{message}");
                assert!(message.contains("candidate_safe"), "{message}");
            }
            other => panic!("expected a withheld error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn candidate_view_respects_the_session_disabled_set() {
        // A server can be candidate_safe AND toggled off mid-session — the
        // candidate view must honor the live disable, same as the main set.
        let docs = connected_client("docs", "search").await;
        let disabled: DisabledServers = Arc::new(Mutex::new(HashSet::new()));
        disabled.lock().unwrap().insert("docs".to_string());
        let set = Arc::new(
            McpToolSet::from_clients(vec![docs])
                .with_candidate_safe_servers(["docs"])
                .with_disabled_servers(disabled),
        );
        let view = set.for_candidates(Arc::new(FakeNative));

        assert!(
            view.schemas().iter().all(|s| s.name != "mcp__docs__search"),
            "disabled server must not be advertised even if candidate_safe"
        );
        assert!(
            view.execute("mcp__docs__search", &Value::Null)
                .await
                .is_error()
        );
    }

    // ---- prefetch_candidate_context (issue #248 Phase 1) ------------------

    #[tokio::test]
    async fn prefetch_calls_only_candidate_safe_zero_arg_tools() {
        let docs = connected_client("docs", "search").await; // {} schema
        let ticket = connected_client_with_schema(
            "ticket",
            "get",
            serde_json::json!({ "type": "object", "required": ["id"] }),
        )
        .await;
        let fs = connected_client("fs", "write").await; // zero-arg, but NOT allowlisted
        let set = McpToolSet::from_clients(vec![docs, ticket, fs])
            .with_candidate_safe_servers(["docs", "ticket"]);

        let context = set
            .prefetch_candidate_context()
            .await
            .expect("docs has a zero-arg candidate_safe tool");
        assert!(context.contains("mcp__docs__search"), "{context}");
        assert!(
            !context.contains("mcp__ticket__get"),
            "a tool requiring input must never be called blind: {context}"
        );
        assert!(
            !context.contains("mcp__fs__write"),
            "a non-candidate_safe server must not be prefetched: {context}"
        );
    }

    #[tokio::test]
    async fn prefetch_returns_none_when_nothing_is_safe_to_call() {
        let fs = connected_client("fs", "write").await;
        // No `with_candidate_safe_servers` call — nothing is allowlisted.
        let set = McpToolSet::from_clients(vec![fs]);
        assert!(set.prefetch_candidate_context().await.is_none());
    }
}
