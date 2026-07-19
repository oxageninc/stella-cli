//! `verify_done` — the deterministic definition of done.
//!
//! A change is done when a *witness test* proves it: the test FAILS against
//! the previous version of the code and PASSES against the new version.
//! Either half alone is worthless — a test that passes on the new code but
//! also passed on the old code witnesses nothing (it would have been green
//! without your change: vacuous, or the feature already existed); a test
//! that fails on the new code means the work isn't done. Only the pair
//! (old→fail, new→pass) is a completed unit of work. This is the
//! shadow-revert mutation-witness gate, as a tool.
//!
//! # How the "previous version" is produced — without touching your tree
//!
//! The working tree is NEVER mutated (no stash, no checkout —). Instead a
//! detached shadow git worktree is created at `HEAD`, the *test* files
//! (only) are copied from the working tree into it, and the test command
//! runs there. `HEAD` is the previous version; the copied test files are
//! the witness. The shadow worktree is removed afterward, success or
//! failure.
//!
//! # Reading the verdict
//!
//! The shadow run's output tail is always included: a shadow failure that
//! is a *compile error* (the test references symbols that don't exist on
//! HEAD) is a much weaker witness than an assertion failure — the agent
//! (and any judge) should check the tail for WHY the old code failed.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};
use tokio::process::Command;

use crate::registry::Tool;

const DEFAULT_TIMEOUT_SECS: u64 = 300;
const TAIL_BYTES: usize = 4_000;

pub struct VerifyDone;

/// Run `command` via `bash -c` in `dir` with a process-group kill on
/// timeout — the shared runner in [`crate::exec`]. Its 30k middle-out cap is
/// invisible here: `tail()` keeps only the last [`TAIL_BYTES`], which the
/// cap's preserved tail half always contains.
async fn run(
    command: &str,
    dir: &std::path::Path,
    timeout_secs: u64,
) -> Result<(i32, String), String> {
    crate::exec::run(command, dir, timeout_secs).await
}

fn tail(s: &str) -> &str {
    if s.len() <= TAIL_BYTES {
        return s;
    }
    let start = s.len() - TAIL_BYTES;
    // Snap forward to a char boundary.
    let mut idx = start;
    while !s.is_char_boundary(idx) {
        idx += 1;
    }
    &s[idx..]
}

/// A `git` command aimed at `dir` and ONLY `dir`: repo-targeting env vars a
/// surrounding git hook may have exported are scrubbed so they cannot
/// redirect the invocation at the outer repository
/// ([`crate::exec::GIT_REPO_ENV_VARS`]).
fn git_in(dir: &std::path::Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir);
    for var in crate::exec::GIT_REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// Best-effort removal of the shadow worktree — both the registration and
/// the directory.
async fn cleanup_shadow(root: &std::path::Path, shadow: &std::path::Path) {
    let _ = git_in(root)
        .args(["worktree", "remove", "--force"])
        .arg(shadow)
        .output()
        .await;
    let _ = tokio::fs::remove_dir_all(shadow).await;
    let _ = git_in(root).args(["worktree", "prune"]).output().await;
}

#[async_trait]
impl Tool for VerifyDone {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "verify_done".into(),
            description: "Prove a change is done: test_cmd must PASS on your code and FAIL on \
                          git HEAD with your test files layered in. Call before declaring any \
                          implementation complete. Never mutates the working tree."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "test_cmd": { "type": "string", "description": "Command that runs the witness test(s), e.g. `cargo test -p my-crate my_test` or `pnpm vitest run path/to/file.test.ts`" },
                    "test_files": { "type": "array", "items": { "type": "string" }, "description": "Workspace-relative paths of the NEW or CHANGED test files that witness this change" },
                    "timeout_secs": { "type": "integer", "description": "Per-run timeout in seconds (default 300, max 600)" }
                },
                "required": ["test_cmd", "test_files"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let Some(test_cmd) = input.get("test_cmd").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "missing required field `test_cmd`".into(),
            };
        };
        let test_files: Vec<String> = input
            .get("test_files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if test_files.is_empty() {
            return ToolOutput::Error {
                message: "missing required field `test_files` — name the new/changed test \
                          file(s) that witness this change"
                    .into(),
            };
        }
        let timeout_secs = crate::exec::timeout_from(input, DEFAULT_TIMEOUT_SECS);

        // The shadow-worktree copy destination must be derived from the
        // canonical path *relative to the root*, never the raw model-supplied
        // string: an absolute `file` would make `shadow.join(file)` discard the
        // shadow prefix and resolve back to the real working-tree file, and
        // `fs::copy(src, src)` truncates it — silently emptying the user's test
        // file and violating the "NEVER mutates the working tree" contract.
        let canon_root = match root.canonicalize() {
            Ok(r) => r,
            Err(e) => {
                return ToolOutput::Error {
                    message: format!("could not canonicalize the workspace root: {e}"),
                };
            }
        };
        // The shadow worktree mirrors the git TOPLEVEL, not the workspace root
        // — which may be a subdirectory of the repo. So test-file destinations
        // must be relative to the toplevel, and the shadow test must run in the
        // subdirectory that corresponds to the workspace root. Assuming
        // root == toplevel produced a false verdict (a false WITNESS CONFIRMED
        // or false VACUOUS) whenever verify_done ran from a repo subdirectory.
        let canon_toplevel = match run("git rev-parse --show-toplevel", root, 30).await {
            Ok((0, out)) => {
                let p = std::path::PathBuf::from(out.trim());
                p.canonicalize().unwrap_or(p)
            }
            // Not a git repo (or older git): the HEAD check below reports it.
            _ => canon_root.clone(),
        };
        let root_rel = canon_root
            .strip_prefix(&canon_toplevel)
            .unwrap_or(std::path::Path::new(""))
            .to_path_buf();
        // Every test file must resolve inside the workspace and exist. Each
        // entry is `(display_name, canonical_src, toplevel_relative_dst)` — the
        // destination is relative to the git TOPLEVEL so `shadow.join(dst)`
        // lands at the file's real position in the repo tree.
        let mut resolved: Vec<(String, std::path::PathBuf, std::path::PathBuf)> = Vec::new();
        for file in &test_files {
            match crate::resolve_within_root(root, file) {
                Some(path) if path.is_file() => {
                    let relpath = match path.strip_prefix(&canon_toplevel) {
                        Ok(r) => r.to_path_buf(),
                        Err(_) => {
                            return ToolOutput::Error {
                                message: format!(
                                    "test file `{file}` resolved outside the git repository"
                                ),
                            };
                        }
                    };
                    resolved.push((file.clone(), path, relpath));
                }
                Some(_) => {
                    return ToolOutput::Error {
                        message: format!("test file `{file}` does not exist in the workspace"),
                    };
                }
                None => {
                    return ToolOutput::Error {
                        message: format!("test file `{file}` escapes the workspace root"),
                    };
                }
            }
        }

        // The previous version is git HEAD — a repo is required.
        let head = match run("git rev-parse HEAD", root, 30).await {
            Ok((0, out)) => out.trim().to_string(),
            _ => {
                return ToolOutput::Error {
                    message: "verify_done requires a git repository: the previous version of \
                              the code is defined as git HEAD"
                        .into(),
                };
            }
        };

        // ---- Half 1: the new code must pass.
        let (new_exit, new_output) = match run(test_cmd, root, timeout_secs).await {
            Ok(pair) => pair,
            Err(e) => return ToolOutput::Error { message: e },
        };
        if new_exit != 0 {
            return ToolOutput::Error {
                message: format!(
                    "NOT DONE — the witness test fails on your NEW code (exit {new_exit}). Fix \
                     the implementation (or the test) and retry.\n--- output tail ---\n{}",
                    tail(&new_output)
                ),
            };
        }

        // ---- Half 2: the previous code (HEAD + your new tests) must fail.
        // Shadow names carry pid + a process-wide counter: two concurrent
        // verify_done calls (parallel tools, parallel tests) must never
        // collide on the same worktree path — a timestamp alone can.
        static SHADOW_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let shadow = std::env::temp_dir().join(format!(
            "stella_verify_{}_{}",
            std::process::id(),
            SHADOW_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let added = git_in(root)
            .args(["worktree", "add", "--detach"])
            .arg(&shadow)
            .arg(&head)
            .output()
            .await;
        match added {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                return ToolOutput::Error {
                    message: format!(
                        "could not create the shadow worktree for the previous version: {}",
                        String::from_utf8_lossy(&out.stderr)
                    ),
                };
            }
            Err(e) => {
                return ToolOutput::Error {
                    message: format!("could not run git worktree add: {e}"),
                };
            }
        }

        // Layer ONLY the test files onto the previous version.
        for (rel, src, relpath) in &resolved {
            let dst = shadow.join(relpath);
            if let Some(parent) = dst.parent()
                && let Err(e) = tokio::fs::create_dir_all(parent).await
            {
                cleanup_shadow(root, &shadow).await;
                return ToolOutput::Error {
                    message: format!("could not stage test file `{rel}` in the shadow: {e}"),
                };
            }
            if let Err(e) = tokio::fs::copy(src, &dst).await {
                cleanup_shadow(root, &shadow).await;
                return ToolOutput::Error {
                    message: format!("could not copy test file `{rel}` into the shadow: {e}"),
                };
            }
        }

        // Run in the shadow subdirectory matching the workspace root, so a
        // relative `test_cmd` (e.g. `cargo test`) resolves the same package it
        // would in the real working tree — not the repo toplevel.
        let shadow_cwd = shadow.join(&root_rel);
        let shadow_run = run(test_cmd, &shadow_cwd, timeout_secs).await;
        cleanup_shadow(root, &shadow).await;
        let (old_exit, old_output) = match shadow_run {
            Ok(pair) => pair,
            Err(e) => {
                return ToolOutput::Error {
                    message: format!("shadow run against the previous version failed to run: {e}"),
                };
            }
        };

        if old_exit == 0 {
            return ToolOutput::Error {
                message: format!(
                    "VACUOUS TEST — the witness test ALSO PASSES on the previous code (HEAD \
                     {}). It does not witness your change: either the behavior already \
                     existed, the test doesn't exercise the new behavior, or your change \
                     isn't wired in. Strengthen the test so it fails without your change.\n\
                     --- previous-code output tail ---\n{}",
                    &head[..head.len().min(8)],
                    tail(&old_output)
                ),
            };
        }

        ToolOutput::Ok {
            content: format!(
                "WITNESS CONFIRMED — deterministic definition of done met:\n\
                 - new code:      `{test_cmd}` exit 0 (PASS)\n\
                 - previous code: HEAD {} + your test files → exit {old_exit} (FAIL)\n\
                 Check the tail below: an assertion failure is a strong witness; a compile \
                 error (test references symbols that don't exist on HEAD) is weaker — \
                 prefer behavioral assertions when possible.\n\
                 --- previous-code failure tail ---\n{}",
                &head[..head.len().min(8)],
                tail(&old_output)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny git repo with a committed "previous version" and an
    /// uncommitted "new version" + witness test, both as shell scripts (no
    /// toolchain dependency: the "test" greps the implementation file).
    async fn scaffold(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "stella_verify_test_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        // Scrub hook-exported GIT_* vars exactly like the production paths
        // (`git_in`) — without this, running the suite from inside a git
        // hook (the pre-push gate) re-targets `git init` at the HOST repo.
        let scratch_git = |args: &[&str]| {
            let mut cmd = std::process::Command::new("git");
            cmd.args(args).current_dir(&root);
            for var in crate::exec::GIT_REPO_ENV_VARS {
                cmd.env_remove(var);
            }
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t.t"],
            &["config", "user.name", "t"],
        ] {
            scratch_git(args);
        }
        std::fs::write(root.join("impl.txt"), "old behavior\n").unwrap();
        for args in [
            &["add", "."][..],
            &["commit", "-q", "-m", "previous version"],
        ] {
            scratch_git(args);
        }
        root
    }

    #[tokio::test]
    async fn confirmed_witness_when_old_fails_and_new_passes() {
        let root = scaffold("confirmed").await;
        // New implementation (uncommitted) and a test that requires it.
        std::fs::write(root.join("impl.txt"), "new behavior\n").unwrap();
        std::fs::write(root.join("witness.sh"), "grep -q 'new behavior' impl.txt\n").unwrap();

        let out = VerifyDone
            .execute(
                &serde_json::json!({
                    "test_cmd": "bash witness.sh",
                    "test_files": ["witness.sh"],
                    "timeout_secs": 60
                }),
                &root,
            )
            .await;
        match &out {
            ToolOutput::Ok { content } => {
                assert!(content.contains("WITNESS CONFIRMED"), "{content}")
            }
            other => panic!("expected confirmation, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn vacuous_test_is_rejected() {
        let root = scaffold("vacuous").await;
        // A test that passes on old AND new code witnesses nothing.
        std::fs::write(root.join("witness.sh"), "grep -q 'behavior' impl.txt\n").unwrap();

        let out = VerifyDone
            .execute(
                &serde_json::json!({
                    "test_cmd": "bash witness.sh",
                    "test_files": ["witness.sh"],
                    "timeout_secs": 60
                }),
                &root,
            )
            .await;
        match &out {
            ToolOutput::Error { message } => {
                assert!(message.contains("VACUOUS TEST"), "{message}")
            }
            other => panic!("expected vacuous rejection, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn failing_new_code_is_not_done() {
        let root = scaffold("notdone").await;
        // Test demands behavior nobody implemented.
        std::fs::write(root.join("witness.sh"), "grep -q 'nonexistent' impl.txt\n").unwrap();

        let out = VerifyDone
            .execute(
                &serde_json::json!({
                    "test_cmd": "bash witness.sh",
                    "test_files": ["witness.sh"],
                    "timeout_secs": 60
                }),
                &root,
            )
            .await;
        match &out {
            ToolOutput::Error { message } => assert!(message.contains("NOT DONE"), "{message}"),
            other => panic!("expected NOT DONE, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn missing_inputs_and_missing_files_are_named_errors() {
        let root = scaffold("inputs").await;
        let no_files = VerifyDone
            .execute(
                &serde_json::json!({"test_cmd": "true", "test_files": []}),
                &root,
            )
            .await;
        assert!(no_files.is_error());

        let ghost = VerifyDone
            .execute(
                &serde_json::json!({"test_cmd": "true", "test_files": ["ghost.sh"]}),
                &root,
            )
            .await;
        match ghost {
            ToolOutput::Error { message } => {
                assert!(message.contains("does not exist"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn non_git_workspace_is_a_named_error() {
        let root = std::env::temp_dir().join(format!("stella_verify_nogit_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("witness.sh"), "true\n").unwrap();
        let out = VerifyDone
            .execute(
                &serde_json::json!({"test_cmd": "true", "test_files": ["witness.sh"]}),
                &root,
            )
            .await;
        match out {
            ToolOutput::Error { message } => {
                assert!(message.contains("requires a git repository"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
