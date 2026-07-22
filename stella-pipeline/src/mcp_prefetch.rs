//! The orchestrator's Best-of-N MCP pre-fetch hook (issue #248 Phase 1):
//! [`fold`] is called once at the top of a fan-out, folding shared context
//! into every candidate's starting messages instead of each candidate
//! independently paying to look it up. Split out of `pipeline.rs` to keep
//! the orchestrator under the file-size ratchet.

use stella_protocol::CompletionMessage;

use crate::ports::McpPrefetchPort;

/// `goal`/`n` describe the fan-out about to start; `port` is `None` when the
/// caller wired no pre-fetch hook. `Some` only when there is genuinely new
/// context to share — `None` means the caller's original `base_messages`
/// must be used unchanged (never allocate a redundant clone).
pub(crate) async fn fold(
    port: Option<&dyn McpPrefetchPort>,
    goal: &str,
    n: u32,
    base_messages: &[CompletionMessage],
) -> Option<Vec<CompletionMessage>> {
    let context = port?.prefetch(goal).await?;
    let mut messages = base_messages.to_vec();
    messages.push(CompletionMessage::user(format!(
        "Shared MCP context gathered once before the {n}-candidate fan-out:\n\n{context}"
    )));
    Some(messages)
}
