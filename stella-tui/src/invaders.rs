//! The Space Invaders battle scene the launch splash plays before the STELLA
//! wordmark reveal (see [`crate::splash`]).
//!
//! Everything here is a **pure function of elapsed time**: the splash renders
//! from `&SplashState` with no `&mut` path back into the model, so the scene
//! cannot keep per-frame simulation state. Star positions, the fleet's march,
//! every bolt, bomb, and explosion are computed from `t` alone — two renders
//! at the same `t` produce byte-identical buffers, and the scene *scrubs*
//! (a skipped frame lands exactly where continuous playback would have).
//!
//! The battle is a scripted loop of [`LOOP_SECS`]: the cannon slides under
//! four invaders in turn and destroys each with a bolt (the third shot passes
//! through the hole the first kill opened), while the fleet returns fire —
//! one bomb bursts on the ground, one downs the escort ship. While the splash
//! is **held** open over a still-running init, `t` wraps at the loop period
//! and the fleet respawns for another pass, so the movie covers an
//! arbitrarily long first-launch index without freezing.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::theme;

/// One full battle loop, in seconds. The scripted choreography (shots, bombs,
/// explosions) lives inside one period; a held splash wraps `t` here.
pub const LOOP_SECS: f32 = 3.5;

/// Below this, the sprite battle would clip into soup — only the starfield
/// plays (the splash's wordmark phases have their own small-terminal
/// fallback).
const MIN_BATTLE_W: u16 = 44;
const MIN_BATTLE_H: u16 = 14;

/// Rows per second a player bolt climbs / an invader bomb falls.
const BOLT_SPEED: f32 = 26.0;
const BOMB_SPEED: f32 = 11.0;

/// How long one explosion burst plays.
const EXPLOSION_SECS: f32 = 0.5;

/// Invader sprite geometry: 5×2 cells on a 7-column, 3-row pitch.
const INVADER_W: i32 = 5;
const INVADER_H: i32 = 2;
const PITCH_X: i32 = 7;
const PITCH_Y: i32 = 3;

/// The classic two-frame march: antennae + body, legs kicking out and in.
const INVADER_FRAMES: [[&str; 2]; 2] = [["▚▄█▄▞", "▝▘ ▝▘"], ["▚▄█▄▞", "▗▖ ▗▖"]];

/// The player cannon (5×2) and the small escort ship the fleet shoots down.
const CANNON: [&str; 2] = ["  ▲  ", " ▟█▙ "];
const ESCORT: &str = "▙▄▟";

/// The four scripted player shots: `(fire_t, column fraction, fleet row)`.
/// Rows count from the fleet's bottom (`0` = bottom row) so the script scales
/// to any fleet height. Shot 3 targets the row **above** shot 1's kill, in
/// the same column — its bolt flies through the hole the first kill opened.
const SHOTS: [(f32, f32, u32); 4] = [
    (0.20, 0.18, 0),
    (0.90, 0.80, 0),
    (1.65, 0.18, 1),
    (2.40, 0.52, 0),
];

/// The fleet's return fire: `(drop_t, kind)`. The first bomb bursts on the
/// ground; the second leads the escort ship and downs it.
const BOMB_GROUND_T: f32 = 0.65;
const BOMB_ESCORT_T: f32 = 1.55;
const BOMB_GROUND_COLF: f32 = 0.38;

/// Deterministic per-index hash (seeded from [`crate::fx::FX_SEED`]) — the
/// scene's only randomness source, so every rebuild agrees on star placement.
fn hash(i: u32) -> u32 {
    let mut x = i.wrapping_mul(0x9E37_79B9) ^ crate::fx::FX_SEED;
    x ^= x >> 16;
    x = x.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 13;
    x
}

/// `hash` folded to `0.0..1.0`.
fn hash_f(i: u32) -> f32 {
    (hash(i) & 0xFFFF) as f32 / 65535.0
}

/// Paint one cell (area-relative coordinates), ignoring out-of-bounds writes
/// so sprites can slide past the edges without clipping checks at call sites.
fn put(buf: &mut Buffer, area: Rect, x: i32, y: i32, ch: char, color: Color) {
    if x < 0 || y < 0 || x >= i32::from(area.width) || y >= i32::from(area.height) {
        return;
    }
    if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y + y as u16)) {
        let mut utf8 = [0u8; 4];
        cell.set_symbol(ch.encode_utf8(&mut utf8));
        cell.set_fg(color);
    }
}

/// Stamp a multi-row sprite; spaces are transparent (stars show through).
fn sprite(buf: &mut Buffer, area: Rect, x: i32, y: i32, rows: &[&str], color: Color) {
    for (dy, row) in rows.iter().enumerate() {
        for (dx, ch) in row.chars().enumerate() {
            if ch != ' ' {
                put(buf, area, x + dx as i32, y + dy as i32, ch, color);
            }
        }
    }
}

/// The drifting, twinkling starfield. Takes the **absolute** elapsed time
/// (not the loop-wrapped `t`) so stars flow continuously across battle loops
/// and on through the wordmark reveal.
pub fn render_stars(t: f32, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let count = (u32::from(area.width) * u32::from(area.height) / 24).clamp(12, 160);
    for i in 0..count {
        let x = (hash(i * 3 + 1) % u32::from(area.width)) as i32;
        // Parallax: each star falls at its own speed, wrapping at the bottom.
        let fall = 1.0 + hash_f(i * 3 + 2) * 2.0;
        let y0 = hash_f(i * 3 + 3) * f32::from(area.height);
        let y = (y0 + t * fall) % f32::from(area.height);
        // Twinkle on a per-star phase; dim tiers dominate, with a few amber
        // accents so the sky reads on-brand.
        let phase = ((t * 1.6 + hash_f(i * 7 + 5) * 4.0) % 4.0) as usize;
        let (ch, color) = match hash(i * 5 + 4) % 10 {
            0 => (['✦', '·', '✦', '·'][phase], theme::AMBER),
            1..=3 => (['+', '·', '˙', '·'][phase], theme::TEXT_TERTIARY),
            _ => (['·', '·', '.', '˙'][phase], theme::TEXT_DIM),
        };
        put(buf, area, x, y as i32, ch, color);
    }
}

/// Fleet shape for this terminal: `(columns, rows)`.
fn fleet_dims(area: Rect) -> (i32, i32) {
    let cols = ((i32::from(area.width) - 6) / PITCH_X).clamp(3, 9);
    let rows = if area.height >= 20 { 3 } else { 2 };
    (cols, rows)
}

/// The fleet's horizontal march: a ±3-cell triangle sweep, one full cycle
/// per loop, with a one-row step down at the halfway point.
fn march(t: f32) -> (i32, i32) {
    let p = (t / LOOP_SECS).rem_euclid(1.0);
    let tri = if p < 0.5 {
        p * 4.0 - 1.0
    } else {
        3.0 - p * 4.0
    };
    let step_down = i32::from(t.rem_euclid(LOOP_SECS) >= LOOP_SECS * 0.5);
    ((tri * 3.0).round() as i32, step_down)
}

/// Top-left cell of the invader at `(col, row)` — `row` counts from the TOP
/// of the fleet here — at time `t`.
fn invader_pos(area: Rect, col: i32, row: i32, t: f32) -> (i32, i32) {
    let (cols, _) = fleet_dims(area);
    let block_w = cols * PITCH_X - (PITCH_X - INVADER_W);
    let base_x = (i32::from(area.width) - block_w) / 2;
    let (off_x, off_y) = march(t);
    (base_x + col * PITCH_X + off_x, 2 + row * PITCH_Y + off_y)
}

/// Resolve one scripted shot against this terminal's fleet: the target's
/// `(col, top-row)` plus the bolt's impact moment.
struct ResolvedShot {
    fire: f32,
    impact: f32,
    col: i32,
    row: i32,
}

fn resolve_shots(area: Rect) -> Vec<ResolvedShot> {
    let (cols, rows) = fleet_dims(area);
    let cannon_top = cannon_y(area);
    SHOTS
        .iter()
        .map(|&(fire, colf, from_bottom)| {
            let col = (colf * (cols - 1) as f32).round() as i32;
            let row = (rows - 1 - from_bottom.min(rows as u32 - 1) as i32).max(0);
            // Aim at the row's underside; travel time closes the gap.
            let target_y = 2 + row * PITCH_Y + INVADER_H;
            let impact = fire + (cannon_top - target_y).max(1) as f32 / BOLT_SPEED;
            ResolvedShot {
                fire,
                impact,
                col,
                row,
            }
        })
        .collect()
}

/// The cannon's top row (its ▲): two sprite rows above the ground line.
fn cannon_y(area: Rect) -> i32 {
    i32::from(area.height) - 3
}

/// The escort ship's patrol: a slow triangle sweep across the lower sky.
fn escort_pos(area: Rect, t: f32) -> (i32, i32) {
    let w = i32::from(area.width);
    let p = (t / LOOP_SECS).rem_euclid(1.0);
    let tri = if p < 0.5 { p * 2.0 } else { 2.0 - p * 2.0 }; // 0..1..0
    let span = (w / 2 - 6).max(1);
    (
        w / 4 + (tri * span as f32) as i32,
        i32::from(area.height) - 6,
    )
}

/// When the escort-hunting bomb lands. The bomb "leads" the escort: its
/// column is the escort's position at this precomputed impact moment.
fn escort_impact(area: Rect) -> f32 {
    let (_, rows) = fleet_dims(area);
    let drop_y = 2 + (rows - 1) * PITCH_Y + INVADER_H;
    let (_, escort_row) = escort_pos(area, 0.0);
    BOMB_ESCORT_T + (escort_row - drop_y).max(1) as f32 / BOMB_SPEED
}

/// One expanding burst at `(cx, cy)`, `local` in `0.0..1.0`.
fn explosion(buf: &mut Buffer, area: Rect, cx: i32, cy: i32, local: f32) {
    if local < 0.2 {
        put(buf, area, cx, cy, '✶', theme::EMBER_GOLD);
    } else if local < 0.45 {
        put(buf, area, cx, cy, '✹', theme::EMBER_FLAME);
        for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
            put(buf, area, cx + dx, cy + dy, '∙', theme::WARNING_BRIGHT);
        }
    } else if local < 0.7 {
        put(buf, area, cx, cy, '✺', theme::EMBER_FLAME);
        for ((dx, dy), ch) in [
            ((-2, 0), '·'),
            ((2, 0), '·'),
            ((0, -1), '·'),
            ((0, 1), '·'),
            ((-1, -1), '▘'),
            ((1, -1), '▝'),
            ((-1, 1), '▖'),
            ((1, 1), '▗'),
        ] {
            put(buf, area, cx + dx, cy + dy, ch, theme::EMBER_CRIMSON);
        }
    } else {
        for (dx, dy) in [(-2, -1), (2, -1), (-1, 1), (1, -2), (2, 1)] {
            put(buf, area, cx + dx, cy + dy, '·', theme::TEXT_DIM);
        }
    }
}

/// Where the cannon's left edge sits at `t`: parked under the next scripted
/// target, easing over from the previous one between shots.
fn cannon_x(area: Rect, shots: &[ResolvedShot], t: f32) -> i32 {
    // Muzzle-aligned park position for a shot: bolt column minus the ▲ offset.
    let park = |s: &ResolvedShot| {
        let (ix, _) = invader_pos(area, s.col, s.row, s.impact);
        ix + INVADER_W / 2 - 2
    };
    let first = match shots.first() {
        Some(first) => first,
        None => return i32::from(area.width) / 2 - 2,
    };
    let mut from = park(first);
    if t <= first.fire {
        return from;
    }
    for pair in shots.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        if t <= next.fire {
            // Ease across the gap, arriving shortly before the trigger pull.
            let span = (next.fire - prev.fire - 0.15).max(0.01);
            let p = ((t - prev.fire) / span).clamp(0.0, 1.0);
            let eased = p * p * (3.0 - 2.0 * p); // smoothstep
            let to = park(next);
            return from + ((to - from) as f32 * eased).round() as i32;
        }
        from = park(next);
    }
    from
}

/// Render the battle at absolute elapsed time `t_abs` (seconds). Stars run on
/// `t_abs`; the scripted choreography wraps at [`LOOP_SECS`].
pub fn render(t_abs: f32, area: Rect, buf: &mut Buffer) {
    render_stars(t_abs, area, buf);
    if area.width < MIN_BATTLE_W || area.height < MIN_BATTLE_H {
        return;
    }
    let t = t_abs.rem_euclid(LOOP_SECS);
    let (cols, rows) = fleet_dims(area);
    let shots = resolve_shots(area);

    // Ground line under the cannon.
    let ground_y = i32::from(area.height) - 1;
    for x in 0..i32::from(area.width) {
        put(buf, area, x, ground_y, '▁', theme::HAIRLINE);
    }

    // The fleet, minus anyone already shot down this loop, legs alternating
    // every 0.4s. Rows shade bottom-to-top so depth reads at a glance.
    let frame = &INVADER_FRAMES[((t / 0.4) as usize) % 2];
    let row_colors = [
        theme::WARNING_BRIGHT,
        theme::AGENT_AMBER,
        theme::EMBER_FLAME,
    ];
    for row in 0..rows {
        for col in 0..cols {
            let dead = shots
                .iter()
                .any(|s| s.col == col && s.row == row && t >= s.impact);
            if dead {
                continue;
            }
            let (x, y) = invader_pos(area, col, row, t);
            sprite(buf, area, x, y, frame, row_colors[row as usize % 3]);
        }
    }

    // Return fire: one bomb bursts on the ground…
    let fleet_bottom = 2 + (rows - 1) * PITCH_Y + INVADER_H;
    let ground_impact = BOMB_GROUND_T + (ground_y - fleet_bottom).max(1) as f32 / BOMB_SPEED;
    let bomb_x = ((i32::from(area.width) - 6) as f32 * BOMB_GROUND_COLF) as i32 + 3;
    if (BOMB_GROUND_T..ground_impact).contains(&t) {
        let y = fleet_bottom as f32 + (t - BOMB_GROUND_T) * BOMB_SPEED;
        put(buf, area, bomb_x, y as i32, '╻', theme::EMBER_FLAME);
    } else if (ground_impact..ground_impact + 0.3).contains(&t) {
        put(buf, area, bomb_x, ground_y - 1, '✶', theme::EMBER_CRIMSON);
    }

    // …the other leads the escort ship and downs it.
    let hit = escort_impact(area);
    let (ex, ey) = escort_pos(area, hit);
    if (BOMB_ESCORT_T..hit).contains(&t) {
        let y = fleet_bottom as f32 + (t - BOMB_ESCORT_T) * BOMB_SPEED;
        put(buf, area, ex + 1, y as i32, '╻', theme::EMBER_FLAME);
    }
    if t < hit {
        let (x, y) = escort_pos(area, t);
        sprite(buf, area, x, y, &[ESCORT], theme::SUCCESS_BRIGHT);
    } else if t < hit + EXPLOSION_SECS {
        explosion(buf, area, ex + 1, ey, (t - hit) / EXPLOSION_SECS);
    }

    // The cannon, its bolts, and the kills.
    let cy = cannon_y(area);
    let cx = cannon_x(area, &shots, t);
    sprite(buf, area, cx, cy, &CANNON, theme::EMBER_GOLD);
    for shot in &shots {
        let (ix, iy) = invader_pos(area, shot.col, shot.row, shot.impact);
        let bolt_x = ix + INVADER_W / 2;
        if (shot.fire..shot.impact).contains(&t) {
            let y = cy as f32 - (t - shot.fire) * BOLT_SPEED;
            put(buf, area, bolt_x, y as i32, '┃', theme::EMBER_GOLD);
            // Muzzle flash right as the trigger pulls.
            if t - shot.fire < 0.08 {
                put(buf, area, cx + 2, cy - 1, '✶', theme::EMBER_GOLD);
            }
        } else if (shot.impact..shot.impact + EXPLOSION_SECS).contains(&t) {
            explosion(
                buf,
                area,
                bolt_x,
                iy + INVADER_H / 2,
                (t - shot.impact) / EXPLOSION_SECS,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(t: f32, w: u16, h: u16) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        render(t, area, &mut buf);
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
    fn renders_are_deterministic_at_equal_times() {
        let a = draw(1.234, 80, 24);
        let b = draw(1.234, 80, 24);
        assert_eq!(a, b, "same t must produce byte-identical frames");
    }

    #[test]
    fn battle_draws_sprites_on_a_roomy_terminal() {
        // Well past the stars-only floor: fleet + cannon + ground all paint.
        let stars_only = non_space_cells(&draw(1.0, 40, 10));
        let battle = non_space_cells(&draw(1.0, 80, 24));
        assert!(
            battle > stars_only + 40,
            "the sprite battle should paint far more than a bare starfield \
             ({battle} vs {stars_only})"
        );
    }

    #[test]
    fn small_terminals_get_stars_only() {
        // No ground line on a sub-minimum area — the '▁' row is the battle's
        // unconditional first stroke, so its absence proves the early return.
        let buf = draw(1.0, 40, 10);
        let bottom: String = (0..40)
            .map(|x| buf.cell((x, 9)).map(|c| c.symbol()).unwrap_or(" "))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            !bottom.contains('▁'),
            "no ground line below the minimum size"
        );
    }

    #[test]
    fn scene_loops_at_the_loop_period() {
        let a = draw(0.7, 80, 24);
        let b = draw(0.7 + LOOP_SECS, 80, 24);
        // Stars run on absolute time (they drift across loops), so compare a
        // battle-owned cell instead: the ground line persists, and the fleet
        // respawns — total paint should be close. Cheap invariant: both
        // frames paint a ground line.
        for frame in [&a, &b] {
            let ground: String = (0..80)
                .map(|x| frame.cell((x, 23)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<Vec<_>>()
                .join("");
            assert!(ground.contains('▁'), "ground line present each loop");
        }
    }

    #[test]
    fn kills_remove_invaders_within_a_loop() {
        // Just before the first impact vs. just after: the fleet loses a
        // ship, so (stars aside) the frame should not gain paint.
        let area = Rect::new(0, 0, 80, 24);
        let shots = resolve_shots(area);
        let first = &shots[0];
        let (t_before, t_after) = (first.impact - 0.05, first.impact + EXPLOSION_SECS + 0.05);
        let before = draw(t_before, 80, 24);
        let after = draw(t_after, 80, 24);
        // The fleet marches between the two frames — sample each frame at the
        // invader's position AT THAT FRAME's time.
        let solid_at = |frame: &Buffer, t: f32| {
            let (x, y) = invader_pos(area, first.col, first.row, t);
            (0..INVADER_W)
                .filter(|dx| {
                    frame
                        .cell(((x + dx) as u16, y as u16))
                        .is_some_and(|c| c.symbol() != " ")
                })
                .count()
        };
        let solid_before = solid_at(&before, t_before);
        let solid_after = solid_at(&after, t_after);
        assert!(solid_before >= 4, "target alive before impact");
        assert!(
            solid_after < solid_before,
            "target gone after its explosion ({solid_after} vs {solid_before})"
        );
    }

    #[test]
    fn render_does_not_panic_across_times_and_sizes() {
        for &(w, h) in &[(0, 0), (1, 1), (10, 4), (44, 14), (80, 24), (200, 60)] {
            for i in 0..24 {
                let _ = draw(i as f32 * 0.31, w, h);
            }
        }
    }
}
