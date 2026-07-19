//! The deck's tab views. Each exposes
//! `render(model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer)`
//! — a deterministic draw of the (model, ui) into a sub-area, recording any
//! viewport metrics it needs for scroll clamping back onto `ui.metrics`.
//! (`engine` is the exception: it is a full-frame overlay, not a tab, and
//! exposes `render_overlay(ui, area, buf)` plus its own modal key handler.)

pub mod agents;
pub mod engine;
pub mod files;
pub mod graph;
pub mod installed;
pub mod issues;
pub mod mcp;
pub mod session;
pub mod skills;
pub mod traces;
