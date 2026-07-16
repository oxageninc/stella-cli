//! THE dispatch seam (L-E9). Subagent fan-out goes through exactly one
//! API — [`Fleet::dispatch`] — that claims the task's declared paths
//! (cooperative file locks in the workspace [`Store`], held for the
//! attempt's duration), allocates the task's workspace (a git worktree per
//! [`Isolation`], isolate-by-default), records the attempt in the
//! [`Ledger`], invokes the [`FleetWorker`] port, stamps the resulting commits
//! and parent→child lineage into the ledger, and **meters the child's spend
//! into the parent [`BudgetGuard`]**. No ad-hoc process spawning for agents:
//! hand-rolled per-call-site fan-out is exactly what lost lineage and left
//! budgets uncounted in the TS era (L-E9).
//!
//! [`Fleet::run_wave`] dispatches a set of dependency-ready tasks concurrently
//! (bounded concurrency), and [`Fleet::run_plan`] walks the whole DAG wave by
//! wave. Budget enforcement follows the engine's contract (`stella_core::budget`):
//! when a child's spend pushes the parent guard over an `enforced` limit, the
//! **remaining waves are not launched** — but in-flight siblings are never
//! cancelled mid-run (no mid-tool kill; a running worker settles first).
//!
//! [`Isolation`]: crate::plan::Isolation

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use stella_core::{BudgetGuard, BudgetOutcome, Clock};
use stella_store::Store;

use crate::git::{GitCli, Worktree, WorktreeError, WorktreeManager};
use crate::ledger::{
    AttemptFinish, AttemptId, AttemptStart, CommitRecord, Ledger, LedgerError, RunRecord,
};
use crate::plan::{Isolation, Plan, PlanError, Task, TaskId};

/// The port a fleet worker implements — the CLI glue later backs this with
/// `stella_core::Engine` / `stella_pipeline`; tests back it with fakes. It
/// receives the task and the workspace root it must operate in (the isolated
/// worktree, or the shared repo root for a [`Isolation::SharedTree`] task) and
/// reports what it did.
///
/// [`Isolation::SharedTree`]: crate::plan::Isolation::SharedTree
#[async_trait]
pub trait FleetWorker: Send + Sync {
    async fn run(&self, task: &Task, workspace_root: &Path) -> WorkerOutcome;
}

/// What a [`FleetWorker`] reports back for one task attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerOutcome {
    /// USD the worker spent — metered into the parent budget (L-E9) and
    /// recorded in the ledger's per-task spend.
    pub cost_usd: f64,
    /// Commits the worker landed (stamped into the ledger).
    pub commits: Vec<CommitRecord>,
    /// A human summary of the attempt (stored on the attempt row).
    pub summary: String,
    /// Whether the task succeeded.
    pub success: bool,
}

/// Static configuration of a fleet run.
#[derive(Debug, Clone)]
pub struct FleetConfig {
    /// This run's id — the ledger key and the lineage parent.
    pub run_id: String,
    /// The git ref every isolated worktree is branched from.
    pub base_ref: String,
    /// Max tasks dispatched concurrently within one wave (clamped to ≥1).
    /// Bounds the fleet's fan-out so it shares a bounded executor rather than
    /// spawning freely (L-S4).
    pub max_concurrency: usize,
}

impl FleetConfig {
    pub fn new(run_id: impl Into<String>, base_ref: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            base_ref: base_ref.into(),
            max_concurrency: 4,
        }
    }

    pub fn with_max_concurrency(mut self, n: usize) -> Self {
        self.max_concurrency = n;
        self
    }
}

/// The completed record of one dispatched task: what the worker produced, the
/// worktree it ran in (if isolated), the ledger attempt id, and the parent
/// budget's disposition after metering this child's spend.
#[derive(Debug, Clone)]
pub struct TaskHandle {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub outcome: WorkerOutcome,
    /// `Some` for an isolated task (its worktree), `None` for a shared-tree
    /// task.
    pub worktree: Option<Worktree>,
    /// The parent [`BudgetGuard`]'s outcome after this child's cost was
    /// metered — `AbortTurn` signals `run_plan` to stop launching new waves.
    pub budget: BudgetOutcome,
}

/// The result of running a whole plan.
#[derive(Debug, Default)]
pub struct FleetRunReport {
    /// Every dispatched task's handle, in a deterministic (task-id) order.
    pub handles: Vec<TaskHandle>,
    /// Task ids whose worker reported success — the set that unblocked
    /// dependents.
    pub completed: HashSet<TaskId>,
    /// Tasks whose dispatch itself errored (worktree creation, ledger I/O)
    /// before a worker could produce a handle, with the reason. Counted as
    /// failures.
    pub dispatch_failures: Vec<(TaskId, String)>,
    /// Tasks never attempted because a dependency failed (or the run stopped
    /// on budget before their wave).
    pub skipped: Vec<TaskId>,
    /// `true` if the run stopped early because the parent budget was enforced
    /// and a child pushed it over — the remaining waves were not launched.
    pub budget_aborted: bool,
}

impl FleetRunReport {
    /// Total spend across every dispatched child.
    pub fn total_cost_usd(&self) -> f64 {
        self.handles.iter().map(|h| h.outcome.cost_usd).sum()
    }

    /// Whether every task ran and reported success — dispatch failures and
    /// dependency-skipped tasks count against this.
    pub fn all_succeeded(&self) -> bool {
        self.dispatch_failures.is_empty()
            && self.skipped.is_empty()
            && self.handles.iter().all(|h| h.outcome.success)
    }
}

/// Typed fleet failures.
#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    /// A declared path is already claimed — by a sibling in this run or by
    /// another run in the same workspace. The holder is named so the user
    /// can tell a live conflict from a crashed run's leftover claim (the
    /// holder embeds its run id).
    #[error("task `{task}` claims `{path}`, already claimed by `{holder}`")]
    ClaimConflict {
        task: TaskId,
        path: String,
        holder: String,
    },
    /// The plan declares claims but no claim store is wired
    /// ([`Fleet::with_claim_store`]) — refusing to run unenforced claims
    /// silently.
    #[error("task `{task}` declares file claims but the fleet has no claim store")]
    ClaimsWithoutStore { task: TaskId },
    #[error(transparent)]
    Claims(#[from] stella_store::StoreError),
}

/// The fleet orchestrator. Owns the worker, the worktree manager, the ledger,
/// and the parent budget guard (the last two behind `Mutex`es so concurrent
/// wave dispatch serializes their fast synchronous writes — a lock is never
/// held across an `.await`). Optionally owns the workspace [`Store`] whose
/// `file_locks` back task claims: workers are in-process, so this single
/// orchestrator-held store is the multi-process "lock-holder" the store's
/// concurrency contract prescribes.
pub struct Fleet<W: FleetWorker, G: GitCli, C: Clock> {
    worker: W,
    worktrees: WorktreeManager<G>,
    ledger: Mutex<Ledger>,
    budget: Mutex<BudgetGuard>,
    clock: C,
    config: FleetConfig,
    claims: Option<Store>,
}

impl<W, G, C> Fleet<W, G, C>
where
    W: FleetWorker,
    G: GitCli,
    C: Clock,
{
    /// Construct a fleet and record its run row. `budget` is the *parent*
    /// guard — every child's spend is metered into it.
    pub fn new(
        worker: W,
        worktrees: WorktreeManager<G>,
        ledger: Ledger,
        budget: BudgetGuard,
        clock: C,
        config: FleetConfig,
    ) -> Result<Self, FleetError> {
        let created_at_ms = clock.now_ms();
        ledger.record_run(&RunRecord {
            id: config.run_id.clone(),
            root_task_count: 0,
            created_at_ms,
        })?;
        Ok(Self {
            worker,
            worktrees,
            ledger: Mutex::new(ledger),
            budget: Mutex::new(budget),
            clock,
            config,
            claims: None,
        })
    }

    /// Back task claims with the workspace store's `file_locks` table
    /// (builder style). Without this, a plan that declares claims fails
    /// dispatch with [`FleetError::ClaimsWithoutStore`] — claims are never
    /// silently unenforced.
    pub fn with_claim_store(mut self, store: Store) -> Self {
        self.claims = Some(store);
        self
    }

    /// Recover the mutex guard even if a prior holder panicked — the fleet
    /// itself never panics while holding a lock, so poison can only come from
    /// a panicking worker on another task; recovering keeps the fleet
    /// panic-free rather than cascading.
    fn lock_ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_budget(&self) -> MutexGuard<'_, BudgetGuard> {
        self.budget.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// THE one dispatch entry point (L-E9). Claims the task's declared
    /// paths, allocates the workspace per the task's isolation, records the
    /// attempt, runs the worker, then stamps its commits + lineage + spend
    /// into the ledger (one transaction) and meters its cost into the parent
    /// budget — returning a [`TaskHandle`].
    pub async fn dispatch(&self, task: &Task) -> Result<TaskHandle, FleetError> {
        // 0. Claim the declared paths before anything else exists for this
        //    attempt — a conflict is a plain dispatch failure with nothing
        //    (worktree, ledger rows) to clean up.
        self.acquire_claims(task)?;
        let result = self.dispatch_claimed(task).await;
        // Claims are per-attempt: released even when the worker failed (its
        // work sits committed on its branch; holding on would starve
        // dependents and retries).
        self.release_claims(task);
        result
    }

    /// [`dispatch`](Self::dispatch) after the task's claims are held.
    async fn dispatch_claimed(&self, task: &Task) -> Result<TaskHandle, FleetError> {
        // 1. Allocate the workspace. Isolate by default.
        let worktree = match task.isolation {
            Isolation::Isolated => Some(
                self.worktrees
                    .create(&task.id, &self.config.base_ref)
                    .await?,
            ),
            Isolation::SharedTree => None,
        };
        let workspace_root = match &worktree {
            Some(wt) => wt.path.clone(),
            None => self.worktrees.repo_root().to_path_buf(),
        };
        let branch = worktree
            .as_ref()
            .map(|w| w.branch.clone())
            .unwrap_or_else(|| "(shared-tree)".to_string());

        // 2. Record the task, its lineage, and the opening of this attempt —
        //    before the worker runs, so a crash mid-attempt still leaves a
        //    row naming what was in flight.
        let started_at_ms = self.clock.now_ms();
        let attempt_id = {
            let ledger = self.lock_ledger();
            ledger.record_task(&self.config.run_id, task)?;
            ledger.record_lineage(&self.config.run_id, &task.id, started_at_ms)?;
            ledger.start_attempt(&AttemptStart {
                run_id: self.config.run_id.clone(),
                task_id: task.id.clone(),
                worktree_path: workspace_root.to_string_lossy().into_owned(),
                branch,
                started_at_ms,
            })?
        };

        // 3. Run the worker — the slow part, concurrent across a wave. No lock
        //    is held here.
        let outcome = self.worker.run(task, &workspace_root).await;

        // 4. Stamp the outcome (attempt close + commits + spend) atomically,
        //    then meter the child's cost into the parent budget (L-E9).
        let finished_at_ms = self.clock.now_ms();
        {
            let ledger = self.lock_ledger();
            ledger.finish_attempt(&AttemptFinish {
                attempt_id,
                run_id: self.config.run_id.clone(),
                task_id: task.id.clone(),
                finished_at_ms,
                success: outcome.success,
                summary: outcome.summary.clone(),
                commits: outcome.commits.clone(),
                cost_usd: outcome.cost_usd,
                spend_at_ms: finished_at_ms,
            })?;
        }
        let budget = {
            let mut guard = self.lock_budget();
            guard.record_spend(outcome.cost_usd)
        };

        Ok(TaskHandle {
            task_id: task.id.clone(),
            attempt_id,
            outcome,
            worktree,
            budget,
        })
    }

    /// The lock-table identity a task claims under: run-scoped, so a crashed
    /// run's leftover claim is distinguishable from this run's by eye (the
    /// run id embeds its start time and pid).
    fn claim_holder(&self, task: &Task) -> String {
        format!("{}/{}", self.config.run_id, task.id)
    }

    /// Acquire every path in `task.claims` — all-or-nothing: on a conflict
    /// (or a store error) the paths already acquired roll back and the error
    /// names what blocked. Acquisition is re-entrant per holder, so a
    /// duplicate path within one task is harmless.
    fn acquire_claims(&self, task: &Task) -> Result<(), FleetError> {
        if task.claims.is_empty() {
            return Ok(());
        }
        let Some(store) = &self.claims else {
            return Err(FleetError::ClaimsWithoutStore {
                task: task.id.clone(),
            });
        };
        let holder = self.claim_holder(task);
        for (i, path) in task.claims.iter().enumerate() {
            let failure = match store.acquire_file_lock(path, &holder) {
                Ok(true) => continue,
                Ok(false) => FleetError::ClaimConflict {
                    task: task.id.clone(),
                    path: path.clone(),
                    holder: store
                        .file_lock_holder(path)
                        .ok()
                        .flatten()
                        // The holder released between the two reads — the
                        // conflict was real when acquisition failed.
                        .unwrap_or_else(|| "(already released)".to_string()),
                },
                Err(e) => FleetError::Claims(e),
            };
            for claimed in &task.claims[..i] {
                let _ = store.release_file_lock(claimed, &holder);
            }
            return Err(failure);
        }
        Ok(())
    }

    /// Release the task's claims. Best-effort by design: release only
    /// deletes rows owned by this holder, and a failed delete leaves a row
    /// that NAMES this run — the next claimant's conflict error points
    /// straight back here, never a silent corruption.
    fn release_claims(&self, task: &Task) {
        let Some(store) = &self.claims else {
            return;
        };
        let holder = self.claim_holder(task);
        for path in &task.claims {
            let _ = store.release_file_lock(path, &holder);
        }
    }

    /// Dispatch a wave of dependency-ready tasks concurrently, bounded by
    /// `max_concurrency`. Results come back in completion order, each tagged
    /// with its task id (a dispatch `Err` — e.g. a failed `worktree add` —
    /// doesn't carry a handle, and the caller must still know WHICH task it
    /// lost); the caller (`run_plan`) reorders deterministically.
    pub async fn run_wave(&self, tasks: &[&Task]) -> Vec<(TaskId, Result<TaskHandle, FleetError>)> {
        let concurrency = self.config.max_concurrency.max(1);
        stream::iter(tasks.iter().copied())
            .map(|task| async move { (task.id.clone(), self.dispatch(task).await) })
            .buffer_unordered(concurrency)
            .collect()
            .await
    }

    /// Execute an entire plan wave by wave, honoring DAG order. Records the
    /// run and its tasks up front, then repeatedly dispatches the ready set
    /// concurrently until the plan drains — or until the parent budget is
    /// enforced and a child trips it, at which point the remaining waves are
    /// not launched (in-flight siblings still settle; see the module docs).
    pub async fn run_plan(&self, plan: &Plan) -> Result<FleetRunReport, FleetError> {
        plan.validate()?;
        let root_task_count = plan
            .tasks
            .iter()
            .filter(|t| t.depends_on.is_empty())
            .count() as u32;
        {
            let ledger = self.lock_ledger();
            ledger.record_run(&RunRecord {
                id: self.config.run_id.clone(),
                root_task_count,
                created_at_ms: self.clock.now_ms(),
            })?;
            for task in &plan.tasks {
                ledger.record_task(&self.config.run_id, task)?;
            }
        }

        let mut report = FleetRunReport::default();
        // `succeeded` gates dependents: a failed (or dispatch-errored) task
        // must NOT unblock the tasks that depend on it — running them against
        // work that never landed just burns budget on doomed turns. They are
        // reported as `skipped` instead. `attempted` keeps a failed task from
        // being re-offered by `ready_tasks` (it is never in `succeeded`).
        let mut succeeded: HashSet<TaskId> = HashSet::new();
        let mut attempted: HashSet<TaskId> = HashSet::new();

        loop {
            let ready: Vec<&Task> = plan
                .ready_tasks(&succeeded)
                .into_iter()
                .filter(|t| !attempted.contains(&t.id))
                .collect();
            if ready.is_empty() {
                break;
            }

            // A dispatch error (worktree creation, ledger I/O) is recorded as
            // that task's failure — never an early return that would throw
            // away the settled siblings' handles (their spend, commits, and
            // worktrees would otherwise vanish from the report).
            let mut handles: Vec<TaskHandle> = Vec::with_capacity(ready.len());
            for (task_id, result) in self.run_wave(&ready).await {
                attempted.insert(task_id.clone());
                match result {
                    Ok(handle) => handles.push(handle),
                    Err(e) => report.dispatch_failures.push((task_id, e.to_string())),
                }
            }
            // Deterministic order regardless of completion timing.
            handles.sort_by(|a, b| a.task_id.cmp(&b.task_id));

            let mut wave_tripped_budget = false;
            for handle in handles {
                if handle.outcome.success {
                    succeeded.insert(handle.task_id.clone());
                }
                if matches!(handle.budget, BudgetOutcome::AbortTurn { .. }) {
                    wave_tripped_budget = true;
                }
                report.handles.push(handle);
            }

            if wave_tripped_budget {
                report.budget_aborted = true;
                break; // stop launching remaining waves (never cancel in-flight)
            }
        }

        report.skipped = plan
            .tasks
            .iter()
            .filter(|t| !attempted.contains(&t.id))
            .map(|t| t.id.clone())
            .collect();
        // Dispatch failures accrue in wave-completion order, which varies run
        // to run; sort by task id so the report reads identically for the same
        // plan (matching the deterministic `handles` ordering).
        report.dispatch_failures.sort_by(|a, b| a.0.cmp(&b.0));
        report.completed = succeeded;
        Ok(report)
    }

    // ---- Read-through accessors (tests + real callers) ----------------

    /// The parent budget guard's current state (a `Copy` snapshot).
    pub fn budget_snapshot(&self) -> BudgetGuard {
        *self.lock_budget()
    }

    /// Total spend recorded in the ledger for this run.
    pub fn ledger_total_spend(&self) -> Result<f64, FleetError> {
        Ok(self.lock_ledger().total_spend(&self.config.run_id)?)
    }

    /// Child task ids recorded as lineage under this run.
    pub fn ledger_lineage_children(&self) -> Result<Vec<String>, FleetError> {
        Ok(self.lock_ledger().lineage_children(&self.config.run_id)?)
    }

    /// Commits recorded for a task in this run.
    pub fn ledger_commits_for_task(&self, task_id: &str) -> Result<Vec<CommitRecord>, FleetError> {
        Ok(self
            .lock_ledger()
            .commits_for_task(&self.config.run_id, task_id)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use stella_protocol::BudgetMode;

    use crate::git::{GitCli, GitError, GitOutput};

    // ---- Fakes --------------------------------------------------------

    /// A monotonically-increasing clock so started_at < finished_at without a
    /// real wall clock.
    struct SeqClock(AtomicU64);
    impl SeqClock {
        fn new() -> Self {
            Self(AtomicU64::new(1))
        }
    }
    impl Clock for SeqClock {
        fn now_ms(&self) -> u64 {
            self.0.fetch_add(1, Ordering::SeqCst)
        }
    }

    /// A `GitCli` that says yes to everything (worktree add etc.) and records
    /// calls — no real repo needed for dispatch-seam tests.
    struct OkGit {
        calls: Arc<StdMutex<Vec<Vec<String>>>>,
    }
    impl OkGit {
        fn new() -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
            }
        }
    }
    #[async_trait]
    impl GitCli for OkGit {
        async fn run(&self, _repo: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(args.iter().map(|s| s.to_string()).collect());
            Ok(GitOutput::ok(""))
        }
    }

    /// A worker that reports a fixed cost + one commit per task and records
    /// the workspace root it was handed.
    struct FakeWorker {
        cost_usd: f64,
        success: bool,
        seen_roots: Arc<StdMutex<Vec<(String, PathBuf)>>>,
    }
    impl FakeWorker {
        fn new(cost_usd: f64) -> Self {
            Self {
                cost_usd,
                success: true,
                seen_roots: Arc::new(StdMutex::new(Vec::new())),
            }
        }
        /// A worker whose every task reports failure — for wave-scheduling
        /// tests that a failed prerequisite must not unblock dependents.
        fn failing(cost_usd: f64) -> Self {
            Self {
                success: false,
                ..Self::new(cost_usd)
            }
        }
    }

    /// A `GitCli` that fails every `git worktree` invocation (add/remove) with
    /// a non-zero exit — the shape of a real "worktree already exists" or
    /// permission failure, so `WorktreeManager::create` returns an error.
    struct FailGit;
    #[async_trait]
    impl GitCli for FailGit {
        async fn run(&self, _repo: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
            if args.first() == Some(&"worktree") {
                return Ok(GitOutput::failed(128, "fatal: worktree add failed"));
            }
            Ok(GitOutput::ok(""))
        }
    }
    #[async_trait]
    impl FleetWorker for FakeWorker {
        async fn run(&self, task: &Task, workspace_root: &Path) -> WorkerOutcome {
            self.seen_roots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((task.id.clone(), workspace_root.to_path_buf()));
            WorkerOutcome {
                cost_usd: self.cost_usd,
                commits: vec![CommitRecord {
                    sha: format!("sha-{}", task.id),
                    branch: format!("fleet/{}", task.id),
                    task_id: task.id.clone(),
                    message: format!("work on {}", task.id),
                    timestamp_ms: 1,
                }],
                summary: format!("did {}", task.id),
                success: self.success,
            }
        }
    }

    fn fleet(
        worker: FakeWorker,
        git: OkGit,
        budget: BudgetGuard,
        config: FleetConfig,
    ) -> Fleet<FakeWorker, OkGit, SeqClock> {
        let manager = WorktreeManager::new(git, "/repo");
        let ledger = Ledger::open_in_memory().unwrap();
        Fleet::new(worker, manager, ledger, budget, SeqClock::new(), config).unwrap()
    }

    // ---- dispatch: the seam records everything ------------------------

    #[tokio::test]
    async fn dispatch_isolated_task_creates_worktree_and_stamps_ledger() {
        let worker = FakeWorker::new(0.10);
        let seen = worker.seen_roots.clone();
        let f = fleet(
            worker,
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            FleetConfig::new("run1", "HEAD"),
        );

        let task = Task::new("t1", "title", "prompt");
        let handle = f.dispatch(&task).await.unwrap();

        // An isolated task got a worktree, and the worker was handed its path.
        // The branch carries a collision-free hash suffix (`fleet/t1-<hash>`).
        let wt = handle.worktree.expect("isolated task has a worktree");
        assert!(wt.branch.starts_with("fleet/t1-"), "{}", wt.branch);
        let roots = seen.lock().unwrap();
        assert_eq!(roots[0].0, "t1");
        assert_eq!(roots[0].1, wt.path);

        // Ledger: lineage, commit, and spend all recorded.
        assert_eq!(f.ledger_lineage_children().unwrap(), vec!["t1".to_string()]);
        let commits = f.ledger_commits_for_task("t1").unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].sha, "sha-t1");
        assert!((f.ledger_total_spend().unwrap() - 0.10).abs() < 1e-9);

        // Parent budget metered the child's cost.
        assert!((f.budget_snapshot().session_spent_usd() - 0.10).abs() < 1e-9);
    }

    #[tokio::test]
    async fn dispatch_shared_tree_task_uses_repo_root_and_no_worktree() {
        let worker = FakeWorker::new(0.05);
        let seen = worker.seen_roots.clone();
        let git = OkGit::new();
        let git_calls = git.calls.clone();
        let f = fleet(
            worker,
            git,
            BudgetGuard::new(BudgetMode::Observed, None, None),
            FleetConfig::new("run1", "HEAD"),
        );

        let task = Task::new("t1", "title", "prompt").shared_tree();
        let handle = f.dispatch(&task).await.unwrap();
        assert!(
            handle.worktree.is_none(),
            "shared-tree task has no worktree"
        );
        assert_eq!(seen.lock().unwrap()[0].1, PathBuf::from("/repo"));
        // No `git worktree add` was issued for a shared-tree task.
        assert!(
            !git_calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.first().map(String::as_str) == Some("worktree")),
            "shared-tree dispatch must not create a worktree"
        );
    }

    // ---- claims: cooperative file locks through the workspace store ---

    fn fleet_with_claims(worker: FakeWorker) -> Fleet<FakeWorker, OkGit, SeqClock> {
        fleet(
            worker,
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            FleetConfig::new("run1", "HEAD").with_max_concurrency(2),
        )
        .with_claim_store(Store::in_memory().unwrap())
    }

    /// A worker that parks until the test grants a permit — keeps its
    /// task's attempt (and claims) open so a sibling's acquire genuinely
    /// races a HELD claim instead of a released one.
    struct GatedWorker {
        gate: Arc<tokio::sync::Semaphore>,
    }
    #[async_trait]
    impl FleetWorker for GatedWorker {
        async fn run(&self, task: &Task, _workspace_root: &Path) -> WorkerOutcome {
            if let Ok(permit) = self.gate.acquire().await {
                permit.forget(); // one permit releases exactly one worker
            }
            WorkerOutcome {
                cost_usd: 0.0,
                commits: Vec::new(),
                summary: format!("did {}", task.id),
                success: true,
            }
        }
    }

    #[tokio::test]
    async fn parallel_siblings_claiming_the_same_path_conflict_once() {
        // Two independent tasks in one concurrent wave, both claiming the
        // same path: exactly one wins the claim and runs; the other is a
        // recorded dispatch failure naming the conflict.
        let plan = Plan::new(vec![
            Task::new("t1", "t1", "p").claims(["src/shared.rs"]),
            Task::new("t2", "t2", "p").claims(["src/shared.rs"]),
        ]);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let f = Fleet::new(
            GatedWorker { gate: gate.clone() },
            WorktreeManager::new(OkGit::new(), "/repo"),
            Ledger::open_in_memory().unwrap(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            SeqClock::new(),
            FleetConfig::new("run1", "HEAD").with_max_concurrency(2),
        )
        .unwrap()
        .with_claim_store(Store::in_memory().unwrap());

        // On this current-thread test runtime both dispatches reach their
        // claim before the permits below land: the winner parks in its
        // gated worker still holding the claim, so the loser's acquire is a
        // true mid-attempt conflict.
        let (report, ()) = tokio::join!(f.run_plan(&plan), async {
            gate.add_permits(2);
        });
        let report = report.unwrap();
        assert_eq!(report.handles.len(), 1, "exactly one claimant ran");
        assert!(report.handles[0].outcome.success);
        assert_eq!(report.dispatch_failures.len(), 1);
        let (loser, reason) = &report.dispatch_failures[0];
        assert_ne!(loser, &report.handles[0].task_id);
        assert!(
            reason.contains("claims `src/shared.rs`") && reason.contains("run1/"),
            "the conflict names the path and the winning holder: {reason}"
        );

        // Both the winner's claim and the loser's rollback released: a fresh
        // task can claim the same path (one gate permit is left for it).
        let retry = Task::new("t3", "t3", "p").claims(["src/shared.rs"]);
        f.dispatch(&retry).await.expect("path released after wave");
    }

    #[tokio::test]
    async fn dependent_tasks_reclaim_a_path_released_by_their_prerequisite() {
        // Claims are per-attempt, so a dependent editing the same file
        // acquires it in the next wave.
        let plan = Plan::new(vec![
            Task::new("a", "a", "p").claims(["src/shared.rs"]),
            Task::new("b", "b", "p")
                .depends_on(["a"])
                .claims(["src/shared.rs"]),
        ]);
        let f = fleet_with_claims(FakeWorker::new(0.10));
        let report = f.run_plan(&plan).await.unwrap();
        assert!(report.all_succeeded(), "sequential claimants never clash");
        assert_eq!(report.handles.len(), 2);
    }

    #[tokio::test]
    async fn a_failed_worker_still_releases_its_claims() {
        let f = fleet_with_claims(FakeWorker::failing(0.10));
        let task = Task::new("t1", "t1", "p").claims(["src/a.rs"]);
        let handle = f.dispatch(&task).await.unwrap();
        assert!(!handle.outcome.success);

        // The failed attempt released its claim — a retry can acquire it.
        let retry = Task::new("t2", "t2", "p").claims(["src/a.rs"]);
        f.dispatch(&retry).await.expect("claim released on failure");
    }

    #[tokio::test]
    async fn a_foreign_claim_fails_dispatch_by_name_and_rolls_back() {
        // A claim held by another run (or a crashed one) in the same
        // workspace: dispatch fails naming that holder, and the paths this
        // task had already acquired roll back.
        let store = Store::in_memory().unwrap();
        assert!(store.acquire_file_lock("src/b.rs", "fleet-9/t9").unwrap());
        let f = fleet(
            FakeWorker::new(0.10),
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            FleetConfig::new("run1", "HEAD"),
        )
        .with_claim_store(store);

        let task = Task::new("t1", "t1", "p").claims(["src/a.rs", "src/b.rs"]);
        match f.dispatch(&task).await {
            Err(FleetError::ClaimConflict { task, path, holder }) => {
                assert_eq!((task.as_str(), path.as_str()), ("t1", "src/b.rs"));
                assert_eq!(holder, "fleet-9/t9");
            }
            other => panic!("expected a claim conflict, got {other:?}"),
        }
        // src/a.rs was acquired before the conflict and must have rolled
        // back — a sibling can claim it.
        let sibling = Task::new("t2", "t2", "p").claims(["src/a.rs"]);
        f.dispatch(&sibling)
            .await
            .expect("partial claims roll back");
    }

    #[tokio::test]
    async fn claims_without_a_store_are_a_named_dispatch_failure() {
        // No claim store wired: a task that declares claims must fail loudly
        // (through the same recorded-dispatch-failure path as any other
        // dispatch error), never run with its claims silently unenforced.
        let plan = Plan::new(vec![Task::new("t1", "t1", "p").claims(["src/a.rs"])]);
        let f = fleet(
            FakeWorker::new(0.10),
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            FleetConfig::new("run1", "HEAD"),
        );
        let report = f.run_plan(&plan).await.unwrap();
        assert!(report.handles.is_empty());
        assert_eq!(report.dispatch_failures.len(), 1);
        assert!(
            report.dispatch_failures[0].1.contains("no claim store"),
            "{}",
            report.dispatch_failures[0].1
        );
    }

    // ---- run_plan: waves, lineage, budget metering, abort -------------

    #[tokio::test]
    async fn run_plan_executes_all_waves_and_meters_total_spend() {
        // a -> {b, c} -> d : two middle tasks run concurrently in one wave.
        let plan = Plan::new(vec![
            Task::new("a", "a", "p"),
            Task::new("b", "b", "p").depends_on(["a"]),
            Task::new("c", "c", "p").depends_on(["a"]),
            Task::new("d", "d", "p").depends_on(["b", "c"]),
        ]);
        let f = fleet(
            FakeWorker::new(0.25),
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            FleetConfig::new("run1", "HEAD").with_max_concurrency(2),
        );

        let report = f.run_plan(&plan).await.unwrap();
        assert_eq!(report.completed.len(), 4);
        assert!(report.all_succeeded());
        assert!(!report.budget_aborted);
        assert!((report.total_cost_usd() - 1.0).abs() < 1e-9);
        // Handles come back in deterministic task-id order.
        let order: Vec<&str> = report.handles.iter().map(|h| h.task_id.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "c", "d"]);
        // Every task is stamped as lineage under the run.
        assert_eq!(
            f.ledger_lineage_children().unwrap(),
            vec!["a", "b", "c", "d"]
        );
        assert!((f.ledger_total_spend().unwrap() - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn enforced_parent_budget_aborts_remaining_waves() {
        // A 4-deep chain a->b->c->d, each costing 0.5, with an enforced $1.00
        // parent limit. Spend after each wave: 0.5, 1.0 (== limit, Continue),
        // 1.5 (> limit → AbortTurn). So c trips it and d never runs.
        let plan = Plan::new(vec![
            Task::new("a", "a", "p"),
            Task::new("b", "b", "p").depends_on(["a"]),
            Task::new("c", "c", "p").depends_on(["b"]),
            Task::new("d", "d", "p").depends_on(["c"]),
        ]);
        let f = fleet(
            FakeWorker::new(0.5),
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Enforced, Some(1.0), None),
            FleetConfig::new("run1", "HEAD"),
        );

        let report = f.run_plan(&plan).await.unwrap();
        assert!(
            report.budget_aborted,
            "the enforced budget must abort the run"
        );
        assert!(
            report.completed.contains("c") && !report.completed.contains("d"),
            "c runs and trips the budget; d must never be dispatched (completed = {:?})",
            report.completed
        );
        assert_eq!(report.completed.len(), 3);
        // d has no attempt row at all — it was never dispatched.
        assert!(f.ledger_commits_for_task("d").unwrap().is_empty());
    }

    #[tokio::test]
    async fn observed_budget_over_limit_never_aborts_the_run() {
        // Same chain and per-task cost, but `observed` mode: it warns, never
        // aborts, so every wave runs.
        let plan = Plan::new(vec![
            Task::new("a", "a", "p"),
            Task::new("b", "b", "p").depends_on(["a"]),
            Task::new("c", "c", "p").depends_on(["b"]),
        ]);
        let f = fleet(
            FakeWorker::new(0.5),
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Observed, Some(0.4), None),
            FleetConfig::new("run1", "HEAD"),
        );
        let report = f.run_plan(&plan).await.unwrap();
        assert!(!report.budget_aborted);
        assert_eq!(report.completed.len(), 3);
    }

    #[tokio::test]
    async fn a_failed_prerequisite_skips_its_dependents() {
        // a (fails) -> b -> c. a's failure must NOT unblock b, and c (behind
        // b) is skipped in turn — dependents of a failure never run, so no
        // budget is spent on doomed turns.
        let plan = Plan::new(vec![
            Task::new("a", "a", "p"),
            Task::new("b", "b", "p").depends_on(["a"]),
            Task::new("c", "c", "p").depends_on(["b"]),
        ]);
        let f = fleet(
            FakeWorker::failing(0.10),
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            FleetConfig::new("run1", "HEAD"),
        );

        let report = f.run_plan(&plan).await.unwrap();
        assert!(
            !report.all_succeeded(),
            "a failed → the run did not succeed"
        );
        assert!(
            !report.completed.contains("a"),
            "a failed, so it is not in the succeeded set"
        );
        // Only a was ever dispatched; b and c were skipped, not attempted.
        assert_eq!(report.handles.len(), 1);
        assert_eq!(report.handles[0].task_id, "a");
        let mut skipped = report.skipped.clone();
        skipped.sort();
        assert_eq!(skipped, vec!["b".to_string(), "c".to_string()]);
    }

    #[tokio::test]
    async fn a_git_worktree_failure_is_a_recorded_dispatch_failure() {
        // Two independent isolated tasks in one wave; git worktree add fails
        // for both. Each is a recorded dispatch failure (not an early return
        // that discards the wave), the run did not succeed, and the failures
        // are reported in deterministic task-id order.
        let plan = Plan::new(vec![Task::new("t1", "t1", "p"), Task::new("t2", "t2", "p")]);
        let f = Fleet::new(
            FakeWorker::new(0.10),
            WorktreeManager::new(FailGit, "/repo"),
            Ledger::open_in_memory().unwrap(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            SeqClock::new(),
            FleetConfig::new("run1", "HEAD").with_max_concurrency(2),
        )
        .unwrap();

        let report = f.run_plan(&plan).await.unwrap();
        assert!(!report.all_succeeded(), "dispatch failures fail the run");
        assert!(report.handles.is_empty(), "no task produced a handle");
        let failed: Vec<&str> = report
            .dispatch_failures
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(failed, vec!["t1", "t2"], "both failures recorded, sorted");
    }
}
