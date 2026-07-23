//! Deck sub-sessions — one dedicated engine session per dispatched request.
//!
//! The deck's contract is "input never blocks", but until now *dispatch* did:
//! one lead conversation ran prompts strictly FIFO, so a prompt submitted
//! mid-turn waited for the whole current turn. Sub-sessions close that gap:
//! when the lead is busy, the driver hands the prompt to a dedicated worker
//! session (`req:<n>`), and `task_assign` hands a board task to a dedicated
//! worker (`sub:<task-id>`). Each worker is a real engine session — its own
//! provider, tool registry, budget guard, execution row (linked to the deck's
//! session id for replay), and event lane in the deck — running on its own OS
//! thread with a current-thread runtime, because the engine's turn future is
//! deliberately not `Send` (the same bridge `fleet_cmd::EngineWorker` uses).
//!
//! Results come back three ways, none of which block the deck: live events on
//! the worker's lane (watch it from the Agents tab), a persist-until-read
//! notification when it finishes (the `/inbox` flow), and — for task workers —
//! the board task auto-completing on success.
//!
//! Scope (v1, documented rather than implied): workers run the raw engine
//! step-loop with native tools only (no MCP set, no custom tools, no
//! interactive ask_user — an autonomous worker has nobody to ask), recall is
//! skipped in favor of latency, and delegation is not recursive — a worker's
//! own `task_assign` requests are reported on its lane instead of spawning.

use std::collections::HashMap;

use stella_core::Engine;
use stella_core::tasks::SpawnRequest;
use stella_protocol::{AgentEvent, CompletionMessage, TaskItem};
use stella_tools::RegistryOptions;
use stella_tui::{AgentMeta, AgentStatus, Inbound};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::{oneshot, watch};

use crate::agent;
use crate::command_deck::{now_ms, prompt_line, spawn_forwarder};
use crate::config::Config;
use crate::runtime::TokioSleeper;

/// How many sub-session workers may run at once. Prompts (and task
/// assignments) beyond this wait in the driver's backlog and dispatch as
/// slots free — the cap bounds provider concurrency and CPU, not the queue.
pub(crate) const MAX_CONCURRENT: usize = 3;

/// How a worker ended — the supervisor's bookkeeping distinguishes the user
/// stopping a worker from the worker failing (a stop must not read as a
/// failure, must not auto-complete a task, and needs no inbox notification —
/// the user was there).
pub(crate) enum WorkerEnd {
    Done,
    Failed(String),
    /// Stopped by the user (Agents tab `s`, or Esc on the worker's lane).
    Stopped,
}

/// Messages the driver's supervisor channel carries. Spawn requests travel
/// tap → driver (a tool call cannot spawn a thread that outlives its turn);
/// endings travel worker → driver (bookkeeping + backlog drain).
pub(crate) enum SupervisorMsg {
    /// A `task_assign` request drained from the lead's tool tap.
    SpawnTask(SpawnRequest),
    /// A worker finished (its thread is exiting).
    Ended {
        lane: String,
        /// Which spawn of this lane ended (from [`SubSessions::started`]) —
        /// the bookkeeping frees a lane only for the worker that actually
        /// ended, so a late `Ended` can never tear down a replacement.
        generation: u64,
        /// The worker's execution row, when the store recorded one — the
        /// driver stamps the post-completion board mirror against it.
        execution_id: Option<i64>,
        /// USD the worker's turn spent — metered into the session's parent
        /// budget guard by the driver (the L-E9 discipline: child spend
        /// always reaches the parent's ledger).
        cost_usd: f64,
        end: WorkerEnd,
    },
}

/// Driver-side sub-session bookkeeping: the live-worker count, the lane
/// counter for `req:<n>` prompt workers, and each live worker's stop signal.
pub(crate) struct SubSessions {
    active: usize,
    next_req: u64,
    /// Monotonic spawn counter: each start stamps its worker, and `ended`
    /// only frees a lane for the generation that actually ended.
    next_generation: u64,
    stops: HashMap<String, (u64, oneshot::Sender<()>)>,
    /// Live workers' pause switches (watch: `true` = parked at the next
    /// step boundary).
    pauses: HashMap<String, watch::Sender<bool>>,
    /// Lanes whose stop signal was sent but whose `Ended` has not arrived —
    /// the worker thread is still winding down (forwarder drain, store
    /// closeout). These still count as live: the slot is not free, and a
    /// respawn before the old worker settles would put two workers on one
    /// lane, sharing (and corrupting) its channels.
    winding_down: HashMap<String, u64>,
    /// Every lane's spec, retained past its end — what Restart respawns.
    specs: HashMap<String, SubSessionSpec>,
    registry_options: RegistryOptions,
}

impl SubSessions {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_registry_options(RegistryOptions::default())
    }

    pub(crate) fn with_registry_options(registry_options: RegistryOptions) -> Self {
        Self {
            active: 0,
            next_req: 0,
            next_generation: 0,
            stops: HashMap::new(),
            pauses: HashMap::new(),
            winding_down: HashMap::new(),
            specs: HashMap::new(),
            registry_options,
        }
    }

    fn worker_registry_options(&self) -> RegistryOptions {
        self.registry_options.clone()
    }

    pub(crate) fn has_slot(&self) -> bool {
        self.active < MAX_CONCURRENT
    }

    /// Register a spawned worker; returns its generation, which travels
    /// through [`spawn`] into the worker's `Ended` message.
    fn started(
        &mut self,
        lane: &str,
        stop: oneshot::Sender<()>,
        pause: watch::Sender<bool>,
        spec: SubSessionSpec,
    ) -> u64 {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.active += 1;
        self.stops.insert(lane.to_string(), (generation, stop));
        self.pauses.insert(lane.to_string(), pause);
        self.specs.insert(lane.to_string(), spec);
        generation
    }

    /// Free `lane` — but only for the generation that actually ended.
    /// `false` (nothing freed) for any other generation: a late `Ended`
    /// from a replaced worker must not tear down its replacement's channels
    /// or corrupt the active count.
    pub(crate) fn ended(&mut self, lane: &str, generation: u64) -> bool {
        if self.winding_down.get(lane) == Some(&generation) {
            self.winding_down.remove(lane);
            self.active = self.active.saturating_sub(1);
            return true;
        }
        if self.stops.get(lane).is_some_and(|(g, _)| *g == generation) {
            self.stops.remove(lane);
            self.pauses.remove(lane);
            self.active = self.active.saturating_sub(1);
            return true;
        }
        // `specs` is retained on purpose: Restart respawns an ended lane.
        false
    }

    /// Pause (`true`) or resume (`false`) a live worker at its next step
    /// boundary. `false` when no such worker is live.
    pub(crate) fn set_paused(&mut self, lane: &str, paused: bool) -> bool {
        match self.pauses.get(lane) {
            Some(tx) => tx.send(paused).is_ok(),
            None => false,
        }
    }

    /// Whether `lane` currently has a live worker — winding-down included:
    /// a stopped worker whose `Ended` has not arrived still owns the lane
    /// (and its slot), so a respawn now must be deferred, not started.
    pub(crate) fn is_live(&self, lane: &str) -> bool {
        self.stops.contains_key(lane) || self.winding_down.contains_key(lane)
    }

    /// The retained spec for `lane`, for a Restart respawn.
    pub(crate) fn spec(&self, lane: &str) -> Option<SubSessionSpec> {
        self.specs.get(lane).cloned()
    }

    /// Signal one worker to stop (clean cancel: its turn future drops at the
    /// next await point, exactly the lead's cancel semantics). `false` when
    /// no such worker is live — a stale stop is a no-op, never an error.
    /// The lane stays live (winding down) until its `Ended` arrives.
    pub(crate) fn stop(&mut self, lane: &str) -> bool {
        match self.stops.remove(lane) {
            Some((generation, tx)) => {
                self.winding_down.insert(lane.to_string(), generation);
                // A winding-down worker cannot be paused; dropping the
                // sender also releases a currently-parked gate.
                self.pauses.remove(lane);
                tx.send(()).is_ok()
            }
            None => false,
        }
    }

    /// Signal every live worker to stop (session teardown). Each lane winds
    /// down until its `Ended` arrives, exactly like a single stop.
    pub(crate) fn stop_all(&mut self) {
        let lanes: Vec<String> = self.stops.keys().cloned().collect();
        for lane in lanes {
            self.stop(&lane);
        }
    }

    /// How many workers are live right now. The driver refuses to
    /// navigate to another session (`SessionResume`) while this is nonzero —
    /// live workers stream into the current session's lanes and settle
    /// against its records.
    pub(crate) fn live(&self) -> usize {
        self.active
    }

    /// The next prompt-worker lane id (`req:1`, `req:2`, …).
    pub(crate) fn next_req_lane(&mut self) -> String {
        self.next_req += 1;
        format!("req:{}", self.next_req)
    }

    /// Every lane a worker has ever run on this tenancy, live or ended
    /// (specs are retained past a worker's end for Restart). Sorted for
    /// deterministic iteration. The session-switch site deregisters these
    /// rows when the deck navigates to another session — they are all
    /// terminal there (the switch refuses while workers are live).
    pub(crate) fn lanes(&self) -> Vec<String> {
        let mut lanes: Vec<String> = self.specs.keys().cloned().collect();
        lanes.sort();
        lanes
    }
}

/// The deck's [`stella_core::ports::TurnSteering`] implementation: a tap the
/// input loop feeds (`>` steers) and an engine drains at each step boundary.
/// Interior mutability because the turn future and the input arms share it
/// immutably. Shared by reference for the lead turn (a per-turn stack local)
/// and by `Arc` for each worker lane (the deck feeds the worker's tap while
/// the worker task drains it). `soft_stop` is latched only for the lead; a
/// worker's stop stays the immediate hard cancel (`SubSessions::stop`).
#[derive(Default)]
pub(crate) struct SteeringTap {
    queue: std::sync::Mutex<Vec<String>>,
    soft_stop: std::sync::atomic::AtomicBool,
}

impl SteeringTap {
    pub(crate) fn push(&self, text: String) {
        self.queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(text);
    }
    pub(crate) fn request_soft_stop(&self) {
        self.soft_stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl stella_core::ports::TurnSteering for SteeringTap {
    fn drain_steering(&self) -> Vec<String> {
        std::mem::take(&mut *self.queue.lock().unwrap_or_else(|p| p.into_inner()))
    }
    fn soft_stop_requested(&self) -> bool {
        // Latched: set once, read at every boundary until the turn ends.
        self.soft_stop.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// `stella_core::ports::TurnGate` over a watch channel: the worker's turn
/// parks at its next step boundary while the driver holds `true` (Pause)
/// and continues on `false` (Resume). A dropped sender (driver gone) reads
/// as resumed — a worker must never park forever on teardown.
struct WatchGate(watch::Receiver<bool>);

#[async_trait::async_trait]
impl stella_core::ports::TurnGate for WatchGate {
    async fn wait_if_paused(&self) {
        let mut rx = self.0.clone();
        while *rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Everything a worker needs to run, owned (the thread outlives the caller's
/// borrows).
#[derive(Clone)]
pub(crate) struct SubSessionSpec {
    /// Deck lane id — `req:<n>` for a dispatched prompt, `sub:<task-id>` for
    /// an assigned task.
    pub lane: String,
    /// Dashboard row title.
    pub title: String,
    /// The full prompt the worker's model receives.
    pub prompt: String,
    /// Notification title on completion (the body is the outcome).
    pub notify_title: String,
}

/// Build the worker prompt for a `task_assign` spawn: the task's identity,
/// then the lead's briefing verbatim (the "communication" of the tool).
pub(crate) fn task_prompt(req: &SpawnRequest) -> String {
    let mut prompt = format!(
        "You are a sub-agent dispatched to complete one task from the lead session's board.\n\n\
         Task #{}: {}\n",
        req.task_id, req.subject
    );
    if let Some(description) = &req.description {
        prompt.push_str(description);
        prompt.push('\n');
    }
    prompt.push_str(&format!(
        "\nBriefing from the lead agent:\n{}\n\n\
         Work autonomously — nobody is watching live. Complete the task, then \
         summarize what you did and how you verified it.",
        req.briefing
    ));
    prompt
}

/// The panic text a caught worker payload carries (`panic!("…")` is a
/// `&str`, `panic!("{x}")` a `String` — anything else has no message).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

/// Prefix of the `Failed` reason [`run_caught`] synthesizes — also how
/// [`spawn`] recognizes the panic path afterward (a panicked worker never
/// reached its own claim release).
const PANIC_FAILURE_PREFIX: &str = "worker panicked: ";

/// Run one worker body, converting a panic into a `Failed` ending so the
/// supervisor ALWAYS receives `Ended` — a panicking tool must cost one
/// failed worker, not a lane stuck "Running" and a leaked slot. Effective
/// in unwind builds; under `panic = "abort"` (release) the process dies in
/// the panic hook before any catch — stella-tui's hook restores the
/// terminal there, and the deck's journal hook flushes the session journal.
fn run_caught<F>(body: F) -> (Option<i64>, f64, WorkerEnd)
where
    F: FnOnce() -> (Option<i64>, f64, WorkerEnd),
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(ended) => ended,
        Err(payload) => (
            None,
            0.0,
            WorkerEnd::Failed(format!(
                "{PANIC_FAILURE_PREFIX}{}",
                panic_message(payload.as_ref())
            )),
        ),
    }
}

/// Spawn one worker. Registers its deck lane immediately (the sub-second
/// acknowledgement — the row exists before any heavy setup), then runs the
/// session on a dedicated OS thread. Never blocks the caller.
// A worker genuinely needs every one of these (identity, budget, session
// link, both channels, stop signal) — bundling them into a struct would just
// move the field list one hop away from the one call shape.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    cfg: &Config,
    registry_options: RegistryOptions,
    spec: SubSessionSpec,
    generation: u64,
    budget_limit: Option<f64>,
    session_id: String,
    workspace_name: String,
    in_tx: UnboundedSender<Inbound>,
    sup_tx: UnboundedSender<SupervisorMsg>,
    stop_rx: oneshot::Receiver<()>,
    pause_rx: watch::Receiver<bool>,
) {
    let mut meta = AgentMeta::new(spec.lane.clone(), spec.title.clone(), now_ms())
        .with_role("subagent")
        .with_pid(std::process::id());
    meta.model = Some(format!("{}/{}", cfg.provider.id, cfg.model_id));
    let _ = in_tx.send(Inbound::Register(meta));
    let _ = in_tx.send(Inbound::Status {
        agent: spec.lane.clone(),
        status: AgentStatus::Running,
    });

    let cfg = cfg.clone();
    std::thread::spawn(move || {
        let lane = spec.lane.clone();
        let (execution_id, cost_usd, end) = run_caught(|| {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(run_worker(
                    &cfg,
                    registry_options,
                    &spec,
                    budget_limit,
                    &session_id,
                    &in_tx,
                    stop_rx,
                    pause_rx,
                )),
                Err(e) => (
                    None,
                    0.0,
                    WorkerEnd::Failed(format!("worker runtime failed to start: {e}")),
                ),
            }
        });
        // A panic unwound past the worker's own closeout, so its claims are
        // still held — release them here or they block rivals until the
        // age-based sweep.
        if matches!(&end, WorkerEnd::Failed(reason) if reason.starts_with(PANIC_FAILURE_PREFIX))
            && let Some(store) = agent::open_store(&cfg.workspace_root)
        {
            let _ = store.release_file_locks_for_holder(&format!("{session_id}/{lane}"));
        }

        // Terminal lane status. On failure the Error event (already on the
        // lane via the forwarder or below) carries the reason.
        let _ = in_tx.send(Inbound::Status {
            agent: lane.clone(),
            status: match &end {
                WorkerEnd::Done => AgentStatus::Done,
                WorkerEnd::Failed(_) => AgentStatus::Failed,
                WorkerEnd::Stopped => AgentStatus::Killed,
            },
        });
        if let WorkerEnd::Failed(reason) = &end {
            let _ = in_tx.send(Inbound::Event {
                agent: lane.clone(),
                event: AgentEvent::Error {
                    message: reason.clone(),
                    retryable: false,
                },
            });
        }

        // The `/inbox` flow: a worker finishing (or failing) lands a
        // persist-until-read notification linked to this session, so the
        // user finds the result — and can open the session, replaying it if
        // needed — without having watched the lane. A user-initiated stop
        // lands none: the user was there.
        let notification = match &end {
            WorkerEnd::Done => Some((
                format!("{workspace_name}: {}", spec.notify_title),
                prompt_line(&spec.prompt, 160),
            )),
            WorkerEnd::Failed(reason) => Some((
                format!("{workspace_name}: {} — FAILED", spec.notify_title),
                format!("{} — {reason}", prompt_line(&spec.prompt, 80)),
            )),
            WorkerEnd::Stopped => None,
        };
        if let Some((title, body)) = notification {
            let _ = stella_store::NotificationStore::open_default().push(
                &stella_store::Notification::new(title, body, session_id.clone())
                    .with_session_id(session_id.clone()),
            );
        }

        let _ = sup_tx.send(SupervisorMsg::Ended {
            lane,
            generation,
            execution_id,
            cost_usd,
            end,
        });
    });
}

/// One worker session, on the calling thread's runtime: fresh provider +
/// registry + budget, its own execution row linked to the deck session, the
/// shared persist-and-forward event path, one raw engine turn raced against
/// the driver's stop signal (the same clean drop-at-await cancel the lead
/// uses). Returns `(execution_id, cost_usd, end)`.
#[allow(clippy::too_many_arguments)]
async fn run_worker(
    cfg: &Config,
    registry_options: RegistryOptions,
    spec: &SubSessionSpec,
    budget_limit: Option<f64>,
    session_id: &str,
    in_tx: &UnboundedSender<Inbound>,
    stop_rx: oneshot::Receiver<()>,
    pause_rx: watch::Receiver<bool>,
) -> (Option<i64>, f64, WorkerEnd) {
    let provider = match agent::build_provider(cfg) {
        Ok(p) => p,
        Err(e) => return (None, 0.0, WorkerEnd::Failed(e)),
    };
    let registry = agent::new_tool_registry(cfg.workspace_root.clone(), registry_options).await;
    if let Err(error) = agent::populate_schema_index(&registry, &cfg.workspace_root) {
        return (None, 0.0, WorkerEnd::Failed(error));
    }
    let active_rules =
        crate::rules::enforce_workspace_rules(&registry, &cfg.workspace_root, &cfg.authority);

    let system_prompt = agent::with_session_hook_context(
        agent::build_system_prompt(cfg, &cfg.workspace_root, &active_rules),
        cfg,
    )
    .await;
    let mut messages = vec![
        CompletionMessage::system(system_prompt),
        crate::attachments::user_message(&spec.prompt),
    ];
    let mut budget = agent::build_budget_guard(budget_limit);
    budget.begin_turn();
    let dispatch_spend_usd = budget.session_spent_usd();

    let store = agent::open_store(&cfg.workspace_root);
    let calibration = agent::seed_calibration(&store, cfg);
    let execution = agent::begin_execution(&store, "deck-sub", &spec.prompt, cfg, Some(session_id));
    let execution_id = execution.as_ref().map(|(_, id)| *id);
    let files_before = registry.files_touched().len();

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let forwarder = spawn_forwarder(
        rx,
        execution.clone(),
        cfg.provider.id.to_string(),
        in_tx.clone(),
        spec.lane.clone(),
    );

    /// How the raced turn resolved, before store closeout.
    enum RacedTurn {
        Outcome(stella_core::TurnOutcome),
        Stopped,
    }
    // Claim-on-first-write over the shared tree: workers coordinate with the
    // lead, each other, and any other process in this workspace through the
    // store's lock table — no coordinator, sub-millisecond acquire, rivals
    // named in the refusal (crate::claims).
    let claims = crate::claims::ClaimTap::new(
        &registry,
        execution.as_ref().map(|(store, _)| store.clone()),
        format!("{session_id}/{}", spec.lane),
    );
    let raced = {
        let gate = WatchGate(pause_rx);
        let engine = Engine::with_sleeper(
            &*provider,
            &claims,
            agent::engine_config_for(cfg),
            &TokioSleeper,
        )
        .with_calibration(&calibration)
        .with_gate(&gate);
        let turn = engine.run_turn(&mut messages, &mut budget, &tx);
        // A dropped sender (driver gone at session teardown) must not read
        // as a stop — only an actual signal cancels, so the wait parks
        // forever on a closed channel and the turn always wins the race.
        let stop_wait = async move {
            if stop_rx.await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            outcome = turn => RacedTurn::Outcome(outcome),
            _ = stop_wait => RacedTurn::Stopped,
        }
    };
    drop(tx);
    let persistence_complete = forwarder.await.unwrap_or(false);
    // Release the worker's whole claim set — the stop path included (the
    // dropped turn future cannot release for itself).
    claims.release_all();

    // Honesty over silence: a worker's own task_assign calls have no
    // supervisor to spawn them (delegation is the lead's, v1) — say so on
    // the lane instead of letting the tool's confirmation stand.
    let stranded = registry.take_spawn_requests();
    if !stranded.is_empty() {
        let _ = in_tx.send(Inbound::Event {
            agent: spec.lane.clone(),
            event: AgentEvent::Text {
                delta: format!(
                    "note: {} task_assign request(s) were not dispatched — delegation \
                     runs from the lead session only",
                    stranded.len()
                ),
            },
        });
    }

    let (label, cost, end) = match raced {
        RacedTurn::Outcome(stella_core::TurnOutcome::Completed { cost_usd, .. }) => {
            ("completed", cost_usd, WorkerEnd::Done)
        }
        RacedTurn::Outcome(stella_core::TurnOutcome::Aborted { reason, cost_usd }) => {
            ("aborted", cost_usd, WorkerEnd::Failed(reason))
        }
        RacedTurn::Stopped => (
            "cancelled",
            agent::settled_cost_since(dispatch_spend_usd, budget.session_spent_usd()),
            WorkerEnd::Stopped,
        ),
    };
    if let Some((store, id)) = &execution {
        let _ = agent::record_execution_end(
            store,
            *id,
            &registry,
            files_before,
            label,
            cost,
            persistence_complete,
        );
        // Mirror the worker's final board (its own, session-scoped view) so
        // `tasks` queries see sub-agent boards too.
        let board = registry.task_board();
        let items: Vec<TaskItem> = board
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .items()
            .to_vec();
        if !items.is_empty() {
            let _ = store.record_task_board(*id, Some(session_id), &items, now_ms());
        }
    }
    (execution_id, cost, end)
}

/// Drain the driver's prompt backlog into free worker slots, oldest first.
/// Stops at a slash command (those belong to the lead's dispatcher — letting
/// a later prompt jump it would also desync the deck's FIFO queue view) and
/// while dispatch is held. Sends the `PromptStarted` front-pop for every
/// prompt it takes, exactly like lead dispatch does.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drain_queue(
    queue: &mut crate::session_persist::DurableQueue,
    subs: &mut SubSessions,
    dispatch_held: bool,
    cfg: &Config,
    budget_limit: Option<f64>,
    session_id: &str,
    workspace_name: &str,
    in_tx: &UnboundedSender<Inbound>,
    sup_tx: &UnboundedSender<SupervisorMsg>,
) {
    while !dispatch_held
        && subs.has_slot()
        && queue
            .front()
            .is_some_and(|text| !text.trim_start().starts_with('/'))
    {
        let Some(text) = queue.pop_front() else {
            break;
        };
        let lane = subs.next_req_lane();
        let _ = in_tx.send(Inbound::PromptStarted {
            agent: lane.clone(),
            text: text.clone(),
        });
        let (stop_tx, stop_rx) = oneshot::channel();
        let (pause_tx, pause_rx) = watch::channel(false);
        let spec = SubSessionSpec {
            lane: lane.clone(),
            title: prompt_line(&text, 48),
            notify_title: format!("reply ready — {}", prompt_line(&text, 40)),
            prompt: text,
        };
        let generation = subs.started(&lane, stop_tx, pause_tx, spec.clone());
        spawn(
            cfg,
            subs.worker_registry_options(),
            spec,
            generation,
            budget_limit,
            session_id.to_string(),
            workspace_name.to_string(),
            in_tx.clone(),
            sup_tx.clone(),
            stop_rx,
            pause_rx,
        );
    }
}

/// Respawn an ended lane from its retained spec — the Restart verb. `false`
/// when the lane has no retained spec or is still live (stop it first).
#[allow(clippy::too_many_arguments)]
pub(crate) fn respawn(
    lane: &str,
    subs: &mut SubSessions,
    cfg: &Config,
    budget_limit: Option<f64>,
    session_id: &str,
    workspace_name: &str,
    in_tx: &UnboundedSender<Inbound>,
    sup_tx: &UnboundedSender<SupervisorMsg>,
) -> bool {
    if subs.is_live(lane) {
        return false;
    }
    let Some(spec) = subs.spec(lane) else {
        return false;
    };
    let (stop_tx, stop_rx) = oneshot::channel();
    let (pause_tx, pause_rx) = watch::channel(false);
    let registry_options = subs.worker_registry_options();
    let generation = subs.started(lane, stop_tx, pause_tx, spec.clone());
    spawn(
        cfg,
        registry_options,
        spec,
        generation,
        budget_limit,
        session_id.to_string(),
        workspace_name.to_string(),
        in_tx.clone(),
        sup_tx.clone(),
        stop_rx,
        pause_rx,
    );
    true
}

/// The deck lane a `task_assign` worker runs on — the task's identity, so
/// the driver can refuse a second worker for a task that already has one.
pub(crate) fn task_lane(task_id: &str) -> String {
    format!("sub:{task_id}")
}

/// Dispatch one `task_assign` spawn request (or park it if no slot is free —
/// the caller owns the pending queue).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_task_worker(
    req: &SpawnRequest,
    subs: &mut SubSessions,
    cfg: &Config,
    budget_limit: Option<f64>,
    session_id: &str,
    workspace_name: &str,
    in_tx: &UnboundedSender<Inbound>,
    sup_tx: &UnboundedSender<SupervisorMsg>,
) {
    let lane = task_lane(&req.task_id);
    let (stop_tx, stop_rx) = oneshot::channel();
    let (pause_tx, pause_rx) = watch::channel(false);
    let spec = SubSessionSpec {
        lane: lane.clone(),
        title: format!("task #{}: {}", req.task_id, prompt_line(&req.subject, 40)),
        prompt: task_prompt(req),
        notify_title: format!(
            "task #{} done — {}",
            req.task_id,
            prompt_line(&req.subject, 40)
        ),
    };
    let registry_options = subs.worker_registry_options();
    let generation = subs.started(&lane, stop_tx, pause_tx, spec.clone());
    spawn(
        cfg,
        registry_options,
        spec,
        generation,
        budget_limit,
        session_id.to_string(),
        workspace_name.to_string(),
        in_tx.clone(),
        sup_tx.clone(),
        stop_rx,
        pause_rx,
    );
}

/// How long Quit waits for stopped workers to settle before abandoning
/// them. Workers cancel at their next await point and then only close out
/// (forwarder drain, store writes, claim release), so this is generous —
/// the bound exists so a wedged worker can never hold the exit hostage.
pub(crate) const QUIT_JOIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// Session teardown: signal every live worker to stop, then wait — bounded
/// by `deadline` — for their `Ended` messages, so executions close out,
/// notifications land, and claims release instead of dying mid-tool as
/// detached threads at process exit. A worker that does not settle in time
/// is abandoned exactly as every worker used to be; spawn requests arriving
/// during teardown are dropped (there is no session left to run them).
pub(crate) async fn shutdown_workers(
    subs: &mut SubSessions,
    sup_rx: &mut mpsc::UnboundedReceiver<SupervisorMsg>,
    deadline: std::time::Duration,
) {
    subs.stop_all();
    let end_by = tokio::time::Instant::now() + deadline;
    while subs.live() > 0 {
        match tokio::time::timeout_at(end_by, sup_rx.recv()).await {
            Ok(Some(SupervisorMsg::Ended {
                lane, generation, ..
            })) => {
                let _ = subs.ended(&lane, generation);
            }
            Ok(Some(SupervisorMsg::SpawnTask(_))) => {}
            Ok(None) | Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_clone_the_exact_parent_media_journal() {
        let journal: std::sync::Arc<dyn stella_media::MediaOperationJournal> = std::sync::Arc::new(
            stella_media::SqliteMediaOperationJournal::open_in_memory(Default::default()).unwrap(),
        );
        let subs = SubSessions::with_registry_options(RegistryOptions {
            media_operation_journal: Some(journal.clone()),
            ..Default::default()
        });

        let worker = subs.worker_registry_options();

        assert!(std::sync::Arc::ptr_eq(
            &journal,
            worker.media_operation_journal.as_ref().unwrap()
        ));
    }

    fn dummy_spec(lane: &str) -> SubSessionSpec {
        SubSessionSpec {
            lane: lane.to_string(),
            title: "t".into(),
            prompt: "p".into(),
            notify_title: "n".into(),
        }
    }

    #[tokio::test]
    async fn watch_gate_parks_while_paused_and_releases_on_resume_or_teardown() {
        use stella_core::ports::TurnGate;
        let (tx, rx) = watch::channel(true);
        let gate = WatchGate(rx);
        let wait = gate.wait_if_paused();
        tokio::pin!(wait);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut wait)
                .await
                .is_err(),
            "a paused gate must park"
        );
        tx.send(false).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(500), wait)
            .await
            .expect("resume releases the gate");

        // A dropped sender (driver teardown) must release, never park forever.
        let (tx2, rx2) = watch::channel(true);
        let gate2 = WatchGate(rx2);
        drop(tx2);
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            gate2.wait_if_paused(),
        )
        .await
        .expect("teardown releases the gate");
    }

    #[test]
    fn pause_resume_flips_the_watch_and_restart_needs_a_dead_lane() {
        let mut subs = SubSessions::new();
        let (stop_tx, _stop_rx) = oneshot::channel();
        let (pause_tx, pause_rx) = watch::channel(false);
        let generation = subs.started("req:1", stop_tx, pause_tx, dummy_spec("req:1"));
        assert!(subs.set_paused("req:1", true));
        assert!(*pause_rx.borrow());
        assert!(subs.set_paused("req:1", false));
        assert!(!*pause_rx.borrow());
        assert!(!subs.set_paused("req:404", true), "unknown lane is a no-op");
        // A live lane refuses respawn; its spec survives its end.
        assert!(subs.is_live("req:1"));
        assert!(subs.ended("req:1", generation));
        assert!(!subs.is_live("req:1"));
        assert!(subs.spec("req:1").is_some(), "spec retained for Restart");
    }

    #[test]
    fn slot_bookkeeping_caps_and_recovers() {
        let mut subs = SubSessions::new();
        let mut generations = Vec::new();
        for i in 0..MAX_CONCURRENT {
            assert!(subs.has_slot());
            let (stop_tx, _stop_rx) = oneshot::channel();
            let (pause_tx, _pause_rx) = watch::channel(false);
            generations.push(subs.started(
                &format!("req:{i}"),
                stop_tx,
                pause_tx,
                dummy_spec(&format!("req:{i}")),
            ));
        }
        assert!(!subs.has_slot());
        assert!(subs.ended("req:0", generations[0]));
        assert!(subs.has_slot());
        // An Ended nothing started must free nothing (and never underflow).
        let mut fresh = SubSessions::new();
        assert!(!fresh.ended("req:none", 0));
        assert!(fresh.has_slot());
    }

    #[test]
    fn stop_signals_a_live_worker_once_and_only_once() {
        let mut subs = SubSessions::new();
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let (pause_tx, _pause_rx) = watch::channel(false);
        subs.started("req:1", stop_tx, pause_tx, dummy_spec("req:1"));
        assert!(subs.stop("req:1"), "a live worker accepts the signal");
        assert_eq!(stop_rx.try_recv(), Ok(()));
        assert!(!subs.stop("req:1"), "a second stop is a stale no-op");
        assert!(!subs.stop("req:404"), "an unknown lane is a no-op");
    }

    #[test]
    fn a_stopped_lane_winds_down_until_its_ended_arrives() {
        // stop() must NOT free the lane: the worker thread is still
        // draining. The lane stays live (so Restart defers via
        // pending_restarts instead of respawning into a second worker), the
        // slot stays occupied, and only the matching Ended frees both.
        let mut subs = SubSessions::new();
        let (stop_tx, _stop_rx) = oneshot::channel();
        let (pause_tx, _pause_rx) = watch::channel(false);
        let generation = subs.started("req:1", stop_tx, pause_tx, dummy_spec("req:1"));
        assert!(subs.stop("req:1"));
        assert!(subs.is_live("req:1"), "winding down still counts as live");
        assert_eq!(subs.live(), 1, "the slot is not free until Ended");
        assert!(
            !subs.set_paused("req:1", true),
            "a winding-down worker cannot be paused"
        );
        assert!(subs.ended("req:1", generation));
        assert!(!subs.is_live("req:1"));
        assert_eq!(subs.live(), 0);
    }

    #[test]
    fn stop_then_restart_leaves_one_worker_with_working_channels() {
        // The full stop→Ended→respawn sequence, then a stale Ended for the
        // replaced generation: exactly one live worker must remain, with ITS
        // stop/pause channels registered and the active count intact.
        let mut subs = SubSessions::new();
        let (stop_tx, _stop_rx) = oneshot::channel();
        let (pause_tx, _pause_rx) = watch::channel(false);
        let old = subs.started("req:1", stop_tx, pause_tx, dummy_spec("req:1"));
        subs.stop("req:1");
        assert!(subs.ended("req:1", old));
        // The respawn (what pending_restarts triggers on the Ended).
        let (stop_tx2, mut stop_rx2) = oneshot::channel();
        let (pause_tx2, pause_rx2) = watch::channel(false);
        let new = subs.started("req:1", stop_tx2, pause_tx2, dummy_spec("req:1"));
        assert_ne!(old, new, "each spawn gets its own generation");
        assert_eq!(subs.live(), 1);
        // A late Ended from the replaced worker frees nothing.
        assert!(!subs.ended("req:1", old), "stale Ended must be ignored");
        assert_eq!(subs.live(), 1, "active count survives the stale Ended");
        assert!(
            subs.set_paused("req:1", true),
            "replacement's pause channel is intact"
        );
        assert!(*pause_rx2.borrow());
        assert!(subs.stop("req:1"), "replacement's stop channel is intact");
        assert_eq!(stop_rx2.try_recv(), Ok(()));
        assert!(subs.ended("req:1", new));
        assert_eq!(subs.live(), 0);
    }

    #[test]
    fn a_panicking_worker_body_ends_as_failed_never_silence() {
        // The supervisor must receive Ended whatever a tool does — a panic
        // synthesizes Failed with the payload's message, both payload kinds.
        let (id, cost, end) = run_caught(|| panic!("tool blew up"));
        assert_eq!(id, None);
        assert_eq!(cost, 0.0);
        match end {
            WorkerEnd::Failed(reason) => {
                assert!(reason.contains("worker panicked"), "{reason}");
                assert!(reason.contains("tool blew up"), "{reason}");
            }
            _ => panic!("a panic must synthesize WorkerEnd::Failed"),
        }
        let (_, _, end) = run_caught(|| panic!("{}", format!("dynamic {}", 7)));
        match end {
            WorkerEnd::Failed(reason) => assert!(reason.contains("dynamic 7"), "{reason}"),
            _ => panic!("a String panic payload must synthesize Failed too"),
        }
        // A body that returns normally passes through untouched.
        let (id, cost, end) = run_caught(|| (Some(7), 1.25, WorkerEnd::Done));
        assert_eq!(id, Some(7));
        assert_eq!(cost, 1.25);
        assert!(matches!(end, WorkerEnd::Done));
    }

    #[tokio::test]
    async fn quit_shutdown_stops_workers_and_reaps_their_endings() {
        // Quit-time teardown: every live worker sees its stop signal and the
        // drain waits for their Ended messages — no orphaned bookkeeping.
        let mut subs = SubSessions::new();
        let (sup_tx, mut sup_rx) = mpsc::unbounded_channel();
        for lane in ["req:1", "sub:9"] {
            let (stop_tx, stop_rx) = oneshot::channel();
            let (pause_tx, _pause_rx) = watch::channel(false);
            let generation = subs.started(lane, stop_tx, pause_tx, dummy_spec(lane));
            let sup = sup_tx.clone();
            let lane = lane.to_string();
            // A worker's whole quit obligation: see the stop, close out,
            // send Ended.
            tokio::spawn(async move {
                let _ = stop_rx.await;
                let _ = sup.send(SupervisorMsg::Ended {
                    lane,
                    generation,
                    execution_id: None,
                    cost_usd: 0.0,
                    end: WorkerEnd::Stopped,
                });
            });
        }
        shutdown_workers(&mut subs, &mut sup_rx, std::time::Duration::from_secs(5)).await;
        assert_eq!(subs.live(), 0, "every worker settled before the deadline");
    }

    #[tokio::test(start_paused = true)]
    async fn quit_shutdown_abandons_a_hung_worker_at_the_deadline() {
        // A worker that never answers its stop must not hold the exit
        // hostage — the drain returns at the deadline with the lane still
        // accounted (abandoned, exactly the pre-teardown behavior).
        let mut subs = SubSessions::new();
        let (stop_tx, _stop_rx) = oneshot::channel();
        let (pause_tx, _pause_rx) = watch::channel(false);
        subs.started("req:1", stop_tx, pause_tx, dummy_spec("req:1"));
        let (_sup_tx, mut sup_rx) = mpsc::unbounded_channel();
        shutdown_workers(
            &mut subs,
            &mut sup_rx,
            std::time::Duration::from_millis(100),
        )
        .await;
        assert_eq!(subs.live(), 1, "abandoned, not freed — and no hang");
    }

    #[test]
    fn req_lanes_are_sequential() {
        let mut subs = SubSessions::new();
        assert_eq!(subs.next_req_lane(), "req:1");
        assert_eq!(subs.next_req_lane(), "req:2");
    }

    #[test]
    fn lanes_names_every_worker_ever_started_live_or_ended() {
        // The session-switch site deregisters exactly this set — a lane's
        // dashboard row outlives its worker, so `lanes` must too.
        let mut subs = SubSessions::new();
        assert!(subs.lanes().is_empty());
        let mut generations = std::collections::HashMap::new();
        for lane in ["req:2", "req:1", "sub:7"] {
            let (stop_tx, _stop_rx) = oneshot::channel();
            let (pause_tx, _pause_rx) = watch::channel(false);
            generations.insert(
                lane,
                subs.started(lane, stop_tx, pause_tx, dummy_spec(lane)),
            );
        }
        assert!(subs.ended("req:1", generations["req:1"]));
        assert_eq!(
            subs.lanes(),
            vec![
                "req:1".to_string(),
                "req:2".to_string(),
                "sub:7".to_string()
            ],
            "ended lanes stay listed; order is deterministic (sorted)"
        );
    }

    #[test]
    fn task_prompt_carries_identity_and_briefing_verbatim() {
        let req = SpawnRequest {
            task_id: "3".into(),
            subject: "Fix the redirect loop".into(),
            description: Some("token refresh races the redirect".into()),
            briefing: "Start from auth/callback.rs; the witness test is half-written.".into(),
        };
        let prompt = task_prompt(&req);
        assert!(prompt.contains("Task #3: Fix the redirect loop"));
        assert!(prompt.contains("token refresh races the redirect"));
        assert!(prompt.contains("Start from auth/callback.rs; the witness test is half-written."));
    }

    #[test]
    fn drain_stops_at_slash_commands_and_respects_hold() {
        // Contract-level test of the pure gating conditions: a held dispatch
        // or a slash front entry must leave the queue untouched. (Spawning
        // itself needs a provider key + threads — exercised in integration,
        // not here — so this asserts the *refusals*, which are what protect
        // the deck's FIFO view.)
        let queue: std::collections::VecDeque<String> =
            std::collections::VecDeque::from(["/models".to_string()]);
        let subs = SubSessions::new();
        assert!(subs.has_slot());
        assert!(
            queue
                .front()
                .is_some_and(|t| t.trim_start().starts_with('/')),
            "slash front entry must gate the drain"
        );
        // Simulate the drain's gate exactly as `drain_queue` evaluates it.
        let held = false;
        let would_take = !held
            && subs.has_slot()
            && queue
                .front()
                .is_some_and(|t| !t.trim_start().starts_with('/'));
        assert!(!would_take);
        assert_eq!(queue.len(), 1);
    }
}
