//! `usage.db` — the **user-tier** telemetry aggregate, one database per
//! developer (not per project), living under the OS data dir (e.g.
//! `~/.local/share/stella/usage.db`). It is a *derived* store: every project's
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
/// notifications). `STELLA_DATA_DIR` overrides; otherwise the platform data
/// dir (NOT the config dir — this is data, not config).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("STELLA_DATA_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Application Support/stella");
    }
    #[cfg(target_os = "windows")]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("stella");
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("stella");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/stella");
    }
    PathBuf::from(".")
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
";

/// The user-tier aggregate store. Read/write, loopback-local, no server.
pub struct UsageStore {
    conn: Mutex<Connection>,
}

impl UsageStore {
    /// Open (creating dirs + schema) the per-user `usage.db` at the default
    /// location. Best-effort callers treat an `Err` as "no cross-project
    /// aggregation this run".
    pub fn open_default() -> Result<Self> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollup(execution_id: i64, tools: Vec<ToolBucket>) -> ExecutionRollupRow {
        ExecutionRollupRow {
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
