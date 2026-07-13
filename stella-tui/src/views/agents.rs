//! Agents tab — the flagship `htop`/`claudectl`-style dashboard: one dense
//! row per agent with live status, spend, resource usage, and activity.
//!
//! Every color comes from [`crate::theme`]; every number is read straight off
//! [`crate::deck::AgentEntry`] (no shadow state, no re-derivation of what the
//! model already computed).

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Widget};

use crate::deck::{ACTIVITY_WINDOW, AgentEntry, WorkspaceModel};
use crate::deck_ui::DeckUi;
use crate::theme;

/// Column headers, in display order — matches the `widths` array in
/// [`render`] index-for-index.
const HEADERS: [&str; 11] = [
    "Agent", "Goal", "Status", "Ctx%", "Cost", "$/hr", "Elapsed", "CPU%", "MEM", "In/Out",
    "Activity",
];

/// The goal/title column is pre-truncated to this many characters so a very
/// long title reads as an intentional ellipsis rather than a hard mid-word
/// cut wherever the terminal happens to clip the column.
const GOAL_MAX_CHARS: usize = 56;

pub fn render(model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    if model.agents.is_empty() {
        render_empty(area, buf);
        return;
    }

    let title = format!(
        " Agents — {} active / {} total ",
        model.active_count(),
        model.agents.len()
    );
    let block = Block::default().borders(Borders::ALL).title(title);

    let header = Row::new(HEADERS.iter().copied().map(Cell::from)).style(theme::accent());

    let rows: Vec<Row> = model
        .agents
        .iter()
        .enumerate()
        .map(|(i, entry)| agent_row(entry, model.now_ms, i == ui.focused))
        .collect();

    // Fixed widths for every column except Goal, which fills whatever is
    // left — this is what keeps the row dense on a wide terminal and never
    // overflows on a narrow one (the Table constraint solver shrinks Fill
    // first, then compresses the rest, but never draws past `area`).
    let widths = [
        Constraint::Length(18),                     // Agent
        Constraint::Fill(1),                        // Goal
        Constraint::Length(12),                     // Status
        Constraint::Length(6),                      // Ctx%
        Constraint::Length(9),                      // Cost
        Constraint::Length(9),                      // $/hr
        Constraint::Length(8),                      // Elapsed
        Constraint::Length(6),                      // CPU%
        Constraint::Length(7),                      // MEM
        Constraint::Length(14),                     // In/Out
        Constraint::Length(ACTIVITY_WINDOW as u16), // Activity
    ];

    Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .render(area, buf);
}

/// Build one dashboard row for `entry`. Every cell owns its content, so the
/// returned row is fully decoupled from `entry`'s borrow.
fn agent_row(entry: &AgentEntry, now_ms: u64, is_focused: bool) -> Row<'static> {
    let status = entry.status;
    let status_color = theme::status_color(status);

    // A plain chevron, not `▶` — that glyph is already `status_glyph(Running)`,
    // and doubling it up when the focused agent happens to be running reads
    // as a rendering glitch rather than a deliberate highlight.
    let caret = if is_focused {
        Span::styled("> ", Style::default().fg(theme::AMBER))
    } else {
        Span::raw("  ")
    };
    let agent_cell = Cell::from(Line::from(vec![
        caret,
        Span::styled(
            format!("{} ", theme::status_glyph(status)),
            Style::default().fg(status_color),
        ),
        Span::styled(entry.meta.id.clone(), Style::default().fg(status_color)),
    ]));

    let goal_cell = Cell::from(truncate(&entry.meta.title, GOAL_MAX_CHARS)).style(theme::body());

    let status_cell = Cell::from(status.label()).style(Style::default().fg(status_color));

    let ctx_frac = ctx_used_fraction(entry.tokens_in);
    let ctx_cell = Cell::from(format!("{:>3.0}%", ctx_frac * 100.0))
        .style(Style::default().fg(theme::gauge_color(ctx_frac)));

    let cost_cell = Cell::from(format!("${:.2}", entry.cost_usd)).style(theme::body());

    let burn = entry.usd_per_hour(now_ms);
    let burn_style = match entry.model.hud.limit_usd {
        // Reuse the agent's own configured budget limit (folded from
        // `BudgetTick`) as the "high" reference: a burn rate that would
        // exhaust the whole budget within an hour reads as red, same ramp as
        // the CPU/Ctx gauges. No limit configured → no signal to compare
        // against, so the cell stays neutral rather than guessing a number.
        Some(limit) if limit > 0.0 => {
            Style::default().fg(theme::gauge_color((burn / limit).min(1.0)))
        }
        _ => theme::body(),
    };
    let burn_cell = Cell::from(format!("${burn:.2}")).style(burn_style);

    let elapsed_cell = Cell::from(fmt_elapsed(entry.elapsed_ms(now_ms))).style(theme::muted());

    let cpu_frac = (entry.res.cpu_pct as f64 / 100.0).clamp(0.0, 1.0);
    let cpu_cell = Cell::from(format!("{:>3.0}%", entry.res.cpu_pct))
        .style(Style::default().fg(theme::gauge_color(cpu_frac)));

    let mem_cell = Cell::from(humanize_bytes(entry.res.mem_bytes)).style(theme::muted());

    let io_cell = Cell::from(format!(
        "{}/{}",
        humanize_count(entry.tokens_in),
        humanize_count(entry.tokens_out)
    ))
    .style(theme::muted());

    let spark: String = entry
        .activity
        .padded()
        .iter()
        .map(|&intensity| theme::spark_glyph(intensity))
        .collect();
    let activity_cell = Cell::from(spark).style(Style::default().fg(theme::AMBER));

    let mut row = Row::new(vec![
        agent_cell,
        goal_cell,
        status_cell,
        ctx_cell,
        cost_cell,
        burn_cell,
        elapsed_cell,
        cpu_cell,
        mem_cell,
        io_cell,
        activity_cell,
    ]);
    if is_focused {
        // Reverses whatever fg/bg each cell resolved to above — a bg tint
        // that needs no new color, layered on top of the amber caret so
        // focus still reads even if a narrow terminal clips the caret
        // column.
        row = row.style(Style::default().add_modifier(Modifier::REVERSED));
    }
    row
}

/// The centered, muted "nothing dispatched yet" state.
fn render_empty(area: Rect, buf: &mut Buffer) {
    let block = Block::default().borders(Borders::ALL).title(" Agents ");
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.height == 0 {
        return;
    }
    let hint = "no agents yet — type a prompt and press Enter to dispatch one";
    let y = inner.y + inner.height.saturating_sub(1) / 2;
    let line_area = Rect::new(inner.x, y, inner.width, 1);
    Paragraph::new(hint)
        .style(theme::muted())
        .alignment(Alignment::Center)
        .render(line_area, buf);
}

/// Context-window utilization, clamped to `[0.0, 1.0]`.
///
/// `tokens_in` is the real per-agent count folded from `StepUsage` events,
/// but the divisor is a NOMINAL 200k-token window — the real per-model
/// context window isn't threaded through `AgentMeta`/`AgentEvent` yet, so
/// this is an approximation good enough for a dashboard density signal, not
/// a hard cutoff. Revisit once model context size rides the wire.
fn ctx_used_fraction(tokens_in: u64) -> f64 {
    const NOMINAL_CONTEXT_WINDOW: f64 = 200_000.0;
    (tokens_in as f64 / NOMINAL_CONTEXT_WINDOW).min(1.0)
}

/// `mm:ss`, growing past two digits of minutes rather than wrapping.
fn fmt_elapsed(ms: u64) -> String {
    let total_secs = ms / 1000;
    format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
}

/// `1234` → `"1.2k"`, `1_200_000` → `"1.2m"`, below 1000 → the bare number.
fn humanize_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Bytes → `"212M"` style, binary (1024) units, whole numbers only.
fn humanize_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    format!("{val:.0}{}", UNITS[unit])
}

/// Truncate to `max` chars with a trailing ellipsis, char-safe (never splits
/// a multi-byte codepoint).
fn truncate(s: &str, max: usize) -> String {
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
    use crate::envelope::{AgentMeta, Inbound};
    use stella_protocol::{AgentEvent, StageKind};

    /// Flatten a `Buffer` to one `String` per row (styling stripped —
    /// content is what we assert on, never raw ANSI).
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
    fn dashboard_renders_agent_id_goal_and_humanized_tokens() {
        let mut model = WorkspaceModel::new();
        model.now_ms = 5_000;
        model.apply_inbound(&Inbound::Register(AgentMeta::new(
            "lead",
            "refactor the billing module",
            0,
        )));
        model.apply_inbound(&Inbound::Register(AgentMeta::new(
            "sub",
            "run tests",
            1_000,
        )));
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Stage {
                name: StageKind::Execute,
            },
        });
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::StepUsage {
                step: 1,
                model: "glm-5.2".into(),
                input_tokens: 62_000,
                output_tokens: 12_400,
                cached_input_tokens: 0,
                estimated_input_tokens: 60_000,
                cost_usd: 0.42, // NOT folded into cost_usd — StepUsage feeds tokens only.
                duration_ms: 100,
                retries: 0,
                tool_calls: 1,
            },
        });
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::BudgetTick {
                spent_usd: 0.42,
                limit_usd: Some(2.0),
                mode: stella_protocol::BudgetMode::Observed,
            },
        });

        let mut ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 8);
        let mut buf = Buffer::empty(area);
        render(&model, &mut ui, area, &mut buf);

        let text = buffer_text(&buf);
        assert!(text.contains("lead"), "agent id shown:\n{text}");
        assert!(
            text.contains("refactor the billing module"),
            "goal shown:\n{text}"
        );
        assert!(text.contains("sub"), "second agent row shown:\n{text}");
        assert!(text.contains("62.0k"), "tokens-in humanized:\n{text}");
        assert!(text.contains("12.4k"), "tokens-out humanized:\n{text}");
        assert!(text.contains("$0.42"), "cost shown:\n{text}");
    }

    #[test]
    fn empty_workspace_shows_the_dispatch_hint() {
        let model = WorkspaceModel::new();
        let mut ui = DeckUi::default();
        let area = Rect::new(0, 0, 80, 6);
        let mut buf = Buffer::empty(area);
        render(&model, &mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(
            text.contains("no agents yet"),
            "empty-state hint shown:\n{text}"
        );
    }

    #[test]
    fn humanizers_format_as_expected() {
        assert_eq!(humanize_count(62_000), "62.0k");
        assert_eq!(humanize_count(500), "500");
        assert_eq!(humanize_count(1_500_000), "1.5m");
        assert_eq!(humanize_bytes(0), "0B");
        assert_eq!(fmt_elapsed(754_000), "12:34");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a very long title indeed", 10), "a very lo…");
    }
}
