//! Error type for the code-graph indexer.
//!
//! Per `02-architecture.md` §1.5 ("Fail loud, recover gracefully") every
//! fallible boundary returns a typed `thiserror` error rather than panicking.
//! The one hot-path subtlety this crate adds (`09-lessons-learned.md` quality
//! bar): a tree-sitter *parse* failure on an arbitrary file is **not** a
//! `GraphError` — it is skipped-with-record inside the indexer so one
//! unparseable file never aborts a whole index batch (L-L1). `GraphError` is
//! reserved for genuine infrastructure faults (SQLite, I/O, a malformed
//! built-in query — all of which are programmer/environment errors, not
//! untrusted input).

use std::path::PathBuf;

/// Anything that can go wrong opening, indexing, or querying the code graph.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// A SQLite operation failed (open, migrate, transaction, query).
    #[error("code-graph store error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Filesystem I/O failed with the path that faulted attached for
    /// debugging (`09-lessons-learned.md` L-T8: a path turns "cannot
    /// reproduce" into a fix).
    #[error("code-graph i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Loading a bundled tree-sitter grammar into a `Parser` failed — a
    /// build/ABI fault, never user input.
    #[error("failed to load the {lang} grammar: {message}")]
    Grammar { lang: &'static str, message: String },

    /// One of the crate's own compile-time `.scm` queries
    /// (`09-lessons-learned.md` L-L2) failed to compile against its grammar.
    /// This is a programmer error caught by the crate's own tests, surfaced
    /// as an error rather than a panic so a mis-edit degrades loudly instead
    /// of aborting a host process.
    #[error("failed to compile the {lang} {kind} query: {message}")]
    Query {
        lang: &'static str,
        kind: &'static str,
        message: String,
    },

    /// The workspace root could not be canonicalized (does not exist, or a
    /// permissions fault) — mounting against a missing root is a caller bug.
    #[error("workspace root {root} is not accessible: {source}")]
    Root {
        root: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A filesystem watcher could not be created or armed (`notify`).
    #[error("code-graph watcher error: {0}")]
    Watch(String),
}
