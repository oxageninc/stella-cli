//! The opt-in web family: `web_search`, `web_fetch`, `web_extract_assets`,
//! `web_download`.
//!
//! Network egress is never ambient — the whole family registers only when
//! settings opt in with `"tools": {"web": "on"}` (any scope), mirroring the
//! `bash` posture: a fetched page is untrusted input *and* an uncontrolled
//! egress channel, so the host turns it on deliberately. `web_search`
//! additionally needs a BYOK provider key (`BRAVE_API_KEY` or
//! `TAVILY_API_KEY`) — no key, no dead schema, exactly like the media tools.
//!
//! Logged-in fetches ride the user's own sessions via
//! `~/.stella/web_auth.toml` (override with `STELLA_WEB_AUTH_FILE`):
//! per-domain cookies/headers are injected at request time and never appear
//! in tool output, so the secrets never enter the model's context. reqwest
//! strips `Cookie`/`Authorization` on a cross-host redirect; a secret placed
//! in a custom `[domains.x.headers]` entry is NOT in reqwest's sensitive set
//! and would follow the redirect, so scope custom-header secrets to hosts
//! you trust to redirect.
//!
//! No SSRF guard: an opted-in session can fetch any http(s) URL the host can
//! reach — including `localhost` and cloud metadata endpoints. This is
//! deliberate (matching the `bash` opt-in, and required for the "fetch my
//! internal tool / dev server" use case); the gate is the settings opt-in,
//! not a network allowlist.
//!
//! Fetch/extract are `read_only` (they observe the web, not the workspace);
//! `web_download` writes through [`crate::resolve_within_root`] and is
//! classified into the file-touch ledger like `write_file`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::registry::Tool;
use crate::web_extract;

/// Cap on a fetched page or stylesheet — enough for any real document,
/// small enough that a runaway stream can't balloon memory.
const FETCH_CAP_BYTES: usize = 4 * 1024 * 1024;
/// Cap on a `web_download` body.
const DOWNLOAD_CAP_BYTES: usize = 64 * 1024 * 1024;
/// Default cap on rendered `web_fetch` content, in characters.
const DEFAULT_MAX_LENGTH: usize = 30_000;
/// Stylesheets fetched per `web_extract_assets` call by default.
const DEFAULT_MAX_STYLESHEETS: usize = 8;

const DEFAULT_USER_AGENT: &str = concat!("stella/", env!("CARGO_PKG_VERSION"));

const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";

/// The per-domain auth file, parsed once at registry construction. A parse
/// failure is carried as the `Err` and surfaced as a named error on every
/// web call — a broken secrets file must be loud, never silently
/// unauthenticated.
pub type WebAuthState = Result<WebAuthConfig, String>;

/// `web_auth.toml` — the whole file. Unknown keys are a hard parse error
/// (the Toggle discipline: a typo must be loud, not silently ignored).
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebAuthConfig {
    #[serde(default)]
    defaults: WebDefaults,
    /// Keyed by registrable domain (`example.com` also matches
    /// `www.example.com`); the longest matching suffix wins.
    #[serde(default)]
    domains: HashMap<String, DomainAuth>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebDefaults {
    /// Override the `stella/<version>` User-Agent for every request.
    user_agent: Option<String>,
}

/// One domain's request decoration. Values are secrets: the custom `Debug`
/// on [`WebAuthConfig`] redacts them, and no tool output ever echoes them.
#[derive(Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DomainAuth {
    /// Sent as the `Cookie` header — paste a logged-in session's cookies.
    cookie: Option<String>,
    /// Sent as the `Authorization` header.
    authorization: Option<String>,
    /// Arbitrary extra headers.
    #[serde(default)]
    headers: HashMap<String, String>,
    /// Per-domain User-Agent override.
    user_agent: Option<String>,
}

impl std::fmt::Debug for WebAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut domains: Vec<&String> = self.domains.keys().collect();
        domains.sort();
        f.debug_struct("WebAuthConfig")
            .field("domains", &domains)
            .field("values", &"<redacted>")
            .finish()
    }
}

impl WebAuthConfig {
    /// Load `$STELLA_WEB_AUTH_FILE`, else `~/.stella/web_auth.toml`.
    /// A missing file is the empty config; an unreadable or unparseable one
    /// is the `Err` every web tool then reports.
    pub fn load_default() -> WebAuthState {
        let path = match std::env::var_os("STELLA_WEB_AUTH_FILE") {
            Some(explicit) => PathBuf::from(explicit),
            None => match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home).join(".stella").join("web_auth.toml"),
                None => return Ok(Self::default()),
            },
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))
    }

    /// The auth entry for `host`: exact match or subdomain, longest
    /// configured suffix winning (`api.example.com` over `example.com`).
    fn for_host(&self, host: &str) -> Option<(&str, &DomainAuth)> {
        self.domains
            .iter()
            .filter(|(domain, _)| host == domain.as_str() || host.ends_with(&format!(".{domain}")))
            .max_by_key(|(domain, _)| domain.len())
            .map(|(domain, auth)| (domain.as_str(), auth))
    }
}

fn build_client(user_agent: &str) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .user_agent(user_agent)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

/// A fetched response body, capped and annotated.
struct Fetched {
    final_url: Url,
    content_type: String,
    bytes: Vec<u8>,
    truncated: bool,
    /// The `web_auth.toml` domain whose auth decorated the request, if any
    /// — reported by NAME only, never by value.
    authed_domain: Option<String>,
}

/// GET `url_str` with the configured per-domain auth, streaming at most
/// `cap` bytes. Non-2xx is an error carrying a login hint on 401/403.
async fn fetch_raw(url_str: &str, auth: &WebAuthConfig, cap: usize) -> Result<Fetched, String> {
    let url = Url::parse(url_str).map_err(|e| format!("invalid URL `{url_str}`: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "only http/https URLs can be fetched — got scheme `{}`",
            url.scheme()
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("URL `{url_str}` has no host"))?
        .to_string();
    let domain_auth = auth.for_host(&host);
    let user_agent = domain_auth
        .and_then(|(_, a)| a.user_agent.as_deref())
        .or(auth.defaults.user_agent.as_deref())
        .unwrap_or(DEFAULT_USER_AGENT);
    let client = build_client(user_agent)?;
    let mut request = client.get(url.clone());
    let mut authed_domain = None;
    if let Some((domain, entry)) = domain_auth {
        if let Some(cookie) = &entry.cookie {
            request = request.header(reqwest::header::COOKIE, cookie.as_str());
        }
        if let Some(authorization) = &entry.authorization {
            request = request.header(reqwest::header::AUTHORIZATION, authorization.as_str());
        }
        for (name, value) in &entry.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        authed_domain = Some(domain.to_string());
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| format!("fetch of {url} failed: {e}"))?;
    let status = response.status();
    let final_url = response.url().clone();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !status.is_success() {
        let hint = if matches!(status.as_u16(), 401 | 403) && authed_domain.is_none() {
            format!(
                " — if this site needs a login, add a `[domains.\"{host}\"]` entry with your \
                 session cookie to ~/.stella/web_auth.toml"
            )
        } else if matches!(status.as_u16(), 401 | 403) {
            format!(" — the configured auth for `{host}` was sent but rejected; it may be expired")
        } else {
            String::new()
        };
        return Err(format!("HTTP {status} fetching {final_url}{hint}"));
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("reading {final_url}: {e}"))?
    {
        if bytes.len() + chunk.len() > cap {
            bytes.extend_from_slice(&chunk[..cap - bytes.len()]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Fetched {
        final_url,
        content_type,
        bytes,
        truncated,
        authed_domain,
    })
}

/// True when the body is worth rendering as text at all.
fn is_texty(content_type: &str, bytes: &[u8]) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("javascript")
        || ct.contains("html")
        || ct.contains("css")
        || ct.contains("svg")
        || (ct.is_empty() && std::str::from_utf8(bytes).is_ok())
}

fn looks_like_html(content_type: &str, body: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.contains("html")
        || (ct.is_empty()
            && (body.trim_start().starts_with("<!") || body.trim_start().starts_with("<html")))
}

fn auth_note(fetched: &Fetched) -> String {
    match &fetched.authed_domain {
        Some(domain) => format!(", authenticated via web_auth.toml for `{domain}`"),
        None => String::new(),
    }
}

fn require_auth(state: &WebAuthState) -> Result<&WebAuthConfig, ToolOutput> {
    state.as_ref().map_err(|e| ToolOutput::Error {
        message: format!("web_auth.toml is broken — fix or remove it: {e}"),
    })
}

fn require_str<'a>(input: &'a Value, field: &str) -> Result<&'a str, ToolOutput> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ToolOutput::Error {
            message: format!("`{field}` is required"),
        })
}

// web_search

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchProvider {
    Brave,
    Tavily,
}

impl SearchProvider {
    fn id(self) -> &'static str {
        match self {
            SearchProvider::Brave => "brave",
            SearchProvider::Tavily => "tavily",
        }
    }
}

/// A key-authenticated search backend pinned to one endpoint. Tests point
/// `endpoint` at a mock server via [`SearchBackend::with_endpoint`].
#[derive(Clone)]
pub struct SearchBackend {
    provider: SearchProvider,
    key: String,
    endpoint: String,
}

impl std::fmt::Debug for SearchBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchBackend")
            .field("provider", &self.provider)
            .field("key", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl SearchBackend {
    pub fn with_endpoint(
        provider: SearchProvider,
        key: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            key: key.into(),
            endpoint: endpoint.into(),
        }
    }
}

/// Detect a BYOK search backend from the environment: `BRAVE_API_KEY` wins
/// (a dedicated web-search API), then `TAVILY_API_KEY`.
pub fn detect_search_backend() -> Option<SearchBackend> {
    detect_search_backend_with(|name| std::env::var(name).ok())
}

/// [`detect_search_backend`] with the env lookup injectable for tests.
pub fn detect_search_backend_with(env: impl Fn(&str) -> Option<String>) -> Option<SearchBackend> {
    for (var, provider, endpoint) in [
        ("BRAVE_API_KEY", SearchProvider::Brave, BRAVE_ENDPOINT),
        ("TAVILY_API_KEY", SearchProvider::Tavily, TAVILY_ENDPOINT),
    ] {
        if let Some(key) = env(var).filter(|k| !k.trim().is_empty()) {
            return Some(SearchBackend::with_endpoint(provider, key, endpoint));
        }
    }
    None
}

pub struct WebSearch(pub SearchBackend);

#[async_trait]
impl Tool for WebSearch {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_search".into(),
            description: "Search the web and get ranked results (title, URL, snippet). Use for \
                          anything the workspace can't answer: current docs, libraries, news, \
                          designs to reference. Follow up with web_fetch to read a result."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "count": {
                        "type": "integer",
                        "description": "Results to return, 1-20 (default 8)"
                    }
                },
                "required": ["query"]
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let query = match require_str(input, "query") {
            Ok(q) => q,
            Err(e) => return e,
        };
        let count = input
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(8)
            .clamp(1, 20);
        let results = match self.0.provider {
            SearchProvider::Brave => brave_search(&self.0, query, count).await,
            SearchProvider::Tavily => tavily_search(&self.0, query, count).await,
        };
        match results {
            Ok(results) if results.is_empty() => ToolOutput::Ok {
                content: format!("no results for \"{query}\" ({})", self.0.provider.id()),
            },
            Ok(results) => {
                let mut content = format!(
                    "{} results for \"{query}\" ({}):\n",
                    results.len(),
                    self.0.provider.id()
                );
                for (idx, r) in results.iter().enumerate() {
                    content.push_str(&format!(
                        "\n{}. {}\n   {}\n   {}\n",
                        idx + 1,
                        r.title,
                        r.url,
                        r.snippet
                    ));
                }
                ToolOutput::Ok { content }
            }
            Err(message) => ToolOutput::Error { message },
        }
    }
}

struct SearchHit {
    title: String,
    url: String,
    snippet: String,
}

async fn brave_search(
    backend: &SearchBackend,
    query: &str,
    count: u64,
) -> Result<Vec<SearchHit>, String> {
    let client = build_client(DEFAULT_USER_AGENT)?;
    let response = client
        .get(&backend.endpoint)
        .query(&[("q", query), ("count", &count.to_string())])
        .header("X-Subscription-Token", backend.key.as_str())
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("Brave search failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let mut preview = body;
        preview.truncate(300);
        return Err(format!("Brave search: HTTP {status}: {preview}"));
    }
    let json: Value =
        serde_json::from_str(&body).map_err(|e| format!("Brave returned non-JSON: {e}"))?;
    let empty = Vec::new();
    let results = json
        .pointer("/web/results")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    Ok(results
        .iter()
        .map(|r| SearchHit {
            title: r.get("title").and_then(|v| v.as_str()).unwrap_or("").into(),
            url: r.get("url").and_then(|v| v.as_str()).unwrap_or("").into(),
            snippet: r
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
        })
        .collect())
}

async fn tavily_search(
    backend: &SearchBackend,
    query: &str,
    count: u64,
) -> Result<Vec<SearchHit>, String> {
    let client = build_client(DEFAULT_USER_AGENT)?;
    let response = client
        .post(&backend.endpoint)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", backend.key),
        )
        .json(&serde_json::json!({ "query": query, "max_results": count }))
        .send()
        .await
        .map_err(|e| format!("Tavily search failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let mut preview = body;
        preview.truncate(300);
        return Err(format!("Tavily search: HTTP {status}: {preview}"));
    }
    let json: Value =
        serde_json::from_str(&body).map_err(|e| format!("Tavily returned non-JSON: {e}"))?;
    let empty = Vec::new();
    let results = json
        .get("results")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    Ok(results
        .iter()
        .map(|r| SearchHit {
            title: r.get("title").and_then(|v| v.as_str()).unwrap_or("").into(),
            url: r.get("url").and_then(|v| v.as_str()).unwrap_or("").into(),
            snippet: r
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
        })
        .collect())
}

// web_fetch

pub struct WebFetch(pub Arc<WebAuthState>);

#[async_trait]
impl Tool for WebFetch {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_fetch".into(),
            description: "Fetch a URL and read it. HTML is rendered as markdown with absolute \
                          links (default), or as plain text or raw HTML via `format`. Sites \
                          configured in web_auth.toml are fetched with the user's own login. \
                          For binary files use web_download instead."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The http(s) URL to fetch"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["markdown", "text", "html"],
                        "description": "Rendering for HTML pages (default markdown)"
                    },
                    "max_length": {
                        "type": "integer",
                        "description": "Character cap on the returned content (default 30000)"
                    }
                },
                "required": ["url"]
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let auth = match require_auth(&self.0) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let url = match require_str(input, "url") {
            Ok(u) => u,
            Err(e) => return e,
        };
        let format = input
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or("markdown");
        let max_length = input
            .get("max_length")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_LENGTH)
            .clamp(200, 400_000);

        let fetched = match fetch_raw(url, auth, FETCH_CAP_BYTES).await {
            Ok(f) => f,
            Err(message) => return ToolOutput::Error { message },
        };
        if !is_texty(&fetched.content_type, &fetched.bytes) {
            return ToolOutput::Ok {
                content: format!(
                    "{} is binary content ({}, {} bytes{}) — use web_download to save it \
                     into the workspace",
                    fetched.final_url,
                    if fetched.content_type.is_empty() {
                        "unknown type"
                    } else {
                        &fetched.content_type
                    },
                    fetched.bytes.len(),
                    if fetched.truncated {
                        "+, truncated"
                    } else {
                        ""
                    },
                ),
            };
        }
        let body = String::from_utf8_lossy(&fetched.bytes);
        let (title, mut content) = if looks_like_html(&fetched.content_type, &body) {
            match format {
                "html" => (None, body.into_owned()),
                "text" => web_extract::html_to_text(&body),
                _ => web_extract::html_to_markdown(&body, Some(&fetched.final_url)),
            }
        } else {
            (None, body.into_owned())
        };

        let total_chars = content.chars().count();
        let mut truncation_note = String::new();
        if total_chars > max_length {
            content = content.chars().take(max_length).collect();
            truncation_note = format!(
                "\n\n[truncated at {max_length} of {total_chars} chars — raise `max_length` \
                 or fetch a more specific page]"
            );
        } else if fetched.truncated {
            truncation_note = format!(
                "\n\n[response body exceeded the {} MB fetch cap and was cut off]",
                FETCH_CAP_BYTES / (1024 * 1024)
            );
        }

        let header = match title {
            Some(title) => format!("# {title}\n"),
            None => String::new(),
        };
        ToolOutput::Ok {
            content: format!(
                "{header}Source: {} ({}, {} bytes{})\n\n{content}{truncation_note}",
                fetched.final_url,
                if fetched.content_type.is_empty() {
                    "unknown type"
                } else {
                    &fetched.content_type
                },
                fetched.bytes.len(),
                auth_note(&fetched),
            ),
        }
    }
}

// web_extract_assets

pub struct WebExtractAssets(pub Arc<WebAuthState>);

#[async_trait]
impl Tool for WebExtractAssets {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_extract_assets".into(),
            description: "Fetch a page and mine its design assets: stylesheets, scripts, \
                          images, fonts, plus design tokens distilled from the CSS — colors \
                          and font families by frequency, custom properties, @font-face \
                          sources. The starting point for building a design system from an \
                          existing site; save individual assets with web_download."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The http(s) page to analyze"
                    },
                    "max_stylesheets": {
                        "type": "integer",
                        "description": "External stylesheets to fetch and analyze (default 8)"
                    }
                },
                "required": ["url"]
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let auth = match require_auth(&self.0) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let url = match require_str(input, "url") {
            Ok(u) => u,
            Err(e) => return e,
        };
        let max_stylesheets = input
            .get("max_stylesheets")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_STYLESHEETS)
            .clamp(0, 24);

        let fetched = match fetch_raw(url, auth, FETCH_CAP_BYTES).await {
            Ok(f) => f,
            Err(message) => return ToolOutput::Error { message },
        };
        let body = String::from_utf8_lossy(&fetched.bytes);
        if !looks_like_html(&fetched.content_type, &body) {
            return ToolOutput::Error {
                message: format!(
                    "{} is not an HTML page ({}) — web_extract_assets needs a page to parse",
                    fetched.final_url,
                    if fetched.content_type.is_empty() {
                        "unknown type"
                    } else {
                        &fetched.content_type
                    },
                ),
            };
        }
        let manifest = web_extract::extract_assets(&body, &fetched.final_url);

        // Distill design tokens from the inline blocks plus the first N
        // external stylesheets, fetched with the same per-domain auth.
        let mut css = web_extract::CssAccumulator::default();
        for inline in &manifest.inline_css {
            css.add_css(inline, Some(&fetched.final_url));
        }
        let mut sheet_notes: Vec<String> = Vec::new();
        for (idx, sheet_url) in manifest.stylesheets.iter().enumerate() {
            if idx >= max_stylesheets {
                sheet_notes.push(format!(
                    "- {sheet_url} (not fetched — over `max_stylesheets`)"
                ));
                continue;
            }
            match fetch_raw(sheet_url, auth, FETCH_CAP_BYTES).await {
                Ok(sheet) => {
                    let text = String::from_utf8_lossy(&sheet.bytes);
                    if let Ok(sheet_base) = Url::parse(sheet_url) {
                        css.add_css(&text, Some(&sheet_base));
                    } else {
                        css.add_css(&text, Some(&fetched.final_url));
                    }
                    sheet_notes.push(format!(
                        "- {sheet_url} (fetched, {:.1} KB)",
                        sheet.bytes.len() as f64 / 1024.0
                    ));
                }
                Err(e) => sheet_notes.push(format!("- {sheet_url} (fetch failed: {e})")),
            }
        }
        let tokens = css.finish();

        let mut out = format!("# Asset manifest for {}\n", fetched.final_url);
        if let Some(title) = &manifest.title {
            out.push_str(&format!("Title: {title}\n"));
        }
        out.push_str(&format!(
            "Fetched: {} bytes{}\n",
            fetched.bytes.len(),
            auth_note(&fetched)
        ));
        for (name, value) in &manifest.meta {
            out.push_str(&format!("{name}: {value}\n"));
        }

        out.push_str(&format!(
            "\n## Stylesheets ({} external, {} inline blocks)\n",
            manifest.stylesheets.len(),
            manifest.inline_css.len()
        ));
        for note in &sheet_notes {
            out.push_str(note);
            out.push('\n');
        }

        out.push_str("\n## Design tokens\n");
        push_ranked(&mut out, "Colors (by frequency)", &tokens.colors, 24);
        push_ranked(&mut out, "Font families", &tokens.font_families, 12);
        if !tokens.font_faces.is_empty() {
            out.push_str("\n### @font-face\n");
            for face in tokens.font_faces.iter().take(12) {
                out.push_str(&format!(
                    "- \"{}\" ← {}\n",
                    face.family,
                    face.sources.join(", ")
                ));
            }
        }
        if !tokens.custom_props.is_empty() {
            out.push_str(&format!(
                "\n### CSS custom properties ({} shown of {})\n",
                tokens.custom_props.len().min(60),
                tokens.custom_props.len()
            ));
            for (name, value) in tokens.custom_props.iter().take(60) {
                out.push_str(&format!("- {name}: {value}\n"));
            }
        }

        push_list(&mut out, "Preloaded fonts", &manifest.fonts, 20);
        push_list(&mut out, "Images", &manifest.images, 40);
        push_list(&mut out, "Scripts", &manifest.scripts, 20);

        out.push_str(
            "\nURLs are absolute — save any of them into the workspace with web_download \
             (e.g. under .stella/artifacts/web/).",
        );
        ToolOutput::Ok { content: out }
    }
}

fn push_ranked(out: &mut String, heading: &str, ranked: &[(String, usize)], cap: usize) {
    if ranked.is_empty() {
        return;
    }
    out.push_str(&format!("\n### {heading}\n"));
    for (value, count) in ranked.iter().take(cap) {
        out.push_str(&format!("- {value} ({count})\n"));
    }
    if ranked.len() > cap {
        out.push_str(&format!("- … {} more\n", ranked.len() - cap));
    }
}

fn push_list(out: &mut String, heading: &str, items: &[String], cap: usize) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("\n## {heading} ({})\n", items.len()));
    for item in items.iter().take(cap) {
        out.push_str(&format!("- {item}\n"));
    }
    if items.len() > cap {
        out.push_str(&format!("- … {} more\n", items.len() - cap));
    }
}

// web_download

pub struct WebDownload(pub Arc<WebAuthState>);

#[async_trait]
impl Tool for WebDownload {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_download".into(),
            description: "Download a URL to a file inside the workspace (images, fonts, \
                          stylesheets, archives — anything web_fetch reports as binary). \
                          Generated assets belong under .stella/artifacts/web/."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The http(s) URL to download"
                    },
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative destination file path"
                    }
                },
                "required": ["url", "path"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let auth = match require_auth(&self.0) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let url = match require_str(input, "url") {
            Ok(u) => u,
            Err(e) => return e,
        };
        let path = match require_str(input, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let Some(full) = crate::resolve_within_root(root, path) else {
            return ToolOutput::Error {
                message: format!("path `{path}` escapes the workspace root"),
            };
        };
        let fetched = match fetch_raw(url, auth, DOWNLOAD_CAP_BYTES).await {
            Ok(f) => f,
            Err(message) => return ToolOutput::Error { message },
        };
        if fetched.truncated {
            return ToolOutput::Error {
                message: format!(
                    "{} exceeds the {} MB download cap — nothing was written",
                    fetched.final_url,
                    DOWNLOAD_CAP_BYTES / (1024 * 1024)
                ),
            };
        }
        if let Some(parent) = full.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return ToolOutput::Error {
                message: format!("cannot create {}: {e}", parent.display()),
            };
        }
        if let Err(e) = std::fs::write(&full, &fetched.bytes) {
            return ToolOutput::Error {
                message: format!("cannot write {}: {e}", full.display()),
            };
        }
        ToolOutput::Ok {
            content: format!(
                "downloaded {} → {path} ({} bytes, {}{})",
                fetched.final_url,
                fetched.bytes.len(),
                if fetched.content_type.is_empty() {
                    "unknown type"
                } else {
                    &fetched.content_type
                },
                auth_note(&fetched),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_have_the_right_names_and_read_only_partition() {
        let auth: Arc<WebAuthState> = Arc::new(Ok(WebAuthConfig::default()));
        let backend =
            SearchBackend::with_endpoint(SearchProvider::Brave, "k", "https://example.test");
        for (schema, read_only) in [
            (WebSearch(backend).schema(), true),
            (WebFetch(auth.clone()).schema(), true),
            (WebExtractAssets(auth.clone()).schema(), true),
            (WebDownload(auth).schema(), false),
        ] {
            assert!(schema.name.starts_with("web_"), "{}", schema.name);
            assert_eq!(schema.read_only, read_only, "{}", schema.name);
        }
    }

    #[test]
    fn search_backend_detection_prefers_brave_and_skips_blank_keys() {
        let backend = detect_search_backend_with(|name| match name {
            "BRAVE_API_KEY" => Some("bk".into()),
            "TAVILY_API_KEY" => Some("tk".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(backend.provider, SearchProvider::Brave);

        let backend = detect_search_backend_with(|name| match name {
            "BRAVE_API_KEY" => Some("   ".into()),
            "TAVILY_API_KEY" => Some("tk".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(backend.provider, SearchProvider::Tavily);

        assert!(detect_search_backend_with(|_| None).is_none());
    }

    #[test]
    fn domain_auth_matches_subdomains_and_prefers_the_longest_suffix() {
        let config: WebAuthConfig = toml::from_str(
            r#"
            [domains."example.com"]
            cookie = "base"
            [domains."api.example.com"]
            cookie = "api"
            "#,
        )
        .unwrap();
        assert_eq!(config.for_host("example.com").unwrap().0, "example.com");
        assert_eq!(config.for_host("www.example.com").unwrap().0, "example.com");
        assert_eq!(
            config.for_host("api.example.com").unwrap().0,
            "api.example.com"
        );
        assert_eq!(
            config.for_host("v2.api.example.com").unwrap().0,
            "api.example.com"
        );
        assert!(config.for_host("example.org").is_none());
        assert!(config.for_host("notexample.com").is_none());
    }

    #[test]
    fn auth_config_debug_never_leaks_values_and_typos_are_loud() {
        let config: WebAuthConfig = toml::from_str(
            r#"
            [domains."example.com"]
            cookie = "secret-session-value"
            authorization = "Bearer secret-token"
            "#,
        )
        .unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("secret"), "{debug}");
        assert!(debug.contains("example.com"), "{debug}");

        let typo: Result<WebAuthConfig, _> = toml::from_str(
            r#"
            [domains."example.com"]
            cokie = "oops"
            "#,
        );
        assert!(typo.is_err(), "unknown keys must be a loud parse error");
    }

    #[test]
    fn search_backend_debug_never_leaks_the_key() {
        let backend = SearchBackend::with_endpoint(
            SearchProvider::Tavily,
            "tvly-secret",
            "https://api.tavily.com/search",
        );
        let debug = format!("{backend:?}");
        assert!(!debug.contains("tvly-secret"), "{debug}");
    }
}
