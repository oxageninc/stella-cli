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

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};
use tokio::process::Command;

use crate::registry::Tool;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = 100_000;

/// grep-family commands whose first positional arg is a search pattern.
const GREP_CMDS: &[&str] = &["grep", "egrep", "fgrep", "rg", "ripgrep", "ag"];

/// A quote-aware word split — enough to pull a `cd` target or a `grep`
/// pattern out of the common command shapes, returning each word already
/// unquoted. NOT a shell parser: it respects `'…'` and `"…"` (so a pattern or
/// path with spaces stays one word) and preserves backslash escapes like
/// `\|` (so an alternation survives into [`is_symbol_shaped`]); bare
/// operators (`&&`, `|`, `;`) come back as their own words to bound a scan.
fn shell_words(command: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut has_word = false;
    let (mut in_single, mut in_double) = (false, false);
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_word = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_word = true;
            }
            '\\' if in_double => {
                // Keep the escape literal (covers `\|`, `\"`, …); we don't
                // interpret it, just preserve it for the symbol test.
                cur.push('\\');
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                has_word = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_word {
                    words.push(std::mem::take(&mut cur));
                    has_word = false;
                }
            }
            c => {
                cur.push(c);
                has_word = true;
            }
        }
    }
    if has_word {
        words.push(cur);
    }
    words
}

/// If the command `cd`s somewhere outside the workspace `root`, return that
/// target — the graph indexes only `root`, so a search or edit under a
/// drifted tree is silently ungraphed (the cross-tree gap the telemetry
/// showed). Targets we can't resolve (`$VAR`, `~`, `-`, a flag) are skipped:
/// better a missed note than a wrong one.
fn cd_escape_target(command: &str, root: &Path) -> Option<String> {
    let words = shell_words(command);
    for pair in words.windows(2) {
        if pair[0] != "cd" {
            continue;
        }
        let target = pair[1].as_str();
        // `-` (cd to previous dir) is caught by the `-` prefix below.
        if target.is_empty()
            || target.starts_with('$')
            || target.starts_with('~')
            || target.starts_with('-')
        {
            continue;
        }
        if crate::resolve_within_root(root, target).is_none() {
            return Some(target.to_string());
        }
    }
    None
}

/// Does the command run a grep-family search whose pattern is symbol-shaped —
/// the `grep -rn "struct X"` that graph_query answers better? The dominant
/// path in the telemetry (symbol searches ran through bash, not the native
/// grep tool), so the same nudge has to reach here. First positional after a
/// grep word is the pattern; flags are skipped, a pipeline boundary ends the
/// scan.
fn bash_grep_is_symbol_shaped(command: &str) -> bool {
    let words = shell_words(command);
    for (i, w) in words.iter().enumerate() {
        if !GREP_CMDS.contains(&w.as_str()) {
            continue;
        }
        for next in &words[i + 1..] {
            if matches!(next.as_str(), "|" | "||" | "&&" | ";") {
                break;
            }
            if next.starts_with('-') {
                continue; // a flag, not the pattern
            }
            if crate::code_map::is_symbol_shaped(next) {
                return true;
            }
            break; // first positional was the pattern; it wasn't symbol-shaped
        }
    }
    false
}

/// The advisory footer for a bash result, when the code graph exists (so its
/// advice is actionable): a cross-root `cd` warning takes precedence — under a
/// drifted tree graph_query can't help — otherwise a symbol-shaped grep gets
/// the same `graph_query` pointer the native search tools carry.
fn graph_advisory(command: &str, root: &Path) -> Option<String> {
    if !matches!(crate::graph::graph_available(root), Ok(true)) {
        return None;
    }
    if let Some(target) = cd_escape_target(command, root) {
        return Some(format!(
            "\n\nnote: this cd'd to `{target}`, outside the session root `{}` — \
             graph_query and the grep/glob code-map footers index only this root, so \
             work under `{target}` isn't covered by the code graph. To use the \
             structural tools there, re-root the session on that tree (run stella from \
             it).",
            root.display()
        ));
    }
    if bash_grep_is_symbol_shaped(command) {
        return Some(format!("\n\n{}", crate::code_map::GRAPH_QUERY_TIP));
    }
    None
}

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
        let timeout_secs = crate::exec::timeout_from(input, DEFAULT_TIMEOUT_SECS);
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
        crate::exec::scrub_sensitive_env(&mut cmd);
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

        // Append after truncation so the steer is never the part that gets
        // cut: a cross-root `cd` warning or a symbol-shaped-grep graph_query
        // nudge, when an index exists.
        if let Some(note) = graph_advisory(command, root) {
            combined.push_str(&note);
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

    /// An indexed workspace — a source file plus a built `.stella/private/codegraph.db`,
    /// exactly what `stella init` leaves so `graph_available` is true.
    fn indexed_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub struct Greeter;\npub fn greet() {}\n",
        )
        .expect("write");
        let db = crate::graph::graph_db_path(dir.path());
        std::fs::create_dir_all(db.parent().expect("parent")).expect("mkdir");
        let graph = stella_graph::CodeGraph::open(dir.path(), &db).expect("open");
        graph.index_all().expect("index");
        graph.shutdown();
        dir
    }

    fn text_of(out: ToolOutput) -> String {
        match out {
            ToolOutput::Ok { content } => content,
            ToolOutput::Error { message } => message,
        }
    }

    #[test]
    fn cd_escape_target_flags_out_of_root_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        // In-root cd — no drift.
        assert_eq!(cd_escape_target("cd sub && ls", dir.path()), None);
        assert_eq!(cd_escape_target("cd . && cargo build", dir.path()), None);
        // Out-of-root — the target is returned.
        assert_eq!(
            cd_escape_target("cd / && grep -rn x .", dir.path()).as_deref(),
            Some("/")
        );
        assert!(cd_escape_target("cd ../.. && ls", dir.path()).is_some());
        // Unresolvable / no cd — skipped.
        assert_eq!(cd_escape_target("cd $HOME && ls", dir.path()), None);
        assert_eq!(cd_escape_target("cd ~/foo && ls", dir.path()), None);
        assert_eq!(cd_escape_target("grep -rn x .", dir.path()), None);
    }

    #[test]
    fn bash_grep_symbol_detection() {
        assert!(bash_grep_is_symbol_shaped(
            r#"grep -rn "struct DeckProviderResolver" stella-tools/"#
        ));
        assert!(bash_grep_is_symbol_shaped("grep -n ReadOnlyTools src/"));
        assert!(bash_grep_is_symbol_shaped(r#"rg -e "pub fn resolve" ."#));
        assert!(bash_grep_is_symbol_shaped(
            r#"grep -rn "pub mod ports\|pub use ports" src/"#
        ));
        // free-text / non-symbol patterns — no nudge
        assert!(!bash_grep_is_symbol_shaped(r#"grep -rn "unwrap()" src/"#));
        assert!(!bash_grep_is_symbol_shaped(r#"grep -rn "TODO:" ."#));
        assert!(!bash_grep_is_symbol_shaped("ls -la && cargo build"));
        assert!(!bash_grep_is_symbol_shaped("cat foo.rs"));
    }

    #[tokio::test]
    async fn a_cross_root_cd_warns_when_indexed() {
        let dir = indexed_tempdir();
        let out = Bash
            .execute(&serde_json::json!({"command": "cd / && pwd"}), dir.path())
            .await;
        let text = text_of(out);
        assert!(
            text.contains("outside the session root"),
            "drift warned: {text}"
        );
    }

    /// The motivating shape from the telemetry: `cd` to a sibling checkout AND
    /// a symbol-shaped grep in one command. Drift takes precedence — under a
    /// tree the graph doesn't index, the grep tip would be misleading, so only
    /// the drift warning fires.
    #[tokio::test]
    async fn drift_wins_over_the_grep_tip_when_both_fire() {
        let dir = indexed_tempdir();
        // Grep /dev/null (instant, hermetic) — the advisory keys off the
        // command string's `cd` + grep pattern, not what grep actually reads,
        // so this exercises the precedence without walking `/`.
        let out = Bash
            .execute(
                &serde_json::json!({"command": "cd / && grep -rn \"struct Greeter\" /dev/null"}),
                dir.path(),
            )
            .await;
        let text = text_of(out);
        assert!(
            text.contains("outside the session root"),
            "drift warned: {text}"
        );
        // The drift note names graph_query (to explain coverage); the *tip* is
        // the thing suppressed. Its distinctive phrase must be absent.
        assert!(
            !text.contains("symbol/dependency lookup"),
            "the grep tip is suppressed under a drifted tree: {text}"
        );
    }

    #[tokio::test]
    async fn a_symbol_shaped_bash_grep_gets_the_graph_tip_when_indexed() {
        let dir = indexed_tempdir();
        let out = Bash
            .execute(
                &serde_json::json!({"command": "grep -rn \"struct Greeter\" ."}),
                dir.path(),
            )
            .await;
        let text = text_of(out);
        assert!(text.contains("graph_query"), "grep nudged: {text}");
    }

    #[tokio::test]
    async fn a_plain_command_gets_no_advisory() {
        let dir = indexed_tempdir();
        let text = text_of(
            Bash.execute(&serde_json::json!({"command": "echo hi"}), dir.path())
                .await,
        );
        assert!(!text.contains("graph_query"), "{text}");
        assert!(!text.contains("outside the session root"), "{text}");
    }

    #[tokio::test]
    async fn no_index_means_no_advisory() {
        let dir = tempfile::tempdir().unwrap();
        let text = text_of(
            Bash.execute(
                &serde_json::json!({"command": "grep -rn \"struct Foo\" . ; cd /"}),
                dir.path(),
            )
            .await,
        );
        assert!(!text.contains("graph_query"), "{text}");
        assert!(!text.contains("outside the session root"), "{text}");
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
