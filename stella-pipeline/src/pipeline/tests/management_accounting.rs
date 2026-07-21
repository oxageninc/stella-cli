//! Management-call deadline and budget-accounting witnesses.

use super::*;

/// Missing the triage decision deadline must not cancel a paid provider call.
/// The late answer is ignored for classification, but its exact usage/cost is
/// emitted and charged before triage returns.
#[tokio::test]
async fn late_triage_is_ignored_but_awaited_and_metered() {
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

    let started = std::time::Instant::now();
    let class = pipeline
        .triage("What is two plus two?", &mut budget, &mut total)
        .await
        .expect("the late but settled triage call remains within budget");

    assert!(started.elapsed() >= Duration::from_millis(20));
    assert_eq!(class, TaskClass::SimpleLookup);
    assert_eq!(total, 0.05);
    assert_eq!(budget.spent_usd(), 0.05);
    let events = drain(&mut rx);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::StepUsage {
            purpose: Some(purpose),
            input_tokens: 123,
            output_tokens: 7,
            ..
        } if purpose == "triage"
    )));
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
