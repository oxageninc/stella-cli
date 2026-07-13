//! The top-level deck frame: the [`ratatui_comfy_tabs`] tab bar + the active
//! view + an always-on composer + a status bar, with the splash as a full-frame
//! overlay until it finishes. This is the tab dispatcher and the one place the
//! deck's chrome is drawn.

use std::time::Duration;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, StatefulWidget, Widget};
use ratatui_comfy_tabs::{TabNav, TabNavState};

use crate::deck::{DeckTab, WorkspaceModel};
use crate::deck_ui::DeckUi;
use crate::{fx, splash, theme, views};

/// How long the deck fades in from muted after the splash hands off.
const REVEAL_MS: u32 = 350;
/// How long the amber sweep plays over the content pane on a tab change.
const TAB_SWITCH_MS: u32 = 180;

pub fn render_deck(model: &WorkspaceModel, ui: &mut DeckUi, frame: &mut Frame) {
    let area = frame.area();
    let buf = frame.buffer_mut();

    // The splash owns the whole frame until it finishes / is skipped.
    if !ui.splash.is_done() {
        splash::render(&ui.splash, area, buf);
        return;
    }

    // tab bar (comfy-tabs needs exactly 3 rows) | content | composer | status
    let bands = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
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
    }

    render_composer(ui, bands[2], buf);
    render_status_bar(model, ui, bands[3], buf);

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
        render_help(area, buf);
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
    // Fresh state each frame: 5 tabs always fit, so there is no scroll to keep.
    let mut state = TabNavState::new(selected);
    StatefulWidget::render(nav, area, buf, &mut state);
}

/// The always-on composer row — typing works from any tab; Enter dispatches.
fn render_composer(ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let text = ui.composer.buffer();
    let line = if text.is_empty() {
        Line::from(vec![
            Span::styled("❯ ", theme::accent()),
            Span::styled(
                "type a prompt — Enter dispatches (never blocks) · Tab switches · ? help",
                theme::muted(),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("❯ ", theme::accent()),
            Span::styled(text.to_string(), theme::body()),
            Span::styled("▏", theme::accent()), // caret
        ])
    };
    Paragraph::new(line).render(area, buf);
}

/// The status bar: routed model · global CPU gauge · spend · active · queue.
fn render_status_bar(model: &WorkspaceModel, ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let cpu = model.global_cpu_pct as f64;
    let mut spans = vec![
        Span::styled(" ✦ stella ", theme::accent()),
        Span::styled("│ ", theme::rule()),
        Span::styled(
            format!("model {} ", model.latest_model().unwrap_or("—")),
            theme::muted(),
        ),
        Span::styled("│ ", theme::rule()),
    ];
    // CPU gauge — a small colored bar + percent.
    spans.push(Span::styled("cpu ", theme::muted()));
    spans.push(Span::styled(
        cpu_bar(cpu),
        theme::muted().fg(theme::gauge_color(cpu / 100.0)),
    ));
    spans.push(Span::styled(
        format!(" {cpu:>3.0}% "),
        theme::body().fg(theme::gauge_color(cpu / 100.0)),
    ));
    spans.push(Span::styled("│ ", theme::rule()));
    spans.push(Span::styled(
        format!("${:.2} ", model.total_cost()),
        theme::body(),
    ));
    spans.push(Span::styled("│ ", theme::rule()));
    spans.push(Span::styled(
        format!("{} active ", model.active_count()),
        theme::muted(),
    ));
    let pending = model.queue.pending();
    if pending > 0 {
        spans.push(Span::styled("│ ", theme::rule()));
        spans.push(Span::styled(
            format!("{pending} queued "),
            theme::body().fg(theme::WARN),
        ));
    }
    // Focused-tab hint on the right is implied by the composer help; keep it lean.
    let _ = ui;
    Paragraph::new(Line::from(spans)).render(area, buf);
}

/// An 8-cell utilization bar for a `[0, 100]` percent.
fn cpu_bar(pct: f64) -> String {
    const CELLS: usize = 8;
    let filled = ((pct / 100.0) * CELLS as f64).round() as usize;
    let mut s = String::new();
    for i in 0..CELLS {
        s.push(if i < filled { '▮' } else { '▯' });
    }
    s
}

/// A centered help overlay listing the deck's keys.
fn render_help(area: Rect, buf: &mut Buffer) {
    let w = area.width.min(56);
    let h = area.height.min(16);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(popup, buf);
    let lines = vec![
        Line::from(Span::styled(" Command Deck — keys", theme::accent())),
        Line::default(),
        Line::from(Span::styled("  Tab / ⇧Tab   switch tabs", theme::body())),
        Line::from(Span::styled("  1–5          jump to a tab", theme::body())),
        Line::from(Span::styled(
            "  Enter        dispatch prompt (never blocks)",
            theme::body(),
        )),
        Line::from(Span::styled(
            "  ↑ ↓          navigate the active tab",
            theme::body(),
        )),
        Line::from(Span::styled(
            "  Agents: p/s/r  pause / stop / restart",
            theme::body(),
        )),
        Line::from(Span::styled(
            "  Traces: f      cycle agent filter",
            theme::body(),
        )),
        Line::from(Span::styled(
            "  Files:  Enter  open the diff",
            theme::body(),
        )),
        Line::from(Span::styled("  Ctrl-C       quit", theme::body())),
        Line::default(),
        Line::from(Span::styled("  any key closes this help", theme::muted())),
    ];
    Paragraph::new(lines)
        .alignment(Alignment::Left)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::accent())
                .title(" ? "),
        )
        .render(popup, buf);
}
