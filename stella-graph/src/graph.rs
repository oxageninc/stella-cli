//! [`CodeGraph`] — the public handle: mount a workspace, index it, and query
//! it for [`ContextFrame`]s.
//!
//! # Warm at mount (L-C1)
//!
//! [`CodeGraph::mount`] opens the store synchronously (so queries are
//! immediately callable) and kicks the incremental catch-up + the live
//! watcher as a **background** task — building the graph lazily on first query
//! added seconds to the first real prompt; warming at mount hides the cost in
//! startup slack.
//!
//! # Two connections, one WAL file
//!
//! Indexing (writes) and queries (reads) use separate SQLite connections to
//! the same WAL file, so a long catch-up transaction never blocks a query on
//! a mutex. During a catch-up the reader sees the last committed snapshot
//! (best-effort, non-blocking) until the batch commits — the L-C1 discipline
//! of never adding latency to a query.
//!
//! # Signal safety (L-L1)
//!
//! There are no `unwrap`/`panic` in the hot path; a poisoned connection mutex
//! is recovered rather than propagated. Because every index batch is one
//! transaction, an abrupt process kill mid-index commits nothing and reopening
//! finds a consistent store.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use contextgraph_types::{ContextFrame, ContextQuery};
use notify::RecommendedWatcher;
use rusqlite::Connection;
use tokio::task::JoinHandle;

use crate::error::GraphError;
use crate::frames;
use crate::import;
use crate::parse::Grammars;
use crate::store::{self, IndexStats};
use crate::watch;

/// Shared interior of a [`CodeGraph`], reference-counted so the background
/// catch-up task and watcher can hold it independently of the public handle.
pub(crate) struct Inner {
    pub(crate) root: PathBuf,
    pub(crate) grammars: Grammars,
    write_conn: Mutex<Connection>,
    read_conn: Mutex<Connection>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    pub(crate) shutdown: AtomicBool,
}

impl Inner {
    fn write_guard(&self) -> MutexGuard<'_, Connection> {
        self.write_conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn read_guard(&self) -> MutexGuard<'_, Connection> {
        self.read_conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Apply a watcher-detected set of changes in one transaction. Called from
    /// the debounce loop on the blocking pool.
    pub(crate) fn apply_changes_blocking(
        &self,
        changed: &[PathBuf],
    ) -> Result<IndexStats, GraphError> {
        let mut conn = self.write_guard();
        store::apply_changes(&mut conn, &self.root, &self.grammars, changed)
    }

    pub(crate) fn push_task(&self, handle: JoinHandle<()>) {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(handle);
    }

    /// Install the live watcher — but only if `shutdown()` has not already
    /// run. The flag check and the store happen under the **same** watcher
    /// lock that `shutdown()` takes to set the flag and clear the slot, so the
    /// install serializes against shutdown and the mount-vs-shutdown TOCTOU
    /// window is closed: a watcher created by the background task after a
    /// concurrent `shutdown()` is dropped here (at end of scope) instead of
    /// being stored, so it can never leak past shutdown.
    fn set_watcher(&self, watcher: RecommendedWatcher) {
        let mut slot = self
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.shutdown.load(Ordering::Relaxed) {
            // `watcher` is dropped at end of scope → stops watching at once.
            return;
        }
        *slot = Some(watcher);
    }
}

/// A mounted code graph over a workspace root, backed by a SQLite store.
pub struct CodeGraph {
    inner: std::sync::Arc<Inner>,
}

/// One symbol in a [`FileNeighborhood`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NeighborhoodSymbol {
    pub name: String,
    /// The stored kind tag as persisted by the index (`"function"`,
    /// `"struct"`, `"class"`, `"table"`, …) — see [`crate::SymbolKind::tag`].
    /// An owned `String` so this public type round-trips through serde without
    /// a borrow lifetime.
    pub kind: String,
    pub start_line: u32,
}

/// One definition site of a named symbol with its exact source span — the
/// raw `(path, start..=end)` location behind [`CodeGraph::definitions`]'
/// rendered frames. Lines are 1-based and inclusive, matching
/// [`crate::Symbol`]; `kind` is the human citation keyword (`"fn"`,
/// `"struct"`, …) so callers can label a site the way a frame's citation
/// does (L-C4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SymbolSpan {
    /// Root-relative forward-slash path of the defining file.
    pub path: String,
    pub name: String,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// The structured neighborhood of one file: its symbols and its import
/// edges in both directions. Root-relative forward-slash paths throughout.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileNeighborhood {
    pub file: String,
    pub symbols: Vec<NeighborhoodSymbol>,
    /// What this file imports: resolved paths where resolution succeeded,
    /// raw specifiers (e.g. Rust `use` paths) otherwise.
    pub imports: Vec<String>,
    /// Files whose imports resolve to this file.
    pub importers: Vec<String>,
}

impl CodeGraph {
    /// Open (or create) the store at `db_path` for the workspace at `root`,
    /// **without** starting background work. Use this for one-shot indexing,
    /// tests, or any caller that drives [`CodeGraph::index_all`] itself.
    ///
    /// `root` must exist; it is canonicalized so the workspace-root jail and
    /// relative-path computation are consistent.
    pub fn open(root: &Path, db_path: &Path) -> Result<CodeGraph, GraphError> {
        let root = root.canonicalize().map_err(|source| GraphError::Root {
            root: root.to_path_buf(),
            source,
        })?;
        let write_conn = store::open(db_path)?;
        let read_conn = store::open(db_path)?;
        let grammars = Grammars::load()?;

        let inner = Inner {
            root,
            grammars,
            write_conn: Mutex::new(write_conn),
            read_conn: Mutex::new(read_conn),
            watcher: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
        };
        Ok(CodeGraph {
            inner: std::sync::Arc::new(inner),
        })
    }

    /// Open the store and kick incremental catch-up + the live watcher in the
    /// background (L-C1). Returns as soon as the store is open; the graph
    /// fills in behind the handle.
    pub async fn mount(root: &Path, db_path: &Path) -> Result<CodeGraph, GraphError> {
        let graph = CodeGraph::open(root, db_path)?;
        let inner = graph.inner.clone();

        let handle = tokio::spawn(async move {
            // 1) Catch-up: diff stored hashes against the current tree.
            let catchup = inner.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let mut conn = catchup.write_guard();
                store::index_tree(&mut conn, &catchup.root, &catchup.grammars)
            })
            .await;

            if inner.shutdown.load(Ordering::Relaxed) {
                return;
            }
            // 2) Arm the live watcher. If it cannot be created, catch-up has
            // already run; live updates simply degrade to manual re-index.
            if let Ok(watcher) = watch::spawn(inner.clone(), watch::DEBOUNCE) {
                inner.set_watcher(watcher);
            }
        });
        graph.inner.push_task(handle);
        Ok(graph)
    }

    /// The canonicalized workspace root.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Test seam: start the live-index pipeline (debounce → transactional
    /// apply) **without** an OS filesystem watcher, returning a
    /// [`crate::WatchInjector`] that feeds synthetic events straight into the
    /// pipeline channel. Deterministic live-index tests write real files into
    /// the workspace, inject the paths, and await the injector's applied-batch
    /// signal — no dependence on OS event delivery timing.
    ///
    /// Hidden from docs: test-facing only, `pub` so integration tests in
    /// `tests/` can reach it. Must be called from within a tokio runtime.
    #[doc(hidden)]
    pub fn watch_pipeline_for_tests(&self, debounce: std::time::Duration) -> watch::WatchInjector {
        watch::spawn_injectable(self.inner.clone(), debounce)
    }

    /// Run a full incremental index pass now (walk, re-parse only changed
    /// files, prune deleted). One transaction (L-L1). Synchronous — callers
    /// in an async context should wrap it in `spawn_blocking`.
    pub fn index_all(&self) -> Result<IndexStats, GraphError> {
        let mut conn = self.inner.write_guard();
        store::index_tree(&mut conn, &self.inner.root, &self.inner.grammars)
    }

    /// Number of files currently in the index.
    pub fn file_count(&self) -> Result<usize, GraphError> {
        store::file_count(&self.inner.read_guard())
    }

    /// Total symbols across the whole index (the graph total, not a per-pass
    /// delta). Used by the startup summary line.
    pub fn symbol_count(&self) -> Result<usize, GraphError> {
        store::symbol_count(&self.inner.read_guard())
    }

    /// Total import edges across the whole index.
    pub fn import_count(&self) -> Result<usize, GraphError> {
        store::import_count(&self.inner.read_guard())
    }

    /// Frames for every definition of `name`.
    pub fn definitions(&self, name: &str) -> Result<Vec<ContextFrame>, GraphError> {
        frames::definitions(&self.inner.read_guard(), &self.inner.root, name)
    }

    /// Every definition site of `name` with its exact source span — the
    /// lookup behind the `read_symbol` tool, which needs the faithful
    /// `(path, start..=end)` range to read (a definition frame renders a
    /// truncated snippet, not an editable span).
    pub fn definition_spans(&self, name: &str) -> Result<Vec<SymbolSpan>, GraphError> {
        Ok(store::definitions(&self.inner.read_guard(), name)?
            .into_iter()
            .map(|row| SymbolSpan {
                path: row.path,
                name: row.name,
                kind: row.kind.keyword().to_string(),
                start_line: row.start_line,
                end_line: row.end_line,
            })
            .collect())
    }

    /// Best-effort textual reference frames for `name`.
    pub fn references(&self, name: &str) -> Result<Vec<ContextFrame>, GraphError> {
        frames::references(&self.inner.read_guard(), &self.inner.root, name)
    }

    /// A frame describing the imports out of `file`.
    pub fn imports_of(&self, file: &Path) -> Result<Vec<ContextFrame>, GraphError> {
        let rel = self.resolve_rel(file);
        frames::imports_of(&self.inner.read_guard(), &self.inner.root, &rel)
    }

    /// A frame describing the files that import `file`.
    pub fn importers_of(&self, file: &Path) -> Result<Vec<ContextFrame>, GraphError> {
        let rel = self.resolve_rel(file);
        frames::importers_of(&self.inner.read_guard(), &self.inner.root, &rel)
    }

    /// The immediate graph neighborhood of `file` (its symbols + edges).
    pub fn neighbors(&self, file: &Path) -> Result<Vec<ContextFrame>, GraphError> {
        let rel = self.resolve_rel(file);
        frames::neighbors(&self.inner.read_guard(), &self.inner.root, &rel)
    }

    /// The CGP-provider query entrypoint: budgeted, provenance-carrying frames
    ///, in-process shape. Consumed at runtime
    /// by the CLI's CGP host (`stella-cli/src/contextgraph.rs`, `GraphProvider`), which
    /// fans recall out to this alongside the memory store on every turn.
    pub fn query(&self, q: &ContextQuery) -> Result<Vec<ContextFrame>, GraphError> {
        frames::query(&self.inner.read_guard(), &self.inner.root, q)
    }

    /// The best-connected file in the index (most symbols + import edges) —
    /// a UI's default focus when the caller hasn't picked a file. `None` on
    /// an empty index.
    pub fn busiest_file(&self) -> Result<Option<String>, GraphError> {
        store::busiest_file(&self.inner.read_guard())
    }

    /// Up to `limit` files nothing imports — binaries, scripts, tests, dead
    /// code: exactly the set worth reading first when orienting in an
    /// unfamiliar tree. Computed as one SQL anti-join, so the cost is
    /// independent of index size — callers need no file-count cap, unlike
    /// the per-file [`importers_of`] scan this replaces. Shallowest path
    /// first, then lexicographic; empty on an empty index.
    ///
    /// [`importers_of`]: CodeGraph::importers_of
    pub fn entry_points(&self, limit: usize) -> Result<Vec<String>, GraphError> {
        store::entry_points(&self.inner.read_guard(), limit)
    }

    /// Every indexed file path (root-relative, forward-slash), sorted. The
    /// deck's Graph tab lists these in its file picker so a user can re-root
    /// the neighborhood on any file, not only the [`busiest_file`] default.
    /// Empty on an empty index.
    ///
    /// [`busiest_file`]: CodeGraph::busiest_file
    pub fn all_files(&self) -> Result<Vec<String>, GraphError> {
        store::all_files(&self.inner.read_guard())
    }

    /// The structured neighborhood of `file` — its symbols and import edges
    /// in both directions — for UI consumers (the deck's Graph tab). The
    /// frame methods above render prose for the model; this keeps the shape.
    pub fn file_neighborhood(&self, file: &Path) -> Result<FileNeighborhood, GraphError> {
        let rel = self.resolve_rel(file);
        let conn = self.inner.read_guard();
        let symbols = store::symbols_in_file(&conn, &rel)?
            .into_iter()
            .map(|row| NeighborhoodSymbol {
                name: row.name,
                kind: row.kind.tag().to_string(),
                start_line: row.start_line,
            })
            .collect();
        let imports = store::imports_from(&conn, &rel)?
            .into_iter()
            .map(|row| row.to_path.unwrap_or(row.specifier))
            .collect();
        let importers = store::importers_of(&conn, &rel)?;
        Ok(FileNeighborhood {
            file: rel,
            symbols,
            imports,
            importers,
        })
    }

    /// The assembled storage map: parsed structure from the index merged
    /// with `stella.storage.toml` meaning (spec §6). Best-effort — an
    /// unreadable store or malformed manifest yields whatever half works,
    /// never an error, matching [`CodeGraph::schema_names`]' posture.
    pub fn storage_snapshot(&self) -> crate::storage::StorageSnapshot {
        let rows = store::storage_rows(&self.inner.read_guard()).unwrap_or_default();
        let manifest = crate::manifest::StorageManifest::load(&self.inner.root)
            .ok()
            .flatten();
        crate::manifest::merge_snapshot(rows, manifest.as_ref())
    }

    /// All known table, type, and view names (lowercased) from the index.
    /// Used by the schema gate to populate the known-schema set at session
    /// start. Returns empty sets if the index is empty or unreadable.
    pub fn schema_names(&self) -> (HashSet<String>, HashSet<String>, HashSet<String>) {
        let conn = self.inner.read_guard();
        let tables = store::names_of_kind(&conn, "table").unwrap_or_default();
        let types = store::names_of_kind(&conn, "schema_enum").unwrap_or_default();
        let views = store::names_of_kind(&conn, "view").unwrap_or_default();
        (tables, types, views)
    }

    /// Stop the watcher and background tasks. Idempotent. Dropping the watcher
    /// closes the event channel, so the debounce loop exits on its own; task
    /// handles are aborted as a backstop.
    pub fn shutdown(&self) {
        // Set the shutdown flag *and* clear the watcher slot under one hold of
        // the watcher lock. `set_watcher` re-checks the flag under this same
        // lock, so install and shutdown serialize: a watcher installed
        // concurrently by the mount background task is either cleared here (if
        // it stored first) or dropped by `set_watcher` (if it runs after us).
        // Invariant: after this returns, no watcher can be (re)installed.
        {
            let mut watcher = self
                .inner
                .watcher
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.inner.shutdown.store(true, Ordering::Relaxed);
            *watcher = None;
        }
        let handles: Vec<JoinHandle<()>> = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect();
        for handle in handles {
            handle.abort();
        }
    }

    /// Resolve a caller-supplied path (absolute or already root-relative) to a
    /// forward-slash path relative to the workspace root.
    fn resolve_rel(&self, path: &Path) -> String {
        let root = &self.inner.root;
        if let Ok(rel) = path.strip_prefix(root) {
            return import::rel_to_slash(rel);
        }
        if let Ok(canonical) = path.canonicalize()
            && let Ok(rel) = canonical.strip_prefix(root)
        {
            return import::rel_to_slash(rel);
        }
        import::rel_to_slash(path)
    }
}

impl Drop for CodeGraph {
    fn drop(&mut self) {
        // Best-effort teardown if the caller never called `shutdown`.
        // Unconditional: `CodeGraph` is not `Clone`, so this drop IS the last
        // public handle going away — the remaining `Arc` holders are the
        // background catch-up task and debounce loop, which hold clones for
        // their whole lives. Gating on strong-count == 1 (the previous
        // behavior) could therefore never fire after a successful `mount`,
        // leaking the OS watcher and both loops until process exit.
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A parked `notify` watcher, not armed on any path — enough to exercise
    /// the install path without depending on OS event delivery.
    fn make_watcher() -> RecommendedWatcher {
        notify::recommended_watcher(|_res: notify::Result<notify::Event>| {}).unwrap()
    }

    fn open_graph() -> (CodeGraph, TempDir, TempDir) {
        let ws = TempDir::new().unwrap();
        let dbdir = TempDir::new().unwrap();
        let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();
        (graph, ws, dbdir)
    }

    fn watcher_is_installed(graph: &CodeGraph) -> bool {
        graph
            .inner
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Reproduces the mount-vs-shutdown race deterministically: `shutdown()`
    /// runs first (flag set, slot cleared), then the racing mount task — having
    /// already passed its pre-install shutdown check — tries to install its
    /// watcher. The guarded `set_watcher` must drop it, not store it, so the
    /// OS watcher cannot leak past shutdown.
    #[test]
    fn set_watcher_after_shutdown_drops_the_watcher() {
        let (graph, _ws, _dbdir) = open_graph();

        graph.shutdown();
        graph.inner.set_watcher(make_watcher());

        assert!(
            !watcher_is_installed(&graph),
            "a watcher installed after shutdown must be dropped, not retained"
        );
    }

    /// Control: with no shutdown, the normal install path stores the watcher.
    #[test]
    fn set_watcher_before_shutdown_stores_the_watcher() {
        let (graph, _ws, _dbdir) = open_graph();

        graph.inner.set_watcher(make_watcher());
        assert!(
            watcher_is_installed(&graph),
            "a watcher installed before shutdown must be retained"
        );

        // And a subsequent shutdown clears it back out.
        graph.shutdown();
        assert!(
            !watcher_is_installed(&graph),
            "shutdown must clear an already-installed watcher"
        );
    }

    /// `all_files` surfaces every indexed file (root-relative, sorted) so the
    /// deck's Graph tab can list them — the file picker's data source.
    #[test]
    fn all_files_lists_every_indexed_file_sorted() {
        let ws = TempDir::new().unwrap();
        let dbdir = TempDir::new().unwrap();
        std::fs::write(ws.path().join("zeta.rs"), "pub fn z() {}\n").unwrap();
        std::fs::write(ws.path().join("alpha.rs"), "pub fn a() {}\n").unwrap();
        let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();
        graph.index_all().unwrap();

        assert_eq!(
            graph.all_files().unwrap(),
            vec!["alpha.rs".to_string(), "zeta.rs".to_string()],
            "every indexed file, root-relative and sorted"
        );
    }

    #[test]
    fn all_files_is_empty_on_an_empty_index() {
        let (graph, _ws, _dbdir) = open_graph();
        assert!(graph.all_files().unwrap().is_empty());
    }

    /// `definition_spans` reports each site's exact 1-based inclusive range —
    /// the raw span `read_symbol` reads, not a rendered/truncated snippet.
    #[test]
    fn definition_spans_carry_the_exact_source_range() {
        let ws = TempDir::new().unwrap();
        let dbdir = TempDir::new().unwrap();
        std::fs::write(
            ws.path().join("lib.rs"),
            "fn alpha() {}\n\nfn target() {\n    let x = 1;\n    let y = 2;\n}\n",
        )
        .unwrap();
        let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();
        graph.index_all().unwrap();

        let spans = graph.definition_spans("target").unwrap();
        assert_eq!(spans.len(), 1, "{spans:?}");
        let span = &spans[0];
        assert_eq!(span.path, "lib.rs");
        assert_eq!(span.name, "target");
        assert_eq!(span.kind, "fn");
        assert_eq!(
            (span.start_line, span.end_line),
            (3, 6),
            "1-based inclusive span of the whole definition"
        );
        assert!(graph.definition_spans("no_such_symbol").unwrap().is_empty());
    }
}
