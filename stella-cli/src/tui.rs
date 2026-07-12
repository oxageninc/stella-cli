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

use std::io::{self, Write};
use std::time::Duration;

use colored::Colorize;
use stella_protocol::AgentEvent;

/// Truncate `s` to at most `max` characters, appending `…` when it was
/// shortened. Char-boundary-safe: operates on `char`s, never byte indices, so
/// it can never panic on multi-byte UTF-8 the way the previous `&s[..57]` /
/// `&preview[..77]` byte-slices did (a slice at a non-char boundary is an
/// immediate panic — e.g. any tool input or output containing an accented
/// letter or emoji at the cut point).
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

use std::sync::atomic::{AtomicUsize, Ordering};

use colored::Color;

/// Selectable accent palette — `/color` switches the session's accent so
/// multiple terminal windows running stella are visually distinct at a
/// glance.
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

/// Pretty per-verb rendering for the CRUD file tools; everything else gets
/// the generic key=value card.
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

/// Print a tool-call card: name, input summary, status.
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

/// Print the welcome banner.
pub fn welcome_banner(provider: &str, model: &str, workspace: &str) {
    let stella = r#"
   ____  _     _     _ __        __
  / ___|| |__ (_)___| |\ \      / /_ _ _ __ ___
  \___ \| '_ \| / __| __\ \ /\ / / _` | '__/ _ \
   ___) | | | | \__ \ |_ \ V  V / (_| | | |  __/
  |____/|_| |_|_|___/_|__\_/\_/ \__,_|_|  \___|
"#;
    println!("{}", stella.color(accent()));
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
            // Only render when a limit is being enforced — running spend
            // with no limit is already covered per step by `StepUsage` and
            // at turn end by `cost_summary`, so an extra line per call is
            // pure noise in observed mode.
            if matches!(mode, stella_protocol::BudgetMode::Off) {
                return;
            }
            if let Some(limit) = limit_usd {
                println!("  {} spend: ${spent_usd:.4} / ${limit:.2}", "$".dimmed());
            }
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
        AgentEvent::ProviderFallback { from, to, reason } => {
            println!(
                "  {} provider fallback {} → {}: {}",
                "⚠".yellow().bold(),
                from.bright_yellow(),
                to.bright_green(),
                reason.dimmed()
            );
        }
        AgentEvent::FileChange { path, kind, .. } => {
            let (icon, verb) = match kind {
                stella_protocol::FileChangeKind::Created => ("＋".green(), "created"),
                stella_protocol::FileChangeKind::Modified => ("✎".cyan(), "modified"),
                stella_protocol::FileChangeKind::Deleted => ("－".red(), "deleted"),
            };
            println!("  {icon} {} {path}", verb.dimmed());
        }
        AgentEvent::ContextRecall { frames, tokens, .. } => {
            println!(
                "  {} recalled {} frame{} ({tokens} tokens)",
                "⋯".dimmed(),
                frames.len(),
                if frames.len() == 1 { "" } else { "s" },
            );
        }
        AgentEvent::ContextWrite {
            provider,
            upserts,
            superseded,
        } => {
            println!(
                "  {} context write [{}]: {upserts} upserted, {superseded} superseded",
                "⋯".dimmed(),
                provider.dimmed(),
            );
        }
        AgentEvent::JudgeVerdict { passed, evidence } => {
            let icon = if *passed {
                "✓".green().bold()
            } else {
                "✗".red().bold()
            };
            println!("  {icon} judge: {}", evidence.summary);
        }
        AgentEvent::ScopeReview { proposal } => {
            println!(
                "  {} scope review: {} ({} step{}, ~{} file{})",
                "⚑".yellow().bold(),
                proposal.summary,
                proposal.steps.len(),
                if proposal.steps.len() == 1 { "" } else { "s" },
                proposal.estimated_files,
                if proposal.estimated_files == 1 {
                    ""
                } else {
                    "s"
                },
            );
        }
        AgentEvent::AskUser {
            question, options, ..
        } => {
            println!("  {} {question}", "?".bright_blue().bold());
            for (i, opt) in options.iter().enumerate() {
                println!("      {}. {}", i + 1, opt.dimmed());
            }
        }
        AgentEvent::MediaProgress { kind, state, .. } => {
            println!("  {} media {kind:?}: {state:?}", "◧".dimmed());
        }
        AgentEvent::MediaComplete { artifact } => {
            println!(
                "  {} media ready: {} → {}",
                "✓".green(),
                artifact.label,
                artifact.path.dimmed(),
            );
        }
        AgentEvent::Commit { sha, message } => {
            let short = sha.chars().take(7).collect::<String>();
            println!("  {} commit {} {message}", "●".magenta(), short.yellow());
        }
        AgentEvent::Pr { url, status } => {
            println!("  {} pull request {status:?}: {}", "⇅".cyan(), url.dimmed());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_leaves_short_strings_untouched() {
        assert_eq!(truncate_with_ellipsis("hello", 57), "hello");
    }

    #[test]
    fn truncate_appends_ellipsis_when_shortened() {
        let long = "a".repeat(100);
        let out = truncate_with_ellipsis(&long, 57);
        assert_eq!(out.chars().count(), 58); // 57 chars + the ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_never_panics_on_multibyte_boundaries() {
        // Regression for the `&s[..57]` / `&preview[..77]` byte-slice panic:
        // a string whose byte-length exceeds the cap but whose cut point
        // lands inside a multi-byte char must truncate cleanly, not crash.
        let accented = "é".repeat(100); // each 'é' is 2 bytes
        let out = truncate_with_ellipsis(&accented, 57);
        assert_eq!(out.chars().count(), 58);
        assert!(out.ends_with('…'));

        let emoji = "🦀".repeat(80); // each is 4 bytes
        let out = truncate_with_ellipsis(&emoji, 77);
        assert_eq!(out.chars().count(), 78);
    }

    #[test]
    fn tool_cards_do_not_panic_on_non_ascii_input_and_output() {
        // Exercises the real call sites (both former panic points) with
        // non-ASCII payloads long enough to trip truncation.
        let input = serde_json::json!({ "path": "café/".to_string() + &"señor".repeat(40) });
        tool_call_card("read_file", &input, "running");
        tool_result_card(
            "read_file",
            &("🦀 ".repeat(60)),
            false,
            Duration::from_millis(3),
        );
    }
}
