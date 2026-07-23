//! I/O-free one-shot provider accounting shared by non-engine callers.

use std::time::{Duration, Instant};

use stella_protocol::{
    AgentEvent, CompletionRequest, CompletionResult, ModelCallRole, Provider, ProviderError,
    UsageIncompleteReason,
};
use tokio::time::timeout;

use crate::budget::{BudgetGuard, BudgetOutcome};
use crate::event_sender::EventSender;
use crate::retry::{RetryPolicy, Sleeper, retry_with_backoff_observed};

pub struct AccountedCall<'a> {
    pub provider: &'a dyn Provider,
    pub role: ModelCallRole,
    pub model_hint: String,
    pub request: CompletionRequest,
    pub retry_policy: RetryPolicy,
    pub timeout: Option<Duration>,
    pub estimated_input_tokens: u64,
}

pub enum AccountedCallError {
    Provider(ProviderError),
    Timeout,
    Budget {
        result: CompletionResult,
        outcome: BudgetOutcome,
    },
}

pub async fn run_accounted_call(
    call: AccountedCall<'_>,
    budget: &mut BudgetGuard,
    events: &EventSender,
    sleeper: &dyn Sleeper,
) -> Result<CompletionResult, AccountedCallError> {
    let started = Instant::now();
    let future = retry_with_backoff_observed(
        &call.retry_policy,
        sleeper,
        || call.provider.complete(call.request.clone()),
        // Per-attempt duration (retry.rs times each dispatch individually):
        // the failed call's own latency, never cumulative across attempts.
        |attempt, _error, attempt_duration| {
            emit_incomplete(
                &call,
                events,
                attempt_duration,
                Some(attempt.saturating_sub(1)),
            );
        },
    );
    let outcome = match call.timeout {
        Some(limit) => match timeout(limit, future).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => {
                return Err(AccountedCallError::Provider(error));
            }
            Err(_) => {
                emit_incomplete(&call, events, started.elapsed(), None);
                return Err(AccountedCallError::Timeout);
            }
        },
        None => match future.await {
            Ok(outcome) => outcome,
            Err(error) => return Err(AccountedCallError::Provider(error)),
        },
    };
    for attempt in &outcome.retries {
        let _ = events.send(AgentEvent::Retry {
            attempt: attempt.attempt,
            reason: attempt.reason.clone(),
        });
    }
    let result = outcome.value;
    let provider = call.provider.id();
    let _ = events.send(AgentEvent::StepUsage {
        step: 0,
        role: call.role,
        provider: provider.to_string(),
        // Every role routed through here is a management or compaction call —
        // none emit a separate `Text` event, so this is the only durable record
        // of what the model actually said (the bench harness's ATIF audit trail
        // reads it). Execute calls take the engine path and leave this `None`.
        output_text: Some(result.text.clone()),
        model: result.model.clone(),
        input_tokens: result.usage.input_tokens,
        output_tokens: result.usage.output_tokens,
        cached_input_tokens: result.usage.cached_input_tokens,
        cache_write_tokens: result.usage.cache_write_tokens,
        estimated_input_tokens: call.estimated_input_tokens,
        cost_usd: result.cost_usd,
        duration_ms: started.elapsed().as_millis() as u64,
        retries: outcome.retries.len() as u32,
        tool_calls: result.tool_calls.len(),
        complete: result.usage.is_complete(),
    });
    let budget_outcome = budget.record_spend(result.cost_usd);
    let _ = events.send(AgentEvent::BudgetTick {
        spent_usd: budget.spent_usd(),
        limit_usd: budget.turn_limit_usd(),
        mode: budget.mode(),
    });
    if let BudgetOutcome::Warn {
        spent_usd,
        limit_usd,
        ..
    } = budget_outcome
    {
        let _ = events.send(AgentEvent::Error {
            message: format!(
                "budget warning: spent ${spent_usd:.4} against a ${limit_usd:.2} observed limit; continuing"
            ),
            retryable: true,
        });
    }
    if matches!(budget_outcome, BudgetOutcome::AbortTurn { .. }) {
        return Err(AccountedCallError::Budget {
            result,
            outcome: budget_outcome,
        });
    }
    Ok(result)
}

fn emit_incomplete(
    call: &AccountedCall<'_>,
    events: &EventSender,
    duration: Duration,
    retries: Option<u32>,
) {
    let _ = events.send(AgentEvent::UsageIncomplete {
        role: call.role,
        provider: call.provider.id().to_string(),
        model: call.model_hint.clone(),
        reason: if retries.is_some() {
            UsageIncompleteReason::ProviderError
        } else {
            UsageIncompleteReason::Timeout
        },
        duration_ms: duration.as_millis() as u64,
        retries,
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use stella_protocol::{BudgetMode, CompletionMessage, CompletionUsage};

    use super::*;

    struct NoopSleeper;

    #[async_trait]
    impl Sleeper for NoopSleeper {
        async fn sleep(&self, _duration_ms: u64) {}
    }

    struct RetryThenSuccess {
        attempts: Mutex<u32>,
    }

    #[async_trait]
    impl Provider for RetryThenSuccess {
        fn id(&self) -> &str {
            "scripted"
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResult, ProviderError> {
            let mut attempts = self.attempts.lock().expect("attempt lock");
            *attempts += 1;
            if *attempts == 1 {
                return Err(ProviderError::Transport("private failed body".into()));
            }
            Ok(CompletionResult {
                text: "done".into(),
                tool_calls: Vec::new(),
                usage: CompletionUsage::reported_zero(),
                model: "scripted-model".into(),
                cost_usd: 0.25,
                finish_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn successful_retry_preserves_failed_attempt_incompleteness_and_known_cost() {
        let provider = RetryThenSuccess {
            attempts: Mutex::new(0),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
        let result = match run_accounted_call(
            AccountedCall {
                provider: &provider,
                role: ModelCallRole::SkillAuthor,
                model_hint: "configured-model".into(),
                request: CompletionRequest {
                    messages: vec![CompletionMessage::user("work")],
                    max_output_tokens: None,
                    temperature: None,
                    effort: None,
                    tools: Vec::new(),
                    reasoning: None,
                    params: None,
                },
                retry_policy: RetryPolicy::new(1, 0, 0),
                timeout: None,
                estimated_input_tokens: 1,
            },
            &mut budget,
            &EventSender::new(tx),
            &NoopSleeper,
        )
        .await
        {
            Ok(result) => result,
            Err(_) => panic!("retry should succeed"),
        };

        assert_eq!(result.cost_usd, 0.25);
        assert_eq!(budget.spent_usd(), 0.25);
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let incomplete: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::UsageIncomplete { .. }))
            .collect();
        assert_eq!(incomplete.len(), 1);
        assert!(matches!(
            incomplete[0],
            AgentEvent::UsageIncomplete {
                role: ModelCallRole::SkillAuthor,
                retries: Some(0),
                ..
            }
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::StepUsage {
                role: ModelCallRole::SkillAuthor,
                cost_usd,
                retries: 1,
                complete: true,
                ..
            } if (*cost_usd - 0.25).abs() < f64::EPSILON
        )));
        assert!(
            !serde_json::to_string(&incomplete)
                .expect("wire")
                .contains("private failed body")
        );
    }
}
