//! The animated launch cinematic shown on deck launch and replayed on `/init`.
//!
//! The timeline is a four-phase movie, never shorter than ~6 seconds:
//!
//! 1. **Battle** ([`crate::invaders`]) — a Space Invaders skirmish over a
//!    drifting starfield: the cannon picks off invaders, the fleet returns
//!    fire, ships explode. Runs at least [`BATTLE_MIN`]; while the splash is
//!    **held** open over a still-running init (session startup, `/init`) the
//!    battle loops until [`SplashState::release`] — the movie covers the
//!    wait, however long a first launch's indexing takes.
//! 2. **Reveal** — the STELLA block-art wordmark ([`tui_big_text`])
//!    materializes cell-by-cell (a seeded [`tachyonfx`] coalesce), stars
//!    still falling behind it.
//! 3. **Hold** — the wordmark stands fully lit.
//! 4. **Fade** — the whole frame fades out to the deck.
//!
//! The splash stays **skippable** on any key so it can never block getting
//! to work, and `--no-anim` (CI, recordings) collapses it to a brief static
//! wordmark. `render` takes `&SplashState` (immutable — the deck draws every
//! frame with no `&mut` path back into the model), so the animation is
//! *scrubbed* from elapsed time rather than driven by persisted effects:
//! each frame builds a **fresh** seeded effect and processes it once with a
//! synthetic elapsed duration. Since a `tachyonfx::EffectTimer` only cares
//! about total elapsed time (not how it got there), one `process` call lands
//! exactly where continuous real-time playback would have.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use tachyonfx::{Duration as FxDuration, EffectTimer, Interpolation, SimpleRng, fx};
use tui_big_text::{BigText, PixelSize};

use crate::{invaders, theme};

/// Minimum battle time before the wordmark may reveal — also the loop the
/// battle wraps at while held. With the wordmark phases on top, the whole
/// movie always runs ≥ ~6.3s (the product floor is 5s).
pub const BATTLE_MIN: Duration = Duration::from_millis(3500);

/// The wordmark phases: patterned coalesce in, full-lit hold, fade out.
const REVEAL: Duration = Duration::from_millis(1300);
const WORDMARK_HOLD: Duration = Duration::from_millis(600);
const FADE: Duration = Duration::from_millis(900);

/// A held splash that never hears `release()` (a driver that died mid-init)
/// still ends: past this cap the battle concedes and the reveal plays. Any
/// key skips long before this matters.
const HOLD_FAILSAFE: Duration = Duration::from_secs(120);

/// `--no-anim`: the whole cinematic collapses to this brief static wordmark.
const REDUCED_TOTAL: Duration = Duration::from_millis(1200);

/// Below this width/height, `tui-big-text`'s wordmark (48 cols wide, plus
/// subtitle and breathing room) won't fit legibly — fall back to a single
/// styled line instead of clipping it.
const MIN_WORDMARK_WIDTH: u16 = 50;
const MIN_WORDMARK_HEIGHT: u16 = 8;

/// Terminals at least this tall get the full 8-row block glyphs
/// ([`PixelSize::Full`]); shorter ones get the 4-row half-height packing.
const FULL_GLYPH_MIN_HEIGHT: u16 = 20;

/// Where the timeline stands right now — the render dispatch.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Mid-battle; carries the absolute elapsed seconds (the scene wraps it).
    Battle(f32),
    /// Wordmark coalescing in; carries local progress `0.0..1.0`.
    Reveal(f32),
    /// Wordmark fully lit.
    Hold,
    /// Whole frame fading out; carries local progress `0.0..1.0`.
    Fade(f32),
    /// Movie over — the deck owns the frame.
    Over,
}

/// Ephemeral splash timing. Not part of the model — pure presentation.
#[derive(Debug, Clone)]
pub struct SplashState {
    start: Instant,
    /// When `skip()` dismissed the splash early, if it did — the moment the
    /// deck took over, used by [`Self::finished_at`].
    skipped_at: Option<Instant>,
    /// Held open over a running init: the battle loops until `release()`.
    held: bool,
    /// When `release()` ended the hold (init finished).
    released_at: Option<Instant>,
    /// `--no-anim`: collapse to a brief static wordmark, ignore hold cues.
    reduced: bool,
}

impl Default for SplashState {
    fn default() -> Self {
        Self::new()
    }
}

impl SplashState {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            skipped_at: None,
            held: false,
            released_at: None,
            reduced: false,
        }
    }

    /// A splash held open over a running init: the battle loops until
    /// [`Self::release`]. The driver replays the cinematic this way at
    /// session start and on `/init`.
    pub fn new_held() -> Self {
        Self {
            held: true,
            ..Self::new()
        }
    }

    /// Init finished — let the timeline advance to the wordmark and fade.
    /// The battle still runs out its [`BATTLE_MIN`] floor first, so a fast
    /// init never truncates the movie below the product minimum. Idempotent;
    /// a no-op on an unheld splash.
    pub fn release(&mut self) {
        if self.held && self.released_at.is_none() {
            self.released_at = Some(Instant::now());
        }
    }

    /// `--no-anim` / `STELLA_NO_ANIM` / `NO_COLOR`: collapse the cinematic
    /// to a brief static wordmark (no battle, no effects).
    pub fn set_reduced(&mut self, reduced: bool) {
        self.reduced = reduced;
    }

    /// Dismiss immediately (any key). Idempotent — the first skip wins, so
    /// `finished_at` stays stable under repeated keypresses.
    pub fn skip(&mut self) {
        if self.skipped_at.is_none() {
            self.skipped_at = Some(Instant::now());
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// When the battle phase ends, relative to `start` — or `None` while the
    /// splash is held open and init hasn't finished (the battle loops).
    fn battle_end(&self) -> Option<Duration> {
        if self.reduced {
            return Some(Duration::ZERO);
        }
        if !self.held {
            return Some(BATTLE_MIN);
        }
        match self.released_at {
            Some(at) => Some(BATTLE_MIN.max(at.duration_since(self.start))),
            None if self.elapsed() >= HOLD_FAILSAFE => Some(HOLD_FAILSAFE),
            None => None,
        }
    }

    /// The whole movie's length, once known ([`Self::battle_end`]).
    fn total(&self) -> Option<Duration> {
        if self.reduced {
            return Some(REDUCED_TOTAL);
        }
        self.battle_end()
            .map(|be| be + REVEAL + WORDMARK_HOLD + FADE)
    }

    fn phase(&self) -> Phase {
        if self.skipped_at.is_some() {
            return Phase::Over;
        }
        let e = self.elapsed();
        if self.reduced {
            return if e < REDUCED_TOTAL {
                Phase::Hold
            } else {
                Phase::Over
            };
        }
        let be = match self.battle_end() {
            None => return Phase::Battle(e.as_secs_f32()),
            Some(be) => be,
        };
        if e < be {
            Phase::Battle(e.as_secs_f32())
        } else if e < be + REVEAL {
            Phase::Reveal((e - be).as_secs_f32() / REVEAL.as_secs_f32())
        } else if e < be + REVEAL + WORDMARK_HOLD {
            Phase::Hold
        } else if e < be + REVEAL + WORDMARK_HOLD + FADE {
            Phase::Fade((e - be - REVEAL - WORDMARK_HOLD).as_secs_f32() / FADE.as_secs_f32())
        } else {
            Phase::Over
        }
    }

    /// True once the splash should hand off to the deck.
    pub fn is_done(&self) -> bool {
        self.phase() == Phase::Over
    }

    /// The moment the splash finished (skipped or played out), or `None`
    /// while it is still playing. The deck uses this to time its reveal fade
    /// (`deck_render` drives [`crate::fx::fade_in`] from it).
    pub fn finished_at(&self) -> Option<Instant> {
        if let Some(at) = self.skipped_at {
            return Some(at);
        }
        match self.total() {
            Some(total) if self.elapsed() >= total => Some(self.start + total),
            _ => None,
        }
    }
}

/// Draw the current frame of the cinematic. Battle → coalescing wordmark →
/// hold → fade, all scrubbed from [`SplashState`]'s clock; reduced splashes
/// render one static wordmark frame.
pub fn render(state: &SplashState, area: Rect, buf: &mut Buffer) {
    if state.reduced {
        render_wordmark_or_fallback(area, buf);
        return;
    }
    let elapsed = state.elapsed().as_secs_f32();
    match state.phase() {
        Phase::Over => {}
        Phase::Battle(t) => invaders::render(t, area, buf),
        Phase::Reveal(local) => {
            invaders::render_stars(elapsed, area, buf);
            let block = render_wordmark_or_fallback(area, buf);
            // The patterned reveal: a seeded coalesce scrubbed to `local`,
            // applied to the wordmark block only so the starfield behind it
            // doesn't flicker in and out with the effect's cell pattern.
            let mut effect = fx::coalesce(EffectTimer::new(
                FxDuration::from(REVEAL),
                Interpolation::QuadOut,
            ))
            .with_rng(SimpleRng::new(crate::fx::FX_SEED));
            crate::fx::apply(
                &mut effect,
                REVEAL.mul_f32(local.clamp(0.0, 1.0)),
                block,
                buf,
            );
        }
        Phase::Hold => {
            invaders::render_stars(elapsed, area, buf);
            render_wordmark_or_fallback(area, buf);
        }
        Phase::Fade(local) => {
            invaders::render_stars(elapsed, area, buf);
            render_wordmark_or_fallback(area, buf);
            // Fade the whole frame — wordmark and stars — down to the ground
            // color for the handoff.
            let mut effect = fx::fade_to(
                theme::GROUND,
                theme::GROUND,
                EffectTimer::new(FxDuration::from(FADE), Interpolation::QuadIn),
            );
            crate::fx::apply(&mut effect, FADE.mul_f32(local.clamp(0.0, 1.0)), area, buf);
        }
    }
}

/// Draw the wordmark (or the tiny-terminal fallback) and return the block it
/// occupies, for effects that should touch only the wordmark.
fn render_wordmark_or_fallback(area: Rect, buf: &mut Buffer) -> Rect {
    if area.width < MIN_WORDMARK_WIDTH || area.height < MIN_WORDMARK_HEIGHT {
        render_fallback(area, buf)
    } else {
        render_wordmark(area, buf)
    }
}

/// The full wordmark: a big block-art "STELLA" in Stella amber over a muted
/// "command deck" subtitle, vertically centered as one block. Tall terminals
/// get the full 8-row glyphs; shorter ones the 4-row half-height packing.
fn render_wordmark(area: Rect, buf: &mut Buffer) -> Rect {
    let (pixel_size, title_height) = if area.height >= FULL_GLYPH_MIN_HEIGHT {
        (PixelSize::Full, 8)
    } else {
        (PixelSize::HalfHeight, 4)
    };
    const GAP: u16 = 1;
    const SUBTITLE_HEIGHT: u16 = 1;
    let block_height = title_height + GAP + SUBTITLE_HEIGHT;

    let top = area.y + (area.height.saturating_sub(block_height)) / 2;

    let title_area = Rect {
        x: area.x,
        y: top,
        width: area.width,
        height: title_height,
    };
    let wordmark = BigText::builder()
        .pixel_size(pixel_size)
        .style(theme::accent())
        .alignment(Alignment::Center)
        .lines(vec![Line::from("STELLA")])
        .build();
    wordmark.render(title_area, buf);

    let subtitle_area = Rect {
        x: area.x,
        y: top + title_height + GAP,
        width: area.width,
        height: SUBTITLE_HEIGHT,
    };
    let subtitle =
        Line::from(Span::styled("command deck", theme::muted())).alignment(Alignment::Center);
    Paragraph::new(subtitle).render(subtitle_area, buf);

    Rect {
        x: area.x,
        y: top,
        width: area.width,
        height: block_height.min(area.height),
    }
}

/// Compact single-line fallback for terminals too small for the big-text
/// wordmark — still centered, still on-brand, never clipped or garbled.
fn render_fallback(area: Rect, buf: &mut Buffer) -> Rect {
    let line = Line::from(vec![
        Span::styled("✦ STELLA", theme::accent()),
        Span::styled(" — command deck", theme::muted()),
    ])
    .alignment(Alignment::Center);

    let y = area.y + (area.height.saturating_sub(1)) / 2;
    let row = Rect {
        x: area.x,
        y,
        width: area.width,
        height: area.height.min(1),
    };
    Paragraph::new(line).render(row, buf);
    row
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    /// Flatten a `TestBackend` buffer to one `String` per row (content, not
    /// raw ANSI — matches the rest of the crate's render tests).
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

    fn has_non_space(rows: &[String]) -> bool {
        rows.iter().any(|row| row.chars().any(|c| c != ' '))
    }

    fn draw(state: &SplashState, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render(state, area, f.buffer_mut());
            })
            .unwrap();
        buffer_rows(terminal.backend().buffer())
    }

    /// Backdates `start` so `elapsed()` reads as `elapsed_ms` without a real
    /// sleep. Legal only because `mod tests` is a descendant of the module
    /// that defines `SplashState`'s private fields.
    fn state_at(elapsed_ms: u64) -> SplashState {
        SplashState {
            start: Instant::now() - Duration::from_millis(elapsed_ms),
            ..SplashState::new()
        }
    }

    /// Total un-held movie length, in ms — the point past which `is_done()`.
    fn total_ms() -> u64 {
        (BATTLE_MIN + REVEAL + WORDMARK_HOLD + FADE).as_millis() as u64
    }

    #[test]
    fn the_movie_runs_at_least_five_seconds() {
        // The product floor: the cinematic may never be shorter than 5s.
        assert!(total_ms() >= 5_000, "movie is {}ms", total_ms());
    }

    #[test]
    fn new_state_is_not_done_and_starts_in_battle() {
        let state = SplashState::new();
        assert!(!state.is_done());
        assert!(matches!(state.phase(), Phase::Battle(_)));
    }

    #[test]
    fn default_matches_new() {
        let state = SplashState::default();
        assert!(!state.is_done());
        assert!(state.skipped_at.is_none());
        assert!(!state.held);
    }

    #[test]
    fn skip_finishes_the_splash_immediately() {
        let mut state = SplashState::new();
        assert!(!state.is_done());
        state.skip();
        assert!(state.is_done());
    }

    #[test]
    fn timeline_walks_battle_reveal_hold_fade_done() {
        let battle = BATTLE_MIN.as_millis() as u64;
        assert!(matches!(state_at(battle - 200).phase(), Phase::Battle(_)));
        assert!(matches!(state_at(battle + 200).phase(), Phase::Reveal(_)));
        let hold_at = battle + REVEAL.as_millis() as u64 + 100;
        assert_eq!(state_at(hold_at).phase(), Phase::Hold);
        let fade_at = hold_at + WORDMARK_HOLD.as_millis() as u64 + 100;
        assert!(matches!(state_at(fade_at).phase(), Phase::Fade(_)));
        assert_eq!(state_at(total_ms() + 100).phase(), Phase::Over);
    }

    #[test]
    fn a_held_splash_loops_the_battle_until_released() {
        // Held + un-released, way past BATTLE_MIN: still in battle, not done.
        let mut state = SplashState {
            held: true,
            ..state_at(10_000)
        };
        assert!(matches!(state.phase(), Phase::Battle(_)));
        assert!(!state.is_done());
        assert!(state.finished_at().is_none());

        // Release: battle ends at the release moment (past the floor), and
        // the wordmark phases run from there.
        state.release();
        assert!(matches!(state.phase(), Phase::Reveal(_)));
    }

    #[test]
    fn an_early_release_still_honors_the_battle_floor() {
        // Init finished after 1s — the battle still runs out BATTLE_MIN.
        let mut state = SplashState {
            held: true,
            ..state_at(1_000)
        };
        state.release();
        assert!(matches!(state.phase(), Phase::Battle(_)));
        assert_eq!(state.battle_end(), Some(BATTLE_MIN));
    }

    #[test]
    fn release_is_a_no_op_on_an_unheld_splash() {
        let mut state = state_at(100);
        state.release();
        assert!(state.released_at.is_none());
        assert_eq!(state.battle_end(), Some(BATTLE_MIN));
    }

    #[test]
    fn a_stuck_hold_ends_at_the_failsafe() {
        let state = SplashState {
            held: true,
            ..state_at((HOLD_FAILSAFE.as_millis() as u64) + 10_000)
        };
        assert!(
            state.is_done(),
            "a held splash that never hears release() must still end"
        );
    }

    #[test]
    fn finished_at_is_none_while_playing_then_stable_once_done() {
        // Still playing: no finish moment yet.
        assert!(state_at(100).finished_at().is_none());

        // Played out: finishes exactly at start + the full movie.
        let timed_out = state_at(total_ms() + 500);
        let fin = timed_out.finished_at().expect("past the timeline");
        assert_eq!(
            fin,
            timed_out.start + BATTLE_MIN + REVEAL + WORDMARK_HOLD + FADE
        );

        // Skipped: the first skip pins the moment; a second skip won't move it.
        let mut skipped = SplashState::new();
        skipped.skip();
        let first = skipped.finished_at().expect("skip finishes the splash");
        skipped.skip();
        assert_eq!(skipped.finished_at(), Some(first));
    }

    #[test]
    fn the_battle_paints_the_frame() {
        let rows = draw(&state_at(1_200), 80, 24);
        assert!(has_non_space(&rows), "mid-battle frames are never blank");
    }

    #[test]
    fn the_reveal_ends_with_the_wordmark_fully_visible() {
        // Just past the coalesce: the STELLA block glyphs are materialized.
        let at = BATTLE_MIN.as_millis() as u64 + REVEAL.as_millis() as u64 + 100;
        let rows = draw(&state_at(at), 80, 24);
        assert!(
            rows.iter().any(|r| r.contains('█')),
            "block-art wordmark should be visible at the hold"
        );
        assert!(
            rows.iter().any(|r| r.contains("command deck")),
            "subtitle should be visible at the hold"
        );
    }

    #[test]
    fn tall_terminals_get_the_full_height_glyphs() {
        let at = BATTLE_MIN.as_millis() as u64 + REVEAL.as_millis() as u64 + 100;
        let tall = draw(&state_at(at), 80, 30);
        let short = draw(&state_at(at), 80, 16);
        let block_rows = |rows: &[String]| rows.iter().filter(|r| r.contains('█')).count();
        assert!(
            block_rows(&tall) > block_rows(&short),
            "PixelSize::Full should paint more glyph rows than HalfHeight"
        );
    }

    #[test]
    fn renders_the_compact_fallback_on_a_tiny_terminal() {
        let at = BATTLE_MIN.as_millis() as u64 + REVEAL.as_millis() as u64 + 100;
        let rows = draw(&state_at(at), 30, 5);
        let text = rows.join("");
        assert!(
            text.contains("STELLA"),
            "tiny terminals get the literal fallback line"
        );
    }

    #[test]
    fn a_reduced_splash_is_one_brief_static_wordmark() {
        let mut state = state_at(100);
        state.set_reduced(true);
        assert_eq!(state.phase(), Phase::Hold);
        let rows = draw(&state, 80, 24);
        assert!(has_non_space(&rows), "reduced splash shows the wordmark");

        let mut over = state_at(REDUCED_TOTAL.as_millis() as u64 + 100);
        over.set_reduced(true);
        assert!(over.is_done(), "reduced splash ends after its brief hold");
        assert!(over.finished_at().is_some());
    }

    #[test]
    fn a_reduced_splash_ignores_hold_cues() {
        let mut state = SplashState {
            held: true,
            ..state_at(REDUCED_TOTAL.as_millis() as u64 + 100)
        };
        state.set_reduced(true);
        assert!(state.is_done(), "reduced splashes never loop the battle");
    }

    #[test]
    fn render_does_not_panic_across_the_whole_timeline() {
        // Battle, the hand-offs, reveal, hold, fade, and past the end — at
        // full, small-battle, fallback, and degenerate sizes.
        let battle = BATTLE_MIN.as_millis() as u64;
        let marks = [
            0,
            400,
            1_750,
            battle - 30,
            battle + 30,
            battle + 700,
            battle + REVEAL.as_millis() as u64 + 100,
            battle + (REVEAL + WORDMARK_HOLD).as_millis() as u64 + 100,
            total_ms() - 30,
            total_ms() + 200,
        ];
        for &(w, h) in &[(80_u16, 24_u16), (44, 14), (30, 5), (0, 0), (1, 1)] {
            for &ms in &marks {
                let _ = draw(&state_at(ms), w, h);
            }
            let mut skipped = SplashState::new();
            skipped.skip();
            let _ = draw(&skipped, w, h);
        }
    }

    #[test]
    fn a_skipped_splash_renders_nothing() {
        let mut state = SplashState::new();
        state.skip();
        let rows = draw(&state, 80, 24);
        assert!(
            !has_non_space(&rows),
            "a skipped splash must leave the frame to the deck"
        );
    }
}
