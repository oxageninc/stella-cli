//! Raw per-call cache-gap facts — [`Store::cache_call_gaps`] — split out of
//! `lib.rs` to keep that file under its size ratchet.
//!
//! Every recorded model call, paired with the gap since its session's
//! previous call, is the raw material a caller (`stella-cli`'s `stats.rs`,
//! the deck's cache-panel producer) turns into the `cache_expired_rewrite`
//! counter (issues #267/#269) by pairing each row's `gap_secs` against the
//! provider's cache TTL
//! (`stella_model::cache_economics::is_cache_expired_rewrite`). No TTL or
//! pricing policy lives here — `stella-store` deliberately does not depend
//! on `stella-model`; this module only surfaces the facts the policy needs.

use crate::{Result, Store};

/// One recorded model call plus the gap since its session's previous call —
/// see [`Store::cache_call_gaps`]. `gap_secs` is `None` for a session's first
/// recorded call (nothing to have gone cold yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheCallGap {
    pub provider: String,
    pub model: String,
    pub cache_write_tokens: i64,
    pub gap_secs: Option<i64>,
}

impl Store {
    /// Every recorded model call, with the gap since its session's previous
    /// call.
    ///
    /// `LAG(t.ts)` partitioned by `e.session_id` and ordered by `(e.id,
    /// t.step)`: execution ids are assigned in AUTOINCREMENT order, so this
    /// walks a session's calls in wall-clock order across execution
    /// boundaries (a session is usually many executions/turns). Rows with no
    /// session_id (pre-v8 telemetry, or executions dispatched outside a
    /// registered session) are excluded — there is no cross-turn gap to
    /// measure without a session to chain them through. The first call in
    /// each session partition has no predecessor, so its `gap_secs` is NULL
    /// (nothing has gone cold yet).
    pub fn cache_call_gaps(&self) -> Result<Vec<CacheCallGap>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT
               e.provider,
               e.model,
               t.cache_write_tokens,
               CAST(
                 (julianday(t.ts) - julianday(
                   LAG(t.ts) OVER (PARTITION BY e.session_id ORDER BY e.id, t.step)
                 )) * 86400 AS INTEGER
               ) AS gap_secs
             FROM telemetry t
             JOIN executions e ON e.id = t.execution_id
             WHERE e.session_id IS NOT NULL
             ORDER BY e.session_id, e.id, t.step",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CacheCallGap {
                provider: row.get(0)?,
                model: row.get(1)?,
                cache_write_tokens: row.get(2)?,
                gap_secs: row.get(3)?,
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
    fn cache_call_gaps_chains_a_session_across_executions_in_call_order() {
        let store = Store::in_memory().unwrap();

        // A two-turn session "s1": two executions, one telemetry row apiece —
        // the exact shape a multi-turn `stella run`/chat session leaves
        // behind. The session's FIRST recorded call has no predecessor.
        let e1 = store
            .begin_execution("run", "p1", "anthropic", "claude")
            .unwrap();
        store.set_execution_session(e1, "s1").unwrap();
        store
            .record_telemetry(e1, &telemetry("anthropic", "claude", 0, 500))
            .unwrap();

        let e2 = store
            .begin_execution("run", "p2", "anthropic", "claude")
            .unwrap();
        store.set_execution_session(e2, "s1").unwrap();
        store
            .record_telemetry(e2, &telemetry("anthropic", "claude", 900, 0))
            .unwrap();

        // A run dispatched with no registered session (pre-v8 shape, or an
        // ad-hoc invocation) — no session to chain it through, so it must be
        // excluded entirely rather than reported with a meaningless gap.
        let e3 = store
            .begin_execution("run", "p3", "zai", "glm-5.2")
            .unwrap();
        store
            .record_telemetry(e3, &telemetry("zai", "glm-5.2", 0, 0))
            .unwrap();

        let gaps = store.cache_call_gaps().unwrap();
        assert_eq!(gaps.len(), 2, "the session-less run is excluded: {gaps:?}");
        assert_eq!(gaps[0].provider, "anthropic");
        assert_eq!(gaps[0].cache_write_tokens, 500);
        assert_eq!(
            gaps[0].gap_secs, None,
            "the session's first recorded call has no predecessor to gap against"
        );
        assert_eq!(gaps[1].cache_write_tokens, 0);
        assert!(
            gaps[1].gap_secs.is_some_and(|g| g >= 0),
            "the second call in the session has a non-negative gap: {:?}",
            gaps[1].gap_secs
        );
    }

    #[test]
    fn cache_call_gaps_empty_store_returns_nothing() {
        let store = Store::in_memory().unwrap();
        assert!(store.cache_call_gaps().unwrap().is_empty());
    }

    /// A minimal telemetry fixture — this module only cares about
    /// provider/model/cache_write_tokens, everything else is zeroed.
    fn telemetry(
        provider: &str,
        model: &str,
        cache_read: u64,
        cache_write: u64,
    ) -> crate::TelemetryRow {
        crate::TelemetryRow {
            step: 0,
            provider: provider.into(),
            model: model.into(),
            input_tokens: cache_read + 1_000,
            estimated_input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: cache_read,
            cache_miss_tokens: 1_000,
            cache_write_tokens: cache_write,
            cost_usd: 0.0,
            duration_ms: 0,
            retries: 0,
            tool_calls: 0,
        }
    }
}
