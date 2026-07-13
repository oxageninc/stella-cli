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

use stella_protocol::{AgentEvent, FileChangeKind};

use crate::envelope::{AgentId, AgentMeta, AgentStatus, Inbound};
use crate::model::SessionModel;

/// The five top-level tabs of the deck.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeckTab {
    Session,
    Agents,
    Traces,
    Graph,
    Files,
}

impl DeckTab {
    pub const ALL: [DeckTab; 5] = [
        DeckTab::Session,
        DeckTab::Agents,
        DeckTab::Traces,
        DeckTab::Graph,
        DeckTab::Files,
    ];

    pub fn title(self) -> &'static str {
        match self {
            DeckTab::Session => "Session",
            DeckTab::Agents => "Agents",
            DeckTab::Traces => "Traces",
            DeckTab::Graph => "Graph",
            DeckTab::Files => "Files",
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
            cost_usd: 0.0,
            budget_ticked: false,
            last_activity_ms: started,
            res: ResourceSample::default(),
            activity: ActivitySpark::new(ACTIVITY_WINDOW),
        }
    }

    /// Elapsed wall-clock ms given the deck's current clock.
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.meta.started_ms)
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

    /// The **sole** mutator: fold one inbound envelope into the deck.
    pub fn apply_inbound(&mut self, inbound: &Inbound) {
        match inbound {
            Inbound::Register(meta) => self.register(meta.clone()),
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
            }
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
                    model,
                    cost_usd,
                    ..
                } => {
                    entry.tokens_in += input_tokens;
                    entry.tokens_out += output_tokens;
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
                }
                _ => {}
            }
        }

        // Cross-agent read-models.
        if let AgentEvent::FileChange { path, kind, diff } = event {
            self.ledger.record(agent, path, *kind, diff);
        }
        if let AgentEvent::StepUsage { model, .. } = event {
            self.routes.record(now, agent.clone(), model.clone());
        }
        let (kind, summary) = trace_of(event);
        self.trace.push(TraceRow {
            ts: now,
            agent: agent.clone(),
            kind,
            summary,
        });
    }
}

// ── File ledger: CRUD + line +/- per (agent, path) ──────────────────────────

/// One file's cumulative change record within the session.
#[derive(Clone, Debug, PartialEq)]
pub struct FileRecord {
    pub agent: AgentId,
    pub path: String,
    pub kind: FileChangeKind,
    pub added: u32,
    pub removed: u32,
    /// How many `FileChange` events have touched this (agent, path).
    pub changes: u32,
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
            rec.kind = kind;
            rec.added += added;
            rec.removed += removed;
            rec.changes += 1;
        } else {
            self.records.push(FileRecord {
                agent: agent.to_string(),
                path: path.to_string(),
                kind,
                added,
                removed,
                changes: 1,
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
}

/// Count added/removed source lines in a unified diff. File headers (`+++ `,
/// `--- `) and hunk markers (`@@`) are ignored; only real `+`/`-` body lines
/// count. The header check requires the trailing space of real header syntax:
/// an added body line whose content starts with `++` (e.g. `++i`) arrives as
/// `+++i` and must count, not be skipped as a header. Robust to
/// `None`/partial diffs — a malformed diff yields `(0, 0)`, never a panic.
pub fn count_diff_lines(diff: &str) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in diff.lines() {
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => added += 1,
            Some(b'-') => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

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
    /// Number of not-yet-dispatched prompts.
    pub fn pending(&self) -> usize {
        self.items.len()
    }
    /// Remove the oldest pending prompt for dispatch, returning its text.
    pub fn take_next(&mut self) -> Option<String> {
        self.items.pop_front().map(|p| p.text)
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
        AgentEvent::Text { .. } => 130,
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
        AgentEvent::ProviderFallback { from, to, .. } => {
            (TraceKind::Other, format!("fallback {from}→{to}"))
        }
        AgentEvent::Retry { attempt, .. } => (TraceKind::Other, format!("retry #{attempt}")),
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
    fn budget_tick_sets_live_spend_without_double_counting_step_usage() {
        let step = |cost_usd: f64| AgentEvent::StepUsage {
            step: 1,
            model: "m".into(),
            input_tokens: 1,
            output_tokens: 1,
            cached_input_tokens: 0,
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
    fn deck_tab_cycles_both_ways() {
        assert_eq!(DeckTab::Session.next(), DeckTab::Agents);
        assert_eq!(DeckTab::Session.prev(), DeckTab::Files);
        assert_eq!(DeckTab::Files.next(), DeckTab::Session);
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
