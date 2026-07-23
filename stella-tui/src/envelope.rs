//! Multi-agent wire types — the envelope that turns the single-session
//! `AgentEvent` stream into a workspace of many agents.
//!
//! The existing [`run`](crate::shell::run)`(events, submissions)` shell speaks
//! one `AgentEvent` stream for one session. The command deck speaks
//! [`Inbound`] (an agent-id-tagged event) in and [`WorkspaceInput`] out, so N
//! agents share one deck. A single-agent session is just one [`AgentId`].
//!
//! This keeps the L-T1 purity per agent: each agent's derived state is still a
//! pure fold of *its* `AgentEvent`s; the envelope only adds the routing tag the
//! deck needs to keep N folds side by side.

use stella_protocol::AgentEvent;

use crate::graph::GraphSnapshot;
use crate::input::UserInput;

/// Stable identifier for one agent/run within the workspace. Human-meaningful
/// where possible (`"lead"`, `"sub:auth-refactor"`) — it is shown on screen, so
/// it is never a raw UUID as the primary label (the L-C4 cite-by-label spirit).
pub type AgentId = String;

/// Everything the dashboard needs to introduce an agent before its first event.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentMeta {
    pub id: AgentId,
    /// The project / goal shown in the dashboard row.
    pub title: String,
    /// `"lead"` | `"subagent"` | free-form role label.
    pub role: String,
    /// OS process id for CPU/MEM attribution, once known.
    pub pid: Option<u32>,
    /// The model handling this agent, once routed.
    pub model: Option<String>,
    /// Wall-clock start (ms since epoch) for elapsed / $-per-hour.
    pub started_ms: u64,
}

impl AgentMeta {
    /// A minimal meta with the free-form defaults filled in.
    pub fn new(id: impl Into<AgentId>, title: impl Into<String>, started_ms: u64) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            role: "agent".to_string(),
            pid: None,
            model: None,
            started_ms,
        }
    }

    /// Builder: set the role.
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = role.into();
        self
    }

    /// Builder: set the OS pid (for resource attribution).
    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }
}

/// The lifecycle status of an agent. Most transitions are derivable from the
/// `AgentEvent` stream (a `Stage` means running, `Complete` means done, an
/// `Error` means failed, `AskUser` means waiting) — but `Queued`, `Paused`,
/// and `Killed` are supervisor states that are *not* in the event stream, so
/// they arrive via [`Inbound::Status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Queued,
    Running,
    Paused,
    WaitingInput,
    Done,
    Failed,
    Killed,
}

impl AgentStatus {
    /// True while the agent is actively holding resources / dispatchable.
    pub fn is_active(self) -> bool {
        matches!(self, AgentStatus::Running | AgentStatus::WaitingInput)
    }

    /// True once the agent has reached a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            AgentStatus::Done | AgentStatus::Failed | AgentStatus::Killed
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Queued => "queued",
            AgentStatus::Running => "running",
            AgentStatus::Paused => "paused",
            AgentStatus::WaitingInput => "needs input",
            AgentStatus::Done => "done",
            AgentStatus::Failed => "failed",
            AgentStatus::Killed => "killed",
        }
    }
}

/// One item on the workspace inbound channel — the multi-agent envelope the
/// deck folds. `--output-format stream-json` remains one `AgentEvent` per line
/// per agent; the envelope only adds the routing tag the deck needs.
#[derive(Clone, Debug)]
pub enum Inbound {
    /// A new agent joined the workspace — its dashboard row appears.
    Register(AgentMeta),
    /// Remove one agent's dashboard row — the visual-lifecycle inverse of
    /// [`Inbound::Register`]. Presentation only: journaling and model state
    /// are unaffected (the removed lane's history stays in its own session's
    /// journal, and the engine's conversation is untouched). The driver only
    /// ever sends it for lanes with no live process behind them — e.g. the
    /// terminal worker rows of a session the deck is navigating away from,
    /// which must not linger on the next session's dashboard. Folding an
    /// unknown id is a no-op.
    Deregister { agent: AgentId },
    /// An `AgentEvent` belonging to one agent.
    Event { agent: AgentId, event: AgentEvent },
    /// A supervisor lifecycle transition not carried by the event stream.
    Status { agent: AgentId, status: AgentStatus },
    /// The dispatcher took the oldest queued prompt and handed it to an
    /// agent. The deck's [`PromptQueue`](crate::deck::PromptQueue) is FIFO on
    /// both sides of the channel, so this pops the front entry — the status
    /// bar's "queued" count goes down the moment work actually starts, and a
    /// trace row records which agent picked the prompt up.
    PromptStarted { agent: AgentId, text: String },
    /// The driver cancelled a turn on [`WorkspaceInput::StopAndHold`]
    /// (double-Esc) and returned that turn's prompt to the FRONT of its
    /// dispatch backlog. Folded as a front-insert into the deck's
    /// [`PromptQueue`](crate::deck::PromptQueue) — the exact inverse of
    /// [`Inbound::PromptStarted`]'s front-pop — so the queue view keeps
    /// matching what will actually run.
    PromptRequeued { agent: AgentId, text: String },
    /// Reset one agent's session to its seq-0 state — a `/clear`. Folded like a
    /// core event (it mutates the model, not just view state): the agent's
    /// transcript is blanked, its cost/token counters and the header clock zero
    /// out, and the progress-bar HUD returns to idle. The driver sends this on
    /// `/clear` (alongside clearing its own LLM message history).
    SessionReset { agent: AgentId },
    /// A refreshed code-graph snapshot for the Graph tab. Unlike the other
    /// variants this is **not** a folded event — the graph is an out-of-band
    /// read-model — out-of-band, not folded from events. It
    /// rides the inbound channel only because that is the driver→deck path;
    /// [`crate::deck_ui::ingest_inbound`] applies it straight to the view
    /// state (`DeckUi::graph`) and the model fold ignores it. The driver
    /// sends one after `/init` rebuilds the index so the tab reflects it
    /// without a restart.
    GraphSnapshot(GraphSnapshot),
    /// A refreshed slash-command vocabulary for the `/` popup. Out-of-band
    /// view state exactly like [`Inbound::GraphSnapshot`]: applied straight
    /// to `DeckUi::slash_commands` by [`crate::deck_ui::ingest_inbound`],
    /// ignored by the model fold. The driver sends one after `/init` adopts
    /// custom commands/skills so the menu reflects them without a restart.
    SlashCommands(Vec<crate::composer::SlashCommand>),
    /// The driver toggled staged-pipeline routing (`/pipeline`): subsequent
    /// turns run triage → witness → execute → verify → judge instead of the
    /// raw engine loop. Folded into [`crate::deck::WorkspaceModel::pipeline`]
    /// so the `PIPELINE` stat box flips live.
    Pipeline(bool),
    /// Derived prompt-cache economics for one agent's latest model call —
    /// dollars saved and the provider's cache TTL — computed by the
    /// pricing-aware producer (the CLI has the model catalog; the deck does
    /// not) and folded into the agent's [`crate::deck::AgentEntry`]. Paired
    /// with the raw `StepUsage` the same call emits (which carries the token
    /// counts the deck already folds): this adds only the two figures that
    /// need list pricing / the TTL table, keeping the single savings formula
    /// in `stella-model` and the deck free of a model-tier dependency.
    ///
    /// `savings_usd_delta` is this call's signed savings (negative when the
    /// write premium outran the reads it bought — the low-hit incident), added
    /// to the agent's running total. `ttl_secs` is the provider's prompt-cache
    /// TTL in seconds (`0` = no prompt cache / no TTL to preserve); the deck
    /// pairs it with the last provider-call time to render a live warmth
    /// countdown. `is_opt_in_provider` is whether this provider only caches
    /// behind an explicit marker (Anthropic/Bedrock/OpenRouter-Claude) —
    /// resolved once here from `stella-model`'s cache-posture table so
    /// [`crate::deck::AgentEntry::cache_diagnosis`] can name
    /// `CacheCause::OptInNeverEngaged` without the deck itself needing to
    /// know which providers require the marker.
    CacheInsight {
        agent: AgentId,
        savings_usd_delta: f64,
        ttl_secs: u64,
        is_opt_in_provider: bool,
    },
    /// The installed-agents list for the Agents tab's INSTALLED AGENTS pane.
    /// Out-of-band view state (applied straight to `DeckUi::installed` by
    /// [`crate::deck_ui::ingest_inbound`], ignored by the model fold). The
    /// driver — which owns the definitions on disk — sends one when the pane
    /// asks ([`WorkspaceInput::AgentsRefresh`]) and after every save / pin /
    /// create so the list stays live. `status`, when set, replaces the
    /// pane's hint line (op outcomes, errors).
    AgentsList {
        entries: Vec<InstalledAgentEntry>,
        status: Option<String>,
    },
    /// A refreshed snapshot of the installed skills for the SKILLS tab. The
    /// driver owns the skills on disk (both scopes), their enabled/version/pin
    /// state, and the npx registry; the deck renders this read-model. Applied
    /// straight to `DeckUi::skills` by [`crate::deck_ui::ingest_inbound`],
    /// ignored by the model fold — same out-of-band contract as
    /// [`Inbound::GraphSnapshot`].
    Skills(SkillsView),
    /// The result of a registry search (`npx skills find <query>`). Folded
    /// into the SKILLS tab's search pane; out-of-band like [`Inbound::Skills`].
    SkillSearch {
        query: String,
        hits: Vec<SkillSearchHit>,
        status: Option<String>,
    },
    /// The rendered `SKILL.md` body for the ctrl+o preview overlay, fetched by
    /// the driver (`npx skills use <id>`) for a not-yet-installed search hit.
    /// Out-of-band like [`Inbound::SkillSearch`]; `id` lets the tab drop a
    /// stale reply if the user closed or re-targeted the preview meanwhile.
    SkillPreview {
        id: String,
        body: String,
        status: Option<String>,
    },
    /// A refreshed snapshot of the configured MCP servers for the MCP tab.
    /// Out-of-band view state exactly like [`Inbound::GraphSnapshot`]: applied
    /// straight to `DeckUi::mcp` by [`crate::deck_ui::ingest_inbound`], ignored
    /// by the model fold. The driver sends one at startup and after every MCP
    /// action (install, toggle, auth, remove) so the tab reflects live state.
    McpServers(Vec<McpServerInfo>),
    /// The result of an MCP registry search the tab requested
    /// ([`WorkspaceInput::McpSearch`]) — also out-of-band, applied to
    /// `DeckUi::mcp` search results.
    McpSearchResults(McpSearchOutcome),
    /// Open the help overlay. Sent by the driver when the user types `/help`
    /// (so the slash command reaches the same rich, scrollable panel the `?`
    /// key opens) and applied straight to `DeckUi::help_open` by
    /// [`crate::deck_ui::ingest_inbound`]. Out-of-band view state, ignored by
    /// the model fold — like [`Inbound::GraphSnapshot`].
    ShowHelp,
    /// A launch-cinematic cue (see [`SplashCue`]): the driver replays the
    /// splash held open over a running init (session startup, `/init`) and
    /// releases it when init finishes. Out-of-band view state, applied
    /// straight to `DeckUi::splash` by [`crate::deck_ui::ingest_inbound`],
    /// ignored by the model fold — like [`Inbound::ShowHelp`].
    Splash(SplashCue),
    /// A refreshed snapshot of the **cross-process session registry** for the
    /// SESSIONS overlay (empty-prompt `←`). Every running stella session on
    /// this machine, grouped by [`SessionPhase`]. Out-of-band view state like
    /// [`Inbound::GraphSnapshot`]; the driver answers
    /// [`WorkspaceInput::SessionsRefresh`] and every archive/delete with one.
    Sessions(Vec<SessionInfo>),
    /// A refreshed snapshot of the persist-until-read notification store for
    /// the inbox overlay and the footer's unread badge. The driver polls the
    /// store (other sessions produce into it too) and pushes one whenever the
    /// set changes. Out-of-band view state, ignored by the model fold.
    Notifications(Vec<NotificationInfo>),
    /// Progress from an in-flight MCP OAuth login the tab started
    /// ([`WorkspaceInput::McpOauthLogin`]). `outcome` is `None` while running,
    /// `Some(ok)` when finished — success triggers the tab to request a fresh
    /// snapshot so the ⚿ oauth badge flips. Out-of-band view state.
    McpOauthStatus {
        server: String,
        message: String,
        outcome: Option<bool>,
    },
    /// A refreshed snapshot of the agent-engine configuration
    /// (`settings.json` → `agent_engine_config`) for the ENGINE overlay
    /// (the SETTINGS tab's config panel). Out-of-band view state exactly like
    /// [`Inbound::GraphSnapshot`]: applied straight to `DeckUi` by
    /// [`crate::deck_ui::ingest_inbound`], ignored by the model fold. The
    /// driver sends one at startup, after every
    /// [`WorkspaceInput::EngineConfigSave`], and on
    /// [`WorkspaceInput::EngineConfigRefresh`]. `status`, when set,
    /// replaces the overlay's hint line (save outcomes, errors).
    EngineConfig {
        state: EngineConfigState,
        status: Option<String>,
    },
    /// The answer to an ISSUES-tab [`WorkspaceInput::IssuesRefresh`] (and the
    /// follow-up refresh a successful [`WorkspaceInput::IssueCreate`]
    /// triggers): the tracker's issue list, or the error that stopped it —
    /// including the "no tracker connected" hint the tab renders as its
    /// empty state. Out-of-band view state like [`Inbound::McpSearchResults`];
    /// `seq` echoes the request so the panel can drop stale replies.
    IssuesList {
        seq: u64,
        outcome: Result<Vec<IssueRow>, String>,
    },
    /// The outcome of one ISSUES-tab mutation ([`WorkspaceInput::IssueCreate`]
    /// / [`WorkspaceInput::IssueAct`]): a human status line on success (the
    /// created key + url, "comment added", …) or the failure reason. `key` is
    /// the issue acted on (the created key for a create; empty when a create
    /// failed before a key existed). Out-of-band, seq-guarded like
    /// [`Inbound::IssuesList`].
    IssueActDone {
        seq: u64,
        key: String,
        outcome: Result<String, String>,
    },
    /// The answer to a type-ahead [`WorkspaceInput::EntitySearch`]: the merged
    /// hit list for the create form's Assignee/Labels popup. `query` echoes
    /// the text searched (display only); `seq` echoes the request so the
    /// per-keystroke stream can drop out-of-order replies — only the newest
    /// emitted seq is ever applied.
    EntityHits {
        field: EntityField,
        seq: u64,
        query: String,
        hits: Vec<EntityHit>,
    },
}

/// One row of the ISSUES tab's browse list — a tracker-agnostic mirror of
/// `stella-tools`' `IssueSummary` (the TUI never links the tools crate; the
/// driver maps one to the other).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueRow {
    /// `#123` (GitHub) or `ENG-123` (Linear).
    pub key: String,
    pub title: String,
    pub state: String,
    pub labels: Vec<String>,
    pub assignee: Option<String>,
    pub url: String,
    pub updated_at: Option<String>,
}

/// One row of the create form's type-ahead popup. `kind` is a display type
/// label ("Person", "Agent", "Memory", "Symbol", "Label", …) — rows render as
/// `Kind: label — description`. `insert` is what picking the row writes into
/// the field: `@login` or an email for people, the label name for labels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityHit {
    pub kind: String,
    pub label: String,
    pub description: String,
    pub insert: String,
}

/// Which create-form field a type-ahead [`WorkspaceInput::EntitySearch`]
/// feeds — each has its own vocabulary (people vs. labels).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityField {
    Assignee,
    Label,
}

impl EntityField {
    pub fn label(self) -> &'static str {
        match self {
            EntityField::Assignee => "assignee",
            EntityField::Label => "labels",
        }
    }
}

/// An action on one existing issue ([`WorkspaceInput::IssueAct`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueAction {
    /// Add a comment (the deck's `c` prompt).
    Comment(String),
    /// Move to a named status (`open`/`closed` on GitHub; any workflow-state
    /// word on Linear).
    SetStatus(String),
    /// Start work: the driver moves the issue to in-progress (`w`).
    StartWork,
}

/// The session-registry lifecycle phase, exactly the grouping the SESSIONS
/// overlay shows. A TUI-local mirror of `stella-store`'s `SessionStatus`
/// (the deck never links the store crate; the driver maps one to the other).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPhase {
    InProgress,
    NeedsInput,
    /// Set aside with its durable state intact — quit (or switched away
    /// from) with work still pending; the first thing resume looks for.
    Paused,
    Cancelled,
    Complete,
    Archived,
    Error,
}

impl SessionPhase {
    /// Display/grouping order: attention-worthy first.
    pub const ALL: [SessionPhase; 7] = [
        SessionPhase::InProgress,
        SessionPhase::NeedsInput,
        SessionPhase::Paused,
        SessionPhase::Cancelled,
        SessionPhase::Complete,
        SessionPhase::Archived,
        SessionPhase::Error,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SessionPhase::InProgress => "In Progress",
            SessionPhase::NeedsInput => "Needs Input",
            SessionPhase::Paused => "Paused",
            SessionPhase::Cancelled => "Cancelled",
            SessionPhase::Complete => "Complete",
            SessionPhase::Archived => "Archived",
            SessionPhase::Error => "Error",
        }
    }
}

/// One row of the SESSIONS overlay — a running (or finished) stella session
/// from the machine-wide registry, with the human title and work summary the
/// registry holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    /// Registry id (`ses-…`), the archive/delete handle.
    pub id: String,
    /// Human title: `<workspace basename>: <first prompt…>`.
    pub title: String,
    /// What work is involved — the latest prompt/goal, truncated.
    pub summary: String,
    /// Workspace path (dimmed detail line).
    pub workspace: String,
    pub phase: SessionPhase,
    pub started_ms: u64,
    pub updated_ms: u64,
    /// True for the record of THIS deck process (rendered with a marker and
    /// protected from delete).
    pub mine: bool,
    /// True when the session can be reopened here: no live process owns it,
    /// it belongs to this deck's workspace, and its durable state (journal /
    /// history) is on disk. `⏎` on such a row sends
    /// [`WorkspaceInput::SessionResume`].
    pub resumable: bool,
}

/// One persist-until-read notification as the inbox overlay lists it. A
/// mirror of `stella-store`'s `Notification` minus storage concerns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationInfo {
    pub id: String,
    pub title: String,
    pub body: String,
    /// Origin hint (session id, server name); may be empty.
    pub source: String,
    pub created_ms: u64,
    pub read: bool,
    /// The session this notification is about, when it has one — what lets
    /// the inbox's `Enter` open the session (replaying it if it is no longer
    /// live) via [`WorkspaceInput::SessionOpen`].
    pub session_id: Option<String>,
}

/// Driver → deck cues for the launch cinematic ([`crate::splash`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplashCue {
    /// Restart the splash **held** open over a running init: the battle
    /// scene loops until `Release`. Ignored on `--no-anim` sessions (a
    /// static frame is their contract).
    Replay,
    /// Init finished — let the timeline advance to the wordmark reveal and
    /// fade out. A no-op if no held splash is playing.
    Release,
}

/// Which config level an installed agent definition lives at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentScope {
    /// The workspace's `.stella/agents/` directory.
    Project,
    /// The user's `~/.config/stella/agents/` directory.
    User,
}

impl AgentScope {
    pub fn label(self) -> &'static str {
        match self {
            AgentScope::Project => "project",
            AgentScope::User => "user",
        }
    }
}

/// One selectable version of an installed agent (the version picker's row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentVersionInfo {
    /// 1-based version number.
    pub version: u32,
    /// Short display label (e.g. the version file's modification time), or
    /// empty when none was available.
    pub label: String,
}

/// One installed agent as the Agents tab's INSTALLED AGENTS pane lists it
/// (see [`Inbound::AgentsList`]). Decoupled from `stella-core`'s `AgentDef`
/// so the TUI crate stays independent of the extensions engine — the driver
/// maps one to the other and adds the version/scope bookkeeping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledAgentEntry {
    /// The agent's loaded definition name.
    pub name: String,
    /// One-line description shown beside the name.
    pub description: String,
    /// The toolbelt grant from the definition's `tools:` frontmatter.
    /// `None` = the definition does not restrict tools — the agent gets
    /// **all** tools, and the pane says so honestly.
    pub tools: Option<Vec<String>>,
    /// Which config level the definition lives at.
    pub scope: AgentScope,
    /// The definition file the loader reads (display/provenance).
    pub source_path: String,
    /// The pinned (active) version — 1 for a never-versioned agent.
    pub version: u32,
    /// Every version on disk, oldest first. Always contains `version`.
    pub versions: Vec<AgentVersionInfo>,
    /// The pinned version's full file content (frontmatter + body) — what
    /// the editor loads.
    pub content: String,
}

/// What the deck sends back to the caller / engine. The single-session
/// [`UserInput`] is wrapped with the target agent; new verbs cover the deck's
/// workspace-level affordances (queueing, agent control, quit).
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceInput {
    /// Route a `UserInput` (prompt, scope decision, ask-user answer) to one agent.
    ToAgent { agent: AgentId, input: UserInput },
    /// Queue a brand-new prompt without blocking on any busy agent — the
    /// router picks the model/agent. The deck never gates input on agent state.
    Enqueue { text: String },
    /// Queue a prompt at the FRONT: the first submission after a
    /// [`WorkspaceInput::StopAndHold`]. The deck sends this instead of
    /// [`WorkspaceInput::Enqueue`] while dispatch is held, so the user's new
    /// prompt runs before the prompt the hold returned to the queue — and
    /// receiving it is what releases the hold.
    EnqueueFront { text: String },
    /// Remove one not-yet-dispatched prompt from the queue (0 = oldest). The
    /// deck's queue editor sends this for `ctrl+x` delete and for pulling a
    /// prompt back into the composer to edit it.
    QueueRemove { index: usize },
    /// Drop every not-yet-dispatched prompt (the deck confirms with a second
    /// `ctrl+d` before sending this).
    QueueClear,
    /// Pause / resume / stop / restart a specific agent.
    Control {
        agent: AgentId,
        control: AgentControl,
    },
    /// Double-Esc: cancel `agent`'s in-flight turn, return that turn's
    /// prompt to the FRONT of the queue, and HOLD dispatch until the user's
    /// next submission — "full stop; what I type next runs first". A single
    /// Esc is the plain [`AgentControl::Stop`]: the lead SOFT-stops at the
    /// next step boundary (completed steps kept); worker lanes cancel
    /// immediately, and the next queued prompt dispatches automatically.
    StopAndHold { agent: AgentId },
    /// Re-root the Graph tab on `file`: the deck's file picker sends this when
    /// the user selects a file, and the driver answers with a fresh
    /// [`Inbound::GraphSnapshot`] centered on it (the same out-of-band refresh
    /// path `/init` uses). `stella-tui` cannot query the graph store itself, so
    /// re-rooting is a round-trip rather than a local recompute — the picker
    /// only knows the file *names* (shipped in [`GraphSnapshot::files`]), never
    /// their neighborhoods. `file` is a root-relative path from that list.
    FocusGraphFile { file: String },
    /// The INSTALLED AGENTS pane opened (or wants a reload): enumerate the
    /// agent definitions installed at both scopes and answer with
    /// [`Inbound::AgentsList`].
    AgentsRefresh,
    /// Save an edited agent definition as a NEW version and pin it — the
    /// prior version is preserved on disk (see `stella-cli`'s
    /// `agents_installed` module for the on-disk scheme). The driver
    /// answers with a fresh [`Inbound::AgentsList`].
    AgentSave {
        name: String,
        scope: AgentScope,
        content: String,
    },
    /// Re-pin an existing version WITHOUT editing — the version count never
    /// changes on a pin (increments happen only on [`WorkspaceInput::AgentSave`]).
    AgentPin {
        name: String,
        scope: AgentScope,
        version: u32,
    },
    /// Create a new agent from a short description with LLM assistance: the
    /// driver drafts the definition through the session's provider, installs
    /// it at `scope`, and answers with a fresh [`Inbound::AgentsList`].
    AgentCreate {
        description: String,
        scope: AgentScope,
    },
    /// A SKILLS-tab operation (list / enable / uninstall / search / install /
    /// create / edit / pin). The driver owns the skills on disk + npx + model
    /// and answers with a refreshed [`Inbound::Skills`] / [`Inbound::SkillSearch`].
    Skill(SkillOp),
    /// MCP tab: flip a configured server's session enable/disable state. The
    /// driver toggles the shared disabled-servers set (hiding/showing the
    /// server's tools on the next model call) and pushes a fresh
    /// [`Inbound::McpServers`] snapshot.
    McpToggle { name: String },
    /// MCP tab: search the configured registry for `query`. The driver runs
    /// the async search and replies with [`Inbound::McpSearchResults`].
    McpSearch { query: String },
    /// MCP tab: install the registry server named `name` into `.stella/mcp.toml`
    /// (then refresh the snapshot).
    McpInstall { name: String },
    /// MCP tab: remove a configured server from `.stella/mcp.toml`.
    McpRemove { name: String },
    /// MCP tab: set an auth credential (env var for stdio, header for http) on a
    /// configured server. The value is a [`Secret`] — its `Debug` is redacted,
    /// so it never reaches the deck's debug log.
    McpAuth {
        server: String,
        field: String,
        value: Secret,
    },
    /// MCP tab: rebuild and re-push the [`Inbound::McpServers`] snapshot.
    McpRefresh,
    /// MCP tab: start the browser OAuth login for a configured **http**
    /// server. The driver runs the flow in the background and streams
    /// [`Inbound::McpOauthStatus`] updates (including the authorize URL).
    McpOauthLogin { server: String },
    /// SESSIONS overlay opened (or `r`): read the machine-wide session
    /// registry and answer with [`Inbound::Sessions`].
    SessionsRefresh,
    /// SESSIONS overlay / inbox: open a session in a replay lane. The driver
    /// loads the session's persisted event journal from the store (linked by
    /// `session_id` since store schema v8), then answers with a normal
    /// [`Inbound::Register`] (a `replay:<id>` lane) followed by every
    /// persisted event as ordinary [`Inbound::Event`]s and a terminal
    /// [`Inbound::Status`] — replay IS the fold, so a session dead for 12
    /// hours reconstructs to exactly the state it died in, with no second
    /// rendering path.
    SessionOpen { id: String },
    /// SESSIONS overlay: tuck a session record away (status → Archived).
    /// Answered with a fresh [`Inbound::Sessions`].
    SessionArchive { id: String },
    /// SESSIONS overlay: delete a session record from the registry.
    /// Answered with a fresh [`Inbound::Sessions`].
    SessionDelete { id: String },
    /// SESSIONS overlay: reopen a resumable session (⏎ on a
    /// [`SessionInfo::resumable`] row) — the deck-native "navigate back
    /// into a session". The driver parks the current session (its durable
    /// state is already on disk; its record flips to Paused), replays the
    /// chosen session's journal through the fold, restores its conversation
    /// and prompt backlog, and re-owns its registry record. Only serviced
    /// between turns — mid-turn the driver answers with a transcript notice
    /// instead of tearing down live work.
    SessionResume { id: String },
    /// Inbox overlay: mark one notification read (it may then be pruned —
    /// "persists until read" is the store's contract). Answered with a fresh
    /// [`Inbound::Notifications`].
    NotificationRead { id: String },
    /// Inbox overlay: mark everything read.
    NotificationsReadAll,
    /// ENGINE overlay: persist the edited agent-engine configuration into
    /// `settings.json` at `scope` (project `.stella/settings.json` or the
    /// user's `~/.config/stella/settings.json`). The driver writes the
    /// `agent_engine_config` object — preserving every other key in the
    /// file — and answers with a fresh [`Inbound::EngineConfig`] carrying
    /// the save outcome in `status`. Saved config applies to runs started
    /// afterwards; in-flight turns keep their resolved models.
    EngineConfigSave {
        state: EngineConfigState,
        scope: AgentScope,
    },
    /// ENGINE overlay opened (or wants a reload): re-read the settings
    /// scope chain and answer with a fresh [`Inbound::EngineConfig`].
    EngineConfigRefresh,
    /// ISSUES tab: list (or tracker-search) issues. `query`/`state` are the
    /// tracker-side filters; the driver answers with [`Inbound::IssuesList`]
    /// echoing `seq` so stale replies can be dropped.
    IssuesRefresh {
        query: Option<String>,
        state: Option<String>,
        seq: u64,
    },
    /// ISSUES tab: create an issue from the `n` form. The driver answers
    /// with [`Inbound::IssueActDone`] (the created key + url on success) and
    /// then a fresh [`Inbound::IssuesList`] under the same `seq`.
    IssueCreate {
        title: String,
        body: String,
        labels: Vec<String>,
        assignee: Option<String>,
        seq: u64,
    },
    /// ISSUES tab: act on one existing issue (comment / set-status / start
    /// work). Answered with [`Inbound::IssueActDone`].
    IssueAct {
        key: String,
        action: IssueAction,
        seq: u64,
    },
    /// ISSUES tab: one per-keystroke type-ahead query from the create form's
    /// Assignee/Labels field. Answered with [`Inbound::EntityHits`] echoing
    /// `seq` — the panel keeps only the newest.
    EntitySearch {
        field: EntityField,
        query: String,
        seq: u64,
    },
    /// Tear down the deck.
    Quit,
}

/// Which pipeline agent a per-agent engine override applies to — the four
/// configurable "agents" of `agent_engine_config`. `Default` is the
/// interactive/step-loop agent; the other three are the staged pipeline's
/// roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineRole {
    Default,
    Worker,
    Judge,
    Triage,
}

impl EngineRole {
    /// Stable settings-key / display order.
    pub const ALL: [EngineRole; 4] = [
        EngineRole::Default,
        EngineRole::Worker,
        EngineRole::Judge,
        EngineRole::Triage,
    ];

    /// The `agent_engine_config.agents.<key>` settings key (and display
    /// label) for this agent.
    pub fn key(self) -> &'static str {
        match self {
            EngineRole::Default => "default",
            EngineRole::Worker => "worker",
            EngineRole::Judge => "judge",
            EngineRole::Triage => "triage",
        }
    }
}

/// One agent's engine overrides as the ENGINE overlay edits them — a
/// TUI-local mirror of `stella-cli`'s `agent_engine_config` per-agent
/// settings, decoupled (like [`InstalledAgentEntry`]) so the TUI crate
/// stays independent of the settings engine; the driver maps one to the
/// other. Every field is optional — `None` renders as "provider default"
/// and is omitted from the saved JSON (the screenshot's unchecked
/// "Include" box).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineAgentState {
    /// Model slug — `"provider/slug"` or a bare slug.
    pub model: Option<String>,
    /// Gateway/provider id override (`"anthropic"`, `"openrouter"`,
    /// `"zai"`, or a settings-defined provider). When set, `model` is sent
    /// verbatim as the slug to THIS provider — how an OpenRouter key routes
    /// an `openai/...` slug.
    pub provider: Option<String>,
    /// Custom system prompt replacing the built-in one for this agent.
    pub prompt: Option<String>,
    /// Reasoning effort: `low|medium|high|xhigh|max`.
    pub effort: Option<String>,
    /// Thinking mode on/off (`None` = provider default).
    pub reasoning: Option<bool>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub frequency_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub repetition_penalty: Option<f32>,
    pub max_tokens: Option<u32>,
    pub seed: Option<u64>,
    /// `low|medium|high`.
    pub verbosity: Option<String>,
    /// `auto|default|flex|priority`.
    pub service_tier: Option<String>,
}

/// The full agent-engine configuration snapshot the ENGINE overlay renders
/// and edits ([`Inbound::EngineConfig`] / [`WorkspaceInput::EngineConfigSave`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineConfigState {
    /// Auto judge-model selection: pick the best allowed model for the
    /// judge (preferring a different family than the worker's).
    pub auto_mode: bool,
    /// Auto per-agent effort (judge high, worker medium, triage low).
    pub effort_auto: bool,
    /// Auto per-agent reasoning (on for judge/worker, off for triage).
    pub reasoning_auto: bool,
    /// The model slugs the model pickers offer (`allowed_models`). Empty
    /// means "no restriction" — pickers fall back to `catalog_models`.
    pub allowed_models: Vec<String>,
    /// Every configured provider id, for the provider picker.
    pub providers: Vec<String>,
    /// `provider/slug` strings from the catalog, scoped by the driver to
    /// providers with a usable credential — the picker's fallback
    /// vocabulary when `allowed_models` is empty.
    pub catalog_models: Vec<String>,
    /// Per-model effort vocabularies, keyed by the same `provider/slug`
    /// strings (plus any `allowed_models` spec): the effort levels this
    /// model, as served by this provider, can actually act on. An empty
    /// list means effort is not a knob for that model (no reasoning
    /// support, or an on/off-only thinking switch); a model absent from
    /// the map is unknown and keeps the full vocabulary.
    pub model_efforts: std::collections::HashMap<String, Vec<String>>,
    /// Exactly one entry per [`EngineRole::ALL`] slot, in that order.
    pub agents: Vec<EngineAgentState>,
}

impl EngineConfigState {
    /// The state for `role`, if present (the driver always sends all four).
    pub fn agent(&self, role: EngineRole) -> Option<&EngineAgentState> {
        EngineRole::ALL
            .iter()
            .position(|r| *r == role)
            .and_then(|i| self.agents.get(i))
    }
}

/// Which scope a skill lives in / is installed to. The loader reads both
/// (`<workspace>/.stella/skills` and `~/.config/stella/skills`); the SKILLS
/// tab asks this on install/create.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillScope {
    /// `<workspace>/.stella/skills` — travels with the repo.
    Project,
    /// `~/.config/stella/skills` — the user-global directory.
    User,
}

impl SkillScope {
    pub fn label(self) -> &'static str {
        match self {
            SkillScope::Project => "project",
            SkillScope::User => "user",
        }
    }
}

/// One installed-skill row in the SKILLS tab — a driver snapshot, deliberately
/// decoupled from `stella_core::skills::Skill` so the TUI crate stays
/// independent of the skills engine (the driver maps one to the other).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillRow {
    pub scope: SkillScope,
    pub name: String,
    pub description: String,
    /// The pinned version's markdown body — carried so the tab can open the
    /// edit overlay with no extra round-trip.
    pub body: String,
    /// Provenance label — `"workspace"`, `"user"`, `"installed"`, `"auto"`.
    pub origin: String,
    /// Session/persistent enabled state (a disabled skill is excluded from
    /// recall injection; the file stays on disk).
    pub enabled: bool,
    /// The pinned/current version the recall path uses.
    pub version: u32,
    /// The highest version on disk (`version < latest` ⇒ pinned to an older one).
    pub latest: u32,
    /// Whether the deck may uninstall (delete) it.
    pub removable: bool,
}

/// One registry search hit, parsed from `npx skills find` into structured
/// fields — never the raw ANSI-laden line. `id` (`owner/repo@skill`) is the
/// display name AND the token passed verbatim to `npx skills add` / `use`;
/// `installs` is the human popularity string (`"15.8K installs"`, empty if the
/// registry printed none) with `installs_rank` its numeric form for ranking/
/// the popularity bar; `url` is the `skills.sh` page (shown in the preview).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillSearchHit {
    pub id: String,
    pub installs: String,
    pub installs_rank: u64,
    pub url: String,
}

/// The full installed-skills read-model rendered by the SKILLS tab.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillsView {
    pub rows: Vec<SkillRow>,
    /// A one-line status/outcome hint (last op result), or `None`.
    pub status: Option<String>,
    /// True while a driver op (npx search/install, LLM create) is in flight,
    /// so the tab can show a working state.
    pub busy: bool,
}

/// A SKILLS-tab operation routed to the driver, which owns the disk + npx +
/// model. The deck emits one and folds back the refreshed [`Inbound::Skills`]
/// / [`Inbound::SkillSearch`] snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillOp {
    /// Re-enumerate installed skills → [`Inbound::Skills`].
    List,
    /// Persistently enable/disable a skill → re-send the list.
    SetEnabled {
        scope: SkillScope,
        name: String,
        enabled: bool,
    },
    /// Delete a skill entirely from disk (deck double-confirms first).
    Uninstall { scope: SkillScope, name: String },
    /// `npx skills find <query>` → [`Inbound::SkillSearch`].
    Search { query: String },
    /// Fetch a search hit's `SKILL.md` for the ctrl+o preview overlay
    /// (`npx skills use <id>`, extracting the wrapped body) →
    /// [`Inbound::SkillPreview`]. No disk write — preview only.
    Preview { id: String },
    /// Install a registry skill into `scope` → refresh the list.
    Install { scope: SkillScope, id: String },
    /// LLM-assisted creation from a short description → refresh the list.
    Create {
        scope: SkillScope,
        description: String,
    },
    /// Save an edited body as a NEW version (increments + pins it).
    Edit {
        scope: SkillScope,
        name: String,
        body: String,
    },
    /// Pin `version` as the active one WITHOUT editing (no version bump).
    Pin {
        scope: SkillScope,
        name: String,
        version: u32,
    },
}

/// A configured MCP server's full state for the MCP tab. The four state axes
/// are distinct on purpose: a server can be *configured* (in `mcp.toml`) yet
/// not *connected* (failed to start, or added after session start), and
/// *enabled* (session intent) is separate from *connected*.
#[derive(Clone, Debug, PartialEq)]
pub struct McpServerInfo {
    /// The local alias (config key + tool-namespace segment).
    pub name: String,
    /// Transport discriminant: `stdio` or `http`.
    pub kind: String,
    /// Enabled for this session (not in the disabled set).
    pub enabled: bool,
    /// Connected in the live tool set this session (tools are actually
    /// reachable). A newly-installed server shows `configured` but not
    /// `connected` until the next session.
    pub connected: bool,
    /// Short health label when connected (e.g. `live`, `reconnecting`).
    pub health: Option<String>,
    /// How many tools it advertises this session (0 when disabled/unconnected).
    pub tool_count: usize,
    /// Configured credential field names (env vars / headers) — presence means
    /// auth is set; the values are never carried here.
    pub auth_fields: Vec<String>,
    /// OAuth state: `None` = not applicable (stdio), `Some(logged_in)` for an
    /// http server (`o` starts the browser login; tokens never ride here).
    pub oauth: Option<bool>,
    /// Total recorded calls to this server's tools (from local telemetry).
    pub calls: u64,
}

/// The outcome of an MCP registry search requested from the tab.
#[derive(Clone, Debug, PartialEq)]
pub struct McpSearchOutcome {
    /// The query that produced these results (echoed for display).
    pub query: String,
    pub items: Vec<McpSearchItem>,
    /// Set when the search failed (network/registry error) instead of matching.
    pub error: Option<String>,
    /// Whether the registry reported more pages beyond this one.
    pub has_more: bool,
}

/// One registry search result row.
#[derive(Clone, Debug, PartialEq)]
pub struct McpSearchItem {
    pub name: String,
    pub description: String,
    /// A compact install-kinds hint, e.g. `npm, remote`.
    pub kinds: String,
    /// Whether a server of this name is already configured locally.
    pub installed: bool,
}

/// A secret string whose `Debug` is redacted, so it can ride the deck's input
/// channel (and any debug log of it) without leaking. The value is readable
/// only via [`Secret::reveal`], used solely to write the credential to config.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }
    /// The raw value — only for writing the credential into config.
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Secret").field(&"<redacted>").finish()
    }
}

/// The agent-control verbs surfaced by the dashboard. `Stop` maps to a clean
/// `UserInput::Cancel` today; `Pause`/`Resume`/`Restart` are RESERVED for
/// the fleet supervisor seam — the deck driver currently drops them, so no key is bound to
/// them (a keypress that visibly does nothing is worse than no key).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentControl {
    Pause,
    Resume,
    Stop,
    Restart,
}

impl AgentControl {
    pub fn label(self) -> &'static str {
        match self {
            AgentControl::Pause => "pause",
            AgentControl::Resume => "resume",
            AgentControl::Stop => "stop",
            AgentControl::Restart => "restart",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_and_terminal_are_disjoint() {
        for s in [
            AgentStatus::Queued,
            AgentStatus::Running,
            AgentStatus::Paused,
            AgentStatus::WaitingInput,
            AgentStatus::Done,
            AgentStatus::Failed,
            AgentStatus::Killed,
        ] {
            assert!(!(s.is_active() && s.is_terminal()), "{s:?}");
        }
    }

    #[test]
    fn meta_builder_sets_fields() {
        let m = AgentMeta::new("lead", "acme-api", 1000)
            .with_role("lead")
            .with_pid(4242);
        assert_eq!(m.id, "lead");
        assert_eq!(m.role, "lead");
        assert_eq!(m.pid, Some(4242));
    }
}
