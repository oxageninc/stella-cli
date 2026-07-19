//! Session tab — the focused agent's REPL surface (identity header + HUD +
//! any pending gate card + the task-board checklist + transcript). It **reuses** the single-session
//! renderers (`render_hud`, `render_transcript`, `render_scope_review`,
//! `render_ask_user`, `entry_lines`) so the classic view is pixel-identical,
//! just scoped to whichever agent `ui.focused` points at. No transcript
//! rendering is duplicated — there is one implementation of "draw a session".

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use std::collections::HashSet;
use std::ops::Range;

use stella_protocol::{TaskItem, TaskStatus};

use crate::deck::{AgentEntry, WorkspaceModel};
use crate::deck_ui::DeckUi;
use crate::model::TranscriptEntry;
use crate::render::{
    entry_lines, inner_height, inner_width, render_ask_user, render_hud, render_scope_review,
    render_transcript_window,
};
use crate::theme;

/// Incremental transcript fold for the Session tab.
///
/// Everything before the last entry is *settled* — streaming deltas only ever
/// mutate the final entry — so settled entries fold (markdown, labels, wrap)
/// exactly once and are cached with their visual-row ranges; only the tail
/// entry re-folds per frame. The cache invalidates whole when anything that
/// changes how settled entries render changes: focused agent, the thinking
/// toggle, a ctrl+o expansion, the pane width, or the retention cap evicting
/// a chunk of the front (which shifts every retained index). This turns the
/// old O(whole-history) fold per frame into O(tail) — typing latency no
/// longer grows with session length.
#[derive(Debug, Clone, Default)]
pub struct SessionFold {
    key: Option<(String, bool, bool, u64, usize, usize)>,
    settled: usize,
    prefix: Vec<Line<'static>>,
    entry_rows: Vec<Range<usize>>,
    tail: Vec<Line<'static>>,
}

impl SessionFold {
    /// Bring the cache up to date for this frame. `expand_all` is the
    /// no-selection ctrl+o overlay: every expandable entry folds as if
    /// individually expanded (it participates in the cache key, so toggling
    /// it invalidates exactly once).
    /// `streaming` is the in-flight `TextDelta` preview
    /// ([`SessionModel::streaming_text`](crate::model::SessionModel)): folded
    /// into the live tail after the last entry, so it re-wraps per frame
    /// like the tail does and vanishes without residue when the
    /// authoritative `Text` clears it — never a settled entry.
    #[allow(clippy::too_many_arguments)]
    fn refresh(
        &mut self,
        agent: &str,
        transcript: &[TranscriptEntry],
        streaming: &str,
        thinking: bool,
        expanded: &HashSet<usize>,
        expand_all: bool,
        expanded_rev: u64,
        width: usize,
    ) {
        // Front-eviction shifts every retained index, so the settled prefix
        // no longer describes the entries now occupying 0..settled. The
        // marker's cumulative count grows on every pass, so keying on it
        // invalidates exactly when the front moves — the shrink check alone
        // misses an eviction whose survivors still outnumber `settled`.
        let evicted = match transcript.first() {
            Some(TranscriptEntry::Evicted { count }) => *count,
            _ => 0,
        };
        let key = (
            agent.to_string(),
            thinking,
            expand_all,
            expanded_rev,
            width,
            evicted,
        );
        if self.key.as_ref() != Some(&key) || self.settled > transcript.len().saturating_sub(1) {
            self.key = Some(key);
            self.settled = 0;
            self.prefix.clear();
            self.entry_rows.clear();
        }
        let target = transcript.len().saturating_sub(1);
        while self.settled < target {
            let i = self.settled;
            let start = self.prefix.len();
            entry_lines(
                &transcript[i],
                thinking,
                expand_all || expanded.contains(&i),
                width,
                &mut self.prefix,
            );
            self.entry_rows.push(start..self.prefix.len());
            self.settled += 1;
        }
        self.tail.clear();
        if let Some(last) = transcript.last() {
            entry_lines(
                last,
                thinking,
                expand_all || expanded.contains(&target),
                width,
                &mut self.tail,
            );
        }
        if !streaming.is_empty() {
            let preview = TranscriptEntry::Text(streaming.to_string());
            entry_lines(&preview, thinking, false, width, &mut self.tail);
        }
    }

    /// Total visual rows (settled prefix + live tail).
    pub fn total(&self) -> usize {
        self.prefix.len() + self.tail.len()
    }

    /// The visual-row range entry `idx` occupies (the live tail entry spans
    /// everything past the prefix).
    pub fn rows_of(&self, idx: usize) -> Range<usize> {
        if idx < self.entry_rows.len() {
            self.entry_rows[idx].clone()
        } else {
            self.prefix.len()..self.total()
        }
    }

    /// Materialize just the rows in `window` — ≤ one viewport of clones.
    fn window_lines(&self, window: Range<usize>) -> Vec<Line<'static>> {
        window
            .filter_map(|r| {
                if r < self.prefix.len() {
                    self.prefix.get(r).cloned()
                } else {
                    self.tail.get(r - self.prefix.len()).cloned()
                }
            })
            .collect()
    }
}

pub fn render(model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    let Some(agent) = model.agents.get(ui.focused) else {
        empty_state(area, buf);
        return;
    };
    let sm = &agent.model;

    // Each pending gate claims its own band (0 = collapsed). Both can be
    // pending at once — nothing clears one when the other arrives — so they
    // render independently, exactly like the single-session `render`; an
    // ask-user question is never hidden behind a scope review.
    let scope_h: u16 = if sm.pending_scope_review.is_some() {
        8
    } else {
        0
    };
    let ask_h: u16 = match &sm.pending_ask_user {
        Some(p) => (p.options.len() as u16 + 5).min(12),
        None => 0,
    };
    // The TASKS card sits between the gates and the transcript when the
    // board is non-empty (0 = collapsed). Its interior is capped at
    // [`TASK_CARD_MAX_ROWS`] rows by [`task_card_rows`], so the whole card
    // never exceeds 10 lines and cannot steal the transcript.
    let task_rows = task_card_rows(&sm.tasks);
    let tasks_h: u16 = if task_rows.is_empty() {
        0
    } else {
        (task_rows.len() as u16 + 2).min(10)
    };

    let bands = Layout::vertical([
        Constraint::Length(1),       // identity header
        Constraint::Length(3),       // HUD
        Constraint::Length(scope_h), // pending scope review (0 = collapsed)
        Constraint::Length(ask_h),   // pending ask-user (0 = collapsed)
        Constraint::Length(tasks_h), // task-board checklist (0 = collapsed)
        Constraint::Min(1),          // transcript
    ])
    .split(area);

    render_header(agent, model.now_ms, bands[0], buf);
    render_hud(&sm.hud, bands[1], buf);
    if let Some(proposal) = &sm.pending_scope_review {
        render_scope_review(proposal, false, bands[2], buf);
    }
    if let Some(prompt) = &sm.pending_ask_user {
        render_ask_user(prompt, false, bands[3], buf);
    }
    if !task_rows.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme::muted())
            .title(" TASKS ");
        Paragraph::new(task_rows).block(block).render(bands[4], buf);
    }

    // Transcript: fold through the incremental cache (settled entries fold
    // once; only the streaming tail re-folds per frame), then reuse the
    // line-exact scroll window over the cached rows.
    let width = inner_width(bands[5]);
    let empty = HashSet::new();
    let expanded_set = ui.expanded.get(&agent.meta.id).unwrap_or(&empty);
    ui.session_fold.refresh(
        &agent.meta.id,
        &sm.transcript,
        &sm.streaming_text,
        ui.thinking_expanded,
        expanded_set,
        ui.transcript_expand_all,
        ui.expanded_rev,
        width,
    );
    let height = inner_height(bands[5]);
    let total = ui.session_fold.total();

    // A selection move from the key handler lands here, where visual-row
    // ranges are known: nudge the scroll window until the entry is visible,
    // then pin it. Follow drops even when no nudge was needed — a streaming
    // tail must not slide the highlight out of view (↓ past the tail, or
    // scrolling back to the bottom, re-arms follow).
    if let Some(sel) = ui.session_pending_scroll.take() {
        let rows = ui.session_fold.rows_of(sel);
        let current = ui.session_scroll.window(total, height);
        ui.session_scroll.top = if rows.start < current.start {
            rows.start
        } else if rows.end > current.end {
            rows.end.saturating_sub(height)
        } else {
            current.start
        };
        ui.session_scroll.follow = false;
    }

    ui.metrics.session_total = total;
    ui.metrics.session_height = height;
    let window = ui.session_scroll.window(total, height);
    let mut visible = ui.session_fold.window_lines(window.clone());
    if let Some(sel) = ui.session_selected {
        // A quiet warm background lift on the selected entry's rows.
        for r in ui.session_fold.rows_of(sel) {
            if window.contains(&r)
                && let Some(line) = visible.get_mut(r - window.start)
            {
                line.style = line.style.bg(theme::SELECT_BG);
            }
        }
    }
    // Contextual help, keyed to the transcript's interaction state: every
    // mode advertises its own way out (ctrl+o/Esc collapse the expand-all
    // overlay, Esc clears a highlight) and the resting state teaches the
    // scroll verbs (↑ scrolls; ⌘/⌃ ] and ⌘/⌃ [ jump to the ends).
    let hint = if ui.transcript_expand_all {
        "all expanded · ⌃O or Esc collapses"
    } else if ui.session_selected.is_some() {
        "⌃O expand/collapse · Esc clears · ↑ ↓ move"
    } else {
        "↑ scroll · ⌘/⌃ ] end · ⌘/⌃ [ start · ⌃O expand all"
    };
    render_transcript_window(
        visible,
        window,
        total,
        ui.session_scroll.follow,
        Some(hint),
        bands[5],
        buf,
    );
}

/// Most checklist rows the TASKS card shows before collapsing the tail.
const TASK_CARD_MAX_ROWS: usize = 8;

/// Build the TASKS card's checklist rows from the latest board snapshot —
/// pure, so the collapse policy is unit-testable, and total on any board
/// contents (no indexing beyond what is counted, no unwraps).
///
/// - Empty board → no rows (the card disappears entirely).
/// - A board that fits ([`TASK_CARD_MAX_ROWS`] or fewer) renders every task
///   in board order.
/// - A larger board prefers the open work (pending + in-progress, still in
///   board order) and folds everything hidden into one dim tail row —
///   `… +K done` for the finished/cancelled tasks, plus `+J more` if even
///   the open set overflows.
fn task_card_rows(tasks: &[TaskItem]) -> Vec<Line<'static>> {
    if tasks.is_empty() {
        return Vec::new();
    }
    if tasks.len() <= TASK_CARD_MAX_ROWS {
        return tasks.iter().map(task_row).collect();
    }
    let open: Vec<&TaskItem> = tasks.iter().filter(|t| t.status.is_open()).collect();
    let done = tasks.len() - open.len();
    let shown = open.len().min(TASK_CARD_MAX_ROWS - 1);
    let mut rows: Vec<Line<'static>> = open.iter().take(shown).map(|t| task_row(t)).collect();
    let hidden_open = open.len() - shown;
    let mut parts: Vec<String> = Vec::new();
    if hidden_open > 0 {
        parts.push(format!("+{hidden_open} more"));
    }
    if done > 0 {
        parts.push(format!("+{done} done"));
    }
    if !parts.is_empty() {
        rows.push(Line::from(Span::styled(
            format!("… {}", parts.join(" · ")),
            theme::muted(),
        )));
    }
    rows
}

/// One checklist row: status glyph · `#id subject` · dim ` (owner)`.
fn task_row(task: &TaskItem) -> Line<'static> {
    let (glyph, glyph_style, text_style) = match task.status {
        TaskStatus::Pending => ("☐", theme::muted(), Style::new().fg(theme::TEXT_SECONDARY)),
        TaskStatus::InProgress => (
            "▸",
            theme::accent().add_modifier(Modifier::BOLD),
            Style::new()
                .fg(theme::TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        TaskStatus::Completed => (
            "✓",
            Style::new().fg(theme::SUCCESS),
            Style::new().fg(theme::TEXT_TERTIARY),
        ),
        TaskStatus::Cancelled => (
            "✗",
            Style::new().fg(theme::DANGER).add_modifier(Modifier::DIM),
            Style::new().fg(theme::TEXT_TERTIARY),
        ),
    };
    let mut spans = vec![
        Span::styled(format!(" {glyph} "), glyph_style),
        Span::styled(format!("#{} {}", task.id, task.subject), text_style),
    ];
    if let Some(owner) = &task.owner {
        spans.push(Span::styled(format!(" ({owner})"), theme::muted()));
    }
    Line::from(spans)
}

/// The one-line identity header: `▶ lead · running   0:00:00`. The trailing
/// slot is the **per-turn wall clock** — live while a turn is in flight, else
/// the last turn's held duration (zero before any turn), formatted `h:mm:ss`
/// and always present.
fn render_header(agent: &AgentEntry, now_ms: u64, area: Rect, buf: &mut Buffer) {
    let st = agent.status;
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", theme::status_glyph(st)),
            Style::new().fg(theme::status_color(st)),
        ),
        Span::styled(agent.meta.id.clone(), theme::accent()),
        Span::styled("  ·  ", theme::rule()),
        Span::styled(
            st.label().to_string(),
            Style::new().fg(theme::status_color(st)),
        ),
        Span::raw("   "),
        Span::styled(fmt_hms(agent.turn_clock_ms(now_ms)), theme::accent()),
    ]);
    Paragraph::new(line).render(area, buf);
}

/// Format a millisecond duration as `h:mm:ss` — hours un-padded, minutes and
/// seconds zero-padded to two digits. Drives the per-turn header clock.
fn fmt_hms(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{h}:{m:02}:{s:02}")
}

/// Shown when there are no agents at all.
fn empty_state(area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let mid = Rect {
        x: area.x,
        y: area.y + area.height / 2,
        width: area.width,
        height: 1,
    };
    Paragraph::new(Span::styled(
        "no active session — type a prompt and press Enter to dispatch one",
        theme::muted(),
    ))
    .alignment(Alignment::Center)
    .render(mid, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{AgentMeta, Inbound};
    use crate::model::{MAX_TRANSCRIPT_ENTRIES, SessionModel};
    use stella_protocol::{AgentEvent, ScopeProposal};

    /// Flatten a `Buffer` to plain text (content, not ANSI — the crate-wide
    /// render-test convention).
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
    fn both_pending_gates_render_at_once() {
        // Nothing clears one gate when the other arrives, so both can be
        // pending simultaneously — and both must be visible, or the user has
        // no way to see (let alone answer) the hidden one.
        let mut model = WorkspaceModel::new();
        model.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "goal", 0)));
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::ScopeReview {
                proposal: ScopeProposal {
                    summary: "widen the refactor".into(),
                    steps: vec![],
                    estimated_files: 3,
                    estimated_cost_usd: None,
                },
            },
        });
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::AskUser {
                id: "q1".into(),
                question: "Which database should the cache use?".into(),
                options: vec!["sqlite".into(), "redis".into()],
            },
        });

        let mut ui = DeckUi::default();
        let area = Rect::new(0, 0, 90, 40);
        let mut buf = Buffer::empty(area);
        render(&model, &mut ui, area, &mut buf);

        let text = buffer_text(&buf);
        assert!(
            text.contains("scope review"),
            "scope-review card visible:\n{text}"
        );
        assert!(
            text.contains("Which database should the cache use?"),
            "ask-user card visible alongside the scope review:\n{text}"
        );
    }

    #[test]
    fn streaming_preview_renders_in_the_session_tab_until_the_text_replaces_it() {
        let mut model = WorkspaceModel::new();
        model.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "goal", 0)));
        for fragment in ["streamed toke", "ns arriving"] {
            model.apply_inbound(&Inbound::Event {
                agent: "lead".into(),
                event: AgentEvent::TextDelta {
                    text: fragment.into(),
                },
            });
        }

        let mut ui = DeckUi::default();
        let area = Rect::new(0, 0, 90, 24);
        let mut buf = Buffer::empty(area);
        render(&model, &mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(
            text.contains("streamed tokens arriving"),
            "deltas render live, before any Text event:\n{text}"
        );

        // The authoritative Text replaces the preview — never a duplicate.
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Text {
                delta: "streamed tokens arriving".into(),
            },
        });
        let mut buf = Buffer::empty(area);
        render(&model, &mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert_eq!(
            text.matches("streamed tokens arriving").count(),
            1,
            "the committed answer appears exactly once:\n{text}"
        );
    }

    #[test]
    fn fold_cache_stays_line_exact_across_front_eviction() {
        // The dangerous shape: the cache settles on a SHORT prefix, then the
        // transcript grows past the cap so a chunk of the front evicts while
        // the survivor count still exceeds `settled` — the shrink check alone
        // cannot see it, only the eviction-count key can.
        let mut model = SessionModel::new();
        let expanded = HashSet::new();
        let retry = |i: usize| AgentEvent::Retry {
            attempt: i as u32,
            reason: "r".into(),
        };
        for i in 0..1_000 {
            model.apply(&retry(i));
        }
        let mut fold = SessionFold::default();
        fold.refresh(
            "lead",
            &model.transcript,
            "",
            false,
            &expanded,
            false,
            0,
            80,
        );
        for i in 1_000..(MAX_TRANSCRIPT_ENTRIES + 50) {
            model.apply(&retry(i));
        }
        assert!(model.evicted_entries() > 0, "an eviction pass occurred");
        fold.refresh(
            "lead",
            &model.transcript,
            "",
            false,
            &expanded,
            false,
            0,
            80,
        );

        let mut fresh = SessionFold::default();
        fresh.refresh(
            "lead",
            &model.transcript,
            "",
            false,
            &expanded,
            false,
            0,
            80,
        );
        assert_eq!(fold.total(), fresh.total());
        assert_eq!(
            fold.window_lines(0..fold.total()),
            fresh.window_lines(0..fresh.total()),
            "the incrementally-maintained fold matches a from-scratch fold"
        );
    }

    #[test]
    fn applying_a_selection_pins_the_window_against_a_streaming_tail() {
        let mut model = WorkspaceModel::new();
        model.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "goal", 0)));
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Text {
                delta: "first-message".into(),
            },
        });

        // Select the (already fully visible) first entry.
        let mut ui = DeckUi {
            session_selected: Some(0),
            session_pending_scroll: Some(0),
            ..DeckUi::default()
        };
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        render(&model, &mut ui, area, &mut buf);
        assert!(
            !ui.session_scroll.follow,
            "a selection pins the window even when no scroll nudge was needed"
        );

        // A streaming tail grows past the viewport — the pinned window must
        // keep the selected entry visible instead of following the tail.
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Text {
                delta: "\nnoise".repeat(40),
            },
        });
        let mut buf2 = Buffer::empty(area);
        render(&model, &mut ui, area, &mut buf2);
        let text = buffer_text(&buf2);
        assert!(
            text.contains("first-message"),
            "selected entry still visible under streaming:\n{text}"
        );
    }

    fn task(id: &str, subject: &str, status: TaskStatus, owner: Option<&str>) -> TaskItem {
        TaskItem {
            id: id.into(),
            subject: subject.into(),
            description: None,
            status,
            owner: owner.map(str::to_string),
        }
    }

    /// Flatten one card row to its plain text (content, not style).
    fn row_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn task_card_rows_empty_board_builds_no_card() {
        assert!(task_card_rows(&[]).is_empty(), "no board → no card");
    }

    #[test]
    fn task_card_rows_small_board_shows_every_task_in_order_with_glyphs() {
        let tasks = vec![
            task("1", "Fix the redirect loop", TaskStatus::Completed, None),
            task(
                "2",
                "Add the session card",
                TaskStatus::InProgress,
                Some("lead"),
            ),
            task("3", "Write the tests", TaskStatus::Pending, None),
        ];
        let rows = task_card_rows(&tasks);
        assert_eq!(rows.len(), 3, "a board that fits shows every task");
        assert_eq!(row_text(&rows[0]), " ✓ #1 Fix the redirect loop");
        assert_eq!(row_text(&rows[1]), " ▸ #2 Add the session card (lead)");
        assert_eq!(row_text(&rows[2]), " ☐ #3 Write the tests");
    }

    #[test]
    fn task_card_rows_large_board_prefers_open_work_and_collapses_the_done_tail() {
        // 12 tasks, 6 done: the 6 open ones (board order) + one collapse row.
        let mut tasks = Vec::new();
        for i in 0..6 {
            tasks.push(task(
                &format!("{}", i + 1),
                "done work",
                TaskStatus::Completed,
                None,
            ));
        }
        tasks.push(task("7", "doing now", TaskStatus::InProgress, None));
        for i in 7..12 {
            tasks.push(task(
                &format!("{}", i + 1),
                "still open",
                TaskStatus::Pending,
                None,
            ));
        }
        let rows = task_card_rows(&tasks);
        assert_eq!(rows.len(), 7, "6 open rows + the collapse row");
        assert_eq!(
            row_text(&rows[0]),
            " ▸ #7 doing now",
            "open work leads, in board order"
        );
        for row in &rows[1..6] {
            assert!(row_text(row).contains("still open"), "pending rows follow");
        }
        assert_eq!(
            row_text(&rows[6]),
            "… +6 done",
            "the finished tail folds into one dim row"
        );
        // And the whole card stays inside its height budget.
        assert!(rows.len() <= TASK_CARD_MAX_ROWS);
    }

    #[test]
    fn task_card_rows_overflowing_open_work_reports_the_hidden_count() {
        // 12 open tasks: 7 rows shown, the rest folded as `+5 more`.
        let tasks: Vec<TaskItem> = (0..12)
            .map(|i| task(&format!("{}", i + 1), "open", TaskStatus::Pending, None))
            .collect();
        let rows = task_card_rows(&tasks);
        assert_eq!(rows.len(), TASK_CARD_MAX_ROWS);
        assert_eq!(row_text(&rows[7]), "… +5 more");
    }

    #[test]
    fn tasks_card_renders_between_hud_and_transcript_and_vanishes_when_empty() {
        let mut model = WorkspaceModel::new();
        model.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "goal", 0)));
        let area = Rect::new(0, 0, 80, 24);

        // Empty board: no card.
        let mut ui = DeckUi::default();
        let mut buf = Buffer::empty(area);
        render(&model, &mut ui, area, &mut buf);
        assert!(
            !buffer_text(&buf).contains("TASKS"),
            "no card on an empty board"
        );

        // A TaskUpdate folds the board in — the card appears with the rows.
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: stella_protocol::AgentEvent::TaskUpdate {
                tasks: vec![
                    task(
                        "1",
                        "Fix the auth redirect",
                        TaskStatus::InProgress,
                        Some("lead"),
                    ),
                    task("2", "Ship the fix", TaskStatus::Pending, None),
                ],
            },
        });
        let mut buf = Buffer::empty(area);
        render(&model, &mut ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("TASKS"), "card title visible:\n{text}");
        assert!(
            text.contains("#1 Fix the auth redirect"),
            "in-progress row visible:\n{text}"
        );
        assert!(text.contains("(lead)"), "owner suffix visible:\n{text}");
        assert!(text.contains("#2 Ship the fix"), "pending row:\n{text}");

        // An empty snapshot clears the board — the card disappears again.
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: stella_protocol::AgentEvent::TaskUpdate { tasks: vec![] },
        });
        let mut buf = Buffer::empty(area);
        render(&model, &mut ui, area, &mut buf);
        assert!(
            !buffer_text(&buf).contains("#1 Fix the auth redirect"),
            "cleared board removes the checklist rows"
        );
    }

    #[test]
    fn fmt_hms_formats_zero_sub_minute_and_over_an_hour() {
        // Always h:mm:ss, hours un-padded, minutes/seconds zero-padded.
        assert_eq!(fmt_hms(0), "0:00:00"); // the at-rest, pre-turn readout
        assert_eq!(fmt_hms(45_000), "0:00:45"); // < 1 min
        assert_eq!(fmt_hms(65_000), "0:01:05"); // rolls into minutes
        assert_eq!(fmt_hms(3_600_000), "1:00:00"); // exactly one hour
        assert_eq!(fmt_hms(3_661_000), "1:01:01"); // > 1 hr
        assert_eq!(fmt_hms(45_296_000), "12:34:56"); // multi-hour, sub-ms floored
    }

    #[test]
    fn header_shows_the_turn_clock_not_the_word_stella() {
        // The workspace-name title used to sit to the right of `running`; in the
        // stella repo that literally read "stella". It is now the turn clock —
        // the header must show the h:mm:ss readout and no longer the word.
        let mut model = WorkspaceModel::new();
        model.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "stella", 0)));
        // A plain event flips the agent to `Running` (so the label reads
        // "running") without starting a turn — the clock stays at zero.
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Text {
                delta: "working".into(),
            },
        });
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = Buffer::empty(area);
        render_header(&model.agents[0], model.now_ms, area, &mut buf);

        let text = buffer_text(&buf);
        assert!(
            !text.contains("stella"),
            "the word `stella` is gone from the header:\n{text}"
        );
        assert!(
            text.contains("0:00:00"),
            "the turn clock reads zero before any turn:\n{text}"
        );
        assert!(text.contains("running"), "status label intact:\n{text}");
    }
}
