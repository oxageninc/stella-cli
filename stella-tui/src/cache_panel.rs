//! The deck's cache-economics panel: the derived accessors and the pure
//! text formatters behind the statline's CACHE / SAVED / WARMTH cells
//! (issues #267 and #269).
//!
//! Presentation only — the pricing and TTL math already happened upstream in
//! the pricing-aware CLI producer (see [`crate::envelope::Inbound::CacheInsight`])
//! and was folded into each [`AgentEntry`]; this module reads those folded
//! aggregates and turns them into the compact strings the deck renders. Kept
//! out of `deck.rs` to keep that file under its size ratchet, and out of
//! `deck_render.rs` so the formatting is unit-testable without a full frame.

use ratatui::style::Style;
use ratatui::text::Span;
use stella_protocol::CacheCause;

use crate::deck::{AgentEntry, WorkspaceModel};
use crate::deck_render::fmt_tokens;
use crate::theme;

/// Below this hit rate (with enough calls to have established a cache to
/// hit) the deck names a probable cause — matches
/// `stella_model::cache_economics::diagnose_cache`'s own "~20%" acceptance
/// bar and `stella-cli`'s `stats.rs::LOW_HIT_RATE_THRESHOLD`, so a session
/// reads the same diagnosis live in the deck as it does afterward in
/// `stella stats`.
pub const LOW_HIT_RATE_THRESHOLD: f64 = 0.20;

impl AgentEntry {
    /// Seconds of prompt-cache warmth remaining: how long until this agent's
    /// cached prefix expires, from its provider TTL minus the idle since the
    /// last metered call. `None` when the provider has no prompt cache
    /// (`cache_ttl_secs == 0`) or no call has landed yet — nothing to preserve.
    /// `Some(0)` means the prefix has already gone cold (the next turn rewrites
    /// it). Saturating, mirroring `stella_model::CacheWarmth::from_elapsed`,
    /// which the pricing-aware producer computes upstream; the deck cannot link
    /// that model-tier crate, so it re-derives the trivial countdown here.
    pub fn cache_warmth_secs(&self, now_ms: u64) -> Option<u64> {
        let last = self.last_provider_call_ms?;
        if self.cache_ttl_secs == 0 {
            return None;
        }
        let elapsed_secs = now_ms.saturating_sub(last) / 1000;
        Some(self.cache_ttl_secs.saturating_sub(elapsed_secs))
    }

    /// The probable cause of this agent's abnormally low cache hit rate, or
    /// `None` when there's nothing to diagnose (too few calls yet, or a
    /// healthy hit rate) — the motivating incident is a session sitting at
    /// CACHE 0% with no hint anywhere on screen. Mirrors
    /// `stella_model::cache_economics::diagnose_cache`'s selection logic
    /// rather than calling it (the deck cannot link that model-tier crate),
    /// using only locally-tracked aggregates plus
    /// [`Self::cache_is_opt_in_provider`], which the pricing-aware CLI
    /// producer resolves once from the provider's cache-posture table and
    /// folds in via `CacheInsight` — the one piece of real domain knowledge
    /// this re-derivation would otherwise have to duplicate.
    pub fn cache_diagnosis(&self, threshold: f64) -> Option<CacheCause> {
        const MIN_TURNS: u64 = 3;
        if self.cache_call_count <= MIN_TURNS {
            return None;
        }
        let hit_rate = if self.tokens_in == 0 {
            0.0
        } else {
            (self.cache_read_tokens as f64 / self.tokens_in as f64).clamp(0.0, 1.0)
        };
        if hit_rate >= threshold {
            return None;
        }
        if self.cache_is_opt_in_provider && self.cache_write_tokens == 0 {
            return Some(CacheCause::OptInNeverEngaged);
        }
        Some(CacheCause::PrefixInstability)
    }
}

impl WorkspaceModel {
    /// Cumulative prompt-cache *write* tokens across all agents — the write
    /// volume the cache panel shows next to the reads.
    pub fn total_cache_write_tokens(&self) -> u64 {
        self.agents.iter().map(|a| a.cache_write_tokens).sum()
    }

    /// Cumulative estimated USD saved by prompt caching across all agents.
    /// Signed: negative when the write premium outran the reads it bought.
    pub fn total_cache_savings_usd(&self) -> f64 {
        self.agents.iter().map(|a| a.cache_savings_usd).sum()
    }
}

/// Cache-hit percentage (0–100, rounded) for the session, or `None` before any
/// input is metered — the panel shows `—` for `None`, never a divide-by-zero.
pub fn hit_pct(cache_read: u64, total_input: u64) -> Option<u32> {
    if total_input == 0 {
        return None;
    }
    Some(
        ((cache_read as f64 / total_input as f64) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u32,
    )
}

/// Format session cache savings as a signed dollar figure: `$1.23` saved, or
/// `-$0.38` when the write premium outran the reads it bought (the low-hit
/// incident worth surfacing — never hidden behind a clamp).
pub fn fmt_savings(savings_usd: f64) -> String {
    if savings_usd < 0.0 {
        format!("-${:.2}", -savings_usd)
    } else {
        format!("${savings_usd:.2}")
    }
}

/// Format remaining cache warmth as a compact countdown: `m:ss` while warm
/// (`4:12`), `cold` once the prefix has expired, `—` when there is no warm
/// prefix to preserve (no TTL, or no call yet).
pub fn fmt_warmth(remaining_secs: Option<u64>) -> String {
    match remaining_secs {
        None => "—".to_string(),
        Some(0) => "cold".to_string(),
        Some(s) => format!("{}:{:02}", s / 60, s % 60),
    }
}

// ── Statline cell span builders ─────────────────────────────────────────────
//
// Each returns the `Span`s for one statline cell, so `deck_render` stays thin
// (and under its size ratchet) and the styling lives next to the formatting it
// dresses. Colors come from [`crate::theme`], matching the surrounding cells.

/// CACHE cell: hit% then the compact read/write token volumes behind it, or the
/// no-data dash before any input is metered.
pub fn cache_cell(cache_read: u64, cache_write: u64, total_input: u64) -> Vec<Span<'static>> {
    let val = Style::default().fg(theme::TEXT_PRIMARY);
    match hit_pct(cache_read, total_input) {
        None => vec![Span::styled("—", val)],
        Some(pct) => vec![
            Span::styled(format!("{pct}%"), val),
            Span::styled(
                format!(
                    " ({} rd · {} wr)",
                    fmt_tokens(cache_read),
                    fmt_tokens(cache_write)
                ),
                Style::default().fg(theme::TEXT_TERTIARY),
            ),
        ],
    }
}

/// SAVED cell: session dollars saved by caching, danger-colored when the write
/// premium outran the reads (the low-hit incident). `metered` gates the dash —
/// `false` (no input yet) shows `—`, never a misleading `$0.00`.
pub fn saved_cell(savings_usd: f64, metered: bool) -> Vec<Span<'static>> {
    if !metered {
        return vec![Span::styled("—", Style::default().fg(theme::TEXT_PRIMARY))];
    }
    let color = if savings_usd < 0.0 {
        theme::DANGER_BRIGHT
    } else {
        theme::SUCCESS_BRIGHT
    };
    vec![Span::styled(
        fmt_savings(savings_usd),
        Style::default().fg(color),
    )]
}

/// WARMTH cell: countdown until the focused agent's cached prefix expires —
/// danger once cold, warning under a minute (about to cool), success while
/// comfortably warm, dim `—` when there is no warm prefix to preserve.
pub fn warmth_cell(remaining_secs: Option<u64>) -> Vec<Span<'static>> {
    let color = match remaining_secs {
        Some(0) => theme::DANGER_BRIGHT,
        Some(s) if s < 60 => theme::WARNING_BRIGHT,
        Some(_) => theme::SUCCESS_BRIGHT,
        None => theme::TEXT_TERTIARY,
    };
    vec![Span::styled(
        fmt_warmth(remaining_secs),
        Style::default().fg(color),
    )]
}

/// The statline's optional third row: a low-hit-rate diagnosis, prefixed
/// with a warning glyph and rendered in `CacheCause::hint`'s full-sentence
/// wording — byte-identical to what `stella stats` prints for the same
/// cause, per `stella-protocol::cache`'s "the CLI and the TUI render
/// identical wording" contract. Always danger-colored: this row only exists
/// when something is actually wrong.
pub fn diagnosis_spans(cause: CacheCause) -> Vec<Span<'static>> {
    let style = Style::default().fg(theme::DANGER_BRIGHT);
    vec![Span::styled("⚠ ", style), Span::styled(cause.hint(), style)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_pct_is_none_before_input_and_rounds_within_bounds() {
        assert_eq!(hit_pct(0, 0), None);
        assert_eq!(hit_pct(500, 1_000), Some(50));
        assert_eq!(hit_pct(2, 3), Some(67)); // rounds
        // Defensive clamp: cached over total never exceeds 100%.
        assert_eq!(hit_pct(2_000, 1_000), Some(100));
    }

    #[test]
    fn savings_shows_sign_and_two_places() {
        assert_eq!(fmt_savings(1.234), "$1.23");
        assert_eq!(fmt_savings(0.0), "$0.00");
        // The negative case is the whole point — never clamped to $0.00.
        assert_eq!(fmt_savings(-0.375), "-$0.38");
    }

    #[test]
    fn warmth_countdown_reads_cold_at_zero_and_dash_without_a_prefix() {
        assert_eq!(fmt_warmth(None), "—");
        assert_eq!(fmt_warmth(Some(0)), "cold");
        assert_eq!(fmt_warmth(Some(252)), "4:12");
        assert_eq!(fmt_warmth(Some(9)), "0:09"); // zero-padded seconds
    }

    fn entry(
        tokens_in: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        cache_call_count: u64,
        is_opt_in: bool,
    ) -> AgentEntry {
        let mut m = WorkspaceModel::new();
        m.apply_inbound(&crate::envelope::Inbound::Register(
            crate::envelope::AgentMeta::new("lead", "goal", 0),
        ));
        let a = &mut m.agents[0];
        a.tokens_in = tokens_in;
        a.cache_read_tokens = cache_read_tokens;
        a.cache_write_tokens = cache_write_tokens;
        a.cache_call_count = cache_call_count;
        a.cache_is_opt_in_provider = is_opt_in;
        m.agents.remove(0)
    }

    #[test]
    fn diagnosis_fires_only_past_min_turns_and_under_threshold() {
        // Too few calls: 0% hit rate but only 3 turns — nothing to say yet.
        assert_eq!(entry(30_000, 0, 0, 3, true).cache_diagnosis(0.20), None);
        // Past MIN_TURNS, still 0% hit — fires.
        assert!(entry(30_000, 0, 0, 4, true).cache_diagnosis(0.20).is_some());
        // Healthy hit rate past MIN_TURNS — quiet.
        assert_eq!(
            entry(30_000, 20_000, 5_000, 6, true).cache_diagnosis(0.20),
            None
        );
    }

    #[test]
    fn diagnosis_names_opt_in_absent_vs_prefix_instability() {
        // Opt-in provider, 0 hits, 0 writes: the marker never engaged.
        assert_eq!(
            entry(30_000, 0, 0, 5, true).cache_diagnosis(0.20),
            Some(CacheCause::OptInNeverEngaged)
        );
        // Opt-in provider that DID write (writes just never got read back):
        // the prefix is unstable, not opt-in-absent.
        assert_eq!(
            entry(30_000, 0, 15_000, 5, true).cache_diagnosis(0.20),
            Some(CacheCause::PrefixInstability)
        );
        // An implicit-cache provider (is_opt_in false) that never wrote
        // still names prefix instability, never opt-in-absent — there is no
        // marker to have missed.
        assert_eq!(
            entry(30_000, 0, 0, 5, false).cache_diagnosis(0.20),
            Some(CacheCause::PrefixInstability)
        );
    }

    // ── Statline integration tests ──────────────────────────────────────────
    //
    // These render a full frame through `deck_render::render_status_bar` to
    // check the CACHE / SAVED / WARMTH cells land in the deck's Running and
    // Complete states — the deck's snapshot-test idiom (assert on a rendered
    // `Buffer`'s flattened text, same pattern as `deck_render`'s own statline
    // tests). Kept here rather than in `deck_render.rs`'s test module purely
    // to stay under that file's size ratchet; `render_status_bar` is
    // `pub(crate)` for exactly this reason.

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use stella_protocol::{AgentEvent, StageKind};

    use crate::deck_render::render_status_bar;
    use crate::deck_ui::DeckUi;
    use crate::envelope::{AgentMeta, Inbound};

    /// Flatten a rendered `Buffer` to one string, styling stripped — content
    /// is what these tests assert on, never raw ANSI.
    fn buffer_text(buf: &Buffer) -> String {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A running "lead" agent, `now_ms` seeded — the minimal fixture these
    /// cache-panel assertions need (no queue: unlike `deck_render`'s own
    /// `running_model_with_queue`, nothing here touches the composer queue).
    fn running_model() -> WorkspaceModel {
        let mut m = WorkspaceModel::new();
        m.now_ms = 1_000;
        m.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "goal", 0)));
        m.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Stage {
                name: StageKind::Execute,
            },
        });
        m
    }

    /// One committed model call, carrying `input`/`cached` usage — the fold
    /// that feeds the CACHE cell. `step_usage` writes nothing to the cache;
    /// `step_usage_full` also sets the write volume.
    fn step_usage(input: u64, cached: u64) -> AgentEvent {
        step_usage_full(input, cached, 0)
    }

    fn step_usage_full(input: u64, cached: u64, write: u64) -> AgentEvent {
        AgentEvent::StepUsage {
            step: 1,
            role: stella_protocol::event::ModelCallRole::Worker,
            provider: "zai".into(),
            model: "glm".into(),
            input_tokens: input,
            output_tokens: 0,
            cached_input_tokens: cached,
            cache_write_tokens: write,
            estimated_input_tokens: 0,
            cost_usd: 0.0,
            duration_ms: 1,
            retries: 0,
            tool_calls: 0,
            complete: true,
        }
    }

    /// The running model plus one metered step with the given cache usage.
    fn model_with_cache(input: u64, cached: u64) -> WorkspaceModel {
        let mut m = running_model();
        m.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: step_usage(input, cached),
        });
        m
    }

    #[test]
    fn statline_cache_box_shows_hit_rate_and_compact_token_counts() {
        // 105.3M cache-read over 211.4M input → 50% (rounded), compact `M`s.
        let model = model_with_cache(211_400_000, 105_300_000);
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("CACHE"), "cache label present:\n{text}");
        assert!(
            text.contains("50% (105.3M rd · 0 wr)"),
            "cache hit rate + compact read/write volumes:\n{text}"
        );
    }

    #[test]
    fn statline_cache_box_sits_after_spend_and_before_engine() {
        let model = model_with_cache(1_000, 500);
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        let pos = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("missing {needle:?}:\n{text}"))
        };
        assert!(pos("SPEND") < pos("CACHE"), "CACHE after SPEND:\n{text}");
        assert!(pos("CACHE") < pos("ENGINE"), "CACHE before ENGINE:\n{text}");
        assert!(
            pos("ENGINE") < pos("PIPELINE"),
            "PIPELINE after ENGINE:\n{text}"
        );
    }

    #[test]
    fn statline_cache_box_renders_zero_and_full_hit_rates() {
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);

        // 0%: input metered, nothing served from cache.
        let cold = model_with_cache(1_000, 0);
        let mut buf = Buffer::empty(area);
        render_status_bar(&cold, &ui, area, &mut buf);
        assert!(
            buffer_text(&buf).contains("0% (0 rd · 0 wr)"),
            "cold cache reads 0%:\n{}",
            buffer_text(&buf)
        );

        // 100%: every input token was a cache hit.
        let warm = model_with_cache(1_000, 1_000);
        let mut buf = Buffer::empty(area);
        render_status_bar(&warm, &ui, area, &mut buf);
        assert!(
            buffer_text(&buf).contains("100% (1.0K rd · 0 wr)"),
            "fully warm cache reads 100%:\n{}",
            buffer_text(&buf)
        );
    }

    #[test]
    fn statline_cache_box_is_a_dash_before_any_usage() {
        // No StepUsage metered yet → the CACHE cell shows the no-data dash and
        // never divides by zero (the render below must not panic).
        let model = running_model();
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 200, 2);
        let mut buf = Buffer::empty(area);
        render_status_bar(&model, &ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("CACHE"), "cache label still present:\n{text}");
        assert!(
            !text.contains(" wr)"),
            "no read/write volumes before any usage:\n{text}"
        );
    }

    #[test]
    fn statline_cache_panel_shows_savings_and_warmth_running_and_complete() {
        let ui = DeckUi::default();
        let area = Rect::new(0, 0, 240, 2);
        let render = |m: &WorkspaceModel| {
            let mut buf = Buffer::empty(area);
            render_status_bar(m, &ui, area, &mut buf);
            buffer_text(&buf)
        };
        let fold = |m: &mut WorkspaceModel, event| {
            m.apply_inbound(&Inbound::Event {
                agent: "lead".into(),
                event,
            });
        };

        // Running: 150K of 200K input served from cache, 40K written; derived
        // economics say $0.42 saved on a 5-min-TTL provider; 120s idle since,
        // so 180s of warmth ("3:00") remains.
        let mut m = running_model();
        fold(&mut m, step_usage_full(200_000, 150_000, 40_000));
        m.apply_inbound(&Inbound::CacheInsight {
            agent: "lead".into(),
            savings_usd_delta: 0.42,
            ttl_secs: 300,
            is_opt_in_provider: true,
        });
        m.now_ms += 120_000;
        let running = render(&m);
        for needle in [
            "75% (150.0K rd · 40.0K wr)",
            "SAVED",
            "$0.42",
            "WARMTH",
            "3:00",
        ] {
            assert!(running.contains(needle), "missing {needle:?}:\n{running}");
        }

        // Complete: the turn ends; the cache panel stays populated.
        fold(
            &mut m,
            AgentEvent::Complete {
                model: "claude".into(),
                cost_usd: 0.05,
            },
        );
        let complete = render(&m);
        assert!(
            complete.contains("75% (150.0K rd · 40.0K wr)") && complete.contains("$0.42"),
            "cache panel persists in Complete:\n{complete}"
        );
    }
}
