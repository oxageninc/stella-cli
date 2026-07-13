//! Reusable [`tachyonfx`] animation building blocks for the deck.
//!
//! A surface that wants motion reaches for a constructor here instead of
//! hand-rolling a `tachyonfx::fx::*` call inline, so the deck's motion language
//! (timing, curves, which colors carry brand meaning) stays consistent in one
//! place. Colors always come from [`crate::theme`].
//!
//! Effects are driven by *scrubbing*, never by persisting a live
//! `tachyonfx::Effect`: a fresh effect is built each frame and [`apply`]d once
//! with the elapsed time since the motion began (state is just that start
//! timestamp). [`crate::deck_render`] drives [`tab_switch`] this way off
//! `DeckUi::tab_switch_ms`, and [`crate::splash`] drives its own coalesce /
//! dissolve identically — see the splash module for why scrubbing beats a
//! persisted, `&mut`-threaded timer here.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tachyonfx::{fx, Duration as FxDuration, Effect, EffectTimer, Interpolation, Motion};

use crate::theme;

/// A brisk amber sweep for tab / view switches in the deck shell: the new
/// content sweeps in left-to-right out of the brand accent color and lands
/// on its real style over `ms`.
pub fn tab_switch(ms: u32) -> Effect {
    fx::sweep_in(
        Motion::LeftToRight,
        10,
        3,
        theme::AMBER_DEEP,
        EffectTimer::from_ms(ms, Interpolation::CircOut),
    )
}

/// Advances `effect` by `dt` and renders the result into `buf` within
/// `area`. Thin wrapper over [`tachyonfx::Effect::process`] so call sites
/// don't need to import `tachyonfx` (or convert its `Duration` type) just to
/// drive one effect forward a frame.
pub fn apply(effect: &mut Effect, dt: std::time::Duration, area: Rect, buf: &mut Buffer) {
    effect.process(FxDuration::from(dt), buf, area);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::style::{Color, Style};
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget};

    use super::*;

    fn painted_buffer(area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        Paragraph::new(Line::from("STELLA COMMAND DECK").style(Style::default().fg(Color::White)))
            .render(area, &mut buf);
        buf
    }

    #[test]
    fn tab_switch_runs_to_completion_and_does_not_panic() {
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = painted_buffer(area);
        let mut effect = tab_switch(150);

        assert!(!effect.done(), "a fresh 150ms effect has not finished");
        for _ in 0..5 {
            apply(&mut effect, Duration::from_millis(40), area, &mut buf);
        }
        // 5 × 40 ms = 200 ms overshoots the 150 ms sweep, so it has settled.
        assert!(effect.done(), "overshooting the duration should finish the effect");
    }

    #[test]
    fn apply_is_a_no_op_on_a_zero_area() {
        let area = Rect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(area);
        let mut effect = tab_switch(100);
        apply(&mut effect, Duration::from_millis(10), area, &mut buf);
    }
}
