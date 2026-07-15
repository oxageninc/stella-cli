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
//!   drains what was already emitted.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use stella_core::ports::ToolExecutor;
use stella_core::{BudgetGuard, CalibrationMap, Engine, EngineConfig, TurnOutcome};
use stella_model::provider::Provider;
use stella_protocol::{AgentEvent, CompletionMessage, FileChangeKind, ToolOutput, ToolSchema};
use stella_store::Store;
use stella_tools::ToolRegistry;
use stella_tools::custom::{CustomTool, CustomToolSet};
use stella_tui::{
    AgentMeta, AgentStatus, DeckOptions, Inbound, UserInput, WorkspaceInput, run_deck,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::agent;
use crate::config::Config;
use crate::interactive::{AskUserIo, FREE_TEXT_LABEL, InteractiveToolSet, SkillRegistry};
use crate::memory::{SessionMemory, inject_recall_block, turn_warrants_reflection};

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
    /// The user stopped it mid-flight; the future was dropped.
    Cancelled,
    /// The deck is going down; stop driving entirely.
    Quit,
}

/// Run a full deck session: the deck shell on its own task, the engine
/// driver inline. Returns when the user quits (Ctrl-C) or the deck's input
/// stream ends.
pub async fn run_deck_session(cfg: &Config, budget_limit: Option<f64>) -> Result<(), String> {
    // ── Session assembly (still on the normal screen — prints are fine) ────
    let provider = agent::build_provider(cfg)?;
    let registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::new(cfg.workspace_root.clone()));
    agent::populate_schema_index(&registry, &cfg.workspace_root);
    let mcp = agent::connect_mcp(cfg, registry.clone(), true).await;
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set,
        None => &*registry,
    };
    let custom_tools = agent::discover_custom_tools(cfg, true);
    let mut budget = agent::build_budget_guard(budget_limit);
    let store = agent::open_store(&cfg.workspace_root);
    let calibration = agent::seed_calibration(&store, cfg);

    let system_prompt = agent::build_system_prompt(&cfg.workspace_root);
    let mut messages = vec![CompletionMessage::system(system_prompt)];
    // `warn: false`: past this point diagnostics would land on the alternate
    // screen; a memory-less session degrades silently here.
    let mut memory = SessionMemory::open(&cfg.workspace_root, false);

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
    // An idle lead is waiting on the human, not queued behind a supervisor.
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
        ..Default::default()
    };
    // The deck owns its channel ends and runs on its own task so rendering
    // never waits on the driver (and vice versa).
    let deck = tokio::spawn(run_deck(opts, in_rx, sub_tx));

    // ── The driver loop ─────────────────────────────────────────────────────
    let mut queue: VecDeque<String> = VecDeque::new();
    'session: loop {
        // Take the next prompt: backlog first, else wait for deck input.
        let prompt = match queue.pop_front() {
            Some(text) => text,
            None => match sub_rx.recv().await {
                None => break 'session,
                Some(WorkspaceInput::Quit) => break 'session,
                Some(WorkspaceInput::Enqueue { text }) => text,
                Some(WorkspaceInput::ToAgent {
                    input: UserInput::Prompt { text },
                    ..
                }) => text,
                // A stray answer/decision/control with no turn in flight has
                // nothing to act on.
                Some(_) => continue 'session,
            },
        };

        let _ = in_tx.send(Inbound::PromptStarted {
            agent: LEAD.to_string(),
            text: prompt.clone(),
        });

        // Per-turn conversation bookkeeping, mirroring `run_interactive`:
        // refresh the volatile recall block, then append the user prompt.
        // `turn_base` is the truncation point that erases the whole turn if
        // it is cancelled; `reflect_start` scopes the reflection gate to what
        // the turn itself appends.
        if let Some(m) = &memory {
            let block = m.recall_block(&prompt).await;
            inject_recall_block(&mut messages, block);
        }
        let turn_base = messages.len();
        messages.push(CompletionMessage::user(&prompt));
        let reflect_start = messages.len();

        // The execution record outlives the turn future so a cancelled turn
        // can still be closed out in the store.
        let execution = agent::begin_execution(&store, "deck", &prompt, cfg);

        let end = {
            let turn = run_lead_turn(
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
            );
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
                        Some(WorkspaceInput::ToAgent {
                            input: UserInput::AskUserAnswer { answer, .. }, ..
                        }) => {
                            let _ = ask_tx.send(answer);
                        }
                        Some(WorkspaceInput::ToAgent { input: UserInput::Cancel, .. })
                        | Some(WorkspaceInput::Control {
                            control: stella_tui::AgentControl::Stop, ..
                        }) => break TurnEnd::Cancelled,
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
                if outcome.is_ok()
                    && turn_warrants_reflection(&messages[reflect_start..])
                    && let Some(m) = &mut memory
                {
                    m.reflect_and_record(&*provider, &messages, true).await;
                }
            }
            TurnEnd::Cancelled => {
                // Erase the partial turn: the next prompt continues from the
                // last committed conversation state.
                messages.truncate(turn_base);
                if let Some((store, id)) = &execution {
                    let _ = store.finish_execution(*id, "cancelled", 0.0);
                }
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
        let engine =
            Engine::new(provider, &tapped, EngineConfig::default()).with_calibration(calibration);
        engine.run_turn(messages, budget, &tx).await
    };
    drop(tx);
    let _ = forwarder.await;

    let files = registry.files_touched();
    if let Some((store, id)) = &execution {
        let _ = store.record_files_touched(*id, &files);
        let (outcome_label, cost) = match &outcome {
            TurnOutcome::Completed { cost_usd, .. } => ("completed", *cost_usd),
            TurnOutcome::Aborted { .. } => ("aborted", 0.0),
        };
        let _ = store.finish_execution(*id, outcome_label, cost);
    }

    match outcome {
        TurnOutcome::Completed { .. } => Ok(()),
        TurnOutcome::Aborted { reason } => Err(reason),
    }
}

/// Drain one turn's engine events: persist each (via the shared
/// [`agent::persist_event`] write path) and forward it to the deck as the
/// lead agent's `Inbound::Event`. The deck-mode replacement for
/// [`agent::spawn_renderer`] — persistence failures degrade silently here
/// because stderr belongs to the alternate screen.
fn spawn_forwarder(
    mut rx: UnboundedReceiver<AgentEvent>,
    execution: Option<(Arc<Store>, i64)>,
    provider_id: String,
    inbound: UnboundedSender<Inbound>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seq = 0u64;
        while let Some(event) = rx.recv().await {
            if let Some((store, id)) = &execution {
                let _ = agent::persist_event(store, *id, seq, &event, &provider_id);
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
        let path = input.get("path").and_then(Value::as_str).map(str::to_string);
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
            let _ = self.events.send(AgentEvent::FileChange {
                path,
                kind,
                diff,
            });
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
            Some((
                FileChangeKind::Deleted,
                old.map(|c| pseudo_diff(&c, "")),
            ))
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
        let mut lines = 0usize;
        for line in content.lines() {
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
            lines += 1;
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
    /// Inbound stream it produces.
    async fn run_prompt(
        options: &[&str],
        answer: &str,
    ) -> (Result<String, String>, Vec<Inbound>) {
        let (in_tx, mut in_rx) = mpsc::unbounded_channel();
        let (ans_tx, ans_rx) = mpsc::unbounded_channel();
        let io = DeckAskUserIo {
            agent: "lead".into(),
            inbound: in_tx,
            answers: Arc::new(tokio::sync::Mutex::new(ans_rx)),
        };
        ans_tx.send(answer.to_string()).unwrap();
        let opts: Vec<String> = options.iter().map(|s| s.to_string()).collect();
        let result = io.prompt("which one?", &opts).await;
        let mut seen = Vec::new();
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
}
