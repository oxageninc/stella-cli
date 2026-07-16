//! Session tab — the focused agent's REPL surface (identity header + HUD +
//! any pending gate card + transcript). It **reuses** the single-session
//! renderers (`render_hud`, `render_transcript`, `render_scope_review`,
//! `render_ask_user`, `entry_lines`) so the classic view is pixel-identical,
//! just scoped to whichever agent `ui.focused` points at. No transcript
//! rendering is duplicated — there is one implementation of "draw a session".

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use std::collections::HashSet;
use std::ops::Range;

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
/// toggle, a ctrl+o expansion, or the pane width. This turns the old
/// O(whole-history) fold per frame into O(tail) — typing latency no longer
/// grows with session length.
#[derive(Debug, Clone, Default)]
pub struct SessionFold {
    key: Option<(String, bool, u64, usize)>,
    settled: usize,
    prefix: Vec<Line<'static>>,
    entry_rows: Vec<Range<usize>>,
    tail: Vec<Line<'static>>,
}

impl SessionFold {
    /// Bring the cache up to date for this frame.
    fn refresh(
        &mut self,
        agent: &str,
        transcript: &[TranscriptEntry],
        thinking: bool,
        expanded: &HashSet<usize>,
        expanded_rev: u64,
        width: usize,
    ) {
        let key = (agent.to_string(), thinking, expanded_rev, width);
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
                expanded.contains(&i),
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
                expanded.contains(&target),
                width,
                &mut self.tail,
            );
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

    let bands = Layout::vertical([
        Constraint::Length(1),       // identity header
        Constraint::Length(3),       // HUD
        Constraint::Length(scope_h), // pending scope review (0 = collapsed)
        Constraint::Length(ask_h),   // pending ask-user (0 = collapsed)
        Constraint::Min(1),          // transcript
    ])
    .split(area);

    render_header(agent, bands[0], buf);
    render_hud(&sm.hud, bands[1], buf);
    if let Some(proposal) = &sm.pending_scope_review {
        render_scope_review(proposal, false, bands[2], buf);
    }
    if let Some(prompt) = &sm.pending_ask_user {
        render_ask_user(prompt, false, bands[3], buf);
    }

    // Transcript: fold through the incremental cache (settled entries fold
    // once; only the streaming tail re-folds per frame), then reuse the
    // line-exact scroll window over the cached rows.
    let width = inner_width(bands[4]);
    let empty = HashSet::new();
    let expanded_set = ui.expanded.get(&agent.meta.id).unwrap_or(&empty);
    ui.session_fold.refresh(
        &agent.meta.id,
        &sm.transcript,
        ui.thinking_expanded,
        expanded_set,
        ui.expanded_rev,
        width,
    );
    let height = inner_height(bands[4]);
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
    render_transcript_window(
        visible,
        window,
        total,
        ui.session_scroll.follow,
        bands[4],
        buf,
    );
}

/// The one-line identity header: `▶ lead · running   goal…`.
fn render_header(agent: &AgentEntry, area: Rect, buf: &mut Buffer) {
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
        Span::styled(agent.meta.title.clone(), theme::muted()),
    ]);
    Paragraph::new(line).render(area, buf);
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
}
