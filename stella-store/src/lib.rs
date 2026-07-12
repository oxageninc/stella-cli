//! `stella-store` — the session's local DuckDB database at
//! `.stella/stella.duckdb`: durable state, full execution records, and
//! analytics-grade telemetry, all on the user's disk (no server, no
//! account — the local-first non-negotiable).
//!
//! What lives here:
//! - **executions** — one row per run/goal/turn: prompt, provider/model,
//!   outcome, total cost.
//! - **events** — the COMPLETE `AgentEvent` stream per execution, one JSON
//!   row per event in order, including `Reasoning` deltas — the full chain
//!   of thought is replayable against its execution.
//! - **telemetry** — one row per committed model call (from `StepUsage`):
//!   provider, model, tokens in/out, cache read hits, cache misses, cache
//!   writes, cost (computed from the model card's pricing × token counts
//!   in the adapter), duration, retries, tool-call count.
//! - **files_touched** — the CRUD ledger per execution.
//! - **file_locks** — cooperative file claims for multi-agent work.
//! - **graph_nodes / graph_edges** — the seam the Phase 3 context plane
//!   (embeddings for md/mdx/txt/doc/docx, code graph) writes into.
//!
//! # Concurrency
//!
//! DuckDB is single-writer per database file. One `Store` per session
//! process is the contract; internally a `Mutex<Connection>` serializes
//! writers across in-process parallel agents. Multi-PROCESS fleets need a
//! lock-holder (or one store per worker + merge) — documented limitation,
//! not a silent one.
//!
//! # Graceful degradation
//!
//! Every method returns `Result`; the CLI treats a failed store as
//! observability loss, never a work stoppage — it warns once and keeps
//! running (persistence must never take the agent down with it).

use std::path::Path;
use std::sync::Mutex;

use duckdb::{Connection, params};
use stella_protocol::AgentEvent;

/// Wrapper error: everything the store can fail with, rendered.
#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "store: {}", self.0)
    }
}
impl std::error::Error for StoreError {}

impl From<duckdb::Error> for StoreError {
    fn from(e: duckdb::Error) -> Self {
        StoreError(e.to_string())
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// One StepUsage-shaped telemetry record (mirrors the event, plus the
/// derived cache-miss column so analytics never re-derive it).
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryRow {
    pub step: u64,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_miss_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub retries: u32,
    pub tool_calls: u64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) the workspace database at
    /// `.stella/stella.duckdb` and apply the schema.
    pub fn open(workspace_root: &Path) -> Result<Self> {
        let dir = workspace_root.join(".stella");
        std::fs::create_dir_all(&dir).map_err(|e| StoreError(e.to_string()))?;
        let conn = Connection::open(dir.join("stella.duckdb"))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// In-memory store — tests and ephemeral runs.
    pub fn in_memory() -> Result<Self> {
        let store = Self {
            conn: Mutex::new(Connection::open_in_memory()?),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute_batch(
            "CREATE SEQUENCE IF NOT EXISTS execution_seq;
             CREATE TABLE IF NOT EXISTS executions (
               id BIGINT PRIMARY KEY DEFAULT nextval('execution_seq'),
               kind TEXT NOT NULL,
               prompt TEXT NOT NULL,
               provider TEXT NOT NULL,
               model TEXT NOT NULL,
               started_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
               finished_at TIMESTAMP,
               outcome TEXT,
               cost_usd DOUBLE NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS events (
               execution_id BIGINT NOT NULL,
               seq BIGINT NOT NULL,
               ts TIMESTAMP NOT NULL DEFAULT current_timestamp,
               event_type TEXT NOT NULL,
               payload JSON NOT NULL
             );
             CREATE TABLE IF NOT EXISTS telemetry (
               execution_id BIGINT NOT NULL,
               step BIGINT NOT NULL,
               ts TIMESTAMP NOT NULL DEFAULT current_timestamp,
               provider TEXT NOT NULL,
               model TEXT NOT NULL,
               input_tokens BIGINT NOT NULL,
               output_tokens BIGINT NOT NULL,
               cache_read_tokens BIGINT NOT NULL,
               cache_miss_tokens BIGINT NOT NULL,
               cache_write_tokens BIGINT NOT NULL DEFAULT 0,
               cost_usd DOUBLE NOT NULL,
               duration_ms BIGINT NOT NULL,
               retries INTEGER NOT NULL,
               tool_calls BIGINT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS files_touched (
               execution_id BIGINT NOT NULL,
               path TEXT NOT NULL,
               ops TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS file_locks (
               path TEXT PRIMARY KEY,
               holder TEXT NOT NULL,
               acquired_at TIMESTAMP NOT NULL DEFAULT current_timestamp
             );
             CREATE TABLE IF NOT EXISTS graph_nodes (
               id TEXT PRIMARY KEY,
               label TEXT NOT NULL,
               properties JSON NOT NULL DEFAULT '{}'
             );
             CREATE TABLE IF NOT EXISTS graph_edges (
               src TEXT NOT NULL,
               dst TEXT NOT NULL,
               edge_type TEXT NOT NULL,
               properties JSON NOT NULL DEFAULT '{}'
             );",
        )?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A poisoned mutex means a panic mid-write; the connection itself
        // is still usable and refusing all further persistence would turn
        // one bad write into total observability loss.
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start an execution record; returns its id.
    pub fn begin_execution(
        &self,
        kind: &str,
        prompt: &str,
        provider: &str,
        model: &str,
    ) -> Result<i64> {
        let conn = self.lock();
        let id: i64 = conn.query_row(
            "INSERT INTO executions (kind, prompt, provider, model) VALUES (?, ?, ?, ?) \
             RETURNING id",
            params![kind, prompt, provider, model],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Append one event to the execution's stream. `seq` is the caller's
    /// monotonically increasing counter (the event drain loop owns order).
    pub fn record_event(&self, execution_id: i64, seq: u64, event: &AgentEvent) -> Result<()> {
        let payload = serde_json::to_string(event).map_err(|e| StoreError(e.to_string()))?;
        // Read the internally-tagged `type` from the parsed value rather than
        // string-scanning for the first `"type":"` literal — the scan silently
        // yields the wrong tag (or "unknown") if serialization is ever
        // pretty-printed, wrapped, or reordered.
        let event_type = serde_json::from_str::<serde_json::Value>(&payload)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_else(|| "unknown".into());
        self.lock().execute(
            "INSERT INTO events (execution_id, seq, event_type, payload) VALUES (?, ?, ?, ?)",
            params![execution_id, seq as i64, event_type, payload],
        )?;
        Ok(())
    }

    /// Record one model call's telemetry.
    pub fn record_telemetry(&self, execution_id: i64, row: &TelemetryRow) -> Result<()> {
        self.lock().execute(
            "INSERT INTO telemetry (execution_id, step, provider, model, input_tokens, \
             output_tokens, cache_read_tokens, cache_miss_tokens, cache_write_tokens, cost_usd, \
             duration_ms, retries, tool_calls) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                execution_id,
                row.step as i64,
                row.provider,
                row.model,
                row.input_tokens as i64,
                row.output_tokens as i64,
                row.cache_read_tokens as i64,
                row.cache_miss_tokens as i64,
                row.cache_write_tokens as i64,
                row.cost_usd,
                row.duration_ms as i64,
                row.retries,
                row.tool_calls as i64,
            ],
        )?;
        Ok(())
    }

    /// Persist the CRUD ledger for an execution.
    pub fn record_files_touched(
        &self,
        execution_id: i64,
        files: &[(String, String)],
    ) -> Result<()> {
        let conn = self.lock();
        for (path, ops) in files {
            conn.execute(
                "INSERT INTO files_touched (execution_id, path, ops) VALUES (?, ?, ?)",
                params![execution_id, path, ops],
            )?;
        }
        Ok(())
    }

    /// Close an execution record.
    pub fn finish_execution(&self, execution_id: i64, outcome: &str, cost_usd: f64) -> Result<()> {
        self.lock().execute(
            "UPDATE executions SET finished_at = current_timestamp, outcome = ?, cost_usd = ? \
             WHERE id = ?",
            params![outcome, cost_usd, execution_id],
        )?;
        Ok(())
    }

    /// Cooperative file lock: succeeds only if `path` is unclaimed or
    /// already held by `holder` (re-entrant). Returns whether the lock is
    /// now held.
    pub fn acquire_file_lock(&self, path: &str, holder: &str) -> Result<bool> {
        let conn = self.lock();
        // Only "no such lock row" means unclaimed. A genuine query error must
        // propagate — `.ok()` would misread it as unclaimed and drive a
        // spurious INSERT (then a PK conflict), corrupting lock state.
        let existing: Option<String> = match conn.query_row(
            "SELECT holder FROM file_locks WHERE path = ?",
            params![path],
            |row| row.get(0),
        ) {
            Ok(holder) => Some(holder),
            Err(duckdb::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e.into()),
        };
        match existing {
            Some(current) => Ok(current == holder),
            None => {
                conn.execute(
                    "INSERT INTO file_locks (path, holder) VALUES (?, ?)",
                    params![path, holder],
                )?;
                Ok(true)
            }
        }
    }

    /// Release a lock (only the holder's release removes it).
    pub fn release_file_lock(&self, path: &str, holder: &str) -> Result<()> {
        self.lock().execute(
            "DELETE FROM file_locks WHERE path = ? AND holder = ?",
            params![path, holder],
        )?;
        Ok(())
    }

    /// Upsert a graph node — the context plane's write seam.
    pub fn upsert_graph_node(&self, id: &str, label: &str, properties: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO graph_nodes (id, label, properties) VALUES (?, ?, ?) \
             ON CONFLICT (id) DO UPDATE SET label = excluded.label, \
             properties = excluded.properties",
            params![id, label, properties],
        )?;
        Ok(())
    }

    /// Insert a graph edge.
    pub fn insert_graph_edge(
        &self,
        src: &str,
        dst: &str,
        edge_type: &str,
        properties: &str,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT INTO graph_edges (src, dst, edge_type, properties) VALUES (?, ?, ?, ?)",
            params![src, dst, edge_type, properties],
        )?;
        Ok(())
    }

    /// Count helper used by tests and `stella config`-style introspection.
    pub fn count(&self, table: &str) -> Result<i64> {
        // Table names can't be bound parameters; allowlist them.
        const TABLES: [&str; 7] = [
            "executions",
            "events",
            "telemetry",
            "files_touched",
            "file_locks",
            "graph_nodes",
            "graph_edges",
        ];
        if !TABLES.contains(&table) {
            return Err(StoreError(format!("unknown table `{table}`")));
        }
        let count: i64 =
            self.lock()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_lifecycle_events_and_telemetry_roundtrip() {
        let store = Store::in_memory().unwrap();
        let id = store
            .begin_execution("goal", "make tests pass", "zai", "glm-5.2")
            .unwrap();

        // Full event stream including chain of thought.
        store
            .record_event(
                id,
                0,
                &AgentEvent::Reasoning {
                    delta: "first I will read the failing test".into(),
                },
            )
            .unwrap();
        store
            .record_event(
                id,
                1,
                &AgentEvent::Text {
                    delta: "done".into(),
                },
            )
            .unwrap();
        assert_eq!(store.count("events").unwrap(), 2);

        store
            .record_telemetry(
                id,
                &TelemetryRow {
                    step: 0,
                    provider: "zai".into(),
                    model: "glm-5.2".into(),
                    input_tokens: 12_000,
                    output_tokens: 400,
                    cache_read_tokens: 9_000,
                    cache_miss_tokens: 3_000,
                    cache_write_tokens: 0,
                    cost_usd: 0.0042,
                    duration_ms: 1_830,
                    retries: 1,
                    tool_calls: 3,
                },
            )
            .unwrap();
        store
            .record_files_touched(id, &[("src/main.rs".into(), "RU".into())])
            .unwrap();
        store.finish_execution(id, "completed", 0.0042).unwrap();

        assert_eq!(store.count("telemetry").unwrap(), 1);
        assert_eq!(store.count("files_touched").unwrap(), 1);
        assert_eq!(store.count("executions").unwrap(), 1);
    }

    #[test]
    fn file_locks_are_exclusive_and_reentrant() {
        let store = Store::in_memory().unwrap();
        assert!(store.acquire_file_lock("src/a.rs", "agent-1").unwrap());
        assert!(
            store.acquire_file_lock("src/a.rs", "agent-1").unwrap(),
            "re-entrant"
        );
        assert!(
            !store.acquire_file_lock("src/a.rs", "agent-2").unwrap(),
            "exclusive"
        );

        // Only the holder's release frees it.
        store.release_file_lock("src/a.rs", "agent-2").unwrap();
        assert!(!store.acquire_file_lock("src/a.rs", "agent-2").unwrap());
        store.release_file_lock("src/a.rs", "agent-1").unwrap();
        assert!(store.acquire_file_lock("src/a.rs", "agent-2").unwrap());
    }

    #[test]
    fn graph_seam_upserts_nodes_and_edges() {
        let store = Store::in_memory().unwrap();
        store
            .upsert_graph_node("doc:readme", "Document", r#"{"path":"README.md"}"#)
            .unwrap();
        store
            .upsert_graph_node("doc:readme", "Document", r#"{"path":"README.md","v":2}"#)
            .unwrap();
        store
            .insert_graph_edge("doc:readme", "sym:main", "mentions", "{}")
            .unwrap();
        assert_eq!(
            store.count("graph_nodes").unwrap(),
            1,
            "upsert, not duplicate"
        );
        assert_eq!(store.count("graph_edges").unwrap(), 1);
    }

    #[test]
    fn count_rejects_unknown_tables() {
        let store = Store::in_memory().unwrap();
        assert!(store.count("users; DROP TABLE executions").is_err());
    }

    #[test]
    fn on_disk_store_persists_across_reopen() {
        let root = std::env::temp_dir().join(format!("stella_store_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        {
            let store = Store::open(&root).unwrap();
            store
                .begin_execution("run", "hello", "anthropic", "claude-fable-5")
                .unwrap();
        }
        {
            let store = Store::open(&root).unwrap();
            assert_eq!(store.count("executions").unwrap(), 1);
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
