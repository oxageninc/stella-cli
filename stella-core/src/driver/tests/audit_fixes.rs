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

/// F7: the overflow summary and the stuck-loop warning both ride User-role
/// messages, but neither is a real user turn — the loop-detection window
/// must see straight through them, and every surviving call must arrive
/// paired with the output it produced.
#[test]
fn engine_injected_user_messages_are_not_loop_window_boundaries() {
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
    let history = |middle: CompletionMessage| {
        vec![
            CompletionMessage::user("task"),
            assistant_with_call("c1"),
            tool_result_msg("c1"),
            middle,
            assistant_with_call("c2"),
            tool_result_msg("c2"),
        ]
    };

    let summary = CompletionMessage::user(format!(
        "{SUMMARY_MARKER_PREFIX} to fit context — full detail was compacted away; \
         re-read files or re-run tools for specifics]\n\nSUMMARY"
    ));
    let records = recent_call_records(&history(summary));
    assert_eq!(
        records
            .iter()
            .map(|r| r.call.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["c1", "c2"],
        "a summarization pass must not truncate the loop window"
    );
    // The detector can only prove no-progress from outputs, so the window
    // must carry each call's result, not bare calls.
    assert!(
        records.iter().all(|r| r.output
            == Some(ToolOutput::Ok {
                content: "ok".into()
            })),
        "records must pair each call with its result: {records:?}"
    );

    let steer = CompletionMessage::user(format!(
        "{LOOP_STEER_PREFIX}] you appear to be looping: change strategy."
    ));
    let records = recent_call_records(&history(steer));
    assert_eq!(
        records.len(),
        2,
        "the stuck-loop warning must not truncate the loop window"
    );

    // A REAL user message (a steer, a REPL turn) still resets the window.
    let records = recent_call_records(&history(CompletionMessage::user("also check the tests")));
    assert_eq!(
        records
            .iter()
            .map(|r| r.call.call_id.as_str())
            .collect::<Vec<_>>(),
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

/// A conversation whose oldest span (after the task statement, before the
/// kept tail) is a summarizable ≥4-message run — the input every direct
/// `summarize_overflow_span` witness below needs.
fn overflow_messages() -> Vec<CompletionMessage> {
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
    ];
    for i in 0..8 {
        messages.push(big_assistant_text(&format!("t{i}")));
    }
    messages
}

fn summary_markers(messages: &[CompletionMessage]) -> usize {
    messages
        .iter()
        .filter(|m| m.content.starts_with(SUMMARY_MARKER_PREFIX))
        .count()
}

/// #368.2: the summarizer is the last line of defense before a terminal
/// context overflow, so a transient blip must be retried (standard policy),
/// not fast-failed (deterministic policy, which discarded the recovery).
#[tokio::test]
async fn overflow_summarizer_retries_a_transient_error() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![
            Err(ProviderError::Transport("429 blip".into())),
            Ok(text_result("SUMMARY")),
        ]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let calls = provider.calls.clone();
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);
    let mut messages = overflow_messages();
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let mut health = SummarizerHealth::default();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = EventSender::new(tx);

    let cost = engine
        .summarize_overflow_span(&mut messages, &mut budget, 500, 1.0, &mut health, &events)
        .await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the transient error must be retried, not fast-failed"
    );
    assert!(cost > 0.0, "the successful retry is paid for: {cost}");
    assert_eq!(
        summary_markers(&messages),
        1,
        "the summary lands after the retry"
    );
    assert_eq!(health.consecutive_failures, 0, "a success clears the latch");
    let events = drain_events(&mut rx);
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "a recovered blip surfaces no failure: {events:?}"
    );
}

/// #368.3: a summary generated and paid for right as the budget trips must
/// still be spliced in — applying it only shrinks the context the resumed
/// session reloads. Discarding it lost paid work for no benefit.
#[tokio::test]
async fn budget_aborted_summary_is_applied_not_discarded() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("SUMMARY"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);
    let mut messages = overflow_messages();
    let before_len = messages.len();
    // The single summarizer call overruns the enforced limit → budget abort
    // with the paid result in hand.
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, Some(0.00005), None);
    let mut health = SummarizerHealth::default();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = EventSender::new(tx);

    let cost = engine
        .summarize_overflow_span(&mut messages, &mut budget, 500, 1.0, &mut health, &events)
        .await;

    assert!(
        (cost - 0.0001).abs() < f64::EPSILON,
        "the paid cost is still returned: {cost}"
    );
    assert_eq!(
        summary_markers(&messages),
        1,
        "the paid summary must be spliced in, not dropped"
    );
    assert!(
        messages.len() < before_len,
        "the span shrank: {} !< {before_len}",
        messages.len()
    );
    let events = drain_events(&mut rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Compaction { summarized, .. } if *summarized > 0
        )),
        "the applied summary is announced: {events:?}"
    );
}

/// #461: the overflow-summary splice names *which* tool-result blocks it
/// folded away in its `Compaction` receipt (spec §6.2). Before the fix
/// `summarized_blocks` was hard-coded empty, so the one compaction path that
/// most changes context reported no block identities. Witness: a tool-result
/// block in the folded span leaves context and is named in the receipt.
#[tokio::test]
async fn overflow_summary_names_the_folded_tool_result_blocks() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("SUMMARY"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);

    // A tool-result block sits in the interior of the summarizable span,
    // paired with its assistant tool_call and surrounded by big assistant
    // texts so neither span bound walks onto a Tool message.
    let folded_output = ToolOutput::Ok {
        content: "read 4096 bytes from src/main.rs".into(),
    };
    let expected_block = crate::receipts::tool_result_block_id(&folded_output);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("the task"),
        big_assistant_text("a0"),
        CompletionMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                call_id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "src/main.rs" }),
            }],
            tool_results: vec![],
            attachments: Vec::new(),
        },
        CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: "c1".into(),
                output: folded_output.clone(),
            }],
            attachments: Vec::new(),
        },
        big_assistant_text("a1"),
        big_assistant_text("a2"),
        big_assistant_text("a3"),
        big_assistant_text("a4"),
    ];

    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let mut health = SummarizerHealth::default();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = EventSender::new(tx);

    let _ = engine
        .summarize_overflow_span(&mut messages, &mut budget, 500, 1.0, &mut health, &events)
        .await;

    assert_eq!(summary_markers(&messages), 1, "the summary was spliced in");
    assert!(
        !messages
            .iter()
            .any(|m| m.tool_results.iter().any(|r| r.call_id == "c1")),
        "the tool-result message was folded into the summary, so it left context"
    );

    let events = drain_events(&mut rx);
    let summarized_blocks = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Compaction {
                summarized,
                summarized_blocks,
                ..
            } if *summarized > 0 => Some(summarized_blocks.clone()),
            _ => None,
        })
        .expect("a summary compaction event was emitted");
    assert!(
        summarized_blocks.contains(&expected_block),
        "summarized_blocks {summarized_blocks:?} must name the folded block {expected_block}"
    );
}

/// #368.4: a summarizer that keeps failing must surface each failure and,
/// after enough consecutive misses, latch — a persistently-timing-out cheap
/// summarizer can't be allowed to re-fire (and re-pay) every remaining step.
#[tokio::test]
async fn repeated_summarizer_failures_emit_events_and_latch() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Err(ProviderError::Transport("down".into()))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let calls = provider.calls.clone();
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, overflow_config(), &sleeper);
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let mut health = SummarizerHealth::default();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = EventSender::new(tx);

    // Every over-budget step this turn hits the same dead summarizer.
    for _ in 0..SUMMARIZER_FAILURE_LATCH {
        let mut messages = overflow_messages();
        let cost = engine
            .summarize_overflow_span(&mut messages, &mut budget, 500, 1.0, &mut health, &events)
            .await;
        assert_eq!(cost, 0.0, "a failed summarizer is free and changes nothing");
        assert_eq!(summary_markers(&messages), 0, "nothing spliced on failure");
    }
    assert!(
        health.is_latched(),
        "consecutive failures must latch the summarizer"
    );
    let calls_while_unlatched = calls.load(Ordering::SeqCst);
    assert!(
        calls_while_unlatched > 0,
        "the summarizer actually ran before latching"
    );
    let failures = drain_events(&mut rx)
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::Error {
                    retryable: true,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        failures, SUMMARIZER_FAILURE_LATCH as usize,
        "each failure surfaces exactly one retryable error"
    );

    // Latched: a further over-budget step must NOT re-fire (or re-pay for)
    // the summarizer.
    let mut messages = overflow_messages();
    let cost = engine
        .summarize_overflow_span(&mut messages, &mut budget, 500, 1.0, &mut health, &events)
        .await;
    assert_eq!(cost, 0.0, "the latched pass spends nothing");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_while_unlatched,
        "latched: the provider is not called again"
    );
    assert!(
        drain_events(&mut rx).is_empty(),
        "latched: no further events are emitted"
    );
}

/// A single observed-mode breach persists across every remaining settled
/// call of the turn, but it must warn once per axis, not once per call —
/// otherwise a session-limit breach on a many-step turn floods the stream.
#[tokio::test]
async fn observed_budget_breach_warns_once_per_axis_per_turn() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        // Two tool-call steps (each settles a cost, each over the limit)
        // then a final answer: three settled calls in one turn.
        script: TokioMutex::new(vec![
            Ok(tool_call_result("call_1", "bash")),
            Ok(tool_call_result("call_2", "bash")),
            Ok(text_result("done")),
        ]),
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
    // Session limit only: the first $0.0001 call already crosses the
    // $0.00005 session cap, and every later call stays over it.
    let mut budget = BudgetGuard::new(BudgetMode::Observed, None, Some(0.00005));
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
    assert_eq!(
        warnings.len(),
        1,
        "the persistent breach must warn once, not once per settled call: {events:?}"
    );
    match warnings[0] {
        AgentEvent::Error { message, .. } => assert!(
            message.contains("session limit"),
            "the breached axis must be named: {message}"
        ),
        other => unreachable!("{other:?}"),
    }
}

/// An enforced session breach that trips as the just-landed call settles
/// aborts through `handle_committed_result` — its reason must name the
/// session axis so the user knows which cap they hit.
#[tokio::test]
async fn enforced_session_breach_abort_reason_names_the_axis() {
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
    // No turn cap, a tiny session cap: the single call breaches the session
    // axis, not the turn axis.
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, None, Some(0.00005));
    let (tx, _rx) = mpsc::unbounded_channel();

    match engine.run_turn(&mut messages, &mut budget, &tx).await {
        TurnOutcome::Aborted { reason, .. } => assert!(
            reason.contains("session limit"),
            "the abort reason must name the breached axis: {reason}"
        ),
        other => panic!("enforced breach must abort: {other:?}"),
    }
}

/// A session already over budget at the turn's opening safe-boundary aborts
/// through `check_budget` (before any call is dispatched) — that reason must
/// name the session axis too.
#[tokio::test]
async fn enforced_session_breach_at_step_boundary_names_the_axis() {
    let provider = ScriptedProvider {
        id: "scripted".into(),
        script: TokioMutex::new(vec![Ok(text_result("done"))]),
        calls: Arc::new(AtomicU32::new(0)),
    };
    let provider_calls = provider.calls.clone();
    let tools = CountingTools {
        calls: Arc::new(AtomicU32::new(0)),
    };
    let sleeper = NoopSleeper;
    let engine = Engine::with_sleeper(&provider, &tools, EngineConfig::default(), &sleeper);
    let mut messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mut budget = BudgetGuard::new(BudgetMode::Enforced, None, Some(0.5));
    // Reseed the session already over its cap (a resumed over-budget
    // session): the very first between-steps check trips.
    budget.reseed_session_spend(1.0);
    let (tx, _rx) = mpsc::unbounded_channel();

    match engine.run_turn(&mut messages, &mut budget, &tx).await {
        TurnOutcome::Aborted { reason, .. } => assert!(
            reason.contains("session limit"),
            "the boundary abort reason must name the breached axis: {reason}"
        ),
        other => panic!("an over-budget session must abort at the boundary: {other:?}"),
    }
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        0,
        "the abort must precede any model call"
    );
}
