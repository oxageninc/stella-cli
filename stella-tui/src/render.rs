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

use stella_protocol::{FileChangeKind, PrStatus};

use crate::composer::{ComposerLayout, SlashMenu, layout as composer_layout, split_row_at};
use crate::model::{AskUserPrompt, FileState, Hud, SessionModel, TranscriptEntry};
use crate::textline::{
    self, budget_mode_label, media_kind_label, media_state_label, pr_status_label, stage_label,
};
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

    let expand_thinking = ui.thinking_expanded;
    let t_width = inner_width(transcript_area);
    ui.ensure_transcript_lines(model, expand_thinking, t_width);
    let t_lines = ui.transcript_lines();
    let t_total = t_lines.len();
    let t_inner_h = inner_height(transcript_area);
    let t_window = ui.scroll.window(t_total, t_inner_h);
    let following = ui.scroll.follow;
    guarded_panel(frame, transcript_area, "transcript", |buf| {
        render_transcript(t_lines, t_window.clone(), following, transcript_area, buf)
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
        Span::styled("stage: ", Style::new().fg(theme::MUTED)),
        Span::styled(
            hud.stage.map(stage_label).unwrap_or("—").to_string(),
            Style::new().fg(theme::AMBER).add_modifier(Modifier::BOLD),
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
    render_transcript_window(visible, window, lines.len(), following, None, area, buf);
}

/// [`render_transcript`] for a caller that already materialized just the
/// visible window (the deck's fold cache clones ≤ one viewport of lines per
/// frame instead of the whole history); `total` sizes the title. `hint`, when
/// set, renders as a dim bottom title — the contextual "what can I press
/// here" line the deck varies with the transcript's interaction state.
pub(crate) fn render_transcript_window(
    visible: Vec<Line<'static>>,
    window: Range<usize>,
    total: usize,
    following: bool,
    hint: Option<&str>,
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
    let mut block = Block::default().borders(Borders::ALL).title(title);
    if let Some(hint) = hint {
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" {hint} "),
                Style::new().fg(Color::DarkGray),
            ))
            .right_aligned(),
        );
    }
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
                Style::new().fg(theme::AMBER).add_modifier(Modifier::BOLD),
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
        .border_style(Style::new().fg(theme::AMBER))
        .title(" question ");
    Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: true })
        .render(area, buf);
}

/// Most command rows the slash popup shows at once before it scrolls. The
/// list grows to this, then windows around the selection (see
/// [`scroll_window_start`]) so ↑/↓ can walk a long menu without the highlight
/// ever leaving the frame.
pub(crate) const SLASH_POPUP_MAX_ROWS: usize = 8;

/// Where the slash popup floats: anchored to the composer's left edge,
/// opening upward, tall enough for the matches (capped at
/// [`SLASH_POPUP_MAX_ROWS`]) and clamped to the frame on small terminals. The
/// `+3` reserves the two borders and the one-line key legend.
pub(crate) fn slash_popup_area(root: Rect, composer: Rect, matches: usize) -> Rect {
    let h = ((matches.min(SLASH_POPUP_MAX_ROWS) as u16) + 3).min(root.height);
    let w = root.width.min(56);
    Rect {
        x: composer.x,
        y: composer.y.saturating_sub(h),
        width: w,
        height: h,
    }
}

/// The first visible row of a scrolling list of `len` rows that shows
/// `visible` at a time, chosen so `selected` stays on screen — the window
/// only moves once the selection would fall off an edge. Mirrors the
/// composer's cursor-row windowing ([`render_composer`]) so the slash popup
/// and the textarea scroll with identical feel.
pub(crate) fn scroll_window_start(len: usize, selected: usize, visible: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    let selected = selected.min(len - 1);
    // Keep `selected` inside [first, first + visible); clamp so the last
    // window never shows blank rows past the end.
    (selected + 1).saturating_sub(visible).min(len - visible)
}

/// The floating slash-command menu: an accent-bordered popup with the
/// selected row highlighted and a one-line key legend. Shared by the
/// single-session REPL and the deck (both anchor it above their composer).
///
/// When more commands match than fit, the rows window around `selected` so
/// arrow-key navigation always keeps the highlight visible, and the legend
/// shows how many rows are hidden above (`▲`) / below (`▼`).
pub(crate) fn render_slash_popup(menu: &SlashMenu, selected: usize, area: Rect, buf: &mut Buffer) {
    ratatui::widgets::Clear.render(area, buf);
    let total = menu.matches.len();
    let selected = selected.min(total.saturating_sub(1));
    // The interior minus the legend line is what the command rows scroll in.
    let visible = inner_height(area).saturating_sub(1).max(1);
    let first = scroll_window_start(total, selected, visible);
    let last = (first + visible).min(total);
    let mut lines: Vec<Line<'static>> = menu.matches[first..last]
        .iter()
        .enumerate()
        .map(|(offset, c)| {
            let is_sel = first + offset == selected;
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
    let hidden_above = first;
    let hidden_below = total.saturating_sub(last);
    let legend = if hidden_above > 0 || hidden_below > 0 {
        // Compact when scrolling so the ▲/▼ affordance still fits the width.
        format!(" ↑↓ choose · tab fill · ⏎ run · esc · ▲{hidden_above} ▼{hidden_below}")
    } else {
        " ↑/↓ choose · tab complete · enter run · esc dismiss".to_string()
    };
    lines.push(Line::from(Span::styled(legend, theme::muted())));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(" / commands · {total} "));
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
            "⏎ send · ⌘⏎ newline · ⌥[ start · ⌥] end"
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
                let mut remainder: Vec<(char, Style)> = current.split_off(space_idx);
                // Consume the wrap-boundary whitespace so the continuation line
                // starts flush at the indent column. Left in place, the leading
                // space stacks on top of `indent` and pushes every wrapped row
                // one column right of the clean left edge — the "extra blank
                // space after the colon" bug.
                let lead = remainder.iter().take_while(|(c, _)| *c == ' ').count();
                remainder.drain(..lead);
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

/// Emit one expanded-detail row (a ctrl+o body line) at the content column.
/// Detail rows sit directly under their parent row's content — aligned to
/// [`LABEL_COL`] exactly like wrap continuations — never at the left margin,
/// so an expanded body reads as part of the same two-column layout.
fn push_detail_line(text: &str, width: usize, out: &mut Vec<Line<'static>>) {
    wrap_one_indent(
        Line::from(vec![
            Span::raw(cont_indent()),
            Span::styled(text.to_owned(), Style::new().fg(theme::MUTED)),
        ]),
        width,
        LABEL_COL,
        out,
    );
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
            // The one transcript entry rendered in a single color end to end:
            // the `[user]:` tag and every line of the prompt ride the same
            // violet as the composer's keybind glyphs and the
            // "deterministic-first" chip (`deck_render`) — the interactive-
            // chrome accent, never the ember heat. Rendered as plain lines
            // (not markdown) so nothing tints part of the prompt a 2nd color.
            let violet = Style::new().fg(theme::VIOLET);
            let lines: Vec<Line<'static>> = text
                .split('\n')
                .map(|l| Line::from(Span::styled(l.to_owned(), violet)))
                .collect();
            push_labeled_block("user", violet, lines, width, out);
        }
        TranscriptEntry::Stage(name) => {
            let style = Style::new()
                .fg(theme::EMBER_FLAME)
                .add_modifier(Modifier::BOLD);
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
            let header_style = Style::new().fg(theme::AGENT_AMBER);
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
                Style::new()
                    .fg(theme::EMBER_FLAME)
                    .add_modifier(Modifier::BOLD),
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
                    push_detail_line(l, width, out);
                }
            }
        }
        TranscriptEntry::ToolResult {
            name,
            ok,
            summary,
            full,
            duration_ms,
            speculated,
            ..
        } => {
            let (glyph, color) = if *ok {
                ("✓", theme::EMBER_GOLD)
            } else {
                ("✗", theme::EMBER_CRIMSON)
            };
            // The result labels itself with the tool it answers (resolved
            // from the start entry) so call/result rows read as a pair.
            let label = format!("{glyph} {name}");
            let label_style = Style::new().fg(color).add_modifier(Modifier::BOLD);
            let extra = full.lines().count().saturating_sub(1);
            // ⚡ marks a speculated result: the duration overlapped the
            // model's own streaming instead of following it.
            let dur = if *speculated {
                format!("⚡{duration_ms}ms")
            } else {
                format!("{duration_ms}ms")
            };
            if expanded {
                push_labeled(
                    &label,
                    label_style,
                    vec![Span::styled(
                        format!("({} lines · {dur})", extra + 1),
                        Style::new().fg(Color::DarkGray),
                    )],
                    width,
                    out,
                );
                for l in full.lines() {
                    push_detail_line(l, width, out);
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
                    format!("  (+{extra} lines · {dur})")
                } else {
                    format!("  ({dur})")
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
            let style = Style::new().fg(theme::WARNING_BRIGHT);
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
            let style = Style::new().fg(theme::EMBER_FLAME);
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
            let style = Style::new().fg(theme::WARNING);
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
            let style = Style::new().fg(theme::EMBER_FLAME);
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
            let style = Style::new().fg(theme::EMBER_FLAME);
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
            let style = Style::new().fg(theme::EMBER_FLAME);
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
            let style = Style::new().fg(theme::EMBER_FLAME);
            push_labeled(
                "🎞 media",
                style,
                vec![Span::styled(
                    format!(
                        "{} {artifact_id}: {}",
                        media_kind_label(*kind),
                        media_state_label(state)
                    ),
                    style,
                )],
                width,
                out,
            );
        }
        TranscriptEntry::MediaComplete { label, path, kind } => {
            let style = Style::new()
                .fg(theme::EMBER_FLAME)
                .add_modifier(Modifier::BOLD);
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
                ("✓", theme::EMBER_GOLD)
            } else {
                ("✗", theme::EMBER_CRIMSON)
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
            let style = Style::new()
                .fg(theme::WARNING_BRIGHT)
                .add_modifier(Modifier::BOLD);
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
            let style = Style::new()
                .fg(theme::WARNING_BRIGHT)
                .add_modifier(Modifier::BOLD);
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
            let style = Style::new().fg(theme::EMBER_FLAME);
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
            let style = Style::new()
                .fg(theme::EMBER_CRIMSON)
                .add_modifier(Modifier::BOLD);
            push_labeled(
                "✗ error",
                style,
                vec![Span::styled(format!("{message}{tag}"), style)],
                width,
                out,
            );
        }
        TranscriptEntry::Complete { model, cost_usd } => {
            // A quiet footnote, not an event — the dimmest ember tier keeps it
            // inside the warm family without shouting.
            let style = Style::new().fg(theme::WARNING);
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

fn pr_status_color(status: PrStatus) -> Color {
    // Kept inside the ember family so the `[⇢ pr]:` gutter reads with the rest
    // of the transcript: quiet amber draft, flame while open, gold on merge,
    // crimson on close.
    match status {
        PrStatus::Draft => theme::WARNING,
        PrStatus::Open => theme::EMBER_FLAME,
        PrStatus::Merged => theme::EMBER_GOLD,
        PrStatus::Closed => theme::EMBER_CRIMSON,
    }
}

fn file_line(file: &FileState, selected: bool) -> Line<'static> {
    let (marker, color) = match file.kind {
        FileChangeKind::Read => ("[r]", Color::DarkGray),
        FileChangeKind::Created => ("[+]", Color::Green),
        FileChangeKind::Modified => ("[~]", Color::Yellow),
        FileChangeKind::Deleted => ("[-]", Color::Red),
    };
    let mut count = if file.changes > 1 {
        format!(" ({})", file.changes)
    } else {
        String::new()
    };
    if file.reads > 0 {
        count.push_str(&format!(" ·r{}", file.reads));
    }
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
        AgentEvent, BudgetMode, FileChangeKind, MediaJobState, MediaKind, ScopeProposal, StageKind,
        ToolCall, ToolOutput,
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
                speculated: false,
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
                state: MediaJobState::Queued,
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
                // Not in `samples`: it deliberately renders as an untagged
                // system note, not a `[label]: ` line — see
                // `eviction_marker_renders_as_a_one_line_system_note`.
                | TranscriptEntry::Evicted { .. }
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

    /// A wrapped continuation line begins flush at the content column: exactly
    /// `LABEL_COL` leading spaces, never one more. Regression for the bug where
    /// the wrap-boundary space was carried onto the next line, stacking on top
    /// of the indent and drifting every wrapped row one column right of the
    /// clean left edge (the "extra blank space after the colon on wrap" report).
    #[test]
    fn wrapped_continuation_starts_flush_at_the_content_column() {
        let content = "the quick brown fox jumps over the lazy dog and then keeps \
                       on running well past the right edge to force several wraps";
        let spans = vec![Span::raw(label_tag("agent")), Span::raw(content)];
        let mut out = Vec::new();
        // Narrow width so the content wraps several times.
        wrap_one_indent(Line::from(spans), 60, LABEL_COL, &mut out);

        assert!(
            out.len() > 1,
            "content must wrap into a continuation row, got {} row(s)",
            out.len()
        );
        for (i, line) in out.iter().enumerate().skip(1) {
            let text: String = line.spans.iter().flat_map(|s| s.content.chars()).collect();
            let leading = text.chars().take_while(|c| *c == ' ').count();
            assert_eq!(
                leading, LABEL_COL,
                "continuation row {i} must start exactly at the content column \
                 (indent {LABEL_COL}, no carried wrap space); got {leading}: {text:?}",
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

    // ---- Slash-popup windowing ---------------------------------------------

    #[test]
    fn scroll_window_start_holds_still_until_the_selection_leaves_the_edge() {
        // Fits entirely: never scrolls.
        assert_eq!(scroll_window_start(5, 4, 8), 0);
        // Selection inside the first window: no movement.
        assert_eq!(scroll_window_start(20, 0, 8), 0);
        assert_eq!(scroll_window_start(20, 7, 8), 0);
        // One past the window's last row: scroll down by one.
        assert_eq!(scroll_window_start(20, 8, 8), 1);
        // The tail clamps so the final window is full, never blank-padded.
        assert_eq!(scroll_window_start(20, 19, 8), 12);
        // Selecting back at the top pulls the window all the way up.
        assert_eq!(scroll_window_start(20, 0, 8), 0);
        // A stale selection past the end (e.g. the filter just shrank the
        // match list) clamps to the last full window instead of panicking.
        assert_eq!(scroll_window_start(20, 999, 8), 12);
        // Degenerate inputs don't panic.
        assert_eq!(scroll_window_start(0, 0, 8), 0);
        assert_eq!(scroll_window_start(5, 0, 0), 0);
    }

    /// Rendering a slash popup taller than its window keeps the *selected*
    /// row on screen and pushes the ones scrolled past off it — the concrete
    /// symptom of the un-windowed version (selection navigable but invisible).
    #[test]
    fn slash_popup_windows_the_selection_into_view() {
        let cmds: Vec<SlashCommand> = (0..15)
            .map(|i| SlashCommand::new(format!("/cmd{i:02}"), "desc"))
            .collect();
        let menu = SlashMenu::filter(&cmds, "/");
        let area = Rect {
            x: 0,
            y: 0,
            width: 56,
            height: (SLASH_POPUP_MAX_ROWS as u16) + 3,
        };
        // Select the very last command: without windowing it renders off the
        // bottom of the popup box and never appears in the buffer.
        let mut buf = Buffer::empty(area);
        render_slash_popup(&menu, 14, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("/cmd14"), "selected row is visible:\n{text}");
        assert!(
            !text.contains("/cmd00"),
            "the top rows scrolled out of view:\n{text}"
        );
        // The legend advertises the hidden rows above.
        assert!(text.contains('▲'), "scroll affordance shown:\n{text}");

        // Selecting the top shows the head and hides the tail instead.
        let mut buf = Buffer::empty(area);
        render_slash_popup(&menu, 0, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("/cmd00"), "top row visible:\n{text}");
        assert!(!text.contains("/cmd14"), "tail hidden:\n{text}");
        assert!(text.contains('▼'), "hidden-below affordance shown:\n{text}");
    }

    /// A stale-high selection (the match list shrank under the cursor before
    /// the upstream clamp caught up) must still render a sane, in-bounds
    /// window rather than panic on the slice.
    #[test]
    fn slash_popup_survives_a_selection_past_the_filtered_end() {
        let cmds: Vec<SlashCommand> = (0..3)
            .map(|i| SlashCommand::new(format!("/cmd{i:02}"), "desc"))
            .collect();
        let menu = SlashMenu::filter(&cmds, "/");
        let area = Rect {
            x: 0,
            y: 0,
            width: 56,
            height: (SLASH_POPUP_MAX_ROWS as u16) + 3,
        };
        let mut buf = Buffer::empty(area);
        // selected far past the 3 matches — the render-side clamp keeps it in
        // view; all three rows fit so nothing scrolls.
        render_slash_popup(&menu, 99, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("/cmd02"), "last row still shown:\n{text}");
        assert!(
            !text.contains('▲') && !text.contains('▼'),
            "short list shows no scroll affordance:\n{text}"
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
    fn ui_memoizes_transcript_lines_and_invalidates_on_a_streaming_delta() {
        // The transcript re-wrap is O(transcript) and ran EVERY frame — a
        // session redraws far more often than it changes. The UiState cache
        // must (a) reuse the parsed lines on an unchanged frame, and (b) still
        // invalidate when a streaming delta grows the trailing entry (its
        // length changes but the entry count does not), or new tokens would
        // never appear.
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::Text {
            delta: "a fairly long assistant message that wraps at a narrow width".into(),
        });
        let mut ui = UiState::default();

        // First frame populates the cache, and it matches a direct render.
        ui.ensure_transcript_lines(&model, false, 40);
        let first = ui.transcript_lines().to_vec();
        let ptr = ui.transcript_lines().as_ptr();
        assert_eq!(first, transcript_lines(&model, false, 40));

        // An unchanged frame reuses the SAME backing allocation — no re-wrap.
        ui.ensure_transcript_lines(&model, false, 40);
        assert_eq!(
            ui.transcript_lines().as_ptr(),
            ptr,
            "an unchanged frame must not rebuild the transcript"
        );

        // A streaming delta coalesces into the trailing Text entry: entry count
        // stays 1, but the tail grows. The cache must rebuild and show it.
        model.apply(&AgentEvent::Text {
            delta: " …and still more streamed text arriving token by token".into(),
        });
        assert_eq!(
            model.transcript.len(),
            1,
            "the delta coalesced, not appended"
        );
        ui.ensure_transcript_lines(&model, false, 40);
        assert_eq!(ui.transcript_lines(), transcript_lines(&model, false, 40));
        assert_ne!(
            ui.transcript_lines(),
            first.as_slice(),
            "a grown trailing entry must produce fresh lines"
        );

        // A width change (a resize) also invalidates.
        let wide = ui.transcript_lines().as_ptr();
        ui.ensure_transcript_lines(&model, false, 20);
        assert_ne!(
            ui.transcript_lines().as_ptr(),
            wide,
            "a wrap-width change must rebuild"
        );
        assert_eq!(ui.transcript_lines(), transcript_lines(&model, false, 20));
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
            speculated: false,
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

    /// Expanded (ctrl+o) detail rows — full tool output and pretty-printed
    /// call args — align at the content column exactly like their parent
    /// row's content, not at the left margin: the transcript's two-column
    /// layout must hold for the hidden rows too.
    #[test]
    fn expanded_detail_rows_align_at_the_content_column() {
        let indent = " ".repeat(LABEL_COL);
        let line_text =
            |l: &Line<'_>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };

        let mut result_rows = Vec::new();
        entry_lines(
            &TranscriptEntry::ToolResult {
                call_id: "c1".into(),
                name: "grep".into(),
                ok: true,
                summary: "hit".into(),
                full: "src/a.rs:1: hit\nsrc/b.rs:2: hit".into(),
                duration_ms: 5,
                speculated: false,
                diff: None,
                speculated: false,
            },
            false,
            true,
            120,
            &mut result_rows,
        );
        let details: Vec<String> = result_rows.iter().skip(1).map(line_text).collect();
        assert_eq!(details.len(), 2, "both output lines render");
        for d in &details {
            assert!(
                d.starts_with(&indent) && !d.starts_with(&format!("{indent} ")),
                "detail row starts exactly at LABEL_COL: {d:?}"
            );
        }

        let mut start_rows = Vec::new();
        entry_lines(
            &TranscriptEntry::ToolStart {
                call_id: "c1".into(),
                name: "grep".into(),
                input: "pattern".into(),
                raw: r#"{"pattern":"hit"}"#.into(),
                path: None,
            },
            false,
            true,
            120,
            &mut start_rows,
        );
        assert!(
            start_rows
                .iter()
                .skip(1)
                .all(|l| line_text(l).starts_with(&indent)),
            "expanded call args align at the content column"
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

    // ---- Transcript prefix colors (ember gutter + violet user prompt) ----

    /// The user prompt is the single exception to the ember gutter: its
    /// `[user]:` tag AND every line of the prompt render in exactly the violet
    /// used for the composer's keybind glyphs and the deterministic-first
    /// chip — as one flat color, with no markdown tinting and no ember heat.
    #[test]
    fn user_prompt_entry_is_one_violet_color_end_to_end() {
        let mut out = Vec::new();
        // Markdown that WOULD tint under `markdown::render` (a code span goes
        // `theme::WARN`, a heading goes bold `INK`): proof none of it leaks.
        entry_lines(
            &TranscriptEntry::User("fix the `parser` bug\nand **ship** it".to_string()),
            false,
            false,
            80,
            &mut out,
        );
        let mut saw_text = false;
        for line in &out {
            for span in &line.spans {
                // Skip pure-whitespace gutter/continuation indent (no fg).
                if span.content.trim().is_empty() {
                    continue;
                }
                saw_text = true;
                assert_eq!(
                    span.style.fg,
                    Some(theme::VIOLET),
                    "user entry span {:?} is not the violet accent",
                    span.content
                );
                // Belt and suspenders: no ember heat anywhere on the entry.
                for banned in [theme::EMBER_GOLD, theme::EMBER_CRIMSON, theme::WARN] {
                    assert_ne!(
                        span.style.fg,
                        Some(banned),
                        "user entry leaks an ember color: {:?}",
                        span.content
                    );
                }
            }
        }
        assert!(
            saw_text,
            "the prompt rendered at least one styled text span"
        );
    }

    /// Every other entry's `[label]:` prefix stays inside the warm ember
    /// family — failure crimson, success gold — so no raw ANSI cyan/blue/
    /// magenta survives from before the ember theme landed.
    #[test]
    fn transcript_prefix_colors_stay_in_the_ember_family() {
        let prefix_fg = |entry: &TranscriptEntry| -> Option<Color> {
            let mut out = Vec::new();
            entry_lines(entry, false, false, 80, &mut out);
            out[0].spans[0].style.fg
        };
        assert_eq!(
            prefix_fg(&TranscriptEntry::Error {
                message: "boom".into(),
                retryable: false,
            }),
            Some(theme::EMBER_CRIMSON),
            "error prefix is crimson",
        );
        assert_eq!(
            prefix_fg(&TranscriptEntry::ToolResult {
                call_id: "c1".into(),
                name: "read_file".into(),
                ok: true,
                summary: "ok".into(),
                full: "ok".into(),
                duration_ms: 3,
                speculated: false,
                diff: None,
            }),
            Some(theme::EMBER_GOLD),
            "successful tool-result prefix is gold",
        );
        assert_eq!(
            prefix_fg(&TranscriptEntry::ToolResult {
                call_id: "c2".into(),
                name: "read_file".into(),
                ok: false,
                summary: "no".into(),
                full: "no".into(),
                duration_ms: 3,
                speculated: false,
                diff: None,
            }),
            Some(theme::EMBER_CRIMSON),
            "failed tool-result prefix is crimson",
        );
        // The stage marker moved off raw cyan onto ember flame.
        assert_eq!(
            prefix_fg(&TranscriptEntry::Stage(StageKind::Execute)),
            Some(theme::EMBER_FLAME),
            "stage prefix is ember flame",
        );
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
