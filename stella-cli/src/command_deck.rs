//! The Command Deck session — `stella` chat on a TTY.
//!
//! This is the bridge between the real engine stack (provider, tools, budget,
//! store, memory — everything `agent::run_interactive` assembles) and the
//! multi-tab deck in `stella-tui` (`run_deck`): engine `AgentEvent`s are
//! wrapped as `Inbound::Event`s for the deck's fold, and the deck's
//! [`WorkspaceInput`]s drive a lead-agent conversation loop.
//!
//! ## Shape
//!
//! One session = one **lead agent** (`"lead"`) holding one conversation, plus
//! a FIFO prompt queue. The deck's contract is "input never blocks": a prompt
//! submitted while a turn is in flight is queued (the deck shows it in the
//! status bar) and dispatched when the turn finishes — [`Inbound::PromptStarted`]
//! pops the deck's queue display at that moment. Multi-agent fan-out stays a
//! supervisor seam (`COMMAND_DECK_DESIGN.md` → "Backend seams"): the deck
//! already folds N agents, but this driver deliberately runs one until
//! `stella-fleet` grows a real spawn/abort API.
//!
//! ## The three engine seams handled here
//!
//! - **ask_user** ([`DeckAskUserIo`]): the plain REPL reads stdin, which raw
//!   mode owns in deck mode. The deck io emits its own `AskUser` card, waits
//!   for the deck's `AskUserAnswer`, then echoes the answer back as that
//!   card's `ToolResult` — the documented event-pure path that clears the
//!   pending gate (`stella_tui::model`).
//! - **File changes** ([`FileChangeTap`]): the engine emits no `FileChange`
//!   events today (the plain renderer reads the registry ledger after the
//!   turn). The tap wraps the tool stack and synthesizes `FileChange`s — with
//!   pseudo-diffs built from the tool inputs — when a mutating file tool
//!   succeeds, so the Files tab and diff panel are live during the turn.
//! - **Cancel** (`Stop` / `UserInput::Cancel`): the engine has no abort input;
//!   cancelling drops the in-flight turn future at its next await point and
//!   truncates the partial turn out of the conversation so the next prompt
//!   starts from the last committed state. Never a mid-await corruption — the
//!   dropped future takes its channel senders with it and the forwarder
//!   drains what was already emitted. After a plain cancel the loop pops the
//!   next queued prompt as usual ("interrupt current, run next" — the deck's
//!   single Esc). A double-Esc `StopAndHold` is the same clean cancel plus
//!   queue discipline: the interrupted prompt returns to the FRONT of the
//!   backlog and dispatch parks until the user's next submission, which
//!   arrives as `EnqueueFront` and runs ahead of it. The pair reaches the
//!   driver as two FIFO messages — the plain `Stop`, then the escalation —
//!   so the first press has always dropped the turn (and would have
//!   forgotten its prompt) before `StopAndHold` is read: [`HoldState`]
//!   retains what that cancel dropped so the second press still has a
//!   prompt to requeue and park.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use stella_core::ports::ToolExecutor;
use stella_core::router::{CircuitBreaker, ProviderProfile};
use stella_core::{BudgetGuard, CalibrationMap, Engine, RoleTable, Router, TurnOutcome};
use stella_model::provider::Provider;
use stella_pipeline::{
    AutoApproveGate, ContextRecallPort, NoContextRecall, Pipeline, PipelineConfig, PipelinePorts,
    PipelineStatus, ProviderResolver,
};
use stella_protocol::{
    AgentEvent, CompletionMessage, CompletionRequest, FileChangeKind, ModelRef, ToolOutput,
    ToolSchema,
};
use stella_store::Store;
use stella_tools::ToolRegistry;
use stella_tools::custom::{CustomTool, CustomToolSet};
use stella_tools::hook_runner::ShellHookRunner;
use stella_tui::{
    AgentMeta, AgentScope, AgentStatus, DeckOptions, Inbound, SlashCommand, UserInput,
    WorkspaceInput, run_deck,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::agent;
use crate::config::Config;
use crate::interactive::{AskUserIo, FREE_TEXT_LABEL, InteractiveToolSet, SkillRegistry};
use crate::memory::{SessionMemory, inject_recall_block, turn_warrants_reflection};
use crate::runtime::{SystemClock, TokioSleeper};

/// The lead agent's id — the one conversation this driver runs.
const LEAD: &str = "lead";

/// Ids for the cards [`DeckAskUserIo`] mints (`deck-ask-N`). Process-unique
/// like `interactive::NEXT_ASK_ID`, and deliberately a different namespace:
/// the deck io's card must be cleared by the deck io's own echoed
/// `ToolResult`, never by an unrelated result.
static NEXT_DECK_ASK: AtomicU64 = AtomicU64::new(0);

/// Cap on the lines a synthesized pseudo-diff retains per side — a whole-file
/// write must not balloon the event log the deck folds.
const PSEUDO_DIFF_MAX_LINES: usize = 200;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `OXAGEN_DEBUG=1` → the structured deck log path (L-T8), mirroring the
/// location `stella_tui::shell::RunOptions` documents. `None` otherwise, and
/// on any failure to create the directory — a lost debug log never gates the
/// session.
fn debug_log_path() -> Option<PathBuf> {
    if std::env::var_os("OXAGEN_DEBUG").is_none_or(|v| v.is_empty() || v == "0") {
        return None;
    }
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    let dir = state_home.join("stella").join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("deck-{}.jsonl", std::process::id())))
}

/// How one dispatched turn ended, as seen by the driver loop.
enum TurnEnd {
    /// The turn future resolved (completed or aborted-with-reason).
    Finished(Result<(), String>),
    /// The user stopped it mid-flight; the future was dropped. `hold` is the
    /// double-Esc variant: the interrupted prompt goes back to the FRONT of
    /// the backlog and dispatch parks until the user's next submission
    /// (which runs ahead of it). A plain cancel (`hold: false`) lets the
    /// loop auto-dispatch the next queued prompt as usual.
    Cancelled { hold: bool },
    /// The deck is going down; stop driving entirely.
    Quit,
}

/// Driver-side bookkeeping for the deck's Esc pair: single Esc cancels now,
/// double-Esc escalates to "requeue what was interrupted and park dispatch".
///
/// The two presses arrive as two FIFO messages — `AgentControl::Stop`, then
/// `WorkspaceInput::StopAndHold` — and the driver consumes the first by
/// dropping the turn future. Without retention the escalation would always
/// land after its target was already cancelled and forgotten: with an empty
/// backlog it would be a silent no-op (no requeue, no hold — while the deck's
/// own `dispatch_held` flag believes otherwise), and with a backlog it would
/// cancel the freshly auto-dispatched NEXT prompt while the prompt the user
/// actually interrupted stayed lost. So every plain cancel deposits its
/// prompt here, and the escalation requeues it whenever it lands.
struct HoldState {
    /// While set, dispatch is parked: the loop waits for the user's next
    /// submission instead of popping the backlog.
    held: bool,
    /// The prompt the last plain cancel dropped, kept until the pair's
    /// escalation consumes it or the next plain cancel replaces it. Never
    /// stale: every `StopAndHold` the deck can emit is preceded — same pair,
    /// no keys in between — by a `Stop` that overwrites this slot.
    cancelled: Option<String>,
}

impl HoldState {
    fn new() -> Self {
        Self {
            held: false,
            cancelled: None,
        }
    }

    /// Whether dispatch is parked (the loop must not pop the backlog).
    fn held(&self) -> bool {
        self.held
    }

    /// A user submission releases the hold and runs immediately.
    fn release(&mut self) {
        self.held = false;
    }

    /// A plain cancel (single Esc / dashboard stop): retain the dropped
    /// prompt so a following escalation can still requeue it.
    fn cancelled(&mut self, submitted: &str) {
        self.cancelled = Some(submitted.to_string());
    }

    /// The double-Esc escalation: park dispatch and return the prompts to
    /// push to the FRONT of the backlog, in push order (front-most last).
    /// `in_flight` is the auto-dispatched prompt this escalation itself
    /// cancelled (if any); it lands BEHIND the retained one so the backlog
    /// reads exactly as the user last saw it. With nothing in flight and
    /// nothing retained there is nothing to hold — a stray escalation stays
    /// a no-op.
    fn stop_and_hold(&mut self, in_flight: Option<&str>) -> Vec<String> {
        let requeue: Vec<String> = in_flight
            .map(str::to_string)
            .into_iter()
            .chain(self.cancelled.take())
            .collect();
        if !requeue.is_empty() {
            self.held = true;
        }
        requeue
    }
}

/// Return cancelled prompts to the FRONT of the backlog (push order:
/// front-most last) and mirror each front-insert into the deck's queue view
/// (`Inbound::PromptRequeued` is the exact inverse of `PromptStarted`'s
/// front-pop), so the two queues never drift.
fn requeue_front(
    queue: &mut VecDeque<String>,
    in_tx: &UnboundedSender<Inbound>,
    texts: Vec<String>,
) {
    for text in texts {
        queue.push_front(text.clone());
        let _ = in_tx.send(Inbound::PromptRequeued {
            agent: LEAD.to_string(),
            text,
        });
    }
}

/// Run a full deck session: the deck shell on its own task, the engine
/// driver inline. Returns when the user quits (Ctrl-C) or the deck's input
/// stream ends.
pub async fn run_deck_session(
    cfg: &Config,
    budget_limit: Option<f64>,
    no_anim: bool,
) -> Result<(), String> {
    // ── Session assembly (still on the normal screen — prints are fine) ────
    // MCP connect is NOT here: it can block up to 10s per server, so it runs
    // after the deck task spawns, narrated as transcript events (#98).
    let provider = agent::build_provider(cfg)?;
    let registry: Arc<ToolRegistry> =
        Arc::new(ToolRegistry::new_detected(cfg.workspace_root.clone()).await);
    agent::populate_schema_index(&registry, &cfg.workspace_root);
    crate::rules::enforce_workspace_rules(&registry, &cfg.workspace_root);
    let custom_tools = agent::discover_custom_tools(cfg, true).await;
    let mut budget = agent::build_budget_guard(budget_limit);
    let store = agent::open_store(&cfg.workspace_root);
    let calibration = agent::seed_calibration(&store, cfg);

    let system_prompt =
        agent::with_session_hook_context(agent::build_system_prompt(&cfg.workspace_root), cfg)
            .await;
    let mut messages = vec![CompletionMessage::system(system_prompt.clone())];
    // `warn: false`: past this point diagnostics would land on the alternate
    // screen; a memory-less session degrades silently here.
    let mut memory = SessionMemory::open(&cfg.workspace_root, false);
    // Custom extensions: ⚡ commands/skills in the slash menu, custom agents
    // behind `/agents`. Reloaded after `/init`, which may adopt new ones.
    let mut custom = crate::extensions::CustomExtensions::load(&cfg.workspace_root);

    // ── Channels: engine → deck (Inbound) and deck → driver (WorkspaceInput)
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Inbound>();
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<WorkspaceInput>();
    let (ask_tx, ask_rx) = mpsc::unbounded_channel::<String>();

    let title = cfg
        .workspace_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();
    let mut lead_meta = AgentMeta::new(LEAD, title, now_ms())
        .with_role("lead")
        .with_pid(std::process::id());
    lead_meta.model = Some(format!("{}/{}", cfg.provider.id, cfg.model_id));
    let _ = in_tx.send(Inbound::Register(lead_meta));
    // Custom definitions that failed to load are reported into the
    // transcript up front — stdout belongs to the alternate screen, and a
    // silently-missing /command is otherwise undiagnosable.
    if let Some(report) = custom.problems_report() {
        let _ = in_tx.send(Inbound::Event {
            agent: LEAD.to_string(),
            event: AgentEvent::Text { delta: report },
        });
    }
    // An idle lead is waiting on the human, not queued behind a supervisor
    // (sent after the problems report — a Text event folds to `Running`).
    let _ = in_tx.send(Inbound::Status {
        agent: LEAD.to_string(),
        status: AgentStatus::WaitingInput,
    });

    let ask_io = DeckAskUserIo {
        agent: LEAD.to_string(),
        inbound: in_tx.clone(),
        answers: Arc::new(tokio::sync::Mutex::new(ask_rx)),
    };

    let opts = DeckOptions {
        debug_log_path: debug_log_path(),
        slash_commands: deck_slash_commands(&custom),
        initial_graph: agent::graph_snapshot(&cfg.workspace_root),
        no_anim,
        // The deck drives turns through the raw `Engine::run_turn` path (see
        // `run_lead_turn`), not the staged pipeline, so PIPELINE reads OFF here.
        pipeline: false,
        ..Default::default()
    };
    // The deck owns its channel ends and runs on its own task so rendering
    // never waits on the driver (and vice versa).
    let deck = tokio::spawn(run_deck(opts, in_rx, sub_tx));

    // ── MCP connect, behind the live deck (#98) ─────────────────────────────
    // This await used to run during assembly, before the deck spawned — a
    // slow or unreachable server meant a blank terminal for up to the 10s
    // per-server timeout. The deck is up now, so its splash absorbs the wait
    // and the attempt is narrated in the transcript. Failure semantics are
    // the plain REPL's: the session continues on native tools only. Prompts
    // submitted while this await runs are never lost — the deck's input
    // never blocks, and everything it sends buffers in `sub_rx` until the
    // driver loop below starts reading.
    let mcp = match agent::load_mcp_plan(cfg) {
        agent::McpPlan::None => None,
        agent::McpPlan::Invalid(reason) => {
            let _ = in_tx.send(Inbound::Event {
                agent: LEAD.to_string(),
                event: AgentEvent::Text { delta: reason },
            });
            let _ = in_tx.send(Inbound::Status {
                agent: LEAD.to_string(),
                status: AgentStatus::WaitingInput,
            });
            None
        }
        agent::McpPlan::Servers(servers) => {
            let _ = in_tx.send(Inbound::Event {
                agent: LEAD.to_string(),
                event: AgentEvent::Text {
                    delta: format!("connecting {} MCP server(s)…", servers.len()),
                },
            });
            let set = agent::connect_mcp_servers(&servers, registry.clone()).await;
            let _ = in_tx.send(Inbound::Event {
                agent: LEAD.to_string(),
                event: AgentEvent::Text {
                    delta: mcp_outcome_report(&set.connected_names(), set.failed_servers()),
                },
            });
            // The Text events above fold the lead to `Running`, but no turn
            // is in flight — restore the idle status or the dashboard would
            // show a busy lead forever.
            let _ = in_tx.send(Inbound::Status {
                agent: LEAD.to_string(),
                status: AgentStatus::WaitingInput,
            });
            Some(set)
        }
    };
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set,
        None => &*registry,
    };

    // ── The driver loop ─────────────────────────────────────────────────────
    let mut queue: VecDeque<String> = VecDeque::new();
    // Double-Esc bookkeeping: parks dispatch and retains what the pair's
    // first press cancelled (see [`HoldState`]).
    let mut dispatch = HoldState::new();
    // `/pipeline`: route lead turns through the staged pipeline (triage →
    // witness → execute → verify → judge) instead of the raw engine loop.
    // Session-local, OFF at start — mirrored to the PIPELINE stat box via
    // `Inbound::Pipeline`.
    let mut pipeline_on = false;
    // An agent-creation request that arrived mid-turn: drafting needs the
    // provider (borrowed by the running turn), so it parks here and runs
    // right after the turn settles.
    let mut pending_create: Option<(String, AgentScope)> = None;
    'session: loop {
        // Take the next prompt: backlog first (unless held), else wait for
        // deck input.
        let next = if dispatch.held() {
            None
        } else {
            queue.pop_front()
        };
        let prompt = match next {
            Some(text) => text,
            None => match sub_rx.recv().await {
                None => break 'session,
                Some(WorkspaceInput::Quit) => break 'session,
                // Any submission releases a hold and runs NOW — ahead of the
                // parked backlog. `EnqueueFront` is the deck's explicit
                // front-insert (sent while it knows dispatch is held); a
                // plain `Enqueue` behaves identically here because running
                // the text immediately IS the front of the queue.
                Some(WorkspaceInput::Enqueue { text })
                | Some(WorkspaceInput::EnqueueFront { text })
                | Some(WorkspaceInput::ToAgent {
                    input: UserInput::Prompt { text },
                    ..
                }) => {
                    dispatch.release();
                    text
                }
                // While a hold parks a non-empty backlog at this recv, the
                // user can still edit it from the queue popup — mirror the
                // edits exactly like the in-turn arm does. (Before holds
                // existed the queue was always empty by the time this recv
                // ran, so these inputs had nothing to act on here.)
                Some(WorkspaceInput::QueueRemove { index }) => {
                    if index < queue.len() {
                        queue.remove(index);
                    }
                    continue 'session;
                }
                Some(WorkspaceInput::QueueClear) => {
                    queue.clear();
                    continue 'session;
                }
                // The double-Esc escalation, landing AFTER its pair's plain
                // `Stop` already dropped the turn — with an empty backlog
                // this recv is exactly where it lands (the channel is FIFO,
                // so the escalation can never reach the turn the pair
                // targeted). Requeue what that cancel dropped and park
                // dispatch; with nothing retained there is nothing to hold
                // and a stray escalation stays a no-op.
                Some(WorkspaceInput::StopAndHold { .. }) => {
                    requeue_front(&mut queue, &in_tx, dispatch.stop_and_hold(None));
                    continue 'session;
                }
                // The Graph tab's file picker asked to re-root on a file:
                // requery its neighborhood and push a fresh snapshot back, the
                // same out-of-band refresh `/init` uses. The loop is idle here,
                // so the read runs inline.
                Some(WorkspaceInput::FocusGraphFile { file }) => {
                    if let Some(snapshot) =
                        agent::graph_snapshot_focus(&cfg.workspace_root, Some(&file))
                    {
                        let _ = in_tx.send(Inbound::GraphSnapshot(snapshot));
                    }
                    continue 'session;
                }
                // A stray answer/decision/control with no turn in flight has
                // nothing to act on.
                Some(_) => continue 'session,
                // LLM-assisted agent creation needs the provider, which is
                // free here (no turn in flight) — draft, install, refresh.
                Some(WorkspaceInput::AgentCreate { description, scope }) => {
                    handle_agent_create(&description, scope, cfg, &*provider, &in_tx).await;
                    continue 'session;
                }
                // The INSTALLED AGENTS pane's synchronous ops (refresh /
                // save / pin) are pure filesystem work — the shared helper
                // serves this idle site and the in-turn site alike. A stray
                // answer/decision/control with no turn in flight falls
                // through it with nothing to act on.
                Some(other) => {
                    handle_agents_input(&other, cfg, &in_tx);
                    continue 'session;
                }
            },
        };

        let _ = in_tx.send(Inbound::PromptStarted {
            agent: LEAD.to_string(),
            text: prompt.clone(),
        });
        // What the user actually submitted — a hold-cancel returns THIS to
        // the queue, not the expansion a custom command may rewrite `prompt`
        // into below (re-dispatching it re-expands).
        let submitted = prompt.clone();

        // Session-level slash commands are the driver's, never the model's —
        // the deck's popup enqueues them like any prompt (tab switches and
        // the help overlay were already handled TUI-side and never reach us).
        let command = run_deck_command(
            &prompt,
            &in_tx,
            &mut messages,
            &system_prompt,
            &*provider,
            &registry,
            cfg,
            &custom,
            &mut pipeline_on,
        )
        .await;
        if matches!(command, DeckCommand::Handled | DeckCommand::InitCompleted) {
            // A handled command emits its answer as `Text`, which flips the
            // lead to `Running` in the deck's fold — but no turn is in flight.
            // Return it to `WaitingInput` so the dashboard reflects reality.
            let _ = in_tx.send(Inbound::Status {
                agent: LEAD.to_string(),
                status: AgentStatus::WaitingInput,
            });
        }
        let prompt = match command {
            DeckCommand::Prompt => prompt,
            // A custom command/skill invocation: the transcript already shows
            // what was typed (`PromptStarted` above); the model runs the
            // expanded template.
            DeckCommand::Expanded(text) => text,
            DeckCommand::Handled => continue 'session,
            DeckCommand::InitCompleted => {
                // `/init` changed the taxonomy and rebuilt the index. Re-open
                // memory so recall/reflection use the new domains this session
                // (not just the next), and push a fresh Graph-tab snapshot.
                memory = SessionMemory::open(&cfg.workspace_root, false);
                if let Some(snapshot) = agent::graph_snapshot(&cfg.workspace_root) {
                    let _ = in_tx.send(Inbound::GraphSnapshot(snapshot));
                }
                // `/init` may also have adopted new custom commands/skills —
                // reload them and refresh the deck's slash menu in place,
                // reporting anything that failed to load (then restoring the
                // idle status the report's Text event flipped).
                custom = crate::extensions::CustomExtensions::load(&cfg.workspace_root);
                let _ = in_tx.send(Inbound::SlashCommands(deck_slash_commands(&custom)));
                if let Some(report) = custom.problems_report() {
                    let _ = in_tx.send(Inbound::Event {
                        agent: LEAD.to_string(),
                        event: AgentEvent::Text { delta: report },
                    });
                    let _ = in_tx.send(Inbound::Status {
                        agent: LEAD.to_string(),
                        status: AgentStatus::WaitingInput,
                    });
                }
                continue 'session;
            }
        };

        // Per-turn conversation bookkeeping, mirroring `run_interactive`:
        // refresh the volatile recall block, then append the user prompt.
        // `turn_base` is the truncation point that erases the whole turn if
        // it is cancelled; `reflect_start` scopes the reflection gate to what
        // the turn itself appends. In pipeline mode the pipeline owns BOTH —
        // recall rides inside its one volatile recall+goal message (L-E8), so
        // the driver appending either would double them.
        if !pipeline_on && let Some(m) = &memory {
            let block = m.recall_block(&prompt).await;
            inject_recall_block(&mut messages, block);
        }
        let turn_base = messages.len();
        if !pipeline_on {
            messages.push(CompletionMessage::user(&prompt));
        }
        let reflect_start = messages.len();

        // The execution record outlives the turn future so a cancelled turn
        // can still be closed out in the store.
        let execution = agent::begin_execution(
            &store,
            if pipeline_on { "deck-pipeline" } else { "deck" },
            &prompt,
            cfg,
        );
        let files_before = registry.files_touched().len();
        let started_unix = crate::memory::unix_now_secs();

        let end = {
            // Both arms return `Result<(), String>`, so one pinned future
            // drives either path through the same select loop.
            let turn = async {
                if pipeline_on {
                    run_lead_pipeline_turn(
                        &*provider,
                        base_tools,
                        &custom_tools,
                        &registry,
                        memory.as_ref(),
                        &prompt,
                        &mut messages,
                        &mut budget,
                        cfg,
                        execution.clone(),
                        &in_tx,
                        &ask_io,
                    )
                    .await
                } else {
                    run_lead_turn(
                        &*provider,
                        base_tools,
                        &custom_tools,
                        &registry,
                        &mut messages,
                        &mut budget,
                        &calibration,
                        cfg,
                        execution.clone(),
                        &in_tx,
                        &ask_io,
                    )
                    .await
                }
            };
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    outcome = &mut turn => break TurnEnd::Finished(outcome),
                    input = sub_rx.recv() => match input {
                        None | Some(WorkspaceInput::Quit) => break TurnEnd::Quit,
                        Some(WorkspaceInput::Enqueue { text })
                        | Some(WorkspaceInput::ToAgent {
                            input: UserInput::Prompt { text }, ..
                        }) => queue.push_back(text),
                        // An explicit front-insert stays a front-insert even
                        // if a turn started before it arrived — the deck's
                        // queue view already shows it first.
                        Some(WorkspaceInput::EnqueueFront { text }) => queue.push_front(text),
                        // The deck's queue editor mutates its own view of the
                        // backlog and mirrors each edit here so the dispatch
                        // queue never drifts from what the user is looking at.
                        Some(WorkspaceInput::QueueRemove { index }) => {
                            if index < queue.len() {
                                queue.remove(index);
                            }
                        }
                        Some(WorkspaceInput::QueueClear) => queue.clear(),
                        Some(WorkspaceInput::ToAgent {
                            input: UserInput::AskUserAnswer { answer, .. }, ..
                        }) => {
                            let _ = ask_tx.send(answer);
                        }
                        Some(WorkspaceInput::ToAgent { input: UserInput::Cancel, .. })
                        | Some(WorkspaceInput::Control {
                            control: stella_tui::AgentControl::Stop, ..
                        }) => break TurnEnd::Cancelled { hold: false },
                        // Double-Esc: cancel AND park dispatch — the
                        // interrupted prompt returns to the front of the
                        // backlog and the user's next submission runs first.
                        Some(WorkspaceInput::StopAndHold { .. }) => {
                            break TurnEnd::Cancelled { hold: true }
                        }
                        // The Graph tab's file picker can re-root mid-turn (a
                        // user browsing the graph while an agent works). The
                        // requery opens SQLite + loads grammars, so run it on
                        // the blocking pool rather than stalling this event
                        // pump; it sends the fresh snapshot back when done.
                        Some(WorkspaceInput::FocusGraphFile { file }) => {
                            let tx = in_tx.clone();
                            let root = cfg.workspace_root.clone();
                            tokio::task::spawn_blocking(move || {
                                if let Some(snapshot) =
                                    agent::graph_snapshot_focus(&root, Some(&file))
                                {
                                    let _ = tx.send(Inbound::GraphSnapshot(snapshot));
                                }
                            });
                        // The INSTALLED AGENTS pane stays live while a turn
                        // runs — refresh / save / pin are pure filesystem
                        // ops, the same shared helper as the idle recv site.
                        Some(
                            input @ (WorkspaceInput::AgentsRefresh
                            | WorkspaceInput::AgentSave { .. }
                            | WorkspaceInput::AgentPin { .. }),
                        ) => {
                            handle_agents_input(&input, cfg, &in_tx);
                        }
                        // Creation needs the provider, which the running
                        // turn is borrowing — park it; it runs the moment
                        // the turn settles (see `pending_create`).
                        Some(WorkspaceInput::AgentCreate { description, scope }) => {
                            pending_create = Some((description, scope));
                            let _ = in_tx.send(agents_list_inbound(
                                &cfg.workspace_root,
                                Some(
                                    "agent creation queued — it runs when the current turn \
                                     finishes"
                                        .to_string(),
                                ),
                            ));
                        }
                        // Scope review is not engine-driven yet, and deep
                        // pause/resume/restart need the fleet supervisor —
                        // both named seams, both no-ops here.
                        Some(WorkspaceInput::ToAgent {
                            input: UserInput::ScopeDecision(_), ..
                        })
                        | Some(WorkspaceInput::Control { .. }) => {}
                    },
                }
            }
            // `turn` (and the channel senders it holds) drops here.
        };

        match end {
            TurnEnd::Finished(outcome) => {
                if let Err(reason) = &outcome {
                    // An aborted turn emits no `Complete`; this row flips the
                    // dashboard to failed AND clears any pending gate.
                    let _ = in_tx.send(Inbound::Event {
                        agent: LEAD.to_string(),
                        event: AgentEvent::Error {
                            message: reason.clone(),
                            retryable: false,
                        },
                    });
                }
                agent::record_turn_episode(
                    &memory,
                    &prompt,
                    &outcome,
                    &registry,
                    files_before,
                    started_unix,
                    &messages[reflect_start..],
                )
                .await;
                if outcome.is_ok()
                    && turn_warrants_reflection(&messages[reflect_start..])
                    && let Some(m) = &mut memory
                {
                    m.reflect_and_record(&*provider, &messages, true).await;
                }
            }
            TurnEnd::Cancelled { hold } => {
                // Erase the partial turn: the next prompt continues from the
                // last committed conversation state.
                messages.truncate(turn_base);
                if hold {
                    // Double-Esc landing mid-turn: this turn is the NEXT
                    // prompt, auto-dispatched in the gap between the pair's
                    // two messages. Park dispatch and return both to the
                    // FRONT of the backlog — the retained prompt (the one
                    // the pair's first press cancelled) ahead of this one,
                    // restoring the order the user last saw. The next
                    // submission will run ahead of them all.
                    requeue_front(&mut queue, &in_tx, dispatch.stop_and_hold(Some(&submitted)));
                } else {
                    // A plain cancel: retain the dropped prompt so the
                    // pair's escalation — which always arrives after this
                    // point (the channel is FIFO) — can still requeue it.
                    dispatch.cancelled(&submitted);
                }
                if let Some((store, id)) = &execution
                    && store.finish_execution(*id, "cancelled", 0.0).is_err()
                {
                    let _ = in_tx.send(Inbound::Event {
                        agent: LEAD.to_string(),
                        event: AgentEvent::Error {
                            message: "store write failed — this cancelled execution was not \
                                      recorded"
                                .to_string(),
                            retryable: true,
                        },
                    });
                }
                // Must stay AFTER the store warning above: the warning is
                // retryable (folds to Running) while this one is not (folds
                // to Failed), so this event is what leaves the lead in a
                // terminal state on the dashboard.
                let _ = in_tx.send(Inbound::Event {
                    agent: LEAD.to_string(),
                    event: AgentEvent::Error {
                        message: "turn stopped by user".to_string(),
                        retryable: false,
                    },
                });
            }
            TurnEnd::Quit => break 'session,
        }

        // A creation request parked during the turn: the provider is free
        // again, so draft + install it before the next dispatch.
        if let Some((description, scope)) = pending_create.take() {
            handle_agent_create(&description, scope, cfg, &*provider, &in_tx).await;
        }
    }

    // Closing our inbound sender ends the deck's stream if the user hasn't
    // already quit; then wait for it to restore the terminal.
    drop(in_tx);
    let deck_result = deck.await;
    if let Some(set) = &mcp {
        set.close_all().await;
    }
    match deck_result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("deck terminal error: {e}")),
        Err(e) => Err(format!("deck task failed: {e}")),
    }
}

/// The transcript report for a finished MCP connect attempt: the connected
/// servers by name, then one row per failure with its reason — the deck-mode
/// analogue of the diagnostics [`agent::connect_mcp`] prints in the plain
/// REPL. Zero connections is stated outright: the degraded session must be
/// visible in the transcript, never inferred from silence.
fn mcp_outcome_report(connected: &[&str], failed: &[(String, String)]) -> String {
    let mut lines = Vec::new();
    match connected.len() {
        0 => lines.push("no MCP servers connected — continuing with native tools only".to_string()),
        n => lines.push(format!(
            "{n} MCP server(s) connected: {}",
            connected.join(", ")
        )),
    }
    for (name, reason) in failed {
        lines.push(format!("MCP server `{name}` unavailable: {reason}"));
    }
    lines.join("\n")
}

/// The disposition of a would-be slash command.
enum DeckCommand {
    /// Not a command — run the model turn as usual.
    Prompt,
    /// A custom command/skill invocation — run the model turn with this
    /// expanded prompt instead of the raw `/name args` input.
    Expanded(String),
    /// Handled as a command; skip the model turn.
    Handled,
    /// `/init` finished successfully; skip the turn AND refresh the session's
    /// derived state (memory domains, Graph tab, custom extensions) which the
    /// new taxonomy/index changed.
    InitCompleted,
}

/// The deck's productized vocabulary, as `(name, menu description)`. One
/// source of truth for the menu's 🔒 rows and the reserved-name guard: a
/// custom definition can never run under one of these names — not from the
/// menu (`slash_entries` drops it) and not typed with arguments either
/// (`expand` refuses reserved heads).
const DECK_BUILTINS: &[(&str, &str)] = &[
    ("/help", "show commands"),
    ("/clear", "reset the conversation"),
    ("/models", "list providers & models"),
    ("/init", "index the workspace: domains + code graph"),
    (
        "/agents",
        "open the Agents tab: executions & installed agents",
    ),
    (
        "/pipeline",
        "toggle the staged pipeline (witness-verified turns)",
    ),
    ("/files", "open the Files tab"),
    ("/diff", "open the diff viewer"),
    ("/graph", "open the code-graph tab"),
];

/// The deck's reserved command names — see [`DECK_BUILTINS`].
fn deck_reserved() -> Vec<&'static str> {
    DECK_BUILTINS.iter().map(|(name, _)| *name).collect()
}

// ── Installed-agents manager (the AGENTS tab's INSTALLED AGENTS pane) ───────

/// Build an [`Inbound::AgentsList`] from the definitions on disk at both
/// scopes. `status`, when set, replaces the pane's hint line.
fn agents_list_inbound(workspace_root: &std::path::Path, status: Option<String>) -> Inbound {
    let project = crate::agents_installed::project_agents_dir(workspace_root);
    let user = crate::agents_installed::user_agents_dir();
    Inbound::AgentsList {
        entries: crate::agents_installed::discover(user.as_deref(), &project),
        status,
    }
}

/// Handle one synchronous installed-agents op (refresh / save / pin) —
/// pure filesystem work, answered with a fresh [`Inbound::AgentsList`].
/// Called from BOTH the idle and the in-turn recv sites, so the manager
/// works whether or not a turn is running. Returns `true` when the input
/// was one of the manager's; anything else is left to the caller's arms.
fn handle_agents_input(
    input: &WorkspaceInput,
    cfg: &Config,
    in_tx: &UnboundedSender<Inbound>,
) -> bool {
    let root = &cfg.workspace_root;
    match input {
        WorkspaceInput::AgentsRefresh => {
            let _ = in_tx.send(agents_list_inbound(root, None));
            true
        }
        WorkspaceInput::AgentSave {
            name,
            scope,
            content,
        } => {
            let status = save_agent(root, name, *scope, content);
            let _ = in_tx.send(agents_list_inbound(root, Some(status)));
            true
        }
        WorkspaceInput::AgentPin {
            name,
            scope,
            version,
        } => {
            let status = pin_agent(root, name, *scope, *version);
            let _ = in_tx.send(agents_list_inbound(root, Some(status)));
            true
        }
        _ => false,
    }
}

/// The edit-save path: archive-then-write a NEW version and pin it (see
/// `agents_installed::save_new_version`). Returns the pane's status line.
fn save_agent(root: &std::path::Path, name: &str, scope: AgentScope, content: &str) -> String {
    let dir = match crate::agents_installed::agents_dir_for(scope, root) {
        Ok(dir) => dir,
        Err(e) => return format!("save failed: {e}"),
    };
    let slug = crate::agents_installed::find_slug(&dir, name)
        .unwrap_or_else(|| crate::agents_installed::slugify(name));
    match crate::agents_installed::save_new_version(&dir, &slug, content) {
        Ok(version) => format!(
            "saved {name} — v{version} is now pinned (previous versions preserved under \
             .versions/{slug}/)"
        ),
        Err(e) => format!("save failed: {e}"),
    }
}

/// The pin-set path: re-point the pin at an existing version — never
/// creates one. Returns the pane's status line.
fn pin_agent(root: &std::path::Path, name: &str, scope: AgentScope, version: u32) -> String {
    let dir = match crate::agents_installed::agents_dir_for(scope, root) {
        Ok(dir) => dir,
        Err(e) => return format!("pin failed: {e}"),
    };
    let Some(slug) = crate::agents_installed::find_slug(&dir, name) else {
        return format!(
            "no installed agent named {name} at the {} scope",
            scope.label()
        );
    };
    match crate::agents_installed::pin_version(&dir, &slug, version) {
        Ok(()) => format!("{name} pinned to v{version} — no new version written"),
        Err(e) => format!("pin failed: {e}"),
    }
}

/// LLM-assisted create-from-prompt: draft the definition through the
/// session's provider (the same one-shot `Provider::complete` path the
/// reflection module uses — no hand-rolled HTTP), validate it with the real
/// loader parser, install it at `scope`, and answer with a fresh list.
async fn handle_agent_create(
    description: &str,
    scope: AgentScope,
    cfg: &Config,
    provider: &dyn Provider,
    in_tx: &UnboundedSender<Inbound>,
) {
    let status = match create_agent(description, scope, cfg, provider).await {
        Ok(status) => status,
        Err(e) => format!("agent creation failed: {e}"),
    };
    let _ = in_tx.send(agents_list_inbound(&cfg.workspace_root, Some(status)));
}

async fn create_agent(
    description: &str,
    scope: AgentScope,
    cfg: &Config,
    provider: &dyn Provider,
) -> Result<String, String> {
    let req = CompletionRequest {
        messages: crate::agents_installed::creation_messages(description),
        max_output_tokens: Some(1200),
        temperature: Some(0.2),
        effort: None,
        tools: vec![],
    };
    let result = provider
        .complete(req)
        .await
        .map_err(|e| format!("draft call failed: {e}"))?;
    let agent = crate::agents_installed::parse_generated_agent(&result.text)?;
    let dir = crate::agents_installed::agents_dir_for(scope, &cfg.workspace_root)?;
    let path = crate::agents_installed::install_new_agent(&dir, &agent)?;
    Ok(format!(
        "created {} ({} scope) at {} — v1 pinned",
        agent.name,
        scope.label(),
        path.display()
    ))
}

/// Cap on the free-text `reason` stamped on an agent-use telemetry row.
const AGENT_USE_REASON_MAX: usize = 120;

/// Record the agent-usage telemetry for a `/agent-name task…` invocation:
/// resolution mirrors `CustomExtensions::expand` (commands shadow skills
/// shadow agents — only a real agent invocation records), `version` is the
/// definition's pinned version at this moment, `reason` is the task
/// snippet. The row rides the registry's ledger and is drained into
/// store.db by `agent::record_execution_end` under the execution the
/// expanded prompt runs as.
fn record_agent_invocation(
    input: &str,
    custom: &crate::extensions::CustomExtensions,
    registry: &ToolRegistry,
) {
    let trimmed = input.trim();
    let (head, args) = match trimmed.split_once(char::is_whitespace) {
        Some((head, args)) => (head, args),
        None => (trimmed, ""),
    };
    if let Some(crate::extensions::Invocation::Agent(agent)) = custom.lookup(head) {
        let version = crate::agents_installed::active_version_for_source(&agent.source_path);
        let reason: String = args.trim().chars().take(AGENT_USE_REASON_MAX).collect();
        registry.record_agent_use(&agent.name, version, &reason);
    }
}

/// The deck's slash vocabulary: the productized commands (🔒) followed by
/// every custom command/skill (⚡) currently on disk. Rebuilt after `/init`
/// so just-adopted definitions appear without a restart.
fn deck_slash_commands(custom: &crate::extensions::CustomExtensions) -> Vec<SlashCommand> {
    let mut commands: Vec<SlashCommand> = DECK_BUILTINS
        .iter()
        .map(|(name, description)| SlashCommand::new(*name, *description))
        .collect();
    let customs = custom.slash_entries(&commands);
    commands.extend(customs);
    commands
}

/// Handle a session-level slash command. Output goes into the lead agent's
/// transcript as `Text` events — the deck renders exclusively from events, so
/// printing to stdout (which the alternate screen owns) is never an option.
///
/// Vocabulary: `/help`, `/clear`, `/models`, `/init`, `/agents`,
/// `/pipeline`. `/files`, `/diff`, `/graph` are deck-local (tab switches) and
/// consumed TUI-side; an unknown bare `/command` gets a hint rather than a
/// wasted model call. Every productized command is no-argument, so the
/// *whole* trimmed input is matched — `/init do the thing` is a model prompt,
/// not a silent reindex that discards the rest. Custom commands/skills (⚡)
/// DO take arguments: `/fix-bug issue-42` expands the `fix-bug` template
/// with `issue-42`.
#[allow(clippy::too_many_arguments)]
async fn run_deck_command(
    prompt: &str,
    in_tx: &UnboundedSender<Inbound>,
    messages: &mut Vec<CompletionMessage>,
    system_prompt: &str,
    provider: &dyn Provider,
    registry: &ToolRegistry,
    cfg: &Config,
    custom: &crate::extensions::CustomExtensions,
    pipeline_on: &mut bool,
) -> DeckCommand {
    let trimmed = prompt.trim();
    if !trimmed.starts_with('/') {
        return DeckCommand::Prompt;
    }
    let say = |text: String| {
        let _ = in_tx.send(Inbound::Event {
            agent: LEAD.to_string(),
            event: AgentEvent::Text { delta: text },
        });
    };
    match trimmed {
        "/help" => {
            let mut help = "commands: /help · /clear (reset conversation) · /models (list \
                 providers) · /init (index the workspace: domains + code graph) · /agents \
                 (open the Agents tab: executions & installed agents) · /pipeline (toggle \
                 witness-verified staged turns) · /files · /diff · /graph (switch tabs) — \
                 anything else is a prompt. ctrl+t queue · ? overlay help"
                .to_string();
            let customs = custom.slash_entries(&[]);
            if !customs.is_empty() {
                let names: Vec<&str> = customs.iter().map(|c| c.name.as_str()).collect();
                help.push_str(&format!("\ncustom (⚡): {}", names.join(" · ")));
            }
            say(help);
        }
        "/clear" => {
            messages.clear();
            messages.push(CompletionMessage::system(system_prompt.to_string()));
            say("conversation cleared".to_string());
        }
        "/models" => {
            say(Config::available_models_plain());
        }
        "/pipeline" => {
            *pipeline_on = !*pipeline_on;
            // Flip the PIPELINE stat box live — the deck renders exclusively
            // from inbound messages, never from driver state it can't see.
            let _ = in_tx.send(Inbound::Pipeline(*pipeline_on));
            say(if *pipeline_on {
                "staged pipeline ON — turns now run triage → recall → (plan → scope review) → \
                 witness → execute → verify → judge, with bounded revision. The witness stage \
                 authors a failing test that must flip to green before work counts as done; \
                 large plans auto-approve in the deck (scope review is narrated, not gated). \
                 `/pipeline` again to return to the raw engine loop."
                    .to_string()
            } else {
                "staged pipeline OFF — turns run the raw engine loop.".to_string()
            });
        }
        "/init" => {
            let mut emit = |line: String| say(line);
            match agent::init_workspace(Some(provider), &cfg.workspace_root, &mut emit).await {
                Ok(_) => {
                    // A fresh index may name tables/types the schema gate
                    // should know about this session, not just the next one.
                    agent::populate_schema_index(registry, &cfg.workspace_root);
                    // Expose the `code_graph` tool for the rest of the session
                    // now that the index exists (it is registered only when an
                    // index is present at construction).
                    registry.enable_code_graph_if_available(&cfg.workspace_root);
                    return DeckCommand::InitCompleted;
                }
                Err(e) => say(format!("init failed: {e}")),
            }
        }
        // Deck-local commands (tab switches, `/agents` opening the Agents
        // tab) are normally consumed TUI-side, but a queued one reaches
        // here — accept it as handled (a no-op) rather than calling it
        // "unknown".
        "/files" | "/diff" | "/graph" | "/agents" => {}
        _ => {
            // A custom command/skill/agent (⚡): expand its template —
            // arguments and all — into the prompt the model turn runs.
            // Reserved names never reach a custom definition (`/init do the
            // thing` stays a model prompt even if a custom `init` exists).
            // An AGENT invocation additionally records a usage-telemetry
            // row (agent, pinned version, task) on the registry's ledger.
            if let Some(expanded) = custom.expand(trimmed, &deck_reserved()) {
                record_agent_invocation(trimmed, custom, registry);
                return DeckCommand::Expanded(expanded);
            }
            // A bare unknown /word is a typo'd command, not a prompt — say so
            // instead of spending a model call. Anything with arguments (e.g.
            // `/src/main.rs explain`) falls through and stays a prompt.
            if trimmed.contains(char::is_whitespace) {
                return DeckCommand::Prompt;
            }
            say(format!(
                "unknown command `{trimmed}` — try /help, /clear, /models, /init, /agents, /pipeline, /files, /diff, /graph"
            ));
        }
    }
    DeckCommand::Handled
}

/// One engine turn for the lead agent: the deck-mode analogue of
/// [`agent::run_turn`] — same engine, same tool stack, same persistence —
/// with the stdout renderer replaced by [`spawn_forwarder`] and the tool
/// stack wrapped in the [`FileChangeTap`].
#[allow(clippy::too_many_arguments)]
async fn run_lead_turn(
    provider: &dyn Provider,
    base_tools: &dyn ToolExecutor,
    custom_tools: &[CustomTool],
    registry: &ToolRegistry,
    messages: &mut Vec<CompletionMessage>,
    budget: &mut BudgetGuard,
    calibration: &CalibrationMap,
    cfg: &Config,
    execution: Option<(Arc<Store>, i64)>,
    in_tx: &UnboundedSender<Inbound>,
    ask_io: &DeckAskUserIo,
) -> Result<(), String> {
    budget.begin_turn();

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let forwarder = spawn_forwarder(
        rx,
        execution.clone(),
        cfg.provider.id.to_string(),
        in_tx.clone(),
    );

    // Same structural drop-order rule as `agent::run_turn`: every tx clone
    // lives in this scope so dropping `tx` after it closes the channel.
    let outcome = {
        let customs = CustomToolSet::new(
            base_tools,
            custom_tools.to_vec(),
            cfg.workspace_root.clone(),
        );
        // The AskUser event channel is a stub: the deck io presents its own
        // card (it must — `install_skill` confirms through the io without any
        // event), so the tool set's own emission would double the card.
        let (stub_tx, _) = mpsc::unbounded_channel();
        let tools = InteractiveToolSet::new(&customs, stub_tx, Box::new(ask_io.clone()))
            .with_skill_registry(SkillRegistry::from_env(cfg.workspace_root.clone()));
        let tapped = FileChangeTap {
            inner: &tools,
            events: tx.clone(),
            root: cfg.workspace_root.clone(),
        };
        let hook_runner = ShellHookRunner;
        let mut engine = Engine::with_sleeper(
            provider,
            &tapped,
            agent::engine_config_for(cfg),
            &TokioSleeper,
        )
        .with_calibration(calibration);
        if let Some(hooks) = &cfg.hooks {
            engine = engine.with_hooks(hooks, &hook_runner);
        }
        engine.run_turn(messages, budget, &tx).await
    };
    drop(tx);
    let _ = forwarder.await;

    if let Some((store, id)) = &execution {
        let (outcome_label, cost) = match &outcome {
            TurnOutcome::Completed { cost_usd, .. } => ("completed", *cost_usd),
            TurnOutcome::Aborted { .. } => ("aborted", 0.0),
        };
        if !agent::record_execution_end(store, *id, registry, outcome_label, cost) {
            let _ = in_tx.send(Inbound::Event {
                agent: LEAD.to_string(),
                event: AgentEvent::Error {
                    message: "store write failed — the audit record (files touched / \
                              memory citations / outcome) for this execution is incomplete"
                        .to_string(),
                    retryable: true,
                },
            });
            // That warning lands AFTER the turn's Complete event, and the
            // deck's status fold maps a retryable Error back to Running — so
            // without this re-assert a finished turn would show as running
            // forever. Restate the turn's terminal status explicitly.
            let _ = in_tx.send(Inbound::Status {
                agent: LEAD.to_string(),
                status: match &outcome {
                    TurnOutcome::Completed { .. } => AgentStatus::Done,
                    TurnOutcome::Aborted { .. } => AgentStatus::Failed,
                },
            });
        }
    }

    match outcome {
        TurnOutcome::Completed { .. } => Ok(()),
        TurnOutcome::Aborted { reason } => Err(reason),
    }
}

/// Maps every role's resolved model to the deck's one borrowed provider. The
/// deck session is single-provider by construction (one `build_provider`
/// call), and its `Router` is built from one profile whose worker/triage/
/// judge refs all name that provider's model — so answering every query with
/// the one adapter is exact, not a fallback.
struct DeckProviderResolver<'p> {
    provider: &'p dyn Provider,
}

impl ProviderResolver for DeckProviderResolver<'_> {
    fn provider_for(&self, _model: &ModelRef) -> Option<&dyn Provider> {
        Some(self.provider)
    }
}

/// One staged-pipeline turn for the lead agent (`/pipeline` ON): the deck
/// analogue of the `stella run` pipeline path — same tool stack, persistence,
/// and event forwarding as [`run_lead_turn`], with `Pipeline::run` (triage →
/// recall → plan → scope → witness → execute → verify → judge → revise) in
/// place of the raw `Engine::run_turn`.
///
/// Deck-mode seams, all named:
/// - **Scope review auto-approves.** The deck cannot block a turn on a stdio
///   gate (the alternate screen owns the terminal), so `headless_bypass` is
///   set and the `ScopeReview` event is narrated, not gated — the same seam
///   the driver's `ScopeDecision` no-op documents. Deck-native scope review
///   is the fleet-supervisor follow-up.
/// - **The session's system prompt stays.** It was assembled once at deck
///   startup (byte-stable for the cache prefix, L-E8); toggling `/pipeline`
///   must not rewrite history. The pipeline's stage prompts (witness, judge,
///   planner) are its own regardless of the worker's system prompt.
/// - **Recall is the pipeline's port** (the workspace memory) — the driver
///   skips its own `inject_recall_block` for pipeline turns.
#[allow(clippy::too_many_arguments)]
async fn run_lead_pipeline_turn(
    provider: &dyn Provider,
    base_tools: &dyn ToolExecutor,
    custom_tools: &[CustomTool],
    registry: &ToolRegistry,
    memory: Option<&SessionMemory>,
    prompt: &str,
    messages: &mut Vec<CompletionMessage>,
    budget: &mut BudgetGuard,
    cfg: &Config,
    execution: Option<(Arc<Store>, i64)>,
    in_tx: &UnboundedSender<Inbound>,
    ask_io: &DeckAskUserIo,
) -> Result<(), String> {
    budget.begin_turn();

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let forwarder = spawn_forwarder(
        rx,
        execution.clone(),
        cfg.provider.id.to_string(),
        in_tx.clone(),
    );

    let result = {
        let customs = CustomToolSet::new(
            base_tools,
            custom_tools.to_vec(),
            cfg.workspace_root.clone(),
        );
        let (stub_tx, _) = mpsc::unbounded_channel();
        let tools = InteractiveToolSet::new(&customs, stub_tx, Box::new(ask_io.clone()))
            .with_skill_registry(SkillRegistry::from_env(cfg.workspace_root.clone()));
        let tapped = FileChangeTap {
            inner: &tools,
            events: tx.clone(),
            root: cfg.workspace_root.clone(),
        };

        let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());
        let resolver = DeckProviderResolver { provider };
        let profile = ProviderProfile::new(
            cfg.provider.id,
            model_ref.clone(),
            model_ref.clone(),
            model_ref,
        );
        let breaker = CircuitBreaker::new(Box::new(SystemClock::new()));
        let router = Router::new(RoleTable::new(), vec![profile], breaker);

        let repo_structure = agent::GitRepoStructure {
            root: cfg.workspace_root.clone(),
        };
        let repo_status = agent::GitRepoStatus {
            root: cfg.workspace_root.clone(),
        };
        let command_runner = agent::ShellCommandRunner {
            root: cfg.workspace_root.clone(),
        };
        let no_recall = NoContextRecall;
        let recall: &dyn ContextRecallPort = match memory {
            Some(m) => m,
            None => &no_recall,
        };
        let hook_runner = ShellHookRunner;
        let ports = PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tapped,
            recall,
            repo: &repo_structure,
            repo_status: &repo_status,
            commands: &command_runner,
            approvals: &AutoApproveGate,
            sleeper: &TokioSleeper,
            hooks: cfg
                .hooks
                .as_ref()
                .map(|h| (h, &hook_runner as &dyn stella_core::hooks::HookRunner)),
        };
        let config = PipelineConfig {
            engine: agent::engine_config_for(cfg),
            headless: true,
            headless_bypass_scope_review: true,
            ..PipelineConfig::default()
        };
        let pipeline = Pipeline::new(ports, tx.clone(), config);
        pipeline.run(prompt, messages, budget).await
    };
    drop(tx);
    let _ = forwarder.await;

    if let Some((store, id)) = &execution {
        let (outcome_label, cost) = match &result {
            Ok(outcome) => {
                let label = match outcome.status {
                    PipelineStatus::Completed => "completed",
                    PipelineStatus::Aborted { .. } => "aborted",
                };
                (label, outcome.total_cost_usd)
            }
            Err(_) => ("error", 0.0),
        };
        if !agent::record_execution_end(store, *id, registry, outcome_label, cost) {
            let _ = in_tx.send(Inbound::Event {
                agent: LEAD.to_string(),
                event: AgentEvent::Error {
                    message: "store write failed — the audit record (files touched / \
                              memory citations / outcome) for this execution is incomplete"
                        .to_string(),
                    retryable: true,
                },
            });
            // Same re-assert as run_lead_turn: the retryable warning above
            // folds the lead back to Running, so restate the terminal state.
            let _ = in_tx.send(Inbound::Status {
                agent: LEAD.to_string(),
                status: match &result {
                    Ok(outcome) if matches!(outcome.status, PipelineStatus::Completed) => {
                        AgentStatus::Done
                    }
                    _ => AgentStatus::Failed,
                },
            });
        }
    }

    match result {
        Ok(outcome) => match outcome.status {
            PipelineStatus::Completed => Ok(()),
            PipelineStatus::Aborted { reason } => Err(reason),
        },
        Err(e) => Err(e.to_string()),
    }
}

/// Drain one turn's engine events: persist each (via the shared
/// [`agent::persist_event`] write path) and forward it to the deck as the
/// lead agent's `Inbound::Event`. The deck-mode replacement for
/// [`agent::spawn_renderer`]. stderr belongs to the alternate screen here,
/// so a persistence failure warns *through the deck* instead — once — as a
/// transcript-visible error event; silently losing the audit trail (disk
/// full, DB locked) is not acceptable.
fn spawn_forwarder(
    mut rx: UnboundedReceiver<AgentEvent>,
    execution: Option<(Arc<Store>, i64)>,
    provider_id: String,
    inbound: UnboundedSender<Inbound>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seq = 0u64;
        let mut store_warned = false;
        while let Some(event) = rx.recv().await {
            if let Some((store, id)) = &execution {
                if !agent::persist_event(store, *id, seq, &event, &provider_id) && !store_warned {
                    store_warned = true;
                    let _ = inbound.send(Inbound::Event {
                        agent: LEAD.to_string(),
                        event: AgentEvent::Error {
                            message: "store write failed — the persisted event/telemetry \
                                      record for this session is incomplete"
                                .to_string(),
                            retryable: true,
                        },
                    });
                }
                seq += 1;
            }
            let _ = inbound.send(Inbound::Event {
                agent: LEAD.to_string(),
                event,
            });
        }
    })
}

// ── ask_user through the deck ───────────────────────────────────────────────

/// [`AskUserIo`] over the deck's channels. `prompt` emits an `AskUser` card,
/// awaits the user's `AskUserAnswer`, echoes the answer back as the card's
/// own `ToolResult` (the event-pure clear), and returns the answer in the
/// shape `interactive::execute_ask_user`'s parser expects: an exact option
/// match becomes its 1-based index (so `install_skill`'s "1 = yes" check and
/// ask_user's numeric quick-pick both work), anything else passes verbatim
/// as free text.
#[derive(Clone)]
struct DeckAskUserIo {
    agent: String,
    inbound: UnboundedSender<Inbound>,
    answers: Arc<tokio::sync::Mutex<UnboundedReceiver<String>>>,
}

#[async_trait]
impl AskUserIo for DeckAskUserIo {
    async fn prompt(&self, question: &str, options: &[String]) -> Result<String, String> {
        // `execute_ask_user` appends the free-text affordance before calling
        // us; the deck's card renders its own free-text affordance (Enter
        // submits the composer), so presenting the label as a pickable
        // option would double it — and picking it would return the label
        // itself as an "answer". Strip it; every other caller's options
        // (e.g. install_skill's yes/no) pass through untouched.
        let mut presented: Vec<String> = options.to_vec();
        if presented
            .last()
            .is_some_and(|o| o.starts_with(FREE_TEXT_LABEL))
        {
            presented.pop();
        }

        let id = format!("deck-ask-{}", NEXT_DECK_ASK.fetch_add(1, Ordering::Relaxed));
        let mut answers = self.answers.lock().await;
        // Drop answers stranded by a cancelled turn — they belong to a card
        // that no longer exists.
        while answers.try_recv().is_ok() {}

        let _ = self.inbound.send(Inbound::Event {
            agent: self.agent.clone(),
            event: AgentEvent::AskUser {
                id: id.clone(),
                question: question.to_string(),
                options: presented.clone(),
            },
        });

        let answer = answers
            .recv()
            .await
            .ok_or_else(|| "the deck closed before the question was answered".to_string())?;

        // The echoed ToolResult is what clears the pending card in the fold
        // (matched by this exact id) — without it the gate would keep eating
        // keys for the rest of the turn.
        let _ = self.inbound.send(Inbound::Event {
            agent: self.agent.clone(),
            event: AgentEvent::ToolResult {
                call_id: id,
                output: ToolOutput::Ok {
                    content: answer.clone(),
                },
                duration_ms: 0,
            },
        });

        match presented.iter().position(|option| *option == answer) {
            Some(i) => Ok((i + 1).to_string()),
            None => Ok(answer),
        }
    }
}

// ── FileChange synthesis ────────────────────────────────────────────────────

/// A [`ToolExecutor`] wrapper that emits `AgentEvent::FileChange` when a
/// file-mutating built-in succeeds, so the deck's Files tab / diff panel and
/// ledger are live during the turn. The diff is synthesized from the tool's
/// own input (`edit_file` carries old/new verbatim; `write_file` the full
/// content; `delete_file` reads the file before executing) — an honest
/// approximation until the tool layer emits real diffs on the event path.
struct FileChangeTap<'a> {
    inner: &'a dyn ToolExecutor,
    events: UnboundedSender<AgentEvent>,
    root: PathBuf,
}

#[async_trait]
impl ToolExecutor for FileChangeTap<'_> {
    fn schemas(&self) -> Vec<ToolSchema> {
        self.inner.schemas()
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string);
        // Pre-state, captured before the mutation: existence decides
        // Created-vs-Modified for write_file; content is delete_file's diff.
        let pre = match (name, &path) {
            ("write_file", Some(p)) => Some((self.root.join(p).exists(), None)),
            ("delete_file", Some(p)) => {
                Some((true, std::fs::read_to_string(self.root.join(p)).ok()))
            }
            _ => None,
        };

        let output = self.inner.execute(name, input).await;
        if output.is_error() {
            return output;
        }

        if let Some(path) = path
            && let Some((kind, diff)) = file_change_of(name, input, pre)
        {
            let _ = self
                .events
                .send(AgentEvent::FileChange { path, kind, diff });
        }
        output
    }
}

/// The `(kind, pseudo-diff)` for one successful mutating tool call, or `None`
/// for tools that don't change files. `pre` is `(existed_before, old_content)`
/// as captured by the tap.
fn file_change_of(
    name: &str,
    input: &Value,
    pre: Option<(bool, Option<String>)>,
) -> Option<(FileChangeKind, Option<String>)> {
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    match name {
        "write_file" => {
            let existed = pre.map(|(existed, _)| existed).unwrap_or(false);
            let kind = if existed {
                FileChangeKind::Modified
            } else {
                FileChangeKind::Created
            };
            Some((kind, text("content").map(|c| pseudo_diff("", c))))
        }
        "edit_file" => {
            let diff = match (text("old_string"), text("new_string")) {
                (Some(old), Some(new)) => Some(pseudo_diff(old, new)),
                _ => None,
            };
            Some((FileChangeKind::Modified, diff))
        }
        "delete_file" => {
            let old = pre.and_then(|(_, content)| content);
            Some((FileChangeKind::Deleted, old.map(|c| pseudo_diff(&c, ""))))
        }
        _ => None,
    }
}

/// A minimal unified-diff-shaped rendering of `old` → `new`: `-` lines then
/// `+` lines, each side capped at [`PSEUDO_DIFF_MAX_LINES`] with an elision
/// marker (prefixed with a space so line counters ignore it).
fn pseudo_diff(old: &str, new: &str) -> String {
    let mut out = String::new();
    let mut side = |content: &str, prefix: char| {
        for (lines, line) in content.lines().enumerate() {
            if lines == PSEUDO_DIFF_MAX_LINES {
                out.push_str(&format!(
                    " … ({} more lines)\n",
                    content.lines().count() - PSEUDO_DIFF_MAX_LINES
                ));
                break;
            }
            out.push(prefix);
            out.push_str(line);
            out.push('\n');
        }
    };
    side(old, '-');
    side(new, '+');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal inner executor that always succeeds (or always errors).
    struct FakeInner {
        error: bool,
    }

    #[async_trait]
    impl ToolExecutor for FakeInner {
        fn schemas(&self) -> Vec<ToolSchema> {
            vec![]
        }
        async fn execute(&self, name: &str, _input: &Value) -> ToolOutput {
            if self.error {
                ToolOutput::Error {
                    message: format!("{name} failed"),
                }
            } else {
                ToolOutput::Ok {
                    content: format!("{name} ok"),
                }
            }
        }
    }

    fn recv_file_change(rx: &mut UnboundedReceiver<AgentEvent>) -> Option<AgentEvent> {
        rx.try_recv().ok()
    }

    #[tokio::test]
    async fn tap_emits_created_for_write_file_to_a_new_path() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let inner = FakeInner { error: false };
        let tap = FileChangeTap {
            inner: &inner,
            events: tx,
            root: dir.path().to_path_buf(),
        };
        let input = serde_json::json!({ "path": "src/new.rs", "content": "a\nb\n" });
        let out = tap.execute("write_file", &input).await;
        assert!(!out.is_error());
        match recv_file_change(&mut rx) {
            Some(AgentEvent::FileChange { path, kind, diff }) => {
                assert_eq!(path, "src/new.rs");
                assert_eq!(kind, FileChangeKind::Created);
                assert_eq!(diff.as_deref(), Some("+a\n+b\n"));
            }
            other => panic!("expected FileChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tap_emits_modified_for_write_file_over_an_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.txt"), "old").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let inner = FakeInner { error: false };
        let tap = FileChangeTap {
            inner: &inner,
            events: tx,
            root: dir.path().to_path_buf(),
        };
        let input = serde_json::json!({ "path": "x.txt", "content": "new" });
        tap.execute("write_file", &input).await;
        match recv_file_change(&mut rx) {
            Some(AgentEvent::FileChange { kind, .. }) => {
                assert_eq!(kind, FileChangeKind::Modified)
            }
            other => panic!("expected FileChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tap_builds_edit_file_diff_from_old_and_new_strings() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let inner = FakeInner { error: false };
        let tap = FileChangeTap {
            inner: &inner,
            events: tx,
            root: dir.path().to_path_buf(),
        };
        let input = serde_json::json!({
            "path": "src/lib.rs",
            "old_string": "fn a() {}",
            "new_string": "fn a() {}\nfn b() {}",
        });
        tap.execute("edit_file", &input).await;
        match recv_file_change(&mut rx) {
            Some(AgentEvent::FileChange { kind, diff, .. }) => {
                assert_eq!(kind, FileChangeKind::Modified);
                let diff = diff.expect("edit_file carries a pseudo-diff");
                assert!(diff.contains("-fn a() {}"));
                assert!(diff.contains("+fn b() {}"));
            }
            other => panic!("expected FileChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tap_reads_the_file_before_delete_for_the_removed_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gone.txt"), "one\ntwo\n").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let inner = FakeInner { error: false };
        let tap = FileChangeTap {
            inner: &inner,
            events: tx,
            root: dir.path().to_path_buf(),
        };
        let input = serde_json::json!({ "path": "gone.txt" });
        tap.execute("delete_file", &input).await;
        match recv_file_change(&mut rx) {
            Some(AgentEvent::FileChange { kind, diff, .. }) => {
                assert_eq!(kind, FileChangeKind::Deleted);
                assert_eq!(diff.as_deref(), Some("-one\n-two\n"));
            }
            other => panic!("expected FileChange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tap_stays_silent_for_errors_and_non_file_tools() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let failing = FakeInner { error: true };
        let tap = FileChangeTap {
            inner: &failing,
            events: tx,
            root: dir.path().to_path_buf(),
        };
        let input = serde_json::json!({ "path": "x", "content": "y" });
        let out = tap.execute("write_file", &input).await;
        assert!(out.is_error());
        assert!(recv_file_change(&mut rx).is_none(), "no event on error");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let inner = FakeInner { error: false };
        let tap = FileChangeTap {
            inner: &inner,
            events: tx,
            root: dir.path().to_path_buf(),
        };
        tap.execute("read_file", &serde_json::json!({ "path": "x" }))
            .await;
        assert!(
            recv_file_change(&mut rx).is_none(),
            "read-only tools emit nothing"
        );
    }

    #[test]
    fn mcp_outcome_report_lists_connected_servers_by_name() {
        let report = mcp_outcome_report(&["files", "search"], &[]);
        assert_eq!(report, "2 MCP server(s) connected: files, search");
    }

    #[test]
    fn mcp_outcome_report_names_each_failure_with_its_reason() {
        let failed = vec![(
            "slow".to_string(),
            "connect timed out after 10000ms".to_string(),
        )];
        let report = mcp_outcome_report(&["files"], &failed);
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines[0], "1 MCP server(s) connected: files");
        assert_eq!(
            lines[1],
            "MCP server `slow` unavailable: connect timed out after 10000ms"
        );
    }

    #[test]
    fn mcp_outcome_report_states_total_failure_outright() {
        let failed = vec![("a".to_string(), "spawn failed".to_string())];
        let report = mcp_outcome_report(&[], &failed);
        assert!(
            report.starts_with("no MCP servers connected"),
            "the degraded mode is stated, not implied: {report}"
        );
        assert!(report.contains("MCP server `a` unavailable: spawn failed"));
    }

    #[test]
    fn pseudo_diff_caps_each_side_with_an_uncounted_elision_line() {
        let big: String = (0..300).map(|i| format!("line {i}\n")).collect();
        let diff = pseudo_diff(&big, "");
        let minus = diff.lines().filter(|l| l.starts_with('-')).count();
        assert_eq!(minus, PSEUDO_DIFF_MAX_LINES);
        assert!(diff.contains("(100 more lines)"));
        // The elision marker must not read as a change line.
        assert!(diff.lines().any(|l| l.starts_with(' ')));
    }

    /// Drive [`DeckAskUserIo::prompt`] with a scripted answer and inspect the
    /// Inbound stream it produces. The answer is sent only AFTER the AskUser
    /// card appears: `prompt` drains stale answers before presenting (the
    /// cancelled-turn contract), so a pre-sent answer would be swallowed and
    /// the await would hang.
    async fn run_prompt(options: &[&str], answer: &str) -> (Result<String, String>, Vec<Inbound>) {
        let (in_tx, mut in_rx) = mpsc::unbounded_channel();
        let (ans_tx, ans_rx) = mpsc::unbounded_channel();
        let io = DeckAskUserIo {
            agent: "lead".into(),
            inbound: in_tx,
            answers: Arc::new(tokio::sync::Mutex::new(ans_rx)),
        };
        let opts: Vec<String> = options.iter().map(|s| s.to_string()).collect();
        let asking = tokio::spawn(async move { io.prompt("which one?", &opts).await });
        let mut seen = Vec::new();
        seen.push(in_rx.recv().await.expect("the AskUser card is presented"));
        ans_tx.send(answer.to_string()).unwrap();
        let result = asking.await.expect("the prompt task settles");
        while let Ok(inbound) = in_rx.try_recv() {
            seen.push(inbound);
        }
        (result, seen)
    }

    #[tokio::test]
    async fn deck_ask_io_strips_the_free_text_option_and_maps_answers_to_indices() {
        let free = format!("{FREE_TEXT_LABEL}…");
        let (result, seen) = run_prompt(&["postgres", "sqlite", free.as_str()], "sqlite").await;
        // The picked option maps to its 1-based index, the shape
        // execute_ask_user's numeric parser expects.
        assert_eq!(result.unwrap(), "2");
        match &seen[0] {
            Inbound::Event {
                event: AgentEvent::AskUser { options, .. },
                ..
            } => {
                assert_eq!(options, &vec!["postgres".to_string(), "sqlite".to_string()]);
            }
            other => panic!("expected the AskUser card first, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deck_ask_io_echoes_the_clearing_tool_result_with_the_card_id() {
        let (_, seen) = run_prompt(&["a", "b"], "b").await;
        let card_id = match &seen[0] {
            Inbound::Event {
                event: AgentEvent::AskUser { id, .. },
                ..
            } => id.clone(),
            other => panic!("expected AskUser, got {other:?}"),
        };
        match &seen[1] {
            Inbound::Event {
                event:
                    AgentEvent::ToolResult {
                        call_id, output, ..
                    },
                ..
            } => {
                assert_eq!(*call_id, card_id, "the echo clears the exact card");
                assert!(!output.is_error());
            }
            other => panic!("expected the echoed ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deck_ask_io_passes_free_text_through_verbatim() {
        let (result, _) = run_prompt(&["a", "b"], "actually do it my way").await;
        assert_eq!(result.unwrap(), "actually do it my way");
    }

    // ── Double-Esc hold ─────────────────────────────────────────────────────

    /// Single Esc: the plain cancel retains the prompt but never parks
    /// dispatch — "interrupt current, run next" is unchanged.
    #[test]
    fn plain_cancel_retains_without_holding() {
        let mut dispatch = HoldState::new();
        dispatch.cancelled("prompt a");
        assert!(!dispatch.held(), "single Esc must not park dispatch");
    }

    /// The pair with an empty backlog: the escalation lands at the idle recv
    /// (its `Stop` was consumed first — the channel is FIFO), and must still
    /// requeue the prompt that cancel dropped and park dispatch. This is the
    /// sequence that used to fall into the stray-input arm and vanish.
    #[test]
    fn stop_and_hold_requeues_the_prompt_the_first_esc_cancelled() {
        let mut dispatch = HoldState::new();
        dispatch.cancelled("prompt a");
        assert_eq!(dispatch.stop_and_hold(None), vec!["prompt a".to_string()]);
        assert!(dispatch.held(), "double Esc parks dispatch");
        // The retention was consumed: a re-sent escalation has nothing more
        // to requeue.
        assert!(dispatch.stop_and_hold(None).is_empty());
    }

    /// The pair with a backlog: the gap between its two messages is where
    /// the driver auto-dispatches the next queued prompt, so the escalation
    /// cancels THAT turn. Both prompts return — the retained one in front of
    /// the auto-dispatched one (push order is front-most last), the order
    /// the user last saw.
    #[test]
    fn stop_and_hold_restores_the_backlog_order_the_user_saw() {
        let mut dispatch = HoldState::new();
        dispatch.cancelled("prompt a"); // first Esc: A dropped, B dispatched
        assert_eq!(
            dispatch.stop_and_hold(Some("prompt b")), // second Esc during B
            vec!["prompt b".to_string(), "prompt a".to_string()],
        );
        assert!(dispatch.held());
    }

    /// A submission releases the hold, and each plain cancel replaces the
    /// retention — the escalation only ever requeues its own pair's prompt.
    #[test]
    fn release_and_overwrite_scope_retention_to_the_latest_pair() {
        let mut dispatch = HoldState::new();
        dispatch.cancelled("stale");
        dispatch.cancelled("fresh");
        assert_eq!(dispatch.stop_and_hold(None), vec!["fresh".to_string()]);
        dispatch.release();
        assert!(!dispatch.held(), "the next submission releases the hold");
    }

    /// A stray escalation with nothing retained and nothing in flight stays
    /// the documented no-op — nothing to requeue, nothing to hold.
    #[test]
    fn stray_stop_and_hold_is_a_no_op() {
        let mut dispatch = HoldState::new();
        assert!(dispatch.stop_and_hold(None).is_empty());
        assert!(!dispatch.held());
    }

    /// `requeue_front` front-inserts in push order and mirrors every insert
    /// to the deck as `PromptRequeued`, so the driver's backlog and the
    /// deck's queue view (which front-inserts each mirror in turn) agree.
    #[test]
    fn requeue_front_mirrors_each_front_insert_to_the_deck() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut queue: VecDeque<String> = VecDeque::from(["c".to_string()]);
        requeue_front(&mut queue, &tx, vec!["b".to_string(), "a".to_string()]);
        assert_eq!(
            queue,
            VecDeque::from(["a".to_string(), "b".to_string(), "c".to_string()])
        );
        for expected in ["b", "a"] {
            match rx.try_recv() {
                Ok(Inbound::PromptRequeued { agent, text }) => {
                    assert_eq!(agent, LEAD);
                    assert_eq!(text, expected);
                }
                other => panic!("expected PromptRequeued({expected}), got {other:?}"),
            }
        }
        assert!(rx.try_recv().is_err(), "exactly one mirror per insert");
    }
}
