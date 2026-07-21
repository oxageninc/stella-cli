use super::*;

impl ScriptedProvider {
    async fn remaining(&self) -> usize {
        self.script.lock().await.len()
    }
}

#[tokio::test]
async fn red_final_verdict_is_verification_failed_not_completed() {
    let provider = ScriptedProvider::new(vec![text_result("single"), text_result("done")]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![false, false], "@@ -1 +1 @@\n-old\n+new");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let (tx, _rx) = mpsc::unbounded_channel();
    let pipeline = Pipeline::new(
        PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall: &recall,
            repo: &repo,
            repo_status: &repo_status,
            commands: &runner,
            approvals: &approvals,
            sleeper: &sleeper,
            hooks: None,
            candidate_workspaces: None,
            steering: None,
        },
        tx,
        PipelineConfig {
            test_command: Some("cargo test -p x".into()),
            max_revisions: 0,
            ..PipelineConfig::default()
        },
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);

    let outcome = pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .expect("verification failure is a typed outcome");

    let verdict = outcome
        .verdict
        .clone()
        .expect("failed evidence is retained");
    assert!(!verdict.passed);
    assert_eq!(
        outcome.status,
        PipelineStatus::VerificationFailed { verdict }
    );
    assert!(
        (outcome.total_cost_usd - 0.0002).abs() < 1e-9,
        "triage and worker spend are retained"
    );
}

#[tokio::test]
async fn enforced_budget_breach_in_triage_stops_before_the_next_paid_stage() {
    let provider = ScriptedProvider::new(vec![
        text_result("multi"),
        text_result(r#"["plan must never run"]"#),
        text_result("worker must never run"),
    ]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![], "");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let (tx, _rx) = mpsc::unbounded_channel();
    let pipeline = Pipeline::new(
        PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall: &recall,
            repo: &repo,
            repo_status: &repo_status,
            commands: &runner,
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
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, Some(0.00005), None);

    let outcome = pipeline
        .run(
            "Refactor the parser and update all callers",
            &mut messages,
            &mut budget,
        )
        .await
        .expect("budget breach is a typed outcome");

    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    assert!(
        (outcome.total_cost_usd - 0.0001).abs() < 1e-9,
        "the over-cap triage call is settled spend"
    );
    assert_eq!(
        provider.remaining().await,
        2,
        "the next paid stage must not start after triage crosses the cap"
    );
}
