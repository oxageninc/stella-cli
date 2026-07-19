//! `bash` — run a shell command in the workspace root with a timeout.
//! Process-group based kill so children don't outlive the timeout.
//!
//! **Opt-in, never ambient.** This tool is registered only when the host
//! enabled it ([`crate::registry::RegistryOptions::bash`], set from the
//! settings key `tools.bash: "on"`); the default tool surface has no
//! shell at all. Prefer the structured executors — `run_tests`,
//! `build_project`, `run_lint`, `format_code`, `run_script`, the process
//! group, and the `repo_*` tools — which spawn enumerable argv and never
//! interpret a shell string.
//!
//! Opt-in OS sandbox: `STELLA_BASH_SANDBOX=workspace-write|restricted` wraps
//! the spawn in `sandbox-exec` (macOS, Seatbelt) or `bwrap` (Linux) — file
//! writes confined to the workspace root + tmp dirs, `restricted` also
//! denies network. Default (`off`/unset) is exactly the historical behavior.
//! A requested sandbox that cannot be applied fails the call instead of
//! silently running unsandboxed. See [`crate::sandbox`] for the full
//! safety/capability tradeoff discussion.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};
use tokio::process::Command;

use crate::registry::Tool;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Ceiling on the model-supplied `timeout_secs`. The timeout is the hang
/// backstop for commands the model itself launches — accepting an arbitrary
/// u64 lets one tool call disable the backstop entirely (u64::MAX ≈ never).
const MAX_TIMEOUT_SECS: u64 = 600;
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
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120, max 600; 0 = default)" },
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
        // 0 means "use the default", matching custom.rs's timeout convention
        // — a literal 0 would otherwise time out every invocation instantly.
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .filter(|&t| t > 0)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
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

        // Opt-in OS sandbox: the mode comes from the environment, argv
        // construction is pure and unit-tested (crate::sandbox). Both an
        // unknown mode and an unavailable backend fail the call — a
        // requested sandbox that silently doesn't apply is worse than an
        // error. `off` yields plain `bash -c <command>`, unchanged.
        let mode = match crate::sandbox::SandboxMode::from_env_value(
            std::env::var("STELLA_BASH_SANDBOX").ok().as_deref(),
        ) {
            Ok(mode) => mode,
            Err(e) => {
                return ToolOutput::Error {
                    message: e.to_string(),
                };
            }
        };
        let (program, args) = match crate::sandbox::host_argv(mode, root, command) {
            Ok(argv) => argv,
            Err(e) => {
                return ToolOutput::Error {
                    message: e.to_string(),
                };
            }
        };

        let mut cmd = Command::new(program);
        cmd.args(args);
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
                    // Guard on a real pid: kill(-0, …) would SIGKILL Stella's
                    // OWN process group.
                    if pid > 0 {
                        libc::kill(-pid, libc::SIGKILL);
                    }
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

        // Truncate from the middle if too long — keep head and tail. Slice
        // only on char boundaries so multibyte UTF-8 output can't panic.
        if combined.len() > MAX_OUTPUT_BYTES {
            let mut head = MAX_OUTPUT_BYTES / 2;
            while !combined.is_char_boundary(head) {
                head -= 1;
            }
            let mut tail_start = combined.len() - MAX_OUTPUT_BYTES / 2;
            while !combined.is_char_boundary(tail_start) {
                tail_start += 1;
            }
            let truncated = tail_start - head;
            let head_str = &combined[..head];
            let tail_str = &combined[tail_start..];
            combined = format!("{head_str}\n... [truncated {truncated} bytes] ...\n{tail_str}");
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
    async fn truncates_multibyte_output_without_panicking() {
        let dir = std::env::temp_dir();
        // Emit well over MAX_OUTPUT_BYTES of a 3-byte UTF-8 char, with no
        // newlines, so the middle cut lands inside a char. A raw byte slice
        // at that offset would panic; the boundary-safe path must not.
        let result = Bash
            .execute(
                &serde_json::json!({"command": "yes '€' | tr -d '\\n' | head -c 200000"}),
                &dir,
            )
            .await;
        match result {
            ToolOutput::Ok { content } => {
                assert!(content.contains("truncated"), "expected truncation marker");
            }
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
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

    /// End-to-end Seatbelt check — spawns real `sandbox-exec`. Ignored by
    /// default so machines without a sandbox (and CI) still pass; run with:
    /// `cargo test -p stella-tools -- --ignored --test-threads=1`
    /// (single-threaded because env-var mutation is process-wide).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "spawns sandbox-exec and mutates process env; run with --ignored --test-threads=1"]
    async fn macos_workspace_write_confines_writes_to_workspace() {
        let ws = tempfile::tempdir().unwrap();
        // SAFETY: process-wide env mutation; this test is #[ignore]d and
        // documented to run with --test-threads=1, so no concurrent reader.
        unsafe { std::env::set_var("STELLA_BASH_SANDBOX", "workspace-write") };
        let inside = Bash
            .execute(
                &serde_json::json!({"command": "echo confined > inside.txt && cat inside.txt"}),
                ws.path(),
            )
            .await;
        let home_probe = "stella_sandbox_e2e_probe.txt";
        let outside = Bash
            .execute(
                &serde_json::json!({"command": format!("echo escape > \"$HOME/{home_probe}\"")}),
                ws.path(),
            )
            .await;
        // SAFETY: same single-threaded contract as the set_var above.
        unsafe { std::env::remove_var("STELLA_BASH_SANDBOX") };

        match inside {
            ToolOutput::Ok { content } => assert!(content.contains("confined"), "{content}"),
            ToolOutput::Error { message } => panic!("workspace write should succeed: {message}"),
        }
        let probe = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(home_probe);
        let leaked = probe.exists();
        std::fs::remove_file(&probe).ok(); // clean up even on assertion failure
        assert!(outside.is_error(), "write outside workspace must fail");
        if let ToolOutput::Error { message } = outside {
            assert!(message.contains("Operation not permitted"), "{message}");
        }
        assert!(!leaked, "probe file must not exist outside the sandbox");
    }
}
