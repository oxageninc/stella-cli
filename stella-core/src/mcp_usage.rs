//! Shared MCP tool-usage telemetry: the record shape and the session-scoped
//! ledger handle.
//!
//! External MCP servers' tools are called through `stella-mcp`'s `McpToolSet`,
//! which bypasses `stella-tools::ToolRegistry` entirely (an `mcp__…` name never
//! falls through to the native executor). So the native file-touch ledger can
//! never observe an MCP call. This module is the shared seam that lets it be
//! observed anyway: `McpToolSet` appends an [`McpUsageRecord`] on every
//! successful call, and the CLI drains the same [`McpUsageLedger`] handle into
//! `store.db` once per execution — the same "written by one object, drained via
//! another" shape the memory-citation ledger uses.
//!
//! Both `stella-mcp` and `stella-tools` depend on `stella-core`, so the type
//! lives here to avoid a dependency cycle between them.

use std::sync::{Arc, Mutex};

/// One recorded MCP tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpUsageRecord {
    /// The MCP server the tool belongs to (the config alias / namespace
    /// segment), e.g. `github`.
    pub server: String,
    /// The un-namespaced tool name the model invoked, e.g. `search_issues`.
    pub tool: String,
    /// Why the call was made, taken from a `reason` key in the tool input when
    /// present. External MCP tools rarely define one, so this is usually empty
    /// — the count and tool identity are always reliable, the reason is
    /// best-effort.
    pub reason: String,
    /// Call time in milliseconds since the Unix epoch (captured at call time,
    /// not at drain time, so the timestamp reflects when the tool actually ran).
    pub called_at_ms: u64,
}

impl McpUsageRecord {
    /// Build a record, stamping the call time from the system clock.
    pub fn now(
        server: impl Into<String>,
        tool: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            server: server.into(),
            tool: tool.into(),
            reason: reason.into(),
            called_at_ms: now_ms(),
        }
    }
}

/// Current time in milliseconds since the Unix epoch (saturating at 0 if the
/// clock is before the epoch — never panics).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A session-scoped, drain-once ledger of MCP tool calls, shared by two owners:
/// the `McpToolSet` clones one handle to *append*, and `ToolRegistry` holds
/// another to *drain* in `record_execution_end`. Draining (rather than a
/// cumulative snapshot) is what lets each call be persisted under exactly one
/// execution id — re-persisting under later executions would inflate the count.
pub type McpUsageLedger = Arc<Mutex<Vec<McpUsageRecord>>>;

/// Append a record to a ledger, tolerating a poisoned lock (telemetry must
/// never take down a tool call).
pub fn push_usage(ledger: &McpUsageLedger, record: McpUsageRecord) {
    let mut guard = ledger.lock().unwrap_or_else(|p| p.into_inner());
    guard.push(record);
}

/// Drain a ledger, returning everything recorded so far and leaving it empty.
pub fn drain_usage(ledger: &McpUsageLedger) -> Vec<McpUsageRecord> {
    let mut guard = ledger.lock().unwrap_or_else(|p| p.into_inner());
    std::mem::take(&mut *guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_drain_moves_records_out_and_leaves_empty() {
        let ledger: McpUsageLedger = Arc::default();
        push_usage(&ledger, McpUsageRecord::now("github", "search_issues", ""));
        push_usage(&ledger, McpUsageRecord::now("fs", "read", "inspect config"));

        let drained = drain_usage(&ledger);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].server, "github");
        assert_eq!(drained[1].reason, "inspect config");

        // Drained once — a second drain is empty (no double-persist).
        assert!(drain_usage(&ledger).is_empty());
    }

    #[test]
    fn a_shared_clone_sees_appends_from_the_other_owner() {
        let writer: McpUsageLedger = Arc::default();
        let draining = writer.clone();
        push_usage(&writer, McpUsageRecord::now("s", "t", ""));
        assert_eq!(drain_usage(&draining).len(), 1);
    }
}
