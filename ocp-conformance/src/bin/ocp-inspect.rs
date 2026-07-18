//! `ocp-inspect` — an interactive OCP prober, analogous to MCP's inspector
//!. Point it at a provider; it completes the
//! handshake, prints the negotiated capabilities, optionally fires a test
//! query, and runs the conformance suite — all in human-readable colored
//! output.
//!
//! ```text
//! ocp-inspect stdio [--query GOAL] [--json] -- <program> [args...]
//! ocp-inspect http <url> [--query GOAL] [--json]
//! ```

use clap::{Parser, Subcommand};
use colored::Colorize;
use ocp_conformance::{CheckStatus, ConformanceReport, ProviderTarget, run_conformance};
use ocp_host::{ConsentRecord, Host};
use ocp_types::{Capabilities, ContextQuery, ProviderInfo};

#[derive(Parser)]
#[command(
    name = "ocp-inspect",
    about = "Probe and conformance-test an Open Context Protocol provider."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Probe a stdio child-process provider.
    Stdio {
        /// Fire a test query with this goal after the handshake.
        #[arg(long)]
        query: Option<String>,
        /// Emit the conformance report as JSON instead of colored text.
        #[arg(long)]
        json: bool,
        /// The provider command, after `--`: `<program> [args...]`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Probe a remote HTTP provider.
    Http {
        /// The provider URL.
        url: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

/// A reconstructable target descriptor — `ocp-inspect` establishes the
/// provider twice (once interactively, once for the conformance run), so it
/// keeps the fields rather than a one-shot [`ProviderTarget`].
enum Descriptor {
    Stdio { program: String, args: Vec<String> },
    Http { url: String },
}

impl Descriptor {
    fn to_target(&self) -> ProviderTarget {
        match self {
            Descriptor::Stdio { program, args } => ProviderTarget::Stdio {
                program: program.clone(),
                args: args.clone(),
            },
            Descriptor::Http { url } => ProviderTarget::Http { url: url.clone() },
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let (descriptor, query_goal, json) = match cli.command {
        Command::Stdio {
            query,
            json,
            command,
        } => {
            let mut parts = command.into_iter();
            let program = parts.next().unwrap_or_default();
            let args: Vec<String> = parts.collect();
            (Descriptor::Stdio { program, args }, query, json)
        }
        Command::Http { url, query, json } => (Descriptor::Http { url }, query, json),
    };

    // ── Phase 1: interactive handshake + optional query ──────────────────
    interactive_probe(&descriptor, query_goal.as_deref()).await;

    // ── Phase 2: the conformance verdict ─────────────────────────────────
    let report = run_conformance(descriptor.to_target()).await;
    print_report(&report, json);

    if report.passed() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

async fn interactive_probe(descriptor: &Descriptor, query_goal: Option<&str>) {
    let mut host = Host::new();
    let id = "provider";
    let added = match descriptor {
        Descriptor::Stdio { program, args } => host.add_stdio(id, program, args).await,
        Descriptor::Http { url } => host.add_http(id, url.clone()).await,
    };

    match added {
        Ok(()) => {
            let (info, caps) = match host.provider(id) {
                Some(provider) => (provider.info().clone(), provider.capabilities().clone()),
                None => {
                    println!(
                        "{} provider vanished immediately after a successful handshake",
                        "internal error:".red().bold()
                    );
                    return;
                }
            };
            print_capabilities(&info, &caps);

            if info.data_flow.egress {
                // The operator ran the probe deliberately — consent to the
                // declared flow so the demo query can run.
                host.record_consent(ConsentRecord::new(
                    id,
                    info.data_flow,
                    "ocp-inspect interactive probe",
                ));
            }

            if let Some(goal) = query_goal {
                fire_query(&host, id, goal).await;
            }

            let _ = host.shutdown().await;
        }
        Err(error) => {
            println!("{} {error}", "handshake failed:".red().bold());
        }
    }
}

fn print_capabilities(info: &ProviderInfo, caps: &Capabilities) {
    println!(
        "{}",
        "── OCP provider ──────────────────────────────".dimmed()
    );
    println!(
        "  {} {} {}",
        "provider".bold(),
        info.name.cyan(),
        format!("v{}", info.version).dimmed()
    );

    let flow = format!(
        "reads={} writes={} egress={}",
        info.data_flow.reads, info.data_flow.writes, info.data_flow.egress
    );
    let flow = if info.data_flow.egress {
        flow.yellow()
    } else {
        flow.green()
    };
    println!("  {} {flow}", "data-flow".bold());
    if info.data_flow.egress {
        println!(
            "  {}",
            "⚠ egress: this provider can send data off-machine — consent required (§3.5)".yellow()
        );
    }

    println!(
        "  {} kinds={:?} filters={:?} upsert={} graph={} subscribe={}",
        "capabilities".bold(),
        caps.query.kinds,
        caps.query.filters,
        caps.upsert,
        caps.graph,
        caps.subscribe
    );
    if let Some(fingerprint) = &caps.embeddings_fingerprint {
        println!("  {} {fingerprint}", "embedder".bold());
    }
}

async fn fire_query(host: &Host, id: &str, goal: &str) {
    let query = ContextQuery {
        goal: goal.into(),
        query_text: Some(goal.into()),
        embedding: None,
        kinds: vec![],
        anchors: vec![],
        max_frames: 8,
        max_tokens: 4096,
        as_of: None,
    };
    match host.query_provider(id, &query).await {
        Ok(result) => {
            println!(
                "{}",
                format!("── {} frame(s) for “{goal}” ──", result.frames.len()).dimmed()
            );
            for frame in &result.frames {
                // Cite by human label, never the raw id.
                let label = frame
                    .citation_label
                    .as_deref()
                    .filter(|l| !l.trim().is_empty())
                    .unwrap_or(&frame.title);
                println!(
                    "  {} {}  {}",
                    format!("[{:.2}]", frame.score).dimmed(),
                    label.cyan(),
                    format!("{}tok", frame.token_cost).dimmed()
                );
            }
            if !result.respects_budget(query.max_tokens) {
                println!(
                    "  {}",
                    "⚠ frames exceed the requested budget — a budget-honesty violation".red()
                );
            }
        }
        Err(error) => println!("  {} {error}", "query failed:".red()),
    }
}

fn print_report(report: &ConformanceReport, json: bool) {
    if json {
        match serde_json::to_string_pretty(report) {
            Ok(text) => println!("{text}"),
            Err(error) => eprintln!("could not serialize report: {error}"),
        }
        return;
    }

    println!(
        "{}",
        format!("── conformance: {} ──", report.target).dimmed()
    );
    for check in &report.checks {
        let mark = match check.status {
            CheckStatus::Pass => "✓".green().bold(),
            CheckStatus::Fail => "✗".red().bold(),
            CheckStatus::Skipped => "–".dimmed(),
        };
        let name = match check.status {
            CheckStatus::Pass => check.name.green(),
            CheckStatus::Fail => check.name.red(),
            CheckStatus::Skipped => check.name.dimmed(),
        };
        println!("  {mark} {name}");
        println!("      {}", check.evidence.dimmed());
    }

    let (passed, failed, skipped) = report.tally();
    let verdict = if report.passed() {
        format!("CONFORMANT — {passed} passed, {skipped} skipped")
            .green()
            .bold()
    } else {
        format!("NOT CONFORMANT — {failed} failed, {passed} passed, {skipped} skipped")
            .red()
            .bold()
    };
    println!("  {verdict}");
}
