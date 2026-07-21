//! Tool-registry options and workspace port adapters.
//!
//! `registry_options` is the single translation point from settings to
//! `RegistryOptions` — every session driver builds its registry through it,
//! so no path can quietly re-enable the shell. The rest are the pipeline's
//! filesystem/VCS/command ports.

use super::*;
use stella_pipeline::{
    ArtifactIdentity, ArtifactKind, DiagnosticInvocation, DiagnosticRunner, TestInvocation,
    TestRunner,
};

/// Repo-structure summary via `git ls-files` for the planner's split context.
pub(crate) struct GitRepoStructure {
    pub(crate) root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl RepoStructurePort for GitRepoStructure {
    async fn structure_summary(&self) -> String {
        let mut cmd = tokio::process::Command::new("git");
        stella_tools::exec::scrub_sensitive_env(&mut cmd);
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
/// pipeline's diagnostic runner (whose output is truncated), this captures the
/// COMPLETE `git ls-files --others` listing and fingerprints each file itself
/// (in-process, with real filesystem access), so a large untracked set is not
/// silently clipped and a modification to an already-untracked file is
/// detectable (its complete content hash changes).
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
        stella_tools::exec::scrub_sensitive_env(&mut cmd);
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

    async fn tracked_fingerprints(&self) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        let mut cmd = tokio::process::Command::new("git");
        stella_tools::exec::scrub_sensitive_env(&mut cmd);
        cmd.args([
            "-c",
            "core.quotePath=false",
            "diff",
            "--name-only",
            "--relative",
            "-z",
            "HEAD",
            "--",
        ])
        .current_dir(&self.root);
        for var in stella_tools::exec::GIT_REPO_ENV_VARS {
            cmd.env_remove(var);
        }
        let Ok(listing) = cmd.output().await else {
            return out;
        };
        if !listing.status.success() {
            return out;
        }
        for rel in String::from_utf8_lossy(&listing.stdout)
            .split('\0')
            .filter(|p| !p.is_empty())
        {
            let fingerprint =
                fs_fingerprint(&self.root.join(rel)).unwrap_or_else(|| "deleted".to_string());
            out.insert(rel.to_string(), fingerprint);
        }
        out
    }

    async fn artifact_identity(&self, path: &str) -> Option<stella_pipeline::ArtifactIdentity> {
        fs_artifact_identity(&self.root.join(path))
    }
}

/// The pipeline's file fingerprint: SHA-256 over the complete bytes. Content
/// hashes are required at the witness authority boundary: size+mtime can be
/// restored after a same-length edit and would incorrectly credit a tampered
/// witness. One definition is shared with candidate snapshots.
pub(crate) fn fs_fingerprint(path: &std::path::Path) -> Option<String> {
    fs_artifact_identity(path).map(|identity| identity.fingerprint)
}

pub(crate) fn fs_artifact_identity(
    path: &std::path::Path,
) -> Option<stella_pipeline::ArtifactIdentity> {
    OpenedWitnessArtifact::open(path)?.identity_for_path(path)
}

struct OpenedWitnessArtifact {
    file: std::fs::File,
    metadata: std::fs::Metadata,
}

impl OpenedWitnessArtifact {
    fn open(path: &std::path::Path) -> Option<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_OPEN_REPARSE_POINT opens a link/reparse point itself
            // instead of following it. Link count is unavailable through a
            // stable std API, so Windows still fails closed below.
            options.custom_flags(0x0020_0000);
        }
        let file = options.open(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.file_type().is_file() || opened_metadata(&metadata).is_none() {
            return None;
        }
        Some(Self { file, metadata })
    }

    fn identity_for_path(mut self, path: &std::path::Path) -> Option<ArtifactIdentity> {
        use std::fmt::Write as _;
        use std::io::Read as _;

        use sha2::{Digest, Sha256};

        let (mode, link_count) = opened_metadata(&self.metadata)?;
        if link_count != 1 || !path_resolves_to_opened_file(path, &self.metadata) {
            return None;
        }
        let mut payload = Vec::new();
        self.file.read_to_end(&mut payload).ok()?;
        let final_metadata = self.file.metadata().ok()?;
        if opened_metadata(&final_metadata) != Some((mode, link_count))
            || !path_resolves_to_opened_file(path, &final_metadata)
        {
            return None;
        }
        let mut hasher = Sha256::new();
        hasher.update(b"regular");
        hasher.update(mode.to_le_bytes());
        hasher.update(link_count.to_le_bytes());
        hasher.update(payload);
        let mut fingerprint = String::from("sha256:");
        for byte in hasher.finalize() {
            write!(&mut fingerprint, "{byte:02x}").ok()?;
        }
        Some(ArtifactIdentity {
            fingerprint,
            kind: ArtifactKind::Regular,
            mode,
            link_count,
        })
    }
}

#[cfg(unix)]
fn opened_metadata(metadata: &std::fs::Metadata) -> Option<(u32, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.mode(), metadata.nlink()))
}

#[cfg(not(unix))]
fn opened_metadata(_metadata: &std::fs::Metadata) -> Option<(u32, u64)> {
    // Stable Rust does not expose a by-handle link count on Windows. Never
    // manufacture `1`: without proof that no hardlink aliases exist, witness
    // identity is unavailable and acceptance fails closed.
    None
}

#[cfg(unix)]
fn path_resolves_to_opened_file(path: &std::path::Path, opened: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(current) = std::fs::symlink_metadata(path) else {
        return false;
    };
    current.file_type().is_file() && current.dev() == opened.dev() && current.ino() == opened.ino()
}

#[cfg(not(unix))]
fn path_resolves_to_opened_file(_path: &std::path::Path, _opened: &std::fs::Metadata) -> bool {
    false
}

/// The workspace-rooted pipeline ports every session driver constructs the
/// same way — repo structure/status, the verification command runner, and
/// best-of-N candidate isolation, all rooted at the same tree. One bundle
/// and one constructor so the four drivers (one-shot, goal loop, deck,
/// fleet worker) can never drift apart on the wiring.
pub(crate) struct WorkspacePorts {
    pub(crate) repo_structure: GitRepoStructure,
    pub(crate) repo_status: GitRepoStatus,
    pub(crate) diagnostic_runner: GitDiagnosticRunner,
    pub(crate) test_runner: TypedTestRunner,
    /// Used for best-of-N and for candidate-local authored witnesses at N=1.
    pub(crate) candidate_workspaces: crate::candidate_ws::GitCandidateWorkspaces,
}

/// Build the [`WorkspacePorts`] bundle rooted at `root` (the session
/// workspace, or a fleet worker's own worktree).
pub(crate) fn workspace_ports(
    root: std::path::PathBuf,
    cfg: &Config,
    registry_options: stella_tools::RegistryOptions,
    active_rules: crate::rules::ResolvedRules,
) -> Result<WorkspacePorts, String> {
    crate::enterprise_telemetry::authorize_execution_surface(
        crate::enterprise_telemetry::ExecutionSurface::WorkspacePorts,
    )?;
    crate::enterprise_telemetry::authorize_execution_surface(
        crate::enterprise_telemetry::ExecutionSurface::CandidateWorkspace,
    )?;
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
    Ok(WorkspacePorts {
        repo_structure: GitRepoStructure { root: root.clone() },
        repo_status: GitRepoStatus { root: root.clone() },
        diagnostic_runner: GitDiagnosticRunner { root: root.clone() },
        test_runner: TypedTestRunner { root: root.clone() },
        candidate_workspaces: crate::candidate_ws::GitCandidateWorkspaces::new(
            root,
            registry_options,
            custom_tools,
            active_rules,
        ),
    })
}

/// Workspace-rooted closed Git diagnostics. Every variant maps to fixed argv;
/// paths remain literal arguments and no shell is involved.
pub(crate) struct GitDiagnosticRunner {
    pub(crate) root: std::path::PathBuf,
}

/// Workspace-rooted typed test runner. It passes an enumerable argv directly
/// to the OS and never invokes a shell.
pub(crate) struct TypedTestRunner {
    pub(crate) root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl TestRunner for TypedTestRunner {
    async fn run_test(&self, invocation: &TestInvocation) -> CmdOutcome {
        run_command(test_process(invocation, &self.root)).await
    }
}

fn test_process(invocation: &TestInvocation, root: &std::path::Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&invocation.program);
    stella_tools::exec::scrub_sensitive_env(&mut cmd);
    cmd.args(&invocation.args)
        .current_dir(root)
        .env("PWD", root);
    for var in stella_tools::exec::GIT_REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    stella_tools::exec::scrub_sensitive_env(&mut cmd);
    cmd
}

#[async_trait::async_trait]
impl DiagnosticRunner for GitDiagnosticRunner {
    async fn run_diagnostic(&self, invocation: &DiagnosticInvocation) -> CmdOutcome {
        let mut cmd = tokio::process::Command::new("git");
        stella_tools::exec::scrub_sensitive_env(&mut cmd);
        match invocation {
            DiagnosticInvocation::GitDiff => {
                cmd.args(["diff"]);
            }
            DiagnosticInvocation::UntrackedNumstat { path } => {
                cmd.args(["diff", "--no-index", "--numstat", "--", "/dev/null", path]);
            }
        }
        cmd.current_dir(&self.root).env("PWD", &self.root);
        for var in stella_tools::exec::GIT_REPO_ENV_VARS {
            cmd.env_remove(var);
        }
        run_command(cmd).await
    }
}

async fn run_command(mut cmd: tokio::process::Command) -> CmdOutcome {
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
    let process_free = crate::enterprise_telemetry::process_free_authority_active();
    let media_operation_journal = host_media_operation_journal(&cfg.workspace_root);
    stella_tools::RegistryOptions {
        bash: cfg.tools_bash && !process_free,
        web: cfg.tools_web && !process_free,
        media_requires_host_approval: cfg.authority.media_requires_host_approval,
        media_operation_journal,
        media_host_data_isolation: process_free
            .then_some(stella_tools::media::HostDataIsolation::ProcessFree),
        ..Default::default()
    }
}

fn host_media_operation_journal(
    workspace_root: &std::path::Path,
) -> Option<Arc<dyn stella_media::MediaOperationJournal>> {
    let workspace_root = workspace_root.canonicalize().ok()?;
    let data_dir = std::path::absolute(stella_store::usage::data_dir()).ok()?;
    if data_dir.starts_with(&workspace_root) {
        return None;
    }
    stella_media::SqliteMediaOperationJournal::open_outside(
        data_dir.join("media-operations.db"),
        workspace_root,
        Default::default(),
    )
    .ok()
    .map(|journal| Arc::new(journal) as Arc<dyn stella_media::MediaOperationJournal>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use stella_media::{
        CostDecision, ImageRequest, MediaArtifact, MediaCapabilities, MediaError, MediaJob,
        MediaJobStatus, MediaKind, MediaProvider, MediaSpendGate, MediaSpendRequest, VideoRequest,
    };

    #[test]
    fn witness_fingerprint_hashes_complete_bytes_not_size_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("witness.rs");
        std::fs::write(&path, b"aaaa").unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let before = fs_fingerprint(&path).unwrap();

        std::fs::write(&path, b"bbbb").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let after = fs_fingerprint(&path).unwrap();

        assert_ne!(
            before, after,
            "same-length, same-mtime edits must be detected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn witness_identity_rejects_symlinks_hardlinks_and_hashes_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("witness.rs");
        let hardlink = dir.path().join("hardlink.rs");
        let symlink = dir.path().join("symlink.rs");
        std::fs::write(&file, "test bytes\n").unwrap();

        let before = fs_artifact_identity(&file).unwrap();
        assert_eq!(before.kind, ArtifactKind::Regular);
        assert!(before.is_regular_single_link());

        std::fs::hard_link(&file, &hardlink).unwrap();
        assert!(
            fs_artifact_identity(&file).is_none(),
            "multi-link files fail closed at the identity boundary"
        );

        std::os::unix::fs::symlink(&file, &symlink).unwrap();
        assert!(
            fs_artifact_identity(&symlink).is_none(),
            "no-follow identity must never open a symlink target"
        );

        std::fs::remove_file(&hardlink).unwrap();
        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&file, permissions).unwrap();
        let executable = fs_artifact_identity(&file).unwrap();
        assert_ne!(before.fingerprint, executable.fingerprint);
    }

    #[cfg(unix)]
    #[test]
    fn opened_witness_identity_rejects_path_retarget_before_fingerprinting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("witness.rs");
        let moved = dir.path().join("original.rs");
        std::fs::write(&path, "original bytes\n").unwrap();
        let opened = OpenedWitnessArtifact::open(&path).expect("regular file opens no-follow");

        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, "replacement bytes\n").unwrap();

        assert!(
            opened.identity_for_path(&path).is_none(),
            "the opened handle must not be credited after its path is retargeted"
        );
    }

    #[test]
    fn witness_identity_requires_established_platform_link_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("witness.rs");
        std::fs::write(&path, "test bytes\n").unwrap();
        let identity = fs_artifact_identity(&path);
        #[cfg(unix)]
        assert!(
            identity.is_some(),
            "Unix exposes link count from the handle"
        );
        #[cfg(not(unix))]
        assert!(
            identity.is_none(),
            "platforms without a stable handle link count fail closed"
        );
    }

    #[tokio::test]
    async fn repo_status_hashes_tracked_working_tree_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let mut command = std::process::Command::new("git");
            stella_tools::exec::scrub_sensitive_std_env(&mut command);
            let output = command.args(args).current_dir(dir.path()).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("src.rs"), "before\n").unwrap();
        git(&["add", "src.rs"]);
        git(&[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-q",
            "-m",
            "base",
        ]);
        std::fs::write(dir.path().join("src.rs"), "after\n").unwrap();

        let files = GitRepoStatus {
            root: dir.path().to_path_buf(),
        }
        .tracked_fingerprints()
        .await;
        assert_eq!(
            files.get("src.rs"),
            fs_fingerprint(&dir.path().join("src.rs")).as_ref()
        );
    }

    #[tokio::test]
    async fn typed_test_runner_never_interprets_redirection_in_an_argument() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("must-not-exist");
        let runner = TypedTestRunner {
            root: dir.path().to_path_buf(),
        };
        let outcome = runner
            .run_test(&stella_pipeline::TestInvocation {
                program: "printf".into(),
                args: vec![format!("owned > {}", target.display())],
            })
            .await;

        assert!(outcome.passed());
        assert!(
            !target.exists(),
            "argv content must never become shell syntax"
        );
    }

    #[tokio::test]
    async fn typed_test_runner_binds_candidate_pwd_and_scrubs_git_repo_pointers() {
        let dir = tempfile::tempdir().unwrap();
        let invocation = TestInvocation {
            program: "sh".into(),
            args: vec!["-c".into(), "printf '%s' \"$PWD\"".into()],
        };
        let command = test_process(&invocation, dir.path());
        let configured_env: std::collections::HashMap<_, _> = command.as_std().get_envs().collect();
        for var in stella_tools::exec::GIT_REPO_ENV_VARS {
            assert_eq!(configured_env.get(std::ffi::OsStr::new(var)), Some(&None));
        }
        let runner = TypedTestRunner {
            root: dir.path().to_path_buf(),
        };
        let outcome = runner.run_test(&invocation).await;

        assert!(outcome.passed(), "{}", outcome.stderr_tail);
        let expected = dir.path().canonicalize().unwrap();
        assert_eq!(
            std::path::Path::new(&outcome.stdout_tail)
                .canonicalize()
                .unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn diagnostic_runner_passes_untracked_paths_as_literal_git_argv() {
        let dir = tempfile::tempdir().unwrap();
        let odd = "odd;touch owned.txt";
        std::fs::write(dir.path().join(odd), "one\ntwo\n").unwrap();
        let runner = GitDiagnosticRunner {
            root: dir.path().to_path_buf(),
        };

        let outcome = runner
            .run_diagnostic(&DiagnosticInvocation::UntrackedNumstat {
                path: odd.to_string(),
            })
            .await;

        assert!(outcome.stdout_tail.contains(odd), "{}", outcome.stdout_tail);
        assert!(!dir.path().join("owned.txt").exists());
    }

    struct FixedOperationId(&'static str);

    impl stella_tools::media::MediaOperationIdSource for FixedOperationId {
        fn operation_id(&self) -> stella_tools::media::HostMediaOperation {
            stella_tools::media::HostMediaOperation {
                opaque_id: self.0.to_string(),
                expires_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 3600,
            }
        }
    }

    struct CountingGate(AtomicUsize);

    #[async_trait::async_trait]
    impl MediaSpendGate for CountingGate {
        async fn authorize(&self, _request: &MediaSpendRequest) -> CostDecision {
            self.0.fetch_add(1, Ordering::SeqCst);
            CostDecision::Approve
        }
    }

    #[test]
    fn host_journal_rejects_workspace_paths_symlinks_and_dot_fallback() {
        let _env = crate::test_env::lock();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let original_dir = std::env::current_dir().unwrap();
        let saved: Vec<_> = ["STELLA_DATA_DIR", "HOME", "XDG_DATA_HOME", "APPDATA"]
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect();

        unsafe { std::env::set_var("STELLA_DATA_DIR", workspace.join(".stella")) };
        assert!(host_media_operation_journal(&workspace).is_none());

        #[cfg(unix)]
        {
            let link = outside.join("linked-data");
            std::os::unix::fs::symlink(&workspace, &link).unwrap();
            unsafe { std::env::set_var("STELLA_DATA_DIR", link) };
            assert!(host_media_operation_journal(&workspace).is_none());

            let ancestor_link = outside.join("linked-parent");
            std::os::unix::fs::symlink(&workspace, &ancestor_link).unwrap();
            let nested = ancestor_link.join("must-not-be-created");
            unsafe { std::env::set_var("STELLA_DATA_DIR", nested) };
            assert!(host_media_operation_journal(&workspace).is_none());
            assert!(
                !workspace.join("must-not-be-created").exists(),
                "workspace must not be mutated before the resolved containment check"
            );
        }

        std::env::set_current_dir(&workspace).unwrap();
        unsafe {
            for name in ["STELLA_DATA_DIR", "HOME", "XDG_DATA_HOME", "APPDATA"] {
                std::env::remove_var(name);
            }
        }
        assert!(host_media_operation_journal(&workspace).is_none());

        std::env::set_current_dir(original_dir).unwrap();
        unsafe {
            for (name, value) in saved {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    struct CountingImageProvider(AtomicUsize);

    #[async_trait::async_trait]
    impl MediaProvider for CountingImageProvider {
        fn id(&self) -> &str {
            "managed-test"
        }

        fn capabilities(&self) -> MediaCapabilities {
            MediaCapabilities {
                provider_id: self.id().into(),
                image: true,
                image_usd_each: Some(0.01),
                ..Default::default()
            }
        }

        async fn generate_image(&self, request: ImageRequest) -> Result<MediaArtifact, MediaError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(MediaArtifact {
                kind: MediaKind::Image,
                bytes: b"image".to_vec(),
                extension: "png".into(),
                label: request.label,
                model: "managed-test".into(),
                cost_usd: 0.01,
            })
        }

        async fn generate_video(&self, _request: VideoRequest) -> Result<MediaJob, MediaError> {
            Err(MediaError::Transport("not under test".into()))
        }

        async fn poll_video(&self, _job: &MediaJob) -> Result<MediaJobStatus, MediaError> {
            Err(MediaError::Transport("not under test".into()))
        }
    }

    fn load_managed_config(
        workspace: &std::path::Path,
        home: &std::path::Path,
        managed: &std::path::Path,
    ) -> (
        crate::settings::Settings,
        Config,
        stella_tools::RegistryOptions,
    ) {
        let _env = crate::test_env::lock();
        let original_dir = std::env::current_dir().unwrap();
        let original_home = std::env::var_os("HOME");
        let original_managed = std::env::var_os("STELLA_MANAGED_SETTINGS");
        let original_data = std::env::var_os("STELLA_DATA_DIR");
        let data_dir = home.join(format!(
            "data-{}",
            workspace.file_name().unwrap().to_string_lossy()
        ));
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("STELLA_MANAGED_SETTINGS", managed);
            std::env::set_var("STELLA_DATA_DIR", &data_dir);
        }
        std::env::set_current_dir(workspace).unwrap();
        let settings = crate::settings::Settings::load(workspace);
        let cfg = Config::load(
            Some("local/managed-test"),
            Some("test-key"),
            Some("http://localhost:11434/v1"),
        );
        let options = registry_options(cfg.as_ref().unwrap());
        std::env::set_current_dir(original_dir).unwrap();
        unsafe {
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match original_managed {
                Some(value) => std::env::set_var("STELLA_MANAGED_SETTINGS", value),
                None => std::env::remove_var("STELLA_MANAGED_SETTINGS"),
            }
            match original_data {
                Some(value) => std::env::set_var("STELLA_DATA_DIR", value),
                None => std::env::remove_var("STELLA_DATA_DIR"),
            }
        }
        assert!(data_dir.join("media-operations.db").exists());
        assert!(!data_dir.starts_with(workspace));
        (settings.unwrap(), cfg.unwrap(), options)
    }

    #[tokio::test]
    async fn managed_media_ceiling_flows_through_config_into_registry_enforcement() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        let mut results = Vec::new();
        for (name, authority_json, expected_allowed) in [
            (
                "off",
                r#"{"authority":{"media_requires_host_approval":"off"}}"#,
                false,
            ),
            (
                "on",
                r#"{"authority":{"media_requires_host_approval":"on"}}"#,
                true,
            ),
            ("absent", r#"{}"#, true),
        ] {
            let workspace = dir.path().join(name);
            let managed = dir.path().join(format!("managed-{name}.json"));
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::write(&managed, authority_json).unwrap();
            let (settings, cfg, mut options) = load_managed_config(&workspace, &home, &managed);
            let gate = Arc::new(CountingGate(AtomicUsize::new(0)));
            let provider = Arc::new(CountingImageProvider(AtomicUsize::new(0)));
            assert!(options.media_operation_journal.is_some());
            options.media_spend_gate = Some(gate.clone());
            options.media_operation_ids = Some(Arc::new(FixedOperationId("host-managed-test")));
            options.media_host_data_isolation =
                Some(stella_tools::media::HostDataIsolation::ProcessFree);
            let registry = stella_tools::ToolRegistry::with_backends_and_options(
                workspace,
                None,
                Some(stella_tools::media::MediaBackend {
                    image: provider.clone(),
                    video: None,
                }),
                options,
            );
            let output = registry
                .execute("generate_image", &serde_json::json!({"prompt": "test"}))
                .await;
            results.push((
                name,
                expected_allowed,
                settings.authority_policy.media_requires_host_approval,
                cfg.authority.media_requires_host_approval,
                gate.0.load(Ordering::SeqCst),
                provider.0.load(Ordering::SeqCst),
                output.is_error(),
            ));
        }

        for (name, allowed, settings_value, config_value, gate, provider, is_error) in results {
            assert_eq!(settings_value, allowed, "settings row {name}");
            assert_eq!(config_value, allowed, "config row {name}");
            let expected_calls = usize::from(allowed);
            assert_eq!(gate, expected_calls, "gate row {name}");
            assert_eq!(provider, expected_calls, "provider row {name}");
            assert_eq!(is_error, !allowed, "output row {name}");
        }
    }
}
