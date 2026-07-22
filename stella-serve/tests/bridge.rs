//! Proves the `!Send` concurrency bridge in isolation, with no HTTP: a mock
//! "host" reads [`ServerFrame`]s off a live [`Session`] and answers the
//! reverse-RPC requests, exactly as the real host (Oxagen) will over HTTP. If
//! this holds, wrapping a socket around it is transport plumbing.

use serde_json::json;
use stella_core::{BudgetGuard, EngineConfig};
use stella_protocol::{
    BudgetMode, CompletionMessage, CompletionResult, CompletionUsage, ToolCall, ToolOutput,
    ToolSchema,
};
use stella_serve::{ServerFrame, Session, SessionSpec, TurnOutcomeWire};

/// Build a mock model result carrying a final text answer and no tool calls —
/// the engine treats this as "the turn is done."
fn final_answer(text: &str) -> CompletionResult {
    CompletionResult {
        text: text.to_string(),
        tool_calls: vec![],
        usage: CompletionUsage {
            reported: true,
            ..CompletionUsage::default()
        },
        model: "mock".to_string(),
        cost_usd: 0.0,
        finish_reason: None,
    }
}

/// Build a mock model result that asks for one tool call.
fn wants_tool(call_id: &str, name: &str, input: serde_json::Value) -> CompletionResult {
    CompletionResult {
        text: String::new(),
        tool_calls: vec![ToolCall {
            call_id: call_id.to_string(),
            name: name.to_string(),
            input,
        }],
        usage: CompletionUsage {
            reported: true,
            ..CompletionUsage::default()
        },
        model: "mock".to_string(),
        cost_usd: 0.0,
        finish_reason: None,
    }
}

fn echo_tool() -> ToolSchema {
    ToolSchema {
        name: "echo".to_string(),
        description: "echo its input".to_string(),
        input_schema: json!({ "type": "object" }),
        read_only: false,
    }
}

fn spec_for(prompt: &str) -> SessionSpec {
    SessionSpec {
        provider_id: "mock".to_string(),
        tools: vec![echo_tool()],
        messages: vec![CompletionMessage::user(prompt)],
        config: EngineConfig::default(),
        budget: BudgetGuard::new(BudgetMode::Off, None, None),
    }
}

/// The full loop: model asks for a tool, the host runs it and answers, the model
/// then produces a final answer, and the turn completes. Proves both reverse-RPC
/// ports (provider + tool) round-trip across the thread boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_round_trip_completes_the_turn() {
    let mut session = Session::start(spec_for("use the echo tool then answer"));

    let mut provider_calls = 0usize;
    let mut tool_calls = 0usize;
    let mut events = 0usize;
    let mut outcome = None;

    while let Some(frame) = session.next_frame().await {
        match frame {
            ServerFrame::Event { .. } => events += 1,
            ServerFrame::ProviderRequest { request_id, .. } => {
                provider_calls += 1;
                let result = if provider_calls == 1 {
                    wants_tool("call-1", "echo", json!({ "text": "hi" }))
                } else {
                    final_answer("done")
                };
                session.resolve_provider(&request_id, result).unwrap();
            }
            ServerFrame::ToolRequest {
                request_id, name, ..
            } => {
                tool_calls += 1;
                assert_eq!(name, "echo");
                session
                    .resolve_tool(
                        &request_id,
                        ToolOutput::Ok {
                            content: "echoed".to_string(),
                        },
                    )
                    .unwrap();
            }
            ServerFrame::TurnComplete { outcome: done } => outcome = Some(done),
        }
    }

    assert_eq!(provider_calls, 2, "model called before and after the tool");
    assert_eq!(tool_calls, 1, "the one requested tool call round-tripped");
    assert!(events > 0, "the turn emitted agent events for the UI");
    assert_eq!(
        outcome,
        Some(TurnOutcomeWire::Completed {
            text: "done".to_string(),
            cost_usd: 0.0,
        }),
    );
}

/// A turn whose model answers immediately needs no tool round-trip and still
/// completes — the minimal path through the bridge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn immediate_completion_needs_no_tools() {
    let mut session = Session::start(spec_for("just answer"));

    let mut provider_calls = 0usize;
    let mut tool_calls = 0usize;
    let mut outcome = None;

    while let Some(frame) = session.next_frame().await {
        match frame {
            ServerFrame::Event { .. } => {}
            ServerFrame::ProviderRequest { request_id, .. } => {
                provider_calls += 1;
                session
                    .resolve_provider(&request_id, final_answer("hello"))
                    .unwrap();
            }
            ServerFrame::ToolRequest { .. } => tool_calls += 1,
            ServerFrame::TurnComplete { outcome: done } => outcome = Some(done),
        }
    }

    assert_eq!(provider_calls, 1);
    assert_eq!(tool_calls, 0);
    assert_eq!(
        outcome,
        Some(TurnOutcomeWire::Completed {
            text: "hello".to_string(),
            cost_usd: 0.0,
        }),
    );
}

/// A classified provider failure aborts the turn cleanly (no panic, a terminal
/// frame). Uses a retryable transport error the engine will retry then give up
/// on — the point is the bridge surfaces the error path, not the retry count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_error_aborts_cleanly() {
    let mut session = Session::start(spec_for("this will fail"));

    let mut outcome = None;
    while let Some(frame) = session.next_frame().await {
        match frame {
            ServerFrame::ProviderRequest { request_id, .. } => {
                session
                    .fail_provider(
                        &request_id,
                        stella_protocol::ProviderError::Terminal("mock outage".to_string()),
                    )
                    .unwrap();
            }
            ServerFrame::TurnComplete { outcome: done } => outcome = Some(done),
            _ => {}
        }
    }

    match outcome {
        Some(TurnOutcomeWire::Aborted { .. }) => {}
        other => panic!("expected a clean abort, got {other:?}"),
    }
}
