//! Best-of-N candidate isolation over real git — the production
//! [`CandidateWorkspacePort`]. Each candidate gets a detached shadow
//! worktree (the `verify_done` shadow pattern: temp-dir path, pid+counter
//! name, forced removal + prune on every exit) snapshotting the user's
//! CURRENT tree state:
//!
//! 1. `git worktree add --detach <tmp> <HEAD>` — never a branch, never a
//!    checkout of the user's tree;
//! 2. the uncommitted tracked delta overlaid via `git diff --binary HEAD`
//!    applied inside the shadow;
//! 3. untracked, non-ignored files copied in with mtimes preserved (the
//!    pipeline's fingerprints are `len:mtime`, so a copy that bumped mtime
//!    would read as witness tampering in every candidate);
//! 4. one baseline commit sealed in the shadow's PRIVATE index (detached
//!    HEAD — no ref of the user's repo moves).
//!
//! The user's working tree, index, and stash are NEVER touched: the stash is
//! shared machine state other sessions rely on, so it is banned outright —
//! no `git stash` appears anywhere in this module, and every command that
//! runs against the real repo is read-only except the final winner-adoption
//! `git apply` (worktree only, no `--index`). Adoption diffs the shadow's
//! final state against its baseline commit and applies the result in one
//! `git apply`, which verifies every preimage before writing anything — a
//! file the user edited mid-run fails the whole adoption loudly, naming the
//! paths, instead of half-applying.
//!
//! # What a candidate's engine can reach
//!
//! Candidates drive a fresh built-in [`ToolRegistry`] rooted at the
//! snapshot (with the session's workspace rules and schema gate applied).
//! MCP servers, custom script tools, and the interactive/discovery layers
//! are deliberately NOT re-wired into candidates: they were spawned against
//! the real workspace, so routing them into a candidate would leak its edits
//! straight into the tree isolation exists to protect.
//! TODO(best-of-n-tool-surface): grow the candidate tool surface — custom
//! script tools re-rooted at the snapshot are straightforward; MCP needs
//! per-candidate server sessions (or a cwd-forwarding protocol extension)
//! before it can be offered safely.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use stella_fleet::git::{GitCli, SystemGitCli};
use stella_pipeline::ports::{
    AdoptedChange, CandidateWorkspace, CandidateWorkspacePort, CommandRunner, RepoStatusPort,
    WorkspaceError,
};
use stella_protocol::FileChangeKind;
use stella_tools::{RegistryOptions, ToolRegistry};

use crate::agent::{GitRepoStatus, ShellCommandRunner, fs_fingerprint};

/// The commit identity for snapshot plumbing commits (which exist only
/// inside the shadow and are discarded with it) — the user's repo may have
/// no identity configured (CI), and their real identity must never be
/// implied on machinery commits.
const SNAPSHOT_IDENT: [&str; 4] = [
    "-c",
    "user.name=stella-pipeline",
    "-c",
    "user.email=pipeline@stella.invalid",
];

/// Shadow names carry pid + a process-wide counter (the `verify_done`
/// pattern): concurrent candidates must never collide on a path.
static SHADOW_SEQ: AtomicU64 = AtomicU64::new(0);

/// One git command against `repo` through the fleet's [`SystemGitCli`]
/// (explicit `-C` targeting, hook-exported `GIT_*` env scrubbed, no
/// terminal prompt). Flattens both spawn failures and non-zero exits into a
/// reason string; callers wrap it into the right [`WorkspaceError`] variant.
async fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    match SystemGitCli.run(repo, args).await {
        Ok(out) if out.success => Ok(out.stdout),
        Ok(out) => Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            out.stderr.trim()
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// `git <args>` with stdout captured as raw bytes and written to `out`.
/// Patches must never round-trip through [`SystemGitCli`]'s lossy UTF-8
/// stdout — a non-UTF-8 source file's diff would be corrupted and then
/// mis-applied. (Raw bytes in memory, not an fd redirect: tokio's
/// `Command::output` forcibly re-pipes any configured stdout.) Same
/// repo-targeting discipline as the port: explicit `-C`, scrubbed `GIT_*`
/// env, no prompt.
async fn git_stdout_to_file(repo: &Path, args: &[&str], out: &Path) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    for var in stella_tools::exec::GIT_REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.kill_on_drop(true);
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("failed to spawn `git {}`: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    tokio::fs::write(out, &output.stdout)
        .await
        .map_err(|e| format!("could not write `{}`: {e}", out.display()))
}

/// Copy `src` to `dst` (creating parents), preserving the modification time.
/// The pipeline fingerprints untracked files as `len:mtime` and the witness
/// tamper watchlist is recorded against the REAL tree — a copy that bumped
/// mtime would make every candidate read its witness as tampered.
async fn copy_preserving_mtime(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::copy(src, dst).await?;
    let meta = tokio::fs::metadata(src).await?;
    if let Ok(modified) = meta.modified() {
        // `std::fs::copy` copies the source's permission bits, so a read-only
        // source (e.g. a `chmod 444` untracked artifact) yields a read-only
        // `dst`. Opening that `.write(true)` to stamp the mtime would fail
        // with EACCES for the owner, aborting the whole candidate snapshot.
        // Temporarily clear the read-only bit, set the time, then restore the
        // original permissions so the snapshot's mode still mirrors the real
        // tree.
        let perms = meta.permissions();
        let restore = if perms.readonly() {
            let mut writable = perms.clone();
            // Grant only the owner-write bit so the mtime stamp below can open
            // `dst` for writing; `set_readonly(false)` would also add the
            // group/other-write bits (`0o222`, world-writable on Unix). The
            // original mode is restored right after, so the writable window is
            // momentary and confined to this private per-candidate shadow tree.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                writable.set_mode(writable.mode() | 0o200);
            }
            #[cfg(not(unix))]
            {
                #[allow(clippy::permissions_set_readonly_false)]
                writable.set_readonly(false);
            }
            std::fs::set_permissions(dst, writable)?;
            Some(perms)
        } else {
            None
        };
        let res = std::fs::OpenOptions::new()
            .write(true)
            .open(dst)
            .and_then(|dst_file| {
                dst_file.set_times(std::fs::FileTimes::new().set_modified(modified))
            });
        if let Some(perms) = restore {
            // Restore the original read-only bits regardless of the result.
            let _ = std::fs::set_permissions(dst, perms);
        }
        res?;
    }
    Ok(())
}

/// Best-effort removal of a candidate shadow worktree — the registration and
/// the directory both, then a prune for any stale registration (the
/// `verify_done` cleanup discipline: called on every path, success or
/// failure).
async fn cleanup(toplevel: &Path, dir: &Path) {
    if let Some(d) = dir.to_str() {
        let _ = SystemGitCli
            .run(toplevel, &["worktree", "remove", "--force", d])
            .await;
    }
    let _ = tokio::fs::remove_dir_all(dir).await;
    let _ = SystemGitCli.run(toplevel, &["worktree", "prune"]).await;
}

/// The production [`CandidateWorkspacePort`]: real-git candidate snapshots
/// rooted at the session's workspace.
pub(crate) struct GitCandidateWorkspaces {
    /// The session workspace root — possibly a subdirectory of the repo
    /// toplevel (the `verify_done` canonicalization trap: the shadow mirrors
    /// the TOPLEVEL, and the candidate's ports re-descend into the matching
    /// subdirectory).
    root: PathBuf,
    /// Registry switches for the per-candidate tool registry (same secure
    /// posture as the session's: `bash` only when settings opt in).
    options: RegistryOptions,
}

impl GitCandidateWorkspaces {
    pub(crate) fn new(root: PathBuf, options: RegistryOptions) -> Self {
        Self { root, options }
    }

    /// The concrete create (the trait impl boxes its result): snapshot the
    /// current tree state into a fresh detached shadow worktree.
    async fn create_workspace(&self) -> Result<GitCandidateWorkspace, WorkspaceError> {
        let snap = |reason: String| WorkspaceError::Snapshot { reason };
        let canon_root = self
            .root
            .canonicalize()
            .map_err(|e| snap(format!("could not canonicalize the workspace root: {e}")))?;
        let toplevel = git(&canon_root, &["rev-parse", "--show-toplevel"])
            .await
            .map_err(snap)?;
        let toplevel = PathBuf::from(toplevel.trim());
        let toplevel = toplevel.canonicalize().unwrap_or(toplevel);
        let head = git(&toplevel, &["rev-parse", "HEAD"]).await.map_err(|e| {
            snap(format!(
                "candidate isolation requires a git repository with at least one commit: {e}"
            ))
        })?;
        let head = head.trim().to_string();
        let root_rel = canon_root
            .strip_prefix(&toplevel)
            .unwrap_or(Path::new(""))
            .to_path_buf();

        let dir = std::env::temp_dir().join(format!(
            "stella_candidate_{}_{}",
            std::process::id(),
            SHADOW_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let dir_str = dir
            .to_str()
            .ok_or_else(|| snap("temp dir path is not valid UTF-8".to_string()))?;
        git(&toplevel, &["worktree", "add", "--detach", dir_str, &head])
            .await
            .map_err(snap)?;

        // From here on every failure must tear the shadow down.
        match populate_snapshot(&toplevel, &dir, &root_rel).await {
            Ok(overlay_untracked) => {
                let ws_root = dir.join(&root_rel);
                let tools = ToolRegistry::new_detected(ws_root.clone(), self.options).await;
                // Same governance as the session registry: workspace rules
                // and the schema gate travel with the tree — best-of-N must
                // not be a way around them.
                crate::agent::populate_schema_index(&tools, &ws_root);
                crate::rules::enforce_workspace_rules(&tools, &ws_root);
                Ok(GitCandidateWorkspace {
                    toplevel,
                    dir: dir.clone(),
                    tools,
                    commands: ShellCommandRunner {
                        root: ws_root.clone(),
                    },
                    repo_status: SnapshotRepoStatus {
                        inner: GitRepoStatus {
                            root: ws_root.clone(),
                        },
                        ws_root,
                        overlay: overlay_untracked,
                    },
                })
            }
            Err(reason) => {
                cleanup(&toplevel, &dir).await;
                Err(snap(reason))
            }
        }
    }
}

#[async_trait]
impl CandidateWorkspacePort for GitCandidateWorkspaces {
    async fn create(&self) -> Result<Box<dyn CandidateWorkspace>, WorkspaceError> {
        Ok(Box::new(self.create_workspace().await?))
    }
}

/// Overlay the user's uncommitted state onto the fresh shadow at `dir` and
/// seal it as the baseline commit. Returns the ws-root-relative paths of the
/// untracked files that were copied in (the [`SnapshotRepoStatus`] overlay
/// set).
async fn populate_snapshot(
    toplevel: &Path,
    dir: &Path,
    root_rel: &Path,
) -> Result<Vec<String>, String> {
    // 1. The uncommitted tracked delta — staged and unstaged both (`git diff
    //    HEAD` sees the union), `--binary` so non-text files survive.
    let patch_file = std::env::temp_dir().join(format!(
        "stella_candidate_overlay_{}_{}.patch",
        std::process::id(),
        SHADOW_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    git_stdout_to_file(toplevel, &["diff", "--binary", "HEAD"], &patch_file).await?;
    let patch_len = std::fs::metadata(&patch_file).map(|m| m.len()).unwrap_or(0);
    let applied = if patch_len > 0 {
        let patch_str = patch_file
            .to_str()
            .ok_or_else(|| "patch path is not valid UTF-8".to_string())?;
        git(dir, &["apply", "--whitespace=nowarn", patch_str]).await
    } else {
        Ok(String::new())
    };
    let _ = tokio::fs::remove_file(&patch_file).await;
    applied?;

    // 2. Untracked, non-ignored files — `git diff` is blind to them, so they
    //    ride as real copies. `-z` NUL-delimits (spaces/newlines in paths),
    //    quotePath off keeps non-ASCII literal.
    let listing = git(
        toplevel,
        &[
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )
    .await?;
    let mut overlay: Vec<String> = Vec::new();
    for rel in listing.split('\0').filter(|p| !p.is_empty()) {
        match copy_preserving_mtime(&toplevel.join(rel), &dir.join(rel)).await {
            Ok(()) => {}
            // A file that vanished between listing and copy is dirty-state
            // churn, not a snapshot failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(format!(
                    "could not copy untracked file `{rel}` into the candidate snapshot: {e}"
                ));
            }
        }
        // Only files under the workspace root enter the overlay set — the
        // real-tree RepoStatusPort (rooted at the workspace root) cannot see
        // the others either.
        if let Ok(ws_rel) = Path::new(rel).strip_prefix(root_rel) {
            overlay.push(ws_rel.to_string_lossy().into_owned());
        }
    }

    // 3. Seal the snapshot as ONE baseline commit in the shadow's PRIVATE
    //    index (detached HEAD — no branch, no user index, no stash). The
    //    blanket `git add -A` the fleet bans for shared/contested trees is
    //    safe — and wanted — here: this worktree was created microseconds
    //    ago and is owned by exactly this candidate, so there is nothing of
    //    anyone else's to sweep. `--no-verify`/`--no-gpg-sign` keep user
    //    hooks and signing out of snapshot plumbing.
    git(dir, &["add", "-A"]).await?;
    let mut commit_args: Vec<&str> = SNAPSHOT_IDENT.to_vec();
    commit_args.extend([
        "commit",
        "--allow-empty",
        "--no-verify",
        "--no-gpg-sign",
        "-q",
        "-m",
        "stella: candidate baseline snapshot",
    ]);
    git(dir, &commit_args).await?;
    Ok(overlay)
}

/// The snapshot's untracked view. Inside the shadow, files that were
/// untracked in the REAL tree are baseline-committed (winner adoption needs
/// them diffable), so plain `git ls-files --others` no longer reports them —
/// and the witness tamper watchlist, recorded against the real tree's
/// untracked fingerprints, would read every witness file as deleted. This
/// port reports the union: the shadow's own untracked files (the
/// candidate's new work) plus the overlay set, fingerprinted from the
/// shadow's filesystem — copies preserve len+mtime, so an untouched overlay
/// file matches its real-tree fingerprint exactly, an edited one differs,
/// and a deleted one is absent: the same semantics the real tree shows.
struct SnapshotRepoStatus {
    inner: GitRepoStatus,
    ws_root: PathBuf,
    /// Ws-root-relative paths of the untracked files copied into the shadow.
    overlay: Vec<String>,
}

#[async_trait]
impl RepoStatusPort for SnapshotRepoStatus {
    async fn untracked_fingerprints(&self) -> HashMap<String, String> {
        let mut map = self.inner.untracked_fingerprints().await;
        for rel in &self.overlay {
            if map.contains_key(rel) {
                continue;
            }
            if let Some(fp) = fs_fingerprint(&self.ws_root.join(rel)) {
                map.insert(rel.clone(), fp);
            }
        }
        map
    }
}

/// One live candidate shadow — see the module docs for the lifecycle.
pub(crate) struct GitCandidateWorkspace {
    /// The real repo's toplevel (canonicalized): the adoption target.
    toplevel: PathBuf,
    /// The shadow worktree directory.
    dir: PathBuf,
    tools: ToolRegistry,
    commands: ShellCommandRunner,
    repo_status: SnapshotRepoStatus,
}

impl GitCandidateWorkspace {
    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Winner-only adoption: seal the shadow's final state as a commit, diff
    /// baseline→final, and apply that patch to the REAL tree in one atomic
    /// `git apply` — no `--index`, so the user's index is untouched and the
    /// adopted files land exactly as uncommitted working-tree changes.
    async fn adopt_inner(&self) -> Result<Vec<AdoptedChange>, WorkspaceError> {
        let fail = |reason: String, paths: Vec<String>| WorkspaceError::Adopt {
            reason,
            paths,
            workspace: self.dir.display().to_string(),
        };
        // Blanket add: the same private-worktree justification as the
        // baseline commit in `populate_snapshot`.
        git(&self.dir, &["add", "-A"])
            .await
            .map_err(|e| fail(e, Vec::new()))?;
        let mut commit_args: Vec<&str> = SNAPSHOT_IDENT.to_vec();
        commit_args.extend([
            "commit",
            "--allow-empty",
            "--no-verify",
            "--no-gpg-sign",
            "-q",
            "-m",
            "stella: candidate result",
        ]);
        git(&self.dir, &commit_args)
            .await
            .map_err(|e| fail(e, Vec::new()))?;

        // Baseline is the first parent of the sealed result commit — the
        // shadow's history is exactly [baseline, result] by construction.
        let names = git(
            &self.dir,
            &[
                "diff",
                "--name-status",
                "--no-renames",
                "-z",
                "HEAD~1",
                "HEAD",
            ],
        )
        .await
        .map_err(|e| fail(e, Vec::new()))?;
        let changes = parse_name_status(&names);
        if changes.is_empty() {
            return Ok(Vec::new());
        }

        let patch_file = std::env::temp_dir().join(format!(
            "stella_candidate_adopt_{}_{}.patch",
            std::process::id(),
            SHADOW_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let all_paths = || changes.iter().map(|c| c.path.clone()).collect::<Vec<_>>();
        git_stdout_to_file(
            &self.dir,
            &["diff", "--binary", "--no-renames", "HEAD~1", "HEAD"],
            &patch_file,
        )
        .await
        .map_err(|e| fail(e, all_paths()))?;
        let patch_str = match patch_file.to_str() {
            Some(s) => s,
            None => {
                return Err(fail(
                    "patch path is not valid UTF-8".to_string(),
                    all_paths(),
                ));
            }
        };
        let apply = SystemGitCli
            .run(&self.toplevel, &["apply", "--whitespace=nowarn", patch_str])
            .await;
        let _ = tokio::fs::remove_file(&patch_file).await;
        match apply {
            Ok(out) if out.success => Ok(changes),
            // `git apply` verifies every hunk's preimage before writing
            // anything, so a rejection means the real tree changed under us
            // (user edits mid-run) — and NOTHING was applied.
            Ok(out) => Err(fail(
                format!("`git apply` rejected the patch: {}", out.stderr.trim()),
                conflict_paths_from_stderr(&out.stderr, &changes),
            )),
            Err(e) => Err(fail(e.to_string(), all_paths())),
        }
    }
}

#[async_trait]
impl CandidateWorkspace for GitCandidateWorkspace {
    fn tools(&self) -> &dyn stella_core::ToolExecutor {
        &self.tools
    }

    fn commands(&self) -> &dyn CommandRunner {
        &self.commands
    }

    fn repo_status(&self) -> &dyn RepoStatusPort {
        &self.repo_status
    }

    async fn adopt(&self) -> Result<Vec<AdoptedChange>, WorkspaceError> {
        self.adopt_inner().await
    }

    async fn remove(&self) {
        cleanup(&self.toplevel, &self.dir).await;
    }
}

/// Parse `git diff --name-status --no-renames -z` output — `S\0path\0`
/// records with statuses A/M/D (renames disabled, so no two-path records).
fn parse_name_status(raw: &str) -> Vec<AdoptedChange> {
    let mut parts = raw.split('\0').filter(|s| !s.is_empty());
    let mut out = Vec::new();
    while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
        let kind = match status.chars().next() {
            Some('A') => FileChangeKind::Created,
            Some('D') => FileChangeKind::Deleted,
            _ => FileChangeKind::Modified,
        };
        out.push(AdoptedChange {
            path: path.to_string(),
            kind,
        });
    }
    out
}

/// The paths named in `git apply`'s stderr (`error: patch failed:
/// <path>:<line>`, `error: <path>: <why>`), deduped and sorted. Falls back
/// to every path in the patch when stderr names none — the adoption error
/// must always name paths.
fn conflict_paths_from_stderr(stderr: &str, changes: &[AdoptedChange]) -> Vec<String> {
    let mut paths: Vec<String> = stderr
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("error: ")?;
            if let Some(failed) = rest.strip_prefix("patch failed: ") {
                let path = failed.rsplit_once(':').map(|(p, _)| p).unwrap_or(failed);
                Some(path.to_string())
            } else {
                rest.split_once(':').map(|(p, _)| p.to_string())
            }
        })
        .collect();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        paths = changes.iter().map(|c| c.path.clone()).collect();
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `git <args>` in `root` with hook-exported `GIT_*` vars scrubbed
    /// (the verify_done test discipline — without it, running the suite from
    /// inside a git hook re-targets every command at the HOST repo) and
    /// return stdout. Panics on failure: these are test fixtures.
    fn scratch_git(root: &Path, args: &[&str]) -> String {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(root);
        for var in stella_tools::exec::GIT_REPO_ENV_VARS {
            cmd.env_remove(var);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Build the canonical dirty repo every test starts from:
    /// - committed: `tracked.txt` ("base\n"), `.gitignore` (ignores
    ///   `ignored.txt`)
    /// - a pre-existing stash entry (fixture only — the production code must
    ///   never touch it)
    /// - uncommitted tracked edit: `tracked.txt` = "base\ndirty\n"
    /// - staged-but-uncommitted new file: `staged.txt`
    /// - untracked: `untracked.txt`; ignored: `ignored.txt`
    fn scaffold(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "stella_cwstest_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t.t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
        ] {
            scratch_git(&root, args);
        }
        std::fs::write(root.join("tracked.txt"), "base\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        scratch_git(&root, &["add", "."]);
        scratch_git(&root, &["commit", "-q", "-m", "base"]);
        // The shared-stash fixture: other sessions' stashes must survive
        // best-of-N byte-identically.
        std::fs::write(root.join("tracked.txt"), "stash-fixture\n").unwrap();
        scratch_git(&root, &["stash", "push", "-q", "-m", "fixture"]);
        // The dirty state candidates must see.
        std::fs::write(root.join("tracked.txt"), "base\ndirty\n").unwrap();
        std::fs::write(root.join("staged.txt"), "staged\n").unwrap();
        scratch_git(&root, &["add", "staged.txt"]);
        std::fs::write(root.join("untracked.txt"), "hello\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "secret\n").unwrap();
        root
    }

    /// The observable state of the real repo that candidate isolation must
    /// leave byte-identical: worktree status, staged diff, stash list, HEAD.
    fn tree_state(root: &Path) -> (String, String, String, String) {
        (
            scratch_git(root, &["status", "--porcelain"]),
            scratch_git(root, &["diff", "--cached"]),
            scratch_git(root, &["stash", "list"]),
            scratch_git(root, &["rev-parse", "HEAD"]),
        )
    }

    fn assert_no_candidate_worktrees(root: &Path) {
        let listing = scratch_git(root, &["worktree", "list", "--porcelain"]);
        assert!(
            !listing.contains("stella_candidate_"),
            "candidate worktrees must not stay registered: {listing}"
        );
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[tokio::test]
    async fn snapshot_mirrors_dirty_staged_and_untracked_state_without_touching_the_real_tree() {
        let root = scaffold("snap");
        let before = tree_state(&root);
        let port = GitCandidateWorkspaces::new(root.clone(), RegistryOptions::default());

        let ws = port.create_workspace().await.unwrap();
        // Uncommitted tracked edit, staged-but-uncommitted new file, and the
        // untracked file are all visible; the ignored file is not.
        assert_eq!(read(&ws.dir().join("tracked.txt")), "base\ndirty\n");
        assert_eq!(read(&ws.dir().join("staged.txt")), "staged\n");
        assert_eq!(read(&ws.dir().join("untracked.txt")), "hello\n");
        assert!(
            !ws.dir().join("ignored.txt").exists(),
            "ignored files are not part of the snapshot"
        );

        // Fingerprint parity: the snapshot reports the overlay untracked file
        // with the REAL tree's fingerprint (len+mtime preserved), so the
        // witness tamper watchlist keeps working inside candidates.
        let real = GitRepoStatus { root: root.clone() }
            .untracked_fingerprints()
            .await;
        let snap = ws.repo_status().untracked_fingerprints().await;
        assert_eq!(
            snap.get("untracked.txt"),
            real.get("untracked.txt"),
            "overlay fingerprints must match the real tree's"
        );

        // The real tree, index, stash, and HEAD are untouched by creation.
        assert_eq!(tree_state(&root), before);

        ws.remove().await;
        assert_no_candidate_worktrees(&root);
        assert!(!ws.dir().exists(), "the shadow directory is removed");
        assert_eq!(
            tree_state(&root),
            before,
            "removal is also side-effect free"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn winner_adoption_lands_only_the_winners_changes() {
        let root = scaffold("adopt");
        let port = GitCandidateWorkspaces::new(root.clone(), RegistryOptions::default());
        let loser = port.create_workspace().await.unwrap();
        let winner = port.create_workspace().await.unwrap();

        // The loser edits a tracked file and creates a new one.
        std::fs::write(loser.dir().join("tracked.txt"), "base\ndirty\nloser\n").unwrap();
        std::fs::write(loser.dir().join("loser.txt"), "residue\n").unwrap();
        // The winner edits the tracked file, creates a file, and deletes the
        // pre-existing untracked file.
        std::fs::write(winner.dir().join("tracked.txt"), "base\ndirty\nwinner\n").unwrap();
        std::fs::write(winner.dir().join("winner.txt"), "new\n").unwrap();
        std::fs::remove_file(winner.dir().join("untracked.txt")).unwrap();

        let (_, before_cached, before_stash, before_head) = tree_state(&root);
        loser.remove().await;

        let mut adopted = winner.adopt().await.unwrap();
        adopted.sort_by(|a, b| a.path.cmp(&b.path));
        let described: Vec<(String, FileChangeKind)> =
            adopted.into_iter().map(|c| (c.path, c.kind)).collect();
        assert_eq!(
            described,
            vec![
                ("tracked.txt".to_string(), FileChangeKind::Modified),
                ("untracked.txt".to_string(), FileChangeKind::Deleted),
                ("winner.txt".to_string(), FileChangeKind::Created),
            ]
        );

        // Winner's changes landed; loser's never touched the real tree.
        assert_eq!(read(&root.join("tracked.txt")), "base\ndirty\nwinner\n");
        assert_eq!(read(&root.join("winner.txt")), "new\n");
        assert!(!root.join("untracked.txt").exists());
        assert!(!root.join("loser.txt").exists());

        // Index, stash, and HEAD are byte-identical: adoption writes only
        // the working tree.
        let (_, after_cached, after_stash, after_head) = tree_state(&root);
        assert_eq!(after_cached, before_cached, "the index is never touched");
        assert_eq!(after_stash, before_stash, "the stash is never touched");
        assert_eq!(after_head, before_head, "HEAD never moves");

        winner.remove().await;
        assert_no_candidate_worktrees(&root);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_mid_run_user_edit_fails_adoption_atomically_naming_the_path() {
        let root = scaffold("conflict");
        let port = GitCandidateWorkspaces::new(root.clone(), RegistryOptions::default());
        let ws = port.create_workspace().await.unwrap();

        std::fs::write(ws.dir().join("tracked.txt"), "base\ndirty\ncandidate\n").unwrap();
        std::fs::write(ws.dir().join("second.txt"), "must not land\n").unwrap();
        // The user edits the same file while the candidate runs.
        std::fs::write(root.join("tracked.txt"), "base\nuser-edit\n").unwrap();

        match ws.adopt().await.unwrap_err() {
            WorkspaceError::Adopt {
                paths, workspace, ..
            } => {
                assert!(
                    paths.iter().any(|p| p.contains("tracked.txt")),
                    "the conflict must name the path: {paths:?}"
                );
                assert_eq!(workspace, ws.dir().display().to_string());
            }
            other => panic!("expected an adoption conflict, got {other:?}"),
        }
        // Atomicity: NOTHING was applied — not even the conflict-free file.
        assert_eq!(read(&root.join("tracked.txt")), "base\nuser-edit\n");
        assert!(
            !root.join("second.txt").exists(),
            "a rejected adoption must not half-apply"
        );
        // The workspace is preserved for recovery until removed explicitly.
        assert!(ws.dir().exists());

        ws.remove().await;
        assert_no_candidate_worktrees(&root);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_repo_without_commits_is_a_clean_snapshot_error() {
        let root = std::env::temp_dir().join(format!(
            "stella_cwstest_nocommit_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        scratch_git(&root, &["init", "-q"]);
        let port = GitCandidateWorkspaces::new(root.clone(), RegistryOptions::default());
        match port.create_workspace().await {
            Err(WorkspaceError::Snapshot { reason }) => {
                assert!(reason.contains("at least one commit"), "{reason}")
            }
            Err(other) => panic!("expected a snapshot error, got {other:?}"),
            Ok(_) => panic!("expected a snapshot error, got a workspace"),
        }
        assert_no_candidate_worktrees(&root);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn name_status_parsing_maps_added_modified_deleted() {
        let raw = "A\0new.txt\0M\0changed.txt\0D\0gone.txt\0";
        let parsed = parse_name_status(raw);
        assert_eq!(
            parsed,
            vec![
                AdoptedChange {
                    path: "new.txt".into(),
                    kind: FileChangeKind::Created
                },
                AdoptedChange {
                    path: "changed.txt".into(),
                    kind: FileChangeKind::Modified
                },
                AdoptedChange {
                    path: "gone.txt".into(),
                    kind: FileChangeKind::Deleted
                },
            ]
        );
    }

    #[test]
    fn conflict_paths_are_parsed_from_apply_stderr() {
        let stderr = "error: patch failed: src/a.rs:12\n\
                      error: src/a.rs: patch does not apply\n\
                      error: new.txt: already exists in working directory";
        let paths = conflict_paths_from_stderr(stderr, &[]);
        assert_eq!(paths, vec!["new.txt".to_string(), "src/a.rs".to_string()]);
        // Unparseable stderr falls back to naming every path in the patch.
        let fallback = conflict_paths_from_stderr(
            "fatal: unrecognized input",
            &[AdoptedChange {
                path: "x.rs".into(),
                kind: FileChangeKind::Modified,
            }],
        );
        assert_eq!(fallback, vec!["x.rs".to_string()]);
    }
}
