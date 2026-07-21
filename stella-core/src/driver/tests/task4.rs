use super::*;

struct BilledResultWithBlockedSpeculation {
    provider_completed: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Provider for BilledResultWithBlockedSpeculation {
    fn id(&self) -> &str {
        "billed-blocked-speculation"
    }

    async fn complete(
        &self,
        _req: CompletionRequest,
    ) -> Result<CompletionResultAlias, ProviderError> {
        unreachable!("the test requires complete_observed")
    }

    async fn complete_observed(
        &self,
        _req: CompletionRequest,
        observer: &dyn stella_protocol::ToolCallObserver,
    ) -> Result<CompletionResultAlias, ProviderError> {
        let call = ToolCall {
            call_id: "blocked-read".into(),
            name: "read_forever".into(),
            input: serde_json::json!({}),
        };
        observer.tool_call_streamed(&call);
        self.provider_completed.notify_one();
        Ok(CompletionResultAlias {
            text: String::new(),
            tool_calls: vec![call],
            usage: CompletionUsage::default(),
            model: self.id().into(),
            cost_usd: 0.25,
            finish_reason: None,
        })
    }
}

struct ForeverRead;

#[async_trait]
impl ToolExecutor for ForeverRead {
    fn schemas(&self) -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "read_forever".into(),
            description: "a deterministic blocked read".into(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: true,
        }]
    }

    async fn execute(&self, _name: &str, _input: &Value) -> ToolOutput {
        std::future::pending().await
    }
}

#[tokio::test]
async fn summary_induced_budget_breach_aborts_with_cost_before_next_provider_call() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Ok(text_result("SUMMARY: earlier steps established the plan")),
            Ok(text_result("must never be called")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let provider_calls = provider.calls.clone();
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
    ];
    for i in 0..6 {
        messages.push(big_assistant_text(&format!("t{i}")));
    }
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, Some(0.00005), None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;

    match outcome {
        TurnOutcome::Aborted { reason, cost_usd } => {
            assert!(reason.contains("budget"));
            assert!(
                (cost_usd - 0.0001).abs() < 1e-9,
                "the abort must retain the settled summary call: {cost_usd}"
            );
        }
        other => panic!("expected a budget abort, got {other:?}"),
    }
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "the summary call may cross the cap, but the next provider call must not start"
    );
}

#[tokio::test]
async fn an_existing_budget_breach_stops_before_paid_compaction() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("must never be called"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let provider_calls = provider.calls.clone();
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
    ];
    for i in 0..6 {
        messages.push(big_assistant_text(&format!("t{i}")));
    }
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, None, Some(0.05));
    budget.reseed_session_spend(0.10);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;

    assert!(matches!(
        outcome,
        TurnOutcome::Aborted { cost_usd, .. } if cost_usd == 0.0
    ));
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        0,
        "an already-over-cap turn must not pay for compaction"
    );
}

#[tokio::test]
async fn cancellation_after_billed_completion_before_speculation_finishes_keeps_the_cost() {
    let provider_completed = Arc::new(tokio::sync::Notify::new());
    let provider = BilledResultWithBlockedSpeculation {
        provider_completed: provider_completed.clone(),
    };
    let tools = ForeverRead;
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![CompletionMessage::user("read")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    {
        let turn = engine.run_turn(&mut messages, &mut budget, &tx);
        tokio::pin!(turn);
        tokio::select! {
            outcome = &mut turn => panic!("blocked speculation must keep the turn pending: {outcome:?}"),
            _ = provider_completed.notified() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                panic!("provider did not complete")
            }
        }
    }

    assert!(
        (budget.session_spent_usd() - 0.25).abs() < 1e-9,
        "a settled provider result stays billed after cancellation: {}",
        budget.session_spent_usd()
    );
}

#[tokio::test]
async fn a_normal_completion_charges_the_budget_exactly_once() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("done"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![CompletionMessage::user("answer")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;

    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert!(
        (budget.session_spent_usd() - 0.0001).abs() < 1e-9,
        "normal completion must charge exactly once: {}",
        budget.session_spent_usd()
    );
}
