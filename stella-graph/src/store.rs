//! SQLite storage for the code graph.
//!
//! mandates **one** `context.db` file with one engine:
//! `stella-context` owns the rest of that file's schema, so every table this
//! crate creates is prefixed `code_graph_` to share the file without
//! colliding. The store is opened against a caller-supplied path so the
//! integration pass can point both crates at the same file.
//!
//! Durability contract ( L-L1, "a kill during indexing
//! leaves a consistent store"): WAL journal mode, and **every index batch is
//! a single transaction**. A process killed mid-batch has committed nothing;
//! reopening sees the previous consistent state and a re-index completes. The
//! crash-consistency test in this module proves it by dropping a transaction
//! (rusqlite rolls back on drop) and asserting the store is unchanged.
//!
//! Byte-compat skip ( L-C2): a file whose content
//! sha256 matches the stored value is never re-parsed. [`IndexStats::files_parsed`]
//! counts real parse invocations, which the skip test asserts drops to zero on
//! an unchanged second pass.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use crate::error::GraphError;
use crate::generated::{self, GeneratedFilter};
use crate::import::{self, ImportKind};
use crate::lang::Language;
use crate::manifest::StorageManifest;
use crate::parse::{Grammars, parse_file};
use crate::storage::{self, FieldEntry, RelationEntry};
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
CREATE TABLE IF NOT EXISTS code_graph_storage_objects (
    id            INTEGER PRIMARY KEY,
    file_id       INTEGER NOT NULL REFERENCES code_graph_files(id) ON DELETE CASCADE,
    parent_id     INTEGER REFERENCES code_graph_storage_objects(id) ON DELETE CASCADE,
    address       TEXT NOT NULL,
    layer         TEXT NOT NULL,
    namespace     TEXT NOT NULL,
    level         TEXT NOT NULL,
    kind          TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    data_type     TEXT,
    nullable      INTEGER,
    default_value TEXT,
    constraints   TEXT,
    ref_target    TEXT,
    comment       TEXT,
    start_line    INTEGER NOT NULL,
    end_line      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS code_graph_storage_addr ON code_graph_storage_objects(address);
CREATE INDEX IF NOT EXISTS code_graph_storage_file ON code_graph_storage_objects(file_id);
"#;

/// Outcome of one index pass. `files_parsed` is the honest parse-invocation
/// count the byte-compat skip test asserts against.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexStats {
    pub files_seen: usize,
    pub files_parsed: usize,
    pub files_skipped_unchanged: usize,
    pub files_skipped_binary: usize,
    /// Files excluded as generated/minified (`crate::generated::is_excluded`)
    /// — `.gitattributes` `linguist-generated=true`, the `*.min.*` filename
    /// convention, or the minified-content heuristic. Surfaced by `stella
    /// init` as "skipped N generated files" (issue #272).
    pub files_skipped_generated: usize,
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
    // Manifest layer mapping and the generated-file filter are each loaded
    // once per pass; a malformed manifest or absent `.gitattributes` degrades
    // to the implicit layer / no-op filter, never aborts the batch (L-L1).
    let manifest = StorageManifest::load(root).ok().flatten();
    let generated_filter = GeneratedFilter::load(root);
    let tx = conn.transaction()?;

    let mut current: HashSet<String> = HashSet::with_capacity(files.len());
    for abs in &files {
        current.insert(rel_path(root, abs));
        index_one(
            &tx,
            root,
            grammars,
            manifest.as_ref(),
            &generated_filter,
            abs,
            &mut stats,
        )?;
    }
    stats.files_pruned += prune_missing(&tx, &current)?;

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
    let manifest = StorageManifest::load(root).ok().flatten();
    let generated_filter = GeneratedFilter::load(root);
    let tx = conn.transaction()?;
    for abs in changed {
        if Language::from_path(abs).is_none() && !storage::indexes_without_language(abs) {
            continue;
        }
        if abs.is_file() {
            index_one(
                &tx,
                root,
                grammars,
                manifest.as_ref(),
                &generated_filter,
                abs,
                &mut stats,
            )?;
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
    manifest: Option<&StorageManifest>,
    generated_filter: &GeneratedFilter,
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

    // Generated/minified exclusion (issue #272) runs **before** the
    // byte-compat skip below, on purpose: a file already sitting in an index
    // built before this filter existed would otherwise never be re-evaluated
    // (its bytes have not changed, so the skip would keep hiding it forever).
    // Deleting any pre-existing row here retroactively cleans that case out
    // on the very next index pass — the same row removal `prune_missing`
    // does for a file that vanished from disk.
    if generated::is_excluded(generated_filter, &rel, &content) {
        stats.files_skipped_generated += 1;
        stats.files_pruned +=
            tx.execute("DELETE FROM code_graph_files WHERE path = ?1", params![rel])?;
        return Ok(());
    }

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
        // Storage-DSL files no grammar claims (`.prisma`): no symbols or
        // imports, but the storage adapter still indexes them (spec §4a).
        if storage::indexes_without_language(abs) {
            let file_id = upsert_file(tx, &rel, "prisma", &sha, mtime_ns(abs))?;
            tx.execute(
                "DELETE FROM code_graph_storage_objects WHERE file_id = ?1",
                params![file_id],
            )?;
            let extract = storage::extract_for_path(grammars, &rel, text);
            let claim = manifest.and_then(|m| m.layer_claim(&rel));
            insert_storage_extract(tx, file_id, claim.as_deref(), &extract)?;
            stats.files_parsed += 1;
        }
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

    // Storage adapters (deep pass, spec §6): the same transaction that
    // replaced this file's symbols replaces its storage rows. Extraction is
    // dispatched by path — SQL DDL, Prisma, TS/JS ORMs, Python ORMs — and
    // marker-gated inside each adapter, so a schema-free source file costs
    // a substring scan.
    tx.execute(
        "DELETE FROM code_graph_storage_objects WHERE file_id = ?1",
        params![file_id],
    )?;
    let extract = storage::extract_for_path(grammars, &rel, text);
    if !extract.is_empty() {
        let claim = manifest.and_then(|m| m.layer_claim(&rel));
        insert_storage_extract(tx, file_id, claim.as_deref(), &extract)?;
    }
    Ok(())
}

/// Persist one file's extracted storage entities: relation rows with their
/// field children, plus parentless field rows for `ALTER TABLE … ADD COLUMN`
/// (attached to their relation by address at snapshot-assembly time).
/// `claim` is the manifest layer whose `paths` glob claims this file — it
/// wins over every relation's own `layer_hint`; with neither, relations
/// land in the implicit relational layer (spec §4a).
fn insert_storage_extract(
    tx: &Transaction,
    file_id: i64,
    claim: Option<&str>,
    extract: &storage::StorageExtract,
) -> Result<(), GraphError> {
    for rel in &extract.relations {
        let layer = claim
            .or(rel.layer_hint.as_deref())
            .unwrap_or(storage::DEFAULT_SQL_LAYER);
        let address = storage::relation_address(layer, &rel.namespace, &rel.name);
        let rel_id: i64 = tx.query_row(
            "INSERT INTO code_graph_storage_objects(\
                 file_id, parent_id, address, layer, namespace, level, kind, \
                 display_name, constraints, comment, start_line, end_line) \
             VALUES(?1, NULL, ?2, ?3, ?4, 'relation', ?5, ?6, ?7, ?8, ?9, ?10) \
             RETURNING id",
            params![
                file_id,
                address,
                storage::normalize_name(layer),
                storage::normalize_name(&rel.namespace),
                rel.kind.tag(),
                rel.name,
                // Relations reuse the constraints column for their enum
                // variants (JSON array; empty for tables/views).
                serde_json::to_string(&rel.enum_values).unwrap_or_default(),
                rel.comment,
                rel.start_line,
                rel.end_line,
            ],
            |row| row.get(0),
        )?;
        for field in &rel.fields {
            insert_storage_field(tx, file_id, Some(rel_id), &address, layer, rel, field)?;
        }
    }
    for addition in &extract.additions {
        // Additions only come from SQL ALTER statements today, so the
        // relational fallback applies when no manifest layer claims the file.
        let layer = claim.unwrap_or(storage::DEFAULT_SQL_LAYER);
        let address = storage::relation_address(layer, &addition.namespace, &addition.relation);
        let rel = storage::RelationDef {
            name: addition.relation.clone(),
            namespace: addition.namespace.clone(),
            kind: storage::RelationKind::Table,
            fields: Vec::new(),
            enum_values: Vec::new(),
            comment: None,
            layer_hint: None,
            start_line: addition.field.line,
            end_line: addition.field.line,
        };
        insert_storage_field(tx, file_id, None, &address, layer, &rel, &addition.field)?;
    }
    Ok(())
}

fn insert_storage_field(
    tx: &Transaction,
    file_id: i64,
    parent_id: Option<i64>,
    relation_address: &str,
    layer: &str,
    rel: &storage::RelationDef,
    field: &storage::FieldDef,
) -> Result<(), GraphError> {
    let address = format!(
        "{relation_address}/{}",
        storage::normalize_name(&field.name)
    );
    tx.execute(
        "INSERT INTO code_graph_storage_objects(\
             file_id, parent_id, address, layer, namespace, level, kind, \
             display_name, data_type, nullable, default_value, constraints, \
             ref_target, comment, start_line, end_line) \
         VALUES(?1, ?2, ?3, ?4, ?5, 'field', 'column', ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
        params![
            file_id,
            parent_id,
            address,
            storage::normalize_name(layer),
            storage::normalize_name(&rel.namespace),
            field.name,
            field.data_type,
            field.nullable,
            field.default_value,
            serde_json::to_string(&field.constraints).unwrap_or_default(),
            field.references,
            field.comment,
            field.line,
        ],
    )?;
    Ok(())
}

// ---- Storage read side (snapshot assembly) -------------------------------

/// Every persisted storage entity, grouped into [`RelationEntry`] shape:
/// relation rows carry their child fields; parentless field rows (`ALTER`
/// additions) fold onto the relation their address names, or synthesize a
/// placeholder relation when the CREATE lives outside the indexed tree.
/// Harvested comments ride along as intent (manifest meaning overrides at
/// merge time). Ordered by address for deterministic snapshots.
pub(crate) fn storage_rows(conn: &Connection) -> Result<Vec<RelationEntry>, GraphError> {
    let mut relations: Vec<RelationEntry> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT o.id, o.address, o.layer, o.namespace, o.kind, o.display_name, \
                    o.constraints, o.comment, o.start_line, f.path \
             FROM code_graph_storage_objects o \
             JOIN code_graph_files f ON f.id = o.file_id \
             WHERE o.level = 'relation' ORDER BY o.address, f.path",
        )?;
        let rows = stmt.query_map([], |row| {
            let enum_values: Option<String> = row.get(6)?;
            Ok((
                row.get::<_, i64>(0)?,
                RelationEntry {
                    address: row.get(1)?,
                    layer: row.get(2)?,
                    namespace: row.get(3)?,
                    kind: row.get(4)?,
                    name: row.get(5)?,
                    fields: Vec::new(),
                    enum_values: enum_values
                        .and_then(|v| serde_json::from_str(&v).ok())
                        .unwrap_or_default(),
                    intent: row.get(7)?,
                    boundary: None,
                    redirects: Vec::new(),
                    source: Some(format!(
                        "{}:{}",
                        row.get::<_, String>(9)?,
                        row.get::<_, u32>(8)?
                    )),
                },
            ))
        })?;
        for row in rows {
            let (_id, entry) = row?;
            // The same address can be defined in several migration files;
            // the first (path-ordered) definition wins, its fields merge in
            // the field pass below regardless of which file defined them.
            if !relations.iter().any(|r| r.address == entry.address) {
                relations.push(entry);
            }
        }
    }

    let mut stmt = conn.prepare(
        "SELECT o.address, o.display_name, o.data_type, o.nullable, o.default_value, \
                o.constraints, o.ref_target, o.comment, o.start_line \
         FROM code_graph_storage_objects o \
         WHERE o.level = 'field' ORDER BY o.address, o.start_line",
    )?;
    let fields = stmt.query_map([], |row| {
        let constraints: Option<String> = row.get(5)?;
        Ok((
            row.get::<_, String>(0)?,
            FieldEntry {
                name: row.get(1)?,
                data_type: row.get(2)?,
                nullable: row.get::<_, Option<bool>>(3)?.unwrap_or(true),
                default_value: row.get(4)?,
                constraints: constraints
                    .and_then(|v| serde_json::from_str(&v).ok())
                    .unwrap_or_default(),
                references: row.get(6)?,
                intent: row.get(7)?,
                line: row.get(8)?,
            },
        ))
    })?;
    for row in fields {
        let (address, field) = row?;
        let Some(relation_address) = address.rsplit_once('/').map(|(rel, _)| rel.to_string())
        else {
            continue;
        };
        let entry = match relations.iter_mut().find(|r| r.address == relation_address) {
            Some(entry) => entry,
            None => {
                // ALTER addition whose CREATE is not indexed: synthesize the
                // relation so the field is still visible and gate-checkable.
                let (layer, rest) = relation_address.split_once('/').unwrap_or(("sql", ""));
                let (namespace, name) = rest.split_once('/').unwrap_or(("default", rest));
                relations.push(RelationEntry {
                    address: relation_address.clone(),
                    layer: layer.to_string(),
                    namespace: namespace.to_string(),
                    name: name.to_string(),
                    kind: "table".into(),
                    fields: Vec::new(),
                    enum_values: Vec::new(),
                    intent: None,
                    boundary: None,
                    redirects: Vec::new(),
                    source: None,
                });
                relations.last_mut().expect("just pushed")
            }
        };
        let key = storage::normalize_name(&field.name);
        if !entry
            .fields
            .iter()
            .any(|f| storage::normalize_name(&f.name) == key)
        {
            entry.fields.push(field);
        }
    }
    Ok(relations)
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
    // `.optional()` maps only "no row for this path" to `None`; any other DB
    // error propagates as `GraphError` instead of being silently swallowed
    // into a spurious "not previously indexed" (which would re-parse and mask
    // a real store fault).
    let sha = tx
        .query_row(
            "SELECT content_sha256 FROM code_graph_files WHERE path = ?1",
            params![rel],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
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

/// Total indexed symbols across the whole tree — the graph total, not a
/// per-pass delta. The startup summary reports this so an incremental pass over
/// an unchanged tree (which re-parses nothing) still shows the real symbol
/// count instead of a misleading zero.
pub(crate) fn symbol_count(conn: &Connection) -> Result<usize, GraphError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM code_graph_symbols", [], |r| r.get(0))?;
    Ok(count as usize)
}

/// Total indexed import edges across the whole tree.
pub(crate) fn import_count(conn: &Connection) -> Result<usize, GraphError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM code_graph_imports", [], |r| r.get(0))?;
    Ok(count as usize)
}

/// The best-connected file in the index: most symbols plus import edges in
/// both directions. UI consumers use it as a default focus when the caller
/// hasn't picked a file yet. `None` on an empty index.
pub(crate) fn busiest_file(conn: &Connection) -> Result<Option<String>, GraphError> {
    let path = conn
        .query_row(
            "SELECT f.path,
                    (SELECT COUNT(*) FROM code_graph_symbols s WHERE s.file_id = f.id)
                  + (SELECT COUNT(*) FROM code_graph_imports i WHERE i.from_file_id = f.id)
                  + (SELECT COUNT(*) FROM code_graph_imports i2 WHERE i2.to_path = f.path)
                    AS degree
             FROM code_graph_files f ORDER BY degree DESC, f.path LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(path)
}

/// All distinct symbol names of the given kind tag (e.g. `"table"`,
/// `"schema_enum"`, `"view"`). Used by the schema gate to populate the
/// known-schema index at session start.
pub(crate) fn names_of_kind(conn: &Connection, kind: &str) -> Result<HashSet<String>, GraphError> {
    let mut stmt =
        conn.prepare("SELECT DISTINCT LOWER(name) FROM code_graph_symbols WHERE kind = ?")?;
    let rows = stmt.query_map(params![kind], |row| row.get::<_, String>(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
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
    fn busiest_file_picks_the_most_connected_file() {
        let ws = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let root = canon(&ws);
        let db = dbdir.path().join("context.db");
        // `hub.rs` carries three symbols; `leaf.rs` carries one. The busiest
        // file is the one with the highest symbol+import degree.
        fs::write(
            root.join("hub.rs"),
            "pub fn a() {}\npub fn b() {}\npub struct C;\n",
        )
        .unwrap();
        fs::write(root.join("leaf.rs"), "pub fn d() {}\n").unwrap();
        let grammars = Grammars::load().unwrap();
        let mut conn = open(&db).unwrap();
        index_tree(&mut conn, &root, &grammars).unwrap();

        assert_eq!(busiest_file(&conn).unwrap().as_deref(), Some("hub.rs"));
    }

    #[test]
    fn busiest_file_is_none_on_an_empty_index() {
        let dbdir = tempdir().unwrap();
        let conn = open(&dbdir.path().join("context.db")).unwrap();
        assert_eq!(busiest_file(&conn).unwrap(), None);
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
    fn sql_files_produce_storage_rows_and_prune_with_their_file() {
        let ws = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let root = canon(&ws);
        let db = dbdir.path().join("context.db");
        fs::write(
            root.join("001_init.sql"),
            "CREATE TABLE payments (\n\
                 id SERIAL PRIMARY KEY,\n\
                 amount NUMERIC(10,2) NOT NULL,\n\
                 user_id INTEGER REFERENCES users(id)\n\
             );\n\
             COMMENT ON COLUMN payments.amount IS 'Gross amount charged.';\n",
        )
        .unwrap();
        fs::write(
            root.join("002_alter.sql"),
            "ALTER TABLE payments ADD COLUMN refunded_at TIMESTAMP;\n",
        )
        .unwrap();
        let grammars = Grammars::load().unwrap();
        let mut conn = open(&db).unwrap();
        index_tree(&mut conn, &root, &grammars).unwrap();

        let rows = storage_rows(&conn).unwrap();
        assert_eq!(rows.len(), 1, "one relation expected: {rows:?}");
        let rel = &rows[0];
        assert_eq!(rel.address, "sql/default/payments");
        assert_eq!(rel.fields.len(), 4, "3 columns + 1 ALTER addition");
        let amount = rel.fields.iter().find(|f| f.name == "amount").unwrap();
        assert_eq!(amount.data_type.as_deref(), Some("NUMERIC(10,2)"));
        assert!(!amount.nullable);
        assert_eq!(amount.intent.as_deref(), Some("Gross amount charged."));
        let user_id = rel.fields.iter().find(|f| f.name == "user_id").unwrap();
        assert_eq!(user_id.references.as_deref(), Some("users"));
        assert!(rel.fields.iter().any(|f| f.name == "refunded_at"));

        // The ALTER's column prunes with its file (CASCADE); the CREATE stays.
        fs::remove_file(root.join("002_alter.sql")).unwrap();
        index_tree(&mut conn, &root, &grammars).unwrap();
        let rows = storage_rows(&conn).unwrap();
        assert_eq!(rows[0].fields.len(), 3);
        assert!(!rows[0].fields.iter().any(|f| f.name == "refunded_at"));
    }

    #[test]
    fn schema_as_code_files_index_through_their_adapters() {
        let ws = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let root = canon(&ws);
        let db = dbdir.path().join("context.db");
        // A grammar-less Prisma DSL file: reached by the walker, indexed by
        // the prisma adapter, no symbols expected.
        fs::write(
            root.join("schema.prisma"),
            "model Payment {\n  id Int @id\n  amount Decimal\n  @@map(\"payments\")\n}\n",
        )
        .unwrap();
        // A Drizzle table in TS: relational, so it shares the implicit sql
        // layer with DDL.
        fs::write(
            root.join("schema.ts"),
            "export const users = pgTable(\"users\", {\n\
                 id: serial(\"id\").primaryKey(),\n\
             });\n",
        )
        .unwrap();
        // A Mongoose collection in JS: its own storage technology, so it
        // lands in the implicit mongo layer.
        fs::write(
            root.join("session.js"),
            "const mongoose = require('mongoose');\n\
             const s = new mongoose.Schema({ token: String });\n\
             mongoose.model('Session', s);\n",
        )
        .unwrap();
        let grammars = Grammars::load().unwrap();
        let mut conn = open(&db).unwrap();
        index_tree(&mut conn, &root, &grammars).unwrap();

        let rows = storage_rows(&conn).unwrap();
        let payments = rows.iter().find(|r| r.name == "payments").unwrap();
        assert_eq!(payments.address, "sql/default/payments");
        assert_eq!(payments.fields.len(), 2);
        assert!(
            payments
                .source
                .as_deref()
                .unwrap()
                .starts_with("schema.prisma:"),
            "{:?}",
            payments.source
        );

        let users = rows.iter().find(|r| r.name == "users").unwrap();
        assert_eq!(users.layer, "sql", "drizzle shares the relational layer");

        let sessions = rows.iter().find(|r| r.name == "sessions").unwrap();
        assert_eq!(sessions.layer, "mongo");
        assert_eq!(sessions.kind, "collection");

        // Removing the prisma file prunes its relation like any source file.
        fs::remove_file(root.join("schema.prisma")).unwrap();
        index_tree(&mut conn, &root, &grammars).unwrap();
        let rows = storage_rows(&conn).unwrap();
        assert!(!rows.iter().any(|r| r.name == "payments"), "{rows:?}");
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

    /// Issue #272: a minified bundle (one giant line, well past the byte
    /// floor) never persists a row, while an ordinary file alongside it
    /// indexes normally.
    #[test]
    fn minified_content_is_skipped_and_not_persisted() {
        let ws = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let root = canon(&ws);
        let db = dbdir.path().join("context.db");
        let long_line = "function bigOne(){return 1;}".repeat(200);
        fs::write(root.join("bundle.js"), format!("{long_line}\n")).unwrap();
        fs::write(root.join("real.js"), "function real() { return 2; }\n").unwrap();
        let grammars = Grammars::load().unwrap();
        let mut conn = open(&db).unwrap();

        let stats = index_tree(&mut conn, &root, &grammars).unwrap();
        assert_eq!(stats.files_skipped_generated, 1, "{stats:?}");
        assert_eq!(
            file_count(&conn).unwrap(),
            1,
            "only the real file is indexed"
        );
        assert!(definitions(&conn, "bigOne").unwrap().is_empty());
        assert!(!definitions(&conn, "real").unwrap().is_empty());
    }

    /// The `*.min.*` filename convention is enough on its own — no minified
    /// content shape required.
    #[test]
    fn min_dot_filename_convention_is_skipped_regardless_of_content_shape() {
        let ws = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let root = canon(&ws);
        let db = dbdir.path().join("context.db");
        fs::write(root.join("app.min.js"), "function tiny() {}\n").unwrap();
        let grammars = Grammars::load().unwrap();
        let mut conn = open(&db).unwrap();

        let stats = index_tree(&mut conn, &root, &grammars).unwrap();
        assert_eq!(stats.files_skipped_generated, 1);
        assert!(definitions(&conn, "tiny").unwrap().is_empty());
    }

    /// Issue #272's retroactive-cleanup requirement: a file indexed normally
    /// under an older version, then later declared `linguist-generated=true`
    /// with its bytes completely unchanged, must still be pruned on the very
    /// next pass — proving the exclusion check runs ahead of (and overrides)
    /// the byte-compat skip rather than being hidden behind it forever.
    #[test]
    fn a_file_marked_generated_after_the_fact_is_pruned_even_with_unchanged_bytes() {
        let ws = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let root = canon(&ws);
        let db = dbdir.path().join("context.db");
        fs::write(root.join("legacy.js"), "function legacyThing() {}\n").unwrap();
        let grammars = Grammars::load().unwrap();
        let mut conn = open(&db).unwrap();

        index_tree(&mut conn, &root, &grammars).unwrap();
        assert!(
            !definitions(&conn, "legacyThing").unwrap().is_empty(),
            "indexed normally before any .gitattributes rule exists"
        );

        // Mark it generated without touching its bytes at all.
        fs::write(
            root.join(".gitattributes"),
            "legacy.js linguist-generated=true\n",
        )
        .unwrap();
        let stats = index_tree(&mut conn, &root, &grammars).unwrap();

        assert_eq!(stats.files_skipped_generated, 1);
        assert!(
            definitions(&conn, "legacyThing").unwrap().is_empty(),
            "a stale pre-fix row must be retroactively pruned, not hidden behind the byte-compat skip"
        );
        assert_eq!(file_count(&conn).unwrap(), 0);
    }
}
