//! The agent loop — ties providers, tools, the step-driver, and TUI
//! together.
//!
//! `run_turn` drives `stella_core::Engine::run_turn` (the step-driver: one
//! model call per step, retry+backoff, compaction, loop detection, budget
//! checks — see `crates/stella-core/src/driver.rs`) and renders its
//! `AgentEvent` stream live via a spawned draining task. This replaces the
//! Phase 0/1 ad-hoc loop that lived here directly (no retry, no
//! compaction, no budget, a flat iteration cap instead of real loop
//! detection) — see `03-plan.md` Phase 2.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use colored::Colorize;
use stella_core::ports::{SystemClock, ToolExecutor};
use stella_core::retry::TokioSleeper;
use stella_core::router::{CircuitBreaker, ProviderProfile};
use stella_core::{
    BudgetGuard, CalibrationMap, Engine, EngineConfig, GoalConfig, GoalOutcome, RoleTable, Router,
    TurnOutcome,
};
use stella_mcp::{McpConfig, McpToolSet};
use stella_model::credential::ApiKey;
use stella_model::provider::Provider;
use stella_pipeline::{
    AutoApproveGate, CmdOutcome, CommandRunner, ContextRecallPort, NoContextRecall, Pipeline,
    PipelineConfig, PipelinePorts, PipelineStatus, ProviderResolver, RepoStructurePort,
    StdioApprovalGate,
};
use stella_protocol::event::BudgetMode;
use stella_protocol::{AgentEvent, CompletionMessage, ModelRef, Role, ToolOutput};
use stella_store::{Store, TelemetryRow};
use stella_tools::ToolRegistry;
use stella_tools::custom::{self, CustomTool, CustomToolSet};
use stella_tools::validate;
use tokio::sync::mpsc;

use crate::OutputFormat;
use crate::config::Config;
use crate::domains::{heuristic_domains, infer_domains};
use crate::interactive::{InteractiveToolSet, SkillRegistry, default_ask_io};
use crate::memory::{SessionMemory, inject_recall_block, turn_warrants_reflection};
use crate::tui;

const SYSTEM_PROMPT: &str = r#"You are Stella, a fast terminal coding agent. You help the user with software engineering tasks by reading files, writing code, running commands, and searching the codebase.

You have these tools available:
- read_file: Read a file with line numbers (supports offset/limit for ranges)
- write_file: Create or overwrite a file (creates parent dirs)
- edit_file: Replace an exact substring in a file (use replace_all for multiple)
- delete_file: Delete a file within the workspace
- bash: Run a shell command in the workspace root (with timeout)
- grep: Search file contents with regex (shells to ripgrep)
- glob: Find files matching a glob pattern
- build_project: Build with the workspace's own toolchain (cargo/npm/go/make)
- run_tests: Run the workspace's test suite
- verify_done: The definition of done — replays a new test against the previous code in a shadow worktree; it must fail there and pass on your change (WITNESS CONFIRMED). Use it to prove a change actually works, not just that the suite is green.
- ask_user: Ask the user a multiple-choice question when a decision is genuinely theirs to make (2-6 options; the UI always adds a free-text option automatically — never add an "Other" option yourself)
- search_skills: Search the public skills registry for reusable skills you don't have locally
- install_skill: Install a registry skill into the project (always requires the user's confirmation)

Some tools have prerequisites: issue tracking (create_issue/update_issue/close_issue/search_issues/start_work_on_issue) appears only when configured; ci_status requires the gh CLI. Use them when present.

Rules:
- Always read a file before editing it — never edit blind.
- Make minimal, surgical edits. Use edit_file, not write_file, for changes to existing files.
- After changing behavior, use run_tests to check the suite, and verify_done to prove the change with a witness test rather than trusting a green suite.
- Be concise in your responses. Show the user what you changed and why.
- If a task requires multiple steps, work through them systematically.
- When a choice is ambiguous AND getting it wrong would be costly, use ask_user rather than guessing; otherwise proceed with your best judgment."#;

/// The pipeline-mode system prompt: encodes a reproduce, localize, minimal
/// fix, verify methodology and rewards the fewest changed lines. Static
/// text so it rides the prompt cache (L-E8).
const PIPELINE_SYSTEM_PROMPT: &str = r#"You are Stella, a software engineering agent that fixes bugs and builds features with surgical precision.

You have these tools available:
- read_file: Read a file with line numbers (supports offset/limit for ranges)
- write_file: Create or overwrite a file (creates parent dirs)
- edit_file: Replace an exact substring in a file (use replace_all for multiple)
- delete_file: Delete a file within the workspace
- bash: Run a shell command in the workspace root (with timeout)
- grep: Search file contents with regex (shells to ripgrep)
- glob: Find files matching a glob pattern
- build_project: Build with the workspace's own toolchain (cargo/npm/go/make)
- run_tests: Run the workspace's test suite
- verify_done: The definition of done, replays a new test against the previous code in a shadow worktree; it must fail there and pass on your change (WITNESS CONFIRMED). Use it to prove a change actually works, not just that the suite is green.
- ask_user: Ask the user a multiple-choice question when a decision is genuinely theirs to make (2-6 options; the UI always adds a free-text option automatically, never add an "Other" option yourself)
- search_skills: Search the public skills registry for reusable skills you don't have locally
- install_skill: Install a registry skill into the project (always requires the user's confirmation)

Some tools have prerequisites: issue tracking (create_issue/update_issue/close_issue/search_issues/start_work_on_issue) appears only when configured; ci_status requires the gh CLI. Use them when present.

Methodology (always follow in order):
1. REPRODUCE: Run the failing test or reproduce the bug before touching any file. Never edit blind, you must see the actual error first.
2. LOCALIZE: Trace the error to its root cause. Read the failing code path. Use grep and glob to navigate precisely.
3. MINIMAL FIX: Make the smallest change that resolves the issue. No refactoring. No style changes. No "while I'm here" edits. One logical change.
4. VERIFY: Run the target test. If it passes, use verify_done to witness the change. If it fails, read the error and adjust.

Rules:
- Never change test files unless the task explicitly requires it.
- Never create backup files, scratch files, or debug artifacts.
- Prefer edit_file (surgical) over write_file (full rewrite).
- Always read a file before editing it, never edit blind.
- If you are editing more than 3 files for a single-task fix, you are overcomplicating it.
- Be concise in your responses. Show the user what you changed and why.
- When a choice is ambiguous AND getting it wrong would be costly, use ask_user rather than guessing; otherwise proceed with your best judgment."#;

/// Cap on memory characters appended to the system prompt — memories ride
/// the prompt cache on every call, so they must stay dense.
const MEMORY_PROMPT_BUDGET_CHARS: usize = 16_000;

/// Assemble the session's system prompt from a `base` instruction set plus
/// the workspace's saved memories. Memories are loaded ONCE per session and
/// concatenated in filename order so the resulting prefix is byte-stable
<<<<<<< HEAD
/// across every model call — that stability is what lets the whole prompt
/// (instructions + memories) ride the provider's prompt cache instead of
/// being re-billed. Memories saved mid-session deliberately do NOT appear
/// until the next session: hot-injecting them would invalidate the cached
/// prefix on every save. This coexists with `SessionMemory`'s per-turn
/// recall block (memory.rs) — the baked prefix carries durable lessons, the
/// recall block carries turn-relevant memories and skills.
pub(crate) fn build_system_prompt(workspace_root: &std::path::Path) -> String {
=======
/// across every model call — that stability preserves prompt-cache hits.
fn assemble_system_prompt(base: &str, workspace_root: &std::path::Path) -> String {
>>>>>>> 41325e9a4a9d778b2906cd2be26473dc260bd7b7
    let dir = workspace_root.join(".stella/memories");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
                .collect()
        })
        .unwrap_or_default();
    if files.is_empty() {
        return base.to_string();
    }
    files.sort();

    let mut memories = String::new();
    let mut used = 0usize;
    let mut dropped = 0usize;
    for file in &files {
        let Ok(body) = std::fs::read_to_string(file) else {
            continue;
        };
        let name = file
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("memory");
        let entry = format!(
            "
### {name}
{}
",
            body.trim()
        );
        let cost = entry.chars().count();
        if used + cost > MEMORY_PROMPT_BUDGET_CHARS {
            dropped += 1;
            continue;
        }
        used += cost;
        memories.push_str(&entry);
    }
    if memories.is_empty() {
        return base.to_string();
    }
    let mut prompt = format!(
        "{base}

Workspace memories (lessons from previous sessions — apply them):
{memories}"
    );
    if dropped > 0 {
        prompt.push_str(&format!(
            "
({dropped} additional memories exceeded the prompt budget and were omitted —              consolidate .stella/memories/ to bring them back)"
        ));
    }
    prompt
}

/// Shortcut: the raw step-loop system prompt plus workspace memories.
fn build_system_prompt(workspace_root: &std::path::Path) -> String {
    assemble_system_prompt(SYSTEM_PROMPT, workspace_root)
}

/// Shortcut: the pipeline-mode system prompt plus workspace memories.
fn build_pipeline_system_prompt(workspace_root: &std::path::Path) -> String {
    assemble_system_prompt(PIPELINE_SYSTEM_PROMPT, workspace_root)
}

/// Run a one-shot prompt. `use_pipeline` selects the staged pipeline (the
/// default) vs the raw step-loop (`--no-pipeline`).
pub async fn run_one_shot(
    cfg: &Config,
    prompt: &str,
    budget_limit: Option<f64>,
    format: OutputFormat,
    use_pipeline: bool,
) -> Result<(), String> {
    if use_pipeline {
        run_pipeline_one_shot(cfg, prompt, budget_limit, format).await
    } else {
        run_raw_one_shot(cfg, prompt, budget_limit, format).await
    }
}

/// Run a one-shot prompt through the staged pipeline: triage fast-paths,
/// split-context planning, repo-structure injection, the deterministic
/// verification ladder, and bounded revision.
async fn run_pipeline_one_shot(
    cfg: &Config,
    prompt: &str,
    budget_limit: Option<f64>,
    format: OutputFormat,
) -> Result<(), String> {
    let provider = build_provider(cfg)?;
    let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());

    let registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::new(cfg.workspace_root.clone()));
    populate_schema_index(&registry, &cfg.workspace_root);
    let mcp = connect_mcp(cfg, registry.clone(), format == OutputFormat::Text).await;
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set,
        None => &*registry,
    };
    let custom_tools = discover_custom_tools(cfg, format == OutputFormat::Text);
    let store = open_store(&cfg.workspace_root);

    if format == OutputFormat::Text {
        tui::section_header("Stella (pipeline)");
        println!(
            "  {}
",
            prompt.dimmed()
        );
    }

    let turn_start = Instant::now();
    let execution = begin_execution(&store, "pipeline", prompt, cfg);

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let renderer = spawn_renderer(rx, format, execution.clone(), cfg.provider.id.to_string());

    let resolver = SingleProviderResolver {
        provider,
        model_ref: model_ref.clone(),
    };

    let mut messages = vec![CompletionMessage::system(build_pipeline_system_prompt(
        &cfg.workspace_root,
    ))];
    let mut memory = SessionMemory::open(&cfg.workspace_root, format == OutputFormat::Text);
    if let Some(m) = &memory {
        inject_recall_block(&mut messages, m.recall_block(prompt).await);
    }
    let mut budget = build_budget_guard(budget_limit);
    budget.begin_turn();

    let result = {
        let customs = CustomToolSet::new(base_tools, custom_tools, cfg.workspace_root.clone());
        let tools = InteractiveToolSet::new(
            &customs,
            tx.clone(),
            default_ask_io(format == OutputFormat::Text),
        )
        .with_skill_registry(SkillRegistry::from_env(cfg.workspace_root.clone()));

        let repo_structure = GitRepoStructure {
            root: cfg.workspace_root.clone(),
        };
        let command_runner = ShellCommandRunner {
            root: cfg.workspace_root.clone(),
        };

        let profile = ProviderProfile::new(
            cfg.provider.id,
            model_ref.clone(),
            model_ref.clone(),
            model_ref,
        );
        let breaker = CircuitBreaker::new(Box::new(SystemClock::new()));
        let router = Router::new(RoleTable::new(), vec![profile], breaker);

        let is_text = format == OutputFormat::Text;
        let pipeline_config = PipelineConfig {
            engine: EngineConfig::default(),
            headless: !is_text,
            headless_bypass_scope_review: !is_text,
            ..Default::default()
        };

        let stdio_gate = StdioApprovalGate;
        let no_recall = NoContextRecall;
        // The workspace memory doubles as the pipeline's recall port so the
        // split-context planner sees the same durable lessons the worker's
        // recall block carries; no open store -> no frames (L-C6).
        let recall: &dyn ContextRecallPort = match &memory {
            Some(m) => m,
            None => &no_recall,
        };
        let ports = PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall,
            repo: &repo_structure,
            commands: &command_runner,
            approvals: if is_text {
                &stdio_gate
            } else {
                &AutoApproveGate
            },
            sleeper: &TokioSleeper,
        };

        let pipeline = Pipeline::new(ports, tx.clone(), pipeline_config);
        pipeline.run(prompt, &mut messages, &mut budget).await
    };

    drop(tx);
    let collected = renderer.await.unwrap_or_default();

    let files = registry.files_touched();
    if let Some((store, id)) = &execution {
        let _ = store.record_files_touched(*id, &files);
        let (outcome_label, cost) = match &result {
            Ok(outcome) => {
                let label = match outcome.status {
                    PipelineStatus::Completed => "completed",
                    PipelineStatus::Aborted { .. } => "aborted",
                };
                (label, outcome.total_cost_usd)
            }
            Err(_) => ("error", 0.0),
        };
        let _ = store.finish_execution(*id, outcome_label, cost);
    }

    if result.is_ok()
        && turn_warrants_reflection(&messages)
        && let Some(m) = &mut memory
    {
        m.reflect_and_record(resolver.provider(), &messages, format != OutputFormat::Text)
            .await;
    }

    if let Some(set) = &mcp {
        set.close_all().await;
    }

    match &result {
        Ok(outcome) => {
            if format == OutputFormat::Json {
                let status_str = match outcome.status {
                    PipelineStatus::Completed => "completed",
                    PipelineStatus::Aborted { .. } => "aborted",
                };
                let reason_str = match &outcome.status {
                    PipelineStatus::Completed => None,
                    PipelineStatus::Aborted { reason } => Some(reason.clone()),
                };
                let summary = serde_json::json!({
                    "status": status_str,
                    "text": outcome.final_text,
                    "cost_usd": outcome.total_cost_usd,
                    "reason": reason_str,
                    "task_class": format!("{:?}", outcome.task_class),
                    "verdict": outcome.verdict.as_ref().map(|v| serde_json::json!({
                        "passed": v.passed,
                        "deterministic": v.deterministic,
                        "summary": v.summary,
                    })),
                    "revisions": outcome.revisions,
                    "candidates_run": outcome.candidates_run,
                    "model": format!("{}/{}", cfg.provider.id, cfg.model_id),
                    "events": collected,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&summary).unwrap_or_else(|e| format!(
                        "{{\"status\":\"error\",\"reason\":\"serialize: {e}\"}}"
                    ))
                );
            }

            if format == OutputFormat::Text {
                tui::files_touched_panel(&files);
                tui::cost_summary(
                    outcome.total_cost_usd,
                    &format!("{}/{}", cfg.provider.id, cfg.model_id),
                    turn_start.elapsed(),
                );
                println!();
            }

            match &outcome.status {
                PipelineStatus::Completed => Ok(()),
                PipelineStatus::Aborted { reason } => Err(reason.clone()),
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

// -----------------------------------------------------------------------
// Pipeline port adapters
// -----------------------------------------------------------------------

/// Owns the boxed provider so the reference returned to the pipeline is
/// valid for the pipeline's entire lifetime.
struct SingleProviderResolver {
    provider: Box<dyn Provider>,
    model_ref: ModelRef,
}

impl SingleProviderResolver {
    fn provider(&self) -> &dyn Provider {
        &*self.provider
    }
}

impl ProviderResolver for SingleProviderResolver {
    fn provider_for(&self, model: &ModelRef) -> Option<&dyn Provider> {
        if model == &self.model_ref {
            Some(&*self.provider)
        } else {
            None
        }
    }
}

/// Repo-structure summary via `git ls-files` for the planner's split context.
struct GitRepoStructure {
    root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl RepoStructurePort for GitRepoStructure {
    async fn structure_summary(&self) -> String {
        let output = tokio::process::Command::new("git")
            .args(["ls-files"])
            .current_dir(&self.root)
            .output()
            .await;
        match output {
            Ok(out) if out.status.success() => {
                render_file_tree(&String::from_utf8_lossy(&out.stdout), 200)
            }
            _ => String::new(),
        }
    }
}

fn render_file_tree(files: &str, max_lines: usize) -> String {
    let mut paths: Vec<&str> = files.lines().filter(|l| !l.is_empty()).collect();
    paths.sort_unstable();
    if paths.is_empty() {
        return String::new();
    }
    let total = paths.len();
    let mut out: String = paths
        .iter()
        .take(max_lines)
        .cloned()
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    if total > max_lines {
        out.push_str(&format!(
            "
... ({} more files)",
            total - max_lines
        ));
    }
    out
}

/// Runs shell commands for the verification ladder (flip oracle tests, diff).
struct ShellCommandRunner {
    root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl CommandRunner for ShellCommandRunner {
    async fn run(&self, command: &str) -> CmdOutcome {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&self.root);
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

/// Run a one-shot prompt through the raw step-loop (Engine::run_turn).
/// Selected via `--no-pipeline`.
async fn run_raw_one_shot(
    cfg: &Config,
    prompt: &str,
    budget_limit: Option<f64>,
    format: OutputFormat,
) -> Result<(), String> {
    let provider = build_provider(cfg)?;
    // Concrete `Arc<ToolRegistry>` (not `Arc<dyn ToolExecutor>`) so the
    // files-touched ledger is reachable after the turn — the trait object
    // hides it. It still coerces to `&dyn ToolExecutor` for the engine.
    let registry: std::sync::Arc<ToolRegistry> =
        std::sync::Arc::new(ToolRegistry::new(cfg.workspace_root.clone()));
    populate_schema_index(&registry, &cfg.workspace_root);
    let mcp = connect_mcp(cfg, registry.clone(), format == OutputFormat::Text).await;
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set,
        None => &*registry,
    };
    let custom_tools = discover_custom_tools(cfg, format == OutputFormat::Text);
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);
    let calibration = seed_calibration(&store, cfg);

    if format == OutputFormat::Text {
        tui::section_header("Stella");
        println!("  {}\n", prompt.dimmed());
    }

    let mut messages = vec![
        CompletionMessage::system(build_system_prompt(&cfg.workspace_root)),
        CompletionMessage::user(prompt),
    ];

    // The self-improvement loop (memory.rs): recall relevant memories +
    // skills into a volatile block after the stable system prefix (L-E8)…
    let mut memory = SessionMemory::open(&cfg.workspace_root, format == OutputFormat::Text);
    if let Some(m) = &memory {
        inject_recall_block(&mut messages, m.recall_block(prompt).await);
    }

    let outcome = run_turn(
        &*provider,
        base_tools,
        &custom_tools,
        &registry,
        &mut messages,
        &mut budget,
        &calibration,
        cfg,
        format,
        &store,
        "run",
        prompt,
    )
    .await;
    // …and reflect on the completed turn, recording domain-tagged lessons
    // (recurring ones auto-promote to SKILL.md files). Best-effort: never
    // fails or slows the turn that just ran. Gated on `turn_warrants_reflection`
    // so a tool-free turn never spends a model call to mine lessons it can't
    // have produced (the whole one-shot transcript IS this turn).
    if outcome.is_ok()
        && turn_warrants_reflection(&messages)
        && let Some(m) = &mut memory
    {
        m.reflect_and_record(&*provider, &messages, format != OutputFormat::Text)
            .await;
    }
    if let Some(set) = &mcp {
        set.close_all().await;
    }
    outcome
}

/// Run a one-shot goal loop (non-interactive): work in judged rounds until
/// a judge model assesses the goal as met (`stella goal "…"`, and `stella
/// monitor` composed on top of it). The judge is routed by role: when a
/// second provider family is configured (BYOK), `run_goal_turn` builds a
/// role `Router` and resolves `Role::Judge` to a DIFFERENT family than the
/// worker for bias-resistant assessment (`07-model-matrix.md` §1); with a
/// single family it stays the worker provider, identical to before. The
/// worker turns get the full tool stack (MCP + custom + interactive +
/// skills), same as `run_one_shot`.
pub async fn run_goal_cmd(
    cfg: &Config,
    goal: &str,
    budget_limit: Option<f64>,
) -> Result<(), String> {
    let provider = build_provider(cfg)?;
    let registry: std::sync::Arc<ToolRegistry> =
        std::sync::Arc::new(ToolRegistry::new(cfg.workspace_root.clone()));
    populate_schema_index(&registry, &cfg.workspace_root);
    let mcp = connect_mcp(cfg, registry.clone(), true).await;
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set,
        None => &*registry,
    };
    let custom_tools = discover_custom_tools(cfg, true);
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);
    let calibration = seed_calibration(&store, cfg);

    tui::section_header("Stella — goal mode");
    println!("  {}\n", goal.dimmed());

    let mut messages = vec![CompletionMessage::system(build_system_prompt(
        &cfg.workspace_root,
    ))];
    let mut memory = SessionMemory::open(&cfg.workspace_root, true);
    if let Some(m) = &memory {
        inject_recall_block(&mut messages, m.recall_block(goal).await);
    }

    let outcome = run_goal_turn(
        &*provider,
        base_tools,
        &custom_tools,
        &registry,
        &mut messages,
        &mut budget,
        &calibration,
        cfg,
        &store,
        goal,
    )
    .await;
    if outcome.is_ok()
        && turn_warrants_reflection(&messages)
        && let Some(m) = &mut memory
    {
        m.reflect_and_record(&*provider, &messages, false).await;
    }
    if let Some(set) = &mcp {
        set.close_all().await;
    }
    outcome
}

/// Run an interactive REPL session. `budget_limit` is per-session: the
/// `BudgetGuard`'s session-scoped total accumulates across every turn in
/// the conversation, while `BudgetGuard::begin_turn` resets only the
/// turn-scoped counter at the start of each one.
pub async fn run_interactive(cfg: &Config, budget_limit: Option<f64>) -> Result<(), String> {
    let provider = build_provider(cfg)?;
    let registry: std::sync::Arc<ToolRegistry> =
        std::sync::Arc::new(ToolRegistry::new(cfg.workspace_root.clone()));
    let mcp = connect_mcp(cfg, registry.clone(), true).await;
    populate_schema_index(&registry, &cfg.workspace_root);
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set,
        None => &*registry,
    };
    let custom_tools = discover_custom_tools(cfg, true);
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);
    // Session-scoped like `budget`: seeded once from prior sessions'
    // telemetry, then sharpened by every turn in this REPL.
    let calibration = seed_calibration(&store, cfg);

    tui::welcome_banner(
        cfg.provider.id,
        &cfg.model_id,
        &cfg.workspace_root.display().to_string(),
    );

    // Built once per session and reused verbatim on /clear — the byte-stable
    // prefix (instructions + baked memories) is the prompt-cache contract
    // (see build_system_prompt).
    let system_prompt = build_system_prompt(&cfg.workspace_root);
    let mut messages = vec![CompletionMessage::system(system_prompt.clone())];
    let mut memory = SessionMemory::open(&cfg.workspace_root, true);

    loop {
        // The rocket-vs-UFO duel animates one line above the prompt while
        // the REPL waits for input (TTY only; STELLA_FUN=0 opts out), and is
        // stopped the moment a line arrives so nothing ever animates while a
        // turn's event stream is printing — see tui.rs's module doc for why
        // that boundary matters.
        let duel = tui::PromptDuel::start();

        print!("{} ", ">".bright_cyan().bold());
        std::io::stdout().flush().map_err(|e| e.to_string())?;

        let mut input = String::new();
        let read = std::io::stdin().read_line(&mut input);
        if let Some(duel) = duel {
            duel.stop();
        }
        match read {
            Ok(0) => break, // EOF (Ctrl+D)
            Ok(_) => {}
            Err(e) => return Err(format!("read error: {e}")),
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "/exit" || input == "/quit" || input == "exit" {
            break;
        }
        if input == "/models" {
            cfg.print_models();
            continue;
        }
        if input == "/config" {
            cfg.print_config();
            continue;
        }
        if input == "/help" {
            print_help();
            continue;
        }
        if input == "/clear" {
            messages = vec![CompletionMessage::system(system_prompt.clone())];
            println!("  {}\n", "conversation cleared".dimmed());
            continue;
        }
        if input == "/files" {
            tui::files_touched_panel(&registry.files_touched());
            println!();
            continue;
        }
        if let Some(title) = input.strip_prefix("/rename ") {
            tui::rename_tab(title.trim());
            println!(
                "  {}\n",
                format!("tab renamed to `{}`", title.trim()).dimmed()
            );
            continue;
        }
        if let Some(color) = input.strip_prefix("/color ") {
            let name = color.trim();
            if tui::set_accent(name) {
                // Acknowledge in the newly-set accent itself — the welcome
                // banner uses a fixed palette and can't reflect the accent,
                // so re-printing it would silently ignore the change.
                println!(
                    "  {} {}\n",
                    "◆".color(tui::accent()),
                    format!("accent set to {name}").color(tui::accent()).bold()
                );
            }
            continue;
        }
        if input == "/goal" {
            println!(
                "  {}\n",
                "usage: /goal <what must be true when done>".dimmed()
            );
            continue;
        }
        if let Some(goal) = input.strip_prefix("/goal ") {
            let goal = goal.trim();
            if goal.is_empty() {
                println!(
                    "  {}\n",
                    "usage: /goal <what must be true when done>".dimmed()
                );
                continue;
            }
            println!();
            if let Some(m) = &memory {
                let block = m.recall_block(goal).await;
                inject_recall_block(&mut messages, block);
            }
            // Everything the goal loop appends past here is this turn's work,
            // gating reflection on it (see `turn_warrants_reflection`).
            let turn_start = messages.len();
            if let Err(e) = run_goal_turn(
                &*provider,
                base_tools,
                &custom_tools,
                &registry,
                &mut messages,
                &mut budget,
                &calibration,
                cfg,
                &store,
                goal,
            )
            .await
            {
                eprintln!("  {} {}\n", "Error:".red().bold(), e);
            } else if turn_warrants_reflection(&messages[turn_start..])
                && let Some(m) = &mut memory
            {
                m.reflect_and_record(&*provider, &messages, false).await;
            }
            continue;
        }

        messages.push(CompletionMessage::user(input));
        println!();

        if let Some(m) = &memory {
            let block = m.recall_block(input).await;
            inject_recall_block(&mut messages, block);
        }

        // Everything `run_turn` appends past here is this turn's work; the
        // reflection gate reads only that slice (see `turn_warrants_reflection`).
        let turn_start = messages.len();
        if let Err(e) = run_turn(
            &*provider,
            base_tools,
            &custom_tools,
            &registry,
            &mut messages,
            &mut budget,
            &calibration,
            cfg,
            OutputFormat::Text,
            &store,
            "chat",
            input,
        )
        .await
        {
            eprintln!("  {} {}\n", "Error:".red().bold(), e);
        } else if turn_warrants_reflection(&messages[turn_start..])
            && let Some(m) = &mut memory
        {
            m.reflect_and_record(&*provider, &messages, false).await;
        }
    }

    if let Some(set) = &mcp {
        set.close_all().await;
    }
    println!("\n  {}", "Goodbye! ✦".magenta());
    Ok(())
}

/// Build the workspace code-graph index into `.stella/codegraph.db` (the
/// `stella-graph` tree-sitter indexer). This is the data side of `init`: the
/// domain taxonomy tags graph nodes/edges, and the index makes the symbols +
/// import edges queryable as `ContextFrame`s by the context plane.
///
/// Idempotent and best-effort: a failure degrades to a warning (init still
/// succeeds, offline included) — the graph can always be rebuilt on a later
/// `init` once a toolchain/parser is available.
fn build_code_graph(workspace_root: &std::path::Path) {
    let dot_stella = workspace_root.join(".stella");
    if let Err(e) = std::fs::create_dir_all(&dot_stella) {
        eprintln!(
            "  {} could not create .stella for the code graph: {e} — skipped",
            "!".yellow()
        );
        return;
    }
    let db_path = dot_stella.join("codegraph.db");
    println!("  {} indexing code graph…", "◈".cyan());
    match stella_graph::CodeGraph::open(workspace_root, &db_path) {
        Ok(graph) => match graph.index_all() {
            Ok(stats) => {
                println!(
                    "  {} code graph: {} symbols, {} imports across {} file{} \
                     ({} parsed, {} unchanged)",
                    "✓".green(),
                    stats.symbols,
                    stats.imports,
                    stats.files_parsed + stats.files_skipped_unchanged,
                    if stats.files_parsed + stats.files_skipped_unchanged == 1 {
                        ""
                    } else {
                        "s"
                    },
                    stats.files_parsed,
                    stats.files_skipped_unchanged,
                );
                graph.shutdown();
            }
            Err(e) => {
                eprintln!(
                    "  {} code-graph indexing failed: {e} — run `stella init` again to retry",
                    "!".yellow()
                );
            }
        },
        Err(e) => {
            eprintln!(
                "  {} code-graph store unavailable: {e} — skipped",
                "!".yellow()
            );
        }
    }
}

/// Open the code graph (read-only) and populate the tool registry's schema
/// index with all known table/type/view names. Best-effort: if the graph
/// can't open (no `.stella/codegraph.db`), the schema gate runs with an
/// empty index — it just won't catch conflicts until `stella init` runs.
fn populate_schema_index(registry: &ToolRegistry, workspace_root: &std::path::Path) {
    let db_path = workspace_root.join(".stella").join("codegraph.db");
    if !db_path.exists() {
        return;
    }
    let graph = match stella_graph::CodeGraph::open(workspace_root, &db_path) {
        Ok(g) => g,
        Err(_) => return,
    };
    let (tables, types, views) = graph.schema_names();
    registry.update_schema_index(tables, types, views);
    graph.shutdown();
}

/// `stella init` — infer the workspace's domain taxonomy, build the code-graph
/// index, and write `.stella/domains.toml` (see `crate::domains`). Domain
/// inference is model-assisted when a provider resolves, with a deterministic
/// directory heuristic fallback, so init always succeeds — offline included.
/// The code graph (`.stella/context.db`) is built unconditionally: it needs no
/// provider, only the on-disk source tree.
pub async fn run_init(
    model_override: Option<&str>,
    api_key_override: Option<&str>,
    base_url_override: Option<&str>,
) -> Result<(), String> {
    let workspace_root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;

    tui::section_header("Stella init");

    let domains = match Config::load(model_override, api_key_override, base_url_override) {
        Ok(cfg) => {
            let provider = build_provider(&cfg)?;
            println!(
                "  {} inferring domains with {}/{}…",
                "◈".cyan(),
                cfg.provider.id,
                cfg.model_id
            );
            infer_domains(&*provider, &workspace_root).await
        }
        Err(_) => {
            println!(
                "  {} no provider configured — using the directory heuristic \
                 (re-run `stella init` with a key for a better taxonomy)",
                "!".yellow()
            );
            heuristic_domains(&workspace_root)
        }
    };

    // The code graph needs no provider — build it regardless of how the
    // domains were inferred, so the index exists even fully offline.
    build_code_graph(&workspace_root);

    let path = domains.save(&workspace_root)?;
    println!(
        "  {} {} domains ({}) → {}",
        "✓".green(),
        domains.domains.len(),
        domains.inferred_by,
        path.display()
    );
    for domain in &domains.domains {
        println!(
            "    {} {} — {} [{}]",
            "·".dimmed(),
            domain.name.bright_blue(),
            domain.description.dimmed(),
            domain.paths.join(", ").dimmed()
        );
    }
    println!(
        "\n  {}",
        "Domains tag memories, reflections, and every code-graph node/edge; recall uses them \
         for relevance."
            .dimmed()
    );
    Ok(())
}

/// Connect the workspace's MCP servers (.stella/mcp.toml), wrapping the
/// native registry so their tools merge into the agent's set under
/// mcp__<server>__<tool> names. Absent config -> None (zero overhead).
/// Connection is best-effort per server (stella-mcp isolates failures);
/// failed servers are reported once in text mode, never fatal.
pub(crate) async fn connect_mcp(
    cfg: &Config,
    native: std::sync::Arc<dyn ToolExecutor>,
    print_diagnostics: bool,
) -> Option<McpToolSet> {
    let path = cfg.workspace_root.join(".stella").join("mcp.toml");
    let text = std::fs::read_to_string(&path).ok()?;
    let parsed = match McpConfig::from_toml_str(&text) {
        Ok(parsed) => parsed,
        Err(e) => {
            if print_diagnostics {
                eprintln!(
                    "  {} {} is invalid: {e} — MCP servers disabled this session",
                    "!".yellow(),
                    path.display()
                );
            }
            return None;
        }
    };
    let servers = parsed.into_servers();
    if servers.is_empty() {
        return None;
    }
    let set = McpToolSet::connect(&servers, std::time::Duration::from_secs(10))
        .await
        .wrapping(native);
    if print_diagnostics {
        for (name, reason) in set.failed_servers() {
            eprintln!(
                "  {} MCP server `{name}` unavailable: {reason}",
                "!".yellow()
            );
        }
        if set.connected_count() > 0 {
            println!(
                "  {} {} MCP server(s) connected",
                "◆".cyan(),
                set.connected_count()
            );
        }
    }
    Some(set)
}

/// Discover developer-defined custom script tools (.stella/tools/*.toml,
/// then ~/.config/stella/tools/*.toml — workspace wins on collision; see
/// stella_tools::custom). Broken manifests never abort a session: their
/// diagnostics print once (text mode) and show up in `stella tools`.
pub(crate) fn discover_custom_tools(cfg: &Config, print_diagnostics: bool) -> Vec<CustomTool> {
    let report = custom::discover(&cfg.workspace_root);
    if print_diagnostics {
        for diagnostic in &report.diagnostics {
            eprintln!(
                "  {} custom tool skipped: {} — {}",
                "!".yellow(),
                diagnostic.path.display(),
                diagnostic.reason
            );
        }
        if !report.diagnostics.is_empty() {
            eprintln!(
                "  {}",
                "run `stella tools --validate` to check every custom tool manifest".dimmed()
            );
        }
    }
    report.tools
}

/// `stella tools` — list the tools the agent would have this session:
/// native built-ins (including the issue tools when a tracker is detected),
/// the interactive/session tools layered on top (ask_user, search_skills,
/// install_skill), developer custom tools (with their source manifests), and
/// any discovery diagnostics for broken manifests. MCP-server tools
/// (.stella/mcp.toml) are merged in at session build time and are not
/// enumerated here — connecting to the servers is out of scope for a listing.
pub fn run_tools_listing() -> Result<(), String> {
    let workspace_root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;
    tui::section_header("Stella tools");

    let registry = ToolRegistry::new(workspace_root.clone());
    println!("  {}", "built-in:".dimmed());
    let mut native: Vec<String> = stella_core::ports::ToolExecutor::schemas(&registry)
        .into_iter()
        .map(|s| s.name)
        .collect();
    native.sort();
    for name in &native {
        println!("    {} {}", "·".dimmed(), name);
    }
    println!(
        "\n  {}",
        "interactive / session tools (added by the CLI each session):".dimmed()
    );
    for (name, note) in [
        (
            "ask_user",
            "ask the user a multiple-choice question (TTY only)",
        ),
        ("search_skills", "search the public skills registry"),
        ("install_skill", "install a registry skill (asks first)"),
    ] {
        println!(
            "    {} {} {}",
            "·".dimmed(),
            name,
            format!("— {note}").dimmed()
        );
    }

    let report = custom::discover(&workspace_root);
    println!(
        "\n  {}",
        "custom (.stella/tools/, ~/.config/stella/tools/):".dimmed()
    );
    if report.tools.is_empty() {
        println!(
            "    {}",
            "none — drop a <name>.toml manifest in .stella/tools/ to add one".dimmed()
        );
    }
    for tool in &report.tools {
        println!(
            "    {} {} — {}",
            "·".green(),
            tool.name.bright_blue(),
            tool.description.dimmed()
        );
    }
    for diagnostic in &report.diagnostics {
        println!(
            "    {} {} — {}",
            "✗".red(),
            diagnostic.path.display(),
            diagnostic.reason.red()
        );
    }

    println!(
        "\n  {}",
        "MCP servers (.stella/mcp.toml) merge more tools at session start — \
         not enumerated here."
            .dimmed()
    );
    Ok(())
}

/// `stella tools --validate [DIR]` — the strict pre-flight for custom tool
/// manifests. Where discovery (and the plain listing above) stays lenient,
/// this checks every `*.toml` in `dir` (or, by default, the same directories
/// discovery scans) and reports errors, warnings, and infos per file — see
/// `stella_tools::validate`. Returns `Err` when any manifest has errors, so
/// the process exits non-zero and a broken manifest is caught *before* a run
/// consumes model budget.
pub fn run_tools_validation(dir: Option<&std::path::Path>) -> Result<(), String> {
    let workspace_root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;
    tui::section_header("Custom tool manifests — validation");

    let report = match dir {
        Some(dir) => {
            if !dir.is_dir() {
                return Err(format!(
                    "`{}` is not a directory — pass a directory of *.toml manifests, or omit \
                     the value to check .stella/tools/ and ~/.config/stella/tools/",
                    dir.display()
                ));
            }
            println!("  {} {}", "checking:".dimmed(), dir.display());
            validate::validate_dir(dir, &workspace_root)
        }
        None => {
            println!(
                "  {} {}",
                "checking:".dimmed(),
                ".stella/tools/, ~/.config/stella/tools/".dimmed()
            );
            validate::validate_default(&workspace_root)
        }
    };

    if report.manifests.is_empty() {
        println!(
            "  {}",
            "no manifests found — drop a <name>.toml in .stella/tools/ to add a custom tool"
                .dimmed()
        );
        return Ok(());
    }

    println!();
    for manifest in &report.manifests {
        let mark = if manifest.has_errors() {
            "✗".red()
        } else {
            "✓".green()
        };
        let name = manifest
            .name
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        println!("  {mark} {}{}", manifest.path.display(), name.bright_blue());
        for issue in &manifest.issues {
            let (label, message) = match issue.severity {
                validate::Severity::Error => ("error:".red().bold(), issue.message.red()),
                validate::Severity::Warning => ("warning:".yellow().bold(), issue.message.normal()),
                validate::Severity::Info => ("info:".dimmed(), issue.message.dimmed()),
            };
            println!("      {label} {message}");
        }
    }

    let failed = report.manifests.iter().filter(|m| m.has_errors()).count();
    let ok = report.manifests.len() - failed;
    println!(
        "\n  {} manifest(s) checked: {} ok, {} with errors, {} warning(s)",
        report.manifests.len(),
        ok,
        failed,
        report.warning_count()
    );

    if failed > 0 {
        Err(format!(
            "{failed} of {} custom tool manifest(s) failed validation",
            report.manifests.len()
        ))
    } else {
        Ok(())
    }
}

/// Construct the turn/session budget guard from `--budget`. No limit at
/// all still meters spend (`BudgetMode::Observed`) so the cost summary and
/// `BudgetTick` events stay meaningful even when nothing is enforced.
pub(crate) fn build_budget_guard(budget_limit: Option<f64>) -> BudgetGuard {
    match budget_limit {
        Some(limit) => BudgetGuard::new(BudgetMode::Enforced, Some(limit), None),
        None => BudgetGuard::new(BudgetMode::Observed, None, None),
    }
}

/// Open the workspace DuckDB store (`.stella/stella.duckdb`). Persistence is
/// observability, not a work dependency: a store that won't open warns once
/// and the session runs on without it — never a startup failure.
pub(crate) fn open_store(workspace_root: &std::path::Path) -> Option<Arc<Store>> {
    match Store::open(workspace_root) {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            eprintln!(
                "  {} local store unavailable ({e}) — executions/telemetry will not be persisted \
                 this session",
                "⚠".yellow()
            );
            None
        }
    }
}

/// How many recent drift samples to replay into a fresh session's
/// calibration. With the estimator's EWMA weight (0.3) anything past ~20
/// samples has negligible influence, and 20 rows is a trivial query.
const DRIFT_SEED_SAMPLES: usize = 20;

/// Build the session's token-drift calibration, seeded from prior sessions'
/// telemetry for the resolved provider/model (`Store::drift_samples`) so the
/// estimator starts already corrected instead of re-learning each session.
/// Best-effort like all persistence: no store (or a failed query) just means
/// starting uncalibrated — factor 1.0, the pre-drift behavior.
pub(crate) fn seed_calibration(store: &Option<Arc<Store>>, cfg: &Config) -> CalibrationMap {
    let calibration = CalibrationMap::new();
    if let Some(store) = store
        && let Ok(samples) = store.drift_samples(cfg.provider.id, &cfg.model_id, DRIFT_SEED_SAMPLES)
        && !samples.is_empty()
    {
        calibration.seed(&cfg.model_id, &samples);
    }
    calibration
}

/// Begin an execution record; a failure degrades to "no persistence for this
/// execution" rather than blocking the work.
pub(crate) fn begin_execution(
    store: &Option<Arc<Store>>,
    kind: &str,
    prompt: &str,
    cfg: &Config,
) -> Option<(Arc<Store>, i64)> {
    let store = store.as_ref()?;
    match store.begin_execution(kind, prompt, cfg.provider.id, &cfg.model_id) {
        Ok(id) => Some((store.clone(), id)),
        Err(_) => None,
    }
}

/// Run one full turn through `stella_core::Engine`, rendering its
/// `AgentEvent` stream live via a spawned draining task running
/// concurrently with the engine (the channel is unbounded and `send` never
/// blocks, so events reach the renderer as soon as an `.await` point in
/// `run_turn` yields — same live-feeling output the old inline-print loop
/// had, just sourced from the event stream instead of direct calls). That
/// same drain task ([`spawn_renderer`]) also persists every event and each
/// `StepUsage` to the workspace store when one is open. `registry` is the
/// concrete tool registry (its CRUD ledger is read after the turn for the
/// Files Touched panel); `base_tools` is the same registry as the engine's
/// executor, possibly MCP-wrapped.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    provider: &dyn Provider,
    base_tools: &dyn ToolExecutor,
    custom_tools: &[CustomTool],
    registry: &ToolRegistry,
    messages: &mut Vec<CompletionMessage>,
    budget: &mut BudgetGuard,
    calibration: &CalibrationMap,
    cfg: &Config,
    format: OutputFormat,
    store: &Option<Arc<Store>>,
    kind: &str,
    prompt: &str,
) -> Result<(), String> {
    budget.begin_turn();
    let turn_start = Instant::now();
    let execution = begin_execution(store, kind, prompt, cfg);

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let renderer = spawn_renderer(rx, format, execution.clone(), cfg.provider.id.to_string());

    // The tool set holds a tx clone (for AskUser events), so it must drop
    // before the renderer is awaited — the channel only closes once EVERY
    // sender is gone, and awaiting the renderer with a live sender would
    // deadlock. The inner scope makes the drop order structural.
    let outcome = {
        // The tool stack, innermost out: native registry <- developer
        // custom script tools (.stella/tools/, stella-tools::custom) <-
        // ask_user (interactive.rs). Headless formats get the io that
        // fails ask_user loudly instead of waiting on stdin that will
        // never answer.
        let customs = CustomToolSet::new(
            base_tools,
            custom_tools.to_vec(),
            cfg.workspace_root.clone(),
        );
        let tools = InteractiveToolSet::new(
            &customs,
            tx.clone(),
            default_ask_io(format == OutputFormat::Text),
        )
        .with_skill_registry(SkillRegistry::from_env(cfg.workspace_root.clone()));
        let engine =
            Engine::new(provider, &tools, EngineConfig::default()).with_calibration(calibration);
        engine.run_turn(messages, budget, &tx).await
    };
    // Dropping the last sender closes the channel, ending the renderer's
    // `recv()` loop; awaiting it ensures every already-queued event has
    // actually printed before this function returns (no events lost to a
    // detached task racing process exit).
    drop(tx);
    let collected = renderer.await.unwrap_or_default();

    // Persist the files-touched ledger and close the execution record. The
    // ledger lives on the concrete registry (the engine drove tool calls
    // through it, MCP-wrapped or not), so it's read here regardless of which
    // executor the engine held.
    let files = registry.files_touched();
    if let Some((store, id)) = &execution {
        let _ = store.record_files_touched(*id, &files);
        let (outcome_label, cost) = match &outcome {
            TurnOutcome::Completed { cost_usd, .. } => ("completed", *cost_usd),
            TurnOutcome::Aborted { .. } => ("aborted", 0.0),
        };
        let _ = store.finish_execution(*id, outcome_label, cost);
    }

    if format == OutputFormat::Json {
        // One final JSON object: the outcome summary plus the full event
        // log (the same objects stream-json would have emitted line by
        // line).
        let (status, text, cost_usd, reason) = match &outcome {
            TurnOutcome::Completed { text, cost_usd } => {
                ("completed", Some(text.clone()), Some(*cost_usd), None)
            }
            TurnOutcome::Aborted { reason } => ("aborted", None, None, Some(reason.clone())),
        };
        let summary = serde_json::json!({
            "status": status,
            "text": text,
            "cost_usd": cost_usd,
            "reason": reason,
            "model": format!("{}/{}", cfg.provider.id, cfg.model_id),
            "events": collected,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).unwrap_or_else(|e| format!(
                "{{\"status\":\"error\",\"reason\":\"serialize: {e}\"}}"
            ))
        );
    }

    match outcome {
        TurnOutcome::Completed { cost_usd, .. } => {
            if format == OutputFormat::Text {
                tui::files_touched_panel(&files);
                tui::cost_summary(
                    cost_usd,
                    &format!("{}/{}", cfg.provider.id, cfg.model_id),
                    turn_start.elapsed(),
                );
                println!();
            }
            Ok(())
        }
        TurnOutcome::Aborted { reason } => Err(reason),
    }
}

/// Drain, render, and persist the engine's event stream concurrently with
/// the engine. `ToolResult` carries only `call_id`, so the tool name is
/// tracked here to label the result card (see `tui::render_event`'s doc for
/// why that pair is handled inline rather than in the generic dispatcher).
/// Persistence (when a store is open) runs for every format before
/// rendering: each event is appended to the execution's stream (chain-of-
/// thought `Reasoning` deltas included) and each `StepUsage` becomes a
/// telemetry row. Store failures degrade to a single warning — rendering
/// never stops for persistence. Returns the collected events (non-empty only
/// in Json mode, where the caller emits one final summary object).
fn spawn_renderer(
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    format: OutputFormat,
    execution: Option<(Arc<Store>, i64)>,
    provider_id: String,
) -> tokio::task::JoinHandle<Vec<AgentEvent>> {
    tokio::spawn(async move {
        let mut tool_names: HashMap<String, String> = HashMap::new();
        let mut collected: Vec<AgentEvent> = Vec::new();
        let mut seq = 0u64;
        let mut store_warned = false;
        while let Some(event) = rx.recv().await {
            if let Some((store, id)) = &execution {
                if !persist_event(store, *id, seq, &event, &provider_id) && !store_warned {
                    eprintln!(
                        "  {} store write failed — telemetry for this execution is incomplete",
                        "⚠".yellow()
                    );
                    store_warned = true;
                }
                seq += 1;
            }
            match format {
                OutputFormat::StreamJson => {
                    // One line per event — the stable machine interface
                    // (02-architecture.md §4). Serialization of a protocol
                    // enum never fails; if it somehow does, surface it on
                    // stderr rather than silently dropping the event.
                    match serde_json::to_string(&event) {
                        Ok(line) => println!("{line}"),
                        Err(e) => {
                            eprintln!("{{\"type\":\"error\",\"message\":\"serialize: {e}\"}}")
                        }
                    }
                }
                OutputFormat::Json => collected.push(event),
                OutputFormat::Text => match &event {
                    AgentEvent::ToolStart { call } => {
                        tool_names.insert(call.call_id.clone(), call.name.clone());
                        tui::tool_call_card(&call.name, &call.input, "running");
                    }
                    AgentEvent::ToolResult {
                        call_id,
                        output,
                        duration_ms,
                    } => {
                        let name = tool_names
                            .get(call_id)
                            .map(String::as_str)
                            .unwrap_or("tool");
                        let content = match output {
                            ToolOutput::Ok { content } => content.clone(),
                            ToolOutput::Error { message } => message.clone(),
                        };
                        tui::tool_result_card(
                            name,
                            &content,
                            output.is_error(),
                            Duration::from_millis(*duration_ms),
                        );
                    }
                    other => tui::render_event(other),
                },
            }
        }
        collected
    })
}

/// Persist one drained event to an open execution record: append it to the
/// event stream and, for `StepUsage`, add a telemetry row. Shared by
/// [`spawn_renderer`] (one-shot/REPL rendering) and the command deck's event
/// forwarder (`crate::command_deck`), so the store's write path lives in
/// exactly one place. Returns `false` when the event-stream append failed so
/// the caller can surface its once-only warning; the telemetry row stays
/// silently best-effort, exactly as before this was extracted.
pub(crate) fn persist_event(
    store: &Store,
    execution_id: i64,
    seq: u64,
    event: &AgentEvent,
    provider_id: &str,
) -> bool {
    let recorded = store.record_event(execution_id, seq, event).is_ok();
    if let AgentEvent::StepUsage {
        step,
        model,
        input_tokens,
        output_tokens,
        cached_input_tokens,
        estimated_input_tokens,
        cost_usd,
        duration_ms,
        retries,
        tool_calls,
    } = event
    {
        let _ = store.record_telemetry(
            execution_id,
            &TelemetryRow {
                step: *step as u64,
                provider: provider_id.to_string(),
                model: model.clone(),
                input_tokens: *input_tokens,
                estimated_input_tokens: *estimated_input_tokens,
                output_tokens: *output_tokens,
                cache_read_tokens: *cached_input_tokens,
                cache_miss_tokens: input_tokens.saturating_sub(*cached_input_tokens),
                // Populated once the usage envelope carries cache-write
                // counts (staged follow-up).
                cache_write_tokens: 0,
                cost_usd: *cost_usd,
                duration_ms: *duration_ms,
                retries: *retries,
                tool_calls: *tool_calls as u64,
            },
        );
    }
    recorded
}

/// Run one goal loop through `stella_core::Engine::run_goal`: working turns
/// interleaved with judge assessments until the judge passes it (or a
/// backstop — rounds, budget, abort — ends it with a named reason). The
/// worker gets the full tool stack (MCP + custom + interactive + skills) and
/// the judge a read-only view of that same stack.
///
/// The judge is routed by role (`resolve_cross_family_judge`): when a second
/// provider family is configured and the `Router` selects it, the judge runs
/// on a DIFFERENT model family than the worker (bias-resistant assessment,
/// `07-model-matrix.md` §1) and a one-line notice is printed. With a single
/// configured family — or on any discovery/build failure — the judge is the
/// worker provider itself, identical to before: no second provider is built
/// and no extra cost is incurred. Text-mode rendering only — goal and
/// monitor never take `--output-format`.
#[allow(clippy::too_many_arguments)]
async fn run_goal_turn(
    provider: &dyn Provider,
    base_tools: &dyn ToolExecutor,
    custom_tools: &[CustomTool],
    registry: &ToolRegistry,
    messages: &mut Vec<CompletionMessage>,
    budget: &mut BudgetGuard,
    calibration: &CalibrationMap,
    cfg: &Config,
    store: &Option<Arc<Store>>,
    goal: &str,
) -> Result<(), String> {
    let turn_start = Instant::now();
    let execution = begin_execution(store, "goal", goal, cfg);

    // Route the JUDGE role. `Some` only when a distinct-family judge was
    // selected AND built; the boxed provider must outlive the `run_goal`
    // call below, so it is bound here. `None` → the judge is the worker
    // provider (single-family/failure fallback — the v1 behavior).
    let configured = crate::config::discover_configured_providers();
    let routed_judge = resolve_cross_family_judge(cfg.provider.id, &cfg.model_id, &configured);
    if let Some((_, judge_id)) = &routed_judge {
        println!(
            "  {} cross-family judge: {} worker · {} judge — independent, bias-resistant \
             assessment\n",
            "◆".cyan(),
            cfg.provider.id.bright_blue(),
            judge_id.bright_green(),
        );
    }
    let judge: &dyn Provider = match &routed_judge {
        Some((boxed, _)) => &**boxed,
        None => provider,
    };

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let renderer = spawn_renderer(
        rx,
        OutputFormat::Text,
        execution.clone(),
        cfg.provider.id.to_string(),
    );

    let outcome = {
        let customs = CustomToolSet::new(
            base_tools,
            custom_tools.to_vec(),
            cfg.workspace_root.clone(),
        );
        let tools = InteractiveToolSet::new(&customs, tx.clone(), default_ask_io(true))
            .with_skill_registry(SkillRegistry::from_env(cfg.workspace_root.clone()));
        let engine =
            Engine::new(provider, &tools, EngineConfig::default()).with_calibration(calibration);
        engine
            .run_goal(judge, goal, messages, budget, &tx, &GoalConfig::default())
            .await
    };
    drop(tx);
    let _ = renderer.await;

    let files = registry.files_touched();
    if let Some((store, id)) = &execution {
        let _ = store.record_files_touched(*id, &files);
        let (outcome_label, cost) = match &outcome {
            GoalOutcome::Met { cost_usd, .. } => ("goal_met", *cost_usd),
            GoalOutcome::Unmet { cost_usd, .. } => ("goal_unmet", *cost_usd),
        };
        let _ = store.finish_execution(*id, outcome_label, cost);
    }
    tui::files_touched_panel(&files);

    match outcome {
        GoalOutcome::Met {
            rounds,
            verdict,
            cost_usd,
        } => {
            println!(
                "\n  {} goal met after {rounds} round{}: {}",
                "✓".green().bold(),
                if rounds == 1 { "" } else { "s" },
                verdict
            );
            tui::cost_summary(
                cost_usd,
                &format!("{}/{}", cfg.provider.id, cfg.model_id),
                turn_start.elapsed(),
            );
            println!();
            Ok(())
        }
        GoalOutcome::Unmet {
            rounds,
            reason,
            cost_usd,
        } => {
            tui::cost_summary(
                cost_usd,
                &format!("{}/{}", cfg.provider.id, cfg.model_id),
                turn_start.elapsed(),
            );
            Err(format!("goal not met after {rounds} round(s): {reason}"))
        }
    }
}

/// Build the provider adapter from config. Consults the catalog first
/// (provider-scoped, since the same slug legitimately exists on several
/// providers — `gemini-3-pro` on both `gemini` and `vertex`) so an
/// unrecognized model slug is a hard, immediate, named error — never a
/// silent construction of a provider that will simply fail its first live
/// call (`07-model-matrix.md` §3, L-M1/L-M2). The one exemption is `local`:
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
pub(crate) fn build_provider(cfg: &Config) -> Result<Box<dyn Provider>, String> {
    build_provider_parts(
        &cfg.provider,
        &cfg.model_id,
        // `cfg.api_key` is already an `ApiKey` (H3) — clone it rather than
        // reconstructing one from a revealed string.
        cfg.api_key.clone(),
        cfg.effective_base_url().to_string(),
        cfg.base_url_override.as_deref(),
    )
}

/// The per-dialect provider factory, over already-resolved parts rather than
/// a whole [`Config`]. Both the worker path ([`build_provider`]) and the
/// goal loop's routed judge ([`resolve_cross_family_judge`]) go through this
/// one match, so the wire-dialect selection — and the anti-phantom-slug
/// catalog check — live in exactly one place. `effective_base_url` is the
/// base URL requests go to (override-or-default); `base_url_override` is the
/// raw `--base-url`, which only the Vertex/Bedrock arms consume (they build
/// region/project-scoped URLs themselves). See [`build_provider`]'s note on
/// the catalog check and the shared Chat Completions arm.
fn build_provider_parts(
    provider_config: &crate::config::ProviderConfig,
    model_id: &str,
    api_key: ApiKey,
    effective_base_url: String,
    base_url_override: Option<&str>,
) -> Result<Box<dyn Provider>, String> {
    use crate::config::Dialect;

    let provider_id = provider_config.id;
    let display_name = provider_config.display_name;
    // `seeded` is false for `local` and for settings.json-defined providers
    // (issue #44): their models are whatever the user's endpoint serves —
    // the anti-phantom-slug rule exists to catch drift in OUR seed data,
    // not to veto the user's own endpoint.
    if provider_config.seeded {
        stella_model::catalog::Catalog::seed()
            .resolve_for(provider_id, model_id)
            .map_err(|e| e.to_string())?;
    }

    match provider_config.dialect {
        Dialect::OpenaiResponses => {
            let provider = stella_model::openai::OpenAiProvider::new(api_key, model_id.to_string())
                .with_base_url(effective_base_url);
            Ok(Box::new(provider))
        }
        Dialect::Anthropic => {
            let provider =
                stella_model::anthropic::AnthropicProvider::new(api_key, model_id.to_string())
                    .with_base_url(effective_base_url);
            Ok(Box::new(provider))
        }
        Dialect::Gemini => {
            let provider = stella_model::gemini::GeminiProvider::new(api_key, model_id.to_string())
                .with_base_url(effective_base_url);
            Ok(Box::new(provider))
        }
        Dialect::Vertex => {
            // The access token is `api_key` (VERTEX_ACCESS_TOKEN via the
            // credential chain); project and location are Vertex-specific
            // addressing, resolved here with named errors rather than
            // burying a doomed request.
            let project = std::env::var("VERTEX_PROJECT_ID")
                .or_else(|_| std::env::var("GOOGLE_CLOUD_PROJECT"))
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    "Vertex AI needs a project id — set VERTEX_PROJECT_ID (or \
                     GOOGLE_CLOUD_PROJECT)"
                        .to_string()
                })?;
            let location = std::env::var("VERTEX_LOCATION")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "global".to_string());
            let mut provider = stella_model::vertex::VertexProvider::new(
                api_key,
                model_id.to_string(),
                project,
                location,
            );
            if let Some(override_url) = base_url_override {
                provider = provider.with_base_url(override_url.to_string());
            }
            Ok(Box::new(provider))
        }
        Dialect::Bedrock => {
            // `api_key` is AWS_ACCESS_KEY_ID via the credential chain; the
            // rest of the standard AWS env set is read here. Secret
            // resolution failure is a named error pointing at the exact
            // var, not a doomed unsigned request.
            let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    "Bedrock needs AWS_SECRET_ACCESS_KEY alongside AWS_ACCESS_KEY_ID".to_string()
                })?;
            let session_token = std::env::var("AWS_SESSION_TOKEN")
                .ok()
                .filter(|v| !v.is_empty())
                .map(ApiKey::new);
            let region = std::env::var("AWS_REGION")
                .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "us-east-1".to_string());
            let mut provider = stella_model::bedrock::BedrockProvider::new(
                api_key,
                ApiKey::new(secret),
                session_token,
                region,
                model_id.to_string(),
            );
            if let Some(override_url) = base_url_override {
                provider = provider.with_base_url(override_url.to_string());
            }
            Ok(Box::new(provider))
        }
        // Z.ai, xAI, DeepSeek, OpenRouter, local, and config-defined
        // providers (settings.json) — the shared Chat Completions adapter,
        // re-identified per provider so its `Provider::id()` and error
        // messages name the surface actually being called.
        Dialect::OpenaiCompatible => {
            let label = match provider_id {
                "zai" => "Z.ai",
                "xai" => "xAI",
                "deepseek" => "DeepSeek",
                "openrouter" => "OpenRouter",
                "local" => "the local endpoint",
                _ => display_name,
            };
            let provider = stella_model::zai::ZaiProvider::new(api_key, model_id.to_string())
                .with_base_url(effective_base_url)
                .with_identity(provider_id, label);
            Ok(Box::new(provider))
        }
    }
}

/// Cross-family grouping key for judge selection. Same-vendor providers must
/// count as the SAME family so a routed judge is genuinely a different model
/// (`07-model-matrix.md` §1): a Gemini judge assessing Gemini-via-Vertex work
/// carries the same bias, as does an Anthropic Claude judge over Bedrock
/// Claude. Anything without a known sibling is its own family (its id).
fn provider_family(provider_id: &str) -> String {
    match provider_id {
        "gemini" | "vertex" => "google".to_string(),
        "anthropic" | "bedrock" => "anthropic".to_string(),
        other => other.to_string(),
    }
}

/// A `ProviderProfile` for a discovered provider, using its `default_model`
/// for all three role tiers (the finest model this layer knows without a
/// per-role catalog) and [`provider_family`] for cross-family grouping.
fn profile_for(config: &crate::config::ProviderConfig) -> ProviderProfile {
    let model = ModelRef::new(config.id, config.default_model);
    ProviderProfile::new(config.id, model.clone(), model.clone(), model)
        .with_family(provider_family(config.id))
}

/// Resolve the JUDGE role for the goal loop. Builds a role [`Router`] whose
/// most-preferred provider is the active worker (`worker_id`/`worker_model`,
/// so the `--model` pin is honored) followed by every OTHER configured
/// provider, then resolves `Role::Judge`. The router prefers a healthy
/// provider whose family differs from the worker's (`resolve_judge`), so:
///
/// - Only the worker's family configured → the router degrades to the worker
///   provider; `model_ref.provider == worker_id`, so we return `None` and no
///   second provider is built (behavior identical to before).
/// - A distinct family is selected → the concrete `ModelRef` is returned.
///
/// Returns `None` (→ caller reuses the worker as judge) on ANY failure —
/// same-family degradation, a resolve error, an unknown judge provider, or a
/// judge-adapter build failure — so judge routing can never break the loop.
/// On success returns the built judge provider and its id (for the notice).
fn resolve_cross_family_judge(
    worker_id: &str,
    worker_model: &str,
    configured: &[crate::config::ConfiguredProvider],
) -> Option<(Box<dyn Provider>, String)> {
    let worker_ref = ModelRef::new(worker_id, worker_model);
    let worker_profile = ProviderProfile::new(
        worker_id,
        worker_ref.clone(),
        worker_ref.clone(),
        worker_ref,
    )
    .with_family(provider_family(worker_id));

    let mut profiles = vec![worker_profile];
    for entry in configured {
        if entry.config.id == worker_id {
            continue; // the worker is already the preferred profile
        }
        profiles.push(profile_for(&entry.config));
    }

    let router = Router::new(
        RoleTable::new(),
        profiles,
        CircuitBreaker::new(Box::new(SystemClock::new())),
    );
    let decision = router.resolve(Role::Judge).ok()?;

    // Same provider as the worker → single-family/degraded: reuse the worker
    // provider directly, never build a duplicate.
    if decision.model_ref.provider == worker_id {
        return None;
    }

    // Build the concrete judge from the discovered credential for the chosen
    // provider. A missing entry or a build error falls back to the worker.
    let entry = configured
        .iter()
        .find(|c| c.config.id == decision.model_ref.provider)?;
    let judge = build_provider_parts(
        &entry.config,
        &decision.model_ref.model_id,
        entry.api_key.clone(),
        entry.config.base_url.to_string(),
        None,
    )
    .ok()?;
    Some((judge, decision.model_ref.provider))
}

fn print_help() {
    println!("  {}\n", "Stella Commands".cyan().bold());
    println!("  {}  Send a prompt to the agent", "type message".dimmed());
    println!(
        "  {}       List configured providers and models",
        "/models".bright_blue()
    );
    println!(
        "  {}        Show current configuration",
        "/config".bright_blue()
    );
    println!(
        "  {}         Clear conversation history",
        "/clear".bright_blue()
    );
    println!(
        "  {}  Work in judged rounds until a judge confirms the goal is met",
        "/goal <text>".bright_blue()
    );
    println!(
        "  {}       Show files touched this session",
        "/files".bright_blue()
    );
    println!(
        "  {} Rename this terminal tab",
        "/rename <name>".bright_blue()
    );
    println!(
        "  {}  Change the accent color (multi-window)",
        "/color <name>".bright_blue()
    );
    println!("  {}          Show this help", "/help".bright_blue());
    println!("  {}          Exit Stella", "/exit".bright_blue());
    println!("  {}         Exit Stella", "Ctrl+D".dimmed());
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfiguredProvider, PROVIDERS, ProviderConfig};
    use stella_model::credential::ApiKey;

    /// A `Config` selecting `provider_id` at its default model, with a dummy
    /// key. `build_provider` only constructs the adapter (no network call),
    /// so the key is never used.
    fn cfg_for(provider_id: &str) -> Config {
        let provider = PROVIDERS
            .iter()
            .find(|p| p.id == provider_id)
            .unwrap_or_else(|| panic!("provider `{provider_id}` not in PROVIDERS"))
            .clone();
        let model_id = provider.default_model.to_string();
        Config {
            provider,
            model_id,
            api_key: ApiKey::new("dummy-key-unused-offline"),
            workspace_root: std::path::PathBuf::from("/tmp"),
            base_url_override: None,
        }
    }

    #[test]
    fn existing_providers_still_route_to_their_current_adapter() {
        // Regression: switching the catalog check to resolve_for, the
        // (provider, id) dedup, and the inserted vertex/bedrock arms must NOT
        // change selection for any provider that worked before. `build_provider`
        // dispatches on `cfg.provider.id`: OpenAI/Anthropic/Gemini each get
        // their own native adapter, while the OpenAI-compatible gateways (xAI,
        // DeepSeek, OpenRouter) share the ZaiProvider implementation but are
        // re-identified via `with_identity`, so each adapter's `id()` is its own
        // provider name — i.e. every provider reports itself.
        for (provider_id, expected_adapter) in [
            ("openai", "openai"),
            ("anthropic", "anthropic"),
            ("zai", "zai"),
            ("xai", "xai"),
            ("deepseek", "deepseek"),
            ("gemini", "gemini"),
            ("openrouter", "openrouter"),
        ] {
            let provider = build_provider(&cfg_for(provider_id))
                .unwrap_or_else(|e| panic!("build_provider({provider_id}) failed: {e}"));
            assert_eq!(
                provider.id(),
                expected_adapter,
                "provider `{provider_id}` must still route to the `{expected_adapter}` adapter"
            );
        }
    }

    #[test]
    fn vertex_and_bedrock_route_to_their_native_adapters_not_a_fallthrough() {
        // The new providers must construct their own native adapter (not the
        // shared ZaiProvider shim, id "zai", nor the anthropic branch). Both
        // arms read extra addressing/credentials from the environment; set
        // the minimum each requires. build_provider only constructs — no
        // network call. These env vars are read by no other test, so setting
        // them here is race-free within the test binary; the missing-project
        // error case shares this test so the set/remove stays serialized.
        unsafe {
            std::env::set_var("VERTEX_PROJECT_ID", "test-project");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "test-secret");
        }

        let vertex = build_provider(&cfg_for("vertex")).expect("vertex builds");
        assert_eq!(vertex.id(), "vertex", "vertex must route to VertexProvider");

        let bedrock = build_provider(&cfg_for("bedrock")).expect("bedrock builds");
        assert_eq!(
            bedrock.id(),
            "bedrock",
            "bedrock must route to BedrockProvider"
        );

        // A vertex selection with no project id must fail loudly with a named
        // error, never silently fall through to another adapter.
        unsafe {
            std::env::remove_var("VERTEX_PROJECT_ID");
            std::env::remove_var("GOOGLE_CLOUD_PROJECT");
        }
        // `.err()` (not `.unwrap_err()`) so the Ok type `Box<dyn Provider>`,
        // which is not `Debug`, is never required to be printed.
        let err = build_provider(&cfg_for("vertex"))
            .err()
            .expect("vertex without a project id must be an error");
        assert!(
            err.contains("VERTEX_PROJECT_ID"),
            "expected a named VERTEX_PROJECT_ID error, got: {err}"
        );

        unsafe {
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
    }

    /// A `ConfiguredProvider` for `provider_id` at its default model with a
    /// dummy key — the offline analogue of `cfg_for` for judge routing. The
    /// key is never sent anywhere: routing only constructs adapters and
    /// reads `.id()`.
    fn configured_provider(provider_id: &str) -> ConfiguredProvider {
        let config = PROVIDERS
            .iter()
            .find(|p| p.id == provider_id)
            .unwrap_or_else(|| panic!("provider `{provider_id}` not in PROVIDERS"))
            .clone();
        ConfiguredProvider {
            config,
            api_key: ApiKey::new("dummy-key-unused-offline"),
        }
    }

    #[test]
    fn single_configured_provider_reuses_the_worker_as_judge() {
        // (a) Only the worker's own provider is configured: no distinct
        // family exists, so the router degrades to the worker and we build no
        // second provider — the judge IS the worker (identical to the
        // pre-routing behavior, no extra cost).
        let configured = vec![configured_provider("zai")];
        assert!(
            resolve_cross_family_judge("zai", "glm-5.2", &configured).is_none(),
            "a single configured family must leave the judge as the worker provider"
        );
    }

    #[test]
    fn same_family_providers_reuse_the_worker_as_judge() {
        // Two providers but ONE family (Gemini and Gemini-via-Vertex both
        // group under `google`): still no bias-resistant judge available, so
        // it stays the worker — proves `provider_family` grouping gates the
        // cross-family judge, not the raw provider count.
        let configured = vec![configured_provider("gemini"), configured_provider("vertex")];
        assert!(
            resolve_cross_family_judge("gemini", "gemini-3-pro", &configured).is_none(),
            "same-vendor providers share a family and must not route a cross-family judge"
        );
    }

    #[test]
    fn distinct_families_route_a_cross_family_judge() {
        // (b) Worker on Z.ai with Anthropic also configured: the router picks
        // the distinct family and we build that concrete adapter. No network
        // — only construction and `.id()`.
        let configured = vec![configured_provider("zai"), configured_provider("anthropic")];
        let (judge, judge_id) = resolve_cross_family_judge("zai", "glm-5.2", &configured)
            .expect("a distinct family must route a cross-family judge");
        assert_eq!(judge_id, "anthropic", "judge must be the distinct family");
        assert_eq!(judge.id(), "anthropic", "judge adapter must be Anthropic's");
        assert_ne!(
            judge.id(),
            "zai",
            "judge must differ from the worker's family"
        );
    }

    #[test]
    fn judge_build_failure_falls_back_to_the_worker() {
        // (c) The router selects a distinct family, but building that judge
        // adapter fails (an unknown model slug the catalog rejects). Judge
        // routing must never break the loop: it falls back to the worker
        // provider (`None`). Fully offline and race-free — no shared env, no
        // network — unlike an env-gated Vertex/Bedrock build failure.
        let faux = ConfiguredProvider {
            config: ProviderConfig {
                id: "faux",
                env_var: "STELLA_TEST_FAUX_KEY",
                env_var_aliases: &[],
                display_name: "Faux (unbuildable)",
                default_model: "faux-model-not-in-catalog",
                base_url: "http://localhost:0",
                dialect: crate::config::Dialect::OpenaiCompatible,
                // Seeded on purpose: the catalog check must reject the
                // phantom slug, which is exactly the build failure this
                // test needs.
                seeded: true,
            },
            api_key: ApiKey::new("dummy-key-unused-offline"),
        };
        let configured = vec![configured_provider("zai"), faux];
        assert!(
            resolve_cross_family_judge("zai", "glm-5.2", &configured).is_none(),
            "a judge adapter that fails to build must fall back to the worker provider"
        );
    }
}
