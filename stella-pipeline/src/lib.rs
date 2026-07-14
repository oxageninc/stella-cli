//! `stella-pipeline` — the orchestration plane that sits *above*
//! `stella-core::Engine` (`02-architecture.md` §5). It drives one prompt
//! through the staged turn flow — **evaluate → enhance → route → execute →
//! verify → judge → revise** — over injected ports, emitting an `AgentEvent`
//! at every stage boundary.
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
//!   heuristic fallback. [`verify`].
//! - **L-M4** — triage runs with `max_retries = 0` under a latency ceiling.
//!   [`pipeline::Pipeline::run`].
//!
//! # The port surface the CLI glue implements
//!
//! The pipeline does no I/O itself; it orchestrates over the traits in
//! [`ports`]: [`ProviderResolver`], [`ContextRecallPort`], [`RepoStructurePort`],
//! [`CommandRunner`], and [`ApprovalGate`] (plus `stella-core`'s `Router`,
//! `ToolExecutor`, and `Sleeper`). The `stella-cli` glue supplies the real
//! implementations; every one has a no-op/default here so the pipeline runs
//! before every subsystem is wired.
//!
//! [`ProviderResolver`]: ports::ProviderResolver
//! [`ContextRecallPort`]: ports::ContextRecallPort
//! [`RepoStructurePort`]: ports::RepoStructurePort
//! [`CommandRunner`]: ports::CommandRunner
//! [`ApprovalGate`]: ports::ApprovalGate

pub mod candidate;
pub mod pipeline;
pub mod plan;
pub mod ports;
pub mod replay;
pub mod scope;
pub mod triage;
pub mod verify;

pub use pipeline::{
    Pipeline, PipelineConfig, PipelineError, PipelineOutcome, PipelinePorts, PipelineStatus,
    Verdict,
};
pub use ports::{
    AlwaysAbortGate, ApprovalGate, AutoApproveGate, CmdOutcome, CommandRunner, ContextRecallPort,
    NoContextRecall, NoRepoStructure, ProviderResolver, RecalledFrame, RepoStructurePort,
    ScopeDecision, StdioApprovalGate,
};
pub use triage::TaskClass;
pub use verify::{FlipOracle, FlipState, LadderDecision, LadderInputs};
