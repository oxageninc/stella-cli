use super::*;

pub(super) fn migrate_store_export_schema(conn: &mut Connection) -> Result<()> {
    let has_compaction_boundary: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('enterprise_export_enrollment')
         WHERE name = 'compacted_through_execution_id')",
        [],
        |row| row.get(0),
    )?;
    if !has_compaction_boundary {
        conn.execute_batch(
            "ALTER TABLE enterprise_export_enrollment
             ADD COLUMN compacted_through_execution_id INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    for (column, ddl) in [
        (
            "skipped_rows",
            "ALTER TABLE enterprise_export_enrollment ADD COLUMN skipped_rows INTEGER NOT NULL DEFAULT 0;",
        ),
        (
            "skipped_missing_rollup_rows",
            "ALTER TABLE enterprise_export_enrollment ADD COLUMN skipped_missing_rollup_rows INTEGER NOT NULL DEFAULT 0;",
        ),
        (
            "skipped_malformed_nonce_rows",
            "ALTER TABLE enterprise_export_enrollment ADD COLUMN skipped_malformed_nonce_rows INTEGER NOT NULL DEFAULT 0;",
        ),
        (
            "skipped_malformed_rollup_rows",
            "ALTER TABLE enterprise_export_enrollment ADD COLUMN skipped_malformed_rollup_rows INTEGER NOT NULL DEFAULT 0;",
        ),
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('enterprise_export_enrollment')
             WHERE name = ?1)",
            params![column],
            |row| row.get(0),
        )?;
        if !exists {
            conn.execute_batch(ddl)?;
        }
    }
    let has_export_nonce: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('enterprise_export_ledger')
         WHERE name = 'export_nonce')",
        [],
        |row| row.get(0),
    )?;
    if !has_export_nonce {
        conn.execute_batch(
            "ALTER TABLE enterprise_export_ledger
             ADD COLUMN export_nonce TEXT NOT NULL DEFAULT '';",
        )?;
        conn.execute(
            "INSERT INTO enterprise_export_migration
             (singleton, version, last_rowid, migrated_rows, batches_completed, is_complete)
             VALUES (1, 1, 0, 0, 0, 0)
             ON CONFLICT(singleton) DO UPDATE SET
                 version = 1, last_rowid = 0, migrated_rows = 0,
                 batches_completed = 0, is_complete = 0",
            [],
        )?;
    } else {
        let state_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM enterprise_export_migration
             WHERE singleton = 1)",
            [],
            |row| row.get(0),
        )?;
        if !state_exists {
            let empty_nonce_exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM enterprise_export_ledger
                 WHERE export_nonce = '')",
                [],
                |row| row.get(0),
            )?;
            conn.execute(
                "INSERT INTO enterprise_export_migration
                 (singleton, version, last_rowid, migrated_rows, batches_completed, is_complete)
                 VALUES (1, 1, 0, 0, 0, ?1)",
                params![i64::from(!empty_nonce_exists)],
            )?;
        }
    }
    for _ in 0..LEGACY_EXPORT_MIGRATION_BATCHES_PER_OPEN {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (last_rowid, is_complete): (i64, bool) = tx.query_row(
            "SELECT last_rowid, is_complete FROM enterprise_export_migration
             WHERE singleton = 1 AND version = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if is_complete {
            tx.commit()?;
            break;
        }
        let batch = {
            let mut stmt = tx.prepare(
                "SELECT rowid FROM enterprise_export_ledger
                 WHERE rowid > ?1 AND export_nonce = '' ORDER BY rowid LIMIT ?2",
            )?;
            let rows = stmt.query_map(
                params![last_rowid, LEGACY_EXPORT_MIGRATION_BATCH_ROWS as i64],
                |row| row.get::<_, i64>(0),
            )?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        if batch.is_empty() {
            tx.execute(
                "UPDATE enterprise_export_migration SET is_complete = 1
                 WHERE singleton = 1",
                [],
            )?;
            tx.commit()?;
            break;
        }
        for rowid in &batch {
            tx.execute(
                "UPDATE enterprise_export_ledger SET export_nonce = ?1 WHERE rowid = ?2",
                params![random_export_nonce(), rowid],
            )?;
        }
        let last_rowid = *batch.last().unwrap_or(&last_rowid);
        let remaining: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM enterprise_export_ledger
             WHERE rowid > ?1 AND export_nonce = '')",
            params![last_rowid],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE enterprise_export_migration
             SET last_rowid = ?1, migrated_rows = migrated_rows + ?2,
                 batches_completed = batches_completed + 1, is_complete = ?3
             WHERE singleton = 1",
            params![last_rowid, batch.len() as i64, i64::from(!remaining)],
        )?;
        tx.commit()?;
        if !remaining {
            break;
        }
    }
    Ok(())
}

pub(super) fn migrate_spool_schema(conn: &mut Connection) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
         WHERE type = 'table' AND name = 'operational_spool')",
        [],
        |row| row.get(0),
    )?;
    if exists {
        let has_sink: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('operational_spool')
             WHERE name = 'sink_fingerprint')",
            [],
            |row| row.get(0),
        )?;
        let has_sequence: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('operational_spool')
             WHERE name = 'insertion_seq')",
            [],
            |row| row.get(0),
        )?;
        if !has_sink || !has_sequence {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "ALTER TABLE operational_spool RENAME TO operational_spool_legacy;
                 CREATE TABLE operational_spool (
                     insertion_seq INTEGER PRIMARY KEY AUTOINCREMENT,
                     event_id TEXT NOT NULL,
                     sink_fingerprint TEXT NOT NULL,
                     payload BLOB NOT NULL,
                     payload_bytes INTEGER NOT NULL CHECK(payload_bytes >= 0),
                     created_at_ms INTEGER NOT NULL,
                     attempts INTEGER NOT NULL DEFAULT 0,
                     next_attempt_ms INTEGER NOT NULL DEFAULT 0,
                     leased_by TEXT,
                     lease_until_ms INTEGER,
                     UNIQUE(sink_fingerprint, event_id)
                 );",
            )?;
            tx.execute(
                "INSERT INTO operational_spool
                 (event_id, sink_fingerprint, payload, payload_bytes, created_at_ms,
                  attempts, next_attempt_ms, leased_by, lease_until_ms)
                 SELECT event_id, ?1, payload, payload_bytes, created_at_ms,
                        attempts, next_attempt_ms, leased_by, lease_until_ms
                 FROM operational_spool_legacy ORDER BY rowid",
                params![LEGACY_UNBOUND_SINK],
            )?;
            tx.execute_batch("DROP TABLE operational_spool_legacy;")?;
            tx.commit()?;
        }
    }
    let meta_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
         WHERE type = 'table' AND name = 'operational_spool_meta')",
        [],
        |row| row.get(0),
    )?;
    if meta_exists {
        let has_rollover: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('operational_spool_meta')
             WHERE name = 'rollover_discarded_rows')",
            [],
            |row| row.get(0),
        )?;
        if !has_rollover {
            conn.execute_batch(
                "ALTER TABLE operational_spool_meta
                 ADD COLUMN rollover_discarded_rows INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        let has_corrupt: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('operational_spool_meta')
             WHERE name = 'corrupt_dropped_rows')",
            [],
            |row| row.get(0),
        )?;
        if !has_corrupt {
            conn.execute_batch(
                "ALTER TABLE operational_spool_meta
                 ADD COLUMN corrupt_dropped_rows INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
    }
    let clock_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
         WHERE type = 'table' AND name = 'operational_spool_clock')",
        [],
        |row| row.get(0),
    )?;
    if clock_exists {
        let has_generation: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('operational_spool_clock')
             WHERE name = 'clock_generation')",
            [],
            |row| row.get(0),
        )?;
        if !has_generation {
            conn.execute_batch(
                "ALTER TABLE operational_spool_clock
                 ADD COLUMN clock_generation INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
    }
    Ok(())
}
