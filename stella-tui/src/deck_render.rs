//! The top-level deck frame: the [`ratatui_comfy_tabs`] tab bar + the active
//! view + an always-on composer + a status bar, with the splash as a full-frame
//! overlay until it finishes. This is the tab dispatcher and the one place the
//! deck's chrome is drawn.

use std::time::Duration;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, StatefulWidget, Widget};
use ratatui_comfy_tabs::{TabNav, TabNavState};

use crate::deck::{DeckTab, WorkspaceModel};
use crate::deck_ui::DeckUi;
use crate::envelope::AgentStatus;
use crate::render::{render_slash_popup, slash_popup_area, stage_label};
use crate::{fx, splash, theme, views};

/// How long the deck fades in from muted after the splash hands off.
const REVEAL_MS: u32 = 350;
/// How long the amber sweep plays over the content pane on a tab change.
const TAB_SWITCH_MS: u32 = 180;
/// How often the garble spinner re-rolls, in deck-clock ms. Matches the shell
/// tick, so the spinner churns every repaint — the "extremely fast" read.
const SPINNER_PHASE_MS: u64 = 33;
/// The spinner's width in cells: short and wide-ish, per the design notes.
const SPINNER_W: usize = 18;

pub fn render_deck(model: &WorkspaceModel, ui: &mut DeckUi, frame: &mut Frame) {
    let area = frame.area();
    let buf = frame.buffer_mut();

    // The splash owns the whole frame until it finishes / is skipped.
    if !ui.splash.is_done() {
        splash::render(&ui.splash, area, buf);
        return;
    }

    // tab bar (comfy-tabs needs exactly 3 rows) | content | [activity strip]
    // | composer | status
    let strip_h: u16 = if activity_strip_active(model) { 1 } else { 0 };
    let bands = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(strip_h),
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

    if strip_h > 0 {
        render_activity_strip(model, ui, bands[2], buf);
    }
    render_composer(ui, bands[3], buf);
    render_status_bar(model, ui, bands[4], buf);

    // Floating popups sit above the chrome: the slash menu anchors to the
    // composer; the queue editor centers over the content.
    let slash = ui.composer.slash_menu(&ui.slash_commands);
    if let Some(menu) = slash.filter(|m| !m.is_empty()) {
        let selected = ui.slash_selected.min(menu.matches.len().saturating_sub(1));
        let popup = slash_popup_area(area, bands[3], menu.matches.len());
        render_slash_popup(&menu, selected, popup, buf);
    }
    if ui.queue_open {
        render_queue_popup(model, ui, area, buf);
    }

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

/// True when the deck should surface the one-line activity strip: something
/// is running (the spinner reassures) or prompts are waiting (the queue count
/// must stay visible).
fn activity_strip_active(model: &WorkspaceModel) -> bool {
    model.queue.pending() > 0
        || model
            .agents
            .iter()
            .any(|a| a.status == AgentStatus::Running)
}

/// The one-line activity strip above the composer: the fast ember garble
/// spinner while anything runs, the focused agent's current stage (the
/// "current task, one line" read), and the queued count with its shortcut.
fn render_activity_strip(model: &WorkspaceModel, ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let pending = model.queue.pending();
    let right_text = if pending > 0 {
        format!("▸ {pending} queued · ctrl+t open ")
    } else {
        String::new()
    };
    let right_w = right_text.chars().count() as u16;
    let cols =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(right_w)]).split(area);

    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    let running = model
        .agents
        .iter()
        .any(|a| a.status == AgentStatus::Running);
    if running {
        // Phase from the deck clock: churns every tick, identical on replay.
        let spinner = fx::garble_line(model.now_ms / SPINNER_PHASE_MS, SPINNER_W);
        spans.extend(spinner.spans);
        spans.push(Span::raw("  "));
    }
    if let Some(agent) = model.agents.get(ui.focused) {
        spans.push(Span::styled(
            format!("{} ", theme::status_glyph(agent.status)),
            Style::default().fg(theme::status_color(agent.status)),
        ));
        spans.push(Span::styled(agent.meta.id.clone(), theme::accent()));
        if let Some(stage) = agent.model.hud.stage {
            spans.push(Span::styled(
                format!(" · {}", stage_label(stage)),
                theme::muted(),
            ));
        }
    }
    Paragraph::new(Line::from(spans)).render(cols[0], buf);
    if pending > 0 {
        Paragraph::new(Line::from(Span::styled(
            right_text,
            theme::body().fg(theme::WARN),
        )))
        .render(cols[1], buf);
    }
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
            Span::styled(format!("{marker}{}. ", i + 1), style.fg(theme::AMBER)),
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

/// The always-on composer row — typing works from any tab; Enter dispatches.
fn render_composer(ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let text = ui.composer.buffer();
    let line = if text.is_empty() {
        Line::from(vec![
            Span::styled("❯ ", theme::accent()),
            Span::styled(
                "type a prompt — Enter queues (never blocks) · ! shell · / commands · ? help",
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
    let w = area.width.min(62);
    let h = area.height.min(20);
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
            "  Enter        queue prompt (never blocks)",
            theme::body(),
        )),
        Line::from(Span::styled(
            "  !cmd         run a shell command NOW (skips the queue)",
            theme::body(),
        )),
        Line::from(Span::styled(
            "  /            command popup · ↑/↓ · tab · enter",
            theme::body(),
        )),
        Line::from(Span::styled(
            "  Ctrl-T / ↑   queue editor · ctrl+x delete · ctrl+d ×2 clear",
            theme::body(),
        )),
        Line::from(Span::styled(
            "  Ctrl-R       expand/collapse thinking",
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

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::envelope::{AgentMeta, Inbound};
    use stella_protocol::{AgentEvent, StageKind};

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
    fn activity_strip_shows_spinner_stage_and_queue_count() {
        let model = running_model_with_queue();
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 90, 1);
        let mut buf = Buffer::empty(area);
        render_activity_strip(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("lead"), "focused agent shown:\n{text}");
        assert!(text.contains("execute"), "current stage shown:\n{text}");
        assert!(
            text.contains("2 queued · ctrl+t"),
            "queue count + shortcut on the right:\n{text}"
        );
        // The spinner is deterministic in the deck clock, so two renders of
        // the same model are identical…
        let mut buf2 = Buffer::empty(area);
        render_activity_strip(&model, &ui, area, &mut buf2);
        assert_eq!(buffer_text(&buf), buffer_text(&buf2));
        // …and a tick later the garble visibly churns.
        let mut later = running_model_with_queue();
        later.now_ms = 1_000 + SPINNER_PHASE_MS;
        let mut buf3 = Buffer::empty(area);
        render_activity_strip(&later, &ui, area, &mut buf3);
        assert_ne!(buffer_text(&buf), buffer_text(&buf3));
    }

    #[test]
    fn strip_is_active_for_running_agents_or_pending_prompts_only() {
        let mut idle = WorkspaceModel::new();
        assert!(!activity_strip_active(&idle));
        idle.queue.enqueue("later".into(), 1);
        assert!(activity_strip_active(&idle), "queued prompts keep it visible");
        assert!(activity_strip_active(&running_model_with_queue()));
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
}
