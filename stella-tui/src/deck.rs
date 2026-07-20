//! The workspace model — the command deck's derived state.
//!
//! Where [`SessionModel`](crate::model::SessionModel) folds one agent's
//! `AgentEvent` log, [`WorkspaceModel`] folds the multi-agent [`Inbound`]
//! stream: it keeps one `SessionModel` per agent (so per-agent purity is
//! untouched) and layers cross-agent read-models on top — the file ledger, the
//! route log, the prompt queue, and the unified trace.
//!
//! ## Purity boundary (L-T1)
//!
//! Everything here is a deterministic fold of the `Inbound` stream **except**
//! a small set of labeled out-of-band fields stamped from outside the event
//! log: [`AgentEntry::res`] and [`WorkspaceModel::global_cpu_pct`] (sampled
//! from the OS by the resource monitor), [`WorkspaceModel::now_ms`] (the
//! deck's clock, stamped by the shell tick), [`WorkspaceModel::queue`]
//! (mutated by the shell when the *user* submits and when the dispatcher
//! drains — a fold of outbound input, not of `Inbound`), and the code-graph
//! snapshot (queried from `stella-graph`, held by the graph view). Those are
//! the only exceptions; naming them is what keeps the boundary honest instead
//! of quietly eroded.

use std::collections::VecDeque;

use stella_protocol::{AgentEvent, CiStatus, FileChangeKind, PrStatus};

use crate::envelope::{AgentId, AgentMeta, AgentStatus, Inbound};
use crate::model::SessionModel;

/// The top-level tabs of the deck.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeckTab {
    Session,
    Agents,
    Traces,
    Graph,
    Files,
    Skills,
    Mcp,
    Issues,
    Settings,
}

impl DeckTab {
    pub const ALL: [DeckTab; 9] = [
        DeckTab::Session,
        DeckTab::Agents,
        DeckTab::Traces,
        DeckTab::Graph,
        DeckTab::Files,
        DeckTab::Skills,
        DeckTab::Mcp,
        DeckTab::Issues,
        DeckTab::Settings,
    ];

    /// The tab-bar label. Deck tab labels are UPPERCASE by convention —
    /// every tab added later must follow (e.g. `SKILLS`, `MCP`).
    /// `Agents` renders as AGENTS: the executions dashboard paired with the
    /// installed-agents view. `Settings` is the home of all config — it hosts
    /// the `agent_engine_config` editor that used to share the Agents tab.
    pub fn title(self) -> &'static str {
        match self {
            DeckTab::Session => "SESSION",
            DeckTab::Agents => "AGENTS",
            DeckTab::Traces => "TRACES",
            DeckTab::Graph => "GRAPH",
            DeckTab::Files => "FILES",
            DeckTab::Skills => "SKILLS",
            DeckTab::Mcp => "MCP",
            DeckTab::Issues => "ISSUES",
            DeckTab::Settings => "SETTINGS",
        }
    }

    pub fn index(self) -> usize {
        DeckTab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub fn from_index(i: usize) -> DeckTab {
        DeckTab::ALL[i % DeckTab::ALL.len()]
    }

    pub fn next(self) -> DeckTab {
        DeckTab::from_index(self.index() + 1)
    }

    pub fn prev(self) -> DeckTab {
        DeckTab::from_index(self.index() + DeckTab::ALL.len() - 1)
    }
}

/// A sampled resource reading for one agent — the one out-of-band field on
/// [`AgentEntry`]. Produced by [`crate::resource::ResourceMonitor`], never
/// folded from events.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResourceSample {
    /// CPU utilization percent (can exceed 100 across cores).
    pub cpu_pct: f32,
    /// Resident memory in bytes.
    pub mem_bytes: u64,
}

/// One agent's slot in the workspace: its pure per-agent fold plus the derived
/// dashboard counters.
#[derive(Clone, Debug)]
pub struct AgentEntry {
    pub meta: AgentMeta,
    /// The existing pure event fold for this agent (Session tab renders it).
    pub model: SessionModel,
    pub status: AgentStatus,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Cumulative prompt-cache *read* (hit) tokens — the `cached_input_tokens`
    /// sum from `StepUsage`. A subset of `tokens_in` by the `CompletionUsage`
    /// contract (`cached_input_tokens ⊆ input_tokens`), so the session
    /// cache-hit rate `cache_read_tokens / tokens_in` is always in `[0, 1]`.
    pub cache_read_tokens: u64,
    /// The **current** context-window occupancy: the `input_tokens` of the most
    /// recent `StepUsage` (the prompt size the last call actually sent), NOT the
    /// running sum. This is what the Ctx% gauge divides by the window — using
    /// the cumulative `tokens_in` pinned the meter at 100% after a few turns,
    /// since the total input across a session dwarfs any single window.
    pub context_tokens: u64,
    /// Live spend. Authoritative once a `BudgetTick` has been seen (its
    /// `spent_usd` already covers step costs — mirrors the HUD accounting in
    /// `SessionModel`); until then, `StepUsage.cost_usd` accumulates here as
    /// a fallback so a stream without budget ticks still shows real spend.
    pub cost_usd: f64,
    /// True once a `BudgetTick` arrived — from then on the budget stream owns
    /// `cost_usd` and `StepUsage` no longer adds to it (that would
    /// double-count).
    pub budget_ticked: bool,
    pub last_activity_ms: u64,
    /// Sampled CPU/MEM — the out-of-band field.
    pub res: ResourceSample,
    /// Recent activity intensity, one sample per event, for the sparkline.
    pub activity: ActivitySpark,
    /// Wall-clock ms at which the in-flight chat turn started — set the instant
    /// the prompt is dispatched (`Inbound::PromptStarted`) and cleared when the
    /// turn ends. `Some` means a turn is live and the header clock counts up
    /// from here; `None` means it holds `last_turn_ms` (see [`Self::turn_clock_ms`]).
    pub turn_started_ms: Option<u64>,
    /// Duration in ms of the most recently completed turn, held until the next
    /// turn begins. `None` before any turn has finished, so the header clock
    /// reads zero at rest.
    pub last_turn_ms: Option<u64>,
}

impl AgentEntry {
    fn new(meta: AgentMeta) -> Self {
        let started = meta.started_ms;
        Self {
            meta,
            model: SessionModel::new(),
            status: AgentStatus::Queued,
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            context_tokens: 0,
            cost_usd: 0.0,
            budget_ticked: false,
            last_activity_ms: started,
            res: ResourceSample::default(),
            activity: ActivitySpark::new(ACTIVITY_WINDOW),
            turn_started_ms: None,
            last_turn_ms: None,
        }
    }

    /// Elapsed wall-clock ms given the deck's current clock.
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.meta.started_ms)
    }

    /// The value the header turn-clock displays, in ms: the live elapsed time
    /// while a turn is in flight, otherwise the last completed turn's duration
    /// (zero before any turn has run). Always defined — the clock is visible at
    /// all times, even at rest.
    pub fn turn_clock_ms(&self, now_ms: u64) -> u64 {
        match self.turn_started_ms {
            Some(start) => now_ms.saturating_sub(start),
            None => self.last_turn_ms.unwrap_or(0),
        }
    }

    /// Freeze the turn clock at `now_ms` if a turn is in flight — the turn's
    /// elapsed becomes the held `last_turn_ms` and the live clock stops. A
    /// no-op when no turn is running, so double-fires (e.g. a cancel that emits
    /// its own terminal event after the engine already completed) are harmless.
    fn end_turn(&mut self, now_ms: u64) {
        if let Some(start) = self.turn_started_ms.take() {
            self.last_turn_ms = Some(now_ms.saturating_sub(start));
        }
    }

    /// Spend per hour, or `0.0` before any wall-clock has elapsed.
    pub fn usd_per_hour(&self, now_ms: u64) -> f64 {
        let secs = self.elapsed_ms(now_ms) as f64 / 1000.0;
        if secs < 1.0 {
            0.0
        } else {
            self.cost_usd / secs * 3600.0
        }
    }
}

/// The deck's PR read-model: the latest `AgentEvent::Pr` observation, from
/// whichever agent emitted it. A session tells one PR story at a time — the
/// newest event wins outright, so a CI update on the same PR simply replaces
/// the snapshot in place. Drives the statline's PR cell.
#[derive(Clone, Debug, PartialEq)]
pub struct PrInfo {
    pub url: String,
    /// The PR number (`#183`), when the monitor parsed one from the URL.
    pub number: Option<u64>,
    pub status: PrStatus,
    /// The head commit's aggregate CI verdict — `None` means "not polled
    /// yet", never "passing".
    pub ci: Option<CiStatus>,
}

/// The whole derived deck state, folded from the [`Inbound`] stream.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceModel {
    /// Agents in first-registered order; look up by `meta.id`.
    pub agents: Vec<AgentEntry>,
    pub ledger: FileLedger,
    pub routes: RouteLog,
    pub queue: PromptQueue,
    pub trace: TraceLog,
    /// The deck's clock (ms since epoch), advanced by the shell tick. Kept in
    /// the model so elapsed/$-per-hour are computed from one source.
    pub now_ms: u64,
    /// Global system CPU utilization percent — the second labeled out-of-band
    /// field (sampled by [`crate::resource::ResourceMonitor`], not folded from
    /// events). Drives the status-bar gauge and dispatch backpressure.
    pub global_cpu_pct: f32,
    /// Whether the session drives turns through the staged pipeline (triage →
    /// witness → execute → verify → judge) rather than the raw engine loop.
    /// Surfaced as the `PIPELINE` stat box. Seeded from
    /// `DeckOptions::pipeline` and toggled live by [`Inbound::Pipeline`]
    /// (the driver's `/pipeline` command).
    pub pipeline: bool,
    /// The latest PR observation across all agents (`AgentEvent::Pr` from the
    /// fleet PR/CI monitor) — the statline's PR cell. Latest event wins;
    /// `None` until a PR has been seen this session.
    pub pr: Option<PrInfo>,
}

impl WorkspaceModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index of an agent by id.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.agents.iter().position(|a| a.meta.id == id)
    }

    /// Count of agents currently active (running / awaiting input).
    pub fn active_count(&self) -> usize {
        self.agents.iter().filter(|a| a.status.is_active()).count()
    }

    /// Total live spend across all agents.
    pub fn total_cost(&self) -> f64 {
        self.agents.iter().map(|a| a.cost_usd).sum()
    }

    /// The most-recently-routed model, if any route has been observed.
    pub fn latest_model(&self) -> Option<&str> {
        self.routes.entries.back().map(|e| e.model.as_str())
    }

    /// Cumulative prompt-cache hit tokens across all agents — the numerator of
    /// the session cache-hit rate (a subset of [`Self::total_input_tokens`]).
    pub fn cache_hit_tokens(&self) -> u64 {
        self.agents.iter().map(|a| a.cache_read_tokens).sum()
    }

    /// Cumulative input tokens across all agents — the denominator of the
    /// session cache-hit rate. `cache_hit_tokens ⊆ total_input_tokens` by the
    /// `CompletionUsage` contract, so the ratio never exceeds 1.
    pub fn total_input_tokens(&self) -> u64 {
        self.agents.iter().map(|a| a.tokens_in).sum()
    }

    /// The **sole** mutator: fold one inbound envelope into the deck.
    pub fn apply_inbound(&mut self, inbound: &Inbound) {
        match inbound {
            Inbound::Register(meta) => self.register(meta.clone()),
            // The visual-lifecycle inverse of `Register`: the row disappears.
            // An unknown id is a no-op — a stale deregister (row already
            // gone, or never seen) must never disturb the fold.
            Inbound::Deregister { agent } => {
                if let Some(idx) = self.index_of(agent) {
                    self.agents.remove(idx);
                }
            }
            Inbound::Event { agent, event } => self.apply_event(agent, event),
            Inbound::Status { agent, status } => {
                // Auto-register an unknown id, exactly like `Event`:
                // supervisor states (`Paused`, `Killed`, …) are not
                // recoverable from the event stream, so a status arriving
                // before registration must never be dropped.
                let i = match self.index_of(agent) {
                    Some(i) => i,
                    None => {
                        self.agents.push(AgentEntry::new(AgentMeta::new(
                            agent.clone(),
                            agent.clone(),
                            self.now_ms,
                        )));
                        self.agents.len() - 1
                    }
                };
                // Killed is terminal-terminal; the supervisor owns it and
                // nothing walks it back.
                if self.agents[i].status != AgentStatus::Killed {
                    self.agents[i].status = *status;
                }
                // `WaitingInput` via `Status` is the host's "back to idle"
                // signal — it arrives after handled commands (`/init`, MCP
                // connect, startup) that skip the model turn, so no turn is in
                // flight. Freeze the header clock; unlike `AskUser` (which
                // reaches `WaitingInput` through the event stream mid-turn),
                // this path means the turn is genuinely over.
                if *status == AgentStatus::WaitingInput {
                    self.agents[i].end_turn(self.now_ms);
                }
            }
            Inbound::PromptStarted { agent, text } => {
                // The dispatcher drained the oldest queued prompt. Both sides
                // are FIFO over one ordered channel, so the front entry is the
                // one that started; `text` is carried for the trace row (and
                // as a guard against a front entry the shell never saw).
                if self
                    .queue
                    .items
                    .front()
                    .is_none_or(|queued| queued.text == *text)
                {
                    let _ = self.queue.take_next();
                }
                let ts = self.now_ms;
                self.trace.push(TraceRow {
                    ts,
                    agent: agent.clone(),
                    kind: TraceKind::Stage,
                    summary: format!("▶ {}", snip(text)),
                });
                // Show the user's prompt inline in the agent's transcript so
                // the conversational scrollback is self-contained, matching
                // the Crush-style layout where user messages are visible.
                if let Some(idx) = self.index_of(agent) {
                    self.agents[idx].model.push_user_prompt(text);
                    // A new chat turn begins the instant the prompt is
                    // dispatched: start the header clock here and drop the
                    // prior turn's held time so the readout switches straight
                    // to the live count.
                    self.agents[idx].turn_started_ms = Some(ts);
                    self.agents[idx].last_turn_ms = None;
                    // Flip to Running now so the progress bar reads in-progress
                    // from the instant of submission — a driver command (e.g.
                    // `/init`) emits no stage events, and the prior turn may have
                    // left the status at `Done`, which would otherwise keep the
                    // bar frozen at full-green until the engine spoke.
                    self.agents[idx].status = AgentStatus::Running;
                }
            }
            Inbound::PromptRequeued { agent, text } => {
                // The driver cancelled a turn (double-Esc) and returned its
                // prompt to the front of its backlog. Front-insert the mirror
                // — the exact inverse of `PromptStarted`'s front-pop — so the
                // queue view keeps matching what will actually run next.
                let ts = self.now_ms;
                self.queue.enqueue_front(text.clone(), ts);
                self.trace.push(TraceRow {
                    ts,
                    agent: agent.clone(),
                    kind: TraceKind::Stage,
                    summary: format!("↩ {}", snip(text)),
                });
            }
            // `/clear`: reset the agent's session to seq-0 — blank the
            // transcript, zero the cost/token counters and the header clock, and
            // return the HUD (progress bar) to idle. The prompt echo the paired
            // `PromptStarted` pushed is wiped along with the rest.
            Inbound::SessionReset { agent } => {
                if let Some(idx) = self.index_of(agent) {
                    let entry = &mut self.agents[idx];
                    entry.model = SessionModel::new();
                    entry.status = AgentStatus::WaitingInput;
                    entry.tokens_in = 0;
                    entry.tokens_out = 0;
                    entry.cache_read_tokens = 0;
                    entry.context_tokens = 0;
                    entry.cost_usd = 0.0;
                    entry.budget_ticked = false;
                    entry.turn_started_ms = None;
                    entry.last_turn_ms = None;
                }
            }
            // The driver flipped staged-pipeline routing (`/pipeline`) — the
            // PIPELINE stat box tracks it live.
            Inbound::Pipeline(on) => self.pipeline = *on,
            // The graph snapshot, the slash vocabulary, the installed-agents
            // list, and the MCP snapshots are out-of-band read-models, not part
            // of the event-log fold — the view state owns them, applied in
            // `ingest_inbound`, so the model deliberately ignores them here.
            Inbound::GraphSnapshot(_)
            | Inbound::SlashCommands(_)
            | Inbound::AgentsList { .. }
            | Inbound::Skills(_)
            | Inbound::SkillSearch { .. }
            | Inbound::SkillPreview { .. }
            | Inbound::McpServers(_)
            | Inbound::McpSearchResults(_)
            | Inbound::Sessions(_)
            | Inbound::Notifications(_)
            | Inbound::McpOauthStatus { .. }
            | Inbound::EngineConfig { .. }
            | Inbound::IssuesList { .. }
            | Inbound::IssueActDone { .. }
            | Inbound::EntityHits { .. }
            | Inbound::ShowHelp
            | Inbound::Splash(_) => {}
        }
    }

    fn register(&mut self, meta: AgentMeta) {
        match self.index_of(&meta.id) {
            Some(i) => self.agents[i].meta = meta, // re-register updates meta
            None => self.agents.push(AgentEntry::new(meta)),
        }
    }

    fn apply_event(&mut self, agent: &AgentId, event: &AgentEvent) {
        // Auto-register an agent we've never seen so a stray event is never
        // dropped — the dashboard row appears with what we know.
        let idx = match self.index_of(agent) {
            Some(i) => i,
            None => {
                self.agents.push(AgentEntry::new(AgentMeta::new(
                    agent.clone(),
                    agent.clone(),
                    self.now_ms,
                )));
                self.agents.len() - 1
            }
        };
        let now = self.now_ms;

        // Per-agent pure fold — untouched.
        self.agents[idx].model.apply(event);

        // Derived counters.
        {
            let entry = &mut self.agents[idx];
            entry.last_activity_ms = now;
            entry.activity.push(event_intensity(event));
            if let Some(status) = status_from_event(event)
                && entry.status != AgentStatus::Killed
                && entry.status != AgentStatus::Paused
            {
                entry.status = status;
            }
            match event {
                AgentEvent::StepUsage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    model,
                    cost_usd,
                    ..
                } => {
                    entry.tokens_in += input_tokens;
                    entry.tokens_out += output_tokens;
                    entry.cache_read_tokens += cached_input_tokens;
                    // Occupancy is the LATEST call's prompt size, not the sum.
                    entry.context_tokens = *input_tokens;
                    entry.meta.model = Some(model.clone());
                    // Fallback accounting: a stream that never emits
                    // `BudgetTick` (scenario feeds, minimal drivers) still
                    // shows real spend. Once a tick has been seen it owns
                    // `cost_usd` outright — adding steps on top of it would
                    // double-count.
                    if !entry.budget_ticked {
                        entry.cost_usd += cost_usd;
                    }
                }
                AgentEvent::BudgetTick { spent_usd, .. } => {
                    entry.budget_ticked = true;
                    entry.cost_usd = *spent_usd;
                }
                AgentEvent::Complete { model, cost_usd } => {
                    entry.meta.model = Some(model.clone());
                    entry.cost_usd = entry.cost_usd.max(*cost_usd);
                    // The turn-completion event: freeze the header clock at its
                    // final elapsed so it holds the last turn's duration.
                    entry.end_turn(now);
                }
                // A non-retryable error also ends the turn — an aborted turn,
                // a user Stop, or a double-Esc hold all fold to one of these
                // (see `command_deck`). Retryable errors mean the turn
                // continues (they fold to `Running`), so the clock keeps
                // ticking; only the terminal kind stops it.
                AgentEvent::Error {
                    retryable: false, ..
                } => entry.end_turn(now),
                _ => {}
            }
        }

        // Cross-agent read-models.
        if let AgentEvent::FileChange { path, kind, diff } = event {
            self.ledger.record(agent, path, *kind, diff);
        }
        if let AgentEvent::Pr {
            url,
            status,
            number,
            ci,
        } = event
        {
            // Latest wins, any agent — the statline tells one PR story, and a
            // CI re-poll on the same PR replaces the snapshot in place.
            self.pr = Some(PrInfo {
                url: url.clone(),
                number: *number,
                status: *status,
                ci: *ci,
            });
        }
        if let AgentEvent::StepUsage { model, .. } = event {
            self.routes.record(now, agent.clone(), model.clone());
        }
        // Streaming previews never reach the trace: one row per token would
        // churn the whole capped ring during a single answer, and the
        // authoritative `Text` event lands the same content as one row.
        if !matches!(event, AgentEvent::TextDelta { .. }) {
            let (kind, summary) = trace_of(event);
            self.trace.push(TraceRow {
                ts: now,
                agent: agent.clone(),
                kind,
                summary,
            });
        }
    }
}

// ── File ledger: CRUD + line +/- per (agent, path) ──────────────────────────

/// One file's cumulative change record within the session.
#[derive(Clone, Debug, PartialEq)]
pub struct FileRecord {
    pub agent: AgentId,
    pub path: String,
    /// The latest *mutation* kind — a read only sets this on a file that has
    /// never been mutated, so an edited file's badge never regresses to `R`
    /// when the agent re-reads it.
    pub kind: FileChangeKind,
    pub added: u32,
    pub removed: u32,
    /// How many *mutating* `FileChange` events have touched this
    /// (agent, path).
    pub changes: u32,
    /// How many times this (agent, path) has been read.
    pub reads: u32,
}

/// Every file touched this session, with CRUD op and line +/- parsed from the
/// unified-diff strings on `FileChange` events (there is no structured
/// +/- in the event — it is derived from the diff text, deterministically).
#[derive(Clone, Debug, Default)]
pub struct FileLedger {
    pub records: Vec<FileRecord>,
}

impl FileLedger {
    fn record(&mut self, agent: &str, path: &str, kind: FileChangeKind, diff: &Option<String>) {
        let (added, removed) = diff.as_deref().map(count_diff_lines).unwrap_or((0, 0));
        if let Some(rec) = self
            .records
            .iter_mut()
            .find(|r| r.agent == agent && r.path == path)
        {
            if kind.is_mutation() {
                rec.kind = kind;
                rec.added += added;
                rec.removed += removed;
                rec.changes += 1;
            } else {
                rec.reads += 1;
            }
        } else {
            let mutation = kind.is_mutation();
            self.records.push(FileRecord {
                agent: agent.to_string(),
                path: path.to_string(),
                kind,
                added,
                removed,
                changes: mutation as u32,
                reads: !mutation as u32,
            });
        }
    }

    pub fn total_added(&self) -> u32 {
        self.records.iter().map(|r| r.added).sum()
    }
    pub fn total_removed(&self) -> u32 {
        self.records.iter().map(|r| r.removed).sum()
    }
    pub fn file_count(&self) -> usize {
        self.records.len()
    }
    pub fn total_reads(&self) -> u32 {
        self.records.iter().map(|r| r.reads).sum()
    }
}

// The diff-counting fold moved to `crate::diff` (one module owns the whole
// "how a diff reads" story); re-exported here so existing call sites hold.
pub use crate::diff::count_diff_lines;

// ── Route log: which model handled what ─────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct RouteEntry {
    pub ts: u64,
    pub agent: AgentId,
    pub model: String,
}

/// A capped log of model-routing observations (one per committed step).
#[derive(Clone, Debug, Default)]
pub struct RouteLog {
    pub entries: VecDeque<RouteEntry>,
}

impl RouteLog {
    const CAP: usize = 256;
    fn record(&mut self, ts: u64, agent: AgentId, model: String) {
        self.entries.push_back(RouteEntry { ts, agent, model });
        while self.entries.len() > Self::CAP {
            self.entries.pop_front();
        }
    }
}

// ── Prompt queue: never blocks input ────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct QueuedPrompt {
    pub text: String,
    pub ts: u64,
}

/// The non-blocking prompt queue. Submitting a prompt always enqueues and
/// returns; the deck never gates typing on a busy agent. Dispatch REMOVES
/// the prompt (`take_next`), so the queue holds only the waiting backlog —
/// no dispatched history is retained, keeping memory proportional to what is
/// actually queued (the same discipline as the capped `RouteLog`/`TraceLog`).
#[derive(Clone, Debug, Default)]
pub struct PromptQueue {
    pub items: VecDeque<QueuedPrompt>,
}

impl PromptQueue {
    pub fn enqueue(&mut self, text: String, ts: u64) {
        self.items.push_back(QueuedPrompt { text, ts });
    }
    /// Insert at the FRONT: a double-Esc requeue (the interrupted prompt
    /// runs before the rest of the backlog) or the first submission after a
    /// hold (the user's new prompt runs before even that).
    pub fn enqueue_front(&mut self, text: String, ts: u64) {
        self.items.push_front(QueuedPrompt { text, ts });
    }
    /// Number of not-yet-dispatched prompts.
    pub fn pending(&self) -> usize {
        self.items.len()
    }
    /// Remove the oldest pending prompt for dispatch, returning its text.
    pub fn take_next(&mut self) -> Option<String> {
        self.items.pop_front().map(|p| p.text)
    }
    /// Remove one queued prompt by position (0 = oldest), returning its text.
    /// The queue is a *list* the user edits — deleting or pulling a prompt
    /// back out for editing must never require dispatching it first.
    pub fn remove(&mut self, index: usize) -> Option<String> {
        self.items.remove(index).map(|p| p.text)
    }
    /// Drop every pending prompt (the deck gates this behind a confirm).
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

// ── Trace log: unified cross-agent timeline ─────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceKind {
    Stage,
    Text,
    Reasoning,
    Tool,
    File,
    Budget,
    Context,
    Verdict,
    Media,
    Vcs,
    Error,
    Complete,
    Other,
}

impl TraceKind {
    pub fn label(self) -> &'static str {
        match self {
            TraceKind::Stage => "stage",
            TraceKind::Text => "text",
            TraceKind::Reasoning => "think",
            TraceKind::Tool => "tool",
            TraceKind::File => "file",
            TraceKind::Budget => "spend",
            TraceKind::Context => "ctx",
            TraceKind::Verdict => "verdict",
            TraceKind::Media => "media",
            TraceKind::Vcs => "vcs",
            TraceKind::Error => "error",
            TraceKind::Complete => "done",
            TraceKind::Other => "·",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TraceRow {
    pub ts: u64,
    pub agent: AgentId,
    pub kind: TraceKind,
    pub summary: String,
}

/// A capped ring buffer of trace rows across all agents.
#[derive(Clone, Debug)]
pub struct TraceLog {
    pub rows: VecDeque<TraceRow>,
    cap: usize,
}

impl Default for TraceLog {
    fn default() -> Self {
        Self {
            rows: VecDeque::new(),
            cap: 2000,
        }
    }
}

impl TraceLog {
    pub fn push(&mut self, row: TraceRow) {
        self.rows.push_back(row);
        while self.rows.len() > self.cap {
            self.rows.pop_front();
        }
    }
    /// Rows for one agent (filtered view).
    pub fn for_agent<'a>(&'a self, agent: &'a str) -> impl Iterator<Item = &'a TraceRow> + 'a {
        self.rows.iter().filter(move |r| r.agent == agent)
    }
}

// ── Activity sparkline ring ─────────────────────────────────────────────────

/// How many recent activity samples the dashboard sparkline keeps per agent.
pub const ACTIVITY_WINDOW: usize = 24;

/// A fixed-width ring of activity intensities (one per event), rendered as a
/// sparkline in the dashboard row.
#[derive(Clone, Debug)]
pub struct ActivitySpark {
    samples: VecDeque<u8>,
    cap: usize,
}

impl ActivitySpark {
    pub fn new(cap: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(cap),
            cap,
        }
    }
    pub fn push(&mut self, intensity: u8) {
        self.samples.push_back(intensity);
        while self.samples.len() > self.cap {
            self.samples.pop_front();
        }
    }
    /// Intensities oldest→newest, left-padded to the full width with zeros.
    pub fn padded(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.cap.saturating_sub(self.samples.len())];
        out.extend(self.samples.iter().copied());
        out
    }
}

// ── Event → derived attributes ──────────────────────────────────────────────

/// Activity intensity for the sparkline, by event kind. Edits and tool calls
/// read as "hot"; streaming text as "warm"; metering ticks as "cool".
fn event_intensity(ev: &AgentEvent) -> u8 {
    match ev {
        AgentEvent::FileChange { .. } => 255,
        AgentEvent::ToolStart { .. } | AgentEvent::ToolResult { .. } => 210,
        AgentEvent::Stage { .. } => 170,
        AgentEvent::Text { .. } | AgentEvent::TextDelta { .. } => 130,
        AgentEvent::Reasoning { .. } => 90,
        AgentEvent::Commit { .. } | AgentEvent::Pr { .. } => 230,
        AgentEvent::BudgetTick { .. } | AgentEvent::StepUsage { .. } => 60,
        AgentEvent::Error { .. } => 255,
        _ => 110,
    }
}

/// Lifecycle status implied by an event, or `None` if it doesn't move the
/// agent's lifecycle.
fn status_from_event(ev: &AgentEvent) -> Option<AgentStatus> {
    match ev {
        AgentEvent::Complete { .. } => Some(AgentStatus::Done),
        AgentEvent::Error { retryable, .. } => Some(if *retryable {
            AgentStatus::Running
        } else {
            AgentStatus::Failed
        }),
        // Both user-response gates block the agent until answered — a scope
        // review is just as much "needs input" as an ask-user question.
        AgentEvent::AskUser { .. } | AgentEvent::ScopeReview { .. } => {
            Some(AgentStatus::WaitingInput)
        }
        AgentEvent::Stage { .. }
        | AgentEvent::Text { .. }
        | AgentEvent::TextDelta { .. }
        | AgentEvent::Reasoning { .. }
        | AgentEvent::ToolStart { .. }
        | AgentEvent::ToolResult { .. } => Some(AgentStatus::Running),
        _ => None,
    }
}

/// A trace kind + short human summary for one event.
fn trace_of(ev: &AgentEvent) -> (TraceKind, String) {
    use stella_protocol::ToolOutput;
    match ev {
        AgentEvent::Stage { name } => (TraceKind::Stage, format!("{name:?}").to_lowercase()),
        AgentEvent::Text { delta } => (TraceKind::Text, snip(delta)),
        // Mapped for completeness; `apply_event` never traces deltas (one
        // row per token would churn the capped ring — see the guard there).
        AgentEvent::TextDelta { text } => (TraceKind::Text, snip(text)),
        AgentEvent::Reasoning { delta } => (TraceKind::Reasoning, snip(delta)),
        AgentEvent::ToolStart { call } => (TraceKind::Tool, format!("{}()", call.name)),
        AgentEvent::ToolResult {
            output,
            duration_ms,
            ..
        } => {
            let ok = matches!(output, ToolOutput::Ok { .. });
            (
                TraceKind::Tool,
                format!("{} in {duration_ms}ms", if ok { "ok" } else { "err" }),
            )
        }
        AgentEvent::FileChange { path, kind, diff } => {
            let (a, r) = diff.as_deref().map(count_diff_lines).unwrap_or((0, 0));
            (
                TraceKind::File,
                format!("{kind:?} {path} +{a}/-{r}").to_lowercase(),
            )
        }
        AgentEvent::BudgetTick { spent_usd, .. } => (TraceKind::Budget, format!("${spent_usd:.4}")),
        AgentEvent::StepUsage {
            model, cost_usd, ..
        } => (TraceKind::Budget, format!("{model} ${cost_usd:.4}")),
        AgentEvent::ContextRecall { frames, tokens, .. } => (
            TraceKind::Context,
            format!("{} frames, {tokens} tok", frames.len()),
        ),
        AgentEvent::ContextWrite {
            upserts,
            superseded,
            ..
        } => (TraceKind::Context, format!("+{upserts} ~{superseded}")),
        AgentEvent::JudgeVerdict { passed, .. } => (
            TraceKind::Verdict,
            if *passed {
                "passed".into()
            } else {
                "failed".into()
            },
        ),
        AgentEvent::GoalVerdict { met, round, .. } => (
            TraceKind::Verdict,
            format!("round {round} {}", if *met { "met" } else { "unmet" }),
        ),
        AgentEvent::MediaProgress { kind, .. } => {
            (TraceKind::Media, format!("{kind:?}").to_lowercase())
        }
        AgentEvent::MediaComplete { artifact } => (TraceKind::Media, artifact.label.clone()),
        AgentEvent::Commit { message, .. } => (TraceKind::Vcs, snip(message)),
        AgentEvent::Pr { status, .. } => (TraceKind::Vcs, format!("pr {status:?}").to_lowercase()),
        AgentEvent::TaskUpdate { tasks } => {
            let done = tasks.iter().filter(|t| !t.status.is_open()).count();
            (TraceKind::Other, format!("tasks {done}/{}", tasks.len()))
        }
        AgentEvent::ProviderFallback { from, to, .. } => {
            (TraceKind::Other, format!("fallback {from}→{to}"))
        }
        AgentEvent::Retry { attempt, .. } => (TraceKind::Other, format!("retry #{attempt}")),
        AgentEvent::Steered { text } => (
            TraceKind::Other,
            format!("steer: {}", text.chars().take(40).collect::<String>()),
        ),
        AgentEvent::Compaction {
            before_tokens,
            after_tokens,
            ..
        } => (
            TraceKind::Other,
            format!("compact {before_tokens}→{after_tokens}"),
        ),
        AgentEvent::ScopeReview { proposal } => (TraceKind::Stage, snip(&proposal.summary)),
        AgentEvent::AskUser { question, .. } => (TraceKind::Other, snip(question)),
        AgentEvent::Error { message, .. } => (TraceKind::Error, snip(message)),
        AgentEvent::Complete { model, cost_usd } => {
            (TraceKind::Complete, format!("{model} ${cost_usd:.4}"))
        }
    }
}

/// A one-line, length-capped snip of free text for a trace row.
fn snip(text: &str) -> String {
    const MAX: usize = 80;
    let flat = text.replace(['\n', '\r'], " ");
    let flat = flat.trim();
    if flat.chars().count() <= MAX {
        flat.to_string()
    } else {
        let head: String = flat.chars().take(MAX - 1).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::{StageKind, ToolCall, ToolOutput};

    fn reg(id: &str) -> Inbound {
        Inbound::Register(AgentMeta::new(id, format!("goal for {id}"), 0))
    }
    fn ev(agent: &str, event: AgentEvent) -> Inbound {
        Inbound::Event {
            agent: agent.into(),
            event,
        }
    }
    fn prompt_started(agent: &str, text: &str) -> Inbound {
        Inbound::PromptStarted {
            agent: agent.into(),
            text: text.into(),
        }
    }

    #[test]
    fn text_deltas_feed_the_preview_without_flooding_the_trace() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        let baseline = w.trace.rows.len();
        for _ in 0..100 {
            w.apply_inbound(&ev(
                "lead",
                AgentEvent::TextDelta {
                    text: "tok ".into(),
                },
            ));
        }
        assert_eq!(
            w.trace.rows.len(),
            baseline,
            "per-token previews never land trace rows"
        );
        assert_eq!(
            w.agents[0].status,
            AgentStatus::Running,
            "streaming still reads as activity"
        );
        assert_eq!(
            w.agents[0].model.streaming_text.len(),
            400,
            "the per-agent fold accumulates the preview"
        );
    }

    #[test]
    fn session_reset_blanks_transcript_zeroes_cost_and_stops_the_clock() {
        let mut w = WorkspaceModel::new();
        w.now_ms = 1_000;
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&prompt_started("lead", "hello"));
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::Text {
                delta: "hi there".into(),
            },
        ));
        if let Some(a) = w.agents.first_mut() {
            a.cost_usd = 0.42;
        }
        assert!(
            !w.agents[0].model.transcript.is_empty(),
            "precondition: content present"
        );
        assert!(
            w.agents[0].turn_started_ms.is_some(),
            "precondition: clock running"
        );

        // `/clear` sends this.
        w.apply_inbound(&Inbound::SessionReset {
            agent: "lead".into(),
        });

        let a = &w.agents[0];
        assert!(a.model.transcript.is_empty(), "transcript blanked");
        assert_eq!(a.cost_usd, 0.0, "cost stat zeroed");
        assert_eq!(w.total_cost(), 0.0, "workspace cost total zeroed");
        assert_eq!(a.turn_started_ms, None, "wall clock stopped");
        assert!(
            !a.model.hud.complete && a.model.hud.stage.is_none(),
            "hud/progress reset to idle"
        );
    }

    #[test]
    fn deregister_removes_the_row_and_only_that_row() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&reg("req:1"));
        w.apply_inbound(&reg("req:2"));
        assert_eq!(w.agents.len(), 3, "precondition: three rows");

        w.apply_inbound(&Inbound::Deregister {
            agent: "req:1".into(),
        });

        assert_eq!(w.agents.len(), 2, "the deregistered row is gone");
        assert!(w.index_of("req:1").is_none(), "req:1 removed");
        assert_eq!(w.index_of("lead"), Some(0), "lead untouched");
        assert_eq!(w.index_of("req:2"), Some(1), "later rows shift down");
    }

    #[test]
    fn deregister_of_an_unknown_id_is_a_noop() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&Inbound::Deregister {
            agent: "req:404".into(),
        });
        assert_eq!(w.agents.len(), 1, "unknown id disturbs nothing");
        assert_eq!(w.index_of("lead"), Some(0));

        // A stale repeat (the row already gone) is equally harmless.
        w.apply_inbound(&Inbound::Deregister {
            agent: "req:404".into(),
        });
        assert_eq!(w.agents.len(), 1);
    }

    #[test]
    fn prompt_started_flips_a_finished_agent_back_to_running() {
        // A completed turn leaves the lead resting; the next submission must flip
        // it to Running (and clear any stale stage) so the progress bar leaves
        // the full-green complete state and restarts in-progress.
        let mut w = WorkspaceModel::new();
        w.now_ms = 1_000;
        w.apply_inbound(&reg("lead"));
        if let Some(a) = w.agents.first_mut() {
            a.status = AgentStatus::Done;
            a.model.hud.stage = Some(StageKind::Complete);
            a.model.hud.complete = true;
        }
        w.apply_inbound(&prompt_started("lead", "next"));
        let a = &w.agents[0];
        assert_eq!(a.status, AgentStatus::Running, "new turn ⇒ running");
        assert!(a.model.hud.stage.is_none(), "stale stage cleared on submit");
        assert!(!a.model.hud.complete, "stale completion cleared on submit");
        assert!(a.turn_started_ms.is_some(), "the header clock restarts");
    }

    #[test]
    fn turn_clock_holds_zero_then_runs_freezes_and_resets() {
        let mut w = WorkspaceModel::new();
        w.now_ms = 1_000;
        w.apply_inbound(&reg("lead"));

        // Idle, pre-turn: the clock reads zero and is always defined.
        assert_eq!(w.agents[0].turn_clock_ms(w.now_ms), 0);
        assert_eq!(w.agents[0].turn_started_ms, None);

        // A prompt is dispatched — the clock starts from now_ms and runs live.
        w.now_ms = 5_000;
        w.apply_inbound(&prompt_started("lead", "do the thing"));
        assert_eq!(w.agents[0].turn_started_ms, Some(5_000));
        w.now_ms = 8_000;
        assert_eq!(
            w.agents[0].turn_clock_ms(w.now_ms),
            3_000,
            "3s elapsed, live"
        );

        // The turn completes — the clock freezes at its final elapsed and holds
        // it as later deck-clock frames advance.
        w.now_ms = 9_500;
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::Complete {
                model: "m".into(),
                cost_usd: 0.0,
            },
        ));
        assert_eq!(w.agents[0].turn_started_ms, None);
        assert_eq!(w.agents[0].last_turn_ms, Some(4_500)); // 9.5s − 5.0s
        w.now_ms = 60_000;
        assert_eq!(
            w.agents[0].turn_clock_ms(w.now_ms),
            4_500,
            "the completed turn's time is held, not still counting up"
        );

        // The next prompt resets: the prior turn's held time is dropped and the
        // clock runs anew from zero.
        w.apply_inbound(&prompt_started("lead", "again"));
        assert_eq!(w.agents[0].turn_started_ms, Some(60_000));
        assert_eq!(w.agents[0].last_turn_ms, None);
        assert_eq!(w.agents[0].turn_clock_ms(w.now_ms), 0);
    }

    #[test]
    fn a_cancelled_turn_freezes_the_clock_but_a_retryable_error_does_not() {
        let mut w = WorkspaceModel::new();
        w.now_ms = 0;
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&prompt_started("lead", "go"));

        // A retryable error is mid-turn noise (it folds to `Running`) — the turn
        // continues, so the clock keeps ticking.
        w.now_ms = 2_000;
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::Error {
                message: "transient".into(),
                retryable: true,
            },
        ));
        assert_eq!(
            w.agents[0].turn_started_ms,
            Some(0),
            "a retryable error leaves the turn running"
        );
        assert_eq!(w.agents[0].turn_clock_ms(w.now_ms), 2_000);

        // A non-retryable error (user Stop / abort / double-Esc all fold to
        // this) ends the turn exactly like `Complete`.
        w.now_ms = 3_000;
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::Error {
                message: "turn stopped by user".into(),
                retryable: false,
            },
        ));
        assert_eq!(w.agents[0].turn_started_ms, None);
        assert_eq!(w.agents[0].last_turn_ms, Some(3_000));
    }

    #[test]
    fn waiting_input_status_stops_the_clock_after_init_or_handled_command() {
        let mut w = WorkspaceModel::new();
        w.now_ms = 0;
        w.apply_inbound(&reg("lead"));

        // `/init` and other handled commands send `PromptStarted` (the deck
        // doesn't classify commands until after) then `Status { WaitingInput }`
        // — but no `Complete`/`Error` event. Before the fix the clock ran
        // forever; now `WaitingInput` via `Status` freezes it.
        w.now_ms = 1_000;
        w.apply_inbound(&prompt_started("lead", "/init"));
        assert_eq!(w.agents[0].turn_started_ms, Some(1_000));

        w.now_ms = 4_000;
        w.apply_inbound(&Inbound::Status {
            agent: "lead".into(),
            status: AgentStatus::WaitingInput,
        });
        assert_eq!(
            w.agents[0].turn_started_ms, None,
            "WaitingInput status must stop the turn clock"
        );
        assert_eq!(
            w.agents[0].last_turn_ms,
            Some(3_000),
            "the clock freezes at its elapsed, not reset to zero"
        );
        // And stays frozen as frames advance — no runaway.
        w.now_ms = 100_000;
        assert_eq!(w.agents[0].turn_clock_ms(w.now_ms), 3_000);
    }

    #[test]
    fn diff_line_counts_ignore_headers_and_hunks() {
        let diff = "--- a/x.rs\n+++ b/x.rs\n@@ -1,3 +1,4 @@\n context\n-old line\n+new line\n+another add\n";
        assert_eq!(count_diff_lines(diff), (2, 1));
    }

    #[test]
    fn diff_line_counts_are_robust_to_empty() {
        assert_eq!(count_diff_lines(""), (0, 0));
        assert_eq!(count_diff_lines("no markers here"), (0, 0));
    }

    #[test]
    fn diff_body_lines_starting_with_extra_plus_or_minus_still_count() {
        // An added line whose content is `++i` arrives as `+++i`; a removed
        // line whose content is `--config` arrives as `---config`. Only real
        // file headers (`+++ b/…`, `--- a/…` — with the space) are skipped.
        let diff = "--- a/x.c\n+++ b/x.c\n@@ -1,2 +1,2 @@\n---config\n+++i\n";
        assert_eq!(count_diff_lines(diff), (1, 1));
    }

    #[test]
    fn register_then_events_route_to_the_right_agent() {
        let mut w = WorkspaceModel::new();
        w.now_ms = 10;
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&reg("sub"));
        assert_eq!(w.agents.len(), 2);
        w.apply_inbound(&ev("sub", AgentEvent::Text { delta: "hi".into() }));
        // The event landed on "sub"'s pure fold, not "lead"'s.
        let sub = &w.agents[w.index_of("sub").unwrap()];
        assert_eq!(sub.model.transcript.len(), 1);
        let lead = &w.agents[w.index_of("lead").unwrap()];
        assert_eq!(lead.model.transcript.len(), 0);
        assert_eq!(sub.status, AgentStatus::Running);
    }

    #[test]
    fn stray_event_auto_registers_its_agent() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&ev(
            "ghost",
            AgentEvent::Stage {
                name: StageKind::Plan,
            },
        ));
        assert!(w.index_of("ghost").is_some());
    }

    #[test]
    fn step_usage_accumulates_tokens_and_file_change_fills_ledger() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::StepUsage {
                step: 1,
                model: "glm-5.2".into(),
                input_tokens: 1200,
                output_tokens: 300,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                estimated_input_tokens: 1200,
                cost_usd: 0.01,
                duration_ms: 100,
                retries: 0,
                tool_calls: 1,
            },
        ));
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::FileChange {
                path: "src/a.rs".into(),
                kind: FileChangeKind::Modified,
                diff: Some("+one\n+two\n-gone\n".into()),
            },
        ));
        let lead = &w.agents[0];
        assert_eq!(lead.tokens_in, 1200);
        assert_eq!(lead.tokens_out, 300);
        assert_eq!(lead.meta.model.as_deref(), Some("glm-5.2"));
        assert_eq!(w.ledger.total_added(), 2);
        assert_eq!(w.ledger.total_removed(), 1);
        assert_eq!(w.ledger.file_count(), 1);
        assert_eq!(w.latest_model(), Some("glm-5.2"));
    }

    #[test]
    fn ledger_counts_reads_without_regressing_the_mutation_badge() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        let read = |path: &str| {
            ev(
                "lead",
                AgentEvent::FileChange {
                    path: path.into(),
                    kind: FileChangeKind::Read,
                    diff: None,
                },
            )
        };
        // A read-only file shows up with an R badge and a read count.
        w.apply_inbound(&read("src/a.rs"));
        w.apply_inbound(&read("src/a.rs"));
        let rec = &w.ledger.records[0];
        assert_eq!(rec.kind, FileChangeKind::Read);
        assert_eq!((rec.changes, rec.reads), (0, 2));

        // A mutation owns the badge and ± totals; a later re-read only
        // counts.
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::FileChange {
                path: "src/a.rs".into(),
                kind: FileChangeKind::Modified,
                diff: Some("+one\n".into()),
            },
        ));
        w.apply_inbound(&read("src/a.rs"));
        let rec = &w.ledger.records[0];
        assert_eq!(rec.kind, FileChangeKind::Modified);
        assert_eq!((rec.changes, rec.reads), (1, 3));
        assert_eq!(w.ledger.total_reads(), 3);
        assert_eq!(w.ledger.total_added(), 1);
        assert_eq!(w.ledger.file_count(), 1);
    }

    #[test]
    fn context_tokens_track_the_latest_window_not_the_cumulative_input() {
        // THE Ctx% P1: the gauge divided the CUMULATIVE input by the window, so
        // after a few turns it pinned at 100%. context_tokens must hold only the
        // most recent call's prompt size (current occupancy), while tokens_in
        // keeps the running total for the I/O column.
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        let step = |input: u64| AgentEvent::StepUsage {
            step: 1,
            model: "glm-5.2".into(),
            input_tokens: input,
            output_tokens: 10,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            estimated_input_tokens: input,
            cost_usd: 0.0,
            duration_ms: 1,
            retries: 0,
            tool_calls: 0,
        };
        // Three calls of 150k each: cumulative 450k dwarfs the 200k window, but
        // the window was only 150k full on the LAST call.
        w.apply_inbound(&ev("lead", step(150_000)));
        w.apply_inbound(&ev("lead", step(150_000)));
        w.apply_inbound(&ev("lead", step(150_000)));

        let lead = &w.agents[0];
        assert_eq!(lead.context_tokens, 150_000, "occupancy = latest call only");
        assert_eq!(lead.tokens_in, 450_000, "cumulative input is still summed");
        // Occupancy reads a real 75%, not a pinned 100% from 450k / 200k.
        assert!((lead.context_tokens as f64 / 200_000.0 - 0.75).abs() < 1e-9);
    }

    #[test]
    fn budget_tick_sets_live_spend_without_double_counting_step_usage() {
        let step = |cost_usd: f64| AgentEvent::StepUsage {
            step: 1,
            model: "m".into(),
            input_tokens: 1,
            output_tokens: 1,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            estimated_input_tokens: 1,
            cost_usd,
            duration_ms: 1,
            retries: 0,
            tool_calls: 0,
        };
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&ev("lead", step(0.10)));
        // Before any tick, step costs are the fallback spend — a stream
        // without BudgetTicks still shows real dollars.
        assert_eq!(w.agents[0].cost_usd, 0.10);
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::BudgetTick {
                spent_usd: 0.42,
                limit_usd: Some(2.0),
                mode: stella_protocol::BudgetMode::Observed,
            },
        ));
        assert_eq!(w.agents[0].cost_usd, 0.42, "the tick is authoritative");
        // Once ticked, later step costs no longer add on top (that would
        // double-count what the next tick already includes).
        w.apply_inbound(&ev("lead", step(5.0)));
        assert_eq!(w.agents[0].cost_usd, 0.42);
    }

    #[test]
    fn supervisor_status_for_an_unknown_agent_auto_registers_it() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&Inbound::Status {
            agent: "ghost".into(),
            status: AgentStatus::Paused,
        });
        let i = w
            .index_of("ghost")
            .expect("status auto-registers, like Event");
        assert_eq!(w.agents[i].status, AgentStatus::Paused);
    }

    #[test]
    fn scope_review_marks_the_agent_waiting_for_input() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::ScopeReview {
                proposal: stella_protocol::ScopeProposal {
                    summary: "widen the refactor".into(),
                    steps: vec![],
                    estimated_files: 2,
                    estimated_cost_usd: None,
                },
            },
        ));
        assert_eq!(w.agents[0].status, AgentStatus::WaitingInput);
    }

    #[test]
    fn supervisor_status_and_terminal_kill_are_respected() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&Inbound::Status {
            agent: "lead".into(),
            status: AgentStatus::Killed,
        });
        // Even a fresh event cannot resurrect a killed agent's lifecycle.
        w.apply_inbound(&ev("lead", AgentEvent::Text { delta: "x".into() }));
        assert_eq!(w.agents[0].status, AgentStatus::Killed);
    }

    #[test]
    fn complete_marks_done_and_records_final_cost() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::Complete {
                model: "glm".into(),
                cost_usd: 0.033,
            },
        ));
        assert_eq!(w.agents[0].status, AgentStatus::Done);
        assert!(w.agents[0].cost_usd >= 0.033);
    }

    #[test]
    fn trace_captures_every_agent_and_filters_by_agent() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("a"));
        w.apply_inbound(&reg("b"));
        w.apply_inbound(&ev(
            "a",
            AgentEvent::Stage {
                name: StageKind::Execute,
            },
        ));
        w.apply_inbound(&ev(
            "b",
            AgentEvent::ToolStart {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                },
            },
        ));
        assert_eq!(w.trace.rows.len(), 2);
        assert_eq!(w.trace.for_agent("a").count(), 1);
        assert_eq!(w.trace.for_agent("b").count(), 1);
    }

    #[test]
    fn ask_user_marks_waiting_then_a_later_event_resumes_running() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::AskUser {
                id: "q".into(),
                question: "which db?".into(),
                options: vec![],
            },
        ));
        assert_eq!(w.agents[0].status, AgentStatus::WaitingInput);
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::ToolResult {
                call_id: "q".into(),
                output: ToolOutput::Ok {
                    content: "sqlite".into(),
                },
                duration_ms: 1,
                speculated: false,
            },
        ));
        assert_eq!(w.agents[0].status, AgentStatus::Running);
    }

    #[test]
    fn prompt_queue_never_blocks_and_dispatches_fifo() {
        let mut q = PromptQueue::default();
        q.enqueue("first".into(), 1);
        q.enqueue("second".into(), 2);
        assert_eq!(q.pending(), 2);
        assert_eq!(q.take_next().as_deref(), Some("first"));
        assert_eq!(q.pending(), 1);
        assert_eq!(q.take_next().as_deref(), Some("second"));
        assert_eq!(q.pending(), 0);
        assert_eq!(q.take_next(), None);
    }

    #[test]
    fn prompt_started_pops_the_front_of_the_queue_and_leaves_a_trace() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        // The shell enqueues on submit (its labeled out-of-band mutation)…
        w.queue.enqueue("first".into(), 1);
        w.queue.enqueue("second".into(), 2);
        // …and the dispatcher's PromptStarted drains it front-first.
        w.apply_inbound(&Inbound::PromptStarted {
            agent: "lead".into(),
            text: "first".into(),
        });
        assert_eq!(w.queue.pending(), 1);
        assert_eq!(
            w.queue.items.front().map(|q| q.text.as_str()),
            Some("second")
        );
        let row = w.trace.rows.back().expect("a trace row was recorded");
        assert_eq!(row.agent, "lead");
        assert!(row.summary.contains("first"), "{}", row.summary);
    }

    #[test]
    fn prompt_started_with_an_unseen_text_never_drops_someone_elses_entry() {
        let mut w = WorkspaceModel::new();
        w.queue.enqueue("queued by the shell".into(), 1);
        // A dispatch the shell never enqueued (e.g. a driver-side prompt)
        // must not eat the front entry the user is still watching.
        w.apply_inbound(&Inbound::PromptStarted {
            agent: "lead".into(),
            text: "driver-side prompt".into(),
        });
        assert_eq!(w.queue.pending(), 1);
    }

    #[test]
    fn prompt_requeued_returns_the_prompt_to_the_front_of_the_queue() {
        let mut w = WorkspaceModel::new();
        w.apply_inbound(&reg("lead"));
        w.queue.enqueue("second".into(), 1);
        w.queue.enqueue("third".into(), 2);
        // A double-Esc cancelled "first" mid-turn; the driver returned it to
        // the front of its backlog and mirrored that here.
        w.apply_inbound(&Inbound::PromptRequeued {
            agent: "lead".into(),
            text: "first".into(),
        });
        assert_eq!(w.queue.pending(), 3);
        assert_eq!(
            w.queue.items.front().map(|q| q.text.as_str()),
            Some("first")
        );
        let row = w.trace.rows.back().expect("a trace row was recorded");
        assert_eq!(row.agent, "lead");
        assert!(row.summary.contains("first"), "{}", row.summary);
    }

    #[test]
    fn front_inserts_stack_so_the_newest_front_insert_runs_first() {
        // Double-Esc requeues the interrupted prompt at the front; the user's
        // next submission front-inserts ABOVE it — new prompt, then the
        // returned prompt, then the rest of the backlog.
        let mut q = PromptQueue::default();
        q.enqueue("rest".into(), 1);
        q.enqueue_front("returned".into(), 2);
        q.enqueue_front("new".into(), 3);
        assert_eq!(q.take_next().as_deref(), Some("new"));
        assert_eq!(q.take_next().as_deref(), Some("returned"));
        assert_eq!(q.take_next().as_deref(), Some("rest"));
    }

    #[test]
    fn prompt_queue_edits_like_a_list() {
        let mut q = PromptQueue::default();
        q.enqueue("a".into(), 1);
        q.enqueue("b".into(), 2);
        q.enqueue("c".into(), 3);
        // Remove by position, not just from the front.
        assert_eq!(q.remove(1).as_deref(), Some("b"));
        assert_eq!(q.pending(), 2);
        assert_eq!(q.remove(9), None, "out of range is a no-op");
        q.clear();
        assert_eq!(q.pending(), 0);
    }

    #[test]
    fn pr_events_fold_into_the_read_model_latest_wins_and_ci_updates_in_place() {
        let mut w = WorkspaceModel::new();
        assert_eq!(w.pr, None, "no PR story before any Pr event");
        w.apply_inbound(&reg("lead"));
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::Pr {
                url: "https://github.com/x/y/pull/183".into(),
                status: PrStatus::Open,
                number: Some(183),
                ci: None,
            },
        ));
        let pr = w.pr.as_ref().expect("the Pr event folded");
        assert_eq!(pr.number, Some(183));
        assert_eq!(pr.status, PrStatus::Open);
        assert_eq!(pr.ci, None, "not polled yet");

        // A CI re-poll on the same PR replaces the snapshot in place.
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::Pr {
                url: "https://github.com/x/y/pull/183".into(),
                status: PrStatus::Open,
                number: Some(183),
                ci: Some(CiStatus::Failing),
            },
        ));
        let pr = w.pr.as_ref().expect("still present");
        assert_eq!(pr.ci, Some(CiStatus::Failing));
        assert_eq!(pr.number, Some(183), "same PR, updated verdict");

        // A later Pr event from ANY agent wins outright — latest wins.
        w.apply_inbound(&ev(
            "sub",
            AgentEvent::Pr {
                url: "https://github.com/x/y/pull/184".into(),
                status: PrStatus::Merged,
                number: Some(184),
                ci: Some(CiStatus::Passing),
            },
        ));
        let pr = w.pr.as_ref().expect("still present");
        assert_eq!(pr.number, Some(184));
        assert_eq!(pr.status, PrStatus::Merged);
        assert_eq!(pr.ci, Some(CiStatus::Passing));
    }

    #[test]
    fn pipeline_toggle_folds_into_the_model() {
        // `/pipeline` flips routing driver-side; the stat box must track it
        // through the fold, both directions.
        let mut w = WorkspaceModel::new();
        assert!(!w.pipeline, "the deck starts on the raw engine loop");
        w.apply_inbound(&Inbound::Pipeline(true));
        assert!(w.pipeline);
        w.apply_inbound(&Inbound::Pipeline(false));
        assert!(!w.pipeline);
    }

    #[test]
    fn deck_tab_cycles_both_ways() {
        assert_eq!(DeckTab::Session.next(), DeckTab::Agents);
        // Tab order ends …Skills → Mcp → Issues → Settings; Settings wraps to
        // Session and is Session's predecessor backward.
        assert_eq!(DeckTab::Files.next(), DeckTab::Skills);
        assert_eq!(DeckTab::Skills.next(), DeckTab::Mcp);
        assert_eq!(DeckTab::Mcp.next(), DeckTab::Issues);
        assert_eq!(DeckTab::Issues.next(), DeckTab::Settings);
        assert_eq!(DeckTab::Settings.next(), DeckTab::Session);
        assert_eq!(DeckTab::Session.prev(), DeckTab::Settings);
    }

    #[test]
    fn deck_tab_all_round_trips_through_index() {
        // Every tab in ALL maps to a unique index and back; a full next()
        // walk visits each exactly once before wrapping.
        for (i, tab) in DeckTab::ALL.iter().enumerate() {
            assert_eq!(tab.index(), i);
            assert_eq!(DeckTab::from_index(i), *tab);
        }
        let mut tab = DeckTab::Session;
        for expected in DeckTab::ALL.iter().skip(1) {
            tab = tab.next();
            assert_eq!(tab, *expected);
        }
        assert_eq!(tab.next(), DeckTab::Session, "the cycle closes");
    }

    #[test]
    fn activity_spark_pads_and_caps() {
        let mut s = ActivitySpark::new(4);
        s.push(10);
        s.push(20);
        assert_eq!(s.padded(), vec![0, 0, 10, 20]);
        for v in [1, 2, 3, 4] {
            s.push(v);
        }
        assert_eq!(s.padded(), vec![1, 2, 3, 4]); // capped at width 4
    }
}
