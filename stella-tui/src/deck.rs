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
    /// Cumulative prompt-cache *write* tokens — the `cache_write_tokens` sum
    /// from `StepUsage`. NOT a subset of `tokens_in` (writes bill separately),
    /// so this is the raw write volume the cache panel shows next to the reads.
    pub cache_write_tokens: u64,
    /// Cumulative estimated USD saved by prompt caching, summed from the signed
    /// per-call [`crate::envelope::Inbound::CacheInsight`] deltas the
    /// pricing-aware producer computes. Signed: negative when the write premium
    /// outran the reads (the low-hit incident worth surfacing).
    pub cache_savings_usd: f64,
    /// The agent provider's prompt-cache TTL in seconds, from the latest
    /// `CacheInsight` (`0` = no prompt cache / no TTL). Paired with
    /// [`Self::last_provider_call_ms`] for the deck's warmth countdown.
    pub cache_ttl_secs: u64,
    /// Whether the agent's current provider only caches behind an explicit
    /// opt-in marker, from the latest `CacheInsight` — see
    /// [`crate::envelope::Inbound::CacheInsight`]. Feeds
    /// [`Self::cache_diagnosis`]'s `OptInNeverEngaged` case.
    pub cache_is_opt_in_provider: bool,
    /// Metered model calls this agent has made (`StepUsage` count) — the
    /// `turns` a low-hit-rate diagnosis needs enough of before a 0% hit rate
    /// is meaningful (turn 1 always writes, never reads).
    pub cache_call_count: u64,
    /// Wall-clock ms of the agent's most recent metered model call (a
    /// `StepUsage`) — the anchor the cache-warmth countdown measures idle from.
    /// `None` before any call has landed.
    pub last_provider_call_ms: Option<u64>,
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
            cache_write_tokens: 0,
            cache_savings_usd: 0.0,
            cache_ttl_secs: 0,
            cache_is_opt_in_provider: false,
            cache_call_count: 0,
            last_provider_call_ms: None,
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
                    entry.cache_write_tokens = 0;
                    entry.cache_savings_usd = 0.0;
                    entry.cache_ttl_secs = 0;
                    entry.cache_is_opt_in_provider = false;
                    entry.cache_call_count = 0;
                    entry.last_provider_call_ms = None;
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
            // Derived cache economics for the agent's latest call: accumulate
            // the signed savings and adopt the provider's TTL. Follows the
            // paired `StepUsage` (which auto-registers the lane), so an unknown
            // id here is a stale/out-of-order envelope — a safe no-op.
            Inbound::CacheInsight {
                agent,
                savings_usd_delta,
                ttl_secs,
                is_opt_in_provider,
            } => {
                if let Some(idx) = self.index_of(agent) {
                    let entry = &mut self.agents[idx];
                    entry.cache_savings_usd += *savings_usd_delta;
                    entry.cache_ttl_secs = *ttl_secs;
                    entry.cache_is_opt_in_provider = *is_opt_in_provider;
                }
            }
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
                    cache_write_tokens,
                    model,
                    cost_usd,
                    ..
                } => {
                    entry.tokens_in += input_tokens;
                    entry.tokens_out += output_tokens;
                    entry.cache_read_tokens += cached_input_tokens;
                    entry.cache_write_tokens += cache_write_tokens;
                    entry.cache_call_count += 1;
                    // A metered call just landed — anchor the cache-warmth
                    // countdown here (the prefix is warmest right now).
                    entry.last_provider_call_ms = Some(now);
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
        AgentEvent::UsageIncomplete { reason, .. } => {
            (TraceKind::Other, format!("usage incomplete: {reason:?}"))
        }
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
mod tests;
