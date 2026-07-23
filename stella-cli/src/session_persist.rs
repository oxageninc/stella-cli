//! Deck-session durability — the driver-side half of `stella-store::journal`.
//!
//! The deck renders exclusively from its inbound envelope stream (L-T1), so
//! durably persisting a session is one interception: every fold-relevant
//! [`Inbound`] is mirrored to the session's append-only journal at the single
//! channel choke point ([`spawn_journal_tee`]), and resuming is replaying
//! that journal back through the same fold. The two remaining pieces of
//! state the envelope stream cannot carry — the LLM conversation
//! (`Vec<CompletionMessage>`, snapshotted at turn boundaries) and the pending
//! prompt backlog ([`DurableQueue`], write-through on every mutation) — are
//! atomic sidecar snapshots.
//!
//! Net effect: quit, Ctrl-C, a killed terminal, a dropped connection, or a
//! power cut — the session reopens where it stood (`stella resume`, or ⏎ in
//! the SESSIONS overlay), with at most the in-flight turn's streamed tail to
//! re-run (its prompt comes back at the front of the queue).
//!
//! Everything here is best-effort by the store's own contract: persistence
//! failure degrades to a one-time transcript warning, never a work stoppage.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use stella_protocol::CompletionMessage;
use stella_store::journal::{self, JournalRecord, SessionJournal};
use stella_store::{SessionRecord, SessionRegistry, SessionStatus};
use stella_tui::{AgentMeta, AgentStatus, Inbound};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// The lane-id prefix of read-only session replays
/// ([`stella_tui::WorkspaceInput::SessionOpen`] → a `replay:<id>` lane).
/// Those envelopes ride the same inbound channel as live work but are
/// HISTORY, not history in the making — [`journal_record`] filters them so
/// opening an old session can never copy its past into THIS session's
/// journal.
pub const REPLAY_LANE_PREFIX: &str = "replay:";

/// What `stella resume` was asked to reopen.
#[derive(Debug, Clone)]
pub enum ResumeRequest {
    /// The most recently active resumable session for this workspace.
    Latest,
    /// One specific registry id (`ses-…`).
    Id(String),
}

/// The fold-relevant subset of one [`Inbound`], mapped to its journal form.
/// `None` for everything the fold ignores — out-of-band view snapshots
/// (graph, skills, MCP, sessions, notifications, issues…) are regenerated at
/// startup, and `PromptRequeued` is deliberately excluded: the pending
/// backlog is restored from `queue.json` and re-seeded, so journaling
/// requeues would double-display those prompts after a resume. Also `None`
/// for every envelope on a [`REPLAY_LANE_PREFIX`] lane: a read-only replay
/// of another session must never be journaled as this session's history.
pub fn journal_record(inbound: &Inbound) -> Option<JournalRecord> {
    if lane(inbound).is_some_and(|l| l.starts_with(REPLAY_LANE_PREFIX)) {
        return None;
    }
    match inbound {
        Inbound::Register(meta) => Some(JournalRecord::Register {
            agent: meta.id.clone(),
            title: meta.title.clone(),
            role: meta.role.clone(),
            model: meta.model.clone(),
        }),
        Inbound::Event { agent, event } => Some(JournalRecord::Event {
            agent: agent.clone(),
            event: event.clone(),
        }),
        Inbound::Status { agent, status } => Some(JournalRecord::Status {
            agent: agent.clone(),
            status: status_key(*status).to_string(),
        }),
        Inbound::PromptStarted { agent, text } => Some(JournalRecord::PromptStarted {
            agent: agent.clone(),
            text: text.clone(),
        }),
        Inbound::SessionReset { agent } => Some(JournalRecord::SessionReset {
            agent: agent.clone(),
        }),
        Inbound::Pipeline(on) => Some(JournalRecord::Pipeline { on: *on }),
        // Deregister is visual lifecycle only — a row removal during THIS
        // process's dashboard handover (session switch), never part of any
        // session's history. Journaling it would erase the departing
        // session's worker rows from its OWN future replay.
        Inbound::Deregister { .. } => None,
        _ => None,
    }
}

/// Every non-lead lane a journal names, deduplicated in first-appearance
/// order — the dashboard rows replaying `records` creates (the fold
/// auto-registers a lane on any envelope naming it, not just `Register`).
/// The driver remembers these across an adoption so that navigating to
/// another session can [`Inbound::Deregister`] them — rows of the session
/// left behind must not linger on the next session's dashboard.
pub fn journal_lanes(records: &[JournalRecord], lead: &str) -> Vec<String> {
    let mut lanes: Vec<String> = Vec::new();
    for record in records {
        let agent = match record {
            JournalRecord::Register { agent, .. }
            | JournalRecord::Event { agent, .. }
            | JournalRecord::Status { agent, .. }
            | JournalRecord::PromptStarted { agent, .. }
            | JournalRecord::SessionReset { agent } => agent,
            JournalRecord::Pipeline { .. } => continue,
        };
        if agent != lead && !lanes.iter().any(|l| l == agent) {
            lanes.push(agent.clone());
        }
    }
    lanes
}

/// The lane (agent id) an envelope belongs to, when it has one.
/// `Pipeline` (and the out-of-band snapshots) are laneless.
fn lane(inbound: &Inbound) -> Option<&str> {
    match inbound {
        Inbound::Register(meta) => Some(&meta.id),
        Inbound::Event { agent, .. }
        | Inbound::Status { agent, .. }
        | Inbound::PromptStarted { agent, .. }
        | Inbound::PromptRequeued { agent, .. }
        | Inbound::SessionReset { agent } => Some(agent),
        _ => None,
    }
}

/// A journal record mapped back to the [`Inbound`] the fold replays.
/// `started_ms` stamps re-registered agents (elapsed clocks restart at
/// resume time — the honest reading, since the old wall-clock gap wasn't
/// work). `None` for a `Status` word this build doesn't know (a record from
/// a newer version): skipped, exactly like an unparsable journal line.
pub fn replay_inbound(record: JournalRecord, started_ms: u64) -> Option<Inbound> {
    match record {
        JournalRecord::Register {
            agent,
            title,
            role,
            model,
        } => {
            let mut meta = AgentMeta::new(agent, title, started_ms).with_role(role);
            meta.model = model;
            Some(Inbound::Register(meta))
        }
        JournalRecord::Event { agent, event } => Some(Inbound::Event { agent, event }),
        JournalRecord::Status { agent, status } => Some(Inbound::Status {
            agent,
            status: status_from_key(&status)?,
        }),
        JournalRecord::PromptStarted { agent, text } => {
            Some(Inbound::PromptStarted { agent, text })
        }
        JournalRecord::SessionReset { agent } => Some(Inbound::SessionReset { agent }),
        JournalRecord::Pipeline { on } => Some(Inbound::Pipeline(on)),
    }
}

/// Stable snake_case wire words for [`AgentStatus`] — the journal's status
/// vocabulary. `waiting_input` is also the settle marker
/// (`stella_store::journal::unsettled_prompts`) — renaming any of these is a
/// wire break.
fn status_key(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Queued => "queued",
        AgentStatus::Running => "running",
        AgentStatus::Paused => "paused",
        AgentStatus::WaitingInput => "waiting_input",
        AgentStatus::Done => "done",
        AgentStatus::Failed => "failed",
        AgentStatus::Killed => "killed",
    }
}

fn status_from_key(key: &str) -> Option<AgentStatus> {
    Some(match key {
        "queued" => AgentStatus::Queued,
        "running" => AgentStatus::Running,
        "paused" => AgentStatus::Paused,
        "waiting_input" => AgentStatus::WaitingInput,
        "done" => AgentStatus::Done,
        "failed" => AgentStatus::Failed,
        "killed" => AgentStatus::Killed,
        _ => return None,
    })
}

/// The shared journal handle: the tee task writes through it, and the driver
/// swaps it when the deck navigates to another session. `None` = journaling
/// unavailable (open failed) — the session still runs, it just isn't durable,
/// and the tee surfaces that once.
pub struct SessionSink {
    journal: Option<SessionJournal>,
}

pub type SharedSink = Arc<Mutex<SessionSink>>;

impl SessionSink {
    pub fn shared(journal: Option<SessionJournal>) -> SharedSink {
        Arc::new(Mutex::new(SessionSink { journal }))
    }

    fn write(&mut self, record: &JournalRecord) -> Result<(), String> {
        match &mut self.journal {
            Some(j) => j.write(record).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// Drain + fsync (clean-shutdown / handoff barrier).
    pub fn sync(&mut self) {
        if let Some(j) = &mut self.journal {
            let _ = j.sync();
        }
    }

    /// Swap to another session's journal; the old one drains and fsyncs on
    /// drop. `None` turns journaling off (the replacement failed to open).
    pub fn swap(&mut self, journal: Option<SessionJournal>) {
        self.journal = journal;
    }
}

/// The boxed panic-hook type `std::panic::take_hook` hands back.
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Installs a layered panic hook that flushes the session journal before an
/// abort-build panic kills the process, and puts the previous hook back on
/// drop — the journal's only flush point on that path, since under
/// `[profile.release] panic = "abort"` neither `catch_unwind` nor any `Drop`
/// runs (stella-tui's own hook restores the terminal the same way). In
/// unwind builds the sync is skipped: panics there are either caught (panel
/// draws, worker bodies) or unwind into the orderly teardown that already
/// syncs. Best-effort by the store's contract — `try_lock`, because the
/// panicking process must never deadlock on a sink another thread holds.
pub struct JournalPanicGuard {
    prev: Arc<PanicHook>,
}

impl JournalPanicGuard {
    pub fn install(sink: SharedSink) -> Self {
        let prev: Arc<PanicHook> = Arc::new(std::panic::take_hook());
        let delegate = Arc::clone(&prev);
        std::panic::set_hook(Box::new(move |info| {
            if cfg!(panic = "abort")
                && let Ok(mut sink) = sink.try_lock()
            {
                sink.sync();
            }
            (*delegate)(info);
        }));
        Self { prev }
    }
}

impl Drop for JournalPanicGuard {
    fn drop(&mut self) {
        let prev = Arc::clone(&self.prev);
        std::panic::set_hook(Box::new(move |info| (*prev)(info)));
    }
}

/// Sit between the driver's send side and the deck's receive side: mirror
/// every fold-relevant envelope into the session journal, then forward it
/// untouched. One interception point — every producer (the driver loop, the
/// turn forwarder, the ask-user io, spawned notifiers) already funnels
/// through this channel, so nothing can bypass the journal by construction.
///
/// The first persistence failure lands one warning in the transcript (via
/// the forward path, so it is visible wherever the user is looking) and
/// journaling goes quiet — the store contract: observability loss, never a
/// work stoppage.
pub fn spawn_journal_tee(
    mut raw_rx: UnboundedReceiver<Inbound>,
    deck_tx: UnboundedSender<Inbound>,
    sink: SharedSink,
    lead: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut warned = false;
        while let Some(inbound) = raw_rx.recv().await {
            if let Some(record) = journal_record(&inbound) {
                let outcome = {
                    let mut sink = sink.lock().unwrap_or_else(|p| p.into_inner());
                    sink.write(&record)
                };
                if let Err(reason) = outcome
                    && !warned
                {
                    warned = true;
                    let _ = deck_tx.send(Inbound::Event {
                        agent: lead.to_string(),
                        event: stella_protocol::AgentEvent::Error {
                            message: format!(
                                "session journal write failed — this session will not be \
                                 resumable past this point ({reason})"
                            ),
                            retryable: true,
                        },
                    });
                }
            }
            let _ = deck_tx.send(inbound);
        }
        // The driver dropped its sender: final barrier before the deck ends.
        sink.lock().unwrap_or_else(|p| p.into_inner()).sync();
    })
}

/// The dispatch backlog with write-through durability: every mutation lands
/// `queue.json` before it is observable, so a prompt the user queued
/// survives any interruption from that instant on. Persistence failures are
/// remembered once ([`DurableQueue::take_warning`]) and never block.
pub struct DurableQueue {
    items: VecDeque<String>,
    dir: PathBuf,
    warning: Option<String>,
}

impl DurableQueue {
    /// A fresh, empty backlog for `dir` (no eager write — the sidecar
    /// appears when there is something to persist).
    pub fn fresh(dir: PathBuf) -> Self {
        Self {
            items: VecDeque::new(),
            dir,
            warning: None,
        }
    }

    /// Adopt a restored backlog (resume / session switch): contents replace
    /// wholesale and are persisted immediately, so the on-disk state and the
    /// adopted state can never drift.
    pub fn adopt(&mut self, dir: PathBuf, items: Vec<String>) {
        self.dir = dir;
        self.items = items.into();
        self.persist();
    }

    fn persist(&mut self) {
        let items: Vec<String> = self.items.iter().cloned().collect();
        if let Err(e) = journal::write_queue(&self.dir, &items)
            && self.warning.is_none()
        {
            self.warning = Some(format!(
                "prompt-queue persistence failed — queued prompts will not survive an \
                 interruption ({e})"
            ));
        }
    }

    /// One-time persistence-failure warning for the driver to surface.
    pub fn take_warning(&mut self) -> Option<String> {
        self.warning.take()
    }

    /// The queue head, undisturbed — the sub-session drain gates on it
    /// (slash commands stay queued for the lead's dispatcher).
    pub fn front(&self) -> Option<&String> {
        self.items.front()
    }

    pub fn pop_front(&mut self) -> Option<String> {
        let item = self.items.pop_front();
        if item.is_some() {
            self.persist();
        }
        item
    }

    pub fn push_back(&mut self, text: String) {
        self.items.push_back(text);
        self.persist();
    }

    pub fn push_front(&mut self, text: String) {
        self.items.push_front(text);
        self.persist();
    }

    pub fn remove(&mut self, index: usize) -> Option<String> {
        let item = self.items.remove(index);
        if item.is_some() {
            self.persist();
        }
        item
    }

    pub fn clear(&mut self) {
        if !self.items.is_empty() {
            self.items.clear();
            self.persist();
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Everything loaded from disk to reopen one session.
pub struct ResumeState {
    /// The registry record as stored (adopt it: new pid, live status).
    pub record: SessionRecord,
    /// The full journal, ready to replay through the fold.
    pub records: Vec<JournalRecord>,
    /// The LLM conversation at the last committed turn boundary.
    pub history: Option<Vec<CompletionMessage>>,
    /// The pending backlog exactly as the user last saw it.
    pub queue: Vec<String>,
    /// The prompts an interruption cut short mid-dispatch — the lead's
    /// turn and/or sub-session lanes (`req:<n>`) that never settled. They go
    /// back at the FRONT of the queue in their original dispatch order
    /// (already deduplicated against `queue`).
    pub interrupted: Vec<String>,
    /// The staged-pipeline toggle to restore (`None` = default).
    pub pipeline: Option<bool>,
    /// Cumulative session spend to re-seed the budget guard (monotone spend
    /// across interruptions; a session budget keeps meaning "this session").
    pub spent_usd: Option<f64>,
}

/// Resolve what a `stella resume` invocation points at, without loading it.
pub fn resolve_resume_target(
    registry: &SessionRegistry,
    workspace: &str,
    request: &ResumeRequest,
) -> Result<SessionRecord, String> {
    match request {
        ResumeRequest::Id(id) => registry
            .get(id)
            .ok_or_else(|| format!("no session `{id}` in the registry")),
        ResumeRequest::Latest => registry.latest_resumable(workspace).ok_or_else(|| {
            format!(
                "no resumable session for this workspace — `stella resume --list` shows \
                 every session ({workspace})"
            )
        }),
    }
}

/// Load one session's durable state, validating that it can be reopened
/// HERE: it must exist, must not be owned by a live process, must belong to
/// this workspace (the deck's tools, memory, and graph are workspace-bound),
/// and must have state on disk.
pub fn load_resume(
    registry: &SessionRegistry,
    id: &str,
    workspace: &str,
) -> Result<ResumeState, String> {
    let record = registry
        .get(id)
        .ok_or_else(|| format!("no session `{id}` in the registry"))?;
    if SessionRegistry::presented_status(&record).is_live() {
        return Err(format!(
            "session `{id}` is live in another stella process (pid {}) — attach is not a \
             thing yet; stop that session first",
            record.pid
        ));
    }
    if record.workspace != workspace {
        return Err(format!(
            "session `{id}` belongs to {} — resume it from that workspace",
            record.workspace
        ));
    }
    let dir = registry.sidecar_dir(id);
    if !journal::has_state(&dir) {
        return Err(format!(
            "session `{id}` has no durable state to restore (it predates session journaling)"
        ));
    }
    let records = journal::read_journal(&dir);
    let history = journal::read_history(&dir);
    let queue = journal::read_queue(&dir);
    let mut interrupted: Vec<String> = journal::unsettled_prompts(&records)
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    // Already at the front of the restored backlog (a crash between the
    // requeue and the next dispatch) → re-adding would double them. Only a
    // leading run can have been requeued, so a prefix match is the test.
    let requeued = interrupted
        .iter()
        .zip(queue.iter())
        .take_while(|(a, b)| a == b)
        .count();
    interrupted.drain(..requeued);
    let pipeline = journal::last_pipeline(&records);
    let spent_usd = journal::last_spent_usd(&records);
    Ok(ResumeState {
        record,
        records,
        history,
        queue,
        interrupted,
        pipeline,
        spent_usd,
    })
}

/// Replay a loaded journal straight into the deck (bypassing the tee — a
/// replay must never re-journal itself, or every resume would double the
/// file). The caller replays BEFORE its first live send so ordering is the
/// original stream's.
///
/// Lanes other than `lead` (sub-session workers) have no process behind
/// them anymore: any lane the journal leaves in a live status is downgraded
/// to `Killed` after its stream — a resumed dashboard must never show a
/// worker "running" that died with the old process. (The lead is exempt:
/// the caller restamps it with a fresh `Register` + `WaitingInput`.) Their
/// unsettled prompts come back through [`ResumeState::interrupted`].
pub fn replay_session(
    records: Vec<JournalRecord>,
    started_ms: u64,
    lead: &str,
    deck_tx: &UnboundedSender<Inbound>,
) {
    let mut last_status: std::collections::HashMap<String, AgentStatus> =
        std::collections::HashMap::new();
    // First-appearance order for the downgrade pass below — same convention
    // as [`journal_lanes`]. Iterating the map directly would replay the
    // synthetic `Killed` statuses in a nondeterministic order run-to-run,
    // undermining byte-for-byte replay of the deck trace surface.
    let mut lane_order: Vec<String> = Vec::new();
    for record in records {
        if let Some(inbound) = replay_inbound(record, started_ms) {
            match &inbound {
                Inbound::Register(meta) => {
                    if !last_status.contains_key(&meta.id) {
                        lane_order.push(meta.id.clone());
                    }
                    last_status
                        .entry(meta.id.clone())
                        .or_insert(AgentStatus::Queued);
                }
                Inbound::Status { agent, status } => {
                    if !last_status.contains_key(agent) {
                        lane_order.push(agent.clone());
                    }
                    last_status.insert(agent.clone(), *status);
                }
                _ => {}
            }
            let _ = deck_tx.send(inbound);
        }
    }
    for agent in lane_order {
        let status = last_status[&agent];
        if agent != lead
            && !matches!(
                status,
                AgentStatus::Done | AgentStatus::Failed | AgentStatus::Killed
            )
        {
            let _ = deck_tx.send(Inbound::Status {
                agent,
                status: AgentStatus::Killed,
            });
        }
    }
}

/// Atomically snapshot the conversation at a turn boundary. Returns the
/// one-line warning to surface on the first failure (`None` = landed).
pub fn snapshot_history(dir: &Path, messages: &[CompletionMessage]) -> Option<String> {
    journal::write_history(dir, messages).err().map(|e| {
        format!(
            "conversation snapshot failed — resuming this session would lose the latest \
             turn ({e})"
        )
    })
}

/// Rebuild `messages` for a resumed conversation: the stored history with
/// the system prompt regenerated fresh (rules/config may have changed since
/// the session first started; the stable prefix must reflect today's).
pub fn restore_messages(
    stored: Vec<CompletionMessage>,
    system_prompt: &str,
) -> Vec<CompletionMessage> {
    let mut messages = stored;
    match messages.first() {
        Some(first) if first.role == stella_protocol::MessageRole::System => {
            messages[0] = CompletionMessage::system(system_prompt.to_string());
        }
        _ => messages.insert(0, CompletionMessage::system(system_prompt.to_string())),
    }
    messages
}

/// A stored registry record re-owned by THIS process at resume time.
pub fn adopt_record(mut record: SessionRecord, status: SessionStatus) -> SessionRecord {
    record.pid = std::process::id();
    record.status = status;
    record
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::AgentEvent;

    fn wire(r: &JournalRecord) -> String {
        serde_json::to_string(r).unwrap()
    }

    #[test]
    fn fold_relevant_inbounds_journal_and_out_of_band_ones_do_not() {
        let meta = AgentMeta::new("lead", "t", 7).with_role("lead");
        assert!(journal_record(&Inbound::Register(meta)).is_some());
        assert!(
            journal_record(&Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::Text { delta: "x".into() },
            })
            .is_some()
        );
        assert!(
            journal_record(&Inbound::Status {
                agent: "lead".into(),
                status: AgentStatus::WaitingInput,
            })
            .is_some()
        );
        assert!(
            journal_record(&Inbound::PromptStarted {
                agent: "lead".into(),
                text: "p".into(),
            })
            .is_some()
        );
        assert!(
            journal_record(&Inbound::SessionReset {
                agent: "lead".into(),
            })
            .is_some()
        );
        assert!(journal_record(&Inbound::Pipeline(true)).is_some());

        // Out-of-band view state must never journal…
        assert!(journal_record(&Inbound::Sessions(vec![])).is_none());
        assert!(journal_record(&Inbound::Notifications(vec![])).is_none());
        assert!(journal_record(&Inbound::ShowHelp).is_none());
        // …and neither does PromptRequeued: the backlog restores from
        // queue.json, so a journaled requeue would double-display it.
        assert!(
            journal_record(&Inbound::PromptRequeued {
                agent: "lead".into(),
                text: "p".into(),
            })
            .is_none()
        );
        // Deregister is visual lifecycle only (a session-switch row removal)
        // — journaling it would erase the departing session's worker rows
        // from its own future replay.
        assert!(
            journal_record(&Inbound::Deregister {
                agent: "req:1".into(),
            })
            .is_none()
        );
    }

    #[test]
    fn journal_lanes_names_every_non_lead_lane_once_in_order() {
        let records = vec![
            JournalRecord::Register {
                agent: "lead".into(),
                title: "t".into(),
                role: "lead".into(),
                model: None,
            },
            // A lane can first appear on ANY record kind — the fold
            // auto-registers it either way.
            JournalRecord::Status {
                agent: "req:2".into(),
                status: "running".into(),
            },
            JournalRecord::Register {
                agent: "req:1".into(),
                title: "worker".into(),
                role: "sub".into(),
                model: None,
            },
            JournalRecord::Event {
                agent: "req:1".into(),
                event: AgentEvent::Text { delta: "x".into() },
            },
            JournalRecord::Pipeline { on: true },
            JournalRecord::PromptStarted {
                agent: "lead".into(),
                text: "p".into(),
            },
        ];
        assert_eq!(
            journal_lanes(&records, "lead"),
            vec!["req:2".to_string(), "req:1".to_string()],
            "non-lead lanes, deduplicated, first-appearance order"
        );
        assert!(journal_lanes(&[], "lead").is_empty());
    }

    #[test]
    fn stale_lane_downgrades_replay_in_first_appearance_order_deterministically() {
        // Two non-lead lanes left in live statuses, first appearing as
        // "req:9" then "req:1" — adversarial against both alphabetical and
        // hash order. The synthetic `Killed` downgrades must come back in
        // first-appearance order on EVERY replay: a `HashMap` iteration
        // here previously made the deck trace surface differ run-to-run
        // (issue #373, item 5).
        let records = || {
            vec![
                JournalRecord::Register {
                    agent: "lead".into(),
                    title: "t".into(),
                    role: "lead".into(),
                    model: None,
                },
                JournalRecord::Status {
                    agent: "req:9".into(),
                    status: "running".into(),
                },
                JournalRecord::Status {
                    agent: "req:1".into(),
                    status: "running".into(),
                },
            ]
        };
        let downgrades = |records: Vec<JournalRecord>| -> Vec<String> {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            replay_session(records, 0, "lead", &tx);
            drop(tx);
            // The replayed stream itself carries no `Killed` (both lanes
            // journaled "running"), so filtering for `Killed` isolates
            // exactly the synthetic downgrades.
            let mut order = Vec::new();
            while let Ok(inbound) = rx.try_recv() {
                if let Inbound::Status {
                    agent,
                    status: AgentStatus::Killed,
                } = inbound
                {
                    order.push(agent);
                }
            }
            order
        };
        for _ in 0..16 {
            assert_eq!(
                downgrades(records()),
                vec!["req:9".to_string(), "req:1".to_string()],
                "downgrade order must be first-appearance, every run"
            );
        }
    }

    #[test]
    fn replay_lane_envelopes_never_journal() {
        // A `SessionOpen` replay streams another session's HISTORY through
        // the same inbound channel — journaling it would copy that past into
        // this session's journal, doubling it on every open.
        let lane = format!("{REPLAY_LANE_PREFIX}ses-42");
        assert!(
            journal_record(&Inbound::Register(
                AgentMeta::new(lane.clone(), "replay — old session", 0).with_role("replay"),
            ))
            .is_none()
        );
        assert!(
            journal_record(&Inbound::Event {
                agent: lane.clone(),
                event: AgentEvent::Text {
                    delta: "historic".into(),
                },
            })
            .is_none()
        );
        assert!(
            journal_record(&Inbound::Status {
                agent: lane,
                status: AgentStatus::Done,
            })
            .is_none()
        );
        // Sub-session lanes are the session's REAL history — they journal.
        assert!(
            journal_record(&Inbound::Event {
                agent: "req:1".into(),
                event: AgentEvent::Text {
                    delta: "live".into()
                },
            })
            .is_some()
        );
        assert!(
            journal_record(&Inbound::PromptStarted {
                agent: "sub:7".into(),
                text: "task".into(),
            })
            .is_some()
        );
    }

    #[test]
    fn status_words_roundtrip_and_unknown_words_skip() {
        for status in [
            AgentStatus::Queued,
            AgentStatus::Running,
            AgentStatus::Paused,
            AgentStatus::WaitingInput,
            AgentStatus::Done,
            AgentStatus::Failed,
            AgentStatus::Killed,
        ] {
            assert_eq!(status_from_key(status_key(status)), Some(status));
        }
        assert_eq!(status_from_key("hyperspace"), None);
        assert!(
            replay_inbound(
                JournalRecord::Status {
                    agent: "lead".into(),
                    status: "hyperspace".into(),
                },
                0,
            )
            .is_none(),
            "a status word from the future is skipped, not misread"
        );
    }

    #[test]
    fn journal_then_replay_is_identity_on_the_fold_relevant_stream() {
        let mut meta = AgentMeta::new("lead", "acme", 1).with_role("lead");
        meta.model = Some("z/glm".into());
        let stream = vec![
            Inbound::Register(meta),
            Inbound::PromptStarted {
                agent: "lead".into(),
                text: "fix it".into(),
            },
            Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::Text { delta: "on".into() },
            },
            Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::Complete {
                    model: "z/glm".into(),
                    cost_usd: 0.01,
                },
            },
            Inbound::Status {
                agent: "lead".into(),
                status: AgentStatus::WaitingInput,
            },
            Inbound::Pipeline(false),
        ];
        for inbound in stream {
            let record = journal_record(&inbound).expect("fold-relevant");
            let replayed = replay_inbound(record.clone(), 1).expect("replayable");
            let back = journal_record(&replayed).expect("still fold-relevant");
            assert_eq!(wire(&record), wire(&back), "replay must not distort");
        }
    }

    #[test]
    fn restore_messages_regenerates_the_system_prefix() {
        let stored = vec![
            CompletionMessage::system("old prompt"),
            CompletionMessage::user("hi"),
        ];
        let restored = restore_messages(stored, "new prompt");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].content, "new prompt");
        assert_eq!(restored[1].content, "hi");

        // A history that somehow lost its system head gets one prepended.
        let headless = vec![CompletionMessage::user("hi")];
        let restored = restore_messages(headless, "new prompt");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].role, stella_protocol::MessageRole::System);
    }

    /// THE durability witness: a live envelope stream folded directly vs.
    /// the same stream journaled to disk (coalescing, torn-tail handling and
    /// all), read back, and replayed through the same fold — the deck state
    /// must be indistinguishable. This is what "nothing is ever lost" means
    /// mechanically: the session on screen IS a pure function of what is on
    /// disk.
    #[test]
    fn replaying_the_journal_reproduces_the_deck_fold_exactly() {
        use stella_protocol::{ToolCall, ToolOutput};
        use stella_tui::WorkspaceModel;

        let mut meta = AgentMeta::new("lead", "acme", 0).with_role("lead");
        meta.model = Some("z/glm".into());
        let stream = vec![
            Inbound::Register(meta),
            Inbound::PromptStarted {
                agent: "lead".into(),
                text: "fix the flaky test".into(),
            },
            Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::Stage {
                    name: stella_protocol::StageKind::Execute,
                },
            },
            Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::Text {
                    delta: "loo".into(),
                },
            },
            Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::Text {
                    delta: "king…".into(),
                },
            },
            Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::ToolStart {
                    call: ToolCall {
                        call_id: "c1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({ "path": "src/lib.rs" }),
                    },
                },
            },
            Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::ToolResult {
                    call_id: "c1".into(),
                    output: ToolOutput::Ok {
                        content: "fn x() {}".into(),
                    },
                    duration_ms: 12,
                    speculated: false,
                },
            },
            Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::StepUsage {
                    output_text: None,
                    step: 1,
                    role: stella_protocol::ModelCallRole::Worker,
                    provider: "z".into(),
                    model: "z/glm".into(),
                    input_tokens: 900,
                    output_tokens: 60,
                    cached_input_tokens: 300,
                    cache_write_tokens: 0,
                    estimated_input_tokens: 880,
                    cost_usd: 0.004,
                    duration_ms: 900,
                    retries: 0,
                    tool_calls: 1,
                    complete: true,
                },
            },
            Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::Text {
                    delta: "done.".into(),
                },
            },
            Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::Complete {
                    model: "z/glm".into(),
                    cost_usd: 0.004,
                },
            },
            Inbound::Status {
                agent: "lead".into(),
                status: AgentStatus::WaitingInput,
            },
            Inbound::Pipeline(false),
        ];

        // Live fold.
        let mut live = WorkspaceModel::new();
        for inbound in &stream {
            live.apply_inbound(inbound);
        }

        // Journal → disk → read → replay → fold.
        let dir = std::env::temp_dir().join(format!("stella-replay-equiv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut j = SessionJournal::open(&dir).unwrap();
            for inbound in &stream {
                if let Some(record) = journal_record(inbound) {
                    j.write(&record).unwrap();
                }
            }
        }
        let mut replayed = WorkspaceModel::new();
        for record in journal::read_journal(&dir) {
            if let Some(inbound) = replay_inbound(record, 0) {
                replayed.apply_inbound(&inbound);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(live.agents.len(), replayed.agents.len());
        let (a, b) = (&live.agents[0], &replayed.agents[0]);
        // The entire per-agent transcript fold (`SessionModel` is PartialEq):
        // prompt echo, stages, coalesced text, tool cards, HUD state.
        assert_eq!(a.model, b.model, "transcripts must be indistinguishable");
        assert_eq!(a.status, b.status);
        assert_eq!(a.tokens_in, b.tokens_in);
        assert_eq!(a.tokens_out, b.tokens_out);
        assert_eq!(a.cost_usd, b.cost_usd);
        assert_eq!(a.meta.model, b.meta.model);
        assert_eq!(live.pipeline, replayed.pipeline);
        // NOT asserted: trace-row count. The trace is a per-event activity
        // ring, and the journal coalesces adjacent streaming deltas by
        // design — two `Text` deltas replay as one event. Everything the
        // user reads back (transcript, HUD, spend, status) is identical.
    }

    #[test]
    fn durable_queue_writes_through_every_mutation() {
        let dir = std::env::temp_dir().join(format!("stella-durable-queue-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut q = DurableQueue::fresh(dir.clone());
        q.push_back("a".into());
        q.push_back("b".into());
        q.push_front("first".into());
        assert_eq!(
            journal::read_queue(&dir),
            vec!["first".to_string(), "a".to_string(), "b".to_string()]
        );

        assert_eq!(q.pop_front().as_deref(), Some("first"));
        assert_eq!(q.remove(1).as_deref(), Some("b"));
        assert_eq!(journal::read_queue(&dir), vec!["a".to_string()]);

        q.clear();
        assert!(journal::read_queue(&dir).is_empty());
        assert!(q.take_warning().is_none(), "healthy disk, no warning");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
