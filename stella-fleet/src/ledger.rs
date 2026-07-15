//! The commit ledger (`02-architecture.md` §2 "commit ledger (SQLite)", §6
//! "`fleet.db` — SQLite: fleet commit ledger"). One embedded SQLite file
//! (`rusqlite`, bundled — `02-architecture.md` §1.6 "one storage engine")
//! recording, for every fleet run: its tasks, each dispatch attempt, the
//! commits an attempt produced, the parent→child lineage, and per-task USD
//! spend.
//!
//! This is the durable audit trail behind the dispatch seam (L-E9): the one
//! place a subagent's commits and cost are stamped, so lineage is never lost
//! and spend is never uncounted. The in-memory [`stella_core::BudgetGuard`]
//! is the *gate*; this ledger is the *record* — both are written on every
//! dispatch (`crate::fleet`).
//!
//! Writes that must be all-or-nothing (an attempt's outcome plus its commits
//! and spend row) go through one transaction
//! ([`Ledger::finish_attempt`]); WAL journaling is enabled at open so a
//! reader is never blocked by an in-flight writer.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::plan::{Isolation, Task, TaskId};

/// A commit recorded in the ledger — also the shape a [`FleetWorker`] reports
/// back (`crate::fleet::WorkerOutcome::commits`) and the value the emit-shape
/// helper turns into an [`stella_protocol::AgentEvent::Commit`]
/// (`crate::monitor::commit_event`).
///
/// [`FleetWorker`]: crate::fleet::FleetWorker
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRecord {
    pub sha: String,
    pub branch: String,
    pub task_id: TaskId,
    pub message: String,
    pub timestamp_ms: u64,
}

/// A fleet run — the top of the ledger hierarchy (run → task → attempt →
/// commits/spend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub id: String,
    pub root_task_count: u32,
    pub created_at_ms: u64,
}

/// The opening half of a dispatch attempt, written before the worker runs so
/// a crash mid-attempt still leaves a row naming what was in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptStart {
    pub run_id: String,
    pub task_id: TaskId,
    pub worktree_path: String,
    pub branch: String,
    pub started_at_ms: u64,
}

/// The closing half of a dispatch attempt: its outcome plus everything it
/// produced. Written in one transaction by [`Ledger::finish_attempt`].
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptFinish {
    pub attempt_id: AttemptId,
    pub run_id: String,
    pub task_id: TaskId,
    pub finished_at_ms: u64,
    pub success: bool,
    pub summary: String,
    pub commits: Vec<CommitRecord>,
    pub cost_usd: f64,
    pub spend_at_ms: u64,
}

/// SQLite rowid of an attempt row, returned by [`Ledger::start_attempt`] and
/// referenced by its commits/spend.
pub type AttemptId = i64;

/// Failures interacting with the ledger — always typed, never a panic.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// The fleet commit ledger over one SQLite connection. Not `Sync` (a
/// `rusqlite::Connection` isn't), so the fleet holds it behind a `Mutex` and
/// serializes its (fast, synchronous) writes — see `crate::fleet`.
pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    /// Open (creating if absent) the ledger at `path` — the CLI opens
    /// `<workspace>/.stella/fleet.db` (`02-architecture.md` §6). Enables WAL
    /// and foreign keys, then applies the schema.
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// An in-memory ledger — for tests and ephemeral runs. Same schema; WAL
    /// is a no-op for `:memory:`.
    pub fn open_in_memory() -> Result<Self, LedgerError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, LedgerError> {
        // execute_batch tolerates the row PRAGMA journal_mode returns (a
        // plain pragma_update errors on it).
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Record a run (idempotent on its id via `INSERT OR REPLACE`).
    pub fn record_run(&self, run: &RunRecord) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO runs (id, root_task_count, created_at_ms) VALUES (?1, ?2, ?3)",
            params![run.id, run.root_task_count, run.created_at_ms as i64],
        )?;
        Ok(())
    }

    /// Record a task belonging to a run (idempotent on (run_id, task_id)).
    pub fn record_task(&self, run_id: &str, task: &Task) -> Result<(), LedgerError> {
        let isolation = match task.isolation {
            Isolation::Isolated => "isolated",
            Isolation::SharedTree => "shared_tree",
        };
        self.conn.execute(
            "INSERT OR REPLACE INTO tasks (run_id, task_id, title, isolation) \
             VALUES (?1, ?2, ?3, ?4)",
            params![run_id, task.id, task.title, isolation],
        )?;
        Ok(())
    }

    /// Open an attempt row and return its id. Written before the worker runs
    /// (see [`AttemptStart`]).
    pub fn start_attempt(&self, start: &AttemptStart) -> Result<AttemptId, LedgerError> {
        self.conn.execute(
            "INSERT INTO attempts \
             (run_id, task_id, worktree_path, branch, started_at_ms, finished_at_ms, success, summary) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL)",
            params![
                start.run_id,
                start.task_id,
                start.worktree_path,
                start.branch,
                start.started_at_ms as i64,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Close an attempt and stamp everything it produced — its commits and
    /// its spend row — in a single transaction (all-or-nothing). This is the
    /// durable half of the dispatch seam (`crate::fleet::Fleet::dispatch`).
    pub fn finish_attempt(&self, finish: &AttemptFinish) -> Result<(), LedgerError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE attempts SET finished_at_ms = ?2, success = ?3, summary = ?4 WHERE id = ?1",
            params![
                finish.attempt_id,
                finish.finished_at_ms as i64,
                finish.success as i64,
                finish.summary,
            ],
        )?;
        for commit in &finish.commits {
            tx.execute(
                "INSERT INTO commits \
                 (attempt_id, run_id, task_id, sha, branch, message, timestamp_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    finish.attempt_id,
                    finish.run_id,
                    commit.task_id,
                    commit.sha,
                    commit.branch,
                    commit.message,
                    commit.timestamp_ms as i64,
                ],
            )?;
        }
        tx.execute(
            "INSERT INTO spend (run_id, task_id, attempt_id, cost_usd, recorded_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                finish.run_id,
                finish.task_id,
                finish.attempt_id,
                finish.cost_usd,
                finish.spend_at_ms as i64,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record a parent-run → child-task lineage edge (L-E9: the dispatch seam
    /// stamps lineage so a subagent's work is always traceable to its
    /// parent).
    pub fn record_lineage(
        &self,
        parent_run_id: &str,
        child_task_id: &str,
        recorded_at_ms: u64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO lineage (parent_run_id, child_task_id, recorded_at_ms) VALUES (?1, ?2, ?3)",
            params![parent_run_id, child_task_id, recorded_at_ms as i64],
        )?;
        Ok(())
    }

    /// Every commit recorded for a task, oldest first.
    pub fn commits_for_task(
        &self,
        run_id: &str,
        task_id: &str,
    ) -> Result<Vec<CommitRecord>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT sha, branch, task_id, message, timestamp_ms FROM commits \
             WHERE run_id = ?1 AND task_id = ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![run_id, task_id], |row| {
            Ok(CommitRecord {
                sha: row.get(0)?,
                branch: row.get(1)?,
                task_id: row.get(2)?,
                message: row.get(3)?,
                timestamp_ms: row.get::<_, i64>(4)? as u64,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Total USD spend recorded against a run (sum over all its tasks'
    /// attempts).
    pub fn total_spend(&self, run_id: &str) -> Result<f64, LedgerError> {
        let total = self.conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM spend WHERE run_id = ?1",
            params![run_id],
            |row| row.get::<_, f64>(0),
        )?;
        Ok(total)
    }

    /// USD spend recorded against a single task within a run.
    pub fn task_spend(&self, run_id: &str, task_id: &str) -> Result<f64, LedgerError> {
        let total = self.conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM spend WHERE run_id = ?1 AND task_id = ?2",
            params![run_id, task_id],
            |row| row.get::<_, f64>(0),
        )?;
        Ok(total)
    }

    /// Child task ids recorded as lineage under a parent run, sorted.
    pub fn lineage_children(&self, parent_run_id: &str) -> Result<Vec<String>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT child_task_id FROM lineage WHERE parent_run_id = ?1 ORDER BY child_task_id",
        )?;
        let rows = stmt.query_map(params![parent_run_id], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// How many attempts a task has had (retries show up as extra rows).
    pub fn attempt_count(&self, run_id: &str, task_id: &str) -> Result<u32, LedgerError> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM attempts WHERE run_id = ?1 AND task_id = ?2",
            params![run_id, task_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(count as u32)
    }

    /// Whether an attempt row's outcome has been stamped yet (`false` while a
    /// worker is still in flight or if it crashed before finishing).
    pub fn attempt_is_finished(&self, attempt_id: AttemptId) -> Result<bool, LedgerError> {
        let finished: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT finished_at_ms FROM attempts WHERE id = ?1",
                params![attempt_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(matches!(finished, Some(Some(_))))
    }
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS runs (
    id              TEXT PRIMARY KEY,
    root_task_count INTEGER NOT NULL,
    created_at_ms   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tasks (
    run_id    TEXT NOT NULL,
    task_id   TEXT NOT NULL,
    title     TEXT NOT NULL,
    isolation TEXT NOT NULL,
    PRIMARY KEY (run_id, task_id)
);
CREATE TABLE IF NOT EXISTS attempts (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id         TEXT NOT NULL,
    task_id        TEXT NOT NULL,
    worktree_path  TEXT NOT NULL,
    branch         TEXT NOT NULL,
    started_at_ms  INTEGER NOT NULL,
    finished_at_ms INTEGER,
    success        INTEGER,
    summary        TEXT
);
CREATE INDEX IF NOT EXISTS attempts_by_task ON attempts (run_id, task_id);
CREATE TABLE IF NOT EXISTS commits (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id   INTEGER NOT NULL REFERENCES attempts (id),
    run_id       TEXT NOT NULL,
    task_id      TEXT NOT NULL,
    sha          TEXT NOT NULL,
    branch       TEXT NOT NULL,
    message      TEXT NOT NULL,
    timestamp_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS commits_by_task ON commits (run_id, task_id);
CREATE TABLE IF NOT EXISTS lineage (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_run_id  TEXT NOT NULL,
    child_task_id  TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS lineage_by_parent ON lineage (parent_run_id);
CREATE TABLE IF NOT EXISTS spend (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id         TEXT NOT NULL,
    task_id        TEXT NOT NULL,
    attempt_id     INTEGER NOT NULL REFERENCES attempts (id),
    cost_usd       REAL NOT NULL,
    recorded_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS spend_by_run ON spend (run_id);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str) -> Task {
        Task::new(id, format!("title {id}"), "prompt")
    }

    fn commit(task_id: &str, sha: &str) -> CommitRecord {
        CommitRecord {
            sha: sha.into(),
            branch: format!("fleet/{task_id}"),
            task_id: task_id.into(),
            message: format!("work on {task_id}"),
            timestamp_ms: 1_000,
        }
    }

    fn seed_run(ledger: &Ledger, run_id: &str) {
        ledger
            .record_run(&RunRecord {
                id: run_id.into(),
                root_task_count: 1,
                created_at_ms: 1,
            })
            .unwrap();
        ledger.record_task(run_id, &task("t1")).unwrap();
    }

    #[test]
    fn open_in_memory_applies_schema_and_is_empty() {
        let ledger = Ledger::open_in_memory().unwrap();
        assert_eq!(ledger.total_spend("run").unwrap(), 0.0);
        assert!(ledger.commits_for_task("run", "t1").unwrap().is_empty());
        assert!(ledger.lineage_children("run").unwrap().is_empty());
    }

    #[test]
    fn attempt_round_trips_commits_and_spend_atomically() {
        let ledger = Ledger::open_in_memory().unwrap();
        seed_run(&ledger, "run1");

        let attempt_id = ledger
            .start_attempt(&AttemptStart {
                run_id: "run1".into(),
                task_id: "t1".into(),
                worktree_path: "/tmp/wt/t1".into(),
                branch: "fleet/t1".into(),
                started_at_ms: 10,
            })
            .unwrap();
        assert!(!ledger.attempt_is_finished(attempt_id).unwrap());

        ledger
            .finish_attempt(&AttemptFinish {
                attempt_id,
                run_id: "run1".into(),
                task_id: "t1".into(),
                finished_at_ms: 20,
                success: true,
                summary: "done".into(),
                commits: vec![commit("t1", "aaa"), commit("t1", "bbb")],
                cost_usd: 0.25,
                spend_at_ms: 21,
            })
            .unwrap();

        assert!(ledger.attempt_is_finished(attempt_id).unwrap());
        let commits = ledger.commits_for_task("run1", "t1").unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "aaa");
        assert_eq!(commits[1].sha, "bbb");
        assert!((ledger.total_spend("run1").unwrap() - 0.25).abs() < 1e-9);
        assert!((ledger.task_spend("run1", "t1").unwrap() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn spend_sums_across_multiple_attempts_of_a_run() {
        let ledger = Ledger::open_in_memory().unwrap();
        seed_run(&ledger, "run1");
        ledger.record_task("run1", &task("t2")).unwrap();

        for (task_id, cost) in [("t1", 0.1), ("t2", 0.4)] {
            let attempt_id = ledger
                .start_attempt(&AttemptStart {
                    run_id: "run1".into(),
                    task_id: task_id.into(),
                    worktree_path: format!("/tmp/{task_id}"),
                    branch: format!("fleet/{task_id}"),
                    started_at_ms: 1,
                })
                .unwrap();
            ledger
                .finish_attempt(&AttemptFinish {
                    attempt_id,
                    run_id: "run1".into(),
                    task_id: task_id.into(),
                    finished_at_ms: 2,
                    success: true,
                    summary: "ok".into(),
                    commits: vec![],
                    cost_usd: cost,
                    spend_at_ms: 3,
                })
                .unwrap();
        }
        assert!((ledger.total_spend("run1").unwrap() - 0.5).abs() < 1e-9);
        assert!((ledger.task_spend("run1", "t1").unwrap() - 0.1).abs() < 1e-9);
        assert!((ledger.task_spend("run1", "t2").unwrap() - 0.4).abs() < 1e-9);
    }

    #[test]
    fn retries_of_a_task_show_up_as_multiple_attempts() {
        let ledger = Ledger::open_in_memory().unwrap();
        seed_run(&ledger, "run1");
        for started in [1, 5] {
            ledger
                .start_attempt(&AttemptStart {
                    run_id: "run1".into(),
                    task_id: "t1".into(),
                    worktree_path: "/tmp/t1".into(),
                    branch: "fleet/t1".into(),
                    started_at_ms: started,
                })
                .unwrap();
        }
        assert_eq!(ledger.attempt_count("run1", "t1").unwrap(), 2);
    }

    #[test]
    fn lineage_records_parent_run_to_child_tasks() {
        let ledger = Ledger::open_in_memory().unwrap();
        seed_run(&ledger, "parent-run");
        ledger.record_lineage("parent-run", "t-child-b", 1).unwrap();
        ledger.record_lineage("parent-run", "t-child-a", 2).unwrap();
        assert_eq!(
            ledger.lineage_children("parent-run").unwrap(),
            vec!["t-child-a".to_string(), "t-child-b".to_string()]
        );
        assert!(ledger.lineage_children("other-run").unwrap().is_empty());
    }

    #[test]
    fn commit_record_json_roundtrips() {
        let c = commit("t1", "deadbeef");
        let back: CommitRecord = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn ledger_persists_to_a_file_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        {
            let ledger = Ledger::open(&path).unwrap();
            seed_run(&ledger, "run1");
            let attempt_id = ledger
                .start_attempt(&AttemptStart {
                    run_id: "run1".into(),
                    task_id: "t1".into(),
                    worktree_path: "/tmp/t1".into(),
                    branch: "fleet/t1".into(),
                    started_at_ms: 1,
                })
                .unwrap();
            ledger
                .finish_attempt(&AttemptFinish {
                    attempt_id,
                    run_id: "run1".into(),
                    task_id: "t1".into(),
                    finished_at_ms: 2,
                    success: true,
                    summary: "ok".into(),
                    commits: vec![commit("t1", "abc")],
                    cost_usd: 0.5,
                    spend_at_ms: 3,
                })
                .unwrap();
        }
        // Reopen the same file: the schema and data survive.
        let reopened = Ledger::open(&path).unwrap();
        assert_eq!(reopened.commits_for_task("run1", "t1").unwrap().len(), 1);
        assert!((reopened.total_spend("run1").unwrap() - 0.5).abs() < 1e-9);
    }
}
