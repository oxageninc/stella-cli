//! Prompt-cache economics — the shared *data* vocabulary for turning the
//! `CompletionUsage` cache counters (`cached_input_tokens`,
//! `cache_write_tokens`) into hit rate, dollars saved, and a probable-cause
//! hint when the hit rate is abnormally low.
//!
//! Zero policy lives here: which provider prices a cache write at what
//! premium, and which provider expires a prefix after what TTL, are decided
//! upstream (`stella-model`'s pricing/parity data, `stella-fleet`'s TTL
//! table) and the *results* flow to every surface — the `stella stats`
//! receipt, the deck's cache panel — as the plain data below. The selection
//! logic that picks a [`CacheCause`] lives in
//! `stella_model::cache_economics::diagnose_cache`; this module only names
//! the causes and the one-line hint each carries, so the CLI and the TUI
//! render identical wording.

use serde::{Deserialize, Serialize};

/// The probable cause of a low prompt-cache hit rate on a multi-turn session.
///
/// A named cause is only meaningful once a session has enough turns to have
/// *established* a cache to hit (the diagnosis gates on that); the variants
/// below are the three failure modes worth calling out, in the order the
/// diagnosis discriminates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheCause {
    /// The provider caches only when the adapter sends an explicit opt-in
    /// marker (Anthropic `cache_control`, Bedrock `cachePoint`, OpenRouter's
    /// Claude routes) and this session wrote *nothing* to the cache across
    /// several turns — the marker never reached the wire. This is the
    /// motivating incident: a whole session billed at the full input rate
    /// with the cache stat pinned at 0%.
    OptInNeverEngaged,
    /// The session *did* write to the cache but reads almost never landed —
    /// the stable prompt prefix is changing between turns (a violation of the
    /// byte-stable-prompt invariant), so each turn re-writes the prefix
    /// instead of reading the previous one.
    PrefixInstability,
    /// Writes happened but the gaps between turns ran longer than the
    /// provider's cache TTL, so the prefix expired before the next turn could
    /// read it. Distinct from [`Self::PrefixInstability`]: the prompt is
    /// stable, the scheduler simply parked the session too long (the failure
    /// mode cache-TTL-aware scheduling exists to prevent).
    IdleBeyondTtl,
}

impl CacheCause {
    /// The one-line probable-cause hint, kept here so the CLI receipt and the
    /// deck panel print byte-identical wording.
    pub fn hint(self) -> &'static str {
        match self {
            Self::OptInNeverEngaged => {
                "cache opt-in never engaged — this provider needs an explicit cache marker \
                 and none was sent (likely a bug); every token billed at the full input rate"
            }
            Self::PrefixInstability => {
                "prompt prefix is unstable between turns — the cached prefix is being \
                 rewritten each turn instead of reused (byte-stable-prompt invariant)"
            }
            Self::IdleBeyondTtl => {
                "idle gaps exceeded the provider cache TTL — the prefix expired before the \
                 next turn could read it; resume warm sessions sooner"
            }
        }
    }

    /// The stable machine token (matches the serde `snake_case` wire form) —
    /// for the `stella stats --format json|csv` receipts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OptInNeverEngaged => "opt_in_never_engaged",
            Self::PrefixInstability => "prefix_instability",
            Self::IdleBeyondTtl => "idle_beyond_ttl",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cause_roundtrips_through_json_as_snake_case() {
        for cause in [
            CacheCause::OptInNeverEngaged,
            CacheCause::PrefixInstability,
            CacheCause::IdleBeyondTtl,
        ] {
            let json = serde_json::to_string(&cause).expect("serialize");
            assert_eq!(json, format!("\"{}\"", cause.as_str()));
            let back: CacheCause = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, cause);
        }
    }

    #[test]
    fn every_cause_carries_a_nonempty_hint() {
        for cause in [
            CacheCause::OptInNeverEngaged,
            CacheCause::PrefixInstability,
            CacheCause::IdleBeyondTtl,
        ] {
            assert!(!cause.hint().is_empty(), "{cause:?} needs a hint");
        }
    }
}
