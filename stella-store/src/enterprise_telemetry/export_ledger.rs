use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{MAX_EXPORT_PAGE_ROWS, random_export_nonce, validate_sink_fingerprint};
use crate::{Result, StoreError};

/// Bounded, content-free reason for permanently skipping one malformed legacy export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterpriseExportSkipReason {
    MissingRollup,
    MalformedNonce,
    MalformedRollup,
}

impl EnterpriseExportSkipReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissingRollup => "missing_rollup",
            Self::MalformedNonce => "malformed_nonce",
            Self::MalformedRollup => "malformed_rollup",
        }
    }

    const fn counter_column(self) -> &'static str {
        match self {
            Self::MissingRollup => "skipped_missing_rollup_rows",
            Self::MalformedNonce => "skipped_malformed_nonce_rows",
            Self::MalformedRollup => "skipped_malformed_rollup_rows",
        }
    }
}

/// Durable aggregate diagnostics for malformed legacy export-ledger rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnterpriseExportLedgerStatus {
    pub skipped_rows: u64,
    pub missing_rollup_rows: u64,
    pub malformed_nonce_rows: u64,
    pub malformed_rollup_rows: u64,
}

impl crate::Store {
    pub fn enterprise_store_uuid(&self) -> Result<String> {
        let conn = self.lock();
        if let Some(value) = conn
            .query_row(
                "SELECT store_uuid FROM enterprise_export_identity WHERE singleton = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(value);
        }
        let generated = super::random_uuid_v4();
        conn.execute(
            "INSERT OR IGNORE INTO enterprise_export_identity(singleton, store_uuid) VALUES (1, ?1)",
            params![generated],
        )?;
        conn.query_row(
            "SELECT store_uuid FROM enterprise_export_identity WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    pub fn begin_enterprise_enrollment(&self, sink_fingerprint: &str) -> Result<()> {
        validate_sink_fingerprint(sink_fingerprint)?;
        self.lock().execute(
            "INSERT OR IGNORE INTO enterprise_export_enrollment
             (sink_fingerprint, enrolled_after_execution_id)
             VALUES (?1, (SELECT COALESCE(MAX(id), 0) FROM executions))",
            params![sink_fingerprint],
        )?;
        Ok(())
    }

    pub fn mark_enterprise_export_pending(
        &self,
        sink_fingerprint: &str,
        execution_id: i64,
    ) -> Result<Option<String>> {
        validate_sink_fingerprint(sink_fingerprint)?;
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let eligible: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM executions e JOIN enterprise_export_enrollment x
                  ON x.sink_fingerprint = ?1
                WHERE e.id = ?2 AND e.id > x.enrolled_after_execution_id
                  AND e.id > x.compacted_through_execution_id
                  AND e.finished_at IS NOT NULL AND e.outcome IS NOT NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM enterprise_export_skips s
                      WHERE s.sink_fingerprint = ?1 AND s.execution_id = ?2
                  ))",
            params![sink_fingerprint, execution_id],
            |row| row.get(0),
        )?;
        if !eligible {
            tx.commit()?;
            return Ok(None);
        }
        if let Some(existing) = tx
            .query_row(
                "SELECT export_nonce FROM enterprise_export_ledger
                 WHERE sink_fingerprint = ?1 AND execution_id = ?2",
                params![sink_fingerprint, execution_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if existing.is_empty() {
                let nonce = random_export_nonce();
                tx.execute(
                    "UPDATE enterprise_export_ledger SET export_nonce = ?1
                     WHERE sink_fingerprint = ?2 AND execution_id = ?3
                       AND export_nonce = ''",
                    params![nonce, sink_fingerprint, execution_id],
                )?;
                tx.commit()?;
                return Ok(Some(nonce));
            }
            tx.commit()?;
            return Ok(Some(existing));
        }
        let nonce = random_export_nonce();
        tx.execute(
            "INSERT OR IGNORE INTO enterprise_export_ledger
             (sink_fingerprint, execution_id, export_nonce, status)
             VALUES (?1, ?2, ?3, 'pending')",
            params![sink_fingerprint, execution_id, nonce],
        )?;
        let persisted = tx.query_row(
            "SELECT export_nonce FROM enterprise_export_ledger
             WHERE sink_fingerprint = ?1 AND execution_id = ?2",
            params![sink_fingerprint, execution_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(Some(persisted))
    }

    pub fn pending_enterprise_export_page(
        &self,
        sink_fingerprint: &str,
        after_execution_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<PendingEnterpriseExport>> {
        validate_sink_fingerprint(sink_fingerprint)?;
        if limit == 0 || limit > MAX_EXPORT_PAGE_ROWS {
            return Err(StoreError(
                "enterprise export page limit must be 1..=256".into(),
            ));
        }
        let after = after_execution_id.unwrap_or(0);
        let sql_limit = i64::try_from(limit)
            .map_err(|_| StoreError("invalid enterprise export page limit".into()))?;
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pending = {
            let mut stmt = tx.prepare(
                "SELECT execution_id, export_nonce FROM enterprise_export_ledger
                 WHERE sink_fingerprint = ?1 AND status = 'pending' AND execution_id > ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM enterprise_export_skips s
                       WHERE s.sink_fingerprint = enterprise_export_ledger.sink_fingerprint
                         AND s.execution_id = enterprise_export_ledger.execution_id
                   )
                 ORDER BY execution_id LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![sink_fingerprint, after, sql_limit], |row| {
                Ok(PendingEnterpriseExport {
                    execution_id: row.get(0)?,
                    export_nonce: row.get(1)?,
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for row in &mut pending {
            if row.export_nonce.is_empty() {
                row.export_nonce = random_export_nonce();
                tx.execute(
                    "UPDATE enterprise_export_ledger SET export_nonce = ?1
                     WHERE sink_fingerprint = ?2 AND execution_id = ?3
                       AND export_nonce = ''",
                    params![row.export_nonce, sink_fingerprint, row.execution_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(pending)
    }

    pub fn mark_enterprise_export_spooled(
        &self,
        sink_fingerprint: &str,
        execution_id: i64,
    ) -> Result<()> {
        validate_sink_fingerprint(sink_fingerprint)?;
        self.lock().execute(
            "UPDATE enterprise_export_ledger SET status = 'spooled'
             WHERE sink_fingerprint = ?1 AND execution_id = ?2",
            params![sink_fingerprint, execution_id],
        )?;
        Ok(())
    }

    pub fn mark_enterprise_export_skipped(
        &self,
        sink_fingerprint: &str,
        execution_id: i64,
        reason: EnterpriseExportSkipReason,
    ) -> Result<()> {
        validate_sink_fingerprint(sink_fingerprint)?;
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO enterprise_export_skips
                 (sink_fingerprint, execution_id, reason)
             SELECT ?2, ?3, ?1
             WHERE EXISTS (
                 SELECT 1 FROM enterprise_export_ledger
                 WHERE sink_fingerprint = ?2 AND execution_id = ?3 AND status = 'pending'
             )",
            params![reason.as_str(), sink_fingerprint, execution_id],
        )?;
        if inserted == 1 {
            let deleted = tx.execute(
                "DELETE FROM enterprise_export_ledger
                 WHERE sink_fingerprint = ?1 AND execution_id = ?2 AND status = 'pending'",
                params![sink_fingerprint, execution_id],
            )?;
            if deleted != 1 {
                return Err(StoreError(
                    "enterprise export skip did not consume exactly one pending row".into(),
                ));
            }
            let sql = format!(
                "UPDATE enterprise_export_enrollment
                 SET skipped_rows = skipped_rows + 1,
                     {} = {} + 1
                 WHERE sink_fingerprint = ?1",
                reason.counter_column(),
                reason.counter_column()
            );
            tx.execute(&sql, params![sink_fingerprint])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn enterprise_export_ledger_status(
        &self,
        sink_fingerprint: &str,
    ) -> Result<EnterpriseExportLedgerStatus> {
        validate_sink_fingerprint(sink_fingerprint)?;
        let row = self
            .lock()
            .query_row(
                "SELECT skipped_rows, skipped_missing_rollup_rows,
                        skipped_malformed_nonce_rows, skipped_malformed_rollup_rows
                 FROM enterprise_export_enrollment WHERE sink_fingerprint = ?1",
                params![sink_fingerprint],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((skipped, missing, nonce, rollup)) = row else {
            return Ok(EnterpriseExportLedgerStatus::default());
        };
        Ok(EnterpriseExportLedgerStatus {
            skipped_rows: checked_ledger_counter("skipped", skipped)?,
            missing_rollup_rows: checked_ledger_counter("missing rollup", missing)?,
            malformed_nonce_rows: checked_ledger_counter("malformed nonce", nonce)?,
            malformed_rollup_rows: checked_ledger_counter("malformed rollup", rollup)?,
        })
    }

    pub fn compact_enterprise_export_ledger(
        &self,
        sink_fingerprint: &str,
        retain_completed: usize,
    ) -> Result<u64> {
        validate_sink_fingerprint(sink_fingerprint)?;
        if retain_completed == 0 || retain_completed > 10_000 {
            return Err(StoreError(
                "enterprise export retention must be 1..=10000 rows".into(),
            ));
        }
        let offset = i64::try_from(retain_completed)
            .map_err(|_| StoreError("invalid enterprise export retention".into()))?;
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cutoff: Option<i64> = tx
            .query_row(
                "SELECT execution_id FROM (
                     SELECT execution_id FROM enterprise_export_ledger
                     WHERE sink_fingerprint = ?1 AND status = 'spooled'
                     UNION ALL
                     SELECT execution_id FROM enterprise_export_skips
                     WHERE sink_fingerprint = ?1
                 )
                 ORDER BY execution_id DESC LIMIT 1 OFFSET ?2",
                params![sink_fingerprint, offset],
                |row| row.get(0),
            )
            .optional()?;
        let Some(cutoff) = cutoff else {
            tx.commit()?;
            return Ok(0);
        };
        let deleted_ledger = tx.execute(
            "DELETE FROM enterprise_export_ledger
             WHERE sink_fingerprint = ?1 AND status = 'spooled'
               AND execution_id <= ?2",
            params![sink_fingerprint, cutoff],
        )?;
        let deleted_skips = tx.execute(
            "DELETE FROM enterprise_export_skips
             WHERE sink_fingerprint = ?1 AND execution_id <= ?2",
            params![sink_fingerprint, cutoff],
        )?;
        tx.execute(
            "UPDATE enterprise_export_enrollment
             SET compacted_through_execution_id = MAX(compacted_through_execution_id, ?2)
             WHERE sink_fingerprint = ?1",
            params![sink_fingerprint, cutoff],
        )?;
        tx.commit()?;
        let deleted = deleted_ledger
            .checked_add(deleted_skips)
            .ok_or_else(|| StoreError("enterprise export compaction count exceeds usize".into()))?;
        u64::try_from(deleted)
            .map_err(|_| StoreError("enterprise export compaction count exceeds u64".into()))
    }
}

fn checked_ledger_counter(label: &str, value: i64) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| StoreError(format!("enterprise export {label} counter is negative")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEnterpriseExport {
    pub execution_id: i64,
    pub export_nonce: String,
}
