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
    /// Pause / resume / stop / restart a specific agent.
    Control {
        agent: AgentId,
        control: AgentControl,
    },
    /// Tear down the deck.
    Quit,
}

/// The agent-control verbs surfaced by the dashboard. `Stop` maps to a clean
/// `UserInput::Cancel` today; `Pause`/`Resume`/`Restart` are honored by the
/// fleet supervisor seam (see `COMMAND_DECK_DESIGN.md` → "Backend seams").
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
