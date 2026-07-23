//! Vendor-neutral repository tools: `repo_status`, `repo_diff`,
//! `repo_commit`, `repo_push`, `repo_pull`, `repo_rollback`.
//!
//! Nothing here says "git" — not the tool names, not the argument names.
//! The tools speak in repository concepts (branch, paths, message) through
//! the [`RepoBackend`] port; today's only adapter is [`GitCli`], which
//! shells to `git` via direct argv spawns (no shell interpretation, no new
//! crates), and a future VCS is a new adapter, never a tool rewrite.
//!
//! Structural rules live in the TOOL layer, above the backend, so no
//! adapter can forget them:
//!
//! - `repo_commit` and `repo_rollback` require a **non-empty, explicit
//!   path list** — there is no `-A`, and a whole-tree operation must be
//!   spelled out path by path, never implied by an empty-args call.
//! - `repo_push` **refuses the repository's default branch** (resolved
//!   from the remote HEAD). No override exists, and force-push does not
//!   exist in this surface at all.
//! - `repo_pull` is **fast-forward only** — divergence is a typed error,
//!   not a merge.
//! - History rewriting (`reset --hard`, rebase, amend) is deliberately
//!   absent: `repo_rollback` restores named paths to the last committed
//!   state and that is the only "undo" this surface offers.
//! - `repo_diff` caps its patch payload at [`MAX_PATCH_BYTES`] with a
//!   **loud elision marker**, so a capped review can never be mistaken
//!   for a complete one.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::exec;
use crate::registry::Tool;

/// Timeout for backend commands (push/pull are network-bound).
const REPO_TIMEOUT_SECS: u64 = 300;
/// Cap on `repo_status` changed-file rows (and `repo_diff` summary rows).
const MAX_CHANGED_ROWS: usize = 200;
/// Cap on `repo_diff` patch bytes; beyond it the patch is cut at a line
/// boundary and the elision is loud (see the module doc). Sits below the
/// shared runner's 30k output cap so this cut, not the runner's middle-out
/// truncation, is the one the agent normally sees.
const MAX_PATCH_BYTES: usize = 24_000;

/// Typed, named failures crossing the [`RepoBackend`] port.
#[derive(Debug)]
pub enum RepoError {
    /// The VCS binary is missing or cannot start at all.
    Unavailable(String),
    /// A backend command ran and failed; `detail` carries its output.
    Failed {
        action: &'static str,
        detail: String,
    },
    /// Local and remote histories diverged — `repo_pull` is fast-forward
    /// only by contract, and history rewriting is out of scope.
    Diverged(String),
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoError::Unavailable(detail) => {
                write!(f, "repository backend unavailable: {detail}")
            }
            RepoError::Failed { action, detail } => write!(f, "{action} failed: {detail}"),
            RepoError::Diverged(detail) => write!(
                f,
                "histories diverged — repo_pull is fast-forward-only and never merges or \
                 rewrites; reconcile manually. {detail}"
            ),
        }
    }
}

/// One changed file in a [`RepoStatus`] — `state` is the backend's short
/// two-column status code (e.g. ` M`, `??`, `A `).
#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub state: String,
    pub path: String,
}

/// The `repo_status` payload: typed rows, bounded.
#[derive(Debug, Clone, Serialize)]
pub struct RepoStatus {
    /// Current branch; `None` when the checkout is detached.
    pub branch: Option<String>,
    /// Commits ahead of upstream; `None` when no upstream is configured.
    pub ahead: Option<u32>,
    /// Commits behind upstream; `None` when no upstream is configured.
    pub behind: Option<u32>,
    /// Changed files, capped at [`MAX_CHANGED_ROWS`].
    pub changed: Vec<ChangedFile>,
    /// True when the changed-file list was truncated at the cap.
    pub truncated: bool,
}

/// One changed file in a [`RepoDiff`] summary; line counts are `None` for
/// a binary change.
#[derive(Debug, Clone)]
pub struct DiffFileStat {
    pub path: String,
    /// Lines added, `None` for a binary change.
    pub added: Option<u64>,
    /// Lines removed, `None` for a binary change.
    pub removed: Option<u64>,
}

/// The `repo_diff` payload: per-file line stats plus the raw patch hunks.
/// The size caps live in the TOOL layer (see the module doc), so the port
/// carries the backend's full answer.
#[derive(Debug, Clone)]
pub struct RepoDiff {
    pub files: Vec<DiffFileStat>,
    pub patch: String,
}

/// Vendor-neutral repository port. Adapters translate these operations to
/// their VCS; the structural refusals (default-branch push, empty path
/// lists) live in the tools, not here — see the module doc.
#[async_trait]
pub trait RepoBackend: Send + Sync {
    async fn status(&self, root: &Path) -> Result<RepoStatus, RepoError>;
    /// Pending changes as patch hunks plus per-file line stats. `staged`
    /// selects changes already staged for commit instead of unstaged ones;
    /// a non-empty `paths` scopes the diff to those paths.
    async fn diff(
        &self,
        root: &Path,
        staged: bool,
        paths: &[String],
    ) -> Result<RepoDiff, RepoError>;
    /// Current branch, `None` when detached.
    async fn current_branch(&self, root: &Path) -> Result<Option<String>, RepoError>;
    /// The repository's default branch per the remote HEAD, `None` when it
    /// cannot be determined.
    async fn default_branch(&self, root: &Path) -> Result<Option<String>, RepoError>;
    /// Stage exactly `paths` and commit them with `message`; returns a
    /// one-line summary of the created commit.
    async fn commit_paths(
        &self,
        root: &Path,
        message: &str,
        paths: &[String],
    ) -> Result<String, RepoError>;
    /// Push `branch` to the primary remote (upstream set; never forced).
    async fn push_branch(&self, root: &Path, branch: &str) -> Result<String, RepoError>;
    /// Fast-forward-only pull; divergence is [`RepoError::Diverged`].
    async fn pull_ff_only(&self, root: &Path) -> Result<String, RepoError>;
    /// Restore exactly `paths` to the last committed state.
    async fn restore_paths(&self, root: &Path, paths: &[String]) -> Result<String, RepoError>;
}

/// The `git` CLI adapter — direct argv spawns through the shared runner
/// (process-group kill, `GIT_*` repo-env scrubbing, credential scrubbing,
/// output truncation).
///
/// Remote operations intentionally do not preserve environment-token auth.
/// Push can execute repository-controlled hooks and credential helpers, so an
/// AWS/GitHub token allowlist here would expose those secrets to untrusted
/// code. SSH agents and OS/configured credential helpers continue to work;
/// environment-only SCM auth requires a future isolated credential broker.
pub struct GitCli;

impl GitCli {
    async fn git(
        root: &Path,
        action: &'static str,
        args: &[&str],
    ) -> Result<(i32, String), RepoError> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        exec::run_argv("git", &owned, root, REPO_TIMEOUT_SECS)
            .await
            .map_err(|e| {
                if e.contains("failed to spawn") {
                    RepoError::Unavailable(e)
                } else {
                    RepoError::Failed { action, detail: e }
                }
            })
    }

    /// Run and require exit 0; a nonzero exit is a named failure carrying
    /// the command's output.
    async fn git_ok(root: &Path, action: &'static str, args: &[&str]) -> Result<String, RepoError> {
        match Self::git(root, action, args).await? {
            (0, output) => Ok(output),
            (code, output) => Err(RepoError::Failed {
                action,
                detail: format!("exit {code}: {}", output.trim()),
            }),
        }
    }
}

#[async_trait]
impl RepoBackend for GitCli {
    async fn status(&self, root: &Path) -> Result<RepoStatus, RepoError> {
        let branch = self.current_branch(root).await?;
        // No upstream configured is the common non-error case → None/None.
        let (ahead, behind) = match Self::git(
            root,
            "status",
            &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
        )
        .await?
        {
            (0, out) => {
                let mut it = out.split_whitespace();
                let ahead = it.next().and_then(|n| n.parse::<u32>().ok());
                let behind = it.next().and_then(|n| n.parse::<u32>().ok());
                (ahead, behind)
            }
            _ => (None, None),
        };
        let porcelain = Self::git_ok(root, "status", &["status", "--porcelain"]).await?;
        let mut changed = Vec::new();
        let mut truncated = false;
        for line in porcelain.lines() {
            if line.len() < 4 {
                continue;
            }
            if changed.len() >= MAX_CHANGED_ROWS {
                truncated = true;
                break;
            }
            changed.push(ChangedFile {
                state: line[..2].to_string(),
                path: line[3..].to_string(),
            });
        }
        Ok(RepoStatus {
            branch,
            ahead,
            behind,
            changed,
            truncated,
        })
    }

    async fn diff(
        &self,
        root: &Path,
        staged: bool,
        paths: &[String],
    ) -> Result<RepoDiff, RepoError> {
        // `--no-ext-diff --no-textconv`: repository config can bind diff
        // drivers to arbitrary commands, and a read-only "show me the
        // hunks" must never execute repository-controlled code.
        let mut base = vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv"];
        if staged {
            base.push("--staged");
        }
        let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut stat_args = base.clone();
        stat_args.push("--numstat");
        if !path_refs.is_empty() {
            stat_args.push("--");
            stat_args.extend(&path_refs);
        }
        let numstat = Self::git_ok(root, "repo_diff", &stat_args).await?;
        // `added\tremoved\tpath` per line; binary changes report `-` in the
        // count columns, which parses to `None`.
        let files = numstat
            .lines()
            .filter_map(|line| {
                let mut cols = line.splitn(3, '\t');
                let added = cols.next()?;
                let removed = cols.next()?;
                let path = cols.next()?;
                Some(DiffFileStat {
                    path: path.to_string(),
                    added: added.parse().ok(),
                    removed: removed.parse().ok(),
                })
            })
            .collect();
        let mut patch_args = base;
        if !path_refs.is_empty() {
            patch_args.push("--");
            patch_args.extend(&path_refs);
        }
        let patch = Self::git_ok(root, "repo_diff", &patch_args).await?;
        Ok(RepoDiff { files, patch })
    }

    async fn current_branch(&self, root: &Path) -> Result<Option<String>, RepoError> {
        let out = Self::git_ok(root, "status", &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
        let name = out.trim();
        Ok((name != "HEAD").then(|| name.to_string()))
    }

    async fn default_branch(&self, root: &Path) -> Result<Option<String>, RepoError> {
        // Cheap local resolution first (set by clone), then ask the remote.
        if let (0, out) = Self::git(
            root,
            "push",
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
        .await?
            && let Some(name) = out.trim().strip_prefix("origin/")
        {
            return Ok(Some(name.to_string()));
        }
        if let (0, out) =
            Self::git(root, "push", &["ls-remote", "--symref", "origin", "HEAD"]).await?
        {
            for line in out.lines() {
                if let Some(rest) = line.strip_prefix("ref: refs/heads/")
                    && let Some(name) = rest.split_whitespace().next()
                {
                    return Ok(Some(name.to_string()));
                }
            }
        }
        Ok(None)
    }

    async fn commit_paths(
        &self,
        root: &Path,
        message: &str,
        paths: &[String],
    ) -> Result<String, RepoError> {
        let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut add = vec!["add", "--"];
        add.extend(&path_refs);
        Self::git_ok(root, "repo_commit", &add).await?;
        // Pathspec-limited commit: exactly the named paths land, whatever
        // else may sit in the index stays uncommitted.
        let mut commit = vec!["commit", "-m", message, "--"];
        commit.extend(&path_refs);
        Self::git_ok(root, "repo_commit", &commit).await?;
        let summary = Self::git_ok(root, "repo_commit", &["log", "-1", "--oneline"]).await?;
        Ok(format!("committed: {}", summary.trim()))
    }

    async fn push_branch(&self, root: &Path, branch: &str) -> Result<String, RepoError> {
        // Fully-qualified refspec: what gets pushed can only ever be a
        // branch head, and never with force.
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let out = Self::git_ok(
            root,
            "repo_push",
            &["push", "--set-upstream", "origin", &refspec],
        )
        .await?;
        Ok(format!("pushed `{branch}`\n{}", out.trim()))
    }

    async fn pull_ff_only(&self, root: &Path) -> Result<String, RepoError> {
        match Self::git(root, "repo_pull", &["pull", "--ff-only"]).await? {
            (0, out) => Ok(out.trim().to_string()),
            (_, out) if out.contains("fast-forward") || out.contains("divergent") => {
                Err(RepoError::Diverged(out.trim().to_string()))
            }
            (code, out) => Err(RepoError::Failed {
                action: "repo_pull",
                detail: format!("exit {code}: {}", out.trim()),
            }),
        }
    }

    async fn restore_paths(&self, root: &Path, paths: &[String]) -> Result<String, RepoError> {
        let mut args = vec!["checkout", "HEAD", "--"];
        args.extend(paths.iter().map(String::as_str));
        Self::git_ok(root, "repo_rollback", &args).await?;
        Ok(format!(
            "restored {} path(s) to the last committed state",
            paths.len()
        ))
    }
}

/// Extract a non-empty `paths: string[]` or return the structural refusal
/// shared by `repo_commit` and `repo_rollback` (see the module doc).
fn required_paths(input: &Value, tool: &str, verb: &str) -> Result<Vec<String>, ToolOutput> {
    let paths: Vec<String> = input
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if paths.is_empty() {
        return Err(ToolOutput::Error {
            message: format!(
                "{tool} {verb} exactly the paths you name — `paths` must be a non-empty \
                 list; a whole-tree operation must be spelled out path by path, never \
                 implied by an empty list"
            ),
        });
    }
    Ok(paths)
}

/// A branch name safe to hand to the backend as its own argv entry: no
/// leading `-` (option injection), no whitespace or control characters.
fn valid_branch_name(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.contains(|c: char| c.is_whitespace() || c.is_control())
}

/// `repo_status` — read-only: names and states, never content (that is
/// `repo_diff`).
pub struct RepoStatusTool(pub Arc<dyn RepoBackend>);

#[async_trait]
impl Tool for RepoStatusTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "repo_status".into(),
            description: "Repository status: current branch, commits ahead/behind upstream, \
                          and the changed files as structured rows."
                .into(),
            input_schema: serde_json::json!({ "type": "object", "properties": {} }),
            read_only: true,
        }
    }

    async fn execute(&self, _input: &Value, root: &Path) -> ToolOutput {
        match self.0.status(root).await {
            Ok(status) => match serde_json::to_string_pretty(&status) {
                Ok(json) => ToolOutput::Ok { content: json },
                Err(e) => ToolOutput::Error {
                    message: format!("cannot render repository status: {e}"),
                },
            },
            Err(e) => ToolOutput::Error {
                message: e.to_string(),
            },
        }
    }
}

/// Render a [`RepoDiff`] as a compact per-file summary followed by the raw
/// patch hunks, capping the summary rows at [`MAX_CHANGED_ROWS`] and the
/// patch at [`MAX_PATCH_BYTES`] — always with loud elision (module doc).
fn render_diff(diff: &RepoDiff, staged: bool) -> String {
    let scope = if staged { "staged" } else { "unstaged" };
    let patch = diff.patch.trim_end();
    if diff.files.is_empty() && patch.is_empty() {
        return format!(
            "no {scope} changes (files never staged or committed have no diff — \
             repo_status lists those)"
        );
    }
    let (added, removed) = diff.files.iter().fold((0u64, 0u64), |(a, r), f| {
        (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0))
    });
    let mut out = format!(
        "{} {scope} file(s) changed: +{added} -{removed}\n",
        diff.files.len()
    );
    for f in diff.files.iter().take(MAX_CHANGED_ROWS) {
        match (f.added, f.removed) {
            (Some(a), Some(r)) => out.push_str(&format!("  {} +{a} -{r}\n", f.path)),
            _ => out.push_str(&format!("  {} (binary)\n", f.path)),
        }
    }
    if diff.files.len() > MAX_CHANGED_ROWS {
        out.push_str(&format!(
            "  [… {} more file(s) not listed …]\n",
            diff.files.len() - MAX_CHANGED_ROWS
        ));
    }
    out.push('\n');
    if patch.len() > MAX_PATCH_BYTES {
        // Snap to a char boundary, then back to a whole line: a half-shown
        // hunk line would read as a content change that isn't there.
        let mut cap = MAX_PATCH_BYTES;
        while !patch.is_char_boundary(cap) {
            cap -= 1;
        }
        let cut = patch[..cap].rfind('\n').map_or(cap, |i| i + 1);
        out.push_str(&patch[..cut]);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!(
            "[… diff truncated: {cut} of {} bytes shown — re-run with `paths` scoped \
             to specific files for their full hunks …]",
            patch.len()
        ));
    } else {
        out.push_str(patch);
    }
    out
}

/// `repo_diff` — the other read-only repository tool: the actual pending
/// hunks, so a pre-commit self-review is grounded in the real patch rather
/// than the agent's narration of it.
pub struct RepoDiffTool(pub Arc<dyn RepoBackend>);

#[async_trait]
impl Tool for RepoDiffTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "repo_diff".into(),
            description: "The pending changes as patch hunks (with file/line context) plus a \
                          per-file +added/-removed summary — review what you ACTUALLY changed \
                          before repo_commit or verify_done. Unstaged changes by default; \
                          `staged: true` shows changes already staged for commit instead. \
                          Files never staged or committed have no diff — repo_status lists \
                          those."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "staged": { "type": "boolean", "description": "Show changes already staged for commit instead of unstaged ones (default false)" },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional workspace-relative paths to scope the diff to" }
                }
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, root: &Path) -> ToolOutput {
        let staged = input
            .get("staged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let paths: Vec<String> = input
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        match self.0.diff(root, staged, &paths).await {
            Ok(diff) => ToolOutput::Ok {
                content: render_diff(&diff, staged),
            },
            Err(e) => ToolOutput::Error {
                message: e.to_string(),
            },
        }
    }
}

/// `repo_commit` — pathspec-explicit commit; see the module doc.
pub struct RepoCommit(pub Arc<dyn RepoBackend>);

#[async_trait]
impl Tool for RepoCommit {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "repo_commit".into(),
            description: "Commit EXACTLY the named paths: stages them, then commits only \
                          them. paths is required and must be non-empty — there is no \
                          stage-everything mode."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "Commit message" },
                    "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "The exact workspace-relative paths to commit" }
                },
                "required": ["message", "paths"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &Path) -> ToolOutput {
        let Some(message) = input
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
        else {
            return ToolOutput::Error {
                message: "missing required field `message`".into(),
            };
        };
        let paths = match required_paths(input, "repo_commit", "commits") {
            Ok(paths) => paths,
            Err(refusal) => return refusal,
        };
        match self.0.commit_paths(root, message, &paths).await {
            Ok(summary) => ToolOutput::Ok { content: summary },
            Err(e) => ToolOutput::Error {
                message: e.to_string(),
            },
        }
    }
}

/// `repo_push` — never the default branch, never forced; see the module doc.
pub struct RepoPush(pub Arc<dyn RepoBackend>);

#[async_trait]
impl Tool for RepoPush {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "repo_push".into(),
            description: "Push the current (or named) branch to the primary remote. \
                          STRUCTURALLY refuses the repository's default branch — publish \
                          work on a feature branch. Force-push does not exist here."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "branch": { "type": "string", "description": "Branch to push (default: the current branch)" }
                }
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &Path) -> ToolOutput {
        let named = input.get("branch").and_then(|v| v.as_str());
        if let Some(branch) = named
            && !valid_branch_name(branch)
        {
            return ToolOutput::Error {
                message: format!(
                    "`{branch}` is not a valid branch name (must not start with `-` or \
                     contain whitespace)"
                ),
            };
        }
        let branch = match named {
            Some(b) => b.to_string(),
            None => match self.0.current_branch(root).await {
                Ok(Some(b)) => b,
                Ok(None) => {
                    return ToolOutput::Error {
                        message: "the checkout is detached (no current branch) — pass \
                                  `branch` explicitly"
                            .into(),
                    };
                }
                Err(e) => {
                    return ToolOutput::Error {
                        message: e.to_string(),
                    };
                }
            },
        };
        // The structural rule: resolve the default branch and refuse it.
        // An UNRESOLVABLE default fails closed — pushing blind could be
        // pushing the default.
        match self.0.default_branch(root).await {
            Ok(Some(default)) if default == branch => {
                return ToolOutput::Error {
                    message: format!(
                        "repo_push refuses to push `{branch}`: it is the repository's \
                         default branch. Publish work on a feature branch instead — this \
                         rule is structural and has no override"
                    ),
                };
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                return ToolOutput::Error {
                    message: "cannot determine the repository's default branch (remote \
                              HEAD) — refusing to push rather than risk pushing it"
                        .into(),
                };
            }
            Err(e) => {
                return ToolOutput::Error {
                    message: e.to_string(),
                };
            }
        }
        match self.0.push_branch(root, &branch).await {
            Ok(out) => ToolOutput::Ok { content: out },
            Err(e) => ToolOutput::Error {
                message: e.to_string(),
            },
        }
    }
}

/// `repo_pull` — fast-forward only; see the module doc.
pub struct RepoPull(pub Arc<dyn RepoBackend>);

#[async_trait]
impl Tool for RepoPull {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "repo_pull".into(),
            description: "Update the current branch from its upstream, fast-forward only. \
                          Diverged histories are a typed error — this tool never merges or \
                          rewrites."
                .into(),
            input_schema: serde_json::json!({ "type": "object", "properties": {} }),
            read_only: false,
        }
    }

    async fn execute(&self, _input: &Value, root: &Path) -> ToolOutput {
        match self.0.pull_ff_only(root).await {
            Ok(out) => ToolOutput::Ok { content: out },
            Err(e) => ToolOutput::Error {
                message: e.to_string(),
            },
        }
    }
}

/// `repo_rollback` — restore named paths to HEAD; see the module doc.
pub struct RepoRollback(pub Arc<dyn RepoBackend>);

#[async_trait]
impl Tool for RepoRollback {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "repo_rollback".into(),
            description: "Restore EXACTLY the named paths to their last committed state, \
                          discarding local modifications to them. paths is required and \
                          must be non-empty. History itself is never rewritten."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "The exact workspace-relative paths to restore" }
                },
                "required": ["paths"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &Path) -> ToolOutput {
        let paths = match required_paths(input, "repo_rollback", "restores") {
            Ok(paths) => paths,
            Err(refusal) => return refusal,
        };
        match self.0.restore_paths(root, &paths).await {
            Ok(out) => ToolOutput::Ok { content: out },
            Err(e) => ToolOutput::Error {
                message: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a real `git` is on PATH; these integration tests skip
    /// cleanly (with a printed note) when it isn't — mirroring the
    /// real-git tests in stella-fleet.
    async fn git_available() -> bool {
        tokio::process::Command::new("git")
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Fixture plumbing: run git expecting success (unwrap is fine in tests).
    async fn sh_git(dir: &Path, args: &[&str]) {
        let out = tokio::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Give a repo a deterministic identity and no signing, so commits work
    /// in any environment.
    async fn config_identity(dir: &Path) {
        for (k, v) in [
            ("user.email", "test@stella.local"),
            ("user.name", "Stella Test"),
            ("commit.gpgsign", "false"),
        ] {
            sh_git(dir, &["config", k, v]).await;
        }
    }

    /// Build: a seed repo on `main` with one commit → a bare `origin.git`
    /// → a working clone `ws` (whose `origin/HEAD` resolves to `main`).
    /// Returns the working clone path.
    async fn fixture(dir: &Path) -> std::path::PathBuf {
        let seed = dir.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        sh_git(&seed, &["init", "--quiet"]).await;
        // Version-portable "main" (predates `init -b`).
        sh_git(&seed, &["symbolic-ref", "HEAD", "refs/heads/main"]).await;
        config_identity(&seed).await;
        std::fs::write(seed.join("README.md"), "seed\n").unwrap();
        sh_git(&seed, &["add", "README.md"]).await;
        sh_git(&seed, &["commit", "-q", "-m", "seed"]).await;

        let origin = dir.join("origin.git");
        sh_git(
            dir,
            &[
                "clone",
                "--quiet",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        )
        .await;
        let ws = dir.join("ws");
        sh_git(
            dir,
            &[
                "clone",
                "--quiet",
                origin.to_str().unwrap(),
                ws.to_str().unwrap(),
            ],
        )
        .await;
        config_identity(&ws).await;
        ws
    }

    fn backend() -> Arc<dyn RepoBackend> {
        Arc::new(GitCli)
    }

    #[tokio::test]
    async fn repo_status_reports_branch_upstream_and_changed_rows() {
        if !git_available().await {
            eprintln!("skipping repo_status test: `git` not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = fixture(dir.path()).await;
        std::fs::write(ws.join("README.md"), "changed\n").unwrap();
        std::fs::write(ws.join("new.txt"), "new\n").unwrap();

        let out = RepoStatusTool(backend()).execute(&Value::Null, &ws).await;
        let ToolOutput::Ok { content } = out else {
            panic!("repo_status must succeed: {out:?}");
        };
        let status: Value = serde_json::from_str(&content).expect("typed JSON payload");
        assert_eq!(status["branch"], "main");
        assert_eq!(status["ahead"], 0);
        assert_eq!(status["behind"], 0);
        let changed: Vec<String> = status["changed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["path"].as_str().unwrap().to_string())
            .collect();
        assert!(changed.contains(&"README.md".to_string()), "{content}");
        assert!(changed.contains(&"new.txt".to_string()), "{content}");
        assert_eq!(status["truncated"], false);
    }

    /// The issue-#332 witness: after modifying a file, `repo_diff` returns
    /// a hunk containing the changed line — not just names and states.
    #[tokio::test]
    async fn repo_diff_returns_hunks_containing_the_changed_line() {
        if !git_available().await {
            eprintln!("skipping repo_diff test: `git` not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = fixture(dir.path()).await;
        std::fs::write(ws.join("README.md"), "reviewed line\n").unwrap();

        let out = RepoDiffTool(backend()).execute(&Value::Null, &ws).await;
        let ToolOutput::Ok { content } = out else {
            panic!("repo_diff must succeed: {out:?}");
        };
        // The actual hunk — old line out, new line in — not a narration.
        assert!(content.contains("+reviewed line"), "{content}");
        assert!(content.contains("-seed"), "{content}");
        // And the compact per-file line summary.
        assert!(content.contains("README.md +1 -1"), "{content}");
    }

    #[tokio::test]
    async fn repo_diff_separates_staged_changes_and_scopes_to_paths() {
        if !git_available().await {
            eprintln!("skipping repo_diff test: `git` not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = fixture(dir.path()).await;

        // A clean tree diffs to nothing, loudly named.
        let clean = RepoDiffTool(backend()).execute(&Value::Null, &ws).await;
        let ToolOutput::Ok { content } = clean else {
            panic!("clean diff must succeed: {clean:?}");
        };
        assert!(content.contains("no unstaged changes"), "{content}");

        // A second tracked file, so path scoping has two candidates.
        std::fs::write(ws.join("b.txt"), "b\n").unwrap();
        sh_git(&ws, &["add", "b.txt"]).await;
        sh_git(&ws, &["commit", "-q", "-m", "add b"]).await;
        std::fs::write(ws.join("README.md"), "readme edit\n").unwrap();
        std::fs::write(ws.join("b.txt"), "b edit\n").unwrap();

        // Unscoped: both files' hunks.
        let both = RepoDiffTool(backend()).execute(&Value::Null, &ws).await;
        let ToolOutput::Ok { content } = both else {
            panic!("unscoped diff must succeed: {both:?}");
        };
        assert!(content.contains("+readme edit"), "{content}");
        assert!(content.contains("+b edit"), "{content}");

        // Scoped to b.txt: README's hunk is gone.
        let scoped = RepoDiffTool(backend())
            .execute(&serde_json::json!({"paths": ["b.txt"]}), &ws)
            .await;
        let ToolOutput::Ok { content } = scoped else {
            panic!("scoped diff must succeed: {scoped:?}");
        };
        assert!(content.contains("+b edit"), "{content}");
        assert!(!content.contains("readme edit"), "{content}");

        // Stage README: it moves from the unstaged view to the staged one.
        sh_git(&ws, &["add", "README.md"]).await;
        let unstaged = RepoDiffTool(backend()).execute(&Value::Null, &ws).await;
        let ToolOutput::Ok { content } = unstaged else {
            panic!("unstaged diff must succeed: {unstaged:?}");
        };
        assert!(content.contains("+b edit"), "{content}");
        assert!(!content.contains("readme edit"), "{content}");
        let staged = RepoDiffTool(backend())
            .execute(&serde_json::json!({"staged": true}), &ws)
            .await;
        let ToolOutput::Ok { content } = staged else {
            panic!("staged diff must succeed: {staged:?}");
        };
        assert!(content.contains("+readme edit"), "{content}");
        assert!(!content.contains("b edit"), "{content}");
    }

    #[test]
    fn repo_diff_render_caps_the_patch_with_loud_elision() {
        let patch = "+padding line\n".repeat(3_000);
        let total = patch.trim_end().len();
        let diff = RepoDiff {
            files: vec![
                DiffFileStat {
                    path: "big.txt".into(),
                    added: Some(3_000),
                    removed: Some(0),
                },
                DiffFileStat {
                    path: "logo.png".into(),
                    added: None,
                    removed: None,
                },
            ],
            patch,
        };
        let rendered = render_diff(&diff, false);
        assert!(rendered.len() < total, "the cap must actually cut");
        assert!(rendered.contains("diff truncated"), "{rendered}");
        assert!(
            rendered.contains(&format!("of {total} bytes shown")),
            "{rendered}"
        );
        // The cut lands on a whole line — no half-shown hunk line.
        let marker = rendered.find("[… diff truncated").unwrap();
        assert_eq!(&rendered[marker - 1..marker], "\n");
        // Binary files render as such, never as +0 -0.
        assert!(rendered.contains("logo.png (binary)"), "{rendered}");
    }

    #[tokio::test]
    async fn repo_commit_stages_exactly_the_named_paths_and_refuses_empty() {
        if !git_available().await {
            eprintln!("skipping repo_commit test: `git` not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = fixture(dir.path()).await;
        std::fs::write(ws.join("a.txt"), "a\n").unwrap();
        std::fs::write(ws.join("b.txt"), "b\n").unwrap();

        // Empty list: the structural refusal, before anything runs.
        let refused = RepoCommit(backend())
            .execute(&serde_json::json!({"message": "nope", "paths": []}), &ws)
            .await;
        match refused {
            ToolOutput::Error { message } => {
                assert!(message.contains("non-empty"), "{message}")
            }
            other => panic!("empty paths must be refused: {other:?}"),
        }

        let out = RepoCommit(backend())
            .execute(
                &serde_json::json!({"message": "add a only", "paths": ["a.txt"]}),
                &ws,
            )
            .await;
        let ToolOutput::Ok { content } = out else {
            panic!("commit must succeed: {out:?}");
        };
        assert!(content.contains("add a only"), "{content}");

        // b.txt was NOT swept into the commit.
        let status = RepoStatusTool(backend()).execute(&Value::Null, &ws).await;
        let ToolOutput::Ok { content } = status else {
            panic!("status after commit");
        };
        assert!(content.contains("b.txt"), "b.txt must remain uncommitted");
        assert!(
            !content.contains("a.txt"),
            "a.txt must be committed: {content}"
        );
    }

    #[tokio::test]
    async fn repo_push_refuses_the_default_branch_but_pushes_a_feature_branch() {
        if !git_available().await {
            eprintln!("skipping repo_push test: `git` not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = fixture(dir.path()).await;

        // On the default branch: the structural refusal names the rule.
        let refused = RepoPush(backend())
            .execute(&serde_json::json!({}), &ws)
            .await;
        match refused {
            ToolOutput::Error { message } => {
                assert!(message.contains("default branch"), "{message}");
                assert!(message.contains("`main`"), "{message}");
            }
            other => panic!("default-branch push must be refused: {other:?}"),
        }

        // An option-shaped branch name is rejected before any spawn.
        let injected = RepoPush(backend())
            .execute(&serde_json::json!({"branch": "--force"}), &ws)
            .await;
        match injected {
            ToolOutput::Error { message } => {
                assert!(message.contains("not a valid branch name"), "{message}")
            }
            other => panic!("option-shaped branch must be refused: {other:?}"),
        }

        // A feature branch pushes fine and lands on the remote.
        sh_git(&ws, &["checkout", "-q", "-b", "feature/x"]).await;
        std::fs::write(ws.join("f.txt"), "f\n").unwrap();
        RepoCommit(backend())
            .execute(
                &serde_json::json!({"message": "feature", "paths": ["f.txt"]}),
                &ws,
            )
            .await;
        let pushed = RepoPush(backend())
            .execute(&serde_json::json!({}), &ws)
            .await;
        assert!(!pushed.is_error(), "{pushed:?}");
        let origin = dir.path().join("origin.git");
        let out = tokio::process::Command::new("git")
            .args(["rev-parse", "--verify", "refs/heads/feature/x"])
            .current_dir(&origin)
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "feature/x must exist on the remote");
    }

    #[tokio::test]
    async fn repo_pull_fast_forwards_and_types_divergence() {
        if !git_available().await {
            eprintln!("skipping repo_pull test: `git` not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = fixture(dir.path()).await;
        // A second clone advances origin/main (fixture plumbing, raw git —
        // repo_push structurally refuses main, which is the point of it).
        let ws2 = dir.path().join("ws2");
        sh_git(
            dir.path(),
            &[
                "clone",
                "--quiet",
                dir.path().join("origin.git").to_str().unwrap(),
                ws2.to_str().unwrap(),
            ],
        )
        .await;
        config_identity(&ws2).await;
        std::fs::write(ws2.join("upstream.txt"), "up\n").unwrap();
        sh_git(&ws2, &["add", "upstream.txt"]).await;
        sh_git(&ws2, &["commit", "-q", "-m", "upstream change"]).await;
        sh_git(&ws2, &["push", "-q", "origin", "main"]).await;

        // Clean fast-forward.
        let pulled = RepoPull(backend()).execute(&Value::Null, &ws).await;
        assert!(!pulled.is_error(), "{pulled:?}");
        assert!(ws.join("upstream.txt").exists(), "ff must land the commit");

        // Diverge: upstream advances again AND ws commits locally.
        std::fs::write(ws2.join("upstream.txt"), "up2\n").unwrap();
        sh_git(&ws2, &["add", "upstream.txt"]).await;
        sh_git(&ws2, &["commit", "-q", "-m", "upstream 2"]).await;
        sh_git(&ws2, &["push", "-q", "origin", "main"]).await;
        std::fs::write(ws.join("local.txt"), "local\n").unwrap();
        sh_git(&ws, &["add", "local.txt"]).await;
        sh_git(&ws, &["commit", "-q", "-m", "local change"]).await;

        let diverged = RepoPull(backend()).execute(&Value::Null, &ws).await;
        match diverged {
            ToolOutput::Error { message } => {
                assert!(message.contains("fast-forward"), "{message}")
            }
            other => panic!("divergence must be a typed error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn repo_rollback_restores_named_paths_and_refuses_an_empty_list() {
        if !git_available().await {
            eprintln!("skipping repo_rollback test: `git` not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = fixture(dir.path()).await;
        std::fs::write(ws.join("README.md"), "mangled\n").unwrap();

        let refused = RepoRollback(backend())
            .execute(&serde_json::json!({"paths": []}), &ws)
            .await;
        match refused {
            ToolOutput::Error { message } => {
                assert!(message.contains("non-empty"), "{message}");
            }
            other => panic!("empty rollback must be refused: {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(ws.join("README.md")).unwrap(),
            "mangled\n",
            "a refused rollback must not touch the tree"
        );

        let rolled = RepoRollback(backend())
            .execute(&serde_json::json!({"paths": ["README.md"]}), &ws)
            .await;
        assert!(!rolled.is_error(), "{rolled:?}");
        assert_eq!(
            std::fs::read_to_string(ws.join("README.md")).unwrap(),
            "seed\n",
            "the named path returns to its committed content"
        );
    }
}
