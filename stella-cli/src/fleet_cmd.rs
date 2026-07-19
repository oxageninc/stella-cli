//! `stella fleet` — multi-agent fan-out from the CLI, wired through
//! `stella-fleet`'s one dispatch seam: a DAG of tasks (from positional
//! prompts or a `--plan` file), a git worktree per isolated task, wave
//! scheduling with bounded concurrency, and every attempt/commit/dollar
//! stamped into the SQLite ledger (`.stella/fleet.db`). A task that declares
//! `claims` (workspace-relative paths it will touch) holds them as
//! cooperative file locks in `.stella/store.db` for the attempt's duration —
//! a path another task (or another run) already claims fails that dispatch
//! by name instead of letting two agents edit the same file.
//!
//! Each worker is a full Stella engine turn (the raw step-loop) running in
//! its task's workspace with the standard tool registry — headless: no MCP,
//! no custom tools, no ask_user, so a worker can never block on stdin. The
//! parent `--budget` is enforced twice, per the fleet's contract: each child
//! runs under its own enforced guard, and the fleet stops launching new
//! waves once the metered total crosses the cap (in-flight siblings settle
//! first, never a mid-tool kill).
//!
//! Every worker also honors its `stella_fleet::WorkerControls`: the stop
//! line races the turn (the clean drop-at-await cancel the deck's
//! sub-sessions use) and the pause line gates the raw step-loop at the
//! engine's step boundary via a `TurnGate`. `stella fleet` itself drives
//! workers to completion — the control verbs (`Fleet::pause_task` /
//! `resume_task` / `stop_task`) exist for a supervisor; surfacing fleet
//! tasks as controllable deck lanes is the named follow-up in
//! `COMMAND_DECK_DESIGN.md`.
//!
//! Worktrees are deliberately left in place after the run — the branches
//! (`fleet/<task>`) carry the work product for the user to review and merge.
//! `git worktree list` shows them; the report names each one.
//!
//! With `--watch`, the run ends in the fleet PR/CI monitor
//! (`stella_fleet::Monitor` over the real `gh`): every branch that carries
//! successful work is watched to CI completion as a capped deferred wait,
//! its PR status is reconciled live, and a red branch fails the command.

use std::collections::HashSet;
use std::path::Path;

use colored::Colorize;
use stella_core::{Engine, TurnOutcome};
use stella_fleet::{
    CiWatchOutcome, CommitRecord, Fleet, FleetConfig, FleetRunReport, FleetWorker, GhCli, Ledger,
    Monitor, MonitorError, Plan, SystemGhCli, SystemGitCli, Task, TaskId, TimeoutReason,
    WatchConfig, WorkerControls, WorkerOutcome, WorktreeManager,
};
use stella_protocol::{AgentEvent, CompletionMessage, PrStatus};
use stella_tools::ToolRegistry;
use stella_tools::hook_runner::ShellHookRunner;
use tokio::sync::{mpsc, watch};

use crate::agent;
use crate::config::Config;
use crate::runtime::{SystemClock, TokioSleeper};
use crate::tui;

/// Cap on the per-task summary line so the report table stays a table.
const SUMMARY_CHARS: usize = 96;

/// Run a fleet: build/load the plan, dispatch it wave by wave, report —
/// then, with `watch`, hold the fleet PR/CI monitor on the branches.
#[allow(clippy::too_many_arguments)] // composition-root wiring; one caller
pub async fn run_fleet(
    cfg: &Config,
    prompts: &[String],
    plan_file: Option<&Path>,
    base_ref: Option<&str>,
    max_concurrency: usize,
    budget_limit: Option<f64>,
    watch: bool,
    use_pipeline: bool,
) -> Result<(), String> {
    let root = cfg.workspace_root.clone();
    let plan = load_plan(prompts, plan_file)?;
    plan.validate().map_err(|e| format!("invalid plan: {e}"))?;

    // Pin the base to a sha now: "HEAD" would silently drift as shared-tree
    // tasks commit, and every isolated branch should cut from the same base.
    let base_sha = git_stdout(
        &root,
        &["rev-parse", "--verify", base_ref.unwrap_or("HEAD")],
    )
    .await
    .map_err(|e| format!("cannot resolve the fleet base ref: {e}"))?;

    tui::section_header("Stella — fleet");
    println!(
        "  {} task(s) from {}, base {}, ≤{} concurrent\n",
        plan.tasks.len(),
        plan_file
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "the command line".to_string()),
        &base_sha[..12.min(base_sha.len())],
        max_concurrency.max(1),
    );
    for task in &plan.tasks {
        let deps = if task.depends_on.is_empty() {
            String::new()
        } else {
            format!(" (after {})", task.depends_on.join(", "))
        };
        println!("    {} {} — {}{deps}", "·".dimmed(), task.id, task.title);
    }
    println!();

    let dot_stella = root.join(".stella");
    std::fs::create_dir_all(&dot_stella)
        .map_err(|e| format!("could not create {}: {e}", dot_stella.display()))?;
    let ledger = Ledger::open(&dot_stella.join("fleet.db"))
        .map_err(|e| format!("could not open the fleet ledger: {e}"))?;

    // Millisecond + pid: two runs in the same second (scripted/CI) must not
    // share a ledger run id — `record_run` is INSERT OR REPLACE, so a
    // collision would merge both runs' accounting under one row. A pre-epoch
    // clock is a hard error rather than a silent fallback to a constant (which
    // would reintroduce the very collision this guards against).
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch — cannot mint a unique fleet run id")?
        .as_millis();
    let run_id = format!("fleet-{now_ms}-{}", std::process::id());
    let worker = EngineWorker {
        cfg: cfg.clone(),
        // Divide the aggregate cap across the concurrency width so one wave's
        // in-flight children can't collectively overshoot `--budget`.
        per_child_budget: budget_limit.map(|b| b / max_concurrency.max(1) as f64),
        use_pipeline,
        run_id: run_id.clone(),
    };
    let fleet = Fleet::new(
        worker,
        // The run id scopes every worktree/branch slug: task ids repeat
        // across runs (`t1`, `t2`, …) and worktrees are kept for review, so
        // an unscoped second run would collide on `git worktree add`.
        WorktreeManager::new(SystemGitCli, root.clone()).with_run_scope(&run_id),
        ledger,
        agent::build_budget_guard(budget_limit),
        SystemClock::new(),
        FleetConfig::new(&run_id, &base_sha).with_max_concurrency(max_concurrency.max(1)),
    )
    .map_err(|e| format!("could not start the fleet: {e}"))?;
    // File claims live in the workspace store (`.stella/store.db`), opened
    // only when the plan declares any: enforcing claims requires the store
    // (a claim silently unenforced defeats its purpose), but a claim-free
    // run must not grow a new failure mode.
    let fleet = if plan.tasks.iter().any(|t| !t.claims.is_empty()) {
        let store = stella_store::Store::open(&root).map_err(|e| {
            format!("this plan declares file claims but the workspace store cannot open: {e}")
        })?;
        fleet.with_claim_store(store)
    } else {
        fleet
    };

    let report = fleet
        .run_plan(&plan)
        .await
        .map_err(|e| format!("fleet run failed: {e}"))?;

    render_report(&plan, &report, &dot_stella);
    if report.budget_aborted {
        return Err(format!(
            "budget cap reached after ${:.4} — remaining waves were not launched",
            report.total_cost_usd()
        ));
    }

    // Post-fanout PR/CI watch (`--watch`): the fleet monitor over the real
    // `gh`. Only branches carrying successful work are watched — failed
    // tasks already fail the run below.
    let mut red_branches: Vec<String> = Vec::new();
    if watch {
        let targets = watch_targets(&report);
        if targets.is_empty() {
            println!(
                "  {}\n",
                "nothing to watch — no successful task landed commits".dimmed()
            );
        } else {
            let config = WatchConfig::default();
            let monitor =
                Monitor::new(SystemGhCli, Box::new(SystemClock::new())).with_config(config);
            println!(
                "  watching CI for {} fleet branch(es) — polling every {}s, wall cap {}m\n",
                targets.len(),
                config.poll_interval_ms / 1_000,
                config.max_total_ms / 60_000,
            );
            for (task_id, branch) in &targets {
                let watched = watch_branch(&monitor, task_id, branch).await;
                render_watch_line(&watched);
                if !watched.is_green() {
                    red_branches.push(watched.branch);
                }
            }
            println!();
        }
    }

    if !report.all_succeeded() {
        return Err("one or more fleet tasks failed — see the report above".to_string());
    }
    if !red_branches.is_empty() {
        return Err(format!(
            "CI is not green for: {} — see the watch report above",
            red_branches.join(", ")
        ));
    }
    Ok(())
}

/// Build the plan: an explicit `--plan` file (JSON or TOML, deserializing
/// straight into `stella_fleet::Plan`), or one independent isolated task per
/// positional prompt.
fn load_plan(prompts: &[String], plan_file: Option<&Path>) -> Result<Plan, String> {
    if let Some(path) = plan_file {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read plan {}: {e}", path.display()))?;
        return match path.extension().and_then(|x| x.to_str()) {
            Some("json") => serde_json::from_str::<Plan>(&raw)
                .map_err(|e| format!("invalid JSON plan {}: {e}", path.display())),
            Some("toml") => toml::from_str::<Plan>(&raw)
                .map_err(|e| format!("invalid TOML plan {}: {e}", path.display())),
            _ => Err(format!(
                "plan file must be .json or .toml, got {}",
                path.display()
            )),
        };
    }
    if prompts.is_empty() {
        return Err("no tasks: pass prompts as arguments or --plan <file>".to_string());
    }
    Ok(Plan::new(
        prompts
            .iter()
            .enumerate()
            .map(|(i, prompt)| {
                let title: String = prompt.chars().take(48).collect();
                Task::new(format!("t{}", i + 1), title, prompt.clone())
            })
            .collect(),
    ))
}

/// One fleet branch's post-fanout verdict: the capped CI watch outcome plus
/// the branch's reconciled PR status. `pr` is `None` when the branch has no
/// PR yet — branches are left for review, so that is a normal state, not an
/// error.
struct BranchWatch {
    task_id: TaskId,
    branch: String,
    ci: Result<CiWatchOutcome, MonitorError>,
    pr: Option<PrStatus>,
}

impl BranchWatch {
    /// Green iff CI completed with a passing overall conclusion — a timeout,
    /// a monitor error, and a failing conclusion are all red.
    fn is_green(&self) -> bool {
        matches!(
            &self.ci,
            Ok(CiWatchOutcome::Completed { conclusion, .. }) if !conclusion.is_failure()
        )
    }
}

/// The branches worth watching after the fan-out: every successful task that
/// landed commits, keyed by the branch its commits actually record (correct
/// for isolated worktrees and shared-tree tasks alike), deduped so a branch
/// shared by several tasks is watched once.
fn watch_targets(report: &FleetRunReport) -> Vec<(TaskId, String)> {
    let mut seen = HashSet::new();
    report
        .handles
        .iter()
        .filter(|h| h.outcome.success)
        .filter_map(|h| {
            let branch = h.outcome.commits.last()?.branch.clone();
            seen.insert(branch.clone())
                .then(|| (h.task_id.clone(), branch))
        })
        .collect()
}

/// Watch one fleet branch: its CI to completion (the monitor's capped
/// deferred wait, L-E4), then a live PR-status reconcile — `gh pr view`
/// resolves a branch name to its PR.
async fn watch_branch<H: GhCli>(monitor: &Monitor<H>, task_id: &str, branch: &str) -> BranchWatch {
    let ci = monitor.watch_ci(branch).await;
    let pr = monitor.pr_status(branch).await.ok();
    BranchWatch {
        task_id: task_id.to_string(),
        branch: branch.to_string(),
        ci,
        pr,
    }
}

/// One report line per watched branch: verdict mark, CI outcome, PR status.
fn render_watch_line(watch: &BranchWatch) {
    let mark = if watch.is_green() {
        "✓".green()
    } else {
        "✗".red()
    };
    let ci = match &watch.ci {
        Ok(CiWatchOutcome::Completed {
            conclusion,
            summary,
        }) => {
            let verdict = if conclusion.is_failure() {
                "red"
            } else {
                "green"
            };
            format!("CI {verdict} — {summary}")
        }
        Ok(CiWatchOutcome::TimedOut {
            reason,
            last_observed,
            waited_ms,
        }) => {
            let reason = match reason {
                TimeoutReason::CumulativeCap => "cumulative cap",
                TimeoutReason::Stalled => "stalled",
                TimeoutReason::NoRunsStarted => "no CI runs started",
            };
            format!(
                "CI watch timed out ({reason}) after {}m — last: {last_observed}",
                waited_ms / 60_000
            )
        }
        Err(e) => format!("CI watch failed: {e}"),
    };
    let pr = match watch.pr {
        Some(PrStatus::Draft) => "PR draft",
        Some(PrStatus::Open) => "PR open",
        Some(PrStatus::Merged) => "PR merged",
        Some(PrStatus::Closed) => "PR closed",
        None => "no PR",
    };
    println!(
        "  {mark} {} {} — {ci} · {pr}",
        watch.task_id.bold(),
        watch.branch.bright_magenta()
    );
}

/// The engine-backed [`FleetWorker`]: one turn per task (the staged pipeline
/// by default, or the raw step-loop with `--no-pipeline`), in the task's own
/// workspace, with the standard (headless) tool registry.
struct EngineWorker {
    cfg: Config,
    /// Per-child spend cap. Derived as `--budget / max_concurrency` (not the
    /// full `--budget`), so a wave of concurrent children can't each spend the
    /// whole cap and blow the aggregate — the parent fleet guard then enforces
    /// the true total, stopping further launches once it is crossed.
    per_child_budget: Option<f64>,
    use_pipeline: bool,
    /// The fleet run id — combined with the task id it forms the worker's
    /// lock-table identity (`<run>/<task>`), the SAME holder string the
    /// fleet's declared-claim acquisition uses, so a task's tool-level
    /// claim-on-first-write is re-entrant with its declared claims.
    run_id: String,
}

#[async_trait::async_trait]
impl FleetWorker for EngineWorker {
    async fn run(
        &self,
        task: &Task,
        workspace_root: &Path,
        controls: WorkerControls,
    ) -> WorkerOutcome {
        // The engine's turn future is deliberately not `Send` (it holds
        // provider futures and the retry jitter RNG across awaits), but the
        // fleet's worker port requires a `Send` future. Bridge the two by
        // giving each task its own OS thread with a current-thread runtime —
        // fleet workers are genuinely parallel — and awaiting the `Send`
        // half of a oneshot from the async side.
        let cfg = self.cfg.clone();
        let per_child_budget = self.per_child_budget;
        let use_pipeline = self.use_pipeline;
        let task = task.clone();
        let root = workspace_root.to_path_buf();
        let claim_holder = format!("{}/{}", self.run_id, task.id);
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("worker runtime failed to start: {e}"))
                .and_then(|rt| {
                    rt.block_on(run_task(
                        &cfg,
                        per_child_budget,
                        use_pipeline,
                        &task,
                        &root,
                        &claim_holder,
                        controls,
                    ))
                });
            let _ = tx.send(result);
        });
        let failed = |reason: String| WorkerOutcome {
            cost_usd: 0.0,
            commits: Vec::new(),
            summary: reason,
            success: false,
        };
        match rx.await {
            Ok(Ok(outcome)) => outcome,
            // A worker that can't even start (provider, git) is a failed
            // attempt with a named reason — never a panic, never a hang.
            Ok(Err(e)) => failed(format!("worker error: {e}")),
            Err(_) => failed("worker thread died before reporting".to_string()),
        }
    }
}

/// `stella_core::ports::TurnGate` over the task's pause line: the worker's
/// turn parks at its next step boundary while a supervisor holds the watch
/// at `true` (`Fleet::pause_task`) and continues on `false`
/// (`Fleet::resume_task`). A dropped sender (the fleet settled this task's
/// controls) reads as resumed — a worker must never park forever on
/// teardown.
///
/// A deliberate small twin of `subsession.rs`'s private `WatchGate` (the
/// deck's sub-session gate): the two adapters sit on opposite sides of the
/// deck/fleet boundary and share only this trivial shape, so a co-located
/// duplicate reads better than a shared item would.
struct WatchGate(watch::Receiver<bool>);

#[async_trait::async_trait]
impl stella_core::ports::TurnGate for WatchGate {
    async fn wait_if_paused(&self) {
        let mut rx = self.0.clone();
        while *rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// One worker turn in `root`, on the calling thread's runtime. When
/// `use_pipeline` is true (the default), the turn runs through the staged
/// pipeline (triage → recall → plan → witness → execute → verify → judge);
/// otherwise it falls back to the raw `Engine::run_turn` step-loop.
async fn run_task(
    cfg: &Config,
    budget_limit: Option<f64>,
    use_pipeline: bool,
    task: &Task,
    root: &Path,
    claim_holder: &str,
    controls: WorkerControls,
) -> Result<WorkerOutcome, String> {
    // Snapshot where this workspace starts so the commit report is
    // exactly this worker's commits — correct for isolated worktrees
    // (== the fleet base) and for sequential shared-tree tasks alike.
    let start_sha = git_stdout(root, &["rev-parse", "--verify", "HEAD"]).await?;

    // The COORDINATION store lives at the original workspace root — shared
    // by every worker of every fleet in this workspace, which is what makes
    // multiple fleets (and the deck) safe in ONE tree. Captured before the
    // per-worker root override below.
    let claims_store = agent::open_store(&cfg.workspace_root);

    let mut cfg = cfg.clone();
    cfg.workspace_root = root.to_path_buf();
    let provider = agent::build_provider(&cfg)?;
    let registry =
        ToolRegistry::new_detected(root.to_path_buf(), agent::registry_options(&cfg)).await;
    crate::rules::enforce_workspace_rules(&registry, root);
    // Claim-on-first-write (crate::claims): tool-level write claims + the
    // transient build lane, coordinated across every writer in the
    // workspace. Same holder as the fleet's declared claims — re-entrant.
    let claims = crate::claims::ClaimTap::new(&registry, claims_store, claim_holder);

    let mut messages = vec![CompletionMessage::system(
        // Each worker is its own session in its own workspace, so its
        // SessionStart hooks fire here, in the worktree.
        agent::with_session_hook_context(agent::build_system_prompt(&cfg, root), &cfg).await,
    )];
    // The raw step-loop path needs the task prompt as a user message in the
    // history; the pipeline path takes the goal separately and appends its own
    // volatile recall+goal message (L-E8), so it must not be pre-seeded here.
    if !use_pipeline {
        messages.push(CompletionMessage::user(&task.prompt));
    }
    // Each child runs under its own enforced guard at the full cap; the
    // parent fleet guard additionally stops new waves on the metered sum.
    let mut budget = agent::build_budget_guard(budget_limit);
    budget.begin_turn();

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    // The task's control lines (stella-fleet's `WorkerControls`). The stop
    // wait mirrors `subsession.rs::run_worker` exactly: a dropped sender
    // (the fleet settled this task's handle — no supervisor will ever
    // signal) must not read as a stop, so the wait parks forever on a
    // closed channel and the work always wins the race.
    let WorkerControls { pause, stop } = controls;
    let stop_wait = async move {
        if stop.await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    /// How a raced future resolved — `subsession.rs`'s `RacedTurn` shape,
    /// generic because the two paths race different outcome types.
    enum Raced<T> {
        Outcome(T),
        Stopped,
    }
    /// The stopped attempt's summary. It reports with `success: false` so a
    /// stopped prerequisite never unblocks its dependents.
    const STOPPED: &str = "stopped by fleet control (Fleet::stop_task)";

    // `success`/`summary` are set by whichever path runs, then folded into
    // the WorkerOutcome after the channel drains.
    let (summary, success): (String, bool) = if use_pipeline {
        use stella_core::router::{CircuitBreaker, Router};
        use stella_pipeline::{
            AutoApproveGate, NoContextRecall, Pipeline, PipelineConfig, PipelinePorts,
            PipelineStatus,
        };
        let model_ref = stella_protocol::ModelRef::new(cfg.provider.id, cfg.model_id.clone());
        // Role wiring from `agent_engine_config` — fleet workers honor the
        // same triage/judge pins and per-role overrides as `stella run`.
        let wiring = agent::resolve_engine_wiring(&cfg, &model_ref);
        for notice in &wiring.notices {
            eprintln!("  ! {notice}");
        }
        let resolver = agent::RoleProviderResolver::new(
            &*provider,
            model_ref.clone(),
            &wiring.extra_providers,
        );
        let breaker = CircuitBreaker::new(Box::new(SystemClock::new()));
        let router = Router::new(wiring.pins.clone(), wiring.profiles.clone(), breaker);
        let repo_structure = agent::GitRepoStructure {
            root: root.to_path_buf(),
        };
        let repo_status = agent::GitRepoStatus {
            root: root.to_path_buf(),
        };
        let command_runner = agent::ShellCommandRunner {
            root: root.to_path_buf(),
        };
        let recall = NoContextRecall;
        let hook_runner = ShellHookRunner;
        let ports = PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &claims,
            recall: &recall,
            repo: &repo_structure,
            repo_status: &repo_status,
            commands: &command_runner,
            approvals: &AutoApproveGate,
            sleeper: &TokioSleeper,
            hooks: cfg
                .hooks
                .as_ref()
                .map(|h| (h, &hook_runner as &dyn stella_core::hooks::HookRunner)),
        };
        let config = PipelineConfig {
            engine: agent::pipeline_engine_config_for(&cfg),
            role_overrides: wiring.role_overrides.clone(),
            headless: true,
            headless_bypass_scope_review: true,
            ..PipelineConfig::default()
        };
        let pipeline = Pipeline::new(ports, tx.clone(), config);
        // The system prompt + task prompt are already in `messages`; the
        // pipeline appends its own volatile recall+goal message, so pass the
        // raw task prompt as the goal (the pipeline never re-reads `messages`
        // for its goal — it takes `task.prompt` directly).
        // The stop line races the whole staged run — the future drops at
        // its next await point, the same clean cancel the raw path gets.
        // Pause is NOT honored here yet: boundary-gating individual stages
        // needs a gate port on `PipelinePorts` (the existing named
        // follow-up) — only the raw step-loop path below holds a TurnGate.
        let raced = tokio::select! {
            result = pipeline.run(&task.prompt, &mut messages, &mut budget) => {
                Raced::Outcome(result)
            }
            _ = stop_wait => Raced::Stopped,
        };
        match raced {
            Raced::Outcome(Ok(outcome)) => match outcome.status {
                PipelineStatus::Completed => (truncate(&outcome.final_text), true),
                PipelineStatus::Aborted { reason } => (truncate(&reason), false),
            },
            Raced::Outcome(Err(e)) => (truncate(&e.to_string()), false),
            Raced::Stopped => (STOPPED.to_string(), false),
        }
    } else {
        // The pause line gates the raw step-loop at the engine's step
        // boundary (never mid-tool), and the stop line races the turn.
        let raced = {
            let gate = WatchGate(pause);
            let hook_runner = ShellHookRunner;
            let mut engine = Engine::with_sleeper(
                &*provider,
                &claims,
                agent::engine_config_for(&cfg),
                &TokioSleeper,
            )
            .with_gate(&gate);
            if let Some(hooks) = &cfg.hooks {
                engine = engine.with_hooks(hooks, &hook_runner);
            }
            tokio::select! {
                outcome = engine.run_turn(&mut messages, &mut budget, &tx) => {
                    Raced::Outcome(outcome)
                }
                _ = stop_wait => Raced::Stopped,
            }
        };
        match raced {
            Raced::Outcome(TurnOutcome::Completed { text, .. }) => (truncate(&text), true),
            Raced::Outcome(TurnOutcome::Aborted { reason }) => (truncate(&reason), false),
            Raced::Stopped => (STOPPED.to_string(), false),
        }
    };
    drop(tx);
    let _ = drain.await;
    claims.release_all();

    let spent = budget.session_spent_usd();
    let commits = collect_commits(root, &start_sha, &task.id).await;
    Ok(WorkerOutcome {
        cost_usd: spent,
        commits,
        summary,
        success,
    })
}

/// The commits this workspace gained since `start_sha`, oldest first, as
/// ledger-ready records.
async fn collect_commits(root: &Path, start_sha: &str, task_id: &str) -> Vec<CommitRecord> {
    let Ok(branch) = git_stdout(root, &["rev-parse", "--abbrev-ref", "HEAD"]).await else {
        return Vec::new();
    };
    let range = format!("{start_sha}..HEAD");
    let Ok(log) = git_stdout(
        root,
        &["log", "--reverse", "--format=%H%x1f%s%x1f%ct", &range],
    )
    .await
    else {
        return Vec::new();
    };
    log.lines()
        .filter_map(|line| {
            let mut parts = line.split('\u{1f}');
            let sha = parts.next()?.to_string();
            let message = parts.next()?.to_string();
            let timestamp_ms = parts.next()?.parse::<u64>().ok()?.saturating_mul(1000);
            Some(CommitRecord {
                sha,
                branch: branch.clone(),
                task_id: task_id.to_string(),
                message,
                timestamp_ms,
            })
        })
        .collect()
}

/// Run `git -C root <args>` and return trimmed stdout, or the stderr as the
/// error. Routed through fleet's [`stella_fleet::SystemGitCli`] — the
/// workspace's one git spawn point — so this path inherits its
/// non-interactive (`GIT_TERMINAL_PROMPT=0`) *and* `kill_on_drop`
/// discipline; the old local `Command` copy could leak a hung git child.
async fn git_stdout(root: &Path, args: &[&str]) -> Result<String, String> {
    use stella_fleet::{GitCli, SystemGitCli};
    let output = SystemGitCli
        .run(root, args)
        .await
        .map_err(|e| format!("git did not run: {e}"))?;
    if !output.success {
        return Err(output.stderr.trim().to_string());
    }
    Ok(output.stdout.trim().to_string())
}

fn truncate(s: &str) -> String {
    let one_line = s.replace('\n', " ");
    let mut out: String = one_line.chars().take(SUMMARY_CHARS).collect();
    if one_line.chars().count() > SUMMARY_CHARS {
        out.push('…');
    }
    out
}

/// The end-of-run report: per task its outcome, spend, commits, and (when
/// isolated) the worktree that holds the work, then the totals and where the
/// receipts live.
fn render_report(plan: &Plan, report: &FleetRunReport, dot_stella: &Path) {
    println!();
    for handle in &report.handles {
        let ok = handle.outcome.success;
        let mark = if ok { "✓".green() } else { "✗".red() };
        let title = plan
            .task(&handle.task_id)
            .map(|t| t.title.as_str())
            .unwrap_or("");
        println!(
            "  {mark} {} — {} (${:.4}, {} commit{})",
            handle.task_id.bold(),
            title,
            handle.outcome.cost_usd,
            handle.outcome.commits.len(),
            if handle.outcome.commits.len() == 1 {
                ""
            } else {
                "s"
            },
        );
        if let Some(worktree) = &handle.worktree {
            println!(
                "      {} {} @ {}",
                "↳".dimmed(),
                worktree.branch.bright_magenta(),
                worktree.path.display().to_string().dimmed()
            );
        }
        if !handle.outcome.summary.is_empty() {
            println!("      {}", handle.outcome.summary.dimmed());
        }
    }
    for (task_id, reason) in &report.dispatch_failures {
        println!(
            "  {} {} — dispatch failed: {}",
            "✗".red(),
            task_id.bold(),
            reason.dimmed()
        );
    }
    if !report.skipped.is_empty() {
        println!(
            "  {} skipped (dependency failed or budget stop): {}",
            "○".yellow(),
            report.skipped.join(", ").dimmed()
        );
    }
    println!(
        "\n  total ${:.4} · ledger {} · worktrees kept for review (`git worktree list`)\n",
        report.total_cost_usd(),
        dot_stella.join("fleet.db").display(),
    );
}

/// Where the fleet command's plan-shape belongs in docs/tests: a plan file is
/// the serde form of [`stella_fleet::Plan`].
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_prompts_become_independent_isolated_tasks() {
        let plan =
            load_plan(&["fix the login bug".into(), "add dark mode".into()], None).expect("plan");
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].id, "t1");
        assert!(plan.tasks[1].depends_on.is_empty());
        plan.validate().expect("valid");
    }

    #[test]
    fn toml_and_json_plans_deserialize_with_deps_and_isolation() {
        let toml_plan = r#"
            [[tasks]]
            id = "schema"
            title = "Add the users table"
            prompt = "add a users table migration"

            [[tasks]]
            id = "api"
            title = "Expose /users"
            prompt = "add the /users endpoint"
            depends_on = ["schema"]
            isolation = "shared_tree"
            claims = ["src/users.rs"]
        "#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plan.toml");
        std::fs::write(&path, toml_plan).expect("write");
        let plan = load_plan(&[], Some(&path)).expect("toml plan");
        assert_eq!(plan.tasks[1].depends_on, vec!["schema".to_string()]);
        assert_eq!(plan.tasks[1].claims, vec!["src/users.rs".to_string()]);
        assert!(plan.tasks[0].claims.is_empty(), "claims default to none");
        plan.validate().expect("valid");

        let json_path = dir.path().join("plan.json");
        std::fs::write(&json_path, serde_json::to_string(&plan).expect("serialize"))
            .expect("write");
        let round = load_plan(&[], Some(&json_path)).expect("json plan");
        assert_eq!(round, plan);
    }

    #[test]
    fn empty_input_and_unknown_extension_are_named_errors() {
        assert!(load_plan(&[], None).is_err());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plan.yaml");
        std::fs::write(&path, "tasks: []").expect("write");
        assert!(load_plan(&[], Some(&path)).is_err());
    }

    #[test]
    fn summaries_are_single_line_and_capped() {
        let long = "a\nb\n".repeat(200);
        let out = truncate(&long);
        assert!(!out.contains('\n'));
        assert!(out.chars().count() <= SUMMARY_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    // ---- the worker's control lines (stella-fleet WorkerControls) -------

    #[tokio::test]
    async fn watch_gate_parks_while_paused_and_releases_on_resume_or_teardown() {
        use stella_core::ports::TurnGate;
        let (tx, rx) = watch::channel(true);
        let gate = WatchGate(rx);
        let wait = gate.wait_if_paused();
        tokio::pin!(wait);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut wait)
                .await
                .is_err(),
            "a paused gate must park"
        );
        tx.send(false).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(500), wait)
            .await
            .expect("resume releases the gate");

        // A dropped sender (the fleet settled the task's controls) must
        // release, never park forever.
        let (tx2, rx2) = watch::channel(true);
        let gate2 = WatchGate(rx2);
        drop(tx2);
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            gate2.wait_if_paused(),
        )
        .await
        .expect("teardown releases the gate");
    }

    // ---- the post-fanout PR/CI watch (--watch) --------------------------

    use std::sync::{Arc, Mutex};

    use stella_core::BudgetOutcome;
    use stella_fleet::{CiConclusion, GhError, GhOutput, TaskHandle};

    /// A routed fake `gh`: `run list` answers with the scripted CI snapshot,
    /// `pr view` with the scripted PR json (or the real "no pull requests"
    /// failure shape when the branch has no PR). Records every call.
    struct RoutedGh {
        runs: String,
        pr: Option<String>,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl RoutedGh {
        fn new(runs: &str, pr: Option<&str>) -> Self {
            Self {
                runs: runs.to_string(),
                pr: pr.map(str::to_string),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl GhCli for RoutedGh {
        async fn run(&self, args: &[&str]) -> Result<GhOutput, GhError> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(args.iter().map(|s| s.to_string()).collect());
            if args.first() == Some(&"run") {
                Ok(GhOutput::ok(self.runs.clone()))
            } else {
                match &self.pr {
                    Some(json) => Ok(GhOutput::ok(json.clone())),
                    None => Ok(GhOutput::failed(1, "no pull requests found")),
                }
            }
        }
    }

    fn handle(task_id: &str, success: bool, branch: Option<&str>) -> TaskHandle {
        TaskHandle {
            task_id: task_id.to_string(),
            attempt_id: 1,
            outcome: WorkerOutcome {
                cost_usd: 0.0,
                commits: branch
                    .map(|b| {
                        vec![CommitRecord {
                            sha: format!("sha-{task_id}"),
                            branch: b.to_string(),
                            task_id: task_id.to_string(),
                            message: "m".to_string(),
                            timestamp_ms: 1,
                        }]
                    })
                    .unwrap_or_default(),
                summary: String::new(),
                success,
            },
            worktree: None,
            budget: BudgetOutcome::Continue,
        }
    }

    #[test]
    fn watch_targets_are_successful_committed_branches_watched_once() {
        let report = FleetRunReport {
            handles: vec![
                handle("t1", true, Some("fleet/t1-a")),
                // No commits → nothing on the branch to watch.
                handle("t2", true, None),
                // Failed → already fails the run; not watched.
                handle("t3", false, Some("fleet/t3-c")),
                // Same branch as t1 (shared-tree) → watched once.
                handle("t4", true, Some("fleet/t1-a")),
            ],
            ..FleetRunReport::default()
        };
        assert_eq!(
            watch_targets(&report),
            vec![("t1".to_string(), "fleet/t1-a".to_string())]
        );
    }

    #[tokio::test]
    async fn watch_branch_reports_green_ci_and_open_pr() {
        let gh = RoutedGh::new(
            r#"[{"status":"completed","conclusion":"success","name":"ci"}]"#,
            Some(r#"{"state":"OPEN","isDraft":false}"#),
        );
        let calls = gh.calls.clone();
        let monitor = Monitor::new(gh, Box::new(SystemClock::new()));

        let watched = watch_branch(&monitor, "t1", "fleet/t1-abc").await;
        assert!(watched.is_green());
        assert_eq!(watched.pr, Some(PrStatus::Open));

        // The CI poll and the PR reconcile both targeted the fleet branch.
        let calls = calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| c.first().map(String::as_str) == Some("run")
                    && c.iter().any(|a| a == "fleet/t1-abc"))
        );
        assert!(
            calls
                .iter()
                .any(|c| c.first().map(String::as_str) == Some("pr")
                    && c.iter().any(|a| a == "fleet/t1-abc"))
        );
    }

    #[tokio::test]
    async fn watch_branch_red_ci_and_a_missing_pr_are_states_not_errors() {
        let gh = RoutedGh::new(
            r#"[{"status":"completed","conclusion":"failure","name":"ci"}]"#,
            None,
        );
        let monitor = Monitor::new(gh, Box::new(SystemClock::new()));

        let watched = watch_branch(&monitor, "t1", "fleet/t1-abc").await;
        assert!(!watched.is_green());
        assert_eq!(watched.pr, None, "no PR for the branch is a normal state");
        assert!(matches!(
            watched.ci,
            Ok(CiWatchOutcome::Completed {
                conclusion: CiConclusion::Failure,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn watch_branch_treats_a_ci_timeout_as_red() {
        // No runs ever appear and the startup grace is already spent at the
        // first decision (elapsed >= grace with a 0ms grace) — the watch ends
        // as NoRunsStarted without sleeping, and the branch is red.
        let gh = RoutedGh::new("[]", None);
        let monitor = Monitor::new(gh, Box::new(SystemClock::new())).with_config(WatchConfig {
            poll_interval_ms: 1,
            max_total_ms: 60_000,
            stall_timeout_ms: 60_000,
            startup_grace_ms: 0,
        });

        let watched = watch_branch(&monitor, "t1", "fleet/t1-abc").await;
        assert!(!watched.is_green());
        assert!(matches!(
            watched.ci,
            Ok(CiWatchOutcome::TimedOut {
                reason: TimeoutReason::NoRunsStarted,
                ..
            })
        ));
    }
}
