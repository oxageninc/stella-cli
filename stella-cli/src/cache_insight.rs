//! Derive [`stella_tui::Inbound::CacheInsight`] from a committed
//! `AgentEvent::StepUsage` (issues #267/#269) — split out of
//! `command_deck.rs` (already over its file-size ratchet; not a file to
//! grow) so `spawn_forwarder`, the one seam shared by every deck lane (the
//! lead's turns and every `crate::subsession` worker), only needs a call
//! site.
//!
//! `StepUsage` already carries the raw token counts the deck folds into the
//! CACHE cell; this adds the three figures that need list pricing, the TTL
//! table, and the cache-posture taxonomy — `stella-model` concerns the
//! model-tier-free `stella-tui` cannot reach — computed once here from the
//! provider id the forwarder already owns. Without this seam the
//! SAVED/WARMTH statline cells are permanently "—" outside a hand-built test
//! fixture: `Inbound::CacheInsight` had no producer anywhere on the live
//! path. `is_opt_in_provider` similarly lets
//! [`stella_tui::AgentEntry::cache_diagnosis`] name
//! `CacheCause::OptInNeverEngaged` without the deck knowing which providers
//! require an explicit cache marker.

use stella_model::provider_parity::{CachePosture, cache_posture};
use stella_model::{Catalog, provider_cache_ttl_secs};
use stella_protocol::{AgentEvent, CompletionUsage};
use stella_tui::Inbound;

/// `None` for every event variant but `StepUsage`. An unresolvable
/// `(provider, model)` — a retired or custom catalog entry — reports `0.0`
/// savings rather than dropping the insight: the TTL half (from the
/// provider id alone) still stands, and a silent $0 delta is honest where a
/// missing warmth countdown would not be.
pub(crate) fn cache_insight_for(
    provider_id: &str,
    lane: &str,
    event: &AgentEvent,
) -> Option<Inbound> {
    let AgentEvent::StepUsage {
        model,
        input_tokens,
        output_tokens,
        cached_input_tokens,
        cache_write_tokens,
        complete,
        ..
    } = event
    else {
        return None;
    };
    let usage = CompletionUsage {
        reported: *complete,
        input_tokens: *input_tokens,
        output_tokens: *output_tokens,
        cached_input_tokens: *cached_input_tokens,
        cache_write_tokens: *cache_write_tokens,
    };
    let savings_usd_delta = Catalog::current()
        .resolve_for(provider_id, model)
        .map(|entry| entry.pricing.cache_savings_usd_for(provider_id, &usage))
        .unwrap_or(0.0);
    let is_opt_in_provider = matches!(cache_posture(provider_id), Some(CachePosture::OptIn { .. }));
    Some(Inbound::CacheInsight {
        agent: lane.to_string(),
        savings_usd_delta,
        ttl_secs: provider_cache_ttl_secs(provider_id).unwrap_or(0),
        is_opt_in_provider,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step_usage(model: &str, input: u64, cached: u64, write: u64) -> AgentEvent {
        AgentEvent::StepUsage {
            step: 1,
            role: stella_protocol::event::ModelCallRole::Worker,
            provider: "test".into(),
            model: model.to_string(),
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

    #[test]
    fn ignores_every_non_step_usage_event() {
        let text = AgentEvent::Text { delta: "hi".into() };
        assert!(cache_insight_for("anthropic", "lead", &text).is_none());
    }

    #[test]
    fn matches_the_pricing_witness_from_stella_model() {
        // Same figures stella-model's own `savings_matches_catalog_pricing_math_by_hand`
        // witness uses: Claude Fable 5 seed pricing (input $3.00/M, cached
        // $0.30/M), 400k cached reads and 100k writes on a 1M-input call ->
        // $1.005 saved net of the 1.25x anthropic write premium.
        let event = step_usage("claude-fable-5", 1_000_000, 400_000, 100_000);
        let insight =
            cache_insight_for("anthropic", "lead", &event).expect("StepUsage yields an insight");
        match insight {
            Inbound::CacheInsight {
                agent,
                savings_usd_delta,
                ttl_secs,
                is_opt_in_provider,
            } => {
                assert_eq!(agent, "lead");
                assert!(
                    (savings_usd_delta - 1.005).abs() < 1e-9,
                    "got {savings_usd_delta}"
                );
                // Anthropic's default prompt-cache TTL is 5 minutes.
                assert_eq!(ttl_secs, 300);
                // Anthropic requires the explicit cache_control marker.
                assert!(is_opt_in_provider);
            }
            other => panic!("expected CacheInsight, got {other:?}"),
        }
    }

    #[test]
    fn implicit_provider_is_not_marked_opt_in() {
        // zai auto-caches with no marker — the opt-in-never-engaged diagnosis
        // must never fire for it, however low the hit rate runs.
        let event = step_usage("glm-5.2", 1_000_000, 0, 0);
        let insight =
            cache_insight_for("zai", "lead", &event).expect("StepUsage yields an insight");
        match insight {
            Inbound::CacheInsight {
                is_opt_in_provider, ..
            } => assert!(!is_opt_in_provider),
            other => panic!("expected CacheInsight, got {other:?}"),
        }
    }

    #[test]
    fn reports_zero_savings_for_an_unresolvable_model_but_keeps_the_ttl() {
        // A retired/custom slug the catalog cannot price: savings is an
        // honest $0 (never a guess), but the TTL still resolves from the
        // provider id alone, so the warmth countdown does not go dark too.
        let event = step_usage("made-up-model-9000", 1_000_000, 400_000, 100_000);
        let insight =
            cache_insight_for("anthropic", "lead", &event).expect("StepUsage yields an insight");
        match insight {
            Inbound::CacheInsight {
                savings_usd_delta,
                ttl_secs,
                ..
            } => {
                assert_eq!(savings_usd_delta, 0.0);
                assert_eq!(ttl_secs, 300);
            }
            other => panic!("expected CacheInsight, got {other:?}"),
        }
    }

    #[test]
    fn ttl_is_zero_for_a_provider_with_no_documented_cache_window() {
        let event = step_usage("glm-5.2", 1_000_000, 400_000, 0);
        let insight =
            cache_insight_for("zai", "lead", &event).expect("StepUsage yields an insight");
        match insight {
            Inbound::CacheInsight { ttl_secs, .. } => assert_eq!(ttl_secs, 0),
            other => panic!("expected CacheInsight, got {other:?}"),
        }
    }
}
