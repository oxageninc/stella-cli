//! `stella-graph` — the code-graph indexer.
//!
//! A tree-sitter symbol + import-edge indexer over a workspace, persisted in
//! SQLite, exposed as a **built-in OCP provider** feeding `stella-context`
//! ("implemented AS a built-in OCP provider … the
//! protocol's first proof of non-triviality"). It depends only on `ocp-types`
//! for the wire shape — never on `stella-context` — so the provider boundary
//! stays one-directional.
//!
//! # What it does
//!
//! - **Indexes** Rust, TypeScript, TSX, JavaScript, Python, and SQL: symbols
//!   (functions, methods, structs/classes/enums/traits/interfaces; for SQL,
//!   tables/views/schema enums — the rows the schema gate reads) and import
//!   edges (`file → module/file`), via compile-time tree-sitter queries
//!   (L-L2).
//! - **Resolves** Python relative imports (`from . import x`,
//!   `from ..pkg import y`) and TS/JS relative specifiers (`./x`, `../y`,
//!   `index.*`) to real files; bare package specifiers are recorded
//!   unresolved (Phase 3 item 3).
//! - **Warms at mount** and re-indexes live via an in-process `notify`
//!   watcher (L-C1).
//! - **Skips byte-identical content** on re-index (L-C2).
//! - **Answers queries as frames** with mandatory human citation labels and
//!   `code-graph` provenance (L-C4).
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use stella_graph::CodeGraph;
//!
//! # fn main() -> Result<(), stella_graph::GraphError> {
//! let graph = CodeGraph::open(Path::new("."), Path::new("./.stella/codegraph.db"))?;
//! graph.index_all()?;
//! for frame in graph.definitions("run_turn")? {
//!     // Cite by the human label, never the raw id (L-C4).
//!     println!("{}", frame.citation_label.as_deref().unwrap_or(&frame.title));
//! }
//! # Ok(())
//! # }
//! ```

mod error;
mod frames;
mod graph;
mod import;
mod lang;
pub mod manifest;
mod parse;
mod queries;
pub mod storage;
mod store;
mod symbol;
mod walk;
mod watch;

pub use error::GraphError;
pub use frames::PROVIDER_ID;
pub use graph::{CodeGraph, FileNeighborhood, NeighborhoodSymbol};
pub use import::{ImportEdge, ImportKind};
pub use lang::Language;
pub use storage::{StorageExtract, StorageExtractor, StorageSnapshot};
pub use store::IndexStats;
pub use symbol::{Symbol, SymbolKind};
#[doc(hidden)]
pub use watch::WatchInjector;

/// Assemble the storage map without mounting a graph: read the persisted
/// index (if `stella init` has built one) and merge `stella.storage.toml`.
/// The pre-write gate and `stella storage` call this per use so the map is
/// always current — no session-start cache to go stale. Best-effort: no
/// index and no manifest yields an empty snapshot, which makes every
/// storage mechanism a no-op (spec §11).
pub fn load_storage_snapshot(workspace_root: &std::path::Path) -> StorageSnapshot {
    let manifest = manifest::StorageManifest::load(workspace_root)
        .ok()
        .flatten();
    let db_path = workspace_root.join(".stella").join("codegraph.db");
    let rows = if db_path.exists() {
        store::open(&db_path)
            .and_then(|conn| store::storage_rows(&conn))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    manifest::merge_snapshot(rows, manifest.as_ref())
}

// Re-export the OCP wire types this provider produces so downstream callers
// need not also depend on `ocp-types` directly for the return shapes.
pub use ocp_types::{ContextFrame, ContextQuery, FrameKind};
