//! Configuration for the external MCP servers this client connects to. The
//! CLI decides *where* the file lives (`<workspace>/.stella/mcp.toml` or a
//! global config); this module owns only the *shape* and its parsing. Both a
//! whole-file document ([`McpConfig`]) and a single entry
//! ([`McpServerConfig`]) round-trip through serde + TOML.
//!
//! Security: a `stdio` server inherits **no**
//! ambient environment. Only the keys listed in its `env` table reach the
//! child — nothing else, so an `ANTHROPIC_API_KEY` in the parent shell can
//! never leak into an MCP subprocess.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::McpError;

/// A whole `mcp.toml` document: a table of named servers.
///
/// ```toml
/// [servers.filesystem]
/// transport = "stdio"
/// cmd = "mcp-server-filesystem"
/// args = ["--root", "/workspace"]
/// env = { LOG_LEVEL = "info" }
///
/// [servers.github]
/// transport = "http"
/// url = "https://mcp.example.com/mcp"
/// headers = { Authorization = "Bearer …" }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    /// Server name -> its entry (transport + policy). A `BTreeMap` so
    /// iteration order is stable (deterministic tool namespacing across runs).
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerEntry>,
}

impl McpConfig {
    /// Parse a whole `mcp.toml` document.
    pub fn from_toml_str(s: &str) -> Result<Self, McpError> {
        toml::from_str(s).map_err(|e| McpError::Config(e.to_string()))
    }

    /// Flatten the document into the [`McpServerConfig`] list the rest of the
    /// crate consumes, carrying each server's name inline.
    pub fn into_servers(self) -> Vec<McpServerConfig> {
        self.servers
            .into_iter()
            .map(|(name, entry)| McpServerConfig {
                name,
                transport: entry.transport,
                candidate_safe: entry.candidate_safe,
            })
            .collect()
    }

    /// The configured server names, in stable (sorted) order.
    pub fn names(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }

    /// Look up a configured server's transport.
    pub fn get(&self, name: &str) -> Option<&McpTransport> {
        self.servers.get(name).map(|e| &e.transport)
    }

    /// Look up a configured server's transport for editing (e.g. an auth flow
    /// setting a credential in place).
    pub fn get_mut(&mut self, name: &str) -> Option<&mut McpTransport> {
        self.servers.get_mut(name).map(|e| &mut e.transport)
    }

    /// Insert or replace a server entry. Installing a registry server is an
    /// upsert: MCP servers are not versioned, so re-installing simply
    /// overwrites the transport under the same alias — an existing entry's
    /// `candidate_safe` flag is preserved across the reinstall (it is a
    /// human-set policy, not part of the transport the registry publishes).
    pub fn upsert(&mut self, name: impl Into<String>, transport: McpTransport) {
        let name = name.into();
        match self.servers.get_mut(&name) {
            Some(entry) => entry.transport = transport,
            None => {
                self.servers.insert(
                    name,
                    McpServerEntry {
                        transport,
                        candidate_safe: false,
                    },
                );
            }
        }
    }

    /// Remove a server entry, returning whether it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        self.servers.remove(name).is_some()
    }

    /// Whether `name` is configured and opted into the Best-of-N candidate
    /// allowlist (`.stella/mcp.toml`'s `candidate_safe = true`, issue #248
    /// Phase 1). `false` for an unconfigured name — never a silent default of
    /// "safe".
    pub fn is_candidate_safe(&self, name: &str) -> bool {
        self.servers.get(name).is_some_and(|e| e.candidate_safe)
    }

    /// Set (or clear) `name`'s `candidate_safe` opt-in, returning whether the
    /// server exists to flag. This is an explicit, human-reviewed allowlist —
    /// read-only, cwd-independent servers only (see `stella-cli/src/
    /// candidate_ws.rs`'s module doc); never inferred from a server's
    /// self-reported `read_only_hint`.
    pub fn set_candidate_safe(&mut self, name: &str, candidate_safe: bool) -> bool {
        match self.servers.get_mut(name) {
            Some(entry) => {
                entry.candidate_safe = candidate_safe;
                true
            }
            None => false,
        }
    }

    /// Serialize the whole document back to TOML (for writing `mcp.toml`).
    /// Note this writes credential values (env/headers) verbatim to disk, the
    /// pre-existing `mcp.toml` convention; the redacted [`McpTransport`] `Debug`
    /// keeps those same values out of logs.
    pub fn to_toml_string(&self) -> Result<String, McpError> {
        toml::to_string_pretty(self).map_err(|e| McpError::Config(e.to_string()))
    }
}

/// One server's on-disk entry: its transport plus Best-of-N candidate policy
/// (issue #248 Phase 1), which lives OUTSIDE the internally-tagged
/// [`McpTransport`] enum since it applies identically regardless of
/// transport kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerEntry {
    #[serde(flatten)]
    pub transport: McpTransport,
    /// Explicit allowlist opt-in, default `false`. A `read_only_hint` on the
    /// server's own tool advertisements is UNTRUSTED and can't distinguish
    /// "reads an external system" (safe to share across candidates) from
    /// "reads the local tree" (would read stale/wrong bytes from a
    /// candidate's isolated snapshot) — so this is a human-reviewed opt-in,
    /// never inferred.
    #[serde(default)]
    pub candidate_safe: bool,
}

/// One named server: its `name` (used as the tool-namespace segment), its
/// transport, and its Best-of-N candidate-allowlist flag (issue #248 Phase 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(flatten)]
    pub transport: McpTransport,
    #[serde(default)]
    pub candidate_safe: bool,
}

/// How to reach a server. `transport` is the discriminant; the remaining
/// fields depend on it.
///
/// `Debug` is **hand-written to redact credential values** (env values and
/// header values) — a plain derive would print an `Authorization` bearer or an
/// API-key env var verbatim under `{:?}`, leaking it into any log or panic
/// message. The keys are kept (they say *which* credentials are configured);
/// only the values become `<redacted>`. Serialization is unaffected, so the
/// on-disk `mcp.toml` still round-trips.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransport {
    /// Spawn a child process and speak newline-delimited JSON-RPC over its
    /// stdio. The child's environment is *scrubbed* except for `env`.
    Stdio {
        cmd: String,
        #[serde(default)]
        args: Vec<String>,
        /// The only environment variables passed to the child. Everything
        /// else — including every credential in the parent shell — is
        /// stripped.
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// POST JSON-RPC to a streamable-HTTP endpoint.
    Http {
        url: String,
        /// Static headers replayed on every request (e.g. an `Authorization`
        /// bearer the operator chose to configure here).
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

impl McpTransport {
    /// The transport discriminant, for display.
    pub fn kind_label(&self) -> &'static str {
        match self {
            McpTransport::Stdio { .. } => "stdio",
            McpTransport::Http { .. } => "http",
        }
    }

    /// The names of the credential-bearing fields configured on this transport:
    /// env-var names for stdio, header names for http. Values are never
    /// returned — this is for a "which credentials are set" UI, not for reading
    /// secrets.
    pub fn credential_names(&self) -> Vec<&str> {
        match self {
            McpTransport::Stdio { env, .. } => env.keys().map(String::as_str).collect(),
            McpTransport::Http { headers, .. } => headers.keys().map(String::as_str).collect(),
        }
    }

    /// Whether any credential field is set (auth appears configured).
    pub fn has_credentials(&self) -> bool {
        match self {
            McpTransport::Stdio { env, .. } => !env.is_empty(),
            McpTransport::Http { headers, .. } => !headers.is_empty(),
        }
    }

    /// Set a credential value in place: an env var for stdio, a header for
    /// http. Used by the auth flow. The value is stored (and later written to
    /// `mcp.toml`) but never logged — see the redacted `Debug`.
    pub fn set_credential(&mut self, field: impl Into<String>, value: String) {
        match self {
            McpTransport::Stdio { env, .. } => {
                env.insert(field.into(), value);
            }
            McpTransport::Http { headers, .. } => {
                headers.insert(field.into(), value);
            }
        }
    }
}

impl std::fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpTransport::Stdio { cmd, args, env } => f
                .debug_struct("Stdio")
                .field("cmd", cmd)
                .field("args", args)
                .field("env", &RedactedValues(env))
                .finish(),
            McpTransport::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", url)
                .field("headers", &RedactedValues(headers))
                .finish(),
        }
    }
}

/// A `Debug` adapter for a string map that prints keys but replaces every value
/// with `<redacted>` — so credential values never reach a log or panic message.
struct RedactedValues<'a>(&'a BTreeMap<String, String>);

impl std::fmt::Debug for RedactedValues<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|k| (k, "<redacted>")))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_document() {
        let cfg = McpConfig::from_toml_str(
            r#"
            [servers.fs]
            transport = "stdio"
            cmd = "mcp-fs"
            args = ["--root", "/w"]
            env = { LOG = "info" }

            [servers.remote]
            transport = "http"
            url = "https://example.com/mcp"
            headers = { Authorization = "Bearer x" }
            "#,
        )
        .unwrap();

        let servers = cfg.into_servers();
        assert_eq!(servers.len(), 2);
        // BTreeMap ordering: "fs" before "remote".
        assert_eq!(servers[0].name, "fs");
        match &servers[0].transport {
            McpTransport::Stdio { cmd, args, env } => {
                assert_eq!(cmd, "mcp-fs");
                assert_eq!(args, &["--root", "/w"]);
                assert_eq!(env.get("LOG").map(String::as_str), Some("info"));
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        match &servers[1].transport {
            McpTransport::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer x")
                );
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn stdio_env_and_args_default_empty() {
        let cfg =
            McpConfig::from_toml_str("[servers.s]\ntransport = \"stdio\"\ncmd = \"x\"\n").unwrap();
        let servers = cfg.into_servers();
        match &servers[0].transport {
            McpTransport::Stdio { args, env, .. } => {
                assert!(args.is_empty());
                assert!(env.is_empty());
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn a_single_entry_round_trips() {
        let entry = McpServerConfig {
            name: "solo".into(),
            transport: McpTransport::Http {
                url: "https://h/mcp".into(),
                headers: BTreeMap::new(),
            },
            candidate_safe: true,
        };
        let s = toml::to_string(&entry).unwrap();
        let back: McpServerConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.name, "solo");
        assert!(matches!(back.transport, McpTransport::Http { .. }));
        assert!(back.candidate_safe);
    }

    #[test]
    fn bad_toml_is_a_typed_config_error() {
        let err = McpConfig::from_toml_str("this is not = = toml").unwrap_err();
        assert!(matches!(err, McpError::Config(_)));
    }

    #[test]
    fn empty_document_yields_no_servers() {
        let cfg = McpConfig::from_toml_str("").unwrap();
        assert!(cfg.into_servers().is_empty());
    }

    #[test]
    fn debug_redacts_credential_values_but_keeps_keys() {
        // stdio env value (an API key) must never appear under `{:?}`.
        let mut env = BTreeMap::new();
        env.insert("API_KEY".to_string(), "super-secret-token".to_string());
        let stdio = McpTransport::Stdio {
            cmd: "srv".into(),
            args: vec!["--flag".into()],
            env,
        };
        let shown = format!("{stdio:?}");
        assert!(
            !shown.contains("super-secret-token"),
            "value leaked: {shown}"
        );
        assert!(shown.contains("API_KEY"), "key should be visible: {shown}");
        assert!(shown.contains("<redacted>"));
        // Non-secret command line stays visible for debugging.
        assert!(shown.contains("srv") && shown.contains("--flag"));

        // http header value (a bearer) must never appear either.
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer leak-me".to_string());
        let http = McpTransport::Http {
            url: "https://h/mcp".into(),
            headers,
        };
        let shown = format!("{http:?}");
        assert!(!shown.contains("leak-me"), "bearer leaked: {shown}");
        assert!(shown.contains("Authorization"));
        // The whole server config (derived Debug) also stays redacted.
        let cfg = McpServerConfig {
            name: "s".into(),
            transport: http,
            candidate_safe: false,
        };
        assert!(!format!("{cfg:?}").contains("leak-me"));
    }

    #[test]
    fn upsert_remove_and_toml_roundtrip() {
        let mut cfg = McpConfig::default();
        cfg.upsert(
            "fs",
            McpTransport::Stdio {
                cmd: "mcp-fs".into(),
                args: vec!["--root".into(), "/w".into()],
                env: BTreeMap::new(),
            },
        );
        // Re-installing overwrites (MCP servers are not versioned).
        cfg.upsert(
            "fs",
            McpTransport::Stdio {
                cmd: "mcp-fs".into(),
                args: vec!["--root".into(), "/other".into()],
                env: BTreeMap::new(),
            },
        );
        assert_eq!(cfg.names(), vec!["fs"]);

        // Serialize → parse → identical document.
        let toml_text = cfg.to_toml_string().unwrap();
        let back = McpConfig::from_toml_str(&toml_text).unwrap();
        assert_eq!(back.get("fs"), cfg.get("fs"));

        assert!(cfg.remove("fs"));
        assert!(!cfg.remove("fs"));
        assert!(cfg.names().is_empty());
    }

    #[test]
    fn set_credential_targets_env_or_headers() {
        let mut stdio = McpTransport::Stdio {
            cmd: "s".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        stdio.set_credential("TOKEN", "v".into());
        assert!(stdio.has_credentials());
        assert_eq!(stdio.credential_names(), vec!["TOKEN"]);

        let mut http = McpTransport::Http {
            url: "https://h".into(),
            headers: BTreeMap::new(),
        };
        http.set_credential("Authorization", "Bearer v".into());
        assert_eq!(http.credential_names(), vec!["Authorization"]);
    }

    // ---- candidate_safe (issue #248 Phase 1) -----------------------------

    #[test]
    fn candidate_safe_parses_true_and_defaults_false_when_absent() {
        let cfg = McpConfig::from_toml_str(
            r#"
            [servers.docs]
            transport = "http"
            url = "https://docs.example.com/mcp"
            candidate_safe = true

            [servers.fs]
            transport = "stdio"
            cmd = "mcp-fs"
            "#,
        )
        .unwrap();
        let servers = cfg.into_servers();
        assert_eq!(servers.len(), 2);
        assert!(servers[0].candidate_safe, "docs opted in explicitly");
        assert!(
            !servers[1].candidate_safe,
            "fs must default to false, never inferred safe"
        );
    }

    #[test]
    fn is_candidate_safe_reads_the_configured_flag_and_false_for_unknown() {
        let cfg = McpConfig::from_toml_str(
            "[servers.docs]\ntransport = \"stdio\"\ncmd = \"x\"\ncandidate_safe = true\n",
        )
        .unwrap();
        assert!(cfg.is_candidate_safe("docs"));
        assert!(!cfg.is_candidate_safe("nonexistent"));
    }

    #[test]
    fn set_candidate_safe_toggles_an_existing_server_and_reports_unknown_names() {
        let mut cfg = McpConfig::default();
        cfg.upsert(
            "fs",
            McpTransport::Stdio {
                cmd: "mcp-fs".into(),
                args: vec![],
                env: BTreeMap::new(),
            },
        );
        assert!(!cfg.is_candidate_safe("fs"));
        assert!(cfg.set_candidate_safe("fs", true));
        assert!(cfg.is_candidate_safe("fs"));
        assert!(!cfg.set_candidate_safe("missing", true));
    }

    #[test]
    fn upsert_reinstall_preserves_the_candidate_safe_flag() {
        // Re-installing a registry server (a version bump, a re-run of
        // `stella mcp install`) must not silently revoke a human's allowlist
        // opt-in — `candidate_safe` is policy, not part of the published
        // transport.
        let mut cfg = McpConfig::default();
        cfg.upsert(
            "docs",
            McpTransport::Http {
                url: "https://docs.example.com/mcp".into(),
                headers: BTreeMap::new(),
            },
        );
        assert!(cfg.set_candidate_safe("docs", true));
        cfg.upsert(
            "docs",
            McpTransport::Http {
                url: "https://docs.example.com/v2/mcp".into(),
                headers: BTreeMap::new(),
            },
        );
        assert!(cfg.is_candidate_safe("docs"), "flag survives reinstall");
        match cfg.get("docs").unwrap() {
            McpTransport::Http { url, .. } => assert_eq!(url, "https://docs.example.com/v2/mcp"),
            other => panic!("expected http, got {other:?}"),
        }
    }
}
