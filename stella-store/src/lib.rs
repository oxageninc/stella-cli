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
//! - **file_locks** — schema + API for cooperative file claims in multi-agent
//!   work. Reserved: no shipping command acquires locks yet.
//! - **graph_nodes / graph_edges** — schema reserved as a future seam for a
//!   context plane; not written by any shipping command today (`stella-context`
//!   and `stella-graph` currently use their own stores).
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
use serde::Serialize;
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

/// One aggregated analytics row per (provider, model): the numbers behind
/// "$-per-resolved-task" receipts, straight from local telemetry.
///
/// Field order is the serialization contract for `stella stats --format
/// json|csv` — append new fields at the end, never reorder.
///
/// `division` is the Arena division *derivable from stored data alone*:
/// provider `local` runs are provably Off-grid (`"off-grid"`); every other
/// provider gets `"-"`. Heavyweight/Featherweight are claims about model
/// class and per-task caps that the store does not record, so they are
/// never inferred here.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageStatsRow {
    pub provider: String,
    pub model: String,
    /// Arena division: `"off-grid"` for provider `local`, else `"-"`.
    pub division: String,
    /// Executions recorded (any outcome, including still-open ones).
    pub runs: i64,
    /// Executions with outcome `completed` — the "resolved" count.
    pub resolved: i64,
    /// `resolved / runs` (a group always has ≥ 1 run).
    pub resolve_rate: f64,
    /// Sum of `executions.cost_usd` — the authoritative per-run total.
    pub total_cost_usd: f64,
    /// `total_cost_usd / resolved`; `None` when nothing resolved — a
    /// division by zero is never papered over with a fake number.
    pub cost_per_resolved_usd: Option<f64>,
    /// Token sums from `telemetry` (zero when a run recorded none).
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Mean model-call wall time per run: `sum(duration_ms) / runs`.
    pub avg_duration_ms: f64,
}

impl UsageStatsRow {
    /// Arena division derivable from the provider id alone (see the struct
    /// docs for why only Off-grid is ever inferred).
    pub fn division_for_provider(provider: &str) -> &'static str {
        if provider == "local" { "off-grid" } else { "-" }
    }
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

    /// Aggregate usage/cost analytics per (provider, model) — the data
    /// behind `stella stats` and every Arena "$-per-resolved-task" receipt.
    ///
    /// Semantics:
    /// - One output row per distinct `executions.(provider, model)` pair;
    ///   telemetry is attributed to its execution's provider/model.
    /// - `runs` counts every execution; `resolved` counts
    ///   `outcome = 'completed'` (aborted and still-open runs are not
    ///   resolved).
    /// - Cost comes from `executions.cost_usd` (the per-run total written
    ///   at finish); token and duration sums come from `telemetry`,
    ///   pre-aggregated per execution before the join so a multi-step run
    ///   can never fan out the executions side.
    /// - `cost_per_resolved_usd` is `None` when `resolved = 0`.
    /// - Rows are ordered by total cost descending (ties broken by
    ///   provider, then model, so output is deterministic).
    ///
    /// Division mapping: only provider `local` maps to an Arena division
    /// (`off-grid`); all other rows carry `"-"` — see [`UsageStatsRow`].
    pub fn usage_stats(&self) -> Result<Vec<UsageStatsRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT e.provider,
                    e.model,
                    count(*) AS runs,
                    count(*) FILTER (WHERE e.outcome = 'completed') AS resolved,
                    coalesce(sum(e.cost_usd), 0) AS total_cost_usd,
                    CAST(coalesce(sum(t.input_tokens), 0) AS BIGINT) AS input_tokens,
                    CAST(coalesce(sum(t.output_tokens), 0) AS BIGINT) AS output_tokens,
                    CAST(coalesce(sum(t.cache_read_tokens), 0) AS BIGINT) AS cache_read_tokens,
                    CAST(coalesce(sum(t.cache_write_tokens), 0) AS BIGINT) AS cache_write_tokens,
                    CAST(coalesce(sum(t.duration_ms), 0) AS BIGINT) AS total_duration_ms
             FROM executions e
             LEFT JOIN (
               SELECT execution_id,
                      sum(input_tokens) AS input_tokens,
                      sum(output_tokens) AS output_tokens,
                      sum(cache_read_tokens) AS cache_read_tokens,
                      sum(cache_write_tokens) AS cache_write_tokens,
                      sum(duration_ms) AS duration_ms
               FROM telemetry
               GROUP BY execution_id
             ) t ON t.execution_id = e.id
             GROUP BY e.provider, e.model
             ORDER BY total_cost_usd DESC, e.provider ASC, e.model ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let provider: String = row.get(0)?;
            let model: String = row.get(1)?;
            let runs: i64 = row.get(2)?;
            let resolved: i64 = row.get(3)?;
            let total_cost_usd: f64 = row.get(4)?;
            let input_tokens: i64 = row.get(5)?;
            let output_tokens: i64 = row.get(6)?;
            let cache_read_tokens: i64 = row.get(7)?;
            let cache_write_tokens: i64 = row.get(8)?;
            let total_duration_ms: i64 = row.get(9)?;
            let division = UsageStatsRow::division_for_provider(&provider).to_string();
            Ok(UsageStatsRow {
                provider,
                model,
                division,
                runs,
                resolved,
                // A GROUP BY group always holds ≥ 1 execution row.
                resolve_rate: resolved as f64 / runs as f64,
                total_cost_usd,
                cost_per_resolved_usd: (resolved > 0).then(|| total_cost_usd / resolved as f64),
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                avg_duration_ms: total_duration_ms as f64 / runs as f64,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
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

    /// Test-only shorthand: a telemetry row with just the analytics-relevant
    /// fields set.
    fn telemetry(
        step: u64,
        provider: &str,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cost: f64,
        duration_ms: u64,
    ) -> TelemetryRow {
        TelemetryRow {
            step,
            provider: provider.into(),
            model: model.into(),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_miss_tokens: input.saturating_sub(cache_read),
            cache_write_tokens: cache_write,
            cost_usd: cost,
            duration_ms,
            retries: 0,
            tool_calls: 0,
        }
    }

    /// Fixture: three providers with mixed outcomes.
    /// - anthropic: 1 aborted run, cost 0.05 → resolved = 0.
    /// - zai: 2 completed (0.02 + 0.01), 1 aborted, 1 never finished.
    /// - local: 1 completed at $0 → the Off-grid division.
    fn seeded_store() -> Store {
        let store = Store::in_memory().unwrap();

        let a = store
            .begin_execution("run", "p1", "anthropic", "claude-fable-5")
            .unwrap();
        store.finish_execution(a, "aborted", 0.05).unwrap();

        let z1 = store
            .begin_execution("run", "p2", "zai", "glm-5.2")
            .unwrap();
        store
            .record_telemetry(
                z1,
                &telemetry(0, "zai", "glm-5.2", 1000, 100, 500, 10, 0.01, 1000),
            )
            .unwrap();
        store
            .record_telemetry(
                z1,
                &telemetry(1, "zai", "glm-5.2", 2000, 200, 500, 0, 0.01, 500),
            )
            .unwrap();
        store.finish_execution(z1, "completed", 0.02).unwrap();

        let z2 = store
            .begin_execution("run", "p3", "zai", "glm-5.2")
            .unwrap();
        store
            .record_telemetry(
                z2,
                &telemetry(0, "zai", "glm-5.2", 3000, 300, 1000, 0, 0.01, 1500),
            )
            .unwrap();
        store.finish_execution(z2, "completed", 0.01).unwrap();

        // Aborted with no telemetry (LEFT JOIN's zero path) and a run that
        // never finished (outcome NULL) — both count as runs, not resolved.
        let z3 = store
            .begin_execution("run", "p4", "zai", "glm-5.2")
            .unwrap();
        store.finish_execution(z3, "aborted", 0.0).unwrap();
        store
            .begin_execution("run", "p5", "zai", "glm-5.2")
            .unwrap();

        let l = store
            .begin_execution("run", "p6", "local", "llama-3.3")
            .unwrap();
        store
            .record_telemetry(
                l,
                &telemetry(0, "local", "llama-3.3", 500, 50, 0, 0, 0.0, 2000),
            )
            .unwrap();
        store.finish_execution(l, "completed", 0.0).unwrap();

        store
    }

    #[test]
    fn usage_stats_aggregates_per_provider_model() {
        let store = seeded_store();
        let rows = store.usage_stats().unwrap();
        assert_eq!(rows.len(), 3);

        // Ordered by total cost desc: anthropic 0.05, zai 0.03, local 0.0.
        assert_eq!(
            rows.iter().map(|r| r.provider.as_str()).collect::<Vec<_>>(),
            ["anthropic", "zai", "local"]
        );

        let zai = &rows[1];
        assert_eq!(zai.model, "glm-5.2");
        assert_eq!(zai.division, "-");
        assert_eq!(zai.runs, 4);
        assert_eq!(zai.resolved, 2);
        assert!((zai.resolve_rate - 0.5).abs() < 1e-12);
        assert!((zai.total_cost_usd - 0.03).abs() < 1e-12);
        assert!((zai.cost_per_resolved_usd.unwrap() - 0.015).abs() < 1e-12);
        assert_eq!(zai.input_tokens, 6000);
        assert_eq!(zai.output_tokens, 600);
        assert_eq!(zai.cache_read_tokens, 2000);
        assert_eq!(zai.cache_write_tokens, 10);
        assert!((zai.avg_duration_ms - 750.0).abs() < 1e-12);
    }

    #[test]
    fn usage_stats_never_divides_by_zero_resolved() {
        let store = seeded_store();
        let rows = store.usage_stats().unwrap();
        let anthropic = &rows[0];
        assert_eq!(anthropic.runs, 1);
        assert_eq!(anthropic.resolved, 0);
        assert_eq!(anthropic.resolve_rate, 0.0);
        assert!((anthropic.total_cost_usd - 0.05).abs() < 1e-12);
        assert_eq!(
            anthropic.cost_per_resolved_usd, None,
            "resolved = 0 must yield None, never a fake number"
        );
        // No telemetry rows at all → token/duration sums are zero.
        assert_eq!(anthropic.input_tokens, 0);
        assert_eq!(anthropic.avg_duration_ms, 0.0);
    }

    #[test]
    fn usage_stats_maps_local_provider_to_off_grid_division() {
        let store = seeded_store();
        let rows = store.usage_stats().unwrap();
        let local = &rows[2];
        assert_eq!(local.provider, "local");
        assert_eq!(local.division, "off-grid");
        assert_eq!(local.resolve_rate, 1.0);
        assert_eq!(local.cost_per_resolved_usd, Some(0.0));
        assert_eq!(UsageStatsRow::division_for_provider("local"), "off-grid");
        assert_eq!(UsageStatsRow::division_for_provider("anthropic"), "-");
        assert_eq!(UsageStatsRow::division_for_provider("openrouter"), "-");
    }

    #[test]
    fn usage_stats_empty_store_returns_no_rows() {
        let store = Store::in_memory().unwrap();
        assert!(store.usage_stats().unwrap().is_empty());
    }

    #[test]
    fn usage_stats_row_serializes_with_stable_field_order() {
        let row = UsageStatsRow {
            provider: "anthropic".into(),
            model: "claude-fable-5".into(),
            division: "-".into(),
            runs: 1,
            resolved: 0,
            resolve_rate: 0.0,
            total_cost_usd: 0.05,
            cost_per_resolved_usd: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            avg_duration_ms: 0.0,
        };
        // Exact string: field ORDER is the machine contract for json/csv
        // receipts, and resolved = 0 must serialize as null (not 0 or NaN).
        assert_eq!(
            serde_json::to_string(&row).unwrap(),
            r#"{"provider":"anthropic","model":"claude-fable-5","division":"-","runs":1,"resolved":0,"resolve_rate":0.0,"total_cost_usd":0.05,"cost_per_resolved_usd":null,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"avg_duration_ms":0.0}"#
        );
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
