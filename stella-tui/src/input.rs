//! The messages the TUI sends *back* to the engine over the submissions
//! channel. This is the other half of the `run(...)` contract: `AgentEvent`s
//! flow in, [`UserInput`] flows out. Kept in its own tiny module so both the
//! pure key-handling layer ([`crate::ui`]) and the interactive shell
//! ([`crate::shell`]) can depend on it without a cycle.

use stella_protocol::Attachment;

/// A message from the user to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserInput {
    /// A prompt to run. `text` is the fully-expanded message — paste chips
    /// have already been expanded to their payloads (L-T3). `attachments`
    /// carries any multimodal inputs (pasted images, attached files) the
    /// composer collected alongside the text.
    Prompt {
        text: String,
        attachments: Vec<Attachment>,
    },
    /// The user's answer to a pending scope-review gate (L-E5).
    ScopeDecision(ScopeDecision),
    /// The user's answer to a pending `ask_user` question. `id` correlates it
    /// back to the question (and, downstream, to the `ask_user` tool call's
    /// `ToolResult`); `answer` is either a chosen option's text or the user's
    /// own free-text reply — the always-available affordance the `AskUser`
    /// renderer contract mandates.
    AskUserAnswer { id: String, answer: String },
    /// A clean cancellation request (`q` / Ctrl-C). The engine should abort
    /// the current turn cleanly — never a mid-tool kill.
    Cancel,
}

/// The three answers a scope-review card offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeDecision {
    /// Run the plan as proposed.
    Approve,
    /// Approve, but trim the plan down first.
    Trim,
    /// Abort the plan.
    Abort,
}
