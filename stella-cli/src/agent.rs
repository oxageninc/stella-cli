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
use std::time::{Duration, Instant};

use colored::Colorize;
use stella_core::{BudgetGuard, Engine, EngineConfig, TurnOutcome};
use stella_model::provider::Provider;
use stella_protocol::event::BudgetMode;
use stella_protocol::{AgentEvent, CompletionMessage, ToolOutput};
use stella_tools::ToolRegistry;
use tokio::sync::mpsc;

use std::sync::Arc;

use stella_store::{Store, TelemetryRow};

use crate::config::Config;
use crate::tui;

use stella_core::{GoalConfig, GoalOutcome};

const SYSTEM_PROMPT: &str = r#"You are Stella, a fast terminal coding agent. You help the user with software engineering tasks by reading files, writing code, running commands, and searching the codebase.

You have these tools available:
- read_file: Read a file with line numbers (supports offset/limit for ranges)
- write_file: Create or overwrite a file (creates parent dirs)
- edit_file: Replace an exact substring in a file (use replace_all for multiple)
- bash: Run a shell command in the workspace root (with timeout)
- grep: Search file contents with regex (shells to ripgrep)
- glob: Find files matching a glob pattern
- explorations: List/read saved codebase maps for this workspace
- save_exploration: Persist an end-to-end map of a codebase slice for reuse
- verify_done: Prove a change with a witness test (fails on previous code, passes on yours)

Definition of done — test-first, witness-verified:
- For any implementation task, work test-first: write the witness test FIRST, run it to see it fail, then implement until it passes.
- You are done when verify_done returns WITNESS CONFIRMED for your change — the test fails on the previous code and passes on the new code. "The code looks right" or "the suite is green" is NOT done: a green suite can hide unwired features and vacuous tests; the witness cannot.
- If verify_done reports VACUOUS TEST, your test doesn't exercise the new behavior or your change isn't wired in — fix that before anything else.

Exploration reuse:
- Before deeply exploring a module or subsystem, call explorations (no arguments) to check whether that slice is already mapped — reading a saved map is vastly cheaper than re-deriving it.
- After any substantial end-to-end exploration, persist it with save_exploration so parallel and future agents reuse your work.

Rules:
- Always read a file before editing it — never edit blind.
- Make minimal, surgical edits. Use edit_file, not write_file, for changes to existing files.
- Independent read-only lookups (read_file/grep/glob/explorations) execute in parallel when you request them together in one step — batch them instead of trickling one at a time.
- Be concise in your responses. Show the user what you changed and why.
- If a task requires multiple steps, work through them systematically."#;

/// Cap on memory characters appended to the system prompt — memories ride
/// the prompt cache on every call, so they must stay dense.
const MEMORY_PROMPT_BUDGET_CHARS: usize = 16_000;

/// Assemble the session's system prompt: the static instructions plus the
/// workspace's saved memories (`.stella/memories/*.md`, the write side is
/// the `save_memory` tool). Memories are loaded ONCE per session and
/// concatenated in filename order so the resulting prefix is byte-stable
/// across every model call — that stability is what lets the whole prompt
/// (instructions + memories) ride the provider's prompt cache instead of
/// being re-billed. Memories saved mid-session deliberately do NOT appear
/// until the next session: hot-injecting them would invalidate the cached
/// prefix on every save.
fn build_system_prompt(workspace_root: &std::path::Path) -> String {
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
        return SYSTEM_PROMPT.to_string();
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
        let entry = format!("\n### {name}\n{}\n", body.trim());
        let cost = entry.chars().count();
        if used + cost > MEMORY_PROMPT_BUDGET_CHARS {
            dropped += 1;
            continue;
        }
        used += cost;
        memories.push_str(&entry);
    }
    if memories.is_empty() {
        return SYSTEM_PROMPT.to_string();
    }
    let mut prompt = format!(
        "{SYSTEM_PROMPT}\n\nWorkspace memories (lessons from previous sessions — apply them):\n{memories}"
    );
    if dropped > 0 {
        prompt.push_str(&format!(
            "\n({dropped} additional memories exceeded the prompt budget and were omitted — \
             consolidate .stella/memories/ to bring them back)"
        ));
    }
    prompt
}

/// Run a one-shot prompt (non-interactive). `budget_limit` is `--budget`
/// (`main.rs`): `Some(n)` enforces a hard per-turn USD cap, `None` meters
/// spend for the cost summary without ever blocking.
pub async fn run_one_shot(
    cfg: &Config,
    prompt: &str,
    budget_limit: Option<f64>,
) -> Result<(), String> {
    let provider = build_provider(cfg)?;
    let registry = ToolRegistry::new(cfg.workspace_root.clone());
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);

    tui::section_header("Stella");
    println!("  {}\n", prompt.dimmed());

    let mut messages = vec![
        CompletionMessage::system(build_system_prompt(&cfg.workspace_root)),
        CompletionMessage::user(prompt),
    ];

    run_turn(
        &*provider,
        &registry,
        &mut messages,
        &mut budget,
        cfg,
        &store,
        "run",
        prompt,
    )
    .await
}

/// Run a one-shot goal loop (non-interactive): work in rounds until a
/// judge model assesses the goal as met (`stella goal "..."`). The judge
/// is the same configured provider/model in v1 — `Engine::run_goal` takes
/// it as a separate `&dyn Provider`, so a cross-family judge is a config
/// change away, not a redesign.
pub async fn run_goal_cmd(
    cfg: &Config,
    goal: &str,
    budget_limit: Option<f64>,
) -> Result<(), String> {
    let provider = build_provider(cfg)?;
    let registry = ToolRegistry::new(cfg.workspace_root.clone());
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);

    tui::section_header("Stella — goal mode");
    println!("  {}\n", goal.dimmed());

    let mut messages = vec![CompletionMessage::system(build_system_prompt(
        &cfg.workspace_root,
    ))];
    run_goal_turn(
        &*provider,
        &registry,
        &mut messages,
        &mut budget,
        cfg,
        &store,
        goal,
    )
    .await
}

/// Run an interactive REPL session. `budget_limit` is per-session: the
/// `BudgetGuard`'s session-scoped total accumulates across every turn in
/// the conversation, while `BudgetGuard::begin_turn` resets only the
/// turn-scoped counter at the start of each one.
pub async fn run_interactive(cfg: &Config, budget_limit: Option<f64>) -> Result<(), String> {
    let provider = build_provider(cfg)?;
    let registry = ToolRegistry::new(cfg.workspace_root.clone());
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);

    tui::welcome_banner(
        cfg.provider.id,
        &cfg.model_id,
        &cfg.workspace_root.display().to_string(),
    );

    // Built once per session and reused verbatim on /clear — the byte-
    // stable prefix is the prompt-cache contract (see build_system_prompt).
    let system_prompt = build_system_prompt(&cfg.workspace_root);
    let mut messages = vec![CompletionMessage::system(system_prompt.clone())];

    loop {
        // Read user input
        print!("{} ", ">".bright_cyan().bold());
        std::io::stdout().flush().map_err(|e| e.to_string())?;

        let mut input = String::new();
        match std::io::stdin().read_line(&mut input) {
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
            if let Err(e) = run_goal_turn(
                &*provider,
                &registry,
                &mut messages,
                &mut budget,
                cfg,
                &store,
                goal,
            )
            .await
            {
                eprintln!("  {} {}\n", "Error:".red().bold(), e);
            }
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
            if tui::set_accent(color.trim()) {
                tui::welcome_banner(
                    cfg.provider.id,
                    &cfg.model_id,
                    &cfg.workspace_root.display().to_string(),
                );
            }
            continue;
        }
        if input == "/files" {
            tui::files_touched_panel(&registry.files_touched());
            println!();
            continue;
        }
        if input == "/goal" {
            println!(
                "  {}\n",
                "usage: /goal <what must be true when done>".dimmed()
            );
            continue;
        }

        messages.push(CompletionMessage::user(input));
        println!();

        if let Err(e) = run_turn(
            &*provider,
            &registry,
            &mut messages,
            &mut budget,
            cfg,
            &store,
            "chat",
            input,
        )
        .await
        {
            eprintln!("  {} {}\n", "Error:".red().bold(), e);
        }
    }

    println!("\n  {}", "Goodbye! ✦".magenta());
    Ok(())
}

/// Construct the budget guard from `--budget`, wired to the SESSION axis:
/// in the REPL the limit caps the whole conversation (per-turn would allow
/// `$limit × turns`, which is not what a money cap means), and in one-shot
/// mode the session is the run, so the semantics coincide. No limit at
/// all still meters spend (`BudgetMode::Observed`) so the cost summary and
/// `BudgetTick` events stay meaningful even when nothing is enforced.
/// Non-finite or non-positive limits are rejected upstream in `main.rs`.
fn build_budget_guard(budget_limit: Option<f64>) -> BudgetGuard {
    match budget_limit {
        Some(limit) => BudgetGuard::new(BudgetMode::Enforced, None, Some(limit)),
        None => BudgetGuard::new(BudgetMode::Observed, None, None),
    }
}

/// Open the workspace DuckDB store (`.stella/stella.duckdb`). Persistence
/// is observability, not a work dependency: a store that won't open warns
/// once and the session runs on without it — never a startup failure.
fn open_store(workspace_root: &std::path::Path) -> Option<Arc<Store>> {
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

/// Begin an execution record; a failure degrades to "no persistence for
/// this execution" rather than blocking the work.
fn begin_execution(
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
/// had, just sourced from the event stream instead of direct calls).
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    provider: &dyn Provider,
    registry: &ToolRegistry,
    messages: &mut Vec<CompletionMessage>,
    budget: &mut BudgetGuard,
    cfg: &Config,
    store: &Option<Arc<Store>>,
    kind: &str,
    prompt: &str,
) -> Result<(), String> {
    budget.begin_turn();
    let turn_start = Instant::now();
    let execution = begin_execution(store, kind, prompt, cfg);

    let engine = Engine::new(provider, registry, EngineConfig::default());
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let renderer = spawn_renderer(rx, execution.clone(), cfg.provider.id.to_string());

    let outcome = engine.run_turn(messages, budget, &tx).await;
    // Dropping the sender closes the channel, ending the renderer's
    // `recv()` loop; awaiting it ensures every already-queued event has
    // actually printed AND persisted before this function returns (no
    // events lost to a detached task racing process exit).
    drop(tx);
    let _ = renderer.await;

    let files = registry.files_touched();
    if let Some((store, id)) = &execution {
        let _ = store.record_files_touched(*id, &files);
        let (outcome_label, cost) = match &outcome {
            TurnOutcome::Completed { cost_usd, .. } => ("completed", *cost_usd),
            TurnOutcome::Aborted { .. } => ("aborted", 0.0),
        };
        let _ = store.finish_execution(*id, outcome_label, cost);
    }

    match outcome {
        TurnOutcome::Completed { cost_usd, .. } => {
            tui::files_touched_panel(&files);
            tui::cost_summary(
                cost_usd,
                &format!("{}/{}", cfg.provider.id, cfg.model_id),
                turn_start.elapsed(),
            );
            println!();
            Ok(())
        }
        TurnOutcome::Aborted { reason } => Err(reason),
    }
}

/// Run one goal loop through `stella_core::Engine::run_goal`: working
/// turns interleaved with judge assessments until the judge passes it (or
/// a backstop — rounds, budget, abort — ends it with a named reason). The
/// judge is the same provider in v1; see `run_goal_cmd`'s doc.
#[allow(clippy::too_many_arguments)]
async fn run_goal_turn(
    provider: &dyn Provider,
    registry: &ToolRegistry,
    messages: &mut Vec<CompletionMessage>,
    budget: &mut BudgetGuard,
    cfg: &Config,
    store: &Option<Arc<Store>>,
    goal: &str,
) -> Result<(), String> {
    let turn_start = Instant::now();
    let execution = begin_execution(store, "goal", goal, cfg);

    let engine = Engine::new(provider, registry, EngineConfig::default());
    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let renderer = spawn_renderer(rx, execution.clone(), cfg.provider.id.to_string());

    let outcome = engine
        .run_goal(
            provider,
            goal,
            messages,
            budget,
            &tx,
            &GoalConfig::default(),
        )
        .await;
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

/// Drain and render the engine's event stream concurrently with the
/// engine itself. `ToolResult` only carries `call_id`, not the tool's
/// name — tracked here so the result card can still show it (see
/// `tui::render_event`'s doc comment for why this pair is handled inline
/// rather than inside that generic dispatcher). With parallel tool
/// execution, results may arrive out of call order — the map keys results
/// back to the right name regardless.
/// Also persists as it renders: every event is appended to the execution's
/// stream (chain-of-thought `Reasoning` deltas included) and each
/// `StepUsage` becomes a telemetry row. Store failures degrade to a single
/// warning — rendering never stops for persistence.
fn spawn_renderer(
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    execution: Option<(Arc<Store>, i64)>,
    provider_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tool_names: HashMap<String, String> = HashMap::new();
        let mut seq = 0u64;
        let mut store_warned = false;
        while let Some(event) = rx.recv().await {
            if let Some((store, id)) = &execution {
                if store.record_event(*id, seq, &event).is_err() && !store_warned {
                    eprintln!(
                        "  {} store write failed — telemetry for this execution is incomplete",
                        "⚠".yellow()
                    );
                    store_warned = true;
                }
                seq += 1;
                if let AgentEvent::StepUsage {
                    step,
                    model,
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    cost_usd,
                    duration_ms,
                    retries,
                    tool_calls,
                } = &event
                {
                    let _ = store.record_telemetry(
                        *id,
                        &TelemetryRow {
                            step: *step as u64,
                            provider: provider_id.clone(),
                            model: model.clone(),
                            input_tokens: *input_tokens,
                            output_tokens: *output_tokens,
                            cache_read_tokens: *cached_input_tokens,
                            cache_miss_tokens: input_tokens.saturating_sub(*cached_input_tokens),
                            // Populated once the usage envelope carries
                            // cache-write counts (staged follow-up).
                            cache_write_tokens: 0,
                            cost_usd: *cost_usd,
                            duration_ms: *duration_ms,
                            retries: *retries,
                            tool_calls: *tool_calls as u64,
                        },
                    );
                }
            }
            match &event {
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
            }
        }
    })
}

/// Build the provider adapter from config. Consults the catalog first so an
/// unrecognized model slug is a hard, immediate, named error — never a
/// silent construction of a provider that will simply fail its first live
/// call (`07-model-matrix.md` §3, L-M1/L-M2: this hard-error rule existed in
/// `stella_model::catalog` since Phase 0 but was never actually called from
/// the request path, so it was unenforced in practice).
///
/// OpenAI gets its own arm ahead of the `openai_compatible` bool: real
/// OpenAI speaks the Responses API (`stella_model::openai`), a wire shape
/// (and tool-call dialect, see `catalog.rs`'s `ToolDialect::OpenaiResponses`)
/// genuinely distinct from the Chat Completions shape every other
/// `openai_compatible` row (Z.ai, xAI, DeepSeek, Gemini's OpenAI-compat
/// shim, OpenRouter) actually speaks — see `openai.rs`'s module doc for why
/// reusing `ZaiProvider` for OpenAI was wrong even though it happened to
/// work.
fn build_provider(cfg: &Config) -> Result<Box<dyn Provider>, String> {
    stella_model::catalog::Catalog::seed()
        .resolve(&cfg.model_id)
        .map_err(|e| e.to_string())?;

    let api_key = cfg.api_key.clone();

    if cfg.provider.id == "openai" {
        let provider = stella_model::openai::OpenAiProvider::new(api_key, cfg.model_id.clone())
            .with_base_url(cfg.provider.base_url.to_string());
        Ok(Box::new(provider))
    } else if cfg.provider.openai_compatible {
        // Z.ai, xAI, DeepSeek, Gemini (OpenAI-compat shim), OpenRouter all
        // use the same Chat Completions adapter shape — only the base URL
        // differs.
        let provider = stella_model::zai::ZaiProvider::new(api_key, cfg.model_id.clone())
            .with_base_url(cfg.provider.base_url.to_string());
        Ok(Box::new(provider))
    } else {
        // Anthropic Messages API
        let provider =
            stella_model::anthropic::AnthropicProvider::new(api_key, cfg.model_id.clone())
                .with_base_url(cfg.provider.base_url.to_string());
        Ok(Box::new(provider))
    }
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
        "  {}  Work in judged rounds until a judge model confirms the goal is met",
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
