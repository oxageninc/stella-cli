//! Reusable [`tachyonfx`] animation building blocks for the deck.
//!
//! Every surface that wants motion — a panel's content settling into place,
//! a tab switch, a view being torn down — should reach for a constructor
//! here instead of hand-rolling a `tachyonfx::fx::*` call inline, so the
//! deck's motion language (timing, curves, which colors carry brand meaning)
//! stays consistent in one place. Colors always come from [`crate::theme`].
//!
//! Production consumers today: [`crate::deck_render`] drives [`fade_in`]
//! (the deck revealing after the splash) and [`tab_switch`] (the sweep on a
//! tab change); [`crate::splash`] drives [`dissolve_out`] for its dissolve
//! phase. The splash's coalesce-in stays a hand-built effect there — it is
//! splash-specific, not part of the deck's shared motion language — but it
//! shares [`FX_SEED`] and the [`apply`] plumbing.
//!
//! Effects here may be rebuilt fresh every frame and *scrubbed* to a point
//! on an external timeline (see `deck_render`/`splash`): a
//! `tachyonfx::EffectTimer` only cares about total elapsed time, so one
//! `process(elapsed)` call on a fresh effect lands exactly where continuous
//! playback would have. That only holds if randomized effects pick the same
//! cells every rebuild, which is why the randomized constructors pin their
//! RNG to [`FX_SEED`].

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tachyonfx::{
    Duration as FxDuration, Effect, EffectTimer, Interpolation, Motion, SimpleRng, fx,
};

use crate::theme;

/// Fixed RNG seed for every randomized deck effect. tachyonfx's default RNG
/// seeds from the wall clock, so a fresh effect built each frame would pick a
/// different cell pattern every frame — scrubbed playback would read as
/// flicker, not motion. One shared seed makes every rebuild agree.
pub(crate) const FX_SEED: u32 = 0x57E11A;

/// A foreground fade-in from muted to each cell's real color, over `ms`.
///
/// Use when a panel's content just became available and should ease into
/// view rather than pop in — e.g. a view's first paint after a tab switch,
/// or a card resolving once its data arrives.
pub fn fade_in(ms: u32) -> Effect {
    fx::fade_from_fg(
        theme::MUTED,
        EffectTimer::from_ms(ms, Interpolation::QuadOut),
    )
}

/// Scatters cells to blank over `ms`, accelerating toward empty.
///
/// Use when a panel is being replaced or torn down — a dissolve reads as
/// "this is going away," distinct from a fade which reads as "this is
/// settling in."
pub fn dissolve_out(ms: u32) -> Effect {
    fx::dissolve(EffectTimer::from_ms(ms, Interpolation::QuadIn)).with_rng(SimpleRng::new(FX_SEED))
}

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
    .with_rng(SimpleRng::new(FX_SEED))
}

/// Advances `effect` by `dt` and renders the result into `buf` within
/// `area`. Thin wrapper over [`tachyonfx::Effect::process`] so call sites
/// don't need to import `tachyonfx` (or convert its `Duration` type) just to
/// drive one effect forward a frame.
pub fn apply(effect: &mut Effect, dt: std::time::Duration, area: Rect, buf: &mut Buffer) {
    effect.process(FxDuration::from(dt), buf, area);
}

// ── The working spinner: fast garbled text in the ember gradient ────────────

/// The glyph pool the garble spinner draws from — dense, boxy, unreadable on
/// purpose (reading as raw computation, not words).
const GARBLE_CHARS: &[char] = &[
    '░', '▒', '▓', '▖', '▘', '▝', '▗', '◆', '◇', '#', '%', '&', '$', '@', '?', '!', '*', '+', '=',
    '<', '>', '/', '\\', '|', '~', '^', ';', ':',
];

/// One frame of the working spinner: `width` cells of pseudo-random glyphs
/// colored across [`theme::EMBER_RAMP`]. Deterministic in `(phase, width)` —
/// the caller derives `phase` from the deck clock (`now_ms / tick`), so the
/// spinner churns every tick, renders identically on replay (L-T1), and
/// asserts cleanly in buffer tests. No wall-clock, no RNG state.
pub fn garble_line(phase: u64, width: usize) -> Line<'static> {
    let ramp = theme::EMBER_RAMP;
    let spans = (0..width)
        .map(|col| {
            let h = mix(phase, col as u64);
            let ch = GARBLE_CHARS[(h as usize) % GARBLE_CHARS.len()];
            // A gradient that ping-pongs dark→bright→dark across the width.
            let steps = ramp.len() * 2 - 2;
            let slot = (col * steps) / width.max(1);
            let idx = if slot < ramp.len() {
                slot
            } else {
                steps - slot
            };
            Span::styled(
                ch.to_string(),
                Style::default().fg(ramp[idx.min(ramp.len() - 1)]),
            )
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// splitmix64-style avalanche over `(phase, col)` — a tiny, allocation-free,
/// deterministic hash so every cell re-rolls every phase step.
fn mix(phase: u64, col: u64) -> u64 {
    let mut z = phase
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(col.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
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

    fn non_space_cells(buf: &Buffer) -> usize {
        let area = *buf.area();
        (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| buf.cell((x, y)).is_some_and(|c| c.symbol() != " "))
            .count()
    }

    #[test]
    fn fade_in_runs_to_completion_and_does_not_panic() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = painted_buffer(area);
        let mut effect = fade_in(100);

        assert!(!effect.done(), "a fresh 100ms effect has not finished");
        apply(&mut effect, Duration::from_millis(50), area, &mut buf);
        assert!(
            !effect.done(),
            "halfway through a 100ms fade should still be running"
        );
        apply(&mut effect, Duration::from_millis(200), area, &mut buf);
        assert!(
            effect.done(),
            "overshooting the duration should finish the effect"
        );
    }

    #[test]
    fn dissolve_out_clears_cells_toward_blank() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = painted_buffer(area);
        let before = non_space_cells(&buf);
        assert!(before > 0, "fixture text should paint some cells");

        let mut effect = dissolve_out(50);
        // Drive well past the effect's own duration so it settles fully
        // dissolved regardless of its internal random cell ordering.
        apply(&mut effect, Duration::from_millis(500), area, &mut buf);

        assert_eq!(
            non_space_cells(&buf),
            0,
            "a fully-run dissolve blanks every cell"
        );
    }

    #[test]
    fn tab_switch_processes_without_panicking_on_a_realistic_area() {
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = painted_buffer(area);
        let mut effect = tab_switch(150);

        for _ in 0..5 {
            apply(&mut effect, Duration::from_millis(40), area, &mut buf);
        }
        assert!(effect.done() || effect.running());
    }

    #[test]
    fn apply_is_a_no_op_on_a_zero_area() {
        let area = Rect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(area);
        let mut effect = fade_in(100);
        apply(&mut effect, Duration::from_millis(10), area, &mut buf);
    }

    fn garble_text(phase: u64, width: usize) -> String {
        garble_line(phase, width)
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect()
    }

    #[test]
    fn garble_is_deterministic_per_phase_and_churns_across_phases() {
        // Same phase → identical frame (replay-safe, testable)…
        assert_eq!(garble_text(7, 24), garble_text(7, 24));
        // …and consecutive phases visibly differ (the "fast movement" read).
        assert_ne!(garble_text(7, 24), garble_text(8, 24));
        assert_eq!(garble_text(7, 24).chars().count(), 24);
    }

    #[test]
    fn garble_handles_degenerate_widths() {
        assert_eq!(garble_text(1, 0), "");
        assert_eq!(garble_text(1, 1).chars().count(), 1);
    }
}
