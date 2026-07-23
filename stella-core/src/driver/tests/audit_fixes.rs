//! Witnesses for the 2026-07 turn-driver correctness audit (F1–F9): the
//! keep-recent-0 panic guard, observed-budget warnings, budget-abort
//! event/history parity, whitespace-only empty turns, the summary marker's
//! loop-window neutrality, and the hard-cancel `Cancelled` usage envelope.

use super::*;

/// F1: `summarize_keep_recent: 0` is a legal config — the tail walk must
/// not index one past the end (this test panicked with "index out of
/// bounds" before the guard).
#[tokio::test]
async fn summarize_keep_recent_zero_does_not_panic() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("SUMMARY")), Ok(text_result("done"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let config = EngineConfig {
        summarize_keep_recent: 0,
        ..overflow_config()
    };
    let engine = Engine::with_sleeper(&provider, &tools, config, &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
    ];
    for i in 0..6 {
        messages.push(big_assistant_text(&format!("t{i}")));
    }
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, _rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "keep_recent 0 must summarize the whole span, not panic: {outcome:?}"
    );
    let markers = messages
        .iter()
        .filter(|m| m.content.starts_with("[earlier history summarized"))
        .count();
    assert_eq!(markers, 1, "exactly one summary message");
}

/// F2: `BudgetOutcome::Warn`'s contract is that the driver surfaces it —
/// an Observed-mode breach must emit a visible warning (exactly once per
/// settled call, so the twice-per-step gate checks cannot spam it).
#[tokio::test]
async fn observed_budget_breach_emits_a_warning_event() {
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
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    // The single $0.0001 call crosses the $0.00005 observed turn limit.
    let mut budget = BudgetGuard::new(BudgetMode::Observed, Some(0.00005), None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(
        matches!(outcome, TurnOutcome::Completed { .. }),
        "observed mode never gates: {outcome:?}"
    );
    let events = drain_events(&mut rx);
    let warnings: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::Error {
                    retryable: true,
                    message,
                } if message.contains("budget warning")
            )
        })
        .collect();
    assert_eq!(warnings.len(), 1, "exactly one warning: {events:?}");
    match warnings[0] {
        AgentEvent::Error { message, .. } => {
            assert!(
                message.contains("turn limit"),
                "axis must be named: {message}"
            );
        }
        other => unreachable!("{other:?}"),
    }
}

/// F5: the budget-abort path's synthetic tool results must reach the event
/// stream, not just `messages` — StepUsage already announced the calls, so
/// a transcript reconstructed from events must resolve them too.
#[tokio::test]
async fn budget_abort_synthetic_results_are_visible_in_the_event_stream() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(tool_call_result("call_1", "bash"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tool_calls = Arc::new(AtomicU32::new(0));
    let tools = CountingTools {
        calls: tool_calls.clone(),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, Some(0.00005), None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(matches!(outcome, TurnOutcome::Aborted { .. }));
    assert_eq!(
        tool_calls.load(Ordering::SeqCst),
        0,
        "the aborted step's calls must never execute"
    );
    let events = drain_events(&mut rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult {
                call_id,
                speculated: false,
                output: ToolOutput::Error { message },
                ..
            } if call_id == "call_1" && message.contains("not executed")
        )),
        "the synthetic result must be on the wire: {events:?}"
    );
}

/// F6: a whitespace-only response is the empty-turn defect, not an answer —
/// it must abort without first streaming a blank `Text` event.
#[tokio::test]
async fn whitespace_only_completion_aborts_without_a_text_event() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("\n\n"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    let outcome = engine.run_turn(&mut messages, &mut budget, &tx).await;
    assert!(
        matches!(outcome, TurnOutcome::Aborted { .. }),
        "whitespace-only is an empty turn: {outcome:?}"
    );
    let events = drain_events(&mut rx);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Text { .. })),
        "no blank Text event may precede the empty-turn abort: {events:?}"
    );
}

/// F7: the overflow summary rides a User-role message, but it is not a real
/// user turn — the loop-detection window must see straight through it.
#[test]
fn summary_marker_is_not_a_loop_window_boundary() {
    let assistant_with_call = |call_id: &str| CompletionMessage {
        role: MessageRole::Assistant,
        content: String::new(),
        tool_calls: vec![ToolCall {
            call_id: call_id.into(),
            name: "bash".into(),
            input: serde_json::json!({"cmd": "ls"}),
        }],
        tool_results: vec![],
        attachments: Vec::new(),
    };
    let tool_result_msg = |call_id: &str| CompletionMessage {
        role: MessageRole::Tool,
        content: String::new(),
        tool_calls: vec![],
        tool_results: vec![ToolResult {
            call_id: call_id.into(),
            output: ToolOutput::Ok {
                content: "ok".into(),
            },
        }],
        attachments: Vec::new(),
    };
    let summary = CompletionMessage::user(format!(
        "{SUMMARY_MARKER_PREFIX} to fit context — full detail was compacted away; \
         re-read files or re-run tools for specifics]\n\nSUMMARY"
    ));

    let with_summary = vec![
        CompletionMessage::user("task"),
        assistant_with_call("c1"),
        tool_result_msg("c1"),
        summary,
        assistant_with_call("c2"),
        tool_result_msg("c2"),
    ];
    let calls = recent_tool_calls(&with_summary);
    assert_eq!(
        calls.iter().map(|c| c.call_id.as_str()).collect::<Vec<_>>(),
        vec!["c1", "c2"],
        "a summarization pass must not truncate the loop window"
    );

    // A REAL user message (a steer, a REPL turn) still resets the window.
    let mut with_real_user = with_summary;
    with_real_user[3] = CompletionMessage::user("also check the tests");
    let calls = recent_tool_calls(&with_real_user);
    assert_eq!(
        calls.iter().map(|c| c.call_id.as_str()).collect::<Vec<_>>(),
        vec!["c2"],
        "a genuine user turn is still a window boundary"
    );
}

/// F9's provider: announces it started streaming, then never resolves —
/// the only way out is the caller dropping the turn (hard cancel).
struct HangingProvider {
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Provider for HangingProvider {
    fn id(&self) -> &str {
        "hanging"
    }

    async fn complete(
        &self,
        _req: CompletionRequest,
    ) -> Result<CompletionResultAlias, ProviderError> {
        unreachable!("the engine must drive complete_observed, never bare complete")
    }

    async fn complete_observed(
        &self,
        _req: CompletionRequest,
        _observer: &dyn stella_protocol::ToolCallObserver,
    ) -> Result<CompletionResultAlias, ProviderError> {
        self.started.notify_one();
        std::future::pending().await
    }
}

/// F9: a hard cancel that drops the turn while a paid attempt is mid-stream
/// must leave exactly one content-free `Cancelled` usage envelope — the
/// call may have real server-side cost and must not vanish from accounting.
#[tokio::test]
async fn hard_cancel_mid_stream_emits_a_cancelled_usage_envelope() {
    let started = Arc::new(tokio::sync::Notify::new());
    let provider = HangingProvider {
        started: started.clone(),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![CompletionMessage::user("secret prompt text")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let (tx, mut rx) = mpsc::unbounded_channel();

    {
        let turn = engine.run_turn(&mut messages, &mut budget, &tx);
        tokio::pin!(turn);
        tokio::select! {
            outcome = &mut turn => panic!("a hanging provider must keep the turn pending: {outcome:?}"),
            _ = started.notified() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                panic!("provider never started streaming")
            }
        }
        // Scope end drops the pinned turn future — the hard cancel.
    }

    let events = drain_events(&mut rx);
    let incomplete: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::UsageIncomplete { .. }))
        .collect();
    assert_eq!(
        incomplete.len(),
        1,
        "exactly one cancelled-usage envelope: {events:?}"
    );
    assert!(matches!(
        incomplete[0],
        AgentEvent::UsageIncomplete {
            reason: stella_protocol::UsageIncompleteReason::Cancelled,
            model,
            retries: None,
            provider,
            ..
        } if model == "unknown" && provider == "hanging"
    ));
    let wire = serde_json::to_string(incomplete[0]).expect("wire");
    assert!(
        !wire.contains("secret prompt text"),
        "the envelope must stay content-free: {wire}"
    );
}
