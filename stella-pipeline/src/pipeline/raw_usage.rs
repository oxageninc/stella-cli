//! Complete per-call accounting for pipeline roles that call providers directly.

use std::time::Duration;

use stella_core::retry::RetryPolicy;
use stella_core::{AccountedCall, AccountedCallError, BudgetGuard, run_accounted_call};
use stella_protocol::{CompletionMessage, CompletionRequest, CompletionResult, ModelCallRole};

use super::stage_budget::{PipelineBudgetAbort, budget_abort};
use super::{Pipeline, ResolvedRole, RoleCallOverrides};

pub(super) struct RawCall<'r, 'a> {
    pub(super) role: ModelCallRole,
    pub(super) resolved: &'r ResolvedRole<'a>,
    pub(super) messages: Vec<CompletionMessage>,
    pub(super) policy: RetryPolicy,
    pub(super) overrides: &'r RoleCallOverrides,
    pub(super) timeout: Option<Duration>,
}

pub(super) enum RawCallError {
    Provider,
    Timeout,
    Budget(PipelineBudgetAbort),
}

impl<'a> Pipeline<'a> {
    /// One metered raw provider completion. Successful calls emit exactly one
    /// `StepUsage` before budget enforcement can return; failures/timeouts emit
    /// one content-free `UsageIncomplete`. All raw roles use this chokepoint.
    pub(super) async fn metered_raw_call(
        &self,
        call: RawCall<'_, 'a>,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> Result<CompletionResult, RawCallError> {
        let messages = match &call.overrides.prompt {
            Some(prompt) => {
                let mut with_system = Vec::with_capacity(call.messages.len() + 1);
                with_system.push(CompletionMessage::system(prompt.clone()));
                with_system.extend(call.messages.clone());
                with_system
            }
            None => call.messages.clone(),
        };
        let engine = &self.config.engine;
        let req = CompletionRequest {
            messages,
            max_output_tokens: call
                .overrides
                .max_output_tokens
                .or(engine.max_output_tokens),
            temperature: call.overrides.temperature.or(engine.temperature),
            effort: call.overrides.effort.or(engine.effort),
            reasoning: call.overrides.reasoning.or(engine.reasoning),
            params: call.overrides.params.or(engine.params),
            tools: Vec::new(),
        };
        match run_accounted_call(
            AccountedCall {
                provider: call.resolved.provider,
                role: call.role,
                model_hint: call.resolved.model_ref.model_id.clone(),
                request: req,
                retry_policy: call.policy,
                timeout: call.timeout,
                estimated_input_tokens: 0,
            },
            budget,
            &self.events,
            self.sleeper,
        )
        .await
        {
            Ok(result) => {
                *total += result.cost_usd;
                Ok(result)
            }
            Err(AccountedCallError::Provider(_)) => Err(RawCallError::Provider),
            Err(AccountedCallError::Timeout) => Err(RawCallError::Timeout),
            Err(AccountedCallError::Budget { result, outcome }) => {
                *total += result.cost_usd;
                Err(RawCallError::Budget(
                    budget_abort(outcome).expect("budget error carries abort outcome"),
                ))
            }
        }
    }
}
