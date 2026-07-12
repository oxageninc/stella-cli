//! `stella-fleet` — the multi-agent fleet layer (`02-architecture.md` §2;
//! `03-plan.md` Phase 5 item 2).
//!
//! Four pieces, one seam:
//!
//! - [`plan`] — the **planner DAG**: [`Plan`]/[`Task`] with dependency edges,
//!   isolate-by-default, wave scheduling ([`Plan::ready_tasks`]), topological
//!   order, and cycle detection.
//! - [`git`] — **git-worktree isolation** over the [`GitCli`] port: a
//!   dedicated worktree per task, plus commit helpers that *always* use
//!   explicit pathspecs (a fleet worker can never sweep a sibling's staged
//!   files).
//! - [`ledger`] — the **commit ledger**: one embedded SQLite file recording
//!   every run, task, attempt, commit, lineage edge, and per-task USD spend.
//! - [`monitor`] — the **PR/CI monitor** over the [`GhCli`] port: live PR
//!   reconciliation and a capped-deferred-wait CI watcher (L-E4).
//!
//! and [`fleet`] — **THE dispatch seam** ([`Fleet::dispatch`], L-E9): the one
//! API subagent fan-out goes through, stamping lineage into the ledger and
//! metering child spend into the parent [`stella_core::BudgetGuard`].
//!
//! Design constraints (from the task and `02-architecture.md`): we shell out
//! to the `git`/`gh` binaries via `tokio::process` behind port traits rather
//! than linking libgit2 (heavy native build), every external interaction is a
//! port so every test runs against fakes, and there is no `unwrap`/`panic`
//! outside tests.
//!
//! [`GitCli`]: git::GitCli
//! [`GhCli`]: monitor::GhCli
//! [`Plan`]: plan::Plan
//! [`Task`]: plan::Task
//! [`Plan::ready_tasks`]: plan::Plan::ready_tasks
//! [`Fleet::dispatch`]: fleet::Fleet::dispatch

pub mod fleet;
pub mod git;
pub mod ledger;
pub mod monitor;
pub mod plan;

pub use fleet::{
    Fleet, FleetConfig, FleetError, FleetRunReport, FleetWorker, TaskHandle, WorkerOutcome,
};
pub use git::{
    GitCli, GitError, GitOutput, RemoveOutcome, SystemGitCli, Worktree, WorktreeEntry,
    WorktreeError, WorktreeManager,
};
pub use ledger::{
    AttemptFinish, AttemptId, AttemptStart, CommitRecord, Ledger, LedgerError, RunRecord,
};
pub use monitor::{
    CiConclusion, CiRun, CiRunStatus, CiSnapshot, CiWatchOutcome, GhCli, GhError, GhOutput,
    Monitor, MonitorError, Sleeper, SystemGhCli, TimeoutReason, TokioSleeper, WatchConfig,
    WatchPhase, commit_event, pr_event,
};
pub use plan::{Isolation, Plan, PlanError, Task, TaskId};
