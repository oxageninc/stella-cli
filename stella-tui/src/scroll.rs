//! Line-exact viewport math (`09-lessons-learned.md` L-T4).
//!
//! In the TS era Ink 7 clipped **both** box edges during scroll; the fix was
//! explicit `clipTop`-style math property-tested at every boundary offset.
//! This module is the pure equivalent: given a total line count, a viewport
//! height, and a scroll anchor, [`ScrollState::window`] returns the exact
//! `[start, end)` line range to display, with follow-mode (auto-stick to the
//! tail) as the default. It is deliberately dependency-free and holds for
//! every degenerate case — empty log, viewport taller than content,
//! exact-fit, over-fill — which the proptest at the bottom pins down.

use std::ops::Range;

/// Where the transcript viewport is anchored.
///
/// `follow` is the resting state: the window stays glued to the tail as new
/// lines arrive. The moment the user scrolls up, `follow` drops and `top`
/// pins the first visible line; scrolling back to the bottom re-arms follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollState {
    /// Index of the first visible line when not following. Ignored while
    /// `follow` is set (the window is computed from the tail instead).
    pub top: usize,
    /// Auto-stick to the tail. Default; any upward scroll clears it.
    pub follow: bool,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            top: 0,
            follow: true,
        }
    }
}

impl ScrollState {
    /// The largest valid `top` for a given content/viewport size: the offset
    /// that shows the last `height` lines and no blank space past the end.
    fn max_top(total: usize, height: usize) -> usize {
        total.saturating_sub(height)
    }

    /// The `[start, end)` line range to render, clamped so it is always a
    /// valid slice of `0..total` no taller than `height`.
    ///
    /// Invariants (all proven by [`window_is_always_a_valid_bounded_slice`]):
    /// `start <= end <= total`, `end - start <= height`, and when
    /// `total <= height` the whole log is shown (`0..total`). While
    /// following, `end == total` whenever `height > 0`.
    pub fn window(&self, total: usize, height: usize) -> Range<usize> {
        if height == 0 || total == 0 {
            return 0..0;
        }
        let start = if self.follow {
            Self::max_top(total, height)
        } else {
            self.top.min(Self::max_top(total, height))
        };
        let end = (start + height).min(total);
        start..end
    }

    /// Scroll up by `n` lines, leaving follow-mode. Clamps at the top.
    pub fn scroll_up(&mut self, n: usize, total: usize, height: usize) {
        // Resolve the current effective top *before* leaving follow so an
        // upward nudge from the tail lands where the user expects.
        let current = self.window(total, height).start;
        self.follow = false;
        self.top = current.saturating_sub(n);
    }

    /// Scroll down by `n` lines. Re-arms follow-mode once it reaches the tail.
    pub fn scroll_down(&mut self, n: usize, total: usize, height: usize) {
        let max_top = Self::max_top(total, height);
        let current = self.window(total, height).start;
        let next = (current + n).min(max_top);
        self.top = next;
        // Reaching the bottom re-sticks to the tail so new lines keep flowing.
        self.follow = next >= max_top;
    }

    /// Page up by a viewport height, leaving follow-mode.
    pub fn page_up(&mut self, total: usize, height: usize) {
        self.scroll_up(height.max(1), total, height);
    }

    /// Page down by a viewport height, re-arming follow at the tail.
    pub fn page_down(&mut self, total: usize, height: usize) {
        self.scroll_down(height.max(1), total, height);
    }

    /// Jump to the very top, leaving follow-mode.
    pub fn to_top(&mut self) {
        self.follow = false;
        self.top = 0;
    }

    /// Jump to the tail and re-arm follow-mode.
    pub fn to_bottom(&mut self) {
        self.follow = true;
        self.top = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn empty_log_yields_empty_window() {
        let s = ScrollState::default();
        assert_eq!(s.window(0, 10), 0..0);
    }

    #[test]
    fn viewport_taller_than_content_shows_everything_from_the_top() {
        let s = ScrollState::default();
        // 3 lines, 10 rows of space → show 0..3, anchored at the top (no
        // blank scroll-off).
        assert_eq!(s.window(3, 10), 0..3);
    }

    #[test]
    fn exact_fit_shows_the_whole_log() {
        let s = ScrollState::default();
        assert_eq!(s.window(10, 10), 0..10);
    }

    #[test]
    fn following_sticks_to_the_tail_on_overfill() {
        let s = ScrollState::default();
        // 100 lines, 10 rows → last 10 lines.
        assert_eq!(s.window(100, 10), 90..100);
    }

    #[test]
    fn scrolling_up_one_from_the_tail_reveals_the_prior_line() {
        let mut s = ScrollState::default();
        s.scroll_up(1, 100, 10);
        assert!(!s.follow);
        assert_eq!(s.window(100, 10), 89..99);
    }

    #[test]
    fn scrolling_down_to_the_bottom_rearms_follow() {
        let mut s = ScrollState::default();
        s.scroll_up(5, 100, 10); // now at 85..95, not following
        assert!(!s.follow);
        s.scroll_down(50, 100, 10); // overshoot the tail
        assert!(s.follow);
        assert_eq!(s.window(100, 10), 90..100);
    }

    #[test]
    fn to_top_pins_the_first_line() {
        let mut s = ScrollState::default();
        s.to_top();
        assert_eq!(s.window(100, 10), 0..10);
    }

    #[test]
    fn top_is_clamped_when_content_shrinks_below_the_saved_offset() {
        let mut s = ScrollState {
            top: 500,
            follow: false,
        };
        // Content is now only 20 lines; a stale top of 500 must clamp to the
        // last full page (10..20), never index past the end.
        assert_eq!(s.window(20, 10), 10..20);
        // The state itself is untouched; only the resolved window clamps.
        s.scroll_down(0, 20, 10);
        assert!(s.follow, "a no-op scroll at a clamped tail re-arms follow");
    }

    proptest! {
        /// The core L-T4 guarantee: for *any* content size, viewport height,
        /// scroll anchor, and follow flag, the rendered window is a valid,
        /// bounded slice — it never runs off either edge (the both-edges
        /// clip bug this whole module exists to prevent).
        #[test]
        fn window_is_always_a_valid_bounded_slice(
            total in 0usize..2000,
            height in 0usize..200,
            top in 0usize..3000,
            follow in any::<bool>(),
        ) {
            let s = ScrollState { top, follow };
            let w = s.window(total, height);
            prop_assert!(w.start <= w.end, "start past end: {w:?}");
            prop_assert!(w.end <= total, "end past content: {w:?} total={total}");
            prop_assert!(w.end - w.start <= height, "window taller than viewport: {w:?} h={height}");
            if total <= height && height > 0 {
                prop_assert_eq!(w.clone(), 0..total, "small content must show in full");
            }
            if follow && height > 0 && total > 0 {
                prop_assert_eq!(w.end, total, "following must end at the tail");
            }
        }
    }
}
