//! `ci_status` — read CI state for a branch, PR, or commit via the `gh`
//! CLI, including failure log tails. Read-only, so judges can verify "CI
//! is green" claims themselves; combined with the goal loop it powers
//! `stella monitor`: keep fixing until the checks pass.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::exec;
use crate::registry::Tool;

pub struct CiStatus;

#[async_trait]
impl Tool for CiStatus {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "ci_status".into(),
            description: "CI state via GitHub (gh). Give branch, pr, or commit; returns runs, \
                          conclusions, and failure log tails. wait=true blocks until the \
                          latest run finishes."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "branch": { "type": "string" },
                    "pr": { "type": "integer" },
                    "commit": { "type": "string" },
                    "wait": { "type": "boolean" },
                    "timeout_secs": { "type": "integer" }
                }
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(120);
        let wait = input.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);

        if exec::run_github("command -v gh", root, 10)
            .await
            .map(|(c, _)| c)
            != Ok(0)
        {
            return ToolOutput::Error {
                message: "the `gh` CLI is required for ci_status and was not found on PATH".into(),
            };
        }

        // PR ref gets `gh pr checks` (the richest view); branch/commit get
        // `gh run list` filtered accordingly.
        let list_cmd = if let Some(pr) = input.get("pr").and_then(|v| v.as_u64()) {
            format!("gh pr checks {pr} 2>&1 || true")
        } else if let Some(commit) = input.get("commit").and_then(|v| v.as_str()) {
            format!(
                "gh run list --commit {} --limit 15 \
                 --json databaseId,name,status,conclusion,headBranch",
                shell_quote(commit)
            )
        } else {
            let branch = input
                .get("branch")
                .and_then(|v| v.as_str())
                .unwrap_or("main");
            format!(
                "gh run list --branch {} --limit 15 \
                 --json databaseId,name,status,conclusion,headBranch",
                shell_quote(branch)
            )
        };

        let (code, mut report) = match exec::run_github(&list_cmd, root, timeout_secs).await {
            Ok(pair) => pair,
            Err(e) => return ToolOutput::Error { message: e },
        };
        if code != 0 {
            return ToolOutput::Error {
                message: format!("gh failed (exit {code}): {report}"),
            };
        }

        // The `gh run list` scope shared by the wait-watch and the
        // failure-log attach below, so both report on the SAME target the
        // top-level query resolved — an unscoped sub-query used to watch or
        // attach logs from an UNRELATED branch's most-recent run.
        let scope = gh_run_scope(input);

        // Optionally block on the newest in-progress run for THIS target,
        // then re-list.
        if wait {
            let _ = exec::run_github(
                &format!(
                    "id=$(gh run list {scope} --limit 1 --json databaseId --jq '.[0].databaseId'); \
                     [ -n \"$id\" ] && [ \"$id\" != \"null\" ] && \
                     gh run watch \"$id\" --exit-status >/dev/null 2>&1; true"
                ),
                root,
                timeout_secs,
            )
            .await;
            if let Ok((0, fresh)) = exec::run_github(&list_cmd, root, timeout_secs).await {
                report = fresh;
            }
        }

        // Attach failure logs for THIS target's most recent failed run, if any.
        let (_, failed_logs) = exec::run_github(
            &format!(
                "id=$(gh run list {scope} --limit 15 --json databaseId,conclusion \
                 --jq '[.[] | select(.conclusion==\"failure\")][0].databaseId'); \
                 if [ -n \"$id\" ] && [ \"$id\" != \"null\" ]; then \
                   echo \"--- failure logs (run $id) ---\"; gh run view \"$id\" --log-failed 2>&1 | tail -120; \
                 fi"
            ),
            root,
            timeout_secs,
        )
        .await
        .unwrap_or((0, String::new()));

        ToolOutput::Ok {
            content: if failed_logs.trim().is_empty() {
                report
            } else {
                format!("{report}\n{failed_logs}")
            },
        }
    }
}

/// Minimal single-quote shell escaping for ref names.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `gh run list` filter flag that scopes a sub-query to the same target
/// (`--branch` / `--commit`, or a PR's resolved head branch) the top-level
/// `ci_status` query used — so the wait-watch and failure-log sub-queries can
/// never report on an unrelated branch's runs.
fn gh_run_scope(input: &Value) -> String {
    if let Some(pr) = input.get("pr").and_then(|v| v.as_u64()) {
        // Resolve the PR's head branch at run time (gh has no run-list-by-PR).
        format!("--branch \"$(gh pr view {pr} --json headRefName --jq .headRefName 2>/dev/null)\"")
    } else if let Some(commit) = input.get("commit").and_then(|v| v.as_str()) {
        format!("--commit {}", shell_quote(commit))
    } else {
        let branch = input
            .get("branch")
            .and_then(|v| v.as_str())
            .unwrap_or("main");
        format!("--branch {}", shell_quote(branch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_read_only_for_judge_use() {
        assert!(CiStatus.schema().read_only);
    }

    #[test]
    fn sub_queries_are_scoped_to_the_requested_target() {
        // A branch request scopes by that branch…
        assert_eq!(
            gh_run_scope(&serde_json::json!({ "branch": "feat/x" })),
            "--branch 'feat/x'"
        );
        // …a commit request by that commit…
        assert_eq!(
            gh_run_scope(&serde_json::json!({ "commit": "abc123" })),
            "--commit 'abc123'"
        );
        // …a PR by its resolved head branch (never an unscoped list)…
        assert!(
            gh_run_scope(&serde_json::json!({ "pr": 42 })).contains("gh pr view 42"),
            "PR scope must resolve the head branch"
        );
        // …and an empty request defaults to main, still scoped (not global).
        assert_eq!(gh_run_scope(&serde_json::json!({})), "--branch 'main'");
    }

    #[test]
    fn shell_quote_neutralizes_quotes() {
        assert_eq!(shell_quote("main"), "'main'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
