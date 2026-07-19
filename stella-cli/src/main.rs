//! `stella` — a fast, BYOK, model-agnostic terminal coding agent.
//!
//! Built on the `stella-*` crate stack: `stella-model` for provider
//! abstraction (Z.ai/GLM 5.2, Anthropic, OpenAI, xAI, DeepSeek, Gemini
//! direct, Vertex AI, Amazon Bedrock, OpenRouter — plus any local
//! OpenAI-compatible endpoint via `--base-url`), `stella-core` for the
//! step-driver engine, `stella-tools` for the built-in tool set, and
//! `stella-protocol` for the shared types.
//!
//! Design goals:
//! - No phone-home requirement — works with zero network calls other than
//!   the user's configured model provider.
//! - BYOK: any provider key, any combination, no account.
//! - Speed: streaming first, prompt-cache-aware system prefix, minimal
//!   overhead between model turns.
//! - Headless one-shot: `stella run --output-format text|json|stream-json`
//!   for scripting (the interactive `chat`/`goal`/`monitor` modes render
//!   human-readable output).

mod agent;
mod agents_installed;
mod attachments;
mod claims;
mod command_deck;
mod config;
mod connect_cmd;
mod domains;
mod engine_config;
mod export;
mod extensions;
mod fleet_cmd;
mod init_fx;
mod interactive;
mod mcp_cmd;
mod memory;
mod memory_cmd;
mod model_catalog;
mod ocp;
mod rules;
mod runtime;
mod settings;
mod skill_manager;
mod stats;
mod subsession;
mod tui;

/// Serializes tests that mutate process environment variables. `setenv` /
/// `getenv` from concurrent threads is documented UB on POSIX, and the test
/// harness runs this binary's test modules on parallel threads — so every
/// test that calls `std::env::set_var`/`remove_var` (agent.rs provider
/// routing, config.rs key resolution) must hold this lock for its whole
/// mutate-read-cleanup window.
#[cfg(test)]
pub(crate) mod test_env {
    /// Acquire the env lock, recovering from a poisoned mutex (a prior
    /// env-mutating test that panicked mid-hold must not cascade).
    pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }
}

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;

/// How turn output reaches the caller (
/// : stream-json is a line-per-`AgentEvent`
/// serialization of the exact protocol enum — a stable machine interface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-oriented interactive rendering (default).
    Text,
    /// One final JSON object summarizing the turn (headless).
    Json,
    /// One JSON line per AgentEvent as it happens (headless streaming).
    StreamJson,
}

#[derive(Parser)]
#[command(
    name = "stella",
    version = version_static(),
    about = "A fast, BYOK, model-agnostic terminal coding agent"
)]
struct Cli {
    /// Override the worker model for this invocation: provider/model_id
    /// (e.g. zai/glm-5.2, anthropic/claude-fable-5, openai/gpt-5.5)
    #[arg(long, env = "STELLA_MODEL")]
    model: Option<String>,

    /// API key for the selected provider, highest-precedence step of the
    /// credential chain (CLI flag -> env var -> credentials file ->
    /// interactive prompt). Prefer an env var or
    /// ~/.config/stella/credentials.toml for anything long-lived — a flag
    /// value is visible in shell history and `ps`.
    #[arg(long)]
    api_key: Option<String>,

    /// Base URL override. Required with --model local/<model> to point at a
    /// local OpenAI-compatible server (Ollama, vLLM, LM Studio, llama.cpp
    /// server — e.g. http://localhost:11434/v1); optional for every other
    /// provider to route through a proxy.
    #[arg(long, env = "STELLA_BASE_URL")]
    base_url: Option<String>,

    /// Output format: text (interactive), json (one final object), or
    /// stream-json (one line per agent event)
    #[arg(long, env = "STELLA_OUTPUT_FORMAT", value_enum, default_value = "text")]
    output_format: OutputFormat,

    /// Hard USD spend limit for the whole run/session — enforced mode
    /// : work aborts cleanly (never mid-tool) once
    /// total spend exceeds this. Omit to meter spend for the cost summary
    /// without ever blocking (observed mode).
    #[arg(long, env = "STELLA_BUDGET", value_parser = parse_budget)]
    budget: Option<f64>,

    /// Use the plain line-based REPL for chat instead of the Command Deck
    /// (the tabbed TUI). The deck also steps aside automatically when stdin
    /// or stdout is not a terminal. Env: STELLA_PLAIN=1.
    #[arg(long)]
    plain: bool,

    /// Freeze all deck animation (the run progress bar's shimmer/pulse and the
    /// caret blink) to a static frame — for CI and asciinema-style recordings.
    /// Also forced on by STELLA_NO_ANIM or NO_COLOR.
    #[arg(long)]
    no_anim: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Send a one-shot prompt (non-interactive)
    Run {
        /// The prompt to send
        prompt: String,

        /// Use the raw step-loop instead of the staged pipeline (triage, plan,
        /// execute, verify, judge). The pipeline is the default; this flag
        /// falls back to the direct Engine::run_turn path.
        #[arg(long)]
        no_pipeline: bool,

        /// Test command the pipeline's verify stage runs deterministically
        /// (e.g. "cargo test -p my-crate"). Arms the fail→pass flip oracle:
        /// a change that flips a failing test to passing can submit without
        /// a model-judge call. Omitted, verification always escalates to the
        /// judge.
        #[arg(long, value_name = "CMD")]
        test_command: Option<String>,
    },

    /// Work in judged rounds until a judge model confirms the goal is met.
    /// Each working round runs through the staged pipeline (triage, plan,
    /// witness, execute, verify) by default; --no-pipeline falls back to the
    /// raw step-loop.
    Goal {
        /// What must be true when done — assessed by the judge each round
        goal: String,

        /// Use the raw step-loop instead of the staged pipeline for each
        /// working round. The pipeline is the default.
        #[arg(long)]
        no_pipeline: bool,
    },

    /// Watch CI for a branch/PR and fix failures until it is fully green
    Monitor {
        /// Branch name or PR number (default: main)
        target: Option<String>,
    },

    /// Start an interactive REPL session
    Chat,

    /// Analyze this workspace and infer its domain taxonomy
    /// (.stella/domains.toml) — the tagging vocabulary for memories,
    /// reflections, and every code-graph node/edge
    Init,

    /// List every tool available to the agent this session — built-ins,
    /// developer custom tools (.stella/tools/), and manifest diagnostics
    Tools {
        /// Validate custom tool manifests instead of listing: parse every
        /// <name>.toml, check names, required fields, timeouts, and
        /// collisions with built-ins and other manifests, then exit
        /// non-zero if any manifest has errors. Pass a directory to check
        /// (defaults to the dirs discovery scans: .stella/tools/ and
        /// ~/.config/stella/tools/).
        #[arg(long, value_name = "DIR")]
        validate: Option<Option<std::path::PathBuf>>,
    },

    /// Fan tasks out to a fleet of worker agents in ONE shared tree —
    /// coordinated by cooperative claims (lock-on-first-write, sub-second,
    /// rivals named), wave-scheduled by dependency, every attempt, commit,
    /// and dollar recorded in .stella/fleet.db. Tasks opting into
    /// isolation = "isolated" get a dedicated worktree whose fleet/<task>
    /// branch is left in place for review.
    Fleet {
        /// Task prompts — each becomes an independent task in the SHARED
        /// tree (cooperative claims coordinate writers; pass a plan file
        /// with `isolation = "isolated"` for per-task worktrees)
        #[arg(required_unless_present = "plan")]
        tasks: Vec<String>,

        /// A plan file instead: .json or .toml with [[tasks]] entries
        /// (id, title, prompt, optional depends_on + isolation + claims —
        /// paths held as cooperative file locks while the task runs)
        #[arg(long, value_name = "FILE", conflicts_with = "tasks")]
        plan: Option<std::path::PathBuf>,

        /// Max tasks dispatched concurrently within one wave
        #[arg(long, default_value_t = 4)]
        max_concurrency: usize,

        /// Git ref `isolation = "isolated"` worktrees branch from
        /// (default: current HEAD); shared-tree tasks ignore it
        #[arg(long)]
        base_ref: Option<String>,

        /// After the fan-out, watch each fleet branch's CI to completion and
        /// reconcile its PR status via `gh` (the fleet PR/CI monitor). Exits
        /// non-zero if any watched branch ends red. Meaningful once the
        /// branches are pushed — e.g. task prompts that push and open PRs.
        #[arg(long)]
        watch: bool,

        /// Use the raw step-loop instead of the staged pipeline (triage,
        /// plan, witness, execute, verify) for each worker. The pipeline is
        /// the default.
        #[arg(long)]
        no_pipeline: bool,
    },

    /// Query the code graph built by `stella init` — symbol definitions and
    /// references, a file's imports/importers, or its graph neighborhood.
    /// Offline: reads .stella/codegraph.db, needs no API key.
    Graph {
        /// What to ask the graph
        #[arg(value_enum)]
        op: GraphOp,

        /// Symbol name (definitions/references) or workspace-relative file
        /// path (imports/importers/neighbors)
        target: String,
    },

    /// List configured providers and available models
    Models {
        #[command(subcommand)]
        cmd: Option<ModelsCmd>,
    },

    /// Summarize cost, tokens, and resolve rate per provider/model from
    /// local telemetry (.stella/store.db) — $/resolved-task receipts
    Stats {
        /// Output format: table (aligned, with TOTAL row), json, or csv
        #[arg(long, value_enum, default_value = "table")]
        format: stats::StatsFormat,

        /// Only show executions for this provider id (e.g. zai, anthropic,
        /// local)
        #[arg(long)]
        provider: Option<String>,
    },

    /// Open the Observatory — a local web dashboard over this workspace's
    /// telemetry (.stella/store.db + fleet.db): spend, tokens, cache
    /// traffic, tool calls, files touched, memory citations, reflections,
    /// and fleet runs. Binds 127.0.0.1 only and opens the stores strictly
    /// read-only — nothing ever leaves this machine.
    Observe {
        /// Port to bind on 127.0.0.1 (0 picks a free port)
        #[arg(long, default_value_t = 7787)]
        port: u16,

        /// Open the dashboard in the default browser once serving
        #[arg(long)]
        open: bool,
    },

    /// Inspect the project's memories through the citation feedback loop —
    /// most-cited first, usefulness scores, truthfulness — and promote an
    /// eligible memory to a project rule (.stella/rules/). Reads local state
    /// only; needs no API key.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },

    /// Manage MCP servers: search a registry, install into .stella/mcp.toml,
    /// list configured servers, and show tool-usage telemetry. Enable/disable
    /// is per-session and lives in the deck's MCP tab (`/mcp`). Reads/writes
    /// local state (+ the registry over HTTP); needs no API key.
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },

    /// Connect an issue tracker (GitHub/Linear) via OAuth or a pasted key —
    /// enables the issue tools (search_issues, create_issue, list_labels, …)
    /// and the deck's Issues tab. Credentials land owner-only in
    /// ~/.config/stella/integrations.json; needs no model API key.
    Connect {
        #[command(subcommand)]
        cmd: ConnectCmd,
    },

    /// Show current configuration
    Config,

    /// Print the version and exit
    Version,
}

/// `stella models` subcommands — the model-catalog surface. A bare
/// `stella models` keeps its provider/key listing; these manage the
/// on-disk master list every slug validates against.
#[derive(Subcommand)]
enum ModelsCmd {
    /// Sync the master list of valid provider/model slugs and pricing from
    /// models.dev (public, no API key). Incremental: an unchanged list is
    /// one conditional request (ETag) and zero writes; pricing changes
    /// append a new model-card version (the latest version is what
    /// displays everywhere).
    Refresh {
        /// Re-download even when the server says the list is unchanged
        /// (recovery hatch for a corrupted local catalog)
        #[arg(long)]
        force: bool,
    },
    /// List the model catalog: every known provider/model slug with its
    /// latest pricing (USD per Mtok), context window, and model maker
    List {
        /// Only this provider id (e.g. anthropic, openrouter)
        #[arg(long)]
        provider: Option<String>,
    },
}

/// `stella connect` subcommands — tracker connections consumed by the issue
/// tools. GitHub uses the OAuth device flow (public client, no secret in the
/// binary); Linear uses browser OAuth when an app is configured, else a
/// personal API key. All traffic is user-initiated — connecting is what opts
/// a workspace into tracker calls.
#[derive(Subcommand)]
pub enum ConnectCmd {
    /// Connect GitHub via the OAuth device flow (or --token to paste a PAT)
    Github {
        /// Paste a personal access token instead of running the device flow
        #[arg(long)]
        token: bool,
    },
    /// Connect Linear via browser OAuth (needs STELLA_LINEAR_CLIENT_ID) or a
    /// personal API key
    Linear {
        /// Paste a personal API key even when an OAuth app is configured
        #[arg(long)]
        api_key: bool,
    },
    /// Show stored connections, their accounts, and credential precedence
    Status,
    /// Forget a stored connection
    Remove {
        /// github | linear
        provider: String,
    },
}

/// `stella mcp` subcommands — the scriptable half of the MCP management surface
/// (the deck's MCP tab is the interactive half; per-session enable/disable and
/// the masked auth prompt live only there).
#[derive(Subcommand)]
pub enum McpCmd {
    /// List configured MCP servers (.stella/mcp.toml)
    List,
    /// Search the MCP server registry (settings.json `mcp.registry_url`, else
    /// the official registry)
    Search {
        /// Substring to match server names (omit to list)
        query: Vec<String>,
        /// Max results in the page
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Install a registry server into .stella/mcp.toml (overwrites — MCP
    /// servers are not versioned)
    Install {
        /// The registry server name (as shown by `stella mcp search`)
        name: String,
        /// Local alias / tool-namespace segment (default: sanitized name)
        #[arg(long)]
        alias: Option<String>,
    },
    /// Remove a configured server from .stella/mcp.toml
    Remove {
        /// The configured server's local name
        name: String,
    },
    /// OAuth login to a configured http server (opens your browser; tokens
    /// land owner-only in .stella/mcp_oauth.json and auto-refresh)
    Login {
        /// The configured server's local name
        name: String,
    },
    /// Forget a server's OAuth tokens
    Logout {
        /// The configured server's local name
        name: String,
    },
    /// Show MCP tool-usage telemetry (.stella/store.db): calls per server/tool
    Usage,
}

/// `stella memory` subcommands — the inspection and promotion surface of the
/// memory-citation loop (agents cite the memories that informed a turn via
/// the `cite_memory` tool; the citations aggregate into the eligibility gate
/// `promote` enforces).
#[derive(Subcommand)]
enum MemoryCmd {
    /// List memories ranked by citation count, with average usefulness,
    /// truthfulness rate, and rule-promotion eligibility
    List {
        /// Output format: table (aligned) or json
        #[arg(long, value_enum, default_value = "table")]
        format: memory_cmd::MemoryFormat,
    },
    /// Promote an eligible memory to a project rule at
    /// .stella/rules/<slug>.md. Eligibility is strict: cited successfully
    /// MORE THAN 10 consecutive times since its last negative remark — one
    /// negative citation resets the count until it is re-earned.
    Promote {
        /// The memory's stable id (nod_…) as shown by `stella memory list`
        id: String,
    },
    /// Re-validate old memories against the current codebase (Proposal 5).
    /// Scans each memory for file-path anchors (e.g. `stella-cli/src/agent.rs`),
    /// checks whether those paths still exist, and flags stale ones. A stale
    /// memory is one whose referenced path no longer exists — a strong signal
    /// the memory is about refactored-away code and may mislead.
    Validate,
}

/// The version string shown by `--version` and `stella version`: the crate
/// version, plus the git sha stamped by dev-mode builds (`scripts/dev.sh`
/// sets `STELLA_BUILD_GIT_SHA` at compile time) so a `stella-dev` binary
/// always names the exact checkout it was built from. Release builds carry
/// no stamp and print the bare semver, unchanged.
fn version_string() -> String {
    match option_env!("STELLA_BUILD_GIT_SHA") {
        Some(sha) if !sha.is_empty() => format!("{}-dev.{sha}", env!("CARGO_PKG_VERSION")),
        _ => env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// clap's `version` attribute needs a `'static` string, but the dev stamp is
/// assembled at runtime — leak it once at parse time (a few bytes, once per
/// process).
fn version_static() -> &'static str {
    version_string().leak()
}

/// Whether `chat` should launch the Command Deck: an explicit `--plain` or
/// STELLA_PLAIN=1 opts out, and both stdin and stdout must be real terminals
/// (raw mode + the alternate screen are meaningless on a pipe).
fn use_deck(plain_flag: bool) -> bool {
    let plain_env = std::env::var_os("STELLA_PLAIN").is_some_and(|v| !v.is_empty() && v != "0");
    !plain_flag && !plain_env && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// The five code-graph queries, mirroring the `graph_query` agent tool's ops
/// one-for-one so a human at the CLI and the model inside a turn see the
/// same frames for the same question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum GraphOp {
    /// Where a symbol is defined
    Definitions,
    /// Best-effort textual references to a symbol
    References,
    /// What a file imports
    Imports,
    /// Which files import a file
    Importers,
    /// A file's immediate graph neighborhood (symbols + edges)
    Neighbors,
}

impl GraphOp {
    fn as_str(self) -> &'static str {
        match self {
            GraphOp::Definitions => "definitions",
            GraphOp::References => "references",
            GraphOp::Imports => "imports",
            GraphOp::Importers => "importers",
            GraphOp::Neighbors => "neighbors",
        }
    }
}

/// `stella observe` — serve the Observatory dashboard for this workspace on
/// `127.0.0.1` until interrupted. Telemetry stores are opened read-only; the
/// page and its assets are embedded, so nothing is fetched from anywhere.
fn run_observe(port: u16, open: bool) -> Result<(), String> {
    let root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to start runtime: {e}"))?;
    rt.block_on(stella_observatory::serve(root, port, move |addr| {
        let url = format!("http://{addr}/");
        println!();
        println!("  {} {}", "◆".bright_magenta(), "Stella Observatory".bold());
        println!("  {} {}", "→".bright_cyan(), url);
        println!(
            "  {}",
            "reads .stella (read-only) · binds 127.0.0.1 only · Ctrl+C to stop".dimmed()
        );
        if open {
            // Best-effort convenience; the printed URL is the contract.
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let _ = std::process::Command::new(opener).arg(&url).spawn();
        }
    }))
    .map_err(|e| e.to_string())
}

/// `stella graph <op> <target>` — the human door to the same query surface
/// the `graph_query` tool gives the agent. Frames print exactly as the model
/// would receive them.
fn run_graph(op: GraphOp, target: &str) -> Result<(), String> {
    let root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;
    match stella_tools::graph::run_query(&root, op.as_str(), target) {
        stella_protocol::tool::ToolOutput::Ok { content } => {
            println!("{content}");
            Ok(())
        }
        stella_protocol::tool::ToolOutput::Error { message } => Err(message),
    }
}

/// `--budget` must be a positive, finite dollar amount — a NaN or negative
/// limit would make every comparison silently false and turn the "hard
/// cap" into a no-op, the worst failure mode for a money control.
fn parse_budget(raw: &str) -> Result<f64, String> {
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!(
            "budget must be a positive dollar amount, got `{raw}`"
        ));
    }
    Ok(value)
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {}", "stella:".red().bold(), e);
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    // Models and Version don't need a configured provider/key.
    match &cli.command {
        Some(Command::Models { cmd }) => {
            return match cmd {
                None => {
                    config::Config::print_available_models();
                    model_catalog::print_catalog_status();
                    Ok(())
                }
                // Needs a runtime for the HTTP fetch — built here because
                // this arm runs before the shared `rt` closure exists.
                Some(ModelsCmd::Refresh { force }) => tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("failed to start runtime: {e}"))?
                    .block_on(model_catalog::run_refresh(*force)),
                Some(ModelsCmd::List { provider }) => model_catalog::run_list(provider.as_deref()),
            };
        }
        Some(Command::Tools { validate }) => {
            return match validate {
                // `--validate` (dir optional) is the strict pre-flight path;
                // a plain `stella tools` stays the lenient listing.
                Some(dir) => agent::run_tools_validation(dir.as_deref()),
                None => agent::run_tools_listing(),
            };
        }
        Some(Command::Graph { op, target }) => {
            // Reads the local index only — works with zero API keys.
            return run_graph(*op, target);
        }
        Some(Command::Stats { format, provider }) => {
            // Reads local telemetry only — works with zero API keys.
            // `*format`: this match borrows `&cli.command` (the Tools arm
            // needs `validate` by ref), so `format` binds as `&StatsFormat`;
            // it is `Copy`, so deref rather than move.
            return stats::run_stats(*format, provider.as_deref());
        }
        Some(Command::Memory { cmd }) => {
            // Reads local stores only (list) / writes one rule file
            // (promote) — works with zero API keys.
            return match cmd {
                MemoryCmd::List { format } => memory_cmd::run_memory_list(*format),
                MemoryCmd::Promote { id } => memory_cmd::run_memory_promote(id),
                MemoryCmd::Validate => memory_cmd::run_memory_validate(),
            };
        }
        Some(Command::Mcp { cmd }) => {
            // MCP management reads/writes local config + the registry over
            // HTTP — no provider or API key required.
            return mcp_cmd::run(cmd);
        }
        Some(Command::Connect { cmd }) => {
            // Tracker OAuth talks only to the tracker the user is connecting
            // — no provider or API key required.
            return connect_cmd::run(cmd);
        }
        Some(Command::Observe { port, open }) => {
            // Loopback-only dashboard over local telemetry — no provider or
            // API key required; the stores are opened strictly read-only.
            return run_observe(*port, *open);
        }
        Some(Command::Version) => {
            println!("stella v{}", version_string());
            return Ok(());
        }
        _ => {}
    }

    let rt = || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("failed to start runtime: {e}"))
    };

    // Everything past here resolves a provider (and may build one), so the
    // model catalog must be live first: open catalog.db, refresh a stale
    // master list (only ever after the user's first explicit
    // `stella models refresh` — the no-phone-home rule), and install the
    // runtime catalog that slug validation and pricing resolve against.
    model_catalog::bootstrap();

    // `init` works offline (heuristic fallback), so config resolution
    // failure downgrades rather than aborting.
    if let Some(Command::Init) = cli.command {
        return rt()?.block_on(agent::run_init(
            cli.model.as_deref(),
            cli.api_key.as_deref(),
            cli.base_url.as_deref(),
            cli.no_anim,
        ));
    }

    // Run/Chat/Config need a resolved config (which requires an API key).
    let cfg = config::Config::load(
        cli.model.as_deref(),
        cli.api_key.as_deref(),
        cli.base_url.as_deref(),
    )?;

    match cli.command.unwrap_or(Command::Chat) {
        Command::Run {
            prompt,
            no_pipeline,
            test_command,
        } => {
            rt()?.block_on(agent::run_one_shot(
                &cfg,
                &prompt,
                cli.budget,
                cli.output_format,
                !no_pipeline,
                test_command.as_deref(),
            ))?;
        }
        Command::Goal { goal, no_pipeline } => {
            rt()?.block_on(agent::run_goal_cmd(&cfg, &goal, cli.budget, !no_pipeline))?;
        }
        Command::Fleet {
            tasks,
            plan,
            max_concurrency,
            base_ref,
            watch,
            no_pipeline,
        } => {
            rt()?.block_on(fleet_cmd::run_fleet(
                &cfg,
                &tasks,
                plan.as_deref(),
                base_ref.as_deref(),
                max_concurrency,
                cli.budget,
                watch,
                !no_pipeline,
            ))?;
        }
        Command::Monitor { target } => {
            let target = target.unwrap_or_else(|| "main".to_string());
            // Monitoring IS a goal: the judge (who can call ci_status
            // itself) ends the loop only on a fully green latest run.
            let goal = format!(
                "Drive CI for `{target}` to fully green. Use ci_status (wait: true) to watch \
                 the latest runs, read the failure logs it returns, fix each root cause in the \
                 code, commit and push the fix, then re-check. The goal is met only when the \
                 latest CI run for `{target}` has completed with every check successful."
            );
            rt()?.block_on(agent::run_goal_cmd(&cfg, &goal, cli.budget, true))?;
        }
        Command::Chat => {
            // The Command Deck (tabbed TUI) is the default chat surface on a
            // real terminal; `--plain` / STELLA_PLAIN=1 / a non-TTY stream
            // falls back to the line-based REPL.
            if use_deck(cli.plain) {
                rt()?.block_on(command_deck::run_deck_session(
                    &cfg,
                    cli.budget,
                    cli.no_anim,
                ))?;
            } else {
                rt()?.block_on(agent::run_interactive(&cfg, cli.budget))?;
            }
        }
        // Models/Version (and Tools) short-circuit in the first match at the
        // top of `run` before a provider is resolved; Init is handled by the
        // caller. Reaching any of them here is impossible.
        Command::Init
        | Command::Tools { .. }
        | Command::Graph { .. }
        | Command::Stats { .. }
        | Command::Memory { .. }
        | Command::Mcp { .. }
        | Command::Connect { .. }
        | Command::Observe { .. }
        | Command::Models { .. }
        | Command::Version => {
            unreachable!("handled before provider resolution")
        }
        Command::Config => {
            cfg.print_config();
        }
    }
    Ok(())
}
