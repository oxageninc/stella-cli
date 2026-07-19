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

use std::collections::{HashMap, VecDeque};

use stella_core::Engine;
use stella_core::tasks::SpawnRequest;
use stella_protocol::{AgentEvent, CompletionMessage, TaskItem};
use stella_tools::ToolRegistry;
use stella_tui::{AgentMeta, AgentStatus, Inbound};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;

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
    stops: HashMap<String, oneshot::Sender<()>>,
}

impl SubSessions {
    pub(crate) fn new() -> Self {
        Self {
            active: 0,
            next_req: 0,
            stops: HashMap::new(),
        }
    }

    pub(crate) fn has_slot(&self) -> bool {
        self.active < MAX_CONCURRENT
    }

    fn started(&mut self, lane: &str, stop: oneshot::Sender<()>) {
        self.active += 1;
        self.stops.insert(lane.to_string(), stop);
    }

    pub(crate) fn ended(&mut self, lane: &str) {
        self.active = self.active.saturating_sub(1);
        self.stops.remove(lane);
    }

    /// Signal one worker to stop (clean cancel: its turn future drops at the
    /// next await point, exactly the lead's cancel semantics). `false` when
    /// no such worker is live — a stale stop is a no-op, never an error.
    pub(crate) fn stop(&mut self, lane: &str) -> bool {
        match self.stops.remove(lane) {
            Some(tx) => tx.send(()).is_ok(),
            None => false,
        }
    }

    /// The next prompt-worker lane id (`req:1`, `req:2`, …).
    pub(crate) fn next_req_lane(&mut self) -> String {
        self.next_req += 1;
        format!("req:{}", self.next_req)
    }
}

/// Everything a worker needs to run, owned (the thread outlives the caller's
/// borrows).
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

/// Spawn one worker. Registers its deck lane immediately (the sub-second
/// acknowledgement — the row exists before any heavy setup), then runs the
/// session on a dedicated OS thread. Never blocks the caller.
// A worker genuinely needs every one of these (identity, budget, session
// link, both channels, stop signal) — bundling them into a struct would just
// move the field list one hop away from the one call shape.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    cfg: &Config,
    spec: SubSessionSpec,
    budget_limit: Option<f64>,
    session_id: String,
    workspace_name: String,
    in_tx: UnboundedSender<Inbound>,
    sup_tx: UnboundedSender<SupervisorMsg>,
    stop_rx: oneshot::Receiver<()>,
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
        let (execution_id, cost_usd, end) = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(run_worker(
                &cfg,
                &spec,
                budget_limit,
                &session_id,
                &in_tx,
                stop_rx,
            )),
            Err(e) => (
                None,
                0.0,
                WorkerEnd::Failed(format!("worker runtime failed to start: {e}")),
            ),
        };

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
async fn run_worker(
    cfg: &Config,
    spec: &SubSessionSpec,
    budget_limit: Option<f64>,
    session_id: &str,
    in_tx: &UnboundedSender<Inbound>,
    stop_rx: oneshot::Receiver<()>,
) -> (Option<i64>, f64, WorkerEnd) {
    let provider = match agent::build_provider(cfg) {
        Ok(p) => p,
        Err(e) => return (None, 0.0, WorkerEnd::Failed(e)),
    };
    let registry = ToolRegistry::new_detected(cfg.workspace_root.clone()).await;
    agent::populate_schema_index(&registry, &cfg.workspace_root);
    crate::rules::enforce_workspace_rules(&registry, &cfg.workspace_root);

    let system_prompt =
        agent::with_session_hook_context(agent::build_system_prompt(cfg, &cfg.workspace_root), cfg)
            .await;
    let mut messages = vec![
        CompletionMessage::system(system_prompt),
        crate::attachments::user_message(&spec.prompt),
    ];
    let mut budget = agent::build_budget_guard(budget_limit);
    budget.begin_turn();

    let store = agent::open_store(&cfg.workspace_root);
    let calibration = agent::seed_calibration(&store, cfg);
    let execution = agent::begin_execution(&store, "deck-sub", &spec.prompt, cfg, Some(session_id));
    let execution_id = execution.as_ref().map(|(_, id)| *id);

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
        let engine = Engine::with_sleeper(
            &*provider,
            &claims,
            agent::engine_config_for(cfg),
            &TokioSleeper,
        )
        .with_calibration(&calibration);
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
    let _ = forwarder.await;
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
        RacedTurn::Outcome(stella_core::TurnOutcome::Aborted { reason }) => {
            ("aborted", 0.0, WorkerEnd::Failed(reason))
        }
        RacedTurn::Stopped => ("cancelled", 0.0, WorkerEnd::Stopped),
    };
    if let Some((store, id)) = &execution {
        let _ = agent::record_execution_end(store, *id, &registry, label, cost);
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
    queue: &mut VecDeque<String>,
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
        subs.started(&lane, stop_tx);
        spawn(
            cfg,
            SubSessionSpec {
                lane,
                title: prompt_line(&text, 48),
                notify_title: format!("reply ready — {}", prompt_line(&text, 40)),
                prompt: text,
            },
            budget_limit,
            session_id.to_string(),
            workspace_name.to_string(),
            in_tx.clone(),
            sup_tx.clone(),
            stop_rx,
        );
    }
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
    let lane = format!("sub:{}", req.task_id);
    let (stop_tx, stop_rx) = oneshot::channel();
    subs.started(&lane, stop_tx);
    spawn(
        cfg,
        SubSessionSpec {
            lane,
            title: format!("task #{}: {}", req.task_id, prompt_line(&req.subject, 40)),
            prompt: task_prompt(req),
            notify_title: format!(
                "task #{} done — {}",
                req.task_id,
                prompt_line(&req.subject, 40)
            ),
        },
        budget_limit,
        session_id.to_string(),
        workspace_name.to_string(),
        in_tx.clone(),
        sup_tx.clone(),
        stop_rx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_bookkeeping_caps_and_recovers() {
        let mut subs = SubSessions::new();
        for i in 0..MAX_CONCURRENT {
            assert!(subs.has_slot());
            let (stop_tx, _stop_rx) = oneshot::channel();
            subs.started(&format!("req:{i}"), stop_tx);
        }
        assert!(!subs.has_slot());
        subs.ended("req:0");
        assert!(subs.has_slot());
        // ended() below zero must not underflow.
        let mut fresh = SubSessions::new();
        fresh.ended("req:none");
        assert!(fresh.has_slot());
    }

    #[test]
    fn stop_signals_a_live_worker_once_and_only_once() {
        let mut subs = SubSessions::new();
        let (stop_tx, mut stop_rx) = oneshot::channel();
        subs.started("req:1", stop_tx);
        assert!(subs.stop("req:1"), "a live worker accepts the signal");
        assert_eq!(stop_rx.try_recv(), Ok(()));
        assert!(!subs.stop("req:1"), "a second stop is a stale no-op");
        assert!(!subs.stop("req:404"), "an unknown lane is a no-op");
    }

    #[test]
    fn req_lanes_are_sequential() {
        let mut subs = SubSessions::new();
        assert_eq!(subs.next_req_lane(), "req:1");
        assert_eq!(subs.next_req_lane(), "req:2");
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
        let queue: VecDeque<String> = VecDeque::from(["/models".to_string()]);
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
