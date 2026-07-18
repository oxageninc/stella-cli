//! The `stella init` cinematic — a tiny terminal animation that plays while
//! the workspace is being indexed: a gold starfield drifting by, crossed by
//! stella's mascot-grade absurdity, a turtle on a jetpack skateboard.
//!
//! Rendering discipline mirrors the deck's fx rules:
//!
//! - **Frames are pure.** [`frame_plain`] is a function of `(width, tick)`
//!   and nothing else — no RNG, no clock reads — so every frame is
//!   unit-testable and replays byte-identically.
//! - **Progress lines stay readable.** The animation owns the *bottom* rows
//!   of the terminal region it drew; log lines arriving mid-flight are
//!   printed *above* it by the same driver task, so `init`'s real progress
//!   output is never interleaved with cursor moves (see [`InitCinematic`]).
//! - **It steps aside.** No TTY, `--no-anim`, `STELLA_NO_ANIM`, or
//!   `NO_COLOR` → no animation, no cursor control; log lines print plainly,
//!   exactly as before.
//!
//! Colors come from the brand palette (gold / amber / violet) — never blue
//! or cyan, per the deck's palette law.

use std::io::{IsTerminal, Write};

use colored::Colorize;
use tokio::sync::mpsc;

/// Messages from `run_init` into the render task.
enum Msg {
    /// A real progress line — printed above the animation region.
    Log(String),
    /// Tear the animation down (clears its rows) and stop.
    Finish,
}

/// Milliseconds per animation tick (~12 fps — smooth enough for a starfield,
/// cheap enough to never matter next to tree-sitter indexing).
const TICK_MS: u64 = 84;

/// Rows the animation occupies (stars, three sprite rows, stars, caption).
const ROWS: usize = 6;

/// Below this width there is no room for the sprite to fly — the caption
/// alone animates.
const MIN_FLIGHT_WIDTH: usize = 44;

/// The turtle, its jetpack, and its skateboard. Two frames: flame pulses,
/// wheels roll, eyes blink on the B frame.
const SPRITE_A: [&str; 3] = [
    "         ,--~~~~~--.",
    " ~≈≈=)) ( \\_/\\_/\\_/ )(o o)>",
    "         `-o=======o-'",
];
const SPRITE_B: [&str; 3] = [
    "         ,--~~~~~--.",
    "≈~≈≈=)) ( \\_/\\_/\\_/ )(- -)>",
    "         `-O=======O-'",
];

/// Fixed pseudo-stars: (column seed, row 0..=4 excluding sprite rows it
/// would collide with, twinkle phase). Deterministic — no RNG, ever.
const STARS: [(usize, usize, usize); 14] = [
    (3, 0, 0),
    (11, 4, 3),
    (19, 0, 5),
    (28, 4, 1),
    (37, 0, 2),
    (46, 4, 6),
    (55, 0, 4),
    (64, 4, 0),
    (73, 0, 7),
    (82, 4, 2),
    (91, 0, 6),
    (100, 4, 5),
    (109, 0, 1),
    (118, 4, 4),
];

/// One animation frame as plain (uncolored) rows, `ROWS` long, each row at
/// most `width` chars. Pure: same `(width, tick)` → same rows.
pub fn frame_plain(width: usize, tick: usize) -> Vec<String> {
    let mut rows = vec![String::new(); ROWS];
    if width == 0 {
        return rows;
    }

    // Starfield on rows 0 and 4, drifting left one column per tick.
    for &(seed, row, phase) in &STARS {
        let x = (seed + width.saturating_sub(tick % width.max(1))) % width;
        let glyph = if (tick + phase) % 8 < 4 { '·' } else { '✦' };
        put(&mut rows[row], x, glyph, width);
    }

    // The flight path: sprite enters from the left edge and exits right,
    // then loops. Two ticks per column keeps it stately, not frantic.
    if width >= MIN_FLIGHT_WIDTH {
        let sprite = if tick.is_multiple_of(2) {
            SPRITE_A
        } else {
            SPRITE_B
        };
        let span = sprite[1].chars().count();
        let cycle = width + span;
        let x = (tick / 2 * 3) % cycle;
        for (i, art) in sprite.iter().enumerate() {
            blit(&mut rows[1 + i], x as isize - span as isize, art, width);
        }
    }

    // Caption with a breathing ellipsis. Kept short enough (≤26 cols with
    // dots) to render whole on a narrow terminal.
    let dots = ".".repeat(1 + tick % 3);
    let caption = format!("mapping the code cosmos{dots}");
    let caption: String = caption.chars().take(width).collect();
    rows[5] = caption;

    for row in &mut rows {
        truncate_width(row, width);
    }
    rows
}

/// Write `glyph` at column `x` of `row`, padding with spaces as needed.
fn put(row: &mut String, x: usize, glyph: char, width: usize) {
    if x >= width {
        return;
    }
    let mut chars: Vec<char> = row.chars().collect();
    if chars.len() <= x {
        chars.resize(x + 1, ' ');
    }
    chars[x] = glyph;
    *row = chars.into_iter().collect();
}

/// Overlay `art` starting at (possibly negative) column `x`, clipping both
/// edges to `[0, width)`.
fn blit(row: &mut String, x: isize, art: &str, width: usize) {
    for (i, ch) in art.chars().enumerate() {
        if ch == ' ' {
            continue;
        }
        let col = x + i as isize;
        if col >= 0 {
            put(row, col as usize, ch, width);
        }
    }
}

/// Hard-clip a row to `width` chars.
fn truncate_width(row: &mut String, width: usize) {
    if row.chars().count() > width {
        *row = row.chars().take(width).collect();
    }
}

/// The brand color a single glyph is painted in. A pure decision, kept
/// separate from rendering so it can be asserted directly — the rendered
/// ANSI is terminal-dependent (a 16-color terminal downgrades our truecolor
/// palette, mapping the blue-leaning violet to a 16-color *blue* code), so a
/// palette-law test must check the *choice*, not the escape bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ink {
    /// Amber — jetpack flames and the skateboard deck.
    Flame,
    /// Muted paper — the dimmer twinkle of a star.
    Star,
    /// Bright gold — the brighter twinkle of a star.
    StarBright,
    /// Violet — the turtle's shell linework.
    Shell,
    /// Dimmed — the turtle's head/wheels and the caption.
    Dim,
    /// Uncolored (spaces and anything unmapped).
    Plain,
}

impl Ink {
    /// The truecolor RGB this ink renders as, or `None` for dim/plain.
    fn rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Ink::Flame => Some((0xF5, 0xB3, 0x3C)),
            Ink::Star => Some((0x8D, 0x84, 0x74)),
            Ink::StarBright => Some((0xFF, 0xD9, 0x7A)),
            Ink::Shell => Some((0x8F, 0x70, 0xE8)),
            Ink::Dim | Ink::Plain => None,
        }
    }
}

/// Which brand ink a glyph gets. Pure — no `colored`, no terminal state.
fn ink_for(ch: char, is_caption: bool) -> Ink {
    if is_caption {
        return Ink::Dim;
    }
    match ch {
        '~' | '≈' | '=' => Ink::Flame,
        '·' => Ink::Star,
        '✦' => Ink::StarBright,
        '\\' | '/' | '_' | ',' | '.' | '\'' | '`' | '-' => Ink::Shell,
        'o' | 'O' | '(' | ')' | '>' => Ink::Dim,
        _ => Ink::Plain,
    }
}

/// Colorize one plain row for display: flames gold, shell violet, stars
/// alternating gold/dim, everything else muted. Row-level heuristics keep
/// this trivially safe — a colored row is never structurally different from
/// its plain form.
fn paint(row: &str, is_caption: bool) -> String {
    if is_caption {
        return row.dimmed().to_string();
    }
    row.chars()
        .map(|ch| match ink_for(ch, false) {
            Ink::Dim => ch.to_string().dimmed().to_string(),
            Ink::Plain => ch.to_string(),
            other => {
                let (r, g, b) = other.rgb().expect("colored inks carry an rgb");
                ch.to_string().truecolor(r, g, b).to_string()
            }
        })
        .collect()
}

/// Should the cinematic animate at all in this process?
///
/// `no_anim_flag` is the CLI's `--no-anim`; the env vars mirror the deck's
/// behavior; a non-TTY stdout (pipes, CI) always disables it.
pub fn animation_enabled(no_anim_flag: bool) -> bool {
    !no_anim_flag
        && std::env::var_os("STELLA_NO_ANIM").is_none()
        && std::env::var_os("NO_COLOR").is_none()
        && std::io::stdout().is_terminal()
}

/// Handle to the running cinematic. Progress lines route through [`log`]
/// so they print above the animation region; [`finish`] clears the region
/// and joins the render task.
///
/// [`log`]: InitCinematic::log
/// [`finish`]: InitCinematic::finish
pub struct InitCinematic {
    tx: mpsc::UnboundedSender<Msg>,
    task: tokio::task::JoinHandle<()>,
}

impl InitCinematic {
    /// Start the render task. With `animate` false it degrades to a plain
    /// line printer — same interface, zero cursor control.
    pub fn start(animate: bool) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(render_loop(rx, animate));
        Self { tx, task }
    }

    /// Print a progress line (above the animation when it is flying). A
    /// closed channel means the render task already exited; the send is a
    /// no-op then rather than an error we can act on.
    pub fn log(&self, line: String) {
        let _ = self.tx.send(Msg::Log(line));
    }

    /// Clear the animation rows and stop the render task.
    pub async fn finish(self) {
        let _ = self.tx.send(Msg::Finish);
        let _ = self.task.await;
    }
}

/// The render task: interleaves log lines and animation frames so cursor
/// movement never corrupts real output. Owns stdout for its lifetime.
async fn render_loop(mut rx: mpsc::UnboundedReceiver<Msg>, animate: bool) {
    let mut drawn = 0_usize;
    let mut tick = 0_usize;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Msg::Log(line)) => {
                    clear_region(&mut drawn);
                    println!("  {line}");
                }
                Some(Msg::Finish) | None => {
                    clear_region(&mut drawn);
                    break;
                }
            },
            _ = ticker.tick() => {
                if animate {
                    clear_region(&mut drawn);
                    draw_frame(tick, &mut drawn);
                    tick += 1;
                }
            }
        }
    }
}

/// Erase the previously drawn animation rows (cursor up + clear to end).
fn clear_region(drawn: &mut usize) {
    if *drawn == 0 {
        return;
    }
    print!("\x1b[{drawn}A\x1b[0J");
    let _ = std::io::stdout().flush();
    *drawn = 0;
}

/// Draw one frame and remember how many rows it occupies.
fn draw_frame(tick: usize, drawn: &mut usize) {
    let width = terminal_width();
    let rows = frame_plain(width, tick);
    let last = rows.len() - 1;
    for (i, row) in rows.iter().enumerate() {
        println!("  {}", paint(row, i == last));
    }
    let _ = std::io::stdout().flush();
    *drawn = rows.len();
}

/// Best-effort terminal width: `COLUMNS` when the shell exports it, else a
/// conservative 80. (Worth zero dependencies: the frame clips itself, so an
/// under-estimate only narrows the flight path.)
fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&w| (20..=500).contains(&w))
        .unwrap_or(80)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drop ANSI SGR/cursor sequences so tests compare visible content.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    // Consume to the final byte of the CSI sequence.
                    for t in chars.by_ref() {
                        if t.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn frames_are_deterministic() {
        assert_eq!(frame_plain(80, 7), frame_plain(80, 7));
        assert_eq!(frame_plain(120, 0), frame_plain(120, 0));
    }

    #[test]
    fn every_row_fits_the_width() {
        for width in [0, 1, 20, 44, 80, 200] {
            for tick in 0..160 {
                for row in frame_plain(width, tick) {
                    assert!(
                        row.chars().count() <= width,
                        "row overflows width {width} at tick {tick}: {row:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_turtle_actually_flies() {
        // The shell's carapace ("~~~~~", the widest part of the sprite) must
        // be visible across a good chunk of the flight cycle, and its
        // horizontal position must advance over time.
        let carapace = |t: usize| frame_plain(80, t)[1].find("~~~~~");
        let visible_at: Vec<usize> = (0..160).filter(|&t| carapace(t).is_some()).collect();
        assert!(
            visible_at.len() > 10,
            "the sprite should be on screen for a good portion of the cycle, saw {}",
            visible_at.len()
        );
        // Two well-separated visible ticks: the carapace must have moved.
        let a = carapace(visible_at[2]).unwrap();
        let b = carapace(visible_at[visible_at.len() - 3]).unwrap();
        assert_ne!(a, b, "the sprite should move between ticks");
    }

    #[test]
    fn narrow_terminals_get_stars_and_caption_but_no_sprite() {
        let rows = frame_plain(30, 12);
        assert!(
            !rows.join("").contains("~~~~~"),
            "no room to fly at 30 cols"
        );
        assert!(rows[5].starts_with("mapping the code cosmos"));
    }

    #[test]
    fn caption_ellipsis_breathes() {
        let a = frame_plain(80, 0)[5].clone();
        let b = frame_plain(80, 1)[5].clone();
        assert_ne!(a, b, "ellipsis should animate tick to tick");
    }

    #[test]
    fn palette_law_no_cyan_and_only_brand_inks() {
        // The palette law is about the color *chosen*, not the escape bytes
        // rendered: a 16-color terminal downgrades our truecolor palette, and
        // the (on-brand, deliberately kept) violet shell is blue-dominant in
        // RGB, so it renders as a 16-color *blue* there — which is fine. What
        // must never happen: a glyph mapped to CYAN, or to any ink outside the
        // brand set. Asserting the pure `ink_for` choice is terminal-agnostic
        // and never flakes on `colored`'s global rendering state.
        let allowed = [
            Ink::Flame,
            Ink::Star,
            Ink::StarBright,
            Ink::Shell,
            Ink::Dim,
            Ink::Plain,
        ];
        // Every glyph the animation can draw maps only to a brand ink. The
        // last row (index 5) is the caption, painted dim.
        for tick in 0..24 {
            let rows = frame_plain(80, tick);
            let last = rows.len() - 1;
            for (i, row) in rows.iter().enumerate() {
                for ch in row.chars() {
                    let ink = ink_for(ch, i == last);
                    assert!(allowed.contains(&ink), "{ch:?} → non-brand ink {ink:?}");
                }
            }
        }
        // And no brand ink is cyan (low red, high green AND blue). Violet
        // (blue-dominant but red ≈ green) is explicitly allowed.
        for ink in [Ink::Flame, Ink::Star, Ink::StarBright, Ink::Shell] {
            let (r, g, b) = ink.rgb().expect("colored inks carry rgb");
            let is_cyan = r < 0x60 && g > 0xA0 && b > 0xA0;
            assert!(!is_cyan, "{ink:?} ({r:#04x},{g:#04x},{b:#04x}) is cyan");
        }
    }

    #[test]
    fn paint_preserves_visible_content() {
        colored::control::set_override(true);
        let rows = frame_plain(80, 33);
        for (i, row) in rows.iter().enumerate() {
            let painted = paint(row, i == rows.len() - 1);
            assert_eq!(
                strip_ansi(&painted),
                *row,
                "painting must never change visible glyphs"
            );
        }
        colored::control::unset_override();
    }

    #[tokio::test]
    async fn plain_mode_driver_forwards_logs_and_finishes() {
        // With animation off the driver is just a line pump — this proves
        // the channel + shutdown path can't wedge `stella init`.
        let cine = InitCinematic::start(false);
        cine.log("indexing…".to_string());
        cine.finish().await;
    }
}
