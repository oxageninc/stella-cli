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
mod command_deck;
mod config;
mod domains;
mod interactive;
mod memory;
mod settings;
mod stats;
mod tui;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;

/// How turn output reaches the caller (`01-product-spec.md`,
/// `02-architecture.md` §4: stream-json is a line-per-`AgentEvent`
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
    version = version_string(),
    about = "A fast, BYOK, model-agnostic terminal coding agent"
)]
struct Cli {
    /// Override the worker model for this invocation: provider/model_id
    /// (e.g. zai/glm-5.2, anthropic/claude-fable-5, openai/gpt-5.5)
    #[arg(long, env = "STELLA_MODEL")]
    model: Option<String>,

    /// API key for the selected provider, highest-precedence step of the
    /// credential chain (CLI flag -> env var -> credentials file ->
    /// interactive prompt, 01-product-spec.md §4). Prefer an env var or
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
    /// (07-model-matrix.md §6): work aborts cleanly (never mid-tool) once
    /// total spend exceeds this. Omit to meter spend for the cost summary
    /// without ever blocking (observed mode).
    #[arg(long, env = "STELLA_BUDGET", value_parser = parse_budget)]
    budget: Option<f64>,

    /// Use the plain line-based REPL for chat instead of the Command Deck
    /// (the tabbed TUI). The deck also steps aside automatically when stdin
    /// or stdout is not a terminal. Env: STELLA_PLAIN=1.
    #[arg(long)]
    plain: bool,

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
    },

    /// Work in judged rounds until a judge model confirms the goal is met
    Goal {
        /// What must be true when done — assessed by the judge each round
        goal: String,
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

    /// List configured providers and available models
    Models,

    /// Summarize cost, tokens, and resolve rate per provider/model from
    /// local telemetry (.stella/stella.duckdb) — $/resolved-task receipts
    Stats {
        /// Output format: table (aligned, with TOTAL row), json, or csv
        #[arg(long, value_enum, default_value = "table")]
        format: stats::StatsFormat,

        /// Only show executions for this provider id (e.g. zai, anthropic,
        /// local)
        #[arg(long)]
        provider: Option<String>,
    },

    /// Show current configuration
    Config,

    /// Print the version and exit
    Version,
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

/// Whether `chat` should launch the Command Deck: an explicit `--plain` or
/// STELLA_PLAIN=1 opts out, and both stdin and stdout must be real terminals
/// (raw mode + the alternate screen are meaningless on a pipe).
fn use_deck(plain_flag: bool) -> bool {
    let plain_env = std::env::var_os("STELLA_PLAIN").is_some_and(|v| !v.is_empty() && v != "0");
    !plain_flag
        && !plain_env
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
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
        Some(Command::Models) => {
            config::Config::print_available_models();
            return Ok(());
        }
        Some(Command::Tools { validate }) => {
            return match validate {
                // `--validate` (dir optional) is the strict pre-flight path;
                // a plain `stella tools` stays the lenient listing.
                Some(dir) => agent::run_tools_validation(dir.as_deref()),
                None => agent::run_tools_listing(),
            };
        }
        Some(Command::Stats { format, provider }) => {
            // Reads local telemetry only — works with zero API keys.
            // `*format`: this match borrows `&cli.command` (the Tools arm
            // needs `validate` by ref), so `format` binds as `&StatsFormat`;
            // it is `Copy`, so deref rather than move.
            return stats::run_stats(*format, provider.as_deref());
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

    // `init` works offline (heuristic fallback), so config resolution
    // failure downgrades rather than aborting.
    if let Some(Command::Init) = cli.command {
        return rt()?.block_on(agent::run_init(
            cli.model.as_deref(),
            cli.api_key.as_deref(),
            cli.base_url.as_deref(),
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
        } => {
            rt()?.block_on(agent::run_one_shot(
                &cfg,
                &prompt,
                cli.budget,
                cli.output_format,
                !no_pipeline,
            ))?;
        }
        Command::Goal { goal } => {
            rt()?.block_on(agent::run_goal_cmd(&cfg, &goal, cli.budget))?;
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
            rt()?.block_on(agent::run_goal_cmd(&cfg, &goal, cli.budget))?;
        }
        Command::Chat => {
            // The Command Deck (tabbed TUI) is the default chat surface on a
            // real terminal; `--plain` / STELLA_PLAIN=1 / a non-TTY stream
            // falls back to the line-based REPL.
            if use_deck(cli.plain) {
                rt()?.block_on(command_deck::run_deck_session(&cfg, cli.budget))?;
            } else {
                rt()?.block_on(agent::run_interactive(&cfg, cli.budget))?;
            }
        }
        // Models/Version (and Tools) short-circuit in the first match at the
        // top of `run` before a provider is resolved; Init is handled by the
        // caller. Reaching any of them here is impossible.
        Command::Init
        | Command::Tools { .. }
        | Command::Stats { .. }
        | Command::Models
        | Command::Version => {
            unreachable!("handled before provider resolution")
        }
        Command::Config => {
            cfg.print_config();
        }
    }
    Ok(())
}
