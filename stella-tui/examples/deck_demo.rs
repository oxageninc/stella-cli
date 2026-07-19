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
//!
//! Also demoable here: `!ls` runs a real shell command immediately on its own
//! `shell` lane; `/` opens the command popup; `ctrl+t` (or `↑` while prompts
//! are queued) opens the queue editor; `ctrl+r` expands collapsed thinking.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use stella_protocol::{AgentEvent, FileChangeKind, StageKind, ToolCall, ToolOutput};
use tokio::sync::mpsc;

use stella_tui::scenario::{demo_graph, demo_inbound};
use stella_tui::{
    AgentControl, AgentMeta, AgentStatus, DeckOptions, EngineConfigState, GraphNode, GraphSnapshot,
    Inbound, ScopeDecision, SkillsView, SlashCommand, UserInput, WorkspaceInput, run_deck,
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
                // The demo has no real dispatch queue, so a front-insert
                // (the first submission after a double-Esc hold) starts a
                // scripted run just like a plain enqueue.
                WorkspaceInput::Enqueue { text } | WorkspaceInput::EnqueueFront { text } => {
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
                // Double-Esc: with no real backlog to requeue, the demo just
                // stops the agent — the same terminal status as Stop.
                WorkspaceInput::StopAndHold { agent } => {
                    let _ = react_tx.send(Inbound::Status {
                        agent,
                        status: AgentStatus::Killed,
                    });
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
                                speculated: false,
                            },
                        });
                    }
                    UserInput::Prompt { .. } | UserInput::Cancel => {}
                },
                // Queue edits are already reflected in the deck's local queue
                // (the shell's out-of-band echo); a real engine would also
                // drop the prompt from its own backlog here.
                WorkspaceInput::QueueRemove { .. } | WorkspaceInput::QueueClear => {}
                // The Graph tab's file picker re-roots on a file. The demo has
                // no code-graph store, so it synthesizes a minimal neighborhood
                // centered on the pick — enough to show the pick → re-root
                // round-trip the real CLI does against `codegraph.db`.
                WorkspaceInput::FocusGraphFile { file } => {
                    let _ = react_tx.send(Inbound::GraphSnapshot(GraphSnapshot {
                        focus: file.clone(),
                        nodes: vec![GraphNode {
                            label: file.clone(),
                            kind: "file".into(),
                            location: Some(file),
                        }],
                        edges: vec![],
                        files: demo_graph().files,
                    }));
                }
                // The installed-agents manager needs the real driver (disk +
                // provider); the demo answers with an empty list so the pane
                // renders its empty state instead of loading forever.
                WorkspaceInput::AgentsRefresh
                | WorkspaceInput::AgentSave { .. }
                | WorkspaceInput::AgentPin { .. }
                | WorkspaceInput::AgentCreate { .. } => {
                    let _ = react_tx.send(Inbound::AgentsList {
                        entries: vec![],
                        status: Some("the demo has no agents on disk".to_string()),
                    });
                }
                // The demo has no skills engine — answer with an empty list so
                // the SKILLS tab renders its empty state instead of hanging.
                WorkspaceInput::Skill(_) => {
                    let _ = react_tx.send(Inbound::Skills(SkillsView {
                        rows: vec![],
                        status: Some("the demo has no skills on disk".to_string()),
                        busy: false,
                    }));
                }
                // The ISSUES tab talks to a real tracker through the CLI
                // driver; the demo has none, so a list request answers with
                // the same no-tracker hint the driver would send and the
                // rest are inert.
                WorkspaceInput::IssuesRefresh { seq, .. } => {
                    let _ = react_tx.send(Inbound::IssuesList {
                        seq,
                        outcome: Err("no tracker connected — run `stella connect github` or \
                             `stella connect linear`"
                            .to_string()),
                    });
                }
                WorkspaceInput::IssueCreate { .. }
                | WorkspaceInput::IssueAct { .. }
                | WorkspaceInput::EntitySearch { .. } => {}
                // The MCP tab's actions are serviced by the real CLI driver;
                // this demo has no MCP state, so they are inert here.
                WorkspaceInput::McpToggle { .. }
                | WorkspaceInput::McpSearch { .. }
                | WorkspaceInput::McpInstall { .. }
                | WorkspaceInput::McpRemove { .. }
                | WorkspaceInput::McpAuth { .. }
                | WorkspaceInput::McpOauthLogin { .. }
                | WorkspaceInput::McpRefresh => {}
                // The SESSIONS overlay reads the machine-wide registry and the
                // inbox reads the notification store — both live in the real
                // CLI driver. The demo answers with empty snapshots so the
                // overlays render their empty states instead of waiting.
                WorkspaceInput::SessionsRefresh
                | WorkspaceInput::SessionArchive { .. }
                | WorkspaceInput::SessionDelete { .. } => {
                    let _ = react_tx.send(Inbound::Sessions(vec![]));
                }
                WorkspaceInput::NotificationRead { .. } | WorkspaceInput::NotificationsReadAll => {
                    let _ = react_tx.send(Inbound::Notifications(vec![]));
                }
                // The ENGINE overlay edits the driver-owned settings
                // snapshot. The demo has no settings on disk: a save echoes
                // the submitted state back (so the round-trip and the
                // modified-marker clearing are demoable), a refresh answers
                // with an empty default so the overlay renders instead of
                // waiting forever.
                WorkspaceInput::EngineConfigSave { state, .. } => {
                    let _ = react_tx.send(Inbound::EngineConfig {
                        state,
                        status: Some("demo: config accepted (not persisted)".to_string()),
                    });
                }
                WorkspaceInput::EngineConfigRefresh => {
                    let _ = react_tx.send(Inbound::EngineConfig {
                        state: EngineConfigState::default(),
                        status: Some("the demo has no settings on disk".to_string()),
                    });
                }
                WorkspaceInput::Quit => break,
            }
        }
    });

    // The original sender is dropped; the two task-held clones keep the inbound
    // stream open for the life of the session.
    drop(in_tx);

    let opts = DeckOptions {
        initial_graph: Some(demo_graph()),
        slash_commands: vec![
            SlashCommand::new("/help", "show the key legend"),
            SlashCommand::new("/models", "list available models"),
            SlashCommand::new("/diff", "open the diff for the selected file"),
            SlashCommand::new("/files", "jump to the Files tab"),
            SlashCommand::new("/clear", "clear the focused transcript"),
        ],
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
            speculated: false,
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
            cache_write_tokens: 0,
            estimated_input_tokens: 3_900,
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
