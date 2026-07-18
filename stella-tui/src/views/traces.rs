//! Traces tab — the unified, scrollable, filterable cross-agent event
//! timeline. Every row is one [`TraceRow`] from [`WorkspaceModel::trace`],
//! oldest → newest top → bottom, following the tail by default exactly like
//! the single-session transcript (`render.rs::render_transcript`, L-T4).
//!
//! `ui.trace_filter` narrows the timeline to one agent (`TraceLog::for_agent`);
//! `None` shows every agent interleaved. Both branches iterate the same
//! `VecDeque` order, so filtering never reorders events.

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::deck::{TraceRow, WorkspaceModel};
use crate::deck_ui::DeckUi;
use crate::theme;

pub fn render(model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let rows: Vec<&TraceRow> = match ui.trace_filter.as_deref() {
        Some(id) => model.trace.for_agent(id).collect(),
        None => model.trace.rows.iter().collect(),
    };
    let total = rows.len();
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;

    // Record viewport metrics for the pure key handler (`handle_traces_key`)
    // to clamp/scroll on the next keypress — the same contract every
    // scrollable tab follows.
    ui.metrics.trace_total = total;
    ui.metrics.trace_height = inner_height;

    let window = ui.trace_scroll.window(total, inner_height);
    let block = Block::default().borders(Borders::ALL).title(title_for(
        ui.trace_filter.as_deref(),
        total,
        &window,
        ui.trace_scroll.follow,
    ));

    if total == 0 {
        let inner = block.inner(area);
        block.render(area, buf);
        render_empty_hint(inner, buf);
        return;
    }

    let lines: Vec<Line<'static>> = rows[window]
        .iter()
        .map(|row| row_line(row, model.now_ms, inner_width))
        .collect();

    Paragraph::new(Text::from(lines))
        .block(block)
        .render(area, buf);
}

/// The block title: active filter + position/following state + the `f: filter`
/// hint, mirroring the `" transcript · N lines · following "` style already
/// used by the session tab.
fn title_for(filter: Option<&str>, total: usize, window: &Range<usize>, following: bool) -> String {
    let scope = filter.unwrap_or("all");
    let position = if total == 0 {
        String::new()
    } else if following {
        format!(" · {total} events · following")
    } else {
        format!(
            " · {}-{} / {total}",
            window.start.min(total),
            window.end.min(total)
        )
    };
    format!(" traces · {scope}{position} · f: filter ")
}

/// One timeline row: muted relative time, a stable per-agent color, a
/// kind-colored chip, then the summary — truncated to fit `width` so a long
/// summary never wraps and breaks the line-exact scroll math (L-T4).
fn row_line(row: &TraceRow, now_ms: u64, width: usize) -> Line<'static> {
    let elapsed_ms = now_ms.saturating_sub(row.ts);
    let mmss = format_mmss(elapsed_ms);
    let kind_chip = format!("[{}]", row.kind.label());
    let prefix_width =
        mmss.chars().count() + 2 + row.agent.chars().count() + 2 + kind_chip.chars().count() + 1;
    let summary = truncate_to_width(&row.summary, width.saturating_sub(prefix_width));

    Line::from(vec![
        Span::styled(mmss, theme::muted()),
        Span::raw("  "),
        Span::styled(
            row.agent.clone(),
            Style::new()
                .fg(theme::agent_color(&row.agent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            kind_chip,
            Style::new()
                .fg(theme::trace_kind_color(row.kind))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(summary, theme::body()),
    ])
}

/// `mm:ss` elapsed since `row.ts`, relative to the deck clock. Grows past two
/// digits of minutes rather than clamping, so a long-running agent's early
/// events still read correctly.
fn format_mmss(elapsed_ms: u64) -> String {
    let total_secs = elapsed_ms / 1000;
    format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
}

/// Truncate to at most `width` chars, adding an ellipsis when clipped. Robust
/// to `width == 0` (empty string) — never panics on a too-narrow terminal.
fn truncate_to_width(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let head: String = text.chars().take(width - 1).collect();
    format!("{head}…")
}

/// Centered muted hint shown when the (possibly filtered) timeline is empty.
fn render_empty_hint(area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mid_y = area.y + area.height / 2;
    let line_area = Rect {
        x: area.x,
        y: mid_y,
        width: area.width,
        height: 1,
    };
    Paragraph::new(Span::styled("no activity yet", theme::muted()))
        .alignment(Alignment::Center)
        .render(line_area, buf);
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::deck::WorkspaceModel;
    use crate::envelope::{AgentMeta, Inbound};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use stella_protocol::{AgentEvent, StageKind};

    fn reg(id: &str) -> Inbound {
        Inbound::Register(AgentMeta::new(id, format!("goal for {id}"), 0))
    }
    fn ev(agent: &str, event: AgentEvent) -> Inbound {
        Inbound::Event {
            agent: agent.into(),
            event,
        }
    }

    /// Flatten a rendered buffer to one string, styling stripped — content is
    /// what tests assert on, per L-T6 (no raw ANSI in assertions).
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

    fn draw(model: &WorkspaceModel, ui: &mut DeckUi, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render(model, ui, area, f.buffer_mut());
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn two_agent_model() -> WorkspaceModel {
        let mut model = WorkspaceModel::new();
        model.now_ms = 65_000; // so elapsed-time formatting has something to chew on
        model.apply_inbound(&reg("a"));
        model.apply_inbound(&reg("b"));
        model.apply_inbound(&ev(
            "a",
            AgentEvent::Stage {
                name: StageKind::Execute,
            },
        ));
        model.apply_inbound(&ev(
            "a",
            AgentEvent::Text {
                delta: "building the auth refactor".into(),
            },
        ));
        model.apply_inbound(&ev(
            "b",
            AgentEvent::FileChange {
                path: "src/lib.rs".into(),
                kind: stella_protocol::FileChangeKind::Modified,
                diff: Some("+one\n-two\n".into()),
            },
        ));
        model
    }

    #[test]
    fn empty_timeline_shows_the_centered_hint() {
        let model = WorkspaceModel::new();
        let mut ui = DeckUi::default();
        let text = draw(&model, &mut ui, 60, 12);
        assert!(text.contains("no activity yet"), "empty hint:\n{text}");
        assert_eq!(ui.metrics.trace_total, 0);
    }

    #[test]
    fn unfiltered_timeline_renders_rows_from_every_agent() {
        let model = two_agent_model();
        let mut ui = DeckUi::default();
        let text = draw(&model, &mut ui, 100, 20);
        assert!(
            text.contains("building the auth refactor"),
            "agent a's text row is visible:\n{text}"
        );
        assert!(
            text.contains("src/lib.rs"),
            "agent b's file row is visible:\n{text}"
        );
        assert!(
            text.contains("traces · all"),
            "header shows unfiltered scope:\n{text}"
        );
        assert!(
            text.contains("f: filter"),
            "header shows the filter hint:\n{text}"
        );
        assert_eq!(ui.metrics.trace_total, model.trace.rows.len());
    }

    #[test]
    fn filtering_to_one_agent_hides_the_others_rows() {
        let model = two_agent_model();
        let mut ui = DeckUi::default();
        ui.trace_filter = Some("a".to_string());
        let text = draw(&model, &mut ui, 100, 20);
        assert!(
            text.contains("building the auth refactor"),
            "agent a's row still shows:\n{text}"
        );
        assert!(
            !text.contains("src/lib.rs"),
            "agent b's row is filtered out:\n{text}"
        );
        assert!(
            text.contains("traces · a"),
            "header shows the active filter:\n{text}"
        );
        assert_eq!(ui.metrics.trace_total, model.trace.for_agent("a").count());
    }

    #[test]
    fn format_mmss_renders_minutes_and_seconds() {
        assert_eq!(format_mmss(0), "00:00");
        assert_eq!(format_mmss(65_000), "01:05");
        assert_eq!(format_mmss(3_661_000), "61:01");
    }

    #[test]
    fn truncate_to_width_adds_an_ellipsis_only_when_clipped() {
        assert_eq!(truncate_to_width("short", 10), "short");
        assert_eq!(truncate_to_width("a very long summary line", 8), "a very …");
        assert_eq!(truncate_to_width("anything", 0), "");
    }
}
