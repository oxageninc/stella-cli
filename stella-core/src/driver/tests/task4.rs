use super::*;

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
