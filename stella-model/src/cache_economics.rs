//! Cache economics: turn the normalized cache counters into dollars saved and
//! a probable-cause diagnosis.
//!
//! Two halves:
//!  - [`Pricing::cache_savings_usd`] — the pure savings arithmetic, keyed off
//!    catalog list pricing. This is the one canonical formula; the deck and
//!    `stella stats` both reach it (directly or through a value the CLI
//!    precomputes and hands the dependency-free TUI). Signed on purpose: a
//!    negative result *is* the "$2.31 session, cache 0%" story — the write
//!    premium was paid and never earned back.
//!  - [`diagnose_cache`] — names the [`CacheCause`] behind an abnormally low
//!    hit rate, consulting the read-only [`crate::provider_parity`] posture
//!    matrix to tell an opt-in-marker bug from prefix instability.
//!
//! The **write premium** (what a cache write costs *over* the base input rate)
//! is provider policy, not arithmetic: today only the opt-in providers
//! (Anthropic, Bedrock, OpenRouter-Claude) report cache writes, and their
//! 5-minute cache writes bill at 1.25x input. The catalog carries no
//! cache-write rate column yet (the staged follow-up to issue #97), so
//! [`cache_write_premium_multiplier`] holds that factor here, keyed by
//! provider — merge-later into the pricing/parity matrix once the column
//! lands.

use stella_protocol::{CacheCause, CompletionUsage};

use crate::catalog::Pricing;
use crate::provider_parity::{CachePosture, cache_posture};

/// USD per million tokens divisor, matching [`Pricing::cost_usd`].
const PER_MTOK: f64 = 1_000_000.0;

/// The multiplier a provider bills a cache *write* at, relative to its base
/// input rate — so the per-token write premium is `input_rate * (mult - 1)`.
///
/// Only the opt-in cache providers actually report `cache_write_tokens`; the
/// implicit-cache providers report zero writes, so their multiplier is never
/// exercised and `1.0` (no premium) is the honest default. Anthropic-family
/// 5-minute cache writes are 1.25x input (the 1-hour TTL is 2x, a per-request
/// choice not visible in the usage envelope, so it is not modeled here).
///
/// Local const table, deliberately: the authoritative home for this factor is
/// the catalog's not-yet-added cache-write rate column (issue #97). Merge this
/// into that column / the `provider_parity` matrix when it lands.
pub fn cache_write_premium_multiplier(provider: &str) -> f64 {
    match provider {
        "anthropic" | "bedrock" | "openrouter" => 1.25,
        _ => 1.0,
    }
}

impl Pricing {
    /// Estimated USD saved by prompt caching for one usage envelope, net of
    /// the write premium:
    ///
    /// ```text
    ///   savings = cached_tokens x (input_rate - cached_rate)
    ///           - write_tokens  x write_premium_per_mtok
    /// ```
    ///
    /// `write_premium_usd_per_mtok` is the premium a cache write costs *over*
    /// the base input rate (see [`cache_write_premium_multiplier`]); pass
    /// `0.0` for providers that bill writes at the input rate. The result is
    /// **signed** — negative when the write premium outweighs the reads it
    /// bought, which is exactly the low-hit-rate incident worth surfacing —
    /// so it is never clamped to zero. Cached tokens are clamped to the
    /// reported input (a provider reporting more cached than total input, which
    /// shouldn't happen, never inflates the saving), mirroring
    /// [`Pricing::cost_usd`].
    pub fn cache_savings_usd(
        &self,
        usage: &CompletionUsage,
        write_premium_usd_per_mtok: f64,
    ) -> f64 {
        let cached = usage.cached_input_tokens.min(usage.input_tokens);
        let read_saved =
            (cached as f64 / PER_MTOK) * (self.input_usd_per_mtok - self.cached_input_usd_per_mtok);
        let write_cost = (usage.cache_write_tokens as f64 / PER_MTOK) * write_premium_usd_per_mtok;
        read_saved - write_cost
    }

    /// [`Pricing::cache_savings_usd`] with the write premium resolved from the
    /// provider's [`cache_write_premium_multiplier`] against this row's own
    /// input rate — the form the CLI receipt and the deck producer use, since
    /// they already know the provider id.
    pub fn cache_savings_usd_for(&self, provider: &str, usage: &CompletionUsage) -> f64 {
        let premium =
            self.input_usd_per_mtok * (cache_write_premium_multiplier(provider) - 1.0).max(0.0);
        self.cache_savings_usd(usage, premium)
    }
}

/// The prompt-cache hit rate for a usage aggregate: cached input over total
/// input, in `[0, 1]`. `0.0` when no input has been metered (an honest
/// "nothing to hit yet", never a divide-by-zero).
pub fn hit_rate(input_tokens: u64, cached_input_tokens: u64) -> f64 {
    if input_tokens == 0 {
        return 0.0;
    }
    (cached_input_tokens as f64 / input_tokens as f64).clamp(0.0, 1.0)
}

/// Name the probable cause of a low cache hit rate, or `None` when there is
/// nothing to diagnose. Pure over its inputs (the posture lookup is static
/// data), so it is table-testable without a runtime.
///
/// Gates first: a diagnosis only fires once a session has run enough turns to
/// have *established* a cache to hit (`turns > MIN_TURNS`) and the hit rate is
/// genuinely under `threshold`. The discriminator between the two opt-in
/// failure modes is **`cache_write_tokens`**, not the hit rate:
///  - opt-in provider that wrote *nothing* over the turns → the marker never
///    reached the wire ([`CacheCause::OptInNeverEngaged`]);
///  - otherwise (writes happened, or an implicit-cache provider) a low hit
///    rate is the prefix being rewritten or expiring between turns
///    ([`CacheCause::PrefixInstability`]).
///
/// `IdleBeyondTtl` is a refinement the live scheduler surfaces from actual
/// idle gaps; this token-only diagnosis cannot see wall-clock gaps, so it
/// never returns it (it stays reachable for the TTL-aware scheduler path).
pub fn diagnose_cache(
    provider: &str,
    turns: u64,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_tokens: u64,
    threshold: f64,
) -> Option<CacheCause> {
    /// A cache needs a few turns to have been established before a low hit
    /// rate is meaningful (turn 1 always writes, never reads).
    const MIN_TURNS: u64 = 3;

    if turns <= MIN_TURNS {
        return None;
    }
    if hit_rate(input_tokens, cached_input_tokens) >= threshold {
        return None;
    }

    let is_opt_in = matches!(cache_posture(provider), Some(CachePosture::OptIn { .. }));
    if is_opt_in && cache_write_tokens == 0 {
        // The provider caches nothing without an explicit marker, and not one
        // token was ever written — the opt-in never engaged.
        return Some(CacheCause::OptInNeverEngaged);
    }
    Some(CacheCause::PrefixInstability)
}

/// Per-provider prompt-cache TTL in seconds — how long a written prefix stays
/// readable before the provider evicts it, keyed by provider id. `None` for a
/// provider with no documented eviction window (nothing to schedule around).
///
/// Anthropic's default cache TTL is 5 minutes; Bedrock and OpenRouter's Claude
/// routes ride the same default. The 1-hour Anthropic/OpenRouter TTL is a
/// per-request opt-in that bills writes at 2x — a caller's choice, not modeled
/// here (this is the *default* a TTL-blind scheduler forfeits).
///
/// Local const table, deliberately: the authoritative home is the
/// `provider_parity` matrix's not-yet-added TTL column. Merge this into that
/// column when it lands — it pairs with [`cache_write_premium_multiplier`].
pub fn provider_cache_ttl_secs(provider: &str) -> Option<u64> {
    match provider {
        "anthropic" | "bedrock" | "openrouter" => Some(300),
        _ => None,
    }
}

/// Remaining prompt-cache warmth for a live session: how long until its written
/// prefix expires, derived from the time since the session's last provider call
/// and the provider TTL. Pure over its inputs — it reads no clock, so the
/// scheduler and the deck's countdown compute it the same way from passed-in
/// elapsed time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheWarmth {
    /// Seconds until the cached prefix expires; `0` once it already has.
    pub remaining_secs: u64,
    /// True once the prefix has expired — the next turn re-writes it.
    pub expired: bool,
}

impl CacheWarmth {
    /// Warmth of a session whose last provider call was `elapsed_secs` ago,
    /// against a `ttl_secs` cache TTL. Saturating: a session idle longer than
    /// the TTL reads `remaining_secs: 0, expired: true`, never underflows.
    pub fn from_elapsed(elapsed_secs: u64, ttl_secs: u64) -> Self {
        let remaining_secs = ttl_secs.saturating_sub(elapsed_secs);
        Self {
            remaining_secs,
            expired: remaining_secs == 0,
        }
    }
}

/// Whether a model call is a *cache-expired rewrite*: the session's prefix went
/// cold (the `gap_secs` since its previous call exceeded the provider
/// `ttl_secs`) **and** this call wrote the cache again (`cache_write_tokens >
/// 0`), so the whole prefix was re-billed at the write rate rather than read
/// back. This is the exact event [`CacheCause::IdleBeyondTtl`] names and
/// TTL-aware scheduling exists to prevent; counting it makes the heuristic's
/// savings measurable (the `cache_expired_rewrite` counter). The strict `>`
/// mirrors the provider contract that a prefix is still readable *at* the TTL
/// boundary, cold only past it.
pub fn is_cache_expired_rewrite(gap_secs: u64, cache_write_tokens: u64, ttl_secs: u64) -> bool {
    gap_secs > ttl_secs && cache_write_tokens > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cached: u64, write: u64) -> CompletionUsage {
        CompletionUsage {
            input_tokens: input,
            output_tokens: 0,
            cached_input_tokens: cached,
            cache_write_tokens: write,
        }
    }

    #[test]
    fn savings_matches_catalog_pricing_math_by_hand() {
        // Claude Fable 5 seed pricing: input $3.00/M, cached $0.30/M.
        // 400k cached reads saved at (3.00 - 0.30)/M and 100k writes at a
        // 1.25x premium (0.25 x 3.00 = 0.75/M):
        //   read  = 400_000 / 1e6 * 2.70 = 1.08
        //   write = 100_000 / 1e6 * 0.75 = 0.075
        //   net   = 1.08 - 0.075        = 1.005
        let pricing = Pricing {
            input_usd_per_mtok: 3.00,
            output_usd_per_mtok: 15.00,
            cached_input_usd_per_mtok: 0.30,
        };
        let premium = 3.00 * (1.25 - 1.0); // 0.75/M
        let got = pricing.cache_savings_usd(&usage(1_000_000, 400_000, 100_000), premium);
        assert!((got - 1.005).abs() < 1e-9, "got {got}");
    }

    #[test]
    fn savings_is_signed_negative_when_writes_never_earn_back() {
        // The motivating incident: a session that keeps writing the cache
        // (a fresh prefix every turn) but never reads it back pays the write
        // premium for nothing — the saving must go negative, not clamp to 0.
        let pricing = Pricing {
            input_usd_per_mtok: 3.00,
            output_usd_per_mtok: 15.00,
            cached_input_usd_per_mtok: 0.30,
        };
        let premium = 3.00 * 0.25;
        let got = pricing.cache_savings_usd(&usage(500_000, 0, 500_000), premium);
        assert!(
            got < 0.0,
            "writes-with-no-reads must show a loss, got {got}"
        );
        // -500_000/1e6 * 0.75 = -0.375
        assert!((got + 0.375).abs() < 1e-9, "got {got}");
    }

    #[test]
    fn savings_for_resolves_the_premium_from_provider_and_input_rate() {
        let pricing = Pricing {
            input_usd_per_mtok: 3.00,
            output_usd_per_mtok: 15.00,
            cached_input_usd_per_mtok: 0.30,
        };
        // anthropic → 1.25x, so the convenience form equals the explicit one.
        let explicit = pricing.cache_savings_usd(&usage(1_000_000, 400_000, 100_000), 3.00 * 0.25);
        let convenient =
            pricing.cache_savings_usd_for("anthropic", &usage(1_000_000, 400_000, 100_000));
        assert!((explicit - convenient).abs() < 1e-12);

        // An implicit-cache provider bills writes at input rate → premium 0;
        // since it reports no writes anyway the reads stand alone.
        let implicit = pricing.cache_savings_usd_for("zai", &usage(1_000_000, 400_000, 0));
        assert!((implicit - (400_000.0 / 1e6 * 2.70)).abs() < 1e-12);
    }

    #[test]
    fn diagnosis_names_opt_in_never_engaged_on_a_zero_hit_multi_turn_session() {
        // The acceptance case: an opt-in provider (Anthropic), N>3 turns, 0%
        // hit, and NOTHING written → the marker never engaged. Discriminated
        // on cache_write_tokens == 0, not on the hit rate alone.
        let cause = diagnose_cache("anthropic", 6, 120_000, 0, 0, 0.20);
        assert_eq!(cause, Some(CacheCause::OptInNeverEngaged));
    }

    #[test]
    fn diagnosis_names_prefix_instability_when_writes_happen_but_reads_do_not() {
        // Same opt-in provider and low hit rate, but the cache WAS written —
        // the marker engaged; the prefix is churning. Must NOT be confused
        // with the opt-in-absent cause.
        let cause = diagnose_cache("anthropic", 6, 120_000, 1_000, 90_000, 0.20);
        assert_eq!(cause, Some(CacheCause::PrefixInstability));
    }

    #[test]
    fn diagnosis_on_implicit_provider_is_prefix_instability_never_opt_in() {
        // An implicit-cache provider (zai) can never have an opt-in-marker
        // bug — a low hit rate there is prefix instability regardless of the
        // (always zero) write count.
        let cause = diagnose_cache("zai", 8, 200_000, 5_000, 0, 0.20);
        assert_eq!(cause, Some(CacheCause::PrefixInstability));
    }

    #[test]
    fn diagnosis_stays_quiet_until_enough_turns_and_only_below_threshold() {
        // Too few turns: no cache established yet, nothing to diagnose.
        assert_eq!(diagnose_cache("anthropic", 3, 50_000, 0, 0, 0.20), None);
        // Healthy hit rate (50% >= 20%): no diagnosis even over many turns.
        assert_eq!(
            diagnose_cache("anthropic", 10, 100_000, 50_000, 10_000, 0.20),
            None
        );
    }

    #[test]
    fn hit_rate_is_zero_on_no_input_and_clamped_to_one() {
        assert_eq!(hit_rate(0, 0), 0.0);
        assert!((hit_rate(1_000, 500) - 0.5).abs() < 1e-12);
        // Defensive clamp: cached over total input never exceeds 1.
        assert!((hit_rate(1_000, 2_000) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn ttl_is_five_minutes_for_the_opt_in_cache_providers_and_none_otherwise() {
        // Anthropic-family default TTL is 5 minutes.
        assert_eq!(provider_cache_ttl_secs("anthropic"), Some(300));
        assert_eq!(provider_cache_ttl_secs("bedrock"), Some(300));
        assert_eq!(provider_cache_ttl_secs("openrouter"), Some(300));
        // Providers with no documented eviction window: nothing to schedule.
        assert_eq!(provider_cache_ttl_secs("zai"), None);
        assert_eq!(provider_cache_ttl_secs("local"), None);
    }

    #[test]
    fn warmth_counts_down_and_saturates_to_expired() {
        // Fresh: full TTL remaining, not expired.
        let warm = CacheWarmth::from_elapsed(0, 300);
        assert_eq!(warm.remaining_secs, 300);
        assert!(!warm.expired);
        // Midway: partial warmth, still live.
        let cooling = CacheWarmth::from_elapsed(120, 300);
        assert_eq!(cooling.remaining_secs, 180);
        assert!(!cooling.expired);
        // At the boundary the prefix is gone (remaining 0 → expired).
        let at_edge = CacheWarmth::from_elapsed(300, 300);
        assert_eq!(at_edge.remaining_secs, 0);
        assert!(at_edge.expired);
        // Idle well past the TTL saturates, never underflows.
        let cold = CacheWarmth::from_elapsed(10_000, 300);
        assert_eq!(cold.remaining_secs, 0);
        assert!(cold.expired);
    }

    #[test]
    fn expired_rewrite_needs_both_a_cold_gap_and_an_actual_write() {
        // Cold gap AND a write → the prefix was re-billed: a rewrite.
        assert!(is_cache_expired_rewrite(600, 40_000, 300));
        // Cold gap but nothing written (a read-only or cache-off turn) → not
        // a rewrite; there was no prefix write to forfeit.
        assert!(!is_cache_expired_rewrite(600, 0, 300));
        // Wrote the cache but well within the TTL → a healthy warm write.
        assert!(!is_cache_expired_rewrite(30, 40_000, 300));
        // Exactly at the boundary is still warm (strict `>`).
        assert!(!is_cache_expired_rewrite(300, 40_000, 300));
    }
}
