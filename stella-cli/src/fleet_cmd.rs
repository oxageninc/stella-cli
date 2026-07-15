//! `stella fleet` — multi-agent fan-out from the CLI, wired through
//! `stella-fleet`'s one dispatch seam: a DAG of tasks (from positional
//! prompts or a `--plan` file), a git worktree per isolated task, wave
//! scheduling with bounded concurrency, and every attempt/commit/dollar
//! stamped into the SQLite ledger (`.stella/fleet.db`).
//!
//! Each worker is a full Stella engine turn (the raw step-loop) running in
//! its task's workspace with the standard tool registry — headless: no MCP,
//! no custom tools, no ask_user, so a worker can never block on stdin. The
//! parent `--budget` is enforced twice, per the fleet's contract: each child
//! runs under its own enforced guard, and the fleet stops launching new
//! waves once the metered total crosses the cap (in-flight siblings settle
//! first, never a mid-tool kill).
//!
//! Worktrees are deliberately left in place after the run — the branches
//! (`fleet/<task>`) carry the work product for the user to review and merge.
//! `git worktree list` shows them; the report names each one.

use std::path::Path;

use colored::Colorize;
use stella_core::ports::SystemClock;
use stella_core::{Engine, TurnOutcome};
use stella_fleet::{
    CommitRecord, Fleet, FleetConfig, FleetRunReport, FleetWorker, Ledger, Plan, SystemGitCli,
    Task, WorkerOutcome, WorktreeManager,
};
use stella_protocol::{AgentEvent, CompletionMessage};
use stella_tools::ToolRegistry;
use stella_tools::hook_runner::ShellHookRunner;
use tokio::sync::mpsc;

use crate::agent;
use crate::config::Config;
use crate::tui;

/// Cap on the per-task summary line so the report table stays a table.
const SUMMARY_CHARS: usize = 96;

/// Run a fleet: build/load the plan, dispatch it wave by wave, report.
pub async fn run_fleet(
    cfg: &Config,
    prompts: &[String],
    plan_file: Option<&Path>,
    base_ref: Option<&str>,
    max_concurrency: usize,
    budget_limit: Option<f64>,
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
        budget_limit,
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
    if !report.all_succeeded() {
        return Err("one or more fleet tasks failed — see the report above".to_string());
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

/// The engine-backed [`FleetWorker`]: one raw step-loop turn per task, in
/// the task's own workspace, with the standard (headless) tool registry.
struct EngineWorker {
    cfg: Config,
    budget_limit: Option<f64>,
}

#[async_trait::async_trait]
impl FleetWorker for EngineWorker {
    async fn run(&self, task: &Task, workspace_root: &Path) -> WorkerOutcome {
        // The engine's turn future is deliberately not `Send` (it holds
        // provider futures and the retry jitter RNG across awaits), but the
        // fleet's worker port requires a `Send` future. Bridge the two by
        // giving each task its own OS thread with a current-thread runtime —
        // fleet workers are genuinely parallel — and awaiting the `Send`
        // half of a oneshot from the async side.
        let cfg = self.cfg.clone();
        let budget_limit = self.budget_limit;
        let task = task.clone();
        let root = workspace_root.to_path_buf();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("worker runtime failed to start: {e}"))
                .and_then(|rt| rt.block_on(run_task(&cfg, budget_limit, &task, &root)));
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

/// One engine turn in `root`, on the calling thread's runtime.
async fn run_task(
    cfg: &Config,
    budget_limit: Option<f64>,
    task: &Task,
    root: &Path,
) -> Result<WorkerOutcome, String> {
    // Snapshot where this workspace starts so the commit report is
    // exactly this worker's commits — correct for isolated worktrees
    // (== the fleet base) and for sequential shared-tree tasks alike.
    let start_sha = git_stdout(root, &["rev-parse", "--verify", "HEAD"]).await?;

    let mut cfg = cfg.clone();
    cfg.workspace_root = root.to_path_buf();
    let provider = agent::build_provider(&cfg)?;
    let registry = ToolRegistry::new_detected(root.to_path_buf()).await;

    let mut messages = vec![
        CompletionMessage::system(
            // Each worker is its own session in its own workspace, so its
            // SessionStart hooks fire here, in the worktree.
            agent::with_session_hook_context(agent::build_system_prompt(root), &cfg).await,
        ),
        CompletionMessage::user(&task.prompt),
    ];
    // Each child runs under its own enforced guard at the full cap; the
    // parent fleet guard additionally stops new waves on the metered sum.
    let mut budget = agent::build_budget_guard(budget_limit);
    budget.begin_turn();

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let outcome = {
        let hook_runner = ShellHookRunner;
        let mut engine = Engine::new(&*provider, &registry, agent::engine_config_for(&cfg));
        if let Some(hooks) = &cfg.hooks {
            engine = engine.with_hooks(hooks, &hook_runner);
        }
        engine.run_turn(&mut messages, &mut budget, &tx).await
    };
    drop(tx);
    let _ = drain.await;

    let spent = budget.session_spent_usd();
    let (summary, success) = match outcome {
        TurnOutcome::Completed { text, .. } => (truncate(&text), true),
        TurnOutcome::Aborted { reason } => (truncate(&reason), false),
    };
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
/// error. Non-interactive by construction (`GIT_TERMINAL_PROMPT=0`).
async fn git_stdout(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|e| format!("git did not run: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
                worktree.branch.bright_blue(),
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
        "#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plan.toml");
        std::fs::write(&path, toml_plan).expect("write");
        let plan = load_plan(&[], Some(&path)).expect("toml plan");
        assert_eq!(plan.tasks[1].depends_on, vec!["schema".to_string()]);
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
}
