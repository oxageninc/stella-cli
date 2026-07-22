//! Management-call deadline and budget-accounting witnesses.

use super::*;

struct DelayedProvider {
    result: TokioMutex<Option<CompletionResult>>,
    delay: Duration,
}

#[async_trait]
impl Provider for DelayedProvider {
    fn id(&self) -> &str {
        "delayed"
    }

    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        tokio::time::sleep(self.delay).await;
        self.result
            .lock()
            .await
            .take()
            .ok_or_else(|| ProviderError::Terminal("delayed provider exhausted".into()))
    }
}

struct AnyProvider<'p>(&'p dyn Provider);

impl ProviderResolver for AnyProvider<'_> {
    fn provider_for(&self, _model: &ModelRef) -> Option<&dyn Provider> {
        Some(self.0)
    }
}

/// A triage call that misses its decision deadline must not silently vanish
/// from the accounting record. The pipeline abandons the answer and falls back
/// to the deterministic floor, but emits an explicit `UsageIncomplete` so the
/// unknowable envelope is visible rather than guessed at (fail closed).
#[tokio::test]
async fn late_triage_is_abandoned_and_reported_incomplete() {
    let mut result = text_result("multi");
    result.cost_usd = 0.05;
    result.usage.input_tokens = 123;
    result.usage.output_tokens = 7;
    let provider = DelayedProvider {
        result: TokioMutex::new(Some(result)),
        delay: Duration::from_millis(20),
    };
    let resolver = AnyProvider(&provider);
    let runner = ScriptedRunner::new(vec![], "");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let pipeline = Pipeline::new(
        PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall: &recall,
            repo: &repo,
            repo_status: &repo_status,
            diagnostics: &runner,
            tests: &runner,
            approvals: &approvals,
            sleeper: &sleeper,
            hooks: None,
            candidate_workspaces: None,
            mcp_prefetch: None,
            steering: None,
        },
        tx,
        PipelineConfig {
            triage_latency_ceiling: Duration::from_millis(1),
            ..PipelineConfig::default()
        },
    );
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, Some(1.0), None);
    let mut total = 0.0;

    let class = pipeline
        .triage("What is two plus two?", &mut budget, &mut total)
        .await
        .expect("a missed triage deadline is never a run-ending failure");

    // The abandoned answer never classifies, so triage lands on the
    // deterministic floor rather than a guess from a call it did not await.
    assert_eq!(class, resolve_task_class(None, "What is two plus two?"));
    // Nothing settled, so nothing is charged — an unknowable envelope is
    // reported as incomplete instead of being invented.
    assert_eq!(total, 0.0);
    assert_eq!(budget.spent_usd(), 0.0);
    let events = drain(&mut rx);
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::UsageIncomplete {
                role: ModelCallRole::Triage,
                reason: stella_protocol::UsageIncompleteReason::Timeout,
                ..
            }
        )),
        "the missed deadline must surface as an explicit incomplete-usage record"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::StepUsage { .. })),
        "an abandoned call must not emit a settled metering record"
    );
}

/// A management call that crosses an enforced turn budget is the last paid
/// call. Its usage is retained, then the pipeline aborts before planning or
/// worker execution.
#[tokio::test]
async fn triage_budget_crossing_aborts_before_a_second_provider_call() {
    let mut triage = text_result("single");
    triage.cost_usd = 0.20;
    let provider = ScriptedProvider::new(vec![triage, text_result("must not run")]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![], "");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let (tx, mut rx) = mpsc::unbounded_channel();

    let pipeline = Pipeline::new(
        PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall: &recall,
            repo: &repo,
            repo_status: &repo_status,
            diagnostics: &runner,
            tests: &runner,
            approvals: &approvals,
            sleeper: &sleeper,
            hooks: None,
            candidate_workspaces: None,
            mcp_prefetch: None,
            steering: None,
        },
        tx,
        PipelineConfig::default(),
    );

    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, Some(0.17), None);
    let outcome = pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .expect("budget abort is a normal pipeline outcome");

    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    assert_eq!(outcome.total_cost_usd, 0.20);
    assert_eq!(outcome.candidates_run, 0);
    assert_eq!(provider.script.lock().await.len(), 1);

    let events = drain(&mut rx);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::StepUsage { .. }))
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Complete { .. }))
    );
}
