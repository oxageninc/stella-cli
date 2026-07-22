//! Execution-store closeout and per-event telemetry persistence.

use colored::Colorize;
use stella_protocol::AgentEvent;
use stella_store::{Store, TelemetryRow};
use stella_tools::ToolRegistry;

/// Best-effort end-of-execution records: the session's file-touch telemetry
/// (read straight off the registry's ledger), the memory citations the
/// `cite_memory` tool collected this turn (drained, so each lands under
/// exactly one execution — the promotion gate counts them), the
/// agent-invocation log (also drained — each invocation is attributed to
/// exactly the execution it happened under), and how the run ended. A
/// failure must not abort the turn, but it must not vanish either — the
/// store is the durable audit record of what the agent did. Returns `false`
/// when any write failed so the caller can surface a warning on its own
/// channel (stderr for the CLI surfaces, a deck event for the TUI, where
/// stderr belongs to the alternate screen).
pub(crate) fn record_execution_end(
    store: &Store,
    execution_id: i64,
    registry: &ToolRegistry,
    outcome_label: &str,
    cost_usd: f64,
) -> bool {
    let files = file_touch_rows(registry);
    let files_ok = store.record_files_touched(execution_id, &files).is_ok();
    let citations = memory_citation_rows(registry);
    let citations_ok = store
        .record_memory_citations(execution_id, &citations)
        .is_ok();
    let uses: Vec<stella_store::AgentUseRow> = registry
        .drain_agent_uses()
        .into_iter()
        .map(|u| stella_store::AgentUseRow {
            agent: u.agent,
            version: u.version,
            reason: u.reason,
        })
        .collect();
    let uses_ok = uses.is_empty() || store.record_agent_uses(execution_id, &uses).is_ok();
    let mcp_usage = mcp_usage_rows(registry);
    let mcp_usage_ok = store.record_mcp_usage(execution_id, &mcp_usage).is_ok();
    let finish_ok = store
        .finish_execution(execution_id, outcome_label, cost_usd)
        .is_ok();
    // Data plane (all best-effort — aggregation must never fail a turn, so
    // these are NOT folded into the returned success flag): normalize the
    // turn's tool calls from its event stream, record the objective
    // self-reflection (prompt + produced_output/wrote_files/truncated), and —
    // after `finish_execution` set the outcome — roll the turn up into the
    // user-tier usage.db for cross-project stats.
    let _ = store.materialize_tool_calls(execution_id);
    let _ = store.finalize_execution_reflection(execution_id);
    let _ = store.sync_to_usage_default(execution_id);
    let _ = crate::enterprise_telemetry::enqueue_finalized_execution(store, execution_id);
    files_ok && citations_ok && uses_ok && mcp_usage_ok && finish_ok
}

/// The registry's MCP tool-usage ledger as store rows. This DRAINS the ledger
/// (like memory citations) so each call persists under exactly one execution —
/// re-persisting under later turns would inflate the per-tool call counts.
fn mcp_usage_rows(registry: &ToolRegistry) -> Vec<stella_store::McpUsageRow> {
    registry
        .take_mcp_usage()
        .into_iter()
        .map(|u| stella_store::McpUsageRow {
            server: u.server,
            tool: u.tool,
            reason: u.reason,
            called_at_ms: u.called_at_ms as i64,
        })
        .collect()
}

/// The registry's session file-touch telemetry as store rows: one per
/// normalized path, `ops` as the deduplicated CRUD letters in
/// first-occurrence order, and the ordered audit log serialized to JSON.
fn file_touch_rows(registry: &ToolRegistry) -> Vec<stella_store::FileTouchRow> {
    registry
        .file_touch_telemetry()
        .files_touched
        .into_iter()
        .map(|record| stella_store::FileTouchRow {
            ops: record.crud_events.iter().map(|op| op.letter()).collect(),
            lines_added: record.lines_added,
            lines_removed: record.lines_removed,
            events_json: serde_json::to_string(&record.events).unwrap_or_else(|_| "[]".into()),
            path: record.path,
        })
        .collect()
}

/// The registry's memory citations as store rows. This DRAINS the ledger
/// (unlike the cumulative file-touch snapshot) so each citation persists
/// under exactly one execution — the >10 promotion count must never be
/// inflated by re-persisting a citation under later turns.
fn memory_citation_rows(registry: &ToolRegistry) -> Vec<stella_store::MemoryCitationRow> {
    registry
        .take_memory_citations()
        .into_iter()
        .map(|c| stella_store::MemoryCitationRow {
            memory_id: c.memory_id,
            useful_score: c.useful_score,
            truthful: c.truthful,
            remark: c.remark,
        })
        .collect()
}

/// The stderr form of the store-write warning, for the non-deck surfaces.
pub(crate) fn warn_store_write_failed(what: &str) {
    eprintln!(
        "  {} store write failed — {what} for this execution is incomplete",
        "⚠".yellow()
    );
}

/// Persist one drained event to an open execution record: append it to the
/// event stream and, for `StepUsage`, add a telemetry row. Shared by
/// [`super::spawn_renderer`] (one-shot/REPL rendering) and the command deck's
/// event forwarder (`crate::command_deck`), so the store's write path lives in
/// exactly one place. Returns `false` when the event-stream append failed OR
/// (for `StepUsage`) the telemetry insert failed, so the caller's once-only
/// "telemetry for this execution is incomplete" warning actually covers the
/// telemetry row too — a telemetry-only failure must not stay silent.
pub(crate) fn persist_event(
    store: &Store,
    execution_id: i64,
    seq: u64,
    event: &AgentEvent,
    provider_id: &str,
) -> bool {
    let recorded = store.record_event(execution_id, seq, event).is_ok();
    // True when the event carried no StepUsage or the insert succeeded.
    let mut telemetry_ok = true;
    if let AgentEvent::StepUsage {
        step,
        model,
        input_tokens,
        output_tokens,
        cached_input_tokens,
        cache_write_tokens,
        estimated_input_tokens,
        cost_usd,
        duration_ms,
        retries,
        tool_calls,
        ..
    } = event
    {
        telemetry_ok = store
            .record_telemetry(
                execution_id,
                &TelemetryRow {
                    step: *step as u64,
                    provider: provider_id.to_string(),
                    model: model.clone(),
                    input_tokens: *input_tokens,
                    estimated_input_tokens: *estimated_input_tokens,
                    output_tokens: *output_tokens,
                    cache_read_tokens: *cached_input_tokens,
                    cache_miss_tokens: input_tokens.saturating_sub(*cached_input_tokens),
                    // Straight from the provider's usage envelope (Anthropic
                    // `cache_creation_input_tokens`, Bedrock
                    // `cacheWriteInputTokens`); 0 for providers that never
                    // report cache writes.
                    cache_write_tokens: *cache_write_tokens,
                    cost_usd: *cost_usd,
                    duration_ms: *duration_ms,
                    retries: *retries,
                    tool_calls: *tool_calls as u64,
                },
            )
            .is_ok();
        // Telemetry stores the wire string verbatim (above); this makes
        // that string JOINABLE: an echoed form the catalog doesn't know
        // yet (dated snapshot, region prefix, gateway-routed id) gets
        // matched to its model card and registered as a learned alias.
        // Best-effort and deduped in-process — never slows the write path.
        crate::model_catalog::note_wire_model(provider_id, model);
    }
    recorded && telemetry_ok
}
