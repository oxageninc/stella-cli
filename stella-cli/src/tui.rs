//! Terminal UI — streaming text, tool-call cards, cost tracking.
//!
//! Designed for speed and engagement: tool calls appear as cards with
//! status, and the model's response prints as soon as it's ready.
//!
//! `render_event` is the TUI's one entry point onto `stella_core::Engine`'s
//! event stream (`02-architecture.md` §4, L-T1: the TUI renders exclusively
//! from `AgentEvent`s — no panel owns state that isn't reconstructible by
//! replaying the event log). It's deliberately a thin dispatcher onto the
//! existing per-kind print helpers below, not a rewrite of them.
//!
//! There is deliberately no animated "thinking" spinner: the Phase 0/1
//! version had one, but it only ticked a fixed 3 frames *before* dispatching
//! the network call, then froze for the entire real wait — a decorative
//! pre-roll, not a live indicator. A correct live spinner would need its own
//! concurrent task racing the event-draining task below, both writing to the
//! terminal — real interleaving risk for a cosmetic win. `Stage::Execute`
//! below gives one clean, immediate "thinking" line instead. There is no
//! decorative "rocket"/spinner animation either — activity is reported by the
//! command deck's honest run progress bar (`stella_tui::progress`), never by a
//! cosmetic character-noise loop.

use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use colored::{Color, ColoredString, Colorize};
use stella_protocol::{AgentEvent, BudgetMode, StageKind};
use stella_tui::textline::{self, EventLine, Tone, fmt_cost};

/// Truncate `s` to at most `max` characters, appending `…` when it was
/// shortened. Char-boundary-safe: operates on `char`s, never byte indices,
/// so it can never panic on multi-byte UTF-8 the way a `&s[..57]` byte-slice
/// does (a slice at a non-char boundary is an immediate panic — e.g. any
/// tool input or output with an accented letter or emoji at the cut point).
fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    } else {
        s.to_string()
    }
}

/// Selectable accent palette — `/color` switches the session's accent so
/// multiple terminal windows running stella are visually distinct at a
/// glance (see [`set_accent`], and [`rename_tab`] for the `/rename` sibling).
const PALETTE: [(&str, Color); 6] = [
    ("magenta", Color::Magenta),
    ("cyan", Color::Cyan),
    ("green", Color::Green),
    ("yellow", Color::Yellow),
    ("blue", Color::Blue),
    ("red", Color::Red),
];

static ACCENT: AtomicUsize = AtomicUsize::new(0);

/// The session accent color (default magenta).
pub fn accent() -> Color {
    PALETTE[ACCENT.load(Ordering::Relaxed) % PALETTE.len()].1
}

/// Set the accent by name; returns false (and prints the options) on an
/// unknown name.
pub fn set_accent(name: &str) -> bool {
    if let Some(index) = PALETTE.iter().position(|(n, _)| *n == name.to_lowercase()) {
        ACCENT.store(index, Ordering::Relaxed);
        true
    } else {
        let names: Vec<&str> = PALETTE.iter().map(|(n, _)| *n).collect();
        println!(
            "  unknown color `{name}` — pick one of: {}",
            names.join(", ")
        );
        false
    }
}

/// Rename the terminal tab/window via the OSC 0 escape — running several
/// stella windows side by side, each can carry its own title.
pub fn rename_tab(title: &str) {
    print!("\x1b]0;{title}\x07");
    let _ = io::stdout().flush();
}

/// Render the Files Touched panel: one `[C|R|U|D]` badge per file, in
/// first-touch order, sourced from the registry's CRUD ledger.
pub fn files_touched_panel(entries: &[(String, String)]) {
    if entries.is_empty() {
        return;
    }
    println!(
        "\n  {} {}",
        "─".repeat(3).dimmed(),
        "Files Touched".color(accent()).bold()
    );
    let width = entries.iter().map(|(_, ops)| ops.len()).max().unwrap_or(1);
    for (path, ops) in entries {
        let colored_ops: String = ops
            .chars()
            .map(|op| match op {
                'C' => "C".green().to_string(),
                'R' => "R".blue().to_string(),
                'U' => "U".yellow().to_string(),
                'D' => "D".red().to_string(),
                other => other.to_string(),
            })
            .collect();
        println!(
            "  [{colored_ops}]{} {}",
            " ".repeat(width - ops.len()),
            path
        );
    }
}

/// Pretty per-verb rendering for the CRUD file tools; returns false (so the
/// caller falls back to the generic key=value card) for anything else.
fn file_tool_card(name: &str, input: &serde_json::Value) -> bool {
    let Some(path) = input.get("path").and_then(|v| v.as_str()) else {
        return false;
    };
    match name {
        "read_file" => {
            let range = match (
                input.get("offset").and_then(|v| v.as_u64()),
                input.get("limit").and_then(|v| v.as_u64()),
            ) {
                (Some(offset), Some(limit)) => format!(" [{offset}..+{limit}]"),
                (Some(offset), None) => format!(" [{offset}..]"),
                _ => String::new(),
            };
            println!(
                "  {} {} {}{}",
                "▷".blue(),
                "read".blue(),
                path,
                range.dimmed()
            );
        }
        "write_file" => {
            let lines = input
                .get("content")
                .and_then(|v| v.as_str())
                .map(|content| content.lines().count())
                .unwrap_or(0);
            println!(
                "  {} {} {} {}",
                "✚".green(),
                "write".green(),
                path,
                format!("({lines} lines)").dimmed()
            );
        }
        "edit_file" => {
            println!("  {} {} {}", "±".yellow(), "edit".yellow(), path);
            if let Some(old) = input.get("old_string").and_then(|v| v.as_str()) {
                let old_line = truncate_with_ellipsis(old.lines().next().unwrap_or(""), 70);
                println!("    {} {}", "-".red(), old_line.red().dimmed());
            }
            if let Some(new) = input.get("new_string").and_then(|v| v.as_str()) {
                let new_line = truncate_with_ellipsis(new.lines().next().unwrap_or(""), 70);
                println!("    {} {}", "+".green(), new_line.green());
            }
        }
        "delete_file" => {
            println!("  {} {} {}", "✖".red(), "delete".red(), path);
        }
        _ => return false,
    }
    true
}

/// Print a tool-call card: name, input summary, status. The CRUD file tools
/// get a dedicated per-verb card (`file_tool_card`) while they're running;
/// everything else gets the generic key=value summary below.
pub fn tool_call_card(name: &str, input: &serde_json::Value, status: &str) {
    if status == "running" && file_tool_card(name, input) {
        return;
    }
    let icon = match status {
        "running" => "▶".cyan(),
        "ok" => "✓".green(),
        "error" => "✗".red(),
        _ => "·".dimmed(),
    };
    let input_str = if input.is_object() {
        // Show key fields compactly
        if let Some(obj) = input.as_object() {
            let summary: Vec<String> = obj
                .iter()
                .take(3)
                .map(|(k, v)| {
                    let val_str = if let Some(s) = v.as_str() {
                        truncate_with_ellipsis(s, 57)
                    } else {
                        v.to_string()
                    };
                    format!("{}={}", k.bright_blue(), val_str)
                })
                .collect();
            summary.join(" ")
        } else {
            input.to_string()
        }
    } else {
        input.to_string()
    };

    println!(
        "  {} {}({})",
        icon,
        name.bright_yellow(),
        input_str.dimmed()
    );
}

/// Print a tool result summary.
pub fn tool_result_card(_name: &str, output: &str, is_error: bool, duration: Duration) {
    let icon = if is_error { "✗".red() } else { "✓".green() };
    let label = if is_error {
        "error".red()
    } else {
        "ok".green()
    };
    let preview = output.lines().next().unwrap_or("(empty)");
    let preview = truncate_with_ellipsis(preview, 77);
    println!(
        "    {} {} in {:.0}ms — {}",
        icon,
        label,
        duration.as_secs_f64() * 1000.0,
        preview.dimmed()
    );
}

/// Print a section header.
pub fn section_header(title: &str) {
    println!(
        "\n{} {}",
        "─".dimmed().repeat(3),
        title.color(accent()).bold()
    );
}

/// Print the assistant's complete response (after streaming).
pub fn assistant_response(text: &str) {
    if !text.is_empty() {
        println!("\n{}", text);
    }
}

/// Print a cost summary for a turn. `AgentEvent::Complete` (the source of
/// this data under `stella_core::Engine`) carries `model`/`cost_usd` only —
/// no per-turn token breakdown, unlike the old ad-hoc loop's manual
/// accumulation — so this is deliberately narrower than the Phase 0/1
/// version. Real per-role token/cost accounting lives in `BudgetTick`
/// (`render_event` below), which fires after every call, not just at
/// turn-end.
pub fn cost_summary(cost_usd: f64, model: &str, elapsed: Duration) {
    println!(
        "\n  {} {} · {} · {:.1}s",
        "◆".dimmed(),
        model.bright_blue(),
        fmt_cost(cost_usd),
        elapsed.as_secs_f64(),
    );
}

/// Print the welcome banner: the STELLA block-letter logomark swept with the
/// stellar gradient, then the session info line. (The previous banner was a
/// hand-pasted figlet that had drifted into garbage — it did not actually
/// spell "Stella" — which is exactly why the wordmark below is *composed*
/// from per-letter glyph data, the same defense the TS CLI's `banner.tsx`
/// uses: composition can't misspell.)
pub fn welcome_banner(provider: &str, model: &str, workspace: &str) {
    println!();
    for line in wordmark_lines() {
        println!("  {line}");
    }
    println!(
        "\n  {} {}",
        "✦".bright_magenta(),
        "a fast, BYOK, model-agnostic coding agent".dimmed()
    );
    println!(
        "  {} {} · {} · {}",
        "◆".cyan(),
        format!("{provider}/{model}").bright_blue(),
        workspace.dimmed(),
        "type your prompt, Ctrl+D to exit".dimmed(),
    );
    println!("  {}\n", "─".repeat(60).dimmed());
}

/// Render one `AgentEvent` from `stella_core::Engine::run_turn`'s stream.
/// `ToolStart`/`ToolResult` are intentionally a no-op here: `ToolResult`
/// doesn't carry the tool's name (only `call_id`), so the call site keeps a
/// small `call_id -> name` map and calls `tool_call_card`/`tool_result_card`
/// directly instead of routing those two through this function — see
/// `agent.rs`'s event-draining task. Every other event kind (including
/// `Text` — the engine emits one per step, not just at turn-end, since a
/// step with tool calls can still carry commentary text) is rendered here.
///
/// Wording comes from `stella_tui::textline` — the one event→text table
/// both this surface and the deck consume (issue #66); the arms below carry
/// only this surface's *policy* (what to suppress, what goes to stderr) and
/// styling. A new annotation variant needs a `textline` entry, nothing here.
pub fn render_event(event: &AgentEvent) {
    match event {
        AgentEvent::Stage {
            name: StageKind::Execute,
        } => {
            println!("  {}", "thinking…".dimmed());
        }
        AgentEvent::Stage { .. } | AgentEvent::ToolStart { .. } | AgentEvent::ToolResult { .. } => {
            // Complete-stage and ToolStart/ToolResult: handled inline at the
            // call site or by a more specific event (see the module doc).
        }
        AgentEvent::Text { delta } => assistant_response(delta),
        AgentEvent::Reasoning { .. } => {}
        AgentEvent::BudgetTick {
            mode: BudgetMode::Off,
            ..
        } => {
            // Unmetered sessions stay quiet about spend.
        }
        AgentEvent::Complete { .. } => {
            // The call site prints `cost_summary` from `TurnOutcome`
            // directly (it has the same model/cost_usd this event carries,
            // plus wall-clock elapsed time this event doesn't).
        }
        AgentEvent::AskUser {
            question, options, ..
        } => {
            // The interactive ask_user tool prints the numbered options and
            // collects the answer itself (it owns stdin while the turn is
            // in flight); this render arm exists for replay/stream-json
            // consumers of the event log. The binding free-text contract
            // (every question always offers a type-your-own option) is
            // enforced at the prompt site, not here.
            println!("  {}", styled_event_line(&textline::ask_user(question)));
            for (i, option) in options.iter().enumerate() {
                println!("    {} {}", format!("{})", i + 1).dimmed(), option);
            }
        }
        AgentEvent::Error { .. } => {
            // Errors belong on stderr — routing is policy, wording is shared.
            if let Some(line) = textline::event_line(event) {
                eprintln!("  {}", styled_event_line(&line));
            }
        }
        other => {
            if let Some(line) = textline::event_line(other) {
                println!("  {}", styled_event_line(&line));
            }
        }
    }
}

/// A shared [`EventLine`] in this surface's dress: the glyph carries the
/// tone via `colored` (bold when `strong`), the detail tail is dimmed.
fn styled_event_line(line: &EventLine) -> String {
    let glyph = match line.tone {
        Tone::Info => line.glyph.cyan(),
        Tone::Success => line.glyph.green(),
        Tone::Warn => line.glyph.yellow(),
        Tone::Error => line.glyph.red(),
        Tone::Muted => line.glyph.dimmed(),
    };
    let glyph: ColoredString = if line.strong { glyph.bold() } else { glyph };
    match &line.detail {
        Some(detail) => format!("{} {} {}", glyph, line.body, detail.dimmed()),
        None => format!("{} {}", glyph, line.body),
    }
}

// ── STELLA logomark ─────────────────────────────────────────────────────────
//
// 5-row block glyphs, kept as literals so the wordmark renders identically
// across terminals (no figlet dependency) — the same composition scheme as
// the TS CLI's `apps/cli/src/tui/banner.tsx`. Each glyph is a fixed-width
// column of 5 rows; the word is composed by joining glyph rows with one
// space so letters never touch.

const GLYPH_ROWS: usize = 5;

fn glyph(ch: char) -> Option<[&'static str; GLYPH_ROWS]> {
    match ch {
        'S' => Some(["███████", "██     ", "███████", "     ██", "███████"]),
        'T' => Some(["███████", "   ██  ", "   ██  ", "   ██  ", "   ██  "]),
        'E' => Some(["███████", "██     ", "█████  ", "██     ", "███████"]),
        'L' => Some(["██     ", "██     ", "██     ", "██     ", "███████"]),
        'A' => Some([" █████ ", "██   ██", "███████", "██   ██", "██   ██"]),
        _ => None,
    }
}

/// Compose a word into its 5-row block-letter form. Unknown chars are
/// skipped, so the output can never contain a garbled letter.
fn compose_wordmark(text: &str) -> Vec<String> {
    let letters: Vec<[&'static str; GLYPH_ROWS]> = text
        .chars()
        .flat_map(|c| glyph(c.to_ascii_uppercase()))
        .collect();
    (0..GLYPH_ROWS)
        .map(|row| letters.iter().map(|g| g[row]).collect::<Vec<_>>().join(" "))
        .collect()
}

// Stellar gradient — violet → magenta → pink → cyan, swept left-to-right
// across the wordmark (the night-sky counterpart of the TS banner's sunset).
const STELLAR_STOPS: [(u8, u8, u8); 4] = [
    (0x8B, 0x5C, 0xF6), // violet
    (0xD9, 0x46, 0xEF), // magenta
    (0xEC, 0x48, 0x99), // pink
    (0x22, 0xD3, 0xEE), // cyan
];

/// Color at horizontal position `t` ∈ [0,1] along the stellar gradient.
fn stellar_color_at(t: f64) -> (u8, u8, u8) {
    let clamped = t.clamp(0.0, 1.0);
    let segments = (STELLAR_STOPS.len() - 1) as f64;
    let scaled = clamped * segments;
    let idx = (scaled.floor() as usize).min(STELLAR_STOPS.len() - 2);
    let local = scaled - idx as f64;
    let from = STELLAR_STOPS[idx];
    let to = STELLAR_STOPS[idx + 1];
    let mix = |a: u8, b: u8| -> u8 { (a as f64 + (b as f64 - a as f64) * local).round() as u8 };
    (mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

/// The STELLA wordmark, each row colored column-by-column along the
/// gradient. Returns display-ready strings (ANSI truecolor when the terminal
/// supports it — `colored` handles the no-color fallback itself).
fn wordmark_lines() -> Vec<String> {
    let rows = compose_wordmark("STELLA");
    let width = rows.first().map(|r| r.chars().count()).unwrap_or(1).max(2);
    rows.iter()
        .map(|row| {
            row.chars()
                .enumerate()
                .map(|(i, ch)| {
                    if ch == ' ' {
                        ch.to_string()
                    } else {
                        let (r, g, b) = stellar_color_at(i as f64 / (width - 1) as f64);
                        ch.to_string().truecolor(r, g, b).to_string()
                    }
                })
                .collect::<String>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── shared event lines ─────────────────────────────────────────────────

    /// Drop ANSI SGR sequences so assertions pin visible text regardless of
    /// whether `colored` detects a tty in the test harness.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for e in chars.by_ref() {
                    if e == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn styled_event_line_composes_glyph_body_detail_with_single_spaces() {
        // Wording is pinned byte-exactly in `stella_tui::textline`'s own
        // fixtures; this guards the composition this surface adds around it
        // (the old renderers wrote `"  {glyph} {body} {detail}"`).
        assert_eq!(
            strip_ansi(&styled_event_line(&textline::retry(2, "rate limited"))),
            "↻ retry #2: rate limited"
        );
        assert_eq!(
            strip_ansi(&styled_event_line(&textline::budget_tick(0.42, None))),
            "$ spend: $0.4200"
        );
    }

    // ── wordmark ───────────────────────────────────────────────────────────

    #[test]
    fn wordmark_composes_six_letters_into_five_equal_width_rows() {
        let rows = compose_wordmark("STELLA");
        assert_eq!(rows.len(), GLYPH_ROWS);
        let width = rows[0].chars().count();
        // 6 glyphs × 7 columns + 5 separator spaces.
        assert_eq!(width, 6 * 7 + 5);
        for row in &rows {
            assert_eq!(row.chars().count(), width, "ragged wordmark row");
            assert!(row.chars().all(|c| c == '█' || c == ' '));
        }
    }

    #[test]
    fn wordmark_skips_unknown_characters_instead_of_garbling() {
        // The old banner shipped a hand-pasted figlet that silently drifted
        // into not spelling "Stella" — composition skips anything it has no
        // glyph for, so a typo shrinks the mark instead of corrupting it.
        assert_eq!(compose_wordmark("S?T"), compose_wordmark("ST"));
    }

    #[test]
    fn stellar_gradient_hits_its_endpoint_stops_exactly() {
        assert_eq!(stellar_color_at(0.0), STELLAR_STOPS[0]);
        assert_eq!(stellar_color_at(1.0), STELLAR_STOPS[3]);
        // Out-of-range positions clamp instead of extrapolating.
        assert_eq!(stellar_color_at(-1.0), STELLAR_STOPS[0]);
        assert_eq!(stellar_color_at(2.0), STELLAR_STOPS[3]);
    }
}
