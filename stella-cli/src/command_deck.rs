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
//! a FIFO prompt queue and a bounded pool of **sub-session workers**
//! (`crate::subsession`). The deck's contract is "input never blocks", and
//! dispatch now honors it too: a prompt submitted while the lead's turn is in
//! flight goes straight to a dedicated worker session (`req:<n>`) instead of
//! waiting the turn out — [`Inbound::PromptStarted`] pops the deck's queue
//! display the moment whichever lane picks it up. `task_assign` spawns task
//! workers (`sub:<task-id>`) the same way, and every worker reports back via
//! its live event lane, an inbox notification, and (for task workers) the
//! board task auto-completing. Prompts queue only past the worker cap, on a
//! dispatch hold, or when they are slash commands (the lead's dispatcher owns
//! those). The fleet layer now carries its own per-task control verbs
//! (`Fleet::pause_task` / `resume_task` / `stop_task`, riding
//! `stella_fleet::WorkerControls` through the `FleetWorker` port);
//! surfacing `stella fleet` tasks as controllable deck lanes and
//! fleet-worktree isolation for deck workers remain follow-ups on that seam.
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
//!   drains what was already emitted. The deck's single Esc is the SOFT stop
//!   for step-loop lead turns (the engine ends at the next step boundary,
//!   keeping completed work — `stella_core::SOFT_STOP_REASON`); pipeline
//!   turns and worker lanes cancel immediately (a pipeline is a multi-stage
//!   flow with no single soft-stop continuation). Mid-turn `>` steering,
//!   though, reaches BOTH lead turn shapes — the step-loop engine and the
//!   pipeline's execute engine both drain the steering tap at their step
//!   boundaries. After a cancel the loop pops the next queued prompt as
//!   usual ("interrupt current, run next").
//!   A double-Esc `StopAndHold` is the immediate clean cancel plus
//!   queue discipline: the interrupted prompt returns to the FRONT of the
//!   backlog and dispatch parks until the user's next submission, which
//!   arrives as `EnqueueFront` and runs ahead of it. The pair reaches the
//!   driver as two FIFO messages — the plain `Stop`, then the escalation —
//!   so the first press has always dropped the turn (and would have
//!   forgotten its prompt) before `StopAndHold` is read: [`HoldState`]
//!   retains what that cancel dropped so the second press still has a
//!   prompt to requeue and park.

mod skills;
use skills::{deck_slash_commands, handle_skills_input, skills_snapshot};

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use stella_core::ports::ToolExecutor;
use stella_core::router::CircuitBreaker;
use stella_core::{BudgetGuard, CalibrationMap, Engine, Router, TurnOutcome};
use stella_model::provider::Provider;
use stella_pipeline::{
    ContextRecallPort, McpPrefetchPort, NoContextRecall, Pipeline, PipelineConfig, PipelinePorts,
    PipelineStatus,
};
use stella_protocol::{
    AgentEvent, CiStatus, CompletionMessage, CompletionRequest, FileChangeKind, ModelRef, PrStatus,
    TaskItem, ToolOutput, ToolSchema,
};
use stella_store::Store;
use stella_tools::ToolRegistry;
use stella_tools::custom::{CustomTool, CustomToolSet};
use stella_tools::hook_runner::ShellHookRunner;
use stella_tools::issue_ops::{CreateParams, IssueFilters, IssueSummary, LabelInfo, MemberInfo};
use stella_tools::issues::IssueBackend;
use stella_tui::{
    AgentMeta, AgentScope, AgentStatus, DeckOptions, EntityField, EntityHit, Inbound, IssueAction,
    IssueRow, SkillOp, SkillScope, SkillSearchHit, SkillsView, SlashCommand, SplashCue, UserInput,
    WorkspaceInput, run_deck,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::agent;
use crate::claims::ClaimTap;
use crate::config::Config;
use crate::interactive::{AskUserIo, FREE_TEXT_LABEL, InteractiveToolSet, SkillRegistry};

mod authoring;
mod forwarder;
use crate::memory::{SessionMemory, inject_recall_block};
use crate::runtime::{SystemClock, TokioSleeper};
use crate::subsession::{self, SubSessions, SupervisorMsg};
use authoring::handle_agent_create;
pub(crate) use forwarder::spawn_forwarder;

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

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// An ephemeral transcript notice for DIRECT deck sends (`deck_tx`), never
/// the journaled `in_tx` path: boot narration, hints, and guidance that must
/// not replay (and pile up) every time the session is resumed.
fn chrome_note(text: String) -> Inbound {
    Inbound::Event {
        agent: LEAD.to_string(),
        event: AgentEvent::Text { delta: text },
    }
}

/// `OXAGEN_DEBUG=1` → the structured deck log path (L-T8), mirroring the
/// location `stella_tui::shell::RunOptions` documents. `None` otherwise, and
/// on any failure to create the directory — a lost debug log never gates the
/// session.
fn debug_log_path() -> Option<PathBuf> {
    if std::env::var_os("OXAGEN_DEBUG").is_none_or(|v| v.is_empty() || v == "0") {
        return None;
    }
    #[cfg(not(unix))]
    return None;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
        let dir = state_home.join("stella").join("logs");
        match std::fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(true).mode(0o700);
                builder.create(&dir).ok()?;
            }
            Err(_) => return None,
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
        Some(dir.join(format!("deck-{}.jsonl", std::process::id())))
    }
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
    queue: &mut crate::session_persist::DurableQueue,
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
    resume: Option<crate::session_persist::ResumeRequest>,
) -> Result<(), String> {
    crate::enterprise_telemetry::authorize_execution_surface(
        crate::enterprise_telemetry::ExecutionSurface::Deck,
    )?;
    let provider = agent::build_provider(cfg)?;
    let registry_options = agent::registry_options(cfg);
    let registry: Arc<ToolRegistry> = Arc::new(
        agent::new_tool_registry(cfg.workspace_root.clone(), registry_options.clone()).await,
    );
    agent::populate_schema_index(&registry, &cfg.workspace_root)?;
    let active_rules =
        crate::rules::enforce_workspace_rules(&registry, &cfg.workspace_root, &cfg.authority);
    let custom_tools = agent::discover_custom_tools(cfg, true).await;
    let mut budget = agent::build_budget_guard(budget_limit);
    let store = agent::open_store(&cfg.workspace_root);
    let calibration = agent::seed_calibration(&store, cfg);

    let system_prompt = agent::with_session_hook_context(
        agent::build_system_prompt(cfg, &cfg.workspace_root, &active_rules),
        cfg,
    )
    .await;
    let mut messages = vec![CompletionMessage::system(system_prompt.clone())];
    // `warn: false`: past this point diagnostics would land on the alternate
    // screen; a memory-less session degrades silently here.
    let mut memory = SessionMemory::open_with_authority(&cfg.workspace_root, false, &cfg.authority);
    // Custom extensions: ⚡ commands/skills in the slash menu, custom agents
    // behind `/agents`. Reloaded after `/init`, which may adopt new ones.
    let mut custom = crate::extensions::CustomExtensions::load_with_authority(
        &cfg.workspace_root,
        &cfg.authority,
    );
    // The npx skills registry (search/install), constructed once for the whole
    // session — the SKILLS tab's ops route through it (see `handle_skills_input`).
    let skill_registry = SkillRegistry::from_env(cfg.workspace_root.clone());

    // ── Durable session identity (still on the normal screen) ──────────────
    // This session announces itself in the machine-wide registry, and every
    // fold-relevant envelope it produces is journaled to the record's sidecar
    // (`session_persist`) — quit / crash / power cut, the session reopens
    // where it stood. A resume request resolves HERE so its errors print on
    // the normal screen instead of dying behind the alternate one.
    let session_registry = stella_store::SessionRegistry::open_default();
    let _ = session_registry.prune(SESSION_RECORD_MAX_AGE_MS);
    let _ = stella_store::NotificationStore::open_default().prune(NOTIFICATION_MAX_AGE_MS);
    let workspace_path = cfg.workspace_root.display().to_string();
    let workspace_name = cfg
        .workspace_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| workspace_path.clone());
    let mut resume_state = match &resume {
        Some(request) => {
            let target = crate::session_persist::resolve_resume_target(
                &session_registry,
                &workspace_path,
                request,
            )?;
            Some(crate::session_persist::load_resume(
                &session_registry,
                &target.id,
                &workspace_path,
            )?)
        }
        None => None,
    };
    let mut session_record = match &mut resume_state {
        // Re-own the stored record: same id (the registry never forks a
        // resumed session's identity), this process's pid, back to waiting.
        Some(rs) => crate::session_persist::adopt_record(
            rs.record.clone(),
            stella_store::SessionStatus::NeedsInput,
        ),
        None => stella_store::SessionRecord::new(workspace_path.clone(), workspace_name.clone()),
    };
    let _ = session_registry.upsert(&session_record);
    // What the record's terminal status will be at exit (last turn wins);
    // quitting with a pending backlog overrides to Paused below — the work
    // is durable now, so an exit with prompts waiting is a pause, not loss.
    let mut session_exit = stella_store::SessionStatus::Complete;
    let mut sidecar_dir = session_registry.sidecar_dir(&session_record.id);
    if let Some(rs) = &mut resume_state {
        messages = crate::session_persist::restore_messages(
            std::mem::take(&mut rs.history).unwrap_or_default(),
            &system_prompt,
        );
        // `--budget` means THIS session on every resume path: the guard's
        // session accumulator reseeds to exactly what the session had
        // already spent (its journal's last `BudgetTick`), so spend stays
        // monotone across interruptions. Same seam as the in-deck session
        // switch (`SessionResume` in the driver loop below).
        budget.reseed_session_spend(rs.spent_usd.unwrap_or(0.0));
    }

    // ── Channels: engine → deck (Inbound) and deck → driver (WorkspaceInput)
    // The driver's send side (`in_tx`) reaches the deck through the journal
    // tee — the single choke point that makes the session durable. Direct
    // `deck_tx` sends bypass the journal: replay (which must never
    // re-journal itself) and ephemeral session chrome (boot narration,
    // hints) that would otherwise pile up in the transcript on every resume.
    let (in_tx, raw_rx) = mpsc::unbounded_channel::<Inbound>();
    let (deck_tx, deck_rx) = mpsc::unbounded_channel::<Inbound>();
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<WorkspaceInput>();
    let (ask_tx, ask_rx) = mpsc::unbounded_channel::<String>();
    // The supervisor channel: `task_assign` spawn requests (tap → driver)
    // and sub-session endings (worker → driver). See `crate::subsession`.
    let (sup_tx, mut sup_rx) = mpsc::unbounded_channel::<SupervisorMsg>();
    let journal_sink = crate::session_persist::SessionSink::shared(
        match stella_store::journal::SessionJournal::open(&sidecar_dir) {
            Ok(j) => Some(j),
            Err(e) => {
                let _ = deck_tx.send(chrome_note(format!(
                    "session journaling unavailable — this session will not be resumable ({e})"
                )));
                None
            }
        },
    );
    let _tee = crate::session_persist::spawn_journal_tee(
        raw_rx,
        deck_tx.clone(),
        journal_sink.clone(),
        LEAD,
    );
    // Replay a resumed session's journal straight onto the deck BEFORE the
    // first live send, so the restored transcript precedes everything this
    // run adds. (The fresh `Register` below then restamps the lead's meta —
    // pid, model, clock — over the replayed one.) The non-lead lanes the
    // replay puts on the dashboard are remembered so an in-deck session
    // switch can deregister them — rows of a session left behind must not
    // linger on the next session's dashboard.
    let mut replayed_lanes: Vec<String> = Vec::new();
    if let Some(rs) = &mut resume_state {
        replayed_lanes = crate::session_persist::journal_lanes(&rs.records, LEAD);
        crate::session_persist::replay_session(
            std::mem::take(&mut rs.records),
            now_ms(),
            LEAD,
            &deck_tx,
        );
    }

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
    // silently-missing /command is otherwise undiagnosable. Session chrome:
    // re-checked every boot, so it never journals.
    if let Some(report) = custom.problems_report() {
        let _ = deck_tx.send(chrome_note(report));
    }
    // Honest degradation (#266): a user who pinned a reasoning effort for the
    // lead but chose a provider whose adapter has no reasoning control gets a
    // one-line notice instead of a silently-dropped setting. Keyed on the
    // explicit settings pin for the lead (Default kind), never the
    // auto-resolved effort — session chrome, re-checked every boot, never
    // journaled.
    if let Some(notice) = crate::engine_config::unsupported_effort_notice(
        cfg.provider.id,
        cfg.provider.display_name,
        cfg.engine_settings
            .as_ref()
            .and_then(|e| e.agent(crate::settings::EngineAgentKind::Default))
            .and_then(|a| a.effort),
    ) {
        let _ = deck_tx.send(chrome_note(notice));
    }
    // An idle lead is waiting on the human, not queued behind a supervisor
    // (sent after the problems report — a Text event folds to `Running`).
    let _ = in_tx.send(Inbound::Status {
        agent: LEAD.to_string(),
        status: AgentStatus::WaitingInput,
    });

    // ── The durable prompt backlog ──────────────────────────────────────────
    // Every mutation writes through to the sidecar, so a queued prompt
    // survives any interruption from the moment it is queued. On resume the
    // restored backlog (and the prompt an interruption cut short, back at
    // the FRONT) is mirrored into the deck's queue view, and dispatch parks
    // until the user's next submission — resuming shows where things stood
    // and costs nothing until the user says go.
    let mut queue = crate::session_persist::DurableQueue::fresh(sidecar_dir.clone());
    let mut resume_hold = false;
    if let Some(rs) = &mut resume_state {
        // Interrupted prompts (any lane's unsettled dispatch) go back at the
        // FRONT, ahead of the stored backlog, in their original order.
        let mut restored = std::mem::take(&mut rs.interrupted);
        restored.extend(std::mem::take(&mut rs.queue));
        if !restored.is_empty() {
            resume_hold = true;
            // Front-inserts mirror back-to-front so the view reads in order.
            for text in restored.iter().rev() {
                let _ = in_tx.send(Inbound::PromptRequeued {
                    agent: LEAD.to_string(),
                    text: text.clone(),
                });
            }
            let _ = deck_tx.send(chrome_note(format!(
                "session restored — {} prompt(s) waiting, dispatch held. Submit anything to \
                 run it first (then the backlog), or ctrl+t to edit the queue.",
                restored.len()
            )));
            queue.adopt(sidecar_dir.clone(), restored);
        } else {
            let _ = deck_tx.send(chrome_note(
                "session restored — the conversation continues where it left off.".to_string(),
            ));
        }
    } else if session_registry.latest_resumable(&workspace_path).is_some() {
        // A fresh session in a workspace that has something to go back to:
        // one pointer, so "navigate back in" is discoverable.
        let _ = deck_tx.send(chrome_note(
            "◂ a previous session is resumable — ← (on an empty prompt) opens SESSIONS, ⏎ \
             reopens one; or run `stella resume`."
                .to_string(),
        ));
    }
    // Seed the SKILLS tab so it has data the instant it is opened (both scopes),
    // without waiting on a `/skills` round-trip.
    let _ = in_tx.send(skills_snapshot(&cfg.workspace_root, None));
    // Seed the ENGINE panel the same way: the merged
    // agent_engine_config plus the picker vocabularies, ready before the
    // user first opens it.
    let _ = in_tx.send(engine_config_inbound(cfg, None));

    let ask_io = DeckAskUserIo {
        agent: LEAD.to_string(),
        inbound: in_tx.clone(),
        answers: Arc::new(tokio::sync::Mutex::new(ask_rx)),
    };

    // The deck drives turns through the staged pipeline by default (triage →
    // recall → plan → scope → witness → execute → verify → judge); `/pipeline`
    // toggles back to the raw `Engine::run_turn` loop (`run_lead_turn`). A
    // resumed session keeps whatever it last had.
    let pipeline_init = resume_state
        .as_ref()
        .and_then(|rs| rs.pipeline)
        .unwrap_or(true);
    let opts = DeckOptions {
        debug_log_path: debug_log_path(),
        slash_commands: deck_slash_commands(&custom),
        initial_graph: agent::graph_snapshot(&cfg.workspace_root),
        no_anim,
        pipeline: pipeline_init,
        ..Default::default()
    };
    // The deck owns its channel ends and runs on its own task so rendering
    // never waits on the driver (and vice versa).
    let deck = tokio::spawn(run_deck(opts, deck_rx, sub_tx));

    // The launch cinematic: hold the splash's battle loop open over session
    // init and release it once BOTH async legs — the background code-graph
    // build below and the MCP connect after it — have finished, so the movie
    // covers however long a first launch's indexing takes instead of handing
    // off to a deck that is still visibly assembling itself. Any key still
    // skips; `--no-anim` sessions ignore the cue entirely.
    let _ = in_tx.send(Inbound::Splash(SplashCue::Replay));
    let init_pending = Arc::new(std::sync::atomic::AtomicUsize::new(2));
    let release_splash = {
        let tx = in_tx.clone();
        move || {
            if init_pending.fetch_sub(1, Ordering::SeqCst) == 1 {
                let _ = tx.send(Inbound::Splash(SplashCue::Release));
            }
        }
    };
    let release_on_graph_ready = release_splash.clone();

    // Auto-build the code-graph index in the background (a cheap incremental
    // refresh if it already exists) and keep it fresh via the live watcher, so
    // `graph_query` is available this session — and the Graph tab populates —
    // without a manual `stella init`. Spawned AFTER the deck is up so its
    // `◈ indexing…`/`✓ …` lines render as transcript events; non-blocking, and
    // the watcher stops when `_session_graph` drops at session end. `_graph_build`
    // (the setup task's JoinHandle) is detached — freshness outlives it.
    // Indexing narration is session chrome (direct `deck_tx`): it re-runs at
    // every boot, so journaling it would replay stale "indexing…" lines on
    // top of every resumed transcript.
    let status_tx = deck_tx.clone();
    let ready_tx = deck_tx.clone();
    let ready_root = cfg.workspace_root.clone();
    let (_session_graph, _graph_build) = agent::spawn_session_graph(
        &cfg.workspace_root,
        registry.clone(),
        Box::new(move |line| {
            let _ = status_tx.send(chrome_note(line));
        }),
        Box::new(move || {
            // Populate the Graph tab now the index exists (it opened on the
            // "run stella init" hint), and restore the lead to idle — the
            // status Text events above fold it to `Running`.
            if let Some(snapshot) = agent::graph_snapshot(&ready_root) {
                let _ = ready_tx.send(Inbound::GraphSnapshot(snapshot));
            }
            let _ = ready_tx.send(Inbound::Status {
                agent: LEAD.to_string(),
                status: AgentStatus::WaitingInput,
            });
            // One of the two init legs the launch splash waits on.
            release_on_graph_ready();
        }),
    );

    // ── MCP connect, OFF the first prompt's critical path (#98 continued) ──
    // The connect used to run inline here: the deck was live, but the driver
    // loop — and therefore the FIRST prompt's dispatch — waited up to 10s
    // per server. It now runs on its own task and lands the connected set in
    // `mcp_slot`; every turn resolves its tool executor from the slot at
    // dispatch, so servers join the session the moment they connect and the
    // first prompt starts immediately (on native tools when connect is still
    // running — narrated once, never silent). Prompts are never lost either
    // way: the deck's input never blocks and `sub_rx` buffers.
    // Session-scoped MCP management state, shared with the MCP tab:
    //   • `mcp_disabled` — server names disabled this session; toggling it
    //     hides a server's tools from the model on the next call (live, no
    //     reconnect), because the engine re-reads schemas each call.
    //   • the usage ledger (from the registry) records every MCP call for the
    //     `mcp_usage` telemetry table.
    let mcp_disabled: stella_mcp::DisabledServers =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    // `Arc<McpToolSet>` (not a bare `McpToolSet`) so a turn can cheaply clone
    // the connected set into the Best-of-N candidate surface + orchestrator
    // pre-fetch (issue #248 Phase 1) alongside its own `&dyn ToolExecutor`.
    let mcp_slot: Arc<tokio::sync::OnceCell<Arc<stella_mcp::McpToolSet>>> =
        Arc::new(tokio::sync::OnceCell::new());
    let mcp_configured = spawn_mcp_connect(
        cfg.clone(),
        registry.clone(),
        mcp_disabled.clone(),
        mcp_slot.clone(),
        in_tx.clone(),
        deck_tx.clone(),
        release_splash.clone(),
    );
    // Whether the "still connecting" note has been narrated (once, on the
    // first turn that dispatches before the slot fills).
    let mut mcp_pending_noted = false;
    // Session-scoped lean-mode activation state (crate::discovery): the tool
    // stack is rebuilt per turn, but a tool the model surfaced via
    // tool_search stays advertised for the rest of the deck session.
    let discovery_activation = crate::discovery::new_activation();

    // The registry record and hygiene ran during assembly (the durable
    // session identity block). Claim-on-first-write identity for the lead's
    // turns, and crash hygiene for the whole workspace: sweep claims old
    // enough that their process is surely gone (a crashed writer cannot
    // release its own). The holder is remade whenever the deck navigates to
    // another session (`SessionResume` below) — claims must name the session
    // actually doing the writing.
    let mut lead_holder = format!("{}/lead", session_record.id);
    if let Some(store) = &store {
        let _ = store.prune_stale_file_locks(crate::claims::STALE_CLAIM_MAX_AGE_SECS);
    }
    // The inbox poller keeps the badge live as other sessions produce
    // persist-until-read notifications.
    spawn_notification_poller(in_tx.clone());

    // The ISSUES tab's lazily-detected tracker backend (see
    // [`issue_backend`]); shared by every spawned issues task.
    let issue_backend_cache: IssueBackendCache = Arc::new(tokio::sync::Mutex::new(None));

    // ── The driver loop ─────────────────────────────────────────────────────
    // (`queue` — the durable backlog — was constructed with the session
    // identity above, restored contents and all.)
    // Double-Esc bookkeeping: parks dispatch and retains what the pair's
    // first press cancelled (see [`HoldState`]). A resumed backlog starts
    // parked — reopening a session shows where it stood; the user's next
    // submission is what sets it moving (and runs first).
    let mut dispatch = HoldState::new();
    dispatch.held = resume_hold;
    // `/pipeline`: route lead turns through the staged pipeline (triage →
    // witness → execute → verify → judge) instead of the raw engine loop.
    // Session-local, ON at start (the deck loads with the pipeline active)
    // unless a resumed session had toggled it — mirrored to the PIPELINE
    // stat box via `Inbound::Pipeline`.
    let mut pipeline_on = pipeline_init;
    // An agent-creation request that arrived mid-turn: drafting needs the
    // provider (borrowed by the running turn), so it parks here and runs
    // right after the turn settles.
    let mut pending_create: Option<(String, AgentScope)> = None;
    // Sub-session bookkeeping: live-worker slots, and `task_assign` requests
    // waiting for one (drained oldest-first as workers end).
    let mut subs = SubSessions::with_registry_options(registry_options.clone());
    let mut pending_spawns: VecDeque<stella_core::tasks::SpawnRequest> = VecDeque::new();
    // Lanes whose Restart arrived while the worker was still live: stop
    // first, respawn on its Ended.
    let mut pending_restarts: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Worker spend not yet metered into the session budget guard — applied
    // at the loop top, where the guard is free (budget aborts happen at
    // safe boundaries only).
    let mut unmetered_spend: f64 = 0.0;
    // PR/CI reconcile: polls `gh` for the workspace's current-branch PR and
    // its checks, feeding the footer's PR cell, the store mirror, and
    // failing-CI inbox notifications. The nudge skips the wait after turns
    // and worker endings — the moments a PR most plausibly just changed.
    let pr_nudge = Arc::new(tokio::sync::Notify::new());
    // The monitor attributes PR rows and CI notifications to a session id;
    // shared + mutable because an in-deck `SessionResume` re-keys it to the
    // adopted session (the monitor follows the deck, not the process's
    // first session).
    let pr_session_id = Arc::new(std::sync::Mutex::new(session_record.id.clone()));
    spawn_pr_monitor(
        cfg.workspace_root.clone(),
        pr_session_id.clone(),
        store.clone(),
        workspace_name.clone(),
        pr_nudge.clone(),
        in_tx.clone(),
    );
    'session: loop {
        // Meter accumulated worker spend into the session guard at this
        // safe boundary — the engine's own budget checks then see the true
        // session total on the next turn.
        if unmetered_spend > 0.0 {
            let _ = budget.record_spend(unmetered_spend);
            unmetered_spend = 0.0;
        }
        // Take the next prompt: backlog first (unless held), else wait for
        // deck input.
        let next = if dispatch.held() {
            None
        } else {
            queue.pop_front()
        };
        // Between prompts the driver waits on BOTH channels: deck input and
        // the supervisor (a sub-session ending or a stray spawn request must
        // not wait for the user's next keystroke to be serviced).
        enum IdleWake {
            Input(Option<WorkspaceInput>),
            Sup(Option<SupervisorMsg>),
        }
        let prompt = match next {
            Some(text) => text,
            None => {
                let wake = tokio::select! {
                    input = sub_rx.recv() => IdleWake::Input(input),
                    msg = sup_rx.recv() => IdleWake::Sup(msg),
                };
                let input = match wake {
                    // The driver holds a live `sup_tx`, so `None` cannot
                    // occur; treat it as a spurious wake regardless.
                    IdleWake::Sup(None) => continue 'session,
                    IdleWake::Sup(Some(msg)) => {
                        handle_supervisor_msg(
                            msg,
                            &mut subs,
                            &mut pending_restarts,
                            &mut pending_spawns,
                            &mut queue,
                            dispatch.held(),
                            &registry,
                            &store,
                            &session_record.id,
                            &workspace_name,
                            cfg,
                            budget_limit,
                            &mut unmetered_spend,
                            &pr_nudge,
                            &in_tx,
                            &sup_tx,
                        );
                        continue 'session;
                    }
                    IdleWake::Input(input) => input,
                };
                match input {
                    None => break 'session,
                    Some(WorkspaceInput::Quit) => break 'session,
                    // Worker controls work between lead turns too — the
                    // lead being idle says nothing about a running worker.
                    Some(WorkspaceInput::Control { agent, control }) if agent != LEAD => {
                        service_worker_control(
                            &agent,
                            control,
                            &mut subs,
                            &mut pending_restarts,
                            cfg,
                            budget_limit,
                            &session_record.id,
                            &workspace_name,
                            &in_tx,
                            &sup_tx,
                        );
                        continue 'session;
                    }
                    // Any submission releases a hold and runs NOW — ahead of the
                    // parked backlog. `EnqueueFront` is the deck's explicit
                    // front-insert (sent while it knows dispatch is held); a
                    // plain `Enqueue` behaves identically here because running
                    // the text immediately IS the front of the queue.
                    Some(WorkspaceInput::Enqueue { text })
                    | Some(WorkspaceInput::EnqueueFront { text })
                    | Some(WorkspaceInput::ToAgent {
                        input: UserInput::Prompt { text, .. },
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
                    // SKILLS-tab ops work whether or not a turn is running — handled
                    // at both recv sites so the manager is live mid-turn too.
                    Some(WorkspaceInput::Skill(op)) => {
                        handle_skills_input(
                            &op,
                            cfg,
                            &in_tx,
                            &skill_registry,
                            agent::remaining_budget(&budget),
                        );
                        continue 'session;
                    }
                    // LLM-assisted agent creation needs the provider, which is
                    // free here (no turn in flight) — draft, install, refresh.
                    Some(WorkspaceInput::AgentCreate { description, scope }) => {
                        handle_agent_create(
                            &description,
                            scope,
                            cfg,
                            &*provider,
                            agent::remaining_budget(&budget),
                            &in_tx,
                        )
                        .await;
                        continue 'session;
                    }
                    // ⏎ on a resumable row in the SESSIONS overlay: navigate into
                    // that session. Only serviced HERE, between turns and with no
                    // live workers — running work is never torn down by a
                    // navigation (the mid-turn arm answers with guidance
                    // instead, and live sub-sessions stream into THIS session's
                    // lanes and settle against its records). The current
                    // session's durable state is already on disk, so switching
                    // away loses nothing.
                    Some(WorkspaceInput::SessionResume { id }) => {
                        let loaded = if id == session_record.id {
                            Err("that is this session — you are already in it".to_string())
                        } else if subs.live() > 0 {
                            Err(format!(
                                "{} worker(s) are still running — stop them (s on the lane) \
                                 or wait for them to finish, then press ⏎ on the session \
                                 again",
                                subs.live()
                            ))
                        } else {
                            crate::session_persist::load_resume(
                                &session_registry,
                                &id,
                                &workspace_path,
                            )
                        };
                        match loaded {
                            Err(reason) => {
                                let _ = deck_tx
                                    .send(chrome_note(format!("cannot resume `{id}`: {reason}")));
                            }
                            Ok(mut rs) => {
                                // Park the CURRENT session: sync the journal,
                                // snapshot the conversation, and either mark it
                                // Paused — or, if nothing ever happened in it,
                                // remove the empty shell instead of littering
                                // the registry with it.
                                journal_sink
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .sync();
                                let _ = crate::session_persist::snapshot_history(
                                    &sidecar_dir,
                                    &messages,
                                );
                                if session_record.summary.is_empty() && queue.is_empty() {
                                    let _ = session_registry.remove(&session_record.id);
                                } else {
                                    session_record.status = stella_store::SessionStatus::Paused;
                                    let _ = session_registry.upsert(&session_record);
                                }

                                // Clear the departing session's worker rows
                                // off the dashboard before the target's
                                // replay claims it: every non-lead lane is
                                // terminal here (the switch refuses while
                                // workers are live), so each one — spawned
                                // this tenancy or replayed at the last
                                // adoption — gets a `Deregister`. Direct
                                // sends (deck_tx): the removal is part of
                                // THIS process's dashboard handover and is
                                // never journaled, so resuming the departing
                                // session later still shows its worker rows.
                                let mut stale_lanes = subs.lanes();
                                stale_lanes.append(&mut replayed_lanes);
                                stale_lanes.sort();
                                stale_lanes.dedup();
                                for lane in stale_lanes {
                                    let _ = deck_tx.send(Inbound::Deregister { agent: lane });
                                }

                                // Adopt the target: same id, this pid, waiting.
                                // Re-key everything that names the session —
                                // the lead's claim holder and the PR monitor's
                                // store/notification attribution follow the
                                // deck, not the process's first session.
                                session_record = crate::session_persist::adopt_record(
                                    rs.record.clone(),
                                    stella_store::SessionStatus::NeedsInput,
                                );
                                let _ = session_registry.upsert(&session_record);
                                sidecar_dir = session_registry.sidecar_dir(&session_record.id);
                                lead_holder = format!("{}/lead", session_record.id);
                                *pr_session_id.lock().unwrap_or_else(|p| p.into_inner()) =
                                    session_record.id.clone();
                                {
                                    let mut sink =
                                        journal_sink.lock().unwrap_or_else(|p| p.into_inner());
                                    match stella_store::journal::SessionJournal::open(&sidecar_dir)
                                    {
                                        Ok(j) => sink.swap(Some(j)),
                                        Err(e) => {
                                            sink.swap(None);
                                            let _ = deck_tx.send(chrome_note(format!(
                                                "session journaling unavailable — this session \
                                                 will no longer be resumable ({e})"
                                            )));
                                        }
                                    }
                                }

                                // Blank the lead pane, replay the adopted
                                // transcript in its place (direct sends — a
                                // replay must never re-journal itself), then
                                // restore conversation, backlog, and pipeline.
                                // (The departing session's worker rows were
                                // deregistered above; the lanes THIS replay
                                // creates are remembered for the next switch.)
                                let _ = deck_tx.send(Inbound::SessionReset {
                                    agent: LEAD.to_string(),
                                });
                                replayed_lanes =
                                    crate::session_persist::journal_lanes(&rs.records, LEAD);
                                crate::session_persist::replay_session(
                                    std::mem::take(&mut rs.records),
                                    now_ms(),
                                    LEAD,
                                    &deck_tx,
                                );
                                messages = crate::session_persist::restore_messages(
                                    rs.history.take().unwrap_or_default(),
                                    &system_prompt,
                                );
                                // Interrupted prompts (any lane's unsettled
                                // dispatch) go back at the FRONT, ahead of the
                                // stored backlog, in their original order.
                                let mut restored = std::mem::take(&mut rs.interrupted);
                                restored.extend(std::mem::take(&mut rs.queue));
                                dispatch = HoldState::new();
                                dispatch.held = !restored.is_empty();
                                for text in restored.iter().rev() {
                                    let _ = in_tx.send(Inbound::PromptRequeued {
                                        agent: LEAD.to_string(),
                                        text: text.clone(),
                                    });
                                }
                                queue.adopt(sidecar_dir.clone(), restored);
                                pipeline_on = rs.pipeline.unwrap_or(true);
                                let _ = in_tx.send(Inbound::Pipeline(pipeline_on));
                                // `--budget` means THIS session, decided and
                                // implemented on both resume paths: reseed
                                // the guard's session accumulator to what
                                // the adopted session had journaled
                                // (`ResumeState::spent_usd`, its last
                                // `BudgetTick` — the same derivation the
                                // startup resume uses). No synthetic tick is
                                // emitted; the next real turn's ticks
                                // reflect the reseeded guard naturally.
                                budget.reseed_session_spend(rs.spent_usd.unwrap_or(0.0));

                                // Fresh meta over the replayed one (pid, model,
                                // clock), back to waiting-on-you, and a fresh
                                // overlay snapshot reflecting the handover.
                                let mut meta =
                                    AgentMeta::new(LEAD, workspace_name.clone(), now_ms())
                                        .with_role("lead")
                                        .with_pid(std::process::id());
                                meta.model = Some(format!("{}/{}", cfg.provider.id, cfg.model_id));
                                let _ = in_tx.send(Inbound::Register(meta));
                                let _ = in_tx.send(Inbound::Status {
                                    agent: LEAD.to_string(),
                                    status: AgentStatus::WaitingInput,
                                });
                                let _ = deck_tx.send(chrome_note(match queue.len() {
                                    0 => "session restored — the conversation continues where \
                                          it left off."
                                        .to_string(),
                                    n => format!(
                                        "session restored — {n} prompt(s) waiting, dispatch \
                                         held. Submit anything to run it first, or ctrl+t to \
                                         edit the queue."
                                    ),
                                }));
                                let _ = in_tx.send(sessions_inbound(
                                    &session_registry,
                                    &session_record.id,
                                    &workspace_path,
                                ));
                            }
                        }
                        continue 'session;
                    }
                    // Fallthrough for everything else, serviced between turns
                    // (install/search hit the network, so they must not stall a
                    // live turn): MCP tab actions first, then the session-registry
                    // / inbox verbs, then the INSTALLED AGENTS pane's synchronous
                    // filesystem ops, then the ISSUES tab's spawned tracker ops.
                    // A stray answer/decision/control with no turn in flight
                    // falls through all four no-ops.
                    Some(other) => {
                        if !service_mcp_action(
                            &other,
                            cfg,
                            mcp_slot.get().map(Arc::as_ref),
                            &mcp_disabled,
                            &in_tx,
                        )
                        .await
                            && !service_registry_action(
                                &other,
                                &session_registry,
                                &session_record.id,
                                &workspace_path,
                                &in_tx,
                            )
                            && !handle_agents_input(&other, cfg, &in_tx)
                            && !handle_issues_input(&other, cfg, &issue_backend_cache, &in_tx)
                        {
                            handle_engine_config_input(&other, cfg, &in_tx);
                        }
                        continue 'session;
                    }
                }
            }
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
            agent::remaining_budget(&budget),
        )
        .await;
        if matches!(command, DeckCommand::Handled | DeckCommand::InitCompleted) {
            // A handled command emits its answer as `Text`, which flips the
            // lead to `Running` in the deck's fold — but no turn is in flight.
            // Return it to `WaitingInput` so the dashboard reflects reality.
            // (That status is also the journal's settle marker for this
            // prompt — a resume must not re-run `/clear`.)
            let _ = in_tx.send(Inbound::Status {
                agent: LEAD.to_string(),
                status: AgentStatus::WaitingInput,
            });
            // `/clear` (and friends) may have rewritten the conversation —
            // keep the boundary snapshot current before the next dispatch.
            let _ = crate::session_persist::snapshot_history(&sidecar_dir, &messages);
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
                memory =
                    SessionMemory::open_with_authority(&cfg.workspace_root, false, &cfg.authority);
                if let Some(snapshot) = agent::graph_snapshot(&cfg.workspace_root) {
                    let _ = in_tx.send(Inbound::GraphSnapshot(snapshot));
                }
                // `/init` may also have adopted new custom commands/skills —
                // reload them and refresh the deck's slash menu in place,
                // reporting anything that failed to load (then restoring the
                // idle status the report's Text event flipped).
                custom = crate::extensions::CustomExtensions::load_with_authority(
                    &cfg.workspace_root,
                    &cfg.authority,
                );
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

        // A real model turn is about to run — announce the work machine-wide.
        // The first prompt names the session (`<workspace>: <prompt…>`),
        // every prompt refreshes the summary, and the phase flips to
        // In Progress for other decks' SESSIONS overlays. Uses `submitted`
        // (what the user typed), never a custom command's expansion.
        if session_record.summary.is_empty() {
            session_record.title = format!("{workspace_name}: {}", prompt_line(&submitted, 48));
        }
        session_record.summary = prompt_line(&submitted, 240);
        session_record.status = stella_store::SessionStatus::InProgress;
        // Advertise which slices this session is mid-mapping (its live draft
        // explorations), so other decks' SESSIONS overlays can warn before a
        // prompt duplicates the work. Cheap: JSON parse, no hashing.
        session_record.exploring = stella_tools::exploration::draft_slices_for_pid(
            &cfg.workspace_root,
            std::process::id(),
        );
        let _ = session_registry.upsert(&session_record);

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
            // Attach any media files the prompt names (including `⌃V`
            // clipboard images, which arrive as their stored payload path).
            messages.push(crate::attachments::user_message(&prompt));
        }
        let reflect_start = messages.len();

        // The execution record outlives the turn future so a cancelled turn
        // can still be closed out in the store.
        // The session link (store schema v8) is what lets the SESSIONS
        // overlay's `Enter` reassemble and replay the full journal long
        // after this process is gone.
        let execution = agent::begin_execution(
            &store,
            if pipeline_on { "deck-pipeline" } else { "deck" },
            &prompt,
            cfg,
            Some(&session_record.id),
        );
        let files_before = registry.files_touched().len();
        let started_unix = crate::memory::unix_now_secs();

        // Skill-version usage telemetry: record which skills recall selected for
        // this turn, at their pinned version, keyed to this execution. Recorded
        // at turn start (the skills are injected regardless of how the turn
        // ends); best-effort, and only for the deck path for now — the other
        // `record_execution_end` sites can adopt it later.
        if let (Some((store, id)), Some(m)) = (&execution, &memory) {
            let selected = m.selected_skills(&prompt);
            if !selected.is_empty() {
                let versions = crate::skill_manager::pinned_versions(&cfg.workspace_root);
                let rows: Vec<stella_store::SkillUsageRow> = selected
                    .into_iter()
                    .map(|(skill, reason)| stella_store::SkillUsageRow {
                        version: versions.get(&skill).copied().unwrap_or(1),
                        skill,
                        reason,
                    })
                    .collect();
                let _ = store.record_skill_usage(*id, &rows);
            }
        }

        // Resolve the turn's tool executor from the MCP slot at dispatch:
        // connected servers join the session the moment the background
        // connect lands, and a turn that beats it runs on native tools —
        // narrated once, never silently degraded.
        // Cloned once per turn (an `Arc` clone, not a reconnect) so it can
        // also be shared into Best-of-N candidates below (issue #248 Ph1).
        let mcp = mcp_slot.get().cloned();
        let base_tools: &dyn ToolExecutor = match &mcp {
            Some(set) => set.as_ref(),
            None => &*registry,
        };
        if mcp_configured && mcp.is_none() && !mcp_pending_noted {
            mcp_pending_noted = true;
            let _ = in_tx.send(Inbound::Event {
                agent: LEAD.to_string(),
                event: AgentEvent::Text {
                    delta: "MCP servers are still connecting — this turn runs with native \
                            tools; connected servers join from the next turn"
                        .to_string(),
                },
            });
        }

        let dispatch_spend_usd = budget.session_spent_usd();

        // Shared with the live input arms below: `>` steers, Esc soft-stops.
        // Per-turn by construction — a stop latched here can't leak into
        // the next turn.
        let steering = subsession::SteeringTap::default();
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
                        &active_rules,
                        &registry_options,
                        execution.clone(),
                        &in_tx,
                        &ask_io,
                        &sup_tx,
                        &lead_holder,
                        &discovery_activation,
                        &steering,
                        mcp.clone(),
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
                        &sup_tx,
                        &lead_holder,
                        &discovery_activation,
                        &steering,
                    )
                    .await
                }
            };
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    outcome = &mut turn => break TurnEnd::Finished(outcome),
                    // Supervisor traffic is serviced while the lead works —
                    // that is the point: a task_assign spawns its worker
                    // mid-turn, and a worker ending frees its slot for the
                    // next backlogged prompt without waiting for the lead.
                    Some(msg) = sup_rx.recv() => {
                        handle_supervisor_msg(
                            msg,
                            &mut subs,
                            &mut pending_restarts,
                            &mut pending_spawns,
                            &mut queue,
                            dispatch.held(),
                            &registry,
                            &store,
                            &session_record.id,
                            &workspace_name,
                            cfg,
                            budget_limit,
                            &mut unmetered_spend,
                            &pr_nudge,
                            &in_tx,
                            &sup_tx,
                        );
                    }
                    input = sub_rx.recv() => match input {
                        None | Some(WorkspaceInput::Quit) => break TurnEnd::Quit,
                        // The lead is busy — the prompt does NOT wait for it.
                        // It backlogs and immediately drains to a dedicated
                        // sub-session if a worker slot is free ("the agent's
                        // job is to spawn a sub-session just for that
                        // request"); only slot exhaustion or a slash command
                        // leaves it queued for the lead.
                        Some(WorkspaceInput::Enqueue { text })
                        | Some(WorkspaceInput::ToAgent {
                            input: UserInput::Prompt { text, .. }, ..
                        }) => {
                            // `>`-prefix = steer THIS turn (step-boundary
                            // injection; the `Steered` event is the ack).
                            // Works for both the step-loop lead turn and the
                            // pipeline execute engine — both drain the tap.
                            if let Some(steer) = text.trim_start().strip_prefix('>') {
                                steering.push(steer.trim_start().to_string());
                                continue;
                            }
                            queue.push_back(text);
                            subsession::drain_queue(
                                &mut queue,
                                &mut subs,
                                dispatch.held(),
                                cfg,
                                budget_limit,
                                &session_record.id,
                                &workspace_name,
                                &in_tx,
                                &sup_tx,
                            );
                        }
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
                        // Stop routes by lane: aimed at the lead it cancels
                        // this turn; aimed at a worker it stops THAT worker
                        // and the lead's turn keeps running.
                        Some(WorkspaceInput::ToAgent { input: UserInput::Cancel, agent })
                        | Some(WorkspaceInput::Control {
                            control: stella_tui::AgentControl::Stop, agent,
                        }) => {
                            if agent == LEAD {
                                // Pipeline turns accept mid-turn `>` steering
                                // (the execute engine drains the tap) but the
                                // STOP stays a hard cancel: a pipeline is
                                // triage→…→judge, so a mid-execute soft stop
                                // has no single obvious continuation. Only the
                                // step-loop turn soft-stops.
                                if pipeline_on {
                                    break TurnEnd::Cancelled { hold: false };
                                }
                                // First Esc = SOFT stop: end at the next
                                // boundary keeping completed steps. The
                                // pair's second press (StopAndHold below)
                                // stays the immediate hard cancel.
                                steering.request_soft_stop();
                                let _ = in_tx.send(Inbound::Event {
                                    agent: LEAD.to_string(),
                                    event: AgentEvent::Text {
                                        delta: "\n[stopping at the next step boundary — Esc again to cancel immediately]\n".to_string(),
                                    },
                                });
                            } else {
                                subs.stop(&agent);
                            }
                        }
                        // Worker Pause/Resume/Restart while the lead works.
                        Some(WorkspaceInput::Control { agent, control }) if agent != LEAD => {
                            service_worker_control(
                                &agent,
                                control,
                                &mut subs,
                                &mut pending_restarts,
                                cfg,
                                budget_limit,
                                &session_record.id,
                                &workspace_name,
                                &in_tx,
                                &sup_tx,
                            );
                        }
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
                        }
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
                        // SKILLS-tab ops run alongside the in-flight turn (disk
                        // ops inline, npx/model ops spawned) — the manager stays
                        // usable while an agent is working. Create spawns its own
                        // provider, so unlike AgentCreate it needs no parking.
                        Some(WorkspaceInput::Skill(op)) => {
                            handle_skills_input(
                                &op,
                                cfg,
                                &in_tx,
                                &skill_registry,
                                budget_limit,
                            );
                        }
                        // MCP tab: a live enable/disable toggle mid-turn is
                        // honored immediately — it only flips the shared set the
                        // tool layer already consults, so the next model call in
                        // this turn sees the change (the tab display refreshes at
                        // the next idle snapshot). The other MCP actions (search,
                        // install, remove, auth) touch config/network and are
                        // serviced between turns; mid-turn they are no-ops.
                        Some(WorkspaceInput::McpToggle { name }) => {
                            let mut set =
                                mcp_disabled.lock().unwrap_or_else(|p| p.into_inner());
                            if !set.remove(&name) {
                                set.insert(name);
                            }
                        }
                        Some(WorkspaceInput::McpSearch { .. })
                        | Some(WorkspaceInput::McpInstall { .. })
                        | Some(WorkspaceInput::McpRemove { .. })
                        | Some(WorkspaceInput::McpAuth { .. })
                        | Some(WorkspaceInput::McpRefresh) => {}
                        // OAuth login is a spawned browser round-trip — safe
                        // to start mid-turn (its transport picks the tokens
                        // up lazily on the next call either way).
                        Some(WorkspaceInput::McpOauthLogin { server }) => {
                            spawn_mcp_oauth_login(
                                server,
                                cfg.workspace_root.clone(),
                                in_tx.clone(),
                            );
                        }
                        // The SESSIONS overlay and the inbox stay live while a
                        // turn runs — cheap local file reads/writes, exactly
                        // like the INSTALLED AGENTS pane above.
                        Some(
                            input @ (WorkspaceInput::SessionsRefresh
                            | WorkspaceInput::SessionOpen { .. }
                            | WorkspaceInput::SessionArchive { .. }
                            | WorkspaceInput::SessionDelete { .. }
                            | WorkspaceInput::NotificationRead { .. }
                            | WorkspaceInput::NotificationsReadAll),
                        ) => {
                            service_registry_action(
                                &input,
                                &session_registry,
                                &session_record.id,
                                &workspace_path,
                                &in_tx,
                            );
                        }
                        // Navigation waits for the road to clear: switching
                        // sessions mid-turn would tear down live work, so the
                        // deck is told how to proceed instead.
                        Some(WorkspaceInput::SessionResume { .. }) => {
                            let _ = deck_tx.send(chrome_note(
                                "a turn is running — esc stops it (esc esc holds the queue \
                                 too), then press ⏎ on the session again."
                                    .to_string(),
                            ));
                        }
                        // The ENGINE overlay stays live while a turn runs —
                        // settings reads/writes are cheap local filesystem
                        // ops, exactly like the INSTALLED AGENTS pane. A
                        // mid-turn save applies to runs started afterwards;
                        // the in-flight turn keeps its resolved models.
                        Some(
                            input @ (WorkspaceInput::EngineConfigSave { .. }
                            | WorkspaceInput::EngineConfigRefresh),
                        ) => {
                            handle_engine_config_input(&input, cfg, &in_tx);
                        }
                        // The ISSUES tab stays live while a turn runs too —
                        // every op spawns its own task and answers from it,
                        // so nothing here blocks the event pump.
                        Some(
                            input @ (WorkspaceInput::IssuesRefresh { .. }
                            | WorkspaceInput::IssueCreate { .. }
                            | WorkspaceInput::IssueAct { .. }
                            | WorkspaceInput::EntitySearch { .. }),
                        ) => {
                            handle_issues_input(&input, cfg, &issue_backend_cache, &in_tx);
                        }
                        // Scope review is not engine-driven yet, and
                        // lead-lane pause/resume/restart still need a
                        // staged-pipeline boundary gate (the PipelinePorts
                        // follow-up; the fleet layer's own per-task verbs
                        // exist now, but fleet tasks are not deck lanes
                        // yet) — named seams, no-ops here.
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
                    if reason == stella_core::SOFT_STOP_REASON {
                        // A user choice, not a failure: no Error row — the
                        // work is kept and the next prompt continues from it.
                        let _ = in_tx.send(Inbound::Event {
                            agent: LEAD.to_string(),
                            event: AgentEvent::Text {
                                delta: "\n[stopped at the step boundary — completed work kept]\n"
                                    .to_string(),
                            },
                        });
                    } else {
                        // An aborted turn emits no `Complete`; this row flips
                        // the dashboard to failed AND clears any pending gate.
                        let _ = in_tx.send(Inbound::Event {
                            agent: LEAD.to_string(),
                            event: AgentEvent::Error {
                                message: reason.clone(),
                                retryable: false,
                            },
                        });
                    }
                }
                authoring::record_and_reflect_turn(
                    &mut memory,
                    &prompt,
                    &outcome,
                    &registry,
                    files_before,
                    started_unix,
                    &messages,
                    reflect_start,
                    &*provider,
                    cfg,
                    &mut budget,
                    &in_tx,
                )
                .await;
                session_exit = if outcome.is_err() {
                    stella_store::SessionStatus::Error
                } else {
                    stella_store::SessionStatus::Complete
                };
                session_record.status = stella_store::SessionStatus::NeedsInput;
                let _ = session_registry.upsert(&session_record);
                let turn_secs = crate::memory::unix_now_secs().saturating_sub(started_unix);
                let inbox = stella_store::NotificationStore::open_default();
                if let Err(reason) = &outcome {
                    let _ = inbox.push(
                        &stella_store::Notification::new(
                            format!("{workspace_name}: turn failed"),
                            format!("{} — {reason}", prompt_line(&submitted, 80)),
                            session_record.id.clone(),
                        )
                        .with_session_id(session_record.id.clone()),
                    );
                } else if turn_secs >= LONG_TURN_NOTIFY_SECS {
                    let _ = inbox.push(
                        &stella_store::Notification::new(
                            format!("{workspace_name}: work finished ({turn_secs}s)"),
                            prompt_line(&submitted, 160),
                            session_record.id.clone(),
                        )
                        .with_session_id(session_record.id.clone()),
                    );
                }
                // The turn may have committed / pushed / opened a PR —
                // reconcile now instead of waiting out the poll interval.
                pr_nudge.notify_one();
                // Mirror the lead's final board into the store's `tasks`
                // table — cross-session findability for what this turn
                // planned and finished (the event-log copy already rode the
                // forwarder for replay).
                if let Some((store, id)) = &execution {
                    let board = registry.task_board();
                    let items: Vec<TaskItem> = board
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .items()
                        .to_vec();
                    if !items.is_empty() {
                        let _ = store.record_task_board(
                            *id,
                            Some(&session_record.id),
                            &items,
                            now_ms(),
                        );
                    }
                }
            }
            TurnEnd::Cancelled { hold } => {
                // Erase the partial turn: the next prompt continues from the
                // last committed conversation state.
                messages.truncate(turn_base);
                // The dropped turn future never reached its own claim
                // release — free the lead's write claims by holder so
                // workers (and other sessions) aren't blocked on a turn
                // that no longer exists.
                if let Some(store) = &store {
                    let _ = store.release_file_locks_for_holder(&lead_holder);
                }
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
                    dispatch.cancelled(&submitted);
                }
                let cancelled_cost =
                    agent::settled_cost_since(dispatch_spend_usd, budget.session_spent_usd());
                if let Some((store, id)) = &execution
                    && !agent::record_execution_end(
                        store,
                        *id,
                        registry.as_ref(),
                        "cancelled",
                        cancelled_cost,
                        false,
                    )
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
                // Registry: an interrupted turn leaves the session waiting on
                // the user; if the deck exits from this state, it exits
                // Cancelled (the user abandoned the interrupted work).
                session_exit = stella_store::SessionStatus::Cancelled;
                session_record.status = stella_store::SessionStatus::NeedsInput;
                let _ = session_registry.upsert(&session_record);
            }
            // Quit landing mid-turn: erase the partial turn from the
            // conversation before the boundary snapshot below — a dangling
            // assistant tool call with no result is a broken history, and
            // the journal's unsettled `PromptStarted` puts this prompt back
            // at the front of the queue on resume anyway.
            TurnEnd::Quit => {
                messages.truncate(turn_base);
                break 'session;
            }
        }

        // Durable turn boundary: the conversation as committed (post-turn or
        // post-cancel-truncation) — what a resume continues from. The queue
        // is write-through already; its one-time failure warning surfaces
        // here, on the same cadence as every other persistence warning.
        if let Some(warning) = crate::session_persist::snapshot_history(&sidecar_dir, &messages)
            .or_else(|| queue.take_warning())
        {
            let _ = in_tx.send(Inbound::Event {
                agent: LEAD.to_string(),
                event: AgentEvent::Error {
                    message: warning,
                    retryable: true,
                },
            });
        }

        // A creation request parked during the turn: the provider is free
        // again, so draft + install it before the next dispatch.
        if let Some((description, scope)) = pending_create.take() {
            handle_agent_create(
                &description,
                scope,
                cfg,
                &*provider,
                agent::remaining_budget(&budget),
                &in_tx,
            )
            .await;
        }
    }

    // The session is over — leave the registry record in its terminal state
    // and the durable state current. (A crash never reaches here; readers
    // downgrade a dead pid to Error — and the journal makes even that
    // resumable.) Quitting with prompts still queued is a PAUSE now, not an
    // abandonment: the backlog is durable and reopens intact. The journal
    // syncs HERE, not just in the tee's own teardown — background senders
    // (the inbox poller) keep the tee alive past this point, and runtime
    // teardown must never be what a buffered tail was waiting on.
    journal_sink
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .sync();
    let _ = crate::session_persist::snapshot_history(&sidecar_dir, &messages);
    session_record.status = if queue.is_empty() {
        session_exit
    } else {
        stella_store::SessionStatus::Paused
    };
    let _ = session_registry.upsert(&session_record);

    // Closing our inbound sender ends the deck's stream if the user hasn't
    // already quit (the journal tee drains, fsyncs, and forwards the close);
    // then wait for it to restore the terminal.
    drop(in_tx);
    let deck_result = deck.await;
    if let Some(set) = mcp_slot.get() {
        set.close_all().await;
    }
    match deck_result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("deck terminal error: {e}")),
        Err(e) => Err(format!("deck task failed: {e}")),
    }
}

/// Run the MCP connect on its own task, landing the connected set in `slot`
/// for turns to pick up at dispatch. Returns whether any servers are
/// configured at all (`false` = the slot will stay empty forever, so no
/// "still connecting" note is ever warranted). Always seeds the MCP tab and
/// releases the splash leg, whatever the plan resolves to.
///
/// Connect narration is session chrome (`chrome_tx`, the direct deck path):
/// it re-runs at every boot, so journaling it would pile stale "connecting…"
/// lines onto every resumed transcript. The status flips ride the journaled
/// `in_tx` — `waiting_input` is also the journal's settle marker.
fn spawn_mcp_connect(
    cfg: Config,
    registry: Arc<ToolRegistry>,
    disabled: stella_mcp::DisabledServers,
    slot: Arc<tokio::sync::OnceCell<Arc<stella_mcp::McpToolSet>>>,
    in_tx: UnboundedSender<Inbound>,
    chrome_tx: UnboundedSender<Inbound>,
    release_splash: impl FnOnce() + Send + 'static,
) -> bool {
    let plan = agent::load_mcp_plan(&cfg);
    let configured = matches!(plan, agent::McpPlan::Servers(_));
    tokio::spawn(async move {
        match plan {
            agent::McpPlan::None => {}
            agent::McpPlan::Invalid(reason) => {
                let _ = chrome_tx.send(chrome_note(reason));
                let _ = in_tx.send(Inbound::Status {
                    agent: LEAD.to_string(),
                    status: AgentStatus::WaitingInput,
                });
            }
            agent::McpPlan::Servers(servers) => {
                let _ = chrome_tx.send(chrome_note(format!(
                    "connecting {} MCP server(s)…",
                    servers.len()
                )));
                match crate::mcp_cmd::oauth_manager(&cfg.workspace_root) {
                    Ok(auth) => {
                        let set = agent::connect_mcp_servers(
                            &servers,
                            registry.clone(),
                            Some(registry.mcp_usage_ledger()),
                            Some(disabled.clone()),
                            Some(auth),
                        )
                        .await;
                        let _ = chrome_tx.send(chrome_note(crate::mcp_cmd::mcp_outcome_report(
                            &set.connected_names(),
                            set.failed_servers(),
                        )));
                        // `set` is infallible here (the cell is set exactly once,
                        // by this task); an in-flight turn keeps its resolved
                        // executor and the NEXT turn picks the servers up. Arc'd so
                        // a turn can share it into Best-of-N candidates (#248 Ph1).
                        let _ = slot.set(Arc::new(set));
                    }
                    Err(error) => {
                        let _ = chrome_tx.send(chrome_note(format!(
                            "MCP authentication unavailable: {error} — continuing with native tools only"
                        )));
                    }
                }
                // The Text events above fold the lead to `Running`, but no
                // turn is in flight — restore the idle status or the
                // dashboard would show a busy lead forever.
                let _ = in_tx.send(Inbound::Status {
                    agent: LEAD.to_string(),
                    status: AgentStatus::WaitingInput,
                });
            }
        }
        // Seed the MCP tab with the configured servers and their live state.
        send_mcp_snapshot(&cfg, slot.get().map(Arc::as_ref), &disabled, &in_tx).await;
        // MCP connect settled (or there was nothing to connect) — the other
        // init leg the launch splash waits on.
        release_splash();
    });
    configured
}

/// Build the MCP tab snapshot: every configured server (`.stella/mcp.toml`)
/// joined with its live session state — enabled (not in the disabled set),
/// connected (in the live tool set), health, per-server tool count (derived
/// from the advertised schemas, so it is 0 the moment a server is disabled),
/// configured credential field names, and total recorded tool calls.
async fn mcp_snapshot(
    cfg: &Config,
    mcp: Option<&stella_mcp::McpToolSet>,
    disabled: &stella_mcp::DisabledServers,
) -> Result<Vec<stella_tui::McpServerInfo>, String> {
    let config = crate::mcp_cmd::load_config(&cfg.workspace_root)?;
    let connected: std::collections::HashSet<String> = mcp
        .map(|s| {
            s.connected_names()
                .into_iter()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let health = match mcp {
        Some(s) => s.health().await,
        None => Vec::new(),
    };
    let schemas = mcp.map(|s| s.schemas()).unwrap_or_default();
    let usage = crate::mcp_cmd::usage_stats(&cfg.workspace_root)?;
    let disabled_set = disabled.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let oauth_logins: std::collections::HashSet<String> =
        crate::mcp_cmd::oauth_logged_in(&cfg.workspace_root)?
            .into_iter()
            .collect();

    Ok(config
        .names()
        .into_iter()
        .map(|name| {
            let transport = config.get(name).expect("name came from the config");
            let enabled = !disabled_set.contains(name);
            let connected_now = connected.contains(name);
            let prefix = format!("mcp__{name}__");
            let tool_count = schemas
                .iter()
                .filter(|s| s.name.starts_with(&prefix))
                .count();
            let health = health.iter().find(|h| h.name == name).map(|h| {
                match h.state {
                    stella_mcp::HealthState::Live => "live",
                    stella_mcp::HealthState::Reconnecting => "reconnecting",
                    stella_mcp::HealthState::Down => "down",
                }
                .to_string()
            });
            let calls: u64 = usage
                .iter()
                .filter(|s| s.server == name)
                .map(|s| s.calls.max(0) as u64)
                .sum();
            stella_tui::McpServerInfo {
                name: name.to_string(),
                kind: transport.kind_label().to_string(),
                enabled,
                connected: connected_now,
                health: connected_now.then_some(health).flatten(),
                tool_count,
                auth_fields: transport
                    .credential_names()
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                oauth: (transport.kind_label() == "http").then(|| oauth_logins.contains(name)),
                calls,
            }
        })
        .collect())
}

/// Build and push a fresh MCP tab snapshot.
async fn send_mcp_snapshot(
    cfg: &Config,
    mcp: Option<&stella_mcp::McpToolSet>,
    disabled: &stella_mcp::DisabledServers,
    in_tx: &mpsc::UnboundedSender<Inbound>,
) {
    match mcp_snapshot(cfg, mcp, disabled).await {
        Ok(rows) => {
            let _ = in_tx.send(Inbound::McpServers(rows));
        }
        Err(error) => {
            let _ = in_tx.send(Inbound::McpServers(Vec::new()));
            let _ = in_tx.send(Inbound::McpOauthStatus {
                server: "MCP state".to_string(),
                message: error,
                outcome: Some(false),
            });
        }
    }
}

/// Run a registry search and shape it for the tab, flagging already-configured
/// servers and deduping the registry's per-version rows to one per name.
async fn run_mcp_search(cfg: &Config, query: &str) -> stella_tui::McpSearchOutcome {
    let query = query.trim().to_string();
    let configured: std::collections::HashSet<String> =
        crate::mcp_cmd::load_config(&cfg.workspace_root)
            .map(|c| c.names().into_iter().map(str::to_string).collect())
            .unwrap_or_default();
    let registry_url = crate::mcp_cmd::resolve_registry_url(&cfg.workspace_root);
    match crate::mcp_cmd::search(&registry_url, Some(&query), None, 20).await {
        Ok(page) => {
            let mut seen = std::collections::HashSet::new();
            let items = page
                .entries
                .into_iter()
                .filter(|e| seen.insert(e.server.name.clone()))
                .map(|e| {
                    let alias = e.server.default_alias();
                    stella_tui::McpSearchItem {
                        installed: configured.contains(&e.server.name)
                            || configured.contains(&alias),
                        kinds: crate::mcp_cmd::install_kinds(&e.server),
                        description: e.server.description.clone().unwrap_or_default(),
                        name: e.server.name,
                    }
                })
                .collect();
            stella_tui::McpSearchOutcome {
                query,
                items,
                error: None,
                has_more: page.next_cursor.is_some(),
            }
        }
        Err(error) => stella_tui::McpSearchOutcome {
            query,
            items: Vec::new(),
            error: Some(error),
            has_more: false,
        },
    }
}

/// Service one MCP-tab action from the deck. Returns `true` if `input` was an
/// MCP verb (so the caller skips its own dispatch). Search/install/remove/auth
/// touch `.stella/mcp.toml` (and, for search, the registry over HTTP); toggle
/// flips the shared disabled set that the tool layer consults live.
async fn service_mcp_action(
    input: &WorkspaceInput,
    cfg: &Config,
    mcp: Option<&stella_mcp::McpToolSet>,
    disabled: &stella_mcp::DisabledServers,
    in_tx: &mpsc::UnboundedSender<Inbound>,
) -> bool {
    match input {
        WorkspaceInput::McpToggle { name } => {
            {
                let mut set = disabled.lock().unwrap_or_else(|p| p.into_inner());
                if !set.remove(name) {
                    set.insert(name.clone());
                }
            }
            send_mcp_snapshot(cfg, mcp, disabled, in_tx).await;
        }
        WorkspaceInput::McpRefresh => send_mcp_snapshot(cfg, mcp, disabled, in_tx).await,
        WorkspaceInput::McpRemove { name } => {
            let _ = crate::mcp_cmd::remove(&cfg.workspace_root, name);
            send_mcp_snapshot(cfg, mcp, disabled, in_tx).await;
        }
        WorkspaceInput::McpAuth {
            server,
            field,
            value,
        } => {
            let _ = crate::mcp_cmd::set_credential(
                &cfg.workspace_root,
                server,
                field,
                value.reveal().to_string(),
            );
            send_mcp_snapshot(cfg, mcp, disabled, in_tx).await;
        }
        WorkspaceInput::McpSearch { query } => {
            let outcome = run_mcp_search(cfg, query).await;
            let _ = in_tx.send(Inbound::McpSearchResults(outcome));
        }
        WorkspaceInput::McpInstall { name } => {
            let registry_url = crate::mcp_cmd::resolve_registry_url(&cfg.workspace_root);
            if let Ok((alias, option)) = crate::mcp_cmd::resolve_install(&registry_url, name).await
            {
                let _ = crate::mcp_cmd::install(&cfg.workspace_root, &alias, option.transport);
            }
            send_mcp_snapshot(cfg, mcp, disabled, in_tx).await;
        }
        WorkspaceInput::McpOauthLogin { server } => {
            spawn_mcp_oauth_login(server.clone(), cfg.workspace_root.clone(), in_tx.clone());
        }
        _ => return false,
    }
    true
}

/// Registry hygiene: terminal session records older than this are swept at
/// deck startup (30 days).
const SESSION_RECORD_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Inbox hygiene: **read** notifications older than this are swept at deck
/// startup (14 days). Unread ones persist regardless — that is the contract.
const NOTIFICATION_MAX_AGE_MS: u64 = 14 * 24 * 60 * 60 * 1000;
/// How often the deck re-reads the machine-wide notification store.
const NOTIFY_POLL_MS: u64 = 3_000;
/// A successful turn at least this long lands a "work finished" notification
/// — long enough that the user has plausibly looked away.
const LONG_TURN_NOTIFY_SECS: i64 = 60;

/// One prompt flattened to a single display line, char-safe-truncated — the
/// session registry's title/summary shape.
pub(crate) fn prompt_line(prompt: &str, max_chars: usize) -> String {
    let flat: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let head: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Service a session-registry / inbox verb from the deck. Returns `true` if
/// `input` was one (so the caller skips its own dispatch). All of these are
/// cheap local file ops, serviced identically idle or mid-turn.
fn service_registry_action(
    input: &WorkspaceInput,
    registry: &stella_store::SessionRegistry,
    my_session_id: &str,
    workspace: &str,
    in_tx: &mpsc::UnboundedSender<Inbound>,
) -> bool {
    match input {
        WorkspaceInput::SessionsRefresh => {
            let _ = in_tx.send(sessions_inbound(registry, my_session_id, workspace));
        }
        WorkspaceInput::SessionOpen { id } => {
            spawn_session_replay(id.clone(), registry.list(), in_tx.clone());
        }
        WorkspaceInput::SessionArchive { id } => {
            let _ = registry.set_status(id, stella_store::SessionStatus::Archived);
            let _ = in_tx.send(sessions_inbound(registry, my_session_id, workspace));
        }
        WorkspaceInput::SessionDelete { id } => {
            // The deck refuses to delete its own record UI-side too; this is
            // the belt-and-suspenders check.
            if id != my_session_id {
                let _ = registry.remove(id);
            }
            let _ = in_tx.send(sessions_inbound(registry, my_session_id, workspace));
        }
        WorkspaceInput::NotificationRead { id } => {
            let store = stella_store::NotificationStore::open_default();
            let _ = store.mark_read(id);
            let _ = in_tx.send(notifications_inbound(&store));
        }
        WorkspaceInput::NotificationsReadAll => {
            let store = stella_store::NotificationStore::open_default();
            let _ = store.mark_all_read();
            let _ = in_tx.send(notifications_inbound(&store));
        }
        _ => return false,
    }
    true
}

/// The SESSIONS overlay snapshot: every registry record mapped to the deck's
/// [`stella_tui::SessionInfo`], flagging this process's own record and the
/// rows that can be reopened HERE (no live owner, this workspace, durable
/// state on disk — ⏎ navigates into those).
fn sessions_inbound(
    registry: &stella_store::SessionRegistry,
    mine: &str,
    workspace: &str,
) -> Inbound {
    let sessions = registry
        .list()
        .into_iter()
        .map(|r| {
            // A session mid-mapping advertises its slices right in the
            // summary line, so a human sees "already being mapped" before
            // typing a prompt that would duplicate the exploration.
            let summary = if r.exploring.is_empty() {
                r.summary
            } else {
                format!("{} [mapping: {}]", r.summary, r.exploring.join(", "))
            };
            stella_tui::SessionInfo {
                mine: r.id == mine,
                resumable: r.id != mine && r.workspace == workspace && registry.resumable(&r.id),
                phase: session_phase(r.status),
                id: r.id,
                title: r.title,
                summary,
                workspace: r.workspace,
                started_ms: r.started_at_ms,
                updated_ms: r.updated_at_ms,
            }
        })
        .collect();
    Inbound::Sessions(sessions)
}

/// Store status → TUI phase (the TUI mirrors the enum so it never links the
/// store crate).
fn session_phase(status: stella_store::SessionStatus) -> stella_tui::SessionPhase {
    match status {
        stella_store::SessionStatus::InProgress => stella_tui::SessionPhase::InProgress,
        stella_store::SessionStatus::NeedsInput => stella_tui::SessionPhase::NeedsInput,
        stella_store::SessionStatus::Paused => stella_tui::SessionPhase::Paused,
        stella_store::SessionStatus::Cancelled => stella_tui::SessionPhase::Cancelled,
        stella_store::SessionStatus::Complete => stella_tui::SessionPhase::Complete,
        stella_store::SessionStatus::Archived => stella_tui::SessionPhase::Archived,
        stella_store::SessionStatus::Error => stella_tui::SessionPhase::Error,
    }
}

/// The inbox snapshot for the deck (badge + overlay), newest first.
fn notifications_inbound(store: &stella_store::NotificationStore) -> Inbound {
    let items = store
        .list()
        .into_iter()
        .map(|n| stella_tui::NotificationInfo {
            id: n.id,
            title: n.title,
            body: n.body,
            source: n.source,
            created_ms: n.created_at_ms,
            read: n.read,
            session_id: n.session_id,
        })
        .collect();
    Inbound::Notifications(items)
}

/// Service one supervisor message: dispatch or park a `task_assign` spawn,
/// and on a worker's end free its slot, close the delegation loop (a task
/// worker succeeding completes its board task), meter the worker's spend
/// toward the session budget, nudge the PR monitor, then drain whatever the
/// freed slot can take — parked spawns first, then the prompt backlog.
#[allow(clippy::too_many_arguments)]
fn handle_supervisor_msg(
    msg: SupervisorMsg,
    subs: &mut SubSessions,
    pending_restarts: &mut std::collections::HashSet<String>,
    pending_spawns: &mut VecDeque<stella_core::tasks::SpawnRequest>,
    queue: &mut crate::session_persist::DurableQueue,
    dispatch_held: bool,
    registry: &ToolRegistry,
    store: &Option<Arc<Store>>,
    session_id: &str,
    workspace_name: &str,
    cfg: &Config,
    budget_limit: Option<f64>,
    unmetered_spend: &mut f64,
    pr_nudge: &Arc<tokio::sync::Notify>,
    in_tx: &UnboundedSender<Inbound>,
    sup_tx: &UnboundedSender<SupervisorMsg>,
) {
    match msg {
        SupervisorMsg::SpawnTask(request) => {
            if subs.has_slot() {
                subsession::spawn_task_worker(
                    &request,
                    subs,
                    cfg,
                    budget_limit,
                    session_id,
                    workspace_name,
                    in_tx,
                    sup_tx,
                );
            } else {
                pending_spawns.push_back(request);
            }
        }
        SupervisorMsg::Ended {
            lane,
            execution_id,
            cost_usd,
            end,
        } => {
            subs.ended(&lane);
            // A Restart that arrived while this worker was live respawns it
            // now — restart takes the freed slot ahead of parked spawns.
            if pending_restarts.remove(&lane) {
                let _ = subsession::respawn(
                    &lane,
                    subs,
                    cfg,
                    budget_limit,
                    session_id,
                    workspace_name,
                    in_tx,
                    sup_tx,
                );
            }
            // Worker spend reaches the session's parent budget guard (the
            // L-E9 discipline). The guard is mutably borrowed by any in-
            // flight lead turn, so the driver accumulates here and meters at
            // the loop top, the next safe boundary — budget aborts happen at
            // boundaries only, never mid-flight.
            *unmetered_spend += cost_usd;
            // A worker may have just pushed a branch / opened a PR — observe
            // now, not at the next 45s tick.
            pr_nudge.notify_one();
            // A task worker finishing successfully completes its board task
            // — the delegation loop closes without the lead's involvement. A
            // failed or stopped worker leaves the task in progress: the
            // board must not claim done what wasn't (the inbox notification
            // names a failure; a stop was the user's own act).
            if let Some(task_id) = lane.strip_prefix("sub:") {
                let board = registry.task_board();
                let items: Vec<TaskItem> = {
                    let mut guard = board.lock().unwrap_or_else(|p| p.into_inner());
                    if matches!(end, subsession::WorkerEnd::Done) {
                        let _ = guard.set_status(task_id, stella_protocol::TaskStatus::Completed);
                    }
                    guard.items().to_vec()
                };
                let _ = in_tx.send(Inbound::Event {
                    agent: LEAD.to_string(),
                    event: AgentEvent::TaskUpdate {
                        tasks: items.clone(),
                    },
                });
                if let (Some(store), Some(exec)) = (store.as_ref(), execution_id) {
                    let _ = store.record_task_board(exec, Some(session_id), &items, now_ms());
                }
            }
            while subs.has_slot()
                && let Some(request) = pending_spawns.pop_front()
            {
                subsession::spawn_task_worker(
                    &request,
                    subs,
                    cfg,
                    budget_limit,
                    session_id,
                    workspace_name,
                    in_tx,
                    sup_tx,
                );
            }
            subsession::drain_queue(
                queue,
                subs,
                dispatch_held,
                cfg,
                budget_limit,
                session_id,
                workspace_name,
                in_tx,
                sup_tx,
            );
        }
    }
}

/// Route one Pause/Resume/Stop/Restart at a worker lane. Pause parks the
/// worker at its next step boundary (never mid-tool — the engine's
/// `TurnGate`); Resume releases it; Restart respawns the lane from its
/// retained spec, stopping the live worker first when necessary.
#[allow(clippy::too_many_arguments)]
fn service_worker_control(
    lane: &str,
    control: stella_tui::AgentControl,
    subs: &mut SubSessions,
    pending_restarts: &mut std::collections::HashSet<String>,
    cfg: &Config,
    budget_limit: Option<f64>,
    session_id: &str,
    workspace_name: &str,
    in_tx: &UnboundedSender<Inbound>,
    sup_tx: &UnboundedSender<SupervisorMsg>,
) {
    match control {
        stella_tui::AgentControl::Stop => {
            subs.stop(lane);
        }
        stella_tui::AgentControl::Pause => {
            if subs.set_paused(lane, true) {
                let _ = in_tx.send(Inbound::Status {
                    agent: lane.to_string(),
                    status: AgentStatus::Paused,
                });
            }
        }
        stella_tui::AgentControl::Resume => {
            if subs.set_paused(lane, false) {
                let _ = in_tx.send(Inbound::Status {
                    agent: lane.to_string(),
                    status: AgentStatus::Running,
                });
            }
        }
        stella_tui::AgentControl::Restart => {
            if subs.is_live(lane) {
                pending_restarts.insert(lane.to_string());
                subs.stop(lane);
            } else {
                let _ = subsession::respawn(
                    lane,
                    subs,
                    cfg,
                    budget_limit,
                    session_id,
                    workspace_name,
                    in_tx,
                    sup_tx,
                );
            }
        }
    }
}

/// Open a session in a replay lane ([`WorkspaceInput::SessionOpen`]): load
/// its persisted journal from the session's own workspace store (linked via
/// `executions.session_id`, store schema v8) and stream it through the
/// deck's ordinary fold. Replay IS the fold — a session dead for 12 hours
/// reconstructs to exactly the state it reached, through the same rendering
/// path a live session uses. Heavy reads run on the blocking pool.
fn spawn_session_replay(
    id: String,
    records: Vec<stella_store::SessionRecord>,
    in_tx: mpsc::UnboundedSender<Inbound>,
) {
    tokio::task::spawn_blocking(move || {
        let Some(record) = records.into_iter().find(|r| r.id == id) else {
            let _ = in_tx.send(Inbound::Event {
                agent: LEAD.to_string(),
                event: AgentEvent::Text {
                    delta: format!("session {id} is no longer in the registry"),
                },
            });
            return;
        };
        // The prefix is the journal tee's filter key
        // (`session_persist::REPLAY_LANE_PREFIX`): everything on this lane
        // rides the ordinary inbound channel but must never be journaled as
        // the CURRENT session's history.
        let lane = format!("{}{id}", crate::session_persist::REPLAY_LANE_PREFIX);
        let meta = AgentMeta::new(lane.clone(), format!("replay — {}", record.title), now_ms())
            .with_role("replay");
        let _ = in_tx.send(Inbound::Register(meta));
        let lane_text = |delta: String| Inbound::Event {
            agent: lane.clone(),
            event: AgentEvent::Text { delta },
        };
        let Some(store) = agent::open_store(std::path::Path::new(&record.workspace)) else {
            let _ = in_tx.send(lane_text(format!(
                "no store found at {} — nothing to replay",
                record.workspace
            )));
            let _ = in_tx.send(Inbound::Status {
                agent: lane,
                status: AgentStatus::Failed,
            });
            return;
        };
        match store.session_events(&id) {
            Ok(journal) => {
                if journal.events.is_empty() {
                    let _ = in_tx.send(lane_text(
                        "no persisted events for this session (it predates session-linked \
                         journals, store schema v8)"
                            .to_string(),
                    ));
                }
                for rec in journal.events {
                    let _ = in_tx.send(Inbound::Event {
                        agent: lane.clone(),
                        event: rec.event,
                    });
                }
                if journal.skipped > 0 {
                    let _ = in_tx.send(lane_text(format!(
                        "{} event(s) could not be decoded and were skipped",
                        journal.skipped
                    )));
                }
                let _ = in_tx.send(Inbound::Status {
                    agent: lane,
                    status: match record.status {
                        stella_store::SessionStatus::Error => AgentStatus::Failed,
                        _ => AgentStatus::Done,
                    },
                });
            }
            Err(e) => {
                let _ = in_tx.send(lane_text(format!(
                    "failed to read the session journal: {e}"
                )));
                let _ = in_tx.send(Inbound::Status {
                    agent: lane,
                    status: AgentStatus::Failed,
                });
            }
        }
    });
}

/// How often the PR monitor re-reads `gh` (live reconcile, L-V3 — nothing
/// renders from cache; every push is a fresh observation).
const PR_POLL_MS: u64 = 45_000;

/// One reconciled PR observation, as compared for change detection.
#[derive(PartialEq, Clone)]
struct PrObservation {
    url: String,
    number: Option<u64>,
    status: PrStatus,
    ci: Option<CiStatus>,
}

/// Poll `gh` for the workspace's current-branch PR and its checks. On every
/// change: a `Pr` event on the lead lane (the deck folds it into the
/// footer's PR cell and the transcript), a store mirror row, and — when CI
/// flips to failing — a persist-until-read inbox notification linked to
/// this session. No PR (or no `gh`) is quietly nothing: the cell stays
/// hidden rather than wrong.
fn spawn_pr_monitor(
    root: PathBuf,
    session_id: Arc<std::sync::Mutex<String>>,
    store: Option<Arc<Store>>,
    workspace_name: String,
    nudge: Arc<tokio::sync::Notify>,
    in_tx: mpsc::UnboundedSender<Inbound>,
) {
    tokio::spawn(async move {
        let mut last: Option<PrObservation> = None;
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(PR_POLL_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // The tick paces routine reconciles; a nudge (turn settled,
            // worker ended) skips straight to one.
            tokio::select! {
                _ = tick.tick() => {}
                _ = nudge.notified() => {}
            }
            if in_tx.is_closed() {
                break;
            }
            let Some(observed) = observe_pr(&root).await else {
                continue;
            };
            if last.as_ref() == Some(&observed) {
                continue;
            }
            let ci_flipped_to_failing = observed.ci == Some(CiStatus::Failing)
                && last
                    .as_ref()
                    .is_none_or(|l| l.ci != Some(CiStatus::Failing));
            last = Some(observed.clone());
            // Resolved per observation: an in-deck session switch re-keys
            // which session this PR activity belongs to.
            let session_id = session_id.lock().unwrap_or_else(|p| p.into_inner()).clone();
            let _ = in_tx.send(Inbound::Event {
                agent: LEAD.to_string(),
                event: AgentEvent::Pr {
                    url: observed.url.clone(),
                    status: observed.status,
                    number: observed.number,
                    ci: observed.ci,
                },
            });
            if let Some(store) = &store {
                let _ = store.upsert_pull_request(
                    Some(&session_id),
                    &observed.url,
                    observed.number,
                    pr_status_token(observed.status),
                    observed.ci.map(ci_status_token),
                    now_ms(),
                );
            }
            if ci_flipped_to_failing {
                let number = observed
                    .number
                    .map(|n| format!("#{n}"))
                    .unwrap_or_else(|| observed.url.clone());
                let _ = stella_store::NotificationStore::open_default().push(
                    &stella_store::Notification::new(
                        format!("{workspace_name}: CI failing on PR {number}"),
                        observed.url.clone(),
                        session_id.clone(),
                    )
                    .with_session_id(session_id.clone()),
                );
            }
        }
    });
}

/// Stable store tokens for PR/CI states (schema strings, not display).
fn pr_status_token(status: PrStatus) -> &'static str {
    match status {
        PrStatus::Draft => "draft",
        PrStatus::Open => "open",
        PrStatus::Merged => "merged",
        PrStatus::Closed => "closed",
    }
}

fn ci_status_token(ci: CiStatus) -> &'static str {
    match ci {
        CiStatus::Pending => "pending",
        CiStatus::Running => "running",
        CiStatus::Passing => "passing",
        CiStatus::Failing => "failing",
    }
}

async fn gh_json(root: &std::path::Path, args: &[&str]) -> Option<Value> {
    let mut command = tokio::process::Command::new("gh");
    command.args(args).current_dir(root).kill_on_drop(true);
    scrub_gh_command(&mut command);
    let output = command.output().await.ok()?;
    serde_json::from_slice(&output.stdout).ok()
}

fn scrub_gh_command(command: &mut tokio::process::Command) {
    stella_tools::subprocess_env::scrub_sensitive_env_except(
        command,
        stella_tools::subprocess_env::GITHUB_CLI_AUTH_ENV_VARS,
    );
}

/// Reconcile the workspace's current-branch PR: `gh pr view` for identity
/// and state, `gh pr checks` for the aggregate CI verdict. `None` when no
/// PR exists for the branch (or `gh` is absent/unauthenticated).
async fn observe_pr(root: &std::path::Path) -> Option<PrObservation> {
    let view = gh_json(root, &["pr", "view", "--json", "url,number,state,isDraft"]).await?;
    let url = view.get("url")?.as_str()?.to_string();
    let number = view.get("number").and_then(Value::as_u64);
    let is_draft = view
        .get("isDraft")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status = match view.get("state").and_then(Value::as_str).unwrap_or("") {
        "MERGED" => PrStatus::Merged,
        "CLOSED" => PrStatus::Closed,
        _ if is_draft => PrStatus::Draft,
        _ => PrStatus::Open,
    };
    let ci = match gh_json(root, &["pr", "checks", "--json", "bucket"]).await {
        Some(Value::Array(rows)) => aggregate_ci(
            &rows
                .iter()
                .filter_map(|r| r.get("bucket").and_then(Value::as_str))
                .collect::<Vec<_>>(),
        ),
        _ => None,
    };
    Some(PrObservation {
        url,
        number,
        status,
        ci,
    })
}

/// Fold `gh pr checks` buckets into one verdict. Any failure wins; then
/// anything still moving; a fully-settled green set is passing. An empty
/// set is `None` — absence of checks must never render as passing.
fn aggregate_ci(buckets: &[&str]) -> Option<CiStatus> {
    if buckets.is_empty() {
        return None;
    }
    if buckets.iter().any(|b| matches!(*b, "fail" | "cancel")) {
        return Some(CiStatus::Failing);
    }
    if buckets.contains(&"pending") {
        return Some(CiStatus::Running);
    }
    Some(CiStatus::Passing)
}

/// Poll the machine-wide notification store and push a fresh snapshot when
/// it changes — other sessions produce into the same store, so the badge
/// must not wait for a local action. Exits with the deck (send fails once
/// the inbound channel closes).
fn spawn_notification_poller(in_tx: mpsc::UnboundedSender<Inbound>) {
    tokio::spawn(async move {
        let store = stella_store::NotificationStore::open_default();
        let mut fingerprint: Vec<(String, bool)> = Vec::new();
        let mut first = true;
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(NOTIFY_POLL_MS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if in_tx.is_closed() {
                break;
            }
            let list = store.list();
            let next: Vec<(String, bool)> = list.iter().map(|n| (n.id.clone(), n.read)).collect();
            // The first pass always pushes (the badge must show pre-existing
            // unread messages); afterwards only changes do.
            if first || next != fingerprint {
                first = false;
                fingerprint = next;
                if in_tx.send(notifications_inbound(&store)).is_err() {
                    break;
                }
            }
        }
    });
}

/// Run the browser OAuth login for an http MCP server in the background.
/// Progress streams to the MCP tab's status line; the authorize URL and the
/// final outcome also land in the persist-until-read inbox (the browser may
/// have opened on another screen, and the tab may not be visible).
fn spawn_mcp_oauth_login(
    server: String,
    workspace_root: std::path::PathBuf,
    in_tx: mpsc::UnboundedSender<Inbound>,
) {
    tokio::spawn(async move {
        let inbox = stella_store::NotificationStore::open_default();
        let progress_tx = in_tx.clone();
        let progress_server = server.clone();
        let progress_inbox = inbox.clone();
        let mut on_event = move |event: stella_mcp::LoginEvent| {
            let message = match event {
                stella_mcp::LoginEvent::Status(line) => line,
                stella_mcp::LoginEvent::AuthorizeUrl(url) => {
                    let _ = progress_inbox.push(&stella_store::Notification::new(
                        format!("MCP OAuth: approve `{progress_server}` in your browser"),
                        url.clone(),
                        progress_server.clone(),
                    ));
                    format!("approve in your browser: {url}")
                }
            };
            let _ = progress_tx.send(Inbound::McpOauthStatus {
                server: progress_server.clone(),
                message,
                outcome: None,
            });
        };
        let result = crate::mcp_cmd::oauth_login(&workspace_root, &server, &mut on_event).await;
        let (message, ok) = match result {
            Ok(()) => ("logged in — tokens auto-refresh".to_string(), true),
            Err(e) => (e, false),
        };
        let title = if ok {
            format!("MCP OAuth: `{server}` logged in")
        } else {
            format!("MCP OAuth: `{server}` login failed")
        };
        let _ = inbox.push(&stella_store::Notification::new(
            title,
            message.clone(),
            server.clone(),
        ));
        let _ = in_tx.send(Inbound::McpOauthStatus {
            server,
            message,
            outcome: Some(ok),
        });
    });
}

// ── ISSUES tab: tracker-backed operations ───────────────────────────────────

/// The lazily-detected issue-tracker backend shared by every ISSUES-tab
/// task. `None` inside the mutex means "not detected yet — or nothing was
/// connected the last time we looked": detection re-runs on the next
/// request, so a `stella connect …` performed mid-session is picked up
/// without a restart; once a backend IS found it is cached for the session.
type IssueBackendCache = Arc<tokio::sync::Mutex<Option<Arc<IssueBackend>>>>;

/// What every ISSUES-tab request answers with while no tracker is connected
/// — the tab renders it as its empty-state hint.
const NO_TRACKER_HINT: &str =
    "no tracker connected — run `stella connect github` or `stella connect linear`";

/// The cached backend, detecting on first use (Linear env/connection beats a
/// GitHub connection beats ambient `gh` auth — `detect_issue_backend_async`).
async fn issue_backend(cache: &IssueBackendCache) -> Result<Arc<IssueBackend>, String> {
    let mut guard = cache.lock().await;
    if let Some(backend) = guard.as_ref() {
        return Ok(backend.clone());
    }
    match stella_tools::issues::detect_issue_backend_async().await {
        Some(backend) => {
            let backend = Arc::new(backend);
            *guard = Some(backend.clone());
            Ok(backend)
        }
        None => Err(NO_TRACKER_HINT.to_string()),
    }
}

/// `IssueSummary` → the TUI's `IssueRow` (the deck never links the tools
/// crate; this driver maps one to the other, field for field).
fn issue_row(summary: IssueSummary) -> IssueRow {
    IssueRow {
        key: summary.key,
        title: summary.title,
        state: summary.state,
        labels: summary.labels,
        assignee: summary.assignee,
        url: summary.url,
        updated_at: summary.updated_at,
    }
}

/// List issues via the cached backend, mapped to deck rows.
async fn list_issue_rows(
    cache: &IssueBackendCache,
    root: &std::path::Path,
    query: Option<String>,
    state: Option<String>,
) -> Result<Vec<IssueRow>, String> {
    let backend = issue_backend(cache).await?;
    let filters = IssueFilters {
        query,
        state,
        ..IssueFilters::default()
    };
    stella_tools::issue_ops::list_issues(&backend, root, &filters)
        .await
        .map(|issues| issues.into_iter().map(issue_row).collect())
}

/// A tracker member as a type-ahead hit (kind "Person"): the label and the
/// inserted text are the handle (`@login` on GitHub, an email on Linear);
/// the description carries the human name/email where they add anything.
fn member_hit(member: MemberInfo) -> EntityHit {
    let description = match (&member.name, &member.email) {
        (Some(name), Some(email)) if *email != member.handle => format!("{name} · {email}"),
        (Some(name), _) => name.clone(),
        (None, Some(email)) if *email != member.handle => email.clone(),
        _ => String::new(),
    };
    EntityHit {
        kind: "Person".to_string(),
        label: member.handle.clone(),
        description,
        insert: member.handle,
    }
}

/// A tracker label as a type-ahead hit (kind "Label"); the description is
/// the label's description, falling back to its color swatch value.
fn label_hit(label: LabelInfo) -> EntityHit {
    EntityHit {
        kind: "Label".to_string(),
        label: label.name.clone(),
        description: label.description.or(label.color).unwrap_or_default(),
        insert: label.name,
    }
}

/// Installed agents whose name or description contains `query`
/// (case-insensitive; an empty query matches all) as "Agent" hits.
fn agent_entity_hits(entries: &[stella_tui::InstalledAgentEntry], query: &str) -> Vec<EntityHit> {
    let needle = query.trim().to_lowercase();
    entries
        .iter()
        .filter(|e| {
            needle.is_empty()
                || e.name.to_lowercase().contains(&needle)
                || e.description.to_lowercase().contains(&needle)
        })
        .map(|e| EntityHit {
            kind: "Agent".to_string(),
            label: e.name.clone(),
            description: e.description.clone(),
            insert: e.name.clone(),
        })
        .collect()
}

/// Cap on the content preview a memory hit carries.
const MEMORY_PREVIEW_CHARS: usize = 60;

/// One memory node as a type-ahead hit: a flattened content preview plus a
/// provenance suffix (`· observed … · valid from …` — valid-from falls back
/// to the observation time, the store's own convention) and, when the
/// memory has been cited, its citation stats.
fn memory_hit(
    display_name: &str,
    content: &str,
    recorded_at: &str,
    valid_from: Option<&str>,
    citations: Option<(i64, f64)>,
) -> EntityHit {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview: String = if flat.chars().count() > MEMORY_PREVIEW_CHARS {
        let head: String = flat.chars().take(MEMORY_PREVIEW_CHARS - 1).collect();
        format!("{head}…")
    } else {
        flat
    };
    let mut description = format!(
        "{preview} · observed {recorded_at} · valid from {}",
        valid_from.unwrap_or(recorded_at)
    );
    if let Some((count, avg)) = citations {
        description.push_str(&format!(" · cited {count}× avg {avg:.1}"));
    }
    EntityHit {
        kind: "Memory".to_string(),
        label: display_name.to_string(),
        description,
        insert: display_name.to_string(),
    }
}

/// One code-graph definition frame as a type-ahead hit: the kind is the
/// frame kind capitalized ("Symbol"), the label its human title (`fn foo`),
/// the description its file location (the citation's parenthetical, else
/// the frame uri), and the inserted text the bare symbol name — the title's
/// last token.
fn symbol_hit(frame: &contextgraph_types::ContextFrame) -> EntityHit {
    let label = frame.title.clone();
    let insert = label
        .split_whitespace()
        .last()
        .unwrap_or(label.as_str())
        .to_string();
    let description = frame
        .citation_label
        .as_deref()
        .and_then(|citation| {
            let start = citation.rfind('(')?;
            let end = citation.rfind(')')?;
            (start + 1 < end).then(|| citation[start + 1..end].to_string())
        })
        .or_else(|| frame.uri.clone())
        .unwrap_or_default();
    EntityHit {
        kind: format!("{:?}", frame.kind),
        label,
        description,
        insert,
    }
}

/// The local (non-tracker) assignee sources, read synchronously (call on
/// the blocking pool): memories from `.stella/private/context.db` — with citation
/// stats joined from `store.db` by `public_id` — and code-graph symbol
/// definitions when an index exists. Read-only politeness (the `stella
/// stats` discipline): a missing database reads as "no hits", never a
/// write. Failures of one source never kill another.
fn local_assignee_hits(root: &std::path::Path, query: &str) -> Vec<EntityHit> {
    let needle = query.trim().to_lowercase();
    let mut hits = Vec::new();

    // Memories: substring over display_name/content; empty query lists all.
    let context_db = stella_store::existing_workspace_private_sqlite_path(root, "context.db")
        .ok()
        .flatten();
    if let Some(context_db) = context_db
        && let Ok(context) = stella_context::ContextStore::open(&context_db)
        && let Ok(nodes) = context.memory_nodes()
    {
        let stats: std::collections::HashMap<String, (i64, f64)> = {
            if stella_store::existing_workspace_private_sqlite_path(root, "store.db")
                .ok()
                .flatten()
                .is_some()
            {
                stella_store::Store::open(root)
                    .and_then(|store| store.memory_citation_stats())
                    .map(|rows| {
                        rows.into_iter()
                            .map(|s| (s.memory_id, (s.citations, s.avg_score)))
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Default::default()
            }
        };
        hits.extend(
            nodes
                .iter()
                .filter(|n| {
                    needle.is_empty()
                        || n.display_name.to_lowercase().contains(&needle)
                        || n.content.to_lowercase().contains(&needle)
                })
                .take(20)
                .map(|n| {
                    memory_hit(
                        &n.display_name,
                        &n.content,
                        &n.recorded_at,
                        n.valid_from.as_deref(),
                        stats.get(&n.public_id).copied(),
                    )
                }),
        );
    }

    // Code-graph definitions of the queried name, when an index exists
    // (definitions are an exact-name lookup, so an empty query has nothing
    // to resolve).
    if !needle.is_empty() {
        let db = stella_tools::graph::graph_db_path(root);
        if db.exists()
            && let Ok(graph) = stella_graph::CodeGraph::open(root, &db)
            && let Ok(frames) = graph.definitions(query.trim())
        {
            hits.extend(frames.iter().map(symbol_hit));
        }
    }
    hits
}

/// Merge the assignee sources in priority order — tracker people first,
/// then installed agents, then local memories/symbols — capped at `cap`.
fn merge_assignee_hits(
    tracker: Vec<EntityHit>,
    agents: Vec<EntityHit>,
    local: Vec<EntityHit>,
    cap: usize,
) -> Vec<EntityHit> {
    let mut merged = tracker;
    merged.extend(agents);
    merged.extend(local);
    merged.truncate(cap);
    merged
}

/// Service one ISSUES-tab request. ALWAYS spawns the work and sends the
/// `Inbound` from the spawned task — the tab is serviced identically idle or
/// mid-turn, and a tracker round-trip must never stall the driver loop
/// (the `spawn_mcp_oauth_login` shape). Returns `true` when the input was
/// one of the tab's.
fn handle_issues_input(
    input: &WorkspaceInput,
    cfg: &Config,
    cache: &IssueBackendCache,
    in_tx: &UnboundedSender<Inbound>,
) -> bool {
    let root = cfg.workspace_root.clone();
    match input {
        WorkspaceInput::IssuesRefresh { query, state, seq } => {
            let (cache, in_tx, seq) = (cache.clone(), in_tx.clone(), *seq);
            let (query, state) = (query.clone(), state.clone());
            tokio::spawn(async move {
                let outcome = list_issue_rows(&cache, &root, query, state).await;
                let _ = in_tx.send(Inbound::IssuesList { seq, outcome });
            });
            true
        }
        WorkspaceInput::IssueCreate {
            title,
            body,
            labels,
            assignee,
            seq,
        } => {
            let (cache, in_tx, seq) = (cache.clone(), in_tx.clone(), *seq);
            let params = CreateParams {
                title: title.clone(),
                body: body.clone(),
                labels: labels.clone(),
                assignee: assignee.clone(),
                team: None,
            };
            tokio::spawn(async move {
                let created = match issue_backend(&cache).await {
                    Ok(backend) => {
                        stella_tools::issue_ops::create_issue(&backend, &root, &params).await
                    }
                    Err(e) => Err(e),
                };
                match created {
                    Ok(created) => {
                        let _ = in_tx.send(Inbound::IssueActDone {
                            seq,
                            key: created.key.clone(),
                            outcome: Ok(format!("created {} — {}", created.key, created.url)),
                        });
                        // The list changed — refresh it under the same seq
                        // (the panel armed its list lane on submit).
                        let outcome = list_issue_rows(&cache, &root, None, None).await;
                        let _ = in_tx.send(Inbound::IssuesList { seq, outcome });
                    }
                    Err(e) => {
                        let _ = in_tx.send(Inbound::IssueActDone {
                            seq,
                            key: String::new(),
                            outcome: Err(e),
                        });
                    }
                }
            });
            true
        }
        WorkspaceInput::IssueAct { key, action, seq } => {
            let (cache, in_tx, seq) = (cache.clone(), in_tx.clone(), *seq);
            let (key, action) = (key.clone(), action.clone());
            tokio::spawn(async move {
                let outcome = match issue_backend(&cache).await {
                    Ok(backend) => match &action {
                        IssueAction::Comment(text) => {
                            stella_tools::issue_ops::add_comment(&backend, &root, &key, text)
                                .await
                                .map(|()| format!("comment added to {key}"))
                        }
                        IssueAction::SetStatus(status) => {
                            stella_tools::issue_ops::set_status(&backend, &root, &key, status).await
                        }
                        // Start work = move the issue to in-progress. Branch
                        // creation/checkout stays the `start_work_on_issue`
                        // tool's job; on GitHub (whose issues know only
                        // open/closed) this reports the tracker's honest
                        // "no such state" message.
                        IssueAction::StartWork => {
                            stella_tools::issue_ops::set_status(
                                &backend,
                                &root,
                                &key,
                                "in progress",
                            )
                            .await
                        }
                    },
                    Err(e) => Err(e),
                };
                let _ = in_tx.send(Inbound::IssueActDone { seq, key, outcome });
            });
            true
        }
        WorkspaceInput::EntitySearch { field, query, seq } => {
            let (cache, in_tx, seq, field) = (cache.clone(), in_tx.clone(), *seq, *field);
            let query = query.clone();
            tokio::spawn(async move {
                let hits = match field {
                    EntityField::Label => match issue_backend(&cache).await {
                        Ok(backend) => {
                            stella_tools::issue_ops::search_labels(&backend, &root, &query, 20)
                                .await
                                .map(|labels| labels.into_iter().map(label_hit).collect())
                                .unwrap_or_default()
                        }
                        // No tracker: no label vocabulary. The popup shows
                        // "no matches"; the list-level requests carry the
                        // connect hint.
                        Err(_) => Vec::new(),
                    },
                    EntityField::Assignee => {
                        // Four independent sources — a failure of one must
                        // not kill the others; collect what succeeds.
                        let tracker = match issue_backend(&cache).await {
                            Ok(backend) => {
                                stella_tools::issue_ops::search_members(&backend, &root, &query, 15)
                                    .await
                                    .map(|members| members.into_iter().map(member_hit).collect())
                                    .unwrap_or_default()
                            }
                            Err(_) => Vec::new(),
                        };
                        let agents = {
                            let project = crate::agents_installed::project_agents_dir(&root);
                            let user = crate::agents_installed::user_agents_dir();
                            agent_entity_hits(
                                &crate::agents_installed::discover(user.as_deref(), &project),
                                &query,
                            )
                        };
                        let local = {
                            let root = root.clone();
                            let query = query.clone();
                            // SQLite opens + tree-sitter grammar loading are
                            // synchronous — keep them off the async workers.
                            tokio::task::spawn_blocking(move || local_assignee_hits(&root, &query))
                                .await
                                .unwrap_or_default()
                        };
                        merge_assignee_hits(tracker, agents, local, 20)
                    }
                };
                let _ = in_tx.send(Inbound::EntityHits {
                    field,
                    seq,
                    query,
                    hits,
                });
            });
            true
        }
        _ => false,
    }
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
    ("/models", "list providers & models (`refresh` re-syncs)"),
    ("/init", "index the workspace: domains + code graph"),
    (
        "/agents",
        "open the AGENTS tab: executions & installed agents",
    ),
    (
        "/pipeline",
        "toggle the staged pipeline (witness-verified turns)",
    ),
    (
        "/export",
        "export session telemetry to a ZIP + HTML dashboard",
    ),
    ("/files", "open the Files tab"),
    ("/diff", "open the diff viewer"),
    ("/graph", "open the code-graph tab"),
    ("/skills", "open the SKILLS tab: manage · search · create"),
    ("/mcp", "open the MCP servers tab"),
    (
        "/sessions",
        "every stella session on this machine, grouped by status (also: ← on an empty prompt)",
    ),
    (
        "/context",
        "this session's active skills + MCP servers (also: → on an empty prompt)",
    ),
    ("/inbox", "notifications — messages persist until read"),
    ("/mcp-search", "search the MCP registry & install servers"),
    // The engine-config editor (per-agent models included) lives on the
    // SETTINGS tab, full-width — THE way to configure models; there are no
    // per-agent slash commands.
    (
        "/settings",
        "open the SETTINGS tab — the home of all config (models included)",
    ),
    ("/donate", "support stella — become a GitHub Sponsor"),
];

/// The deck's reserved command names — see [`DECK_BUILTINS`].
fn deck_reserved() -> Vec<&'static str> {
    DECK_BUILTINS.iter().map(|(name, _)| *name).collect()
}

/// An argument-carrying form of `/models` — handled model-free: when the
/// configured model itself is broken, `/models refresh` is how the user
/// digs out, and routing it into a model turn fails on the very error
/// being fixed. Parsed conservatively — a single recognized token (plus
/// `refresh --force`); anything sentence-like stays a prompt, matching
/// the "`/init do the thing` is a model prompt" rule.
enum ModelsCommand {
    /// `/models refresh [--force]` — re-sync the catalog, no model call.
    Refresh { force: bool },
    /// `/models list` — the same listing the bare `/models` prints.
    List,
    /// `/models <typo>` — one unrecognized token: a mistyped subcommand,
    /// answered with usage instead of a wasted model call.
    Usage(String),
}

/// Parse `trimmed` as a [`ModelsCommand`]; `None` leaves it on the normal
/// path (custom expansion, then prompt).
fn parse_models_command(trimmed: &str) -> Option<ModelsCommand> {
    let (head, rest) = trimmed.split_once(char::is_whitespace)?;
    let rest = rest.trim();
    if head != "/models" || rest.is_empty() {
        return None;
    }
    let mut words = rest.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("refresh"), None, None) => Some(ModelsCommand::Refresh { force: false }),
        (Some("refresh"), Some("--force"), None) => Some(ModelsCommand::Refresh { force: true }),
        (Some("list"), None, None) => Some(ModelsCommand::List),
        (Some(word), None, None) => Some(ModelsCommand::Usage(word.to_string())),
        // A sentence after `/models` stays a prompt.
        _ => None,
    }
}

// ── Agent-engine config (the SETTINGS tab's config panel) ─────────────────────

/// Build an [`Inbound::EngineConfig`] snapshot: the freshly merged
/// `agent_engine_config` from the settings scope chain, plus the picker
/// vocabularies — every provider whose credential currently resolves, and
/// the catalog's `provider/slug` list as the model-picker fallback when
/// `allowed_models` is empty. The model list is scoped to those same
/// credentialed providers (plus the session's active one): a model you
/// have no key for is not an option, and offering it anyway was exactly
/// the "selectable but unusable" bug. Re-reading the chain (rather than
/// caching) keeps the overlay honest about hand edits and about what a
/// save at one scope means under the others.
fn engine_config_inbound(cfg: &Config, status: Option<String>) -> Inbound {
    let engine = crate::settings::Settings::load(&cfg.workspace_root)
        .ok()
        .and_then(|s| s.agent_engine_config)
        .unwrap_or_default();
    let providers: Vec<String> = crate::config::discover_configured_providers()
        .into_iter()
        .map(|p| p.config.id.to_string())
        .collect();
    // The session's provider is always usable — its credential resolved at
    // startup (possibly interactively, which discovery never does).
    let mut usable: std::collections::HashSet<&str> =
        providers.iter().map(String::as_str).collect();
    usable.insert(cfg.provider.id);
    let catalog = stella_model::catalog::Catalog::current();
    let mut catalog_models: Vec<String> = Vec::new();
    let mut model_efforts: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for entry in catalog
        .entries()
        .iter()
        .filter(|entry| usable.contains(entry.provider.as_str()))
    {
        let spec = format!("{}/{}", entry.provider, entry.id);
        let levels = crate::engine_config::effort_levels(
            &entry.provider,
            crate::config::PROVIDERS
                .iter()
                .find(|p| p.id == entry.provider)
                .map(|p| p.dialect)
                .unwrap_or(crate::config::Dialect::OpenaiCompatible),
            entry.supports_reasoning,
        );
        model_efforts.insert(spec.clone(), levels.iter().map(|s| s.to_string()).collect());
        catalog_models.push(spec);
    }
    // `allowed_models` specs are picker entries too — give each its effort
    // vocabulary so the effort row is model-aware under a restriction.
    for raw in engine.allowed_models() {
        if model_efforts.contains_key(raw) {
            continue;
        }
        if let Some(spec) = crate::engine_config::parse_model_spec(raw, &|id| usable.contains(id)) {
            let levels = crate::engine_config::effort_levels_for_spec(&spec.provider, &spec.model);
            model_efforts.insert(raw.clone(), levels.iter().map(|s| s.to_string()).collect());
        }
    }
    Inbound::EngineConfig {
        state: crate::engine_config::state_from_settings(
            &engine,
            providers,
            catalog_models,
            model_efforts,
        ),
        status,
    }
}

/// Handle one ENGINE-overlay op (refresh / save) — cheap local settings
/// I/O, answered with a fresh [`Inbound::EngineConfig`]. Called from BOTH
/// recv sites so the overlay works mid-turn too. Returns `true` when the
/// input was one of the overlay's.
fn handle_engine_config_input(
    input: &WorkspaceInput,
    cfg: &Config,
    in_tx: &UnboundedSender<Inbound>,
) -> bool {
    match input {
        WorkspaceInput::EngineConfigRefresh => {
            let _ = in_tx.send(engine_config_inbound(cfg, None));
            true
        }
        WorkspaceInput::EngineConfigSave { state, scope } => {
            let engine = crate::engine_config::settings_from_state(state);
            let path = match scope {
                AgentScope::User => crate::settings::user_settings_path(),
                AgentScope::Project => {
                    Some(crate::settings::project_settings_path(&cfg.workspace_root))
                }
            };
            let status = match path {
                None => "save failed: cannot determine $HOME for user settings".to_string(),
                Some(path) => match engine.save_to(&path) {
                    Ok(()) => format!(
                        "saved to {} — applies to runs started from now on",
                        path.display()
                    ),
                    Err(e) => format!("save failed: {e}"),
                },
            };
            // The snapshot sent back is the MERGED view — if a project
            // scope overrides what was just saved at the user scope, the
            // overlay shows the effective value, not the wish.
            let _ = in_tx.send(engine_config_inbound(cfg, Some(status)));
            true
        }
        _ => false,
    }
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
    budget_limit: Option<f64>,
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
            // Open the same rich, scrollable overlay the `?` key opens —
            // every key, every tab, every slash command in one place. Far
            // more useful (and readable) than a cramped one-line summary.
            let _ = in_tx.send(Inbound::ShowHelp);
        }
        "/clear" => {
            // Reset the driver's own LLM history…
            messages.clear();
            messages.push(CompletionMessage::system(system_prompt.to_string()));
            // …and the deck's session view: blank the transcript (including the
            // `/clear` echo the paired PromptStarted just pushed), zero the cost
            // stat, and return the progress bar to idle. No `say()` — that would
            // re-populate the transcript we are clearing.
            let _ = in_tx.send(Inbound::SessionReset {
                agent: LEAD.to_string(),
            });
        }
        "/models" => {
            say(Config::available_models_plain(None));
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
                 large plans stop with a named scope-review error until a deck-native host \
                 approval gate is available. \
                 `/pipeline` again to return to the raw engine loop."
                    .to_string()
            } else {
                "staged pipeline OFF — turns run the raw engine loop.".to_string()
            });
        }
        "/init" => {
            // Replay the launch cinematic over the reindex: the battle loops
            // for as long as init runs, then the wordmark reveal hands the
            // frame back to the deck. The progress lines still land in the
            // transcript behind the splash (and any key skips straight to
            // them). Released on BOTH outcomes — a failed init must never
            // strand a held splash.
            let _ = in_tx.send(Inbound::Splash(SplashCue::Replay));
            let mut emit = |line: String| say(line);
            let outcome = agent::init_workspace(
                Some(provider),
                &cfg.workspace_root,
                Some(&cfg.model_id),
                budget_limit,
                &mut emit,
            )
            .await;
            let _ = in_tx.send(Inbound::Splash(SplashCue::Release));
            match outcome {
                Ok((_domains, _cost_usd)) => {
                    // A fresh index may name tables/types the schema gate
                    // should know about this session, not just the next one.
                    if let Err(error) = agent::populate_schema_index(registry, &cfg.workspace_root)
                    {
                        say(format!("schema governance unavailable: {error}"));
                        return DeckCommand::Handled;
                    }
                    // Expose the `graph_query` tool for the rest of the session
                    // now that the index exists (it is registered only when an
                    // index is present at construction).
                    if let Err(error) = registry.enable_code_graph_if_available(&cfg.workspace_root)
                    {
                        say(format!("graph tool unavailable: {error}"));
                    }
                    return DeckCommand::InitCompleted;
                }
                Err(e) => say(format!("init failed: {e}")),
            }
        }
        "/export" => {
            // Export all session telemetry to a timestamped ZIP archive
            // containing raw JSON dumps + a self-contained HTML dashboard.
            match crate::export::export_session(&cfg.workspace_root) {
                Ok(path) => {
                    let display = path.display();
                    say(format!(
                        "Export Session Telemetry — archive written to {display}\n\
                         The ZIP contains a `dashboard.html` (open in any browser) and raw \
                         JSON dumps of every telemetry table. The timestamped folder name \
                         matches the last log entry's timestamp."
                    ));
                }
                Err(e) => say(format!("export failed: {e}")),
            }
        }
        "/donate" => {
            say("❤️  Support Stella\n\
                 \n\
                 Stella is free, open-source, and local-first — no server, no \
                 account, no telemetry sent home. If it's saving you time or \
                 money, consider becoming a GitHub Sponsor:\n\
                 \n\
                   → https://github.com/sponsors/macanderson\n\
                 \n\
                 Recurring sponsorships keep development sustainable. You'll \
                 see the available tiers and perks (one-time and monthly) on \
                 that page. Every pledge helps fund the next feature, the next \
                 provider, and the next release.\n\
                 \n\
                 Thank you! 🙏"
                .to_string());
        }
        // Deck-local commands (tab switches, `/agents` opening the Agents
        // tab, the transcript-page overlays) are normally consumed TUI-side,
        // but a queued one reaches here — accept it as handled (a no-op)
        // rather than calling it "unknown".
        "/files" | "/diff" | "/graph" | "/agents" | "/skills" | "/mcp" | "/mcp-search"
        | "/settings" | "/sessions" | "/context" | "/inbox" => {}
        _ => {
            // The `/models` argument forms first (see [`ModelsCommand`]):
            // handled model-free — a catalog refresh is part of digging out
            // of a broken model setting, so it can never be allowed to
            // depend on a working model.
            if let Some(command) = parse_models_command(trimmed) {
                match command {
                    ModelsCommand::Refresh { force } => {
                        say("Model catalog refresh…".to_string());
                        let mut emit = |line: String| say(line);
                        if let Err(e) =
                            crate::model_catalog::run_refresh_emit(force, &mut emit).await
                        {
                            say(format!("refresh failed: {e}"));
                        }
                    }
                    ModelsCommand::List => say(Config::available_models_plain(None)),
                    ModelsCommand::Usage(word) => say(format!(
                        "`/models {word}` — unknown subcommand; try `/models` or `/models list` \
                         (the listing) or `/models refresh [--force]` (re-sync the catalog)"
                    )),
                }
                return DeckCommand::Handled;
            }
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
                "unknown command `{trimmed}` — try /help, /clear, /models, /init, /agents, /pipeline, /export, /donate, /files, /diff, /graph"
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
    sup_tx: &UnboundedSender<SupervisorMsg>,
    claim_holder: &str,
    activated: &crate::discovery::ActivatedTools,
    steering: &subsession::SteeringTap,
) -> Result<(), String> {
    budget.begin_turn();

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let forwarder = spawn_forwarder(
        rx,
        execution.clone(),
        cfg.provider.id.to_string(),
        in_tx.clone(),
        LEAD.to_string(),
    );

    // Claim-on-first-write over the shared tree (crate::claims): wraps the
    // base executor, so a refused write surfaces as the tool's own error —
    // FileChangeTap's is_error early-return keeps phantom events out.
    // Released after the turn settles, cancel included.
    let claims = ClaimTap::new(
        base_tools,
        execution.as_ref().map(|(store, _)| store.clone()),
        claim_holder,
    );

    // Same structural drop-order rule as `agent::run_turn`: every tx clone
    // lives in this scope so dropping `tx` after it closes the channel.
    let outcome = {
        let customs =
            CustomToolSet::new(&claims, custom_tools.to_vec(), cfg.workspace_root.clone());
        // The AskUser event channel is a stub: the deck io presents its own
        // card (it must — `install_skill` confirms through the io without any
        // event), so the tool set's own emission would double the card.
        let (stub_tx, _) = mpsc::unbounded_channel();
        let interactive = InteractiveToolSet::new(&customs, stub_tx, Box::new(ask_io.clone()))
            .with_skill_registry(SkillRegistry::from_env(cfg.workspace_root.clone()));
        // Discovery layer above the interactive set (it must see the full
        // catalog), below the taps (searches are read-only; taps watch writes).
        let tools =
            crate::discovery::DiscoveryToolSet::new(&interactive, cfg.workspace_root.clone())
                .with_project_prompts_allowed(cfg.authority.project_prompts_allowed)
                .with_activation(activated.clone());
        let tapped = FileChangeTap {
            inner: &tools,
            events: tx.clone(),
            root: cfg.workspace_root.clone(),
        };
        let tapped = TaskTap {
            inner: &tapped,
            events: tx.clone(),
            registry,
            supervisor: Some(sup_tx.clone()),
        };
        let hook_runner = ShellHookRunner;
        let mut engine = Engine::with_sleeper(
            provider,
            &tapped,
            agent::engine_config_for(cfg),
            &TokioSleeper,
        )
        .with_calibration(calibration)
        .with_steering(steering);
        if let Some(hooks) = &cfg.hooks {
            engine = engine.with_hooks(hooks, &hook_runner);
        }
        engine.run_turn(messages, budget, &tx).await
    };
    drop(tx);
    let persistence_complete = forwarder.await.unwrap_or(false);
    claims.release_all();

    if let Some((store, id)) = &execution {
        let (outcome_label, cost) = match &outcome {
            TurnOutcome::Completed { cost_usd, .. } => ("completed", *cost_usd),
            TurnOutcome::Aborted { cost_usd, .. } => ("aborted", *cost_usd),
        };
        if !agent::record_execution_end(
            store,
            *id,
            registry,
            outcome_label,
            cost,
            persistence_complete,
        ) {
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
        TurnOutcome::Aborted { reason, .. } => Err(reason),
    }
}

/// One staged-pipeline turn for the lead agent (`/pipeline` ON): the deck
/// analogue of the `stella run` pipeline path — same tool stack, persistence,
/// and event forwarding as [`run_lead_turn`], with `Pipeline::run` (triage →
/// recall → plan → scope → witness → execute → verify → judge → revise) in
/// place of the raw `Engine::run_turn`.
///
/// Deck-mode seams, all named:
/// - **Scope review fails closed.** The deck cannot block a turn on a stdio
///   gate (the alternate screen owns the terminal), so a large plan returns
///   `ScopeReviewRequiredHeadless`. Deck-native host approval remains the
///   follow-up; output/event rendering is never treated as authority.
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
    active_rules: &crate::rules::ResolvedRules,
    registry_options: &stella_tools::RegistryOptions,
    execution: Option<(Arc<Store>, i64)>,
    in_tx: &UnboundedSender<Inbound>,
    ask_io: &DeckAskUserIo,
    sup_tx: &UnboundedSender<SupervisorMsg>,
    claim_holder: &str,
    activated: &crate::discovery::ActivatedTools,
    steering: &subsession::SteeringTap,
    mcp: Option<Arc<stella_mcp::McpToolSet>>,
) -> Result<(), String> {
    budget.begin_turn();

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let forwarder = spawn_forwarder(
        rx,
        execution.clone(),
        cfg.provider.id.to_string(),
        in_tx.clone(),
        LEAD.to_string(),
    );

    // Claim-on-first-write over the shared tree — same wiring as
    // `run_lead_turn` (see the comment there).
    let claims = ClaimTap::new(
        base_tools,
        execution.as_ref().map(|(store, _)| store.clone()),
        claim_holder,
    );

    let result = {
        let customs =
            CustomToolSet::new(&claims, custom_tools.to_vec(), cfg.workspace_root.clone());
        let (stub_tx, _) = mpsc::unbounded_channel();
        let interactive = InteractiveToolSet::new(&customs, stub_tx, Box::new(ask_io.clone()))
            .with_skill_registry(SkillRegistry::from_env(cfg.workspace_root.clone()));
        let tools =
            crate::discovery::DiscoveryToolSet::new(&interactive, cfg.workspace_root.clone())
                .with_project_prompts_allowed(cfg.authority.project_prompts_allowed)
                .with_activation(activated.clone());
        let tapped = FileChangeTap {
            inner: &tools,
            events: tx.clone(),
            root: cfg.workspace_root.clone(),
        };
        let tapped = TaskTap {
            inner: &tapped,
            events: tx.clone(),
            registry,
            supervisor: Some(sup_tx.clone()),
        };

        let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());
        // Role wiring from `agent_engine_config`: worker/triage/judge pins +
        // their adapters + per-role request overrides. Notices land in the
        // transcript — stderr is invisible under the alternate screen.
        let configured = crate::config::discover_configured_providers();
        let wiring = agent::resolve_engine_wiring(cfg, &model_ref, &configured);
        for notice in &wiring.notices {
            let _ = tx.send(AgentEvent::Text {
                delta: format!("! {notice}\n"),
            });
        }
        let resolver =
            agent::RoleProviderResolver::new(provider, model_ref.clone(), &wiring.extra_providers);
        let breaker = CircuitBreaker::new(Box::new(SystemClock::new()));
        let router = Router::new(wiring.pins.clone(), wiring.profiles.clone(), breaker);

        let ws_ports = agent::workspace_ports(
            cfg.workspace_root.clone(),
            cfg,
            registry_options.clone(),
            active_rules.clone(),
            mcp,
        )?;
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
            repo: &ws_ports.repo_structure,
            repo_status: &ws_ports.repo_status,
            diagnostics: &ws_ports.diagnostic_runner,
            tests: &ws_ports.test_runner,
            approvals: &agent::HEADLESS_APPROVAL_GATE,
            sleeper: &TokioSleeper,
            hooks: cfg
                .hooks
                .as_ref()
                .map(|h| (h, &hook_runner as &dyn stella_core::hooks::HookRunner)),
            candidate_workspaces: Some(&ws_ports.candidate_workspaces),
            mcp_prefetch: ws_ports
                .mcp_prefetch
                .as_ref()
                .map(|p| p as &dyn McpPrefetchPort),
            // The deck's per-turn tap: `>` steers the execute engine mid-turn
            // (the same tap the step-loop lead turn uses).
            steering: Some(steering),
        };
        let config = PipelineConfig {
            engine: agent::pipeline_engine_config_for(cfg, &wiring.worker_model),
            role_overrides: wiring.role_overrides.clone(),
            headless: true,
            headless_bypass_scope_review: agent::HEADLESS_SCOPE_REVIEW_BYPASS,
            ..PipelineConfig::default()
        };
        let pipeline = Pipeline::new(ports, tx.clone(), config);
        pipeline.run(prompt, messages, budget).await
    };
    drop(tx);
    let persistence_complete = forwarder.await.unwrap_or(false);
    claims.release_all();

    if let Some((store, id)) = &execution {
        let (outcome_label, cost) = agent::pipeline_execution_closeout(&result);
        if !agent::record_execution_end(
            store,
            *id,
            registry,
            outcome_label,
            cost,
            persistence_complete,
        ) {
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
            PipelineStatus::VerificationFailed { verdict } => {
                Err(format!("verification failed: {}", verdict.summary))
            }
            PipelineStatus::Aborted { reason } => Err(reason),
        },
        Err(e) => Err(e.to_string()),
    }
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
                speculated: false,
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
/// file-touching built-in succeeds, so the deck's Files tab / diff panel and
/// ledger are live during the turn. The diff is synthesized from the tool's
/// own input (`edit_file` carries old/new verbatim; `write_file` the full
/// content; `delete_file` reads the file before executing) — an honest
/// approximation until the tool layer emits real diffs on the event path.
/// Successful `read_file` calls emit too (kind `Read`, no diff) — the Files
/// tab counts reads per file, matching the registry ledger's `R` events.
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

/// Mirrors the task board into the event stream: after any `task_*` tool
/// call the FULL board snapshot rides the turn's channel as
/// `AgentEvent::TaskUpdate` — persisted by the forwarder, so replay shows
/// the checklist exactly as it moved — and `task_assign`'s spawn requests
/// are handed to the driver's supervisor channel. `supervisor: None` is the
/// worker configuration (v1 delegation runs from the lead only; a worker's
/// stranded requests are reported on its lane by `crate::subsession`).
pub(crate) struct TaskTap<'a> {
    pub(crate) inner: &'a dyn ToolExecutor,
    pub(crate) events: UnboundedSender<AgentEvent>,
    pub(crate) registry: &'a ToolRegistry,
    pub(crate) supervisor: Option<UnboundedSender<SupervisorMsg>>,
}

#[async_trait]
impl ToolExecutor for TaskTap<'_> {
    fn schemas(&self) -> Vec<ToolSchema> {
        self.inner.schemas()
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        let output = self.inner.execute(name, input).await;
        if name.starts_with("task_") {
            let tasks: Vec<TaskItem> = {
                let board = self.registry.task_board();
                let guard = board.lock().unwrap_or_else(|p| p.into_inner());
                guard.items().to_vec()
            };
            let _ = self.events.send(AgentEvent::TaskUpdate { tasks });
            if let Some(sup) = &self.supervisor {
                for request in self.registry.take_spawn_requests() {
                    let _ = sup.send(SupervisorMsg::SpawnTask(request));
                }
            }
        }
        output
    }
}

/// The `(kind, pseudo-diff)` for one successful file-touching tool call, or
/// `None` for tools that don't touch files. `pre` is
/// `(existed_before, old_content)` as captured by the tap.
fn file_change_of(
    name: &str,
    input: &Value,
    pre: Option<(bool, Option<String>)>,
) -> Option<(FileChangeKind, Option<String>)> {
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    match name {
        "read_file" => Some((FileChangeKind::Read, None)),
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
mod tests;
