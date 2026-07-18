//! The animated branded splash shown on deck launch.
//!
//! Timing/state lives here; the wordmark ([`tui_big_text`]) and the
//! coalesce-in / dissolve-out ([`tachyonfx`]) are drawn in [`render`]. The
//! splash is **time-boxed** and **skippable** on any key so it can never
//! block getting to work.
//!
//! `render` takes `&SplashState` (immutable — the deck draws every frame
//! with no `&mut` path back into the model), so the animation is *scrubbed*
//! from [`SplashState::progress`] rather than driven by a persisted,
//! time-advancing `tachyonfx::Effect`: each frame builds a **fresh**
//! coalesce or dissolve effect and processes it, once, with a synthetic
//! elapsed duration computed from `progress()` instead of a real frame
//! delta. Since a `tachyonfx::EffectTimer` only cares about total elapsed
//! time (not how it got there), one `process(synthetic_elapsed, ..)` call on
//! a never-before-touched effect lands it exactly where continuous real-time
//! playback would have — no persisted timer, no interior mutability, and
//! `skip()` trivially scrubs to the fully-dissolved end state.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use tachyonfx::{Duration as FxDuration, Effect, EffectTimer, Interpolation, SimpleRng, fx};
use tui_big_text::{BigText, PixelSize};

use crate::theme;

/// Total on-screen time before the splash auto-dismisses.
pub const SPLASH_DURATION: Duration = Duration::from_millis(1600);

/// Fraction of [`SPLASH_DURATION`] spent coalescing the wordmark into view;
/// the remainder dissolves it back out.
const REVEAL_FRACTION: f32 = 0.6;

/// Below this width/height, `tui-big-text`'s wordmark (48 cols x 4 rows at
/// [`PixelSize::HalfHeight`], plus subtitle and breathing room) won't fit
/// legibly — fall back to a single styled line instead of clipping it.
const MIN_WORDMARK_WIDTH: u16 = 50;
const MIN_WORDMARK_HEIGHT: u16 = 8;

/// Ephemeral splash timing. Not part of the model — pure presentation.
#[derive(Debug, Clone)]
pub struct SplashState {
    start: Instant,
    /// When `skip()` dismissed the splash early, if it did — the moment the
    /// deck took over, used by [`Self::finished_at`].
    skipped_at: Option<Instant>,
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
        }
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

    /// Progress `0.0..=1.0` through the splash timeline.
    pub fn progress(&self) -> f32 {
        if self.skipped_at.is_some() {
            return 1.0;
        }
        (self.elapsed().as_secs_f32() / SPLASH_DURATION.as_secs_f32()).clamp(0.0, 1.0)
    }

    /// True once the splash should hand off to the deck.
    pub fn is_done(&self) -> bool {
        self.skipped_at.is_some() || self.elapsed() >= SPLASH_DURATION
    }

    /// The moment the splash finished (skipped or timed out), or `None` while
    /// it is still playing. The deck uses this to time its reveal fade
    /// (`deck_render` drives [`crate::fx::fade_in`] from it).
    pub fn finished_at(&self) -> Option<Instant> {
        if let Some(at) = self.skipped_at {
            return Some(at);
        }
        if self.elapsed() >= SPLASH_DURATION {
            return Some(self.start + SPLASH_DURATION);
        }
        None
    }
}

/// Draw the splash: a centered "STELLA" wordmark over a muted "command
/// deck" subtitle, coalescing into view over the first ~60% of the timeline
/// and dissolving back out as `state.progress()` approaches `1.0`. Falls
/// back to a single compact line on terminals too small for the big-text
/// wordmark.
pub fn render(state: &SplashState, area: Rect, buf: &mut Buffer) {
    if area.width < MIN_WORDMARK_WIDTH || area.height < MIN_WORDMARK_HEIGHT {
        render_fallback(area, buf);
    } else {
        render_wordmark(area, buf);
    }

    let (mut effect, elapsed) = timeline_effect(state.progress());
    crate::fx::apply(&mut effect, elapsed, area, buf);
}

/// Picks the coalesce-in or dissolve-out effect for the current splash
/// `progress`, paired with the synthetic elapsed duration that, applied to a
/// *fresh* copy of that effect, lands it exactly where continuous real-time
/// playback would have at this point in the timeline. Both effects run on
/// the pinned [`crate::fx::FX_SEED`] RNG so the random cell pattern is
/// identical across per-frame rebuilds (an unseeded effect would re-roll the
/// pattern every frame and the scrub would read as flicker).
fn timeline_effect(progress: f32) -> (Effect, Duration) {
    let reveal_dur = SPLASH_DURATION.mul_f32(REVEAL_FRACTION);
    let dissolve_dur = SPLASH_DURATION.saturating_sub(reveal_dur);

    if progress < REVEAL_FRACTION {
        // Splash-specific: the coalesce-in is not part of the deck's shared
        // motion language (crate::fx), so it is built here.
        let local = (progress / REVEAL_FRACTION).clamp(0.0, 1.0);
        let effect = fx::coalesce(EffectTimer::new(
            FxDuration::from(reveal_dur),
            Interpolation::QuadOut,
        ))
        .with_rng(SimpleRng::new(crate::fx::FX_SEED));
        (effect, reveal_dur.mul_f32(local))
    } else {
        // The dissolve IS the deck's shared teardown motion — reuse it.
        let local = ((progress - REVEAL_FRACTION) / (1.0 - REVEAL_FRACTION)).clamp(0.0, 1.0);
        let effect = crate::fx::dissolve_out(dissolve_dur.as_millis() as u32);
        (effect, dissolve_dur.mul_f32(local))
    }
}

/// The full wordmark: a big "STELLA" in Stella amber over a muted "command
/// deck" subtitle, vertically centered as one block.
fn render_wordmark(area: Rect, buf: &mut Buffer) {
    const TITLE_HEIGHT: u16 = 4; // PixelSize::HalfHeight packs the 8px glyph into 4 rows.
    const GAP: u16 = 1;
    const SUBTITLE_HEIGHT: u16 = 1;
    const BLOCK_HEIGHT: u16 = TITLE_HEIGHT + GAP + SUBTITLE_HEIGHT;

    let top = area.y + (area.height.saturating_sub(BLOCK_HEIGHT)) / 2;

    let title_area = Rect {
        x: area.x,
        y: top,
        width: area.width,
        height: TITLE_HEIGHT,
    };
    let wordmark = BigText::builder()
        .pixel_size(PixelSize::HalfHeight)
        .style(theme::accent())
        .alignment(Alignment::Center)
        .lines(vec![Line::from("STELLA")])
        .build();
    wordmark.render(title_area, buf);

    let subtitle_area = Rect {
        x: area.x,
        y: top + TITLE_HEIGHT + GAP,
        width: area.width,
        height: SUBTITLE_HEIGHT,
    };
    let subtitle =
        Line::from(Span::styled("command deck", theme::muted())).alignment(Alignment::Center);
    Paragraph::new(subtitle).render(subtitle_area, buf);
}

/// Compact single-line fallback for terminals too small for the big-text
/// wordmark — still centered, still on-brand, never clipped or garbled.
fn render_fallback(area: Rect, buf: &mut Buffer) {
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
            skipped_at: None,
        }
    }

    /// A few milliseconds into the dissolve phase (just past the 60%
    /// coalesce/dissolve hand-off). `EffectTimer`'s elapsed-time conversion
    /// truncates to whole milliseconds, so this reads as the dissolve
    /// effect's alpha `0.0` (its very start) regardless of how many
    /// microseconds of test overhead land between construction and render —
    /// i.e. deterministically fully visible, not a coin flip.
    const HANDOFF_MS: u64 = 961;

    #[test]
    fn new_state_is_not_done_and_has_zero_ish_progress() {
        let state = SplashState::new();
        assert!(!state.is_done());
        assert!(state.progress() < 0.05);
    }

    #[test]
    fn default_matches_new() {
        let state = SplashState::default();
        assert!(!state.is_done());
        assert!(state.skipped_at.is_none());
    }

    #[test]
    fn skip_finishes_the_splash_immediately() {
        let mut state = SplashState::new();
        assert!(!state.is_done());
        state.skip();
        assert!(state.is_done());
        assert_eq!(state.progress(), 1.0);
    }

    #[test]
    fn finished_at_is_none_while_playing_then_stable_once_done() {
        // Still playing: no finish moment yet.
        assert!(state_at(100).finished_at().is_none());

        // Timed out: finishes exactly at start + SPLASH_DURATION.
        let timed_out = state_at(2_000);
        let fin = timed_out.finished_at().expect("past the timeline");
        assert_eq!(fin, timed_out.start + SPLASH_DURATION);

        // Skipped: the first skip pins the moment; a second skip won't move it.
        let mut skipped = SplashState::new();
        skipped.skip();
        let first = skipped.finished_at().expect("skip finishes the splash");
        skipped.skip();
        assert_eq!(skipped.finished_at(), Some(first));
    }

    #[test]
    fn progress_is_clamped_to_unit_range() {
        let state = SplashState::new();
        let p = state.progress();
        assert!((0.0..=1.0).contains(&p));
    }

    #[test]
    fn renders_the_big_wordmark_on_a_roomy_terminal_at_the_handoff_point() {
        let rows = draw(&state_at(HANDOFF_MS), 80, 24);
        assert!(
            has_non_space(&rows),
            "wordmark should be (fully) visible right past coalesce-in"
        );
    }

    #[test]
    fn renders_the_compact_fallback_on_a_tiny_terminal_at_the_handoff_point() {
        let rows = draw(&state_at(HANDOFF_MS), 30, 5);
        let text = rows.join("");
        assert!(
            text.contains("STELLA"),
            "tiny terminals get the literal fallback line"
        );
    }

    #[test]
    fn a_brand_new_splash_starts_hidden_and_coalesces_in() {
        // progress() ~= 0: the coalesce effect is essentially unstarted, so
        // almost nothing has materialized yet — the opposite end of the
        // reveal from the handoff-point test above.
        let rows = draw(&state_at(0), 80, 24);
        assert!(
            !has_non_space(&rows),
            "a just-started splash should still read as (near-)blank"
        );
    }

    #[test]
    fn render_does_not_panic_across_the_whole_progress_timeline() {
        // Exercise the coalesce phase, the hand-off point, and the dissolve
        // phase, at both the big-text and fallback sizes, plus degenerate
        // (zero/one-cell) areas.
        for &skip in &[true, false] {
            for &(w, h) in &[(80_u16, 24_u16), (30, 5), (0, 0), (1, 1)] {
                let mut state = SplashState::new();
                if skip {
                    state.skip();
                }
                let _ = draw(&state, w, h);
            }
        }
        for &elapsed_ms in &[0, 100, 800, 960, HANDOFF_MS, 1200, 1600, 2000] {
            let _ = draw(&state_at(elapsed_ms), 80, 24);
        }
    }

    #[test]
    fn fully_dissolved_end_state_leaves_the_wordmark_area_blank() {
        // At progress() == 1.0, the dissolve effect is processed with its
        // own full duration, landing its alpha at exactly 1.0 — which clears
        // every cell it touches deterministically (tachyonfx's dissolve
        // threshold is `cell_alpha > rng()` sampled from [0.0, 1.0), so
        // alpha == 1.0 always wins).
        let mut state = SplashState::new();
        state.skip();
        let rows = draw(&state, 80, 24);
        assert!(
            !has_non_space(&rows),
            "a skipped splash should render fully dissolved"
        );
    }
}
