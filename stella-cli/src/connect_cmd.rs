//! `stella connect` — establish (and inspect) tracker connections.
//!
//! `connect github` runs the OAuth device flow (or stores a pasted personal
//! access token with `--token`); `connect linear` runs browser OAuth when a
//! Linear app is configured (`STELLA_LINEAR_CLIENT_ID`/`_SECRET`), else
//! stores a personal API key. Credentials land owner-only in
//! `~/.stella/integrations.json` and are consumed by the issue tools
//! (`search_issues`, `create_issue`, …), which register automatically in the
//! next session once a connection exists.

use colored::Colorize as _;
use stella_tools::tracker_auth::{
    ConnectEvent, ConnectionKind, GitHubDeviceConfig, LinearOAuthConfig, TrackerConnection,
    TrackerProvider, TrackerStore, fetch_account_label, github_device_login, linear_oauth_login,
    now_secs,
};

/// Entry point for `stella connect <cmd>`.
pub fn run(cmd: &crate::ConnectCmd) -> Result<(), String> {
    match cmd {
        crate::ConnectCmd::Github { token } => run_github(*token),
        crate::ConnectCmd::Linear { paste_key } => run_linear(*paste_key),
        crate::ConnectCmd::Status => run_status(),
        crate::ConnectCmd::Remove { provider } => run_remove(provider),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to start runtime: {e}"))
}

fn store() -> Result<TrackerStore, String> {
    TrackerStore::open_default()
        .ok_or_else(|| "no resolvable home directory — cannot store connections".to_string())
}

fn print_event(event: ConnectEvent) {
    match event {
        ConnectEvent::Status(line) => println!("  {} {line}", "·".green()),
        ConnectEvent::UserCode { code, url } => {
            println!(
                "  {} open {} and enter code:",
                "◆".yellow(),
                url.bright_magenta()
            );
            println!("\n      {}\n", code.bold().bright_white());
        }
        ConnectEvent::AuthorizeUrl(url) => {
            println!(
                "  {} approve access in your browser (opened automatically):",
                "◆".yellow()
            );
            println!("    {}", url.bright_magenta());
        }
    }
}

/// Best-effort `open`/`xdg-open`/`start`. Failure is fine — the URL is
/// printed for manual opening either way.
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let launcher = ("open", vec![url.to_string()]);
    #[cfg(target_os = "windows")]
    let launcher = ("cmd", vec!["/C".into(), "start".into(), url.to_string()]);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let launcher = ("xdg-open", vec![url.to_string()]);
    let mut command = std::process::Command::new(launcher.0);
    command
        .args(&launcher.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    stella_tools::subprocess_env::scrub_sensitive_std_env(&mut command);
    let _ = command.spawn();
}

/// Masked secret prompt on a real TTY; refuses to run piped (a pasted secret
/// in a script belongs in the environment instead).
fn prompt_secret(prompt: &str) -> Result<String, String> {
    let value = rpassword::prompt_password(prompt)
        .map_err(|e| format!("cannot read secret from terminal: {e}"))?;
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err("empty secret — aborted".to_string());
    }
    Ok(value)
}

/// Verify the fresh connection, stamp its account label, persist it, and
/// print where it landed.
fn finish(
    provider: TrackerProvider,
    mut connection: TrackerConnection,
    rt: &tokio::runtime::Runtime,
) -> Result<(), String> {
    let account = rt.block_on(fetch_account_label(provider, &connection))?;
    connection.account = Some(account.clone());
    let store = store()?;
    store.put(provider, &connection)?;
    println!(
        "  {} connected {} as {} — credentials owner-only in {}",
        "◆".yellow(),
        provider.as_str().bright_magenta(),
        account.bold(),
        TrackerStore::default_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    println!(
        "  {} issue tools (search_issues, create_issue, list_labels, …) register in the next session",
        "·".green()
    );
    Ok(())
}

fn run_github(paste_token: bool) -> Result<(), String> {
    crate::tui::section_header("Connect GitHub");
    let rt = runtime()?;
    let connection = if paste_token {
        let token = prompt_secret("  GitHub personal access token (repo scope): ")?;
        TrackerConnection {
            kind: ConnectionKind::ApiKey,
            access_token: token,
            refresh_token: None,
            expires_at: None,
            token_endpoint: None,
            client_id: None,
            client_secret: None,
            scope: None,
            obtained_at: now_secs(),
            account: None,
        }
    } else {
        let config = GitHubDeviceConfig::resolve()?;
        rt.block_on(github_device_login(&config, &print_event))?
    };
    finish(TrackerProvider::GitHub, connection, &rt)
}

fn run_linear(force_api_key: bool) -> Result<(), String> {
    crate::tui::section_header("Connect Linear");
    let rt = runtime()?;
    let oauth = if force_api_key {
        None
    } else {
        LinearOAuthConfig::resolve()
    };
    let connection = match oauth {
        Some(config) => rt.block_on(linear_oauth_login(&config, &|event| {
            if let ConnectEvent::AuthorizeUrl(url) = &event {
                open_in_browser(url);
            }
            print_event(event);
        }))?,
        None => {
            if !force_api_key {
                println!(
                    "  {} no Linear OAuth app configured (STELLA_LINEAR_CLIENT_ID) — using a \
                     personal API key instead",
                    "·".green()
                );
                println!(
                    "  {} create one at {}",
                    "·".green(),
                    "https://linear.app/settings/api".bright_magenta()
                );
            }
            let key = prompt_secret("  Linear personal API key: ")?;
            TrackerConnection {
                kind: ConnectionKind::ApiKey,
                access_token: key,
                refresh_token: None,
                expires_at: None,
                token_endpoint: None,
                client_id: None,
                client_secret: None,
                scope: None,
                obtained_at: now_secs(),
                account: None,
            }
        }
    };
    finish(TrackerProvider::Linear, connection, &rt)
}

fn run_status() -> Result<(), String> {
    crate::tui::section_header("Tracker connections");
    let connections = store()?.connections()?;
    if connections.is_empty() {
        println!(
            "  {}",
            "none — run `stella connect github` or `stella connect linear`.".dimmed()
        );
    }
    let now = now_secs();
    for (provider, connection) in &connections {
        let kind = match connection.kind {
            ConnectionKind::OAuth => "oauth",
            ConnectionKind::ApiKey => "api key",
        };
        let age_days = now.saturating_sub(connection.obtained_at) / 86_400;
        let expiry = match connection.expires_at {
            Some(at) if at <= now => " — token EXPIRED, reconnect".red().to_string(),
            Some(at) => format!(" — expires in {}h", (at - now) / 3_600),
            None => String::new(),
        };
        println!(
            "  {} {} · {kind} · {} · connected {age_days}d ago{expiry}",
            "◆".yellow(),
            provider.as_str().bright_magenta(),
            connection.account.as_deref().unwrap_or("unverified").bold(),
        );
    }
    // Ambient credentials that also register a backend, and precedence.
    if std::env::var("LINEAR_API_KEY").is_ok_and(|k| !k.trim().is_empty()) {
        println!(
            "  {} LINEAR_API_KEY is set in the environment — it takes precedence over stored \
             connections",
            "·".green()
        );
    }
    println!(
        "  {}",
        "precedence: LINEAR_API_KEY env → linear connection → github connection → gh CLI".dimmed()
    );
    Ok(())
}

fn run_remove(provider: &str) -> Result<(), String> {
    let provider = TrackerProvider::parse(provider)
        .ok_or_else(|| format!("unknown provider `{provider}` — use github|linear"))?;
    if store()?.remove(provider)? {
        println!(
            "  {} disconnected {}",
            "◆".yellow(),
            provider.as_str().bright_magenta()
        );
        Ok(())
    } else {
        Err(format!("no stored connection for `{}`", provider.as_str()))
    }
}
