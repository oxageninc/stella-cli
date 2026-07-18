//! The production [`HookRunner`] — the real-I/O half of the hooks framework
//! (`stella_core::hooks` owns the matching/blocking logic; this crate owns
//! process spawning, mirroring how `ToolRegistry` implements the engine's
//! `ToolExecutor` port).
//!
//! Each hook action runs as `bash -c <command>` in the workspace root, with
//! the event payload piped in as JSON on stdin, bounded by the action's
//! clamped timeout. `kill_on_drop` guarantees a timed-out hook's process is
//! reaped when its future is dropped — a hung hook can stall its own
//! timeout window, never the session.

use std::process::Stdio;

use async_trait::async_trait;
use stella_core::hooks::{HookAction, HookExecError, HookExecResult, HookRunner};
use tokio::io::AsyncWriteExt;

pub struct ShellHookRunner;

#[async_trait]
impl HookRunner for ShellHookRunner {
    async fn run(
        &self,
        action: &HookAction,
        payload_json: &str,
        cwd: &str,
    ) -> Result<HookExecResult, HookExecError> {
        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&action.command)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| HookExecError::SpawnFailed {
                command: action.command.clone(),
                message: e.to_string(),
            })?;

        // Feed the payload and close stdin so a hook reading to EOF (`cat`,
        // `jq`) terminates. A hook that never reads stdin is fine too — the
        // write may fail with EPIPE once the child exits, which is not an
        // error of ours.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload_json.as_bytes()).await;
            drop(stdin);
        }

        let timeout_ms = action.effective_timeout_ms();
        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            child.wait_with_output(),
        )
        .await;
        match waited {
            // Timeout elapsed: dropping the in-flight `wait_with_output`
            // future drops the child handle, and `kill_on_drop` reaps it.
            Err(_) => Err(HookExecError::TimedOut {
                command: action.command.clone(),
                timeout_ms,
            }),
            Ok(Err(e)) => Err(HookExecError::SpawnFailed {
                command: action.command.clone(),
                message: format!("could not collect hook output: {e}"),
            }),
            Ok(Ok(output)) => Ok(HookExecResult {
                exit_code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(command: &str) -> HookAction {
        HookAction::new(command)
    }

    #[tokio::test]
    async fn runs_the_command_in_the_given_cwd_and_captures_stdout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = ShellHookRunner
            .run(&action("pwd"), "{}", &dir.path().display().to_string())
            .await
            .expect("hook runs");
        assert_eq!(out.exit_code, 0);
        // Canonicalized comparison: macOS tempdirs live behind /private.
        let reported = std::path::Path::new(out.stdout.trim())
            .canonicalize()
            .expect("canonical stdout path");
        assert_eq!(reported, dir.path().canonicalize().expect("canonical dir"));
    }

    #[tokio::test]
    async fn pipes_the_payload_json_on_stdin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = ShellHookRunner
            .run(
                &action("cat"),
                r#"{"event":"PreToolUse"}"#,
                &dir.path().display().to_string(),
            )
            .await
            .expect("hook runs");
        assert!(out.stdout.contains("PreToolUse"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_a_result_not_an_error_with_stderr_captured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = ShellHookRunner
            .run(
                &action("echo blocked 1>&2; exit 3"),
                "{}",
                &dir.path().display().to_string(),
            )
            .await
            .expect("ran to completion");
        assert_eq!(out.exit_code, 3);
        assert!(out.stderr.contains("blocked"));
    }

    #[tokio::test]
    async fn a_hung_hook_times_out_with_the_named_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut hung = action("sleep 30");
        hung.timeout_ms = Some(150);
        let started = std::time::Instant::now();
        let err = ShellHookRunner
            .run(&hung, "{}", &dir.path().display().to_string())
            .await
            .expect_err("times out");
        assert!(matches!(err, HookExecError::TimedOut { .. }));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "timeout enforced promptly"
        );
    }

    #[tokio::test]
    async fn a_hook_that_ignores_stdin_still_completes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = ShellHookRunner
            .run(
                &action("echo ok"),
                "{\"big\":\"payload\"}",
                &dir.path().display().to_string(),
            )
            .await
            .expect("hook runs");
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("ok"));
    }
}
