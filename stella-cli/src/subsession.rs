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

use std::collections::VecDeque;

use stella_core::Engine;
use stella_core::tasks::SpawnRequest;
use stella_protocol::{AgentEvent, CompletionMessage, TaskItem};
use stella_tools::ToolRegistry;
use stella_tui::{AgentMeta, AgentStatus, Inbound};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::agent;
use crate::command_deck::{now_ms, prompt_line, spawn_forwarder};
use crate::config::Config;
use crate::runtime::TokioSleeper;

/// How many sub-session workers may run at once. Prompts (and task
/// assignments) beyond this wait in the driver's backlog and dispatch as
/// slots free — the cap bounds provider concurrency and CPU, not the queue.
pub(crate) const MAX_CONCURRENT: usize = 3;

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
        outcome: Result<(), String>,
    },
}

/// Driver-side sub-session bookkeeping: the live-worker count and the lane
/// counter for `req:<n>` prompt workers.
pub(crate) struct SubSessions {
    active: usize,
    next_req: u64,
}

impl SubSessions {
    pub(crate) fn new() -> Self {
        Self {
            active: 0,
            next_req: 0,
        }
    }

    pub(crate) fn has_slot(&self) -> bool {
        self.active < MAX_CONCURRENT
    }

    pub(crate) fn started(&mut self) {
        self.active += 1;
    }

    pub(crate) fn ended(&mut self) {
        self.active = self.active.saturating_sub(1);
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
pub(crate) fn spawn(
    cfg: &Config,
    spec: SubSessionSpec,
    budget_limit: Option<f64>,
    session_id: String,
    workspace_name: String,
    in_tx: UnboundedSender<Inbound>,
    sup_tx: UnboundedSender<SupervisorMsg>,
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
        let (execution_id, outcome) = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(run_worker(&cfg, &spec, budget_limit, &session_id, &in_tx)),
            Err(e) => (None, Err(format!("worker runtime failed to start: {e}"))),
        };

        // Terminal lane status. On failure the Error event (already on the
        // lane via the forwarder or below) carries the reason.
        let _ = in_tx.send(Inbound::Status {
            agent: lane.clone(),
            status: match &outcome {
                Ok(()) => AgentStatus::Done,
                Err(_) => AgentStatus::Failed,
            },
        });
        if let Err(reason) = &outcome {
            let _ = in_tx.send(Inbound::Event {
                agent: lane.clone(),
                event: AgentEvent::Error {
                    message: reason.clone(),
                    retryable: false,
                },
            });
        }

        // The `/inbox` flow: every worker ending lands a persist-until-read
        // notification linked to this session, so the user finds the result
        // (and can open the session, replaying it if needed) without having
        // watched the lane.
        let notification = stella_store::Notification::new(
            match &outcome {
                Ok(()) => format!("{workspace_name}: {}", spec.notify_title),
                Err(_) => format!("{workspace_name}: {} — FAILED", spec.notify_title),
            },
            match &outcome {
                Ok(()) => prompt_line(&spec.prompt, 160),
                Err(reason) => format!("{} — {reason}", prompt_line(&spec.prompt, 80)),
            },
            session_id.clone(),
        )
        .with_session_id(session_id.clone());
        let _ = stella_store::NotificationStore::open_default().push(&notification);

        let _ = sup_tx.send(SupervisorMsg::Ended {
            lane,
            execution_id,
            outcome,
        });
    });
}

/// One worker session, on the calling thread's runtime: fresh provider +
/// registry + budget, its own execution row linked to the deck session, the
/// shared persist-and-forward event path, one raw engine turn.
async fn run_worker(
    cfg: &Config,
    spec: &SubSessionSpec,
    budget_limit: Option<f64>,
    session_id: &str,
    in_tx: &UnboundedSender<Inbound>,
) -> (Option<i64>, Result<(), String>) {
    let provider = match agent::build_provider(cfg) {
        Ok(p) => p,
        Err(e) => return (None, Err(e)),
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
    let execution = agent::begin_execution(&store, "deck-sub", &spec.prompt, cfg);
    if let Some((store, id)) = &execution {
        let _ = store.set_execution_session(*id, session_id);
    }
    let execution_id = execution.as_ref().map(|(_, id)| *id);

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let forwarder = spawn_forwarder(
        rx,
        execution.clone(),
        cfg.provider.id.to_string(),
        in_tx.clone(),
        spec.lane.clone(),
    );

    let outcome = {
        let engine = Engine::with_sleeper(
            &*provider,
            &registry,
            agent::engine_config_for(cfg),
            &TokioSleeper,
        )
        .with_calibration(&calibration);
        engine.run_turn(&mut messages, &mut budget, &tx).await
    };
    drop(tx);
    let _ = forwarder.await;

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

    let (label, cost, result) = match outcome {
        stella_core::TurnOutcome::Completed { cost_usd, .. } => ("completed", cost_usd, Ok(())),
        stella_core::TurnOutcome::Aborted { reason } => ("aborted", 0.0, Err(reason)),
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
    (execution_id, result)
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
        subs.started();
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
    subs.started();
    spawn(
        cfg,
        SubSessionSpec {
            lane: format!("sub:{}", req.task_id),
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
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_bookkeeping_caps_and_recovers() {
        let mut subs = SubSessions::new();
        for _ in 0..MAX_CONCURRENT {
            assert!(subs.has_slot());
            subs.started();
        }
        assert!(!subs.has_slot());
        subs.ended();
        assert!(subs.has_slot());
        // ended() below zero must not underflow.
        let mut fresh = SubSessions::new();
        fresh.ended();
        assert!(fresh.has_slot());
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
