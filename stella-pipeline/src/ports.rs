//! The pipeline's port boundary. Like
//! `stella-core`, `stella-pipeline` never imports a provider SDK, a shell, a
//! context store, or a terminal — it orchestrates the staged turn flow
//! entirely through the traits defined here. The
//! `stella-cli` glue layer implements each of them against the real
//! subsystems (`stella-model`, `stella-tools`, `stella-context`, the TUI's
//! approval prompt); tests implement them with scripted doubles.
//!
//! Why these live here and not in `stella-core`: the engine (`stella-core`)
//! drives *one turn* through `Provider`/`ToolExecutor`. The pipeline sits
//! *above* the engine and needs three things the engine deliberately does
//! not: a way to pick which provider a role resolved to
//! ([`ProviderResolver`]), the surrounding context/repo material that shapes
//! triage and planning ([`ContextRecallPort`], [`RepoStructurePort`]), and
//! the deterministic verification substrate ([`TestRunner`], [`DiagnosticRunner`],
//! [`ApprovalGate`]). None of these belong inside the step-driver.

use async_trait::async_trait;
use stella_protocol::{FileChangeKind, ModelRef, Provider, ScopeProposal};

/// Maps a router-resolved [`ModelRef`] to the concrete provider adapter that
/// serves it. This is the one seam that connects `stella-core`'s
/// catalog-free [`stella_core::Router`] (which only ever produces *data* — a
/// `ModelRef`) to `stella-model`'s concrete adapters, without either crate
/// depending on the other. The CLI glue owns the adapter set and answers
/// this query; the pipeline never constructs an adapter.
///
/// Returning `&dyn Provider` (a borrow, not an owned box) keeps the adapter
/// set owned by the caller for the pipeline run's lifetime — exactly how
/// `stella-core::Engine` already borrows its `&dyn Provider`.
pub trait ProviderResolver: Send + Sync {
    /// The provider adapter that will serve `model`, or `None` if no
    /// configured adapter matches (a resolution the glue reports as a hard
    /// error — never a silent fallback; L-M1).
    fn provider_for(&self, model: &ModelRef) -> Option<&dyn Provider>;
}

/// One frame recalled from the context plane at turn start (
/// §5 "context recall"). A deliberately minimal local shape: the real
/// `stella-context` crate is being built in parallel and owns the rich
/// `ContextFrame`/retrieval types; the CLI glue adapts its frames down to
/// this at the seam so `stella-pipeline` takes **no** dependency on
/// `stella-context` (dependency direction discipline,
/// §1). `citation_label` is mandatory and human-readable (L-C4); a
/// not-yet-materialized frame carries `id: None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecalledFrame {
    /// Human-readable citation, never a raw id (L-C4).
    pub citation_label: String,
    /// The OCP provider leg that returned this frame.
    pub provider: String,
    /// The record's original source from its provenance chain. This can
    /// differ from `provider` when an adapter fronts another context store.
    pub source: String,
    /// Protocol frame kind (`symbol`, `memory`, `graph`, ...).
    pub kind: String,
    /// Canonical source URI, when declared.
    pub uri: Option<String>,
    /// Most-derived provenance method, when declared.
    pub method: Option<String>,
    /// The actual text injected into the volatile recall message.
    pub content: String,
    /// Token cost of this frame, for the `ContextRecall` event's budget
    /// report (L-C5).
    pub token_cost: u32,
    /// Present only when the frame is materialized in the store; a candidate
    /// frame carries `None` (L-C4).
    pub id: Option<String>,
}

/// Context recall at turn start (L-E8): a *live provider query*, never a
/// cached prompt block. The recalled frames ride as a volatile message
/// **after** the byte-stable system prefix so prompt-cache hits on that
/// prefix are preserved — the assembly discipline is documented and enforced
/// in [`crate::pipeline`]. A caller with no context plane wired yet supplies
/// [`NoContextRecall`].
#[async_trait]
pub trait ContextRecallPort: Send + Sync {
    /// Recall material relevant to `goal`. Returns an empty vec when nothing
    /// is relevant or no plane is configured — never an error the pipeline
    /// has to special-case (weak/absent context degrades to "no frames",
    /// L-C6).
    async fn recall(&self, goal: &str) -> Vec<RecalledFrame>;
}

/// A repository-structure summary for the planner's **split context** (L-E6):
/// the planner receives goal + recall + this structure summary, never the
/// accumulated transcript. Kept a plain string (a tree/outline the glue
/// renders) so the pipeline stays agnostic to how structure is computed
/// (`stella-graph` code index, a `git ls-files` outline, etc.). A caller
/// with nothing to offer supplies [`NoRepoStructure`].
#[async_trait]
pub trait RepoStructurePort: Send + Sync {
    /// A bounded, human-readable summary of the repo's structure for
    /// planning. Empty string is valid (plan from goal + recall alone).
    async fn structure_summary(&self) -> String;
}

/// A snapshot of the working tree's untracked (git-ignored-aware) files, each
/// with a content fingerprint. The verification ladder diffs a before-turn and
/// after-turn snapshot to find files the turn **created or modified**, since
/// `git diff` alone is blind to untracked files.
///
/// Distinct from [`DiagnosticRunner`] on purpose: the listing must be COMPLETE —
/// diagnostic output is middle-out truncated (L-S3), so a large untracked
/// set would lose files and corrupt the diff-size accounting — and it must
/// carry per-file fingerprints so a modification (not just a creation) to an
/// already-untracked file is visible. A caller without a git working tree
/// supplies [`NoRepoStatus`].
#[async_trait]
pub trait RepoStatusPort: Send + Sync {
    /// Untracked files as `path -> fingerprint`, where the fingerprint changes
    /// whenever the file's content does (a complete content hash). Complete and
    /// never truncated. Empty when the workspace is not a git repo or on any
    /// failure — the guard then rests on the tracked `git diff` alone.
    async fn untracked_fingerprints(&self) -> std::collections::HashMap<String, String>;

    /// Tracked paths currently changed from the workspace baseline, with a
    /// complete content fingerprint (or a deletion sentinel). Witness
    /// authoring compares before/after maps so existing production edits can
    /// never enter an accepted witness artifact.
    async fn tracked_fingerprints(&self) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }

    /// Filesystem identity for one repo-relative artifact, obtained without
    /// following symlinks. Witness acceptance requires a regular single-link
    /// file; callers without filesystem access return `None`.
    async fn artifact_identity(&self, _path: &str) -> Option<ArtifactIdentity> {
        None
    }
}

/// Filesystem object kind captured without following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Regular,
    Symlink,
    Other,
}

/// Complete witness artifact identity. Accepted identities are regular,
/// single-link files whose fingerprint commits to bytes, type, mode, and link
/// count from one no-follow file handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIdentity {
    pub fingerprint: String,
    pub kind: ArtifactKind,
    pub mode: u32,
    pub link_count: u64,
}

impl ArtifactIdentity {
    pub fn is_regular_single_link(&self) -> bool {
        self.kind == ArtifactKind::Regular && self.link_count == 1
    }
}

/// The outcome of running one local process through a pipeline runner. Output
/// is pre-truncated by the runner (middle-out, L-S3) into head+tail tails —
/// the pipeline never needs the full stream, only exit status and enough
/// text to summarize evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutcome {
    /// Process exit code. `0` conventionally means success; any non-zero is
    /// a failure. A signal-killed process reports a conventional 128+n here
    /// (the runner's responsibility, L-L1).
    pub exit_code: i32,
    /// Truncated stdout tail (middle-out elision applied by the runner).
    pub stdout_tail: String,
    /// Truncated stderr tail.
    pub stderr_tail: String,
}

/// One test process invocation after the untrusted command text has crossed
/// the pipeline's strict parser. `program` and `args` are passed directly to a
/// process builder; neither field is ever interpreted by a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestInvocation {
    /// A known test runner executable (for example `cargo` or `pytest`).
    pub program: String,
    /// The exact argument vector supplied to the test runner.
    pub args: Vec<String>,
}

impl CmdOutcome {
    /// Whether the command succeeded (exit code 0). The single place the
    /// pipeline decides pass/fail for a command — never string-sniffing the
    /// output.
    pub fn passed(&self) -> bool {
        self.exit_code == 0
    }
}

/// Closed diagnostic vocabulary. Every variant maps to fixed executable argv;
/// no caller-provided shell string crosses this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticInvocation {
    GitDiff,
    UntrackedNumstat { path: String },
}

#[async_trait]
pub trait DiagnosticRunner: Send + Sync {
    /// Run one fixed diagnostic invocation directly, without a shell.
    async fn run_diagnostic(&self, invocation: &DiagnosticInvocation) -> CmdOutcome;
}

/// Runs only already-validated [`TestInvocation`] values. Kept separate from
/// [`DiagnosticRunner`] so model-authored test text can never retarget the fixed
/// Git diagnostic vocabulary.
#[async_trait]
pub trait TestRunner: Send + Sync {
    /// Spawn `invocation.program` with `invocation.args` directly.
    async fn run_test(&self, invocation: &TestInvocation) -> CmdOutcome;
}

/// One file the winning best-of-N candidate changed, as applied to the real
/// tree by [`CandidateWorkspace::adopt`]. Paths are repo-relative; the kind
/// feeds the `FileChange` events the pipeline emits for adopted work (the
/// session's own file tracking never saw the winner's edits — they happened
/// inside the snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedChange {
    pub path: String,
    pub kind: FileChangeKind,
}

/// A typed candidate-isolation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    /// The current tree state could not be snapshotted (not a git repository,
    /// no commits yet, git unavailable, worktree creation failed). The
    /// pipeline scores the affected candidate as aborted — it never falls
    /// back to running that candidate in the shared tree.
    #[error("could not snapshot the working tree: {reason}")]
    Snapshot { reason: String },
    /// Candidate state could not be committed into the immutable tree that
    /// final verification and adoption must share.
    #[error("could not seal candidate workspace `{workspace}`: {reason}")]
    Seal { reason: String, workspace: String },
    /// Applying the winning candidate's changes to the real tree failed —
    /// typically because the user edited the same files mid-run. Adoption is
    /// all-or-nothing: NOTHING was applied, `paths` names the conflicts, and
    /// the candidate workspace is preserved at `workspace` so the winning
    /// work is recoverable by hand.
    #[error(
        "adopting the winning candidate's changes failed ({reason}); conflicting paths: {}; \
         nothing was applied — the candidate's work is preserved at `{workspace}`",
        paths.join(", ")
    )]
    Adopt {
        reason: String,
        paths: Vec<String>,
        workspace: String,
    },
}

/// One isolated best-of-N candidate workspace: a snapshot of the working
/// tree's *current* state (HEAD plus uncommitted and untracked files) that a
/// candidate executes and verifies inside, so sibling candidates never see
/// each other's edits and losers leave no residue in the real tree. The
/// bundled ports are rooted at the snapshot — the pipeline threads them (not
/// the session ports) through the candidate's engine turns and verification
/// runs.
#[async_trait]
pub trait CandidateWorkspace: Send + Sync {
    /// Absolute workspace root used to bind engine hook payloads and hook
    /// process execution to this candidate rather than the session tree.
    fn root(&self) -> &str;
    /// The tool executor rooted at this workspace: every engine-turn tool
    /// call (read/edit/shell) lands in the snapshot, never in the real tree.
    fn tools(&self) -> &dyn stella_core::ToolExecutor;
    /// Capability-minimal witness executor, constructed with the candidate:
    /// candidate-root reads plus one atomic, create-only witness test action.
    /// It must never expose general writes, edits, processes, hooks, MCP,
    /// custom tools, credentials, or external adapters.
    fn witness_tools(&self) -> &dyn stella_core::ToolExecutor;
    /// Runs closed diagnostic invocations inside the snapshot.
    fn diagnostics(&self) -> &dyn DiagnosticRunner;
    /// Typed test-process runner rooted at this workspace.
    fn tests(&self) -> &dyn TestRunner;
    /// Untracked-file fingerprints of the snapshot, mirroring the real
    /// tree's semantics (the tamper watchlist and zero-diff guard must keep
    /// working unchanged inside a candidate).
    fn repo_status(&self) -> &dyn RepoStatusPort;
    /// Commit the current candidate bytes into its private immutable history
    /// immediately before a final verification observation.
    async fn seal(&self) -> Result<(), WorkspaceError>;
    /// Whether the live worktree and HEAD still exactly match the last seal.
    async fn sealed_is_unchanged(&self) -> Result<bool, WorkspaceError>;
    /// Apply this workspace's changes — relative to its starting snapshot —
    /// to the real tree. All-or-nothing: on conflict the real tree is left
    /// byte-identical and the error names the conflicting paths
    /// ([`WorkspaceError::Adopt`]); the user's index and stash are never
    /// touched on any path.
    async fn adopt(&self) -> Result<Vec<AdoptedChange>, WorkspaceError>;
    /// Remove the workspace. Best-effort and infallible — the cleanup
    /// discipline is the pipeline's (every workspace is removed on every
    /// path, except a winner whose adoption failed, which is preserved for
    /// recovery).
    async fn remove(&self);
}

/// Candidate isolation (L-E7): snapshots the current working-tree state into
/// one isolated [`CandidateWorkspace`] per candidate. Best-of-N uses it when
/// available; authored witnesses require it even for one candidate.
#[async_trait]
pub trait CandidateWorkspacePort: Send + Sync {
    /// Snapshot the current tree state into a fresh isolated workspace.
    async fn create(&self) -> Result<Box<dyn CandidateWorkspace>, WorkspaceError>;
}

/// A human's decision at the scope-review gate (L-E5). `Trim` carries the
/// indices (into the proposed plan) the user chose to keep, so the pipeline
/// executes a reduced plan rather than the whole thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeDecision {
    /// Execute the plan as proposed.
    Approve,
    /// Execute only the steps at these indices (into the proposed step list),
    /// in the given order. Out-of-range indices are ignored by the pipeline.
    Trim { keep_steps: Vec<usize> },
    /// Abandon the turn cleanly — no execution happens.
    Abort,
}

/// The interactive scope-review gate (L-E5). Above configured thresholds the
/// pipeline emits a `ScopeReview` event and blocks on this port for the
/// user's [`ScopeDecision`]. Headless runs supply [`AutoApproveGate`] (with
/// an explicit config bypass) or [`AlwaysAbortGate`]; a headless run that
/// hits the gate with **no** bypass configured is a named error, never a
/// silent auto-approve (see [`crate::PipelineConfig`]).
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    /// Present `proposal` and await the user's decision.
    async fn review(&self, proposal: &ScopeProposal) -> ScopeDecision;
}

// ---------------------------------------------------------------------
// No-op / default port implementations
// ---------------------------------------------------------------------
//
// These let a caller run the pipeline before every subsystem is wired (the
// whole point of the port boundary) and give tests trivial doubles for the
// ports they aren't exercising.

/// A [`ContextRecallPort`] that recalls nothing — the correct default before
/// `stella-context` is wired, and for tasks where context grounding is off.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoContextRecall;

#[async_trait]
impl ContextRecallPort for NoContextRecall {
    async fn recall(&self, _goal: &str) -> Vec<RecalledFrame> {
        Vec::new()
    }
}

/// A [`RepoStructurePort`] that offers no structure — the planner falls back
/// to goal + recall alone.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRepoStructure;

#[async_trait]
impl RepoStructurePort for NoRepoStructure {
    async fn structure_summary(&self) -> String {
        String::new()
    }
}

/// A [`RepoStatusPort`] that reports no untracked files — for callers with no
/// git working tree (the zero-diff guard then uses the tracked diff alone).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRepoStatus;

#[async_trait]
impl RepoStatusPort for NoRepoStatus {
    async fn untracked_fingerprints(&self) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }
}

/// An [`ApprovalGate`] that approves every proposal without prompting — for
/// headless runs that have *explicitly* opted into bypassing scope review
/// (`PipelineConfig::headless_bypass_scope_review`). Never the default: the
/// pipeline requires the bypass flag before it will even construct the gate,
/// so a headless run can't silently auto-approve a large plan.
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoApproveGate;

#[async_trait]
impl ApprovalGate for AutoApproveGate {
    async fn review(&self, _proposal: &ScopeProposal) -> ScopeDecision {
        ScopeDecision::Approve
    }
}

/// An [`ApprovalGate`] that aborts every proposal — a conservative headless
/// default (never runs large work unattended).
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysAbortGate;

#[async_trait]
impl ApprovalGate for AlwaysAbortGate {
    async fn review(&self, _proposal: &ScopeProposal) -> ScopeDecision {
        ScopeDecision::Abort
    }
}

/// An [`ApprovalGate`] that prints the proposal to stdout and reads a
/// y/n/trim response from stdin. For interactive text mode only — headless
/// runs use [`AutoApproveGate`] or [`AlwaysAbortGate`].
pub struct StdioApprovalGate;

#[async_trait]
impl ApprovalGate for StdioApprovalGate {
    async fn review(&self, proposal: &ScopeProposal) -> ScopeDecision {
        use std::io::{self, BufRead, Write};

        println!();
        println!("  ┌─ Scope Review ──────────────────────────────");
        println!("  │ {}", proposal.summary);
        for (i, step) in proposal.steps.iter().enumerate() {
            println!("  │   {}. {}", i + 1, step);
        }
        if let Some(cost) = proposal.estimated_cost_usd {
            println!("  │ est. cost: ${cost:.4}");
        }
        println!("  └──────────────────────────────────────────────");
        print!("  Approve? [y/N/a=bort]: ");
        let _ = io::stdout().flush();

        let stdin = io::stdin();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).is_err() {
            return ScopeDecision::Abort;
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => ScopeDecision::Approve,
            _ => ScopeDecision::Abort,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_outcome_passed_is_exit_zero_only() {
        assert!(
            CmdOutcome {
                exit_code: 0,
                stdout_tail: String::new(),
                stderr_tail: String::new(),
            }
            .passed()
        );
        for code in [1, 2, 127, 130, -1] {
            assert!(
                !CmdOutcome {
                    exit_code: code,
                    stdout_tail: String::new(),
                    stderr_tail: String::new(),
                }
                .passed(),
                "exit {code} must not be treated as passing"
            );
        }
    }

    #[tokio::test]
    async fn no_op_ports_are_inert() {
        assert!(NoContextRecall.recall("anything").await.is_empty());
        assert!(NoRepoStructure.structure_summary().await.is_empty());
        let proposal = ScopeProposal {
            summary: "x".into(),
            steps: vec!["a".into()],
            estimated_files: 1,
            estimated_cost_usd: None,
        };
        assert_eq!(
            AutoApproveGate.review(&proposal).await,
            ScopeDecision::Approve
        );
        assert_eq!(
            AlwaysAbortGate.review(&proposal).await,
            ScopeDecision::Abort
        );
    }
}
