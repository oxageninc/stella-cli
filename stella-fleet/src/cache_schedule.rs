//! Cache-TTL-aware fleet scheduling (issue #269).
//!
//! When the fleet parks a session mid-conversation for longer than its
//! provider's prompt-cache TTL — waiting on a judge, a queue slot, or a human
//! gate — the cached prefix expires and the next turn re-writes it at up to
//! 1.25x input price. At fleet scale that is a recurring, invisible tax, and
//! today the scheduler is TTL-blind. This module is the *decision* half: among
//! runnable sessions of equal priority, prefer the one whose cache expires
//! soonest ("warmest-first"), so a session about to lose its prefix is resumed
//! before it goes cold.
//!
//! Pure over passed-in state, exactly like [`crate::plan`]: warmth is computed
//! upstream (`stella_model::CacheWarmth`, from each session's last provider call
//! and the provider TTL) and handed in as `warmth_secs`. No clock read, no I/O
//! — every ordering property is table-checkable. `stella-fleet` deliberately
//! does not depend on `stella-model`, keeping the pricing/TTL *policy* on one
//! side of the crate boundary and the scheduling *heuristic* on the other.

/// A runnable session the scheduler is choosing among. `priority` is the
/// caller's existing dispatch priority (higher runs first, matching the
/// wave-scheduler's convention); `warmth_secs` is the seconds until this
/// session's prompt cache expires — `None` means there is no warm prefix to
/// preserve (a provider with no TTL, or a session that has not yet called the
/// model). `id` is opaque; the caller maps it back to its own session handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnableSession {
    pub id: String,
    pub priority: u8,
    pub warmth_secs: Option<u64>,
}

impl RunnableSession {
    /// Convenience constructor for a session with a live warm prefix.
    pub fn warm(id: impl Into<String>, priority: u8, warmth_secs: u64) -> Self {
        Self {
            id: id.into(),
            priority,
            warmth_secs: Some(warmth_secs),
        }
    }

    /// Convenience constructor for a session with no warm prefix to preserve.
    pub fn cold(id: impl Into<String>, priority: u8) -> Self {
        Self {
            id: id.into(),
            priority,
            warmth_secs: None,
        }
    }
}

/// Sort key for warmth: a live prefix sorts by seconds-to-expiry ascending
/// (soonest-to-expire first); `None` (no prefix to preserve) sorts after every
/// live one. `u64::MAX` as the sentinel keeps the comparison total and
/// branch-free.
fn warmth_key(warmth_secs: Option<u64>) -> u64 {
    warmth_secs.unwrap_or(u64::MAX)
}

/// Order `runnable` sessions for dispatch: higher `priority` first, then —
/// within an equal-priority class — **warmest-first**, i.e. the session whose
/// cache expires soonest (smallest `warmth_secs`) goes first, so its prefix is
/// resumed before the TTL forfeits it. Sessions with no warm prefix
/// (`warmth_secs: None`) sort last within their class (nothing to preserve),
/// and `id` is the final deterministic tie-break so the order is total and
/// stable. Does not mutate its input; returns borrows in dispatch order.
///
/// Priority strictly dominates warmth: a hotter low-priority session never
/// jumps ahead of a colder high-priority one — the heuristic only reorders
/// *ties*, never overrides the caller's priority.
pub fn warmest_first(runnable: &[RunnableSession]) -> Vec<&RunnableSession> {
    let mut ordered: Vec<&RunnableSession> = runnable.iter().collect();
    ordered.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| warmth_key(a.warmth_secs).cmp(&warmth_key(b.warmth_secs)))
            .then_with(|| a.id.cmp(&b.id))
    });
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmest_first_orders_an_equal_priority_class_soonest_expiry_first() {
        let sessions = vec![
            RunnableSession::warm("a", 0, 300),
            RunnableSession::warm("b", 0, 60),
            RunnableSession::warm("c", 0, 120),
        ];
        let order: Vec<&str> = warmest_first(&sessions)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        // b (60s to expiry) is warmest-about-to-cool → first; a (300s) last.
        assert_eq!(order, ["b", "c", "a"]);
    }

    #[test]
    fn priority_dominates_warmth() {
        // A cold-but-high-priority session must still outrank a warm low one:
        // the heuristic only reorders within a priority class.
        let sessions = vec![
            RunnableSession::warm("low_warm", 0, 5),
            RunnableSession::warm("high_cool", 9, 250),
        ];
        let order: Vec<&str> = warmest_first(&sessions)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(order, ["high_cool", "low_warm"]);
    }

    #[test]
    fn sessions_without_a_warm_prefix_sort_last_within_their_class() {
        let sessions = vec![
            RunnableSession::cold("no_cache_1", 0),
            RunnableSession::warm("warm", 0, 200),
            RunnableSession::cold("no_cache_2", 0),
        ];
        let order: Vec<&str> = warmest_first(&sessions)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        // The warm session leads; the two cold ones follow in id order.
        assert_eq!(order, ["warm", "no_cache_1", "no_cache_2"]);
    }

    #[test]
    fn ordering_is_total_and_stable_via_the_id_tie_break() {
        // Same priority AND same warmth → deterministic id order, so the
        // dispatch sequence never depends on input order.
        let sessions = vec![
            RunnableSession::warm("z", 0, 100),
            RunnableSession::warm("a", 0, 100),
            RunnableSession::warm("m", 0, 100),
        ];
        let order: Vec<&str> = warmest_first(&sessions)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(order, ["a", "m", "z"]);
    }

    /// Simulate a serial-capacity fleet (one dispatch slot per step): every
    /// session is parked mid-conversation and must be resumed once. A session
    /// not picked this step ages by `step_secs`; when finally resumed, if its
    /// warmth has run out it re-writes an expired prefix (a
    /// `cache_expired_rewrite`). `order` picks the resume sequence.
    fn expired_rewrites(
        initial_warmth: &[(&str, u64)],
        step_secs: u64,
        order: impl Fn(&[RunnableSession]) -> Vec<String>,
    ) -> usize {
        let sessions: Vec<RunnableSession> = initial_warmth
            .iter()
            .map(|(id, w)| RunnableSession::warm(*id, 0, *w))
            .collect();
        let sequence = order(&sessions);
        let warmth: std::collections::HashMap<&str, u64> =
            initial_warmth.iter().map(|(id, w)| (*id, *w)).collect();
        sequence
            .iter()
            .enumerate()
            .filter(|(slot, id)| {
                // Idle accrued before this session got its slot.
                let idled = *slot as u64 * step_secs;
                warmth[id.as_str()] <= idled
            })
            .count()
    }

    #[test]
    fn warmest_first_forfeits_fewer_prefixes_than_a_ttl_blind_baseline() {
        // Four equal-priority sessions parked with varied remaining warmth;
        // one resume slot every 90s.
        let fleet = [("s0", 300), ("s1", 60), ("s2", 120), ("s3", 30)];
        let step = 90;

        // TTL-blind baseline: resume in registration (id) order.
        let blind = expired_rewrites(&fleet, step, |sessions| {
            let mut ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
            ids.sort();
            ids
        });

        // Warmest-first: resume soonest-to-expire first.
        let warm = expired_rewrites(&fleet, step, |sessions| {
            warmest_first(sessions)
                .iter()
                .map(|s| s.id.clone())
                .collect()
        });

        // Recorded for the PR: warmest-first forfeits fewer prefixes.
        assert_eq!(blind, 3, "ttl-blind baseline forfeits 3 of 4 prefixes");
        assert_eq!(warm, 2, "warmest-first forfeits 2 of 4 prefixes");
        assert!(
            warm < blind,
            "warmest-first must drop rewrite volume vs the TTL-blind baseline (warm {warm} < blind {blind})"
        );
    }
}
