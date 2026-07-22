//! Durable adapter for paid one-shot calls that are not part of an engine turn.

use std::path::Path;
use std::time::Duration;

use stella_core::{
    AccountedCall, AccountedCallError, BudgetGuard, RetryPolicy, run_accounted_call,
};
use stella_protocol::{AgentEvent, CompletionRequest, CompletionResult, ModelCallRole, Provider};
use stella_store::Store;
use tokio::sync::mpsc;

use crate::agent;
use crate::runtime::TokioSleeper;

#[derive(Debug)]
pub(crate) struct StandaloneCompletion {
    pub(crate) result: CompletionResult,
    pub(crate) cost_usd: f64,
    pub(crate) events: Vec<AgentEvent>,
}

#[derive(Debug)]
pub(crate) struct StandaloneCallError {
    pub(crate) message: String,
    pub(crate) cost_usd: f64,
    pub(crate) events: Vec<AgentEvent>,
}

pub(crate) async fn complete_standalone(
    workspace_root: &Path,
    provider: &dyn Provider,
    role: ModelCallRole,
    kind: &str,
    model_hint: &str,
    budget_limit: Option<f64>,
    request: CompletionRequest,
) -> Result<StandaloneCompletion, StandaloneCallError> {
    let store = Store::open(workspace_root).map_err(|error| StandaloneCallError {
        message: format!("accounting store unavailable before model dispatch: {error}"),
        cost_usd: 0.0,
        events: Vec::new(),
    })?;
    let execution_id = store
        .begin_execution(
            kind,
            "content-free system operation",
            provider.id(),
            model_hint,
        )
        .map_err(|error| StandaloneCallError {
            message: format!("accounting execution unavailable before model dispatch: {error}"),
            cost_usd: 0.0,
            events: Vec::new(),
        })?;
    let mut budget: BudgetGuard = agent::build_budget_guard(budget_limit);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = run_accounted_call(
        AccountedCall {
            provider,
            role,
            model_hint: model_hint.to_string(),
            request,
            retry_policy: RetryPolicy::deterministic(),
            timeout: Some(Duration::from_secs(120)),
            estimated_input_tokens: 0,
        },
        &mut budget,
        &tx,
        &TokioSleeper,
    )
    .await;
    drop(tx);
    let mut persistence_complete = true;
    let mut seq = 0;
    let mut settled_events = Vec::new();
    while let Some(event) = rx.recv().await {
        persistence_complete &=
            agent::persist_event(&store, execution_id, seq, &event, provider.id());
        settled_events.push(event);
        seq += 1;
    }
    match outcome {
        Ok(result) => {
            let cost_usd = result.cost_usd;
            let complete = persistence_complete
                && store
                    .finish_execution_accounted(
                        execution_id,
                        "completed",
                        cost_usd,
                        persistence_complete,
                    )
                    .is_ok();
            if !complete {
                return Err(StandaloneCallError {
                    message: "model call settled but its accounting closeout failed".into(),
                    cost_usd,
                    events: settled_events,
                });
            }
            Ok(StandaloneCompletion {
                result,
                cost_usd,
                events: settled_events,
            })
        }
        Err(AccountedCallError::Budget { result, .. }) => {
            let cost_usd = result.cost_usd;
            let _ = store.finish_execution_accounted(
                execution_id,
                "aborted",
                cost_usd,
                persistence_complete,
            );
            Err(StandaloneCallError {
                message: "model call settled over the configured budget".into(),
                cost_usd,
                events: settled_events,
            })
        }
        Err(AccountedCallError::Provider(error)) => {
            let _ = store.finish_execution_accounted(execution_id, "failed", 0.0, false);
            Err(StandaloneCallError {
                message: error.to_string(),
                cost_usd: 0.0,
                events: settled_events,
            })
        }
        Err(AccountedCallError::Timeout) => {
            let _ = store.finish_execution_accounted(execution_id, "failed", 0.0, false);
            Err(StandaloneCallError {
                message: "model call timed out".into(),
                cost_usd: 0.0,
                events: settled_events,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use stella_protocol::{CompletionUsage, ProviderError};

    use super::*;

    struct PaidProvider;

    #[async_trait]
    impl Provider for PaidProvider {
        fn id(&self) -> &str {
            "paid-test"
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResult, ProviderError> {
            Ok(CompletionResult {
                text: "[]".into(),
                tool_calls: Vec::new(),
                usage: CompletionUsage {
                    reported: true,
                    input_tokens: 10,
                    output_tokens: 2,
                    ..CompletionUsage::default()
                },
                model: "paid-model".into(),
                cost_usd: 0.0125,
                finish_reason: None,
            })
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            messages: Vec::new(),
            max_output_tokens: Some(32),
            temperature: None,
            effort: None,
            tools: Vec::new(),
            reasoning: None,
            params: None,
        }
    }

    #[tokio::test]
    async fn all_four_standalone_paid_call_sites_return_and_persist_exact_cost() {
        let root = tempfile::tempdir().expect("root");
        for (role, kind) in [
            (ModelCallRole::AgentAuthor, "agent_author"),
            (ModelCallRole::SkillAuthor, "skill_author"),
            (ModelCallRole::DomainInference, "domain_inference"),
            (ModelCallRole::Reflection, "reflection"),
        ] {
            let outcome = complete_standalone(
                root.path(),
                &PaidProvider,
                role,
                kind,
                "paid-model",
                None,
                request(),
            )
            .await
            .expect("accounted call");
            assert_eq!(outcome.cost_usd, 0.0125);
            assert!(outcome.events.iter().any(|event| matches!(
                event,
                AgentEvent::StepUsage { role: actual, .. } if actual == &role
            )));
        }

        let store = Store::open(root.path()).expect("store");
        assert_eq!(store.count("telemetry").expect("telemetry count"), 4);
        let json = store
            .export_all_json()
            .expect("export")
            .into_iter()
            .find_map(|(table, json)| (table == "telemetry").then_some(json))
            .expect("telemetry");
        for role in [
            "agent_author",
            "skill_author",
            "domain_inference",
            "reflection",
        ] {
            assert!(json.contains(role), "missing persisted role {role}: {json}");
        }
    }

    #[tokio::test]
    async fn over_limit_call_persists_exact_cost_before_model_output_can_apply() {
        let root = tempfile::tempdir().expect("root");
        let error = complete_standalone(
            root.path(),
            &PaidProvider,
            ModelCallRole::SkillAuthor,
            "skill_author",
            "paid-model",
            Some(0.001),
            request(),
        )
        .await
        .expect_err("settled call exceeds guard");
        assert_eq!(error.cost_usd, 0.0125);
        let store = Store::open(root.path()).expect("store");
        let rollup = store
            .execution_rollup(1, root.path())
            .expect("rollup")
            .expect("execution");
        assert_eq!(rollup.cost_usd, 0.0125);
        assert_eq!(rollup.outcome, "aborted");
    }
}
