//! SETTINGS tab — the home of all config in stella. Today it hosts the
//! `agent_engine_config` editor (the per-role model / prompt / sampling
//! overrides plus the global routing toggles) that used to share the AGENTS
//! tab's right column; the panel now fills the whole tab. As more config
//! surfaces move here they become sections of this tab.
//!
//! The editor itself lives in [`crate::views::engine`] — this module is the
//! thin tab shell that places [`crate::views::engine::render_panel`] into the
//! tab's content area. The panel is **modal while focused** (`e` focuses it,
//! Esc hands the keyboard back), exactly as it was in its former home, so the
//! always-on composer stays live until you enter the editor.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::deck::WorkspaceModel;
use crate::deck_ui::DeckUi;

/// Draw the SETTINGS tab: the config editor, full-area. `model` is unused for
/// now (the editor works over the driver-owned snapshot held in `ui.engine`),
/// kept in the signature to match every other tab view.
pub fn render(_model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    crate::views::engine::render_panel(ui, area, buf);
}
