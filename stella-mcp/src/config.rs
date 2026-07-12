//! Configuration for the external MCP servers this client connects to. The
//! CLI decides *where* the file lives (`<workspace>/.stella/mcp.toml` or a
//! global config); this module owns only the *shape* and its parsing. Both a
//! whole-file document ([`McpConfig`]) and a single entry
//! ([`McpServerConfig`]) round-trip through serde + TOML.
//!
//! Security (`02-architecture.md` §8): a `stdio` server inherits **no**
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
    /// Server name -> its transport definition. A `BTreeMap` so iteration
    /// order is stable (deterministic tool namespacing across runs).
    #[serde(default)]
    pub servers: BTreeMap<String, McpTransport>,
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
            .map(|(name, transport)| McpServerConfig { name, transport })
            .collect()
    }
}

/// One named server: its `name` (used as the tool-namespace segment) and its
/// transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(flatten)]
    pub transport: McpTransport,
}

/// How to reach a server. `transport` is the discriminant; the remaining
/// fields depend on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        /// stripped (`02-architecture.md` §8).
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
        };
        let s = toml::to_string(&entry).unwrap();
        let back: McpServerConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.name, "solo");
        assert!(matches!(back.transport, McpTransport::Http { .. }));
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
}
