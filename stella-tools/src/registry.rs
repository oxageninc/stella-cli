//! Tool trait + registry. The agent loop drives every tool through
//! `ToolRegistry::execute` — no tool-specific code lives outside this crate.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use stella_core::bus::{self, HookBus, HookDecision, HookEventDraft, names as hook_names};
use stella_core::ports::ToolExecutor;
use stella_protocol::tool::{ToolOutput, ToolSchema};

pub use crate::file_touch::FileOp;
use crate::file_touch::{
    FileTouchEvent, FileTouchLedger, FileTouchTelemetry, count_lines, line_diff,
    normalize_workspace_path,
};

/// One tool the agent can call. Input arrives as the model-produced JSON;
/// output is always a typed `ToolOutput` (never a bare string).
#[async_trait]
pub trait Tool: Send + Sync {
    /// Schema advertised to the model: name, description, JSON Schema.
    fn schema(&self) -> ToolSchema;

    /// Execute the tool. `root` is the workspace root for path resolution;
    /// tools must never read or write outside it without explicit opt-in.
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput;
}

/// A file op classified before execution: the normalized path the ledger
/// aggregates by, the resolved on-disk path (for pre/post content capture),
/// and the CRUD event. Create-vs-update is decided here — it depends on
/// whether the file exists BEFORE the write, not after.
struct PendingTouch {
    path: String,
    full: PathBuf,
    op: FileOp,
}

/// Registry of all built-in tools, keyed by name. Also the session's
/// file-lifecycle ledger: every successful read/create/update/delete is
/// recorded per normalized path as a [`FileTouchEvent`] (CRUD event, reason,
/// line delta) — rendered by the TUI as the "Files Touched" panel and
/// persisted as the session's file-touch telemetry.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// Tools enabled AFTER construction — currently just `code_graph`, once a
    /// mid-session `stella init` / `/init` builds the index. Kept in a
    /// separate interior-mutable overlay so the primary `tools` map stays
    /// immutable and lock-free; the overlay is consulted as a fallback in the
    /// three read paths (`schemas`, dispatch, name listing).
    late_tools: std::sync::RwLock<HashMap<String, Arc<dyn Tool>>>,
    root: PathBuf,
    touched: std::sync::Mutex<FileTouchLedger>,
    /// The session's memory-citation ledger, shared with the registered
    /// `cite_memory` tool instance and drained per execution by
    /// [`ToolRegistry::take_memory_citations`].
    citations: crate::memory::CitationLedger,
    schema_index: std::sync::Mutex<SchemaIndex>,
    /// The session's extension hook bus, once a host attaches one
    /// ([`ToolRegistry::attach_bus`]). Every emission is `None`-guarded, so
    /// a bus-less registry behaves exactly as it did before hooks existed.
    bus: std::sync::RwLock<Option<HookBus>>,
}

/// The known schema objects in the workspace, used by the schema gate to
/// prevent duplicate table/type/view creation. Populated from the code graph
/// or empty when no graph is open.
#[derive(Debug, Clone, Default)]
pub struct SchemaIndex {
    pub tables: HashSet<String>,
    pub types: HashSet<String>,
    pub views: HashSet<String>,
}

impl ToolRegistry {
    /// Construct with auto-detected optional backends (issue tracker, media
    /// provider). Prefer [`ToolRegistry::new_detected`] from async contexts —
    /// this synchronous form probes `gh` inline, blocking the calling thread.
    pub fn new(root: PathBuf) -> Self {
        Self::with_backends(
            root,
            crate::issues::detect_issue_backend(),
            crate::media::detect_media_backend(),
        )
    }

    /// [`ToolRegistry::new`] with the process-spawning issue-backend probe
    /// routed through the blocking pool (#64) — the constructor every async
    /// session driver uses.
    pub async fn new_detected(root: PathBuf) -> Self {
        Self::with_backends(
            root,
            crate::issues::detect_issue_backend_async().await,
            crate::media::detect_media_backend(),
        )
    }

    /// Construct with an explicit issue backend (or none) and no media
    /// backend — the seam unit tests use so tool counts depend on neither
    /// the host's `gh` auth nor its provider env keys.
    pub fn with_issue_backend(
        root: PathBuf,
        issue_backend: Option<crate::issues::IssueBackend>,
    ) -> Self {
        Self::with_backends(root, issue_backend, None)
    }

    /// Construct with every optional backend explicit — the full seam.
    pub fn with_backends(
        root: PathBuf,
        issue_backend: Option<crate::issues::IssueBackend>,
        media_backend: Option<crate::media::MediaBackend>,
    ) -> Self {
        let citations: crate::memory::CitationLedger = Arc::default();
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        let mut entries: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::read::ReadFile),
            Arc::new(crate::write::WriteFile),
            Arc::new(crate::edit::EditFile),
            Arc::new(crate::delete::DeleteFile),
            Arc::new(crate::bash::Bash),
            Arc::new(crate::grep::Grep),
            Arc::new(crate::glob::Glob),
            Arc::new(crate::exploration::Explorations),
            Arc::new(crate::exploration::SaveExploration),
            Arc::new(crate::memory::SaveMemory),
            Arc::new(crate::memory::CiteMemory(citations.clone())),
            Arc::new(crate::verify::VerifyDone),
            Arc::new(crate::project::BuildProject),
            Arc::new(crate::project::RunTests),
            Arc::new(crate::ci::CiStatus),
            Arc::new(crate::screenshot::Screenshot),
            // SVG generation is client-side (the model authors the SVG, the
            // pipeline validates/sanitizes) — no provider key needed, so
            // unlike image/video it is always registered.
            Arc::new(crate::media::GenerateSvg),
        ];
        // The code-graph query tool exists only when `stella init` has built
        // an index — same conditional-registration discipline as the issue
        // tools: no index, no dead schema entry.
        if crate::graph::graph_available(&root) {
            entries.push(Arc::new(crate::graph::CodeGraphQuery));
        }
        // Media generation exists only when an image-capable provider key is
        // configured — BYOK end to end. The async video pair additionally
        // needs the key family to have a video adapter.
        if let Some(media) = media_backend {
            entries.push(Arc::new(crate::media::GenerateImage(media.image)));
            if let Some(video) = media.video {
                entries.push(Arc::new(crate::media::GenerateVideo(video.clone())));
                entries.push(Arc::new(crate::media::PollVideo(video)));
            }
        }
        // Issue tools exist only when a tracker is configured — no dead
        // schema entries burning tokens, no surface that errors on use.
        if let Some(backend) = issue_backend {
            let backend = Arc::new(backend);
            entries.push(Arc::new(crate::issues::CreateIssue(backend.clone())));
            entries.push(Arc::new(crate::issues::UpdateIssue(backend.clone())));
            entries.push(Arc::new(crate::issues::CloseIssue(backend.clone())));
            entries.push(Arc::new(crate::issues::SearchIssues(backend.clone())));
            entries.push(Arc::new(crate::issues::StartWorkOnIssue(backend)));
        }
        for tool in entries {
            let name = tool.schema().name;
            tools.insert(name, tool);
        }
        Self {
            tools,
            late_tools: std::sync::RwLock::new(HashMap::new()),
            root,
            touched: std::sync::Mutex::new(FileTouchLedger::default()),
            citations,
            schema_index: std::sync::Mutex::new(SchemaIndex::default()),
            bus: std::sync::RwLock::new(None),
        }
    }

    /// Attach the session's extension hook bus. From this point every tool
    /// call runs the blocking policy chains (`tool.call.requested`, then
    /// `file.created`/`file.updated`/`file.deleted` or `command.started`)
    /// before executing, and emits the observer events documented in
    /// `docs/hooks.md`. Also emits one `tool.registered` per registered
    /// tool, name-sorted, so extensions see the tool surface up front.
    pub fn attach_bus(&self, bus: HookBus) {
        let mut schemas = ToolRegistry::schemas(self);
        schemas.sort_by(|a, b| a.name.cmp(&b.name));
        for schema in schemas {
            bus.emit_named(
                hook_names::TOOL_REGISTERED,
                serde_json::json!({ "tool": schema.name, "read_only": schema.read_only }),
            );
        }
        *self.bus.write().unwrap_or_else(|p| p.into_inner()) = Some(bus);
    }

    /// The attached hook bus, if any (cheap clone — shared inner).
    fn bus(&self) -> Option<HookBus> {
        self.bus.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Enable the `code_graph` query tool if `stella init` has since built the
    /// index and it isn't already registered. Idempotent, cheap, and safe to
    /// call every turn. Lets a mid-session `/init` expose `code_graph` to
    /// subsequent turns without rebuilding the whole registry (and its MCP
    /// wrapper) — the overlay is shared through every `&ToolRegistry` /
    /// `Arc<ToolRegistry>` reference the session already holds.
    pub fn enable_code_graph_if_available(&self, root: &std::path::Path) {
        if !crate::graph::graph_available(root) {
            return;
        }
        let tool: Arc<dyn Tool> = Arc::new(crate::graph::CodeGraphQuery);
        let name = tool.schema().name;
        if self.tools.contains_key(&name) {
            return; // already registered at construction
        }
        let mut late = self.late_tools.write().unwrap_or_else(|p| p.into_inner());
        late.entry(name).or_insert(tool);
    }

    /// All tool schemas, for advertising to the model — primary tools plus any
    /// late-enabled overlay tools.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        let mut schemas: Vec<ToolSchema> = self.tools.values().map(|t| t.schema()).collect();
        let late = self.late_tools.read().unwrap_or_else(|p| p.into_inner());
        schemas.extend(late.values().map(|t| t.schema()));
        schemas
    }

    /// Execute a tool by name. Returns an error `ToolOutput` if the name is
    /// unknown or the input is malformed — never panics.
    pub async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        let bus = self.bus();
        let started_at = std::time::Instant::now();

        // Extension policy, stage 1: the `tool.call.requested` blocking
        // chain. Runs FIRST — a `modify` decision replaces the input, and
        // everything downstream (classification, pre-content capture, the
        // schema gate, execution) must see the final input, not the
        // original.
        let mut modified_input: Option<Value> = None;
        if let Some(bus) = &bus {
            match self.gate_tool_call(bus, name, input) {
                Ok(replacement) => modified_input = replacement,
                Err(denied) => return denied,
            }
        }
        let input: &Value = modified_input.as_ref().unwrap_or(input);

        // Classify the file op BEFORE executing: create-vs-update depends
        // on whether the file exists now, not after the write.
        let pending_op = self.classify_file_op(name, input);
        // Updates need the pre-write content for the line diff; deletes need
        // it for the pre-deletion line count. Lossy UTF-8 so a binary file
        // still yields a deterministic (if approximate) line count.
        let pre_content: Option<String> = match &pending_op {
            Some(pending) if matches!(pending.op, FileOp::Update | FileOp::Delete) => {
                std::fs::read(&pending.full)
                    .ok()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            }
            _ => None,
        };

        // Schema gate: if the target is a SQL file, parse the proposed
        // content for DDL objects and check them against the known schema
        // index before the write/edit lands. Objects the target file already
        // defines on disk are exempt — rewriting an existing migration in
        // place is not a duplicate.
        let mut pending_schema: Vec<crate::schema_gate::DdlObject> = Vec::new();
        if let Some(path) = input.get("path").and_then(|v| v.as_str())
            && matches!(name, "write_file" | "edit_file")
            && crate::schema_gate::is_schema_file(path)
        {
            // write_file carries the full file in `content`; edit_file
            // introduces new DDL only through `new_string`.
            let proposed_src = match name {
                "write_file" => input.get("content").and_then(|v| v.as_str()),
                _ => input.get("new_string").and_then(|v| v.as_str()),
            };
            let proposed = proposed_src
                .map(crate::schema_gate::extract_ddl_objects)
                .unwrap_or_default();
            if !proposed.is_empty() {
                let own: HashSet<String> = crate::resolve_within_root(&self.root, path)
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .map(|current| {
                        crate::schema_gate::extract_ddl_objects(&current)
                            .into_iter()
                            .map(|o| o.name.to_lowercase())
                            .collect()
                    })
                    .unwrap_or_default();
                let fresh: Vec<crate::schema_gate::DdlObject> = proposed
                    .into_iter()
                    .filter(|o| !own.contains(&o.name.to_lowercase()))
                    .collect();
                if !fresh.is_empty() {
                    let index = self.schema_index.lock().unwrap_or_else(|p| p.into_inner());
                    let conflicts = crate::schema_gate::find_conflicts(
                        &fresh,
                        &index.tables,
                        &index.types,
                        &index.views,
                    );
                    if !conflicts.is_empty() {
                        return ToolOutput::Error {
                            message: crate::schema_gate::format_conflicts(&conflicts),
                        };
                    }
                    pending_schema = fresh;
                }
            }
        }

        // Extension policy, stage 2: the per-side-effect blocking chains
        // (`file.created`/`file.updated`/`file.deleted`, `command.started`)
        // plus the payload-hygiene detectors, then the observable
        // `tool.call.started` (with content-bearing fields sanitized).
        if let Some(bus) = &bus {
            if let Err(denied) = self.gate_side_effects(bus, name, input, pending_op.as_ref()) {
                return denied;
            }
            bus.emit_named(
                hook_names::TOOL_CALL_STARTED,
                serde_json::json!({ "tool": name, "input": bus::sanitize_tool_input(input) }),
            );
        }

        // Resolve from the primary map, falling back to the late-enabled
        // overlay. Clone the `Arc` out so the overlay's read lock is released
        // before the `.await` (a lock guard must not cross an await point).
        let tool = self.tools.get(name).cloned().or_else(|| {
            self.late_tools
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .get(name)
                .cloned()
        });
        let output = match tool {
            Some(tool) => tool.execute(input, &self.root).await,
            None => ToolOutput::Error {
                message: format!(
                    "unknown tool `{name}` — available: {}",
                    self.available_names()
                ),
            },
        };
        if let Some(bus) = &bus {
            let duration_ms = started_at.elapsed().as_millis() as u64;
            let command = || input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            match &output {
                ToolOutput::Error { message } => {
                    bus.emit_named(
                        hook_names::TOOL_CALL_FAILED,
                        serde_json::json!({
                            "tool": name, "error": message, "duration_ms": duration_ms,
                        }),
                    );
                    if name == "bash" {
                        bus.emit_named(
                            hook_names::COMMAND_FAILED,
                            serde_json::json!({ "command": command(), "error": message }),
                        );
                    }
                }
                ToolOutput::Ok { .. } => {
                    bus.emit_named(
                        hook_names::TOOL_CALL_COMPLETED,
                        serde_json::json!({ "tool": name, "duration_ms": duration_ms }),
                    );
                    if name == "bash" {
                        bus.emit_named(
                            hook_names::COMMAND_COMPLETED,
                            serde_json::json!({ "command": command() }),
                        );
                    }
                }
            }
        }
        if !output.is_error() {
            // The write landed, so the objects it creates now exist: grow
            // the index so a duplicate later this session conflicts even
            // before any re-index from the code graph.
            if !pending_schema.is_empty() {
                self.record_schema_objects(&pending_schema);
            }
            if let Some(pending) = pending_op {
                self.record_touch(pending, pre_content, name, input, bus.as_ref());
            }
        }
        output
    }

    /// Run the `tool.call.requested` blocking chain. `Ok(None)`: allowed
    /// unchanged. `Ok(Some(input))`: allowed with a policy-modified input.
    /// `Err(output)`: denied or pending approval — the error the model sees
    /// instead of a tool result. The chain receives the RAW input (the
    /// interception point is privileged by design); observable events carry
    /// only sanitized inputs.
    fn gate_tool_call(
        &self,
        bus: &HookBus,
        name: &str,
        input: &Value,
    ) -> Result<Option<Value>, ToolOutput> {
        let outcome = bus.emit_blocking(HookEventDraft::new(
            hook_names::TOOL_CALL_REQUESTED,
            serde_json::json!({ "tool": name, "input": input }),
        ));
        match outcome.decision {
            HookDecision::Deny { reason } => Err(ToolOutput::Error {
                message: format!("`{name}` was denied by an extension policy: {reason}"),
            }),
            HookDecision::RequireApproval { reason } => Err(ToolOutput::Error {
                message: format!("`{name}` requires approval before it can run: {reason}"),
            }),
            _ => {
                if !outcome.modified {
                    return Ok(None);
                }
                match outcome.event.payload.get("input") {
                    Some(new_input) => Ok(Some(new_input.clone())),
                    None => {
                        // A `modify` that dropped the `input` field is a
                        // broken policy handler — surface it, keep the
                        // original input rather than executing garbage.
                        bus.emit_named(
                            hook_names::EXTENSION_ERROR,
                            serde_json::json!({
                                "failed_event": hook_names::TOOL_CALL_REQUESTED,
                                "error": "modify decision dropped the `input` field; original input kept",
                            }),
                        );
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Run the per-side-effect blocking chains and payload-hygiene
    /// detectors for one already-final input: `sensitive_operation.detected`
    /// and `secret.detected` observers first (an auditor sees the attempt
    /// even when a policy then denies it), then the `file.*` chain for a
    /// classified C/U/D op and the `command.started` chain for `bash`.
    /// These chains gate — `modify` decisions are recorded but not honored
    /// here, because the input was already final after
    /// `tool.call.requested`. Reads are observable but never interceptable.
    fn gate_side_effects(
        &self,
        bus: &HookBus,
        name: &str,
        input: &Value,
        pending: Option<&PendingTouch>,
    ) -> Result<(), ToolOutput> {
        if let Some(pending) = pending {
            if bus::is_sensitive_path(&pending.path) {
                bus.emit_named(
                    hook_names::SENSITIVE_OPERATION_DETECTED,
                    serde_json::json!({
                        "path": pending.path,
                        "tool": name,
                        "operation": pending.op.letter().to_string(),
                    }),
                );
            }
            if matches!(name, "write_file" | "edit_file") {
                let proposed = input
                    .get("content")
                    .or_else(|| input.get("new_string"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let kinds = bus::scan_for_secrets(proposed);
                if !kinds.is_empty() {
                    bus.emit_named(
                        hook_names::SECRET_DETECTED,
                        serde_json::json!({
                            "path": pending.path,
                            "kinds": kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
                        }),
                    );
                }
            }
            let event_name = match pending.op {
                FileOp::Create => Some(hook_names::FILE_CREATED),
                FileOp::Update => Some(hook_names::FILE_UPDATED),
                FileOp::Delete => Some(hook_names::FILE_DELETED),
                FileOp::Read => None,
            };
            if let Some(event_name) = event_name {
                let outcome = bus.emit_blocking(HookEventDraft::new(
                    event_name,
                    serde_json::json!({
                        "path": pending.path,
                        "tool": name,
                        "operation": pending.op.letter().to_string(),
                    }),
                ));
                match outcome.decision {
                    HookDecision::Deny { reason } => {
                        return Err(ToolOutput::Error {
                            message: format!(
                                "`{name}` on `{}` was denied by an extension policy: {reason}",
                                pending.path
                            ),
                        });
                    }
                    HookDecision::RequireApproval { reason } => {
                        return Err(ToolOutput::Error {
                            message: format!(
                                "`{name}` on `{}` requires approval: {reason}",
                                pending.path
                            ),
                        });
                    }
                    _ => {}
                }
            }
        }
        if name == "bash" {
            let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let outcome = bus.emit_blocking(HookEventDraft::new(
                hook_names::COMMAND_STARTED,
                serde_json::json!({ "command": command }),
            ));
            match outcome.decision {
                HookDecision::Deny { reason } => {
                    return Err(ToolOutput::Error {
                        message: format!("command was denied by an extension policy: {reason}"),
                    });
                }
                HookDecision::RequireApproval { reason } => {
                    return Err(ToolOutput::Error {
                        message: format!("command requires approval: {reason}"),
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Merge just-created DDL objects into the schema index (lowercased —
    /// `find_conflicts` matches case-insensitively against lowercase names).
    fn record_schema_objects(&self, objects: &[crate::schema_gate::DdlObject]) {
        let mut index = self.schema_index.lock().unwrap_or_else(|p| p.into_inner());
        for obj in objects {
            let name = obj.name.to_lowercase();
            match obj.kind {
                crate::schema_gate::DdlKind::Table => index.tables.insert(name),
                crate::schema_gate::DdlKind::Type => index.types.insert(name),
                crate::schema_gate::DdlKind::View => index.views.insert(name),
            };
        }
    }

    /// `[C|R|U|D]`-classify a call: reads → R, writes → C (new) or U
    /// (existing), edits → U, deletes → D. The path is normalized to its
    /// workspace-relative POSIX form here, so equivalent spellings
    /// (`src/./a.rs`, `src/../src/a.rs`) aggregate into one ledger record;
    /// escaping paths classify as `None` (the tool rejects them anyway).
    /// `bash` is opaque — file ops done through the shell aren't
    /// attributable, which is why the CRUD tools exist and the prompt steers
    /// agents toward them.
    fn classify_file_op(&self, tool: &str, input: &Value) -> Option<PendingTouch> {
        let raw = input.get("path").and_then(|v| v.as_str())?;
        let full = crate::resolve_within_root(&self.root, raw)?;
        let path = normalize_workspace_path(&self.root, raw)?;
        let op = match tool {
            "read_file" => FileOp::Read,
            "edit_file" => FileOp::Update,
            "delete_file" => FileOp::Delete,
            "write_file" => {
                if full.exists() {
                    FileOp::Update
                } else {
                    FileOp::Create
                }
            }
            _ => return None,
        };
        Some(PendingTouch { path, full, op })
    }

    /// Record one SUCCESSFUL file touch: derive the event's line delta per
    /// the counting rules (R: zero; C: full new line count; U: line diff of
    /// pre vs post content; D: full pre-deletion line count), attach the
    /// model-supplied `reason` (or a per-op default), and append it to the
    /// ledger. The append and its aggregate updates happen under one lock,
    /// so concurrent tool completions never lose or interleave an event.
    ///
    /// With a bus attached, the touch also emits its `file.*` fact event
    /// (path + reason + line deltas, never contents) and a
    /// `files_touched.updated` carrying the changed record plus the ledger
    /// `revision` assigned under the lock — concurrent touches may deliver
    /// out of order, but consumers keep the highest revision and stay
    /// consistent.
    fn record_touch(
        &self,
        pending: PendingTouch,
        pre_content: Option<String>,
        tool: &str,
        input: &Value,
        bus: Option<&HookBus>,
    ) {
        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| default_reason(pending.op).to_string());
        let (lines_added, lines_removed) = match pending.op {
            FileOp::Read => (0, 0),
            FileOp::Create => (
                input
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(count_lines)
                    .unwrap_or(0),
                0,
            ),
            FileOp::Update => {
                // write_file carries the post content in its input; edit_file
                // landed it on disk (mutating tools are never parallelized —
                // see the read_only partition test — so this read-back sees
                // exactly what the edit wrote).
                let post = match tool {
                    "write_file" => input
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    _ => std::fs::read(&pending.full)
                        .ok()
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .unwrap_or_default(),
                };
                line_diff(pre_content.as_deref().unwrap_or(""), &post)
            }
            FileOp::Delete => (0, pre_content.as_deref().map(count_lines).unwrap_or(0)),
        };
        let path = pending.path;
        let (revision, record, total_files) = {
            let mut touched = self.touched.lock().unwrap_or_else(|p| p.into_inner());
            touched.record(
                path.clone(),
                FileTouchEvent {
                    event: pending.op,
                    reason: reason.clone(),
                    lines_added,
                    lines_removed,
                },
            );
            (
                touched.revision(),
                touched.get(&path).cloned(),
                touched.len(),
            )
        };
        if let Some(bus) = bus {
            let fact = match pending.op {
                FileOp::Read => hook_names::FILE_READ,
                FileOp::Create => hook_names::FILE_CREATED,
                FileOp::Update => hook_names::FILE_UPDATED,
                FileOp::Delete => hook_names::FILE_DELETED,
            };
            bus.emit_named(
                fact,
                serde_json::json!({
                    "path": path,
                    "reason": reason,
                    "lines_added": lines_added,
                    "lines_removed": lines_removed,
                }),
            );
            bus.emit_named(
                hook_names::FILES_TOUCHED_UPDATED,
                serde_json::json!({
                    "revision": revision,
                    "path": path,
                    "record": record,
                    "total_files": total_files,
                }),
            );
        }
    }

    /// Snapshot of every file touched this session, insertion-ordered,
    /// with its ops as a compact CRUD string (e.g. `"RU"`).
    pub fn files_touched(&self) -> Vec<(String, String)> {
        self.touched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .compact()
    }

    /// The full session file-touch telemetry payload: per normalized path,
    /// deduplicated CRUD events (first-occurrence order), line-delta totals,
    /// and the complete ordered audit log.
    pub fn file_touch_telemetry(&self) -> FileTouchTelemetry {
        self.touched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .snapshot()
    }

    /// Drain the memory citations the `cite_memory` tool recorded since the
    /// last drain. Draining (unlike the cumulative file-touch snapshot) is
    /// what lets the CLI persist each citation under exactly one execution —
    /// re-persisting under later executions would inflate the promotion
    /// eligibility count.
    pub fn take_memory_citations(&self) -> Vec<crate::memory::MemoryCitation> {
        std::mem::take(&mut *self.citations.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Comma-separated sorted list of registered tool names, for error
    /// messages.
    pub fn available_names(&self) -> String {
        let late = self.late_tools.read().unwrap_or_else(|p| p.into_inner());
        let mut names: Vec<String> = self
            .tools
            .keys()
            .cloned()
            .chain(late.keys().cloned())
            .collect();
        names.sort();
        names.join(", ")
    }

    /// The workspace root this registry resolves paths against.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Update the known schema index from the code graph. Called at session
    /// start and whenever schema files are re-indexed. The schema gate uses
    /// this to prevent duplicate table/type/view creation.
    pub fn update_schema_index(
        &self,
        tables: HashSet<String>,
        types: HashSet<String>,
        views: HashSet<String>,
    ) {
        let mut index = self.schema_index.lock().unwrap_or_else(|p| p.into_inner());
        index.tables = tables;
        index.types = types;
        index.views = views;
    }
}

/// Fallback audit-log reason when the model omitted the tool's optional
/// `reason` field — the schema requires a non-empty human-readable string.
fn default_reason(op: FileOp) -> &'static str {
    match op {
        FileOp::Create => "file created (no reason given)",
        FileOp::Read => "file read (no reason given)",
        FileOp::Update => "file updated (no reason given)",
        FileOp::Delete => "file deleted (no reason given)",
    }
}

/// `ToolRegistry` is the production implementation of `stella-core`'s
/// `ToolExecutor` port — the engine drives every tool call through this
/// impl, never through `stella-tools` types directly.
#[async_trait]
impl ToolExecutor for ToolRegistry {
    fn schemas(&self) -> Vec<ToolSchema> {
        ToolRegistry::schemas(self)
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        ToolRegistry::execute(self, name, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::issues::IssueBackend;

    #[tokio::test]
    async fn unknown_tool_returns_error_not_panic() {
        let reg = ToolRegistry::with_issue_backend(PathBuf::from("/tmp"), None);
        let result = reg.execute("nonexistent", &Value::Null).await;
        assert!(result.is_error());
    }

    #[test]
    fn every_registry_tool_is_reserved_against_custom_shadowing() {
        // RESERVED_NAMES (custom.rs) hand-mirrors the registry so a custom
        // manifest can never shadow a built-in. Its comment says "keep this
        // in sync if either set changes" — this test IS that sync: a new
        // built-in that isn't reserved would be silently shadowable.
        let reg =
            ToolRegistry::with_issue_backend(PathBuf::from("/tmp"), Some(IssueBackend::GitHub));
        for schema in reg.schemas() {
            assert!(
                crate::custom::RESERVED_NAMES.contains(&schema.name.as_str()),
                "built-in tool `{}` is missing from custom::RESERVED_NAMES — \
                 a custom manifest could shadow it",
                schema.name
            );
        }

        // Conditionally-registered tools never show up in a bare registry's
        // schemas (code_graph needs an index on disk, the media tools need a
        // capable key), so the registry-driven loop above can't catch them
        // drifting out of RESERVED_NAMES. Pin them explicitly — if you add a
        // new conditionally-registered tool, add its name to this array.
        const CONDITIONALLY_REGISTERED: &[&str] = &[
            "code_graph",
            "generate_image",
            "generate_video",
            "poll_video",
        ];
        for name in CONDITIONALLY_REGISTERED {
            assert!(
                crate::custom::RESERVED_NAMES.contains(name),
                "conditionally-registered tool `{name}` is missing from \
                 custom::RESERVED_NAMES — a custom manifest could shadow it"
            );
        }
    }

    #[test]
    fn registry_advertises_the_full_tool_set() {
        let reg =
            ToolRegistry::with_issue_backend(PathBuf::from("/tmp"), Some(IssueBackend::GitHub));
        let names: Vec<String> = reg.schemas().iter().map(|s| s.name.clone()).collect();
        for expected in [
            "read_file",
            "write_file",
            "edit_file",
            "delete_file",
            "bash",
            "grep",
            "glob",
            "explorations",
            "save_exploration",
            "save_memory",
            "cite_memory",
            "verify_done",
            "build_project",
            "run_tests",
            "ci_status",
            "screenshot",
            "generate_svg",
            "create_issue",
            "update_issue",
            "close_issue",
            "search_issues",
            "start_work_on_issue",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert_eq!(names.len(), 22, "unexpected tool count: {names:?}");
    }

    #[test]
    fn issue_tools_absent_without_a_configured_backend() {
        let reg = ToolRegistry::with_issue_backend(PathBuf::from("/tmp"), None);
        let names: Vec<String> = reg.schemas().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names.len(), 17, "unexpected tool count: {names:?}");
        for absent in [
            "create_issue",
            "update_issue",
            "close_issue",
            "search_issues",
            "start_work_on_issue",
        ] {
            assert!(
                !names.contains(&absent.to_string()),
                "{absent} must be absent"
            );
        }
    }

    #[test]
    fn video_tools_register_only_with_a_video_capable_media_backend() {
        // A provider that satisfies the port but is never called — tool
        // registration is what's under test here, not generation.
        struct NullMedia;
        #[async_trait]
        impl stella_media::MediaProvider for NullMedia {
            fn id(&self) -> &str {
                "null"
            }
            fn capabilities(&self) -> stella_media::MediaCapabilities {
                stella_media::MediaCapabilities::default()
            }
            async fn generate_image(
                &self,
                _req: stella_media::ImageRequest,
            ) -> Result<stella_media::MediaArtifact, stella_media::MediaError> {
                Err(stella_media::MediaError::Terminal("not under test".into()))
            }
            async fn generate_video(
                &self,
                _req: stella_media::VideoRequest,
            ) -> Result<stella_media::MediaJob, stella_media::MediaError> {
                Err(stella_media::MediaError::Terminal("not under test".into()))
            }
            async fn poll_video(
                &self,
                _job: &stella_media::MediaJob,
            ) -> Result<stella_media::MediaJobStatus, stella_media::MediaError> {
                Err(stella_media::MediaError::Terminal("not under test".into()))
            }
        }
        let provider: Arc<dyn stella_media::MediaProvider> = Arc::new(NullMedia);
        let names = |backend| {
            ToolRegistry::with_backends(PathBuf::from("/tmp"), None, Some(backend))
                .schemas()
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        };

        let with_video = names(crate::media::MediaBackend {
            image: provider.clone(),
            video: Some(provider.clone()),
        });
        for expected in ["generate_image", "generate_video", "poll_video"] {
            assert!(
                with_video.contains(&expected.to_string()),
                "missing {expected}: {with_video:?}"
            );
        }

        let image_only = names(crate::media::MediaBackend {
            image: provider,
            video: None,
        });
        assert!(image_only.contains(&"generate_image".to_string()));
        for absent in ["generate_video", "poll_video"] {
            assert!(
                !image_only.contains(&absent.to_string()),
                "{absent} must be absent without a video adapter"
            );
        }
    }

    #[tokio::test]
    async fn code_graph_tool_is_enabled_after_a_mid_session_index_build() {
        // A session that starts without a code-graph index doesn't advertise
        // `code_graph`; once `stella init` / `/init` builds the index,
        // `enable_code_graph_if_available` must expose it for the rest of the
        // session (schema advertised AND dispatchable) without a rebuild.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let reg = ToolRegistry::with_issue_backend(root.clone(), None);
        let has_code_graph = |r: &ToolRegistry| r.schemas().iter().any(|s| s.name == "code_graph");

        assert!(!has_code_graph(&reg), "no index → not advertised");
        reg.enable_code_graph_if_available(&root); // no index yet: a no-op
        assert!(!has_code_graph(&reg));

        // Build a minimal index, exactly what `stella init` does.
        std::fs::write(root.join("lib.rs"), "pub fn f() {}\n").unwrap();
        let db = root.join(".stella").join("codegraph.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let graph = stella_graph::CodeGraph::open(&root, &db).unwrap();
        graph.index_all().unwrap();
        graph.shutdown();

        reg.enable_code_graph_if_available(&root);
        assert!(has_code_graph(&reg), "index built → now advertised");
        // And it actually dispatches through the overlay.
        let out = reg
            .execute(
                "code_graph",
                &serde_json::json!({"op": "definitions", "target": "f"}),
            )
            .await;
        assert!(!out.is_error(), "code_graph must dispatch: {out:?}");
    }

    #[test]
    fn read_only_flags_partition_the_registry_correctly() {
        // The engine parallelizes on this flag — a mutating tool marked
        // read-only would race writes; a read-only tool marked mutating
        // just loses concurrency. Pin the partition explicitly.
        let reg =
            ToolRegistry::with_issue_backend(PathBuf::from("/tmp"), Some(IssueBackend::GitHub));
        for schema in reg.schemas() {
            let expected = matches!(
                schema.name.as_str(),
                "read_file" | "grep" | "glob" | "explorations" | "ci_status" | "search_issues"
            );
            assert_eq!(
                schema.read_only, expected,
                "read_only flag wrong for {}",
                schema.name
            );
        }
    }

    #[tokio::test]
    async fn schema_gate_blocks_duplicate_table_on_write() {
        let dir = std::env::temp_dir().join(format!("stella_gate_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        // Populate the schema index with a known table.
        let mut tables = HashSet::new();
        tables.insert("users".to_string());
        reg.update_schema_index(tables, HashSet::new(), HashSet::new());

        // Attempt to write a SQL file that creates `users` again.
        let result = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "migrations/002.sql",
                    "content": "CREATE TABLE users (id INT);\n"
                }),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolOutput::Error { message } => {
                assert!(message.contains("Table `users` already exists"));
                assert!(message.contains("ALTER"));
            }
            _ => panic!("expected error"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_gate_allows_new_table_on_write() {
        let dir = std::env::temp_dir().join(format!("stella_gate_ok_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        let mut tables = HashSet::new();
        tables.insert("users".to_string());
        reg.update_schema_index(tables, HashSet::new(), HashSet::new());

        // Write a SQL file with a genuinely new table.
        let result = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "migrations/003.sql",
                    "content": "CREATE TABLE orders (id INT);\n"
                }),
            )
            .await;
        assert!(!result.is_error(), "new table should pass the gate");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_gate_exempts_rewriting_a_files_own_objects() {
        let dir = std::env::temp_dir().join(format!("stella_gate_own_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("migrations")).unwrap();
        std::fs::write(
            dir.join("migrations/001.sql"),
            "CREATE TABLE users (id INT);\n",
        )
        .unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        let mut tables = HashSet::new();
        tables.insert("users".to_string());
        reg.update_schema_index(tables, HashSet::new(), HashSet::new());

        // Rewriting the file that defines `users` is an in-place change to
        // the existing object, not a duplicate creation.
        let result = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "migrations/001.sql",
                    "content": "CREATE TABLE users (id INT, email TEXT);\n"
                }),
            )
            .await;
        assert!(!result.is_error(), "same-file rewrite must pass the gate");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_gate_grows_the_index_after_a_successful_write() {
        let dir = std::env::temp_dir().join(format!("stella_gate_grow_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        // Empty index: the first migration passes and registers `users`.
        let first = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "migrations/001.sql",
                    "content": "CREATE TABLE users (id INT);\n"
                }),
            )
            .await;
        assert!(!first.is_error());

        // A second file re-creating `users` now conflicts — caught by the
        // in-session index growth, no graph re-index needed.
        let second = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "migrations/002.sql",
                    "content": "CREATE TABLE users (id INT);\n"
                }),
            )
            .await;
        assert!(second.is_error(), "in-session duplicate must be caught");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_gate_checks_edit_file_new_string() {
        let dir = std::env::temp_dir().join(format!("stella_gate_edit_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("migrations")).unwrap();
        std::fs::write(dir.join("migrations/002.sql"), "-- add payments\n").unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        let mut tables = HashSet::new();
        tables.insert("users".to_string());
        reg.update_schema_index(tables, HashSet::new(), HashSet::new());

        // An edit whose replacement introduces a duplicate CREATE is gated
        // exactly like a write.
        let result = reg
            .execute(
                "edit_file",
                &serde_json::json!({
                    "path": "migrations/002.sql",
                    "old_string": "-- add payments",
                    "new_string": "CREATE TABLE users (id INT);"
                }),
            )
            .await;
        assert!(
            result.is_error(),
            "edit introducing a duplicate must be gated"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_gate_allows_non_sql_write() {
        let dir = std::env::temp_dir().join(format!("stella_gate_nosql_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        let mut tables = HashSet::new();
        tables.insert("users".to_string());
        reg.update_schema_index(tables, HashSet::new(), HashSet::new());

        // Writing a Rust file should never trigger the schema gate.
        let result = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "src/main.rs",
                    "content": "fn main() {}\n"
                }),
            )
            .await;
        assert!(!result.is_error());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- file-touch telemetry ----------------------------------------

    /// Fresh registry over a fresh tempdir, no optional backends.
    fn telemetry_fixture() -> (tempfile::TempDir, ToolRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.path().to_path_buf(), None);
        (dir, reg)
    }

    async fn exec_ok(reg: &ToolRegistry, name: &str, input: serde_json::Value) {
        let out = reg.execute(name, &input).await;
        assert!(!out.is_error(), "{name} {input} failed: {out:?}");
    }

    #[tokio::test]
    async fn touch_read_only_file_records_one_r_with_zero_deltas() {
        let (dir, reg) = telemetry_fixture();
        std::fs::write(dir.path().join("notes.txt"), "one\ntwo\n").unwrap();

        exec_ok(&reg, "read_file", serde_json::json!({"path": "notes.txt"})).await;

        let payload = reg.file_touch_telemetry();
        payload.validate().unwrap();
        assert_eq!(payload.files_touched.len(), 1);
        let record = &payload.files_touched[0];
        assert_eq!(record.path, "notes.txt");
        assert_eq!(record.crud_events, vec![FileOp::Read]);
        assert_eq!((record.lines_added, record.lines_removed), (0, 0));
        assert_eq!(record.events.len(), 1);
        assert_eq!(
            (record.events[0].lines_added, record.events[0].lines_removed),
            (0, 0)
        );
    }

    #[tokio::test]
    async fn touch_multiple_reads_are_one_record_with_multiple_events() {
        let (dir, reg) = telemetry_fixture();
        std::fs::write(dir.path().join("notes.txt"), "one\ntwo\n").unwrap();

        for _ in 0..3 {
            exec_ok(&reg, "read_file", serde_json::json!({"path": "notes.txt"})).await;
        }

        let payload = reg.file_touch_telemetry();
        payload.validate().unwrap();
        assert_eq!(payload.files_touched.len(), 1, "one record per file");
        let record = &payload.files_touched[0];
        assert_eq!(record.crud_events, vec![FileOp::Read], "crud deduplicated");
        assert_eq!(record.events.len(), 3, "audit log never deduplicated");
    }

    #[tokio::test]
    async fn touch_create_counts_the_new_files_full_line_count() {
        let (_dir, reg) = telemetry_fixture();

        exec_ok(
            &reg,
            "write_file",
            serde_json::json!({"path": "src/new.rs", "content": "a\nb\nc\n"}),
        )
        .await;

        let payload = reg.file_touch_telemetry();
        payload.validate().unwrap();
        let record = &payload.files_touched[0];
        assert_eq!(record.path, "src/new.rs");
        assert_eq!(record.crud_events, vec![FileOp::Create]);
        assert_eq!((record.lines_added, record.lines_removed), (3, 0));
    }

    #[tokio::test]
    async fn touch_update_counts_the_line_diff() {
        let (dir, reg) = telemetry_fixture();
        std::fs::write(dir.path().join("code.rs"), "a\nb\nc\n").unwrap();

        // edit_file: "b" becomes two lines — +2 −1.
        exec_ok(
            &reg,
            "edit_file",
            serde_json::json!({"path": "code.rs", "old_string": "b", "new_string": "x\ny"}),
        )
        .await;
        // write_file over an existing file is also an update — the diff of
        // pre vs post, not the full content: "a\nx\ny\nc\n" → "a\nc\n" is −2.
        exec_ok(
            &reg,
            "write_file",
            serde_json::json!({"path": "code.rs", "content": "a\nc\n"}),
        )
        .await;

        let payload = reg.file_touch_telemetry();
        payload.validate().unwrap();
        let record = &payload.files_touched[0];
        assert_eq!(record.crud_events, vec![FileOp::Update]);
        assert_eq!(record.events.len(), 2);
        assert_eq!(
            (record.events[0].lines_added, record.events[0].lines_removed),
            (2, 1)
        );
        assert_eq!(
            (record.events[1].lines_added, record.events[1].lines_removed),
            (0, 2)
        );
        assert_eq!((record.lines_added, record.lines_removed), (2, 3));
    }

    #[tokio::test]
    async fn touch_delete_counts_the_full_pre_deletion_line_count() {
        let (dir, reg) = telemetry_fixture();
        std::fs::write(dir.path().join("doomed.txt"), "1\n2\n3\n4\n5\n").unwrap();

        exec_ok(
            &reg,
            "delete_file",
            serde_json::json!({"path": "doomed.txt"}),
        )
        .await;

        let payload = reg.file_touch_telemetry();
        payload.validate().unwrap();
        let record = &payload.files_touched[0];
        assert_eq!(record.crud_events, vec![FileOp::Delete]);
        assert_eq!((record.lines_added, record.lines_removed), (0, 5));
    }

    #[tokio::test]
    async fn touch_read_update_delete_keeps_order_and_dedupes_crud() {
        let (dir, reg) = telemetry_fixture();
        std::fs::write(dir.path().join("f.rs"), "a\nb\nc\n").unwrap();

        exec_ok(
            &reg,
            "read_file",
            serde_json::json!({"path": "f.rs", "reason": "inspect before edit"}),
        )
        .await;
        exec_ok(
            &reg,
            "edit_file",
            serde_json::json!({
                "path": "f.rs", "old_string": "b", "new_string": "x\ny",
                "reason": "split b into x and y"
            }),
        )
        .await;
        exec_ok(
            &reg,
            "delete_file",
            serde_json::json!({"path": "f.rs", "reason": "no longer needed"}),
        )
        .await;

        let payload = reg.file_touch_telemetry();
        payload.validate().unwrap();
        assert_eq!(payload.files_touched.len(), 1);
        let record = &payload.files_touched[0];
        assert_eq!(
            record.crud_events,
            vec![FileOp::Read, FileOp::Update, FileOp::Delete],
            "deduplicated, first-occurrence order"
        );
        let ops: Vec<FileOp> = record.events.iter().map(|e| e.event).collect();
        assert_eq!(ops, vec![FileOp::Read, FileOp::Update, FileOp::Delete]);
        let reasons: Vec<&str> = record.events.iter().map(|e| e.reason.as_str()).collect();
        assert_eq!(
            reasons,
            vec![
                "inspect before edit",
                "split b into x and y",
                "no longer needed"
            ]
        );
        // R: 0/0. U: +2 −1. D: −4 (the post-edit file "a\nx\ny\nc\n").
        assert_eq!((record.lines_added, record.lines_removed), (2, 5));
    }

    #[tokio::test]
    async fn touch_equivalent_path_spellings_aggregate_into_one_record() {
        let (dir, reg) = telemetry_fixture();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        for spelling in [
            "src/main.rs",
            "src/./main.rs",
            "src/../src/main.rs",
            "./src/main.rs",
        ] {
            exec_ok(&reg, "read_file", serde_json::json!({"path": spelling})).await;
        }

        let payload = reg.file_touch_telemetry();
        payload.validate().unwrap();
        assert_eq!(
            payload.files_touched.len(),
            1,
            "equivalent spellings must aggregate: {payload:?}"
        );
        let record = &payload.files_touched[0];
        assert_eq!(record.path, "src/main.rs");
        assert_eq!(record.events.len(), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn touch_concurrent_tools_lose_no_events_and_keep_totals_exact() {
        let (dir, reg) = telemetry_fixture();
        for i in 0..4 {
            std::fs::write(dir.path().join(format!("read{i}.txt")), "x\n").unwrap();
        }
        let reg = std::sync::Arc::new(reg);

        let mut handles = Vec::new();
        // 32 concurrent reads across 4 files (read-only tools are the ones
        // the engine actually parallelizes)...
        for i in 0..32 {
            let reg = reg.clone();
            handles.push(tokio::spawn(async move {
                let input = serde_json::json!({"path": format!("read{}.txt", i % 4)});
                let out = reg.execute("read_file", &input).await;
                assert!(!out.is_error(), "{out:?}");
            }));
        }
        // ...plus 8 concurrent creates of distinct files.
        for i in 0..8 {
            let reg = reg.clone();
            handles.push(tokio::spawn(async move {
                let input = serde_json::json!({
                    "path": format!("new{i}.txt"),
                    "content": "l1\nl2\n"
                });
                let out = reg.execute("write_file", &input).await;
                assert!(!out.is_error(), "{out:?}");
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        let payload = reg.file_touch_telemetry();
        payload.validate().unwrap();
        assert_eq!(payload.files_touched.len(), 12, "4 read + 8 created files");
        let mut read_events = 0;
        let mut created_lines = 0;
        for record in &payload.files_touched {
            if record.path.starts_with("read") {
                assert_eq!(record.crud_events, vec![FileOp::Read]);
                read_events += record.events.len();
            } else {
                assert_eq!(record.crud_events, vec![FileOp::Create]);
                assert_eq!(record.events.len(), 1);
                created_lines += record.lines_added;
            }
        }
        assert_eq!(read_events, 32, "no read event may be lost");
        assert_eq!(created_lines, 16, "8 files × 2 lines");
    }

    #[tokio::test]
    async fn touch_failed_operations_record_nothing() {
        let (dir, reg) = telemetry_fixture();
        std::fs::write(dir.path().join("f.rs"), "a\n").unwrap();

        // Missing file, escaping path, and a failed edit precondition.
        for (tool, input) in [
            ("read_file", serde_json::json!({"path": "ghost.txt"})),
            ("read_file", serde_json::json!({"path": "../../etc/passwd"})),
            ("delete_file", serde_json::json!({"path": "ghost.txt"})),
            (
                "edit_file",
                serde_json::json!({"path": "f.rs", "old_string": "nope", "new_string": "x"}),
            ),
        ] {
            let out = reg.execute(tool, &input).await;
            assert!(out.is_error(), "{tool} {input} should fail");
        }

        assert!(
            reg.file_touch_telemetry().files_touched.is_empty(),
            "failed operations must not create CRUD events"
        );
    }

    #[tokio::test]
    async fn touch_reason_defaults_when_omitted() {
        let (dir, reg) = telemetry_fixture();
        std::fs::write(dir.path().join("f.rs"), "a\n").unwrap();

        exec_ok(&reg, "read_file", serde_json::json!({"path": "f.rs"})).await;

        let payload = reg.file_touch_telemetry();
        payload.validate().unwrap();
        let event = &payload.files_touched[0].events[0];
        assert!(
            !event.reason.is_empty(),
            "schema requires a non-empty reason"
        );
    }

    #[tokio::test]
    async fn files_touched_compact_view_still_uses_crud_display_order() {
        // The legacy (path, "RU") view feeds the TUI panel and episodic
        // memory: letters stay in C,R,U,D display order even though the
        // telemetry payload orders by first occurrence.
        let (dir, reg) = telemetry_fixture();
        std::fs::write(dir.path().join("f.rs"), "a\nb\n").unwrap();

        exec_ok(
            &reg,
            "edit_file",
            serde_json::json!({"path": "f.rs", "old_string": "a", "new_string": "z"}),
        )
        .await;
        exec_ok(&reg, "read_file", serde_json::json!({"path": "f.rs"})).await;

        assert_eq!(
            reg.files_touched(),
            vec![("f.rs".to_string(), "RU".to_string())]
        );
        assert_eq!(
            reg.file_touch_telemetry().files_touched[0].crud_events,
            vec![FileOp::Update, FileOp::Read],
            "payload keeps first-occurrence order"
        );
    }

    // ---- extension hook bus integration -------------------------------

    use std::sync::Arc as StdArc;
    use std::sync::Mutex as StdMutex;
    use stella_core::bus::{HookBus, HookDecision, HookEvent, names as hook_names};

    /// Capture every event a `"*"` observer sees on `bus`.
    fn record_bus_events(bus: &HookBus) -> StdArc<StdMutex<Vec<HookEvent>>> {
        let seen = StdArc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        bus.on("*", move |event| {
            sink.lock().unwrap().push(event.clone());
            Ok(())
        })
        .detach();
        seen
    }

    fn event_names(seen: &StdArc<StdMutex<Vec<HookEvent>>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|e| e.name.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_deny_policy_blocks_the_tool_and_leaves_no_touch() {
        let (dir, reg) = telemetry_fixture();
        std::fs::write(dir.path().join("f.rs"), "keep me\n").unwrap();
        let bus = HookBus::new("sess");
        bus.on_blocking(hook_names::TOOL_CALL_REQUESTED, |_| HookDecision::Deny {
            reason: "workspace is read-only".into(),
        })
        .detach();
        reg.attach_bus(bus);

        // The delete is refused by the policy; the file survives and the
        // ledger records nothing.
        let out = reg
            .execute("delete_file", &serde_json::json!({"path": "f.rs"}))
            .await;
        assert!(out.is_error(), "deny must surface as a tool error");
        match out {
            ToolOutput::Error { message } => assert!(message.contains("read-only"), "{message}"),
            _ => unreachable!(),
        }
        assert!(
            dir.path().join("f.rs").exists(),
            "denied delete must not run"
        );
        assert!(
            reg.file_touch_telemetry().files_touched.is_empty(),
            "a blocked op records no file touch"
        );
    }

    #[tokio::test]
    async fn a_successful_write_emits_the_file_and_files_touched_events() {
        let (_dir, reg) = telemetry_fixture();
        let bus = HookBus::new("sess");
        let seen = record_bus_events(&bus);
        reg.attach_bus(bus);

        exec_ok(
            &reg,
            "write_file",
            serde_json::json!({"path": "src/new.rs", "content": "a\nb\nc\n", "reason": "scaffold"}),
        )
        .await;

        let names = event_names(&seen);
        // The OBSERVABLE lifecycle for a create. The raw `tool.call.requested`
        // / `file.created` blocking events are delivered only to blocking
        // (privileged) handlers, never to observers — observers see their
        // safe outcomes (`policy.*`), the sanitized `tool.call.started`, the
        // completion, and the `file.created` FACT + aggregate update.
        for expected in [
            hook_names::POLICY_ALLOWED,
            hook_names::TOOL_CALL_STARTED,
            hook_names::TOOL_CALL_COMPLETED,
            hook_names::FILE_CREATED,
            hook_names::FILES_TOUCHED_UPDATED,
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing {expected} in {names:?}"
            );
        }
        // The raw blocking event never reaches an observer.
        assert!(
            !names.iter().any(|n| n == hook_names::TOOL_CALL_REQUESTED),
            "raw tool.call.requested must not reach observers: {names:?}"
        );
        // The file.created fact carries the line delta but never the content.
        let events = seen.lock().unwrap();
        let created = events
            .iter()
            .find(|e| e.name == hook_names::FILE_CREATED)
            .unwrap();
        assert_eq!(created.payload["path"], "src/new.rs");
        assert_eq!(created.payload["lines_added"], 3);
        let started = events
            .iter()
            .find(|e| e.name == hook_names::TOOL_CALL_STARTED)
            .unwrap();
        assert!(
            started.payload["input"]["content"]
                .as_str()
                .unwrap()
                .starts_with("<omitted:"),
            "tool.call.started must not leak file content"
        );
    }

    #[tokio::test]
    async fn a_modify_policy_rewrites_the_input_the_tool_runs() {
        let (dir, reg) = telemetry_fixture();
        let bus = HookBus::new("sess");
        bus.on_blocking(hook_names::TOOL_CALL_REQUESTED, |event| {
            // Redirect any write to a quarantine path.
            let mut input = event.payload["input"].clone();
            input["path"] = serde_json::Value::String("quarantine.txt".into());
            let mut payload = event.payload.clone();
            payload["input"] = input;
            HookDecision::Modify { payload }
        })
        .detach();
        reg.attach_bus(bus);

        exec_ok(
            &reg,
            "write_file",
            serde_json::json!({"path": "original.txt", "content": "x\n"}),
        )
        .await;

        assert!(
            dir.path().join("quarantine.txt").exists(),
            "the modified path is what actually got written"
        );
        assert!(!dir.path().join("original.txt").exists());
        assert_eq!(
            reg.file_touch_telemetry().files_touched[0].path,
            "quarantine.txt",
            "the ledger records the path the tool actually touched"
        );
    }

    #[tokio::test]
    async fn cite_memory_dispatches_through_the_registry_and_drains_once() {
        let (_dir, reg) = telemetry_fixture();
        exec_ok(
            &reg,
            "cite_memory",
            serde_json::json!({
                "memory_id": "nod_0123456789abcdef01234567",
                "useful_score": 5,
                "truthful": true,
                "remark": "named the failing module outright",
            }),
        )
        .await;
        let drained = reg.take_memory_citations();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].memory_id, "nod_0123456789abcdef01234567");
        assert!(
            reg.take_memory_citations().is_empty(),
            "a drained citation must never persist under a second execution"
        );
    }

    #[tokio::test]
    async fn a_denied_command_never_runs_and_a_bus_less_registry_is_unchanged() {
        let (_dir, reg) = telemetry_fixture();
        let bus = HookBus::new("sess");
        bus.on_blocking(hook_names::COMMAND_STARTED, |_| HookDecision::Deny {
            reason: "no shell".into(),
        })
        .detach();
        reg.attach_bus(bus);
        let out = reg
            .execute("bash", &serde_json::json!({"command": "echo hi"}))
            .await;
        assert!(out.is_error());

        // A registry with no bus attached behaves exactly as before hooks.
        let (dir2, plain) = telemetry_fixture();
        std::fs::write(dir2.path().join("f.txt"), "hi\n").unwrap();
        exec_ok(&plain, "read_file", serde_json::json!({"path": "f.txt"})).await;
        assert_eq!(plain.file_touch_telemetry().files_touched.len(), 1);
    }
}
