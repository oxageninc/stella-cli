//! The pipeline's port boundary (`02-architecture.md` §1.1, §3). Like
//! `stella-core`, `stella-pipeline` never imports a provider SDK, a shell, a
//! context store, or a terminal — it orchestrates the staged turn flow
//! (`02-architecture.md` §5) entirely through the traits defined here. The
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
//! the deterministic verification substrate ([`CommandRunner`],
//! [`ApprovalGate`]). None of these belong inside the step-driver.

use async_trait::async_trait;
use stella_protocol::{ModelRef, Provider, ScopeProposal};

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

/// One frame recalled from the context plane at turn start (`02-architecture.md`
/// §5 "context recall"). A deliberately minimal local shape: the real
/// `stella-context` crate is being built in parallel and owns the rich
/// `ContextFrame`/retrieval types; the CLI glue adapts its frames down to
/// this at the seam so `stella-pipeline` takes **no** dependency on
/// `stella-context` (dependency direction discipline, `02-architecture.md`
/// §1). `citation_label` is mandatory and human-readable (L-C4); a
/// not-yet-materialized frame carries `id: None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecalledFrame {
    /// Human-readable citation, never a raw id (L-C4).
    pub citation_label: String,
    /// Which provider produced the frame (`"code-graph"`, `"memory"`,
    /// `"git-history"`, or an external OCP provider id).
    pub source: String,
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

/// The outcome of running one shell command through [`CommandRunner`]. Output
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

impl CmdOutcome {
    /// Whether the command succeeded (exit code 0). The single place the
    /// pipeline decides pass/fail for a command — never string-sniffing the
    /// output.
    pub fn passed(&self) -> bool {
        self.exit_code == 0
    }
}

/// Runs one shell command for the deterministic verification ladder (L-E11):
/// the flip oracle's test command (before and after execute) and the diff
/// command that measures what the turn touched. Injected so the ladder's
/// evidence gathering is testable with scripted outcomes and so
/// `stella-pipeline` never shells out itself (the real process-group /
/// timeout / signal handling lives in `stella-tools`, L-S1).
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `cmd` to completion and report its outcome. Implementations must
    /// never panic — a command that cannot even start reports a non-zero
    /// `exit_code` with the reason in `stderr_tail`, never an `Err`, so the
    /// ladder always has evidence to reason over.
    async fn run(&self, cmd: &str) -> CmdOutcome;
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
