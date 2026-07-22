//! `stella-tui` — the ratatui event-log REPL (
//! ADR-023, §T).
//!
//! This crate renders **exclusively** from [`stella_protocol::AgentEvent`]s
//! (L-T1). It never touches the engine directly: `AgentEvent`s flow in over a
//! channel, [`UserInput`]s flow back out. The design is two layers:
//!
//! - **A pure core.** [`SessionModel`] folds the append-only event log into
//!   derived state — transcript lines, the files-touched map, HUD numbers, the
//!   pending scope-review — via its single mutator [`SessionModel::apply`]. No
//!   panel owns state that isn't reconstructible by replaying the log from seq
//!   1 (so replay is a supported debug mode, and the panic boundary is sound).
//!   [`render`] draws that model into a `ratatui` frame as a deterministic
//!   function of `(model, ui)`. Ephemeral interaction state (scroll, composer,
//!   focus) lives in [`UiState`], never in the model.
//!
//! - **A thin shell.** [`run`] wires the pure core to a real terminal: raw
//!   mode + alternate screen (always restored on drop), the crossterm event
//!   loop, and the two channels. It carries no decision logic — key→action is
//!   [`handle_key`], event→state is [`ingest`], both unit-tested.
//!
//! Binding TUI requirements §T are honored
//! structurally: event-derived rendering (L-T1), mouse-off-by-default for
//! native copy (L-T2, [`RunOptions::mouse_capture`]), paste chips (L-T3,
//! [`Composer::paste`]), line-exact scroll (L-T4, [`ScrollState`]), diffs on
//! the single event path (L-T5, [`model::FileState`]), buffer-not-ANSI tests
//! (L-T6), the panel panic boundary (L-T7, [`render`]), and the debug channel
//! (L-T8, [`DebugLog`]).

pub mod attach;
pub mod clipboard;
pub mod composer;
pub mod input;
pub mod model;
pub mod render;
pub mod scroll;
pub mod shell;
pub(crate) mod term;
pub mod textline;
pub mod ui;

// ── Command Deck: the multi-tab, multi-agent operations workspace ───────────
// Extends the single-session REPL above into a tabbed deck (Session · Agents ·
// Traces · Graph · Files) while preserving the pure-core / thin-shell design.
// See `COMMAND_DECK_DESIGN.md`.
pub mod cache_panel;
pub mod deck;
pub mod deck_render;
pub mod deck_shell;
pub mod deck_ui;
pub mod diff;
pub mod envelope;
pub mod fx;
pub mod graph;
pub mod invaders;
pub mod markdown;
pub mod progress;
pub mod resource;
pub mod scenario;
pub mod splash;
pub mod theme;
pub mod views;

pub use attach::probe_path_attachment;
pub use clipboard::{ClipboardPaste, default_attachments_dir};
pub use composer::{
    Composer, ComposerEntry, DEFAULT_PASTE_LINE_THRESHOLD, SlashCommand, SlashKind, SlashMenu,
    Submission,
};
pub use input::{ScopeDecision, UserInput};
pub use model::{FileState, Hud, SessionModel, TranscriptEntry};
pub use render::render;
pub use scroll::ScrollState;
pub use shell::{DebugLog, RunOptions, run};
pub use textline::{EventLine, Tone, event_line};
pub use ui::{PanelFocus, ShellAction, UiState, ViewportMetrics, handle_key, ingest};

// Command Deck public surface.
pub use deck::{
    AgentEntry, DeckTab, FileLedger, FileRecord, PrInfo, ResourceSample, RouteLog, TraceKind,
    TraceLog, TraceRow, WorkspaceModel,
};
pub use deck_render::render_deck;
pub use deck_shell::{DeckOptions, run_deck};
pub use deck_ui::{
    DeckAction, DeckUi, IssueField, IssuesMode, IssuesPanel, ScopeAction, SkillPrompt, SkillsFocus,
    SkillsPanel, TypeAhead, handle_deck_key, ingest_inbound,
};
pub use envelope::{
    AgentControl, AgentId, AgentMeta, AgentScope, AgentStatus, AgentVersionInfo, EngineAgentState,
    EngineConfigState, EngineRole, EntityField, EntityHit, Inbound, InstalledAgentEntry,
    IssueAction, IssueRow, McpSearchItem, McpSearchOutcome, McpServerInfo, NotificationInfo,
    Secret, SessionInfo, SessionPhase, SkillOp, SkillRow, SkillScope, SkillSearchHit, SkillsView,
    SplashCue, WorkspaceInput,
};
pub use graph::{GraphEdge, GraphNode, GraphSnapshot};
pub use resource::ResourceMonitor;
pub use splash::SplashState;
