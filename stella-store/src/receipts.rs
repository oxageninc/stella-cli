//! Context-receipts persistence (spec §4/§5): the block registry and the
//! per-step request manifest. Content-free — these rows carry digests, ids,
//! and small ints, never payload bytes. The preimage of any block lives in the
//! originating event in the journal; these tables are the queryable index over
//! it that makes any past step reconstructable and auditable.

use rusqlite::params;

use crate::{Result, Store, sqlite_i64};

/// One context block as registered at birth (`context_blocks`, spec §4).
/// `kind`/`cache_zone` are the wire enums already serialized to their
/// snake_case strings by the caller — the store stays string-typed and never
/// depends on the protocol enum shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlockRow {
    pub block_id: String,
    pub kind: String,
    pub origin_turn: u32,
    pub origin_step: u64,
    pub call_id: Option<String>,
    pub memory_id: Option<String>,
    pub token_cost: u32,
    pub content_digest: String,
    pub citation_label: Option<String>,
    /// Local-only preimage for gap kinds the journal cannot resolve (system
    /// prefix, assembled user/recall message); `None` for journal-resolvable
    /// kinds (spec §5.3). Never leaves the local store.
    pub content: Option<String>,
}

/// One block's membership in a step's manifest (`step_manifest`, spec §5),
/// in wire order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestBlockRow {
    pub block_id: String,
    pub cache_zone: String,
    pub token_cost: u32,
    pub resident_since_step: u64,
    /// The sent-message this block belonged to; reconstruction regroups blocks
    /// sharing a `message_index` back into one `CompletionMessage` (spec §5.1).
    pub message_index: u64,
}

/// A full per-step manifest: the header (`step_receipt`) plus its ordered
/// blocks (`step_manifest`). Persisted atomically so a receipt is never
/// half-written.
#[derive(Debug, Clone, PartialEq)]
pub struct StepManifestRow {
    pub turn_instance: u32,
    pub step: u64,
    pub provider: String,
    pub model: String,
    pub call_role: String,
    pub effective_budget_tokens: u64,
    pub calibration_factor: f64,
    pub estimated_input_tokens: u64,
    pub blocks: Vec<ManifestBlockRow>,
}

impl Store {
    /// Register one context block. Idempotent: a byte-identical block
    /// re-entering context resolves to the same `block_id`, so a repeat
    /// registration is a no-op (`INSERT OR IGNORE`), never an error or a
    /// double-count — matching the content-addressed identity contract.
    pub fn record_context_block(&self, execution_id: i64, row: &ContextBlockRow) -> Result<()> {
        let origin_step = sqlite_i64("context block origin step", row.origin_step)?;
        let token_cost = i64::from(row.token_cost);
        self.lock().execute(
            "INSERT OR IGNORE INTO context_blocks
               (execution_id, block_id, kind, origin_turn, origin_step, call_id, memory_id,
                token_cost, content_digest, citation_label, content)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                execution_id,
                row.block_id,
                row.kind,
                i64::from(row.origin_turn),
                origin_step,
                row.call_id,
                row.memory_id,
                token_cost,
                row.content_digest,
                row.citation_label,
                row.content,
            ],
        )?;
        Ok(())
    }

    /// Persist one step's full manifest — the header row plus every ordered
    /// block row — atomically. `INSERT OR REPLACE` so a re-emitted manifest for
    /// the same (turn_instance, step) overwrites cleanly rather than colliding
    /// on the primary key (the engine emits one manifest per step, but a caller
    /// that replays must be able to re-persist idempotently).
    pub fn record_step_manifest(&self, execution_id: i64, row: &StepManifestRow) -> Result<()> {
        let step = sqlite_i64("manifest step", row.step)?;
        let turn = i64::from(row.turn_instance);
        let budget = sqlite_i64("manifest effective budget", row.effective_budget_tokens)?;
        let estimated = sqlite_i64("manifest estimated input", row.estimated_input_tokens)?;
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO step_receipt
               (execution_id, turn_instance, step, provider, model, call_role,
                effective_budget_tokens, calibration_factor, estimated_input_tokens)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                execution_id,
                turn,
                step,
                row.provider,
                row.model,
                row.call_role,
                budget,
                row.calibration_factor,
                estimated,
            ],
        )?;
        // Clear any prior ordinals for this step before rewriting, so a
        // shorter re-emitted manifest cannot leave stale tail rows behind.
        tx.execute(
            "DELETE FROM step_manifest
             WHERE execution_id = ? AND turn_instance = ? AND step = ?",
            params![execution_id, turn, step],
        )?;
        // token_cost is a property of the block (context_blocks.token_cost),
        // not of a manifest entry, so step_manifest stores only ordering, zone,
        // and residency; the reader joins token_cost back from context_blocks.
        for (ordinal, block) in row.blocks.iter().enumerate() {
            let ordinal = sqlite_i64("manifest ordinal", ordinal as u64)?;
            let resident = sqlite_i64("manifest residency", block.resident_since_step)?;
            let message_index = sqlite_i64("manifest message index", block.message_index)?;
            tx.execute(
                "INSERT INTO step_manifest
                   (execution_id, turn_instance, step, ordinal, block_id, cache_zone,
                    resident_since_step, message_index)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    execution_id,
                    turn,
                    step,
                    ordinal,
                    block.block_id,
                    block.cache_zone,
                    resident,
                    message_index,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every block registered for an execution, insertion order.
    pub fn context_blocks(&self, execution_id: i64) -> Result<Vec<ContextBlockRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT block_id, kind, origin_turn, origin_step, call_id, memory_id,
                    token_cost, content_digest, citation_label, content
             FROM context_blocks WHERE execution_id = ? ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map(params![execution_id], |r| {
                Ok(ContextBlockRow {
                    block_id: r.get(0)?,
                    kind: r.get(1)?,
                    origin_turn: r.get::<_, i64>(2)? as u32,
                    origin_step: r.get::<_, i64>(3)? as u64,
                    call_id: r.get(4)?,
                    memory_id: r.get(5)?,
                    token_cost: r.get::<_, i64>(6)? as u32,
                    content_digest: r.get(7)?,
                    citation_label: r.get(8)?,
                    content: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The ordered blocks of one step's manifest — the receipt of exactly what
    /// the model saw, in wire order.
    pub fn step_manifest(
        &self,
        execution_id: i64,
        turn_instance: u32,
        step: u64,
    ) -> Result<Vec<ManifestBlockRow>> {
        let step = sqlite_i64("manifest step", step)?;
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT sm.block_id, sm.cache_zone, cb.token_cost, sm.resident_since_step,
                    sm.message_index
             FROM step_manifest sm
             LEFT JOIN context_blocks cb
               ON cb.execution_id = sm.execution_id AND cb.block_id = sm.block_id
             WHERE sm.execution_id = ? AND sm.turn_instance = ? AND sm.step = ?
             ORDER BY sm.ordinal",
        )?;
        let rows = stmt
            .query_map(params![execution_id, i64::from(turn_instance), step], |r| {
                Ok(ManifestBlockRow {
                    block_id: r.get(0)?,
                    cache_zone: r.get(1)?,
                    // token_cost may be NULL if the block row is missing (a
                    // manifest referencing an unregistered block — a bug worth
                    // surfacing, not crashing on); default 0.
                    token_cost: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u32,
                    resident_since_step: r.get::<_, i64>(3)? as u64,
                    message_index: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(id: &str, step: u64) -> ContextBlockRow {
        ContextBlockRow {
            block_id: id.into(),
            kind: "tool_result".into(),
            origin_turn: 0,
            origin_step: step,
            call_id: Some(format!("call_{step}")),
            memory_id: None,
            token_cost: 100 + step as u32,
            content_digest: format!("sha256:{id}"),
            citation_label: None,
            content: None,
        }
    }

    #[test]
    fn block_registration_is_idempotent_on_the_content_addressed_id() {
        let store = Store::in_memory().unwrap();
        let id = store
            .begin_execution("run", "p", "anthropic", "opus")
            .unwrap();
        store.record_context_block(id, &block("blk_a", 0)).unwrap();
        // Re-registering the same block (identical content re-enters context)
        // must be a silent no-op, never an error or a duplicate row.
        store.record_context_block(id, &block("blk_a", 0)).unwrap();
        store.record_context_block(id, &block("blk_b", 1)).unwrap();
        let blocks = store.context_blocks(id).unwrap();
        assert_eq!(blocks.len(), 2, "identical block must collapse to one row");
        assert_eq!(blocks[0].block_id, "blk_a");
        assert_eq!(blocks[1].call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn manifest_round_trips_in_wire_order_with_its_header() {
        let store = Store::in_memory().unwrap();
        let id = store
            .begin_execution("run", "p", "anthropic", "opus")
            .unwrap();
        store
            .record_context_block(id, &block("blk_sys", 0))
            .unwrap();
        store
            .record_context_block(id, &block("blk_tail", 3))
            .unwrap();
        let manifest = StepManifestRow {
            turn_instance: 0,
            step: 3,
            provider: "anthropic".into(),
            model: "opus".into(),
            call_role: "worker".into(),
            effective_budget_tokens: 136_363,
            calibration_factor: 1.1,
            estimated_input_tokens: 203,
            blocks: vec![
                ManifestBlockRow {
                    block_id: "blk_sys".into(),
                    cache_zone: "stable_prefix".into(),
                    token_cost: 100,
                    resident_since_step: 0,
                    message_index: 0,
                },
                ManifestBlockRow {
                    block_id: "blk_tail".into(),
                    cache_zone: "volatile".into(),
                    token_cost: 103,
                    resident_since_step: 3,
                    message_index: 1,
                },
            ],
        };
        store.record_step_manifest(id, &manifest).unwrap();

        let back = store.step_manifest(id, 0, 3).unwrap();
        assert_eq!(back.len(), 2);
        // Order is the receipt — index 0 is the system prefix.
        assert_eq!(back[0].block_id, "blk_sys");
        assert_eq!(back[0].cache_zone, "stable_prefix");
        assert_eq!(back[0].resident_since_step, 0);
        assert_eq!(back[1].block_id, "blk_tail");
        // token_cost is joined back from context_blocks (the block's property).
        assert_eq!(back[1].token_cost, 103);
    }

    #[test]
    fn re_emitting_a_shorter_manifest_leaves_no_stale_tail_rows() {
        let store = Store::in_memory().unwrap();
        let id = store
            .begin_execution("run", "p", "anthropic", "opus")
            .unwrap();
        let three = StepManifestRow {
            turn_instance: 1,
            step: 2,
            provider: "anthropic".into(),
            model: "opus".into(),
            call_role: "worker".into(),
            effective_budget_tokens: 100,
            calibration_factor: 1.0,
            estimated_input_tokens: 3,
            blocks: (0..3)
                .map(|i| ManifestBlockRow {
                    block_id: format!("blk_{i}"),
                    cache_zone: "cacheable".into(),
                    token_cost: 1,
                    resident_since_step: 0,
                    message_index: 0,
                })
                .collect(),
        };
        store.record_step_manifest(id, &three).unwrap();
        let shorter = StepManifestRow {
            blocks: three.blocks[..1].to_vec(),
            ..three.clone()
        };
        store.record_step_manifest(id, &shorter).unwrap();
        let back = store.step_manifest(id, 1, 2).unwrap();
        assert_eq!(
            back.len(),
            1,
            "a shorter re-emission must replace, not append"
        );
    }
}
