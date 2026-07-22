//! stdio transport: spawn an MCP server as a child process and speak
//! newline-delimited JSON-RPC over its stdin/stdout.
//!
//! Two properties are load-bearing:
//!
//! 1. **Explicit environment.** The child is spawned
//!    with [`Command::env_clear`] and receives only keys explicitly listed in
//!    the server's config `env`, plus `PATH`. No parent-shell credential
//!    (`ANTHROPIC_API_KEY`, `AWS_*`, …) is inherited. Explicit config values
//!    remain available because they are MCP servers' documented auth channel;
//!    `PATH` is the sole inherited exception — it is not a secret and
//!    a bare runner command (`npx`, `uvx`, `docker`, …) cannot resolve
//!    without it — and a config `env` may still override it.
//! 2. **Concurrent in-flight requests.** Each request gets a monotonically
//!    increasing id and a `oneshot` slot in a pending-map; a single reader
//!    task demultiplexes responses back to the right waiter by id. Many
//!    requests can be outstanding at once.
//!
//! Server stderr is discarded (`Stdio::null`) so a server that logs to stderr
//! cannot corrupt the JSON-RPC framing on stdout. Non-JSON lines that *do*
//! appear on stdout (a misbehaving server logging to the wrong stream) are
//! tolerated — skipped, never fatal.
//!
//! **No auto-reconnect.** A child that exits stays gone for the session: the
//! reader drains every outstanding and future request with
//! [`McpError::Closed`], and [`crate::McpToolSet`] turns that into a
//! `ToolOutput::Error` naming the server. Reconnection is a caller decision,
//! not a silent transport behavior.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::error::McpError;
use crate::protocol::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest};
use crate::transport::Transport;

/// How long `close()` waits for a clean exit after closing stdin before it
/// resorts to SIGKILL.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;

/// A live stdio connection to one MCP server.
pub struct StdioTransport {
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
    pending: Pending,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    reader: Mutex<Option<JoinHandle<()>>>,
    server_name: String,
}

impl StdioTransport {
    /// Spawn `cmd args…` with a scrubbed environment plus exactly the keys in
    /// `env`, and start the reader task. `server_name` is used only to make
    /// error messages self-identifying.
    pub async fn spawn(
        server_name: &str,
        cmd: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<Self, McpError> {
        let mut command = Command::new(cmd);
        command
            .args(args)
            .env_clear() // SCRUB — no ambient inheritance.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // keep server logs off the JSON-RPC stream.
            .kill_on_drop(true);
        // The environment is scrubbed by design so no ambient *credential*
        // (`ANTHROPIC_API_KEY`, `AWS_*`, …) ever leaks into an MCP subprocess.
        // `PATH` is the one exception: it is not a secret, and without it a
        // server invoked by a bare runner name — `npx`, `uvx`, `docker`,
        // `dnx`, `node` — cannot be found at all, which is exactly how the
        // registry installs npm/pypi/oci servers. So PATH is inherited from
        // the parent unless the config pins its own; everything else stays
        // scrubbed. (An absolute `cmd` path needs no PATH and is unaffected.)
        if !env.contains_key("PATH")
            && let Some(path) = std::env::var_os("PATH")
        {
            command.env("PATH", path);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .map_err(|e| McpError::Transport(format!("failed to spawn `{cmd}`: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child process has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child process has no stdout".into()))?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let reader = tokio::spawn(read_loop(stdout, pending.clone(), closed.clone()));

        Ok(Self {
            stdin: Mutex::new(Some(stdin)),
            child: Mutex::new(Some(child)),
            pending,
            next_id: AtomicU64::new(1),
            closed,
            reader: Mutex::new(Some(reader)),
            server_name: server_name.to_string(),
        })
    }

    fn closed_error(&self) -> McpError {
        McpError::Closed(format!("server `{}` is not connected", self.server_name))
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(self.closed_error());
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let request = JsonRpcRequest::new(id, method, params);
        let line = encode_line(&request)?;

        // Write under the stdin lock; on any write failure, reclaim the
        // pending slot so it never leaks.
        if let Err(e) = self.write_line(&line).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        // The reader task fulfills this via the oneshot. A dropped sender
        // (reader exited on EOF) surfaces as a closed connection.
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(self.closed_error()),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(self.closed_error());
        }
        let note = JsonRpcNotification::new(method, params);
        let line = encode_line(&note)?;
        self.write_line(&line).await
    }

    async fn close(&self) -> Result<(), McpError> {
        self.closed.store(true, Ordering::SeqCst);

        // Close stdin to signal EOF to a well-behaved server.
        {
            let mut guard = self.stdin.lock().await;
            guard.take(); // dropping ChildStdin closes it.
        }

        // Wait briefly for a clean exit, then kill.
        {
            let mut guard = self.child.lock().await;
            if let Some(mut child) = guard.take() {
                match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
                    Ok(_) => {}
                    Err(_) => {
                        let _ = child.kill().await;
                    }
                }
            }
        }

        // Stop the reader task if it hasn't already exited on EOF.
        if let Some(handle) = self.reader.lock().await.take() {
            handle.abort();
        }
        Ok(())
    }
}

impl StdioTransport {
    async fn write_line(&self, line: &str) -> Result<(), McpError> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or_else(|| self.closed_error())?;
        stdin.write_all(line.as_bytes()).await.map_err(|e| {
            McpError::Transport(format!("write to `{}` failed: {e}", self.server_name))
        })?;
        stdin.flush().await.map_err(|e| {
            McpError::Transport(format!("flush to `{}` failed: {e}", self.server_name))
        })?;
        Ok(())
    }
}

/// Serialize a JSON-RPC value to a single newline-terminated line.
fn encode_line<T: serde::Serialize>(value: &T) -> Result<String, McpError> {
    let mut line = serde_json::to_string(value).map_err(|e| McpError::Protocol(e.to_string()))?;
    line.push('\n');
    Ok(line)
}

/// Reader task: demultiplex JSON-RPC messages from the child's stdout back to
/// their waiting requests. Exits on EOF, then fails every outstanding request
/// so no caller hangs on a dead server.
async fn read_loop(stdout: ChildStdout, pending: Pending, closed: Arc<AtomicBool>) {
    let mut lines = BufReader::new(stdout).lines();
    // The loop ends on `Ok(None)` (EOF) or a read error — either way the
    // connection is gone and we fall through to drain every pending request.
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Non-JSON noise on stdout is tolerated: skip it, keep reading.
        let Ok(message) = serde_json::from_str::<JsonRpcMessage>(trimmed) else {
            continue;
        };
        // Only a genuine RESPONSE fulfills its waiter. A server-initiated
        // request (e.g. `ping`, which carries a `method` AND an id) or a
        // notification must never be routed to a pending client waiter — an id
        // collision would otherwise hand a caller the server's request as its
        // answer, failing the real call and orphaning the real response. An
        // unknown/timed-out id is dropped — v1 drives none of the
        // server->client surface.
        if message.is_response()
            && let Some(id) = message.correlated_id()
            && let Some(tx) = pending.lock().await.remove(&id)
        {
            let _ = tx.send(message.into_result());
        }
    }

    closed.store(true, Ordering::SeqCst);
    let mut map = pending.lock().await;
    for (_id, tx) in map.drain() {
        let _ = tx.send(Err(McpError::Closed(
            "server closed the connection before responding".into(),
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawning_a_missing_binary_is_a_transport_error() {
        let env = BTreeMap::new();
        let result =
            StdioTransport::spawn("ghost", "definitely-not-a-real-binary-xyzzy", &[], &env).await;
        assert!(matches!(result, Err(McpError::Transport(_))));
    }

    /// A bare runner command (the shape the registry installs for
    /// npm/pypi/oci servers — `npx`, `uvx`, `docker`) must resolve via an
    /// inherited PATH even though the environment is otherwise scrubbed.
    /// Without the PATH pass-through this failed to spawn — every
    /// registry-installed stdio server was dead on arrival.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_bare_runner_command_resolves_via_inherited_path() {
        // `cat` is a bare name that only resolves through PATH; with the
        // scrub-everything-but-PATH policy it must still be found and spawned.
        let env = BTreeMap::new();
        let transport = StdioTransport::spawn("cat-server", "cat", &[], &env)
            .await
            .expect("a bare runner must resolve via the inherited PATH");
        let _ = transport.close().await;
    }
}
