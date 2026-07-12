//! `stella-tui` — the ratatui event-log REPL (`02-architecture.md` §2,
//! ADR-023, `09-lessons-learned.md` §T).
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
//! Binding TUI requirements from `09-lessons-learned.md` §T are honored
//! structurally: event-derived rendering (L-T1), mouse-off-by-default for
//! native copy (L-T2, [`RunOptions::mouse_capture`]), paste chips (L-T3,
//! [`Composer::paste`]), line-exact scroll (L-T4, [`ScrollState`]), diffs on
//! the single event path (L-T5, [`model::FileState`]), buffer-not-ANSI tests
//! (L-T6), the panel panic boundary (L-T7, [`render`]), and the debug channel
//! (L-T8, [`DebugLog`]).

pub mod composer;
pub mod input;
pub mod model;
pub mod render;
pub mod scroll;
pub mod shell;
pub mod ui;

pub use composer::{
    Composer, ComposerEntry, DEFAULT_PASTE_LINE_THRESHOLD, SlashCommand, SlashMenu,
};
pub use input::{ScopeDecision, UserInput};
pub use model::{FileState, Hud, SessionModel, TranscriptEntry};
pub use render::render;
pub use scroll::ScrollState;
pub use shell::{DebugLog, RunOptions, run};
pub use ui::{PanelFocus, ShellAction, UiState, ViewportMetrics, handle_key, ingest};
