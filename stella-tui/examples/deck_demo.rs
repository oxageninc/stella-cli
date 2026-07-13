//! A runnable, interactive demo of the Stella command deck.
//!
//! ```sh
//! cargo run -p stella-tui --example deck_demo
//! ```
//!
//! It plays a scripted multi-agent scenario (`scenario::demo_inbound`) and then
//! stays interactive: typing a prompt + Enter spins up a new agent that runs a
//! short scripted turn, and the Agents-tab controls (`p`/`s`/`r`) flip an
//! agent's status live. Agent pids are the demo process's own, so the CPU/MEM
//! columns show real numbers. Ctrl-C quits.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use stella_protocol::{AgentEvent, FileChangeKind, StageKind, ToolCall, ToolOutput};
use tokio::sync::mpsc;

use stella_tui::scenario::{demo_graph, demo_inbound};
use stella_tui::{
    AgentControl, AgentMeta, AgentStatus, DeckOptions, Inbound, ScopeDecision, UserInput,
    WorkspaceInput, run_deck,
};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Inbound>();
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<WorkspaceInput>();

    let pid = std::process::id();

    // Playback: stream the scripted scenario in with human-paced delays.
    let play_tx = in_tx.clone();
    tokio::spawn(async move {
        for inbound in demo_inbound(now_ms(), pid) {
            if play_tx.send(inbound).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    // Reactor: make the deck interactive. A queued prompt becomes a new agent
    // that runs a short scripted turn; a control flips an agent's status.
    let react_tx = in_tx.clone();
    tokio::spawn(async move {
        let mut n = 0u32;
        while let Some(input) = sub_rx.recv().await {
            match input {
                WorkspaceInput::Enqueue { text } => {
                    n += 1;
                    let id = format!("you:{n}");
                    let _ = react_tx.send(Inbound::Register(
                        AgentMeta::new(id.clone(), text, now_ms())
                            .with_role("user")
                            .with_pid(pid),
                    ));
                    let tx = react_tx.clone();
                    tokio::spawn(async move { mini_run(&tx, &id).await });
                }
                WorkspaceInput::Control { agent, control } => {
                    let status = match control {
                        AgentControl::Pause => AgentStatus::Paused,
                        AgentControl::Resume | AgentControl::Restart => AgentStatus::Running,
                        AgentControl::Stop => AgentStatus::Killed,
                    };
                    let _ = react_tx.send(Inbound::Status { agent, status });
                }
                // Gate answers (scope decisions, ask-user replies) loop back
                // as the inbound events a real engine would emit, so the
                // pending gate actually clears and the demo's advertised
                // in-place answering works end to end.
                WorkspaceInput::ToAgent { agent, input } => match input {
                    UserInput::ScopeDecision(decision) => {
                        let _ = react_tx.send(Inbound::Event {
                            agent: agent.clone(),
                            event: AgentEvent::Text {
                                delta: format!("scope decision: {decision:?}\n"),
                            },
                        });
                        // Any non-ScopeReview stage clears the pending gate;
                        // an abort ends the run instead.
                        let next = match decision {
                            ScopeDecision::Approve | ScopeDecision::Trim => AgentEvent::Stage {
                                name: StageKind::Execute,
                            },
                            ScopeDecision::Abort => AgentEvent::Complete {
                                model: "glm-5.2".into(),
                                cost_usd: 0.0,
                            },
                        };
                        let _ = react_tx.send(Inbound::Event { agent, event: next });
                    }
                    // The answer to ask_user returns as that call's
                    // ToolResult, correlated by id — the documented path
                    // that clears the pending question.
                    UserInput::AskUserAnswer { id, answer } => {
                        let _ = react_tx.send(Inbound::Event {
                            agent,
                            event: AgentEvent::ToolResult {
                                call_id: id,
                                output: ToolOutput::Ok { content: answer },
                                duration_ms: 0,
                            },
                        });
                    }
                    UserInput::Prompt { .. } | UserInput::Cancel => {}
                },
                WorkspaceInput::Quit => break,
            }
        }
    });

    // The original sender is dropped; the two task-held clones keep the inbound
    // stream open for the life of the session.
    drop(in_tx);

    let opts = DeckOptions {
        initial_graph: Some(demo_graph()),
        ..Default::default()
    };
    run_deck(opts, in_rx, sub_tx).await
}

/// A short scripted turn for a user-dispatched agent.
async fn mini_run(tx: &mpsc::UnboundedSender<Inbound>, id: &str) {
    let ev = |event: AgentEvent| Inbound::Event {
        agent: id.to_string(),
        event,
    };
    let steps = vec![
        ev(AgentEvent::Stage {
            name: StageKind::Triage,
        }),
        ev(AgentEvent::Stage {
            name: StageKind::Execute,
        }),
        ev(AgentEvent::ToolStart {
            call: ToolCall {
                call_id: format!("{id}-c1"),
                name: "read_file".into(),
                input: json!({ "path": "src/lib.rs" }),
            },
        }),
        ev(AgentEvent::ToolResult {
            call_id: format!("{id}-c1"),
            output: ToolOutput::Ok {
                content: "ok".into(),
            },
            duration_ms: 30,
        }),
        ev(AgentEvent::FileChange {
            path: "src/lib.rs".into(),
            kind: FileChangeKind::Modified,
            diff: Some("@@ -1 +1,2 @@\n-old\n+new\n+line\n".into()),
        }),
        ev(AgentEvent::StepUsage {
            step: 1,
            model: "glm-5.2".into(),
            input_tokens: 4_000,
            output_tokens: 260,
            cached_input_tokens: 2_000,
            cost_usd: 0.008,
            duration_ms: 1_100,
            retries: 0,
            tool_calls: 1,
        }),
        ev(AgentEvent::BudgetTick {
            spent_usd: 0.008,
            limit_usd: Some(1.0),
            mode: stella_protocol::BudgetMode::Observed,
        }),
        ev(AgentEvent::Complete {
            model: "glm-5.2".into(),
            cost_usd: 0.008,
        }),
    ];
    for step in steps {
        if tx.send(step).is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(350)).await;
    }
}
