//! Reflection gating, accounted provider dispatch, and response parsing.

use std::path::Path;

use stella_model::provider::Provider;
use stella_protocol::{AgentEvent, CompletionMessage, CompletionRequest, MessageRole};

use super::ReflectionLesson;

#[derive(Debug, Clone, Default)]
#[must_use = "reflection cost and usage events must be surfaced by every caller"]
pub struct ReflectionReport {
    pub recorded: usize,
    pub model_error: Option<String>,
    pub cost_usd: f64,
    pub events: Vec<AgentEvent>,
}

pub fn turn_warrants_reflection(turn_messages: &[CompletionMessage]) -> bool {
    turn_messages
        .iter()
        .any(|message| !message.tool_calls.is_empty())
}

/// Whether a finished interactive turn should feed reflection at all.
/// Failures ARE reflected on — a failed turn is a high-value learning
/// signal, and the one-shot pipeline path has always treated it as one —
/// EXCEPT a user-chosen soft stop, which is not a failure: reflecting on
/// it would teach the memory that deliberate interruptions are errors.
/// (`contains`, not `==`: goal paths wrap the turn's abort reason in their
/// own prefix.) Callers still pass `result.is_ok()` as
/// `reflect_and_record`'s `succeeded` flag so a failure is recorded AS a
/// failure.
pub fn should_reflect_on(result: &Result<(), String>) -> bool {
    match result {
        Ok(()) => true,
        Err(reason) => !reason.contains(stella_core::SOFT_STOP_REASON),
    }
}

pub async fn reflect_on_turn(
    provider: &dyn Provider,
    model_hint: &str,
    workspace_root: &Path,
    transcript: &[CompletionMessage],
    domain_names: &[String],
    succeeded: bool,
    budget_limit: Option<f64>,
) -> Result<(Vec<ReflectionLesson>, f64, Vec<AgentEvent>), crate::accounted_call::StandaloneCallError>
{
    let digest = transcript
        .iter()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|message| {
            let role = match message.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };
            let content: String = message.content.chars().take(300).collect();
            let tools = if message.tool_calls.is_empty() {
                String::new()
            } else {
                format!(
                    " [called: {}]",
                    message
                        .tool_calls
                        .iter()
                        .map(|call| call.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            format!("{role}: {content}{tools}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let task_frame = if succeeded {
        "This turn SUCCEEDED. Identify durable lessons worth remembering for \
         FUTURE similar tasks: approaches or conventions that worked well, \
         repo conventions discovered, user style preferences revealed. Most \
         successful turns have nothing worth recording — an empty list is the \
         common, correct answer."
    } else {
        "This turn FAILED. Identify the root cause and a durable lesson: what \
         was the most likely cause of failure — a wrong assumption, a missed \
         file, a bad approach, a misunderstood requirement? What should change \
         next time to avoid repeating this failure? Focus on actionable, \
         forward-looking lessons. If the failure was clearly external (network, \
         timeout), return an empty list."
    };
    let prompt = format!(
        "Review this coding-agent turn transcript and reflect on the agent's \
         performance. {task_frame}\n\n\
         Respond with ONLY a JSON array (max 3):\n\
         [{{\"lesson\": \"...\", \"domains\": [\"...\"]}}]\n\
         Allowed domain tags (use only these, or []): {}\n\nTranscript:\n{digest}",
        domain_names.join(", ")
    );
    let request = CompletionRequest {
        messages: vec![
            CompletionMessage::system(
                "You are a self-reflection module. Respond with only a JSON array.",
            ),
            CompletionMessage::user(prompt),
        ],
        max_output_tokens: Some(512),
        temperature: Some(0.0),
        effort: None,
        tools: Vec::new(),
        reasoning: None,
        params: None,
    };
    let accounted = crate::accounted_call::complete_standalone(
        workspace_root,
        provider,
        stella_protocol::ModelCallRole::Reflection,
        "reflection",
        model_hint,
        budget_limit,
        request,
    )
    .await?;
    Ok((
        parse_lessons(&accounted.result.text, domain_names),
        accounted.cost_usd,
        accounted.events,
    ))
}

pub fn parse_lessons(text: &str, allowed_domains: &[String]) -> Vec<ReflectionLesson> {
    let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) else {
        return Vec::new();
    };
    if end <= start {
        return Vec::new();
    }
    let Ok(mut lessons) = serde_json::from_str::<Vec<ReflectionLesson>>(&text[start..=end]) else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    lessons.truncate(3);
    for lesson in &mut lessons {
        lesson.occurred_at = now;
        lesson.domains.retain(|domain| {
            allowed_domains
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(domain))
        });
    }
    lessons.retain(|lesson| !lesson.lesson.trim().is_empty());
    lessons
}
