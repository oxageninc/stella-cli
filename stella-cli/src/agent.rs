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
use stella_core::ports::ToolExecutor;
use stella_core::{BudgetGuard, Engine, EngineConfig, GoalConfig, GoalOutcome, TurnOutcome};
use stella_mcp::{McpConfig, McpToolSet};
use stella_model::credential::ApiKey;
use stella_model::provider::Provider;
use stella_protocol::event::BudgetMode;
use stella_protocol::{AgentEvent, CompletionMessage, ToolOutput};
use stella_store::{Store, TelemetryRow};
use stella_tools::ToolRegistry;
use stella_tools::custom::{self, CustomTool, CustomToolSet};
use tokio::sync::mpsc;

use crate::OutputFormat;
use crate::config::Config;
use crate::domains::{heuristic_domains, infer_domains};
use crate::interactive::{InteractiveToolSet, SkillRegistry, default_ask_io};
use crate::memory::{SessionMemory, inject_recall_block};
use crate::tui;

const SYSTEM_PROMPT: &str = r#"You are Stella, a fast terminal coding agent. You help the user with software engineering tasks by reading files, writing code, running commands, and searching the codebase.

You have these tools available:
- read_file: Read a file with line numbers (supports offset/limit for ranges)
- write_file: Create or overwrite a file (creates parent dirs)
- edit_file: Replace an exact substring in a file (use replace_all for multiple)
- bash: Run a shell command in the workspace root (with timeout)
- grep: Search file contents with regex (shells to ripgrep)
- glob: Find files matching a glob pattern
- ask_user: Ask the user a multiple-choice question when a decision is genuinely theirs to make (2-6 options; the UI always adds a free-text option automatically — never add an "Other" option yourself)
- search_skills: Search the public skills registry for reusable skills you don't have locally
- install_skill: Install a registry skill into the project (always requires the user's confirmation)

Rules:
- Always read a file before editing it — never edit blind.
- Make minimal, surgical edits. Use edit_file, not write_file, for changes to existing files.
- Run tests after making changes to verify they pass.
- Be concise in your responses. Show the user what you changed and why.
- If a task requires multiple steps, work through them systematically.
- When a choice is ambiguous AND getting it wrong would be costly, use ask_user rather than guessing; otherwise proceed with your best judgment."#;

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
/// prefix on every save. This coexists with `SessionMemory`'s per-turn
/// recall block (memory.rs) — the baked prefix carries durable lessons, the
/// recall block carries turn-relevant memories and skills.
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

/// Run a one-shot prompt. `budget_limit` is `--budget` (`main.rs`):
/// `Some(n)` enforces a hard per-turn USD cap, `None` meters spend for the
/// cost summary without ever blocking. `format` selects human rendering vs
/// the two headless modes (json / stream-json) — headless runs also get the
/// headless `ask_user` io, which fails the tool with a named error instead
/// of waiting on stdin.
pub async fn run_one_shot(
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
    let mcp = connect_mcp(cfg, registry.clone(), format == OutputFormat::Text).await;
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set,
        None => &*registry,
    };
    let custom_tools = discover_custom_tools(cfg, format == OutputFormat::Text);
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);

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
        cfg,
        format,
        &store,
        "run",
        prompt,
    )
    .await;
    // …and reflect on the completed turn, recording domain-tagged lessons
    // (recurring ones auto-promote to SKILL.md files). Best-effort: never
    // fails or slows the turn that just ran.
    if outcome.is_ok()
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
/// monitor` composed on top of it). The judge is the same configured
/// provider/model in v1 — `Engine::run_goal` takes it as a separate
/// `&dyn Provider`, so a cross-family judge is a config change away, not a
/// redesign. The worker turns get the full tool stack (MCP + custom +
/// interactive + skills), same as `run_one_shot`.
pub async fn run_goal_cmd(
    cfg: &Config,
    goal: &str,
    budget_limit: Option<f64>,
) -> Result<(), String> {
    let provider = build_provider(cfg)?;
    let registry: std::sync::Arc<ToolRegistry> =
        std::sync::Arc::new(ToolRegistry::new(cfg.workspace_root.clone()));
    let mcp = connect_mcp(cfg, registry.clone(), true).await;
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set,
        None => &*registry,
    };
    let custom_tools = discover_custom_tools(cfg, true);
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);

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
        cfg,
        &store,
        goal,
    )
    .await;
    if outcome.is_ok()
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
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set,
        None => &*registry,
    };
    let custom_tools = discover_custom_tools(cfg, true);
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);

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
            if tui::set_accent(color.trim()) {
                tui::welcome_banner(
                    cfg.provider.id,
                    &cfg.model_id,
                    &cfg.workspace_root.display().to_string(),
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
            if let Err(e) = run_goal_turn(
                &*provider,
                base_tools,
                &custom_tools,
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
            } else if let Some(m) = &mut memory {
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

        if let Err(e) = run_turn(
            &*provider,
            base_tools,
            &custom_tools,
            &registry,
            &mut messages,
            &mut budget,
            cfg,
            OutputFormat::Text,
            &store,
            "chat",
            input,
        )
        .await
        {
            eprintln!("  {} {}\n", "Error:".red().bold(), e);
        } else if let Some(m) = &mut memory {
            m.reflect_and_record(&*provider, &messages, false).await;
        }
    }

    if let Some(set) = &mcp {
        set.close_all().await;
    }
    println!("\n  {}", "Goodbye! ✦".magenta());
    Ok(())
}

/// `stella init` — infer the workspace's domain taxonomy and write
/// `.stella/domains.toml` (see `crate::domains`). Model-assisted when a
/// provider resolves; deterministic directory heuristic otherwise, so init
/// always succeeds — offline included.
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
async fn connect_mcp(
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
fn discover_custom_tools(cfg: &Config, print_diagnostics: bool) -> Vec<CustomTool> {
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
    }
    report.tools
}

/// `stella tools` — list every tool the agent would have this session:
/// native built-ins, developer custom tools (with their source manifests),
/// ask_user, and any discovery diagnostics for broken manifests.
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
        "    {} ask_user {}",
        "·".dimmed(),
        "(interactive sessions)".dimmed()
    );

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
    Ok(())
}

/// Construct the turn/session budget guard from `--budget`. No limit at
/// all still meters spend (`BudgetMode::Observed`) so the cost summary and
/// `BudgetTick` events stay meaningful even when nothing is enforced.
fn build_budget_guard(budget_limit: Option<f64>) -> BudgetGuard {
    match budget_limit {
        Some(limit) => BudgetGuard::new(BudgetMode::Enforced, Some(limit), None),
        None => BudgetGuard::new(BudgetMode::Observed, None, None),
    }
}

/// Open the workspace DuckDB store (`.stella/stella.duckdb`). Persistence is
/// observability, not a work dependency: a store that won't open warns once
/// and the session runs on without it — never a startup failure.
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

/// Begin an execution record; a failure degrades to "no persistence for this
/// execution" rather than blocking the work.
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
        let engine = Engine::new(provider, &tools, EngineConfig::default());
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

/// Run one goal loop through `stella_core::Engine::run_goal`: working turns
/// interleaved with judge assessments until the judge passes it (or a
/// backstop — rounds, budget, abort — ends it with a named reason). The
/// worker gets the full tool stack (MCP + custom + interactive + skills) and
/// the judge a read-only view of that same stack; the judge is the same
/// provider in v1 (see `run_goal_cmd`). Text-mode rendering only — goal and
/// monitor never take `--output-format`.
#[allow(clippy::too_many_arguments)]
async fn run_goal_turn(
    provider: &dyn Provider,
    base_tools: &dyn ToolExecutor,
    custom_tools: &[CustomTool],
    registry: &ToolRegistry,
    messages: &mut Vec<CompletionMessage>,
    budget: &mut BudgetGuard,
    cfg: &Config,
    store: &Option<Arc<Store>>,
    goal: &str,
) -> Result<(), String> {
    let turn_start = Instant::now();
    let execution = begin_execution(store, "goal", goal, cfg);

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
        let engine = Engine::new(provider, &tools, EngineConfig::default());
        engine
            .run_goal(
                provider,
                goal,
                messages,
                budget,
                &tx,
                &GoalConfig::default(),
            )
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
fn build_provider(cfg: &Config) -> Result<Box<dyn Provider>, String> {
    if cfg.provider.id != "local" {
        stella_model::catalog::Catalog::seed()
            .resolve_for(cfg.provider.id, &cfg.model_id)
            .map_err(|e| e.to_string())?;
    }

    // `cfg.api_key` is already an `ApiKey` (H3) — clone it rather than
    // reconstructing one from a revealed string.
    let api_key = cfg.api_key.clone();
    let base_url = cfg.effective_base_url().to_string();

    match cfg.provider.id {
        "openai" => {
            let provider = stella_model::openai::OpenAiProvider::new(api_key, cfg.model_id.clone())
                .with_base_url(base_url);
            Ok(Box::new(provider))
        }
        "anthropic" => {
            let provider =
                stella_model::anthropic::AnthropicProvider::new(api_key, cfg.model_id.clone())
                    .with_base_url(base_url);
            Ok(Box::new(provider))
        }
        "gemini" => {
            let provider = stella_model::gemini::GeminiProvider::new(api_key, cfg.model_id.clone())
                .with_base_url(base_url);
            Ok(Box::new(provider))
        }
        "vertex" => {
            // The access token is cfg.api_key (VERTEX_ACCESS_TOKEN via the
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
                cfg.model_id.clone(),
                project,
                location,
            );
            if let Some(override_url) = &cfg.base_url_override {
                provider = provider.with_base_url(override_url.clone());
            }
            Ok(Box::new(provider))
        }
        "bedrock" => {
            // cfg.api_key is AWS_ACCESS_KEY_ID via the credential chain;
            // the rest of the standard AWS env set is read here. Secret
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
                cfg.model_id.clone(),
            );
            if let Some(override_url) = &cfg.base_url_override {
                provider = provider.with_base_url(override_url.clone());
            }
            Ok(Box::new(provider))
        }
        // Z.ai, xAI, DeepSeek, OpenRouter, local — the shared Chat
        // Completions adapter, re-identified per provider.
        other => {
            let label = match other {
                "zai" => "Z.ai",
                "xai" => "xAI",
                "deepseek" => "DeepSeek",
                "openrouter" => "OpenRouter",
                "local" => "the local endpoint",
                _ => cfg.provider.display_name,
            };
            let provider = stella_model::zai::ZaiProvider::new(api_key, cfg.model_id.clone())
                .with_base_url(base_url)
                .with_identity(other, label);
            Ok(Box::new(provider))
        }
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
    use crate::config::PROVIDERS;
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
        }
    }

    #[test]
    fn existing_providers_still_route_to_their_current_adapter() {
        // Regression: switching the catalog check to resolve_for, the
        // (provider, id) dedup, and the inserted vertex/bedrock arms must NOT
        // change selection for any provider that worked before. OpenAI keeps
        // its Responses-API adapter ahead of the openai_compatible flag;
        // every OpenAI-compatible provider still routes to the shared
        // ZaiProvider shim (id "zai"); Anthropic keeps the Messages adapter.
        for (provider_id, expected_adapter) in [
            ("openai", "openai"),
            ("anthropic", "anthropic"),
            ("zai", "zai"),
            ("xai", "zai"),
            ("deepseek", "zai"),
            ("gemini", "zai"),
            ("openrouter", "zai"),
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
        // openai_compatible shim, id "zai", nor the anthropic branch). Both
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
}
