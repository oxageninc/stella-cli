//! Provider-call retry, speculative overlap, and paid-usage durability.

use std::future::Future;
use std::pin::Pin;

use stella_protocol::ProviderError;

use super::*;
use crate::retry::{RetryOutcome, retry_with_backoff};

/// The boxed-future shape `retry_with_backoff` needs from its `attempt_fn`
/// — named here purely to keep the call site in `run_turn` readable. Each
/// attempt yields the completion and the still-draining speculative-pump
/// future from that same attempt. Keeping the pump separate is load-bearing:
/// paid usage is durably emitted before awaiting it.
type CompletionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompletionResultAlias, ProviderError>> + 'a>>;
type SpeculationFuture<'a> = Pin<Box<dyn Future<Output = SpeculationPool> + 'a>>;
type RetryAttemptFn<'a> = Box<
    dyn FnMut() -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            (CompletionResultAlias, SpeculationFuture<'a>),
                            ProviderError,
                        >,
                    > + 'a,
            >,
        > + 'a,
>;

impl<'a> Engine<'a> {
    /// One model call with retry+backoff (`crate::retry`). On commit,
    /// flushes the step's deferred `Retry` events (module docs, L-E10) and
    /// returns the result bundled with the request-time snapshots the
    /// later phases consume; on exhausted retries, emits the terminal
    /// error and returns the turn's clean abort.
    ///
    /// The estimate captured here is the raw (uncalibrated) estimate of
    /// exactly what this step sends — recorded against the provider's
    /// reported usage by [`Engine::handle_committed_result`]. Raw, not
    /// calibrated: the drift ratio is actual/raw, and recording a
    /// corrected estimate would compound corrections on every feedback
    /// pass.
    pub(super) async fn run_model_call(
        &self,
        step: usize,
        messages: &[CompletionMessage],
        budget: &mut BudgetGuard,
        events: &EventSender,
    ) -> Result<CommittedStep, String> {
        let tools_schema = self.tools.schemas();
        let read_only_tools: HashSet<String> = tools_schema
            .iter()
            .filter(|s| s.read_only)
            .map(|s| s.name.clone())
            .collect();
        let estimated_input_tokens = estimate_conversation_tokens(messages);
        let messages_snapshot = messages.to_vec();
        let req_config = &self.config;
        let speculation_read_only = read_only_tools.clone();
        // The gate forwards answer-text fragments straight onto the turn's
        // event stream as `TextDelta` previews. Deliberately NOT rolled back
        // on a failed attempt: a retry's deltas re-stream from the start
        // with no reset marker — the eventual `Text` event is authoritative
        // and consumers replace the preview with it (protocol docs).
        let delta_events = events.clone();
        // Each attempt polls the provider call and speculation pump
        // concurrently: the pump executes read-only calls the moment the
        // adapter announces them (`crate::speculation`), so their wall-clock
        // overlaps the stream instead of following it. Crucially, a
        // successful attempt returns as soon as the PROVIDER resolves, along
        // with the still-running pump future. That lets the paid-call usage
        // cross the caller's synchronous journal boundary before a slow or
        // hung speculative tool can delay/cancel the rest of the turn. The
        // gate (and its channel sender) drops with the provider future, so the
        // returned pump only drains calls already announced. A failed attempt
        // drops its unfinished read-only work and the retry starts fresh.
        let attempt: RetryAttemptFn = Box::new(move || {
            let req = CompletionRequest {
                messages: messages_snapshot.clone(),
                max_output_tokens: req_config.max_output_tokens,
                temperature: req_config.temperature,
                effort: req_config.effort,
                reasoning: req_config.reasoning,
                params: req_config.params,
                tools: tools_schema.clone(),
            };
            let read_only = speculation_read_only.clone();
            let delta_tx = delta_events.clone();
            Box::pin(async move {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let pump: SpeculationFuture<'_> = Box::pin(self.pump_speculations(rx));
                let complete: CompletionFuture<'_> = Box::pin(async move {
                    let gate = SpeculationGate::new(read_only, tx, delta_tx);
                    self.provider.complete_observed(req, &gate).await
                    // `gate` (and its sender) drop here → the pump's
                    // stream ends once in-flight executions drain.
                });

                match futures_util::future::select(complete, pump).await {
                    futures_util::future::Either::Left((result, pending_pump)) => {
                        result.map(|result| (result, pending_pump))
                    }
                    futures_util::future::Either::Right((pool, pending_complete)) => {
                        let result = pending_complete.await?;
                        let completed_pump: SpeculationFuture<'_> = Box::pin(async move { pool });
                        Ok((result, completed_pump))
                    }
                }
            })
        });

        let call_started = std::time::Instant::now();
        let outcome = retry_with_backoff(&self.config.retry_policy, self.sleeper, attempt).await;
        // Stop the provider duration clock at provider completion. The
        // speculative pump may still be draining below and is deliberately
        // excluded from model-call wall time.
        let call_duration_ms = call_started.elapsed().as_millis() as u64;

        let RetryOutcome {
            value: (result, pending_speculation),
            retries,
            ..
        } = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let message = error.to_string();
                let _ = events.send(AgentEvent::Error {
                    message: message.clone(),
                    retryable: error.is_retryable(),
                });
                return Err(format!("model call failed: {message}"));
            }
        };

        // Deferred-flush: these `Retry` events only reach the wire now
        // that the step has actually committed (see module docs).
        for attempt in &retries {
            let _ = events.send(AgentEvent::Retry {
                attempt: attempt.attempt,
                reason: attempt.reason.clone(),
            });
        }

        // This is the paid-call durability boundary. No await may appear
        // between successful provider resolution above and this send: a
        // benchmark EventSender appends+flushes synchronously here. If the
        // outer timeout cancels while a speculative tool hangs below, the
        // completed provider call remains recoverable with exact usage/cost.
        let _ = events.send(AgentEvent::StepUsage {
            step,
            purpose: Some("execute".to_string()),
            output_text: None,
            model: result.model.clone(),
            input_tokens: result.usage.input_tokens,
            output_tokens: result.usage.output_tokens,
            cached_input_tokens: result.usage.cached_input_tokens,
            cache_write_tokens: result.usage.cache_write_tokens,
            estimated_input_tokens,
            cost_usd: result.cost_usd,
            duration_ms: call_duration_ms,
            retries: retries.len() as u32,
            tool_calls: result.tool_calls.len(),
        });

        // Settle the same landed call before the first post-provider await.
        // This is separate from StepUsage because the budget guard is the
        // execution control-plane ledger, while EventSender is the durable
        // evidence boundary. Both must survive cancellation while a
        // speculative read-only tool is still draining.
        let budget_outcome = budget.record_spend(result.cost_usd);
        let _ = events.send(AgentEvent::BudgetTick {
            spent_usd: budget.spent_usd(),
            limit_usd: budget.turn_limit_usd(),
            mode: budget.mode(),
        });

        // Preserve dispatch's speculation-harvest semantics, but only after
        // the completed paid call is journaled. Dropping this turn future now
        // cancels read-only speculative work without losing metering.
        let speculation = pending_speculation.await;

        Ok(CommittedStep {
            result,
            budget_outcome,
            read_only_tools,
            speculation,
            estimated_input_tokens,
        })
    }
}
