//! Engine-backed paid-call incompleteness witnesses.

use super::*;

#[tokio::test]
async fn exhausted_worker_call_emits_one_content_free_incompleteness_event() {
    let provider = ScriptedProvider {
        id: "anthropic-fallback".into(),
        script: TokioMutex::new(vec![Err(ProviderError::Terminal(
            "private upstream body".into(),
        ))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("work"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(matches!(outcome, TurnOutcome::Aborted { .. }));
    let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let incomplete: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::UsageIncomplete { .. }))
        .collect();
    assert_eq!(incomplete.len(), 1);
    assert!(matches!(
        incomplete[0],
        AgentEvent::UsageIncomplete {
            role: stella_protocol::ModelCallRole::Worker,
            provider,
            model,
            reason: stella_protocol::UsageIncompleteReason::ProviderError,
            retries: Some(0),
            ..
        } if provider == "anthropic-fallback" && model == "unknown" && model != provider
    ));
    let wire = serde_json::to_string(incomplete[0]).unwrap();
    assert!(!wire.contains("private upstream body"));
}

#[tokio::test]
async fn successful_retry_keeps_the_failed_attempt_usage_incomplete() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Err(ProviderError::Transport("private failed attempt".into())),
            Ok(text_result("done")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let config = EngineConfig {
        retry_policy: RetryPolicy::new(1, 0, 0),
        ..EngineConfig::default()
    };
    let engine = Engine::with_sleeper(&provider, &tools, config, &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("work"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;

    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    let events = drain_events(&mut rx);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::UsageIncomplete { .. }))
            .count(),
        1,
        "the first dispatched attempt has unknowable usage even though its retry succeeded"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::StepUsage {
            retries: 1,
            complete: true,
            ..
        }
    )));
    let incomplete: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::UsageIncomplete { .. }))
        .collect();
    let wire = serde_json::to_string(&incomplete).expect("wire");
    assert!(!wire.contains("private failed attempt"));
}

fn overflow_messages() -> Vec<CompletionMessage> {
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("task"),
    ];
    for index in 0..6 {
        messages.push(big_assistant_text(&format!("t{index}")));
    }
    messages
}

#[tokio::test]
async fn overflow_summarizer_emits_its_own_usage_envelope() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("SUMMARY")), Ok(text_result("done"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);
    let mut messages = overflow_messages();
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;

    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    let events = drain_events(&mut rx);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::StepUsage {
            role: stella_protocol::ModelCallRole::Summarization,
            provider,
            model,
            ..
        } if provider == "scripted" && model == "scripted"
    )));
}

#[tokio::test]
async fn failed_overflow_summarizer_emits_content_free_incompleteness() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Err(ProviderError::Terminal("private upstream body".into())),
            Ok(text_result("done")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);
    let mut messages = overflow_messages();
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;

    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    let events = drain_events(&mut rx);
    let incomplete = events
        .iter()
        .find(|event| {
            matches!(
                event,
                AgentEvent::UsageIncomplete {
                    role: stella_protocol::ModelCallRole::Summarization,
                    reason: stella_protocol::UsageIncompleteReason::ProviderError,
                    ..
                }
            )
        })
        .expect("summarizer incomplete envelope");
    assert!(
        !serde_json::to_string(incomplete)
            .expect("wire")
            .contains("private upstream body")
    );
}
