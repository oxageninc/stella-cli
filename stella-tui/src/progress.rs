//! The full-width run progress bar that sits directly above the composer —
//! the deck's sole activity indicator (it replaced the garble spinner).
//!
//! ## Honesty
//!
//! Every mark on this bar is bound to real run state; nothing performs activity
//! it can't substantiate (the project thesis: *report* state, don't fake it).
//! Concretely:
//!
//! - The **determinate fill** is a *stage-position* readout, not a fabricated
//!   completion percentage. There is no progress fraction anywhere in the model
//!   (`Hud.stage` is a categorical [`StageKind`], not a monotonic 0→N counter),
//!   so the bar maps the real current stage onto the three display phases
//!   `plan → execute → verify`: completed phases fill solid, the active phase
//!   fills to its midpoint, and the percent is derived from that position. It
//!   moves only when the engine actually emits a new `Stage` event.
//! - The **shimmer** and the **pulsing head** are the *only* indeterminate cues,
//!   and they signal liveness (`AgentStatus::Running`) — never progress. They
//!   ride *on top of* the determinate fill and never advance it.
//! - **tok/s** is the focused agent's real `tokens_out / elapsed`; it is omitted
//!   (not guessed) whenever there's nothing real to divide. **ETA** is always
//!   omitted — the planner exposes no estimate to substantiate one.
//! - On **failure** the fill freezes at the stage the run reached and the head
//!   turns crimson; on **completion** it reads a full success-green track.
//!
//! ## Cost
//!
//! One coalesced repaint per deck tick (see `deck_shell`): the shimmer/pulse are
//! pure functions of `model.now_ms`, so this never spins a timer of its own and
//! renders identically on replay. `--no-anim` (and `NO_COLOR`) freeze the motion
//! to a static frame for CI and recordings.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use stella_protocol::StageKind;

use crate::deck::{AgentEntry, WorkspaceModel};
use crate::deck_ui::DeckUi;
use crate::envelope::AgentStatus;
use crate::theme::{self, ColorMode};

/// The shimmer sweep's period, in ms — one pass of the light band across the
/// filled region (§3: ~1.6–1.8s).
const SHIMMER_PERIOD_MS: u64 = 1_700;
/// The head-pulse period, in ms — a gentle brighten/dim at the fill frontier.
const PULSE_PERIOD_MS: u64 = 900;

/// The three display phases the bar collapses the real [`StageKind`] pipeline
/// onto. The engine's ten stages are conditional and unordered-in-advance, so a
/// literal per-stage bar would lie about totals; these three are the stable
/// spine every turn actually walks.
const PHASE_LABELS: [&str; 3] = ["plan", "execute", "verify"];

/// One display phase's state, left → right across the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegState {
    /// The run has moved past this phase.
    Done,
    /// The run is in this phase right now.
    Active,
    /// The run has not reached this phase.
    Pending,
}

/// The run's lifecycle, as the bar reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    /// Nothing is running — a flat dim track, no shimmer, no head.
    Idle,
    /// A turn is in flight (`Running`/`WaitingInput`/`Paused`).
    Running,
    /// The turn finished cleanly — a full success-green track.
    Complete,
    /// The turn failed — fill frozen at the failure point, crimson head.
    Error,
}

/// The bar's fully-derived, render-ready state — a pure function of the model
/// (see [`ProgressState::derive`]) so it is unit-testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProgressState {
    pub phase: RunPhase,
    pub segments: [SegState; 3],
    /// Fill fraction of the track, `[0, 1]` — stage position, not fabricated %.
    pub fill: f64,
    /// The percent shown at the right, derived from `fill`.
    pub pct: u8,
    /// Real tokens/sec of the focused agent, or `None` when there's nothing
    /// honest to divide.
    pub tok_per_s: Option<u64>,
    /// Whether the shimmer / head-pulse should move this frame.
    pub animate: bool,
}

/// Which display phase (0=plan, 1=execute, 2=verify) a real stage belongs to.
fn stage_phase(stage: StageKind) -> usize {
    match stage {
        // Witness authoring is pre-execution work: it groups with planning on
        // the 3-segment display (the bar must not jump to "verify" before the
        // worker has run).
        StageKind::Triage
        | StageKind::ContextRecall
        | StageKind::Plan
        | StageKind::ScopeReview
        | StageKind::Witness => 0,
        StageKind::Execute => 1,
        StageKind::Verify | StageKind::Judge | StageKind::Reflect | StageKind::ContextWrite => 2,
        // Complete is handled via `Hud.complete`; treat as end-of-verify.
        StageKind::Complete => 2,
    }
}

/// The honest fill for an active phase: prior phases full, this phase to its
/// midpoint. Phase 0 → 1/6, phase 1 → 1/2, phase 2 → 5/6.
fn phase_fill(active: usize) -> f64 {
    (active as f64 + 0.5) / PHASE_LABELS.len() as f64
}

impl ProgressState {
    /// The idle bar — nothing running.
    fn idle() -> Self {
        Self {
            phase: RunPhase::Idle,
            segments: [SegState::Pending; 3],
            fill: 0.0,
            pct: 0,
            tok_per_s: None,
            animate: false,
        }
    }

    /// Derive the bar from the focused agent's real run state. `no_anim` forces
    /// a static frame (CI / recordings).
    pub fn derive(agent: Option<&AgentEntry>, now_ms: u64, no_anim: bool) -> Self {
        let Some(agent) = agent else {
            return Self::idle();
        };
        let hud = &agent.model.hud;
        let complete = hud.complete || agent.status == AgentStatus::Done;
        let error = matches!(agent.status, AgentStatus::Failed | AgentStatus::Killed);

        // Idle: no turn in flight and nothing to show — a flat track, exactly
        // like having no agent at all. Keyed on the header clock
        // (`turn_started_ms`), the one honest "a turn is running" signal: it is
        // set on `PromptStarted` and cleared by `end_turn` on completion. Status
        // alone is unreliable here — `WaitingInput` is `is_active()` yet is also
        // the post-command resting state, which would otherwise strand the bar
        // mid-fill after a handled command (e.g. `/init`) finishes.
        if !complete && !error && hud.stage.is_none() && agent.turn_started_ms.is_none() {
            return Self::idle();
        }

        let active = hud.stage.map(stage_phase);

        if complete {
            return Self {
                phase: RunPhase::Complete,
                segments: [SegState::Done; 3],
                fill: 1.0,
                pct: 100,
                tok_per_s: None,
                animate: false,
            };
        }

        // Running or failed: fill to the reached stage position (frozen there on
        // error). With no stage yet but an active status, we're at the very
        // start of plan.
        let active = active.unwrap_or(0);
        let fill = phase_fill(active);
        let segments = std::array::from_fn(|i| {
            use std::cmp::Ordering::*;
            match i.cmp(&active) {
                Less => SegState::Done,
                Equal => SegState::Active,
                Greater => SegState::Pending,
            }
        });

        let running = agent.status == AgentStatus::Running;
        let tok_per_s = if running {
            let elapsed_ms = agent.elapsed_ms(now_ms);
            (elapsed_ms > 0 && agent.tokens_out > 0)
                .then(|| agent.tokens_out.saturating_mul(1000) / elapsed_ms)
        } else {
            None
        };

        Self {
            phase: if error {
                RunPhase::Error
            } else {
                RunPhase::Running
            },
            segments,
            fill,
            pct: (fill * 100.0).round() as u8,
            tok_per_s,
            // Motion is liveness: only while genuinely Running, never when
            // paused / awaiting input / failed, and never under `--no-anim`.
            animate: running && !no_anim,
        }
    }
}

/// Render the progress bar for the focused agent into `area` (one row).
pub fn render(model: &WorkspaceModel, ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let state = ProgressState::derive(model.agents.get(ui.focused), model.now_ms, ui.no_anim);
    render_state(&state, model.now_ms, ui.color_mode, area, buf);
}

/// The label chunk (`✓ plan · ▸ execute · verify`) as styled spans, plus its
/// display width. Done = success green, Active = flame, Pending = dim.
fn label_line(state: &ProgressState) -> (Vec<Span<'static>>, usize) {
    let mut spans = Vec::new();
    let mut width = 0usize;
    for (i, name) in PHASE_LABELS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default().fg(theme::HAIRLINE)));
            width += 2;
        }
        let (glyph, color, bold) = match state.segments[i] {
            SegState::Done => ("✓", theme::SUCCESS_BRIGHT, false),
            SegState::Active if state.phase == RunPhase::Error => ("✗", theme::EMBER_CRIMSON, true),
            SegState::Active => ("▸", theme::EMBER_FLAME, true),
            SegState::Pending => ("·", theme::TEXT_DIM, false),
        };
        let mut style = Style::default().fg(color);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(format!("{glyph} {name}"), style));
        width += 2 + name.chars().count(); // glyph + space + name
    }
    (spans, width)
}

/// The right-aligned telemetry (`NN%  ·  <tok/s>`) as styled spans plus width.
/// Percent in primary text, the rest dim; ETA omitted (no honest estimate).
fn telemetry_line(state: &ProgressState) -> (Vec<Span<'static>>, usize) {
    match state.phase {
        RunPhase::Idle => (
            vec![Span::styled("idle", Style::default().fg(theme::TEXT_DIM))],
            4,
        ),
        RunPhase::Complete => (
            vec![Span::styled(
                "100% · done",
                Style::default().fg(theme::SUCCESS_BRIGHT),
            )],
            11,
        ),
        RunPhase::Error => (
            vec![Span::styled(
                "failed",
                Style::default()
                    .fg(theme::EMBER_CRIMSON)
                    .add_modifier(Modifier::BOLD),
            )],
            6,
        ),
        RunPhase::Running => {
            let pct = format!("{}%", state.pct);
            let mut spans = vec![Span::styled(
                pct.clone(),
                Style::default().fg(theme::TEXT_PRIMARY),
            )];
            let mut width = pct.chars().count();
            if let Some(tps) = state.tok_per_s {
                let tail = format!("  ·  {tps} tok/s");
                width += tail.chars().count();
                spans.push(Span::styled(tail, Style::default().fg(theme::TEXT_DIM)));
            }
            (spans, width)
        }
    }
}

/// Paint the derived state into `area`. Split out from [`render`] so tests can
/// drive it with a hand-built [`ProgressState`] and a fixed clock.
fn render_state(state: &ProgressState, now_ms: u64, mode: ColorMode, area: Rect, buf: &mut Buffer) {
    let y = area.y;
    let total = area.width as usize;
    let (labels, label_w) = label_line(state);
    let (telem, telem_w) = telemetry_line(state);

    // Zone layout: labels (left) · bar (middle, gets the rest) · telemetry
    // (right). On a narrow row, drop the labels first, then the telemetry, so
    // the bar itself always survives — it is the load-bearing element.
    let gap = 2usize; // one space either side of the bar
    let mut bar_x = area.x as usize;
    let mut bar_w = total;
    let mut show_labels = false;
    let mut show_telem = false;

    if total >= label_w + telem_w + gap + 8 {
        show_labels = true;
        show_telem = true;
        bar_x = area.x as usize + label_w + 1;
        bar_w = total - label_w - telem_w - gap;
    } else if total >= telem_w + 1 + 10 {
        show_telem = true;
        bar_x = area.x as usize;
        bar_w = total - telem_w - 1;
    }

    if show_labels {
        Paragraph::new(Line::from(labels)).render(
            Rect {
                x: area.x,
                y,
                width: label_w as u16,
                height: 1,
            },
            buf,
        );
    }
    if show_telem {
        let telem_x = area.x + (total - telem_w) as u16;
        Paragraph::new(Line::from(telem).alignment(ratatui::layout::Alignment::Right)).render(
            Rect {
                x: telem_x,
                y,
                width: telem_w as u16,
                height: 1,
            },
            buf,
        );
    }

    render_track(state, now_ms, mode, bar_x as u16, y, bar_w as u16, buf);
}

/// Paint just the fill track (gradient fill, dim groove, notches, shimmer,
/// pulsing head) into `[x, x+w)` on row `y`.
fn render_track(
    state: &ProgressState,
    now_ms: u64,
    mode: ColorMode,
    x: u16,
    y: u16,
    w: u16,
    buf: &mut Buffer,
) {
    let w = w as usize;
    if w == 0 {
        return;
    }
    let truecolor = mode.is_truecolor();
    let fill_cells = (state.fill * w as f64).round() as usize;
    // Notch columns mark the plan|execute|verify boundaries at 1/3 and 2/3.
    let notch1 = w / 3;
    let notch2 = (2 * w) / 3;

    // Shimmer: a light band whose center sweeps left→right within the filled
    // region only. A pure function of the clock — no persisted state.
    let shimmer_center = if state.animate && fill_cells > 0 {
        let t = (now_ms % SHIMMER_PERIOD_MS) as f64 / SHIMMER_PERIOD_MS as f64;
        Some(t * fill_cells as f64)
    } else {
        None
    };
    // Head pulse amount at the frontier.
    let pulse = if state.animate {
        let t = (now_ms % PULSE_PERIOD_MS) as f64 / PULSE_PERIOD_MS as f64;
        0.25 + 0.35 * (0.5 - (t - 0.5).abs()) * 2.0 // triangle 0.25→0.6→0.25
    } else {
        0.30
    };
    let head = fill_cells.saturating_sub(1); // last filled cell

    for i in 0..w {
        let Some(cell) = buf.cell_mut((x + i as u16, y)) else {
            continue;
        };

        // A notch marks a plan|execute|verify boundary — a thin divider glyph
        // that reads even with color stripped.
        if (i == notch1 || i == notch2) && w >= 6 {
            cell.set_symbol("┊");
            cell.set_fg(theme::HAIRLINE);
            continue;
        }

        if i < fill_cells {
            // The fill is a glyph (not a background), so the bar's *shape* reads
            // even under `NO_COLOR`, where every color drops to the terminal
            // default; the ember gradient rides the glyph's foreground.
            let t = if w > 1 {
                i as f64 / (w - 1) as f64
            } else {
                0.0
            };
            let mut fg = if truecolor {
                theme::ember_gradient(t)
            } else {
                theme::EMBER_FLAME
            };

            // Shimmer: a soft light band on truecolor (a lightened RGB has no
            // indexed fallback, so it must not reach a lesser terminal), which
            // degrades to a single moving highlight cell.
            if let Some(center) = shimmer_center {
                if truecolor {
                    let d = (i as f64 - center).abs();
                    if d < 2.5 {
                        fg = theme::lighten(fg, 0.4 * (1.0 - d / 2.5));
                    }
                } else if i == center.round() as usize {
                    fg = theme::EMBER_GOLD;
                }
            }

            // Pulsing head at the frontier: crimson on error, a lifted gradient
            // cell on truecolor, else a single bright cell.
            if i == head {
                if state.phase == RunPhase::Error {
                    fg = theme::EMBER_CRIMSON;
                } else if truecolor {
                    fg = theme::lighten(fg, pulse);
                } else {
                    fg = theme::EMBER_GOLD;
                }
            }

            // A completed run reads as a solid success-green bar.
            if state.phase == RunPhase::Complete {
                fg = theme::SUCCESS_BRIGHT;
            }
            cell.set_symbol("█");
            cell.set_fg(fg);
        } else {
            // Unfilled track — a dim groove.
            cell.set_symbol("░");
            cell.set_fg(theme::TEXT_DIM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{AgentMeta, Inbound};
    use stella_protocol::AgentEvent;

    fn agent_running(stage: StageKind) -> WorkspaceModel {
        let mut m = WorkspaceModel::new();
        m.now_ms = 10_000;
        m.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "goal", 0)));
        m.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Stage { name: stage },
        });
        m
    }

    fn focused(m: &WorkspaceModel) -> Option<&AgentEntry> {
        m.agents.first()
    }

    #[test]
    fn no_agent_is_idle() {
        let s = ProgressState::derive(None, 0, false);
        assert_eq!(s.phase, RunPhase::Idle);
        assert_eq!(s.fill, 0.0);
        assert!(!s.animate);
    }

    #[test]
    fn stage_maps_to_the_right_phase_and_fill() {
        let plan = agent_running(StageKind::Plan);
        let s = ProgressState::derive(focused(&plan), plan.now_ms, false);
        assert_eq!(s.phase, RunPhase::Running);
        assert_eq!(s.segments[0], SegState::Active);
        assert!(
            (s.fill - 1.0 / 6.0).abs() < 1e-9,
            "plan → 1/6, got {}",
            s.fill
        );

        let exec = agent_running(StageKind::Execute);
        let s = ProgressState::derive(focused(&exec), exec.now_ms, false);
        assert_eq!(s.segments[0], SegState::Done);
        assert_eq!(s.segments[1], SegState::Active);
        assert_eq!(s.segments[2], SegState::Pending);
        assert!((s.fill - 0.5).abs() < 1e-9);
        assert_eq!(s.pct, 50);

        let verify = agent_running(StageKind::Verify);
        let s = ProgressState::derive(focused(&verify), verify.now_ms, false);
        assert_eq!(s.segments[2], SegState::Active);
        assert!((s.fill - 5.0 / 6.0).abs() < 1e-9);
    }

    fn lead_registered(now_ms: u64) -> WorkspaceModel {
        let mut m = WorkspaceModel::new();
        m.now_ms = now_ms;
        m.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "goal", 0)));
        m
    }

    #[test]
    fn command_in_flight_with_no_stage_reads_in_progress_not_idle() {
        // A driver command (e.g. /init) emits no Stage events, but PromptStarted
        // starts the clock — the bar must show the default in-progress state
        // (plan), never a stale fill and never idle.
        let mut m = lead_registered(5_000);
        m.apply_inbound(&Inbound::PromptStarted {
            agent: "lead".into(),
            text: "/init".into(),
        });
        let s = ProgressState::derive(focused(&m), m.now_ms, false);
        assert_eq!(s.phase, RunPhase::Running, "clock running ⇒ in-progress");
        assert_eq!(
            s.segments[0],
            SegState::Active,
            "default in-progress = plan"
        );
        assert!(
            (s.fill - 1.0 / 6.0).abs() < 1e-9,
            "restarts at the beginning"
        );
    }

    #[test]
    fn resting_after_a_command_reads_idle_not_stranded() {
        // When a handled command completes, the clock stops (WaitingInput →
        // end_turn) and, with no stage/complete, the bar returns to idle — it is
        // never left frozen mid-fill even though WaitingInput is `is_active()`.
        let mut m = lead_registered(5_000);
        m.apply_inbound(&Inbound::PromptStarted {
            agent: "lead".into(),
            text: "/init".into(),
        });
        m.apply_inbound(&Inbound::Status {
            agent: "lead".into(),
            status: AgentStatus::WaitingInput,
        });
        let s = ProgressState::derive(focused(&m), m.now_ms, false);
        assert_eq!(s.phase, RunPhase::Idle, "clock stopped + no stage ⇒ idle");
        assert_eq!(s.fill, 0.0);
    }

    #[test]
    fn new_prompt_after_completion_restarts_from_the_beginning() {
        // A completed turn leaves the bar full-green; the NEXT submission must
        // reset it to the in-progress start, not resume frozen at verify/100%.
        let mut m = agent_running(StageKind::Verify);
        m.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Complete {
                model: "glm-5.2".into(),
                cost_usd: 0.1,
            },
        });
        assert_eq!(
            ProgressState::derive(focused(&m), m.now_ms, false).phase,
            RunPhase::Complete,
            "precondition: full-green"
        );
        m.apply_inbound(&Inbound::PromptStarted {
            agent: "lead".into(),
            text: "another".into(),
        });
        let s = ProgressState::derive(focused(&m), m.now_ms, false);
        assert_eq!(
            s.phase,
            RunPhase::Running,
            "reset to in-progress, not stale complete"
        );
        assert!(
            (s.fill - 1.0 / 6.0).abs() < 1e-9,
            "back to the plan-phase start, got {}",
            s.fill
        );
    }

    #[test]
    fn completion_fills_green_to_100() {
        let mut m = agent_running(StageKind::Verify);
        m.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Complete {
                model: "glm-5.2".into(),
                cost_usd: 0.1,
            },
        });
        let s = ProgressState::derive(focused(&m), m.now_ms, false);
        assert_eq!(s.phase, RunPhase::Complete);
        assert_eq!(s.fill, 1.0);
        assert_eq!(s.pct, 100);
        assert!(s.segments.iter().all(|&x| x == SegState::Done));
        assert!(!s.animate, "a finished run does not shimmer");
    }

    #[test]
    fn failure_freezes_and_stops_motion() {
        let mut m = agent_running(StageKind::Execute);
        m.apply_inbound(&Inbound::Status {
            agent: "lead".into(),
            status: AgentStatus::Failed,
        });
        let s = ProgressState::derive(focused(&m), m.now_ms, false);
        assert_eq!(s.phase, RunPhase::Error);
        // Frozen at the execute position — not advanced, not zeroed.
        assert!((s.fill - 0.5).abs() < 1e-9);
        assert!(!s.animate);
    }

    #[test]
    fn tok_per_s_is_real_or_omitted() {
        // Running with real tokens over real elapsed → a real rate.
        let mut m = agent_running(StageKind::Execute);
        if let Some(a) = m.agents.first_mut() {
            a.tokens_out = 500;
            a.meta.started_ms = 0;
        }
        m.now_ms = 10_000; // 10s
        let s = ProgressState::derive(focused(&m), m.now_ms, false);
        assert_eq!(s.tok_per_s, Some(50)); // 500 tok / 10 s
        // No tokens yet → omitted, never guessed.
        let plain = agent_running(StageKind::Execute);
        let s = ProgressState::derive(focused(&plain), plain.now_ms, false);
        assert_eq!(s.tok_per_s, None);
    }

    #[test]
    fn no_anim_forces_a_static_frame() {
        let exec = agent_running(StageKind::Execute);
        let s = ProgressState::derive(focused(&exec), exec.now_ms, true);
        assert!(!s.animate, "--no-anim freezes the shimmer/pulse");
    }

    #[test]
    fn renders_without_panic_at_narrow_and_wide_widths() {
        for w in [8u16, 20, 40, 80, 200] {
            let exec = agent_running(StageKind::Execute);
            let state = ProgressState::derive(focused(&exec), exec.now_ms, false);
            let area = Rect::new(0, 0, w, 1);
            let mut buf = Buffer::empty(area);
            render_state(&state, 1234, ColorMode::Truecolor, area, &mut buf);
            // The bar painted a filled glyph for a mid-run state on any width.
            let filled = (0..w).any(|x| buf.cell((x, 0)).is_some_and(|c| c.symbol() == "█"));
            assert!(filled, "width {w} should paint a fill");
        }
    }

    #[test]
    fn non_truecolor_fill_uses_named_tokens_never_an_interpolated_rgb() {
        let exec = agent_running(StageKind::Execute);
        let state = ProgressState::derive(focused(&exec), exec.now_ms, false);
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        // The track itself (not the labels/telemetry) is what must degrade
        // cleanly — an interpolated gradient RGB has no indexed fallback.
        render_track(&state, 1234, ColorMode::Ansi256, 0, 0, 40, &mut buf);
        let allowed = [
            theme::EMBER_FLAME,
            theme::EMBER_GOLD,
            theme::TEXT_DIM,
            theme::HAIRLINE,
            ratatui::style::Color::Reset,
        ];
        for x in 0..40 {
            if let Some(c) = buf.cell((x, 0)) {
                assert!(allowed.contains(&c.fg), "unexpected fg {:?} at x={x}", c.fg);
            }
        }
    }

    #[test]
    fn no_color_keeps_the_bar_shape() {
        // Under NO_COLOR every color drops to the terminal default, but the
        // fill glyph must survive so the determinate bar still conveys progress.
        let exec = agent_running(StageKind::Execute);
        let state = ProgressState::derive(focused(&exec), exec.now_ms, false);
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        render_state(&state, 1234, ColorMode::Truecolor, area, &mut buf);
        theme::degrade_buffer(&mut buf, ColorMode::None);
        let filled = (0..40)
            .filter(|&x| buf.cell((x, 0)).is_some_and(|c| c.symbol() == "█"))
            .count();
        let track = (0..40)
            .filter(|&x| buf.cell((x, 0)).is_some_and(|c| c.symbol() == "░"))
            .count();
        assert!(filled > 0, "the fill shape survives NO_COLOR");
        assert!(track > 0, "the track shape survives NO_COLOR");
        // …and every color really was stripped to the default.
        assert!(
            (0..40).all(|x| buf
                .cell((x, 0))
                .is_some_and(|c| c.fg == ratatui::style::Color::Reset)),
            "NO_COLOR leaves no residual color"
        );
    }
}
