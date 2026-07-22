//! [`ContextStore`] — the one SQLite file, one engine that backs the context
//! plane ("SQLite everywhere … one WAL, one backup
//! story, one file format"). It holds the bi-temporal property graph
//! (`node` + `edge`), the fingerprinted embedding index (`embedding`),
//! episodic memory (`episode`), and the embedder-fingerprint registry.
//!
//! Crash consistency (`L-L1`): every write batch is one transaction, so a
//! kill mid-index rolls back cleanly and reopening finds a consistent store
//! with no partial rows. Warming (`L-C1`): [`ContextStore::open_and_warm`]
//! kicks embedding catch-up as a background task at mount instead of paying it
//! lazily on the first real query.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::clock::{Clock, SystemClock};
use crate::embed::{Embedder, EmbedderFingerprint, HashEmbedder};
use crate::error::ContextError;

use contextgraph_types::FrameKind;

/// The current on-disk schema version, tracked in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 3;

/// The v1 schema. Applied once, inside the migration transaction. Bi-temporal
/// columns (`valid_from`/`valid_to`/`recorded_at`/`superseded_at`) exist on both
/// `node` and `edge`, but only EDGES are actually versioned: `apply_fact`
/// closes an edge's interval on supersession, never deleting (`L-C3`), and
/// `facts_as_of` reads history back. NODES are mutable current-state —
/// `upsert_node` overwrites content in place, so their time columns stay
/// effectively unused and there is no point-in-time node reader. Fact history is
/// recoverable; node content history is not.
const MIGRATION_V1: &str = "\
CREATE TABLE node (
    id            INTEGER PRIMARY KEY,
    public_id     TEXT NOT NULL UNIQUE,
    kind          TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    content       TEXT NOT NULL DEFAULT '',
    content_hash  TEXT NOT NULL,
    uri           TEXT,
    properties    TEXT NOT NULL DEFAULT '{}',
    valid_from    TEXT,
    valid_to      TEXT,
    recorded_at   TEXT NOT NULL,
    superseded_at TEXT
);
CREATE INDEX idx_node_kind ON node(kind);
CREATE INDEX idx_node_uri ON node(uri);
CREATE INDEX idx_node_content_hash ON node(content_hash);

CREATE TABLE edge (
    id            INTEGER PRIMARY KEY,
    public_id     TEXT NOT NULL,
    rel           TEXT NOT NULL,
    src_id        INTEGER NOT NULL REFERENCES node(id),
    dst_id        INTEGER NOT NULL REFERENCES node(id),
    weight        REAL NOT NULL DEFAULT 1.0,
    properties    TEXT NOT NULL DEFAULT '{}',
    valid_from    TEXT,
    valid_to      TEXT,
    recorded_at   TEXT NOT NULL,
    superseded_at TEXT,
    supersedes    INTEGER REFERENCES edge(id)
);
CREATE INDEX idx_edge_src ON edge(src_id);
CREATE INDEX idx_edge_dst ON edge(dst_id);
CREATE INDEX idx_edge_rel ON edge(rel);

CREATE TABLE embedding (
    content_hash  TEXT NOT NULL,
    fingerprint   TEXT NOT NULL,
    dims          INTEGER NOT NULL,
    vector        BLOB NOT NULL,
    recorded_at   TEXT NOT NULL,
    PRIMARY KEY (content_hash, fingerprint)
) WITHOUT ROWID;

CREATE TABLE episode (
    id            INTEGER PRIMARY KEY,
    public_id     TEXT NOT NULL UNIQUE,
    summary       TEXT NOT NULL,
    files_touched TEXT NOT NULL DEFAULT '[]',
    outcome       TEXT NOT NULL,
    salience      REAL NOT NULL DEFAULT 0.0,
    started_at    TEXT NOT NULL,
    ended_at      TEXT NOT NULL,
    recorded_at   TEXT NOT NULL
);

CREATE TABLE embedder_fingerprint (
    id            TEXT PRIMARY KEY,
    model_id      TEXT NOT NULL,
    revision      TEXT NOT NULL,
    dims          INTEGER NOT NULL,
    normalization TEXT NOT NULL,
    first_seen_at TEXT NOT NULL
);
";

/// The v2 schema (scope update): workspace **domains** as first-class tags, and
/// a **memory** record type (reflections). Domains are a normalized table plus
/// indexable junctions — never a JSON blob — so "everything in domain X" is a
/// key-lookup, not a scan. A domain tag rides node and edge/fact rows (and, via
/// their mirror nodes, episodes and memories). Reflection memories are their
/// own record with a `kind`, mirrored to a retrievable `Memory` node so recall
/// scores them by similarity + domain overlap + recency.
const MIGRATION_V2: &str = "\
CREATE TABLE domain (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    description  TEXT,
    recorded_at  TEXT NOT NULL
);

CREATE TABLE node_domains (
    node_id      INTEGER NOT NULL REFERENCES node(id),
    domain_id    INTEGER NOT NULL REFERENCES domain(id),
    PRIMARY KEY (node_id, domain_id)
) WITHOUT ROWID;
CREATE INDEX idx_node_domains_domain ON node_domains(domain_id);

CREATE TABLE edge_domains (
    edge_id      INTEGER NOT NULL REFERENCES edge(id),
    domain_id    INTEGER NOT NULL REFERENCES domain(id),
    PRIMARY KEY (edge_id, domain_id)
) WITHOUT ROWID;
CREATE INDEX idx_edge_domains_domain ON edge_domains(domain_id);

CREATE TABLE memory (
    id           INTEGER PRIMARY KEY,
    public_id    TEXT NOT NULL UNIQUE,
    kind         TEXT NOT NULL,
    content      TEXT NOT NULL,
    salience     REAL NOT NULL DEFAULT 0.0,
    recorded_at  TEXT NOT NULL
);
CREATE INDEX idx_memory_kind ON memory(kind);
";

/// V3 — evict the code graph's tables from `context.db`. Historically the
/// tree-sitter index shared this one file (`stella-graph`'s original
/// single-file design, prefixing its tables `code_graph_`); it now lives in its
/// own `.stella/private/codegraph.db`, which every consumer (`graph_query`, the CGP
/// `GraphProvider`) reads. Any `code_graph_*` tables still in `context.db` are
/// orphaned duplicates no code reads or updates — dropping them removes the
/// "two databases hold the code graph" duplication. Children (FK to
/// `code_graph_files`) are dropped first. `IF EXISTS` so a fresh store is a
/// no-op.
const MIGRATION_V3: &str = "\
DROP TABLE IF EXISTS code_graph_symbols;
DROP TABLE IF EXISTS code_graph_imports;
DROP TABLE IF EXISTS code_graph_files;
";

/// Typed node vocabulary. Stored as its
/// `as_str` form; retrieval maps it onto an `contextgraph_types::FrameKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Symbol,
    Concept,
    Fact,
    Episode,
    Person,
    Artifact,
    Task,
    /// A memory (e.g. a post-turn reflection). Mirrors a `memory` record so it
    /// is retrievable and domain-taggable through the normal node pipeline.
    Memory,
}

impl NodeKind {
    /// The canonical string stored in `node.kind`.
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Symbol => "symbol",
            NodeKind::Concept => "concept",
            NodeKind::Fact => "fact",
            NodeKind::Episode => "episode",
            NodeKind::Person => "person",
            NodeKind::Artifact => "artifact",
            NodeKind::Task => "task",
            NodeKind::Memory => "memory",
        }
    }

    /// Parse a stored `node.kind` back into the enum.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "file" => NodeKind::File,
            "symbol" => NodeKind::Symbol,
            "concept" => NodeKind::Concept,
            "fact" => NodeKind::Fact,
            "episode" => NodeKind::Episode,
            "person" => NodeKind::Person,
            "artifact" => NodeKind::Artifact,
            "task" => NodeKind::Task,
            "memory" => NodeKind::Memory,
            _ => return None,
        })
    }

    /// Map onto the CGP frame kind a retrieved node surfaces as.
    pub fn to_frame_kind(self) -> FrameKind {
        match self {
            NodeKind::File => FrameKind::Snippet,
            NodeKind::Symbol => FrameKind::Symbol,
            NodeKind::Episode => FrameKind::Episode,
            NodeKind::Artifact => FrameKind::Doc,
            NodeKind::Memory => FrameKind::Memory,
            // Concept/Fact/Person/Task all read as facts to a consuming host.
            NodeKind::Concept | NodeKind::Fact | NodeKind::Person | NodeKind::Task => {
                FrameKind::Fact
            }
        }
    }
}

/// A node to write. `display_name` is mandatory and non-empty — it is the
/// human citation label (`L-C4`), enforced at write time so retrieval can
/// never later fail to cite.
#[derive(Debug, Clone)]
pub struct NodeInput {
    pub kind: NodeKind,
    pub display_name: String,
    pub content: String,
    pub uri: Option<String>,
    pub properties: serde_json::Value,
    /// Workspace domain tags (e.g. `["auth", "billing"]`). One or more; stored
    /// via the `node_domains` junction (indexable), never a JSON blob.
    pub domains: Vec<String>,
}

impl NodeInput {
    /// A node with the given kind and label, empty content, no uri, no domains.
    pub fn new(kind: NodeKind, display_name: impl Into<String>) -> Self {
        Self {
            kind,
            display_name: display_name.into(),
            content: String::new(),
            uri: None,
            properties: serde_json::json!({}),
            domains: Vec::new(),
        }
    }

    /// Attach retrievable content.
    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = content.into();
        self
    }

    /// Attach a source uri (used for anchor matching and provenance).
    pub fn with_uri(mut self, uri: impl Into<String>) -> Self {
        self.uri = Some(uri.into());
        self
    }

    /// Tag with one or more workspace domains.
    pub fn with_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.domains = domains.into_iter().map(Into::into).collect();
        self
    }

    /// The identity key: the uri when present, else the display name. Two
    /// writes with the same identity update the same node (content-on-touch).
    fn natural_key(&self) -> &str {
        match &self.uri {
            Some(u) if !u.is_empty() => u.as_str(),
            _ => self.display_name.as_str(),
        }
    }

    fn public_id(&self) -> String {
        node_public_id(self.kind, self.natural_key())
    }
}

/// A node read back from the store.
#[derive(Debug, Clone)]
pub struct NodeRow {
    pub id: i64,
    pub public_id: String,
    pub kind: NodeKind,
    pub display_name: String,
    pub content: String,
    pub content_hash: String,
    pub uri: Option<String>,
    /// Valid time: when the fact became true in the world — may precede
    /// `recorded_at` (observation), never follows it. `None` = unknown,
    /// treated as valid-since-observation.
    pub valid_from: Option<String>,
    pub recorded_at: String,
}

/// The context plane's storage handle. Cheap to clone conceptually (share the
/// `Arc`s); see [`ContextStore::open`].
pub struct ContextStore {
    /// The DB path, kept so warming can open its own WAL connection.
    path: PathBuf,
    /// `Arc<Mutex<..>>` restores `Sync` (a bare `Connection` is `Send` only)
    /// so the store can implement the `Send + Sync` provider trait. All
    /// SQLite work happens inside the lock with no `await` held.
    conn: Arc<Mutex<Connection>>,
    embedder: Arc<dyn Embedder>,
    fingerprint: EmbedderFingerprint,
    clock: Arc<dyn Clock>,
    /// The background warm task, joinable via `await_warm`.
    warm: Mutex<Option<tokio::task::JoinHandle<Result<usize, ContextError>>>>,
}

impl ContextStore {
    /// Open (creating if absent) the store at `path` with the default
    /// [`HashEmbedder`] and system clock. Runs migrations and registers the
    /// embedder fingerprint. Does **not** warm — see [`Self::open_and_warm`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ContextError> {
        Self::open_with(
            path,
            Arc::new(HashEmbedder::default()),
            Arc::new(SystemClock),
        )
    }

    /// Open with an explicit embedder and clock (the injectable form used by
    /// tests and by callers that pin a specific embedder/time source).
    pub fn open_with(
        path: impl AsRef<Path>,
        embedder: Arc<dyn Embedder>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ContextError> {
        let path = path.as_ref().to_path_buf();
        let conn = open_connection(&path)?;
        migrate(&conn)?;
        let fingerprint = embedder.fingerprint();
        register_fingerprint(&conn, &fingerprint, &clock.now_rfc3339())?;
        Ok(Self {
            path,
            conn: Arc::new(Mutex::new(conn)),
            embedder,
            fingerprint,
            clock,
            warm: Mutex::new(None),
        })
    }

    /// Open and immediately kick embedding catch-up as a background tokio task
    /// (`L-C1`: warm at mount, don't pay indexing on the first prompt). Must be
    /// called inside a tokio runtime; if none is running the store is returned
    /// un-warmed (catch-up can still be driven explicitly via [`Self::warm_now`]).
    /// Join the task with [`Self::await_warm`].
    pub fn open_and_warm(
        path: impl AsRef<Path>,
        embedder: Arc<dyn Embedder>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ContextError> {
        let store = Self::open_with(path, embedder, clock)?;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let path = store.path.clone();
            let embedder = store.embedder.clone();
            let fingerprint = store.fingerprint.id();
            let clock = store.clock.clone();
            let task =
                handle.spawn(async move { warm_index(path, embedder, fingerprint, clock).await });
            *lock(&store.warm) = Some(task);
        }
        Ok(store)
    }

    /// Alias for [`Self::open_and_warm`] matching the spec's `mount()` verb.
    pub fn mount(
        path: impl AsRef<Path>,
        embedder: Arc<dyn Embedder>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ContextError> {
        Self::open_and_warm(path, embedder, clock)
    }

    /// Join the background warm task if one was spawned, returning the number
    /// of vectors it computed. Returns `Ok(0)` if warming never started.
    pub async fn await_warm(&self) -> Result<usize, ContextError> {
        let handle = lock(&self.warm).take();
        match handle {
            Some(h) => h
                .await
                .map_err(|e| ContextError::Corruption(format!("warm task failed to join: {e}")))?,
            None => Ok(0),
        }
    }

    /// Drive embedding catch-up to completion synchronously (awaitable).
    /// Reused by the background warm task; exposed for callers/tests that want
    /// a deterministic, joined warm without a spawn.
    pub async fn warm_now(&self) -> Result<usize, ContextError> {
        warm_index(
            self.path.clone(),
            self.embedder.clone(),
            self.fingerprint.id(),
            self.clock.clone(),
        )
        .await
    }

    /// The active embedder fingerprint. Retrieval compares only vectors under
    /// this fingerprint (`L-C2`).
    pub fn fingerprint(&self) -> &EmbedderFingerprint {
        &self.fingerprint
    }

    /// The embedder, for pipelines that need to embed the query text.
    pub(crate) fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Run `PRAGMA integrity_check`; `Err(Corruption)` if not `"ok"`. The
    /// kill-during-index consistency test asserts this holds after a torn
    /// write (`L-L1`).
    pub fn integrity_check(&self) -> Result<(), ContextError> {
        let conn = lock(&self.conn);
        let result: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if result == "ok" {
            Ok(())
        } else {
            Err(ContextError::Corruption(result))
        }
    }

    /// Lock the connection for a synchronous unit of work. Poison-tolerant:
    /// a panic in one section never wedges the store for the rest.
    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        lock(&self.conn)
    }

    /// Count of currently-live nodes (`superseded_at IS NULL`).
    pub fn node_count(&self) -> Result<usize, ContextError> {
        let conn = lock(&self.conn);
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM node WHERE superseded_at IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// All workspace domains as `(name, description)` — for a `context status`
    /// surface. The domains themselves are produced by `stella init` and arrive
    /// through the write path as data.
    pub fn domains(&self) -> Result<Vec<(String, Option<String>)>, ContextError> {
        let conn = lock(&self.conn);
        list_domains(&conn)
    }

    /// Every live Memory-kind node, newest first — the inspection surface
    /// behind `stella memory` (its citation stats join on `public_id`, the
    /// same stable id recalled frames carry).
    pub fn memory_nodes(&self) -> Result<Vec<NodeRow>, ContextError> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT id, public_id, kind, display_name, content, content_hash, uri, valid_from, recorded_at
             FROM node WHERE kind = 'memory' AND superseded_at IS NULL
             ORDER BY recorded_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], map_node_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// A live node by its stable public id (`nod_…`) — how `stella memory
    /// promote` resolves a cited id back to the memory's content.
    pub fn node_by_public_id(&self, public_id: &str) -> Result<Option<NodeRow>, ContextError> {
        let conn = lock(&self.conn);
        let row = conn
            .query_row(
                "SELECT id, public_id, kind, display_name, content, content_hash, uri, valid_from, recorded_at
                 FROM node WHERE public_id = ?1 AND superseded_at IS NULL",
                params![public_id],
                map_node_row,
            )
            .optional()?;
        Ok(row)
    }
}

/// Lock a mutex, recovering the guard even if a previous holder panicked. This
/// keeps the store usable after a panic in one operation (no `unwrap` on the
/// poison error, which the house style forbids outside tests).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Open a connection with the plane's fixed pragmas: WAL for concurrent
/// reader/writer, `NORMAL` sync (durable enough with WAL, far cheaper than
/// `FULL`), foreign keys on, and a busy timeout so a warm-task write never
/// races the main connection into `SQLITE_BUSY`.
fn open_connection(path: &Path) -> Result<Connection, ContextError> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;\
         PRAGMA synchronous=NORMAL;\
         PRAGMA foreign_keys=ON;\
         PRAGMA busy_timeout=5000;",
    )?;
    Ok(conn)
}

/// Apply pending migrations inside a single transaction, bumping
/// `user_version` atomically with the DDL.
fn migrate(conn: &Connection) -> Result<(), ContextError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    if version < 1 {
        tx.execute_batch(MIGRATION_V1)?;
    }
    if version < 2 {
        tx.execute_batch(MIGRATION_V2)?;
    }
    if version < 3 {
        tx.execute_batch(MIGRATION_V3)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

/// Record the active fingerprint (idempotent). Its presence in the registry is
/// what lets a later `status` command report which embedder the index was
/// built with.
fn register_fingerprint(
    conn: &Connection,
    fp: &EmbedderFingerprint,
    now: &str,
) -> Result<(), ContextError> {
    conn.execute(
        "INSERT INTO embedder_fingerprint (id, model_id, revision, dims, normalization, first_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO NOTHING",
        params![
            fp.id(),
            fp.model_id,
            fp.revision,
            fp.dims as i64,
            fp.normalization,
            now
        ],
    )?;
    Ok(())
}

/// Background embedding catch-up: embed every live node whose content lacks a
/// vector under the active fingerprint. Opens its own WAL connection (so it
/// never contends with the store's lock) and writes each batch as one
/// transaction (`L-L1` crash consistency). Returns the count embedded.
async fn warm_index(
    path: PathBuf,
    embedder: Arc<dyn Embedder>,
    fingerprint: String,
    clock: Arc<dyn Clock>,
) -> Result<usize, ContextError> {
    // An in-memory store's second connection would be a different empty DB;
    // warming only makes sense for a file-backed store.
    if path.as_os_str() == ":memory:" {
        return Ok(0);
    }
    let conn = open_connection(&path)?;
    let pending = nodes_missing_embedding(&conn, &fingerprint)?;
    if pending.is_empty() {
        return Ok(0);
    }

    let mut embedded = 0usize;
    // Batch to keep memory bounded on a large first index.
    const BATCH: usize = 64;
    for chunk in pending.chunks(BATCH) {
        let texts: Vec<String> = chunk.iter().map(|(_, c)| c.clone()).collect();
        let vectors = embedder.embed(&texts).await?;
        let now = clock.now_rfc3339();
        let tx = conn.unchecked_transaction()?;
        for ((content_hash, _), emb) in chunk.iter().zip(vectors.iter()) {
            store_embedding(&tx, content_hash, &fingerprint, &emb.vector, &now)?;
            embedded += 1;
        }
        tx.commit()?;
    }
    Ok(embedded)
}

// ---------------------------------------------------------------------------
// Low-level accessors (pub(crate)) shared by retrieval.rs and writeback.rs.
// All take a `&Connection` (a `&Transaction` derefs to one), so a caller can
// batch many of them inside a single transaction.
// ---------------------------------------------------------------------------

/// Lowercase hex of raw bytes. Replaces `format!("{:x}", digest)`: digest
/// 0.11 (sha2 0.11) returns an `Output` array that no longer implements
/// `LowerHex`. Byte-for-byte identical to the old rendering — these hashes
/// are persisted stable ids, so the encoding must not drift.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// sha256 hex of a string — the content hash keying embeddings (`L-C2`).
pub(crate) fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    to_hex(&h.finalize())
}

fn node_public_id(kind: NodeKind, natural_key: &str) -> String {
    let mut h = Sha256::new();
    h.update(kind.as_str().as_bytes());
    h.update([0u8]);
    h.update(natural_key.as_bytes());
    let hex = to_hex(&h.finalize());
    format!("nod_{}", &hex[..24])
}

/// Encode a vector as a little-endian f32 BLOB.
pub(crate) fn vector_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Decode a little-endian f32 BLOB back to a vector. A length that isn't a
/// multiple of 4 is corruption, reported loudly rather than truncated.
pub(crate) fn blob_to_vector(blob: &[u8]) -> Result<Vec<f32>, ContextError> {
    if !blob.len().is_multiple_of(4) {
        return Err(ContextError::Corruption(format!(
            "embedding blob length {} is not a multiple of 4",
            blob.len()
        )));
    }
    Ok(blob
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Upsert a node by identity, updating content-on-touch. Returns its rowid.
/// Rejects an empty display name (`L-C4` — a node must be citable).
pub(crate) fn upsert_node(
    conn: &Connection,
    node: &NodeInput,
    now: &str,
) -> Result<i64, ContextError> {
    if node.display_name.trim().is_empty() {
        return Err(ContextError::InvalidInput(
            "node display_name must be non-empty (every node must be humanly citable, L-C4)".into(),
        ));
    }
    let content_hash = sha256_hex(&node.content);
    let props = serde_json::to_string(&node.properties)?;
    let id: i64 = conn.query_row(
        "INSERT INTO node (public_id, kind, display_name, content, content_hash, uri, properties, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(public_id) DO UPDATE SET
             display_name = excluded.display_name,
             content      = excluded.content,
             content_hash = excluded.content_hash,
             uri          = excluded.uri,
             properties   = excluded.properties
         RETURNING id",
        params![
            node.public_id(),
            node.kind.as_str(),
            node.display_name,
            node.content,
            content_hash,
            node.uri,
            props,
            now
        ],
        |r| r.get(0),
    )?;
    Ok(id)
}

fn map_node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRow> {
    let kind_str: String = row.get("kind")?;
    Ok(NodeRow {
        id: row.get("id")?,
        public_id: row.get("public_id")?,
        kind: NodeKind::parse(&kind_str).unwrap_or(NodeKind::Concept),
        display_name: row.get("display_name")?,
        content: row.get("content")?,
        content_hash: row.get("content_hash")?,
        uri: row.get("uri")?,
        valid_from: row.get("valid_from")?,
        recorded_at: row.get("recorded_at")?,
    })
}

/// Every live node (for recency scoring, lexical fallback, and warm scanning).
pub(crate) fn live_nodes(conn: &Connection) -> Result<Vec<NodeRow>, ContextError> {
    let mut stmt = conn.prepare(
        "SELECT id, public_id, kind, display_name, content, content_hash, uri, valid_from, recorded_at
         FROM node WHERE superseded_at IS NULL",
    )?;
    let rows = stmt.query_map([], map_node_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Live node ids whose uri matches one of `uris` (anchor resolution).
pub(crate) fn node_ids_for_uris(
    conn: &Connection,
    uris: &[String],
) -> Result<Vec<i64>, ContextError> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare("SELECT id FROM node WHERE uri = ?1 AND superseded_at IS NULL")?;
    for uri in uris {
        let ids = stmt.query_map(params![uri], |r| r.get::<_, i64>(0))?;
        for id in ids {
            out.push(id?);
        }
    }
    Ok(out)
}

/// (node_id, vector) for every live node with an embedding under `fingerprint`.
/// The join on `content_hash` is what enforces "never mix fingerprints"
/// structurally — a vector under any other fingerprint is simply not selected.
pub(crate) fn vectors_for_fingerprint(
    conn: &Connection,
    fingerprint: &str,
) -> Result<Vec<(i64, Vec<f32>)>, ContextError> {
    let mut stmt = conn.prepare(
        "SELECT n.id, e.vector
         FROM embedding e
         JOIN node n ON n.content_hash = e.content_hash
         WHERE e.fingerprint = ?1 AND n.superseded_at IS NULL",
    )?;
    let rows = stmt.query_map(params![fingerprint], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (id, blob) = r?;
        out.push((id, blob_to_vector(&blob)?));
    }
    Ok(out)
}

/// Fetch a single node by rowid.
pub(crate) fn node_by_id(conn: &Connection, id: i64) -> Result<Option<NodeRow>, ContextError> {
    let row = conn
        .query_row(
            "SELECT id, public_id, kind, display_name, content, content_hash, uri, valid_from, recorded_at
             FROM node WHERE id = ?1",
            params![id],
            map_node_row,
        )
        .optional()?;
    Ok(row)
}

/// 1-hop neighbors of `seeds` over currently-believed fact edges, as
/// `(neighbor_id, edge_weight)`. `as_of` (transaction time) pins which beliefs
/// are visible: `None` = currently believed (`superseded_at IS NULL`).
pub(crate) fn neighbors(
    conn: &Connection,
    seeds: &[i64],
    as_of: Option<&str>,
) -> Result<Vec<(i64, f64)>, ContextError> {
    let mut out = Vec::new();
    // Undirected 1-hop: a seed on either endpoint pulls in the other.
    let sql = match as_of {
        None => {
            "SELECT CASE WHEN src_id = ?1 THEN dst_id ELSE src_id END AS other, weight
             FROM edge
             WHERE (src_id = ?1 OR dst_id = ?1) AND superseded_at IS NULL"
        }
        Some(_) => {
            "SELECT CASE WHEN src_id = ?1 THEN dst_id ELSE src_id END AS other, weight
             FROM edge
             WHERE (src_id = ?1 OR dst_id = ?1)
               AND recorded_at <= ?2
               AND (superseded_at IS NULL OR superseded_at > ?2)"
        }
    };
    let map = |r: &rusqlite::Row<'_>| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?));
    let mut stmt = conn.prepare(sql)?;
    for &seed in seeds {
        // Each arm's `MappedRows` is a distinct closure type, so consume it
        // fully inside the arm rather than binding it across the `match`.
        match as_of {
            None => {
                for r in stmt.query_map(params![seed], map)? {
                    out.push(r?);
                }
            }
            Some(t) => {
                for r in stmt.query_map(params![seed, t], map)? {
                    out.push(r?);
                }
            }
        }
    }
    Ok(out)
}

/// Whether a vector already exists for `(content_hash, fingerprint)` — the
/// byte-compat skip that makes re-indexing cheap (`L-C2`).
pub(crate) fn embedding_exists(
    conn: &Connection,
    content_hash: &str,
    fingerprint: &str,
) -> Result<bool, ContextError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM embedding WHERE content_hash = ?1 AND fingerprint = ?2",
        params![content_hash, fingerprint],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Insert a vector (idempotent under the composite primary key). Returns
/// whether a new row was written (`false` = it already existed = reused).
pub(crate) fn store_embedding(
    conn: &Connection,
    content_hash: &str,
    fingerprint: &str,
    vector: &[f32],
    now: &str,
) -> Result<bool, ContextError> {
    let changed = conn.execute(
        "INSERT INTO embedding (content_hash, fingerprint, dims, vector, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(content_hash, fingerprint) DO NOTHING",
        params![
            content_hash,
            fingerprint,
            vector.len() as i64,
            vector_to_blob(vector),
            now
        ],
    )?;
    Ok(changed > 0)
}

/// A fact edge read back for point-in-time queries. Endpoints are node rowids
/// the caller resolves to human labels (`L-C4`).
#[derive(Debug, Clone)]
pub(crate) struct EdgeView {
    pub rel: String,
    pub src_id: i64,
    pub dst_id: i64,
    pub recorded_at: String,
    pub superseded_at: Option<String>,
}

/// Process-local sequence making each edge's `public_id` unique even for two
/// facts asserted at the same clock second.
static EDGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Insert a fact edge. `supersedes` links to the edge this one replaced (the
/// `SUPERSEDES` relation of), or `None` for a
/// fresh assertion. Returns the new edge's rowid.
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_edge(
    conn: &Connection,
    rel: &str,
    src_id: i64,
    dst_id: i64,
    weight: f64,
    properties: &serde_json::Value,
    valid_from: Option<&str>,
    valid_to: Option<&str>,
    now: &str,
    supersedes: Option<i64>,
) -> Result<i64, ContextError> {
    let seq = EDGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let public_id = format!(
        "edg_{}",
        &sha256_hex(&format!("{rel}:{src_id}:{dst_id}:{now}:{seq}"))[..24]
    );
    let props = serde_json::to_string(properties)?;
    let id: i64 = conn.query_row(
        "INSERT INTO edge (public_id, rel, src_id, dst_id, weight, properties, valid_from, valid_to, recorded_at, supersedes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         RETURNING id",
        params![public_id, rel, src_id, dst_id, weight, props, valid_from, valid_to, now, supersedes],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// The currently-believed edge with this subject and relation, if any, as
/// `(edge_id, dst_id)`. Used to decide whether a new assertion is idempotent,
/// a correction (supersede), or fresh.
pub(crate) fn currently_valid_edge(
    conn: &Connection,
    src_id: i64,
    rel: &str,
) -> Result<Option<(i64, i64)>, ContextError> {
    let row = conn
        .query_row(
            "SELECT id, dst_id FROM edge
             WHERE src_id = ?1 AND rel = ?2 AND superseded_at IS NULL
             ORDER BY id DESC LIMIT 1",
            params![src_id, rel],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    Ok(row)
}

/// Close an edge's intervals: set `superseded_at` (transaction time) and, if
/// not already ended, `valid_to` (world time). **Never deletes** (`L-C3`) — the
/// row survives so "what did we believe at T1" still answers.
pub(crate) fn close_edge(
    conn: &Connection,
    edge_id: i64,
    superseded_at: &str,
    valid_to: &str,
) -> Result<(), ContextError> {
    conn.execute(
        "UPDATE edge SET superseded_at = ?2, valid_to = COALESCE(valid_to, ?3) WHERE id = ?1",
        params![edge_id, superseded_at, valid_to],
    )?;
    Ok(())
}

/// Fact edges as believed at a transaction-time instant. `as_of = None` means
/// "currently believed" (`superseded_at IS NULL`); `Some(t)` reconstructs the
/// belief set at `t` — the bi-temporal audit query (`L-C3`).
pub(crate) fn edges_as_of(
    conn: &Connection,
    as_of: Option<&str>,
) -> Result<Vec<EdgeView>, ContextError> {
    let map = |r: &rusqlite::Row<'_>| {
        Ok(EdgeView {
            rel: r.get(0)?,
            src_id: r.get(1)?,
            dst_id: r.get(2)?,
            recorded_at: r.get(3)?,
            superseded_at: r.get(4)?,
        })
    };
    let mut out = Vec::new();
    match as_of {
        None => {
            let mut stmt = conn.prepare(
                "SELECT rel, src_id, dst_id, recorded_at, superseded_at
                 FROM edge WHERE superseded_at IS NULL",
            )?;
            for r in stmt.query_map([], map)? {
                out.push(r?);
            }
        }
        Some(t) => {
            let mut stmt = conn.prepare(
                "SELECT rel, src_id, dst_id, recorded_at, superseded_at
                 FROM edge
                 WHERE recorded_at <= ?1 AND (superseded_at IS NULL OR superseded_at > ?1)",
            )?;
            for r in stmt.query_map(params![t], map)? {
                out.push(r?);
            }
        }
    }
    Ok(out)
}

/// Insert or update an episode (idempotent by `public_id`). Returns rowid.
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_episode(
    conn: &Connection,
    public_id: &str,
    summary: &str,
    files_touched: &serde_json::Value,
    outcome: &str,
    salience: f64,
    started_at: &str,
    ended_at: &str,
    now: &str,
) -> Result<i64, ContextError> {
    let files = serde_json::to_string(files_touched)?;
    let id: i64 = conn.query_row(
        "INSERT INTO episode (public_id, summary, files_touched, outcome, salience, started_at, ended_at, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(public_id) DO UPDATE SET
             summary = excluded.summary,
             files_touched = excluded.files_touched,
             outcome = excluded.outcome,
             salience = excluded.salience
         RETURNING id",
        params![public_id, summary, files, outcome, salience, started_at, ended_at, now],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Insert or update a memory record (idempotent by `public_id`). Returns rowid.
pub(crate) fn insert_memory(
    conn: &Connection,
    public_id: &str,
    kind: &str,
    content: &str,
    salience: f64,
    now: &str,
) -> Result<i64, ContextError> {
    let id: i64 = conn.query_row(
        "INSERT INTO memory (public_id, kind, content, salience, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(public_id) DO UPDATE SET
             kind = excluded.kind,
             content = excluded.content,
             salience = excluded.salience
         RETURNING id",
        params![public_id, kind, content, salience, now],
        |r| r.get(0),
    )?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Domains: a normalized table + indexable junctions (scope update). Tagging is
// idempotent; unknown domain names are auto-created (the workspace's `stella
// init` domains arrive as data, so a write may reference a name before an
// explicit definition with a description exists).
// ---------------------------------------------------------------------------

/// Insert a domain by name (idempotent), optionally setting/refreshing its
/// description. Returns its rowid.
pub(crate) fn upsert_domain(
    conn: &Connection,
    name: &str,
    description: Option<&str>,
    now: &str,
) -> Result<i64, ContextError> {
    if name.trim().is_empty() {
        return Err(ContextError::InvalidInput(
            "domain name must be non-empty".into(),
        ));
    }
    // COALESCE keeps an existing description when a later tag-only write passes
    // None, but lets an explicit definition set or update it.
    let id: i64 = conn.query_row(
        "INSERT INTO domain (name, description, recorded_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(name) DO UPDATE SET
             description = COALESCE(excluded.description, domain.description)
         RETURNING id",
        params![name, description, now],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Tag a node with domains (auto-creating unknown ones). Returns the number of
/// new tag associations written.
pub(crate) fn tag_node_domains(
    conn: &Connection,
    node_id: i64,
    domains: &[String],
    now: &str,
) -> Result<usize, ContextError> {
    let mut added = 0;
    for name in domains {
        let domain_id = upsert_domain(conn, name, None, now)?;
        added += conn.execute(
            "INSERT INTO node_domains (node_id, domain_id) VALUES (?1, ?2)
             ON CONFLICT(node_id, domain_id) DO NOTHING",
            params![node_id, domain_id],
        )?;
    }
    Ok(added)
}

/// Tag an edge/fact with domains (auto-creating unknown ones). Returns the
/// number of new tag associations written.
pub(crate) fn tag_edge_domains(
    conn: &Connection,
    edge_id: i64,
    domains: &[String],
    now: &str,
) -> Result<usize, ContextError> {
    let mut added = 0;
    for name in domains {
        let domain_id = upsert_domain(conn, name, None, now)?;
        added += conn.execute(
            "INSERT INTO edge_domains (edge_id, domain_id) VALUES (?1, ?2)
             ON CONFLICT(edge_id, domain_id) DO NOTHING",
            params![edge_id, domain_id],
        )?;
    }
    Ok(added)
}

/// Every LIVE node's domain names in one scan, sorted per node for stable
/// citation display — the batched form of the old per-node query. Recall
/// runs this once per prompt; one statement per live node was an N+1 whose
/// cost grew with lifetime memory size. Superseded nodes are filtered in
/// SQL (same liveness predicate as [`live_nodes`]): recall only looks up
/// live candidates, so loading dead nodes' tags made the scan grow with
/// historical store size for no reader.
pub(crate) fn domains_by_node(
    conn: &Connection,
) -> Result<std::collections::HashMap<i64, Vec<String>>, ContextError> {
    let mut stmt = conn.prepare(
        "SELECT nd.node_id, d.name FROM node_domains nd
         JOIN domain d ON d.id = nd.domain_id
         JOIN node n ON n.id = nd.node_id
         WHERE n.superseded_at IS NULL
         ORDER BY nd.node_id, d.name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    let mut out: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
    for r in rows {
        let (id, name) = r?;
        out.entry(id).or_default().push(name);
    }
    Ok(out)
}

/// Live node ids that carry a domain tag but NONE in `scope` — exactly the
/// set a scoped recall must exclude. Untagged nodes are never returned (they
/// stay candidates): most memories — reflections whose lessons name no domain,
/// episodes from turns that touched no taxonomy-covered file — are untagged,
/// and a scope filter that dropped them would silence recall entirely the
/// moment `stella init` writes a taxonomy. An empty `scope` returns an empty
/// set (nothing is out of scope).
///
/// The out-of-scope test runs in one SQL statement (an anti-join against the
/// in-scope tag set) rather than materializing every tagged id in memory and
/// differencing in Rust — a large initialized workspace can carry many tags
/// unrelated to the active scope.
pub(crate) fn node_ids_excluded_by_scope(
    conn: &Connection,
    scope: &[String],
) -> Result<std::collections::HashSet<i64>, ContextError> {
    if scope.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let placeholders = std::iter::repeat_n("?", scope.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT DISTINCT nd.node_id FROM node_domains nd
         JOIN node n ON n.id = nd.node_id
         WHERE n.superseded_at IS NULL
           AND nd.node_id NOT IN (
             SELECT nd2.node_id FROM node_domains nd2
             JOIN domain d ON d.id = nd2.domain_id
             WHERE d.name IN ({placeholders})
           )"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut set = std::collections::HashSet::new();
    for r in stmt.query_map(rusqlite::params_from_iter(scope.iter()), |r| {
        r.get::<_, i64>(0)
    })? {
        set.insert(r?);
    }
    Ok(set)
}

/// All defined domains as `(name, description)`, for status/inspection.
pub(crate) fn list_domains(
    conn: &Connection,
) -> Result<Vec<(String, Option<String>)>, ContextError> {
    let mut stmt = conn.prepare("SELECT name, description FROM domain ORDER BY name")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Live nodes lacking a vector under `fingerprint`, as `(content_hash, content)`.
/// Deduplicated by content hash so identical content is embedded once.
fn nodes_missing_embedding(
    conn: &Connection,
    fingerprint: &str,
) -> Result<Vec<(String, String)>, ContextError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT n.content_hash, n.content
         FROM node n
         WHERE n.superseded_at IS NULL
           AND n.content <> ''
           AND NOT EXISTS (
               SELECT 1 FROM embedding e
               WHERE e.content_hash = n.content_hash AND e.fingerprint = ?1
           )",
    )?;
    let rows = stmt.query_map(params![fingerprint], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_store() -> (TempDir, ContextStore) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("context.db");
        let store = ContextStore::open(&path).expect("open");
        (dir, store)
    }

    #[test]
    fn open_creates_a_consistent_store_at_schema_version_1() {
        let (_dir, store) = tmp_store();
        store.integrity_check().expect("integrity");
        let conn = store.conn();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn reopening_is_idempotent_and_does_not_remigrate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        {
            let s = ContextStore::open(&path).unwrap();
            let conn = s.conn();
            upsert_node(
                &conn,
                &NodeInput::new(NodeKind::Concept, "keep me"),
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        }
        let s2 = ContextStore::open(&path).unwrap();
        assert_eq!(s2.node_count().unwrap(), 1, "data survives reopen");
        s2.integrity_check().unwrap();
    }

    #[test]
    fn opening_drops_orphaned_code_graph_tables_from_context_db() {
        // A legacy context.db that still carries the code graph's tables (from
        // the era when the tree-sitter index shared this one file) must have
        // them dropped on open — the graph now lives in codegraph.db, and
        // leaving duplicates here is the "two DBs hold the code graph" defect.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(MIGRATION_V2).unwrap();
            // Simulate the orphaned graph tables + a pre-V3 schema version.
            conn.execute_batch(
                "CREATE TABLE code_graph_files (id INTEGER PRIMARY KEY, path TEXT);\
                 CREATE TABLE code_graph_symbols (id INTEGER PRIMARY KEY, file_id INTEGER);\
                 CREATE TABLE code_graph_imports (id INTEGER PRIMARY KEY, from_file_id INTEGER);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
        }
        // Reopen through the store: the V3 migration must evict the orphans.
        let store = ContextStore::open(&path).unwrap();
        let conn = store.conn();
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='table' AND name LIKE 'code_graph_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "orphaned code_graph_* tables must be dropped");
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn upsert_node_updates_content_on_touch_keeping_identity() {
        let (_dir, store) = tmp_store();
        let conn = store.conn();
        let a = upsert_node(
            &conn,
            &NodeInput::new(NodeKind::File, "src/lib.rs")
                .with_uri("file:///repo/src/lib.rs")
                .with_content("v1"),
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let b = upsert_node(
            &conn,
            &NodeInput::new(NodeKind::File, "src/lib.rs")
                .with_uri("file:///repo/src/lib.rs")
                .with_content("v2"),
            "2026-01-02T00:00:00Z",
        )
        .unwrap();
        assert_eq!(a, b, "same identity → same rowid");
        let node = node_by_id(&conn, a).unwrap().unwrap();
        assert_eq!(node.content, "v2");
    }

    #[test]
    fn memory_nodes_and_public_id_lookup_serve_the_inspection_surface() {
        let (_dir, store) = tmp_store();
        {
            let conn = store.conn();
            upsert_node(
                &conn,
                &NodeInput::new(NodeKind::Memory, "prefer rg over grep")
                    .with_content("prefer rg over grep in this workspace"),
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
            upsert_node(
                &conn,
                &NodeInput::new(NodeKind::Memory, "tests live next to code")
                    .with_content("tests live next to code, not in tests/"),
                "2026-01-02T00:00:00Z",
            )
            .unwrap();
            // A non-memory node must never surface in the memory listing.
            upsert_node(
                &conn,
                &NodeInput::new(NodeKind::Concept, "budgeting").with_content("c"),
                "2026-01-03T00:00:00Z",
            )
            .unwrap();
        }
        let memories = store.memory_nodes().unwrap();
        assert_eq!(memories.len(), 2, "only Memory-kind nodes");
        assert_eq!(
            memories[0].display_name, "tests live next to code",
            "newest first"
        );
        assert!(memories.iter().all(|m| m.kind == NodeKind::Memory));

        let looked_up = store
            .node_by_public_id(&memories[0].public_id)
            .unwrap()
            .expect("public id resolves");
        assert_eq!(looked_up.content, "tests live next to code, not in tests/");
        assert!(store.node_by_public_id("nod_missing").unwrap().is_none());
    }

    #[test]
    fn empty_display_name_is_rejected() {
        let (_dir, store) = tmp_store();
        let conn = store.conn();
        let err = upsert_node(
            &conn,
            &NodeInput::new(NodeKind::Concept, "  "),
            "2026-01-01T00:00:00Z",
        )
        .unwrap_err();
        assert!(matches!(err, ContextError::InvalidInput(_)));
    }

    #[test]
    fn vector_blob_roundtrips_little_endian() {
        let v = vec![1.0f32, -2.5, 3.25, 0.0];
        let blob = vector_to_blob(&v);
        assert_eq!(blob.len(), 16);
        assert_eq!(blob_to_vector(&blob).unwrap(), v);
    }

    #[test]
    fn odd_length_blob_is_reported_as_corruption() {
        assert!(matches!(
            blob_to_vector(&[0u8, 1, 2]),
            Err(ContextError::Corruption(_))
        ));
    }

    #[test]
    fn embedding_store_is_idempotent_and_reports_reuse() {
        let (_dir, store) = tmp_store();
        let conn = store.conn();
        let first =
            store_embedding(&conn, "hashA", "fp", &[0.1, 0.2], "2026-01-01T00:00:00Z").unwrap();
        let second =
            store_embedding(&conn, "hashA", "fp", &[0.1, 0.2], "2026-01-01T00:00:00Z").unwrap();
        assert!(first, "first insert writes a row");
        assert!(
            !second,
            "second insert is a no-op (byte-compat reuse, L-C2)"
        );
        assert!(embedding_exists(&conn, "hashA", "fp").unwrap());
        assert!(!embedding_exists(&conn, "hashA", "other-fp").unwrap());
    }

    #[test]
    fn kill_mid_index_rolls_back_to_a_consistent_store() {
        // L-L1: a batch dropped without commit must leave no partial rows.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        {
            let s = ContextStore::open(&path).unwrap();
            let conn = s.conn();
            // Commit one durable node so the file has real content.
            upsert_node(
                &conn,
                &NodeInput::new(NodeKind::Concept, "committed"),
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        }
        {
            // Start a batch, write several rows, then DROP without commit —
            // the stand-in for a kill mid-index.
            let s = ContextStore::open(&path).unwrap();
            let conn = s.conn();
            let tx = conn.unchecked_transaction().unwrap();
            for i in 0..10 {
                upsert_node(
                    &tx,
                    &NodeInput::new(NodeKind::Concept, format!("partial-{i}")),
                    "2026-01-02T00:00:00Z",
                )
                .unwrap();
            }
            drop(tx); // rollback
        }
        let s = ContextStore::open(&path).unwrap();
        s.integrity_check()
            .expect("store must be consistent after a torn write");
        assert_eq!(
            s.node_count().unwrap(),
            1,
            "only the committed node survives; no partial rows"
        );
    }

    /// An embedder that counts how many texts it was asked to embed, wrapping
    /// the real hashing projection. Lets tests prove where embedding work
    /// happens (`L-C1`: never inline on query) and that identical content is
    /// not re-embedded (`L-C2`).
    struct CountingEmbedder {
        inner: crate::embed::HashEmbedder,
        embedded: std::sync::atomic::AtomicUsize,
    }

    impl CountingEmbedder {
        fn new(revision: &str) -> Arc<Self> {
            Arc::new(Self {
                inner: crate::embed::HashEmbedder::with_revision(revision),
                embedded: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn count(&self) -> usize {
            self.embedded.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::embed::Embedder for CountingEmbedder {
        fn fingerprint(&self) -> EmbedderFingerprint {
            self.inner.fingerprint()
        }
        async fn embed(
            &self,
            texts: &[String],
        ) -> Result<Vec<crate::embed::Embedding>, crate::embed::EmbedError> {
            self.embedded.fetch_add(texts.len(), Ordering::SeqCst);
            self.inner.embed(texts).await
        }
    }

    #[tokio::test]
    async fn recall_never_embeds_stored_content_inline_only_the_query() {
        // L-C1: the first query does not pay indexing. Seed content (embedded
        // once at upsert), reset the counter, then a recall must embed exactly
        // ONE text — the query itself — and nothing stored.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        let embedder = CountingEmbedder::new("1");
        let store =
            ContextStore::open_with(&path, embedder.clone(), Arc::new(SystemClock)).unwrap();
        store
            .upsert(
                crate::writeback::ContextDelta::new()
                    .with_node(NodeInput::new(NodeKind::Concept, "a").with_content("alpha content"))
                    .with_node(NodeInput::new(NodeKind::Concept, "b").with_content("beta content")),
            )
            .await
            .unwrap();
        let before = embedder.count();
        let q = contextgraph_types::ContextQuery {
            goal: "find alpha".into(),
            query_text: Some("alpha content".into()),
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames: 10,
            max_tokens: 4000,
            as_of: None,
        };
        store.recall(&q).await.unwrap();
        assert_eq!(
            embedder.count() - before,
            1,
            "recall embeds only the query text, never stored content"
        );
    }

    #[tokio::test]
    async fn open_and_warm_catches_up_embeddings_in_the_background() {
        // L-C1: warm at mount. Seed content under fingerprint rev-1, then mount
        // with a rev-2 embedder whose index is empty for this content. The
        // background warm task embeds it; after joining, a query is
        // vector-grounded (not lexical fallback) — proving warm did the work.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        {
            let a = ContextStore::open_with(
                &path,
                Arc::new(crate::embed::HashEmbedder::with_revision("1")),
                Arc::new(SystemClock),
            )
            .unwrap();
            a.upsert(
                crate::writeback::ContextDelta::new()
                    .with_node(NodeInput::new(NodeKind::Concept, "warmable").with_content(
                        "content that must be re-embedded under the new fingerprint",
                    )),
            )
            .await
            .unwrap();
        }
        let embedder = CountingEmbedder::new("2");
        let store =
            ContextStore::open_and_warm(&path, embedder.clone(), Arc::new(SystemClock)).unwrap();
        let warmed = store.await_warm().await.unwrap();
        assert_eq!(
            warmed, 1,
            "the background task embedded the stale-fingerprint node"
        );
        assert!(
            embedder.count() >= 1,
            "warm did real embedding work off the query path"
        );

        let q = contextgraph_types::ContextQuery {
            goal: "find it".into(),
            query_text: Some("content that must be re-embedded under the new fingerprint".into()),
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames: 5,
            max_tokens: 2000,
            as_of: None,
        };
        let result = store.recall(&q).await.unwrap();
        assert!(
            !result.used_lexical_fallback,
            "after warm, retrieval is vector-grounded"
        );
        assert!(!result.frames.is_empty());
    }

    #[tokio::test]
    async fn scoped_recall_keeps_untagged_nodes_and_drops_out_of_scope_ones() {
        // Regression: the post-`stella init` failure mode. A workspace
        // taxonomy makes every recall domain-scoped; most memories are
        // written untagged (reflections with no domain, episodes touching no
        // covered file). The scope must keep those and exclude only nodes
        // tagged exclusively out of scope — the old hard filter returned
        // zero frames forever once a taxonomy existed.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        let store = ContextStore::open_with(
            &path,
            Arc::new(crate::embed::HashEmbedder::default()),
            Arc::new(SystemClock),
        )
        .unwrap();
        store
            .upsert(
                crate::writeback::ContextDelta::new()
                    .with_node(
                        NodeInput::new(NodeKind::Concept, "untagged-lesson")
                            .with_content("prefer rg over grep in shell commands"),
                    )
                    .with_node(
                        NodeInput::new(NodeKind::Concept, "in-scope")
                            .with_content("billing retries use exponential backoff")
                            .with_domains(["billing".to_string()]),
                    )
                    .with_node(
                        NodeInput::new(NodeKind::Concept, "out-of-scope")
                            .with_content("frontend uses tailwind for styling")
                            .with_domains(["frontend".to_string()]),
                    ),
            )
            .await
            .unwrap();

        let q = contextgraph_types::ContextQuery {
            goal: "recall everything".into(),
            query_text: Some("prefer rg over grep billing retries".into()),
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames: 10,
            max_tokens: 4000,
            as_of: None,
        };
        let result = store
            .recall_scoped(&q, &["billing".to_string()])
            .await
            .unwrap();
        let titles: Vec<&str> = result.frames.iter().map(|f| f.title.as_str()).collect();
        assert!(
            titles.contains(&"untagged-lesson"),
            "untagged nodes must survive a domain scope: {titles:?}"
        );
        assert!(
            !titles.contains(&"out-of-scope"),
            "nodes tagged exclusively out of scope must be excluded: {titles:?}"
        );
    }

    #[tokio::test]
    async fn warm_now_embeds_only_content_missing_under_the_active_fingerprint() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        let store = ContextStore::open(&path).unwrap();
        {
            let conn = store.conn();
            upsert_node(
                &conn,
                &NodeInput::new(NodeKind::Concept, "thing").with_content("real content here"),
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        }
        let n = store.warm_now().await.unwrap();
        assert_eq!(n, 1, "one node embedded");
        // A second warm is a no-op — the vector already exists.
        assert_eq!(store.warm_now().await.unwrap(), 0);
    }
}
