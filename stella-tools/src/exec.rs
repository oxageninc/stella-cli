//! Shared subprocess runner for tools that spawn commands: process-group
//! spawn, hard timeout with a group kill, combined output, middle-out
//! truncation. Two entry points: [`run`] shells out via `bash -c`
//! (`verify_done`, `build_project`, `run_tests`, the opt-in `bash` tool);
//! [`run_argv`] execs an argv vector directly with NO shell anywhere
//! (`run_script`, `run_lint`, `format_code`).

use std::time::Duration;

use tokio::process::Command;

pub(crate) const MAX_OUTPUT_BYTES: usize = 30_000;

/// Environment variables that re-target git at a specific repository. Tool
/// subprocesses always run against their explicit working dir; when Stella
/// itself was spawned from inside a git hook (which exports `GIT_DIR` et
/// al.), letting them leak through would silently aim every git invocation
/// at the OUTER repo instead — `git init` in a scratch dir re-initing the
/// host repo, `verify_done` diffing against the wrong HEAD. Scrub these from
/// every subprocess that shells out with an explicit dir.
pub const GIT_REPO_ENV_VARS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// Run `command` via `bash -c` in `dir`. Returns `(exit_code, combined
/// stdout+stderr)`; `Err` is spawn failure or timeout (the process group is
/// killed on timeout so nothing lingers).
pub(crate) async fn run(
    command: &str,
    dir: &std::path::Path,
    timeout_secs: u64,
) -> Result<(i32, String), String> {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(command);
    drive(cmd, command, dir, timeout_secs).await
}

/// Run `program` with `args` DIRECTLY — argv exec, no shell anywhere — in
/// `dir`. The runner for the manifest-verb tools (`run_script`, `run_lint`,
/// `format_code`): arguments reach the child exactly as given, so no
/// model-supplied string is ever shell-interpreted. Same process-group
/// spawn, timeout kill, and truncation as [`run`].
pub(crate) async fn run_argv(
    program: &str,
    args: &[String],
    dir: &std::path::Path,
    timeout_secs: u64,
) -> Result<(i32, String), String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    let display = std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    drive(cmd, &display, dir, timeout_secs).await
}

/// Shared spawn/wait/kill/truncate body of [`run`] and [`run_argv`] —
/// `command` is the human-readable command line for error messages.
async fn drive(
    mut cmd: Command,
    command: &str,
    dir: &std::path::Path,
    timeout_secs: u64,
) -> Result<(i32, String), String> {
    cmd.current_dir(dir);
    for var in GIT_REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn `{command}`: {e}"))?;
    #[cfg(unix)]
    let pid = child.id().unwrap_or(0) as i32;

    let output =
        match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
            .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(format!("command failed: {e}")),
            Err(_) => {
                #[cfg(unix)]
                unsafe {
                    // Guard on a real pid: kill(-0, …) would SIGKILL Stella's
                    // OWN process group.
                    if pid > 0 {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                return Err(format!("`{command}` timed out after {timeout_secs}s"));
            }
        };

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    Ok((
        output.status.code().unwrap_or(-1),
        truncate_middle(combined),
    ))
}

/// [`run`] with the PASSED/FAILED framing shared by `build_project`,
/// `run_tests`, and `run_script` — the model reads success or failure from
/// the first line without a follow-up question.
pub(crate) async fn run_and_report(
    command: &str,
    dir: &std::path::Path,
    timeout_secs: u64,
) -> stella_protocol::tool::ToolOutput {
    use stella_protocol::tool::ToolOutput;
    match run(command, dir, timeout_secs).await {
        Ok((0, output)) => ToolOutput::Ok {
            content: format!("`{command}` PASSED (exit 0)\n{output}"),
        },
        Ok((code, output)) => ToolOutput::Error {
            message: format!("`{command}` FAILED (exit {code})\n{output}"),
        },
        Err(e) => ToolOutput::Error { message: e },
    }
}

/// Keep head and tail when output exceeds the cap — build/test failures
/// matter at both ends (first error, final summary).
pub(crate) fn truncate_middle(s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let half = MAX_OUTPUT_BYTES / 2;
    let mut head_end = half;
    while !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - half;
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n[… {} bytes truncated …]\n{}",
        &s[..head_end],
        s.len() - head_end - (s.len() - tail_start),
        &s[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_captures_exit_code_and_output() {
        let (code, out) = run("echo hi; exit 3", std::path::Path::new("/tmp"), 30)
            .await
            .unwrap();
        assert_eq!(code, 3);
        assert!(out.contains("hi"));
    }

    #[tokio::test]
    async fn run_argv_execs_without_a_shell() {
        // `echo` is a real binary; the argument is NOT shell-interpreted, so
        // a metacharacter-laden string arrives verbatim.
        let (code, out) = run_argv(
            "echo",
            &["$(id); `id`; && ||".to_string()],
            std::path::Path::new("/tmp"),
            30,
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        assert!(out.contains("$(id); `id`; && ||"), "{out}");
    }

    #[tokio::test]
    async fn run_times_out_and_kills() {
        let err = run("sleep 30", std::path::Path::new("/tmp"), 1)
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let s = "a".repeat(20_000) + &"z".repeat(20_000);
        let t = truncate_middle(s);
        assert!(t.len() < 40_000);
        assert!(t.starts_with('a'));
        assert!(t.ends_with('z'));
        assert!(t.contains("truncated"));
    }
}
