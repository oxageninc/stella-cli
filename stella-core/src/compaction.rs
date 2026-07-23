//! Context compaction — pure synchronous logic over owned data
//! Four mechanisms, applied least-lossy first:
//!
//! 1. **Dedup of repeated identical tool outputs** (L-E3): a byte-identical
//!    tool output appearing more than once keeps only its earliest copy; the
//!    later ones are stubbed with a pointer. Keeping the EARLIEST copy is
//!    deliberate: byte-identical content is position-independent, so the
//!    stub lands in the newest part of the conversation and the provider
//!    prompt-cache prefix stays byte-identical (#372). Supersession below is
//!    the opposite — it is about staleness, so it keeps the latest.
//! 2. **Supersession**: when the SAME call (same tool name, byte-identical
//!    input) ran more than once, only the latest result reflects current
//!    state — the older ones are stale by construction (a re-read after an
//!    edit, a re-listed directory) and are stubbed even though their
//!    content differs.
//! 3. **Aging**: still over budget, old large outputs are middle-out
//!    truncated to head+tail before anything is dropped whole — error
//!    lines and file headers survive where full eviction would lose them.
//! 4. **Tool-output eviction**: oldest large tool outputs are replaced with
//!    a stub once the conversation still exceeds the budget. A tool result
//!    whose call is still the most recent one is never evicted (the
//!    property test below: compaction never drops a still-referenced tool
//!    result).
//!
//! The system message and the latest user message are never touched.

use stella_protocol::{CompletionMessage, MessageRole, ToolOutput};

use crate::estimator::{estimate_conversation_tokens, estimate_message_tokens};
use crate::receipts::tool_result_block_id;

/// What a compaction pass did, for the `Compaction` event. Carries both the
/// counts (back-compat) and the **identities** — the `block_id`s each pass
/// stubbed — so the receipt records *which* blocks left context, not just how
/// many (spec §6.2). Each `*_blocks` vec's length equals its count.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompactionReport {
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub evicted: usize,
    pub deduped: usize,
    /// Older results of a repeated identical call, stubbed as stale.
    pub superseded: usize,
    /// Large old outputs middle-out truncated instead of dropped whole.
    pub aged: usize,
    /// `block_id`s evicted (pass 4), deduped (pass 1), superseded (pass 2), and
    /// aged (pass 3) — the content-addressed identity of each stubbed result,
    /// matching the id the manifest cited for it.
    pub evicted_blocks: Vec<String>,
    pub deduped_blocks: Vec<String>,
    pub superseded_blocks: Vec<String>,
    pub aged_blocks: Vec<String>,
}

const EVICTION_STUB: &str =
    "[tool output evicted to fit context — re-run the tool if you need it again]";

/// Aging only touches outputs big enough that head+tail plus the marker is
/// a real saving; below this it would churn bytes for nothing.
const AGE_THRESHOLD_CHARS: usize = 2_000;
/// What aging keeps from each end. Head carries the tool's framing (the
/// PASSED/FAILED line, file headers); tail carries the errors.
const AGE_KEEP_CHARS: usize = 800;

fn dedup_stub() -> String {
    // Models can't see message indices — point at the surviving copy in
    // terms they can act on.
    "[identical output repeated — the full content already appears in an earlier tool result]"
        .to_string()
}

fn supersession_stub() -> String {
    "[stale result of a repeated call — the same tool ran again with identical input; the \
     current output appears in a more recent tool result]"
        .to_string()
}

/// Middle-out truncate `content` on char boundaries, keeping
/// [`AGE_KEEP_CHARS`] from each end. Caller guarantees
/// `content.len() > AGE_THRESHOLD_CHARS`, which the keep windows never
/// overlap.
fn age_content(content: &str) -> String {
    let mut head_end = AGE_KEEP_CHARS.min(content.len());
    while !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = content.len() - AGE_KEEP_CHARS.min(content.len());
    while !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n[… middle elided during compaction — re-run the tool for the full output …]\n{}",
        &content[..head_end],
        &content[tail_start..]
    )
}

/// Evict + dedup until the conversation fits `budget_tokens`, or until
/// nothing more can be safely removed. Returns `None` if no compaction was
/// needed (already under budget) — or if the pass changed nothing (all
/// remaining content is protected), so a permanently-over-budget
/// conversation doesn't emit a no-op `Compaction` event before every step.
pub fn compact(messages: &mut [CompletionMessage], budget_tokens: u64) -> Option<CompactionReport> {
    let before_tokens = estimate_conversation_tokens(messages);
    if before_tokens <= budget_tokens {
        return None;
    }

    let mut deduped = 0usize;
    let mut superseded = 0usize;
    let mut aged = 0usize;
    let mut evicted = 0usize;
    // Identities alongside the counts — the block_id each pass stubbed (§6.2).
    let mut deduped_blocks: Vec<String> = Vec::new();
    let mut superseded_blocks: Vec<String> = Vec::new();
    let mut aged_blocks: Vec<String> = Vec::new();
    let mut evicted_blocks: Vec<String> = Vec::new();
    // Each tool result's ORIGINAL block_id, captured before any pass mutates it,
    // keyed by call_id (unique per result). A result aged then evicted in the
    // same call must be recorded under the id the manifest cited, not the id of
    // its intermediate aged content.
    let original_ids: std::collections::HashMap<String, String> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .flat_map(|m| &m.tool_results)
        .map(|r| (r.call_id.clone(), tool_result_block_id(&r.output)))
        .collect();
    let id_of = |call_id: &str| original_ids.get(call_id).cloned().unwrap_or_default();

    // Index of the last Tool message — its results answer the most recent
    // assistant tool calls and must never be evicted or deduped away.
    let last_tool_idx = messages.iter().rposition(|m| m.role == MessageRole::Tool);

    // Pass 1: dedup byte-identical Ok outputs (keep the EARLIEST copy).
    // Byte-identical content is position-independent, so keeping the first
    // occurrence and stubbing later ones frees the same tokens while leaving
    // the prompt prefix — and the provider prompt cache built over it —
    // untouched (#372). Walk forward, recording first occurrences.
    {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        // First record positions of the earliest occurrence.
        for (idx, message) in messages.iter().enumerate() {
            if message.role != MessageRole::Tool {
                continue;
            }
            for result in &message.tool_results {
                if let ToolOutput::Ok { content } = &result.output
                    && content.len() > 200
                {
                    seen.entry(content.clone()).or_insert(idx);
                }
            }
        }
        // Then stub every later duplicate.
        for (idx, message) in messages.iter_mut().enumerate() {
            if Some(idx) == last_tool_idx || message.role != MessageRole::Tool {
                continue;
            }
            for result in &mut message.tool_results {
                if let ToolOutput::Ok { content } = &result.output
                    && let Some(&kept_at) = seen.get(content)
                    && kept_at < idx
                {
                    deduped_blocks.push(id_of(&result.call_id));
                    result.output = ToolOutput::Ok {
                        content: dedup_stub(),
                    };
                    deduped += 1;
                }
            }
        }
    }

    // Pass 2: supersession — when the SAME invocation (tool name +
    // byte-identical input) produced results more than once, older results
    // are stale by construction: the newer run reflects newer workspace
    // state. Unlike pass 1 this fires even when the CONTENT differs (a
    // re-read after an edit). Keyed through the assistant messages' tool
    // calls because results themselves only carry a call_id.
    {
        use std::collections::HashMap;
        // call_id -> invocation key. Input serialization is deterministic
        // for a given call because it round-trips the same serde_json Value.
        let mut invocation: HashMap<&str, String> = HashMap::new();
        for message in messages.iter() {
            for call in &message.tool_calls {
                invocation.insert(
                    call.call_id.as_str(),
                    format!("{}\u{0}{}", call.name, call.input),
                );
            }
        }
        // Latest tool-message index — and that result's call_id — per
        // invocation key. The call_id lets the staleness check below compare
        // ORIGINAL content identities even after pass 1 stubbed a copy.
        let mut latest: HashMap<&str, (usize, String)> = HashMap::new();
        for (idx, message) in messages.iter().enumerate() {
            if message.role != MessageRole::Tool {
                continue;
            }
            for result in &message.tool_results {
                if let Some(key) = invocation.get(result.call_id.as_str()) {
                    latest.insert(key.as_str(), (idx, result.call_id.clone()));
                }
            }
        }
        let mut stale: Vec<(usize, String)> = Vec::new();
        for (idx, message) in messages.iter().enumerate() {
            if Some(idx) == last_tool_idx || message.role != MessageRole::Tool {
                continue;
            }
            for result in &message.tool_results {
                let Some(key) = invocation.get(result.call_id.as_str()) else {
                    continue;
                };
                // Supersession only restubs Ok results. A superseded error is
                // left to aging/eviction below, which reclaim it by size
                // rather than by staleness — a still-small diagnostic survives
                // whole, only a large one is truncated head+tail.
                let ToolOutput::Ok { content } = &result.output else {
                    continue;
                };
                let Some((latest_idx, latest_call)) = latest.get(key.as_str()) else {
                    continue;
                };
                // A later run whose output was byte-identical is redundancy,
                // not staleness — pass 1 already stubbed the later copy, and
                // stubbing this one too would destroy BOTH copies. Compare
                // ORIGINAL content identities (captured before pass 1 could
                // replace the later copy with a stub).
                if content.len() > 200
                    && *latest_idx > idx
                    && id_of(latest_call) != id_of(&result.call_id)
                {
                    stale.push((idx, result.call_id.clone()));
                }
            }
        }
        for (idx, call_id) in stale {
            for result in &mut messages[idx].tool_results {
                if result.call_id == call_id {
                    superseded_blocks.push(id_of(&result.call_id));
                    result.output = ToolOutput::Ok {
                        content: supersession_stub(),
                    };
                    superseded += 1;
                }
            }
        }
    }

    // Pass 3: aging — before dropping anything whole, shrink old large
    // outputs to head+tail. Oldest first, incremental accounting, stop as
    // soon as the budget fits; what aging saves, eviction never has to
    // destroy.
    let mut current_tokens = estimate_conversation_tokens(messages);
    if current_tokens > budget_tokens {
        for (idx, message) in messages.iter_mut().enumerate() {
            if Some(idx) == last_tool_idx || message.role != MessageRole::Tool {
                continue;
            }
            let before = estimate_message_tokens(message);
            for result in &mut message.tool_results {
                let (payload, is_error) = match &result.output {
                    ToolOutput::Ok { content } => (content, false),
                    ToolOutput::Error { message } => (message, true),
                };
                if payload.len() > AGE_THRESHOLD_CHARS {
                    let aged_payload = age_content(payload);
                    result.output = if is_error {
                        ToolOutput::Error {
                            message: aged_payload,
                        }
                    } else {
                        ToolOutput::Ok {
                            content: aged_payload,
                        }
                    };
                    aged_blocks.push(id_of(&result.call_id));
                    aged += 1;
                }
            }
            let after = estimate_message_tokens(message);
            current_tokens = current_tokens.saturating_sub(before.saturating_sub(after));
            if current_tokens <= budget_tokens {
                break;
            }
        }
    }

    // Pass 4: evict oldest large tool outputs until under budget. The running
    // total is tracked incrementally (diffing one message's estimate before
    // and after mutation) rather than by re-scanning the whole conversation
    // on every eviction — the borrow checker won't allow an immutable
    // whole-slice re-scan while a mutable borrow of one message is live, and
    // an O(n) rescan per eviction would be wasteful besides. (Re-scanned
    // once here so aging's incremental drift can't leak into eviction.)
    current_tokens = estimate_conversation_tokens(messages);
    if current_tokens > budget_tokens {
        for (idx, message) in messages.iter_mut().enumerate() {
            if Some(idx) == last_tool_idx || message.role != MessageRole::Tool {
                continue;
            }
            let before = estimate_message_tokens(message);
            for result in &mut message.tool_results {
                let (payload_len, is_error) = match &result.output {
                    ToolOutput::Ok { content } => (content.len(), false),
                    ToolOutput::Error { message } => (message.len(), true),
                };
                if payload_len > 400 {
                    evicted_blocks.push(id_of(&result.call_id));
                    result.output = if is_error {
                        ToolOutput::Error {
                            message: EVICTION_STUB.to_string(),
                        }
                    } else {
                        ToolOutput::Ok {
                            content: EVICTION_STUB.to_string(),
                        }
                    };
                    evicted += 1;
                }
            }
            let after = estimate_message_tokens(message);
            current_tokens = current_tokens.saturating_sub(before.saturating_sub(after));
            if current_tokens <= budget_tokens {
                break;
            }
        }
    }

    if evicted == 0 && deduped == 0 && superseded == 0 && aged == 0 {
        // Over budget but nothing compactable — don't report a no-op.
        return None;
    }
    let after_tokens = estimate_conversation_tokens(messages);
    Some(CompactionReport {
        before_tokens,
        after_tokens,
        evicted,
        deduped,
        superseded,
        aged,
        evicted_blocks,
        deduped_blocks,
        superseded_blocks,
        aged_blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::{ToolCall, ToolResult};

    fn tool_msg(call_id: &str, content: String) -> CompletionMessage {
        CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: call_id.into(),
                output: ToolOutput::Ok { content },
            }],
            attachments: Vec::new(),
        }
    }

    fn tool_error_msg(call_id: &str, message: String) -> CompletionMessage {
        CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: call_id.into(),
                output: ToolOutput::Error { message },
            }],
            attachments: Vec::new(),
        }
    }

    fn assistant_with_call_on(call_id: &str, path: &str) -> CompletionMessage {
        CompletionMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                call_id: call_id.into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": path }),
            }],
            tool_results: vec![],
            attachments: Vec::new(),
        }
    }

    /// Distinct target per call id, so tests exercising dedup/eviction in
    /// isolation don't also trip the supersession pass (which keys on
    /// identical name+input).
    fn assistant_with_call(call_id: &str) -> CompletionMessage {
        CompletionMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                call_id: call_id.into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": call_id }),
            }],
            tool_results: vec![],
            attachments: Vec::new(),
        }
    }

    #[test]
    fn no_compaction_when_under_budget() {
        let mut messages = vec![
            CompletionMessage::system("sys"),
            CompletionMessage::user("hi"),
        ];
        assert!(compact(&mut messages, 1_000_000).is_none());
    }

    #[test]
    fn evicts_oldest_large_output_first_and_reports() {
        let mut messages = vec![
            CompletionMessage::system("sys"),
            CompletionMessage::user("do things"),
            assistant_with_call("c1"),
            tool_msg("c1", "old ".repeat(2000)),
            assistant_with_call("c2"),
            tool_msg("c2", "new ".repeat(2000)),
        ];
        let report = compact(&mut messages, 2500).expect("compaction should run");
        assert!(report.evicted >= 1);
        assert!(report.after_tokens < report.before_tokens);
        // The OLD output (idx 3) was evicted…
        match &messages[3].tool_results[0].output {
            ToolOutput::Ok { content } => assert!(content.contains("evicted")),
            _ => panic!("expected stub"),
        }
    }

    #[test]
    fn eviction_reports_the_block_identity_the_manifest_cited() {
        // §6.2: the report names WHICH block was evicted, by the same
        // content-addressed id the receipt manifest recorded for it — so a
        // later pass can prove that block was dropped before it was ever used.
        let old_output = ToolOutput::Ok {
            content: "old ".repeat(2000),
        };
        let expected = crate::receipts::tool_result_block_id(&old_output);
        let mut messages = vec![
            CompletionMessage::system("sys"),
            CompletionMessage::user("do things"),
            assistant_with_call("c1"),
            tool_msg("c1", "old ".repeat(2000)),
            assistant_with_call("c2"),
            tool_msg("c2", "new ".repeat(2000)),
        ];
        let report = compact(&mut messages, 2500).expect("compaction should run");
        assert_eq!(
            report.evicted_blocks.len(),
            report.evicted,
            "one identity per evicted block"
        );
        assert!(
            report.evicted_blocks.contains(&expected),
            "evicted_blocks {:?} must name the evicted output's block_id {expected}",
            report.evicted_blocks
        );
    }

    #[test]
    fn never_evicts_the_most_recent_tool_result() {
        // Property: compaction never drops a still-referenced tool result —
        // the result answering the latest assistant call survives even under
        // an impossible budget.
        let latest = "latest ".repeat(2000);
        let mut messages = vec![
            CompletionMessage::system("sys"),
            assistant_with_call("c1"),
            tool_msg("c1", "old ".repeat(2000)),
            assistant_with_call("c2"),
            tool_msg("c2", latest.clone()),
        ];
        compact(&mut messages, 1); // impossible budget
        match &messages[4].tool_results[0].output {
            ToolOutput::Ok { content } => assert_eq!(content, &latest),
            _ => panic!("latest tool result must survive"),
        }
    }

    #[test]
    fn dedups_identical_outputs_keeping_the_earliest() {
        let repeated = "same big output ".repeat(100);
        let mut messages = vec![
            CompletionMessage::system("sys"),
            assistant_with_call("c1"),
            tool_msg("c1", repeated.clone()),
            assistant_with_call("c2"),
            tool_msg("c2", repeated.clone()),
            assistant_with_call("c3"),
            tool_msg("c3", "different".into()),
        ];
        // Budget must be tight enough to force compaction (below the
        // ~1000-token pre-dedup total) but loose enough that the single
        // surviving copy left after dedup (~500 tokens) doesn't ALSO need
        // to be evicted — otherwise this test would be indistinguishable
        // from the eviction test above.
        let report = compact(&mut messages, 700).expect("should compact");
        assert!(report.deduped >= 1);
        // The EARLIEST copy (idx 2) survives byte-identical, so the prompt
        // prefix — and the provider cache built over it — is untouched (#372).
        match &messages[2].tool_results[0].output {
            ToolOutput::Ok { content } => assert_eq!(content, &repeated),
            _ => panic!("earliest copy must be intact"),
        }
        // The later copy (idx 4) is stubbed with a pointer to the earlier one.
        match &messages[4].tool_results[0].output {
            ToolOutput::Ok { content } => {
                assert!(content.contains("earlier tool result"), "got: {content}")
            }
            _ => panic!("expected dedup stub"),
        }
    }

    #[test]
    fn identical_rerun_is_deduped_not_superseded() {
        // The SAME call (name + byte-identical input) run twice returning
        // byte-identical output is redundancy, not staleness: pass 1 stubs
        // the later copy and pass 2 must NOT then stub the surviving earlier
        // copy — that would destroy both (#372 interplay guard).
        let repeated = "identical contents ".repeat(100);
        let mut messages = vec![
            CompletionMessage::system("sys"),
            assistant_with_call_on("c1", "src/lib.rs"),
            tool_msg("c1", repeated.clone()),
            assistant_with_call_on("c2", "src/lib.rs"),
            tool_msg("c2", repeated.clone()),
            assistant_with_call("c3"),
            tool_msg("c3", "different".into()),
        ];
        let report = compact(&mut messages, 700).expect("should compact");
        assert_eq!(report.superseded, 0, "{report:?}");
        assert!(report.deduped >= 1, "{report:?}");
        match &messages[2].tool_results[0].output {
            ToolOutput::Ok { content } => assert_eq!(content, &repeated),
            _ => panic!("earliest copy must survive intact"),
        }
        match &messages[4].tool_results[0].output {
            ToolOutput::Ok { content } => {
                assert!(content.contains("earlier tool result"), "got: {content}")
            }
            _ => panic!("expected a dedup stub on the later copy"),
        }
    }

    #[test]
    fn eviction_is_monotonic_under_shrinking_budgets() {
        // Property: budget eviction monotonic — a smaller budget never
        // yields MORE tokens than a bigger one on the same input.
        let build = || {
            vec![
                CompletionMessage::system("sys"),
                assistant_with_call("c1"),
                tool_msg("c1", "aaaa ".repeat(1000)),
                assistant_with_call("c2"),
                tool_msg("c2", "bbbb ".repeat(1000)),
                assistant_with_call("c3"),
                tool_msg("c3", "cccc ".repeat(1000)),
            ]
        };
        let mut generous = build();
        let mut tight = build();
        compact(&mut generous, 3000);
        compact(&mut tight, 500);
        assert!(
            estimate_conversation_tokens(&tight) <= estimate_conversation_tokens(&generous),
            "tighter budget must not leave more tokens"
        );
    }

    #[test]
    fn repeated_identical_call_supersedes_older_differing_results() {
        // Same tool, same input, run twice with DIFFERENT outputs (a
        // re-read after an edit): the older result is stale by
        // construction and must be stubbed even though byte-dedup can't
        // touch it. A third call on a DIFFERENT target must be untouched.
        let mut messages = vec![
            CompletionMessage::system("sys"),
            assistant_with_call_on("c1", "src/lib.rs"),
            tool_msg("c1", "pre-edit contents ".repeat(100)),
            assistant_with_call_on("c2", "src/other.rs"),
            tool_msg("c2", "unrelated file ".repeat(100)),
            assistant_with_call_on("c3", "src/lib.rs"),
            tool_msg("c3", "post-edit contents ".repeat(100)),
        ];
        // Below the raw total (~1300 tokens) but above what supersession
        // alone leaves (~900), so eviction never has to fire and the
        // untouched-neighbors assertions below stay meaningful.
        let report = compact(&mut messages, 1_100).expect("should compact");
        assert!(report.superseded >= 1, "{report:?}");
        match &messages[2].tool_results[0].output {
            ToolOutput::Ok { content } => {
                assert!(content.contains("stale result"), "got: {content}")
            }
            _ => panic!("expected supersession stub"),
        }
        // The different-target read keeps its full content…
        match &messages[4].tool_results[0].output {
            ToolOutput::Ok { content } => {
                assert!(content.starts_with("unrelated file"), "got: {content}")
            }
            _ => panic!("different invocation must not be superseded"),
        }
        // …and the superseding (latest) result is intact.
        match &messages[6].tool_results[0].output {
            ToolOutput::Ok { content } => {
                assert!(content.starts_with("post-edit"), "got: {content}")
            }
            _ => panic!("latest result must survive"),
        }
    }

    #[test]
    fn aging_shrinks_old_outputs_keeping_head_and_tail_before_eviction() {
        let body = format!("HEADLINE\n{}\nTAILLINE", "filler ".repeat(6000));
        let mut messages = vec![
            CompletionMessage::system("sys"),
            assistant_with_call("c1"),
            tool_msg("c1", body),
            assistant_with_call("c2"),
            tool_msg("c2", "recent ".repeat(50)),
        ];
        // Budget below the raw size but comfortably above the aged size:
        // aging alone must satisfy it, so nothing gets evicted whole.
        let report = compact(&mut messages, 2_000).expect("should compact");
        assert!(report.aged >= 1, "{report:?}");
        assert_eq!(report.evicted, 0, "aging must run before eviction");
        match &messages[2].tool_results[0].output {
            ToolOutput::Ok { content } => {
                assert!(content.starts_with("HEADLINE"), "head lost: {content:.40}");
                assert!(content.ends_with("TAILLINE"), "tail lost");
                assert!(content.contains("middle elided"));
                assert!(content.len() < 2_000, "aged output still huge");
            }
            _ => panic!("expected aged content"),
        }
    }

    #[test]
    fn small_error_output_is_left_intact() {
        // A small error is pure diagnostic and below every size floor: it
        // must survive compaction whole even as large neighbors are reclaimed.
        let mut messages = vec![
            CompletionMessage::system("sys"),
            assistant_with_call("c1"),
            tool_error_msg("c1", "diagnostic that matters".into()),
            assistant_with_call("c2"),
            tool_msg("c2", "filler ".repeat(2000)),
            assistant_with_call("c3"),
            tool_msg("c3", "recent ".repeat(10)),
        ];
        compact(&mut messages, 200);
        match &messages[2].tool_results[0].output {
            ToolOutput::Error { message } => {
                assert_eq!(message, "diagnostic that matters")
            }
            _ => panic!("small error diagnostics must survive compaction"),
        }
    }

    #[test]
    fn aging_shrinks_old_error_outputs_keeping_head_and_tail_before_eviction() {
        // A large error is truncated middle-out like a large Ok output: the
        // head (framing) and tail (the failure lines) survive where whole
        // eviction would lose them.
        let body = format!("HEADLINE\n{}\nTAILLINE", "filler ".repeat(6000));
        let mut messages = vec![
            CompletionMessage::system("sys"),
            assistant_with_call("c1"),
            tool_error_msg("c1", body),
            assistant_with_call("c2"),
            tool_msg("c2", "recent ".repeat(50)),
        ];
        let report = compact(&mut messages, 2_000).expect("should compact");
        assert!(report.aged >= 1, "{report:?}");
        assert_eq!(report.evicted, 0, "aging must run before eviction");
        match &messages[2].tool_results[0].output {
            ToolOutput::Error { message } => {
                assert!(message.starts_with("HEADLINE"), "head lost: {message:.40}");
                assert!(message.ends_with("TAILLINE"), "tail lost");
                assert!(message.contains("middle elided"));
                assert!(message.len() < 2_000, "aged error still huge");
            }
            _ => panic!("expected aged error content"),
        }
    }

    #[test]
    fn large_error_output_is_evicted_like_large_ok() {
        // Between the aging threshold and the eviction size floor, so aging
        // can't touch it and eviction is what reclaims it — mirroring Ok.
        let mut messages = vec![
            CompletionMessage::system("sys"),
            assistant_with_call("c1"),
            tool_error_msg("c1", "boom ".repeat(300)),
            assistant_with_call("c2"),
            tool_msg("c2", "recent ".repeat(50)),
        ];
        let report = compact(&mut messages, 200).expect("should compact");
        assert!(report.evicted >= 1, "{report:?}");
        match &messages[2].tool_results[0].output {
            ToolOutput::Error { message } => assert!(message.contains("evicted")),
            _ => panic!("expected an eviction stub that keeps the error variant"),
        }
    }

    #[test]
    fn red_loop_of_large_errors_is_reclaimable() {
        // The bug: a red loop of repeated ~100 KB failures accumulated context
        // no pure compaction pass could reclaim. Now every large error but the
        // most recent is reclaimable, so the conversation fits budget again.
        let big_err = |n: usize| format!("failure {n}\n{}", "E".repeat(100_000));
        let mut messages = vec![
            CompletionMessage::system("sys"),
            assistant_with_call("c1"),
            tool_error_msg("c1", big_err(1)),
            assistant_with_call("c2"),
            tool_error_msg("c2", big_err(2)),
            assistant_with_call("c3"),
            tool_error_msg("c3", big_err(3)),
            assistant_with_call("c4"),
            tool_error_msg("c4", big_err(4)),
        ];
        let before = estimate_conversation_tokens(&messages);
        let budget = 35_000;
        let report = compact(&mut messages, budget).expect("should compact");
        assert!(
            report.aged >= 3,
            "older failures must be reclaimed: {report:?}"
        );
        let after = estimate_conversation_tokens(&messages);
        assert!(after < before, "compaction must reclaim tokens");
        assert!(
            after <= budget,
            "still over budget after compaction: {after}"
        );
        // The most recent failure — the one the agent is acting on — survives.
        match &messages[8].tool_results[0].output {
            ToolOutput::Error { message } => {
                assert!(
                    message.starts_with("failure 4"),
                    "latest error must survive whole"
                );
                assert!(
                    message.len() > 100_000,
                    "latest error must not be truncated"
                );
            }
            _ => panic!("most recent error must survive intact"),
        }
    }
}
