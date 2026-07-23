//! THE dispatch seam (L-E9). Subagent fan-out goes through exactly one
//! API — [`Fleet::dispatch`] — that claims the task's declared paths
//! (cooperative file locks in the workspace [`Store`], held for the
//! attempt's duration), allocates the task's workspace (shared tree by
//! default; a git worktree when the task opts into isolation), records the attempt in the
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
//! The seam also carries **per-task control**: every dispatched worker
//! receives [`WorkerControls`] (a pause watch + a stop oneshot — the exact
//! channel shapes the deck's sub-sessions use), and the fleet exposes the
//! matching verbs, [`Fleet::pause_task`] / [`Fleet::resume_task`] /
//! [`Fleet::stop_task`]. This closes the "fleet supervisor seam"
//! `command_deck.rs` named as the follow-up:
//! per-worker pause/stop now exists at the fleet layer, not just for deck
//! sub-session lanes. Restart is deliberately not a fleet verb —
//! [`Fleet::dispatch`] is re-runnable, so a restart is the caller
//! re-dispatching the same [`Task`]; the fleet keeps no respawn state.
//!
//! [`Isolation`]: crate::plan::Isolation

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use stella_core::{BudgetGuard, BudgetOutcome, Clock};
use stella_store::Store;
use tokio::sync::{oneshot, watch};

use crate::cache_schedule::{RunnableSession, warmest_first};
use crate::git::{GitCli, Worktree, WorktreeError, WorktreeManager};
use crate::ledger::{
    AttemptFinish, AttemptId, AttemptStart, CommitRecord, Ledger, LedgerError, RunRecord,
};
use crate::plan::{Isolation, Plan, PlanError, Task, TaskId};

/// The port a fleet worker implements — the CLI glue later backs this with
/// `stella_core::Engine` / `stella_pipeline`; tests back it with fakes. It
/// receives the task, the workspace root it must operate in (the isolated
/// worktree, or the shared repo root for a [`Isolation::SharedTree`] task),
/// and its [`WorkerControls`], and reports what it did.
///
/// [`Isolation::SharedTree`]: crate::plan::Isolation::SharedTree
#[async_trait]
pub trait FleetWorker: Send + Sync {
    async fn run(
        &self,
        task: &Task,
        workspace_root: &Path,
        controls: WorkerControls,
    ) -> WorkerOutcome;
}

/// The receiver halves of one task's control lines, handed to the
/// [`FleetWorker`] alongside its task — the fleet-side twin of the deck
/// sub-sessions' channels (`stella-cli/src/subsession.rs`): a pause watch
/// and a stop oneshot, driven by [`Fleet::pause_task`] /
/// [`Fleet::resume_task`] / [`Fleet::stop_task`].
///
/// Because the controls ride the [`FleetWorker`] port itself, the
/// capability is structural: workers run no nested spawns today (a
/// worker's own `task_assign` requests are reported, not dispatched — the
/// deck documents that v1 scope), so there is nothing recursive to wire —
/// but any future nested spawn dispatched back through this port would
/// receive its own control lines with no new machinery.
pub struct WorkerControls {
    /// `true` = park at the next safe step boundary until it flips back
    /// (the engine's `TurnGate` boundary — never mid-tool). A closed
    /// channel reads as resumed: a worker must never park forever after
    /// the fleet dropped its handle.
    pub pause: watch::Receiver<bool>,
    /// Fires when [`Fleet::stop_task`] consumes this task's stop line. A
    /// channel closed *without* firing means no stop will ever come —
    /// treat it as "run to completion", never as a stop.
    pub stop: oneshot::Receiver<()>,
}

/// The sender halves the fleet retains for one live task. `stop` is
/// `Option` because firing a oneshot consumes it — a second stop on the
/// same attempt is a stale no-op, exactly the deck sub-session semantics.
struct TaskControlHandle {
    pause: watch::Sender<bool>,
    stop: Option<oneshot::Sender<()>>,
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
    /// The aggregate parent budget was already exhausted, so this task was
    /// skipped before launching a worker rather than being run. Reported as a
    /// budget skip, not a task failure.
    #[error("task `{task}` skipped: fleet budget exhausted before it could start")]
    BudgetExhausted { task: TaskId },
}

/// The fleet orchestrator. Owns the worker, the worktree manager, the ledger,
/// the parent budget guard, and the live tasks' control lines (the last
/// three behind `Mutex`es so concurrent wave dispatch serializes their fast
/// synchronous writes — a lock is never held across an `.await`).
/// Optionally owns the workspace [`Store`] whose
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
    /// Live workers' control lines, keyed by task id — registered by
    /// `dispatch_claimed` for exactly the span its worker runs, so the
    /// control verbs address live workers only.
    controls: Mutex<HashMap<TaskId, TaskControlHandle>>,
    /// Per-task prompt-cache warmth, for [`Fleet::with_cache_warmth`]
    /// (issue #269) — `None` (the default) leaves every ready wave in its
    /// existing order.
    warmth: Option<CacheWarmthLookup>,
}

/// Seconds until a task's session prompt-cache prefix expires, `None` when
/// there is no warm prefix to preserve — the caller-supplied signal
/// [`Fleet::with_cache_warmth`] reorders ready waves against. Boxed so a
/// caller can close over its own store/clock rather than the fleet owning
/// one; `stella-fleet` stays free of `stella-model`/`stella-store`, matching
/// [`crate::cache_schedule`]'s pure, no-I/O contract (this closure is the
/// caller's I/O, called synchronously between waves).
pub type CacheWarmthLookup = Box<dyn Fn(&TaskId) -> Option<u64> + Send + Sync>;

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
            controls: Mutex::new(HashMap::new()),
            warmth: None,
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

    /// Make each ready wave's dispatch order cache-TTL-aware (builder style,
    /// issue #269): before every `run_wave`, `run_plan` sorts that wave
    /// warmest-first (soonest-to-expire prefix first) using `lookup`,
    /// falling back to `ready_tasks`' existing order for any task the lookup
    /// reports no warmth for. This only changes anything when a wave has more
    /// ready tasks than `max_concurrency` (`run_wave`'s `buffer_unordered`
    /// fills its bounded concurrency window in the order it is handed), so a
    /// session about to lose its cached prefix is resumed before a colder one
    /// gets the slot instead. Without this call (the default) every wave
    /// dispatches in `ready_tasks`' order, unchanged from before #269.
    pub fn with_cache_warmth(mut self, lookup: CacheWarmthLookup) -> Self {
        self.warmth = Some(lookup);
        self
    }

    /// Reorder one ready wave warmest-first when a warmth lookup is
    /// installed; a single priority class (fleet.rs has no priority concept
    /// of its own — [`crate::cache_schedule::warmest_first`]'s priority
    /// dimension goes unused here, `id` tie-breaking the rest) so this is
    /// purely a warmth sort. Returns `ready` unchanged when no lookup was
    /// installed, so every existing caller keeps today's order.
    fn order_ready_by_warmth<'a>(&self, ready: Vec<&'a Task>) -> Vec<&'a Task> {
        let Some(lookup) = &self.warmth else {
            return ready;
        };
        let sessions: Vec<RunnableSession> = ready
            .iter()
            .map(|t| RunnableSession {
                id: t.id.clone(),
                priority: 0,
                warmth_secs: lookup(&t.id),
            })
            .collect();
        warmest_first(&sessions)
            .into_iter()
            .filter_map(|s| ready.iter().find(|t| t.id == s.id).copied())
            .collect()
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

    fn lock_controls(&self) -> MutexGuard<'_, HashMap<TaskId, TaskControlHandle>> {
        self.controls.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// THE one dispatch entry point (L-E9). Claims the task's declared
    /// paths, allocates the workspace per the task's isolation, records the
    /// attempt, runs the worker, then stamps its commits + lineage + spend
    /// into the ledger (one transaction) and meters its cost into the parent
    /// budget — returning a [`TaskHandle`].
    pub async fn dispatch(&self, task: &Task) -> Result<TaskHandle, FleetError> {
        // 0a. Aggregate budget gate — enforced BEFORE the worker runs. The
        //     post-run `record_spend` alone only stopped launching the NEXT
        //     wave, so a single wide fan-out wave (the common case: every
        //     positional prompt is an independent task, all in one wave) ran to
        //     completion and could spend far past `--budget`. Checking the
        //     shared parent guard as each task claims a concurrency slot stops
        //     launching further workers once the cap is crossed. Combined with
        //     each child's per-child remaining-budget sub-cap (fleet_cmd), the
        //     aggregate honors the flag.
        if let BudgetOutcome::AbortTurn { .. } = self.lock_budget().evaluate() {
            return Err(FleetError::BudgetExhausted {
                task: task.id.clone(),
            });
        }
        // 0b. Claim the declared paths before anything else exists for this
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
        // 1. Allocate the workspace: a worktree only for tasks that opted
        //    into isolation; the shared tree (default) allocates nothing.
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

        // 3. Run the worker — the slow part, concurrent across a wave. No
        //    lock is held across the await. Its control lines are registered
        //    first and deregistered the moment it settles, so the control
        //    verbs address exactly the tasks with a live worker. (Task ids
        //    are unique within a plan; an ad-hoc re-dispatch of a still-live
        //    id would re-key the registration to the newer attempt.)
        let (pause_tx, pause_rx) = watch::channel(false);
        let (stop_tx, stop_rx) = oneshot::channel();
        self.lock_controls().insert(
            task.id.clone(),
            TaskControlHandle {
                pause: pause_tx,
                stop: Some(stop_tx),
            },
        );
        let controls = WorkerControls {
            pause: pause_rx,
            stop: stop_rx,
        };
        let outcome = self.worker.run(task, &workspace_root, controls).await;
        // The worker settled — drop its control handle so a later pause or
        // stop for this id reports "no live worker" instead of signalling
        // into the void.
        self.lock_controls().remove(&task.id);

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
            // Cache-TTL-aware scheduling (#269): a no-op unless
            // `with_cache_warmth` was installed.
            let ready = self.order_ready_by_warmth(ready);

            // A dispatch error (worktree creation, ledger I/O) is recorded as
            // that task's failure — never an early return that would throw
            // away the settled siblings' handles (their spend, commits, and
            // worktrees would otherwise vanish from the report).
            let mut handles: Vec<TaskHandle> = Vec::with_capacity(ready.len());
            let mut wave_tripped_budget = false;
            for (task_id, result) in self.run_wave(&ready).await {
                match result {
                    Ok(handle) => {
                        attempted.insert(task_id.clone());
                        handles.push(handle);
                    }
                    // A pre-dispatch budget skip is NOT a task failure and NOT
                    // an attempt — the worker never ran. Leaving it out of
                    // `attempted` lets the final recompute report it (and every
                    // task behind the aborted wave) as `skipped`. Just trip the
                    // run so no further waves launch.
                    Err(FleetError::BudgetExhausted { .. }) => {
                        wave_tripped_budget = true;
                    }
                    Err(e) => {
                        attempted.insert(task_id.clone());
                        report.dispatch_failures.push((task_id, e.to_string()));
                    }
                }
            }
            // Deterministic order regardless of completion timing.
            handles.sort_by(|a, b| a.task_id.cmp(&b.task_id));
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

    // ---- Per-task control: Pause / Resume / Stop ----------------------

    /// Pause a live worker at its next safe step boundary (the engine's
    /// `TurnGate` — the same boundary budget aborts use, never mid-tool).
    /// `false` when no live worker is registered under `id`; a stale pause
    /// is a no-op, never an error — the deck sub-session semantics.
    ///
    /// Restart is deliberately NOT a fleet verb: [`dispatch`](Self::dispatch)
    /// is already re-runnable, so a restart is the caller re-dispatching the
    /// same [`Task`] — the fleet keeps no respawn state.
    pub fn pause_task(&self, id: &TaskId) -> bool {
        match self.lock_controls().get(id) {
            Some(handle) => handle.pause.send(true).is_ok(),
            None => false,
        }
    }

    /// Resume a paused worker (flip its pause watch back to `false`).
    /// `false` when no live worker is registered under `id`.
    pub fn resume_task(&self, id: &TaskId) -> bool {
        match self.lock_controls().get(id) {
            Some(handle) => handle.pause.send(false).is_ok(),
            None => false,
        }
    }

    /// Fire a live worker's stop line (the CLI worker drops its turn future
    /// at the next await point — the same clean cancel the deck uses).
    /// Consumes the line: `false` when no live worker is registered under
    /// `id`, or when this attempt was already stopped.
    pub fn stop_task(&self, id: &TaskId) -> bool {
        match self.lock_controls().get_mut(id).and_then(|h| h.stop.take()) {
            Some(tx) => tx.send(()).is_ok(),
            None => false,
        }
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
        async fn run(
            &self,
            task: &Task,
            workspace_root: &Path,
            _controls: WorkerControls,
        ) -> WorkerOutcome {
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

        let task = Task::new("t1", "title", "prompt").isolated();
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
        async fn run(
            &self,
            task: &Task,
            _workspace_root: &Path,
            _controls: WorkerControls,
        ) -> WorkerOutcome {
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
        // a -> {b, c} -> d: two middle tasks run concurrently in one wave.
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
    async fn enforced_budget_stops_a_wide_single_wave_fan_out() {
        // THE fleet-budget P1: six INDEPENDENT tasks (no deps) all land in one
        // wave — the common `stella fleet` shape where every positional prompt
        // is its own task. Each costs 0.5 under an enforced $1.00 cap. The old
        // code metered spend only AFTER each worker and only gated the NEXT
        // wave, so this single wave ran all six to completion and spent
        // 6 × 0.5 = $3.00 — 3× the cap. The pre-dispatch gate in `dispatch()`
        // re-checks the shared parent guard as each task claims its slot, so
        // once cumulative spend passes the cap the remaining tasks are skipped.
        //
        // Serialize the wave (max_concurrency = 1) so the trip point is exact:
        // a,b run (spend 0.5, 1.0 == cap → Continue), c runs (spend 1.5 > cap),
        // then d,e,f each evaluate over-cap and are skipped before running.
        let plan = Plan::new(vec![
            Task::new("a", "a", "p"),
            Task::new("b", "b", "p"),
            Task::new("c", "c", "p"),
            Task::new("d", "d", "p"),
            Task::new("e", "e", "p"),
            Task::new("f", "f", "p"),
        ]);
        let f = fleet(
            FakeWorker::new(0.5),
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Enforced, Some(1.0), None),
            FleetConfig::new("run1", "HEAD").with_max_concurrency(1),
        );

        let report = f.run_plan(&plan).await.unwrap();

        assert!(report.budget_aborted, "the wide wave must trip the cap");
        // Only three tasks ran; the fan-out did NOT spend 6 × 0.5.
        assert_eq!(
            report.completed.len(),
            3,
            "fan-out ran past the cap (completed = {:?})",
            report.completed
        );
        // Total ledger spend is bounded near the cap, not N × cap.
        let spent = f.ledger_total_spend().unwrap();
        assert!(
            spent <= 1.5 + 1e-9,
            "aggregate spend {spent} exceeded the bounded trip point"
        );
        // The three un-launched tasks are reported as skipped (not silently
        // dropped, not counted as failures), and never touched the ledger.
        assert_eq!(report.skipped.len(), 3, "skipped = {:?}", report.skipped);
        for id in ["d", "e", "f"] {
            assert!(
                report.skipped.contains(&id.to_string()),
                "{id} should be skipped: {:?}",
                report.skipped
            );
            assert!(
                f.ledger_commits_for_task(id).unwrap().is_empty(),
                "{id} must have no attempt/commit row — it never ran"
            );
        }
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
        let plan = Plan::new(vec![
            Task::new("t1", "t1", "p").isolated(),
            Task::new("t2", "t2", "p").isolated(),
        ]);
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

    // ---- per-task control: pause / resume / stop ----------------------

    /// A worker that observes its pause line: parks until the fleet flips
    /// it `true` (announcing the sighting on `saw_pause`), then until it
    /// flips back `false`, recording each value it saw — proof the
    /// pause/resume verbs reach the worker through the [`FleetWorker`]
    /// port.
    struct PauseProbeWorker {
        observed: Arc<StdMutex<Vec<bool>>>,
        /// Fired after the worker observes `true` — the test resumes only
        /// then, so the pause edge can never be overwritten unseen (a
        /// `join!` may poll the control arm before re-polling the worker).
        saw_pause: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl FleetWorker for PauseProbeWorker {
        async fn run(
            &self,
            task: &Task,
            _workspace_root: &Path,
            mut controls: WorkerControls,
        ) -> WorkerOutcome {
            // `wait_for` errors only if the fleet dropped the sender — then
            // nothing is recorded and the assertions below fail loudly.
            if let Ok(v) = controls.pause.wait_for(|paused| *paused).await {
                self.observed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(*v);
                self.saw_pause.notify_one();
            }
            if let Ok(v) = controls.pause.wait_for(|paused| !*paused).await {
                self.observed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(*v);
            }
            WorkerOutcome {
                cost_usd: 0.0,
                commits: Vec::new(),
                summary: format!("did {}", task.id),
                success: true,
            }
        }
    }

    /// A worker that runs until its stop line fires — proof `stop_task`
    /// cancels through the port, and that the line is consumed (a second
    /// stop on the same attempt is a stale no-op).
    struct StopProbeWorker;
    #[async_trait]
    impl FleetWorker for StopProbeWorker {
        async fn run(
            &self,
            _task: &Task,
            _workspace_root: &Path,
            controls: WorkerControls,
        ) -> WorkerOutcome {
            let stopped = controls.stop.await.is_ok();
            WorkerOutcome {
                cost_usd: 0.0,
                commits: Vec::new(),
                summary: if stopped { "stopped" } else { "never stopped" }.to_string(),
                // A stopped task must not read as success — it would unblock
                // dependents against work that never landed.
                success: !stopped,
            }
        }
    }

    fn control_fleet<W: FleetWorker>(worker: W) -> Fleet<W, OkGit, SeqClock> {
        Fleet::new(
            worker,
            WorktreeManager::new(OkGit::new(), "/repo"),
            Ledger::open_in_memory().unwrap(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            SeqClock::new(),
            FleetConfig::new("run1", "HEAD"),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn pause_and_resume_reach_the_worker_through_the_port() {
        let worker = PauseProbeWorker {
            observed: Arc::new(StdMutex::new(Vec::new())),
            saw_pause: Arc::new(tokio::sync::Notify::new()),
        };
        let observed = worker.observed.clone();
        let saw_pause = worker.saw_pause.clone();
        let f = control_fleet(worker);
        let task = Task::new("t1", "t1", "p").shared_tree();
        let id = "t1".to_string();

        // The first `join!` poll drives the dispatch far enough to register
        // the task's controls and park the worker on its pause watch, so
        // `pause_task` addresses a live worker; resuming waits for the
        // worker's own "saw the pause" signal so the `true` edge can never
        // be overwritten before it was observed.
        let (handle, ()) = tokio::join!(f.dispatch(&task), async {
            assert!(f.pause_task(&id), "a live worker accepts pause");
            saw_pause.notified().await;
            assert!(f.resume_task(&id), "a live worker accepts resume");
        });
        assert!(handle.unwrap().outcome.success);
        assert_eq!(
            observed.lock().unwrap().as_slice(),
            &[true, false],
            "the worker observed the pause, then the resume"
        );
    }

    #[tokio::test]
    async fn stop_fires_once_and_a_second_stop_is_a_stale_no_op() {
        let f = control_fleet(StopProbeWorker);
        let task = Task::new("t1", "t1", "p").shared_tree();
        let id = "t1".to_string();

        // The worker parks on its stop line before the verbs run (the
        // current-thread join polls the dispatch first); both stops land
        // while it is still parked, so the second exercises the consumed
        // line — not a deregistered one.
        let (handle, ()) = tokio::join!(f.dispatch(&task), async {
            assert!(f.stop_task(&id), "a live worker accepts the stop");
            assert!(!f.stop_task(&id), "the stop line is consumed");
        });
        let handle = handle.unwrap();
        assert_eq!(handle.outcome.summary, "stopped");
        assert!(
            !handle.outcome.success,
            "a stopped attempt is not a success"
        );
    }

    #[test]
    fn control_verbs_on_an_unknown_task_are_stale_no_ops() {
        let f = control_fleet(FakeWorker::new(0.0));
        let ghost = "ghost".to_string();
        assert!(!f.pause_task(&ghost));
        assert!(!f.resume_task(&ghost));
        assert!(!f.stop_task(&ghost));
    }

    #[tokio::test]
    async fn control_handles_are_removed_after_the_worker_settles() {
        let f = control_fleet(FakeWorker::new(0.0));
        let task = Task::new("t1", "t1", "p").shared_tree();
        f.dispatch(&task).await.unwrap();
        let id = "t1".to_string();
        assert!(!f.pause_task(&id), "a settled task has no live pause line");
        assert!(!f.resume_task(&id));
        assert!(!f.stop_task(&id), "a settled task has no live stop line");
    }

    // ---- cache-TTL-aware scheduling (#269) -----------------------------

    #[test]
    fn ready_order_is_unchanged_without_a_warmth_lookup() {
        let f = fleet(
            FakeWorker::new(0.0),
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            FleetConfig::new("run1", "HEAD"),
        );
        let (a, b) = (Task::new("a", "a", "p"), Task::new("b", "b", "p"));
        let order = f.order_ready_by_warmth(vec![&a, &b]);
        assert_eq!(
            order.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    /// End-to-end proof `run_plan` actually consults the warmth order: three
    /// independent (no `depends_on`) tasks land in one ready wave,
    /// `max_concurrency: 1` makes `run_wave`'s `buffer_unordered` strictly
    /// serial, so `FakeWorker`'s recorded dispatch order IS the warmth order
    /// — fails on the pre-#269 `Fleet` (no `with_cache_warmth` to call).
    #[tokio::test]
    async fn run_plan_dispatches_a_serial_wave_warmest_first() {
        let plan = Plan::new(vec![
            Task::new("cold", "cold", "p"),
            Task::new("hot", "hot", "p"),
            Task::new("warm", "warm", "p"),
        ]);
        let worker = FakeWorker::new(0.0);
        let seen = worker.seen_roots.clone();
        let f = fleet(
            worker,
            OkGit::new(),
            BudgetGuard::new(BudgetMode::Observed, None, None),
            FleetConfig::new("run1", "HEAD").with_max_concurrency(1),
        )
        .with_cache_warmth(Box::new(|id: &TaskId| match id.as_str() {
            "cold" => Some(280),
            "hot" => Some(10),
            "warm" => Some(90),
            _ => None,
        }));
        f.run_plan(&plan).await.unwrap();
        let dispatched: Vec<String> = seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(dispatched, vec!["hot", "warm", "cold"]);
    }
}
