//! `bash` — run a shell command in the workspace root with a timeout.
//! Process-group based kill so children don't outlive the timeout.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};
use tokio::process::Command;

use crate::registry::Tool;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = 100_000;

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bash".into(),
            description: "Run a shell command in the workspace root. Returns stdout+stderr with a timeout backstop.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120)" },
                    "trace": { "type": "boolean", "description": "Echo each executed line (set -x) as an execution trace" }
                },
                "required": ["command"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let command = match input.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolOutput::Error {
                    message: "missing required field `command`".into(),
                };
            }
        };
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        // trace: true prefixes `set -x` so every executed line echoes to
        // stderr — an execution trace a judge can demand as evidence.
        let traced;
        let command = if input
            .get("trace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            traced = format!("set -x\n{command}");
            traced.as_str()
        } else {
            command
        };

        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd.current_dir(root);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // New process group so we can kill the whole tree on timeout.
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ToolOutput::Error {
                    message: format!("failed to spawn: {e}"),
                };
            }
        };

        // Capture pid before wait_with_output takes ownership.
        #[cfg(unix)]
        let pid = child.id().unwrap_or(0) as i32;

        let timeout = Duration::from_secs(timeout_secs);
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return ToolOutput::Error {
                    message: format!("command failed: {e}"),
                };
            }
            Err(_) => {
                // Timeout — kill the process group.
                #[cfg(unix)]
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                return ToolOutput::Error {
                    message: format!("command timed out after {timeout_secs}s"),
                };
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let status = output.status.code().unwrap_or(-1);

        let mut combined = String::new();
        if !stdout.is_empty() {
            combined.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str("[stderr]\n");
            combined.push_str(&stderr);
        }
        combined.push_str(&format!("\n[exit code: {status}]"));

        // Truncate from the middle if too long — keep head and tail.
        if combined.len() > MAX_OUTPUT_BYTES {
            let head = MAX_OUTPUT_BYTES / 2;
            let tail = MAX_OUTPUT_BYTES / 2;
            let head_str = &combined[..head];
            let tail_str = &combined[combined.len() - tail..];
            combined = format!(
                "{head_str}\n... [truncated {truncated} bytes] ...\n{tail_str}",
                truncated = combined.len() - MAX_OUTPUT_BYTES
            );
        }

        if output.status.success() {
            ToolOutput::Ok { content: combined }
        } else {
            ToolOutput::Error { message: combined }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_echo_command() {
        let dir = std::env::temp_dir();
        let result = Bash
            .execute(&serde_json::json!({"command": "echo hello_stella"}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => assert!(content.contains("hello_stella")),
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
    }

    #[tokio::test]
    async fn captures_stderr() {
        let dir = std::env::temp_dir();
        let result = Bash
            .execute(&serde_json::json!({"command": "echo err >&2"}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => assert!(content.contains("err")),
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
    }

    #[tokio::test]
    async fn nonzero_exit_is_error() {
        let dir = std::env::temp_dir();
        let result = Bash
            .execute(&serde_json::json!({"command": "exit 42"}), &dir)
            .await;
        assert!(result.is_error());
        if let ToolOutput::Error { message } = result {
            assert!(message.contains("42"))
        }
    }

    #[tokio::test]
    async fn timeout_kills_command() {
        let dir = std::env::temp_dir();
        let result = Bash
            .execute(
                &serde_json::json!({"command": "sleep 30", "timeout_secs": 1}),
                &dir,
            )
            .await;
        assert!(result.is_error());
        if let ToolOutput::Error { message } = result {
            assert!(message.contains("timed out"))
        }
    }

    #[tokio::test]
    async fn runs_in_workspace_root() {
        let dir = std::env::temp_dir();
        let result = Bash
            .execute(&serde_json::json!({"command": "pwd"}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => {
                // macOS temp_dir is a symlink; compare canonicalized paths.
                let pwd = content.lines().next().unwrap_or("").trim();
                let canonical_pwd = std::fs::canonicalize(pwd).unwrap_or_default();
                let canonical_dir = std::fs::canonicalize(&dir).unwrap_or_default();
                assert_eq!(
                    canonical_pwd,
                    canonical_dir,
                    "pwd `{pwd}` should resolve to workspace root `{}`",
                    canonical_dir.display()
                );
            }
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
    }
}
