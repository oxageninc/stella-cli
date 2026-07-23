//! `stella usage` — the cross-project telemetry hub (`~/.stella/usage.db`),
//! and `stella cloud` — the stub registration that scopes it to a cloud org.
//!
//! `usage report` reads only the hub: it works even while project stores are
//! locked by live sessions, and it still covers projects whose checkouts
//! were deleted. `usage sync` replicates the current workspace's telemetry
//! above the hub cursor; `--all` walks every project the hub has ever seen
//! and heals any cursor that fell behind (the repair path for turns whose
//! best-effort sync failed).
//!
//! `cloud register` is the deliberate stub for cloud accounts: it persists
//! `org_id` in `~/.stella/cloud.json` (where the future OAuth login will
//! keep its token) and mints the durable, committable per-workspace
//! `workspace_id`. Until registration, hub rows carry NULL org/workspace ids.

use std::path::Path;

use clap::Subcommand;
use colored::Colorize as _;
use stella_store::usage::UsageStore;
use stella_store::{Store, identity};

use crate::stats::StatsFormat;

#[derive(Subcommand, Clone)]
pub enum UsageCmd {
    /// Global telemetry report from the hub: per (org, provider, model)
    /// calls, tokens, cache reads, cost, and contributing projects
    Report {
        /// Output format: table (aligned) or json
        #[arg(long, value_enum, default_value = "table")]
        format: StatsFormat,

        /// Only rows replicated under this org id
        #[arg(long)]
        org: Option<String>,
    },
    /// Replicate this workspace's telemetry into the hub (cursor-based;
    /// safe to re-run). --all heals every project the hub knows about
    Sync {
        /// Walk the hub's project registry instead of just the cwd
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand, Clone)]
pub enum CloudCmd {
    /// Show this installation's cloud identity: org, workspace, repo ids
    Status,
    /// Register to a cloud org (stub: persists ids locally; the OAuth
    /// login that will attach a token lands later)
    Register {
        /// The cloud organization id telemetry rolls up under
        #[arg(long)]
        org: String,

        /// Adopt a cloud-assigned workspace id instead of minting one
        #[arg(long)]
        workspace_id: Option<String>,
    },
}

pub fn run_usage(cmd: Option<UsageCmd>) -> Result<(), String> {
    match cmd.unwrap_or(UsageCmd::Report {
        format: StatsFormat::Table,
        org: None,
    }) {
        UsageCmd::Report { format, org } => report(format, org.as_deref()),
        UsageCmd::Sync { all } => sync(all),
    }
}

fn open_hub() -> Result<UsageStore, String> {
    UsageStore::open_default().map_err(|e| format!("cannot open the usage hub: {e}"))
}

fn report(format: StatsFormat, org: Option<&str>) -> Result<(), String> {
    let hub = open_hub()?;
    let rows = hub
        .global_telemetry_totals(org)
        .map_err(|e| format!("cannot read the usage hub: {e}"))?;
    match format {
        StatsFormat::Json => {
            let objects: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "org_id": r.org_id,
                        "provider": r.provider,
                        "model": r.model,
                        "calls": r.calls,
                        "input_tokens": r.input_tokens,
                        "output_tokens": r.output_tokens,
                        "cache_read_tokens": r.cache_read_tokens,
                        "cost_usd": r.cost_usd,
                        "projects": r.projects,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&objects).map_err(|e| e.to_string())?
            );
        }
        StatsFormat::Csv => {
            println!(
                "org_id,provider,model,calls,input_tokens,output_tokens,cache_read_tokens,cost_usd,projects"
            );
            for r in &rows {
                println!(
                    "{},{},{},{},{},{},{},{},{}",
                    r.org_id.as_deref().unwrap_or(""),
                    r.provider,
                    r.model,
                    r.calls,
                    r.input_tokens,
                    r.output_tokens,
                    r.cache_read_tokens,
                    r.cost_usd,
                    r.projects,
                );
            }
        }
        StatsFormat::Table => {
            if rows.is_empty() {
                println!(
                    "usage hub is empty — run `stella usage sync` in a workspace \
                     (or finish a turn) to replicate telemetry"
                );
                return Ok(());
            }
            println!(
                "{:<14} {:<10} {:<28} {:>8} {:>12} {:>12} {:>12} {:>10} {:>9}",
                "ORG",
                "PROVIDER",
                "MODEL",
                "CALLS",
                "INPUT",
                "OUTPUT",
                "CACHE-READ",
                "COST",
                "PROJECTS"
            );
            let mut totals = (0i64, 0i64, 0i64, 0i64, 0f64);
            for r in &rows {
                println!(
                    "{:<14} {:<10} {:<28} {:>8} {:>12} {:>12} {:>12} {:>10.4} {:>9}",
                    r.org_id.as_deref().unwrap_or("(local)"),
                    r.provider,
                    r.model,
                    r.calls,
                    r.input_tokens,
                    r.output_tokens,
                    r.cache_read_tokens,
                    r.cost_usd,
                    r.projects,
                );
                totals.0 += r.calls;
                totals.1 += r.input_tokens;
                totals.2 += r.output_tokens;
                totals.3 += r.cache_read_tokens;
                totals.4 += r.cost_usd;
            }
            println!(
                "{:<14} {:<10} {:<28} {:>8} {:>12} {:>12} {:>12} {:>10.4} {:>9}",
                "TOTAL", "", "", totals.0, totals.1, totals.2, totals.3, totals.4, ""
            );
        }
    }
    Ok(())
}

fn sync(all: bool) -> Result<(), String> {
    let hub = open_hub()?;
    if !all {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let shipped = sync_one(&hub, &cwd)?;
        println!("replicated {shipped} telemetry row(s) into the hub");
        return Ok(());
    }
    let projects = hub
        .registered_projects()
        .map_err(|e| format!("cannot list hub projects: {e}"))?;
    if projects.is_empty() {
        println!("the hub has no registered projects yet");
        return Ok(());
    }
    let mut total = 0u64;
    for (_, name, root) in &projects {
        let root = Path::new(root);
        if !root.join(".stella/private/store.db").is_file() {
            println!("{:<24} skipped (no local store)", name);
            continue;
        }
        match sync_one(&hub, root) {
            Ok(shipped) => {
                total += shipped;
                println!("{:<24} {shipped} row(s)", name);
            }
            Err(error) => println!("{:<24} failed: {error}", name),
        }
    }
    println!(
        "replicated {total} telemetry row(s) across {} project(s)",
        projects.len()
    );
    Ok(())
}

fn sync_one(hub: &UsageStore, root: &Path) -> Result<u64, String> {
    let store = Store::open(root).map_err(|e| format!("cannot open workspace store: {e}"))?;
    store
        .replicate_telemetry_to_usage(hub, root)
        .map_err(|e| format!("replication failed: {e}"))
}

pub fn run_cloud(cmd: CloudCmd) -> Result<(), String> {
    match cmd {
        CloudCmd::Status => {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            let scope = identity::TelemetryScope::resolve(&cwd);
            let registered = |v: &Option<String>| match v {
                Some(id) => id.clone(),
                None => "(not registered)".to_string(),
            };
            println!("{}   {}", "org_id:".bold(), registered(&scope.org_id));
            println!(
                "{}   {}",
                "workspace:".bold(),
                registered(&scope.workspace_id)
            );
            println!("{}   {}", "repo_id:".bold(), scope.repo_id);
            println!("{}   {}", "project:".bold(), scope.project_id);
            if scope.org_id.is_none() {
                println!(
                    "\nregister with `stella cloud register --org <org-id>` — \
                     telemetry replicates locally either way; org scoping gates \
                     what a future cloud sync would ship"
                );
            }
            Ok(())
        }
        CloudCmd::Register { org, workspace_id } => {
            let org = org.trim().to_string();
            if org.is_empty() {
                return Err("--org must not be empty".into());
            }
            let mut reg = identity::cloud_registration();
            reg.org_id = Some(org.clone());
            identity::save_cloud_registration(&reg).map_err(|e| e.to_string())?;
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            let ws = identity::register_workspace(&cwd, workspace_id.as_deref())
                .map_err(|e| e.to_string())?;
            println!(
                "registered to org {} (stub — OAuth login lands later)",
                org.bold()
            );
            println!("workspace id {ws} written to .stella/workspace.json");
            println!("commit .stella/workspace.json so every clone reports as this workspace");
            Ok(())
        }
    }
}
