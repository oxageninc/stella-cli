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
        let mut command = tokio::process::Command::new("bash");
        command
            .arg("-c")
            .arg(&action.command)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::subprocess_env::scrub_sensitive_env(&mut command);
        let mut child = command.spawn().map_err(|e| HookExecError::SpawnFailed {
            command: action.command.clone(),
            message: e.to_string(),
        })?;

        // Feed the payload on a DETACHED task and let the timeout-bounded
        // `wait_with_output` below drain stdout concurrently. Writing inline
        // before the wait was a hang: a hook that never reads stdin blocks the
        // write once the payload exceeds the OS pipe buffer (~64 KiB), so the
        // session hung forever, before the timeout window even opened. Writing
        // concurrently — and dropping stdin to signal EOF for hooks that DO
        // read (`cat`, `jq`) — means a non-reading hook is instead bounded by
        // the timeout; a timed-out kill drops the pipe and the write EPIPEs
        // and the task ends (no leak). `kill_on_drop` reaps the child.
        if let Some(mut stdin) = child.stdin.take() {
            let payload = payload_json.to_string(); // owned: the task outlives the borrow
            tokio::spawn(async move {
                let _ = stdin.write_all(payload.as_bytes()).await;
                // `stdin` drops here → EOF for a hook reading to end-of-input.
            });
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
    async fn hook_scrubs_inherited_credentials_but_keeps_benign_env() {
        let _fixture = crate::subprocess_env::test_support::InheritedCredentialFixture::install();
        let dir = tempfile::tempdir().expect("tempdir");
        let out = ShellHookRunner
            .run(
                &action(crate::subprocess_env::test_support::PROBE_COMMAND),
                "{}",
                &dir.path().display().to_string(),
            )
            .await
            .expect("hook runs");
        crate::subprocess_env::test_support::assert_scrubbed(&out.stdout);
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

    #[tokio::test]
    async fn a_non_reading_hook_with_a_large_payload_times_out_not_hangs() {
        // `sleep 30` never reads stdin. With a payload larger than the OS
        // pipe buffer (~64 KiB), the old inline `write_all` blocked forever
        // BEFORE the timeout window opened — the session hung. The write is
        // now detached, so the timeout still bounds the run.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut hung = action("sleep 30");
        hung.timeout_ms = Some(150);
        let big_payload = format!("{{\"blob\":\"{}\"}}", "x".repeat(256 * 1024));
        let started = std::time::Instant::now();
        let err = ShellHookRunner
            .run(&hung, &big_payload, &dir.path().display().to_string())
            .await
            .expect_err("must time out, not hang on the stdin write");
        assert!(matches!(err, HookExecError::TimedOut { .. }));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "timeout must still be enforced despite the unread large payload"
        );
    }
}
