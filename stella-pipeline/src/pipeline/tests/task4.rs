use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct FirstTwoProviderLookups<'a> {
    provider: &'a ScriptedProvider,
    lookups: AtomicUsize,
}

impl ProviderResolver for FirstTwoProviderLookups<'_> {
    fn provider_for(&self, _model: &ModelRef) -> Option<&dyn Provider> {
        (self.lookups.fetch_add(1, Ordering::SeqCst) < 2).then_some(self.provider as &dyn Provider)
    }
}

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
    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Complete { .. })),
        "a failed verification must never emit the success terminal event"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::Error { message, retryable: false }
                if message.contains("verification failed")
                    && message.contains(&outcome.verdict.as_ref().unwrap().summary)
        )),
        "the terminal failure event must retain verdict evidence: {events:?}"
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

#[tokio::test]
async fn unavailable_independent_witness_fails_closed_before_authoring() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("TEST_COMMAND: cargo test --test witness witness -- --exact"),
    ]);
    let resolver = FirstTwoProviderLookups {
        provider: &provider,
        lookups: AtomicUsize::new(0),
    };
    let runner = ScriptedRunner::new(vec![false], "");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace = FakeWorkspace::new(0, vec![false], Ok(vec![]), log.clone()).with_repo_status(
        SeqRepoStatus::new(vec![vec![], vec![("tests/witness.rs", "sha256:test")]]),
    );
    let candidate_workspaces = FakeWorkspacePort::new(vec![Ok(workspace)], log);
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
            diagnostics: &runner,
            tests: &runner,
            approvals: &approvals,
            sleeper: &sleeper,
            hooks: None,
            candidate_workspaces: Some(&candidate_workspaces),
            steering: None,
        },
        tx,
        PipelineConfig::default(),
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);

    let outcome = pipeline
        .run("Fix the parser", &mut messages, &mut budget)
        .await
        .expect("unavailable independent witness is a truthful abort");

    assert!(matches!(
        outcome.status,
        PipelineStatus::Aborted { ref reason }
            if reason.contains("independent witness author")
    ));
    assert!(
        (outcome.total_cost_usd - 0.0001).abs() < 1e-9,
        "only triage spend settles before role independence is checked: {outcome:?}"
    );
}
