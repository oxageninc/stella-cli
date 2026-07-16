//! The AGENTS tab's INSTALLED AGENTS pane: the agents configured on disk at
//! the user (`~/.config/stella/agents`) and project (`.stella/agents`)
//! levels — name, description, and toolbelt per row — plus the pane's modal
//! sub-views (the definition editor, the create-from-prompt flow, and the
//! version picker).
//!
//! Every color comes from [`crate::theme`]; the list content comes verbatim
//! from the driver's [`crate::envelope::Inbound::AgentsList`] snapshot held
//! on [`crate::deck_ui::InstalledPanel`] (no shadow state).

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Widget, Wrap};

use crate::composer;
use crate::deck_ui::{DeckUi, InstalledMode, InstalledPanel};
use crate::envelope::InstalledAgentEntry;
use crate::theme;

/// Column headers for the browse list, matching `widths` in [`render_list`].
const HEADERS: [&str; 5] = ["Agent", "Scope", "Ver", "Description", "Toolbelt"];

pub fn render(ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    match ui.installed.mode {
        InstalledMode::Browse => render_list(&ui.installed, area, buf),
        InstalledMode::Edit => render_editor(&ui.installed, area, buf),
        InstalledMode::CreateDescribe => render_create_describe(&ui.installed, area, buf),
        InstalledMode::CreateScope => render_create_scope(&ui.installed, area, buf),
        InstalledMode::PickVersion => render_version_picker(&ui.installed, area, buf),
    }
}

/// The toolbelt cell text: the granted tools, or the honest "all tools"
/// when the definition doesn't restrict them.
pub fn toolbelt_label(tools: &Option<Vec<String>>) -> String {
    match tools {
        None => "all tools".to_string(),
        Some(list) => list.join(", "),
    }
}

fn render_list(panel: &InstalledPanel, area: Rect, buf: &mut Buffer) {
    let title = format!(" Installed Agents — {} on disk ", panel.entries.len());
    let block = Block::default().borders(Borders::ALL).title(title);

    let bands = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let (list_area, foot_area) = (bands[0], bands[1]);

    if panel.entries.is_empty() {
        let inner = block.inner(list_area);
        block.render(list_area, buf);
        let hint = if panel.busy {
            "loading installed agents…"
        } else if panel.loaded {
            "no agents installed — press n to create one from a prompt"
        } else {
            "press r to load the installed agents"
        };
        if inner.height > 0 {
            let y = inner.y + inner.height.saturating_sub(1) / 2;
            Paragraph::new(hint)
                .style(theme::muted())
                .alignment(Alignment::Center)
                .render(Rect::new(inner.x, y, inner.width, 1), buf);
        }
        render_footer(panel, foot_area, buf);
        return;
    }

    let header = Row::new(HEADERS.iter().copied().map(Cell::from)).style(theme::accent());
    let rows: Vec<Row> = panel
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| agent_row(entry, i == panel.sel))
        .collect();
    let widths = [
        Constraint::Length(20), // Agent
        Constraint::Length(8),  // Scope
        Constraint::Length(5),  // Ver
        Constraint::Fill(3),    // Description
        Constraint::Fill(2),    // Toolbelt
    ];
    Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .render(list_area, buf);

    render_footer(panel, foot_area, buf);
}

fn agent_row(entry: &InstalledAgentEntry, is_selected: bool) -> Row<'static> {
    let caret = if is_selected {
        Span::styled("> ", Style::default().fg(theme::AMBER))
    } else {
        Span::raw("  ")
    };
    let name_cell = Cell::from(Line::from(vec![
        caret,
        Span::styled(entry.name.clone(), Style::default().fg(theme::AMBER)),
    ]));
    let scope_cell = Cell::from(entry.scope.label()).style(theme::muted());
    let ver_cell = Cell::from(format!("v{}", entry.version)).style(theme::body());
    let desc_cell = Cell::from(entry.description.clone()).style(theme::body());
    let tools_cell = Cell::from(toolbelt_label(&entry.tools)).style(theme::muted());
    let mut row = Row::new(vec![name_cell, scope_cell, ver_cell, desc_cell, tools_cell]);
    if is_selected {
        row = row.style(Style::default().add_modifier(Modifier::REVERSED));
    }
    row
}

/// The bottom line: the transient status when set, the key legend otherwise.
fn render_footer(panel: &InstalledPanel, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let text = match &panel.status {
        Some(status) => status.clone(),
        None => "↑/↓ select · ⏎ edit (new pinned version) · v versions · n new from prompt · \
                 r reload · ←/→ panes"
            .to_string(),
    };
    Paragraph::new(text).style(theme::muted()).render(area, buf);
}

/// The definition editor: the pinned version's full content in a textarea.
/// A save (ctrl+s) is ALWAYS a new version; the window follows the cursor.
fn render_editor(panel: &InstalledPanel, area: Rect, buf: &mut Buffer) {
    let (name, scope) = match &panel.editing {
        Some((name, scope)) => (name.as_str(), scope.label()),
        None => ("agent", "?"),
    };
    let title =
        format!(" Edit {name} ({scope}) — ctrl+s saves a NEW pinned version · esc discards ");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(theme::AMBER));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let layout = composer::layout(&panel.editor, inner.width as usize);
    let height = inner.height as usize;
    // Scroll the window so the cursor row is always visible (bottom-anchored
    // once the content exceeds the viewport).
    let start = (layout.cursor_row + 1).saturating_sub(height);
    for (i, row) in layout.rows.iter().skip(start).take(height).enumerate() {
        let y = inner.y + i as u16;
        Paragraph::new(row.as_str())
            .style(theme::body())
            .render(Rect::new(inner.x, y, inner.width, 1), buf);
    }
    // The cursor cell, reversed — same visual as a terminal caret.
    let cy = inner.y + (layout.cursor_row - start) as u16;
    let cx = inner.x + (layout.cursor_col as u16).min(inner.width.saturating_sub(1));
    if cy < inner.y + inner.height {
        buf.set_style(
            Rect::new(cx, cy, 1, 1),
            Style::default().add_modifier(Modifier::REVERSED),
        );
    }
}

/// Create-from-prompt, step 1: the description input.
fn render_create_describe(panel: &InstalledPanel, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" New Agent — describe what it should do · ⏎ next · esc cancel ")
        .border_style(Style::default().fg(theme::AMBER));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 {
        return;
    }
    let text = format!("> {}▏", panel.create_desc);
    Paragraph::new(text)
        .style(theme::body())
        .wrap(Wrap { trim: false })
        .render(inner, buf);
    if inner.height > 2 {
        let hint = "the session model drafts the definition (name, description, toolbelt, \
                    system prompt) from this description";
        Paragraph::new(hint).style(theme::muted()).render(
            Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
            buf,
        );
    }
}

/// Create-from-prompt, step 2: the install-scope picker.
fn render_create_scope(panel: &InstalledPanel, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" New Agent — install scope · ↑/↓ choose · ⏎ create · esc back ")
        .border_style(Style::default().fg(theme::AMBER));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 {
        return;
    }
    let options = [
        "project — .stella/agents (this workspace)",
        "user — ~/.config/stella/agents (all projects)",
    ];
    for (i, option) in options.iter().enumerate() {
        if (i as u16) >= inner.height {
            break;
        }
        let selected = i == panel.scope_sel.min(1);
        let line = format!("{} {option}", if selected { ">" } else { " " });
        let style = if selected {
            Style::default()
                .fg(theme::AMBER)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::body()
        };
        Paragraph::new(line)
            .style(style)
            .render(Rect::new(inner.x, inner.y + i as u16, inner.width, 1), buf);
    }
}

/// The version picker: every version on disk, the pinned one marked. ⏎
/// re-pins WITHOUT writing a new version.
fn render_version_picker(panel: &InstalledPanel, area: Rect, buf: &mut Buffer) {
    let Some(entry) = panel.selected() else {
        return;
    };
    let title = format!(
        " Versions — {} · ⏎ pin (no new version) · esc close ",
        entry.name
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(theme::AMBER));
    let inner = block.inner(area);
    block.render(area, buf);
    for (i, info) in entry.versions.iter().enumerate() {
        if (i as u16) >= inner.height {
            break;
        }
        let selected = i == panel.version_sel;
        let pinned = if info.version == entry.version {
            "  ● pinned"
        } else {
            ""
        };
        let line = format!(
            "{} v{}  {}{pinned}",
            if selected { ">" } else { " " },
            info.version,
            info.label,
        );
        let mut style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            theme::body()
        };
        if info.version == entry.version {
            style = style.fg(theme::AMBER);
        }
        Paragraph::new(line)
            .style(style)
            .render(Rect::new(inner.x, inner.y + i as u16, inner.width, 1), buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{AgentScope, AgentVersionInfo};

    fn buffer_text(buf: &Buffer) -> String {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn entry(name: &str, tools: Option<Vec<String>>) -> InstalledAgentEntry {
        InstalledAgentEntry {
            name: name.into(),
            description: format!("what {name} does"),
            tools,
            scope: AgentScope::Project,
            source_path: format!("/ws/.stella/agents/{name}.md"),
            version: 2,
            versions: vec![
                AgentVersionInfo {
                    version: 1,
                    label: "2026-07-01".into(),
                },
                AgentVersionInfo {
                    version: 2,
                    label: "2026-07-16".into(),
                },
            ],
            content: format!("---\nname: {name}\n---\nbody"),
        }
    }

    fn ui_with(entries: Vec<InstalledAgentEntry>) -> DeckUi {
        let mut ui = DeckUi::default();
        ui.installed.entries = entries;
        ui.installed.loaded = true;
        ui
    }

    #[test]
    fn toolbelt_labels_are_honest_about_unrestricted_grants() {
        assert_eq!(toolbelt_label(&None), "all tools");
        assert_eq!(
            toolbelt_label(&Some(vec!["Read".into(), "Grep".into()])),
            "Read, Grep"
        );
    }

    #[test]
    fn list_renders_name_description_toolbelt_and_version() {
        let mut ui = ui_with(vec![
            entry("reviewer", Some(vec!["Read".into(), "Grep".into()])),
            entry("planner", None),
        ]);
        let area = Rect::new(0, 0, 120, 10);
        let mut buf = Buffer::empty(area);
        render(&mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("reviewer"), "name shown:\n{text}");
        assert!(
            text.contains("what reviewer does"),
            "description shown:\n{text}"
        );
        assert!(text.contains("Read, Grep"), "toolbelt shown:\n{text}");
        assert!(
            text.contains("all tools"),
            "an unrestricted grant reads as `all tools`:\n{text}"
        );
        assert!(text.contains("v2"), "pinned version shown:\n{text}");
        assert!(text.contains("project"), "scope shown:\n{text}");
    }

    #[test]
    fn empty_loaded_list_hints_at_create_from_prompt() {
        let mut ui = ui_with(vec![]);
        let area = Rect::new(0, 0, 80, 8);
        let mut buf = Buffer::empty(area);
        render(&mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(
            text.contains("press n to create one from a prompt"),
            "{text}"
        );
    }

    #[test]
    fn editor_renders_the_loaded_content_and_the_save_contract() {
        let mut ui = ui_with(vec![entry("reviewer", None)]);
        ui.installed.mode = InstalledMode::Edit;
        ui.installed.editing = Some(("reviewer".into(), AgentScope::Project));
        ui.installed.editor.load("---\nname: reviewer\n---\nbody");
        let area = Rect::new(0, 0, 90, 10);
        let mut buf = Buffer::empty(area);
        render(&mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("Edit reviewer"), "{text}");
        assert!(
            text.contains("NEW pinned version"),
            "the save-is-a-new-version contract is on screen:\n{text}"
        );
        assert!(text.contains("name: reviewer"), "content shown:\n{text}");
    }

    #[test]
    fn version_picker_marks_the_pinned_version() {
        let mut ui = ui_with(vec![entry("reviewer", None)]);
        ui.installed.mode = InstalledMode::PickVersion;
        ui.installed.version_sel = 0;
        let area = Rect::new(0, 0, 80, 8);
        let mut buf = Buffer::empty(area);
        render(&mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("v1"), "{text}");
        assert!(text.contains("v2  2026-07-16  ● pinned"), "{text}");
        assert!(
            text.contains("no new version"),
            "the pin-does-not-increment contract is on screen:\n{text}"
        );
    }

    #[test]
    fn create_flow_renders_description_then_scope() {
        let mut ui = ui_with(vec![]);
        ui.installed.mode = InstalledMode::CreateDescribe;
        ui.installed.create_desc = "reviews diffs".into();
        let area = Rect::new(0, 0, 100, 8);
        let mut buf = Buffer::empty(area);
        render(&mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("reviews diffs"), "{text}");
        assert!(text.contains("describe what it should do"), "{text}");

        ui.installed.mode = InstalledMode::CreateScope;
        let mut buf = Buffer::empty(area);
        render(&mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains(".stella/agents"), "{text}");
        assert!(text.contains("~/.config/stella/agents"), "{text}");
    }
}
