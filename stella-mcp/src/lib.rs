//! `stella-mcp` — an **MCP client**. It connects to external Model Context
//! Protocol servers (stdio child processes and streamable-HTTP endpoints),
//! discovers their tools, and merges them into the engine's tool registry so
//! `stella-core::Engine` can call them exactly like a built-in tool
//!
//!
//! The single integration point is [`McpToolSet`], which implements
//! `stella_core::ports::ToolExecutor` — the same port `stella-tools`'
//! `ToolRegistry` implements. Compose it over the native tool set and the
//! engine can't tell MCP tools from local ones:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use std::time::Duration;
//! # use stella_mcp::{McpConfig, McpError, McpToolSet};
//! # async fn wire(native: Arc<dyn stella_core::ports::ToolExecutor>) -> Result<(), McpError> {
//! // Parsed from e.g. `<workspace>/.stella/mcp.toml`.
//! let toml = r#"
//!     [servers.filesystem]
//!     transport = "stdio"
//!     cmd = "mcp-server-filesystem"
//!     args = ["--root", "/workspace"]
//! "#;
//! let servers = McpConfig::from_toml_str(toml)?.into_servers();
//! let tools = McpToolSet::connect(&servers, Duration::from_secs(60))
//!     .await
//!     .wrapping(native); // MCP tools + native tools behind one ToolExecutor
//! # let _ = tools;
//! # Ok(())
//! # }
//! ```
//!
//! # Design notes
//!
//! - **Protocol revision `2025-06-18`**, negotiated down to any revision in
//!   [`protocol::SUPPORTED_PROTOCOL_VERSIONS`] the server counter-offers
//!   ([`client`]).
//! - **Tolerant by construction.** As a client of a public protocol, every
//!   inbound type ignores unknown fields and defaults missing ones — a server
//!   on a newer minor revision never breaks us ([`protocol`]).
//! - **Security.** stdio servers are spawned with a
//!   *scrubbed* environment: no ambient credential is ever inherited by a
//!   child ([`stdio`]).
//! - **Resilience.** Per-call timeouts; a dead/hung server yields a
//!   server-named `ToolOutput::Error` and never poisons its siblings or the
//!   native tools. A dropped connection **auto-reconnects with bounded
//!   backoff** — a single blip self-heals within the turn, and a long-dead
//!   server degrades gracefully instead of aborting the agent ([`toolset`],
//!   [`client`]). Per-server status is exposed via [`McpToolSet::health`].

pub mod client;
pub mod config;
pub mod error;
pub mod http;
pub mod protocol;
pub mod registry;
mod sse;
pub mod stdio;
pub mod toolset;
pub mod transport;

pub use client::{HealthState, McpClient, McpToolInfo, ServerHealth, render_content};
pub use config::{McpConfig, McpServerConfig, McpTransport};
pub use error::McpError;
pub use http::HttpTransport;
pub use registry::{
    AuthField, AuthLocation, DEFAULT_REGISTRY_URL, InstallOption, RegistryClient, RegistryEntry,
    RegistryPage, RegistryServer,
};
pub use stdio::StdioTransport;
pub use toolset::{DEFAULT_CALL_TIMEOUT, DisabledServers, McpToolSet};
pub use transport::Transport;
