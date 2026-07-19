//! Long-lived process group: `start_process`, `read_output`, `send_stdin`,
//! `stop_process`.
//!
//! For servers, REPLs, and watchers — anything that outlives one tool call.
//! One-shot work belongs in `run_tests` / `build_project` / `run_script`,
//! and every tool description here says so, steering the model away from
//! parking a build in a process slot.
//!
//! Contracts:
//! - `start_process` spawns an **argv vector directly** (no shell), cwd
//!   pinned to the workspace root, in its own process group, with
//!   `kill_on_drop`. It returns a handle id (`proc-N`).
//! - Output (stdout + stderr, interleaved by arrival) accumulates in a
//!   capped ring buffer per process; when the cap overflows the oldest
//!   bytes are dropped and the drop is FLAGGED on the next read.
//! - `read_output` returns everything buffered since the last read and
//!   reports `running` / `exited (code N)`; `clear: true` discards the
//!   buffered output instead of returning it.
//! - `stop_process` closes stdin, sends SIGTERM to the process group,
//!   waits [`STOP_GRACE_MS`], then SIGKILLs the group — and any process
//!   still alive when the registry (and with it this table) drops is
//!   killed the same way, so nothing outlives the session.
//!
//! All errors are typed and named: unknown handle, exited process, closed
//! stdin, empty argv.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};

use crate::registry::Tool;

/// Byte cap on each process's buffered (unread) output.
const MAX_BUFFER_BYTES: usize = 200_000;
/// How long `stop_process` waits after SIGTERM before SIGKILL.
const STOP_GRACE_MS: u64 = 2_000;

/// The shared process table — held by the registry's four process tool
/// instances, so it lives exactly as long as the registry and its `Drop`
/// reaps whatever is still running when the session ends.
pub type ProcessTableHandle = Arc<Mutex<ProcessTable>>;

/// Capped interleaved stdout+stderr buffer. Overflow drops the OLDEST
/// bytes (a server's latest lines are the useful ones) and counts them so
/// the next read can flag the gap.
#[derive(Default)]
struct OutputBuffer {
    data: Vec<u8>,
    dropped: u64,
}

impl OutputBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.data.extend_from_slice(chunk);
        if self.data.len() > MAX_BUFFER_BYTES {
            let excess = self.data.len() - MAX_BUFFER_BYTES;
            self.data.drain(..excess);
            self.dropped += excess as u64;
        }
    }

    /// Take everything buffered since the last take, with the dropped-byte
    /// count for the same window.
    fn take(&mut self) -> (Vec<u8>, u64) {
        (
            std::mem::take(&mut self.data),
            std::mem::take(&mut self.dropped),
        )
    }
}

/// One live (or recently exited) process.
struct ProcessEntry {
    /// Optional human label from `start_process`.
    name: Option<String>,
    /// The spawned argv, joined for display.
    display: String,
    child: Child,
    /// Taken out of `child` at spawn so `send_stdin` can write without
    /// holding the table lock across an await; `None` once closed.
    stdin: Option<ChildStdin>,
    output: Arc<Mutex<OutputBuffer>>,
    /// Group-kill target (unix); 0 when the pid was unavailable.
    pid: i32,
    /// Cached exit code once observed (signal exits report -1).
    exit_code: Option<i32>,
}

impl ProcessEntry {
    /// Poll (and cache) the exit status without blocking.
    fn poll_exit(&mut self) -> Option<i32> {
        if self.exit_code.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.exit_code = Some(status.code().unwrap_or(-1));
        }
        self.exit_code
    }
}

/// The session's process table. Handles are `proc-N`, N monotonic.
#[derive(Default)]
pub struct ProcessTable {
    next_id: u64,
    entries: HashMap<String, ProcessEntry>,
}

impl ProcessTable {
    fn known_handles(&self) -> String {
        if self.entries.is_empty() {
            return "none".to_string();
        }
        let mut names: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        names.sort_unstable();
        names.join(", ")
    }
}

impl Drop for ProcessTable {
    /// Reap every process still running when the table (and the registry
    /// holding it) drops: group-SIGKILL on unix so grandchildren die too;
    /// `kill_on_drop` on each `Child` backs this up for the direct child.
    fn drop(&mut self) {
        for entry in self.entries.values_mut() {
            if entry.poll_exit().is_none() {
                #[cfg(unix)]
                if entry.pid > 0 {
                    // SAFETY: plain libc::kill on a recorded child pgid;
                    // guarded > 0 so we never signal our own group.
                    unsafe {
                        libc::kill(-entry.pid, libc::SIGKILL);
                    }
                }
                let _ = entry.child.start_kill();
            }
        }
    }
}

fn unknown_handle_error(table: &ProcessTable, handle: &str) -> ToolOutput {
    ToolOutput::Error {
        message: format!(
            "unknown process handle `{handle}` — known handles: {}",
            table.known_handles()
        ),
    }
}

/// Pump one child stream into the shared buffer until EOF. The lock is
/// held only for the synchronous push, never across an await.
fn spawn_pump<R>(mut reader: R, output: Arc<Mutex<OutputBuffer>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => output
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(&buf[..n]),
            }
        }
    });
}

/// `start_process` — see the module doc.
pub struct StartProcess(pub ProcessTableHandle);

#[async_trait]
impl Tool for StartProcess {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "start_process".into(),
            description: "Start a LONG-RUNNING process (dev server, REPL, watcher) from an \
                          argv vector — no shell, cwd = workspace root. Returns a handle for \
                          read_output / send_stdin / stop_process. For one-shot commands \
                          prefer run_tests, build_project, or run_script instead."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "argv": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Program and arguments, spawned directly (argv[0] is the program)" },
                    "name": { "type": "string", "description": "Optional human label for the handle" }
                },
                "required": ["argv"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let argv: Vec<String> = input
            .get("argv")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if argv.is_empty() {
            return ToolOutput::Error {
                message: "`argv` must be a non-empty array of strings (argv[0] is the program)"
                    .into(),
            };
        }
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.current_dir(root);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        // Own process group so stop/reap can take down grandchildren.
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ToolOutput::Error {
                    message: format!("failed to spawn `{}`: {e}", argv.join(" ")),
                };
            }
        };
        let pid = child.id().unwrap_or(0) as i32;
        let stdin = child.stdin.take();
        let output: Arc<Mutex<OutputBuffer>> = Arc::default();
        if let Some(stdout) = child.stdout.take() {
            spawn_pump(stdout, output.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_pump(stderr, output.clone());
        }

        let display = argv.join(" ");
        let mut table = self.0.lock().unwrap_or_else(|p| p.into_inner());
        table.next_id += 1;
        let handle = format!("proc-{}", table.next_id);
        table.entries.insert(
            handle.clone(),
            ProcessEntry {
                name: name.clone(),
                display: display.clone(),
                child,
                stdin,
                output,
                pid,
                exit_code: None,
            },
        );
        ToolOutput::Ok {
            content: format!(
                "started `{display}`{} as {handle} (pid {pid}) — poll it with read_output, \
                 stop it with stop_process",
                name.map(|n| format!(" ({n})")).unwrap_or_default()
            ),
        }
    }
}

/// `read_output` — see the module doc.
pub struct ReadOutput(pub ProcessTableHandle);

#[async_trait]
impl Tool for ReadOutput {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_output".into(),
            description: "Read a started process's buffered stdout+stderr since the last \
                          read, plus its running/exited state. clear: discard the buffered \
                          output instead of returning it."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string", "description": "Handle returned by start_process" },
                    "clear": { "type": "boolean", "description": "Discard buffered output instead of returning it" }
                },
                "required": ["handle"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let Some(handle) = input.get("handle").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "missing required field `handle`".into(),
            };
        };
        let clear = input
            .get("clear")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut table = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = table.entries.get_mut(handle) else {
            return unknown_handle_error(&table, handle);
        };
        let status = match entry.poll_exit() {
            Some(code) => format!("exited (code {code})"),
            None => "running".to_string(),
        };
        let (bytes, dropped) = entry
            .output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let label = entry
            .name
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        let mut content = format!("{handle} `{}`{label}: {status}", entry.display);
        if dropped > 0 {
            content.push_str(&format!(
                "\n[output truncated: {dropped} oldest bytes dropped before this read]"
            ));
        }
        if clear {
            content.push_str(&format!("\n[cleared {} buffered bytes]", bytes.len()));
        } else if bytes.is_empty() {
            content.push_str("\n[no new output]");
        } else {
            content.push('\n');
            content.push_str(&crate::exec::truncate_middle(
                String::from_utf8_lossy(&bytes).into_owned(),
            ));
        }
        ToolOutput::Ok { content }
    }
}

/// `send_stdin` — see the module doc.
pub struct SendStdin(pub ProcessTableHandle);

#[async_trait]
impl Tool for SendStdin {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "send_stdin".into(),
            description: "Write text to a started process's stdin (e.g. a REPL command — \
                          include the trailing newline yourself). Errors if the process has \
                          exited or its stdin was closed."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string", "description": "Handle returned by start_process" },
                    "text": { "type": "string", "description": "Bytes to write, verbatim (no newline is added)" }
                },
                "required": ["handle", "text"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let Some(handle) = input.get("handle").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "missing required field `handle`".into(),
            };
        };
        let Some(text) = input.get("text").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "missing required field `text`".into(),
            };
        };
        // Take stdin out under the lock, write outside it (a lock guard
        // must not cross an await), then put it back.
        let mut stdin = {
            let mut table = self.0.lock().unwrap_or_else(|p| p.into_inner());
            let Some(entry) = table.entries.get_mut(handle) else {
                return unknown_handle_error(&table, handle);
            };
            if let Some(code) = entry.poll_exit() {
                return ToolOutput::Error {
                    message: format!("{handle} has already exited (code {code})"),
                };
            }
            match entry.stdin.take() {
                Some(stdin) => stdin,
                None => {
                    return ToolOutput::Error {
                        message: format!("{handle}'s stdin is closed"),
                    };
                }
            }
        };
        let write = async {
            stdin.write_all(text.as_bytes()).await?;
            stdin.flush().await
        }
        .await;
        let result = match write {
            Ok(()) => ToolOutput::Ok {
                content: format!("wrote {} bytes to {handle}", text.len()),
            },
            Err(e) => ToolOutput::Error {
                message: format!("write to {handle} stdin failed: {e}"),
            },
        };
        let mut table = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = table.entries.get_mut(handle) {
            entry.stdin = Some(stdin);
        }
        result
    }
}

/// `stop_process` — see the module doc.
pub struct StopProcess(pub ProcessTableHandle);

#[async_trait]
impl Tool for StopProcess {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "stop_process".into(),
            description: "Stop a started process: closes stdin, sends SIGTERM to its process \
                          group, waits briefly, then SIGKILLs. Remaining output stays \
                          readable via read_output."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string", "description": "Handle returned by start_process" }
                },
                "required": ["handle"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let Some(handle) = input.get("handle").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "missing required field `handle`".into(),
            };
        };
        // Phase 1, under the lock: close stdin (EOF lets well-behaved
        // REPLs/servers exit on their own) and send the group SIGTERM.
        let pid = {
            let mut table = self.0.lock().unwrap_or_else(|p| p.into_inner());
            let Some(entry) = table.entries.get_mut(handle) else {
                return unknown_handle_error(&table, handle);
            };
            entry.stdin = None;
            if let Some(code) = entry.poll_exit() {
                return ToolOutput::Ok {
                    content: format!("{handle} had already exited (code {code})"),
                };
            }
            #[cfg(unix)]
            if entry.pid > 0 {
                // SAFETY: signalling the child's own process group; the
                // > 0 guard means we never signal our own group.
                unsafe {
                    libc::kill(-entry.pid, libc::SIGTERM);
                }
            }
            entry.pid
        };
        // Phase 2: poll for exit without holding the lock across sleeps.
        let mut waited = 0u64;
        let exit_after_term = loop {
            {
                let mut table = self.0.lock().unwrap_or_else(|p| p.into_inner());
                let Some(entry) = table.entries.get_mut(handle) else {
                    return ToolOutput::Error {
                        message: format!("{handle} disappeared while stopping"),
                    };
                };
                if let Some(code) = entry.poll_exit() {
                    break Some(code);
                }
            }
            if waited >= STOP_GRACE_MS {
                break None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            waited += 100;
        };
        if let Some(code) = exit_after_term {
            return ToolOutput::Ok {
                content: format!("{handle} terminated (code {code})"),
            };
        }
        // Phase 3: it ignored SIGTERM — SIGKILL the group.
        #[cfg(unix)]
        if pid > 0 {
            // SAFETY: same guarded group signal as above.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = pid;
        let mut table = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = table.entries.get_mut(handle) {
            let _ = entry.child.start_kill();
            let code = entry.poll_exit();
            return ToolOutput::Ok {
                content: match code {
                    Some(code) => format!("{handle} killed after SIGTERM grace (code {code})"),
                    None => format!("{handle} killed after SIGTERM grace"),
                },
            };
        }
        ToolOutput::Error {
            message: format!("{handle} disappeared while stopping"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> (ProcessTableHandle, std::path::PathBuf) {
        (Arc::default(), std::env::temp_dir())
    }

    async fn start(table: &ProcessTableHandle, root: &std::path::Path, argv: &[&str]) -> String {
        let out = StartProcess(table.clone())
            .execute(&serde_json::json!({ "argv": argv }), root)
            .await;
        match out {
            ToolOutput::Ok { content } => content
                .split_whitespace()
                .find(|w| w.starts_with("proc-"))
                .expect("handle in start output")
                .to_string(),
            ToolOutput::Error { message } => panic!("start failed: {message}"),
        }
    }

    #[test]
    fn output_buffer_caps_and_counts_dropped_bytes() {
        let mut buf = OutputBuffer::default();
        buf.push(&vec![b'a'; MAX_BUFFER_BYTES]);
        buf.push(&[b'z'; 10]);
        let (bytes, dropped) = buf.take();
        assert_eq!(bytes.len(), MAX_BUFFER_BYTES, "capped at the max");
        assert_eq!(dropped, 10, "the 10 oldest bytes were dropped");
        assert!(bytes.ends_with(b"zzzzzzzzzz"), "newest bytes survive");
        // Take drains: a second take is empty with no drop count.
        assert_eq!(buf.take(), (Vec::new(), 0));
    }

    #[tokio::test]
    async fn empty_argv_is_a_named_error() {
        let (table, root) = tools();
        let out = StartProcess(table)
            .execute(&serde_json::json!({"argv": []}), &root)
            .await;
        match out {
            ToolOutput::Error { message } => assert!(message.contains("argv"), "{message}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_handles_are_named_errors_on_every_tool() {
        let (table, root) = tools();
        for out in [
            ReadOutput(table.clone())
                .execute(&serde_json::json!({"handle": "proc-9"}), &root)
                .await,
            SendStdin(table.clone())
                .execute(&serde_json::json!({"handle": "proc-9", "text": "x"}), &root)
                .await,
            StopProcess(table.clone())
                .execute(&serde_json::json!({"handle": "proc-9"}), &root)
                .await,
        ] {
            match out {
                ToolOutput::Error { message } => {
                    assert!(
                        message.contains("unknown process handle `proc-9`"),
                        "{message}"
                    )
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cat_echoes_stdin_and_stop_reports_the_lifecycle() {
        let (table, root) = tools();
        let handle = start(&table, &root, &["cat"]).await;

        let write = SendStdin(table.clone())
            .execute(
                &serde_json::json!({"handle": handle, "text": "hello_process\n"}),
                &root,
            )
            .await;
        assert!(!write.is_error(), "{write:?}");

        // Poll until the pump has delivered the echo (bounded).
        let mut echoed = String::new();
        for _ in 0..50 {
            let out = ReadOutput(table.clone())
                .execute(&serde_json::json!({"handle": handle}), &root)
                .await;
            let ToolOutput::Ok { content } = out else {
                panic!("read_output failed");
            };
            echoed.push_str(&content);
            if echoed.contains("hello_process") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(echoed.contains("hello_process"), "{echoed}");
        assert!(echoed.contains("running"), "{echoed}");

        // Stop: cat exits on stdin EOF/SIGTERM; afterwards the state is
        // exited and stdin writes are refused.
        let stopped = StopProcess(table.clone())
            .execute(&serde_json::json!({"handle": handle}), &root)
            .await;
        assert!(!stopped.is_error(), "{stopped:?}");
        let after = SendStdin(table.clone())
            .execute(&serde_json::json!({"handle": handle, "text": "x"}), &root)
            .await;
        match after {
            ToolOutput::Error { message } => {
                assert!(
                    message.contains("exited") || message.contains("stdin is closed"),
                    "{message}"
                )
            }
            other => panic!("stdin to a stopped process must fail: {other:?}"),
        }
        let status = ReadOutput(table)
            .execute(&serde_json::json!({"handle": handle}), &root)
            .await;
        let ToolOutput::Ok { content } = status else {
            panic!("read after stop must still work");
        };
        assert!(content.contains("exited"), "{content}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_naturally_exited_process_reports_its_code() {
        let (table, root) = tools();
        let handle = start(&table, &root, &["true"]).await;
        // Wait for the exit to be observable.
        for _ in 0..50 {
            let out = ReadOutput(table.clone())
                .execute(&serde_json::json!({"handle": handle}), &root)
                .await;
            let ToolOutput::Ok { content } = out else {
                panic!("read_output failed");
            };
            if content.contains("exited (code 0)") {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("`true` never reported exit");
    }
}
