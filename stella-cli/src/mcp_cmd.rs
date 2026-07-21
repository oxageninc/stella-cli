//! MCP management orchestration: the shared logic behind both the `stella mcp`
//! subcommand and the deck's MCP tab. It owns *where* `.stella/mcp.toml` lives
//! and the registry-URL resolution; `stella-mcp` owns the transport shapes, the
//! registry client, and the install mapping.
//!
//! Nothing here logs a credential value: config is written to disk (the
//! pre-existing `mcp.toml` convention, owner-only where the platform allows),
//! and the [`stella_mcp::McpTransport`] `Debug` redacts values, so a diagnostic
//! never leaks a token.

use std::path::{Path, PathBuf};

use colored::Colorize;
use stella_mcp::{InstallOption, McpConfig, McpTransport, RegistryClient, RegistryPage};

use crate::settings::Settings;

/// The workspace's MCP server config file.
pub fn mcp_toml_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".stella").join("mcp.toml")
}

/// Load `.stella/mcp.toml` (an absent file is an empty config, not an error).
pub fn load_config(workspace_root: &Path) -> Result<McpConfig, String> {
    let path = mcp_toml_path(workspace_root);
    match stella_store::read_sensitive_file_to_string(&path) {
        Ok(text) => McpConfig::from_toml_str(&text)
            .map_err(|e| format!("{} is invalid: {e}", path.display())),
        Err(_)
            if matches!(
                std::fs::symlink_metadata(&path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ) =>
        {
            Ok(McpConfig::default())
        }
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Write `.stella/mcp.toml` atomically (temp + rename), owner-only on Unix
/// since it may hold credentials.
pub fn save_config(workspace_root: &Path, cfg: &McpConfig) -> Result<(), String> {
    let path = mcp_toml_path(workspace_root);
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let toml = cfg.to_toml_string().map_err(|e| e.to_string())?;
    stella_store::write_sensitive_file_atomic(&path, toml.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The configured MCP registry URL (settings.json `mcp.registry_url`, else the
/// official default).
pub fn resolve_registry_url(workspace_root: &Path) -> String {
    Settings::load(workspace_root)
        .map(|s| s.mcp_registry_url())
        .unwrap_or_else(|_| stella_mcp::DEFAULT_REGISTRY_URL.to_string())
}

/// Deck-mode report for MCP connection outcomes, including total failure.
pub(crate) fn mcp_outcome_report(connected: &[&str], failed: &[(String, String)]) -> String {
    let mut lines = match connected.len() {
        0 => vec!["no MCP servers connected — continuing with native tools only".to_string()],
        n => vec![format!(
            "{n} MCP server(s) connected: {}",
            connected.join(", ")
        )],
    };
    lines.extend(
        failed
            .iter()
            .map(|(name, reason)| format!("MCP server `{name}` unavailable: {reason}")),
    );
    lines.join("\n")
}

/// Search a registry over HTTP (async, non-blocking).
pub async fn search(
    registry_url: &str,
    query: Option<&str>,
    cursor: Option<&str>,
    limit: u32,
) -> Result<RegistryPage, String> {
    let client = RegistryClient::new(registry_url).map_err(|e| e.to_string())?;
    client
        .search(query, cursor, limit)
        .await
        .map_err(|e| e.to_string())
}

/// Install (or overwrite — MCP servers are not versioned) one server entry.
pub fn install(workspace_root: &Path, alias: &str, transport: McpTransport) -> Result<(), String> {
    let mut cfg = load_config(workspace_root)?;
    cfg.upsert(alias, transport);
    save_config(workspace_root, &cfg)
}

/// Set a credential (env var for stdio, header for http) on a configured
/// server — the auth / re-auth write path. The value is never logged.
pub fn set_credential(
    workspace_root: &Path,
    server: &str,
    field: &str,
    value: String,
) -> Result<(), String> {
    let mut cfg = load_config(workspace_root)?;
    let transport = cfg.get_mut(server).ok_or_else(|| {
        format!(
            "no MCP server `{server}` in {}",
            mcp_toml_path(workspace_root).display()
        )
    })?;
    transport.set_credential(field, value);
    save_config(workspace_root, &cfg)
}

/// Remove a configured server; returns whether it existed.
pub fn remove(workspace_root: &Path, name: &str) -> Result<bool, String> {
    let mut cfg = load_config(workspace_root)?;
    let removed = cfg.remove(name);
    if removed {
        save_config(workspace_root, &cfg)?;
    }
    Ok(removed)
}

/// Resolve the workspace's owner-only OAuth token store, migrating a safe
/// legacy `.stella/mcp_oauth.json` before any caller constructs a token store.
pub fn oauth_store_path(workspace_root: &Path) -> Result<PathBuf, String> {
    stella_store::workspace_private_state_path(workspace_root, "mcp_oauth.json")
        .map_err(|e| format!("cannot resolve private MCP OAuth token store: {e}"))
}

/// The session's OAuth manager: lazy per-server bearer sources over the
/// workspace token store. Cheap; construct once per connect.
pub fn oauth_manager(
    workspace_root: &Path,
) -> Result<std::sync::Arc<stella_mcp::OAuthManager>, String> {
    Ok(std::sync::Arc::new(stella_mcp::OAuthManager::new(
        oauth_store_path(workspace_root)?,
    )))
}

/// Look up the URL a configured **http** server points at — the OAuth login
/// target. A stdio server has no authorization server, so it is an error.
pub fn http_server_url(workspace_root: &Path, server: &str) -> Result<String, String> {
    let cfg = load_config(workspace_root)?;
    match cfg.get(server) {
        Some(stella_mcp::McpTransport::Http { url, .. }) => Ok(url.clone()),
        Some(_) => Err(format!(
            "`{server}` is a stdio server — OAuth login applies to http servers only"
        )),
        None => Err(format!(
            "no MCP server `{server}` in {}",
            mcp_toml_path(workspace_root).display()
        )),
    }
}

/// Run the interactive OAuth login for a configured http server, emitting
/// progress through `notify` (the CLI prints; the deck forwards to its MCP
/// tab). The browser is opened best-effort for `AuthorizeUrl` events — the
/// URL is always also surfaced so the user can open it by hand.
pub async fn oauth_login(
    workspace_root: &Path,
    server: &str,
    notify: &mut (dyn FnMut(stella_mcp::LoginEvent) + Send),
) -> Result<(), String> {
    let url = http_server_url(workspace_root, server)?;
    let store_path = oauth_store_path(workspace_root)?;
    let tokens = stella_mcp::oauth::login(
        server,
        &url,
        &stella_mcp::LoginOptions::default(),
        &mut |event| {
            if let stella_mcp::LoginEvent::AuthorizeUrl(url) = &event {
                open_in_browser(url);
            }
            notify(event);
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    stella_mcp::TokenStore::new(store_path)
        .put(server, &tokens)
        .map_err(|e| e.to_string())
}

/// Forget a server's OAuth tokens; returns whether any existed.
pub fn oauth_logout(workspace_root: &Path, server: &str) -> Result<bool, String> {
    stella_mcp::TokenStore::new(oauth_store_path(workspace_root)?)
        .remove(server)
        .map_err(|e| e.to_string())
}

/// The configured servers that currently hold OAuth logins.
pub fn oauth_logged_in(workspace_root: &Path) -> Result<Vec<String>, String> {
    stella_mcp::TokenStore::new(oauth_store_path(workspace_root)?)
        .logged_in_servers()
        .map_err(|e| e.to_string())
}

/// Best-effort `open`/`xdg-open`/`start` of the authorize URL. Failure is
/// fine — the URL is printed/shown for manual opening either way.
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let launcher = ("open", vec![url.to_string()]);
    #[cfg(target_os = "windows")]
    let launcher = ("cmd", vec!["/C".into(), "start".into(), url.to_string()]);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let launcher = ("xdg-open", vec![url.to_string()]);
    let _ = std::process::Command::new(launcher.0)
        .args(&launcher.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Per-(server, tool) usage aggregates from local telemetry
/// (`.stella/private/store.db`). Missing store → empty (never creates the file).
pub fn usage_stats(workspace_root: &Path) -> Result<Vec<stella_store::McpUsageStat>, String> {
    if stella_store::existing_workspace_private_sqlite_path(workspace_root, "store.db")
        .map_err(|e| format!("cannot resolve store: {e}"))?
        .is_none()
    {
        return Ok(Vec::new());
    }
    let store =
        stella_store::Store::open(workspace_root).map_err(|e| format!("cannot open store: {e}"))?;
    store
        .mcp_usage_stats()
        .map_err(|e| format!("cannot read MCP usage: {e}"))
}

/// Resolve a registry server name to `(alias, first install option)` — the
/// non-interactive install path (`stella mcp install <name>`). Prefers an exact
/// name match; a server with neither a runnable package nor a remote errors.
pub async fn resolve_install(
    registry_url: &str,
    name: &str,
) -> Result<(String, InstallOption), String> {
    let page = search(registry_url, Some(name), None, 30).await?;
    let entry = page
        .entries
        .into_iter()
        .find(|e| e.server.name == name)
        .ok_or_else(|| {
            format!("no registry server named `{name}` — try `stella mcp search {name}`")
        })?;
    let alias = entry.server.default_alias();
    let mut options = entry.server.install_options();
    if options.is_empty() {
        return Err(format!(
            "`{name}` publishes neither a runnable package nor a remote endpoint"
        ));
    }
    Ok((alias, options.remove(0)))
}

// ── `stella mcp` subcommand ──────────────────────────────────────────────────

/// Entry point for `stella mcp <cmd>`. Enable/disable are deliberately absent:
/// they are session-scoped (a running conversation's tool set), so they live in
/// the deck's MCP tab, not in a stateless CLI invocation.
pub fn run(cmd: &crate::McpCmd) -> Result<(), String> {
    let workspace_root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;
    match cmd {
        crate::McpCmd::List => run_list(&workspace_root),
        crate::McpCmd::Search { query, limit } => run_search(
            &workspace_root,
            &query.join(" "),
            limit.unwrap_or(stella_mcp::registry::DEFAULT_PAGE_LIMIT),
        ),
        crate::McpCmd::Install { name, alias } => run_install(&workspace_root, name, alias.clone()),
        crate::McpCmd::Remove { name } => run_remove(&workspace_root, name),
        crate::McpCmd::Login { name } => run_login(&workspace_root, name),
        crate::McpCmd::Logout { name } => run_logout(&workspace_root, name),
        crate::McpCmd::Usage => run_usage(&workspace_root),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to start runtime: {e}"))
}

fn run_list(workspace_root: &Path) -> Result<(), String> {
    crate::tui::section_header("Configured MCP servers");
    let cfg = load_config(workspace_root)?;
    if cfg.names().is_empty() {
        println!(
            "  {}",
            "none — `stella mcp search <query>` then `stella mcp install <name>`".dimmed()
        );
        return Ok(());
    }
    for name in cfg.names() {
        let transport = cfg.get(name).expect("name came from the config");
        let auth = if transport.has_credentials() {
            format!("· auth: {}", transport.credential_names().join(", ")).dimmed()
        } else {
            "· no auth".dimmed()
        };
        println!(
            "  {} {} {} {}",
            "·".green(),
            name.bright_magenta(),
            format!("[{}]", transport.kind_label()).dimmed(),
            auth
        );
    }
    println!(
        "\n  {}",
        "enable/disable is per-session — toggle servers live in the deck's MCP tab (/mcp)."
            .dimmed()
    );
    Ok(())
}

fn run_search(workspace_root: &Path, query: &str, limit: u32) -> Result<(), String> {
    let registry_url = resolve_registry_url(workspace_root);
    crate::tui::section_header(&format!("MCP registry search — {registry_url}"));
    let query_opt = (!query.trim().is_empty()).then_some(query);
    let page = runtime()?.block_on(search(&registry_url, query_opt, None, limit))?;
    if page.entries.is_empty() {
        println!("  {}", "no matching servers".dimmed());
        return Ok(());
    }
    // The registry returns one row per published version; collapse to one row
    // per server name (MCP servers are not versioned in stella's config).
    let mut seen = std::collections::HashSet::new();
    for entry in page
        .entries
        .iter()
        .filter(|e| seen.insert(e.server.name.clone()))
    {
        let server = &entry.server;
        let kinds = install_kinds(server);
        println!(
            "  {} {} {}",
            "·".green(),
            server.name.bright_magenta(),
            format!("[{kinds}]").dimmed()
        );
        if let Some(desc) = &server.description {
            println!("      {}", truncate(desc, 100).dimmed());
        }
    }
    if page.next_cursor.is_some() {
        println!("\n  {}", "more results available (pagination)".dimmed());
    }
    println!("\n  {}", "install with: stella mcp install <name>".dimmed());
    Ok(())
}

fn run_install(workspace_root: &Path, name: &str, alias: Option<String>) -> Result<(), String> {
    let registry_url = resolve_registry_url(workspace_root);
    let (default_alias, option) = runtime()?.block_on(resolve_install(&registry_url, name))?;
    let alias = alias.unwrap_or(default_alias);
    install(workspace_root, &alias, option.transport)?;
    println!(
        "  {} installed {} as {} ({})",
        "◆".bright_cyan(),
        name.bright_magenta(),
        alias.bright_magenta(),
        option.label.dimmed()
    );
    if !option.auth.is_empty() {
        let required: Vec<&str> = option
            .auth
            .iter()
            .filter(|f| f.required || f.secret)
            .map(|f| f.name.as_str())
            .collect();
        if !required.is_empty() {
            println!(
                "  {} needs credentials: {} — set them in the deck's MCP tab (a) or edit {}",
                "!".yellow(),
                required.join(", "),
                mcp_toml_path(workspace_root).display()
            );
        }
    }
    Ok(())
}

fn run_remove(workspace_root: &Path, name: &str) -> Result<(), String> {
    if remove(workspace_root, name)? {
        println!("  {} removed {}", "◆".bright_cyan(), name.bright_magenta());
        Ok(())
    } else {
        Err(format!("no configured MCP server named `{name}`"))
    }
}

fn run_login(workspace_root: &Path, name: &str) -> Result<(), String> {
    crate::tui::section_header(&format!("OAuth login — {name}"));
    runtime()?.block_on(oauth_login(
        workspace_root,
        name,
        &mut |event| match event {
            stella_mcp::LoginEvent::Status(line) => println!("  {} {line}", "·".green()),
            stella_mcp::LoginEvent::AuthorizeUrl(url) => {
                println!(
                    "  {} approve access in your browser (opened automatically):",
                    "◆".bright_cyan()
                );
                println!("    {}", url.bright_magenta());
            }
        },
    ))?;
    println!(
        "  {} logged in — tokens in {} (auto-refreshed; `stella mcp logout {name}` to forget)",
        "◆".bright_cyan(),
        oauth_store_path(workspace_root)?.display()
    );
    Ok(())
}

fn run_logout(workspace_root: &Path, name: &str) -> Result<(), String> {
    if oauth_logout(workspace_root, name)? {
        println!(
            "  {} logged out of {}",
            "◆".bright_cyan(),
            name.bright_magenta()
        );
        Ok(())
    } else {
        Err(format!("no stored OAuth login for `{name}`"))
    }
}

fn run_usage(workspace_root: &Path) -> Result<(), String> {
    crate::tui::section_header("MCP tool usage (.stella/private/store.db)");
    let stats = usage_stats(workspace_root)?;
    if stats.is_empty() {
        println!(
            "  {}",
            "no MCP tool calls recorded yet — run a session that uses an MCP server.".dimmed()
        );
        return Ok(());
    }
    for stat in &stats {
        let reason = if stat.last_reason.is_empty() {
            String::new()
        } else {
            format!("· {}", truncate(&stat.last_reason, 60))
                .dimmed()
                .to_string()
        };
        println!(
            "  {} {} {} {} {}",
            "·".green(),
            format!("{}×", stat.calls).bright_magenta(),
            stat.server.bright_magenta(),
            stat.tool,
            reason
        );
    }
    Ok(())
}

/// A compact "npm, remote, …" list of a server's install kinds, for search.
pub(crate) fn install_kinds(server: &stella_mcp::RegistryServer) -> String {
    let mut kinds: Vec<String> = Vec::new();
    if !server.remotes.is_empty() {
        kinds.push("remote".to_string());
    }
    for pkg in &server.packages {
        if !pkg.registry_type.is_empty() && !kinds.iter().any(|k| k == &pkg.registry_type) {
            kinds.push(pkg.registry_type.clone());
        }
    }
    if kinds.is_empty() {
        "no install target".to_string()
    } else {
        kinds.join(", ")
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn install_load_and_remove_roundtrip_through_mcp_toml() {
        let dir = std::env::temp_dir().join(format!("stella-mcp-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Absent file → empty config.
        assert!(load_config(&dir).unwrap().names().is_empty());

        // Install a stdio server → it round-trips through the file.
        let transport = McpTransport::Stdio {
            cmd: "npx".into(),
            args: vec!["-y".into(), "some-mcp".into()],
            env: BTreeMap::new(),
        };
        install(&dir, "some", transport.clone()).unwrap();
        let cfg = load_config(&dir).unwrap();
        assert_eq!(cfg.names(), vec!["some"]);
        assert_eq!(cfg.get("some"), Some(&transport));

        // Auth sets a credential without disturbing the rest.
        set_credential(&dir, "some", "API_KEY", "secret".into()).unwrap();
        let cfg = load_config(&dir).unwrap();
        assert!(cfg.get("some").unwrap().has_credentials());
        // The written file must not contain the raw value under a Debug dump.
        assert!(!format!("{:?}", cfg.get("some").unwrap()).contains("secret"));

        // Remove.
        assert!(remove(&dir, "some").unwrap());
        assert!(!remove(&dir, "some").unwrap());
        assert!(load_config(&dir).unwrap().names().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn credential_config_is_owner_only_and_rejects_symlink_targets() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let cfg = McpConfig::default();
        save_config(dir.path(), &cfg).unwrap();
        assert_eq!(
            std::fs::metadata(mcp_toml_path(dir.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let target = dir.path().join("outside.toml");
        std::fs::write(&target, "[servers]\n").unwrap();
        std::fs::remove_file(mcp_toml_path(dir.path())).unwrap();
        symlink(&target, mcp_toml_path(dir.path())).unwrap();
        assert!(save_config(dir.path(), &cfg).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "[servers]\n");
    }

    #[cfg(unix)]
    #[test]
    fn oauth_path_migrates_a_safe_legacy_token_store_into_private_state() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let dot = dir.path().join(".stella");
        std::fs::create_dir_all(&dot).unwrap();
        std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o700)).unwrap();
        let previous_generated = "*.db\n*.db-wal\n*.db-shm\nreflections.jsonl\n";
        let custom = "# keep this custom rule\nexports/\n";
        let ignore_path = dot.join(".gitignore");
        std::fs::write(&ignore_path, format!("{previous_generated}{custom}")).unwrap();
        std::fs::set_permissions(&ignore_path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let legacy = dot.join("mcp_oauth.json");
        std::fs::write(&legacy, br#"{"servers":{}}"#).unwrap();

        let resolved: Result<PathBuf, String> = oauth_store_path(dir.path());
        let resolved = resolved.unwrap();
        assert_eq!(resolved, dot.join("private/mcp_oauth.json"));
        assert!(!legacy.exists());
        assert_eq!(std::fs::read(resolved).unwrap(), br#"{"servers":{}}"#);
        assert_eq!(
            std::fs::read_to_string(&ignore_path).unwrap(),
            format!("{previous_generated}{custom}private/\n")
        );
        assert_eq!(
            std::fs::metadata(&ignore_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640,
            "committable ignore mode must survive the atomic update"
        );
        oauth_store_path(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&ignore_path)
                .unwrap()
                .lines()
                .filter(|line| *line == "private/")
                .count(),
            1,
            "idempotent resolution must not duplicate the ignore rule"
        );

        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        let ignored = std::process::Command::new("git")
            .args(["check-ignore", ".stella/private/mcp_oauth.json"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(ignored.status.success(), "OAuth tokens must never stage");
    }
}
