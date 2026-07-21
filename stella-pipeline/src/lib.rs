//! `stella-pipeline` — the orchestration plane that sits *above*
//! `stella-core::Engine`. It drives one prompt
//! through the staged turn flow — **evaluate → enhance → route → witness →
//! execute → verify → judge → revise** — over injected ports, emitting an
//! `AgentEvent` at every stage boundary.
//!
//! # What lives here vs. the engine
//!
//! `stella-core::Engine::run_turn` drives *one* model-call/tool loop. This
//! crate composes turns into a governed pipeline: it classifies the prompt
//! (triage), recalls context, plans multi-step work, gates large plans behind
//! interactive scope review, executes each step through the engine, and
//! verifies the result with a deterministic-first ladder before ever spending
//! a model-judge call.
//!
//! # The design lessons this crate encodes
//!
//! - **L-E2** — triage fast paths: simple lookups skip planner + judge (with a
//!   self-revoking zero-diff guard); single-task goals skip DAG planning.
//!   [`triage`], and the fast-path wiring in [`pipeline`].
//! - **L-E5** — the scope-review gate. [`scope`].
//! - **L-E6** — the planner's split context (goal + recall + structure, never
//!   the transcript). [`plan::build_planner_prompt`].
//! - **L-E7** — single-shot default; best-of-N opt-in. [`candidate`].
//! - **L-E8** — recall rides as a volatile message after the stable system
//!   prefix (cache discipline). [`pipeline`].
//! - **L-E11** — the deterministic verification ladder: the flip-oracle state
//!   machine, the evidence ladder that skips the judge on strong evidence, and
//!   a model judge (judge ≠ worker) only on inconclusive evidence with a
//!   heuristic fallback. [`verify`]. Its front half is **witness authoring**:
//!   when the user armed no `--test-command`, an independent model (the
//!   judge's resolution) writes the failing witness test that the flip oracle
//!   tracks — visible to the worker, integrity-checked by tamper exclusion,
//!   never hidden. [`witness`].
//! - **L-M4** — triage runs with `max_retries = 0` under a latency ceiling.
//!   [`pipeline::Pipeline::run`].
//! - **Distress guidance** — on the second consecutive deterministic
//!   verification failure, one judge call steers the next revision
//!   (event-triggered course-correction, never a fixed mid-run checkpoint).
//!   [`verify::guidance_prompt`], wired in [`pipeline`].
//!
//! # The port surface the CLI glue implements
//!
//! The pipeline does no I/O itself; it orchestrates over the traits in
//! [`ports`]: [`ProviderResolver`], [`ContextRecallPort`], [`RepoStructurePort`],
//! [`TestRunner`], [`DiagnosticRunner`], [`ApprovalGate`], and [`CandidateWorkspacePort`]
//! (best-of-N candidate isolation) — plus `stella-core`'s `Router`,
//! `ToolExecutor`, and `Sleeper`. The `stella-cli` glue supplies the real
//! implementations; every one has a no-op/default here so the pipeline runs
//! before every subsystem is wired.
//!
//! [`ProviderResolver`]: ports::ProviderResolver
//! [`ContextRecallPort`]: ports::ContextRecallPort
//! [`RepoStructurePort`]: ports::RepoStructurePort
//! [`DiagnosticRunner`]: ports::DiagnosticRunner
//! [`TestRunner`]: ports::TestRunner
//! [`ApprovalGate`]: ports::ApprovalGate
//! [`CandidateWorkspacePort`]: ports::CandidateWorkspacePort

pub mod candidate;
pub mod pipeline;
pub mod plan;
pub mod ports;
pub mod replay;
pub mod scope;
pub mod triage;
pub mod verify;
pub mod witness;

pub use pipeline::{
    Pipeline, PipelineConfig, PipelineError, PipelineOutcome, PipelinePorts, PipelineRoleOverrides,
    PipelineRunError, PipelineStatus, RoleCallOverrides, Verdict,
};
pub use ports::{
    AdoptedChange, AlwaysAbortGate, ApprovalGate, ArtifactIdentity, ArtifactKind, AutoApproveGate,
    CandidateWorkspace, CandidateWorkspacePort, CmdOutcome, ContextRecallPort,
    DiagnosticInvocation, DiagnosticRunner, NoContextRecall, NoRepoStatus, NoRepoStructure,
    ProviderResolver, RecalledFrame, RepoStatusPort, RepoStructurePort, ScopeDecision,
    StdioApprovalGate, TestInvocation, TestRunner, WorkspaceError,
};
pub use triage::TaskClass;
pub use verify::{FlipOracle, FlipState, LadderDecision, LadderInputs};
pub use witness::{
    TestInvocationError, Witness, WitnessArtifactError, parse_test_invocation,
    parse_witness_command, tampered_paths, validate_witness_artifact, validate_witness_identity,
    validate_witness_invocation, witness_identity_matches, witness_watchlist,
};
