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
//! below gives one clean, immediate "thinking" line instead.
//!
//! The [`PromptDuel`] animation (a rocket dueling a UFO one line above the
//! prompt, ported from the TS CLI's `SpaceInvaders` rail) does NOT violate
//! that rationale: it runs only while the REPL is blocked on `read_line` —
//! no turn in flight, no event stream to race — and is stopped before the
//! turn starts. The only concurrent writer while it animates is the
//! terminal's own echo of the user's keystrokes, which lives on a different
//! row than the one the animator repaints.

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use colored::{Color, Colorize};
use stella_protocol::AgentEvent;

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

/// Render a token count compactly: `842`, `12.3k`, `1.2M`.
fn fmt_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
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

/// Print streaming text delta (no newline — accumulates on one line).
#[allow(dead_code)]
pub fn print_delta(text: &str) {
    print!("{}", text);
    let _ = io::stdout().flush();
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
        "\n  {} {} · ${cost_usd:.4} · {:.1}s",
        "◆".dimmed(),
        model.bright_blue(),
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
pub fn render_event(event: &AgentEvent) {
    match event {
        AgentEvent::Stage {
            name: stella_protocol::StageKind::Execute,
        } => {
            println!("  {}", "thinking…".dimmed());
        }
        AgentEvent::Stage { .. } | AgentEvent::ToolStart { .. } | AgentEvent::ToolResult { .. } => {
            // Complete-stage and ToolStart/ToolResult: handled inline at the
            // call site or by a more specific event (see the module doc).
        }
        AgentEvent::Text { delta } => assistant_response(delta),
        AgentEvent::Reasoning { .. } => {}
        AgentEvent::Retry { attempt, reason } => {
            println!("  {} retry #{attempt}: {}", "↻".yellow(), reason.dimmed());
        }
        AgentEvent::Compaction {
            before_tokens,
            after_tokens,
            evicted,
            deduped,
        } => {
            println!(
                "  {} compacted context: {before_tokens} → {after_tokens} tokens ({evicted} evicted, {deduped} deduped)",
                "⤵".cyan(),
            );
        }
        AgentEvent::BudgetTick {
            spent_usd,
            limit_usd,
            mode,
        } => {
            if matches!(mode, stella_protocol::BudgetMode::Off) {
                return;
            }
            match limit_usd {
                Some(limit) => println!("  {} spend: ${spent_usd:.4} / ${limit:.2}", "$".dimmed()),
                None => println!("  {} spend: ${spent_usd:.4}", "$".dimmed()),
            }
        }
        AgentEvent::ProviderFallback { from, to, reason } => {
            println!(
                "  {} provider fallback {} → {}: {}",
                "⚠".yellow().bold(),
                from.bright_yellow(),
                to.bright_green(),
                reason.dimmed()
            );
        }
        AgentEvent::StepUsage {
            step,
            model,
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cost_usd,
            duration_ms,
            retries,
            tool_calls,
        } => {
            // One dimmed telemetry line per committed model call: the live
            // HUD a metering consumer would reconstruct from this event.
            let cached = if *cached_input_tokens > 0 {
                format!(" ({} cached)", fmt_tokens(*cached_input_tokens))
            } else {
                String::new()
            };
            let retried = if *retries > 0 {
                format!(" · {retries} retry")
            } else {
                String::new()
            };
            let tools = if *tool_calls > 0 {
                format!(
                    " · {tool_calls} tool call{}",
                    if *tool_calls == 1 { "" } else { "s" }
                )
            } else {
                String::new()
            };
            println!(
                "  {} step {} · {} · {}{} in → {} out · ${cost_usd:.4} · {:.1}s{}{}",
                "·".dimmed(),
                step + 1,
                model.dimmed(),
                fmt_tokens(*input_tokens),
                cached.dimmed(),
                fmt_tokens(*output_tokens),
                (*duration_ms as f64) / 1000.0,
                retried.dimmed(),
                tools.dimmed(),
            );
        }
        AgentEvent::GoalVerdict {
            round,
            met,
            reasoning,
            ..
        } => {
            if *met {
                println!(
                    "  {} judge verdict (round {round}): goal met — {}",
                    "✓".green().bold(),
                    reasoning
                );
            } else {
                println!(
                    "  {} judge verdict (round {round}): not yet met — {}",
                    "○".yellow(),
                    reasoning
                );
            }
        }
        AgentEvent::FileChange { path, kind, .. } => {
            let verb = match kind {
                stella_protocol::FileChangeKind::Created => "created",
                stella_protocol::FileChangeKind::Modified => "modified",
                stella_protocol::FileChangeKind::Deleted => "deleted",
            };
            println!("  {} {} {}", "±".cyan(), verb.dimmed(), path);
        }
        AgentEvent::ContextRecall {
            frames,
            provider_mix,
            tokens,
        } => {
            let mix = provider_mix
                .iter()
                .map(|share| format!("{}×{}", share.frames, share.provider))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "  {} recalled {} frames ({tokens} tokens: {mix})",
                "◈".cyan(),
                frames.len()
            );
        }
        AgentEvent::ContextWrite {
            provider,
            upserts,
            superseded,
        } => {
            println!(
                "  {} context write-back via {provider}: {upserts} upserts, {superseded} superseded",
                "◈".dimmed()
            );
        }
        AgentEvent::MediaProgress {
            artifact_id,
            kind,
            state,
        } => {
            let state_str = match state {
                stella_protocol::MediaJobState::Queued => "queued".dimmed(),
                stella_protocol::MediaJobState::Running => "running".cyan(),
                stella_protocol::MediaJobState::Succeeded => "succeeded".green(),
                stella_protocol::MediaJobState::Failed { reason } => {
                    println!(
                        "  {} {kind:?} job {artifact_id} failed: {reason}",
                        "✗".red()
                    );
                    return;
                }
            };
            println!("  {} {kind:?} job {artifact_id}: {state_str}", "▣".cyan());
        }
        AgentEvent::MediaComplete { artifact } => {
            println!(
                "  {} {} ready: {} ({})",
                "▣".green(),
                artifact.label,
                artifact.path,
                format!("{:?}", artifact.kind).to_lowercase()
            );
        }
        AgentEvent::JudgeVerdict { passed, evidence } => {
            let icon = if *passed { "✓".green() } else { "✗".red() };
            let source = if evidence.deterministic {
                "deterministic"
            } else {
                "model judge"
            };
            println!("  {icon} verify ({source}): {}", evidence.summary.dimmed());
        }
        AgentEvent::ScopeReview { proposal } => {
            println!(
                "  {} scope review: {} ({} steps, ~{} files{})",
                "⌾".yellow().bold(),
                proposal.summary,
                proposal.steps.len(),
                proposal.estimated_files,
                proposal
                    .estimated_cost_usd
                    .map(|c| format!(", ~${c:.2}"))
                    .unwrap_or_default()
            );
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
            println!("  {} {}", "?".yellow().bold(), question.bold());
            for (i, option) in options.iter().enumerate() {
                println!("    {} {}", format!("{})", i + 1).dimmed(), option);
            }
        }
        AgentEvent::Commit { sha, message } => {
            let short = sha.get(..8).unwrap_or(sha.as_str());
            println!("  {} committed {short} {}", "●".green(), message.dimmed());
        }
        AgentEvent::Pr { url, status } => {
            println!(
                "  {} PR {}: {url}",
                "⇡".cyan(),
                format!("{status:?}").to_lowercase()
            );
        }
        AgentEvent::Error { message, retryable } => {
            let label = if *retryable { "warning" } else { "error" };
            eprintln!("  {} {}: {}", "✗".red(), label.red().bold(), message);
        }
        AgentEvent::Complete { .. } => {
            // The call site prints `cost_summary` from `TurnOutcome`
            // directly (it has the same model/cost_usd this event carries,
            // plus wall-clock elapsed time this event doesn't).
        }
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

// ── Fun: a rocket duels a UFO one line above the prompt ─────────────────────
//
// A straight port of the TS CLI's `renderInvadersLane` (apps/cli/src/repl/
// components.tsx) — same sprites, same constants, same deterministic
// physics, so the two CLIs share one signature animation. Sprites are
// monochrome geometric glyphs (no color emoji) so they read the same in any
// terminal theme. The rocket idles on a flickering thrust flame and snaps
// into a recoil frame the instant it fires; the UFO patrols a weaving beat
// and pulses its running light, mostly taking the hit and blooming into a
// brief explosion, occasionally weaving clear for a near-miss flyby.

const ROCKET: [&str; 2] = ["}=>", "-=>"];
const ROCKET_RECOIL: &str = "{=>";
const ROCKET_IDLE: &str = "|=>";
const ROCKET_LEN: usize = 3;
const UFO: [&str; 2] = ["<●>", "<○>"];
const UFO_LEN: usize = 3;
const EXPLOSION: [&str; 2] = ["✳✳✳", " ✸ "]; // flash, then dissipate to a spark
const BOLT_HEAD: char = '•';
const BOLT_TRAIL: char = '·';
const BOLT_SPEED: usize = 3; // cells advanced per tick — a snappy tracer

/// Write `sprite` into the fixed-width lane at `at`, clipping at both edges.
fn place_sprite(lane: &mut [char], at: isize, sprite: &str) {
    for (i, ch) in sprite.chars().enumerate() {
        let idx = at + i as isize;
        if idx >= 0 && (idx as usize) < lane.len() {
            lane[idx as usize] = ch;
        }
    }
}

/// Bounces `n` back and forth across [0, span] — the UFO's weaving patrol.
fn triangle_wave(n: u64, span: usize) -> usize {
    if span == 0 {
        return 0;
    }
    let period = (span * 2) as u64;
    let m = (n % period) as usize;
    if m <= span { m } else { period as usize - m }
}

/// Whether half-open ranges [a, a+a_len) and [b, b+b_len) intersect.
fn overlaps(a: usize, a_len: usize, b: usize, b_len: usize) -> bool {
    a < b + b_len && b < a + a_len
}

/// Pure renderer for one row of the rocket-vs-UFO duel, at a fixed `width`.
/// When `active` the rocket fires on a steady cadence while the UFO weaves a
/// patrol near the far end of the lane; most bolts land and bloom into a
/// brief explosion before the UFO reforms, but the weave occasionally
/// carries it clear of the shot for a near-miss flyby instead. Kept pure
/// (no timers, no I/O) so every frame is deterministic and unit-testable.
pub fn render_invaders_lane(tick: u64, width: usize, active: bool) -> String {
    let w = width.clamp(14, 30);
    let mut lane: Vec<char> = vec![' '; w];
    let muzzle = ROCKET_LEN; // column right after the rocket's nose

    if !active {
        place_sprite(&mut lane, 0, ROCKET_IDLE);
        place_sprite(&mut lane, (w - UFO_LEN) as isize, UFO[0]);
        return lane.into_iter().collect();
    }

    // The UFO patrols a weave zone near the far end, well clear of the rocket.
    let ufo_right = w - UFO_LEN;
    let ufo_left = (muzzle + 3).max(ufo_right.saturating_sub(5));
    let weave_span = (ufo_right - ufo_left).max(1);
    let ufo_at = |t: u64| -> usize { ufo_left + triangle_wave(t, weave_span) };

    // The rocket fires every `cycle` ticks: a recoil/muzzle-flash tick spawns
    // the bolt, which streaks out at BOLT_SPEED cells/tick until it lands a
    // hit or clears the lane, followed by a short beat before it reloads.
    let flight_ticks = (w - muzzle).div_ceil(BOLT_SPEED) as u64;
    let cycle = flight_ticks + 2;
    let cycle_pos = tick % cycle;
    let volley_start = tick - cycle_pos;

    // Whether (and when) this volley's bolt connects is a pure function of
    // the volley's start tick, so hits and near-misses fall out of the
    // rocket/UFO phase relationship instead of a coin flip.
    let mut hit_frame: Option<u64> = None;
    for f in 0..flight_ticks {
        let bolt_at = muzzle + f as usize * BOLT_SPEED;
        if overlaps(bolt_at, 1, ufo_at(volley_start + f), UFO_LEN) {
            hit_frame = Some(f);
            break;
        }
    }

    place_sprite(
        &mut lane,
        0,
        if cycle_pos == 0 {
            ROCKET_RECOIL
        } else {
            ROCKET[(tick % 2) as usize]
        },
    );

    if let Some(hit) = hit_frame
        && cycle_pos >= hit
    {
        // The bolt has landed — flash the impact, then let the UFO reform
        // and resume its patrol for the rest of the beat before the next
        // volley.
        let since_hit = (cycle_pos - hit) as usize;
        if since_hit < EXPLOSION.len() {
            place_sprite(
                &mut lane,
                ufo_at(volley_start + hit) as isize,
                EXPLOSION[since_hit],
            );
        } else {
            place_sprite(&mut lane, ufo_at(tick) as isize, UFO[(tick % 2) as usize]);
        }
        return lane.into_iter().collect();
    }

    if cycle_pos < flight_ticks {
        // Still in flight (a miss this volley, or a hit not yet reached) —
        // the bolt leaves a fading trail behind its head as it streaks.
        let bolt_at = muzzle + cycle_pos as usize * BOLT_SPEED;
        if bolt_at > muzzle {
            lane[bolt_at - 1] = BOLT_TRAIL;
        }
        if bolt_at < w {
            lane[bolt_at] = BOLT_HEAD;
        }
    }
    place_sprite(&mut lane, ufo_at(tick) as isize, UFO[(tick % 2) as usize]);
    lane.into_iter().collect()
}

/// Default lane width — matches the TS `SpaceInvaders` component.
const DUEL_WIDTH: usize = 24;
/// Frame period — matches the TS component's 140ms interval.
const DUEL_FRAME: Duration = Duration::from_millis(140);

/// The animated duel one line above the prompt input. `start()` prints the
/// opening frame on its own line (call it right before printing the prompt
/// marker, so the lane sits exactly one row above the input), then a
/// background thread repaints that row in place every ~140ms while the REPL
/// is blocked reading input. `stop()` freezes the duel on its last frame —
/// it stays behind in scrollback, the same way the TS rail goes still the
/// moment its turn ends.
///
/// Repainting uses save-cursor / up-one-row / clear-line / restore in a
/// single buffered write, so the user's in-progress typing on the row below
/// is never touched. The only race is the beat between the user pressing
/// Enter (which scrolls the prompt row up) and the caller invoking `stop()`;
/// callers keep that window sub-millisecond by stopping immediately after
/// `read_line` returns, against a 140ms frame period.
pub struct PromptDuel {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PromptDuel {
    /// Start the duel if this session wants it: stdout must be a real
    /// terminal (cursor-movement escapes would corrupt a pipe), and the fun
    /// switch must not be off (`STELLA_FUN=0`, or the TS CLI's legacy
    /// `STELLA_CLI_FUN=0`). Returns `None` — and prints nothing — otherwise.
    pub fn start() -> Option<Self> {
        let fun_off = |var: &str| std::env::var(var).map(|v| v == "0").unwrap_or(false);
        if !io::stdout().is_terminal() || fun_off("STELLA_FUN") || fun_off("STELLA_CLI_FUN") {
            return None;
        }

        println!(
            "  {} {}",
            "▸".dimmed(),
            render_invaders_lane(0, DUEL_WIDTH, true).cyan()
        );

        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut tick: u64 = 0;
            loop {
                std::thread::sleep(DUEL_FRAME);
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                tick += 1;
                let frame = format!(
                    // save cursor → up one row → col 1 → clear row → lane →
                    // restore cursor, as one write so a keystroke echo can't
                    // interleave mid-escape.
                    "\x1b7\x1b[1A\r\x1b[2K  {} {}\x1b8",
                    "▸".dimmed(),
                    render_invaders_lane(tick, DUEL_WIDTH, true).cyan()
                );
                let mut out = io::stdout();
                let _ = out.write_all(frame.as_bytes());
                let _ = out.flush();
            }
        });

        Some(Self {
            stop,
            handle: Some(handle),
        })
    }

    /// Stop animating and wait for the painter thread to exit, leaving the
    /// last frame in scrollback.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── invaders lane ──────────────────────────────────────────────────────
    //
    // Fixtures are ported verbatim from the TS suite
    // (apps/cli/src/repl/__tests__/components.test.tsx) so the two CLIs'
    // animations can never drift apart silently: at W=24 the first volley's
    // bolt connects on tick 6, and the 9th volley (ticks 72–80) is the
    // deterministic near-miss.

    const W: usize = 24;

    #[test]
    fn stands_down_when_idle_rocket_parked_ufo_calm() {
        let lane = render_invaders_lane(0, W, false);
        assert_eq!(lane.chars().count(), W); // fixed width — no layout jitter
        assert!(lane.starts_with("|=>")); // powered-down rocket, thrust off
        assert!(lane.trim_end().ends_with("<●>")); // UFO holding position
        assert!(!lane.contains('•')); // no bolt in flight while still
    }

    #[test]
    fn fires_when_active_bolt_streaks_toward_the_ufo() {
        // Tick 2 is mid-flight (well before the tick-6 hit for this width),
        // so rocket, bolt, and UFO are all on the lane at once.
        let lane = render_invaders_lane(2, W, true);
        assert_eq!(lane.chars().count(), W);
        let chars: Vec<char> = lane.chars().collect();
        assert!(matches!(chars[0], '{' | '}' | '-')); // a rocket frame at the nose
        let nose = 2; // first char after the rocket sprite
        let bolt = chars
            .iter()
            .position(|&c| c == '•')
            .expect("bolt in flight");
        let ufo = chars
            .iter()
            .position(|&c| c == '●' || c == '○')
            .expect("ufo on lane");
        assert!(bolt > nose);
        assert!(ufo > bolt); // the UFO is still further out ahead of the bolt
    }

    #[test]
    fn advances_the_bolt_frame_over_frame() {
        let bolt_at = |tick: u64| -> usize {
            render_invaders_lane(tick, W, true)
                .chars()
                .position(|c| c == '•')
                .expect("bolt in flight")
        };
        // Ticks 0-2 are the opening flight of the first volley at this width
        // — the bolt moves BOLT_SPEED (3) cells closer to the UFO each tick.
        assert_eq!(bolt_at(0), 3);
        assert_eq!(bolt_at(1), 6);
        assert_eq!(bolt_at(2), 9);
    }

    #[test]
    fn blooms_into_an_explosion_on_a_hit_then_reforms_the_ufo() {
        // Verified fixture: at W=24 the first volley's bolt connects on tick 6.
        let impact = render_invaders_lane(6, W, true);
        assert!(impact.contains('✳') || impact.contains('✸'));
        assert!(!impact.contains('•')); // the bolt is spent on impact
        assert!(!impact.contains('●') && !impact.contains('○')); // explosion replaces the UFO

        let dissipating = render_invaders_lane(7, W, true);
        assert!(dissipating.contains('✳') || dissipating.contains('✸'));

        // Later in the same beat (before the next volley reloads) the UFO reforms.
        let reformed = render_invaders_lane(8, W, true);
        assert!(reformed.contains('●') || reformed.contains('○'));
        assert!(!reformed.contains('✳') && !reformed.contains('✸'));
    }

    #[test]
    fn occasionally_the_weave_carries_the_ufo_clear_for_a_near_miss() {
        // Verified fixture: at W=24 (fire cycle 9, weave period 10 — coprime,
        // so the phase drifts volley to volley) the 9th volley (ticks 72-80)
        // never lines up — the bolt streaks past and the UFO is untouched.
        let frames: Vec<String> = (72..81).map(|t| render_invaders_lane(t, W, true)).collect();
        assert!(!frames.iter().any(|f| f.contains('✳') || f.contains('✸')));
        assert!(frames.iter().any(|f| f.contains('•'))); // the rocket still fired
        assert!(
            frames.iter().all(|f| f.contains('●') || f.contains('○')) // the UFO survives throughout
        );
    }

    #[test]
    fn is_deterministic_and_fixed_width_across_a_long_run() {
        for tick in 0..200 {
            let a = render_invaders_lane(tick, W, true);
            let b = render_invaders_lane(tick, W, true);
            assert_eq!(a, b, "tick {tick} not deterministic");
            assert_eq!(a.chars().count(), W, "tick {tick} broke the lane width");
        }
    }

    #[test]
    fn clamps_degenerate_widths_instead_of_panicking() {
        // Below the 14-cell minimum and above the 30-cell maximum both clamp.
        assert_eq!(render_invaders_lane(5, 1, true).chars().count(), 14);
        assert_eq!(render_invaders_lane(5, 500, true).chars().count(), 30);
    }
}
