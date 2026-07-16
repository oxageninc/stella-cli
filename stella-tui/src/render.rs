//! Pure rendering: `(model, ui) -> frame`. Every panel is drawn by a function
//! that reads only `&SessionModel` / small `Copy` view values, so the whole
//! surface is a deterministic function of the event log plus the ephemeral
//! scroll/compose state (L-T1) — the replay-determinism proptest at the bottom
//! renders two independently-folded models and asserts identical backing cell
//! buffers.
//!
//! # Panel panic boundary (L-T7)
//!
//! Each panel is drawn through [`guarded_panel`], which renders it into its
//! **own** throwaway [`Buffer`] inside `catch_unwind`. If a panel panics
//! mid-write, that local buffer is discarded and an error card is drawn in its
//! place; the app keeps running with input alive. This is sound because the
//! draw closures capture only immutable references (`&SessionModel` and
//! `Copy` values — no interior mutability) and the sole mutable state they
//! touch is the freshly-created local buffer, which is thrown away on panic.
//! The frame's real buffer is only ever written by the infallible [`blit`]
//! *after* the panel has finished, so a half-written panel can never reach the
//! screen. Hence the `AssertUnwindSafe` wrapper is justified rather than
//! papered over.

use std::ops::Range;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use stella_protocol::FileChangeKind;

use crate::composer::{ComposerLayout, SlashMenu, layout as composer_layout, split_row_at};
use crate::model::{AskUserPrompt, FileState, Hud, SessionModel, TranscriptEntry};
use crate::textline::{self, EventLine, Tone, budget_mode_label, stage_label};
use crate::ui::{PanelFocus, UiState, ViewportMetrics};
use crate::{diff, theme};

/// Draw the whole TUI for one frame. Records the panels' viewport sizes back
/// into `ui.metrics` so the pure key handler can clamp scrolling on the next
/// keypress (the only reason this takes `&mut UiState`).
pub fn render(model: &SessionModel, ui: &mut UiState, frame: &mut Frame) {
    let root = frame.area();
    let has_scope = model.pending_scope_review.is_some();
    let has_ask = model.pending_ask_user.is_some();

    // Vertical bands: HUD, main, [scope], [ask], composer. The slash menu is
    // no longer a band — it floats above the composer as a popup.
    let mut constraints = vec![Constraint::Length(3), Constraint::Min(1)];
    if has_scope {
        constraints.push(Constraint::Length(6));
    }
    if let Some(prompt) = model.pending_ask_user.as_ref() {
        // question + one row per option + a free-text hint, within a border.
        constraints.push(Constraint::Length(
            (prompt.options.len() as u16 + 4).min(10),
        ));
    }
    // The composer band grows with its soft-wrapped content (textarea
    // semantics) up to a cap, then scrolls to keep the cursor row visible.
    // Text width: the band spans the root minus 2 border columns and the
    // 2-column `› ` prompt prefix.
    let c_layout = composer_layout(&ui.composer, root.width.saturating_sub(4).max(1) as usize);
    let composer_rows = c_layout.rows.len().clamp(1, COMPOSER_MAX_ROWS) as u16;
    constraints.push(Constraint::Length(composer_rows + 2));
    let bands = Layout::vertical(constraints).split(root);

    let hud_area = bands[0];
    let main_area = bands[1];
    let mut idx = 2;
    let scope_area = if has_scope {
        let a = bands[idx];
        idx += 1;
        Some(a)
    } else {
        None
    };
    let ask_area = if has_ask {
        let a = bands[idx];
        idx += 1;
        Some(a)
    } else {
        None
    };
    let composer_area = bands[idx];

    // ---- HUD.
    guarded_panel(frame, hud_area, "hud", |buf| {
        render_hud(&model.hud, hud_area, buf)
    });

    // ---- Main: transcript (left) + files/diff (right).
    let cols = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(main_area);
    let transcript_area = cols[0];
    let right_area = cols[1];

    let t_lines = transcript_lines(model, ui.thinking_expanded, inner_width(transcript_area));
    let t_total = t_lines.len();
    let t_inner_h = inner_height(transcript_area);
    let t_window = ui.scroll.window(t_total, t_inner_h);
    let following = ui.scroll.follow;
    guarded_panel(frame, transcript_area, "transcript", |buf| {
        render_transcript(&t_lines, t_window.clone(), following, transcript_area, buf)
    });

    // ---- Right pane: diff viewer when open, else the files-touched panel.
    let (diff_total, diff_inner_h) = if ui.diff_open {
        let file = model.files.get(ui.selected_file);
        let diff_text = file.and_then(|f| f.latest_diff.as_deref());
        let d_lines = diff_text
            .map(|d| diff::body_lines(d, file.map(|f| f.path.as_str())))
            .unwrap_or_default();
        let (added, removed) = diff_text.map(diff::count_diff_lines).unwrap_or((0, 0));
        let d_total = d_lines.len();
        let d_inner_h = inner_height(right_area);
        let d_window = ui.diff_scroll.window(d_total, d_inner_h);
        let title = file
            .map(|f| f.path.clone())
            .unwrap_or_else(|| "diff".to_string());
        guarded_panel(frame, right_area, "diff", |buf| {
            render_diff(
                &d_lines,
                d_window.clone(),
                &title,
                (added, removed),
                right_area,
                buf,
            )
        });
        (d_total, d_inner_h)
    } else {
        let selected = ui.selected_file;
        let focus = ui.focus;
        guarded_panel(frame, right_area, "files", |buf| {
            render_files(&model.files, selected, focus, right_area, buf)
        });
        (0, 0)
    };

    // ---- Scope-review card (when a gate is pending).
    if let (Some(area), Some(proposal)) = (scope_area, model.pending_scope_review.as_ref()) {
        let answered = ui.scope_answered;
        guarded_panel(frame, area, "scope-review", |buf| {
            render_scope_review(proposal, answered, area, buf)
        });
    }

    // ---- Ask-user card (when a question is pending).
    if let (Some(area), Some(prompt)) = (ask_area, model.pending_ask_user.as_ref()) {
        let answered = ui.ask_answered;
        guarded_panel(frame, area, "ask-user", |buf| {
            render_ask_user(prompt, answered, area, buf)
        });
    }

    // ---- Composer.
    let composer_focused = ui.focus == PanelFocus::Composer;
    let composer_blank = ui.composer.is_blank();
    let enter_submits = ui.enter_submits;
    guarded_panel(frame, composer_area, "composer", |buf| {
        render_composer(
            &c_layout,
            composer_blank,
            composer_focused,
            enter_submits,
            composer_area,
            buf,
        )
    });

    // ---- Slash-command popup, floating just above the composer (drawn last
    // so it sits over the transcript, Crush-style, instead of reflowing it).
    let slash = ui.composer.slash_menu(&ui.slash_commands);
    if let Some(menu) = slash.filter(|m| !m.is_empty()) {
        let selected = ui.slash_selected.min(menu.matches.len().saturating_sub(1));
        let area = slash_popup_area(root, composer_area, menu.matches.len());
        guarded_panel(frame, area, "slash-menu", |buf| {
            render_slash_popup(&menu, selected, area, buf)
        });
    }

    // Cache viewport sizes for the next keypress's scroll clamping.
    ui.metrics = ViewportMetrics {
        transcript_height: t_inner_h,
        transcript_total: t_total,
        diff_height: diff_inner_h,
        diff_total,
    };
}

/// The usable interior height of a single-border panel.
pub(crate) fn inner_height(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

/// The usable interior width of a single-border panel.
pub(crate) fn inner_width(area: Rect) -> usize {
    area.width.saturating_sub(2) as usize
}

// ---------------------------------------------------------------------------
// Word-aware line wrapping (pre-wrap so scroll math stays line-exact, L-T4)
// ---------------------------------------------------------------------------

/// Coalesce adjacent same-styled characters into spans for compact output.
fn styled_chars_to_spans(chars: Vec<(char, Style)>) -> Vec<Span<'static>> {
    if chars.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut style = chars[0].1;
    for (ch, st) in chars {
        if st != style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), style));
            style = st;
        }
        if buf.is_empty() {
            style = st;
        }
        buf.push(ch);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    spans
}

// ---------------------------------------------------------------------------
// Panel panic boundary (L-T7)
// ---------------------------------------------------------------------------

/// Render one panel into a throwaway buffer under `catch_unwind`; on panic,
/// substitute a visible error card. See the module docs for the soundness
/// argument behind `AssertUnwindSafe`.
///
/// The [`crate::term::PanelBoundary`] marker tells the panic hook this panic
/// is caught here (in unwind builds), so it must not restore the terminal
/// mid-session; in abort builds the catch is inert and the hook restores
/// unconditionally — the process is about to die either way.
pub(crate) fn guarded_panel<F>(frame: &mut Frame, area: Rect, label: &str, draw: F)
where
    F: Fn(&mut Buffer),
{
    if area.width == 0 || area.height == 0 {
        return;
    }
    let drawn = {
        let _boundary = crate::term::PanelBoundary::enter();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut buf = Buffer::empty(area);
            draw(&mut buf);
            buf
        }))
    };
    let buf = match drawn {
        Ok(buf) => buf,
        Err(payload) => error_card(area, label, &panic_message(&*payload)),
    };
    blit(frame.buffer_mut(), &buf, area);
}

/// Copy every cell of `src` in `area` into `dst`. Infallible — the only write
/// to the real frame buffer, always after a panel has fully drawn or failed.
fn blit(dst: &mut Buffer, src: &Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = src.cell((x, y)).cloned();
            if let (Some(cell), Some(slot)) = (cell, dst.cell_mut((x, y))) {
                *slot = cell;
            }
        }
    }
}

/// Extract a human message from a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panicked with a non-string payload".to_string()
    }
}

/// A visible red error card standing in for a panel that panicked.
fn error_card(area: Rect, label: &str, message: &str) -> Buffer {
    let mut buf = Buffer::empty(area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Red))
        .title(format!(" ⚠ panel '{label}' panicked "));
    let body = format!("{message}\n\nthe rest of the TUI is still running");
    Paragraph::new(body)
        .block(block)
        .wrap(Wrap { trim: true })
        .style(Style::new().fg(Color::Red).add_modifier(Modifier::BOLD))
        .render(area, &mut buf);
    buf
}

// ---------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------

pub(crate) fn render_hud(hud: &Hud, area: Rect, buf: &mut Buffer) {
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("stage: ", Style::new().fg(Color::DarkGray)),
        Span::styled(
            hud.stage.map(stage_label).unwrap_or("—").to_string(),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("model: ", Style::new().fg(Color::DarkGray)),
        Span::raw(hud.model.clone().unwrap_or_else(|| "—".to_string())),
        Span::raw("   "),
        Span::styled("spend: ", Style::new().fg(Color::DarkGray)),
        Span::styled(
            textline::spend_amount(hud.spent_usd, hud.limit_usd),
            Style::new().fg(spend_color(hud)),
        ),
    ];
    if let Some(mode) = hud.budget_mode {
        spans.push(Span::styled(
            format!(" ({})", budget_mode_label(mode)),
            Style::new().fg(Color::DarkGray),
        ));
    }
    if hud.complete {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            "✓ complete",
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ));
    }
    let block = Block::default().borders(Borders::ALL).title(" stella ");
    Paragraph::new(Line::from(spans))
        .block(block)
        .render(area, buf);
}

pub(crate) fn render_transcript(
    lines: &[Line<'static>],
    window: Range<usize>,
    following: bool,
    area: Rect,
    buf: &mut Buffer,
) {
    let visible: Vec<Line<'static>> = lines
        .get(window.clone())
        .map(<[Line]>::to_vec)
        .unwrap_or_default();
    render_transcript_window(visible, window, lines.len(), following, area, buf);
}

/// [`render_transcript`] for a caller that already materialized just the
/// visible window (the deck's fold cache clones ≤ one viewport of lines per
/// frame instead of the whole history); `total` sizes the title.
pub(crate) fn render_transcript_window(
    visible: Vec<Line<'static>>,
    window: Range<usize>,
    total: usize,
    following: bool,
    area: Rect,
    buf: &mut Buffer,
) {
    let title = if following {
        format!(" transcript · {total} lines · following ")
    } else {
        format!(
            " transcript · {}-{} / {total} ",
            window.start.min(total),
            window.end.min(total)
        )
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    // No wrap: one logical line per row keeps the scroll math line-exact
    // (L-T4); overflow is clipped horizontally, not reflowed.
    Paragraph::new(Text::from(visible))
        .block(block)
        .render(area, buf);
}

fn render_files(
    files: &[FileState],
    selected: usize,
    focus: PanelFocus,
    area: Rect,
    buf: &mut Buffer,
) {
    let title = format!(" files touched · {} ", files.len());
    let block = Block::default().borders(Borders::ALL).title(title);
    let lines: Vec<Line<'static>> = if files.is_empty() {
        vec![Line::from(Span::styled(
            "no files touched yet",
            Style::new().fg(Color::DarkGray),
        ))]
    } else {
        files
            .iter()
            .enumerate()
            .map(|(i, f)| file_line(f, i == selected && focus == PanelFocus::Files))
            .collect()
    };
    Paragraph::new(Text::from(lines))
        .block(block)
        .render(area, buf);
}

/// The diff viewer, PR-style: the full file path inline in a rule above the
/// body, the numbered/styled body in the middle, and a closing rule below
/// counting the additions/removals (`crate::diff` owns all three parts, so
/// this pane and the deck's Files tab read identically).
fn render_diff(
    lines: &[Line<'static>],
    window: Range<usize>,
    path: &str,
    (added, removed): (u32, u32),
    area: Rect,
    buf: &mut Buffer,
) {
    if area.height < 2 {
        return;
    }
    let bands = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    let w = area.width as usize;
    Paragraph::new(diff::header_line(path, w)).render(bands[0], buf);
    let visible: Vec<Line<'static>> = lines.get(window).map(<[Line]>::to_vec).unwrap_or_default();
    Paragraph::new(Text::from(visible)).render(bands[1], buf);
    Paragraph::new(diff::footer_line(added, removed, w)).render(bands[2], buf);
}

pub(crate) fn render_scope_review(
    proposal: &stella_protocol::ScopeProposal,
    answered: bool,
    area: Rect,
    buf: &mut Buffer,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        proposal.summary.clone(),
        Style::new().add_modifier(Modifier::BOLD),
    )));
    let cost = proposal
        .estimated_cost_usd
        .map(|c| format!("  ·  est. cost ~${c:.2}"))
        .unwrap_or_default();
    lines.push(Line::from(Span::styled(
        format!(
            "{} steps  ·  ~{} files{cost}",
            proposal.steps.len(),
            proposal.estimated_files
        ),
        Style::new().fg(Color::DarkGray),
    )));
    lines.push(if answered {
        Line::from(Span::styled(
            "decision sent — awaiting engine…",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ))
    } else {
        Line::from(vec![
            Span::styled(
                "[a]",
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::raw("pprove  "),
            Span::styled(
                "[t]",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw("rim  "),
            Span::styled(
                "[x]",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("abort"),
        ])
    });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Yellow))
        .title(" scope review ");
    Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: true })
        .render(area, buf);
}

pub(crate) fn render_ask_user(
    prompt: &AskUserPrompt,
    answered: bool,
    area: Rect,
    buf: &mut Buffer,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        prompt.question.clone(),
        Style::new().add_modifier(Modifier::BOLD),
    )));
    // The structured options, numbered for quick-pick.
    for (i, option) in prompt.options.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}. ", i + 1),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(option.clone()),
        ]));
    }
    // BINDING: always exactly one additional free-text affordance, on every
    // question, whether or not the model listed one.
    lines.push(if answered {
        Line::from(Span::styled(
            "answer sent — awaiting engine…",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ))
    } else {
        Line::from(Span::styled(
            "  or type your own answer, then Enter",
            Style::new().fg(Color::Green).add_modifier(Modifier::ITALIC),
        ))
    });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" question ");
    Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: true })
        .render(area, buf);
}

/// Where the slash popup floats: anchored to the composer's left edge,
/// opening upward, tall enough for the matches (capped) and clamped to the
/// frame on small terminals.
pub(crate) fn slash_popup_area(root: Rect, composer: Rect, matches: usize) -> Rect {
    let h = ((matches.min(8) as u16) + 3).min(root.height);
    let w = root.width.min(56);
    Rect {
        x: composer.x,
        y: composer.y.saturating_sub(h),
        width: w,
        height: h,
    }
}

/// The floating slash-command menu: an accent-bordered popup with the
/// selected row highlighted and a one-line key legend. Shared by the
/// single-session REPL and the deck (both anchor it above their composer).
pub(crate) fn render_slash_popup(menu: &SlashMenu, selected: usize, area: Rect, buf: &mut Buffer) {
    ratatui::widgets::Clear.render(area, buf);
    let mut lines: Vec<Line<'static>> = menu
        .matches
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let is_sel = i == selected;
            let marker = if is_sel { "▸ " } else { "  " };
            let mut name_style = theme::accent();
            let mut desc_style = theme::muted();
            if is_sel {
                name_style = name_style.add_modifier(Modifier::REVERSED);
                desc_style = desc_style.add_modifier(Modifier::REVERSED);
            }
            Line::from(vec![
                Span::styled(marker.to_string(), name_style),
                Span::styled(format!("{} ", c.kind.glyph()), name_style),
                Span::styled(c.name.clone(), name_style),
                Span::styled("  ", desc_style),
                Span::styled(c.description.clone(), desc_style),
            ])
        })
        .collect();
    lines.push(Line::from(Span::styled(
        " ↑/↓ choose · tab complete · enter run · esc dismiss",
        theme::muted(),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(" / commands · {} ", menu.matches.len()));
    Paragraph::new(Text::from(lines))
        .block(block)
        .render(area, buf);
}

/// Cap on the composer's visible content rows: it grows with the prompt up
/// to this, then scrolls to keep the cursor row in view.
pub(crate) const COMPOSER_MAX_ROWS: usize = 8;

/// The multi-line composer panel. Rows come pre-wrapped from
/// [`crate::composer::layout`]; this draws the capped window that keeps the
/// cursor row visible, with a block cursor at the exact cursor column.
fn render_composer(
    layout: &ComposerLayout,
    blank: bool,
    focused: bool,
    enter_submits: bool,
    area: Rect,
    buf: &mut Buffer,
) {
    let accent = Style::new().fg(if focused {
        Color::Green
    } else {
        Color::DarkGray
    });
    let cursor_style = Style::new()
        .fg(Color::Green)
        .add_modifier(Modifier::REVERSED);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if blank {
        // Empty composer: the cursor block plus a key hint matched to the
        // terminal's Enter semantics.
        let hint = if enter_submits {
            "⏎ send · ⌥⏎ newline"
        } else {
            "⏎ newline · ⌘⏎ send · ⌥[ start · ⌥] end"
        };
        let mut spans = vec![Span::styled("› ", accent)];
        if focused {
            spans.push(Span::styled(" ", cursor_style));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(hint, Style::new().fg(Color::DarkGray)));
        lines.push(Line::from(spans));
    } else {
        let visible = inner_height(area).max(1);
        let first = (layout.cursor_row + 1).saturating_sub(visible);
        for (i, row) in layout.rows.iter().enumerate().skip(first).take(visible) {
            // The prompt glyph marks the first row; continuations align.
            let prefix = if i == 0 { "› " } else { "  " };
            let mut spans = vec![Span::styled(prefix, accent)];
            if focused && i == layout.cursor_row {
                let (before, under, after) = split_row_at(row, layout.cursor_col);
                spans.push(Span::raw(before));
                spans.push(Span::styled(
                    under.map(String::from).unwrap_or_else(|| " ".into()),
                    cursor_style,
                ));
                spans.push(Span::raw(after));
            } else {
                spans.push(Span::raw(row.clone()));
            }
            lines.push(Line::from(spans));
        }
    }
    let block = Block::default().borders(Borders::ALL).title(" prompt ");
    Paragraph::new(Text::from(lines))
        .block(block)
        .render(area, buf);
}

// ---------------------------------------------------------------------------
// Two-column transcript layout
// ---------------------------------------------------------------------------

/// Width of the right-aligned label column: 20 chars for `[name]` (right-aligned),
/// then `:` and one space. Content always begins at column 22.
pub(crate) const LABEL_COL: usize = 22;

/// Format a label as `[name]` right-aligned so content starts at
/// [`LABEL_COL`]. Padding is display-width aware — a wide glyph or emoji in
/// the label must not shift the content column.
fn label_tag(name: &str) -> String {
    let bracketed = format!("[{name}]");
    let pad = (LABEL_COL - 2).saturating_sub(UnicodeWidthStr::width(bracketed.as_str()));
    format!("{}{bracketed}: ", " ".repeat(pad))
}

/// Indent string for wrapped continuation lines — exactly `LABEL_COL` spaces.
fn cont_indent() -> String {
    " ".repeat(LABEL_COL)
}

/// Wrap a single styled line into multiple lines of at most `max_width`,
/// with continuation lines indented by `indent` spaces. The first line
/// passes through unchanged (it already has its label prefix).
fn wrap_one_indent(
    line: Line<'static>,
    max_width: usize,
    indent: usize,
    out: &mut Vec<Line<'static>>,
) {
    let line_width = line.width();
    if line_width <= max_width || max_width == 0 {
        out.push(line);
        return;
    }
    let styled: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();

    let content_width = max_width.saturating_sub(indent);
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut current_w = 0usize;

    let flush = |cur: &mut Vec<(char, Style)>, first: bool, out: &mut Vec<Line<'static>>| {
        if !cur.is_empty() {
            let pairs = std::mem::take(cur);
            if first {
                out.push(Line::from(styled_chars_to_spans(pairs)));
            } else {
                let mut spans = vec![Span::raw(" ".repeat(indent))];
                spans.extend(styled_chars_to_spans(pairs));
                out.push(Line::from(spans));
            }
        }
    };

    let mut is_first = true;
    for (ch, style) in styled {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_w + cw > content_width && !current.is_empty() {
            if let Some(space_idx) = current.iter().rposition(|(c, _)| *c == ' ') {
                let remainder: Vec<(char, Style)> = current.split_off(space_idx);
                flush(&mut current, is_first, out);
                is_first = false;
                current = remainder;
                current_w = current
                    .iter()
                    .map(|(c, _)| UnicodeWidthChar::width(*c).unwrap_or(0))
                    .sum();
            } else {
                flush(&mut current, is_first, out);
                is_first = false;
                current_w = 0;
            }
        }
        current.push((ch, style));
        current_w += cw;
    }
    flush(&mut current, is_first, out);
}

/// Emit one transcript row in the canonical two-column layout: a
/// right-aligned `[label]: ` tag in the gutter, content starting at
/// [`LABEL_COL`], wrap continuations indented to the content column.
///
/// Every `entry_lines` arm MUST route its rows through this (or
/// [`push_labeled_block`] for multi-line content) — no transcript row
/// renders at the left margin. The
/// `every_transcript_entry_renders_in_the_label_gutter` test enforces it.
fn push_labeled(
    label: &str,
    label_style: Style,
    content: Vec<Span<'static>>,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    push_labeled_block(label, label_style, vec![Line::from(content)], width, out);
}

/// Multi-line form of [`push_labeled`]: the tag labels the first line and
/// every following line indents to the content column.
fn push_labeled_block(
    label: &str,
    label_style: Style,
    lines: Vec<Line<'static>>,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    for (i, line) in lines.into_iter().enumerate() {
        let mut spans = if i == 0 {
            vec![Span::styled(label_tag(label), label_style)]
        } else {
            vec![Span::raw(cont_indent())]
        };
        spans.extend(line.spans);
        wrap_one_indent(Line::from(spans), width, LABEL_COL, out);
    }
}

// ---------------------------------------------------------------------------
// Pure content builders (unit-tested directly)
// ---------------------------------------------------------------------------

/// The full visual-line list for the transcript. Each entry is rendered with
/// per-entry wrapping so continuation lines respect the label column.
pub(crate) fn transcript_lines(
    model: &SessionModel,
    expand_thinking: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for entry in &model.transcript {
        entry_lines(entry, expand_thinking, expand_thinking, width, &mut out);
    }
    out
}

pub(crate) fn entry_lines(
    entry: &TranscriptEntry,
    expand_thinking: bool,
    expanded: bool,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    match entry {
        TranscriptEntry::Evicted { count } => out.push(Line::from(Span::styled(
            format!("… {count} earlier entries evicted"),
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ))),
        TranscriptEntry::User(text) => {
            push_labeled_block(
                "user",
                theme::accent(),
                crate::markdown::render(text),
                width,
                out,
            );
        }
        TranscriptEntry::Stage(name) => {
            let style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            push_labeled(
                "stage",
                style,
                vec![Span::styled(stage_label(*name), style)],
                width,
                out,
            );
        }
        TranscriptEntry::Text(text) => {
            push_labeled_block(
                "agent",
                theme::accent(),
                crate::markdown::render(text),
                width,
                out,
            );
        }
        TranscriptEntry::Reasoning(text) => {
            let total_lines = text.lines().count().max(1);
            let show_all = expand_thinking || expanded;
            let chevron = if show_all { "⏶" } else { "⏵" };
            let header_style = Style::new().fg(theme::AMBER);
            let reasoning_style = Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            let mut block = vec![Line::from(Span::styled(
                format!("{total_lines} lines"),
                header_style,
            ))];
            if show_all {
                for l in text.split('\n') {
                    block.push(Line::from(Span::styled(l.to_owned(), reasoning_style)));
                }
            } else {
                let preview_count = 3;
                let mut shown = 0;
                for l in text.lines() {
                    if shown >= preview_count {
                        break;
                    }
                    if !l.trim().is_empty() {
                        block.push(Line::from(Span::styled(l.to_owned(), reasoning_style)));
                        shown += 1;
                    }
                }
                if total_lines > preview_count {
                    block.push(Line::from(Span::styled(
                        "⋯ ctrl+o expands this thought · ctrl+r all",
                        Style::new().fg(Color::DarkGray),
                    )));
                }
            }
            push_labeled_block(
                &format!("{chevron} thinking"),
                header_style,
                block,
                width,
                out,
            );
        }
        TranscriptEntry::ToolStart {
            name, input, raw, ..
        } => {
            push_labeled(
                name,
                Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
                vec![Span::styled(
                    input.clone(),
                    Style::new().fg(Color::DarkGray),
                )],
                width,
                out,
            );
            if expanded {
                // ctrl+o: the full argument object, pretty-printed and dim.
                // An over-budget argument may not parse (char-capped raw) —
                // show it wrapped rather than clipped at the pane edge.
                let pretty = serde_json::from_str::<serde_json::Value>(raw)
                    .and_then(|v| serde_json::to_string_pretty(&v))
                    .unwrap_or_else(|_| raw.clone());
                for l in pretty.lines() {
                    wrap_one_indent(
                        Line::from(Span::styled(
                            format!("    {l}"),
                            Style::new().fg(theme::MUTED),
                        )),
                        width,
                        4,
                        out,
                    );
                }
            }
        }
        TranscriptEntry::ToolResult {
            name,
            ok,
            summary,
            full,
            duration_ms,
            ..
        } => {
            let (glyph, color) = if *ok {
                ("✓", Color::Green)
            } else {
                ("✗", Color::Red)
            };
            // The result labels itself with the tool it answers (resolved
            // from the start entry) so call/result rows read as a pair.
            let label = format!("{glyph} {name}");
            let label_style = Style::new().fg(color).add_modifier(Modifier::BOLD);
            let extra = full.lines().count().saturating_sub(1);
            if expanded {
                push_labeled(
                    &label,
                    label_style,
                    vec![Span::styled(
                        format!("({} lines · {duration_ms}ms)", extra + 1),
                        Style::new().fg(Color::DarkGray),
                    )],
                    width,
                    out,
                );
                for l in full.lines() {
                    wrap_one_indent(
                        Line::from(Span::styled(
                            format!("    {l}"),
                            Style::new().fg(theme::MUTED),
                        )),
                        width,
                        4,
                        out,
                    );
                }
            } else {
                // Collapsed: exactly one output line; the hint names how many
                // more ctrl+o would reveal. Multi-line output NEVER floods the
                // transcript uninvited.
                let first = full
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or(summary.as_str());
                let shown: String = first.chars().take(160).collect();
                let hint = if extra > 0 {
                    format!("  (+{extra} lines · {duration_ms}ms)")
                } else {
                    format!("  ({duration_ms}ms)")
                };
                push_labeled(
                    &label,
                    label_style,
                    vec![
                        Span::raw(shown),
                        Span::styled(hint, Style::new().fg(Color::DarkGray)),
                    ],
                    width,
                    out,
                );
            }
        }
        TranscriptEntry::Retry { attempt, reason } => {
            let style = Style::new().fg(Color::Yellow);
            push_labeled(
                "↻ retry",
                style,
                vec![Span::styled(format!("#{attempt}: {reason}"), style)],
                width,
                out,
            );
        }
        TranscriptEntry::Compaction {
            before_tokens,
            after_tokens,
            evicted,
            deduped,
        } => {
            let style = Style::new().fg(Color::Blue);
            push_labeled(
                "⇣ compacted",
                style,
                vec![Span::styled(
                    format!(
                        "{before_tokens}→{after_tokens} tok (evicted {evicted}, deduped {deduped})"
                    ),
                    style,
                )],
                width,
                out,
            );
        }
        TranscriptEntry::BudgetTick {
            spent_usd,
            limit_usd,
            mode,
        } => {
            let limit = limit_usd.map(|l| format!("/${l:.2}")).unwrap_or_default();
            let style = Style::new().fg(Color::DarkGray);
            push_labeled(
                "spend",
                style,
                vec![Span::styled(
                    format!("${spent_usd:.4}{limit} ({})", budget_mode_label(*mode)),
                    style,
                )],
                width,
                out,
            );
        }
        TranscriptEntry::ProviderFallback { from, to, reason } => {
            let style = Style::new().fg(Color::Magenta);
            push_labeled(
                "⚡ fallback",
                style,
                vec![Span::styled(format!("{from} → {to}: {reason}"), style)],
                width,
                out,
            );
        }
        TranscriptEntry::ContextRecall {
            frames,
            tokens,
            labels,
        } => {
            let cited = labels.join(", ");
            let style = Style::new().fg(Color::Blue);
            push_labeled(
                "◉ recalled",
                style,
                vec![Span::styled(
                    format!("{frames} frames ({tokens} tok): {cited}"),
                    style,
                )],
                width,
                out,
            );
        }
        TranscriptEntry::ContextWrite {
            provider,
            upserts,
            superseded,
        } => {
            let style = Style::new().fg(Color::Blue);
            push_labeled(
                "✎ memory",
                style,
                vec![Span::styled(
                    format!("{upserts} facts ({superseded} superseded) → {provider}"),
                    style,
                )],
                width,
                out,
            );
        }
        TranscriptEntry::MediaProgress {
            artifact_id,
            kind,
            state,
        } => {
            let style = Style::new().fg(Color::Magenta);
            push_labeled(
                "🎞 media",
                style,
                vec![Span::styled(
                    format!("{} {artifact_id}: {state}", media_kind_label(*kind)),
                    style,
                )],
                width,
                out,
            );
        }
        TranscriptEntry::MediaComplete { label, path, kind } => {
            let style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
            push_labeled(
                "🎨 media",
                style,
                vec![
                    Span::styled(format!("{} {label} ", media_kind_label(*kind)), style),
                    Span::styled(path.clone(), Style::new().fg(Color::DarkGray)),
                ],
                width,
                out,
            );
        }
        TranscriptEntry::JudgeVerdict {
            passed,
            summary,
            deterministic,
        } => {
            let (glyph, color) = if *passed {
                ("✓", Color::Green)
            } else {
                ("✗", Color::Red)
            };
            let tag = if *deterministic {
                "deterministic"
            } else {
                "model-judge"
            };
            push_labeled(
                &format!("{glyph} verdict"),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
                vec![
                    Span::styled(format!("[{tag}] "), Style::new().fg(Color::DarkGray)),
                    Span::raw(summary.clone()),
                ],
                width,
                out,
            );
        }
        TranscriptEntry::ScopeReview {
            summary,
            steps,
            estimated_files,
        } => {
            let style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
            push_labeled(
                "⏸ scope",
                style,
                vec![Span::styled(
                    format!("{summary} ({steps} steps, ~{estimated_files} files)"),
                    style,
                )],
                width,
                out,
            );
        }
        TranscriptEntry::AskUser { question, options } => {
            let style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            push_labeled(
                "? ask",
                style,
                vec![Span::styled(
                    format!("{question} ({options} options + free text)"),
                    style,
                )],
                width,
                out,
            );
        }
        TranscriptEntry::Commit { sha, message } => {
            let short = sha.chars().take(9).collect::<String>();
            let style = Style::new().fg(Color::Cyan);
            push_labeled(
                "● commit",
                style,
                vec![
                    Span::styled(format!("{short} "), style),
                    Span::raw(message.clone()),
                ],
                width,
                out,
            );
        }
        TranscriptEntry::Pr { url, status } => {
            let style = Style::new()
                .fg(pr_status_color(*status))
                .add_modifier(Modifier::BOLD);
            push_labeled(
                "⇢ pr",
                style,
                vec![
                    Span::styled(format!("[{}] ", pr_status_label(*status)), style),
                    Span::styled(url.clone(), Style::new().fg(Color::DarkGray)),
                ],
                width,
                out,
            );
        }
        TranscriptEntry::Error { message, retryable } => {
            let tag = if *retryable { " (retryable)" } else { "" };
            let style = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
            push_labeled(
                "✗ error",
                style,
                vec![Span::styled(format!("{message}{tag}"), style)],
                width,
                out,
            );
        }
        TranscriptEntry::Complete { model, cost_usd } => {
            // Fully muted on purpose: completion is a footnote, not an event.
            let style = Style::new().fg(theme::MUTED);
            push_labeled(
                "cost",
                style,
                vec![Span::styled(format!("${cost_usd:.4} · {model}"), style)],
                width,
                out,
            );
        }
    }
}

fn file_line(file: &FileState, selected: bool) -> Line<'static> {
    let (marker, color) = match file.kind {
        FileChangeKind::Created => ("[+]", Color::Green),
        FileChangeKind::Modified => ("[~]", Color::Yellow),
        FileChangeKind::Deleted => ("[-]", Color::Red),
    };
    let count = if file.changes > 1 {
        format!(" ({})", file.changes)
    } else {
        String::new()
    };
    let mut style = Style::new().fg(color);
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Line::from(vec![
        Span::styled(format!("{marker} "), Style::new().fg(color)),
        Span::styled(format!("{}{count}", file.path), style),
    ])
}

// ---------------------------------------------------------------------------
// Labels — wording in `crate::textline`; only palette mapping lives here
// ---------------------------------------------------------------------------

fn spend_color(hud: &Hud) -> Color {
    match hud.limit_usd {
        Some(limit) if limit > 0.0 && hud.spent_usd >= limit => Color::Red,
        Some(limit) if limit > 0.0 && hud.spent_usd >= limit * 0.8 => Color::Yellow,
        _ => Color::Green,
    }
}

#[cfg(test)]
// Test fixtures build a default `UiState` and then poke one or two fields to
// set up a scenario; struct-update syntax for each would only obscure intent.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::composer::{Composer, SlashCommand};
    use proptest::prelude::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use stella_protocol::{
        AgentEvent, BudgetMode, FileChangeKind, ScopeProposal, StageKind, ToolCall, ToolOutput,
    };

    /// Flatten a `TestBackend` buffer to one `String` per row (styling
    /// stripped — content is what we assert on, never raw ANSI, per L-T6).
    fn buffer_rows(buf: &Buffer) -> Vec<String> {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect()
    }

    fn buffer_text(buf: &Buffer) -> String {
        buffer_rows(buf).join("\n")
    }

    fn draw(model: &SessionModel, ui: &mut UiState, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(model, ui, f)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn hud_and_transcript_render_the_event_content() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::Stage {
            name: StageKind::Execute,
        });
        model.apply(&AgentEvent::Text {
            delta: "building the thing".into(),
        });
        model.apply(&AgentEvent::Complete {
            model: "glm-5.2".into(),
            cost_usd: 0.0123,
        });
        let mut ui = UiState::default();
        let text = draw(&model, &mut ui, 100, 30);
        assert!(text.contains("glm-5.2"), "HUD shows the model:\n{text}");
        assert!(
            text.contains("building the thing"),
            "transcript shows text:\n{text}"
        );
        assert!(text.contains("complete"), "shows completion:\n{text}");
    }

    /// Every transcript entry renders through the shared label gutter: its
    /// first line starts with a right-aligned `[label]: ` tag whose display
    /// width is exactly `LABEL_COL`. Nothing prints at the left margin.
    #[test]
    fn every_transcript_entry_renders_in_the_label_gutter() {
        let samples = vec![
            TranscriptEntry::User("hi".into()),
            TranscriptEntry::Stage(StageKind::Execute),
            TranscriptEntry::Text("ok".into()),
            TranscriptEntry::Reasoning("hmm".into()),
            TranscriptEntry::ToolStart {
                call_id: "c1".into(),
                name: "bash".into(),
                input: "ls".into(),
                raw: "{}".into(),
                path: None,
            },
            TranscriptEntry::ToolResult {
                call_id: "c1".into(),
                name: "bash".into(),
                ok: true,
                summary: "done".into(),
                full: "done".into(),
                duration_ms: 3,
                diff: None,
            },
            TranscriptEntry::Retry {
                attempt: 1,
                reason: "rate limit".into(),
            },
            TranscriptEntry::Compaction {
                before_tokens: 10,
                after_tokens: 5,
                evicted: 1,
                deduped: 2,
            },
            TranscriptEntry::BudgetTick {
                spent_usd: 0.01,
                limit_usd: Some(1.0),
                mode: BudgetMode::Observed,
            },
            TranscriptEntry::ProviderFallback {
                from: "a".into(),
                to: "b".into(),
                reason: "down".into(),
            },
            TranscriptEntry::ContextRecall {
                frames: 2,
                tokens: 120,
                labels: vec!["adr".into()],
            },
            TranscriptEntry::ContextWrite {
                provider: "mem".into(),
                upserts: 2,
                superseded: 1,
            },
            TranscriptEntry::MediaProgress {
                artifact_id: "m1".into(),
                kind: MediaKind::Image,
                state: "queued".into(),
            },
            TranscriptEntry::MediaComplete {
                label: "logo".into(),
                path: "out.png".into(),
                kind: MediaKind::Image,
            },
            TranscriptEntry::JudgeVerdict {
                passed: true,
                summary: "ok".into(),
                deterministic: true,
            },
            TranscriptEntry::ScopeReview {
                summary: "auth".into(),
                steps: 2,
                estimated_files: 3,
            },
            TranscriptEntry::AskUser {
                question: "which db?".into(),
                options: 2,
            },
            TranscriptEntry::Commit {
                sha: "abc123def456".into(),
                message: "fix".into(),
            },
            TranscriptEntry::Pr {
                url: "https://example.test/pr/1".into(),
                status: PrStatus::Open,
            },
            TranscriptEntry::Error {
                message: "boom".into(),
                retryable: false,
            },
            TranscriptEntry::Complete {
                model: "glm-5.2".into(),
                cost_usd: 0.1,
            },
        ];
        for entry in &samples {
            // Exhaustive on purpose: adding a `TranscriptEntry` variant fails
            // to compile here — add a sample above and render the new arm
            // through `push_labeled`/`push_labeled_block`.
            match entry {
                TranscriptEntry::User(_)
                | TranscriptEntry::Stage(_)
                | TranscriptEntry::Text(_)
                | TranscriptEntry::Reasoning(_)
                | TranscriptEntry::ToolStart { .. }
                | TranscriptEntry::ToolResult { .. }
                | TranscriptEntry::Retry { .. }
                | TranscriptEntry::Compaction { .. }
                | TranscriptEntry::BudgetTick { .. }
                | TranscriptEntry::ProviderFallback { .. }
                | TranscriptEntry::ContextRecall { .. }
                | TranscriptEntry::ContextWrite { .. }
                | TranscriptEntry::MediaProgress { .. }
                | TranscriptEntry::MediaComplete { .. }
                | TranscriptEntry::JudgeVerdict { .. }
                | TranscriptEntry::ScopeReview { .. }
                | TranscriptEntry::AskUser { .. }
                | TranscriptEntry::Commit { .. }
                | TranscriptEntry::Pr { .. }
                | TranscriptEntry::Error { .. }
                | TranscriptEntry::Complete { .. } => {}
            }
            let mut lines = Vec::new();
            entry_lines(entry, false, false, 0, &mut lines);
            let first = lines
                .first()
                .unwrap_or_else(|| panic!("{entry:?} renders no lines"));
            let tag = first.spans.first().expect("first span is the label tag");
            assert!(
                tag.content.ends_with("]: "),
                "{entry:?} must start with a `[label]: ` tag, got {:?}",
                tag.content
            );
            assert_eq!(
                UnicodeWidthStr::width(tag.content.as_ref()),
                LABEL_COL,
                "{entry:?} label tag must place content at LABEL_COL, got {:?}",
                tag.content
            );
        }
    }

    #[test]
    fn files_panel_lists_touched_files_by_label() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::FileChange {
            path: "src/driver.rs".into(),
            kind: FileChangeKind::Modified,
            diff: Some("@@\n-old\n+new".into()),
        });
        let mut ui = UiState::default();
        let text = draw(&model, &mut ui, 100, 20);
        assert!(text.contains("src/driver.rs"), "files panel:\n{text}");
        assert!(text.contains("files touched"), "panel title:\n{text}");
    }

    #[test]
    fn diff_viewer_shows_the_selected_files_diff() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::FileChange {
            path: "a.rs".into(),
            kind: FileChangeKind::Modified,
            diff: Some("@@ -1 +1 @@\n-removed\n+added".into()),
        });
        let mut ui = UiState::default();
        ui.diff_open = true;
        let text = draw(&model, &mut ui, 100, 20);
        assert!(text.contains("removed"), "diff shows removals:\n{text}");
        assert!(text.contains("added"), "diff shows additions:\n{text}");
        // The PR-style chrome: the path rides the top rule, the line counts
        // ride the bottom rule.
        assert!(text.contains("a.rs"), "path in the header rule:\n{text}");
        assert!(
            text.contains("+1 addition") && text.contains("-1 removal"),
            "counts in the footer rule:\n{text}"
        );
    }

    #[test]
    fn thinking_is_collapsed_by_default_and_expands_with_the_toggle() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::Reasoning {
            delta: "alpha\nbeta\ngamma".into(),
        });
        let mut ui = UiState::default();
        let collapsed = draw(&model, &mut ui, 100, 24);
        assert!(
            collapsed.contains("[⏵ thinking]: 3 lines"),
            "collapsed header:\n{collapsed}"
        );
        // Collapsed = header + 3 preview lines (all 3 fit within preview count).
        let c_lines = transcript_lines(&model, false, 0);
        assert_eq!(c_lines.len(), 4, "header + 3 preview lines");
        // Preview shows the reasoning content.
        let c_text: String = c_lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            c_text.contains("alpha"),
            "collapsed shows first line:\n{c_text}"
        );

        ui.thinking_expanded = true;
        let expanded = draw(&model, &mut ui, 100, 24);
        assert!(
            expanded.contains("alpha") && expanded.contains("gamma"),
            "expanded shows the full reasoning:\n{expanded}"
        );
        // Expanded = header + 3 content lines.
        assert_eq!(transcript_lines(&model, true, 0).len(), 4);
    }

    #[test]
    fn collapsed_thinking_shows_preview_lines_not_the_full_wall() {
        let mut model = SessionModel::new();
        let long = format!("{}THE-TAIL", "reasoning noise ".repeat(20));
        model.apply(&AgentEvent::Reasoning { delta: long });
        let lines = transcript_lines(&model, false, 0);
        // 1 line of text → header + 1 preview = 2 lines.
        assert_eq!(lines.len(), 2);
        // The preview should be visible.
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("reasoning"), "preview shows content:\n{text}");
    }

    #[test]
    fn collapsed_thinking_preview_updates_as_deltas_stream() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::Reasoning {
            delta: "planning the refactor".into(),
        });
        let before = transcript_lines(&model, false, 0);
        model.apply(&AgentEvent::Reasoning {
            delta: " now checking tests".into(),
        });
        let after = transcript_lines(&model, false, 0);
        // Both produce header + 1 preview = 2 lines.
        assert_eq!(before.len(), 2);
        assert_eq!(after.len(), 2, "still header + 1 preview line");
        // The preview line (index 1) visibly changes with each delta.
        assert_ne!(
            before[1], after[1],
            "the preview visibly changes with each delta"
        );
    }

    #[test]
    fn scope_card_renders_the_decision_legend_when_unanswered() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::ScopeReview {
            proposal: ScopeProposal {
                summary: "refactor auth".into(),
                steps: vec!["s1".into(), "s2".into()],
                estimated_files: 9,
                estimated_cost_usd: Some(1.5),
            },
        });
        let mut ui = UiState::default();
        let text = draw(&model, &mut ui, 100, 30);
        assert!(text.contains("refactor auth"), "card summary:\n{text}");
        assert!(text.contains("pprove"), "shows approve legend:\n{text}");
        // Once answered, the legend flips to the awaiting message.
        ui.scope_answered = true;
        let text2 = draw(&model, &mut ui, 100, 30);
        assert!(text2.contains("awaiting"), "flips to awaiting:\n{text2}");
    }

    #[test]
    fn ask_user_card_always_offers_a_free_text_affordance() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::AskUser {
            id: "q1".into(),
            question: "which database?".into(),
            options: vec!["postgres".into(), "sqlite".into()],
        });
        let mut ui = UiState::default();
        let text = draw(&model, &mut ui, 100, 30);
        assert!(text.contains("which database?"), "question:\n{text}");
        assert!(text.contains("postgres"), "option 1:\n{text}");
        assert!(text.contains("sqlite"), "option 2:\n{text}");
        // The binding renderer contract: a free-text affordance every time.
        assert!(
            text.contains("type your own answer"),
            "free-text option:\n{text}"
        );
    }

    #[test]
    fn slash_menu_renders_filtered_commands() {
        let model = SessionModel::new();
        let mut composer = Composer::new();
        for c in "/di".chars() {
            composer.insert_char(c);
        }
        let mut ui = UiState::new(
            composer,
            vec![
                SlashCommand::new("/diff", "open the diff viewer"),
                SlashCommand::new("/files", "focus files"),
            ],
        );
        let text = draw(&model, &mut ui, 100, 30);
        assert!(text.contains("/diff"), "slash menu lists /diff:\n{text}");
        assert!(!text.contains("/files"), "filtered out /files:\n{text}");
    }

    #[test]
    fn slash_popup_marks_the_selected_command() {
        let model = SessionModel::new();
        let mut composer = Composer::new();
        composer.insert_char('/');
        let mut ui = UiState::new(
            composer,
            vec![
                SlashCommand::new("/help", "show help"),
                SlashCommand::new("/diff", "open the diff viewer"),
            ],
        );
        ui.slash_selected = 1;
        let text = draw(&model, &mut ui, 100, 30);
        // The glyph is double-width: its trailing filler cell dumps as an
        // extra space before the explicit separator.
        assert!(
            text.contains("▸ 🔒  /diff"),
            "selection marker + builtin glyph on the second row:\n{text}"
        );
        assert!(
            text.contains("/ commands · 2"),
            "popup title with the match count:\n{text}"
        );
    }

    #[test]
    fn slash_popup_glyphs_distinguish_builtin_from_custom_commands() {
        let model = SessionModel::new();
        let mut composer = Composer::new();
        composer.insert_char('/');
        let mut ui = UiState::new(
            composer,
            vec![
                SlashCommand::new("/help", "show help"),
                SlashCommand::custom("/fix-bug", "fix the named bug"),
            ],
        );
        let text = draw(&model, &mut ui, 100, 30);
        assert!(
            text.contains("🔒  /help"),
            "productized commands carry the lock glyph:\n{text}"
        );
        assert!(
            text.contains("⚡  /fix-bug"),
            "custom commands carry the lightning glyph:\n{text}"
        );
    }

    #[test]
    fn transcript_scrolls_line_exact_to_show_the_tail() {
        let mut model = SessionModel::new();
        for i in 0..200 {
            model.apply(&AgentEvent::Text {
                delta: format!("LINE{i:03}\n"),
            });
        }
        // The trailing streaming text is one entry with 200 embedded newlines
        // → 201 visual lines; following must land on the last of them.
        let mut ui = UiState::default();
        let text = draw(&model, &mut ui, 80, 20);
        assert!(
            text.contains("LINE199"),
            "tail is visible while following:\n{text}"
        );
        assert!(!text.contains("LINE000"), "head is scrolled off:\n{text}");
    }

    #[test]
    fn a_panicking_panel_becomes_an_error_card_and_input_stays_alive() {
        // L-T7: force a panel to panic via a panicking draw closure and prove
        // (a) it renders as a visible error card, (b) a sibling panel still
        // renders normally, and (c) the pure input path still processes keys.
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|f| {
                let cols = Layout::horizontal([Constraint::Percentage(50); 2]).split(f.area());
                guarded_panel(f, cols[0], "boom", |_buf| panic!("kaboom in a panel"));
                guarded_panel(f, cols[1], "ok", |buf| {
                    Paragraph::new("still-alive")
                        .block(Block::default().borders(Borders::ALL))
                        .render(cols[1], buf);
                });
            })
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("panicked"), "error card is visible:\n{text}");
        assert!(
            text.contains("kaboom"),
            "carries the panic message:\n{text}"
        );
        assert!(
            text.contains("still-alive"),
            "sibling panel unaffected:\n{text}"
        );

        // Input handling is entirely independent of rendering and keeps
        // working — the app did not die.
        use crate::ui::{ShellAction, handle_key};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let model = SessionModel::new();
        let mut ui = UiState::default();
        let action = handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
            &model,
            &mut ui,
        );
        assert_eq!(action, ShellAction::Handled);
        assert_eq!(ui.composer.buffer(), "z");
    }

    #[test]
    fn tool_cards_and_verdicts_style_content_deterministically() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::ToolStart {
            call: ToolCall {
                call_id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "x"}),
            },
        });
        model.apply(&AgentEvent::ToolResult {
            call_id: "c1".into(),
            output: ToolOutput::Error {
                message: "not found".into(),
            },
            duration_ms: 12,
        });
        let lines = transcript_lines(&model, false, 0);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect();
        assert!(joined.contains("read_file"));
        assert!(joined.contains("not found"));
        assert!(joined.contains("12ms"));
        // The result row resolves its tool name from the call so the two
        // rows read as an aligned pair.
        assert!(
            joined.contains("[✗ read_file]"),
            "result labels itself with its tool: {joined}"
        );
    }

    #[test]
    fn eviction_marker_renders_as_a_one_line_system_note() {
        let mut out = Vec::new();
        entry_lines(
            &TranscriptEntry::Evicted { count: 1234 },
            false,
            false,
            80,
            &mut out,
        );
        assert_eq!(out.len(), 1, "the marker costs exactly one visual row");
        let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "… 1234 earlier entries evicted");
    }

    // ---- Replay determinism (L-T1) ------------------------------------

    /// A small event strategy over a representative spread of variants.
    fn any_event() -> impl Strategy<Value = AgentEvent> {
        prop_oneof![
            "[a-z ]{0,12}".prop_map(|delta| AgentEvent::Text { delta }),
            any::<u8>().prop_map(|n| AgentEvent::Stage {
                name: match n % 4 {
                    0 => StageKind::Triage,
                    1 => StageKind::Plan,
                    2 => StageKind::Execute,
                    _ => StageKind::Verify,
                },
            }),
            ("[a-z/.]{1,10}", any::<bool>()).prop_map(|(path, created)| AgentEvent::FileChange {
                path,
                kind: if created {
                    FileChangeKind::Created
                } else {
                    FileChangeKind::Modified
                },
                diff: Some("@@\n-a\n+b".into()),
            }),
            (any::<f64>(), any::<f64>()).prop_map(|(a, b)| AgentEvent::BudgetTick {
                spent_usd: a.abs() % 10.0,
                limit_usd: Some(b.abs() % 10.0),
                mode: BudgetMode::Observed,
            }),
            Just(AgentEvent::Complete {
                model: "glm".into(),
                cost_usd: 0.01,
            }),
        ]
    }

    proptest! {
        /// The core L-T1 guarantee: folding the same event vector into two
        /// fresh models and rendering both yields byte-identical backing cell
        /// buffers. State derived from the log cannot drift.
        #[test]
        fn replaying_a_log_renders_identical_buffers(events in prop::collection::vec(any_event(), 0..40)) {
            let mut a = UiState::default();
            let mut b = UiState::default();
            let model_a = SessionModel::replay(&events);
            let model_b = SessionModel::replay(&events);

            let mut ta = Terminal::new(TestBackend::new(90, 24)).unwrap();
            let mut tb = Terminal::new(TestBackend::new(90, 24)).unwrap();
            ta.draw(|f| render(&model_a, &mut a, f)).unwrap();
            tb.draw(|f| render(&model_b, &mut b, f)).unwrap();

            prop_assert_eq!(
                buffer_rows(ta.backend().buffer()),
                buffer_rows(tb.backend().buffer())
            );
        }
    }
}
