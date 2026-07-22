//! Schema versioning for `.stella/private/store.db`: the ordered [`MIGRATIONS`]
//! list, the fresh-file bootstrap ([`create_latest_schema`]), and the
//! transactional runner ([`apply_migration`]) that stamps
//! `PRAGMA user_version` inside the same transaction as the reshape — a
//! crash mid-migration rolls the file back to the old version and old
//! shape, never a mix. The DDL itself lives in [`crate::ddl`]; the
//! fresh-vs-legacy disambiguation and the step loop that drives this
//! module live in `Store::migrate` (see the crate docs' "Schema
//! versioning" section).

use rusqlite::{Connection, params};

use crate::ddl::{
    AGENT_USES_DDL, EXECUTION_REFLECTION_DDL, EXECUTIONS_DDL, MCP_USAGE_DDL, MEMORY_CITATIONS_DDL,
    PULL_REQUESTS_DDL, REFLECTIONS_DDL, RULES_TABLE, SKILL_USAGE_DDL, TABLES, TASKS_DDL,
    TELEMETRY_INDEX, TOOL_CALLS_DDL, UNCHANGED_TABLES, events_ddl, files_touched_ddl,
    telemetry_ddl,
};
use crate::{Result, StoreError};

/// One schema migration: upgrades an existing database exactly one
/// `user_version` step, inside the transaction the runner opened for it.
/// The runner stamps the new version and commits; the migration only
/// reshapes.
pub(crate) type Migration = fn(&rusqlite::Transaction<'_>) -> Result<()>;

/// Ordered migration list for EXISTING databases: `MIGRATIONS[i]` upgrades
/// a file at `user_version` i to i + 1. Fresh files never run these — they
/// get [`create_latest_schema`] and are stamped at [`SCHEMA_VERSION`]
/// directly.
pub(crate) const MIGRATIONS: [Migration; 10] = [
    // v0 → v1: dedupe events/telemetry, then retrofit the UNIQUE keys
    // their write paths have always assumed.
    migrate_v0_to_v1,
    // v1 → v2: files_touched grows line-delta totals + the JSON audit log,
    // and the UNIQUE (execution_id, path) key.
    migrate_v1_to_v2,
    // v2 → v3: the memory_citations table and the `rules` table
    // (extension-authored workspace rules for the stella-core rules
    // engine) — both purely additive; no existing table changes shape.
    migrate_v2_to_v3,
    // v3 → v4: the agent_uses invocation log (purely additive).
    migrate_v3_to_v4,
    // v4 → v5: the skill_usage invocation log (purely additive — SKILLS tab).
    migrate_v4_to_v5,
    // v5 → v6: the additive `mcp_usage` table (per-call MCP tool telemetry).
    // Renumbered from v4 at cascade-merge behind the agent_uses (v4) and
    // skill_usage (v5) migrations; SCHEMA_VERSION follows MIGRATIONS.len().
    migrate_v5_to_v6,
    // v6 → v7: the data-plane tables (all purely additive) — `tool_calls`
    // (normalized per-call log), `execution_reflection` (per-turn self-review
    // tied to the prompt), and `reflections` (unified durable lessons).
    migrate_v6_to_v7,
    // v7 → v8: the session plane — `executions` grows the nullable
    // `session_id` link (+ its by-session index), plus the additive `tasks`
    // (per-session task-board snapshot) and `pull_requests` tables.
    migrate_v7_to_v8,
    // v8 → v9: fail-closed paid-call accounting state.
    migrate_v8_to_v9,
    // v9 → v10: explicit pending/complete/incomplete execution lifecycle.
    migrate_v9_to_v10,
];

/// The schema version this build writes — the `PRAGMA user_version` of
/// every database it has opened. Version 0 is the legacy pre-versioning
/// shape (and the default stamp of a fresh empty file, which is why fresh
/// detection also probes for tables).
pub(crate) const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// The full latest schema, applied in one shot to fresh databases only.
/// Existing files never see this — [`MIGRATIONS`] upgrades them shape by
/// shape, so this can always describe the CURRENT shape.
pub(crate) fn create_latest_schema(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(EXECUTIONS_DDL)?;
    tx.execute_batch(UNCHANGED_TABLES)?;
    tx.execute_batch(&events_ddl("events"))?;
    tx.execute_batch(&telemetry_ddl("telemetry"))?;
    tx.execute_batch(&files_touched_ddl("files_touched"))?;
    tx.execute_batch(RULES_TABLE)?;
    tx.execute_batch(TELEMETRY_INDEX)?;
    tx.execute_batch(MEMORY_CITATIONS_DDL)?;
    tx.execute_batch(AGENT_USES_DDL)?;
    tx.execute_batch(SKILL_USAGE_DDL)?;
    tx.execute_batch(MCP_USAGE_DDL)?;
    tx.execute_batch(TOOL_CALLS_DDL)?;
    tx.execute_batch(EXECUTION_REFLECTION_DDL)?;
    tx.execute_batch(REFLECTIONS_DDL)?;
    tx.execute_batch(TASKS_DDL)?;
    tx.execute_batch(PULL_REQUESTS_DDL)?;
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        params![table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Whether any store-owned table exists — distinguishes a fresh empty file
/// (create latest schema directly) from a legacy pre-versioning file (run
/// the migration list), since both carry `user_version` 0.
pub(crate) fn any_store_table_exists(conn: &Connection) -> Result<bool> {
    let placeholders = TABLES.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let count: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ({placeholders})"
        ),
        rusqlite::params_from_iter(TABLES),
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info(?) WHERE name = ?",
        params![table, column],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// v0 → v1: retrofit the UNIQUE constraints the write paths have always
/// assumed (see [`events_ddl`]/[`telemetry_ddl`]), deduping first — a
/// constraint cannot land on a table holding historic duplicates.
///
/// Keep-rule: the newest row per natural key — `max(rowid)`, which is
/// insertion order. A duplicate key can only come from a double-write of
/// the same logical record (the writers' counters are monotonic per
/// execution), and readers want the writer's final word: replay renders one
/// event per stream position, and analytics prices one row per committed
/// call — exactly the row an upsert would have retained.
///
/// SQLite cannot ALTER a UNIQUE constraint in, so both tables are rebuilt
/// per the documented procedure (lang_altertable §7): create-new →
/// INSERT SELECT → DROP old → RENAME. The old tables' indexes drop with
/// them; `telemetry_by_model` is recreated and `events_by_execution` is
/// superseded by the UNIQUE constraint's implicit index on exactly its
/// columns. No store table declares foreign keys in either direction, so
/// the rebuild moves no FK edges — but the runner still follows the full §7
/// procedure (`foreign_keys` OFF outside the transaction, `foreign_key_check`
/// before commit) so a future FK-bearing schema cannot be corrupted by this
/// path.
///
/// A v0 file is not guaranteed to hold every table (partial files exist —
/// e.g. pre-drift fixtures with only `telemetry`), so missing tables are
/// created fresh in the v1 shape: empty, nothing to dedupe.
fn migrate_v0_to_v1(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(UNCHANGED_TABLES)?;
    // executions changed shape in v8 (the session_id column), so it left
    // UNCHANGED_TABLES — but a v1 database has its ERA's shape, which this
    // step must keep producing (the v8 ALTER later in the chain runs
    // against it).
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS executions (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           kind TEXT NOT NULL,
           prompt TEXT NOT NULL,
           provider TEXT NOT NULL,
           model TEXT NOT NULL,
           started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
           finished_at TEXT,
           outcome TEXT,
           cost_usd REAL NOT NULL DEFAULT 0
         );",
    )?;
    // files_touched changed shape again in v2, so it left UNCHANGED_TABLES —
    // but a v1 database has its ERA's shape, which this step must keep
    // producing (the v2 rebuild right after runs against it).
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS files_touched (
           execution_id INTEGER NOT NULL,
           path TEXT NOT NULL,
           ops TEXT NOT NULL
         );",
    )?;
    // New executions must never reuse an id that historic rows already
    // reference: a reused id mis-attributes those orphaned rows to the new
    // run, and — with the UNIQUE keys this migration retrofits — collides
    // with their (execution_id, seq/step) positions. A partial v0 file can
    // hold events/telemetry that outlive their executions table (whose
    // AUTOINCREMENT counter then restarts at 1), so the counter is seeded
    // past every execution id in sight. sqlite_sequence exists here:
    // creating any AUTOINCREMENT table (executions, just ensured) creates
    // it, and its content is plain-DML-writable by design.
    let max_in_executions: Option<i64> =
        tx.query_row("SELECT max(id) FROM executions", [], |row| row.get(0))?;
    let mut max_execution_id = max_in_executions.unwrap_or(0);
    // events and telemetry may still be missing here (they are ensured or
    // rebuilt below), so each referencing table is probed individually.
    for table in ["events", "telemetry", "files_touched"] {
        if table_exists(tx, table)? {
            let max_id: Option<i64> = tx.query_row(
                &format!("SELECT max(execution_id) FROM {table}"),
                [],
                |row| row.get(0),
            )?;
            max_execution_id = max_execution_id.max(max_id.unwrap_or(0));
        }
    }
    if max_execution_id > 0 {
        let seeded = tx.execute(
            "UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'executions' AND seq < ?1",
            params![max_execution_id],
        )?;
        if seeded == 0 {
            // No row updated: either the counter is already past the ids
            // (nothing to do) or the counter row does not exist yet.
            let exists: i64 = tx.query_row(
                "SELECT count(*) FROM sqlite_sequence WHERE name = 'executions'",
                [],
                |row| row.get(0),
            )?;
            if exists == 0 {
                tx.execute(
                    "INSERT INTO sqlite_sequence (name, seq) VALUES ('executions', ?1)",
                    params![max_execution_id],
                )?;
            }
        }
    }
    if table_exists(tx, "events")? {
        tx.execute_batch(&events_ddl("events_v1"))?;
        tx.execute_batch(
            "INSERT INTO events_v1 (execution_id, seq, ts, event_type, payload)
             SELECT execution_id, seq, ts, event_type, payload FROM events
             WHERE rowid IN (SELECT max(rowid) FROM events GROUP BY execution_id, seq);
             DROP TABLE events;
             ALTER TABLE events_v1 RENAME TO events;",
        )?;
    } else {
        tx.execute_batch(&events_ddl("events"))?;
    }
    if table_exists(tx, "telemetry")? {
        // Pre-drift files lack estimated_input_tokens; the rebuild
        // backfills 0 = "no estimate was taken", which drift_samples
        // excludes as signal-free — same semantics the old ALTER-based
        // migration gave those rows.
        let estimated = if column_exists(tx, "telemetry", "estimated_input_tokens")? {
            "estimated_input_tokens"
        } else {
            "0"
        };
        tx.execute_batch(&telemetry_ddl("telemetry_v1"))?;
        tx.execute_batch(&format!(
            "INSERT INTO telemetry_v1 (execution_id, step, ts, provider, model, input_tokens,
               estimated_input_tokens, output_tokens, cache_read_tokens, cache_miss_tokens,
               cache_write_tokens, cost_usd, duration_ms, retries, tool_calls)
             SELECT execution_id, step, ts, provider, model, input_tokens,
               {estimated}, output_tokens, cache_read_tokens, cache_miss_tokens,
               cache_write_tokens, cost_usd, duration_ms, retries, tool_calls
             FROM telemetry
             WHERE rowid IN (SELECT max(rowid) FROM telemetry GROUP BY execution_id, step);
             DROP TABLE telemetry;
             ALTER TABLE telemetry_v1 RENAME TO telemetry;",
        ))?;
    } else {
        tx.execute_batch(&telemetry_ddl("telemetry"))?;
    }
    tx.execute_batch(TELEMETRY_INDEX)?;
    Ok(())
}

/// v1 → v2: `files_touched` grows per-file line-delta totals and the ordered
/// JSON audit log ([`FileTouchRow`](crate::FileTouchRow)), plus the UNIQUE (execution_id, path)
/// key its writer has always assumed (the ledger emits exactly one record
/// per normalized path per execution). SQLite cannot ALTER a UNIQUE
/// constraint in, so the table is rebuilt per lang_altertable §7 with the
/// same newest-row keep-rule as [`migrate_v0_to_v1`]. Legacy rows predate
/// line telemetry and are backfilled with the column defaults: zero deltas,
/// `'[]'` audit log.
fn migrate_v1_to_v2(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    if table_exists(tx, "files_touched")? {
        tx.execute_batch(&files_touched_ddl("files_touched_v2"))?;
        tx.execute_batch(
            "INSERT INTO files_touched_v2 (execution_id, path, ops)
             SELECT execution_id, path, ops FROM files_touched
             WHERE rowid IN (SELECT max(rowid) FROM files_touched GROUP BY execution_id, path);
             DROP TABLE files_touched;
             ALTER TABLE files_touched_v2 RENAME TO files_touched;",
        )?;
    } else {
        // Partial v1 files exist just like partial v0 files: nothing to
        // rebuild, create the v2 shape fresh.
        tx.execute_batch(&files_touched_ddl("files_touched"))?;
    }
    Ok(())
}

/// v2 → v3: the `memory_citations` table. Purely additive — no existing
/// table is touched, so no rebuild, no dedupe, no backfill.
fn migrate_v2_to_v3(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(MEMORY_CITATIONS_DDL)?;
    tx.execute_batch(RULES_TABLE)?;
    Ok(())
}

/// v3 → v4: the `agent_uses` invocation log (agent-version usage telemetry,
/// drained per execution like `files_touched`). Purely additive — no
/// existing table changes shape, so no §7 rebuild is needed; `IF NOT
/// EXISTS` also covers a partial file that somehow already grew the table.
fn migrate_v3_to_v4(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(AGENT_USES_DDL)?;
    Ok(())
}

/// v4 → v5: the additive `skill_usage` table (skill-version usage telemetry,
/// SKILLS tab). Purely additive, mirroring [`migrate_v3_to_v4`].
fn migrate_v4_to_v5(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(SKILL_USAGE_DDL)?;
    Ok(())
}

/// v5 → v6: the `mcp_usage` table. Purely additive — no existing table is
/// touched, so no rebuild, no dedupe, no backfill.
fn migrate_v5_to_v6(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(MCP_USAGE_DDL)?;
    Ok(())
}

/// v6 → v7: the additive data-plane tables — `tool_calls`,
/// `execution_reflection`, and `reflections`. No existing table changes shape.
fn migrate_v6_to_v7(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch(TOOL_CALLS_DDL)?;
    tx.execute_batch(EXECUTION_REFLECTION_DDL)?;
    tx.execute_batch(REFLECTIONS_DDL)?;
    Ok(())
}

/// v7 → v8: the session plane. `executions` grows the nullable `session_id`
/// column linking each per-turn row to the cross-process session registry id
/// (a plain ADD COLUMN — nullable, no default rewrite, so no §7 rebuild),
/// plus the `executions_by_session` index and the additive `tasks` and
/// `pull_requests` tables. The ALTER is guarded by a column probe, matching
/// the house `IF NOT EXISTS` tolerance for partial files that somehow
/// already grew the shape.
fn migrate_v7_to_v8(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    if !column_exists(tx, "executions", "session_id")? {
        tx.execute_batch("ALTER TABLE executions ADD COLUMN session_id TEXT;")?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS executions_by_session
           ON executions(session_id, id);",
    )?;
    tx.execute_batch(TASKS_DDL)?;
    tx.execute_batch(PULL_REQUESTS_DDL)?;
    Ok(())
}

/// v8 → v9: every legacy execution and telemetry row fails closed. The v10
/// lifecycle migration supersedes v9's former writer-side optimistic start.
fn migrate_v8_to_v9(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    if !column_exists(tx, "executions", "usage_complete")? {
        tx.execute_batch(
            "ALTER TABLE executions ADD COLUMN usage_complete INTEGER NOT NULL DEFAULT 0
               CHECK(usage_complete IN (0, 1));",
        )?;
    }
    if !column_exists(tx, "telemetry", "call_role")? {
        tx.execute_batch(
            "ALTER TABLE telemetry ADD COLUMN call_role TEXT NOT NULL DEFAULT 'unknown';",
        )?;
    }
    if !column_exists(tx, "telemetry", "usage_complete")? {
        tx.execute_batch(
            "ALTER TABLE telemetry ADD COLUMN usage_complete INTEGER NOT NULL DEFAULT 0
               CHECK(usage_complete IN (0, 1));",
        )?;
    }
    Ok(())
}

/// v9 → v10: replace the optimistic execution bit with an explicit lifecycle.
/// Historic finalized rows retain `complete` only when their v9 bit was true;
/// unfinished rows become pending and every other row remains fail-closed.
fn migrate_v9_to_v10(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    if !column_exists(tx, "executions", "usage_status")? {
        tx.execute_batch(
            "ALTER TABLE executions ADD COLUMN usage_status TEXT NOT NULL DEFAULT 'incomplete'
               CHECK(usage_status IN ('pending', 'complete', 'incomplete'));
             UPDATE executions
                SET usage_status = CASE
                    WHEN finished_at IS NULL THEN 'pending'
                    WHEN usage_complete = 1 THEN 'complete'
                    ELSE 'incomplete'
                END,
                    usage_complete = CASE
                    WHEN finished_at IS NOT NULL AND usage_complete = 1 THEN 1
                    ELSE 0
                END;",
        )?;
    }
    Ok(())
}

/// Run one migration in its own transaction, stamping `user_version` before
/// commit so version and shape can never disagree on disk. The caller has
/// already suspended foreign-key enforcement (a no-op inside a
/// transaction).
pub(crate) fn apply_migration(
    conn: &mut Connection,
    migration: Migration,
    target: i64,
) -> Result<()> {
    let tx = conn.transaction()?;
    migration(&tx)?;
    // lang_altertable §7 requires a full FK audit before committing work
    // done with enforcement off. No store table declares foreign keys
    // today, so this passes trivially — it is what keeps the runner safe
    // for schemas that will.
    let violations: i64 =
        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations > 0 {
        return Err(StoreError(format!(
            "migration to schema version {target} would leave {violations} \
             foreign-key violation(s); rolling back"
        )));
    }
    tx.pragma_update(None, "user_version", target)?;
    tx.commit().map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v9_migration_fails_legacy_usage_closed() {
        let mut conn = Connection::open_in_memory().expect("db");
        conn.execute_batch(
            "CREATE TABLE executions (id INTEGER PRIMARY KEY);
             INSERT INTO executions (id) VALUES (1);
             CREATE TABLE telemetry (execution_id INTEGER, step INTEGER);
             INSERT INTO telemetry (execution_id, step) VALUES (1, 0);",
        )
        .expect("legacy schema");

        apply_migration(&mut conn, migrate_v8_to_v9, 9).expect("migrate");

        let execution_complete: bool = conn
            .query_row(
                "SELECT usage_complete FROM executions WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .expect("execution bit");
        let (role, call_complete): (String, bool) = conn
            .query_row(
                "SELECT call_role, usage_complete FROM telemetry WHERE execution_id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("telemetry defaults");
        assert!(!execution_complete);
        assert_eq!(role, "unknown");
        assert!(!call_complete);
    }

    #[test]
    fn v10_migration_derives_lifecycle_without_exporting_unfinished_rows() {
        let mut conn = Connection::open_in_memory().expect("db");
        conn.execute_batch(
            "CREATE TABLE executions (
               id INTEGER PRIMARY KEY,
               finished_at TEXT,
               usage_complete INTEGER NOT NULL
             );
             INSERT INTO executions VALUES (1, NULL, 1);
             INSERT INTO executions VALUES (2, '2026-07-21', 1);
             INSERT INTO executions VALUES (3, '2026-07-21', 0);",
        )
        .expect("v9 schema");

        apply_migration(&mut conn, migrate_v9_to_v10, 10).expect("migrate");

        let mut stmt = conn
            .prepare("SELECT usage_status, usage_complete FROM executions ORDER BY id")
            .unwrap();
        let rows: Vec<(String, bool)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("pending".into(), false),
                ("complete".into(), true),
                ("incomplete".into(), false),
            ]
        );
    }
}
