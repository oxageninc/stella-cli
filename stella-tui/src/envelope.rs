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
    /// A refreshed code-graph snapshot for the Graph tab. Unlike the other
    /// variants this is **not** a folded event — the graph is an out-of-band
    /// read-model (see `COMMAND_DECK_DESIGN.md` → "The purity boundary"). It
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
    /// Esc is the plain [`AgentControl::Stop`]: cancel, and the next queued
    /// prompt dispatches automatically.
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
    /// Tear down the deck.
    Quit,
}

/// The agent-control verbs surfaced by the dashboard. `Stop` maps to a clean
/// `UserInput::Cancel` today; `Pause`/`Resume`/`Restart` are RESERVED for
/// the fleet supervisor seam (see `COMMAND_DECK_DESIGN.md` → "Backend
/// seams") — the deck driver currently drops them, so no key is bound to
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
