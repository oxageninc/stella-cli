//! Code-graph build cluster split out of `agent.rs` to keep that file a
//! manageable size. Pure relocation — no behavior change.
use super::*;

/// Build the workspace code-graph index into `.stella/private/codegraph.db` (the
/// `stella-graph` tree-sitter indexer). This is the data side of `init`: the
/// domain taxonomy tags graph nodes/edges, and the index makes the symbols +
/// import edges queryable as `ContextFrame`s by the context plane.
///
/// Idempotent and best-effort: a failure degrades to a warning (init still
/// succeeds, offline included) — the graph can always be rebuilt on a later
/// `init` once a toolchain/parser is available. Progress goes to `emit`
/// (plain text, no ANSI) so both the CLI and the deck transcript can show it.
pub(super) async fn build_code_graph(
    workspace_root: &std::path::Path,
    emit: &mut dyn FnMut(String),
) {
    emit("◈ indexing code graph…".to_string());
    // A full-tree tree-sitter index is seconds-to-minutes of blocking file
    // reads + parsing + SQLite on a large repo. Run it on the blocking pool
    // so it never pins a runtime worker — the deck driver awaits `/init`
    // inline and must stay responsive to queue edits and cancels meanwhile
    // (the incremental watcher path already does this, stella-graph
    // watch.rs). `emit` stays on this side of the boundary: the only
    // pre-completion line is the one above.
    let root = workspace_root.to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || index_workspace_graph_blocking(&root)).await;
    match outcome {
        Ok(Ok(stats)) => emit(format_graph_stats(&stats)),
        Ok(Err(warning)) => emit(warning),
        Err(e) => emit(format!(
            "! code-graph indexing task failed: {e} — run `stella init` again to retry"
        )),
    }
}

/// Blocking: create `.stella/`, open the store, run one full incremental index
/// pass (sha-skip makes byte-identical files free, L-C2), and shut down.
/// Returns the stats or a ready-to-emit human warning. Emits nothing itself —
/// so it is `Send` and callable from any spawned task; both `stella init`'s
/// [`build_code_graph`] and the session auto-builder [`spawn_session_graph`]
/// drive it, then narrate the result on their own side of the async boundary.
pub(super) fn index_workspace_graph_blocking(
    workspace_root: &std::path::Path,
) -> Result<GraphSummary, String> {
    let db_path = stella_store::workspace_private_sqlite_path(workspace_root, "codegraph.db")
        .map_err(|e| format!("! could not prepare private code graph state: {e} — skipped"))?;
    let graph = stella_graph::CodeGraph::open(workspace_root, &db_path)
        .map_err(|e| format!("! code-graph store unavailable: {e} — skipped"))?;
    let stats = graph.index_all().map_err(|e| {
        format!("! code-graph indexing failed: {e} — run `stella init` again to retry")
    })?;
    // Report the whole-index TOTALS (what the model can actually query), not
    // this pass's parse delta: an incremental pass over an unchanged tree
    // re-parses nothing, so `stats.symbols`/`stats.imports` are 0 even though
    // the graph is fully populated — the misleading "0 symbols" line users saw.
    let summary = GraphSummary {
        total_symbols: graph.symbol_count().unwrap_or(0),
        total_imports: graph.import_count().unwrap_or(0),
        total_files: graph
            .file_count()
            .unwrap_or(stats.files_parsed + stats.files_skipped_unchanged),
        files_parsed: stats.files_parsed,
        files_unchanged: stats.files_skipped_unchanged,
        files_skipped_generated: stats.files_skipped_generated,
    };
    graph.shutdown();
    Ok(summary)
}

/// Whole-index totals plus this pass's parse/skip split, for the startup line.
pub(super) struct GraphSummary {
    pub(super) total_symbols: usize,
    pub(super) total_imports: usize,
    pub(super) total_files: usize,
    pub(super) files_parsed: usize,
    pub(super) files_unchanged: usize,
    /// Files this pass excluded as generated/minified (issue #272:
    /// `.gitattributes` `linguist-generated=true`, `*.min.*`, or the
    /// minified-content heuristic — see `stella_graph::generated`). Reported
    /// separately from `total_files` so the exclusion is visible, not just
    /// silently absent from the count.
    pub(super) files_skipped_generated: usize,
}

/// The `✓ code graph: N symbols, M imports…` summary line, shared by `stella
/// init` and the session auto-builder so both surfaces read identically.
/// Reports index totals; the parenthetical is this pass's parse/skip split.
/// When this pass excluded any generated/minified files, an explicit
/// "skipped N generated files" clause makes that visible rather than letting
/// them silently vanish from the file count (issue #272).
pub(super) fn format_graph_stats(summary: &GraphSummary) -> String {
    let base = format!(
        "✓ code graph: {} symbols, {} imports across {} file{} ({} re-parsed, {} unchanged this pass)",
        summary.total_symbols,
        summary.total_imports,
        summary.total_files,
        if summary.total_files == 1 { "" } else { "s" },
        summary.files_parsed,
        summary.files_unchanged,
    );
    if summary.files_skipped_generated == 0 {
        return base;
    }
    format!(
        "{base} — skipped {} generated file{}",
        summary.files_skipped_generated,
        if summary.files_skipped_generated == 1 {
            ""
        } else {
            "s"
        }
    )
}

/// A session-lifetime holder for the live code graph. It keeps the in-process
/// `notify` watcher (and its debounce task) alive so file changes — the
/// agent's own edits and external ones — incrementally re-index into
/// `.stella/private/codegraph.db` for the rest of the session. Dropping it (or calling
/// [`SessionGraph::shutdown`]) tears the watcher down cleanly. The mounted
/// graph is installed only once the background build finishes, so an early
/// session exit simply leaves the slot empty (and the never-installed watcher
/// never armed).
pub(crate) struct SessionGraph {
    graph: Arc<std::sync::Mutex<Option<stella_graph::CodeGraph>>>,
}

impl SessionGraph {
    /// Stop the watcher and its background tasks. Idempotent; also runs on drop.
    pub(crate) fn shutdown(&self) {
        if let Some(graph) = self.graph.lock().unwrap_or_else(|p| p.into_inner()).take() {
            graph.shutdown();
        }
    }
}

impl Drop for SessionGraph {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Ensure the workspace code-graph index exists and stays fresh for the life of
/// a session — WITHOUT blocking startup and WITHOUT re-running the full,
/// LLM-driven [`init_workspace`]. This is the *data* side of init only:
///
/// 1. If there is no index yet it is built in the background
///    ([`index_workspace_graph_blocking`], the same index step `stella init`
///    runs); if one already exists it is a cheap incremental catch-up
///    (byte-identical files are skipped, L-C2).
/// 2. The moment the index is ready the `graph_query` tool is enabled for the
///    rest of the session ([`ToolRegistry::enable_code_graph_if_available`])
///    and the schema gate learns any new table/type names — so a session that
///    launched in a repo with no `.stella/private/codegraph.db` gains the tool
///    mid-session, no restart, no manual `stella init`.
/// 3. The live `notify` watcher is then armed via
///    [`stella_graph::CodeGraph::mount`] so subsequent edits incrementally
///    re-index. mount's own catch-up sha-skips everything just indexed in
///    step 1 — the watcher is the point of the second open.
///
/// Non-blocking: returns immediately with a [`SessionGraph`] the caller keeps
/// alive for the session (dropping it stops the watcher) and the setup task's
/// `JoinHandle`, which completes once the tool has been enabled — a
/// deterministic "index ready" signal for tests. `status` receives the same
/// `◈ indexing code graph…` / `✓ …` lines `stella init` prints (route it to
/// stderr or the deck transcript, never to a machine-readable stdout);
/// `on_ready` fires once after the tool is enabled (the deck refreshes its
/// Graph tab there; other callers pass a no-op).
pub(crate) fn spawn_session_graph(
    workspace_root: &std::path::Path,
    registry: Arc<ToolRegistry>,
    mut status: Box<dyn FnMut(String) + Send>,
    on_ready: Box<dyn FnOnce() + Send>,
) -> (SessionGraph, tokio::task::JoinHandle<()>) {
    let slot: Arc<std::sync::Mutex<Option<stella_graph::CodeGraph>>> =
        Arc::new(std::sync::Mutex::new(None));
    let slot_task = slot.clone();
    let root = workspace_root.to_path_buf();
    let handle = tokio::spawn(async move {
        // 1) Build (fresh) or incrementally refresh (existing) the index to
        //    completion, on the blocking pool. `status` (a `Send` box) is
        //    called between awaits, never held across one — so this task stays
        //    `Send`. (We drive the shared blocking helper directly rather than
        //    `build_code_graph`, whose `&mut dyn FnMut` emit is not `Send`.)
        status("◈ indexing code graph…".to_string());
        let build_root = root.clone();
        let outcome =
            tokio::task::spawn_blocking(move || index_workspace_graph_blocking(&build_root)).await;
        match outcome {
            Ok(Ok(stats)) => status(format_graph_stats(&stats)),
            Ok(Err(warning)) => status(warning),
            Err(e) => status(format!(
                "! code-graph indexing task failed: {e} — the graph tool stays off this session"
            )),
        }
        // 2) Expose `graph_query` for the rest of the session and teach the
        //    schema gate any table/type names the fresh index now carries.
        if let Err(error) = registry.enable_code_graph_if_available(&root) {
            status(format!("! graph tool unavailable: {error}"));
        }
        if let Err(error) = populate_schema_index(&registry, &root) {
            status(format!("! schema governance unavailable: {error}"));
        }
        on_ready();
        // 3) Arm the live watcher on a mounted graph kept alive for the
        //    session. Best-effort: a mount failure only loses live refresh, it
        //    never loses the index built in step 1.
        let db_path = stella_tools::graph::graph_db_path(&root);
        match stella_graph::CodeGraph::mount(&root, &db_path).await {
            Ok(graph) => {
                *slot_task.lock().unwrap_or_else(|p| p.into_inner()) = Some(graph);
            }
            Err(e) => status(format!(
                "! code-graph watcher unavailable: {e} — the index will refresh on the next launch"
            )),
        }
    });
    (SessionGraph { graph: slot }, handle)
}
