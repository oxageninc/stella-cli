//! `stella-graph` — the code-graph indexer.
//!
//! A tree-sitter symbol + import-edge indexer over a workspace, persisted in
//! SQLite, exposed as a **built-in OCP provider** feeding `stella-context`
//! (`02-architecture.md` §2: "implemented AS a built-in OCP provider … the
//! protocol's first proof of non-triviality"). It depends only on `ocp-types`
//! for the wire shape — never on `stella-context` — so the provider boundary
//! stays one-directional.
//!
//! # What it does
//!
//! - **Indexes** Rust, TypeScript, TSX, JavaScript, and Python: symbols
//!   (functions, methods, structs/classes/enums/traits/interfaces) and import
//!   edges (`file → module/file`), via compile-time tree-sitter queries
//!   (`09-lessons-learned.md` L-L2).
//! - **Resolves** Python relative imports (`from . import x`,
//!   `from ..pkg import y`) and TS/JS relative specifiers (`./x`, `../y`,
//!   `index.*`) to real files; bare package specifiers are recorded
//!   unresolved (`03-plan.md` Phase 3 item 3).
//! - **Warms at mount** and re-indexes live via an in-process `notify`
//!   watcher (L-C1, `02-architecture.md` §6).
//! - **Skips byte-identical content** on re-index (L-C2).
//! - **Answers queries as frames** with mandatory human citation labels and
//!   `code-graph` provenance (L-C4, `06-context-protocol.md` §3.4).
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use stella_graph::CodeGraph;
//!
//! # fn main() -> Result<(), stella_graph::GraphError> {
//! let graph = CodeGraph::open(Path::new("."), Path::new("./.stella/context.db"))?;
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
mod parse;
mod queries;
mod store;
mod symbol;
mod walk;
mod watch;

pub use error::GraphError;
pub use frames::PROVIDER_ID;
pub use graph::CodeGraph;
pub use import::{ImportEdge, ImportKind};
pub use lang::Language;
pub use store::IndexStats;
pub use symbol::{Symbol, SymbolKind};
#[doc(hidden)]
pub use watch::WatchInjector;

// Re-export the OCP wire types this provider produces so downstream callers
// need not also depend on `ocp-types` directly for the return shapes.
pub use ocp_types::{ContextFrame, ContextQuery, FrameKind};
