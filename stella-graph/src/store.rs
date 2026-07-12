//! SQLite storage for the code graph.
//!
//! `02-architecture.md` §6 mandates **one** `context.db` file with one engine:
//! `stella-context` owns the rest of that file's schema, so every table this
//! crate creates is prefixed `code_graph_` to share the file without
//! colliding. The store is opened against a caller-supplied path so the
//! integration pass can point both crates at the same file.
//!
//! Durability contract (`09-lessons-learned.md` L-L1, "a kill during indexing
//! leaves a consistent store"): WAL journal mode, and **every index batch is
//! a single transaction**. A process killed mid-batch has committed nothing;
//! reopening sees the previous consistent state and a re-index completes. The
//! crash-consistency test in this module proves it by dropping a transaction
//! (rusqlite rolls back on drop) and asserting the store is unchanged.
//!
//! Byte-compat skip (`09-lessons-learned.md` L-C2): a file whose content
//! sha256 matches the stored value is never re-parsed. [`IndexStats::files_parsed`]
//! counts real parse invocations, which the skip test asserts drops to zero on
//! an unchanged second pass.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Transaction, params};
use sha2::{Digest, Sha256};

use crate::error::GraphError;
use crate::import::{self, ImportKind};
use crate::lang::Language;
use crate::parse::{Grammars, parse_file};
use crate::symbol::SymbolKind;
use crate::walk::walk_indexable;

/// DDL for the code graph's slice of `context.db`. `IF NOT EXISTS` throughout
/// so opening an existing store (possibly already carrying `stella-context`'s
/// tables) is a no-op.
const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS code_graph_files (
    id             INTEGER PRIMARY KEY,
    path           TEXT NOT NULL UNIQUE,
    language       TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    mtime_ns       INTEGER NOT NULL,
    indexed_at     INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS code_graph_symbols (
    id         INTEGER PRIMARY KEY,
    file_id    INTEGER NOT NULL REFERENCES code_graph_files(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS code_graph_imports (
    id           INTEGER PRIMARY KEY,
    from_file_id INTEGER NOT NULL REFERENCES code_graph_files(id) ON DELETE CASCADE,
    specifier    TEXT NOT NULL,
    to_path      TEXT,
    kind         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS code_graph_symbols_name ON code_graph_symbols(name);
CREATE INDEX IF NOT EXISTS code_graph_symbols_file ON code_graph_symbols(file_id);
CREATE INDEX IF NOT EXISTS code_graph_imports_from ON code_graph_imports(from_file_id);
CREATE INDEX IF NOT EXISTS code_graph_imports_to   ON code_graph_imports(to_path);
"#;

/// Outcome of one index pass. `files_parsed` is the honest parse-invocation
/// count the byte-compat skip test asserts against.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexStats {
    pub files_seen: usize,
    pub files_parsed: usize,
    pub files_skipped_unchanged: usize,
    pub files_skipped_binary: usize,
    pub files_unreadable: usize,
    pub parse_failures: usize,
    pub files_pruned: usize,
    pub symbols: usize,
    pub imports: usize,
}

/// One symbol row joined to its file — the shape every symbol-producing query
/// returns.
#[derive(Debug, Clone)]
pub(crate) struct DefRow {
    pub path: String,
    pub sha: String,
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
}

/// One import edge out of a file.
#[derive(Debug, Clone)]
pub(crate) struct ImportRow {
    pub specifier: String,
    pub to_path: Option<String>,
    pub kind: ImportKind,
}

/// Open (creating if needed) the store at `db_path`, set the per-connection
/// pragmas, and run migrations. Safe to call against a file that already
/// holds another crate's tables.
pub(crate) fn open(db_path: &Path) -> Result<Connection, GraphError> {
    let conn = Connection::open(db_path)?;
    // WAL persists in the file; foreign_keys / busy_timeout are per-connection
    // and must be re-set on every open. NORMAL sync is the standard WAL
    // durability/throughput trade for a rebuildable index.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;\
         PRAGMA foreign_keys=ON;\
         PRAGMA busy_timeout=5000;\
         PRAGMA synchronous=NORMAL;",
    )?;
    conn.execute_batch(MIGRATION)?;
    Ok(conn)
}

/// Full incremental index of `root`: walk, re-parse only changed files
/// (byte-compat skip), and prune rows for files that no longer exist. One
/// transaction (L-L1).
pub(crate) fn index_tree(
    conn: &mut Connection,
    root: &Path,
    grammars: &Grammars,
) -> Result<IndexStats, GraphError> {
    let files = walk_indexable(root);
    let mut stats = IndexStats::default();
    let tx = conn.transaction()?;

    let mut current: HashSet<String> = HashSet::with_capacity(files.len());
    for abs in &files {
        current.insert(rel_path(root, abs));
        index_one(&tx, root, grammars, abs, &mut stats)?;
    }
    stats.files_pruned = prune_missing(&tx, &current)?;

    tx.commit()?;
    Ok(stats)
}

/// Apply a specific set of changed paths (from the watcher): index the ones
/// that still exist and are indexable, drop rows for the ones that vanished.
/// One transaction; no full-tree prune.
pub(crate) fn apply_changes(
    conn: &mut Connection,
    root: &Path,
    grammars: &Grammars,
    changed: &[PathBuf],
) -> Result<IndexStats, GraphError> {
    let mut stats = IndexStats::default();
    let tx = conn.transaction()?;
    for abs in changed {
        if Language::from_path(abs).is_none() {
            continue;
        }
        if abs.is_file() {
            index_one(&tx, root, grammars, abs, &mut stats)?;
        } else {
            let rel = rel_path(root, abs);
            stats.files_pruned +=
                tx.execute("DELETE FROM code_graph_files WHERE path = ?1", params![rel])?;
        }
    }
    tx.commit()?;
    Ok(stats)
}

/// Index one file into an open transaction. Every failure mode here
/// (unreadable, non-UTF-8, unparseable) is recorded in `stats` and skipped —
/// never propagated — so one bad file cannot abort the batch (L-L1).
fn index_one(
    tx: &Transaction,
    root: &Path,
    grammars: &Grammars,
    abs: &Path,
    stats: &mut IndexStats,
) -> Result<(), GraphError> {
    stats.files_seen += 1;
    let rel = rel_path(root, abs);

    let content = match std::fs::read(abs) {
        Ok(bytes) => bytes,
        Err(_) => {
            stats.files_unreadable += 1;
            return Ok(());
        }
    };
    let sha = sha256_hex(&content);

    // Byte-compat skip (L-C2): identical content is never re-parsed.
    if let Some(existing) = file_sha(tx, &rel)?
        && existing == sha
    {
        stats.files_skipped_unchanged += 1;
        return Ok(());
    }

    let text = match std::str::from_utf8(&content) {
        Ok(text) => text,
        Err(_) => {
            stats.files_skipped_binary += 1;
            return Ok(());
        }
    };
    let Some(lang) = Language::from_path(abs) else {
        return Ok(());
    };
    let parsed = match parse_file(grammars, lang, text) {
        Some(parsed) => parsed,
        None => {
            stats.parse_failures += 1;
            return Ok(());
        }
    };
    stats.files_parsed += 1;

    let edges = import::resolve(parsed.imports, root, abs);
    let file_id = upsert_file(tx, &rel, lang.tag(), &sha, mtime_ns(abs))?;
    tx.execute(
        "DELETE FROM code_graph_symbols WHERE file_id = ?1",
        params![file_id],
    )?;
    tx.execute(
        "DELETE FROM code_graph_imports WHERE from_file_id = ?1",
        params![file_id],
    )?;

    for symbol in &parsed.symbols {
        tx.execute(
            "INSERT INTO code_graph_symbols(file_id, name, kind, start_line, end_line) \
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                file_id,
                symbol.name,
                symbol.kind.tag(),
                symbol.start_line,
                symbol.end_line
            ],
        )?;
    }
    for edge in &edges {
        tx.execute(
            "INSERT INTO code_graph_imports(from_file_id, specifier, to_path, kind) \
             VALUES(?1, ?2, ?3, ?4)",
            params![file_id, edge.specifier, edge.to_path, edge.kind.tag()],
        )?;
    }
    stats.symbols += parsed.symbols.len();
    stats.imports += edges.len();
    Ok(())
}

/// Upsert a file row keyed by its unique path, returning the (stable) row id.
fn upsert_file(
    tx: &Transaction,
    rel: &str,
    lang: &str,
    sha: &str,
    mtime_ns: i64,
) -> Result<i64, GraphError> {
    let id = tx.query_row(
        "INSERT INTO code_graph_files(path, language, content_sha256, mtime_ns, indexed_at) \
         VALUES(?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(path) DO UPDATE SET \
           language = excluded.language, \
           content_sha256 = excluded.content_sha256, \
           mtime_ns = excluded.mtime_ns, \
           indexed_at = excluded.indexed_at \
         RETURNING id",
        params![rel, lang, sha, mtime_ns, now_unix()],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Delete rows for any stored file whose path is not in the current tree.
/// `ON DELETE CASCADE` clears its symbols and imports.
fn prune_missing(tx: &Transaction, current: &HashSet<String>) -> Result<usize, GraphError> {
    let stored: Vec<String> = {
        let mut stmt = tx.prepare("SELECT path FROM code_graph_files")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut removed = 0;
    for path in stored {
        if !current.contains(&path) {
            removed += tx.execute(
                "DELETE FROM code_graph_files WHERE path = ?1",
                params![path],
            )?;
        }
    }
    Ok(removed)
}

fn file_sha(tx: &Transaction, rel: &str) -> Result<Option<String>, GraphError> {
    let sha = tx
        .query_row(
            "SELECT content_sha256 FROM code_graph_files WHERE path = ?1",
            params![rel],
            |row| row.get::<_, String>(0),
        )
        .ok();
    Ok(sha)
}

// ---- Read side (frames come from these) ----------------------------------

/// Every symbol named `name`, across the whole indexed tree.
pub(crate) fn definitions(conn: &Connection, name: &str) -> Result<Vec<DefRow>, GraphError> {
    let mut stmt = conn.prepare(
        "SELECT f.path, f.content_sha256, s.name, s.kind, s.start_line, s.end_line \
         FROM code_graph_symbols s JOIN code_graph_files f ON f.id = s.file_id \
         WHERE s.name = ?1 ORDER BY f.path, s.start_line",
    )?;
    let rows = stmt.query_map(params![name], map_def_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Every symbol defined in a given file.
pub(crate) fn symbols_in_file(conn: &Connection, rel: &str) -> Result<Vec<DefRow>, GraphError> {
    let mut stmt = conn.prepare(
        "SELECT f.path, f.content_sha256, s.name, s.kind, s.start_line, s.end_line \
         FROM code_graph_symbols s JOIN code_graph_files f ON f.id = s.file_id \
         WHERE f.path = ?1 ORDER BY s.start_line",
    )?;
    let rows = stmt.query_map(params![rel], map_def_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The import edges out of a file.
pub(crate) fn imports_from(conn: &Connection, rel: &str) -> Result<Vec<ImportRow>, GraphError> {
    let mut stmt = conn.prepare(
        "SELECT i.specifier, i.to_path, i.kind \
         FROM code_graph_imports i JOIN code_graph_files f ON f.id = i.from_file_id \
         WHERE f.path = ?1 ORDER BY i.specifier",
    )?;
    let rows = stmt.query_map(params![rel], |row| {
        Ok(ImportRow {
            specifier: row.get(0)?,
            to_path: row.get(1)?,
            kind: import_kind_from_tag(&row.get::<_, String>(2)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The files that import (resolve to) a given file.
pub(crate) fn importers_of(conn: &Connection, rel: &str) -> Result<Vec<String>, GraphError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT f.path \
         FROM code_graph_imports i JOIN code_graph_files f ON f.id = i.from_file_id \
         WHERE i.to_path = ?1 ORDER BY f.path",
    )?;
    let rows = stmt.query_map(params![rel], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// All indexed file paths (used by the linear reference scan).
pub(crate) fn all_files(conn: &Connection) -> Result<Vec<String>, GraphError> {
    let mut stmt = conn.prepare("SELECT path FROM code_graph_files ORDER BY path")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Count of indexed files — used by status/tests.
pub(crate) fn file_count(conn: &Connection) -> Result<usize, GraphError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM code_graph_files", [], |r| r.get(0))?;
    Ok(count as usize)
}

fn map_def_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DefRow> {
    Ok(DefRow {
        path: row.get(0)?,
        sha: row.get(1)?,
        name: row.get(2)?,
        kind: SymbolKind::from_tag(&row.get::<_, String>(3)?),
        start_line: row.get(4)?,
        end_line: row.get(5)?,
    })
}

fn import_kind_from_tag(tag: &str) -> ImportKind {
    match tag {
        "relative" => ImportKind::Relative,
        "absolute" => ImportKind::Absolute,
        _ => ImportKind::Bare,
    }
}

// ---- small helpers -------------------------------------------------------

/// Forward-slash path of `abs` relative to `root` (falls back to the whole
/// path if `abs` is somehow not under `root`).
fn rel_path(root: &Path, abs: &Path) -> String {
    match abs.strip_prefix(root) {
        Ok(rel) => import::rel_to_slash(rel),
        Err(_) => import::rel_to_slash(abs),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mtime_ns(abs: &Path) -> i64 {
    std::fs::metadata(abs)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::Grammars;
    use std::fs;
    use tempfile::tempdir;

    fn canon(ws: &tempfile::TempDir) -> PathBuf {
        ws.path().canonicalize().expect("canonicalize")
    }

    #[test]
    fn byte_identical_content_is_never_reparsed() {
        let ws = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let root = canon(&ws);
        let db = dbdir.path().join("context.db");
        fs::write(root.join("x.py"), "def foo():\n    pass\n").unwrap();
        let grammars = Grammars::load().unwrap();
        let mut conn = open(&db).unwrap();

        let first = index_tree(&mut conn, &root, &grammars).unwrap();
        assert_eq!(first.files_parsed, 1);

        // Second pass over unchanged content: zero parses (L-C2).
        let second = index_tree(&mut conn, &root, &grammars).unwrap();
        assert_eq!(
            second.files_parsed, 0,
            "unchanged content must not re-parse"
        );
        assert_eq!(second.files_skipped_unchanged, 1);

        // Change the bytes → it parses again.
        fs::write(root.join("x.py"), "def foo():\n    return 1\n").unwrap();
        let third = index_tree(&mut conn, &root, &grammars).unwrap();
        assert_eq!(third.files_parsed, 1);
    }

    #[test]
    fn kill_during_indexing_leaves_a_consistent_store() {
        let ws = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let root = canon(&ws);
        let db = dbdir.path().join("context.db");
        fs::write(root.join("a.rs"), "pub fn a() {}\n").unwrap();
        fs::write(root.join("b.rs"), "pub struct B;\n").unwrap();
        let grammars = Grammars::load().unwrap();

        // Consistent state A: two files indexed.
        let mut conn = open(&db).unwrap();
        let stats = index_tree(&mut conn, &root, &grammars).unwrap();
        assert_eq!(stats.files_parsed, 2);
        assert_eq!(file_count(&conn).unwrap(), 2);

        // Simulate a kill mid-batch: open a transaction, write a partial row,
        // then drop it WITHOUT committing (rusqlite rolls back on drop).
        {
            let tx = conn.transaction().unwrap();
            tx.execute(
                "INSERT INTO code_graph_files(path, language, content_sha256, mtime_ns, indexed_at) \
                 VALUES('c.rs', 'rust', 'deadbeef', 0, 0)",
                [],
            )
            .unwrap();
            // no commit → rollback on drop here
        }
        drop(conn);

        // Reopen: the rolled-back batch left nothing behind.
        let conn2 = open(&db).unwrap();
        assert_eq!(
            file_count(&conn2).unwrap(),
            2,
            "an uncommitted batch must not persist"
        );
        drop(conn2);

        // And a re-index still completes cleanly against the consistent store.
        let mut conn3 = open(&db).unwrap();
        let reindex = index_tree(&mut conn3, &root, &grammars).unwrap();
        assert_eq!(reindex.files_parsed, 0);
        assert_eq!(reindex.files_skipped_unchanged, 2);
        assert_eq!(file_count(&conn3).unwrap(), 2);
    }

    #[test]
    fn deleted_files_are_pruned_on_reindex() {
        let ws = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let root = canon(&ws);
        let db = dbdir.path().join("context.db");
        fs::write(root.join("keep.rs"), "pub fn keep() {}\n").unwrap();
        fs::write(root.join("drop.rs"), "pub fn gone() {}\n").unwrap();
        let grammars = Grammars::load().unwrap();
        let mut conn = open(&db).unwrap();
        index_tree(&mut conn, &root, &grammars).unwrap();
        assert_eq!(file_count(&conn).unwrap(), 2);

        fs::remove_file(root.join("drop.rs")).unwrap();
        let stats = index_tree(&mut conn, &root, &grammars).unwrap();
        assert_eq!(stats.files_pruned, 1);
        assert_eq!(file_count(&conn).unwrap(), 1);
        // The pruned file's symbols are gone (CASCADE).
        assert!(definitions(&conn, "gone").unwrap().is_empty());
        assert!(!definitions(&conn, "keep").unwrap().is_empty());
    }
}
