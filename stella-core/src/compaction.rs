//! Context compaction — pure synchronous logic over owned data
//! Two mechanisms, applied in order:
//!
//! 1. **Tool-output eviction**: oldest large tool outputs are replaced with
//!    a stub once the conversation exceeds the budget. A tool result whose
//!    call is still the most recent one is never evicted (the property test
//!    below: compaction never drops a still-referenced tool result).
//! 2. **Dedup of repeated identical tool outputs** (L-E3): a byte-identical
//!    tool output appearing more than once keeps only its latest copy; the
//!    older ones are stubbed with a pointer.
//!
//! The system message and the latest user message are never touched.

use stella_protocol::{CompletionMessage, MessageRole, ToolOutput};

use crate::estimator::{estimate_conversation_tokens, estimate_message_tokens};

/// What a compaction pass did, for the `Compaction` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionReport {
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub evicted: usize,
    pub deduped: usize,
}

const EVICTION_STUB: &str =
    "[tool output evicted to fit context — re-run the tool if you need it again]";

fn dedup_stub() -> String {
    // Models can't see message indices — point at the surviving copy in
    // terms they can act on.
    "[identical output repeated — the full content appears again in a more recent tool result]"
        .to_string()
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
    let mut evicted = 0usize;

    // Index of the last Tool message — its results answer the most recent
    // assistant tool calls and must never be evicted or deduped away.
    let last_tool_idx = messages.iter().rposition(|m| m.role == MessageRole::Tool);

    // Pass 1: dedup byte-identical Ok outputs (keep the LATEST copy).
    // Walk from the end, recording seen content; stub earlier duplicates.
    {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        // First record positions of latest occurrence.
        for (idx, message) in messages.iter().enumerate().rev() {
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
        // Then stub every earlier duplicate.
        for (idx, message) in messages.iter_mut().enumerate() {
            if Some(idx) == last_tool_idx || message.role != MessageRole::Tool {
                continue;
            }
            for result in &mut message.tool_results {
                if let ToolOutput::Ok { content } = &result.output
                    && let Some(&kept_at) = seen.get(content)
                    && kept_at > idx
                {
                    result.output = ToolOutput::Ok {
                        content: dedup_stub(),
                    };
                    deduped += 1;
                }
            }
        }
    }

    // Pass 2: evict oldest large tool outputs until under budget. The running
    // total is tracked incrementally (diffing one message's estimate before
    // and after mutation) rather than by re-scanning the whole conversation
    // on every eviction — the borrow checker won't allow an immutable
    // whole-slice re-scan while a mutable borrow of one message is live, and
    // an O(n) rescan per eviction would be wasteful besides.
    let mut current_tokens = estimate_conversation_tokens(messages);
    if current_tokens > budget_tokens {
        for (idx, message) in messages.iter_mut().enumerate() {
            if Some(idx) == last_tool_idx || message.role != MessageRole::Tool {
                continue;
            }
            let before = estimate_message_tokens(message);
            for result in &mut message.tool_results {
                let is_large = match &result.output {
                    ToolOutput::Ok { content } => content.len() > 400,
                    ToolOutput::Error { .. } => false, // errors are small + diagnostic
                };
                if is_large {
                    result.output = ToolOutput::Ok {
                        content: EVICTION_STUB.to_string(),
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

    if evicted == 0 && deduped == 0 {
        // Over budget but nothing compactable — don't report a no-op.
        return None;
    }
    let after_tokens = estimate_conversation_tokens(messages);
    Some(CompactionReport {
        before_tokens,
        after_tokens,
        evicted,
        deduped,
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
        }
    }

    fn assistant_with_call(call_id: &str) -> CompletionMessage {
        CompletionMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                call_id: call_id.into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "x"}),
            }],
            tool_results: vec![],
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
    fn dedups_identical_outputs_keeping_the_latest() {
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
        // Earlier copy (idx 2) stubbed with a pointer to the later one.
        match &messages[2].tool_results[0].output {
            ToolOutput::Ok { content } => {
                assert!(content.contains("repeated"), "got: {content}")
            }
            _ => panic!("expected dedup stub"),
        }
        // Later copy (idx 4) intact.
        match &messages[4].tool_results[0].output {
            ToolOutput::Ok { content } => assert_eq!(content, &repeated),
            _ => panic!("later copy must be intact"),
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
    fn error_outputs_are_never_evicted() {
        let mut messages = vec![
            CompletionMessage::system("sys"),
            assistant_with_call("c1"),
            CompletionMessage {
                role: MessageRole::Tool,
                content: String::new(),
                tool_calls: vec![],
                tool_results: vec![ToolResult {
                    call_id: "c1".into(),
                    output: ToolOutput::Error {
                        message: "diagnostic that matters".into(),
                    },
                }],
            },
            assistant_with_call("c2"),
            tool_msg("c2", "filler ".repeat(2000)),
        ];
        compact(&mut messages, 100);
        match &messages[2].tool_results[0].output {
            ToolOutput::Error { message } => {
                assert_eq!(message, "diagnostic that matters")
            }
            _ => panic!("error diagnostics must survive compaction"),
        }
    }
}
