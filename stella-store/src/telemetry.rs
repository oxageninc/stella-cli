//! Per-call telemetry persistence and its execution-level trust boundary.

use rusqlite::params;

use crate::{Result, Store, sqlite_i64};

/// One StepUsage-shaped telemetry record (mirrors the event, plus the
/// derived cache-miss column so analytics never re-derive it).
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryRow {
    pub step: u64,
    pub provider: String,
    pub call_role: String,
    pub model: String,
    pub input_tokens: u64,
    /// The engine's raw pre-call estimate of `input_tokens` — paired they
    /// are one drift sample ([`Store::drift_samples`]); 0 means no estimate.
    pub estimated_input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_miss_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub retries: u32,
    pub tool_calls: u64,
    pub usage_complete: bool,
}

impl Store {
    /// Record one uniquely identified model call's telemetry.
    pub fn record_telemetry(&self, execution_id: i64, row: &TelemetryRow) -> Result<()> {
        let step = sqlite_i64("telemetry step", row.step)?;
        let input_tokens = sqlite_i64("telemetry input tokens", row.input_tokens)?;
        let estimated_input_tokens = sqlite_i64(
            "telemetry estimated input tokens",
            row.estimated_input_tokens,
        )?;
        let output_tokens = sqlite_i64("telemetry output tokens", row.output_tokens)?;
        let cache_read_tokens = sqlite_i64("telemetry cache-read tokens", row.cache_read_tokens)?;
        let cache_miss_tokens = sqlite_i64("telemetry cache-miss tokens", row.cache_miss_tokens)?;
        let cache_write_tokens =
            sqlite_i64("telemetry cache-write tokens", row.cache_write_tokens)?;
        let duration_ms = sqlite_i64("telemetry duration", row.duration_ms)?;
        let tool_calls = sqlite_i64("telemetry tool calls", row.tool_calls)?;
        self.lock().execute(
            "INSERT INTO telemetry (execution_id, step, provider, call_role, model, input_tokens, \
             estimated_input_tokens, output_tokens, cache_read_tokens, cache_miss_tokens, \
             cache_write_tokens, cost_usd, duration_ms, retries, tool_calls, usage_complete) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                execution_id,
                step,
                row.provider,
                row.call_role,
                row.model,
                input_tokens,
                estimated_input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_miss_tokens,
                cache_write_tokens,
                row.cost_usd,
                duration_ms,
                row.retries,
                tool_calls,
                row.usage_complete,
            ],
        )?;
        Ok(())
    }

    /// Recent complete (estimated, actual) input-token pairs, oldest first.
    pub fn drift_samples(
        &self,
        provider: &str,
        model: &str,
        limit: usize,
    ) -> Result<Vec<(u64, u64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT estimated_input_tokens, input_tokens FROM (
               SELECT estimated_input_tokens, input_tokens, execution_id, step
               FROM telemetry
               WHERE provider = ? AND model = ? AND usage_complete = 1
                 AND estimated_input_tokens > 0 AND input_tokens > 0
               ORDER BY execution_id DESC, step DESC
               LIMIT ?
             ) ORDER BY execution_id ASC, step ASC",
        )?;
        let rows = stmt.query_map(params![provider, model, limit as i64], |row| {
            let estimated: i64 = row.get(0)?;
            let actual: i64 = row.get(1)?;
            Ok((estimated as u64, actual as u64))
        })?;
        let mut samples = Vec::new();
        for row in rows {
            samples.push(row?);
        }
        Ok(samples)
    }

    /// Close an execution record with a complete local accounting envelope.
    pub fn finish_execution(&self, execution_id: i64, outcome: &str, cost_usd: f64) -> Result<()> {
        self.finish_execution_accounted(execution_id, outcome, cost_usd, true)
    }

    /// Close an execution while monotonically carrying renderer/forwarder
    /// persistence completeness into the durable export gate.
    pub fn finish_execution_accounted(
        &self,
        execution_id: i64,
        outcome: &str,
        cost_usd: f64,
        usage_complete: bool,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE executions SET finished_at = CURRENT_TIMESTAMP, outcome = ?1, cost_usd = ?2, \
                    usage_complete = CASE \
                      WHEN usage_status = 'incomplete' OR NOT ?3 THEN 0 ELSE 1 END, \
                    usage_status = CASE \
                      WHEN usage_status = 'incomplete' OR NOT ?3 \
                      THEN 'incomplete' ELSE 'complete' END WHERE id = ?4",
            params![outcome, cost_usd, usage_complete, execution_id],
        )?;
        Ok(())
    }

    /// Permanently downgrade one execution's accounting state.
    pub fn mark_execution_usage_incomplete(&self, execution_id: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE executions SET usage_complete = 0, usage_status = 'incomplete' WHERE id = ?1",
            params![execution_id],
        )?;
        Ok(())
    }

    /// Durable completeness bit used by local and enterprise projections.
    pub fn execution_usage_complete(&self, execution_id: i64) -> Result<bool> {
        self.lock()
            .query_row(
                "SELECT finished_at IS NOT NULL AND usage_complete = 1 \
                        AND usage_status = 'complete' FROM executions WHERE id = ?1",
                params![execution_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }
}
