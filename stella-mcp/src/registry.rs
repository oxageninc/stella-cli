//! A client for the **MCP Server Registry API** — the generic, OpenAPI-frozen
//! `GET /v0.1/servers` surface the official registry
//! (<https://registry.modelcontextprotocol.io>) serves and that alternative
//! registries implement to the same shape. This module owns three things:
//!
//! 1. **Wire types** ([`RegistryServer`], [`RegistryPackage`], [`RegistryRemote`],
//!    …) — tolerant by construction (every field defaults, unknown fields are
//!    ignored) so a registry on a newer minor revision never breaks parsing.
//! 2. **[`RegistryClient`]** — an async, non-blocking search over the frozen
//!    endpoint with substring search + cursor pagination. It reuses the
//!    workspace's `reqwest` client exactly as [`crate::http`] does; parsing is
//!    split out ([`RegistryClient::parse_page`]) so tests exercise recorded
//!    responses with **no live network**.
//! 3. **Install mapping** ([`RegistryServer::install_options`]) — turns a
//!    registry entry's `packages` (npm/pypi/oci → a spawnable stdio command)
//!    and `remotes` (streamable-http/sse → an HTTP endpoint) into concrete
//!    [`McpTransport`]s ready to write into `.stella/mcp.toml`, alongside the
//!    [`AuthField`]s (required/secret env vars and headers) a later auth flow
//!    must fill. Credential *values* are never fabricated here and never logged.
//!
//! MCP servers are **not** versioned in stella's config: a registry entry
//! carries a `version`, but installing pins nothing — the transport is what we
//! keep, and re-installing simply overwrites it.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use crate::config::McpTransport;
use crate::error::McpError;

/// The official MCP Registry — the default when `mcp.registry_url` is unset.
/// The API shape is the generic "MCP Server Registry API" standard, so any
/// registry serving the same `GET /v0.1/servers` contract can be swapped in.
pub const DEFAULT_REGISTRY_URL: &str = "https://registry.modelcontextprotocol.io";

/// The frozen (v0.1) list/search endpoint path.
const SERVERS_PATH: &str = "/v0.1/servers";

/// The registry's own default page size (OpenAPI `limit` default). We pass it
/// explicitly so paging is deterministic across registries.
pub const DEFAULT_PAGE_LIMIT: u32 = 30;

/// Bound on each registry HTTP request.
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(30);

// ── Wire types ──────────────────────────────────────────────────────────────

/// The top-level list response: a page of servers plus pagination metadata.
#[derive(Debug, Clone, Deserialize)]
struct ListResponse {
    #[serde(default)]
    servers: Vec<ServerEnvelope>,
    #[serde(default)]
    metadata: Metadata,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Metadata {
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
    #[serde(default)]
    count: Option<u64>,
}

/// Each list element wraps the published `server` document beside registry
/// bookkeeping under `_meta`.
#[derive(Debug, Clone, Deserialize)]
struct ServerEnvelope {
    server: RegistryServer,
    #[serde(default, rename = "_meta")]
    meta: RegistryMeta,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RegistryMeta {
    #[serde(default, rename = "io.modelcontextprotocol.registry/official")]
    official: OfficialMeta,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct OfficialMeta {
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "isLatest")]
    is_latest: Option<bool>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<String>,
}

/// A published `server.json` document: identity, description, and the ways to
/// run it (`packages` for local spawn, `remotes` for hosted endpoints).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegistryServer {
    /// Reverse-DNS-ish unique name, e.g. `com.pulsemcp/remote-filesystem`.
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// The registry entry's version. Informational only — stella pins nothing.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub repository: Option<Repository>,
    /// Installable packages (npm/pypi/oci/nuget) — each maps to a local
    /// spawn (stdio) or, rarely, a package-declared HTTP transport.
    #[serde(default)]
    pub packages: Vec<RegistryPackage>,
    /// Hosted endpoints — streamable-http or sse.
    #[serde(default)]
    pub remotes: Vec<RegistryRemote>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Repository {
    pub url: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub subfolder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegistryPackage {
    #[serde(rename = "registryType", default)]
    pub registry_type: String,
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(rename = "runtimeHint", default)]
    pub runtime_hint: Option<String>,
    #[serde(default)]
    pub transport: Option<PackageTransport>,
    #[serde(rename = "runtimeArguments", default)]
    pub runtime_arguments: Vec<Argument>,
    #[serde(rename = "packageArguments", default)]
    pub package_arguments: Vec<Argument>,
    #[serde(rename = "environmentVariables", default)]
    pub environment_variables: Vec<EnvVar>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PackageTransport {
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Present when a package declares its own HTTP endpoint rather than stdio.
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Argument {
    #[serde(default)]
    pub value: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EnvVar {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "isRequired", default)]
    pub is_required: bool,
    #[serde(rename = "isSecret", default)]
    pub is_secret: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegistryRemote {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: Vec<Header>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Header {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "isRequired", default)]
    pub is_required: bool,
    #[serde(rename = "isSecret", default)]
    pub is_secret: bool,
}

// ── Normalized (UI-facing) page types ────────────────────────────────────────

/// One search result: the published document plus the registry status flags a
/// UI wants to show (active/deprecated, whether this is the latest version).
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryEntry {
    pub server: RegistryServer,
    pub status: Option<String>,
    pub is_latest: Option<bool>,
    pub updated_at: Option<String>,
}

/// A page of results with the opaque cursor for the next page (`None` = last).
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryPage {
    pub entries: Vec<RegistryEntry>,
    pub next_cursor: Option<String>,
    pub count: Option<u64>,
}

// ── Install mapping ──────────────────────────────────────────────────────────

/// Where a credential lives on a transport: an environment variable (stdio) or
/// an HTTP header (remote).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthLocation {
    Env,
    Header,
}

/// A credential the operator must supply for a server to work — surfaced by
/// install so the auth flow knows what to prompt for. Carries only metadata
/// (name/description/flags), never a value.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthField {
    pub location: AuthLocation,
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
    pub secret: bool,
}

/// One concrete way to install a server: a ready-to-write [`McpTransport`], a
/// human label, and the credentials still to be filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct InstallOption {
    pub label: String,
    pub transport: McpTransport,
    pub auth: Vec<AuthField>,
}

impl RegistryServer {
    /// The default local alias (config table key + tool-namespace segment) for
    /// this server: the last path segment, sanitized to be namespaceable
    /// (non-empty, free of the reserved `__` separator).
    pub fn default_alias(&self) -> String {
        sanitize_alias(&self.name)
    }

    /// Every concrete install target this server offers, remotes first (hosted,
    /// nothing to install locally) then packages. Empty means the entry is
    /// unusable as published (neither a runnable package nor a remote).
    pub fn install_options(&self) -> Vec<InstallOption> {
        let mut options = Vec::new();
        for remote in &self.remotes {
            let (transport, auth) = remote_to_option(remote);
            options.push(InstallOption {
                label: format!(
                    "remote · {} · {}",
                    non_empty(&remote.kind, "http"),
                    remote.url
                ),
                transport,
                auth,
            });
        }
        for package in &self.packages {
            if let Some((transport, auth)) = package_to_transport(package) {
                let kind = package
                    .transport
                    .as_ref()
                    .map(|t| t.kind.as_str())
                    .unwrap_or("stdio");
                options.push(InstallOption {
                    label: format!(
                        "{} · {} · {}",
                        non_empty(&package.registry_type, "package"),
                        package.identifier,
                        kind
                    ),
                    transport,
                    auth,
                });
            }
        }
        options
    }
}

/// Sanitize a registry server name into a config alias: last path segment,
/// non-`[A-Za-z0-9._-]` → `-`, and never containing the reserved `__`.
pub fn sanitize_alias(name: &str) -> String {
    let segment = name.rsplit('/').next().unwrap_or(name);
    let mut out: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // The `__` separator is reserved by the tool namespace; collapse any run.
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '.' || c == '_');
    if trimmed.is_empty() {
        "server".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Map a package to a spawnable transport plus the credentials to collect.
/// Returns `None` when the package declares no runnable runtime.
fn package_to_transport(pkg: &RegistryPackage) -> Option<(McpTransport, Vec<AuthField>)> {
    let transport_kind = pkg
        .transport
        .as_ref()
        .map(|t| t.kind.as_str())
        .unwrap_or("stdio");

    // A package that declares its own HTTP endpoint is really a remote.
    if transport_kind == "streamable-http" || transport_kind == "sse" {
        let url = pkg.transport.as_ref().and_then(|t| t.url.clone())?;
        return Some((
            McpTransport::Http {
                url,
                headers: BTreeMap::new(),
            },
            Vec::new(),
        ));
    }

    let (cmd, lead_args) = runtime_invocation(pkg)?;
    let mut args = lead_args;
    for arg in &pkg.runtime_arguments {
        if let Some(value) = arg_value(arg) {
            args.push(value);
        }
    }
    if !pkg.identifier.is_empty() {
        args.push(pkg.identifier.clone());
    }
    for arg in &pkg.package_arguments {
        if let Some(value) = arg_value(arg) {
            args.push(value);
        }
    }

    let mut env = BTreeMap::new();
    let mut auth = Vec::new();
    for var in &pkg.environment_variables {
        // Preset a non-secret default so the server can start unattended.
        if let Some(default) = &var.default
            && !var.is_secret
        {
            env.insert(var.name.clone(), default.clone());
        }
        if var.is_required || var.is_secret {
            auth.push(AuthField {
                location: AuthLocation::Env,
                name: var.name.clone(),
                description: var.description.clone(),
                required: var.is_required,
                secret: var.is_secret,
            });
        }
    }

    Some((McpTransport::Stdio { cmd, args, env }, auth))
}

/// Map a remote endpoint to an HTTP transport plus the header credentials to
/// collect. Secret/required headers are surfaced as auth (never preset with a
/// fabricated value); plain headers with a literal value are carried through.
fn remote_to_option(remote: &RegistryRemote) -> (McpTransport, Vec<AuthField>) {
    let mut headers = BTreeMap::new();
    let mut auth = Vec::new();
    for header in &remote.headers {
        if header.is_secret || header.is_required {
            auth.push(AuthField {
                location: AuthLocation::Header,
                name: header.name.clone(),
                description: header.description.clone(),
                required: header.is_required,
                secret: header.is_secret,
            });
        } else if let Some(value) = &header.value {
            headers.insert(header.name.clone(), value.clone());
        }
    }
    (
        McpTransport::Http {
            url: remote.url.clone(),
            headers,
        },
        auth,
    )
}

/// Resolve the command + leading args for a package's runtime. Prefers the
/// explicit `runtimeHint`, else maps the registry type to the conventional
/// runner. `docker` gets `run --rm -i` unless the entry already provides it.
fn runtime_invocation(pkg: &RegistryPackage) -> Option<(String, Vec<String>)> {
    let cmd = pkg
        .runtime_hint
        .clone()
        .filter(|h| !h.is_empty())
        .or_else(|| match pkg.registry_type.as_str() {
            "npm" => Some("npx".to_string()),
            "pypi" => Some("uvx".to_string()),
            "oci" | "docker" => Some("docker".to_string()),
            "nuget" => Some("dnx".to_string()),
            _ => None,
        })?;
    let lead = if cmd == "docker"
        && !pkg
            .runtime_arguments
            .iter()
            .any(|a| a.value.as_deref() == Some("run"))
    {
        vec!["run".to_string(), "--rm".to_string(), "-i".to_string()]
    } else {
        Vec::new()
    };
    Some((cmd, lead))
}

/// A CLI argument's concrete value: its literal `value`, else its `default`.
/// Named args without a value contribute nothing (we cannot guess the value).
fn arg_value(arg: &Argument) -> Option<String> {
    arg.value.clone().or_else(|| arg.default.clone())
}

fn non_empty<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() { fallback } else { s }
}

// ── Client ───────────────────────────────────────────────────────────────────

/// An async client over one MCP Server Registry.
pub struct RegistryClient {
    client: reqwest::Client,
    base_url: String,
}

impl RegistryClient {
    /// Build a client for `base_url` (trailing slashes trimmed) with the
    /// standard request timeout.
    pub fn new(base_url: impl Into<String>) -> Result<Self, McpError> {
        let client = reqwest::Client::builder()
            .timeout(REGISTRY_TIMEOUT)
            .build()
            .map_err(|e| {
                McpError::Transport(format!("failed to build registry HTTP client: {e}"))
            })?;
        Ok(Self::with_client(client, base_url))
    }

    /// Build over a caller-provided `reqwest::Client` — used by tests to point
    /// at a local mock server.
    pub fn with_client(client: reqwest::Client, base_url: impl Into<String>) -> Self {
        let mut base = base_url.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self {
            client,
            base_url: base,
        }
    }

    /// The registry base URL this client targets.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Search the registry. `query` is a case-insensitive substring over server
    /// names (omit/empty for a plain listing); `cursor` continues a previous
    /// page (`RegistryPage::next_cursor`); `limit` caps the page size.
    pub async fn search(
        &self,
        query: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<RegistryPage, McpError> {
        let url = format!("{}{SERVERS_PATH}", self.base_url);
        let limit = if limit == 0 {
            DEFAULT_PAGE_LIMIT
        } else {
            limit
        };
        let mut params: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if let Some(q) = query.map(str::trim).filter(|q| !q.is_empty()) {
            params.push(("search", q.to_string()));
        }
        if let Some(c) = cursor.filter(|c| !c.is_empty()) {
            params.push(("cursor", c.to_string()));
        }

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .map_err(|e| McpError::Transport(format!("registry request to `{url}` failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(McpError::Transport(format!(
                "registry `{}` returned HTTP {status}: {}",
                self.base_url,
                truncate(&body, 300)
            )));
        }

        let body = response.text().await.map_err(|e| {
            McpError::Transport(format!("reading registry response body failed: {e}"))
        })?;
        Self::parse_page(&body)
    }

    /// Parse a raw list-endpoint JSON body into a normalized page. Split from
    /// [`RegistryClient::search`] so tests exercise recorded responses offline.
    pub fn parse_page(body: &str) -> Result<RegistryPage, McpError> {
        let response: ListResponse = serde_json::from_str(body)
            .map_err(|e| McpError::Protocol(format!("could not decode registry response: {e}")))?;
        let entries = response
            .servers
            .into_iter()
            .map(|envelope| RegistryEntry {
                server: envelope.server,
                status: envelope.meta.official.status,
                is_latest: envelope.meta.official.is_latest,
                updated_at: envelope.meta.official.updated_at,
            })
            .collect();
        Ok(RegistryPage {
            entries,
            next_cursor: response.metadata.next_cursor,
            count: response.metadata.count,
        })
    }
}

/// Char-boundary-safe truncation for diagnostic bodies (mirrors [`crate::http`]).
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

    const LIST: &str = include_str!("../tests/fixtures/registry_list.json");
    const SEARCH_PACKAGES: &str = include_str!("../tests/fixtures/registry_search_packages.json");
    const SEARCH_REMOTE: &str = include_str!("../tests/fixtures/registry_search_remote.json");

    #[test]
    fn parses_a_recorded_list_page_with_pagination() {
        let page = RegistryClient::parse_page(LIST).unwrap();
        assert!(!page.entries.is_empty());
        // The recorded list has a next cursor and a count.
        assert!(page.next_cursor.is_some(), "next cursor present");
        assert_eq!(page.count, Some(page.entries.len() as u64));
        // Status flags are lifted out of `_meta`.
        assert!(
            page.entries
                .iter()
                .any(|e| e.status.as_deref() == Some("active"))
        );
        assert!(page.entries.iter().any(|e| e.is_latest == Some(true)));
    }

    #[test]
    fn last_page_has_no_next_cursor() {
        // A body with an empty metadata object → no cursor (end of results).
        let body = r#"{"servers":[],"metadata":{"count":0}}"#;
        let page = RegistryClient::parse_page(body).unwrap();
        assert!(page.entries.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn tolerates_unknown_fields_and_missing_optionals() {
        let body = r#"{
            "servers":[{"server":{"name":"x/y","future_field":123},"_meta":{}}],
            "metadata":{"nextCursor":"c1"},
            "extra":"ignored"
        }"#;
        let page = RegistryClient::parse_page(body).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].server.name, "x/y");
        assert!(page.entries[0].server.description.is_none());
        assert!(page.entries[0].server.packages.is_empty());
        assert_eq!(page.next_cursor.as_deref(), Some("c1"));
    }

    #[test]
    fn malformed_json_is_a_typed_protocol_error() {
        let err = RegistryClient::parse_page("not json").unwrap_err();
        assert!(matches!(err, McpError::Protocol(_)));
    }

    #[test]
    fn npm_package_maps_to_an_npx_stdio_transport_with_auth() {
        let page = RegistryClient::parse_page(SEARCH_PACKAGES).unwrap();
        let server = &page
            .entries
            .iter()
            .find(|e| !e.server.packages.is_empty())
            .expect("a package entry")
            .server;
        let options = server.install_options();
        let stdio = options
            .iter()
            .find(|o| matches!(o.transport, McpTransport::Stdio { .. }))
            .expect("a stdio option");

        match &stdio.transport {
            McpTransport::Stdio { cmd, args, .. } => {
                assert_eq!(cmd, "npx");
                // runtimeArguments (`-y`) precede the package identifier.
                assert_eq!(args.first().map(String::as_str), Some("-y"));
                assert!(args.iter().any(|a| a == "remote-filesystem-mcp-server"));
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        // The required + secret env vars are surfaced for the auth flow; the
        // secret one is flagged, and no secret value is ever preset.
        assert!(
            stdio
                .auth
                .iter()
                .any(|f| f.name == "GCS_BUCKET" && f.required)
        );
        assert!(
            stdio
                .auth
                .iter()
                .any(|f| f.name == "GCS_PRIVATE_KEY" && f.secret)
        );
    }

    #[test]
    fn remote_with_secret_header_maps_to_http_and_surfaces_auth() {
        let page = RegistryClient::parse_page(SEARCH_REMOTE).unwrap();
        let server = &page
            .entries
            .iter()
            .find(|e| !e.server.remotes.is_empty())
            .expect("a remote entry")
            .server;
        let options = server.install_options();
        let http = options
            .iter()
            .find(|o| matches!(o.transport, McpTransport::Http { .. }))
            .expect("an http option");

        match &http.transport {
            McpTransport::Http { url, headers } => {
                assert!(url.starts_with("https://"));
                // A secret header value is NOT written into the config verbatim.
                assert!(!headers.contains_key("Authorization"));
            }
            other => panic!("expected http, got {other:?}"),
        }
        assert!(
            http.auth
                .iter()
                .any(|f| f.location == AuthLocation::Header && f.secret),
            "secret header surfaced as auth"
        );
    }

    #[test]
    fn sanitize_alias_strips_path_and_reserved_separator() {
        assert_eq!(
            sanitize_alias("com.pulsemcp/remote-filesystem"),
            "remote-filesystem"
        );
        assert_eq!(sanitize_alias("ai.smithery/A__B"), "A_B");
        assert_eq!(sanitize_alias("weird//name!!"), "name");
        // Never empty, never contains `__`.
        assert!(!sanitize_alias("____").contains("__"));
        assert!(!sanitize_alias("").is_empty());
    }
}
