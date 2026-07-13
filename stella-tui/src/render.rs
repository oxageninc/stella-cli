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

use stella_protocol::{BudgetMode, FileChangeKind, MediaKind, PrStatus, StageKind};

use crate::composer::SlashMenu;
use crate::model::{AskUserPrompt, FileState, Hud, SessionModel, TranscriptEntry};
use crate::ui::{PanelFocus, UiState, ViewportMetrics};

/// Draw the whole TUI for one frame. Records the panels' viewport sizes back
/// into `ui.metrics` so the pure key handler can clamp scrolling on the next
/// keypress (the only reason this takes `&mut UiState`).
pub fn render(model: &SessionModel, ui: &mut UiState, frame: &mut Frame) {
    let root = frame.area();
    let has_scope = model.pending_scope_review.is_some();
    let has_ask = model.pending_ask_user.is_some();
    let slash = ui.composer.slash_menu(&ui.slash_commands);
    let show_slash = slash.as_ref().is_some_and(|m| !m.is_empty());

    // Vertical bands: HUD, main, [scope], [ask], [slash], composer.
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
    if show_slash {
        let rows = slash.as_ref().map(|m| m.matches.len()).unwrap_or(0);
        constraints.push(Constraint::Length((rows as u16 + 2).min(8)));
    }
    constraints.push(Constraint::Length(3));
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
    let slash_area = if show_slash {
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

    let t_lines = transcript_lines(model);
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
        let d_lines = file
            .and_then(|f| f.latest_diff.as_deref())
            .map(diff_lines)
            .unwrap_or_default();
        let d_total = d_lines.len();
        let d_inner_h = inner_height(right_area);
        let d_window = ui.diff_scroll.window(d_total, d_inner_h);
        let title = file
            .map(|f| f.path.clone())
            .unwrap_or_else(|| "diff".to_string());
        guarded_panel(frame, right_area, "diff", |buf| {
            render_diff(&d_lines, d_window.clone(), &title, right_area, buf)
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

    // ---- Slash menu.
    if let (Some(area), Some(menu)) = (slash_area, slash.as_ref()) {
        guarded_panel(frame, area, "slash-menu", |buf| {
            render_slash_menu(menu, area, buf)
        });
    }

    // ---- Composer.
    let composer_line = ui.composer.display_line();
    let composer_focused = ui.focus == PanelFocus::Composer;
    guarded_panel(frame, composer_area, "composer", |buf| {
        render_composer(&composer_line, composer_focused, composer_area, buf)
    });

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

// ---------------------------------------------------------------------------
// Panel panic boundary (L-T7)
// ---------------------------------------------------------------------------

/// Render one panel into a throwaway buffer under `catch_unwind`; on panic,
/// substitute a visible error card. See the module docs for the soundness
/// argument behind `AssertUnwindSafe`.
pub(crate) fn guarded_panel<F>(frame: &mut Frame, area: Rect, label: &str, draw: F)
where
    F: Fn(&mut Buffer),
{
    if area.width == 0 || area.height == 0 {
        return;
    }
    let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut buf = Buffer::empty(area);
        draw(&mut buf);
        buf
    }));
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
        Span::styled(spend_label(hud), Style::new().fg(spend_color(hud))),
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
    let total = lines.len();
    let visible: Vec<Line<'static>> = lines
        .get(window.clone())
        .map(<[Line]>::to_vec)
        .unwrap_or_default();
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

fn render_diff(
    lines: &[Line<'static>],
    window: Range<usize>,
    title: &str,
    area: Rect,
    buf: &mut Buffer,
) {
    let visible: Vec<Line<'static>> = lines
        .get(window.clone())
        .map(<[Line]>::to_vec)
        .unwrap_or_default();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" diff · {title} "));
    Paragraph::new(Text::from(visible))
        .block(block)
        .render(area, buf);
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

fn render_slash_menu(menu: &SlashMenu, area: Rect, buf: &mut Buffer) {
    let lines: Vec<Line<'static>> = menu
        .matches
        .iter()
        .map(|c| {
            Line::from(vec![
                Span::styled(
                    c.name.clone(),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(c.description.clone(), Style::new().fg(Color::DarkGray)),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" commands · {} ", menu.matches.len()));
    Paragraph::new(Text::from(lines))
        .block(block)
        .render(area, buf);
}

fn render_composer(line: &str, focused: bool, area: Rect, buf: &mut Buffer) {
    let prompt = Span::styled(
        "› ",
        Style::new().fg(if focused {
            Color::Green
        } else {
            Color::DarkGray
        }),
    );
    let mut spans = vec![prompt, Span::raw(line.to_string())];
    if focused {
        spans.push(Span::styled("▏", Style::new().fg(Color::Green)));
    }
    let block = Block::default().borders(Borders::ALL).title(" prompt ");
    Paragraph::new(Line::from(spans))
        .block(block)
        .render(area, buf);
}

// ---------------------------------------------------------------------------
// Pure content builders (unit-tested directly)
// ---------------------------------------------------------------------------

/// The full visual-line list for the transcript, one-or-more lines per entry.
/// Text/reasoning entries split on newlines so the scroll math stays
/// line-exact (L-T4); everything else is a single styled line.
pub(crate) fn transcript_lines(model: &SessionModel) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for entry in &model.transcript {
        entry_lines(entry, &mut out);
    }
    out
}

pub(crate) fn entry_lines(entry: &TranscriptEntry, out: &mut Vec<Line<'static>>) {
    match entry {
        TranscriptEntry::Stage(name) => out.push(Line::from(Span::styled(
            format!("── {} ──", stage_label(*name)),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))),
        TranscriptEntry::Text(text) => {
            for l in text.split('\n') {
                out.push(Line::from(Span::raw(l.to_string())));
            }
        }
        TranscriptEntry::Reasoning(text) => {
            for l in text.split('\n') {
                out.push(Line::from(Span::styled(
                    l.to_string(),
                    Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                )));
            }
        }
        TranscriptEntry::ToolStart { name, input, .. } => out.push(Line::from(vec![
            Span::styled("→ ", Style::new().fg(Color::Blue)),
            Span::styled(
                name.clone(),
                Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {input}"), Style::new().fg(Color::DarkGray)),
        ])),
        TranscriptEntry::ToolResult {
            ok,
            summary,
            duration_ms,
            ..
        } => {
            let (glyph, color) = if *ok {
                ("✓", Color::Green)
            } else {
                ("✗", Color::Red)
            };
            out.push(Line::from(vec![
                Span::styled(format!("{glyph} "), Style::new().fg(color)),
                Span::raw(summary.clone()),
                Span::styled(format!("  ({duration_ms}ms)"), Style::new().fg(Color::DarkGray)),
            ]));
        }
        TranscriptEntry::Retry { attempt, reason } => out.push(Line::from(Span::styled(
            format!("↻ retry #{attempt}: {reason}"),
            Style::new().fg(Color::Yellow),
        ))),
        TranscriptEntry::Compaction {
            before_tokens,
            after_tokens,
            evicted,
            deduped,
        } => out.push(Line::from(Span::styled(
            format!(
                "⇣ compacted {before_tokens}→{after_tokens} tok (evicted {evicted}, deduped {deduped})"
            ),
            Style::new().fg(Color::Blue),
        ))),
        TranscriptEntry::BudgetTick {
            spent_usd,
            limit_usd,
            mode,
        } => {
            let limit = limit_usd
                .map(|l| format!("/${l:.2}"))
                .unwrap_or_default();
            out.push(Line::from(Span::styled(
                format!("$ spend ${spent_usd:.4}{limit} ({})", budget_mode_label(*mode)),
                Style::new().fg(Color::DarkGray),
            )));
        }
        TranscriptEntry::ProviderFallback { from, to, reason } => out.push(Line::from(Span::styled(
            format!("⚡ provider fallback {from} → {to}: {reason}"),
            Style::new().fg(Color::Magenta),
        ))),
        TranscriptEntry::ContextRecall {
            frames,
            tokens,
            labels,
        } => {
            let cited = labels.join(", ");
            out.push(Line::from(Span::styled(
                format!("◉ recalled {frames} frames ({tokens} tok): {cited}"),
                Style::new().fg(Color::Blue),
            )));
        }
        TranscriptEntry::ContextWrite {
            provider,
            upserts,
            superseded,
        } => out.push(Line::from(Span::styled(
            format!("✎ wrote {upserts} facts ({superseded} superseded) → {provider}"),
            Style::new().fg(Color::Blue),
        ))),
        TranscriptEntry::MediaProgress {
            artifact_id,
            kind,
            state,
        } => out.push(Line::from(Span::styled(
            format!("🎞 {} {artifact_id}: {state}", media_kind_label(*kind)),
            Style::new().fg(Color::Magenta),
        ))),
        TranscriptEntry::MediaComplete { label, path, kind } => out.push(Line::from(vec![
            Span::styled(
                format!("🎨 {} {label} ", media_kind_label(*kind)),
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ),
            Span::styled(path.clone(), Style::new().fg(Color::DarkGray)),
        ])),
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
            let tag = if *deterministic { "deterministic" } else { "model-judge" };
            out.push(Line::from(vec![
                Span::styled(format!("{glyph} verdict "), Style::new().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(format!("[{tag}] "), Style::new().fg(Color::DarkGray)),
                Span::raw(summary.clone()),
            ]));
        }
        TranscriptEntry::ScopeReview {
            summary,
            steps,
            estimated_files,
        } => out.push(Line::from(Span::styled(
            format!("⏸ scope review: {summary} ({steps} steps, ~{estimated_files} files)"),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))),
        TranscriptEntry::AskUser { question, options } => out.push(Line::from(Span::styled(
            format!("? {question} ({options} options + free text)"),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))),
        TranscriptEntry::Commit { sha, message } => {
            let short = sha.chars().take(9).collect::<String>();
            out.push(Line::from(vec![
                Span::styled(format!("● {short} "), Style::new().fg(Color::Cyan)),
                Span::raw(message.clone()),
            ]));
        }
        TranscriptEntry::Pr { url, status } => out.push(Line::from(vec![
            Span::styled(
                format!("⇢ PR [{}] ", pr_status_label(*status)),
                Style::new().fg(pr_status_color(*status)).add_modifier(Modifier::BOLD),
            ),
            Span::styled(url.clone(), Style::new().fg(Color::DarkGray)),
        ])),
        TranscriptEntry::Error { message, retryable } => {
            let tag = if *retryable { " (retryable)" } else { "" };
            out.push(Line::from(Span::styled(
                format!("✗ error{tag}: {message}"),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
        }
        TranscriptEntry::Complete { model, cost_usd } => out.push(Line::from(Span::styled(
            format!("✓ complete · {model} · ${cost_usd:.4}"),
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ))),
    }
}

/// Split a unified diff into styled lines (+ green, - red, @@ cyan header).
pub(crate) fn diff_lines(diff: &str) -> Vec<Line<'static>> {
    diff.split('\n').map(diff_line).collect()
}

fn diff_line(line: &str) -> Line<'static> {
    let color = if line.starts_with("@@") {
        Color::Cyan
    } else if line.starts_with("+++") || line.starts_with("---") {
        Color::DarkGray
    } else if line.starts_with('+') {
        Color::Green
    } else if line.starts_with('-') {
        Color::Red
    } else if line.starts_with("diff ") || line.starts_with("index ") {
        Color::DarkGray
    } else {
        Color::Reset
    };
    Line::from(Span::styled(line.to_string(), Style::new().fg(color)))
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
// Labels
// ---------------------------------------------------------------------------

fn stage_label(stage: StageKind) -> &'static str {
    match stage {
        StageKind::Triage => "triage",
        StageKind::ContextRecall => "context recall",
        StageKind::Plan => "plan",
        StageKind::ScopeReview => "scope review",
        StageKind::Execute => "execute",
        StageKind::Verify => "verify",
        StageKind::Judge => "judge",
        StageKind::Reflect => "reflect",
        StageKind::ContextWrite => "context write",
        StageKind::Complete => "complete",
    }
}

fn budget_mode_label(mode: BudgetMode) -> &'static str {
    match mode {
        BudgetMode::Off => "off",
        BudgetMode::Observed => "observed",
        BudgetMode::Enforced => "enforced",
    }
}

fn media_kind_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Svg => "svg",
        MediaKind::Video => "video",
    }
}

fn pr_status_label(status: PrStatus) -> &'static str {
    match status {
        PrStatus::Draft => "draft",
        PrStatus::Open => "open",
        PrStatus::Merged => "merged",
        PrStatus::Closed => "closed",
    }
}

fn pr_status_color(status: PrStatus) -> Color {
    match status {
        PrStatus::Draft => Color::DarkGray,
        PrStatus::Open => Color::Green,
        PrStatus::Merged => Color::Magenta,
        PrStatus::Closed => Color::Red,
    }
}

fn spend_label(hud: &Hud) -> String {
    match hud.limit_usd {
        Some(limit) => format!("${:.4} / ${:.2}", hud.spent_usd, limit),
        None => format!("${:.4}", hud.spent_usd),
    }
}

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
        AgentEvent, FileChangeKind, ScopeProposal, StageKind, ToolCall, ToolOutput,
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
        let lines = transcript_lines(&model);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect();
        assert!(joined.contains("read_file"));
        assert!(joined.contains("not found"));
        assert!(joined.contains("12ms"));
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
