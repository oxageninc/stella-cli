//! Paid-call accounting witnesses for every raw pipeline role.

use super::*;

fn usage_events(events: &[AgentEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|event| serde_json::to_value(event).ok())
        .filter(|event| {
            matches!(
                event.get("type").and_then(serde_json::Value::as_str),
                Some("step_usage" | "usage_incomplete")
            )
        })
        .collect()
}

fn usage_roles(events: &[AgentEvent]) -> Vec<String> {
    usage_events(events)
        .into_iter()
        .filter_map(|event| event.get("role")?.as_str().map(str::to_owned))
        .collect()
}

struct AnyProvider<'a>(&'a dyn Provider);

impl ProviderResolver for AnyProvider<'_> {
    fn provider_for(&self, _model: &ModelRef) -> Option<&dyn Provider> {
        Some(self.0)
    }
}

struct ErrorProvider;

#[async_trait]
impl Provider for ErrorProvider {
    fn id(&self) -> &str {
        "paid-error"
    }

    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        Err(ProviderError::Terminal("upstream failed".into()))
    }
}

struct SlowProvider;

#[async_trait]
impl Provider for SlowProvider {
    fn id(&self) -> &str {
        "paid-timeout"
    }

    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(text_result("lookup"))
    }
}

async fn run_triage_only(
    provider: &dyn Provider,
    config: PipelineConfig,
    budget: &mut BudgetGuard,
) -> (Result<TaskClass, PipelineBudgetAbort>, f64, Vec<AgentEvent>) {
    let resolver = AnyProvider(provider);
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let runner = ScriptedRunner::new(vec![], "");
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
        config,
    );
    let mut total = 0.0;
    let result = pipeline
        .triage("inspect the repository", budget, &mut total)
        .await;
    (result, total, drain(&mut rx))
}

#[tokio::test]
async fn triage_success_emits_usage_before_budget_abort() {
    let provider = ScriptedProvider::new(vec![text_result("lookup")]);
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, Some(0.00001), None);
    let (result, total, events) =
        run_triage_only(&provider, PipelineConfig::default(), &mut budget).await;

    assert!(result.is_err(), "the settled call crosses the tiny budget");
    assert_eq!(total, 0.0001);
    let serialized: Vec<_> = events
        .iter()
        .filter_map(|event| serde_json::to_value(event).ok())
        .collect();
    let usage = serialized
        .iter()
        .position(|event| event["type"] == "step_usage")
        .expect("the paid call must emit usage");
    let tick = serialized
        .iter()
        .position(|event| event["type"] == "budget_tick")
        .expect("the paid call must settle the budget");
    assert!(
        usage < tick,
        "usage must be durable before an abort can return"
    );
    assert_eq!(serialized[usage]["role"], "triage");
    assert_eq!(serialized[usage]["provider"], "scripted");
    assert_eq!(serialized[usage]["model"], "scripted");
    // The provider call itself succeeded with a real, trustworthy usage
    // envelope (`text_result`'s `reported: true`) — `complete` tracks that,
    // not the turn's outcome. The *subsequent* budget check aborting the
    // turn is a separate concern from whether this call's own accounting
    // record can be trusted; it can.
    assert_eq!(serialized[usage]["complete"], true);
}

#[tokio::test]
async fn triage_provider_error_emits_content_free_incompleteness() {
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (result, _, events) =
        run_triage_only(&ErrorProvider, PipelineConfig::default(), &mut budget).await;
    assert_eq!(result.unwrap(), TaskClass::SimpleLookup);
    let usage = usage_events(&events);
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["type"], "usage_incomplete");
    assert_eq!(usage[0]["role"], "triage");
    assert_eq!(usage[0]["reason"], "provider_error");
    assert!(
        usage[0].get("message").is_none(),
        "no provider content leaks"
    );
}

#[tokio::test]
async fn triage_timeout_emits_content_free_incompleteness() {
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let config = PipelineConfig {
        triage_latency_ceiling: Duration::from_millis(1),
        ..PipelineConfig::default()
    };
    let (result, _, events) = run_triage_only(&SlowProvider, config, &mut budget).await;
    assert_eq!(result.unwrap(), TaskClass::SimpleLookup);
    let usage = usage_events(&events);
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["type"], "usage_incomplete");
    assert_eq!(usage[0]["reason"], "timeout");
}

#[tokio::test]
async fn plan_and_plan_repair_each_emit_one_paid_call_envelope() {
    let provider = ScriptedProvider::new(vec![
        text_result("multi"),
        text_result("not-json"),
        text_result(r#"["s1","s2","s3","s4","s5","s6"]"#),
    ]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![], "");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = FixedGate(ScopeDecision::Approve);
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
            headless: true,
            headless_bypass_scope_review: false,
            ..PipelineConfig::default()
        },
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let error = pipeline
        .run(
            "Refactor across the codebase and update every caller",
            &mut messages,
            &mut budget,
        )
        .await
        .expect_err("large headless plan stops after planning");
    assert!((error.total_cost_usd - 0.0003).abs() < f64::EPSILON * 4.0);
    assert_eq!(
        usage_roles(&drain(&mut rx)),
        ["triage", "plan", "plan_repair"]
    );
}

#[tokio::test]
async fn witness_author_and_repair_are_individually_metered_on_abort() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("TEST_COMMAND: cargo test --test witness always_green -- --exact"),
        text_result("TEST_COMMAND: cargo test --test witness still_green -- --exact"),
    ]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace =
        FakeWorkspace::new(0, vec![true, true], Ok(vec![]), log.clone()).with_repo_status(
            SeqRepoStatus::new(vec![vec![], vec![("tests/witness.rs", "w1")]]),
        );
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log);
    let (outcome, events, _) = run_isolated(
        &provider,
        &port,
        PipelineConfig::default(),
        "Fix the retry bug",
    )
    .await;
    assert!(matches!(
        outcome.unwrap().status,
        PipelineStatus::Aborted { .. }
    ));
    assert_eq!(
        usage_roles(&events),
        ["triage", "witness_author", "witness_repair"]
    );
}

#[tokio::test]
async fn distress_guidance_and_engine_revisions_keep_distinct_envelopes() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("done"),
        text_result("first fix"),
        text_result("fix the parser instead"),
        text_result("second fix"),
    ]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![false, false, false, false], "@@ -1 +1 @@\n-a\n+b");
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
            max_revisions: 2,
            ..PipelineConfig::default()
        },
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let outcome = pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .unwrap();
    assert_eq!(outcome.revisions, 2);
    assert_eq!(
        usage_roles(&drain(&mut rx)),
        ["triage", "worker", "worker", "distress_guidance", "worker"]
    );
}

#[tokio::test]
async fn model_judge_call_is_metered_separately_from_worker() {
    let provider = ScriptedProvider::new(vec![
        text_result("lookup"),
        text_result("done"),
        text_result("PASS verified"),
    ]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![], "@@ -1 +1 @@\n-a\n+b");
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
            witness_writer: false,
            ..PipelineConfig::default()
        },
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    pipeline
        .run("Look up the answer", &mut messages, &mut budget)
        .await
        .unwrap();
    assert_eq!(usage_roles(&drain(&mut rx)), ["triage", "worker", "judge"]);
}
