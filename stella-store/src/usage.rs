//! `usage.db` — the **user-tier** telemetry hub, one database per
//! developer (not per project), living at `~/.stella/usage.db`. It is a
//! *derived* store: every project's
//! `.stella/private/store.db` is the source of truth, and each finished turn is rolled
//! up here so a future cross-project dashboard can answer "how do I actually
//! use Stella, across all my repos?" without opening every project database.
//!
//! Privacy: this tier stores **metadata and rollups**, never source code or
//! tool outputs. Prompts are reduced to a digest plus a short preview.
//!
//! Direction of flow is one-way: `store.db` → `usage.db`. Nothing here writes
//! back to a project store, and a missing/again-openable `usage.db` never
//! blocks a turn — sync is best-effort.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, params};

use crate::Result;

/// The user-tier stella data dir (usage rollup, session registry,
/// notifications, enterprise spool). `STELLA_DATA_DIR` overrides; otherwise
/// `~/.stella` on every platform (see [`crate::home::stella_home`]) — no
/// platform-specific guessing.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("STELLA_DATA_DIR") {
        return PathBuf::from(dir);
    }
    crate::home::stella_home().unwrap_or_else(|| PathBuf::from("."))
}

/// Where the user-tier aggregate lives: `data_dir()/usage.db`.
pub fn usage_db_path() -> PathBuf {
    data_dir().join("usage.db")
}

/// A stable, dependency-free project identity: FNV-1a/64 of the canonical
/// workspace path, hex-encoded. Deterministic across runs and processes so the
/// same repo always rolls up under one id. (Not cryptographic — it only needs
/// to be stable and collision-resistant for a handful of local paths.)
pub fn project_id_for(root: &Path) -> String {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let s = canon.to_string_lossy();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// One per-tool bucket for the usage histogram (the "you grep symbols a lot but
/// never call graph_query" signal). `calls`/`errors` for one execution; the
/// aggregate is accumulated per (project, tool, surface, day).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBucket {
    pub tool: String,
    pub surface: String,
    pub calls: i64,
    pub errors: i64,
}

/// Everything the user tier records for one finished turn. Assembled from a
/// project `Store` (see `Store::execution_rollup`) and handed to
/// [`UsageStore::sync_execution`]. Carries no source content — only metadata,
/// a prompt digest + short preview, and rolled-up counts.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionRollupRow {
    pub project_id: String,
    pub project_name: String,
    pub project_root: String,
    pub execution_id: i64,
    pub kind: String,
    pub prompt_digest: String,
    pub prompt_preview: String,
    pub model: String,
    pub provider: String,
    pub outcome: String,
    pub cost_usd: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub duration_ms: i64,
    pub tool_calls: i64,
    pub files_written: i64,
    pub produced_output: bool,
    /// False when any paid-call envelope or persistence boundary is unknown.
    pub usage_complete: bool,
    pub self_rating: Option<i64>,
    pub started_at: String,
    /// The turn's day bucket (`YYYY-MM-DD`, from `started_at`) for the rollups.
    pub day: String,
    /// Per-tool buckets for this turn, folded into `tool_usage_rollup`.
    pub tool_histogram: Vec<ToolBucket>,
}

const USAGE_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS projects (
    project_id    TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    root_path     TEXT NOT NULL,
    first_seen_at TEXT NOT NULL,
    last_seen_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS execution_rollup (
    project_id      TEXT NOT NULL,
    execution_id    INTEGER NOT NULL,
    kind            TEXT NOT NULL,
    prompt_digest   TEXT NOT NULL,
    prompt_preview  TEXT NOT NULL DEFAULT '',
    model           TEXT NOT NULL,
    provider        TEXT NOT NULL,
    outcome         TEXT NOT NULL,
    cost_usd        REAL NOT NULL,
    input_tokens    INTEGER NOT NULL,
    output_tokens   INTEGER NOT NULL,
    duration_ms     INTEGER NOT NULL,
    tool_calls      INTEGER NOT NULL,
    files_written   INTEGER NOT NULL,
    produced_output INTEGER NOT NULL,
    self_rating     INTEGER,
    started_at      TEXT NOT NULL,
    PRIMARY KEY (project_id, execution_id)
);
CREATE INDEX IF NOT EXISTS execution_rollup_by_model
    ON execution_rollup(model, project_id);
CREATE TABLE IF NOT EXISTS tool_usage_rollup (
    project_id TEXT NOT NULL,
    tool       TEXT NOT NULL,
    surface    TEXT NOT NULL,
    day        TEXT NOT NULL,
    calls      INTEGER NOT NULL DEFAULT 0,
    errors     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (project_id, tool, surface, day)
);
CREATE TABLE IF NOT EXISTS telemetry (
    project_id     TEXT NOT NULL,
    source_rowid   INTEGER NOT NULL,
    org_id         TEXT,
    workspace_id   TEXT,
    repo_id        TEXT NOT NULL DEFAULT '',
    execution_id   INTEGER NOT NULL,
    step           INTEGER NOT NULL,
    recorded_at    TEXT NOT NULL DEFAULT '',
    provider       TEXT NOT NULL,
    call_role      TEXT NOT NULL,
    model          TEXT NOT NULL,
    input_tokens   INTEGER NOT NULL,
    estimated_input_tokens INTEGER NOT NULL,
    output_tokens  INTEGER NOT NULL,
    cache_read_tokens  INTEGER NOT NULL,
    cache_miss_tokens  INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    cost_usd       REAL NOT NULL,
    duration_ms    INTEGER NOT NULL,
    retries        INTEGER NOT NULL,
    tool_calls     INTEGER NOT NULL,
    usage_complete INTEGER NOT NULL,
    PRIMARY KEY (project_id, source_rowid)
);
CREATE INDEX IF NOT EXISTS telemetry_by_org
    ON telemetry(org_id, recorded_at);
CREATE INDEX IF NOT EXISTS telemetry_by_model
    ON telemetry(provider, model);
CREATE TABLE IF NOT EXISTS telemetry_sync_cursors (
    project_id        TEXT PRIMARY KEY,
    last_source_rowid INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS cloud_sync_cursors (
    org_id         TEXT PRIMARY KEY,
    last_hub_rowid INTEGER NOT NULL DEFAULT 0
);
";

/// The user-tier aggregate store. Read/write, loopback-local, no server.
pub struct UsageStore {
    conn: Mutex<Connection>,
}

impl UsageStore {
    /// Open (creating dirs + schema) the per-user `usage.db` at the default
    /// location, migrating the legacy split layout into `~/.stella` first.
    /// Best-effort callers treat an `Err` as "no cross-project aggregation
    /// this run".
    pub fn open_default() -> Result<Self> {
        crate::home::migrate_legacy_global_dirs();
        Self::open_at(&usage_db_path())
    }

    /// Open (creating parent dirs + schema) at an explicit path.
    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            crate::ensure_private_dir(parent)?;
        }
        Self::init(crate::open_private_sqlite(path)?)
    }

    /// In-memory aggregate — tests and ephemeral runs.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(USAGE_SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Roll one finished turn up into the aggregate: upsert its project, insert
    /// (or replace) the execution rollup, and fold the tool histogram into the
    /// per-day counts. One transaction; idempotent on (project_id,
    /// execution_id) so a re-sync (e.g. `stella usage sync`) never double-counts
    /// the execution rollup. Tool-day counts are additive, so a backfill must
    /// run against a fresh aggregate (documented on the sync command).
    pub fn sync_execution(&self, r: &ExecutionRollupRow) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        // Upsert the project (first_seen sticks; last_seen advances).
        tx.execute(
            "INSERT INTO projects (project_id, name, root_path, first_seen_at, last_seen_at) \
             VALUES (?1, ?2, ?3, ?4, ?4) \
             ON CONFLICT(project_id) DO UPDATE SET \
               name = excluded.name, \
               root_path = excluded.root_path, \
               last_seen_at = excluded.last_seen_at",
            params![r.project_id, r.project_name, r.project_root, r.started_at],
        )?;
        // Was this execution already rolled up? If so, its tool counts were too
        // — skip the additive fold to stay idempotent.
        let already: bool = tx
            .query_row(
                "SELECT 1 FROM execution_rollup WHERE project_id = ?1 AND execution_id = ?2",
                params![r.project_id, r.execution_id],
                |_| Ok(()),
            )
            .is_ok();
        tx.execute(
            "INSERT OR REPLACE INTO execution_rollup \
             (project_id, execution_id, kind, prompt_digest, prompt_preview, model, provider, \
              outcome, cost_usd, input_tokens, output_tokens, duration_ms, tool_calls, \
              files_written, produced_output, self_rating, started_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                r.project_id,
                r.execution_id,
                r.kind,
                r.prompt_digest,
                r.prompt_preview,
                r.model,
                r.provider,
                r.outcome,
                r.cost_usd,
                r.input_tokens,
                r.output_tokens,
                r.duration_ms,
                r.tool_calls,
                r.files_written,
                r.produced_output as i64,
                r.self_rating,
                r.started_at,
            ],
        )?;
        if !already {
            for b in &r.tool_histogram {
                tx.execute(
                    "INSERT INTO tool_usage_rollup (project_id, tool, surface, day, calls, errors) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT(project_id, tool, surface, day) DO UPDATE SET \
                       calls = calls + excluded.calls, \
                       errors = errors + excluded.errors",
                    params![r.project_id, b.tool, b.surface, r.day, b.calls, b.errors],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Number of projects known to the aggregate.
    pub fn project_count(&self) -> Result<i64> {
        Ok(self
            .lock()
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))?)
    }

    /// Cross-project per-tool call totals, most-used first — the histogram a
    /// dashboard/recommender reads to spot "grep a lot, graph_query never".
    pub fn tool_totals(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT tool, SUM(calls) AS c FROM tool_usage_rollup \
             GROUP BY tool ORDER BY c DESC, tool ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Count of rolled-up executions for a project.
    pub fn execution_count(&self, project_id: &str) -> Result<i64> {
        Ok(self.lock().query_row(
            "SELECT COUNT(*) FROM execution_rollup WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )?)
    }

    /// The replication watermark for one project: the highest source-store
    /// `telemetry.rowid` already in the hub. 0 for a never-synced project.
    pub fn telemetry_cursor(&self, project_id: &str) -> Result<i64> {
        Ok(self
            .lock()
            .query_row(
                "SELECT last_source_rowid FROM telemetry_sync_cursors WHERE project_id = ?1",
                params![project_id],
                |r| r.get(0),
            )
            .unwrap_or(0))
    }

    /// Replicate a batch of source telemetry rows into the hub and advance
    /// the project's cursor, in one transaction. Idempotent on
    /// (project_id, source_rowid): a re-replicated row overwrites itself, so
    /// a crash between commit and the caller observing it never double-counts.
    pub fn replicate_telemetry(
        &self,
        scope: &crate::identity::TelemetryScope,
        rows: &[crate::SourceTelemetryRow],
    ) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut max_rowid: i64 = 0;
        for row in rows {
            let t = &row.telemetry;
            tx.execute(
                "INSERT OR REPLACE INTO telemetry \
                 (project_id, source_rowid, org_id, workspace_id, repo_id, execution_id, step, \
                  recorded_at, provider, call_role, model, input_tokens, estimated_input_tokens, \
                  output_tokens, cache_read_tokens, cache_miss_tokens, cache_write_tokens, \
                  cost_usd, duration_ms, retries, tool_calls, usage_complete) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    scope.project_id,
                    row.source_rowid,
                    scope.org_id,
                    scope.workspace_id,
                    scope.repo_id,
                    row.execution_id,
                    t.step as i64,
                    row.recorded_at,
                    t.provider,
                    t.call_role,
                    t.model,
                    t.input_tokens as i64,
                    t.estimated_input_tokens as i64,
                    t.output_tokens as i64,
                    t.cache_read_tokens as i64,
                    t.cache_miss_tokens as i64,
                    t.cache_write_tokens as i64,
                    t.cost_usd,
                    t.duration_ms as i64,
                    t.retries,
                    t.tool_calls as i64,
                    t.usage_complete,
                ],
            )?;
            max_rowid = max_rowid.max(row.source_rowid);
        }
        tx.execute(
            "INSERT INTO telemetry_sync_cursors (project_id, last_source_rowid) \
             VALUES (?1, ?2) \
             ON CONFLICT(project_id) DO UPDATE SET \
               last_source_rowid = MAX(last_source_rowid, excluded.last_source_rowid)",
            params![scope.project_id, max_rowid],
        )?;
        tx.commit()?;
        Ok(rows.len() as u64)
    }

    /// The global report: per (org, provider, model) call counts, token and
    /// cache totals, cost, and how many projects contributed — the query a
    /// cross-project dashboard or `stella usage` renders. `org` filters to
    /// one org id; `None` reports everything (NULL-org rows group as
    /// unregistered/local).
    pub fn global_telemetry_totals(&self, org: Option<&str>) -> Result<Vec<GlobalTelemetryRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT org_id, provider, model, COUNT(*), SUM(input_tokens), SUM(output_tokens), \
                    SUM(cache_read_tokens), SUM(cost_usd), COUNT(DISTINCT project_id) \
             FROM telemetry \
             WHERE (?1 IS NULL OR org_id = ?1) \
             GROUP BY org_id, provider, model \
             ORDER BY SUM(cost_usd) DESC, provider, model",
        )?;
        let rows = stmt.query_map(params![org], |r| {
            Ok(GlobalTelemetryRow {
                org_id: r.get(0)?,
                provider: r.get(1)?,
                model: r.get(2)?,
                calls: r.get(3)?,
                input_tokens: r.get(4)?,
                output_tokens: r.get(5)?,
                cache_read_tokens: r.get(6)?,
                cost_usd: r.get(7)?,
                projects: r.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Hub rows for one org not yet acknowledged by the cloud, oldest first
    /// — the drain a cloud syncer walks before [`Self::ack_cloud_synced`].
    pub fn cloud_pending(&self, org_id: &str, limit: usize) -> Result<Vec<CloudTelemetryEvent>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT t.rowid, t.org_id, t.workspace_id, t.repo_id, t.project_id, t.execution_id, \
                    t.step, t.recorded_at, t.provider, t.call_role, t.model, t.input_tokens, \
                    t.estimated_input_tokens, t.output_tokens, t.cache_read_tokens, \
                    t.cache_miss_tokens, t.cache_write_tokens, t.cost_usd, t.duration_ms, \
                    t.retries, t.tool_calls, t.usage_complete \
             FROM telemetry t \
             WHERE t.org_id = ?1 \
               AND t.rowid > COALESCE((SELECT last_hub_rowid FROM cloud_sync_cursors \
                                       WHERE org_id = ?1), 0) \
             ORDER BY t.rowid ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![org_id, limit as i64], |r| {
            Ok(CloudTelemetryEvent {
                hub_rowid: r.get(0)?,
                org_id: r.get(1)?,
                workspace_id: r.get(2)?,
                repo_id: r.get(3)?,
                project_id: r.get(4)?,
                execution_id: r.get(5)?,
                recorded_at: r.get(7)?,
                telemetry: crate::TelemetryRow {
                    step: r.get::<_, i64>(6)? as u64,
                    provider: r.get(8)?,
                    call_role: r.get(9)?,
                    model: r.get(10)?,
                    input_tokens: r.get::<_, i64>(11)? as u64,
                    estimated_input_tokens: r.get::<_, i64>(12)? as u64,
                    output_tokens: r.get::<_, i64>(13)? as u64,
                    cache_read_tokens: r.get::<_, i64>(14)? as u64,
                    cache_miss_tokens: r.get::<_, i64>(15)? as u64,
                    cache_write_tokens: r.get::<_, i64>(16)? as u64,
                    cost_usd: r.get(17)?,
                    duration_ms: r.get::<_, i64>(18)? as u64,
                    retries: r.get(19)?,
                    tool_calls: r.get::<_, i64>(20)? as u64,
                    usage_complete: r.get(21)?,
                },
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Acknowledge cloud receipt of every hub row up to `up_to_hub_rowid`
    /// for one org. Monotonic — an out-of-order ack never rewinds.
    pub fn ack_cloud_synced(&self, org_id: &str, up_to_hub_rowid: i64) -> Result<()> {
        self.lock().execute(
            "INSERT INTO cloud_sync_cursors (org_id, last_hub_rowid) VALUES (?1, ?2) \
             ON CONFLICT(org_id) DO UPDATE SET \
               last_hub_rowid = MAX(last_hub_rowid, excluded.last_hub_rowid)",
            params![org_id, up_to_hub_rowid],
        )?;
        Ok(())
    }

    /// Every project the hub knows: (project_id, name, root_path) — the
    /// registry `stella usage sync --all` walks for backfill.
    pub fn registered_projects(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT project_id, name, root_path FROM projects ORDER BY last_seen_at DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// One line of the global telemetry report: per (org, provider, model)
/// totals across every replicated project. A `None` org is
/// unregistered/local usage.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalTelemetryRow {
    pub org_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
    pub projects: i64,
}

/// One org-scoped hub row awaiting cloud acknowledgement.
#[derive(Debug, Clone, PartialEq)]
pub struct CloudTelemetryEvent {
    pub hub_rowid: i64,
    pub org_id: String,
    pub workspace_id: Option<String>,
    pub repo_id: String,
    pub project_id: String,
    pub execution_id: i64,
    pub recorded_at: String,
    pub telemetry: crate::TelemetryRow,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollup(execution_id: i64, tools: Vec<ToolBucket>) -> ExecutionRollupRow {
        ExecutionRollupRow {
            usage_complete: true,
            project_id: "proj_a".into(),
            project_name: "stella".into(),
            project_root: "/w/stella".into(),
            execution_id,
            kind: "deck".into(),
            prompt_digest: "digest".into(),
            prompt_preview: "build the feature".into(),
            model: "glm-5.2".into(),
            provider: "zai".into(),
            outcome: "completed".into(),
            cost_usd: 0.05,
            input_tokens: 61_000,
            output_tokens: 8_192,
            duration_ms: 133_700,
            tool_calls: 3,
            files_written: 0,
            produced_output: false,
            self_rating: None,
            started_at: "2026-07-17T13:00:00Z".into(),
            day: "2026-07-17".into(),
            tool_histogram: tools,
        }
    }

    #[test]
    fn sync_records_project_execution_and_tool_histogram() {
        let usage = UsageStore::in_memory().unwrap();
        usage
            .sync_execution(&rollup(
                1,
                vec![
                    ToolBucket {
                        tool: "grep".into(),
                        surface: "native".into(),
                        calls: 2,
                        errors: 0,
                    },
                    ToolBucket {
                        tool: "read_file".into(),
                        surface: "native".into(),
                        calls: 1,
                        errors: 1,
                    },
                ],
            ))
            .unwrap();
        assert_eq!(usage.project_count().unwrap(), 1);
        assert_eq!(usage.execution_count("proj_a").unwrap(), 1);
        let totals = usage.tool_totals().unwrap();
        assert_eq!(totals[0], ("grep".to_string(), 2));
    }

    #[test]
    fn re_syncing_the_same_execution_is_idempotent() {
        let usage = UsageStore::in_memory().unwrap();
        let r = rollup(
            7,
            vec![ToolBucket {
                tool: "grep".into(),
                surface: "native".into(),
                calls: 2,
                errors: 0,
            }],
        );
        usage.sync_execution(&r).unwrap();
        usage.sync_execution(&r).unwrap(); // re-sync must not double-count
        assert_eq!(usage.execution_count("proj_a").unwrap(), 1);
        assert_eq!(usage.tool_totals().unwrap(), vec![("grep".to_string(), 2)]);
    }

    #[test]
    fn two_projects_aggregate_independently_but_share_tool_totals() {
        let usage = UsageStore::in_memory().unwrap();
        let mut a = rollup(
            1,
            vec![ToolBucket {
                tool: "grep".into(),
                surface: "native".into(),
                calls: 3,
                errors: 0,
            }],
        );
        usage.sync_execution(&a).unwrap();
        a.project_id = "proj_b".into();
        a.project_name = "arena".into();
        a.execution_id = 1;
        usage.sync_execution(&a).unwrap();
        assert_eq!(usage.project_count().unwrap(), 2);
        assert_eq!(usage.tool_totals().unwrap(), vec![("grep".to_string(), 6)]);
    }

    fn scope(org: Option<&str>, workspace: Option<&str>) -> crate::identity::TelemetryScope {
        crate::identity::TelemetryScope {
            org_id: org.map(String::from),
            workspace_id: workspace.map(String::from),
            repo_id: "repo01".into(),
            project_id: "proj_a".into(),
        }
    }

    fn source_row(source_rowid: i64, cost: f64) -> crate::SourceTelemetryRow {
        crate::SourceTelemetryRow {
            source_rowid,
            execution_id: 1,
            recorded_at: "2026-07-23T10:00:00Z".into(),
            telemetry: crate::TelemetryRow {
                step: source_rowid as u64,
                provider: "zai".into(),
                call_role: "engine".into(),
                model: "glm-5.2".into(),
                input_tokens: 1000,
                estimated_input_tokens: 900,
                output_tokens: 100,
                cache_read_tokens: 500,
                cache_miss_tokens: 500,
                cache_write_tokens: 0,
                cost_usd: cost,
                duration_ms: 1200,
                retries: 0,
                tool_calls: 2,
                usage_complete: true,
            },
        }
    }

    #[test]
    fn replication_advances_the_cursor_and_is_idempotent() {
        let hub = UsageStore::in_memory().unwrap();
        let s = scope(None, None);
        assert_eq!(hub.telemetry_cursor("proj_a").unwrap(), 0);
        hub.replicate_telemetry(&s, &[source_row(1, 0.01), source_row(2, 0.02)])
            .unwrap();
        assert_eq!(hub.telemetry_cursor("proj_a").unwrap(), 2);
        // Re-replicating the same rows overwrites, never duplicates.
        hub.replicate_telemetry(&s, &[source_row(1, 0.01), source_row(2, 0.02)])
            .unwrap();
        let totals = hub.global_telemetry_totals(None).unwrap();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].calls, 2);
        assert_eq!(totals[0].org_id, None, "unregistered rows carry NULL org");
        assert!((totals[0].cost_usd - 0.03).abs() < 1e-9);
    }

    #[test]
    fn org_scoping_filters_the_report_and_the_cloud_drain() {
        let hub = UsageStore::in_memory().unwrap();
        hub.replicate_telemetry(&scope(None, None), &[source_row(1, 0.01)])
            .unwrap();
        let acme = scope(Some("acme"), Some("ws-1"));
        let mut acme_scope = acme.clone();
        acme_scope.project_id = "proj_b".into();
        hub.replicate_telemetry(&acme_scope, &[source_row(1, 0.05), source_row(2, 0.05)])
            .unwrap();

        // The org filter sees only acme's rows; None sees both groups.
        assert_eq!(
            hub.global_telemetry_totals(Some("acme")).unwrap()[0].calls,
            2
        );
        assert_eq!(hub.global_telemetry_totals(None).unwrap().len(), 2);

        // The cloud drain is org-scoped: NULL-org (unregistered) rows are
        // never shipped.
        let pending = hub.cloud_pending("acme", 10).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(pending[0].repo_id, "repo01");

        // Ack advances monotonically and drains the backlog.
        let last = pending.last().unwrap().hub_rowid;
        hub.ack_cloud_synced("acme", last).unwrap();
        assert!(hub.cloud_pending("acme", 10).unwrap().is_empty());
        hub.ack_cloud_synced("acme", last - 1).unwrap(); // out-of-order ack
        assert!(
            hub.cloud_pending("acme", 10).unwrap().is_empty(),
            "an out-of-order ack never rewinds the cursor"
        );
    }

    #[test]
    fn project_id_is_stable_and_path_derived() {
        let a = project_id_for(Path::new("/tmp"));
        let b = project_id_for(Path::new("/tmp"));
        assert_eq!(a, b, "same path → same id");
        assert_eq!(a.len(), 16, "16 hex chars");
    }

    #[test]
    fn usage_db_path_honors_the_data_dir_override() {
        // SAFETY: single-threaded test; we set and read one process env var.
        unsafe {
            std::env::set_var("STELLA_DATA_DIR", "/tmp/stella-usage-test");
        }
        assert_eq!(
            usage_db_path(),
            PathBuf::from("/tmp/stella-usage-test/usage.db")
        );
        unsafe {
            std::env::remove_var("STELLA_DATA_DIR");
        }
    }

    #[cfg(unix)]
    #[test]
    fn usage_store_repairs_private_dir_and_file_modes() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let private = tmp.path().join("stella-data");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o777)).unwrap();
        let db = private.join("usage.db");

        drop(UsageStore::open_at(&db).unwrap());
        let mode = |path: &Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&private), 0o700);
        assert_eq!(mode(&db), 0o600);

        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o666)).unwrap();
        drop(UsageStore::open_at(&db).unwrap());
        assert_eq!(mode(&db), 0o600, "existing private DB is repaired");
    }

    #[cfg(unix)]
    #[test]
    fn usage_store_rejects_a_symlink_database() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let private = tmp.path().join("stella-data");
        std::fs::create_dir_all(&private).unwrap();
        let target = tmp.path().join("outside.db");
        std::fs::write(&target, b"outside").unwrap();
        symlink(&target, private.join("usage.db")).unwrap();
        assert!(UsageStore::open_at(&private.join("usage.db")).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"outside");
    }
}
