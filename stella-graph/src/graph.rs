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

use notify::RecommendedWatcher;
use ocp_types::{ContextFrame, ContextQuery};
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

    fn set_watcher(&self, watcher: RecommendedWatcher) {
        *self
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(watcher);
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
    /// `"struct"`, `"class"`, `"table"`, …) — see [`SymbolKind::tag`]. An
    /// owned `String` so this public type round-trips through serde without a
    /// borrow lifetime.
    pub kind: String,
    pub start_line: u32,
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
    /// relative-path computation are consistent (`02-architecture.md` §8).
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

    /// Frames for every definition of `name`.
    pub fn definitions(&self, name: &str) -> Result<Vec<ContextFrame>, GraphError> {
        frames::definitions(&self.inner.read_guard(), &self.inner.root, name)
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

    /// The OCP-provider query entrypoint: budgeted, provenance-carrying frames
    /// (`06-context-protocol.md` §3.3), in-process shape. Consumed at runtime
    /// by the CLI's OCP host (`stella-cli/src/ocp.rs`, `GraphProvider`), which
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
        self.inner.shutdown.store(true, Ordering::Relaxed);
        *self
            .inner
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
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
