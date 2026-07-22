//! Paid management-call accounting and synchronous engine event forwarding.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use stella_core::estimator::estimate_conversation_tokens;
use stella_core::retry::retry_with_backoff;
use stella_protocol::{CompletionRequest, CompletionResult};

use super::*;

impl<'a> Pipeline<'a> {
    /// One raw provider completion (triage/plan/judge — not the execute engine)
    /// under `policy`, emitting a `Retry` event per committed retry (L-E10).
    /// `overrides` refines the engine config per role — pass
    /// `&RoleCallOverrides::default()` for calls that ride the worker's
    /// settings (plan, repair). A custom role prompt is prepended as a
    /// system message here, one place, so every call site composes it the
    /// same way.
    pub(super) async fn complete_once(
        &self,
        provider: &dyn Provider,
        purpose: &str,
        messages: Vec<CompletionMessage>,
        policy: RetryPolicy,
        overrides: &RoleCallOverrides,
    ) -> Result<CompletionResult, stella_protocol::ProviderError> {
        let messages = match &overrides.prompt {
            Some(prompt) => {
                let mut with_system = Vec::with_capacity(messages.len() + 1);
                with_system.push(CompletionMessage::system(prompt.clone()));
                with_system.extend(messages);
                with_system
            }
            None => messages,
        };
        let engine = &self.config.engine;
        let req = CompletionRequest {
            messages,
            max_output_tokens: overrides.max_output_tokens.or(engine.max_output_tokens),
            temperature: overrides.temperature.or(engine.temperature),
            effort: overrides.effort.or(engine.effort),
            reasoning: overrides.reasoning.or(engine.reasoning),
            params: overrides.params.or(engine.params),
            tools: Vec::new(),
        };
        let estimated_input_tokens = estimate_conversation_tokens(&req.messages);
        let call_started = std::time::Instant::now();
        let outcome =
            retry_with_backoff(&policy, self.sleeper, || provider.complete(req.clone())).await?;
        let duration_ms = call_started.elapsed().as_millis() as u64;
        for attempt in &outcome.retries {
            self.emit(AgentEvent::Retry {
                attempt: attempt.attempt,
                reason: attempt.reason.clone(),
            });
        }
        let result = outcome.value;
        self.emit(AgentEvent::StepUsage {
            // Pipeline-owned role calls are single-completion stages rather
            // than tool-loop steps. Stage events immediately preceding this
            // record retain the purpose; zero is the stage-local step index.
            step: 0,
            purpose: Some(purpose.to_string()),
            output_text: Some(result.text.clone()),
            model: result.model.clone(),
            input_tokens: result.usage.input_tokens,
            output_tokens: result.usage.output_tokens,
            cached_input_tokens: result.usage.cached_input_tokens,
            cache_write_tokens: result.usage.cache_write_tokens,
            estimated_input_tokens,
            cost_usd: result.cost_usd,
            duration_ms,
            retries: outcome.retries.len() as u32,
            tool_calls: result.tool_calls.len(),
        });
        Ok(result)
    }

    /// Run one engine turn, forwarding every event to the consumer **live**
    /// except the engine's `Stage`/`Complete` (the pipeline owns those), and
    /// tallying mutation `FileChange`s for the zero-diff guard. The filtered
    /// sender is synchronous: when the outer sender has a durability boundary,
    /// a paid StepUsage cannot return to the engine before append+flush.
    pub(super) async fn run_engine_turn(
        &self,
        engine: &Engine<'_>,
        messages: &mut Vec<CompletionMessage>,
        budget: &mut BudgetGuard,
        file_changes: &mut u32,
    ) -> TurnOutcome {
        let seen_file_changes = Arc::new(AtomicU32::new(0));
        let count = seen_file_changes.clone();
        let consumer = self.events.clone();
        let filtered = EventSender::from_fn(move |event| {
            match &event {
                // The pipeline is the sole authority for stage boundaries and
                // the terminal Complete — drop the engine's per-turn copies.
                AgentEvent::Stage { .. } | AgentEvent::Complete { .. } => Ok(()),
                AgentEvent::FileChange { kind, .. } => {
                    // Reads ride the same event for the files panel but are not
                    // mutations and must not defeat the zero-diff guard.
                    if kind.is_mutation() {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    consumer.send(event)
                }
                _ => consumer.send(event),
            }
        });
        let outcome = engine
            .run_turn_with_sender(messages, budget, &filtered)
            .await;
        *file_changes += seen_file_changes.load(Ordering::Relaxed);
        outcome
    }
}
