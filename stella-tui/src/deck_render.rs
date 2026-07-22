//! The top-level deck frame: the [`ratatui_comfy_tabs`] tab bar + the active
//! view + an always-on composer + a status bar, with the splash as a full-frame
//! overlay until it finishes. This is the tab dispatcher and the one place the
//! deck's chrome is drawn.

use std::time::Duration;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, StatefulWidget, Widget};
use ratatui_comfy_tabs::{TabNav, TabNavState};

use stella_protocol::{CiStatus, PrStatus};

use crate::cache_panel;
use crate::composer::{ComposerLayout, layout as composer_layout, split_row_at};
use crate::deck::{DeckTab, PrInfo, WorkspaceModel};
use crate::deck_ui::DeckUi;
use crate::render::{render_slash_popup, scroll_window_start, slash_popup_area};
use crate::textline::{pr_status_label, stage_label};
use crate::{fx, splash, theme, views};

/// How long the deck fades in from muted after the splash hands off.
const REVEAL_MS: u32 = 350;
/// How long the amber sweep plays over the content pane on a tab change.
const TAB_SWITCH_MS: u32 = 180;

/// The gold prompt prefix on every composer row. Chrome, not content — it
/// is never part of the submitted string and the caret cannot enter it.
const PROMPT_PREFIX: &str = ">>> ";
/// Display width of [`PROMPT_PREFIX`].
const PROMPT_PREFIX_W: usize = 4;
/// One reserved column on the composer's right for the scroll indicator.
const COMPOSER_GUTTER_W: usize = 1;
/// Half-period of the caret blink, in deck-clock ms.
const CARET_BLINK_MS: u64 = 530;

pub fn render_deck(model: &WorkspaceModel, ui: &mut DeckUi, frame: &mut Frame) {
    let area = frame.area();
    let buf = frame.buffer_mut();

    // The navy-black ground is a real frame fill, not an assumption about
    // the user's terminal background — the deck looks the same over a white
    // terminal as over a black one. `degrade_buffer` narrows it per color
    // depth, and NO_COLOR strips it entirely (structure survives).
    buf.set_style(area, Style::default().bg(theme::GROUND));

    // The splash owns the whole frame until it finishes / is skipped.
    if !ui.splash.is_done() {
        splash::render(&ui.splash, area, buf);
        return;
    }

    // tab bar (comfy-tabs needs exactly 3 rows) | content | run progress bar |
    // composer | composer footer | statline. The progress bar is always present
    // (idle collapses it to a flat track). The composer grows with its
    // soft-wrapped content up to a cap, then scrolls to keep the cursor visible;
    // its text width is the frame minus the 4-column `>>> ` prefix and the
    // 1-column scroll gutter.
    let text_w = (area.width as usize).saturating_sub(PROMPT_PREFIX_W + COMPOSER_GUTTER_W);
    let c_layout = composer_layout(&ui.composer, text_w.max(1));
    let composer_h = c_layout.rows.len().clamp(1, DECK_COMPOSER_MAX_ROWS) as u16;
    // The statline grows a third row only when the focused agent has earned a
    // low-hit-rate diagnosis (#267) — the common case stays the compact
    // label-over-value pair; a session that needs the warning gets it without
    // permanently taxing every other session's content area.
    let has_diagnosis = model
        .agents
        .get(ui.focused)
        .and_then(|a| a.cache_diagnosis(cache_panel::LOW_HIT_RATE_THRESHOLD))
        .is_some();
    let statline_h = if has_diagnosis { 3 } else { 2 };
    let bands = Layout::vertical([
        Constraint::Length(3),          // tab bar
        Constraint::Min(1),             // active view
        Constraint::Length(2),          // trace micro-summary strip (rule + line)
        Constraint::Length(1),          // run progress bar
        Constraint::Length(composer_h), // composer
        Constraint::Length(1),          // composer footer (keys + line counter)
        Constraint::Length(statline_h), // statline (label over value[, diagnosis])
    ])
    .split(area);

    render_tab_bar(ui.tab, bands[0], buf);

    let content = bands[1];
    let tab = ui.tab;
    match tab {
        DeckTab::Session => views::session::render(model, ui, content, buf),
        DeckTab::Agents => views::agents::render(model, ui, content, buf),
        DeckTab::Traces => views::traces::render(model, ui, content, buf),
        DeckTab::Graph => views::graph::render(model, ui, content, buf),
        DeckTab::Files => views::files::render(model, ui, content, buf),
        DeckTab::Skills => views::skills::render(model, ui, content, buf),
        DeckTab::Mcp => views::mcp::render(model, ui, content, buf),
        DeckTab::Issues => views::issues::render(model, ui, content, buf),
        DeckTab::Settings => views::settings::render(model, ui, content, buf),
    }

    render_trace_strip(model, bands[2], buf);
    crate::progress::render(model, ui, bands[3], buf);
    render_composer(ui, &c_layout, model.now_ms, bands[4], buf);
    render_composer_footer(model, ui, &c_layout, bands[5], buf);
    render_status_bar(model, ui, bands[6], buf);

    // Floating popups sit above the chrome: the slash menu anchors to the
    // composer; the queue editor centers over the content.
    let slash = ui.composer.slash_menu(&ui.slash_commands);
    if let Some(menu) = slash.filter(|m| !m.is_empty()) {
        let selected = ui.slash_selected.min(menu.matches.len().saturating_sub(1));
        let popup = slash_popup_area(area, bands[4], menu.matches.len());
        render_slash_popup(&menu, selected, popup, buf);
    }
    if ui.queue_open {
        render_queue_popup(model, ui, area, buf);
    }
    if ui.graph_picker_open {
        render_graph_picker(ui, area, buf);
    }
    // The transcript-page overlays (SESSIONS / INBOX / CONTEXT) center over
    // the whole frame like the queue editor; help (below) still wins the top.
    if ui.sessions_open {
        render_sessions_overlay(model, ui, area, buf);
    }
    if ui.inbox_open {
        render_inbox_overlay(model, ui, area, buf);
    }
    if ui.context_open {
        render_context_overlay(ui, area, buf);
    }
    // (The former ENGINE overlay is gone: the engine panel is the full-width
    // body of the SETTINGS tab — see `views::settings::render`.)

    // Deck motion (crate::fx), scrubbed like the splash: each frame builds a
    // fresh effect and processes it once at its wall-clock elapsed, so no
    // Effect is persisted in the (Clone + Debug) ui state. Colors only —
    // content/glyphs are never moved, so render tests stay byte-stable.
    if let Some(at) = ui.tab_switched_at {
        let elapsed = at.elapsed();
        if elapsed < Duration::from_millis(u64::from(TAB_SWITCH_MS)) {
            let mut sweep = fx::tab_switch(TAB_SWITCH_MS);
            fx::apply(&mut sweep, elapsed, content, buf);
        } else {
            ui.tab_switched_at = None; // motion finished — stop rebuilding it
        }
    }
    if let Some(done_at) = ui.splash.finished_at() {
        let elapsed = done_at.elapsed();
        if elapsed < Duration::from_millis(u64::from(REVEAL_MS)) {
            let mut reveal = fx::fade_in(REVEAL_MS);
            fx::apply(&mut reveal, elapsed, area, buf);
        }
    }

    if ui.help_open {
        render_help(ui, area, buf);
    }
}

/// The comfy-tabs navigation bar.
fn render_tab_bar(tab: DeckTab, area: Rect, buf: &mut Buffer) {
    let labels: Vec<&str> = DeckTab::ALL.iter().map(|t| t.title()).collect();
    let selected = tab.index();
    let nav = TabNav::new(&labels, selected)
        .style(theme::muted())
        .highlight_style(theme::accent())
        .border_style(theme::rule());
    // Fresh state each frame: the tab set always fits, so there is no scroll to
    // keep (comfy-tabs handles any overflow itself).
    let mut state = TabNavState::new(selected);
    StatefulWidget::render(nav, area, buf, &mut state);
}

/// The queue editor popup: every waiting prompt as a navigable list, newest
/// last, with the edit/delete/clear legend (and the armed clear-all warning).
fn render_queue_popup(model: &WorkspaceModel, ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let pending = model.queue.pending();
    let w = area.width.min(64);
    let h = ((pending + 4).min(14) as u16).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(popup, buf);

    let selected = ui.queue_sel.min(pending.saturating_sub(1));
    let mut lines: Vec<Line<'static>> = Vec::new();
    if pending == 0 {
        lines.push(Line::from(Span::styled("queue is empty", theme::muted())));
    }
    // Keep the selected row in view on long queues.
    let visible_rows = (h as usize).saturating_sub(4).max(1);
    let start = selected
        .saturating_sub(visible_rows.saturating_sub(1) / 2)
        .min(pending.saturating_sub(visible_rows));
    for (i, item) in model
        .queue
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
    {
        let is_sel = i == selected;
        let marker = if is_sel { "▸ " } else { "  " };
        let mut style = theme::body();
        if is_sel {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let text: String = item
            .text
            .chars()
            .take((w as usize).saturating_sub(6))
            .collect();
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{}. ", i + 1), style.fg(theme::ACCENT)),
            Span::styled(text, style),
        ]));
    }
    lines.push(Line::default());
    lines.push(if ui.queue_confirm_clear {
        Line::from(Span::styled(
            " press ctrl+d again to clear ALL queued prompts",
            theme::body().fg(theme::WARN).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            " ↑/↓ select · enter edit · ctrl+x delete · ctrl+d ctrl+d clear · esc close",
            theme::muted(),
        ))
    });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(" queue · {pending} pending "));
    Paragraph::new(lines).block(block).render(popup, buf);
}

/// The SESSIONS overlay (empty-prompt `←`, `/sessions`): every stella
/// session on this machine from the cross-process registry, grouped by
/// status in [`crate::envelope::SessionPhase::ALL`] order, each with its
/// human title and a summary of the work involved. Selection walks the
/// flattened rows ([`crate::deck_ui::grouped_session_rows`]).
fn render_sessions_overlay(model: &WorkspaceModel, ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let w = area.width.saturating_sub(6).min(110);
    let h = area.height.saturating_sub(4).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(popup, buf);

    let rows = crate::deck_ui::grouped_session_rows(ui);
    let selected = ui.sessions_sel.min(rows.len().saturating_sub(1));
    let mut lines: Vec<Line<'static>> = Vec::new();

    if rows.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  no stella sessions registered yet",
            theme::muted(),
        )));
    }

    // Two lines per session + one heading per non-empty group; window on the
    // *selected session* so long lists keep it in view.
    let visible_sessions = ((h as usize).saturating_sub(4) / 3).max(1);
    let start = selected
        .saturating_sub(visible_sessions.saturating_sub(1) / 2)
        .min(rows.len().saturating_sub(visible_sessions));

    let mut flat_idx = 0usize;
    let mut emitted = 0usize;
    for phase in crate::envelope::SessionPhase::ALL {
        let group: Vec<_> = rows.iter().filter(|s| s.phase == phase).collect();
        if group.is_empty() {
            continue;
        }
        let mut heading_emitted = false;
        for session in group {
            let in_window = flat_idx >= start && emitted < visible_sessions;
            if in_window {
                if !heading_emitted {
                    lines.push(Line::from(Span::styled(
                        format!("  {} ({})", phase.label().to_uppercase(), {
                            rows.iter().filter(|s| s.phase == phase).count()
                        }),
                        theme::accent().add_modifier(Modifier::BOLD),
                    )));
                    heading_emitted = true;
                }
                let is_sel = flat_idx == selected;
                let marker = if is_sel { "▸ " } else { "  " };
                let dot = Span::styled("● ", Style::default().fg(phase_color(phase)));
                let mut title_style = Style::default().fg(theme::INK);
                if is_sel {
                    title_style = title_style
                        .bg(theme::SELECT_BG)
                        .add_modifier(Modifier::BOLD);
                }
                let mine = if session.mine { "  (this session)" } else { "" };
                // The ⏎ affordance rides the selected row, right where the
                // eye already is — every resumable row also carries a subtle
                // ↩ so the list is scannable for "where can I go back in".
                let tag = if session.resumable && !session.mine {
                    if is_sel { "  ↩ ⏎ resume" } else { "  ↩" }
                } else {
                    ""
                };
                let title: String = session
                    .title
                    .chars()
                    .take((w as usize).saturating_sub(24 + mine.len() + tag.chars().count()))
                    .collect();
                lines.push(Line::from(vec![
                    Span::raw(marker),
                    dot,
                    Span::styled(title, title_style),
                    Span::styled(mine.to_string(), theme::muted()),
                    Span::styled(tag.to_string(), theme::accent()),
                ]));
                let summary = if session.summary.is_empty() {
                    "(no work recorded yet)".to_string()
                } else {
                    session.summary.clone()
                };
                let detail = format!(
                    "      {} — {} · {}",
                    truncate_chars(&summary, (w as usize).saturating_sub(40)),
                    session.workspace,
                    fmt_age(model.now_ms.saturating_sub(session.updated_ms)),
                );
                lines.push(Line::from(Span::styled(
                    truncate_chars(&detail, (w as usize).saturating_sub(4)),
                    theme::muted(),
                )));
                emitted += 1;
            }
            flat_idx += 1;
        }
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " ↑/↓ select · ↵ resume/open · a archive · x delete · r refresh · esc/← close",
        theme::muted(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(" stella sessions · {} ", rows.len()));
    Paragraph::new(lines).block(block).render(popup, buf);
}

/// The color of a session-phase dot (ember palette; no pink/purple).
fn phase_color(phase: crate::envelope::SessionPhase) -> ratatui::style::Color {
    use crate::envelope::SessionPhase;
    match phase {
        SessionPhase::InProgress => theme::SUCCESS_BRIGHT,
        SessionPhase::NeedsInput => theme::WARNING_BRIGHT,
        SessionPhase::Paused => theme::ACCENT,
        SessionPhase::Cancelled => theme::TEXT_TERTIARY,
        SessionPhase::Complete => theme::SUCCESS,
        SessionPhase::Archived => theme::TEXT_TERTIARY,
        SessionPhase::Error => theme::DANGER_BRIGHT,
    }
}

/// The INBOX overlay (`/inbox`): the persist-until-read notifications,
/// newest first — unread bold with a ● dot, read dimmed with ✓, and a `↗`
/// marker on rows that link a session (⏎ marks those read AND opens the
/// session). Marking read (⏎/Space, or `R` for all) is the only way a
/// message leaves the badge.
fn render_inbox_overlay(model: &WorkspaceModel, ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let w = area.width.saturating_sub(8).min(96);
    let h = area.height.saturating_sub(6).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(popup, buf);

    let unread = ui.notifications.iter().filter(|n| !n.read).count();
    let selected = ui.inbox_sel.min(ui.notifications.len().saturating_sub(1));
    let mut lines: Vec<Line<'static>> = Vec::new();

    if ui.notifications.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  inbox zero — notifications persist here until read",
            theme::muted(),
        )));
    }

    let visible = ((h as usize).saturating_sub(4) / 2).max(1);
    let start = selected
        .saturating_sub(visible.saturating_sub(1) / 2)
        .min(ui.notifications.len().saturating_sub(visible));
    for (i, n) in ui
        .notifications
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
    {
        let is_sel = i == selected;
        let marker = if is_sel { "▸ " } else { "  " };
        let (dot, mut title_style) = if n.read {
            ("✓ ", theme::muted())
        } else {
            (
                "● ",
                Style::default().fg(theme::INK).add_modifier(Modifier::BOLD),
            )
        };
        if is_sel {
            title_style = title_style.bg(theme::SELECT_BG);
        }
        let dot_style = if n.read {
            theme::muted()
        } else {
            Style::default().fg(theme::WARNING_BRIGHT)
        };
        let mut row = vec![
            Span::raw(marker),
            Span::styled(dot, dot_style),
            Span::styled(
                truncate_chars(&n.title, (w as usize).saturating_sub(10)),
                title_style,
            ),
        ];
        if n.session_id.is_some() {
            // A subtle link marker: ⏎ on this row opens the session it is
            // about (replaying it when it is no longer live).
            row.push(Span::styled(" ↗", theme::muted()));
        }
        lines.push(Line::from(row));
        let source = if n.source.is_empty() {
            String::new()
        } else {
            format!(" · {}", n.source)
        };
        let detail = format!(
            "      {}{} · {}",
            truncate_chars(&n.body, (w as usize).saturating_sub(24)),
            source,
            fmt_age(model.now_ms.saturating_sub(n.created_ms)),
        );
        lines.push(Line::from(Span::styled(
            truncate_chars(&detail, (w as usize).saturating_sub(4)),
            theme::muted(),
        )));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " ↑/↓ select · ↵ open · ␣ mark read · R mark all read · esc close",
        theme::muted(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(" inbox · {unread} unread "));
    Paragraph::new(lines).block(block).render(popup, buf);
}

/// The CONTEXT overlay (empty-prompt `→`, `/context`): what THIS session is
/// running with — the active skills and the MCP servers — without leaving
/// the transcript. Read-only; management stays on the SKILLS/MCP tabs.
fn render_context_overlay(ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    let w = area.width.saturating_sub(8).min(96);
    let h = area.height.saturating_sub(6).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(popup, buf);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let skills = &ui.skills.view.rows;
    let enabled_skills = skills.iter().filter(|s| s.enabled).count();
    lines.push(Line::from(Span::styled(
        format!("  ACTIVE SKILLS ({enabled_skills}/{})", skills.len()),
        theme::accent().add_modifier(Modifier::BOLD),
    )));
    if skills.is_empty() {
        lines.push(Line::from(Span::styled(
            "    none installed — /skills to browse",
            theme::muted(),
        )));
    }
    for skill in skills {
        let (glyph, glyph_style) = if skill.enabled {
            ("●", Style::default().fg(theme::SUCCESS_BRIGHT))
        } else {
            ("○", theme::muted())
        };
        let desc = truncate_chars(&skill.description, (w as usize).saturating_sub(30));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(glyph, glyph_style),
            Span::raw(" "),
            Span::styled(skill.name.clone(), Style::default().fg(theme::INK)),
            Span::styled(format!("  [{}]", skill.origin), theme::muted()),
            Span::styled(format!("  {desc}"), theme::muted()),
        ]));
    }

    lines.push(Line::default());
    let servers = &ui.mcp.servers;
    let connected = servers.iter().filter(|s| s.connected).count();
    lines.push(Line::from(Span::styled(
        format!("  MCP SERVERS ({connected}/{} connected)", servers.len()),
        theme::accent().add_modifier(Modifier::BOLD),
    )));
    if servers.is_empty() {
        lines.push(Line::from(Span::styled(
            "    none configured — /mcp to search + install",
            theme::muted(),
        )));
    }
    for server in servers {
        let (glyph, glyph_style) = if server.enabled && server.connected {
            ("●", Style::default().fg(theme::SUCCESS_BRIGHT))
        } else if server.enabled {
            ("◌", Style::default().fg(theme::WARNING_BRIGHT))
        } else {
            ("○", theme::muted())
        };
        let state = if !server.enabled {
            "disabled".to_string()
        } else if server.connected {
            server.health.clone().unwrap_or_else(|| "live".to_string())
        } else {
            "not connected".to_string()
        };
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(glyph, glyph_style),
            Span::raw(" "),
            Span::styled(server.name.clone(), Style::default().fg(theme::INK)),
            Span::styled(format!("  [{}]", server.kind), theme::muted()),
            Span::styled(format!("  {state}"), theme::muted()),
            Span::styled(format!("  · {} tools", server.tool_count), theme::muted()),
        ];
        match server.oauth {
            Some(true) => spans.push(Span::styled(
                "  ⚿ oauth ✓",
                Style::default().fg(theme::SUCCESS),
            )),
            Some(false) => spans.push(Span::styled("  ⚿ no oauth login", theme::muted())),
            None => {}
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " ↑/↓ scroll · manage on the SKILLS / MCP tabs · esc/→ close",
        theme::muted(),
    )));

    // Clamp the scroll to the measured content so ↓ can't run off the end.
    let inner_h = (h as usize).saturating_sub(2);
    let max_scroll = lines.len().saturating_sub(inner_h);
    ui.context_scroll = ui.context_scroll.min(max_scroll);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(" session context ");
    Paragraph::new(lines)
        .block(block)
        .scroll((ui.context_scroll as u16, 0))
        .render(popup, buf);
}

/// Char-safe prefix truncation with an ellipsis.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// A compact "3m ago"-style age from a millisecond delta.
fn fmt_age(delta_ms: u64) -> String {
    let secs = delta_ms / 1000;
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    format!("{}d ago", hours / 24)
}

/// The Graph tab's file picker: a centered overlay listing every indexed file,
/// narrowed by a filter-as-you-type query, with the selection highlighted and
/// windowed so it stays in view on long lists (the shared
/// [`scroll_window_start`] the slash popup uses). Selecting a row re-roots the
/// neighborhood on that file; the current focus opens pre-selected as the
/// sensible default.
fn render_graph_picker(ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let Some(graph) = ui.graph.as_ref() else {
        return;
    };
    let matches = graph.matching_files(&ui.graph_picker_query);

    let w = area.width.min(64);
    let h = area.height.min(18);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(popup, buf);

    // Query line (top) + legend line (bottom) + two borders bracket the rows.
    let inner_h = (h as usize).saturating_sub(2);
    let visible_rows = inner_h.saturating_sub(2).max(1);
    let selected = ui.graph_picker_sel.min(matches.len().saturating_sub(1));
    let first = scroll_window_start(matches.len(), selected, visible_rows);
    let last = (first + visible_rows).min(matches.len());

    // The filter query, with a violet caret so the keybind/edit accent reads.
    let mut lines: Vec<Line<'static>> = vec![Line::from(vec![
        Span::styled("filter ", theme::muted()),
        Span::styled(ui.graph_picker_query.clone(), theme::body()),
        Span::styled("▏", Style::new().fg(theme::VIOLET)),
    ])];

    if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no files match — Backspace to widen",
            theme::muted(),
        )));
    }
    for (i, file) in matches.iter().enumerate().take(last).skip(first) {
        let is_sel = i == selected;
        let is_focus = *file == graph.focus;
        let marker = if is_sel { "▸ " } else { "  " };
        let mut style = theme::body();
        if is_sel {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let name = (*file)
            .chars()
            .take((w as usize).saturating_sub(6))
            .collect::<String>();
        let mut spans = vec![
            Span::styled(marker.to_string(), style.fg(theme::ACCENT)),
            Span::styled(name, style),
        ];
        // Mark the file the neighborhood is currently rooted on (the default).
        if is_focus {
            spans.push(Span::styled("  · current", theme::muted()));
        }
        lines.push(Line::from(spans));
    }

    // Pad so the legend sits on the last interior row regardless of match count.
    while lines.len() < inner_h.saturating_sub(1).max(1) {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        " type to filter · ↑/↓ select · enter open · esc close",
        theme::muted(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(" files · {} indexed ", graph.files.len()));
    Paragraph::new(lines).block(block).render(popup, buf);
}

/// The deck-wide transcript micro-summary strip: a hairline rule with, under
/// it, one dimmed line summarizing the NEWEST entry of the cross-agent trace
/// ([`WorkspaceModel::trace`]) — a glanceable "what just happened" on every
/// tab, refreshed naturally every frame. Sits directly above the composer
/// chrome (two rows: rule + summary).
fn render_trace_strip(model: &WorkspaceModel, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let rule: String = "─".repeat(area.width as usize);
    Paragraph::new(Line::from(Span::styled(rule, theme::rule())))
        .render(Rect { height: 1, ..area }, buf);
    if area.height < 2 {
        return;
    }
    let line = match model.trace.rows.back() {
        Some(row) => Line::from(vec![
            Span::styled(
                format!(" {} ", row.kind.label()),
                Style::default().fg(theme::trace_kind_color(row.kind)),
            ),
            Span::styled(
                truncate_chars(&row.summary, (area.width as usize).saturating_sub(10)),
                Style::default().fg(theme::TEXT_TERTIARY),
            ),
        ]),
        None => Line::from(Span::styled(
            " · no activity yet",
            Style::default().fg(theme::TEXT_TERTIARY),
        )),
    };
    Paragraph::new(line).render(
        Rect {
            y: area.y + 1,
            height: 1,
            ..area
        },
        buf,
    );
}

/// Cap on the deck composer's visible rows — it grows with the prompt up to
/// this, then scrolls (with a gutter indicator) to keep the cursor row in view.
const DECK_COMPOSER_MAX_ROWS: usize = 4;

/// The always-on composer — typing works from any tab. A multi-line textarea:
/// rows come pre-wrapped from [`crate::composer::layout`]; every row carries a
/// literal gold `>>> ` prefix (chrome, never part of the submitted text), and
/// an empty composer is a single `>>> ` line with the caret right after it.
/// Beyond [`DECK_COMPOSER_MAX_ROWS`] the box stops growing and scrolls, showing
/// a slim thumb in the right gutter while keeping the caret in view.
fn render_composer(
    ui: &DeckUi,
    layout: &ComposerLayout,
    now_ms: u64,
    area: Rect,
    buf: &mut Buffer,
) {
    let visible = (area.height as usize).max(1);
    let total = layout.rows.len();
    // Scroll so the caret's row is always within the visible window.
    let first = if layout.cursor_row < visible {
        0
    } else {
        layout.cursor_row + 1 - visible
    };

    // A gentle caret blink, coalesced into the deck's one render tick (a pure
    // function of the clock — no timer). `--no-anim` pins it solid.
    let caret_on = ui.no_anim || (now_ms / CARET_BLINK_MS).is_multiple_of(2);
    let cursor_style = theme::accent().add_modifier(Modifier::REVERSED);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, row) in layout.rows.iter().enumerate().skip(first).take(visible) {
        // The gold `>>> ` prefix rides every row and scrolls with it.
        let mut spans = vec![Span::styled(
            PROMPT_PREFIX,
            Style::default().fg(theme::AURORA_CYAN),
        )];
        if i == layout.cursor_row {
            let (before, under, after) = split_row_at(row, layout.cursor_col);
            let under_ch = under.map(String::from).unwrap_or_else(|| " ".into());
            spans.push(Span::styled(before, theme::body()));
            spans.push(Span::styled(
                under_ch,
                if caret_on {
                    cursor_style
                } else {
                    theme::body()
                },
            ));
            spans.push(Span::styled(after, theme::body()));
        } else {
            spans.push(Span::styled(row.clone(), theme::body()));
        }
        lines.push(Line::from(spans));
    }

    // Reserve the last column for the scroll gutter so text never collides
    // with the indicator.
    let text_area = Rect {
        width: area.width.saturating_sub(COMPOSER_GUTTER_W as u16),
        ..area
    };
    Paragraph::new(lines).render(text_area, buf);

    if total > visible {
        render_scroll_gutter(first, visible, total, area, buf);
    }
}

/// A slim scrollbar in the composer's right gutter: a dim track with a violet
/// thumb sized/positioned to the visible window over `total` rows.
fn render_scroll_gutter(first: usize, visible: usize, total: usize, area: Rect, buf: &mut Buffer) {
    let h = area.height as usize;
    if h == 0 || total <= visible {
        return;
    }
    let gx = area.x + area.width.saturating_sub(1);
    // Thumb height proportional to the visible fraction (≥ 1 row).
    let thumb_h = ((visible * h) / total).max(1).min(h);
    let max_off = total.saturating_sub(visible);
    let thumb_top = (first * (h - thumb_h)).checked_div(max_off).unwrap_or(0);
    for i in 0..h {
        if let Some(cell) = buf.cell_mut((gx, area.y + i as u16)) {
            let on = i >= thumb_top && i < thumb_top + thumb_h;
            cell.set_symbol(if on { "▐" } else { "│" });
            cell.set_fg(if on { theme::VIOLET } else { theme::HAIRLINE });
        }
    }
}

/// Total display width of a span run (chars ≈ terminal cells for our glyphs).
fn span_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Render `spans` right-aligned at the end of `area` without clearing the rest
/// of the row — draws only its own sub-rect, so a left-aligned line drawn first
/// survives everywhere it doesn't overlap.
fn render_right(spans: Vec<Span<'static>>, area: Rect, buf: &mut Buffer) {
    let w = span_width(&spans).min(area.width as usize) as u16;
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w),
        y: area.y,
        width: w,
        height: 1,
    };
    Paragraph::new(Line::from(spans)).render(rect, buf);
}

/// The quiet keybind + line-counter row directly under the composer and above
/// the statline. Keybind glyphs are violet; the right end carries the
/// live line counter and the queue status.
fn render_composer_footer(
    model: &WorkspaceModel,
    ui: &DeckUi,
    _layout: &ComposerLayout,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.height == 0 {
        return;
    }
    let key = Style::default().fg(theme::VIOLET);
    let dim = Style::default().fg(theme::TEXT_TERTIARY);
    let sep = Style::default().fg(theme::HAIRLINE);

    // Right: live logical-line counter · queue status. Built first so its width
    // is known and the left affordances can be clipped to what remains.
    let n_lines = ui.composer.buffer().split('\n').count().max(1);
    let counter = format!("{n_lines} line{}", if n_lines == 1 { "" } else { "s" });
    let pending = model.queue.pending();
    let (q_text, q_style) = if pending > 0 && ui.dispatch_held {
        (
            format!("{pending} held"),
            Style::default().fg(theme::AURORA_MAGENTA),
        )
    } else if pending > 0 {
        (
            format!("{pending} queued"),
            Style::default().fg(theme::WARNING_BRIGHT),
        )
    } else {
        ("queue empty".to_string(), dim)
    };
    let right = vec![
        Span::styled(counter, dim),
        Span::styled("  ·  ", sep),
        Span::styled(q_text, q_style),
        Span::raw(" "),
    ];
    let right_w = span_width(&right) as u16;

    // Advertise the newline chord the terminal can actually report: `⌘⏎`/`⌃⏎`
    // where the kitty protocol is live, else the universally-safe `⌥⏎`. Drop
    // the lower-value affordances first on a narrow row so nothing collides
    // with the counter.
    let newline = if ui.enter_submits { "⌥⏎" } else { "⌘⏎" };
    let mut left = vec![
        Span::raw(" "),
        Span::styled(newline, key),
        Span::styled(" new line", dim),
        Span::styled("  ·  ", sep),
        Span::styled("⏎", key),
        Span::styled(" queue (never blocks)", dim),
    ];
    let extras = [("!", " shell"), ("/", " commands")];
    let left_budget = (area.width.saturating_sub(right_w + 1)) as usize;
    for (glyph, word) in extras {
        let add = 5 + glyph.chars().count() + word.chars().count(); // "  ·  " + glyph + word
        if span_width(&left) + add <= left_budget {
            left.push(Span::styled("  ·  ", sep));
            left.push(Span::styled(glyph, key));
            left.push(Span::styled(word, dim));
        }
    }
    let left_area = Rect {
        width: area.width.saturating_sub(right_w),
        ..area
    };
    Paragraph::new(Line::from(left)).render(left_area, buf);
    render_right(right, area, buf);
}

/// The statline: labeled cells (dim micro-label over bright value) separated by
/// hairlines, with the brand pinned left and the ethos chip pinned right.
/// Two rows tall; the context/token meter is kept prominent.
///
/// `pub(crate)`: the cache-panel integration tests in
/// [`crate::cache_panel`] render a full statline to assert on the CACHE /
/// SAVED / WARMTH cells, so they need to call this from outside the file
/// (kept out of `deck_render.rs`'s own test module to respect its size
/// ratchet — see that module's doc comment).
pub(crate) fn render_status_bar(model: &WorkspaceModel, ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let top_y = area.y;
    let bot_y = area.y + (area.height.saturating_sub(1)).min(1);

    let dim = Style::default().fg(theme::TEXT_TERTIARY);
    let val = Style::default().fg(theme::TEXT_PRIMARY);
    let sep = Style::default().fg(theme::HAIRLINE);

    // ── the cells, left → right (brand + ethos are pinned separately) ──────
    let cpu = f64::from(model.global_cpu_pct);
    let focused = model.agents.get(ui.focused);
    // Current window occupancy = the latest call's prompt size, not the
    // session's cumulative input (which dwarfs the window after a few turns).
    let ctx_tokens = focused.map_or(0, |a| a.context_tokens);
    const CTX_WINDOW: u64 = 200_000;
    let ctx_frac = (ctx_tokens as f64 / CTX_WINDOW as f64).min(1.0);

    // STAGE with its pulsing ember dot (truecolor only — a lightened RGB has
    // no indexed fallback, so lesser terminals get a steady flame dot).
    let stage_txt = focused
        .and_then(|a| a.model.hud.stage)
        .map(stage_label)
        .unwrap_or("idle");
    let dot_color = if ui.color_mode.is_truecolor() && !ui.no_anim {
        let t = (model.now_ms % 1200) as f64 / 1200.0;
        theme::lighten(theme::AURORA_AZURE, (0.5 - (t - 0.5).abs()) * 0.7)
    } else {
        theme::AURORA_AZURE
    };

    // Cache economics panel (#267/#269) — CACHE hit%/volumes, SAVED dollars,
    // WARMTH countdown; the pricing/TTL math already happened in the producer.
    let cache_total = model.total_input_tokens();
    let cache_spans = cache_panel::cache_cell(
        model.cache_hit_tokens(),
        model.total_cache_write_tokens(),
        cache_total,
    );
    let saved_spans = cache_panel::saved_cell(model.total_cache_savings_usd(), cache_total > 0);
    let warmth_spans =
        cache_panel::warmth_cell(focused.and_then(|a| a.cache_warmth_secs(model.now_ms)));

    // PIPELINE: ON when the session drives the staged pipeline, OFF for the
    // raw engine loop (`model.pipeline`).
    let (pipeline_txt, pipeline_style) = if model.pipeline {
        ("ON", Style::default().fg(theme::SUCCESS_BRIGHT))
    } else {
        ("OFF", dim)
    };

    // (label, value, priority) in SVG order. Higher priority survives a narrow
    // row longer; STAGE and CONTEXT are must-keep (`MUST_KEEP`) because the
    // stage and the token meter are the load-bearing cells.
    const MUST_KEEP: u8 = 9;
    let mut cells: Vec<(&str, Vec<Span<'static>>, u8)> = vec![
        (
            "AGENT",
            vec![Span::styled(
                focused
                    .map(|a| a.meta.id.clone())
                    .unwrap_or_else(|| "—".into()),
                val,
            )],
            5,
        ),
        (
            "STAGE",
            vec![
                Span::styled("● ", Style::default().fg(dot_color)),
                Span::styled(stage_txt.to_string(), val),
            ],
            MUST_KEEP,
        ),
        (
            "MODEL",
            vec![Span::styled(
                model.latest_model().unwrap_or("—").to_string(),
                val,
            )],
            6,
        ),
        (
            "CPU",
            vec![
                Span::styled(
                    meter_bar(cpu / 100.0),
                    Style::default().fg(theme::gauge_color(cpu / 100.0)),
                ),
                Span::styled(format!(" {cpu:>3.0}%"), val),
            ],
            5,
        ),
        (
            "CONTEXT",
            vec![
                Span::styled(
                    meter_bar(ctx_frac),
                    Style::default().fg(theme::gauge_color(ctx_frac)),
                ),
                Span::styled(format!(" {}/{}", fmt_k(ctx_tokens), fmt_k(CTX_WINDOW)), val),
            ],
            MUST_KEEP,
        ),
        (
            "SPEND",
            vec![Span::styled(
                format!("${:.2}", model.total_cost()),
                Style::default().fg(theme::SUCCESS_BRIGHT),
            )],
            6,
        ),
        ("CACHE", cache_spans, 4),
        ("SAVED", saved_spans, 3),
        ("WARMTH", warmth_spans, 3),
        (
            "ENGINE",
            vec![Span::styled(
                format!("{} active", model.active_count()),
                val,
            )],
            4,
        ),
        (
            "PIPELINE",
            vec![Span::styled(pipeline_txt, pipeline_style)],
            3,
        ),
    ];
    // PR: only once a Pr event has been observed. Failing CI raises the drop
    // priority the same way an unread INBOX badge does — a red ✗ must survive
    // a narrow row.
    if let Some(pr) = &model.pr {
        let priority = if pr.ci == Some(CiStatus::Failing) {
            8
        } else {
            5
        };
        cells.push(("PR", pr_cell(pr), priority));
    }
    cells.push((
        "INBOX",
        {
            // Persist-until-read notifications: the badge is the always-on
            // surface; `/inbox` opens the overlay that clears it.
            let unread = ui.notifications.iter().filter(|n| !n.read).count();
            if unread == 0 {
                vec![Span::styled("—", dim)]
            } else {
                vec![Span::styled(
                    format!("✉ {unread}"),
                    Style::default()
                        .fg(theme::WARNING_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                )]
            }
        },
        if ui.notifications.iter().any(|n| !n.read) {
            8
        } else {
            2
        },
    ));

    let brand = " ✦ stella ";
    let brand_w = brand.chars().count();
    // The ethos chip is pinned right — pure chrome, so it is dropped *first*
    // when the row is too narrow (before any data cell).
    let ethos = "↝ deterministic-first ";
    let ethos_w = ethos.chars().count();
    let cell_need: Vec<usize> = cells
        .iter()
        .map(|(l, v, _)| 3 + span_width(v).max(l.chars().count()))
        .collect();

    // Fit: drop the ethos chip, then the lowest-priority non-must-keep cell,
    // until the row fits (or only must-keep cells remain — then accept a clip).
    let mut kept = vec![true; cells.len()];
    let mut ethos_on = true;
    loop {
        let cells_w: usize = (0..cells.len())
            .filter(|&i| kept[i])
            .map(|i| cell_need[i])
            .sum();
        let need = brand_w + cells_w + if ethos_on { ethos_w } else { 0 };
        if need <= area.width as usize {
            break;
        }
        if ethos_on {
            ethos_on = false;
            continue;
        }
        match (0..cells.len())
            .filter(|&i| kept[i] && cells[i].2 < MUST_KEEP)
            .min_by_key(|&i| cells[i].2)
        {
            Some(i) => kept[i] = false,
            None => break,
        }
    }

    let mut top: Vec<Span<'static>> = vec![Span::raw(" ".repeat(brand_w))];
    let mut bot: Vec<Span<'static>> = vec![Span::styled(
        brand,
        Style::default()
            .fg(theme::AURORA_CYAN)
            .add_modifier(Modifier::BOLD),
    )];
    for (i, (label, value, _)) in cells.into_iter().enumerate() {
        if !kept[i] {
            continue;
        }
        let vw = span_width(&value);
        let cw = vw.max(label.chars().count());
        top.push(Span::styled(" │ ", sep));
        bot.push(Span::styled(" │ ", sep));
        top.push(Span::styled(format!("{label:<cw$}"), dim));
        // Value, right-padded into the same column width so labels align.
        let pad = cw.saturating_sub(vw);
        bot.extend(value);
        if pad > 0 {
            bot.push(Span::raw(" ".repeat(pad)));
        }
    }

    Paragraph::new(Line::from(top)).render(
        Rect {
            x: area.x,
            y: top_y,
            width: area.width,
            height: 1,
        },
        buf,
    );
    Paragraph::new(Line::from(bot)).render(
        Rect {
            x: area.x,
            y: bot_y,
            width: area.width,
            height: 1,
        },
        buf,
    );
    if ethos_on {
        render_right(
            vec![Span::styled(
                ethos,
                Style::default()
                    .fg(theme::VIOLET)
                    .add_modifier(Modifier::BOLD),
            )],
            Rect {
                x: area.x,
                y: bot_y,
                width: area.width,
                height: 1,
            },
            buf,
        );
    }

    // Third row: the low-hit-rate diagnosis, full-sentence and byte-identical
    // to `stella stats`'s wording — only present when `render_deck` reserved
    // the extra row (`AgentEntry::cache_diagnosis` fired for the focused
    // agent) AND the area actually has it (a caller that hands this function
    // a bare 2-row area, as every pre-#267 snapshot fixture still does, gets
    // no diagnosis row rather than a clipped one).
    if area.height >= 3
        && let Some(cause) =
            focused.and_then(|a| a.cache_diagnosis(cache_panel::LOW_HIT_RATE_THRESHOLD))
    {
        Paragraph::new(Line::from(cache_panel::diagnosis_spans(cause))).render(
            Rect {
                x: area.x,
                y: top_y + 2,
                width: area.width,
                height: 1,
            },
            buf,
        );
    }
}

/// A compact 6-cell utilization meter for a `[0, 1]` fraction.
fn meter_bar(frac: f64) -> String {
    const CELLS: usize = 6;
    let filled = (frac.clamp(0.0, 1.0) * CELLS as f64).round() as usize;
    (0..CELLS)
        .map(|i| if i < filled { '▮' } else { '▯' })
        .collect()
}

/// The PR statline cell's spans: `⇢ #183 open` (or the URL tail when the
/// monitor parsed no number) colored by PR status, plus a CI glyph once a
/// verdict has been observed — `✓` passing, `✗` failing (bold), `◌` pending /
/// `…` running (dim).
fn pr_cell(pr: &PrInfo) -> Vec<Span<'static>> {
    let status_style = Style::default().fg(pr_status_color(pr.status));
    let ident = match pr.number {
        Some(n) => format!("⇢ #{n}"),
        // No parsed number — the URL tail still identifies the PR.
        None => format!(
            "⇢ {}",
            pr.url.rsplit('/').find(|s| !s.is_empty()).unwrap_or("pr")
        ),
    };
    let mut spans = vec![
        Span::styled(ident, status_style.add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {}", pr_status_label(pr.status)), status_style),
    ];
    if let Some(ci) = pr.ci {
        let (glyph, style) = match ci {
            CiStatus::Passing => ("✓", Style::default().fg(theme::OK)),
            CiStatus::Failing => (
                "✗",
                Style::default().fg(theme::BAD).add_modifier(Modifier::BOLD),
            ),
            CiStatus::Pending => ("◌", Style::default().fg(theme::TEXT_TERTIARY)),
            CiStatus::Running => ("…", Style::default().fg(theme::TEXT_TERTIARY)),
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(glyph, style));
    }
    spans
}

/// The transcript's PR aurora ramp — `render`'s `pr_status_color` is private
/// to that module, so the statline replicates it: quiet amber draft (the one
/// semantic-warning exception), azure while open, cyan on merge, magenta on
/// close.
fn pr_status_color(status: PrStatus) -> ratatui::style::Color {
    match status {
        PrStatus::Draft => theme::WARNING,
        PrStatus::Open => theme::AURORA_AZURE,
        PrStatus::Merged => theme::AURORA_CYAN,
        PrStatus::Closed => theme::AURORA_MAGENTA,
    }
}

/// Format a token count compactly: `42k`, `1.2k`, `950`.
fn fmt_k(n: u64) -> String {
    if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Format a token count with uppercase scale suffixes and one decimal:
/// `105.3M`, `211.4K`, `950` — the CACHE-cell convention. `fmt_k` (the context
/// meter) caps at `k`; cumulative cache counts reach the millions, so this
/// carries an `M` tier, matching the requested `67% (105.3M/211.4M tokens)`.
pub(crate) fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// One aligned `key → description` row of the help overlay. The key column is
/// padded to a fixed width so the descriptions line up into a scannable
/// second column.
fn help_row(key: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {key:<13} "), theme::accent()),
        Span::styled(desc.to_string(), theme::body()),
    ])
}

/// The shortcuts specific to one deck tab, as `(key, description)` pairs.
/// Keyed off [`DeckTab`] so the overlay only ever shows keys that work where
/// the user actually is — the per-tab handlers in `deck_ui` are the behavior
/// these rows must mirror.
fn tab_shortcuts(tab: DeckTab) -> &'static [(&'static str, &'static str)] {
    match tab {
        DeckTab::Session => &[
            ("↑ ↓", "select a message · esc clears the selection"),
            ("⇞ ⇟", "scroll the transcript"),
            ("⌘[ / ⌘]", "jump to transcript start / end (⌃ works too)"),
            ("ctrl-o", "expand/collapse the selected message (none: all)"),
            ("ctrl-r", "expand/collapse all thinking"),
            ("↑", "with prompts queued: open the queue editor"),
            ("←", "SESSIONS overlay — every session on this machine"),
            ("→", "CONTEXT overlay — active skills + MCP servers"),
        ],
        DeckTab::Agents => &[
            ("← →", "switch panes — executions / installed"),
            ("↑ ↓", "select an agent"),
            ("s", "stop the selected running agent"),
            ("⏎", "edit the selected installed agent"),
            ("v", "show the selected agent's versions"),
            ("n", "new agent — drafted by the LLM"),
            ("r", "reload installed agents"),
        ],
        DeckTab::Traces => &[
            ("↑ ↓ ⇞ ⇟", "scroll the event log"),
            ("f", "cycle the per-agent filter"),
        ],
        DeckTab::Graph => &[
            ("← → ↑ ↓", "walk the neighborhood"),
            ("/ or ⏎", "file picker — re-root on any indexed file"),
        ],
        DeckTab::Files => &[("↑ ↓", "select a file"), ("⏎", "open / close the diff")],
        DeckTab::Skills => &[
            ("← →", "switch panes"),
            ("↑ ↓", "select a skill"),
            ("space", "enable / disable"),
            ("e", "edit the selected skill"),
            ("p", "pin / unpin"),
            ("n", "new skill — drafted by the LLM"),
            ("ctrl-o", "preview"),
            ("ctrl-x ×2", "delete (press twice to confirm)"),
            ("type", "search skills"),
        ],
        DeckTab::Mcp => &[
            ("↑ ↓", "select a server"),
            ("space / e", "enable / disable"),
            ("a", "authenticate (env credentials)"),
            ("o", "OAuth login (http servers)"),
            ("s", "search the registry"),
            ("x", "remove the server"),
            ("r", "refresh"),
        ],
        DeckTab::Issues => &[
            ("↑ ↓", "select an issue"),
            ("r", "refresh the list"),
            ("/", "search the tracker"),
            ("n", "new issue — tab cycles fields · ctrl-s creates"),
            ("c", "comment on the selected issue"),
            ("s", "set the selected issue's status"),
            ("w", "start work on the selected issue"),
        ],
        DeckTab::Settings => &[
            ("e", "edit the agents config — models, prompts & params"),
            (
                "tab",
                "in the editor: switch agent — global / default / worker / …",
            ),
            ("⏎", "in the editor: edit the selected row / pick a model"),
            ("space", "in the editor: toggle the selected row"),
            ("x", "in the editor: clear the selected row"),
            ("s / S", "in the editor: save to user / project settings"),
            ("r", "in the editor: reload from disk"),
            ("esc", "in the editor: hand the keyboard back to the tab"),
        ],
    }
}

/// Deck-wide shortcuts that work on every tab.
const GLOBAL_SHORTCUTS: &[(&str, &str)] = &[
    ("tab / ⇧tab", "switch tabs"),
    ("⌘⏎ / ⌃⏎", "queue the prompt — never blocks a running turn"),
    ("⏎", "insert a line break in the prompt"),
    ("!cmd", "run a shell command NOW (skips the queue)"),
    ("/", "slash commands — ↑↓ pick · tab completes · ⏎ runs"),
    ("ctrl-v", "paste — a copied image is attached to the prompt"),
    ("ctrl-t", "open the queue editor"),
    (
        ">text",
        "steer the running turn — lands at the next step boundary",
    ),
    (
        "esc",
        "soft-stop at the next step boundary — completed work kept",
    ),
    (
        "esc esc",
        "cancel NOW & hold — nothing runs until your next prompt",
    ),
    ("ctrl-c", "quit stella"),
];

/// The help overlay: the active tab's keys first, then the deck-wide keys —
/// one shortcut per line, key column aligned. Context-aware on purpose: only
/// shortcuts that work on the tab the user is looking at are shown, so the
/// overlay stays short enough to read at a glance. Opened by `?` (empty
/// composer) or `/help`; scrolls with ↑/↓/⇞/⇟/Home/End on a short terminal;
/// closes with esc/`q`/`?`.
fn render_help(ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("  {} tab", ui.tab.title()),
        theme::heading(),
    )));
    for (key, desc) in tab_shortcuts(ui.tab) {
        lines.push(help_row(key, desc));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("  everywhere", theme::heading())));
    for (key, desc) in GLOBAL_SHORTCUTS {
        lines.push(help_row(key, desc));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  letter & arrow hotkeys apply while the prompt box is empty",
        theme::muted(),
    )));

    // Size the panel to its content, capped to the frame.
    let w = area.width.min(68);
    let h = area.height.min(lines.len() as u16 + 2);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(popup, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(" help — {} · esc close ", ui.tab.title()));
    let inner = block.inner(popup);
    block.render(popup, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let total = lines.len();
    let height = inner.height as usize;
    // Record viewport metrics for the pure key handler (`handle_help_key`) —
    // when the panel is clipped, ↑/↓/⇞/⇟/Home/End scroll it.
    ui.metrics.help_total = total;
    ui.metrics.help_height = height;
    let window = ui.help_scroll.window(total, height);
    Paragraph::new(lines)
        .scroll((window.start as u16, 0))
        .render(inner, buf);
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::envelope::{AgentMeta, Inbound};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use stella_protocol::{AgentEvent, StageKind};

    /// End-to-end: render the whole deck frame (tab bar · view · progress bar ·
    /// composer · footer · statline) and assert every §-level element is
    /// present, the rocket/garble is gone, and nothing panics at 80 columns.
    /// Run with `--nocapture` to eyeball the frame.
    #[test]
    fn full_deck_frame_composes_every_band_at_80_cols() {
        let mut model = running_model_with_queue();
        if let Some(a) = model.agents.first_mut() {
            a.tokens_out = 900;
            a.tokens_in = 42_000;
            a.cost_usd = 0.14;
            a.meta.started_ms = 0;
        }
        model.now_ms = 10_000;
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Text {
                delta: "Found the root cause.".into(),
            },
        });

        let mut ui = DeckUi::default();
        ui.splash.skip(); // the splash owns the frame until done
        for c in "add nav".chars() {
            ui.composer.insert_char(c);
        }

        // 190 (was 160, before that 120) so the now-wider statline (CACHE +
        // PIPELINE, then SAVED + WARMTH for #267/#269) still has room for the
        // ethos chip, which is dropped first.
        for (w, h) in [(80u16, 24u16), (190, 40)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| render_deck(&model, &mut ui, f)).unwrap();
            let text = buffer_text(term.backend().buffer());
            if w == 80 {
                eprintln!("\n──── deck @ {w}×{h} ────\n{text}\n");
            }
            for needle in [
                ">>>",      // gold prompt prefix (§4)
                "add nav",  // typed prompt text
                "new line", // composer footer affordance (§4)
                "queue",    // footer / queue status
                "plan",     // progress stage labels (§3)
                "execute", "verify", "stella",  // statline brand (§5)
                "CONTEXT", // load-bearing token meter, kept at every width (§5)
            ] {
                assert!(
                    text.contains(needle),
                    "deck @{w}×{h} missing {needle:?}:\n{text}"
                );
            }
            // The ethos chip is chrome — it only needs to appear once the row is
            // wide enough (it is the first thing dropped on a narrow statline).
            if w >= 120 {
                assert!(
                    text.contains("deterministic-first"),
                    "deck @{w}×{h} missing ethos chip:\n{text}"
                );
            }
            // The killed rocket/garble leave no trace.
            assert!(
                !text.contains("}=>"),
                "rocket sprite still rendered:\n{text}"
            );
            assert!(!text.contains("<●>"), "UFO sprite still rendered:\n{text}");
        }
    }

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

    #[test]
    fn full_deck_frame_grows_a_third_statline_row_for_a_diagnosed_agent() {
        // The acceptance case: an opt-in provider (anthropic), 4 calls (past
        // MIN_TURNS=3), 0% hit rate, 0 cache writes — the marker never
        // engaged. `render_deck` must reserve the statline's third row and
        // `render_status_bar` must fill it with the full-sentence hint, not a
        // clipped fragment.
        let mut model = running_model_with_queue();
        for step in 1..=4usize {
            model.apply_inbound(&Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::StepUsage {
                    step,
                    model: "claude-fable-5".into(),
                    input_tokens: 10_000,
                    output_tokens: 500,
                    cached_input_tokens: 0,
                    cache_write_tokens: 0,
                    estimated_input_tokens: 0,
                    cost_usd: 0.05,
                    duration_ms: 100,
                    retries: 0,
                    tool_calls: 0,
                },
            });
        }
        model.apply_inbound(&Inbound::CacheInsight {
            agent: "lead".into(),
            savings_usd_delta: 0.0,
            ttl_secs: 300,
            is_opt_in_provider: true,
        });

        let mut ui = DeckUi::default();
        ui.splash.skip();
        let mut term = Terminal::new(TestBackend::new(190, 40)).unwrap();
        term.draw(|f| render_deck(&model, &mut ui, f)).unwrap();
        let text = buffer_text(term.backend().buffer());
        assert!(
            text.contains("cache opt-in never engaged"),
            "diagnosis row missing from the full frame:\n{text}"
        );

        // A healthy session (no StepUsage at all) never grows the row or
        // shows the sentence — the common case stays the compact two rows.
        let healthy = running_model_with_queue();
        let mut healthy_ui = DeckUi::default();
        healthy_ui.splash.skip();
        let mut healthy_term = Terminal::new(TestBackend::new(190, 40)).unwrap();
        healthy_term
            .draw(|f| render_deck(&healthy, &mut healthy_ui, f))
            .unwrap();
        let healthy_text = buffer_text(healthy_term.backend().buffer());
        assert!(
            !healthy_text.contains("cache opt-in never engaged")
                && !healthy_text.contains("prompt prefix is unstable"),
            "a healthy session must not show a diagnosis:\n{healthy_text}"
        );
    }

    #[test]
    fn trace_strip_shows_a_rule_and_the_newest_trace_entry() {
        let mut model = running_model_with_queue();
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Text {
                delta: "Found the root cause.".into(),
            },
        });
        let area = Rect::new(0, 0, 60, 2);
        let mut buf = Buffer::empty(area);
        render_trace_strip(&model, area, &mut buf);
        let text = buffer_text(&buf);
        let rows: Vec<&str> = text.lines().collect();
        assert!(
            rows[0].starts_with("──"),
            "hairline rule on top: {:?}",
            rows[0]
        );
        assert!(rows[1].contains("text"), "kind label shown:\n{text}");
        assert!(
            rows[1].contains("Found the root cause."),
            "newest entry summarized:\n{text}"
        );

        // No trace yet → a quiet idle line, never a panic.
        let empty = WorkspaceModel::new();
        let mut buf = Buffer::empty(area);
        render_trace_strip(&empty, area, &mut buf);
        assert!(buffer_text(&buf).contains("no activity yet"));
    }

    fn running_model_with_queue() -> WorkspaceModel {
        let mut m = WorkspaceModel::new();
        m.now_ms = 1_000;
        m.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "goal", 0)));
        m.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Stage {
                name: StageKind::Execute,
            },
        });
        m.queue.enqueue("write the tests".into(), 1);
        m.queue.enqueue("open a pr".into(), 2);
        m
    }

    #[test]
    fn empty_composer_is_a_single_gold_prompt_line_with_the_caret() {
        let ui = DeckUi::default(); // blank composer
        let layout = crate::composer::layout(&ui.composer, 40);
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        render_composer(&ui, &layout, 0, area, &mut buf);
        let text = buffer_text(&buf);
        let rows: Vec<&str> = text.lines().collect();
        assert!(
            rows[0].starts_with(">>> "),
            "row 0 is the gold prompt: {:?}",
            rows[0]
        );
        // Exactly one prompt line — the rest of the box is empty.
        assert!(
            rows[1..].iter().all(|r| !r.contains(">>>")),
            "only one prompt line:\n{text}"
        );
        // The caret sits right after the prefix (a reversed cell at col 4).
        assert!(
            buf.cell((4, 0))
                .is_some_and(|c| c.modifier.contains(Modifier::REVERSED)),
            "caret right after the prefix"
        );
    }

    #[test]
    fn a_multiline_paste_prefixes_every_row_and_scrolls_instead_of_chipping() {
        // 8 lines — well past the old 6-line chip threshold, but the deck's
        // composer keeps it inline (one `>>>` per line) and scrolls, rather
        // than collapsing it to a `[pasted: N lines]` chip (acceptance §4/#5).
        let mut ui = DeckUi::default();
        let paste: String = (1..=8).map(|n| format!("line{n}\n")).collect();
        ui.composer.paste(&paste);
        assert!(
            ui.composer.chips().is_empty(),
            "no chip — rendered per line"
        );

        let layout = crate::composer::layout(&ui.composer, 40);
        let area = Rect::new(0, 0, 40, 4); // capped at DECK_COMPOSER_MAX_ROWS
        let mut buf = Buffer::empty(area);
        render_composer(&ui, &layout, 0, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(!text.contains("[pasted"), "not chipped:\n{text}");
        for (i, row) in text.lines().enumerate() {
            assert!(row.starts_with(">>> "), "row {i} prefixed: {row:?}");
        }
        // Beyond 4 rows the box scrolls — the gutter shows a violet thumb.
        assert!(
            (0..area.height).any(|yy| buf
                .cell((area.width - 1, yy))
                .is_some_and(|c| c.symbol() == "▐")),
            "scroll indicator present:\n{text}"
        );
    }

    #[test]
    fn composer_footer_shows_keys_line_counter_and_queue_status() {
        let model = running_model_with_queue();
        let ui = DeckUi::default();
        let layout = crate::composer::layout(&ui.composer, 40);
        let area = Rect::new(0, 0, 100, 1);
        let mut buf = Buffer::empty(area);
        render_composer_footer(&model, &ui, &layout, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("new line"), "newline affordance:\n{text}");
        assert!(
            text.contains("queue (never blocks)"),
            "queue affordance:\n{text}"
        );
        assert!(text.contains("1 line"), "live line counter:\n{text}");
        assert!(
            text.contains("2 queued"),
            "queue status on the right:\n{text}"
        );
    }

    #[test]
    fn composer_footer_reports_a_held_queue() {
        let model = running_model_with_queue();
        let mut ui = DeckUi::default();
        ui.dispatch_held = true;
        let layout = crate::composer::layout(&ui.composer, 40);
        let area = Rect::new(0, 0, 100, 1);
        let mut buf = Buffer::empty(area);
        render_composer_footer(&model, &ui, &layout, area, &mut buf);
        assert!(buffer_text(&buf).contains("2 held"), "held status shown");
    }

    #[test]
    fn statline_shows_labeled_cells_and_the_ethos_chip() {
        // A wide terminal fits every cell plus the (chrome) ethos chip. 160
        // (was 120) leaves room for the CACHE + PIPELINE cells added to the row.
        let model = running_model_with_queue();
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 160, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        for needle in [
            "stella",
            "AGENT",
            "lead",
            "STAGE",
            "execute",
            "MODEL",
            "CPU",
            "CONTEXT",
            "SPEND",
            "CACHE",
            "ENGINE",
            "PIPELINE",
            "deterministic-first",
        ] {
            assert!(
                text.contains(needle),
                "statline missing {needle:?}:\n{text}"
            );
        }
    }

    #[test]
    fn statline_keeps_the_context_meter_on_a_narrow_terminal() {
        // At 80 columns the row must still render without panicking and keep
        // the brand, the stage, and the load-bearing context meter — the ethos
        // chip (pure chrome) is what gives way, not the data.
        let model = running_model_with_queue();
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 80, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("stella"), "brand kept:\n{text}");
        assert!(text.contains("STAGE"), "stage kept:\n{text}");
        assert!(text.contains("CONTEXT"), "context meter kept:\n{text}");
    }

    /// One committed model call, carrying `input`/`cached` usage — the fold
    /// that feeds the CACHE cell.
    fn step_usage(input: u64, cached: u64) -> AgentEvent {
        AgentEvent::StepUsage {
            step: 1,
            role: stella_protocol::ModelCallRole::Worker,
            provider: "test".into(),
            model: "glm".into(),
            input_tokens: input,
            output_tokens: 0,
            cached_input_tokens: cached,
            cache_write_tokens: 0,
            estimated_input_tokens: 0,
            cost_usd: 0.0,
            duration_ms: 1,
            retries: 0,
            tool_calls: 0,
            complete: true,
        }
    }

    /// The running model plus one metered step with the given cache usage.
    fn model_with_cache(input: u64, cached: u64) -> WorkspaceModel {
        let mut m = running_model_with_queue();
        m.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: step_usage(input, cached),
        });
        m
    }

    #[test]
    fn statline_cache_box_shows_hit_rate_and_compact_token_counts() {
        // 105.3M cache-read over 211.4M input → 50% (rounded), compact `M`s.
        let model = model_with_cache(211_400_000, 105_300_000);
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("CACHE"), "cache label present:\n{text}");
        assert!(
            text.contains("50% (105.3M/211.4M tokens)"),
            "cache hit rate + compact counts:\n{text}"
        );
    }

    #[test]
    fn statline_cache_box_sits_after_spend_and_before_engine() {
        let model = model_with_cache(1_000, 500);
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        let pos = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("missing {needle:?}:\n{text}"))
        };
        assert!(pos("SPEND") < pos("CACHE"), "CACHE after SPEND:\n{text}");
        assert!(pos("CACHE") < pos("ENGINE"), "CACHE before ENGINE:\n{text}");
        assert!(
            pos("ENGINE") < pos("PIPELINE"),
            "PIPELINE after ENGINE:\n{text}"
        );
    }

    #[test]
    fn statline_cache_box_renders_zero_and_full_hit_rates() {
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);

        // 0%: input metered, nothing served from cache.
        let cold = model_with_cache(1_000, 0);
        let mut buf = Buffer::empty(area);
        render_status_bar(&cold, &ui, area, &mut buf);
        assert!(
            buffer_text(&buf).contains("0% (0/1.0K tokens)"),
            "cold cache reads 0%:\n{}",
            buffer_text(&buf)
        );

        // 100%: every input token was a cache hit.
        let warm = model_with_cache(1_000, 1_000);
        let mut buf = Buffer::empty(area);
        render_status_bar(&warm, &ui, area, &mut buf);
        assert!(
            buffer_text(&buf).contains("100% (1.0K/1.0K tokens)"),
            "fully warm cache reads 100%:\n{}",
            buffer_text(&buf)
        );
    }

    #[test]
    fn statline_cache_box_is_a_dash_before_any_usage() {
        // No StepUsage metered yet → the CACHE cell shows the no-data dash and
        // never divides by zero (the render below must not panic).
        let model = running_model_with_queue();
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("CACHE"), "cache label still present:\n{text}");
        assert!(
            !text.contains("tokens"),
            "no token counts before any usage:\n{text}"
        );
    }

    /// One `Pr` event folded onto the running model.
    fn model_with_pr(number: Option<u64>, ci: Option<stella_protocol::CiStatus>) -> WorkspaceModel {
        let mut m = running_model_with_queue();
        m.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Pr {
                url: "https://github.com/x/y/pull/183".into(),
                status: stella_protocol::PrStatus::Open,
                number,
                ci,
            },
        });
        m
    }

    #[test]
    fn statline_has_no_pr_cell_before_any_pr_event() {
        let model = running_model_with_queue();
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(
            !text.contains("PR") && !text.contains('⇢'),
            "no PR cell before a Pr event:\n{text}"
        );
    }

    #[test]
    fn statline_pr_cell_shows_number_status_and_a_bold_failing_cross() {
        let model = model_with_pr(Some(183), Some(stella_protocol::CiStatus::Failing));
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("PR"), "PR label present:\n{text}");
        assert!(
            text.contains("⇢ #183 open"),
            "number + status in the cell:\n{text}"
        );
        assert!(text.contains('✗'), "failing CI shows the cross:\n{text}");
    }

    #[test]
    fn statline_pr_cell_falls_back_to_the_url_tail_without_a_number() {
        let model = model_with_pr(None, Some(stella_protocol::CiStatus::Passing));
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(
            text.contains("⇢ 183 open"),
            "the URL tail identifies the PR when no number parsed:\n{text}"
        );
        assert!(text.contains('✓'), "passing CI shows the check:\n{text}");
    }

    #[test]
    fn statline_pipeline_box_reads_on_or_off_with_the_mode() {
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);

        let mut off = running_model_with_queue();
        off.pipeline = false;
        let mut buf = Buffer::empty(area);
        render_status_bar(&off, &ui, area, &mut buf);
        let off_text = buffer_text(&buf);
        assert!(off_text.contains("PIPELINE"), "pipeline label:\n{off_text}");
        assert!(
            off_text.contains("OFF"),
            "raw engine loop reads OFF:\n{off_text}"
        );

        let mut on = running_model_with_queue();
        on.pipeline = true;
        let mut buf = Buffer::empty(area);
        render_status_bar(&on, &ui, area, &mut buf);
        let on_text = buffer_text(&buf);
        assert!(
            !on_text.contains("OFF"),
            "staged pipeline is not OFF:\n{on_text}"
        );
        // The CONTEXT *label* also contains "ON", so scope the positive check
        // to the value row (the second rendered line).
        let values = on_text.lines().nth(1).expect("value row");
        assert!(
            values.contains("ON"),
            "staged pipeline reads ON:\n{on_text}"
        );
    }

    #[test]
    fn queue_popup_lists_prompts_and_arms_the_clear_confirm() {
        let model = running_model_with_queue();
        let mut ui = DeckUi::default();
        ui.queue_open = true;
        ui.queue_sel = 1;
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        render_queue_popup(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("queue · 2 pending"), "title:\n{text}");
        assert!(text.contains("write the tests"), "row 1:\n{text}");
        assert!(text.contains("▸ 2. open a pr"), "row 2 selected:\n{text}");
        assert!(text.contains("ctrl+x delete"), "legend:\n{text}");
        // Armed confirm swaps the legend for the warning.
        ui.queue_confirm_clear = true;
        let mut buf2 = Buffer::empty(area);
        render_queue_popup(&model, &ui, area, &mut buf2);
        let warned = buffer_text(&buf2);
        assert!(
            warned.contains("press ctrl+d again"),
            "confirm warning:\n{warned}"
        );
    }

    /// A `DeckUi` on the Graph tab with an `n`-file snapshot rooted on the
    /// middle file, its picker open.
    fn ui_with_graph_picker(n: usize) -> DeckUi {
        use crate::graph::{GraphNode, GraphSnapshot};
        let files: Vec<String> = (0..n).map(|i| format!("crate/mod_{i:02}.rs")).collect();
        let focus = files[n / 2].clone();
        let mut ui = DeckUi::default();
        ui.tab = DeckTab::Graph;
        ui.graph = Some(GraphSnapshot {
            focus: focus.clone(),
            nodes: vec![GraphNode {
                label: focus,
                kind: "file".into(),
                location: None,
            }],
            edges: vec![],
            files,
        });
        ui.graph_picker_open = true;
        ui
    }

    #[test]
    fn graph_picker_lists_files_marks_the_current_and_shows_the_legend() {
        let mut ui = ui_with_graph_picker(4);
        ui.graph_picker_sel = 2; // the focus file (crate/mod_02.rs)
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_graph_picker(&ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("files · 4 indexed"), "title:\n{text}");
        assert!(text.contains("filter"), "filter query line:\n{text}");
        assert!(text.contains("crate/mod_00.rs"), "lists files:\n{text}");
        assert!(text.contains("· current"), "marks the rooted file:\n{text}");
        assert!(text.contains("type to filter"), "legend:\n{text}");
    }

    #[test]
    fn graph_picker_windows_the_list_to_keep_the_selection_visible() {
        // Far more files than the popup's rows, selection at the end: the
        // window must slide (via the shared `scroll_window_start`) so the last
        // file shows and the first scrolls out.
        let mut ui = ui_with_graph_picker(60);
        ui.graph_picker_sel = 59;
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_graph_picker(&ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(
            text.contains("crate/mod_59.rs"),
            "selection scrolled in:\n{text}"
        );
        assert!(
            !text.contains("crate/mod_00.rs"),
            "head scrolled out:\n{text}"
        );
    }

    #[test]
    fn graph_picker_narrows_to_the_filter_query() {
        let mut ui = ui_with_graph_picker(20);
        ui.graph_picker_query = "mod_1".to_string();
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_graph_picker(&ui, area, &mut buf);
        let text = buffer_text(&buf);
        // mod_10..mod_19 match; mod_00..mod_09 (except none contain "mod_1")
        // do not.
        assert!(text.contains("crate/mod_10.rs"), "matches shown:\n{text}");
        assert!(
            !text.contains("crate/mod_02.rs"),
            "non-matches hidden:\n{text}"
        );
    }
}
