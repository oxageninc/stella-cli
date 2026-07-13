//! The five tab views. Each exposes
//! `render(model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer)`
//! — a deterministic draw of the (model, ui) into a sub-area, recording any
//! viewport metrics it needs for scroll clamping back onto `ui.metrics`.

pub mod agents;
pub mod files;
pub mod graph;
pub mod session;
pub mod traces;
