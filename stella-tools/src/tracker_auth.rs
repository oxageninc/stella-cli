//! OAuth connections to issue trackers (`stella connect github|linear`).
//!
//! Stores tracker credentials in a user-global, owner-only (0600) JSON file
//! — `~/.config/stella/integrations.json` — following the same threat model
//! as `~/.config/stella/credentials.toml` (plaintext secrets guarded by file
//! permissions, no OS keychain). The store is consumed by
//! [`crate::issues::detect_issue_backend`], which is what turns a stored
//! connection into registered issue tools.
//!
//! Two flows live here:
//! - **GitHub device flow** (RFC 8628): a public client — no secret to
//!   embed in an open-source binary — purpose-built for terminals. The user
//!   opens a verification URL and types a short code.
//! - **Linear authorization-code + PKCE** over a loopback redirect. Linear
//!   requires a registered application (client id/secret), so this flow is
//!   available when one is configured; `stella connect linear` falls back
//!   to a personal API key otherwise.
//!
//! Every request here is user-initiated via `stella connect` — Stella's
//! no-phone-home invariant is preserved: nothing in this module runs unless
//! the user explicitly connects a tracker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// GitHub device-flow endpoints (fixed; no discovery for GitHub).
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
/// The OAuth app client id shipped with Stella. Device-flow apps are public
/// clients, so embedding the id (NOT a secret) is standard practice — `gh`
/// does the same. Until the Stella OAuth app is registered this is empty and
/// `STELLA_GITHUB_CLIENT_ID` must be set (or use `stella connect github
/// --token`).
const DEFAULT_GITHUB_CLIENT_ID: &str = "";
/// `repo` covers issue read/write on public and private repositories.
const GITHUB_SCOPE: &str = "repo";

/// Linear OAuth endpoints (fixed).
const LINEAR_AUTHORIZE_URL: &str = "https://linear.app/oauth/authorize";
const LINEAR_TOKEN_URL: &str = "https://api.linear.app/oauth/token";
/// `read,write` covers issue search/create/update/comment.
const LINEAR_SCOPE: &str = "read,write";

/// How long `stella connect` waits for the browser round-trip.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

// ── Providers & stored connections ──────────────────────────────────────────

/// The trackers `stella connect` knows how to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackerProvider {
    GitHub,
    Linear,
}

impl TrackerProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrackerProvider::GitHub => "github",
            TrackerProvider::Linear => "linear",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "github" => Some(TrackerProvider::GitHub),
            "linear" => Some(TrackerProvider::Linear),
            _ => None,
        }
    }
}

/// How the stored credential was obtained — an OAuth grant (refreshable,
/// may expire) or a pasted API key / personal access token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionKind {
    OAuth,
    ApiKey,
}

/// One tracker connection, serialized verbatim into the owner-only store —
/// never into logs (see the redacting `Debug`).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackerConnection {
    pub kind: ConnectionKind,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds after which `access_token` is stale. `None` = no expiry
    /// reported (GitHub OAuth-app tokens, Linear API keys).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Where a refresh grant is sent, when refreshing is possible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Unix seconds when the connection was first established — provenance
    /// for `stella connect status`.
    #[serde(default)]
    pub obtained_at: u64,
    /// Human label for the connected account (login / email), captured at
    /// connect time for `stella connect status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

impl TrackerConnection {
    /// Stale (within 60s of expiry) and therefore due for a refresh.
    pub fn needs_refresh(&self, now_secs: u64) -> bool {
        self.expires_at
            .is_some_and(|at| now_secs.saturating_add(60) >= at)
    }
}

impl std::fmt::Debug for TrackerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackerConnection")
            .field("kind", &self.kind)
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("scope", &self.scope)
            .field("obtained_at", &self.obtained_at)
            .field("account", &self.account)
            .finish()
    }
}

// ── The on-disk store ───────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct IntegrationsFile {
    #[serde(default)]
    trackers: BTreeMap<String, TrackerConnection>,
}

/// The owner-only JSON store at `~/.config/stella/integrations.json`
/// (override with `STELLA_INTEGRATIONS_FILE` — used by tests and unusual
/// setups). User-global on purpose: a GitHub/Linear account connection is a
/// property of the person, not the workspace — unlike `.stella/private/mcp_oauth.json`,
/// which is per-project because MCP server sets are.
#[derive(Debug, Clone)]
pub struct TrackerStore {
    path: PathBuf,
}

impl TrackerStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The default store path, honoring the `STELLA_INTEGRATIONS_FILE`
    /// override. `None` when no home directory is resolvable.
    pub fn default_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("STELLA_INTEGRATIONS_FILE")
            && !path.trim().is_empty()
        {
            return Some(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        Some(
            home.join(".config")
                .join("stella")
                .join("integrations.json"),
        )
    }

    /// A store at the default path, or `None` when homeless (callers treat
    /// that as "no connections").
    pub fn open_default() -> Option<Self> {
        Self::default_path().map(Self::new)
    }

    fn load(&self) -> Result<IntegrationsFile, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                format!(
                    "integrations store {} is corrupt: {e} — delete it and connect again",
                    self.path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IntegrationsFile::default()),
            Err(e) => Err(format!(
                "cannot read integrations store {}: {e}",
                self.path.display()
            )),
        }
    }

    fn save(&self, file: &IntegrationsFile) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(file)
            .map_err(|e| format!("cannot serialize integrations store: {e}"))?;
        // Atomic + owner-only from birth: temp sibling opened 0600, then
        // renamed over the target — same discipline as credentials.toml.
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        write_owner_only(&tmp, json.as_bytes()).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot write {}: {e}", tmp.display())
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot replace {}: {e}", self.path.display())
        })?;
        Ok(())
    }

    pub fn get(&self, provider: TrackerProvider) -> Result<Option<TrackerConnection>, String> {
        Ok(self.load()?.trackers.remove(provider.as_str()))
    }

    pub fn put(
        &self,
        provider: TrackerProvider,
        connection: &TrackerConnection,
    ) -> Result<(), String> {
        let mut file = self.load()?;
        file.trackers
            .insert(provider.as_str().to_string(), connection.clone());
        self.save(&file)
    }

    /// Drop a connection (disconnect); returns whether one existed.
    pub fn remove(&self, provider: TrackerProvider) -> Result<bool, String> {
        let mut file = self.load()?;
        let existed = file.trackers.remove(provider.as_str()).is_some();
        if existed {
            self.save(&file)?;
        }
        Ok(existed)
    }

    /// Providers with stored connections, with their connections.
    pub fn connections(&self) -> Result<Vec<(TrackerProvider, TrackerConnection)>, String> {
        Ok(self
            .load()?
            .trackers
            .into_iter()
            .filter_map(|(name, connection)| TrackerProvider::parse(&name).map(|p| (p, connection)))
            .collect())
    }
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

// ── Login-flow eventing ─────────────────────────────────────────────────────

/// Progress events for a login flow, UI-agnostic — the CLI prints them, the
/// deck can render them. Mirrors `stella-mcp`'s `LoginEvent`.
#[derive(Debug, Clone)]
pub enum ConnectEvent {
    Status(String),
    /// Device flow: show this code and URL to the user.
    UserCode {
        code: String,
        url: String,
    },
    /// Browser flow: open (or show) this URL.
    AuthorizeUrl(String),
}

pub type ConnectNotify<'a> = &'a (dyn Fn(ConnectEvent) + Send + Sync);

// ── GitHub device flow (RFC 8628) ───────────────────────────────────────────

/// Endpoints + client id for the GitHub device flow. `resolve()` builds the
/// production config; tests construct one pointing at a mock server.
#[derive(Debug, Clone)]
pub struct GitHubDeviceConfig {
    pub client_id: String,
    pub device_code_url: String,
    pub token_url: String,
}

impl GitHubDeviceConfig {
    /// The production config: `STELLA_GITHUB_CLIENT_ID` beats the built-in
    /// app id. A clear error when neither is available.
    pub fn resolve() -> Result<Self, String> {
        let client_id = match std::env::var("STELLA_GITHUB_CLIENT_ID") {
            Ok(id) if !id.trim().is_empty() => id,
            _ if !DEFAULT_GITHUB_CLIENT_ID.is_empty() => DEFAULT_GITHUB_CLIENT_ID.to_string(),
            _ => {
                return Err(
                    "no GitHub OAuth client id configured — set STELLA_GITHUB_CLIENT_ID to the \
                     client id of a GitHub OAuth app with device flow enabled, or run \
                     `stella connect github --token` to paste a personal access token instead"
                        .to_string(),
                );
            }
        };
        Ok(Self {
            client_id,
            device_code_url: GITHUB_DEVICE_CODE_URL.to_string(),
            token_url: GITHUB_TOKEN_URL.to_string(),
        })
    }
}

/// Run the device flow to completion: request a device code, surface the
/// user code + verification URL, poll until approved (or denied/expired).
pub async fn github_device_login(
    config: &GitHubDeviceConfig,
    notify: ConnectNotify<'_>,
) -> Result<TrackerConnection, String> {
    let http = http_client()?;

    notify(ConnectEvent::Status("requesting device code…".to_string()));
    let response = http
        .post(&config.device_code_url)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", config.client_id.as_str()),
            ("scope", GITHUB_SCOPE),
        ])
        .send()
        .await
        .map_err(|e| format!("device-code request failed: {e}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("device-code response was not JSON (HTTP {status}): {e}"))?;
    if let Some(error) = body.get("error").and_then(|v| v.as_str()) {
        return Err(format!("device-code request was refused: {error}"));
    }
    let device_code = require_str(&body, "device_code", "device-code response")?;
    let user_code = require_str(&body, "user_code", "device-code response")?;
    let verification_uri = require_str(&body, "verification_uri", "device-code response")?;
    let expires_in = body
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(900);
    let mut interval = body.get("interval").and_then(|v| v.as_u64()).unwrap_or(5);

    notify(ConnectEvent::UserCode {
        code: user_code,
        url: verification_uri,
    });
    notify(ConnectEvent::Status(
        "waiting for approval on github.com…".to_string(),
    ));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires_in);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("device code expired before approval — run connect again".to_string());
        }
        tokio::time::sleep(Duration::from_secs(interval.max(1))).await;

        let response = http
            .post(&config.token_url)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", config.client_id.as_str()),
                ("device_code", device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(|e| format!("device-token poll failed: {e}"))?;
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("device-token response was not JSON: {e}"))?;

        match body.get("error").and_then(|v| v.as_str()) {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                // RFC 8628 §3.5: add 5 seconds to the poll interval.
                interval = interval.saturating_add(5);
                continue;
            }
            Some("expired_token") => {
                return Err("device code expired before approval — run connect again".to_string());
            }
            Some("access_denied") => {
                return Err("authorization was denied on github.com".to_string());
            }
            Some(other) => {
                let description = body
                    .get("error_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                return Err(format!("device flow failed: {other} {description}")
                    .trim()
                    .to_string());
            }
            None => {}
        }

        let access_token = require_str(&body, "access_token", "device-token response")?;
        let now = now_secs();
        return Ok(TrackerConnection {
            kind: ConnectionKind::OAuth,
            access_token,
            refresh_token: body
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            expires_at: body
                .get("expires_in")
                .and_then(|v| v.as_u64())
                .map(|s| now.saturating_add(s)),
            token_endpoint: Some(config.token_url.clone()),
            client_id: Some(config.client_id.clone()),
            client_secret: None,
            scope: body
                .get("scope")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or(Some(GITHUB_SCOPE.to_string())),
            obtained_at: now,
            account: None,
        });
    }
}

// ── Linear authorization-code + PKCE over a loopback redirect ───────────────

/// Endpoints + registered-app credentials for Linear OAuth. `resolve()`
/// reads the environment; tests construct one pointing at a mock server.
#[derive(Clone)]
pub struct LinearOAuthConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub authorize_url: String,
    pub token_url: String,
}

impl std::fmt::Debug for LinearOAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearOAuthConfig")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("authorize_url", &self.authorize_url)
            .field("token_url", &self.token_url)
            .finish()
    }
}

impl LinearOAuthConfig {
    /// The production config from `STELLA_LINEAR_CLIENT_ID` /
    /// `STELLA_LINEAR_CLIENT_SECRET`. `None` when no app is configured —
    /// the caller falls back to an API key.
    pub fn resolve() -> Option<Self> {
        let client_id = std::env::var("STELLA_LINEAR_CLIENT_ID").ok()?;
        if client_id.trim().is_empty() {
            return None;
        }
        let client_secret = std::env::var("STELLA_LINEAR_CLIENT_SECRET")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Some(Self {
            client_id,
            client_secret,
            authorize_url: LINEAR_AUTHORIZE_URL.to_string(),
            token_url: LINEAR_TOKEN_URL.to_string(),
        })
    }
}

/// Run the browser flow to completion: bind a loopback redirect, surface the
/// authorize URL, wait for the code, exchange it for tokens.
pub async fn linear_oauth_login(
    config: &LinearOAuthConfig,
    notify: ConnectNotify<'_>,
) -> Result<TrackerConnection, String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use rand::Rng as _;
    use sha2::Digest as _;

    let http = http_client()?;

    // Loopback redirect target, bound before the URL is constructed.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("cannot bind loopback listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("cannot read loopback address: {e}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    // PKCE S256 + CSRF state.
    let mut random = [0u8; 48];
    rand::rng().fill_bytes(&mut random);
    let verifier = URL_SAFE_NO_PAD.encode(random);
    let challenge = URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let mut random = [0u8; 24];
    rand::rng().fill_bytes(&mut random);
    let state = URL_SAFE_NO_PAD.encode(random);

    let mut authorize = reqwest::Url::parse(&config.authorize_url)
        .map_err(|e| format!("bad authorize URL: {e}"))?;
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("scope", LINEAR_SCOPE)
        .append_pair("actor", "user");
    notify(ConnectEvent::AuthorizeUrl(authorize.to_string()));
    notify(ConnectEvent::Status(
        "waiting for approval in the browser…".to_string(),
    ));

    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(listener, &state))
        .await
        .map_err(|_| {
            format!(
                "login timed out after {}s waiting for the browser",
                LOGIN_TIMEOUT.as_secs()
            )
        })??;

    notify(ConnectEvent::Status(
        "exchanging code for tokens…".to_string(),
    ));
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", config.client_id.clone()),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = &config.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let response = http
        .post(&config.token_url)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // OAuth error bodies carry codes, not credentials — safe to surface.
        let mut preview = text;
        preview.truncate(300);
        return Err(format!("token endpoint returned HTTP {status}: {preview}"));
    }
    let body: Value =
        serde_json::from_str(&text).map_err(|e| format!("malformed token response: {e}"))?;
    let access_token = require_str(&body, "access_token", "token response")?;
    let now = now_secs();
    Ok(TrackerConnection {
        kind: ConnectionKind::OAuth,
        access_token,
        refresh_token: body
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        expires_at: body
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .map(|s| now.saturating_add(s)),
        token_endpoint: Some(config.token_url.clone()),
        client_id: Some(config.client_id.clone()),
        client_secret: config.client_secret.clone(),
        scope: body
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or(Some(LINEAR_SCOPE.to_string())),
        obtained_at: now,
        account: None,
    })
}

/// Refresh an OAuth connection whose token is stale. Returns the refreshed
/// connection (already persisted by the caller). Errors when the connection
/// has no refresh token or no token endpoint.
pub async fn refresh_connection(
    connection: &TrackerConnection,
) -> Result<TrackerConnection, String> {
    let refresh_token = connection
        .refresh_token
        .as_deref()
        .ok_or("connection has no refresh token — run `stella connect` again")?;
    let endpoint = connection
        .token_endpoint
        .as_deref()
        .ok_or("connection has no token endpoint — run `stella connect` again")?;
    let client_id = connection.client_id.as_deref().unwrap_or_default();

    let http = http_client()?;
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", client_id.to_string()),
    ];
    if let Some(secret) = &connection.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let response = http
        .post(endpoint)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("refresh request failed: {e}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("refresh response was not JSON (HTTP {status}): {e}"))?;
    if let Some(error) = body.get("error").and_then(|v| v.as_str()) {
        return Err(format!("refresh was refused: {error}"));
    }
    let access_token = require_str(&body, "access_token", "refresh response")?;
    let now = now_secs();
    let mut refreshed = connection.clone();
    refreshed.access_token = access_token;
    // A rotated refresh token replaces the old one; an absent one keeps it
    // (RFC 6749 §6).
    if let Some(rotated) = body.get("refresh_token").and_then(|v| v.as_str()) {
        refreshed.refresh_token = Some(rotated.to_string());
    }
    refreshed.expires_at = body
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .map(|s| now.saturating_add(s));
    Ok(refreshed)
}

/// Serve the loopback redirect: accept connections until one carries
/// `GET /callback?…` with our `state`, answer with a tiny page, return the
/// authorization code. Adapted from `stella-mcp`'s OAuth login.
async fn wait_for_code(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| format!("loopback accept failed: {e}"))?;

        let mut buf = vec![0u8; 8192];
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => continue,
            Ok(n) => n,
        };
        let request = String::from_utf8_lossy(&buf[..n]);
        let Some(target) = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
        else {
            continue;
        };

        if !target.starts_with("/callback") {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await;
            continue;
        }

        let parsed = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
            .map_err(|e| format!("malformed redirect: {e}"))?;
        let mut code = None;
        let mut state = None;
        let mut error = None;
        let mut error_description = None;
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                "error" => error = Some(value.into_owned()),
                "error_description" => error_description = Some(value.into_owned()),
                _ => {}
            }
        }

        let outcome: Result<String, String> = if let Some(error) = error {
            Err(match error_description {
                Some(desc) => format!("authorization was refused: {error} — {desc}"),
                None => format!("authorization was refused: {error}"),
            })
        } else if state.as_deref() != Some(expected_state) {
            Err("redirect state mismatch — possible CSRF; aborting login".to_string())
        } else if let Some(code) = code {
            Ok(code)
        } else {
            Err("redirect carried neither a code nor an error".to_string())
        };

        let page = match &outcome {
            Ok(_) => {
                "<!doctype html><meta charset=\"utf-8\"><title>stella</title>\
                 <body style=\"font-family:system-ui;padding:3rem\">\
                 <h2>✓ Connected</h2><p>You can close this tab and return to stella.</p>"
            }
            Err(_) => {
                "<!doctype html><meta charset=\"utf-8\"><title>stella</title>\
                 <body style=\"font-family:system-ui;padding:3rem\">\
                 <h2>Connection failed</h2><p>Return to stella for details.</p>"
            }
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;

        return outcome;
    }
}

// ── Endpoint resolution & connection verification ───────────────────────────

/// Linear's GraphQL endpoint (`STELLA_LINEAR_API_URL` overrides — tests,
/// proxies).
pub fn linear_api_url() -> String {
    match std::env::var("STELLA_LINEAR_API_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => "https://api.linear.app/graphql".to_string(),
    }
}

/// GitHub's REST base (`STELLA_GITHUB_API_URL` overrides — tests, GHE).
pub fn github_api_base() -> String {
    match std::env::var("STELLA_GITHUB_API_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => "https://api.github.com".to_string(),
    }
}

/// The Authorization header value a connection authenticates with. Linear
/// personal API keys are sent verbatim; OAuth tokens as `Bearer …`.
pub fn auth_header_value(provider: TrackerProvider, connection: &TrackerConnection) -> String {
    match (provider, connection.kind) {
        (TrackerProvider::Linear, ConnectionKind::ApiKey) => connection.access_token.clone(),
        _ => format!("Bearer {}", connection.access_token),
    }
}

/// Verify a connection by asking the tracker who it belongs to; returns a
/// human account label (`@login` / `name <email>`). Used right after login
/// (proof the credential works) and by `stella connect status`.
pub async fn fetch_account_label(
    provider: TrackerProvider,
    connection: &TrackerConnection,
) -> Result<String, String> {
    let http = http_client()?;
    match provider {
        TrackerProvider::GitHub => {
            let response = http
                .get(format!("{}/user", github_api_base()))
                .header("Authorization", auth_header_value(provider, connection))
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", "stella-cli")
                .send()
                .await
                .map_err(|e| format!("GitHub verification failed: {e}"))?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("GitHub rejected the credential (HTTP {status})"));
            }
            let body: Value = response
                .json()
                .await
                .map_err(|e| format!("GitHub returned non-JSON: {e}"))?;
            require_str(&body, "login", "GitHub /user response").map(|login| format!("@{login}"))
        }
        TrackerProvider::Linear => {
            let response = http
                .post(linear_api_url())
                .header("Authorization", auth_header_value(provider, connection))
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({ "query": "query { viewer { displayName email } }" }))
                .send()
                .await
                .map_err(|e| format!("Linear verification failed: {e}"))?;
            let status = response.status();
            let body: Value = response
                .json()
                .await
                .map_err(|e| format!("Linear returned non-JSON (HTTP {status}): {e}"))?;
            let viewer = &body["data"]["viewer"];
            match (viewer["displayName"].as_str(), viewer["email"].as_str()) {
                (Some(name), Some(email)) => Ok(format!("{name} <{email}>")),
                (None, Some(email)) => Ok(email.to_string()),
                _ => Err(format!(
                    "Linear rejected the credential (HTTP {status}): {}",
                    body["errors"][0]["message"].as_str().unwrap_or("no viewer")
                )),
            }
        }
    }
}

// ── Small helpers ───────────────────────────────────────────────────────────

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))
}

fn require_str(body: &Value, field: &str, context: &str) -> Result<String, String> {
    body.get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{context} had no `{field}`"))
}

/// Unix seconds now — shared by the flows and `stella connect status`.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> TrackerConnection {
        TrackerConnection {
            kind: ConnectionKind::OAuth,
            access_token: "secret-token".into(),
            refresh_token: Some("secret-refresh".into()),
            expires_at: Some(1_900_000_000),
            token_endpoint: Some("https://example.test/token".into()),
            client_id: Some("client".into()),
            client_secret: Some("secret".into()),
            scope: Some("repo".into()),
            obtained_at: 1_800_000_000,
            account: Some("octocat".into()),
        }
    }

    #[test]
    fn debug_never_leaks_secrets() {
        let debug = format!("{:?}", connection());
        assert!(!debug.contains("secret-token"), "{debug}");
        assert!(!debug.contains("secret-refresh"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn store_roundtrips_and_removes() {
        let dir = std::env::temp_dir().join(format!("stella-tracker-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = TrackerStore::new(dir.join("integrations.json"));

        assert!(store.get(TrackerProvider::GitHub).unwrap().is_none());
        store.put(TrackerProvider::GitHub, &connection()).unwrap();
        let loaded = store.get(TrackerProvider::GitHub).unwrap().unwrap();
        assert_eq!(loaded, connection());
        assert_eq!(store.connections().unwrap().len(), 1);
        assert!(store.remove(TrackerProvider::GitHub).unwrap());
        assert!(!store.remove(TrackerProvider::GitHub).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn store_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("stella-tracker-perms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("integrations.json");
        let store = TrackerStore::new(&path);
        store.put(TrackerProvider::Linear, &connection()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "integrations store must be owner-only");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_names_roundtrip() {
        for provider in [TrackerProvider::GitHub, TrackerProvider::Linear] {
            assert_eq!(TrackerProvider::parse(provider.as_str()), Some(provider));
        }
        assert_eq!(
            TrackerProvider::parse("GitHub"),
            Some(TrackerProvider::GitHub)
        );
        assert!(TrackerProvider::parse("jira").is_none());
    }

    #[test]
    fn needs_refresh_respects_skew() {
        let mut c = connection();
        c.expires_at = Some(1_000);
        assert!(c.needs_refresh(950));
        assert!(!c.needs_refresh(800));
        c.expires_at = None;
        assert!(!c.needs_refresh(u64::MAX));
    }
}
