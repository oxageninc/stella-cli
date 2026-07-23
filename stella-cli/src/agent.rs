//! The agent loop — ties providers, tools, the step-driver, and TUI
//! together.
//!
//! `run_turn` drives `stella_core::Engine::run_turn` (the step-driver: one
//! model call per step, retry+backoff, compaction, loop detection, budget
//! checks — see `stella-core/src/driver.rs`) and renders its
//! `AgentEvent` stream live via a spawned draining task.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use colored::Colorize;
use stella_core::ports::ToolExecutor;
use stella_core::router::{CircuitBreaker, ProviderProfile};
use stella_core::{
    BudgetGuard, CalibrationMap, Engine, EngineConfig, GoalConfig, GoalOutcome, RoleTable, Router,
    TurnOutcome,
};
use stella_mcp::{McpConfig, McpServerConfig, McpToolSet};
use stella_model::credential::ApiKey;
use stella_model::provider::Provider;
use stella_pipeline::{
    AlwaysAbortGate, CmdOutcome, ContextRecallPort, McpPrefetchPort, NoContextRecall, Pipeline,
    PipelineConfig, PipelinePorts, PipelineStatus, ProviderResolver, RepoStatusPort,
    RepoStructurePort, StdioApprovalGate,
};
use stella_protocol::event::BudgetMode;
use stella_protocol::{AgentEvent, CompletionMessage, ModelRef, Role, ToolOutput};
use stella_store::{ContextBlockRow, ManifestBlockRow, StepManifestRow, Store, TelemetryRow};
use stella_tools::ToolRegistry;
use stella_tools::custom::{self, CustomTool, CustomToolSet};
use stella_tools::hook_runner::ShellHookRunner;
use stella_tools::validate;
use tokio::sync::mpsc;

use crate::OutputFormat;
use crate::config::Config;
use crate::domains::{Domains, heuristic_domains, infer_domains};
use crate::interactive::{InteractiveToolSet, SkillRegistry, default_ask_io};
use crate::memory::{
    ReflectionReport, SessionMemory, inject_recall_block, should_reflect_on,
    turn_warrants_reflection,
};
use crate::runtime::{SystemClock, TokioSleeper};
use crate::tui;
use stella_context::EpisodeOutcome;

mod engine;
mod goal;
mod graph;
mod outcome;
mod output;
mod persistence;
mod prompt;
mod tools;

pub(crate) use engine::*;
pub(crate) use goal::*;
use graph::build_code_graph;
pub(crate) use graph::spawn_session_graph;
#[cfg(test)]
use graph::{GraphSummary, format_graph_stats, index_workspace_graph_blocking};
use outcome::{
    pipeline_episode_outcome, pipeline_failure_reason, pipeline_status_label,
    pipeline_status_result,
};
pub(crate) use outcome::{pipeline_execution_closeout, settled_cost_since};
use output::*;
pub(crate) use persistence::{
    persist_event, record_execution_end, spawn_renderer, warn_store_write_failed,
};
pub(crate) use prompt::*;
pub(crate) use tools::*;

/// Construct the native tool registry without consulting optional host/user backends when the
/// trusted benchmark launcher seals filesystem state; ordinary sessions retain auto-detection.
pub(crate) async fn new_tool_registry(
    workspace_root: std::path::PathBuf,
    options: stella_tools::RegistryOptions,
) -> ToolRegistry {
    if crate::settings::filesystem_settings_disabled() {
        ToolRegistry::with_backends_and_options(workspace_root, None, None, options)
    } else {
        ToolRegistry::new_detected(workspace_root, options).await
    }
}

/// Public skill-registry commands are an extension surface omitted from a filesystem-isolated
/// tool schema; ordinary sessions retain them.
fn skill_registry_for_run(workspace_root: std::path::PathBuf) -> Option<SkillRegistry> {
    (!crate::settings::filesystem_settings_disabled())
        .then(|| SkillRegistry::from_env(workspace_root))
}

/// Run a one-shot prompt. `use_pipeline` selects the staged pipeline (the
/// default) vs the raw step-loop (`--no-pipeline`). `test_command`, when
/// given, arms the pipeline's deterministic verification ladder (the
/// fail→pass flip oracle); without it, verification falls back to the model
/// judge on every iteration.
pub async fn run_one_shot(
    cfg: &Config,
    prompt: &str,
    budget_limit: Option<f64>,
    format: OutputFormat,
    use_pipeline: bool,
    test_command: Option<&str>,
) -> Result<(), String> {
    // A benchmark's durable sink is part of the accounting boundary. Prove the exact mounted file
    // is writable before provider construction or any code path that can make a paid call.
    preflight_durable_stream(format)?;
    crate::enterprise_telemetry::authorize_one_shot(use_pipeline)?;
    if use_pipeline {
        run_pipeline_one_shot(cfg, prompt, budget_limit, format, test_command).await
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
    test_command: Option<&str>,
) -> Result<(), String> {
    let provider = build_provider(cfg)?;
    let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());
    let registry_options = registry_options(cfg);
    let registry: Arc<ToolRegistry> =
        Arc::new(new_tool_registry(cfg.workspace_root.clone(), registry_options.clone()).await);
    populate_schema_index(&registry, &cfg.workspace_root)?;
    let active_rules =
        crate::rules::enforce_workspace_rules(&registry, &cfg.workspace_root, &cfg.authority);
    // Auto-build + live-refresh the code graph in the background so the
    // pipeline's localize step can reach for `graph_query` once it is ready.
    // Status goes to stderr — stdout may be machine-readable JSON.
    let (_session_graph, _graph_build) = spawn_session_graph(
        &cfg.workspace_root,
        registry.clone(),
        Box::new(|line| eprintln!("  {line}")),
        Box::new(|| {}),
    );
    let mcp = connect_mcp(
        cfg,
        registry.clone(),
        Some(registry.mcp_usage_ledger()),
        format == OutputFormat::Text,
    )
    .await?;
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set.as_ref(),
        None => &*registry,
    };
    let custom_tools = discover_custom_tools(cfg, format == OutputFormat::Text).await;
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
    let started_unix = crate::memory::unix_now_secs();
    // Machine-wide presence: the deck's SESSIONS overlay sees this run live
    // and can replay its journal after it ends.
    let mut presence = SessionPresence::announce(cfg, prompt);
    let execution = begin_execution(&store, "pipeline", prompt, cfg, Some(presence.id()));

    let (raw_tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (tx, durable_pre_persisted) = event_sender_for_run(raw_tx, format);
    let renderer = spawn_renderer(
        rx,
        format,
        execution.clone(),
        cfg.provider.id.to_string(),
        durable_pre_persisted,
    );

    // Role wiring from `agent_engine_config`: per-role model pins (worker/
    // triage/judge), their adapters, and per-role request overrides. Notices
    // are stderr diagnostics — stdout may be machine-readable JSON.
    let configured = crate::config::discover_configured_providers();
    let wiring = resolve_engine_wiring(cfg, &model_ref, &configured);
    for notice in &wiring.notices {
        eprintln!("  ! {notice}");
    }
    let resolver =
        RoleProviderResolver::new(&*provider, model_ref.clone(), &wiring.extra_providers);

    let mut messages = vec![CompletionMessage::system(
        with_session_hook_context(
            build_pipeline_system_prompt(cfg, &cfg.workspace_root, &active_rules),
            cfg,
        )
        .await,
    )];
    let mut memory = SessionMemory::open_with_authority(
        &cfg.workspace_root,
        format == OutputFormat::Text,
        &cfg.authority,
    );
    if let Some(m) = &memory {
        inject_recall_block(&mut messages, m.recall_block(prompt).await);
    }
    let mut budget = build_budget_guard(budget_limit);
    budget.begin_turn();

    let result = {
        let customs = CustomToolSet::new(base_tools, custom_tools, cfg.workspace_root.clone());
        let interactive = InteractiveToolSet::new(
            &customs,
            tx.clone(),
            default_ask_io(format == OutputFormat::Text),
        );
        let interactive = match skill_registry_for_run(cfg.workspace_root.clone()) {
            Some(skills) => interactive.with_skill_registry(skills),
            None => interactive,
        };
        // Outermost: the discovery layer (tool_search/skill_search/mcp_search)
        // must see the complete advertised catalog below it.
        let tools =
            crate::discovery::DiscoveryToolSet::new(&interactive, cfg.workspace_root.clone())
                .with_project_prompts_allowed(cfg.authority.project_prompts_allowed);

        let ws_ports = workspace_ports(
            cfg.workspace_root.clone(),
            cfg,
            registry_options,
            active_rules.clone(),
            mcp.clone(),
        )?;

        let breaker = CircuitBreaker::new(Box::new(SystemClock::new()));
        let router = Router::new(wiring.pins.clone(), wiring.profiles.clone(), breaker);

        let is_text = format == OutputFormat::Text;
        // The exact condition lives in `approval_capability_for` so it stays
        // directly unit-testable — inlining it here is what a prior
        // squash-merge collapsed into a bare `is_text` check, with no test to
        // catch the regression.
        let approval_capability = approval_capability_for(
            is_text,
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        );
        // `--test-command` arms the deterministic verify ladder: the
        // fail→pass flip oracle and SubmitFast/Revise decisions all key off
        // it. Left unset, every verification escalates to the model judge.
        let mut pipeline_config = pipeline_config_for_approval_capability(
            cfg,
            approval_capability,
            test_command,
            &wiring.worker_model,
        );
        pipeline_config.role_overrides = wiring.role_overrides.clone();

        let stdio_gate = StdioApprovalGate;
        let no_recall = NoContextRecall;
        // The workspace memory doubles as the pipeline's recall port so the
        // split-context planner sees the same durable lessons the worker's
        // recall block carries; no open store -> no frames (L-C6).
        let recall: &dyn ContextRecallPort = match &memory {
            Some(m) => m,
            None => &no_recall,
        };
        let hook_runner = ShellHookRunner;
        let ports = PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall,
            repo: &ws_ports.repo_structure,
            repo_status: &ws_ports.repo_status,
            diagnostics: &ws_ports.diagnostic_runner,
            tests: &ws_ports.test_runner,
            approvals: if approval_capability == PipelineApprovalCapability::Stdio {
                &stdio_gate
            } else {
                &HEADLESS_APPROVAL_GATE
            },
            sleeper: &TokioSleeper,
            hooks: cfg
                .hooks
                .as_ref()
                .map(|h| (h, &hook_runner as &dyn stella_core::hooks::HookRunner)),
            candidate_workspaces: Some(&ws_ports.candidate_workspaces),
            mcp_prefetch: ws_ports
                .mcp_prefetch
                .as_ref()
                .map(|p| p as &dyn McpPrefetchPort),
            // Headless / fleet: no concurrent input channel to steer from.
            steering: None,
        };

        let pipeline = Pipeline::new(ports, tx.clone(), pipeline_config);
        pipeline.run(prompt, &mut messages, &mut budget).await
    };

    let files = registry.files_touched();

    // Episodic memory: a run that did work (tools or file changes) becomes a
    // retrievable Episode node — outcome, files touched, time window.
    if let Some(m) = &memory
        && (turn_warrants_reflection(&messages) || !files.is_empty())
    {
        let episode_outcome = match &result {
            Ok(outcome) => pipeline_episode_outcome(&outcome.status),
            Err(_) => EpisodeOutcome::Failure,
        };
        m.record_episode(prompt, episode_outcome, &files, started_unix)
            .await;
    }

    // Reflect on turns that did real work — success AND failure. A failed
    // pipeline run is a high-value learning signal (root-cause prompt via
    // `succeeded=false`).
    //
    // The gate is `did real work` = tool-calls in the transcript OR files
    // changed on disk. On the pipeline path the worker's tool-calling turns
    // are deliberately kept OUT of `messages` (planner context hygiene,
    // L-E6), so `turn_warrants_reflection(&messages)` alone is always false
    // there and the whole self-improvement loop never fired on `stella run`.
    // Falling back to `!files.is_empty()` — mirroring the episode gate above
    // — is what makes the primary surface actually learn. The reflector is
    // then handed an enriched transcript (final answer + a note of what
    // changed) so it has signal even when the tool turns aren't in `messages`.
    // Output format does not change Stella's learning semantics: text, JSON,
    // and stream-JSON runs all reflect by default. Ephemeral automation may
    // explicitly opt out with `STELLA_DISABLE_REFLECTION` when it must avoid a
    // post-turn provider call (for example, a benchmark adapter that meters
    // only the task-solving envelope).
    let mut reflection_report = ReflectionReport::default();
    if one_shot_reflection_enabled(format)
        && (turn_warrants_reflection(&messages) || !files.is_empty())
        && let Some(m) = &mut memory
    {
        if format == OutputFormat::StreamJson {
            let _ = tx.send(AgentEvent::Stage {
                name: stella_protocol::StageKind::Reflect,
            });
        }
        let mut reflect_transcript = messages.clone();
        if let Ok(outcome) = &result
            && !outcome.final_text.trim().is_empty()
        {
            reflect_transcript.push(CompletionMessage::assistant(&outcome.final_text));
        }
        if !files.is_empty() {
            let changed = files
                .iter()
                .map(|(path, ops)| format!("{path} ({ops})"))
                .collect::<Vec<_>>()
                .join(", ");
            reflect_transcript.push(CompletionMessage::user(format!(
                "(files changed this turn: {changed})"
            )));
        }
        let mut report = m
            .reflect_and_record(
                &*provider,
                &cfg.model_id,
                &reflect_transcript,
                format != OutputFormat::Text,
                matches!(
                    &result,
                    Ok(outcome) if matches!(outcome.status, PipelineStatus::Completed)
                ),
                remaining_budget(&budget),
            )
            .await;
        settle_reflection_budget(&mut report, &mut budget);
        if format == OutputFormat::StreamJson {
            for event in &report.events {
                let _ = tx.send(event.clone());
            }
        } else {
            surface_reflection(&report, format);
        }
        reflection_report = report;
    }

    if format == OutputFormat::StreamJson
        && let Ok(outcome) = &result
    {
        // Replace the pipeline's earlier terminal frame with the true
        // all-calls total. The renderer retains only the latest Complete and
        // releases it after every queued reflection/accounting event.
        let _ = tx.send(AgentEvent::Complete {
            model: format!("{}/{}", cfg.provider.id, cfg.model_id),
            cost_usd: outcome.total_cost_usd + reflection_report.cost_usd,
        });
    }
    drop(tx);
    let rendered = renderer.await.unwrap_or_default();
    let persistence_complete = rendered.persistence_complete;
    let collected = rendered.events;

    if let Some((store, id)) = &execution {
        let (outcome_label, cost) = pipeline_execution_closeout(&result);
        if !record_execution_end(
            store,
            *id,
            &registry,
            outcome_label,
            cost + reflection_report.cost_usd,
            persistence_complete,
        ) {
            warn_store_write_failed(
                "the audit record (files touched / memory citations / outcome)",
            );
        }
    }

    if let Some(set) = &mcp {
        set.close_all().await;
    }

    // Terminal registry status + the headless → `/inbox` flow: a failed run
    // always lands a notification; a successful one only when it ran long
    // enough that the user has plausibly looked away. `Enter` on the
    // notification (or the SESSIONS overlay) replays the journal.
    let run_ok = matches!(&result, Ok(o) if matches!(o.status, PipelineStatus::Completed));
    let run_secs = turn_start.elapsed().as_secs();
    let notify = if !run_ok {
        Some((
            format!("{}: run FAILED", presence.name()),
            crate::command_deck::prompt_line(prompt, 160),
        ))
    } else if run_secs >= 60 {
        Some((
            format!("{}: run finished ({run_secs}s)", presence.name()),
            crate::command_deck::prompt_line(prompt, 160),
        ))
    } else {
        None
    };
    presence.finish(run_ok, notify);

    match &result {
        Ok(outcome) => {
            if format == OutputFormat::Json {
                let status_str = pipeline_status_label(&outcome.status);
                let reason_str = pipeline_failure_reason(&outcome.status);
                let summary = serde_json::json!({
                    "status": status_str,
                    "text": outcome.final_text,
                    "cost_usd": outcome.total_cost_usd + reflection_report.cost_usd,
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
                    "reflection": reflection_json(&reflection_report),
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
                    outcome.total_cost_usd + reflection_report.cost_usd,
                    &format!("{}/{}", cfg.provider.id, cfg.model_id),
                    turn_start.elapsed(),
                );
                if cfg.enable_recap {
                    tui::recap_panel(&outcome.status, outcome.verdict.as_ref(), &files);
                }
                println!();
            }

            pipeline_status_result(&outcome.status)
        }
        Err(e) => {
            // A hard pipeline error must still produce the JSON summary a
            // `--output-format json` consumer contracted for: without this,
            // stdout is empty, the collected event log is lost, and only
            // stderr says anything (issue #373, item 2). Same shape as the
            // success summary, `text: null`, spend read from the guard (the
            // pipeline metered whatever it spent before failing).
            if format == OutputFormat::Json {
                let summary = serde_json::json!({
                    "status": "error",
                    "text": null,
                    "cost_usd": budget.session_spent_usd(),
                    "reason": e.to_string(),
                    "model": format!("{}/{}", cfg.provider.id, cfg.model_id),
                    "events": collected,
                    "reflection": reflection_json(&reflection_report),
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&summary).unwrap_or_else(|e| format!(
                        "{{\"status\":\"error\",\"reason\":\"serialize: {e}\"}}"
                    ))
                );
            }
            Err(e.to_string())
        }
    }
}

/// The REPL's productized command names — reserved: a custom definition can
/// never run under one of these, argument-carrying forms included (the
/// exact-match handlers in the loop only claim the bare forms). Must cover
/// every `/`-command the loop below handles.
const REPL_RESERVED: &[&str] = &[
    "/exit", "/quit", "/models", "/config", "/help", "/clear", "/files", "/agents", "/init",
    "/rename", "/color", "/goal",
];

/// Run an interactive REPL session. `budget_limit` is per-session: the
/// `BudgetGuard`'s session-scoped total accumulates across every turn in
/// the conversation, while `BudgetGuard::begin_turn` resets only the
/// turn-scoped counter at the start of each one.
pub async fn run_interactive(cfg: &Config, budget_limit: Option<f64>) -> Result<(), String> {
    crate::enterprise_telemetry::authorize_execution_surface(
        crate::enterprise_telemetry::ExecutionSurface::Interactive,
    )?;
    let provider = build_provider(cfg)?;
    let registry: std::sync::Arc<ToolRegistry> = std::sync::Arc::new(
        new_tool_registry(cfg.workspace_root.clone(), registry_options(cfg)).await,
    );
    let mcp = connect_mcp(
        cfg,
        registry.clone(),
        Some(registry.mcp_usage_ledger()),
        true,
    )
    .await?;
    populate_schema_index(&registry, &cfg.workspace_root)?;
    let active_rules =
        crate::rules::enforce_workspace_rules(&registry, &cfg.workspace_root, &cfg.authority);
    // Auto-build the code-graph index in the background (a cheap incremental
    // refresh if it already exists) and keep it fresh via the live watcher, so
    // `graph_query` becomes available this session without a manual `stella
    // init`. Non-blocking; status goes to stderr so it never disturbs the
    // prompt. Kept alive for the whole REPL; the watcher stops when it drops.
    let (_session_graph, _graph_build) = spawn_session_graph(
        &cfg.workspace_root,
        registry.clone(),
        Box::new(|line| eprintln!("  {line}")),
        Box::new(|| {}),
    );
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set.as_ref(),
        None => &*registry,
    };
    let custom_tools = discover_custom_tools(cfg, true).await;
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
    // prefix (instructions + baked memories + SessionStart hook context) is
    // the prompt-cache contract (see build_system_prompt).
    let system_prompt = with_session_hook_context(
        build_system_prompt(cfg, &cfg.workspace_root, &active_rules),
        cfg,
    )
    .await;
    let mut messages = vec![CompletionMessage::system(system_prompt.clone())];
    let mut memory = SessionMemory::open_with_authority(&cfg.workspace_root, true, &cfg.authority);
    // Custom extensions: ⚡ commands/skills invocable as `/name args`, custom
    // agents behind `/agents`. Reloaded after `/init`, which may adopt new
    // ones from `.claude/`/`.agents/`. Load problems print up front so a
    // definition that failed to parse is visible, not silently absent.
    let mut custom = crate::extensions::CustomExtensions::load_with_authority(
        &cfg.workspace_root,
        &cfg.authority,
    );
    if let Some(report) = custom.problems_report() {
        for line in report.lines() {
            println!("  {line}");
        }
        println!();
    }

    // Machine-wide presence: the plain REPL registers like the deck does,
    // so its sessions are findable in every SESSIONS overlay and replayable
    // from their journals. No inbox notifications — the user is right here.
    let mut presence = SessionPresence::announce(cfg, "interactive session");

    // Session-scoped lean-mode activation state: the tool stack is rebuilt
    // every turn, but a tool the model surfaced via tool_search must stay
    // advertised for the rest of the session (see crate::discovery).
    let repl_activation = crate::discovery::new_activation();

    loop {
        print!("{} ", ">".bright_cyan().bold());
        std::io::stdout().flush().map_err(|e| e.to_string())?;

        let mut input = String::new();
        let read = std::io::stdin().read_line(&mut input);
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
        if input == "/models" || input == "/models list" {
            cfg.print_models();
            continue;
        }
        // `/models refresh` is handled model-free: when the configured model
        // itself is broken, the catalog re-sync is part of digging out —
        // routing it into a model turn would fail on the very error being
        // fixed. (Changing a model happens in the deck's SETTINGS tab, via
        // `--model`, or by editing settings.json — not through a command.)
        if input == "/models refresh" || input == "/models refresh --force" {
            println!();
            if let Err(e) = crate::model_catalog::run_refresh(input.ends_with("--force")).await {
                println!("  {} refresh failed: {e}", "✗".red());
            }
            println!();
            continue;
        }
        if input == "/config" {
            // The REPL fallback has no startup dotenv-load record handy —
            // the source label just degrades to the generic `env:VAR` form
            // (see `Config::print_config`'s doc).
            cfg.print_config(None);
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
        if input == "/agents" {
            println!("  {}\n", custom.render_agent_list().replace('\n', "\n  "));
            continue;
        }
        if input == "/init" {
            println!();
            let mut emit = |line: String| println!("  {line}");
            match init_workspace(
                Some(&*provider),
                &cfg.workspace_root,
                Some(&cfg.model_id),
                remaining_budget(&budget),
                &mut emit,
            )
            .await
            {
                Ok((_domains, _cost_usd)) => {
                    // A fresh index may name tables/types the schema gate
                    // should know about this session, not just the next one.
                    // Warn-and-fall-through: a schema-index failure must not
                    // skip the graph-tool enable, the memory reopen, or the
                    // extensions reload below — those refreshes are
                    // independent of it (issue #373, item 3).
                    if let Err(error) = populate_schema_index(&registry, &cfg.workspace_root) {
                        println!("  {} schema governance unavailable: {error}", "!".yellow());
                    }
                    // The code graph now exists — expose the `graph_query` tool
                    // to the rest of this session (it is registered only when
                    // an index is present, so a session that started without
                    // one otherwise wouldn't see it until relaunch).
                    if let Err(error) = registry.enable_code_graph_if_available(&cfg.workspace_root)
                    {
                        println!("  {} graph tool unavailable: {error}", "!".yellow());
                    }
                    // Re-open memory so recall/reflection use the taxonomy
                    // `/init` just wrote — otherwise the cached domains stay
                    // stale until the next launch.
                    memory = SessionMemory::open_with_authority(
                        &cfg.workspace_root,
                        true,
                        &cfg.authority,
                    );
                    // `/init` may also have adopted new custom
                    // commands/skills/agents — make them invocable now, and
                    // report anything that failed to load.
                    custom = crate::extensions::CustomExtensions::load_with_authority(
                        &cfg.workspace_root,
                        &cfg.authority,
                    );
                    if let Some(report) = custom.problems_report() {
                        for line in report.lines() {
                            println!("  {line}");
                        }
                    }
                }
                Err(e) => println!("  {} init failed: {e}", "✗".red()),
            }
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
            let files_before = registry.files_touched().len();
            let started_unix = crate::memory::unix_now_secs();
            presence.update_prompt(goal);
            let result = run_goal_turn(
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
                Some(presence.id()),
            )
            .await;
            presence.needs_input();
            record_turn_episode(
                &memory,
                goal,
                &result,
                &registry,
                files_before,
                started_unix,
                &messages[turn_start..],
            )
            .await;
            if let Err(e) = &result {
                eprintln!("  {} {}\n", "Error:".red().bold(), e);
            }
            // Failures reflect too (minus the user's own soft stop) — the
            // one-shot pipeline path has always treated a failed run as a
            // high-value learning signal; interactive paths now match
            // (issue #373, item 7).
            if should_reflect_on(&result)
                && turn_warrants_reflection(&messages[turn_start..])
                && let Some(m) = &mut memory
            {
                let mut report = m
                    .reflect_and_record(
                        &*provider,
                        &cfg.model_id,
                        &messages,
                        false,
                        result.is_ok(),
                        remaining_budget(&budget),
                    )
                    .await;
                settle_reflection_budget(&mut report, &mut budget);
                surface_reflection(&report, OutputFormat::Text);
            }
            continue;
        }

        // A custom command/skill (⚡): expand the template — arguments and
        // all — into the prompt the model turn runs. Reserved names never
        // reach a custom definition, so the REPL vocabulary above cannot be
        // shadowed even in argument-carrying forms the exact-match handlers
        // let through (e.g. `/help topic`).
        let expanded = if input.starts_with('/') {
            custom.expand(input, REPL_RESERVED)
        } else {
            None
        };
        let input = expanded.as_deref().unwrap_or(input);

        messages.push(crate::attachments::user_message(input));
        println!();

        if let Some(m) = &mut memory {
            // Proposal 4: A/B recall measurement — on ~1/10 turns (the A/B
            // rate), suppress recall so the outcome is comparable to recalled
            // turns. The suppressed flag rides with the turn for attribution.
            m.maybe_suppress_recall(STELLA_AB_RECALL_RATE);
            let block = m.recall_block(input).await;
            inject_recall_block(&mut messages, block);
        }

        // Everything `run_turn` appends past here is this turn's work; the
        // reflection gate reads only that slice (see `turn_warrants_reflection`).
        let turn_start = messages.len();
        let files_before = registry.files_touched().len();
        let started_unix = crate::memory::unix_now_secs();
        presence.update_prompt(input);
        let result = run_turn(
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
            Some(presence.id()),
            &repl_activation,
        )
        .await;
        presence.needs_input();
        record_turn_episode(
            &memory,
            input,
            &result,
            &registry,
            files_before,
            started_unix,
            &messages[turn_start..],
        )
        .await;
        if let Err(e) = &result {
            eprintln!("  {} {}\n", "Error:".red().bold(), e);
        }
        // Same reflect-on-failure policy as the `/goal` block above and the
        // one-shot pipeline path (issue #373, item 7): a soft stop is a user
        // choice and stays excluded; a real failure is a learning signal.
        if should_reflect_on(&result)
            && turn_warrants_reflection(&messages[turn_start..])
            && let Some(m) = &mut memory
        {
            let mut report = m
                .reflect_and_record(
                    &*provider,
                    &cfg.model_id,
                    &messages,
                    false,
                    result.is_ok(),
                    remaining_budget(&budget),
                )
                .await;
            settle_reflection_budget(&mut report, &mut budget);
            surface_reflection(&report, OutputFormat::Text);
        }
    }

    if let Some(set) = &mcp {
        set.close_all().await;
    }
    presence.finish(true, None);
    println!("\n  {}", "Goodbye! ✦".magenta());
    Ok(())
}

/// Record one interactive turn as an episode: only new paths this turn (the
/// slice past `files_before` in the session-cumulative ledger — a re-touch of
/// an earlier path is not re-listed, an accepted approximation), gated the
/// same way as reflection so trivial conversational turns write nothing.
/// `pub(crate)`: the Command Deck's turn driver records through the same
/// helper.
pub(crate) async fn record_turn_episode(
    memory: &Option<SessionMemory>,
    prompt: &str,
    result: &Result<(), String>,
    registry: &ToolRegistry,
    files_before: usize,
    started_unix: i64,
    turn_messages: &[CompletionMessage],
) {
    let Some(m) = memory else {
        return;
    };
    let mut all_files = registry.files_touched();
    let turn_files = all_files.split_off(files_before.min(all_files.len()));
    if !turn_warrants_reflection(turn_messages) && turn_files.is_empty() {
        return;
    }
    let episode_outcome = if result.is_ok() {
        EpisodeOutcome::Success
    } else {
        EpisodeOutcome::Failure
    };
    // Proposal 4: tag the episode summary when recall was suppressed (A/B
    // control turn) so future analysis can compare recalled vs control.
    let ab_tag = if m.recall_was_suppressed() {
        " [ab-control]"
    } else {
        ""
    };
    m.record_episode(
        &format!("{prompt}{ab_tag}"),
        episode_outcome,
        &turn_files,
        started_unix,
    )
    .await;
}

/// Surface a post-turn [`ReflectionReport`] for human text output. Machine
/// streams route reflection events through their execution renderer so
/// `Complete` remains the unique final frame; this helper never writes a
/// second, unframed stdout sequence after that terminal barrier.
pub(crate) fn surface_reflection(report: &ReflectionReport, format: OutputFormat) {
    if format == OutputFormat::Text {
        for event in &report.events {
            match event {
                AgentEvent::StepUsage {
                    role,
                    provider,
                    model,
                    input_tokens,
                    output_tokens,
                    cost_usd,
                    retries,
                    complete,
                    ..
                } => eprintln!(
                    "  {} {:?} {provider}/{model}: {input_tokens} in, {output_tokens} out, \
                     ${cost_usd:.4}, {retries} retries, complete={complete}",
                    "✦".magenta(),
                    role
                ),
                AgentEvent::UsageIncomplete {
                    role,
                    provider,
                    model,
                    reason,
                    retries,
                    ..
                } => eprintln!(
                    "  {} {:?} {provider}/{model}: usage incomplete ({reason:?}, retries={retries:?})",
                    "!".yellow(),
                    role
                ),
                _ => {}
            }
        }
    }
    if let Some(err) = &report.model_error {
        eprintln!(
            "  {} post-turn reflection skipped — model call failed: {err}",
            "!".yellow()
        );
    }
}

fn reflection_json(report: &ReflectionReport) -> serde_json::Value {
    serde_json::json!({
        "recorded": report.recorded,
        "error": report.model_error,
        "cost_usd": report.cost_usd,
        "events": report.events,
    })
}
/// The shared init flow behind `stella init` and the `/init` chat command:
/// infer the domain taxonomy (model-assisted when a provider is available,
/// directory heuristic otherwise), build the code-graph index, persist
/// `.stella/domains.toml`, and record the taxonomy into the context plane.
/// Progress lines stream to `emit` — the CLI prints them, the deck forwards
/// them into the transcript — so both surfaces share one implementation.
pub(crate) async fn init_workspace(
    provider: Option<&dyn Provider>,
    workspace_root: &std::path::Path,
    model_hint: Option<&str>,
    budget_limit: Option<f64>,
    emit: &mut dyn FnMut(String),
) -> Result<(Domains, f64), String> {
    let (domains, inference_cost_usd) = match provider {
        Some(p) => {
            infer_domains(
                p,
                workspace_root,
                model_hint.unwrap_or("unknown"),
                budget_limit,
            )
            .await
        }
        None => (heuristic_domains(workspace_root), 0.0),
    };

    // The code graph needs no provider — build it regardless of how the
    // domains were inferred, so the index exists even fully offline.
    build_code_graph(workspace_root, emit).await;

    // Adopt commands/skills/agents other code agents keep in `.claude/` and
    // `.agents/` (workspace + user scope) as symlinks into stella's own
    // directories — idempotent, never clobbers, never fatal.
    crate::extensions::sync_extensions(workspace_root, emit);

    let path = domains.save(workspace_root)?;

    // Persist the taxonomy into the context plane too: domain descriptions
    // plus bi-temporal `covers_path` facts, so recall can fuse on them and a
    // re-run after the taxonomy shifts supersedes (never deletes) the old
    // beliefs. Best-effort — a store that won't open already warned.
    if let Some(m) = SessionMemory::open(workspace_root, false) {
        m.record_taxonomy(&domains).await;
    }

    emit(format!(
        "✓ {} domains ({}) → {}",
        domains.domains.len(),
        domains.inferred_by,
        path.display()
    ));
    if inference_cost_usd > 0.0 {
        emit(format!(
            "domain inference model cost: ${inference_cost_usd:.6}"
        ));
    }
    Ok((domains, inference_cost_usd))
}

/// Query the code graph (if `stella init` has built it) for the
/// best-connected file's neighborhood, converted to the deck's Graph-tab
/// snapshot. `None` when there is no index, it is empty, or any read fails —
/// the tab then shows its "run stella init" hint instead of an empty graph.
///
/// This is [`graph_snapshot_focus`] with no explicit focus, so the neighborhood
/// centers on [`busiest_file`](stella_graph::CodeGraph::busiest_file) — the
/// sensible default the deck opens on and can re-root away from via the picker.
pub(crate) fn graph_snapshot(
    workspace_root: &std::path::Path,
) -> Option<stella_tui::GraphSnapshot> {
    graph_snapshot_focus(workspace_root, None)
}

/// Build the Graph-tab snapshot centered on `focus` (a root-relative file
/// path), or on the busiest file when `focus` is `None`. The snapshot always
/// carries the full [`files`](stella_tui::GraphSnapshot::files) list so the
/// deck's picker can re-root onto any of them — the deck answers a
/// `FocusGraphFile` request by calling this with `Some(file)` and shipping the
/// result back as a fresh `Inbound::GraphSnapshot`. `None` when there is no
/// index, it is empty, or any read fails.
pub(crate) fn graph_snapshot_focus(
    workspace_root: &std::path::Path,
    focus: Option<&str>,
) -> Option<stella_tui::GraphSnapshot> {
    use stella_tui::{GraphEdge, GraphNode, GraphSnapshot};

    let db_path =
        stella_store::existing_workspace_private_sqlite_path(workspace_root, "codegraph.db")
            .ok()??;
    if !db_path.exists() {
        return None;
    }
    let graph = stella_graph::CodeGraph::open(workspace_root, &db_path).ok()?;
    // An explicit pick roots there; otherwise fall back to the busiest file.
    let focus = match focus {
        Some(f) => f.to_string(),
        None => graph.busiest_file().ok()??,
    };
    let hood = graph.file_neighborhood(std::path::Path::new(&focus)).ok()?;
    // The full file list backs the picker (a superset of this neighborhood).
    let files = graph.all_files().unwrap_or_default();
    graph.shutdown();

    let mut nodes = vec![GraphNode {
        label: hood.file.clone(),
        kind: "file".to_string(),
        location: Some(hood.file.clone()),
    }];
    let mut edges = Vec::new();
    for symbol in &hood.symbols {
        edges.push(GraphEdge {
            from: 0,
            to: nodes.len(),
            kind: "defines".to_string(),
        });
        nodes.push(GraphNode {
            label: symbol.name.clone(),
            kind: symbol.kind.clone(),
            location: Some(format!("{}:{}", hood.file, symbol.start_line)),
        });
    }
    for import in &hood.imports {
        edges.push(GraphEdge {
            from: 0,
            to: nodes.len(),
            kind: "imports".to_string(),
        });
        nodes.push(GraphNode {
            label: import.clone(),
            kind: "module".to_string(),
            location: None,
        });
    }
    for importer in &hood.importers {
        edges.push(GraphEdge {
            from: nodes.len(),
            to: 0,
            kind: "imports".to_string(),
        });
        nodes.push(GraphNode {
            label: importer.clone(),
            kind: "file".to_string(),
            location: Some(importer.clone()),
        });
    }
    Some(GraphSnapshot {
        focus: hood.file,
        nodes,
        edges,
        files,
    })
}

/// Seed the storage gate; unsafe legacy state is an error, not an empty map.
pub(crate) fn populate_schema_index(
    registry: &ToolRegistry,
    workspace_root: &std::path::Path,
) -> Result<(), String> {
    registry.update_storage_index(stella_tools::graph::load_storage_snapshot(workspace_root)?);
    Ok(())
}

/// `stella init` — infer the workspace's domain taxonomy, build the code-graph
/// index, and write `.stella/domains.toml` (see `crate::domains`). Domain
/// inference is model-assisted when a provider resolves, with a deterministic
/// directory heuristic fallback, so init always succeeds — offline included.
/// The code graph (`.stella/private/codegraph.db`) is built unconditionally: it needs
/// no provider, only the on-disk source tree.
pub async fn run_init(
    model_override: Option<&str>,
    api_key_override: Option<&str>,
    base_url_override: Option<&str>,
    no_anim: bool,
) -> Result<(), String> {
    let workspace_root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;

    tui::section_header("Stella init");

    let (provider, model_hint) =
        match Config::load(model_override, api_key_override, base_url_override) {
            Ok(cfg) => {
                let provider = build_provider(&cfg)?;
                println!(
                    "  {} inferring domains with {}/{}…",
                    "◈".bright_cyan(),
                    cfg.provider.id,
                    cfg.model_id
                );
                (Some(provider), Some(cfg.model_id))
            }
            Err(_) => {
                println!(
                    "  {} no provider configured — using the directory heuristic \
                 (re-run `stella init` with a key for a better taxonomy)",
                    "!".yellow()
                );
                (None, None)
            }
        };

    // Play the launch cinematic (starfield + jetpack turtle) over the indexing
    // work. Progress lines route THROUGH it so they print above the animation
    // instead of fighting its cursor moves; it steps aside on a non-TTY,
    // --no-anim, STELLA_NO_ANIM, or NO_COLOR (`InitCinematic` degrades to a
    // plain line printer). `finish()` clears the animation rows before the
    // domain summary prints.
    let cine = crate::init_fx::InitCinematic::start(crate::init_fx::animation_enabled(no_anim));
    let mut emit = |line: String| cine.log(line);
    let (domains, _inference_cost_usd) = init_workspace(
        provider.as_deref(),
        &workspace_root,
        model_hint.as_deref(),
        None,
        &mut emit,
    )
    .await?;
    cine.finish().await;

    for domain in &domains.domains {
        println!(
            "    {} {} — {} [{}]",
            "·".dimmed(),
            domain.name.bright_magenta(),
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

/// Cap on each MCP server's connect (and, until overridden, each later
/// call) — the per-server bound `McpToolSet::connect` enforces.
const MCP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The parse of `.stella/mcp.toml`, split from the connect so a caller that
/// owns a UI (the deck) can announce the slow part before awaiting it (#98).
pub(crate) enum McpPlan {
    /// No config file, or one naming zero servers — nothing to connect.
    None,
    /// The config exists but is unreadable/invalid: MCP is disabled this
    /// session, and the reason must be surfaced exactly once.
    Invalid(String),
    /// Servers to connect via [`connect_mcp_servers`].
    Servers(Vec<McpServerConfig>),
}

pub(crate) fn load_mcp_plan(cfg: &Config) -> McpPlan {
    if crate::settings::filesystem_settings_disabled()
        || crate::enterprise_telemetry::process_free_authority_active()
    {
        return McpPlan::None;
    }
    let path = cfg.workspace_root.join(".stella").join("mcp.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return McpPlan::None;
    };
    // Trust gate. A cloned repo's `.stella/mcp.toml` can name an arbitrary
    // stdio `command` (executed at session start — RCE on `git clone && stella`)
    // or an attacker-controlled http endpoint (egress + a would-be-whitelisted
    // phone-home). This is the same code-execution risk as project hooks, so it
    // is gated by the same flag: untrusted, we do not connect and say why once.
    // (Project settings.json hooks/credential-routing are already gated in
    // settings.rs; this closes the parallel .stella/mcp.toml hole.)
    if !crate::settings::project_code_execution_trusted() {
        return McpPlan::Invalid(format!(
            "{} was NOT loaded — set STELLA_TRUST_PROJECT=1 to let this repo start its \
             MCP servers (they run commands / open connections on your machine)",
            path.display()
        ));
    }
    let parsed = match McpConfig::from_toml_str(&text) {
        Ok(parsed) => parsed,
        Err(e) => {
            return McpPlan::Invalid(format!(
                "{} is invalid: {e} — MCP servers disabled this session",
                path.display()
            ));
        }
    };
    let servers = parsed.into_servers();
    if servers.is_empty() {
        McpPlan::None
    } else {
        McpPlan::Servers(servers)
    }
}

/// Stage 2 of MCP assembly: the slow part — up to [`MCP_CONNECT_TIMEOUT`]
/// per server. Best-effort and isolated per server (stella-mcp records
/// failures in the set instead of propagating them); the returned set wraps
/// `native` so non-`mcp__` tool names fall through to it.
pub(crate) async fn connect_mcp_servers(
    servers: &[McpServerConfig],
    native: std::sync::Arc<dyn ToolExecutor>,
    usage: Option<stella_core::mcp_usage::McpUsageLedger>,
    disabled: Option<stella_mcp::DisabledServers>,
    auth: Option<std::sync::Arc<stella_mcp::OAuthManager>>,
) -> McpToolSet {
    let mut set = McpToolSet::connect_with_auth(servers, MCP_CONNECT_TIMEOUT, auth)
        .await
        .wrapping(native);
    // Record each successful MCP call into the session's usage ledger, and
    // honor the session's disabled-servers set (both may be absent for a
    // one-shot run that never toggles servers).
    if let Some(usage) = usage {
        set = set.with_usage_ledger(usage);
    }
    if let Some(disabled) = disabled {
        set = set.with_disabled_servers(disabled);
    }
    set
}

/// Connect the workspace's MCP servers (.stella/mcp.toml), wrapping the
/// native registry so their tools merge into the agent's set under
/// mcp__<server>__<tool> names. Absent config -> None (zero overhead).
/// Connection is best-effort per server (stella-mcp isolates failures);
/// failed servers are reported once in text mode, never fatal. Deck mode
/// stages [`load_mcp_plan`] / [`connect_mcp_servers`] itself instead: the
/// connect must run behind the live TUI, with diagnostics as transcript
/// events rather than prints (#98).
pub(crate) async fn connect_mcp(
    cfg: &Config,
    native: std::sync::Arc<dyn ToolExecutor>,
    usage: Option<stella_core::mcp_usage::McpUsageLedger>,
    print_diagnostics: bool,
) -> Result<Option<Arc<McpToolSet>>, String> {
    let servers = match load_mcp_plan(cfg) {
        McpPlan::None => return Ok(None),
        McpPlan::Invalid(reason) => {
            if print_diagnostics {
                eprintln!("  {} {reason}", "!".yellow());
            }
            return Ok(None);
        }
        McpPlan::Servers(servers) => servers,
    };
    // A one-shot run has no interactive enable/disable, so no disabled set.
    let auth = crate::mcp_cmd::oauth_manager(&cfg.workspace_root)?;
    let set = connect_mcp_servers(&servers, native, usage, None, Some(auth)).await;
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
                "◆".bright_cyan(),
                set.connected_count()
            );
        }
    }
    // Arc'd so a pipeline driver can share the same connected set into the
    // Best-of-N candidate tool surface and orchestrator pre-fetch (issue
    // #248 Phase 1) alongside its own `&dyn ToolExecutor` borrow.
    Ok(Some(Arc::new(set)))
}

pub(crate) async fn discover_custom_tools(
    cfg: &Config,
    print_diagnostics: bool,
) -> Vec<CustomTool> {
    if crate::enterprise_telemetry::process_free_authority_active() {
        return Vec::new();
    }
    // The manifest walk is synchronous directory I/O — off the runtime
    // worker thread it goes (#64).
    let root = cfg.workspace_root.clone();
    let include_workspace = cfg.authority.project_custom_tools_allowed;
    let report = tokio::task::spawn_blocking(move || {
        custom_tool_report_for_scopes(&root, include_workspace)
    })
    .await
    .unwrap_or_else(|_| custom_tool_report_for_scopes(&cfg.workspace_root, include_workspace));
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

    // The listing mirrors a real session's settings-driven tool switches
    // (bash/web opt-ins).
    let settings = crate::settings::Settings::load(&workspace_root)?;
    let bash_enabled = settings.bash_tool_enabled();
    let web_enabled = settings.web_tools_enabled();
    let registry = ToolRegistry::new(
        workspace_root.clone(),
        stella_tools::RegistryOptions {
            bash: bash_enabled,
            web: web_enabled,
            ..Default::default()
        },
    );
    println!("  {}", "built-in:".dimmed());
    let mut native: Vec<String> = stella_core::ports::ToolExecutor::schemas(&registry)
        .into_iter()
        .map(|s| s.name)
        .collect();
    native.sort();
    for name in &native {
        println!("    {} {}", "·".dimmed(), name);
    }
    if !bash_enabled {
        println!(
            "    {} {}",
            "·".dimmed(),
            "bash — disabled (default); enable with \"tools\": {\"bash\": \"on\"} in settings"
                .dimmed()
        );
    }
    if !web_enabled {
        println!(
            "    {} {}",
            "·".dimmed(),
            "web_search/web_fetch/web_extract_assets/web_download — disabled (default); \
             enable with \"tools\": {\"web\": \"on\"} in settings"
                .dimmed()
        );
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
        (
            "tool_search",
            "search every session tool (built-in/MCP/custom) by keyword",
        ),
        (
            "skill_search",
            "search the skills installed in this workspace",
        ),
        (
            "mcp_search",
            "find MCP servers — configured (.stella/mcp.toml) or the public registry",
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

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let report = custom::discover_in_scopes(
        &workspace_root,
        home.as_deref(),
        settings.authority_policy.project_custom_tools_allowed,
    );
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
            tool.name.bright_magenta(),
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
        println!(
            "  {mark} {}{}",
            manifest.path.display(),
            name.bright_magenta()
        );
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

pub(crate) fn remaining_budget(guard: &BudgetGuard) -> Option<f64> {
    guard
        .turn_limit_usd()
        .map(|limit| (limit - guard.spent_usd()).max(0.0))
}

pub(crate) fn settle_reflection_budget(report: &mut ReflectionReport, guard: &mut BudgetGuard) {
    let had_accounting = report.events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::StepUsage { .. }
                | AgentEvent::UsageIncomplete { .. }
                | AgentEvent::BudgetTick { .. }
        )
    });
    report
        .events
        .retain(|event| !matches!(event, AgentEvent::BudgetTick { .. }));
    if report.cost_usd > 0.0 {
        let _ = guard.record_spend(report.cost_usd);
    }
    if had_accounting {
        report.events.push(AgentEvent::BudgetTick {
            spent_usd: guard.spent_usd(),
            limit_usd: guard.turn_limit_usd(),
            mode: guard.mode(),
        });
    }
}
/// Open the workspace SQLite store (`.stella/private/store.db`). Persistence is
/// observability, not a work dependency: a store that won't open warns once
/// and the session runs on without it — never a startup failure.
pub(crate) fn open_store(workspace_root: &std::path::Path) -> Option<Arc<Store>> {
    // Persisted telemetry can feed calibration and extension-authored rules
    // back into later sessions. Claim-mode trials are isolated and ephemeral:
    // do not read that state or create `.stella/store.db` in the task.
    if crate::settings::filesystem_settings_disabled() {
        return None;
    }
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
    session: Option<&str>,
) -> Option<(Arc<Store>, i64)> {
    let store = store.as_ref()?;
    match store.begin_execution(kind, prompt, cfg.provider.id, &cfg.model_id) {
        Ok(id) => {
            // Link the execution to its session (store schema v8) — what
            // lets the deck's SESSIONS overlay reassemble and replay the
            // session's full journal later. Best-effort like every other
            // store write: a failed link degrades replay, never the turn.
            if let Some(session) = session {
                let _ = store.set_execution_session(id, session);
            }
            Some((store.clone(), id))
        }
        Err(_) => None,
    }
}

/// A headless/plain session's presence in the machine-wide registry: the
/// deck's SESSIONS overlay finds it live and — because every execution links
/// back via [`begin_execution`]'s `session` — can replay it long after it
/// ended. Registration is best-effort throughout: a failed registry write
/// never disturbs the run.
pub(crate) struct SessionPresence {
    registry: stella_store::SessionRegistry,
    record: stella_store::SessionRecord,
    name: String,
}

impl SessionPresence {
    /// Announce the session (status In Progress), titled from the workspace
    /// and the prompt/goal that started it.
    pub(crate) fn announce(cfg: &Config, prompt: &str) -> Self {
        let name = cfg
            .workspace_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| cfg.workspace_root.display().to_string());
        let mut record = stella_store::SessionRecord::new(
            cfg.workspace_root.display().to_string(),
            name.clone(),
        );
        record.title = format!("{name}: {}", crate::command_deck::prompt_line(prompt, 48));
        record.summary = crate::command_deck::prompt_line(prompt, 240);
        let registry = stella_store::SessionRegistry::open_default();
        let _ = registry.upsert(&record);
        Self {
            registry,
            record,
            name,
        }
    }

    /// The registry id — what executions link to and notifications carry.
    pub(crate) fn id(&self) -> &str {
        &self.record.id
    }

    /// The workspace's display name (notification titles).
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// A new prompt is running: refresh the summary (and the title, if the
    /// session was announced before its first real prompt).
    pub(crate) fn update_prompt(&mut self, prompt: &str) {
        self.record.summary = crate::command_deck::prompt_line(prompt, 240);
        self.record.status = stella_store::SessionStatus::InProgress;
        self.record.title = format!(
            "{}: {}",
            self.name,
            crate::command_deck::prompt_line(prompt, 48)
        );
        let _ = self.registry.upsert(&self.record);
    }

    /// Between turns an interactive session waits on the human.
    pub(crate) fn needs_input(&mut self) {
        self.record.status = stella_store::SessionStatus::NeedsInput;
        let _ = self.registry.upsert(&self.record);
    }

    /// Terminal status, plus an optional persist-until-read inbox
    /// notification linked to this session — the headless → `/inbox` flow:
    /// a finished `stella run` surfaces in every deck's inbox, and `Enter`
    /// replays it.
    pub(crate) fn finish(&mut self, ok: bool, notify: Option<(String, String)>) {
        self.record.status = if ok {
            stella_store::SessionStatus::Complete
        } else {
            stella_store::SessionStatus::Error
        };
        let _ = self.registry.upsert(&self.record);
        if let Some((title, body)) = notify {
            let _ = stella_store::NotificationStore::open_default().push(
                &stella_store::Notification::new(title, body, self.record.id.clone())
                    .with_session_id(self.record.id.clone()),
            );
        }
    }
}

/// Run one full turn through `stella_core::Engine`, rendering its
/// `AgentEvent` stream live via a spawned draining task running
/// concurrently with the engine. Ordinary runs enqueue to an unbounded
/// channel; benchmark stream-json runs synchronously append+flush each event
/// before enqueueing it, so paid-call evidence survives a paused/cancelled
/// renderer. Events still reach the renderer as soon as `run_turn` yields.
/// The drain task ([`spawn_renderer`]) persists every event and each
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
    session: Option<&str>,
    activated: &crate::discovery::ActivatedTools,
) -> Result<(), String> {
    budget.begin_turn();
    let turn_start = Instant::now();
    let execution = begin_execution(store, kind, prompt, cfg, session);

    let (raw_tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (tx, durable_pre_persisted) = event_sender_for_run(raw_tx, format);
    let renderer = spawn_renderer(
        rx,
        format,
        execution.clone(),
        cfg.provider.id.to_string(),
        durable_pre_persisted,
    );

    // The scoped tool set must drop its tx clone before awaiting the renderer.
    let outcome = if crate::enterprise_telemetry::process_free_authority_active() {
        let engine =
            Engine::with_sleeper(provider, registry, engine_config_for(cfg), &TokioSleeper)
                .with_calibration(calibration);
        engine.run_turn_with_sender(messages, budget, &tx).await
    } else {
        let customs = CustomToolSet::new(
            base_tools,
            custom_tools.to_vec(),
            cfg.workspace_root.clone(),
        );
        let interactive = InteractiveToolSet::new(
            &customs,
            tx.clone(),
            default_ask_io(format == OutputFormat::Text),
        );
        let interactive = match skill_registry_for_run(cfg.workspace_root.clone()) {
            Some(skills) => interactive.with_skill_registry(skills),
            None => interactive,
        };
        // Outermost discovery layer; the session-scoped `activated` handle
        // keeps lean-mode activations across the per-turn stack rebuild.
        let tools =
            crate::discovery::DiscoveryToolSet::new(&interactive, cfg.workspace_root.clone())
                .with_project_prompts_allowed(cfg.authority.project_prompts_allowed)
                .with_activation(activated.clone());
        let hook_runner = ShellHookRunner;
        let mut engine =
            Engine::with_sleeper(provider, &tools, engine_config_for(cfg), &TokioSleeper)
                .with_calibration(calibration);
        if let Some(hooks) = &cfg.hooks {
            engine = engine.with_hooks(hooks, &hook_runner);
        }
        engine.run_turn_with_sender(messages, budget, &tx).await
    };
    // Dropping the last sender closes the channel, ending the renderer's
    // `recv()` loop; awaiting it ensures every already-queued event has
    // actually printed before this function returns (no events lost to a
    // detached task racing process exit).
    drop(tx);
    let rendered = renderer.await.unwrap_or_default();
    let persistence_complete = rendered.persistence_complete;
    let collected = rendered.events;

    // Persist the files-touched ledger and close the execution record. The
    // ledger lives on the concrete registry (the engine drove tool calls
    // through it, MCP-wrapped or not), so it's read here regardless of which
    // executor the engine held.
    let files = registry.files_touched();
    if let Some((store, id)) = &execution {
        let (outcome_label, cost) = match &outcome {
            TurnOutcome::Completed { cost_usd, .. } => ("completed", *cost_usd),
            TurnOutcome::Aborted { cost_usd, .. } => ("aborted", *cost_usd),
        };
        if !record_execution_end(
            store,
            *id,
            registry,
            outcome_label,
            cost,
            persistence_complete,
        ) {
            warn_store_write_failed(
                "the audit record (files touched / memory citations / outcome)",
            );
        }
    }

    if format == OutputFormat::Json {
        // One final JSON object: the outcome summary plus the full event
        // log (the same objects stream-json would have emitted line by
        // line).
        let (status, text, cost_usd, reason) = match &outcome {
            TurnOutcome::Completed { text, cost_usd } => {
                ("completed", Some(text.clone()), Some(*cost_usd), None)
            }
            TurnOutcome::Aborted { reason, cost_usd } => {
                ("aborted", None, Some(*cost_usd), Some(reason.clone()))
            }
        };
        let summary = serde_json::json!({
            "status": status,
            "text": text,
            "cost_usd": cost_usd,
            "reason": reason,
            "model": format!("{}/{}", cfg.provider.id, cfg.model_id),
            "events": collected,
            // The session file-touch telemetry payload (one record per
            // normalized path: crud_events, line-delta totals, audit log).
            "files_touched": registry.file_touch_telemetry().to_json(),
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
        TurnOutcome::Aborted { reason, .. } => Err(reason),
    }
}

fn print_help() {
    println!("  {}\n", "Stella Commands".bright_cyan().bold());
    println!("  {}  Send a prompt to the agent", "type message".dimmed());
    println!(
        "  {}       List configured providers and models (`/models refresh` re-syncs the catalog)",
        "/models".bright_magenta()
    );
    println!(
        "  {}        Show current configuration",
        "/config".bright_magenta()
    );
    println!(
        "  {}         Clear conversation history",
        "/clear".bright_magenta()
    );
    println!(
        "  {}  Work in judged rounds until a judge confirms the goal is met",
        "/goal <text>".bright_magenta()
    );
    println!(
        "  {}       Show files touched this session",
        "/files".bright_magenta()
    );
    println!(
        "  {}      List custom agents (⚡ from .stella/agents or ~/.config/stella/agents)",
        "/agents".bright_magenta()
    );
    println!(
        "  {} Rename this terminal tab",
        "/rename <name>".bright_magenta()
    );
    println!(
        "  {}  Change the accent color (multi-window)",
        "/color <name>".bright_magenta()
    );
    println!(
        "  {}          Index the workspace: domain taxonomy + code graph",
        "/init".bright_magenta()
    );
    println!("  {}          Show this help", "/help".bright_magenta());
    println!("  {}          Exit Stella", "/exit".bright_magenta());
    println!("  {}         Exit Stella", "Ctrl+D".dimmed());
    println!();
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
