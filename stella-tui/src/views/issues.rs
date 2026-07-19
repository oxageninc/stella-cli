//! ISSUES tab — the tracker-backed issue panel: browse/search the connected
//! tracker's issues, create one through a form, comment, move status, and
//! start work — all without leaving the deck.
//!
//! State lives entirely in [`crate::deck_ui::IssuesPanel`] (a field on
//! `DeckUi`); the driver services the [`crate::envelope::WorkspaceInput`]
//! requests the key handlers emit and answers with out-of-band
//! [`crate::envelope::Inbound::IssuesList`] / `IssueActDone` / `EntityHits`
//! snapshots. The create form's Assignee/Labels fields carry the type-ahead
//! popup (people · agents · memories · symbols · labels), rendered here
//! anchored under the active field.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use crate::deck::WorkspaceModel;
use crate::deck_ui::{DeckUi, IssueField, IssuesMode, IssuesPanel};
use crate::envelope::{EntityHit, IssueRow};
use crate::render::scroll_window_start;
use crate::theme;

/// Most hit rows the type-ahead popup shows before it scrolls.
const TYPEAHEAD_MAX_ROWS: usize = 8;
/// Most body lines the create form previews before eliding.
const FORM_BODY_MAX_LINES: usize = 6;

pub fn render(_model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    let title = format!(" ISSUES — {} listed ", ui.issues.rows.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(theme::rule());
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    // The line index (within `lines`) of the create form's active field —
    // the type-ahead popup anchors right under it.
    let mut active_field_line = 0usize;

    match ui.issues.mode {
        IssuesMode::Create => {
            active_field_line = render_form(&ui.issues, inner.width as usize, &mut lines);
        }
        IssuesMode::SearchTracker => {
            lines.push(Line::from(vec![
                Span::styled("  search tracker ", theme::accent()),
                Span::styled(
                    ui.issues.search_query.clone(),
                    Style::default().fg(theme::INK),
                ),
                Span::styled("▏", Style::default().fg(theme::EMBER_GOLD)),
            ]));
            lines.push(Line::default());
            render_list(&ui.issues, inner, &mut lines);
        }
        IssuesMode::Comment | IssuesMode::SetStatus => {
            let (label, target) = (
                if ui.issues.mode == IssuesMode::Comment {
                    "  comment on "
                } else {
                    "  set status of "
                },
                ui.issues
                    .selected()
                    .map(|r| r.key.clone())
                    .unwrap_or_default(),
            );
            lines.push(Line::from(vec![
                Span::styled(label, theme::accent()),
                Span::styled(target, Style::default().fg(theme::INK)),
                Span::styled(": ", theme::muted()),
                Span::styled(ui.issues.input.clone(), Style::default().fg(theme::INK)),
                Span::styled("▏", Style::default().fg(theme::EMBER_GOLD)),
            ]));
            lines.push(Line::default());
            render_list(&ui.issues, inner, &mut lines);
        }
        IssuesMode::Browse => render_list(&ui.issues, inner, &mut lines),
    }

    // Notice line (op outcomes, errors, the no-tracker hint) + key footer.
    lines.push(Line::default());
    if let Some(notice) = &ui.issues.notice {
        lines.push(Line::from(Span::styled(
            format!("  {}{notice}", if ui.issues.busy { "◌ " } else { "" }),
            Style::default().fg(theme::EMBER_GOLD),
        )));
    }
    lines.push(footer(ui.issues.mode));

    Paragraph::new(lines).render(inner, buf);

    // The type-ahead popup floats above the form, anchored to its field.
    if ui.issues.mode == IssuesMode::Create && ui.issues.typeahead.open() {
        render_typeahead(ui, inner, active_field_line, buf);
    }
}

/// The browse list, windowed on the selection so long lists keep it in view.
fn render_list(issues: &IssuesPanel, inner: Rect, lines: &mut Vec<Line<'static>>) {
    if issues.rows.is_empty() {
        if !issues.busy {
            lines.push(Line::from(Span::styled(
                if issues.loaded {
                    "  No issues matched."
                } else {
                    "  No issues loaded yet — press r to fetch the tracker's list."
                },
                theme::muted(),
            )));
        }
        return;
    }
    let reserved = lines.len() + 3; // header lines already pushed + notice/footer
    let visible = (inner.height as usize).saturating_sub(reserved).max(1);
    let selected = issues.sel.min(issues.rows.len() - 1);
    let first = scroll_window_start(issues.rows.len(), selected, visible);
    let last = (first + visible).min(issues.rows.len());
    for (i, row) in issues.rows.iter().enumerate().take(last).skip(first) {
        lines.push(issue_line(row, i == selected, inner.width as usize));
    }
    if last < issues.rows.len() {
        lines.push(Line::from(Span::styled(
            format!("  … {} more", issues.rows.len() - last),
            theme::muted(),
        )));
    }
}

/// One issue row: `▸ KEY [state] title · assignee · labels`.
fn issue_line(row: &IssueRow, selected: bool, width: usize) -> Line<'static> {
    let marker = if selected { "▸ " } else { "  " };
    let mut key_style = theme::accent();
    let mut body_style = Style::default().fg(theme::INK);
    let mut dim_style = theme::muted();
    if selected {
        key_style = key_style.add_modifier(Modifier::REVERSED);
        body_style = body_style.add_modifier(Modifier::REVERSED);
        dim_style = dim_style.add_modifier(Modifier::REVERSED);
    }
    let mut tail = String::new();
    if let Some(assignee) = &row.assignee {
        tail.push_str(&format!(" · {assignee}"));
    }
    if !row.labels.is_empty() {
        tail.push_str(&format!(" · {}", row.labels.join(", ")));
    }
    let head = format!("{marker}{} ", row.key);
    let state = format!("[{}] ", row.state);
    let budget = width
        .saturating_sub(head.chars().count() + state.chars().count() + tail.chars().count())
        .max(8);
    Line::from(vec![
        Span::styled(head, key_style),
        Span::styled(state, dim_style),
        Span::styled(truncate(&row.title, budget), body_style),
        Span::styled(tail, dim_style),
    ])
}

/// The create form. Returns the line index of the active field (for the
/// type-ahead popup's anchor).
fn render_form(issues: &IssuesPanel, width: usize, lines: &mut Vec<Line<'static>>) -> usize {
    lines.push(Line::from(Span::styled(
        "  new issue",
        theme::accent().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());
    let mut active = 0usize;

    let field_line = |lines: &mut Vec<Line<'static>>, label: &str, value: &str, focused: bool| {
        let label_style = if focused {
            theme::accent()
        } else {
            theme::muted()
        };
        let mut spans = vec![
            Span::styled(format!("  {label:<9} "), label_style),
            Span::styled(
                truncate(value, width.saturating_sub(16)),
                Style::default().fg(theme::INK),
            ),
        ];
        if focused {
            spans.push(Span::styled("▏", Style::default().fg(theme::EMBER_GOLD)));
        }
        lines.push(Line::from(spans));
    };

    // Title.
    if issues.form_field == IssueField::Title {
        active = lines.len();
    }
    field_line(
        lines,
        "title",
        &issues.form_title,
        issues.form_field == IssueField::Title,
    );

    // Body — the one multi-line field; preview capped, caret on the tail.
    let body_focused = issues.form_field == IssueField::Body;
    if body_focused {
        active = lines.len();
    }
    let body = issues.form_body.buffer().to_string();
    let body_lines: Vec<&str> = body.split('\n').collect();
    let shown = body_lines.len().min(FORM_BODY_MAX_LINES);
    for (i, body_line) in body_lines.iter().take(shown).enumerate() {
        let label = if i == 0 { "body" } else { "" };
        let is_tail = i + 1 == shown && body_lines.len() <= FORM_BODY_MAX_LINES;
        field_line(lines, label, body_line, body_focused && is_tail);
    }
    if body_lines.len() > FORM_BODY_MAX_LINES {
        lines.push(Line::from(Span::styled(
            format!("            … {} more lines", body_lines.len() - shown),
            theme::muted(),
        )));
    }

    // Labels + assignee — the type-ahead fields.
    if issues.form_field == IssueField::Labels {
        active = lines.len();
    }
    field_line(
        lines,
        "labels",
        &issues.form_labels,
        issues.form_field == IssueField::Labels,
    );
    if issues.form_field == IssueField::Assignee {
        active = lines.len();
    }
    field_line(
        lines,
        "assignee",
        &issues.form_assignee,
        issues.form_field == IssueField::Assignee,
    );
    active
}

/// The `Kind: label — description` text of one type-ahead row, split at the
/// kind prefix (styled separately) and char-safe-truncated to `max_chars`
/// across the pair. Pure — the row-format contract lives here.
pub(crate) fn entity_hit_parts(hit: &EntityHit, max_chars: usize) -> (String, String) {
    let kind = format!("{}: ", hit.kind);
    let rest = if hit.description.is_empty() {
        hit.label.clone()
    } else {
        format!("{} — {}", hit.label, hit.description)
    };
    let kind_len = kind.chars().count();
    if kind_len >= max_chars {
        return (truncate(&kind, max_chars), String::new());
    }
    (kind, truncate(&rest, max_chars - kind_len))
}

/// The floating type-ahead popup: accent-bordered, selection windowed, one
/// dim legend line. Anchored right under the active form field (clamped to
/// the panel).
fn render_typeahead(ui: &DeckUi, inner: Rect, field_line: usize, buf: &mut Buffer) {
    let ta = &ui.issues.typeahead;
    let rows = ta.hits.len().clamp(1, TYPEAHEAD_MAX_ROWS);
    let h = (rows as u16 + 3).min(inner.height);
    let w = inner.width.saturating_sub(4).clamp(20, 56).min(inner.width);
    let below = inner.y + (field_line as u16).saturating_add(1);
    let y = if below + h <= inner.y + inner.height {
        below
    } else {
        (inner.y + inner.height).saturating_sub(h)
    };
    let popup = Rect {
        x: inner.x + inner.width.saturating_sub(w) / 2,
        y,
        width: w,
        height: h,
    };
    Clear.render(popup, buf);

    let visible = (h as usize).saturating_sub(3).max(1);
    let selected = ta.sel.min(ta.hits.len().saturating_sub(1));
    let first = scroll_window_start(ta.hits.len(), selected, visible);
    let last = (first + visible).min(ta.hits.len());

    let mut lines: Vec<Line<'static>> = Vec::new();
    if ta.hits.is_empty() {
        lines.push(Line::from(Span::styled(
            if ta.loading {
                "  searching…"
            } else {
                "  no matches"
            },
            theme::muted(),
        )));
    }
    for (i, hit) in ta.hits.iter().enumerate().take(last).skip(first) {
        let is_sel = i == selected;
        let marker = if is_sel { "▸ " } else { "  " };
        let mut kind_style = theme::accent();
        let mut rest_style = theme::body();
        if is_sel {
            kind_style = kind_style.add_modifier(Modifier::REVERSED);
            rest_style = rest_style.add_modifier(Modifier::REVERSED);
        }
        let (kind, rest) = entity_hit_parts(hit, (w as usize).saturating_sub(6));
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), kind_style),
            Span::styled(kind, kind_style),
            Span::styled(rest, rest_style),
        ]));
    }
    // Pad so the legend sits on the last interior row.
    while lines.len() < (h as usize).saturating_sub(3) + 1 {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        " ↑↓ select · enter/tab insert · esc close",
        theme::muted(),
    )));

    let field = ta.field.map(|f| f.label().to_string()).unwrap_or_default();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(" {field} · {} ", ta.hits.len()));
    Paragraph::new(lines).block(block).render(popup, buf);
}

/// Char-safe prefix truncation with an ellipsis.
fn truncate(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// The keybind footer, per mode — the same violet-key/dim-word convention as
/// the MCP tab.
fn footer(mode: IssuesMode) -> Line<'static> {
    let pairs: &[(&str, &str)] = match mode {
        IssuesMode::Browse => &[
            ("↑↓", "select"),
            ("r", "refresh"),
            ("/", "search tracker"),
            ("n", "new issue"),
            ("c", "comment"),
            ("s", "set status"),
            ("w", "start work"),
        ],
        IssuesMode::SearchTracker => &[("type", "query"), ("enter", "search"), ("esc", "back")],
        IssuesMode::Create => &[
            ("tab/⇧tab", "field"),
            ("type", "@ or a first letter opens the picker"),
            ("ctrl+s", "create"),
            ("esc", "cancel"),
        ],
        IssuesMode::Comment | IssuesMode::SetStatus => {
            &[("type", "text"), ("enter", "send"), ("esc", "cancel")]
        }
    };
    let mut spans = vec![Span::raw("  ")];
    for (key, desc) in pairs {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(theme::VIOLET)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!("{desc}  "), theme::muted()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: &str, label: &str, description: &str) -> EntityHit {
        EntityHit {
            kind: kind.into(),
            label: label.into(),
            description: description.into(),
            insert: label.into(),
        }
    }

    #[test]
    fn entity_hit_rows_read_kind_label_description() {
        let (kind, rest) = entity_hit_parts(&hit("Person", "octocat", "Octo Cat"), 80);
        assert_eq!(format!("{kind}{rest}"), "Person: octocat — Octo Cat");

        // No description: the dash separator is dropped, never dangling.
        let (kind, rest) = entity_hit_parts(&hit("Label", "bug", ""), 80);
        assert_eq!(format!("{kind}{rest}"), "Label: bug");
    }

    #[test]
    fn entity_hit_rows_truncate_char_safely() {
        // A multi-byte description must truncate at a char boundary with an
        // ellipsis, never mid-codepoint.
        let (kind, rest) = entity_hit_parts(&hit("Memory", "naming", "prefer déjà-vu naming"), 20);
        let combined = format!("{kind}{rest}");
        assert!(combined.chars().count() <= 20, "{combined:?}");
        assert!(combined.ends_with('…'), "{combined:?}");

        // A kind wider than the budget still never panics.
        let (kind, rest) = entity_hit_parts(&hit("Symbol", "x", "y"), 4);
        assert!(kind.chars().count() <= 4, "{kind:?}");
        assert!(rest.is_empty());
    }

    #[test]
    fn issue_lines_mark_the_selection_and_carry_assignee_and_labels() {
        let row = IssueRow {
            key: "ENG-42".into(),
            title: "Fix flaky test".into(),
            state: "In Progress".into(),
            labels: vec!["bug".into(), "ci".into()],
            assignee: Some("mona@example.com".into()),
            url: String::new(),
            updated_at: None,
        };
        let text =
            |line: Line<'_>| -> String { line.spans.iter().map(|s| s.content.clone()).collect() };
        let selected = text(issue_line(&row, true, 120));
        assert!(selected.starts_with("▸ ENG-42"), "{selected}");
        assert!(selected.contains("[In Progress]"), "{selected}");
        assert!(selected.contains("mona@example.com"), "{selected}");
        assert!(selected.contains("bug, ci"), "{selected}");
        let plain = text(issue_line(&row, false, 120));
        assert!(plain.starts_with("  ENG-42"), "{plain}");
    }
}
