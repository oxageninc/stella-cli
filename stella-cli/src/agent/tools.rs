//! Tool-registry options and workspace port adapters.
//!
//! `registry_options` is the single translation point from settings to
//! `RegistryOptions` — every session driver builds its registry through it,
//! so no path can quietly re-enable the shell. The rest are the pipeline's
//! filesystem/VCS/command ports.

use super::*;

/// Repo-structure summary via `git ls-files` for the planner's split context.
pub(crate) struct GitRepoStructure {
    pub(crate) root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl RepoStructurePort for GitRepoStructure {
    async fn structure_summary(&self) -> String {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(["ls-files"]).current_dir(&self.root);
        // Hook-exported GIT_* vars must not re-target this at another repo.
        for var in stella_tools::exec::GIT_REPO_ENV_VARS {
            cmd.env_remove(var);
        }
        let output = cmd.output().await;
        match output {
            Ok(out) if out.status.success() => {
                render_file_tree(&String::from_utf8_lossy(&out.stdout), 200)
            }
            _ => String::new(),
        }
    }
}

/// Untracked-file fingerprints for the pipeline's zero-diff guard. Unlike the
/// pipeline's `CommandRunner` (whose output is truncated), this captures the
/// COMPLETE `git ls-files --others` listing and fingerprints each file itself
/// (in-process, with real filesystem access), so a large untracked set is not
/// silently clipped and a modification to an already-untracked file is
/// detectable (its `len:mtime` fingerprint changes).
pub(crate) struct GitRepoStatus {
    pub(crate) root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl RepoStatusPort for GitRepoStatus {
    async fn untracked_fingerprints(&self) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        // `-z` NUL-delimits paths (robust to spaces/newlines); quotePath off
        // keeps non-ASCII literal. Full stdout is read — never truncated.
        let mut cmd = tokio::process::Command::new("git");
        cmd.args([
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .current_dir(&self.root);
        // Hook-exported GIT_* vars must not re-target this at another repo.
        for var in stella_tools::exec::GIT_REPO_ENV_VARS {
            cmd.env_remove(var);
        }
        let output = cmd.output().await;
        let Ok(listing) = output else {
            return out;
        };
        if !listing.status.success() {
            return out; // not a git repo, or git unavailable
        }
        for rel in String::from_utf8_lossy(&listing.stdout)
            .split('\0')
            .filter(|p| !p.is_empty())
        {
            // Unreadable metadata → a sentinel so the file still registers
            // as present.
            let fingerprint =
                fs_fingerprint(&self.root.join(rel)).unwrap_or_else(|| "unreadable".to_string());
            out.insert(rel.to_string(), fingerprint);
        }
        out
    }
}

/// The pipeline's untracked-file fingerprint: `len:mtime_nanos` — changes
/// whenever the file is written, without reading (and hashing) potentially
/// large files on every snapshot. `None` when the file's metadata cannot be
/// read (absent or unreadable). One definition, shared with the best-of-N
/// candidate snapshot ports, so fingerprints from the real tree and a
/// snapshot are comparable (the witness tamper watchlist depends on it).
pub(crate) fn fs_fingerprint(path: &std::path::Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some(format!("{}:{mtime}", meta.len()))
}

/// The workspace-rooted pipeline ports every session driver constructs the
/// same way — repo structure/status, the verification command runner, and
/// best-of-N candidate isolation, all rooted at the same tree. One bundle
/// and one constructor so the four drivers (one-shot, goal loop, deck,
/// fleet worker) can never drift apart on the wiring.
pub(crate) struct WorkspacePorts {
    pub(crate) repo_structure: GitRepoStructure,
    pub(crate) repo_status: GitRepoStatus,
    pub(crate) command_runner: ShellCommandRunner,
    /// Inert unless `candidates > 1` — the pipeline never calls it on
    /// single-shot runs.
    pub(crate) candidate_workspaces: crate::candidate_ws::GitCandidateWorkspaces,
}

/// Build the [`WorkspacePorts`] bundle rooted at `root` (the session
/// workspace, or a fleet worker's own worktree).
pub(crate) fn workspace_ports(root: std::path::PathBuf, cfg: &Config) -> WorkspacePorts {
    // The candidate registry mirrors the session's custom tool surface —
    // discovered from the same root, so a candidate sees exactly the custom
    // tools the session does (re-rooted at its snapshot at create time).
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let custom_tools = stella_tools::custom::discover_in_scopes(
        &root,
        home.as_deref(),
        cfg.authority.project_custom_tools_allowed,
    )
    .tools;
    // Resolve policy against the real session workspace exactly once. Every
    // best-of-N shadow receives this immutable snapshot; ignored rule files
    // and store-backed guards therefore cannot disappear during git overlay.
    let active_rules = crate::rules::load_workspace_rules(&root, &cfg.authority);
    WorkspacePorts {
        repo_structure: GitRepoStructure { root: root.clone() },
        repo_status: GitRepoStatus { root: root.clone() },
        command_runner: ShellCommandRunner { root: root.clone() },
        candidate_workspaces: crate::candidate_ws::GitCandidateWorkspaces::new(
            root,
            registry_options(cfg),
            custom_tools,
            active_rules,
        ),
    }
}

/// Runs shell commands for the verification ladder (flip oracle tests, diff).
pub(crate) struct ShellCommandRunner {
    pub(crate) root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl CommandRunner for ShellCommandRunner {
    async fn run(&self, command: &str) -> CmdOutcome {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&self.root);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
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
                return CmdOutcome {
                    exit_code: -1,
                    stdout_tail: String::new(),
                    stderr_tail: format!("failed to spawn: {e}"),
                };
            }
        };
        #[cfg(unix)]
        let pid = child.id().unwrap_or(0) as i32;

        let timeout = Duration::from_secs(300);
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return CmdOutcome {
                    exit_code: -1,
                    stdout_tail: String::new(),
                    stderr_tail: format!("command failed: {e}"),
                };
            }
            Err(_) => {
                #[cfg(unix)]
                unsafe {
                    if pid > 0 {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                return CmdOutcome {
                    exit_code: -1,
                    stdout_tail: String::new(),
                    stderr_tail: format!("command timed out after {}s", timeout.as_secs()),
                };
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        CmdOutcome {
            exit_code: output.status.code().unwrap_or(-1),
            stdout_tail: truncate_tail(&stdout, 100_000),
            stderr_tail: truncate_tail(&stderr, 20_000),
        }
    }
}

fn truncate_tail(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let start = s.len() - max_bytes;
    let mut idx = start;
    while !s.is_char_boundary(idx) {
        idx += 1;
    }
    s[idx..].to_string()
}

/// Build the provider adapter from config. Consults the catalog first
/// (provider-scoped, since the same slug legitimately exists on several
/// providers — `gemini-3-pro` on both `gemini` and `vertex`) so an
/// unrecognized model slug is a hard, immediate, named error — never a
/// silent construction of a provider that will simply fail its first live
/// call (L-M1/L-M2). The one exemption is `local`:
/// a local server's models are whatever the user pulled into it — there is
/// no curated catalog to check against, and the anti-phantom-slug rule
/// exists to catch drift in OUR seed data, not to veto the user's own
/// endpoint.
///
/// Each wire dialect gets its own arm: OpenAI (Responses API), Anthropic
/// (Messages), Gemini direct + Vertex (generateContent), Bedrock (Converse,
/// SigV4). Everything else — Z.ai, xAI, DeepSeek, OpenRouter, local — is
/// genuinely the same Chat Completions shape behind different base URLs,
/// served by the shared adapter re-identified per provider so its
/// `Provider::id()` and error messages name the surface actually being
/// called (an xAI 401 must never read "Z.ai rejected the API key").
/// The registry feature switches for this session's config — the ONE
/// translation point from settings (`tools.bash`, default off) to
/// [`stella_tools::RegistryOptions`]. Every session driver (one-shot, goal,
/// interactive, deck, sub-session workers, fleet workers) builds its
/// registry through this, so no path can quietly re-enable the shell.
pub(crate) fn registry_options(cfg: &Config) -> stella_tools::RegistryOptions {
    stella_tools::RegistryOptions {
        bash: cfg.tools_bash,
        web: cfg.tools_web,
    }
}
