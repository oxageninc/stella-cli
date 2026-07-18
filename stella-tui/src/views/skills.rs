//! SKILLS tab — the filesystem-first skills manager.
//!
//! Two panes, switched with ←/→: **Installed** (the manage list — activate,
//! disable, uninstall, edit, pin) and **Registry search** (`npx skills find` →
//! install). The driver owns the skills on disk (both scopes), their
//! enabled/version/pin state, and the npx registry; this view renders the
//! [`crate::envelope::SkillsView`] read-model it pushes and draws the scope /
//! create / edit / pin overlays. Every color comes from [`crate::theme`]; the
//! content is a deterministic function of `(ui.skills)` so buffer tests stay
//! byte-stable.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

use crate::deck::WorkspaceModel;
use crate::deck_ui::{DeckUi, SkillPrompt, SkillsFocus};
use crate::theme;

pub fn render(_model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    // Two panes over a status line.
    let bands = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let panes = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(bands[0]);

    render_installed(ui, panes[0], buf);
    render_search(ui, panes[1], buf);
    render_status(ui, bands[1], buf);

    // Overlays (scope picker / create / edit / pin) float above the panes.
    if ui.skills.prompt.is_some() {
        render_overlay(ui, area, buf);
    }
    // The ctrl+o markdown preview is the topmost overlay (mutually exclusive
    // with the prompts at the key layer, but drawn last defensively).
    if ui.skills.preview.is_some() {
        render_preview(ui, area, buf);
    }
}

/// The manage pane: one row per installed skill with its enabled box, pinned
/// version, scope + origin, and description.
fn render_installed(ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let focused = ui.skills.focus == SkillsFocus::Installed;
    let rows = &ui.skills.view.rows;
    let title = format!(" Installed — {} (user + project) ", rows.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            theme::accent()
        } else {
            theme::rule()
        })
        .title(title);
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if rows.is_empty() {
        let hint = if ui.skills.view.busy {
            "loading…"
        } else {
            "no skills installed — press → to search the registry and add one"
        };
        Paragraph::new(hint)
            .style(theme::muted())
            .alignment(Alignment::Center)
            .render(centered_row(inner), buf);
        return;
    }

    let visible = inner.height as usize;
    let sel = ui.skills.sel.min(rows.len() - 1);
    let start = window_start(rows.len(), sel, visible);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, row) in rows.iter().enumerate().skip(start).take(visible) {
        let is_sel = i == sel && focused;
        let marker = if is_sel { "▸ " } else { "  " };
        let boxed = if row.enabled { "[x] " } else { "[ ] " };
        let box_style = if row.enabled {
            Style::default().fg(theme::AMBER)
        } else {
            theme::muted()
        };
        let ver = if row.latest > row.version {
            format!(" v{}/{}", row.version, row.latest)
        } else {
            format!(" v{}", row.version)
        };
        let meta = format!("  ({}·{})", row.scope.label(), row.origin);
        // Description fills whatever width remains, truncated char-safe.
        let used = marker.len() + boxed.len() + row.name.chars().count() + ver.len() + meta.len();
        let desc_room = (inner.width as usize).saturating_sub(used + 3);
        let desc = if desc_room >= 6 && !row.description.is_empty() {
            format!("  {}", truncate(&row.description, desc_room))
        } else {
            String::new()
        };
        let mut line = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme::AMBER)),
            Span::styled(boxed, box_style),
            Span::styled(row.name.clone(), theme::body()),
            Span::styled(ver, theme::muted()),
            Span::styled(meta, theme::muted()),
            Span::styled(desc, theme::muted()),
        ]);
        if is_sel {
            line = line.style(Style::default().add_modifier(Modifier::REVERSED));
        }
        lines.push(line);
    }
    Paragraph::new(lines).render(inner, buf);
}

/// The registry-search pane: the live query line, then the last search's hits.
fn render_search(ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let focused = ui.skills.focus == SkillsFocus::Search;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            theme::accent()
        } else {
            theme::rule()
        })
        .title(" Registry search ");
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    // Query line with a block caret when this pane is focused.
    let caret = if focused { "▌" } else { "" };
    lines.push(Line::from(vec![
        Span::styled("find: ", theme::muted()),
        Span::styled(ui.skills.query.clone(), theme::body()),
        Span::styled(caret.to_string(), Style::default().fg(theme::AMBER)),
    ]));
    if ui.skills.searching {
        lines.push(Line::from(Span::styled("working…", theme::muted())));
    }
    lines.push(Line::default());

    if ui.skills.hits.is_empty() && !ui.skills.searching {
        lines.push(Line::from(Span::styled(
            "type a term and press ⏎ to search",
            theme::muted(),
        )));
    } else {
        let header_rows = lines.len();
        let visible = (inner.height as usize).saturating_sub(header_rows).max(1);
        let sel = ui
            .skills
            .search_sel
            .min(ui.skills.hits.len().saturating_sub(1));
        let start = window_start(ui.skills.hits.len(), sel, visible);
        // The most-installed hit anchors the popularity bar's full width.
        let peak = ui
            .skills
            .hits
            .iter()
            .map(|h| h.installs_rank)
            .max()
            .unwrap_or(0);
        let width = inner.width as usize;
        for (i, hit) in ui.skills.hits.iter().enumerate().skip(start).take(visible) {
            let is_sel = i == sel && focused;
            let marker = if is_sel { "▸ " } else { "  " };
            // Right column: an amber popularity bar + the dim installs metric,
            // both empty when the registry printed no count.
            let bar = popularity_bar(hit.installs_rank, peak);
            let metric = hit.installs.clone();
            let bar_w = bar.chars().count();
            let metric_w = metric.chars().count();
            // Widths: bar, one gap before the metric, and one gap before the bar.
            let right_w = if metric.is_empty() {
                0
            } else {
                bar_w + usize::from(bar_w > 0) + metric_w + 1
            };
            let name =
                truncate_skill_id(&hit.id, width.saturating_sub(marker.len() + right_w).max(4));
            let pad = width
                .saturating_sub(marker.len() + name.chars().count() + right_w)
                .max(1);
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(theme::AMBER)),
                Span::styled(name, theme::body()),
                Span::styled(" ".repeat(pad), theme::muted()),
            ];
            if !bar.is_empty() {
                spans.push(Span::styled(
                    format!("{bar} "),
                    Style::default().fg(theme::AMBER),
                ));
            }
            if !metric.is_empty() {
                spans.push(Span::styled(metric, theme::muted()));
            }
            let mut line = Line::from(spans);
            if is_sel {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            lines.push(line);
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "ctrl+o preview · ⏎ install",
            theme::muted(),
        )));
    }
    Paragraph::new(lines).render(inner, buf);
}

/// The `▰▱`-style micro-bar for a hit's install count relative to the
/// most-installed hit in the result set. Empty when there is no signal.
fn popularity_bar(rank: u64, peak: u64) -> String {
    if peak == 0 || rank == 0 {
        return String::new();
    }
    // 1..=4 filled blocks, scaled against the peak.
    let filled = (((rank as f64 / peak as f64) * 4.0).round() as usize).clamp(1, 4);
    let mut s = String::with_capacity(4 * 3);
    for _ in 0..filled {
        s.push('▰');
    }
    for _ in filled..4 {
        s.push('▱');
    }
    s
}

/// The bottom status / legend line.
fn render_status(ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let legend = match ui.skills.focus {
        SkillsFocus::Installed => {
            "space on/off · ctrl+o preview · ctrl+x×2 delete · e edit · p pin · n new · → search"
        }
        SkillsFocus::Search => {
            "⏎ search / install · ↑/↓ pick · ctrl+o preview · ← installed · Tab leaves"
        }
    };
    let line = match &ui.skills.status {
        Some(status) => Line::from(Span::styled(
            format!(" {status}"),
            theme::body().fg(theme::AMBER),
        )),
        None => Line::from(Span::styled(format!(" {legend}"), theme::muted())),
    };
    Paragraph::new(line).render(area, buf);
}

/// Draw the active overlay centered over the panes.
fn render_overlay(ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    match &ui.skills.prompt {
        Some(SkillPrompt::Scope { action, user }) => {
            let verb = match action {
                crate::deck_ui::ScopeAction::Install { id } => format!("Install {id}"),
                crate::deck_ui::ScopeAction::Create { .. } => "Create the new skill".to_string(),
            };
            let choose = |label: &str, hint: &str, selected: bool| {
                let marker = if selected { "▸ " } else { "  " };
                let style = if selected {
                    theme::body().fg(theme::AMBER).add_modifier(Modifier::BOLD)
                } else {
                    theme::body()
                };
                Line::from(vec![
                    Span::styled(marker, Style::default().fg(theme::AMBER)),
                    Span::styled(label.to_string(), style),
                    Span::styled(format!("  {hint}"), theme::muted()),
                ])
            };
            let lines = vec![
                Line::from(Span::styled(verb, theme::body())),
                Line::from(Span::styled("Where should it live?", theme::muted())),
                Line::default(),
                choose(
                    "[p] Project",
                    ".stella/skills — travels with the repo",
                    !*user,
                ),
                choose("[u] User", "~/.config/stella/skills — global to you", *user),
                Line::default(),
                Line::from(Span::styled(
                    "←/→ or p/u choose · ⏎ confirm · esc cancel",
                    theme::muted(),
                )),
            ];
            popup(" install scope ", lines, area, buf);
        }
        Some(SkillPrompt::CreateDescription { buffer }) => {
            let lines = vec![
                Line::from(Span::styled(
                    "Describe the skill you want (the agent will search the",
                    theme::muted(),
                )),
                Line::from(Span::styled(
                    "registry, rank matches, and assemble one skill):",
                    theme::muted(),
                )),
                Line::default(),
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(theme::AMBER)),
                    Span::styled(buffer.clone(), theme::body()),
                    Span::styled("▌", Style::default().fg(theme::AMBER)),
                ]),
                Line::default(),
                Line::from(Span::styled("⏎ continue · esc cancel", theme::muted())),
            ];
            popup(" new skill (LLM-assisted) ", lines, area, buf);
        }
        Some(SkillPrompt::Pin {
            name, latest, sel, ..
        }) => {
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("Pin a version of {name}:"),
                    theme::body(),
                )),
                Line::default(),
            ];
            for v in 1..=*latest {
                let selected = v == *sel;
                let marker = if selected { "▸ " } else { "  " };
                let tag = if v == *latest { "  (latest)" } else { "" };
                let style = if selected {
                    theme::body().fg(theme::AMBER).add_modifier(Modifier::BOLD)
                } else {
                    theme::body()
                };
                lines.push(Line::from(vec![
                    Span::styled(marker, Style::default().fg(theme::AMBER)),
                    Span::styled(format!("v{v}{tag}"), style),
                ]));
            }
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "↑/↓ choose · ⏎ pin · esc cancel",
                theme::muted(),
            )));
            popup(" pin version ", lines, area, buf);
        }
        Some(SkillPrompt::Edit { name, buffer, .. }) => {
            render_edit_overlay(name, buffer, area, buf)
        }
        None => {}
    }
}

/// A taller popup for the edit buffer: the (multi-line) body with a caret and a
/// save/cancel legend. The buffer scrolls to keep the last line visible.
fn render_edit_overlay(name: &str, buffer: &str, area: Rect, buf: &mut Buffer) {
    let w = area.width.saturating_sub(6).clamp(20, 88);
    let h = area.height.saturating_sub(2).clamp(6, 20);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(rect, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(
            " edit {name} — ctrl+s save (new version) · esc cancel "
        ));
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.height == 0 {
        return;
    }
    // Body lines with a block caret appended, tail-scrolled to the last rows.
    let mut lines: Vec<Line<'static>> = Vec::new();
    let text_lines: Vec<&str> = buffer.split('\n').collect();
    let visible = inner.height as usize;
    let start = text_lines.len().saturating_sub(visible);
    for (i, l) in text_lines.iter().enumerate().skip(start) {
        let is_last = i == text_lines.len() - 1;
        if is_last {
            lines.push(Line::from(vec![
                Span::styled((*l).to_string(), theme::body()),
                Span::styled("▌", Style::default().fg(theme::AMBER)),
            ]));
        } else {
            lines.push(Line::from(Span::styled((*l).to_string(), theme::body())));
        }
    }
    Paragraph::new(lines).render(inner, buf);
}

/// The ctrl+o markdown preview: a large centered popup with the skill's
/// `SKILL.md` rendered through Stella's own theme-obeying markdown renderer
/// (the same one the transcript uses) and scrolled vertically. A `None` body
/// renders a loading state; the scroll offset is clamped to content here (the
/// key handler only increments it).
fn render_preview(ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    let Some(preview) = ui.skills.preview.as_mut() else {
        return;
    };
    let w = area.width.saturating_sub(4).clamp(24, 100);
    let h = area.height.saturating_sub(2).clamp(6, 32);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(rect, buf);
    let title = truncate(&preview.title, (w as usize).saturating_sub(20).max(8));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(" {title} — ↑/↓ scroll · esc close "));
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    // A dim subtitle line (url / scope), then the scrollable body below it.
    let bands = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    if !preview.subtitle.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            truncate(&preview.subtitle, inner.width as usize),
            theme::muted(),
        )))
        .render(bands[0], buf);
    }
    let body_area = bands[1];

    match preview.body.clone() {
        None => {
            Paragraph::new("fetching SKILL.md…")
                .style(theme::muted())
                .alignment(Alignment::Center)
                .render(centered_row(body_area), buf);
        }
        Some(body) => {
            // Render the markdown through Stella's own renderer so the preview
            // stays inside the ember palette — no external crate's baby-blue
            // headings, H1 background, or code-block fill. Then scroll it; the
            // offset is clamped to line count so the last page stays reachable.
            let text = Text::from(crate::markdown::render(&body));
            let content_h = text.height();
            let max_scroll = content_h.saturating_sub(body_area.height as usize) as u16;
            let scroll = preview.scroll.min(max_scroll);
            preview.scroll = scroll;
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0))
                .render(body_area, buf);
        }
    }
}

/// A centered bordered popup with `title` and `lines`.
fn popup(title: &str, lines: Vec<Line<'static>>, area: Rect, buf: &mut Buffer) {
    let w = area.width.min(60);
    let h = ((lines.len() + 2) as u16).min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(rect, buf);
    Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::accent())
                .title(title.to_string()),
        )
        .render(rect, buf);
}

/// Keep `sel` visible in a window of `visible` rows over `len` items.
fn window_start(len: usize, sel: usize, visible: usize) -> usize {
    if len <= visible {
        return 0;
    }
    sel.saturating_sub(visible.saturating_sub(1) / 2)
        .min(len - visible)
}

/// The single centered row of an inner area, for a one-line hint.
fn centered_row(inner: Rect) -> Rect {
    let y = inner.y + inner.height.saturating_sub(1) / 2;
    Rect::new(inner.x, y, inner.width, 1)
}

/// Truncate an `owner/repo@skill` id to `max` columns, preferring to keep the
/// `@skill` segment (the most identifying part) whole — the owner/repo prefix
/// gives way to an ellipsis first. Falls back to a plain tail-ellipsis when
/// even `@skill` cannot fit.
fn truncate_skill_id(id: &str, max: usize) -> String {
    if id.chars().count() <= max || max == 0 {
        return truncate(id, max);
    }
    if let Some(at) = id.rfind('@') {
        let skill = &id[at..]; // "@skill"
        let skill_w = skill.chars().count();
        // Keep the whole @skill tail plus an ellipsis, filling the rest with the
        // head of owner/repo — but only when that leaves real owner context.
        if skill_w + 2 <= max {
            let owner_room = max - skill_w - 1; // room minus the ellipsis
            let owner_head: String = id[..at].chars().take(owner_room).collect();
            return format!("{owner_head}…{skill}");
        }
    }
    truncate(id, max)
}

/// Truncate to `max` chars with a trailing ellipsis, char-safe.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck_ui::SkillPreview;
    use crate::envelope::{SkillRow, SkillScope, SkillSearchHit, SkillsView};

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

    fn row(name: &str, scope: SkillScope, enabled: bool, version: u32, latest: u32) -> SkillRow {
        SkillRow {
            scope,
            name: name.to_string(),
            description: format!("{name} does a thing"),
            body: format!("body of {name}"),
            origin: "workspace".to_string(),
            enabled,
            version,
            latest,
            removable: true,
        }
    }

    #[test]
    fn installed_pane_shows_rows_with_enabled_box_and_version() {
        let mut ui = DeckUi {
            tab: crate::deck::DeckTab::Skills,
            ..Default::default()
        };
        ui.skills.view = SkillsView {
            rows: vec![
                row("sql-style", SkillScope::Project, true, 2, 3),
                row("pdf-extract", SkillScope::User, false, 1, 1),
            ],
            status: None,
            busy: false,
        };
        let area = Rect::new(0, 0, 120, 12);
        let mut buf = Buffer::empty(area);
        render(&WorkspaceModel::new(), &mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("sql-style"), "{text}");
        assert!(text.contains("[x]"), "enabled box:\n{text}");
        assert!(text.contains("[ ]"), "disabled box:\n{text}");
        assert!(text.contains("v2/3"), "pinned-older version shown:\n{text}");
        assert!(text.contains("pdf-extract"), "{text}");
    }

    #[test]
    fn empty_installed_pane_hints_at_search() {
        let mut ui = DeckUi {
            tab: crate::deck::DeckTab::Skills,
            ..Default::default()
        };
        let area = Rect::new(0, 0, 100, 10);
        let mut buf = Buffer::empty(area);
        render(&WorkspaceModel::new(), &mut ui, area, &mut buf);
        assert!(buffer_text(&buf).contains("no skills installed"));
    }

    #[test]
    fn scope_overlay_lists_both_destinations() {
        let mut ui = DeckUi {
            tab: crate::deck::DeckTab::Skills,
            ..Default::default()
        };
        ui.skills.prompt = Some(SkillPrompt::Scope {
            action: crate::deck_ui::ScopeAction::Install {
                id: "acme/auth".into(),
            },
            user: false,
        });
        let area = Rect::new(0, 0, 90, 16);
        let mut buf = Buffer::empty(area);
        render(&WorkspaceModel::new(), &mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("Project"), "{text}");
        assert!(text.contains("User"), "{text}");
        assert!(text.contains("acme/auth"), "{text}");
    }

    fn hit(id: &str, installs: &str, rank: u64) -> SkillSearchHit {
        SkillSearchHit {
            id: id.to_string(),
            installs: installs.to_string(),
            installs_rank: rank,
            url: format!("https://skills.sh/{}", id.replace('@', "/")),
        }
    }

    #[test]
    fn search_pane_shows_clean_name_and_installs_no_ansi_leak() {
        let mut ui = DeckUi {
            tab: crate::deck::DeckTab::Skills,
            ..Default::default()
        };
        ui.skills.focus = SkillsFocus::Search;
        ui.skills.query = "rust".into();
        ui.skills.query_dirty = false;
        ui.skills.hits = vec![
            hit(
                "wshobson/agents@rust-async-patterns",
                "15.8K installs",
                15800,
            ),
            hit(
                "apollographql/skills@rust-best-practices",
                "13.9K installs",
                13900,
            ),
        ];
        // A realistic deck width; the search pane is 45% of it.
        let area = Rect::new(0, 0, 120, 14);
        let mut buf = Buffer::empty(area);
        render(&WorkspaceModel::new(), &mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        // The identifying `@skill` segment survives truncation (owner/repo gives
        // way first), and the installs metric shows.
        assert!(
            text.contains("@rust-async-patterns"),
            "skill segment shown:\n{text}"
        );
        assert!(text.contains("15.8K installs"), "installs shown:\n{text}");
        // The whole point: no raw ANSI / SGR codes leak into the rendered list.
        assert!(!text.contains("[38;5"), "no raw ANSI escapes:\n{text}");
        assert!(!text.contains("[0m"), "no raw reset codes:\n{text}");
    }

    #[test]
    fn preview_overlay_renders_markdown_heading_and_body() {
        let mut ui = DeckUi {
            tab: crate::deck::DeckTab::Skills,
            ..Default::default()
        };
        ui.skills.preview = Some(SkillPreview {
            title: "acme/auth@oauth".into(),
            subtitle: "https://skills.sh/acme/auth/oauth".into(),
            pending: None,
            body: Some("# OAuth Guide\n\nAlways use PKCE for public clients.".into()),
            scroll: 0,
        });
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        render(&WorkspaceModel::new(), &mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("acme/auth@oauth"), "title in border:\n{text}");
        assert!(text.contains("OAuth Guide"), "rendered heading:\n{text}");
        assert!(text.contains("PKCE"), "rendered body:\n{text}");
    }

    #[test]
    fn preview_overlay_shows_loading_state_when_body_absent() {
        let mut ui = DeckUi {
            tab: crate::deck::DeckTab::Skills,
            ..Default::default()
        };
        ui.skills.preview = Some(SkillPreview {
            title: "x/y@z".into(),
            subtitle: String::new(),
            pending: Some("x/y@z".into()),
            body: None,
            scroll: 0,
        });
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        render(&WorkspaceModel::new(), &mut ui, area, &mut buf);
        assert!(
            buffer_text(&buf).contains("fetching"),
            "loading state shown"
        );
    }
}
