//! Git-worktree isolation ("git-worktree isolation").
//!
//! We shell out to the `git` binary through the [`GitCli`] port rather than
//! linking libgit2/git2 — a deliberate design constraint: the native build is
//! heavy, and the TS fleet shelled out to `git` for the same reason and it
//! was the right call. Every command runs behind [`GitCli`] so the scheduling
//! and pathspec logic is exercised against fakes, while [`SystemGitCli`] is
//! the one place a real subprocess is spawned.
//!
//! [`WorktreeManager`] gives each isolated task its own `git worktree` on a
//! fresh branch, so a fleet worker can never see or clobber a sibling's
//! uncommitted files. Its commit helper **always** uses explicit pathspecs
//! (`git add -- <paths>`, never `git add -A`/`.`, never `git commit -a`) —
//! this codebase's hard-won convention: under a shared or contested tree a
//! blanket add sweeps another worker's staged files (the TS monorepo lost
//! work to exactly this; see the "shared-worktree staged sweep" lesson). A
//! pathspec-less commit is not expressible through this API.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::process::Command;

/// The port every git command goes through. The real implementation
/// ([`SystemGitCli`]) spawns `git`; tests inject fakes that record the
/// invocation and return canned output.
#[async_trait]
pub trait GitCli: Send + Sync {
    /// Run `git <args>` with `repo` as the repository directory (`git -C
    /// <repo> <args>` semantics). A non-zero git exit is reported *in* the
    /// returned [`GitOutput`] (`success == false`), not as `Err` — `Err` is
    /// reserved for a failure to spawn the process at all (e.g. `git` not on
    /// `PATH`), so callers distinguish "git ran and said no" from "git isn't
    /// here".
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<GitOutput, GitError>;
}

/// The result of one git invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    /// A successful invocation with the given stdout — test ergonomics.
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A failed invocation with an exit code and stderr — test ergonomics.
    pub fn failed(code: i32, stderr: impl Into<String>) -> Self {
        Self {
            success: false,
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// A failure to *run* git at all — never a non-zero exit (that's a
/// `success == false` [`GitOutput`]).
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("failed to spawn `git {command}`: {reason}")]
    Spawn { command: String, reason: String },
}

/// The production [`GitCli`]: spawns the real `git` binary via
/// `tokio::process`. Never inherits an interactive prompt
/// (`GIT_TERMINAL_PROMPT=0`) so a credential-needing command fails fast
/// instead of hanging a fleet worker.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGitCli;

#[async_trait]
impl GitCli for SystemGitCli {
    async fn run(&self, repo: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).args(args);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        // The repo is ALWAYS the explicit `-C` argument — never whatever the
        // environment happens to point at. Inside a git hook (e.g. the
        // pre-push gate running `cargo test`) git exports GIT_DIR et al.,
        // which would silently override `-C` and aim every command here —
        // init, config, commit, worktree add — at the OUTER repo. That is
        // not hypothetical: it once rewrote the host repo's user identity
        // and committed test fixtures onto a real branch.
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
        ] {
            cmd.env_remove(var);
        }
        cmd.kill_on_drop(true);
        let output = cmd.output().await.map_err(|e| GitError::Spawn {
            command: args.join(" "),
            reason: e.to_string(),
        })?;
        Ok(GitOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// An isolated worktree created for a task: its checkout directory, its
/// branch, and the base ref it was cut from (needed to decide, on removal,
/// whether the branch carries unmerged work worth keeping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
}

/// A worktree as reported by `git worktree list` — its branch may be absent
/// (a detached checkout) and its base is unknown from the listing alone, so
/// this is a lighter read-model than [`Worktree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// What [`WorktreeManager::remove`] did with the task's branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveOutcome {
    /// The branch was deleted because it carried no commits beyond its base
    /// (nothing to keep).
    pub branch_deleted: bool,
    /// The branch had commits beyond its base, so it was preserved (the
    /// worker's work is not thrown away on cleanup).
    pub branch_had_commits: bool,
}

/// Typed worktree/commit failures.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error(transparent)]
    Git(#[from] GitError),

    #[error("`git {command}` failed (exit {code:?}): {stderr}")]
    Command {
        command: String,
        code: Option<i32>,
        stderr: String,
    },

    /// A commit helper was called with no paths — a pathspec-less commit is
    /// forbidden by construction (see the module docs on why blanket adds are
    /// banned).
    #[error("refusing to commit with an empty pathspec — fleet commits must name explicit paths")]
    EmptyPathspec,

    #[error("path is not valid UTF-8 and cannot be passed to git: {path}")]
    NonUtf8Path { path: String },
}

fn ensure_ok(output: GitOutput, command: &str) -> Result<GitOutput, WorktreeError> {
    if output.success {
        Ok(output)
    } else {
        Err(WorktreeError::Command {
            command: command.to_string(),
            code: output.code,
            stderr: output.stderr,
        })
    }
}

fn path_arg(path: &Path) -> Result<&str, WorktreeError> {
    path.to_str().ok_or_else(|| WorktreeError::NonUtf8Path {
        path: path.display().to_string(),
    })
}

/// Turn a task id into a filesystem/branch-safe slug: keep alphanumerics and
/// `. _ -`, replace everything else with `-`, trim leading/trailing `-`/`.`
/// (git refs may not start with them), and never yield empty.
fn slugify(task_id: &str) -> String {
    let mapped: String = task_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches(['-', '.']);
    if trimmed.is_empty() {
        "task".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A stable, deterministic hash of `s` as 16 lowercase hex chars (the full
/// 64-bit digest).
///
/// [`slugify`] is lossy — `fix/the thing`, `fix the thing`, and `fix-the-thing`
/// all map to `fix-the-thing`. `Plan::validate` guarantees unique task *ids*
/// but not unique *slugs*, so two distinct tasks could otherwise collide on the
/// same worktree directory and branch. Appending this hash of the full
/// `run_scope`+task id makes the slug **collision-resistant** (a 64-bit digest,
/// not truncated to 32 bits as before — the birthday bound at 16 hex chars is
/// far past any realistic per-run task count). FNV-1a (not the stdlib's
/// randomizable `Hasher`) so the result is identical across processes and Rust
/// versions — a later wave must recompute the exact branch name for the same
/// scope+task id.
fn short_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The worktree directory + branch slug for `task_id` within `run_scope`: a
/// human-readable [`slugify`] stem plus a [`short_hash`] suffix that keeps it
/// collision-resistant even when two task ids slugify to the same stem. The run
/// scope enters the hash so re-running a plan with the same task ids (`t1`,
/// `t2`, … — every positional invocation) gets fresh directories/branches
/// instead of colliding with last run's kept-for-review worktrees.
fn worktree_slug(run_scope: &str, task_id: &str) -> String {
    format!(
        "{}-{}",
        slugify(task_id),
        short_hash(&format!("{run_scope}\u{1f}{task_id}"))
    )
}

/// Manages the lifecycle of isolated task worktrees over a [`GitCli`].
pub struct WorktreeManager<G: GitCli> {
    git: G,
    repo_root: PathBuf,
    worktrees_root: PathBuf,
    branch_prefix: String,
    run_scope: String,
}

impl<G: GitCli> WorktreeManager<G> {
    /// A manager rooted at `repo_root`, placing worktrees under
    /// `<repo_root>/.stella/worktrees/` (alongside
    /// the fleet ledger) with branches named `fleet/<task-slug>`.
    pub fn new(git: G, repo_root: impl Into<PathBuf>) -> Self {
        let repo_root = repo_root.into();
        let worktrees_root = repo_root.join(".stella").join("worktrees");
        Self {
            git,
            repo_root,
            worktrees_root,
            branch_prefix: "fleet/".to_string(),
            run_scope: String::new(),
        }
    }

    /// Scope worktree/branch names to one run (builder style): the scope is
    /// hashed into every slug, so two runs sharing task ids never collide on
    /// a directory or branch left behind by the earlier run.
    pub fn with_run_scope(mut self, scope: impl Into<String>) -> Self {
        self.run_scope = scope.into();
        self
    }

    /// The repository root — the workspace a [`Isolation::SharedTree`] task
    /// runs in directly.
    ///
    /// [`Isolation::SharedTree`]: crate::plan::Isolation::SharedTree
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Create an isolated worktree for `task_id`, branched from `base_ref`:
    /// `git worktree add <dir> -b fleet/<slug> <base_ref>`. The slug is a
    /// human-readable stem plus a stable hash suffix so two task ids that
    /// slugify to the same stem never collide on the same directory/branch.
    pub async fn create(&self, task_id: &str, base_ref: &str) -> Result<Worktree, WorktreeError> {
        let slug = worktree_slug(&self.run_scope, task_id);
        let dir = self.worktrees_root.join(&slug);
        let branch = format!("{}{slug}", self.branch_prefix);
        let dir_str = path_arg(&dir)?;
        let out = self
            .git
            .run(
                &self.repo_root,
                &["worktree", "add", dir_str, "-b", &branch, base_ref],
            )
            .await?;
        ensure_ok(
            out,
            &format!("worktree add {dir_str} -b {branch} {base_ref}"),
        )?;
        Ok(Worktree {
            path: dir,
            branch,
            base_ref: base_ref.to_string(),
        })
    }

    /// Remove a worktree (`git worktree remove`), then clean up its branch
    /// **only if unchanged** — a branch with commits beyond its base is
    /// preserved so a worker's committed work is never discarded on cleanup.
    /// Fails (does not force) if the worktree has uncommitted changes.
    pub async fn remove(&self, worktree: &Worktree) -> Result<RemoveOutcome, WorktreeError> {
        let path_str = path_arg(&worktree.path)?;
        let out = self
            .git
            .run(&self.repo_root, &["worktree", "remove", path_str])
            .await?;
        ensure_ok(out, &format!("worktree remove {path_str}"))?;

        let has_commits = self
            .branch_has_commits_beyond_base(&worktree.branch, &worktree.base_ref)
            .await?;
        let branch_deleted = if has_commits {
            false
        } else {
            // Branch is at base with no new commits → safe to delete. `-D`
            // (not `-d`) because "merged" is judged by us via rev-list, not
            // by git's HEAD-relative check.
            let out = self
                .git
                .run(&self.repo_root, &["branch", "-D", &worktree.branch])
                .await?;
            // Surface a failed delete (e.g. the branch is checked out
            // elsewhere) rather than silently reporting a clean no-delete,
            // which would hide a leaked branch and the real git error.
            ensure_ok(out, &format!("branch -D {}", worktree.branch))?;
            true
        };
        Ok(RemoveOutcome {
            branch_deleted,
            branch_had_commits: has_commits,
        })
    }

    /// List the repository's worktrees (`git worktree list --porcelain`).
    pub async fn list(&self) -> Result<Vec<WorktreeEntry>, WorktreeError> {
        let out = self
            .git
            .run(&self.repo_root, &["worktree", "list", "--porcelain"])
            .await?;
        let out = ensure_ok(out, "worktree list --porcelain")?;
        Ok(parse_worktree_list(&out.stdout))
    }

    /// Stage exactly `paths` and commit them — **always** an explicit
    /// pathspec (`git add -- <paths>`, then `git commit -m <message> --
    /// <paths>`), never `-a` and never a pathspec-less commit that would sweep
    /// the whole index. Returns the new commit sha. Rejects an empty pathspec
    /// ([`WorktreeError::EmptyPathspec`]).
    pub async fn commit_paths(
        &self,
        repo: &Path,
        paths: &[&Path],
        message: &str,
    ) -> Result<String, WorktreeError> {
        if paths.is_empty() {
            return Err(WorktreeError::EmptyPathspec);
        }

        // `git add -- <path>...`: the `--` makes every following token an
        // explicit pathspec (a path that looks like a flag can't be
        // misread), and there is no `-A`/`.` anywhere near it.
        let mut add_args: Vec<&str> = vec!["add", "--"];
        for path in paths {
            add_args.push(path_arg(path)?);
        }
        let out = self.git.run(repo, &add_args).await?;
        ensure_ok(out, "add -- <paths>")?;

        // Scope the commit to the SAME pathspec. `git commit -m <msg>` with no
        // pathspec commits the whole index — which in a shared-tree fleet would
        // sweep another worker's concurrently-staged files (or a leftover
        // staged file from a prior failed call) into this commit, the exact
        // "staged sweep" this API exists to prevent. A pathspec-limited commit
        // ignores anything else in the index.
        let mut commit_args: Vec<&str> = vec!["commit", "-m", message, "--"];
        for path in paths {
            commit_args.push(path_arg(path)?);
        }
        let out = self.git.run(repo, &commit_args).await?;
        ensure_ok(out, "commit -m <message> -- <paths>")?;

        let out = self.git.run(repo, &["rev-parse", "HEAD"]).await?;
        let out = ensure_ok(out, "rev-parse HEAD")?;
        Ok(out.stdout.trim().to_string())
    }

    /// Porcelain working-tree status of `repo` (`git status --porcelain`) —
    /// used to prove isolation (a file staged in worktree A never appears in
    /// worktree B's status).
    pub async fn status(&self, repo: &Path) -> Result<String, WorktreeError> {
        let out = self.git.run(repo, &["status", "--porcelain"]).await?;
        let out = ensure_ok(out, "status --porcelain")?;
        Ok(out.stdout)
    }

    /// Whether `branch` has any commit beyond `base_ref` (`git rev-list
    /// --count base..branch > 0`). On any error resolving the range, err
    /// conservative — report `true` so the branch is kept, never silently
    /// deleted.
    async fn branch_has_commits_beyond_base(
        &self,
        branch: &str,
        base_ref: &str,
    ) -> Result<bool, WorktreeError> {
        let range = format!("{base_ref}..{branch}");
        let out = self
            .git
            .run(&self.repo_root, &["rev-list", "--count", &range])
            .await?;
        if !out.success {
            return Ok(true);
        }
        let count: u64 = out.stdout.trim().parse().unwrap_or(1);
        Ok(count > 0)
    }
}

/// Parse `git worktree list --porcelain` into entries. Records are
/// `worktree <path>` / `HEAD <sha>` / (`branch refs/heads/<name>` |
/// `detached`), separated by blank lines; we key off the `worktree` line and
/// pick up the branch if present.
fn parse_worktree_list(stdout: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(prev) = path.take() {
                entries.push(WorktreeEntry {
                    path: prev,
                    branch: branch.take(),
                });
            }
            path = Some(PathBuf::from(rest.trim()));
            branch = None;
        } else if let Some(rest) = line.strip_prefix("branch ") {
            let name = rest
                .trim()
                .strip_prefix("refs/heads/")
                .unwrap_or(rest.trim());
            branch = Some(name.to_string());
        }
    }
    if let Some(prev) = path.take() {
        entries.push(WorktreeEntry { path: prev, branch });
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // ---- Fake GitCli --------------------------------------------------

    type GitHandler = Box<dyn Fn(&[String]) -> GitOutput + Send + Sync>;

    /// A [`GitCli`] that records every invocation and answers via a handler
    /// closure — the seam that lets us assert on the exact git arguments
    /// (esp. the pathspec discipline) without a real repo.
    struct ScriptedGit {
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        handler: GitHandler,
    }

    impl ScriptedGit {
        fn new(handler: impl Fn(&[String]) -> GitOutput + Send + Sync + 'static) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                handler: Box::new(handler),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl GitCli for ScriptedGit {
        async fn run(&self, _repo: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(owned.clone());
            Ok((self.handler)(&owned))
        }
    }

    fn manager(git: ScriptedGit) -> WorktreeManager<ScriptedGit> {
        WorktreeManager::new(git, "/repo")
    }

    // ---- slugify ------------------------------------------------------

    #[test]
    fn slugify_makes_ids_ref_and_path_safe() {
        assert_eq!(slugify("refactor-auth"), "refactor-auth");
        assert_eq!(slugify("fix/the thing!"), "fix-the-thing");
        assert_eq!(slugify("--weird--"), "weird");
        assert_eq!(slugify("///"), "task");
        assert_eq!(slugify("t0"), "t0");
    }

    // ---- create -------------------------------------------------------

    #[tokio::test]
    async fn create_issues_worktree_add_with_branch_and_base() {
        let git = ScriptedGit::new(|_| GitOutput::ok(""));
        let mgr = manager(git);
        let wt = mgr.create("task-1", "HEAD").await.unwrap();
        // The branch/dir carry a stable hash suffix (collision-free slug), so
        // assert the readable stem as a prefix rather than an exact match.
        assert!(wt.branch.starts_with("fleet/task-1-"), "{}", wt.branch);
        assert_eq!(wt.base_ref, "HEAD");
        assert!(
            wt.path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("task-1-")
        );

        let calls = mgr.git.calls();
        assert_eq!(calls.len(), 1);
        let add = &calls[0];
        assert_eq!(add[0], "worktree");
        assert_eq!(add[1], "add");
        assert!(add.contains(&"-b".to_string()));
        assert!(add.iter().any(|a| a.starts_with("fleet/task-1-")));
        assert!(add.contains(&"HEAD".to_string()));
    }

    #[test]
    fn worktree_slug_is_injective_for_ids_that_share_a_stem() {
        // `slugify` alone maps these three distinct task ids to the same stem;
        // the hash suffix must keep the full slugs distinct so their worktrees
        // and branches never collide.
        let a = worktree_slug("run", "fix/the thing");
        let b = worktree_slug("run", "fix the thing");
        let c = worktree_slug("run", "fix-the-thing");
        assert!(a.starts_with("fix-the-thing-"));
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        // Deterministic across calls.
        assert_eq!(a, worktree_slug("run", "fix/the thing"));
    }

    #[test]
    fn worktree_slug_differs_across_runs_for_the_same_task_id() {
        // Re-running a plan reuses task ids (`t1`, `t2`, …) and the previous
        // run's worktrees/branches are kept for review — the run scope must
        // make the second run's slugs fresh, or `git worktree add` fails the
        // whole run with "already exists".
        let first = worktree_slug("fleet-1", "t1");
        let second = worktree_slug("fleet-2", "t1");
        assert_ne!(first, second);
        assert!(first.starts_with("t1-") && second.starts_with("t1-"));
    }

    #[tokio::test]
    async fn create_surfaces_a_git_failure_as_a_named_error() {
        let git = ScriptedGit::new(|_| GitOutput::failed(128, "fatal: already exists"));
        let mgr = manager(git);
        match mgr.create("t", "HEAD").await.unwrap_err() {
            WorktreeError::Command { stderr, .. } => assert!(stderr.contains("already exists")),
            other => panic!("expected a command error, got {other:?}"),
        }
    }

    // ---- commit_paths: the pathspec discipline ------------------------

    #[tokio::test]
    async fn commit_paths_always_uses_explicit_pathspecs_never_add_all() {
        let git = ScriptedGit::new(|args| {
            if args.first().map(String::as_str) == Some("rev-parse") {
                GitOutput::ok("abc123\n")
            } else {
                GitOutput::ok("")
            }
        });
        let mgr = manager(git);
        let sha = mgr
            .commit_paths(
                Path::new("/repo"),
                &[Path::new("src/a.rs"), Path::new("src/b.rs")],
                "feat: x",
            )
            .await
            .unwrap();
        assert_eq!(sha, "abc123");

        let calls = mgr.git.calls();
        // add -- src/a.rs src/b.rs
        let add = &calls[0];
        assert_eq!(add[0], "add");
        assert_eq!(add[1], "--");
        assert!(add.contains(&"src/a.rs".to_string()));
        assert!(add.contains(&"src/b.rs".to_string()));
        // The banned blanket-add forms must never appear anywhere.
        for call in &calls {
            assert!(
                !call.contains(&"-A".to_string()),
                "must never `git add -A`: {call:?}"
            );
            assert!(
                !call.contains(&"--all".to_string()),
                "must never `git add --all`: {call:?}"
            );
            assert!(
                !call.iter().any(|a| a == "."),
                "must never `git add .`: {call:?}"
            );
        }
        // commit must never carry -a (which would sweep tracked changes).
        let commit = calls
            .iter()
            .find(|c| c.first().map(String::as_str) == Some("commit"))
            .unwrap();
        assert!(
            !commit.iter().any(|a| a == "-a" || a == "--all"),
            "commit must never use -a: {commit:?}"
        );
        // And the commit must be scoped to the SAME explicit pathspec, so a
        // stray staged file elsewhere in the index is never swept in.
        assert!(
            commit.contains(&"--".to_string()),
            "commit must carry an explicit pathspec separator: {commit:?}"
        );
        assert!(commit.contains(&"src/a.rs".to_string()), "{commit:?}");
        assert!(commit.contains(&"src/b.rs".to_string()), "{commit:?}");
    }

    #[tokio::test]
    async fn commit_paths_rejects_an_empty_pathspec() {
        let git = ScriptedGit::new(|_| GitOutput::ok(""));
        let mgr = manager(git);
        assert!(matches!(
            mgr.commit_paths(Path::new("/repo"), &[], "msg")
                .await
                .unwrap_err(),
            WorktreeError::EmptyPathspec
        ));
        // Nothing was ever handed to git.
        assert!(mgr.git.calls().is_empty());
    }

    // ---- remove: branch cleanup only on unchanged ---------------------

    #[tokio::test]
    async fn remove_deletes_the_branch_when_it_has_no_commits_beyond_base() {
        let git = ScriptedGit::new(|args| match args.first().map(String::as_str) {
            Some("rev-list") => GitOutput::ok("0\n"), // unchanged
            _ => GitOutput::ok(""),
        });
        let mgr = manager(git);
        let wt = Worktree {
            path: PathBuf::from("/repo/.stella/worktrees/t"),
            branch: "fleet/t".into(),
            base_ref: "HEAD".into(),
        };
        let outcome = mgr.remove(&wt).await.unwrap();
        assert!(outcome.branch_deleted);
        assert!(!outcome.branch_had_commits);
        let calls = mgr.git.calls();
        assert!(
            calls
                .iter()
                .any(|c| c[0] == "branch" && c.contains(&"-D".to_string()))
        );
    }

    #[tokio::test]
    async fn remove_preserves_a_branch_that_has_commits() {
        let git = ScriptedGit::new(|args| match args.first().map(String::as_str) {
            Some("rev-list") => GitOutput::ok("3\n"), // three commits beyond base
            _ => GitOutput::ok(""),
        });
        let mgr = manager(git);
        let wt = Worktree {
            path: PathBuf::from("/repo/.stella/worktrees/t"),
            branch: "fleet/t".into(),
            base_ref: "HEAD".into(),
        };
        let outcome = mgr.remove(&wt).await.unwrap();
        assert!(!outcome.branch_deleted);
        assert!(outcome.branch_had_commits);
        // Never issued a branch delete.
        assert!(!mgr.git.calls().iter().any(|c| c[0] == "branch"));
    }

    // ---- list parsing -------------------------------------------------

    #[test]
    fn parse_worktree_list_reads_paths_and_branches() {
        let porcelain = "\
worktree /repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/.stella/worktrees/t1
HEAD 2222222222222222222222222222222222222222
branch refs/heads/fleet/t1

worktree /repo/.stella/worktrees/detached
HEAD 3333333333333333333333333333333333333333
detached
";
        let entries = parse_worktree_list(porcelain);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].path, PathBuf::from("/repo/.stella/worktrees/t1"));
        assert_eq!(entries[1].branch.as_deref(), Some("fleet/t1"));
        assert_eq!(entries[2].branch, None, "a detached worktree has no branch");
    }

    // ---- Real-git integration: isolation ------------------------------

    /// Whether a real `git` is on PATH; the integration tests skip cleanly
    /// (with a printed note) when it isn't — CI has it.
    async fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Init a throwaway repo in a tempdir with a seed commit; return its base
    /// commit sha. Sets a local identity so commits work in a pristine env.
    async fn seed_repo(dir: &Path) -> String {
        let git = SystemGitCli;
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "fleet@test.local"],
            vec!["config", "user.name", "Fleet Test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            let out = git.run(dir, &args).await.unwrap();
            assert!(out.success, "git {args:?} failed: {}", out.stderr);
        }
        std::fs::write(dir.join("README.md"), "seed\n").unwrap();
        let out = git.run(dir, &["add", "--", "README.md"]).await.unwrap();
        assert!(out.success, "add failed: {}", out.stderr);
        let out = git.run(dir, &["commit", "-q", "-m", "seed"]).await.unwrap();
        assert!(out.success, "commit failed: {}", out.stderr);
        let out = git.run(dir, &["rev-parse", "HEAD"]).await.unwrap();
        assert!(out.success);
        out.stdout.trim().to_string()
    }

    #[tokio::test]
    async fn real_git_worktrees_are_isolated_from_each_other() {
        if !git_available().await {
            eprintln!("skipping real-git isolation test: `git` not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let base = seed_repo(repo).await;

        let mgr = WorktreeManager::new(SystemGitCli, repo);
        let wt_a = mgr.create("task-a", &base).await.unwrap();
        let wt_b = mgr.create("task-b", &base).await.unwrap();
        assert_ne!(wt_a.path, wt_b.path);
        assert_ne!(wt_a.branch, wt_b.branch);

        // Write and commit a file ONLY in worktree A.
        std::fs::write(wt_a.path.join("only_in_a.txt"), "hello from A\n").unwrap();
        let sha = mgr
            .commit_paths(&wt_a.path, &[Path::new("only_in_a.txt")], "add A file")
            .await
            .unwrap();
        assert!(!sha.is_empty());

        // Worktree B never sees A's file: not on disk, and clean status.
        assert!(
            !wt_b.path.join("only_in_a.txt").exists(),
            "A's file must not appear in B's checkout"
        );
        let status_b = mgr.status(&wt_b.path).await.unwrap();
        assert!(
            !status_b.contains("only_in_a.txt"),
            "A's change must not appear in B's status: {status_b:?}"
        );
        // And A's own status is clean after committing (the commit landed).
        let status_a = mgr.status(&wt_a.path).await.unwrap();
        assert!(
            status_a.trim().is_empty(),
            "A should be clean: {status_a:?}"
        );

        // list sees the base repo plus both task worktrees.
        let listed = mgr.list().await.unwrap();
        let branches: Vec<String> = listed.iter().filter_map(|e| e.branch.clone()).collect();
        assert!(
            branches.iter().any(|b| b.starts_with("fleet/task-a-")),
            "{branches:?}"
        );
        assert!(
            branches.iter().any(|b| b.starts_with("fleet/task-b-")),
            "{branches:?}"
        );
    }

    #[tokio::test]
    async fn real_git_remove_cleans_an_unchanged_branch_but_keeps_a_committed_one() {
        if !git_available().await {
            eprintln!("skipping real-git remove test: `git` not available");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let base = seed_repo(repo).await;
        let mgr = WorktreeManager::new(SystemGitCli, repo);

        // An untouched worktree → branch deleted on removal.
        let clean = mgr.create("clean", &base).await.unwrap();
        let outcome = mgr.remove(&clean).await.unwrap();
        assert!(outcome.branch_deleted, "an unchanged branch is cleaned up");

        // A worktree with a commit → branch preserved on removal.
        let worked = mgr.create("worked", &base).await.unwrap();
        std::fs::write(worked.path.join("f.txt"), "x\n").unwrap();
        mgr.commit_paths(&worked.path, &[Path::new("f.txt")], "work")
            .await
            .unwrap();
        let outcome = mgr.remove(&worked).await.unwrap();
        assert!(!outcome.branch_deleted, "a committed branch is preserved");
        assert!(outcome.branch_had_commits);
    }
}
