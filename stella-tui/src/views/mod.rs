//! The deck's tab views. Each exposes
//! `render(model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer)`
//! — a deterministic draw of the (model, ui) into a sub-area, recording any
//! viewport metrics it needs for scroll clamping back onto `ui.metrics`.
//! (`engine` is the exception: it is the config editor the SETTINGS tab
//! ([`settings`]) hosts, not a tab renderer of its own — it exposes
//! `render_panel(ui, area, buf)` plus its own key handler, modal while the
//! panel is focused.)

pub mod agents;
pub mod engine;
pub mod files;
pub mod graph;
pub mod installed;
pub mod issues;
pub mod mcp;
pub mod session;
pub mod settings;
pub mod skills;
pub mod traces;
