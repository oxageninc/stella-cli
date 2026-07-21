//! The discovery tool layer — `tool_search`, `skill_search`, `mcp_search`.
//!
//! The product problem this solves: a workspace will end up with hundreds of
//! tools (native + MCP servers + custom manifests) and potentially thousands
//! of installed skills. Frontloading all of that into every agent bloats the
//! prompt and buries the relevant few, so the agent gets three read-only
//! *search* tools instead, all ranked by the shared relevance engine in
//! [`stella_core::discovery`]:
//!
//! - **`tool_search`** — search everything the session's tool stack
//!   advertises (native registry, `mcp__server__tool` MCP tools, custom
//!   script tools, session tools) by keyword, `+required` term, or
//!   `select:exact,names`.
//! - **`skill_search`** — search user-global skills plus authority-permitted
//!   workspace skills, optionally returning
//!   the best match's full instructions. Distinct from the existing
//!   `search_skills`, which queries the public internet registry for skills
//!   to install.
//! - **`mcp_search`** — find MCP servers: the workspace's configured servers
//!   (`.stella/mcp.toml`) and their tools, and/or the public MCP registry
//!   for servers worth installing (`stella mcp add <name>`).
//!
//! # Lean frontloading (opt-in)
//!
//! With `STELLA_LEAN_TOOLS=1` (or a comma-separated custom core list) the
//! session advertises only a small core toolset plus the three discovery
//! tools; everything else stays hidden until a `tool_search` surfaces it —
//! matched tools are *activated* and advertised from the next model call.
//! Execution is deliberately permissive: a hidden tool called by name still
//! runs (the model may know it from history), so lean mode can never
//! deadlock a turn. Activation state lives in a shared [`ActivatedTools`]
//! handle so callers with a session loop (REPL, deck) keep it across turns
//! even though the tool stack is rebuilt per turn.
//!
//! Architecture: [`DiscoveryToolSet`] is a `ToolExecutor` decorator layered
//! OUTERMOST (wrapping `InteractiveToolSet`), because only the outermost
//! layer sees the complete advertised catalog. The MCP registry lookup goes
//! through the [`McpRegistrySearch`] port so tests never touch the network.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::Value;
use stella_core::discovery::{Candidate, DiscoveryQuery, parse_query, rank};
use stella_core::ports::ToolExecutor;
use stella_protocol::{ToolOutput, ToolSchema};

pub const TOOL_SEARCH: &str = "tool_search";
pub const SKILL_SEARCH: &str = "skill_search";
pub const MCP_SEARCH: &str = "mcp_search";

/// The env switch for lean frontloading: `1`/`true` for the default core
/// set, a comma-separated list for a custom core, unset/`0`/`false` for the
/// full catalog (today's behavior).
pub const LEAN_TOOLS_ENV: &str = "STELLA_LEAN_TOOLS";

/// The default lean core: the tools an agent needs to make *any* progress
/// (file CRUD, exec, search, the definition of done, and asking the user).
/// Everything else — issue tools, media, tasks, MCP, custom — is one
/// `tool_search` away.
const DEFAULT_CORE_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "bash",
    "grep",
    "glob",
    "gather_context",
    "verify_done",
    "build_project",
    "run_tests",
    "ask_user",
];

/// Result-list caps: enough hits to choose from, few enough to stay lean.
const TOOL_SEARCH_DEFAULT_LIMIT: usize = 10;
const TOOL_SEARCH_MAX_LIMIT: usize = 25;
const SKILL_SEARCH_DEFAULT_LIMIT: usize = 8;
const SKILL_SEARCH_MAX_LIMIT: usize = 20;
const MCP_SEARCH_DEFAULT_LIMIT: usize = 8;
const MCP_SEARCH_MAX_LIMIT: usize = 20;

/// One-line description clip in result listings.
const DESCRIPTION_CLIP_CHARS: usize = 220;
/// Cap on a skill body returned by `skill_search` with `include_body`.
const SKILL_BODY_CLIP_CHARS: usize = 6_000;
/// Registry page size fetched before client-side re-ranking trims to the
/// caller's limit.
const REGISTRY_FETCH_LIMIT: u32 = 30;

/// Session-scoped activation state for lean mode, shared across the per-turn
/// tool-stack rebuilds (same discipline as `stella_mcp::DisabledServers`).
pub type ActivatedTools = Arc<RwLock<HashSet<String>>>;

/// A fresh, empty activation handle.
pub fn new_activation() -> ActivatedTools {
    Arc::default()
}

/// Lean-mode configuration: the always-advertised core tool names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeanConfig {
    pub core: HashSet<String>,
}

impl LeanConfig {
    /// The default core set (see [`DEFAULT_CORE_TOOLS`]).
    pub fn default_core() -> Self {
        Self {
            core: DEFAULT_CORE_TOOLS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Parse [`LEAN_TOOLS_ENV`] into a lean config (`None` = full catalog).
pub(crate) fn lean_from_env() -> Option<LeanConfig> {
    let raw = std::env::var(LEAN_TOOLS_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "0" || trimmed.eq_ignore_ascii_case("false") {
        return None;
    }
    if trimmed == "1" || trimmed.eq_ignore_ascii_case("true") {
        return Some(LeanConfig::default_core());
    }
    Some(LeanConfig {
        core: trimmed
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    })
}

/// The MCP-registry lookup port — injectable so unit tests never hit the
/// network. The production implementation resolves the registry URL from
/// settings.json exactly like `stella mcp search` does.
#[async_trait]
pub trait McpRegistrySearch: Send + Sync {
    async fn search(&self, query: &str, limit: u32) -> Result<stella_mcp::RegistryPage, String>;
}

/// Production registry lookup over HTTP (settings-resolved URL).
pub struct HttpMcpRegistrySearch {
    workspace_root: PathBuf,
}

#[async_trait]
impl McpRegistrySearch for HttpMcpRegistrySearch {
    async fn search(&self, query: &str, limit: u32) -> Result<stella_mcp::RegistryPage, String> {
        let url = crate::mcp_cmd::resolve_registry_url(&self.workspace_root);
        crate::mcp_cmd::search(&url, Some(query), None, limit).await
    }
}

/// A `ToolExecutor` decorator adding the three discovery tools (and, when
/// lean mode is on, the advertise-only-what's-needed filter) on top of an
/// inner executor. See the module docs.
pub struct DiscoveryToolSet<'a> {
    inner: &'a dyn ToolExecutor,
    workspace_root: PathBuf,
    registry: Box<dyn McpRegistrySearch>,
    lean: Option<LeanConfig>,
    activated: ActivatedTools,
    include_workspace_skills: bool,
}

impl<'a> DiscoveryToolSet<'a> {
    /// Wrap `inner` with the discovery tools. Lean mode comes from
    /// [`LEAN_TOOLS_ENV`]; activation state is fresh — callers with a session
    /// loop should pass a persistent handle via [`Self::with_activation`].
    pub fn new(inner: &'a dyn ToolExecutor, workspace_root: PathBuf) -> Self {
        Self {
            inner,
            registry: Box::new(HttpMcpRegistrySearch {
                workspace_root: workspace_root.clone(),
            }),
            workspace_root,
            lean: lean_from_env(),
            activated: new_activation(),
            include_workspace_skills: false,
        }
    }

    /// Add workspace skill definitions only when the effective prompt-source
    /// authority permits them; the default remains user-global only.
    pub fn with_project_prompts_allowed(mut self, allowed: bool) -> Self {
        self.include_workspace_skills = allowed;
        self
    }

    /// Share a session-scoped activation handle so lean-mode activations
    /// survive the per-turn tool-stack rebuild.
    pub fn with_activation(mut self, activated: ActivatedTools) -> Self {
        self.activated = activated;
        self
    }

    /// Override the registry port (tests) .
    #[cfg(test)]
    pub fn with_registry_search(mut self, registry: Box<dyn McpRegistrySearch>) -> Self {
        self.registry = registry;
        self
    }

    /// Override lean mode explicitly (tests / future settings wiring).
    #[cfg(test)]
    pub fn with_lean(mut self, lean: Option<LeanConfig>) -> Self {
        self.lean = lean;
        self
    }

    // ── Schemas ─────────────────────────────────────────────────────────────

    fn discovery_schemas(&self) -> Vec<ToolSchema> {
        let lean_note = if self.lean.is_some() {
            " This session advertises a lean core toolset: tools this search returns are \
             activated and become callable — search BEFORE assuming a capability is missing."
        } else {
            ""
        };
        vec![
            ToolSchema {
                name: TOOL_SEARCH.into(),
                description: format!(
                    "Search every tool available in this session — built-ins, MCP server tools \
                     (mcp__<server>__<tool>), custom workspace tools — ranked by fit. Query \
                     forms: keywords ('issue tracking'), +required terms ('+github pr'), or \
                     select:name1,name2 for exact lookup. Use it when you need a capability you \
                     don't see advertised, before concluding it doesn't exist.{lean_note}"
                ),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keywords, '+required terms', or 'select:name1,name2'."
                        },
                        "limit": { "type": "integer", "minimum": 1, "maximum": TOOL_SEARCH_MAX_LIMIT }
                    },
                    "required": ["query"]
                }),
                read_only: true,
            },
            ToolSchema {
                name: SKILL_SEARCH.into(),
                description: "Search user-global skills plus authority-permitted workspace \
                              skills and rank them by fit for a \
                              task. Set include_body: true to also get the top match's full \
                              instructions when you intend to apply it. Use this FIRST; \
                              search_skills queries the public internet registry for skills you \
                              would still have to install."
                    .into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": SKILL_SEARCH_MAX_LIMIT },
                        "include_body": {
                            "type": "boolean",
                            "description": "Also return the best match's full SKILL.md instructions."
                        }
                    },
                    "required": ["query"]
                }),
                read_only: true,
            },
            ToolSchema {
                name: MCP_SEARCH.into(),
                description: "Find MCP servers and their tools, ranked by fit. scope \
                              'workspace' (default): servers configured in .stella/mcp.toml and \
                              their connected mcp__<server>__<tool> tools. scope 'registry': the \
                              public MCP server registry — servers you could install with \
                              `stella mcp add <name>`. scope 'all': both."
                    .into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "scope": { "type": "string", "enum": ["workspace", "registry", "all"] },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MCP_SEARCH_MAX_LIMIT }
                    },
                    "required": ["query"]
                }),
                read_only: true,
            },
        ]
    }

    // ── tool_search ─────────────────────────────────────────────────────────

    async fn execute_tool_search(&self, input: &Value) -> ToolOutput {
        let Some(query) = input.get("query").and_then(Value::as_str).map(str::trim) else {
            return ToolOutput::Error {
                message: "tool_search: missing required string field `query`".into(),
            };
        };
        if query.is_empty() {
            return ToolOutput::Error {
                message: "tool_search: `query` is empty — pass keywords, +required terms, or \
                          select:name1,name2"
                    .into(),
            };
        }
        let limit = clamp_limit(input, TOOL_SEARCH_DEFAULT_LIMIT, TOOL_SEARCH_MAX_LIMIT);

        // The FULL catalog — self.inner, not self.schemas(), so lean mode's
        // advertised filter never hides anything from the search itself.
        let schemas = self.inner.schemas();
        let candidates: Vec<Candidate> = schemas
            .iter()
            .map(|s| Candidate {
                id: s.name.clone(),
                name: s.name.clone(),
                description: s.description.clone(),
                keywords: match mcp_parts(&s.name) {
                    Some((server, _)) => vec!["mcp".into(), server.to_string()],
                    None => Vec::new(),
                },
            })
            .collect();
        let hits = rank(query, &candidates, limit);

        if hits.is_empty() {
            return ToolOutput::Ok {
                content: format!(
                    "no tools matched `{query}` (searched {} tools) — try broader or different \
                     keywords, or mcp_search for servers that are not connected",
                    candidates.len()
                ),
            };
        }

        // Lean mode: what the model found, the model can call — activate the
        // hits so the next model call advertises their schemas.
        if self.lean.is_some() {
            let mut activated = self.activated.write().unwrap_or_else(|p| p.into_inner());
            for hit in &hits {
                activated.insert(schemas[hit.index].name.clone());
            }
        }

        let mut out = format!(
            "{} of {} session tools matched `{query}`:\n",
            hits.len(),
            candidates.len()
        );
        for (i, hit) in hits.iter().enumerate() {
            let schema = &schemas[hit.index];
            let source = match mcp_parts(&schema.name) {
                Some((server, _)) => format!(" (MCP: {server})"),
                None => String::new(),
            };
            let ro = if schema.read_only { " [read-only]" } else { "" };
            out.push_str(&format!(
                "{}. {}{source}{ro} — {}\n",
                i + 1,
                schema.name,
                clip_line(&schema.description, DESCRIPTION_CLIP_CHARS)
            ));
        }
        if let DiscoveryQuery::Select(ids) = parse_query(query) {
            let missing: Vec<&str> = ids
                .iter()
                .filter(|id| !schemas.iter().any(|s| s.name.eq_ignore_ascii_case(id)))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                out.push_str(&format!("not found: {}\n", missing.join(", ")));
            }
        }
        if self.lean.is_some() {
            out.push_str(
                "These matches are now activated and callable. Tools not listed stay hidden — \
                 search again to surface others.\n",
            );
        } else {
            out.push_str("All of these are already callable by name.\n");
        }
        ToolOutput::Ok { content: out }
    }

    // ── skill_search ────────────────────────────────────────────────────────

    async fn execute_skill_search(&self, input: &Value) -> ToolOutput {
        let Some(query) = input.get("query").and_then(Value::as_str).map(str::trim) else {
            return ToolOutput::Error {
                message: "skill_search: missing required string field `query`".into(),
            };
        };
        if query.is_empty() {
            return ToolOutput::Error {
                message: "skill_search: `query` is empty".into(),
            };
        }
        let limit = clamp_limit(input, SKILL_SEARCH_DEFAULT_LIMIT, SKILL_SEARCH_MAX_LIMIT);
        let include_body = input
            .get("include_body")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Fresh load (a handful of file reads) so a skill installed or
        // auto-created seconds ago is already findable.
        let root = self.workspace_root.clone();
        let include_workspace = self.include_workspace_skills;
        let skills = tokio::task::spawn_blocking(move || {
            crate::memory::load_workspace_skills_with_authority(&root, include_workspace).skills
        })
        .await
        .unwrap_or_default();
        let candidates: Vec<Candidate> = skills
            .iter()
            .map(|s| Candidate {
                id: s.name.clone(),
                name: s.name.clone(),
                description: s.description.clone(),
                keywords: s.domains.clone(),
            })
            .collect();
        let hits = rank(query, &candidates, limit);

        if hits.is_empty() {
            return ToolOutput::Ok {
                content: format!(
                    "no installed skills matched `{query}` (searched {}) — search_skills queries \
                     the public registry if a new skill is worth installing",
                    candidates.len()
                ),
            };
        }

        let mut out = format!(
            "{} of {} installed skills matched `{query}`:\n",
            hits.len(),
            candidates.len()
        );
        for (i, hit) in hits.iter().enumerate() {
            let skill = &skills[hit.index];
            let domains = if skill.domains.is_empty() {
                String::new()
            } else {
                format!(" [{}]", skill.domains.join(", "))
            };
            out.push_str(&format!(
                "{}. {}{domains} — {} ({} · {})\n",
                i + 1,
                skill.name,
                clip_line(&skill.description, DESCRIPTION_CLIP_CHARS),
                origin_label(skill.origin),
                skill.source_path
            ));
        }
        if include_body {
            let top = &skills[hits[0].index];
            out.push_str(&format!(
                "\n── {} (best match — full instructions) ──\n{}\n",
                top.name,
                clip_chars(&top.body, SKILL_BODY_CLIP_CHARS)
            ));
            if hits.len() > 1 {
                out.push_str("(read_file a listed source path for another skill's body)\n");
            }
        } else {
            out.push_str(
                "Pass include_body: true (or read_file a source path) for a skill's full \
                 instructions.\n",
            );
        }
        ToolOutput::Ok { content: out }
    }

    // ── mcp_search ──────────────────────────────────────────────────────────

    async fn execute_mcp_search(&self, input: &Value) -> ToolOutput {
        let Some(query) = input.get("query").and_then(Value::as_str).map(str::trim) else {
            return ToolOutput::Error {
                message: "mcp_search: missing required string field `query`".into(),
            };
        };
        if query.is_empty() {
            return ToolOutput::Error {
                message: "mcp_search: `query` is empty".into(),
            };
        }
        let scope = input
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("workspace");
        if !matches!(scope, "workspace" | "registry" | "all") {
            return ToolOutput::Error {
                message: format!(
                    "mcp_search: unknown scope `{scope}` — use workspace, registry, or all"
                ),
            };
        }
        let limit = clamp_limit(input, MCP_SEARCH_DEFAULT_LIMIT, MCP_SEARCH_MAX_LIMIT);

        let mut sections: Vec<String> = Vec::new();
        if scope != "registry" {
            sections.push(self.mcp_workspace_section(query, limit));
        }
        if scope != "workspace" {
            sections.push(self.mcp_registry_section(query, limit).await);
        }
        ToolOutput::Ok {
            content: sections.join("\n"),
        }
    }

    /// The workspace half: configured servers (`.stella/mcp.toml`) merged
    /// with the servers actually connected this session (derived from the
    /// advertised `mcp__server__tool` names), ranked by the query against
    /// server names AND their tool names.
    fn mcp_workspace_section(&self, query: &str, limit: usize) -> String {
        struct ServerInfo {
            transport: Option<String>,
            tools: Vec<(String, String)>, // (full advertised name, description)
        }
        let mut servers: BTreeMap<String, ServerInfo> = BTreeMap::new();

        let mut config_note = String::new();
        match crate::mcp_cmd::load_config(&self.workspace_root) {
            Ok(config) => {
                for name in config.names() {
                    let transport = config.get(name).map(|t| match t {
                        stella_mcp::McpTransport::Stdio { cmd, .. } => format!("stdio · {cmd}"),
                        stella_mcp::McpTransport::Http { url, .. } => format!("http · {url}"),
                    });
                    servers.insert(
                        name.to_string(),
                        ServerInfo {
                            transport,
                            tools: Vec::new(),
                        },
                    );
                }
            }
            Err(e) => config_note = format!("note: {e}\n"),
        }
        for schema in self.inner.schemas() {
            if let Some((server, _)) = mcp_parts(&schema.name) {
                servers
                    .entry(server.to_string())
                    .or_insert(ServerInfo {
                        transport: None,
                        tools: Vec::new(),
                    })
                    .tools
                    .push((schema.name.clone(), schema.description.clone()));
            }
        }

        if servers.is_empty() {
            return format!(
                "{config_note}no MCP servers configured in .stella/mcp.toml — try scope: \
                 \"registry\" to search the public MCP registry for servers to install"
            );
        }

        let names: Vec<String> = servers.keys().cloned().collect();
        let candidates: Vec<Candidate> = names
            .iter()
            .map(|name| {
                let info = &servers[name];
                Candidate {
                    id: name.clone(),
                    name: name.clone(),
                    description: info.transport.clone().unwrap_or_default(),
                    keywords: info.tools.iter().map(|(full, _)| full.clone()).collect(),
                }
            })
            .collect();
        let hits = rank(query, &candidates, limit);
        if hits.is_empty() {
            return format!(
                "{config_note}no configured MCP server matched `{query}` ({} configured: {}) — \
                 try scope: \"registry\" for installable servers",
                names.len(),
                names.join(", ")
            );
        }

        let mut out = format!(
            "{config_note}{} of {} workspace MCP servers matched `{query}`:\n",
            hits.len(),
            names.len()
        );
        for (i, hit) in hits.iter().enumerate() {
            let name = &names[hit.index];
            let info = &servers[name];
            let state = match (&info.transport, info.tools.is_empty()) {
                (Some(t), false) => format!("{t} · connected, {} tools", info.tools.len()),
                (Some(t), true) => format!("{t} · configured, not connected this session"),
                (None, _) => format!("connected, {} tools", info.tools.len()),
            };
            out.push_str(&format!("{}. {name} ({state})\n", i + 1));
            // The server's own best-matching tools, so the model can go
            // straight from server hit to callable name.
            let tool_candidates: Vec<Candidate> = info
                .tools
                .iter()
                .map(|(full, desc)| Candidate {
                    id: full.clone(),
                    name: full.clone(),
                    description: desc.clone(),
                    keywords: Vec::new(),
                })
                .collect();
            let tool_hits = rank(query, &tool_candidates, 5);
            let shown: Vec<&(String, String)> = if tool_hits.is_empty() {
                info.tools.iter().take(3).collect()
            } else {
                tool_hits.iter().map(|h| &info.tools[h.index]).collect()
            };
            for (full, desc) in shown {
                out.push_str(&format!(
                    "   · {full} — {}\n",
                    clip_line(desc, DESCRIPTION_CLIP_CHARS)
                ));
            }
        }
        out
    }

    /// The registry half: substring search on the public registry, with a
    /// per-term retry when a multi-word query returns nothing (the registry
    /// matches names only), re-ranked client-side.
    async fn mcp_registry_section(&self, query: &str, limit: usize) -> String {
        // The registry's `search` param is a name substring — strip the
        // `+`/`select:` query syntax down to plain terms before sending.
        let terms: Vec<String> = match parse_query(query) {
            DiscoveryQuery::Keywords(terms) => terms.into_iter().map(|t| t.term).collect(),
            DiscoveryQuery::Select(ids) => ids,
        };
        let http_query = terms.join(" ");

        let mut entries: Vec<stella_mcp::RegistryEntry> = match self
            .registry
            .search(&http_query, REGISTRY_FETCH_LIMIT)
            .await
        {
            Ok(page) => page.entries,
            Err(e) => return format!("registry search failed: {e}"),
        };
        if entries.is_empty() && terms.len() > 1 {
            for term in terms.iter().take(3) {
                if let Ok(page) = self.registry.search(term, REGISTRY_FETCH_LIMIT).await {
                    for entry in page.entries {
                        if !entries.iter().any(|e| e.server.name == entry.server.name) {
                            entries.push(entry);
                        }
                    }
                }
            }
        }
        if entries.is_empty() {
            return format!("no public-registry MCP servers matched `{query}`");
        }

        let candidates: Vec<Candidate> = entries
            .iter()
            .map(|entry| Candidate {
                id: entry.server.name.clone(),
                name: entry.server.name.clone(),
                description: entry
                    .server
                    .description
                    .clone()
                    .or_else(|| entry.server.title.clone())
                    .unwrap_or_default(),
                keywords: Vec::new(),
            })
            .collect();
        let hits = rank(query, &candidates, limit);
        // The registry already substring-matched; an empty client-side
        // re-rank (e.g. bare names, no descriptions) falls back to its order.
        let ordered: Vec<usize> = if hits.is_empty() {
            (0..entries.len().min(limit)).collect()
        } else {
            hits.iter().map(|h| h.index).collect()
        };

        let mut out = format!(
            "{} public-registry MCP servers matched `{query}`:\n",
            ordered.len()
        );
        for (i, index) in ordered.iter().enumerate() {
            let entry = &entries[*index];
            let version = entry
                .server
                .version
                .as_deref()
                .map(|v| format!(" v{v}"))
                .unwrap_or_default();
            let status = entry
                .status
                .as_deref()
                .filter(|s| !s.eq_ignore_ascii_case("active"))
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            out.push_str(&format!(
                "{}. {}{version}{status} — {}\n   install: stella mcp add {}\n",
                i + 1,
                entry.server.name,
                clip_line(
                    candidates[*index].description.as_str(),
                    DESCRIPTION_CLIP_CHARS
                ),
                entry.server.name
            ));
        }
        out.push_str("Installed servers connect at the next session start.\n");
        out
    }
}

#[async_trait]
impl ToolExecutor for DiscoveryToolSet<'_> {
    fn schemas(&self) -> Vec<ToolSchema> {
        let mut schemas = self.inner.schemas();
        if let Some(lean) = &self.lean {
            let activated = self.activated.read().unwrap_or_else(|p| p.into_inner());
            schemas.retain(|s| lean.core.contains(&s.name) || activated.contains(&s.name));
        }
        schemas.extend(self.discovery_schemas());
        schemas
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        match name {
            TOOL_SEARCH => self.execute_tool_search(input).await,
            SKILL_SEARCH => self.execute_skill_search(input).await,
            MCP_SEARCH => self.execute_mcp_search(input).await,
            // Deliberately permissive in lean mode: a hidden tool called by
            // name still executes — hiding is a prompt-budget measure, never
            // a capability gate that could wedge a turn.
            _ => self.inner.execute(name, input).await,
        }
    }
}

/// Split an advertised `mcp__server__tool` name into (server, tool).
fn mcp_parts(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix("mcp__")?.split_once("__")
}

fn origin_label(origin: stella_core::skills::SkillOrigin) -> &'static str {
    match origin {
        stella_core::skills::SkillOrigin::Workspace => "workspace",
        stella_core::skills::SkillOrigin::User => "user",
        stella_core::skills::SkillOrigin::AutoCreated => "auto-created",
        stella_core::skills::SkillOrigin::Installed => "installed",
    }
}

/// Read `limit` from the input, clamped to `[1, max]`, defaulting when
/// absent or malformed.
fn clamp_limit(input: &Value, default: usize, max: usize) -> usize {
    input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, max))
        .unwrap_or(default)
}

/// One display line: newlines flattened, clipped on a char boundary.
fn clip_line(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    clip_chars(&flat, max_chars)
}

/// Clip to `max_chars` characters with an ellipsis (char-boundary-safe).
fn clip_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max_chars).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake session stack: a few natives plus two MCP servers' tools.
    struct FakeInner;

    fn schema(name: &str, description: &str, read_only: bool) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only,
        }
    }

    #[async_trait]
    impl ToolExecutor for FakeInner {
        fn schemas(&self) -> Vec<ToolSchema> {
            vec![
                schema("read_file", "Read a file with line numbers", true),
                schema("bash", "Run a shell command", false),
                schema("screenshot", "Capture a screenshot", true),
                schema(
                    "search_issues",
                    "Search the configured issue tracker by keywords",
                    true,
                ),
                schema(
                    "mcp__github__list_commits",
                    "List commits on a GitHub branch",
                    true,
                ),
                schema(
                    "mcp__linear__list_issues",
                    "List Linear issues in the workspace",
                    true,
                ),
            ]
        }
        async fn execute(&self, name: &str, _input: &Value) -> ToolOutput {
            ToolOutput::Ok {
                content: format!("inner ran {name}"),
            }
        }
    }

    /// A scripted registry: returns a fixed page, records nothing.
    struct StubRegistry;

    #[async_trait]
    impl McpRegistrySearch for StubRegistry {
        async fn search(
            &self,
            _query: &str,
            _limit: u32,
        ) -> Result<stella_mcp::RegistryPage, String> {
            Ok(stella_mcp::RegistryPage {
                entries: vec![stella_mcp::RegistryEntry {
                    server: stella_mcp::RegistryServer {
                        name: "io.github.acme/postgres-mcp".into(),
                        description: Some("Query Postgres databases over MCP".into()),
                        title: None,
                        version: Some("1.2.0".into()),
                        repository: None,
                        packages: Vec::new(),
                        remotes: Vec::new(),
                    },
                    status: Some("active".into()),
                    is_latest: Some(true),
                    updated_at: None,
                }],
                next_cursor: None,
                count: Some(1),
            })
        }
    }

    fn full_set(inner: &FakeInner, root: PathBuf) -> DiscoveryToolSet<'_> {
        DiscoveryToolSet::new(inner, root)
            .with_lean(None)
            .with_registry_search(Box::new(StubRegistry))
    }

    fn content(out: ToolOutput) -> String {
        match out {
            ToolOutput::Ok { content } => content,
            ToolOutput::Error { message } => panic!("expected Ok, got error: {message}"),
        }
    }

    #[tokio::test]
    async fn schemas_add_the_discovery_trio_on_top_of_inner() {
        let inner = FakeInner;
        let set = full_set(&inner, std::env::temp_dir());
        let names: Vec<String> = set.schemas().into_iter().map(|s| s.name).collect();
        for expected in [TOOL_SEARCH, SKILL_SEARCH, MCP_SEARCH, "bash", "read_file"] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        // All three are read-only, so the goal judge's ReadOnlyTools keeps them.
        for s in set.schemas() {
            if [TOOL_SEARCH, SKILL_SEARCH, MCP_SEARCH].contains(&s.name.as_str()) {
                assert!(s.read_only, "{} must be read-only", s.name);
            }
        }
    }

    #[tokio::test]
    async fn non_discovery_tools_fall_through_to_inner() {
        let inner = FakeInner;
        let set = full_set(&inner, std::env::temp_dir());
        match set.execute("bash", &Value::Null).await {
            ToolOutput::Ok { content } => assert_eq!(content, "inner ran bash"),
            other => panic!("expected fallthrough, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_search_ranks_and_labels_mcp_tools() {
        let inner = FakeInner;
        let set = full_set(&inner, std::env::temp_dir());
        let out = content(
            set.execute(
                TOOL_SEARCH,
                &serde_json::json!({"query": "+github commits"}),
            )
            .await,
        );
        let first_hit = out.lines().nth(1).expect("a first hit line");
        assert!(
            first_hit.contains("mcp__github__list_commits") && first_hit.contains("(MCP: github)"),
            "got: {out}"
        );
        assert!(
            !out.contains("mcp__linear__"),
            "+github must exclude linear tools: {out}"
        );
    }

    #[tokio::test]
    async fn tool_search_select_form_reports_unknown_names() {
        let inner = FakeInner;
        let set = full_set(&inner, std::env::temp_dir());
        let out = content(
            set.execute(
                TOOL_SEARCH,
                &serde_json::json!({"query": "select:bash,no_such_tool"}),
            )
            .await,
        );
        assert!(out.contains("1. bash"), "got: {out}");
        assert!(out.contains("not found: no_such_tool"), "got: {out}");
    }

    #[tokio::test]
    async fn tool_search_requires_a_query() {
        let inner = FakeInner;
        let set = full_set(&inner, std::env::temp_dir());
        assert!(
            set.execute(TOOL_SEARCH, &serde_json::json!({}))
                .await
                .is_error()
        );
        assert!(
            set.execute(TOOL_SEARCH, &serde_json::json!({"query": "  "}))
                .await
                .is_error()
        );
    }

    #[tokio::test]
    async fn lean_mode_advertises_only_core_plus_discovery_until_a_search_activates() {
        let inner = FakeInner;
        let set = DiscoveryToolSet::new(&inner, std::env::temp_dir())
            .with_registry_search(Box::new(StubRegistry))
            .with_lean(Some(LeanConfig {
                core: ["read_file", "bash"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            }));

        let names: Vec<String> = set.schemas().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&TOOL_SEARCH.to_string()));
        assert!(
            !names.iter().any(|n| n.starts_with("mcp__")),
            "lean mode must hide non-core tools: {names:?}"
        );

        // A search for the hidden tool activates it…
        let _ = set
            .execute(TOOL_SEARCH, &serde_json::json!({"query": "github commits"}))
            .await;
        let names: Vec<String> = set.schemas().into_iter().map(|s| s.name).collect();
        assert!(
            names.contains(&"mcp__github__list_commits".to_string()),
            "a searched tool must be advertised afterwards: {names:?}"
        );
    }

    #[tokio::test]
    async fn lean_mode_still_executes_hidden_tools_by_name() {
        let inner = FakeInner;
        let set = DiscoveryToolSet::new(&inner, std::env::temp_dir())
            .with_registry_search(Box::new(StubRegistry))
            .with_lean(Some(LeanConfig {
                core: HashSet::new(),
            }));
        match set.execute("screenshot", &Value::Null).await {
            ToolOutput::Ok { content } => assert_eq!(content, "inner ran screenshot"),
            other => panic!("hidden tools must still execute, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lean_activation_survives_a_tool_stack_rebuild_via_the_shared_handle() {
        let inner = FakeInner;
        let activation = new_activation();
        let lean = Some(LeanConfig {
            core: HashSet::new(),
        });
        {
            let set = DiscoveryToolSet::new(&inner, std::env::temp_dir())
                .with_registry_search(Box::new(StubRegistry))
                .with_lean(lean.clone())
                .with_activation(activation.clone());
            let _ = set
                .execute(TOOL_SEARCH, &serde_json::json!({"query": "github commits"}))
                .await;
        }
        // A NEW per-turn stack with the same session handle still advertises
        // the activated tool.
        let set = DiscoveryToolSet::new(&inner, std::env::temp_dir())
            .with_registry_search(Box::new(StubRegistry))
            .with_lean(lean)
            .with_activation(activation);
        let names: Vec<String> = set.schemas().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"mcp__github__list_commits".to_string()));
    }

    /// Write a skill in the ecosystem layout under the given root.
    fn write_skill(root: &std::path::Path, slug: &str, description: &str, body: &str) {
        let dir = root.join(".stella/skills").join(slug);
        std::fs::create_dir_all(&dir).expect("skill dir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {slug}\ndescription: {description}\n---\n{body}\n"),
        )
        .expect("skill file");
    }

    /// Run one skill_search against a workspace with HOME redirected to an
    /// empty tempdir, so the developer's real user-global skills can't leak
    /// into assertions. Sync on purpose: the env lock must be held across
    /// the whole mutation window, and holding a `MutexGuard` across an
    /// `.await` trips `clippy::await_holding_lock` — so the async execute
    /// runs on a local runtime inside the sync scope instead.
    fn skill_search_with_isolated_home(ws: &std::path::Path, input: Value) -> String {
        let _lock = crate::test_env::lock();
        let home = tempfile::tempdir().expect("home");
        let prev_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };

        let inner = FakeInner;
        let set = full_set(&inner, ws.to_path_buf()).with_project_prompts_allowed(true);
        let out = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(set.execute(SKILL_SEARCH, &input));

        match prev_home {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        content(out)
    }

    #[test]
    fn skill_search_finds_installed_skills_and_returns_the_body_on_request() {
        let ws = tempfile::tempdir().expect("ws");
        write_skill(
            ws.path(),
            "sql-formatting",
            "How this repo formats SQL migrations",
            "Always uppercase keywords.",
        );
        write_skill(
            ws.path(),
            "release-notes",
            "Writing release notes for the changelog",
            "Lead with user impact.",
        );

        let out = skill_search_with_isolated_home(
            ws.path(),
            serde_json::json!({"query": "sql migrations", "include_body": true}),
        );

        let first_hit = out.lines().nth(1).expect("a first hit line");
        assert!(first_hit.contains("sql-formatting"), "got: {out}");
        assert!(
            out.contains("Always uppercase keywords."),
            "include_body must return the top match's instructions: {out}"
        );
    }

    #[test]
    fn skill_search_with_no_match_points_at_the_public_registry() {
        let ws = tempfile::tempdir().expect("ws");
        let out =
            skill_search_with_isolated_home(ws.path(), serde_json::json!({"query": "kubernetes"}));
        assert!(out.contains("search_skills"), "got: {out}");
    }

    #[tokio::test]
    async fn mcp_search_workspace_scope_merges_config_and_connected_servers() {
        let ws = tempfile::tempdir().expect("ws");
        std::fs::create_dir_all(ws.path().join(".stella")).expect("dir");
        std::fs::write(
            ws.path().join(".stella/mcp.toml"),
            "[servers.linear]\ntransport = \"http\"\nurl = \"https://mcp.linear.app/mcp\"\n\
             [servers.offline]\ntransport = \"stdio\"\ncmd = \"npx\"\n",
        )
        .expect("mcp.toml");

        let inner = FakeInner;
        let set = full_set(&inner, ws.path().to_path_buf());
        let out = content(
            set.execute(MCP_SEARCH, &serde_json::json!({"query": "linear issues"}))
                .await,
        );
        assert!(out.contains("linear"), "got: {out}");
        assert!(
            out.contains("mcp__linear__list_issues"),
            "the server's matching tools must be listed: {out}"
        );
        // `github` is connected but absent from mcp.toml — still findable.
        let out = content(
            set.execute(MCP_SEARCH, &serde_json::json!({"query": "github commits"}))
                .await,
        );
        assert!(out.contains("github"), "got: {out}");
    }

    #[tokio::test]
    async fn mcp_search_registry_scope_lists_installable_servers() {
        let ws = tempfile::tempdir().expect("ws");
        let inner = FakeInner;
        let set = full_set(&inner, ws.path().to_path_buf());
        let out = content(
            set.execute(
                MCP_SEARCH,
                &serde_json::json!({"query": "postgres", "scope": "registry"}),
            )
            .await,
        );
        assert!(out.contains("io.github.acme/postgres-mcp"), "got: {out}");
        assert!(
            out.contains("stella mcp add io.github.acme/postgres-mcp"),
            "an install hint must be present: {out}"
        );
    }

    #[tokio::test]
    async fn mcp_search_rejects_an_unknown_scope() {
        let inner = FakeInner;
        let set = full_set(&inner, std::env::temp_dir());
        assert!(
            set.execute(
                MCP_SEARCH,
                &serde_json::json!({"query": "x", "scope": "everywhere"})
            )
            .await
            .is_error()
        );
    }

    #[test]
    fn lean_env_parsing_covers_all_forms() {
        let _lock = crate::test_env::lock();
        let cases: &[(&str, Option<usize>)] = &[
            ("0", None),
            ("false", None),
            ("", None),
            ("1", Some(DEFAULT_CORE_TOOLS.len())),
            ("true", Some(DEFAULT_CORE_TOOLS.len())),
            ("read_file, bash", Some(2)),
        ];
        for (value, expected_core_len) in cases {
            unsafe { std::env::set_var(LEAN_TOOLS_ENV, value) };
            let lean = lean_from_env();
            assert_eq!(
                lean.as_ref().map(|l| l.core.len()),
                *expected_core_len,
                "for env value {value:?}"
            );
        }
        unsafe { std::env::remove_var(LEAN_TOOLS_ENV) };
        assert_eq!(lean_from_env(), None);
    }
}
