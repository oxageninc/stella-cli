//! Token estimation for compaction decisions. The estimator returns a
//! deliberately conservative (over-)estimate — over-estimating triggers
//! compaction earlier, the safe direction. Provider-reported usage flows
//! through `AgentEvent::StepUsage` for telemetry; feeding it back to
//! correct estimator drift per provider (`07-model-matrix.md` §4.3) is
//! planned but NOT implemented yet — do not rely on drift correction.

use stella_protocol::CompletionMessage;

/// Chars-per-token divisor. 4 is the classic English-prose heuristic; code
/// and JSON run denser (more tokens per char), so we use 3.5 to bias the
/// estimate high — over-estimating triggers compaction *earlier*, which is
/// the safe direction (silent truncation by the provider is the failure
/// mode this exists to prevent).
const CHARS_PER_TOKEN: f64 = 3.5;

/// Fixed per-message framing overhead (role tags, separators) in tokens.
const PER_MESSAGE_OVERHEAD: u64 = 4;

/// Estimate the token cost of one message, including any tool calls and
/// tool results it carries.
pub fn estimate_message_tokens(message: &CompletionMessage) -> u64 {
    let mut chars = message.content.len();
    for call in &message.tool_calls {
        chars += call.name.len();
        chars += call.input.to_string().len();
    }
    for result in &message.tool_results {
        chars += result.call_id.len();
        chars += match &result.output {
            stella_protocol::ToolOutput::Ok { content } => content.len(),
            stella_protocol::ToolOutput::Error { message } => message.len(),
        };
    }
    (chars as f64 / CHARS_PER_TOKEN).ceil() as u64 + PER_MESSAGE_OVERHEAD
}

/// Estimate the total token cost of a conversation.
pub fn estimate_conversation_tokens(messages: &[CompletionMessage]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::{MessageRole, ToolOutput, ToolResult};

    #[test]
    fn empty_message_costs_only_overhead() {
        let m = CompletionMessage {
            role: MessageRole::User,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![],
        };
        assert_eq!(estimate_message_tokens(&m), PER_MESSAGE_OVERHEAD);
    }

    #[test]
    fn estimate_grows_with_content_length() {
        let short = CompletionMessage::user("hi");
        let long = CompletionMessage::user("a".repeat(3500));
        assert!(estimate_message_tokens(&long) > estimate_message_tokens(&short));
        // 3500 chars / 3.5 = 1000 tokens + overhead
        assert_eq!(estimate_message_tokens(&long), 1000 + PER_MESSAGE_OVERHEAD);
    }

    #[test]
    fn tool_results_count_toward_the_estimate() {
        let bare = CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![],
        };
        let loaded = CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: "c1".into(),
                output: ToolOutput::Ok {
                    content: "x".repeat(7000),
                },
            }],
        };
        assert!(estimate_message_tokens(&loaded) > estimate_message_tokens(&bare) + 1900);
    }

    #[test]
    fn conversation_estimate_is_sum_of_messages() {
        let messages = vec![
            CompletionMessage::system("sys"),
            CompletionMessage::user("hello"),
        ];
        let total = estimate_conversation_tokens(&messages);
        let sum: u64 = messages.iter().map(estimate_message_tokens).sum();
        assert_eq!(total, sum);
    }
}
