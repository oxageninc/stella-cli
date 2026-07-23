//! Retry + backoff for the step-driver. Pure
//! decision logic plus one narrow async driver — `stella-core` has zero I/O
//! of its own, so even the async loop here drives through an injectable
//! [`Sleeper`] port rather than calling `tokio::time::sleep` directly, the
//! same "ports, not concretions" seam as [`crate::ports::Clock`].
//!
//! Binding lessons this module encodes:
//!
//! - **L-M4**: deterministic fast paths (triage/classification) run with
//!   `max_retries = 0` and fall through on the first failure — never hang,
//!   never retry. [`RetryPolicy::deterministic`] is that policy.
//! - **L-M7**: retry classification is never re-derived here. Every decision
//!   defers to [`stella_protocol::ProviderError::is_retryable`], which
//!   providers set at the source.
//! - **L-E10**: speculative side-effect events flush only when the step
//!   commits. Retry history follows that rule, while paid-call accounting is
//!   intentionally stricter: [`retry_with_backoff_observed`] synchronously
//!   exposes every failed dispatched attempt so unknown provider usage can be
//!   persisted even when a later attempt succeeds.
//!
//! Jitter and per-call timeouts (L-E4) are a caller concern layered on top
//! of `attempt_fn`; this module only owns "should we try again, and if so
//! after how long".

use std::future::Future;

use async_trait::async_trait;
use rand::{Rng, RngExt};
use stella_protocol::ProviderError;

/// The delay port `retry_with_backoff` sleeps through between attempts.
/// Injectable so retry-loop tests run instantly and deterministically
/// instead of paying real wall-clock delays — the same seam
/// [`crate::ports::Clock`] provides for reading time, but for the one place
/// this crate needs to actually suspend a task. Only the trait lives here —
/// the production tokio-backed impl belongs to the binary that constructs
/// the engine (the CLI's `runtime` module).
#[async_trait]
pub trait Sleeper: Send + Sync {
    /// Suspend the current task for `duration_ms` milliseconds.
    async fn sleep(&self, duration_ms: u64);
}

/// Retry policy for one model or tool call: how many times to retry a
/// retryable failure, and the backoff envelope between attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum number of retries *after* the initial attempt. `0` means the
    /// call is tried exactly once and never retried (L-M4).
    pub max_retries: u32,
    /// Backoff floor in milliseconds — the delay before the first retry.
    pub base_delay_ms: u64,
    /// Backoff ceiling in milliseconds — no computed or server-hinted delay
    /// is ever slept past this.
    pub max_delay_ms: u64,
}

impl RetryPolicy {
    /// Build a policy from explicit values.
    pub fn new(max_retries: u32, base_delay_ms: u64, max_delay_ms: u64) -> Self {
        Self {
            max_retries,
            base_delay_ms,
            max_delay_ms,
        }
    }

    /// The deterministic fast-path policy (L-M4): `max_retries = 0`. Triage
    /// and classification calls use this so a flaky provider call fails
    /// fast and falls through to the full path instead of adding retry
    /// latency to what's supposed to be the cheap path.
    pub fn deterministic() -> Self {
        Self {
            max_retries: 0,
            base_delay_ms: 0,
            max_delay_ms: 0,
        }
    }

    /// The default policy for ordinary provider calls: up to 3 retries,
    /// 250ms floor, 8s ceiling.
    pub fn standard() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 250,
            max_delay_ms: 8_000,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

/// One retry taken during a [`retry_with_backoff`] call: which attempt
/// failed, why, and how long the driver slept before trying again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryAttempt {
    /// 1-indexed number of the attempt that failed and triggered this
    /// retry (matches `stella_protocol::AgentEvent::Retry::attempt`).
    pub attempt: u32,
    /// The failed attempt's error, rendered via `Display` — this is the
    /// `reason` a caller passes straight into `AgentEvent::Retry`.
    pub reason: String,
    /// Milliseconds slept before the next attempt.
    pub delay_ms: u64,
}

/// The result of a [`retry_with_backoff`] call that ultimately committed —
/// `attempt_fn` returned `Ok`, whether on the first try or after some
/// retries.
#[derive(Debug, Clone)]
pub struct RetryOutcome<T> {
    /// The successful value returned by `attempt_fn`.
    pub value: T,
    /// Total number of calls made to `attempt_fn` (1 if it succeeded on the
    /// first try).
    pub attempts: u32,
    /// One entry per retry taken, in order. Empty if the call succeeded on
    /// its first attempt. The caller emits one `AgentEvent::Retry` per
    /// entry (see the module doc's L-E10 note).
    pub retries: Vec<RetryAttempt>,
}

/// Compute the jittered exponential backoff delay before the next retry,
/// given the attempt number that just failed (`0`-indexed: `0` means the
/// very first call failed and this is the delay before retry #1).
///
/// Shape: `base_delay_ms * 2^attempt`, capped at `max_delay_ms`. The result
/// is drawn uniformly from `[base_delay_ms, cap]` — "equal jitter" rather
/// than full jitter down to zero: a retry should still wait at least the
/// configured floor before hammering the provider again, and callers get a
/// hard lower bound to assert against. Because the draw is uniform across
/// that range, concurrent callers retrying the same failure land on
/// different points in it instead of all waking at the same instant
/// (thundering herd).
///
/// Pure and synchronous by design — the only
/// non-determinism is the injected `rng`, so bounds and shape are directly
/// assertable without sleeping.
pub fn compute_backoff_delay_ms(policy: &RetryPolicy, attempt: u32, rng: &mut impl Rng) -> u64 {
    let base = policy.base_delay_ms;
    let cap = policy.max_delay_ms.max(base);
    let exponential = base.saturating_mul(2u64.saturating_pow(attempt));
    let high = exponential.min(cap);

    if high <= base {
        // Degenerate range (attempt 0, or a misconfigured policy where the
        // cap doesn't exceed the floor): nothing to jitter, return the
        // floor rather than call `rng.random_range` on an empty range.
        return base;
    }
    rng.random_range(base..=high)
}

/// Drive `attempt_fn` to completion, retrying retryable
/// (`ProviderError::is_retryable`) failures with jittered exponential
/// backoff up to `policy.max_retries`. A terminal error — or a retryable
/// one once retries are exhausted — returns immediately as `Err`; this
/// function never re-derives retry classification, it only ever asks
/// `is_retryable()`.
///
/// A `RateLimited` error's `retry_after_ms` hint, when present, is honored
/// verbatim (still capped at `policy.max_delay_ms`) instead of the computed
/// jittered delay — respecting a server's stated backoff beats guessing at
/// one.
///
/// On success, [`RetryOutcome::retries`] carries the full retry history so
/// the caller (`driver.rs`) can walk it and emit one
/// `stella_protocol::AgentEvent::Retry { attempt, reason }` per entry —
/// `retry.rs` has no event channel of its own, so this is returned as data,
/// never raised as a side effect. On failure, only the terminal
/// `ProviderError` comes back: per the module doc's L-E10 note, a call that
/// never commits speaks through its failure alone, not through the doomed
/// attempts leading up to it.
pub async fn retry_with_backoff<F, Fut, T>(
    policy: &RetryPolicy,
    sleeper: &dyn Sleeper,
    attempt_fn: F,
) -> Result<RetryOutcome<T>, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    retry_with_backoff_observed(policy, sleeper, attempt_fn, |_, _, _| {}).await
}

/// Retry while synchronously observing every failed dispatched attempt.
///
/// Accounting callers use this hook to emit a durable, content-free usage
/// incompleteness envelope before any later attempt can succeed. A successful
/// retry can report its own usage, but can never make an earlier provider
/// attempt's unknown usage knowable after the fact.
///
/// The observer receives the 1-indexed attempt number, the error, and THAT
/// attempt's own elapsed duration — never a cumulative figure across earlier
/// attempts or the backoff sleeps between them, so a consumer characterizing
/// provider failure latency from the durable envelopes sees per-call truth.
pub(crate) async fn retry_with_backoff_observed<F, Fut, T, O>(
    policy: &RetryPolicy,
    sleeper: &dyn Sleeper,
    mut attempt_fn: F,
    mut observe_failure: O,
) -> Result<RetryOutcome<T>, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
    O: FnMut(u32, &ProviderError, std::time::Duration),
{
    let mut retries = Vec::new();
    let mut attempt: u32 = 0;
    loop {
        let attempt_started = std::time::Instant::now();
        match attempt_fn().await {
            Ok(value) => {
                return Ok(RetryOutcome {
                    value,
                    attempts: attempt + 1,
                    retries,
                });
            }
            Err(error) => {
                observe_failure(attempt + 1, &error, attempt_started.elapsed());
                if !error.is_retryable() || attempt >= policy.max_retries {
                    return Err(error);
                }

                let delay_ms = match &error {
                    ProviderError::RateLimited {
                        retry_after_ms: Some(hint),
                        ..
                    } => (*hint).min(policy.max_delay_ms),
                    _ => compute_backoff_delay_ms(policy, attempt, &mut rand::rng()),
                };

                retries.push(RetryAttempt {
                    attempt: attempt + 1,
                    reason: error.to_string(),
                    delay_ms,
                });

                sleeper.sleep(delay_ms).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use rand::SeedableRng;
    use rand::rngs::StdRng;

    use super::*;

    /// A [`Sleeper`] that never actually waits — it just records every
    /// requested delay so async-loop tests can assert on retry timing
    /// without paying real wall-clock cost or flaking under load.
    #[derive(Default)]
    struct NoopSleeper {
        delays_ms: Mutex<Vec<u64>>,
    }

    #[async_trait]
    impl Sleeper for NoopSleeper {
        async fn sleep(&self, duration_ms: u64) {
            self.delays_ms
                .lock()
                .expect("mutex poisoned")
                .push(duration_ms);
        }
    }

    // ---- RetryPolicy ----------------------------------------------------

    #[test]
    fn deterministic_policy_never_retries() {
        assert_eq!(RetryPolicy::deterministic().max_retries, 0);
    }

    #[test]
    fn default_policy_is_standard() {
        assert_eq!(RetryPolicy::default(), RetryPolicy::standard());
    }

    // ---- compute_backoff_delay_ms (pure, no sleeping) --------------------

    #[test]
    fn first_retry_delay_equals_the_base_when_no_jitter_room() {
        // attempt 0: base * 2^0 == base, so the jitter range is degenerate
        // and the result is deterministic.
        let policy = RetryPolicy::new(5, 250, 8_000);
        let mut rng = StdRng::seed_from_u64(1);
        assert_eq!(compute_backoff_delay_ms(&policy, 0, &mut rng), 250);
    }

    #[test]
    fn delay_stays_within_base_and_cap_bounds_across_many_attempts() {
        let policy = RetryPolicy::new(10, 100, 5_000);
        let mut rng = StdRng::seed_from_u64(7);
        for attempt in 0..30 {
            let delay = compute_backoff_delay_ms(&policy, attempt, &mut rng);
            assert!(
                (policy.base_delay_ms..=policy.max_delay_ms).contains(&delay),
                "attempt {attempt}: delay {delay} out of [{}, {}]",
                policy.base_delay_ms,
                policy.max_delay_ms
            );
        }
    }

    #[test]
    fn delay_grows_with_attempt_number_up_to_the_cap() {
        let policy = RetryPolicy::new(10, 50, 4_000);
        let mut rng = StdRng::seed_from_u64(42);
        // Use the upper bound of what's achievable at each attempt (the
        // exponential envelope) rather than one jittered sample, since a
        // single draw can dip low even as the ceiling climbs.
        let envelope = |attempt: u32| -> u64 {
            (policy
                .base_delay_ms
                .saturating_mul(2u64.saturating_pow(attempt)))
            .min(policy.max_delay_ms)
        };
        let mut previous = envelope(0);
        for attempt in 1..8 {
            let current = envelope(attempt);
            assert!(
                current >= previous,
                "envelope must be non-decreasing: attempt {attempt} gave {current} < {previous}"
            );
            previous = current;
        }
        // And the ceiling is actually reached for a large attempt number.
        assert_eq!(envelope(20), policy.max_delay_ms);
        // Sanity: real samples at a late attempt never exceed the cap.
        for _ in 0..20 {
            assert!(compute_backoff_delay_ms(&policy, 20, &mut rng) <= policy.max_delay_ms);
        }
    }

    #[test]
    fn delay_varies_across_calls_at_the_same_attempt() {
        // The jitter must be real, not a constant disguised as random: a
        // wide [base, cap] range sampled many times at a fixed attempt
        // should not collapse to a single value.
        let policy = RetryPolicy::new(10, 100, 10_000);
        let mut rng = StdRng::seed_from_u64(99);
        let samples: Vec<u64> = (0..50)
            .map(|_| compute_backoff_delay_ms(&policy, 5, &mut rng))
            .collect();
        let first = samples[0];
        assert!(
            samples.iter().any(|&d| d != first),
            "expected variance in jittered delay, got {samples:?}"
        );
    }

    #[test]
    fn zero_policy_never_panics_and_returns_zero() {
        let policy = RetryPolicy::deterministic();
        let mut rng = StdRng::seed_from_u64(3);
        assert_eq!(compute_backoff_delay_ms(&policy, 0, &mut rng), 0);
        assert_eq!(compute_backoff_delay_ms(&policy, 7, &mut rng), 0);
    }

    #[test]
    fn misconfigured_cap_below_base_does_not_panic() {
        // cap < base is a misconfiguration, but must degrade safely rather
        // than panic the sampler on an empty range.
        let policy = RetryPolicy::new(3, 5_000, 100);
        let mut rng = StdRng::seed_from_u64(11);
        let delay = compute_backoff_delay_ms(&policy, 4, &mut rng);
        assert_eq!(delay, policy.base_delay_ms);
    }

    proptest::proptest! {
        #[test]
        fn backoff_delay_always_within_base_and_cap(
            base in 0u64..2_000,
            extra in 0u64..10_000,
            attempt in 0u32..40,
        ) {
            let policy = RetryPolicy::new(10, base, base + extra);
            let mut rng = StdRng::seed_from_u64(u64::from(attempt) ^ base ^ extra);
            let delay = compute_backoff_delay_ms(&policy, attempt, &mut rng);
            proptest::prop_assert!(delay >= policy.base_delay_ms);
            proptest::prop_assert!(delay <= policy.max_delay_ms);
        }
    }

    // ---- retry_with_backoff (async loop, NoopSleeper — no real waiting) -

    #[tokio::test]
    async fn max_retries_zero_never_retries_even_on_a_retryable_error() {
        let policy = RetryPolicy::deterministic();
        let sleeper = NoopSleeper::default();
        let calls = AtomicU32::new(0);

        let result = retry_with_backoff(&policy, &sleeper, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(ProviderError::Transport("timeout".into())) }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "must call attempt_fn exactly once"
        );
        assert!(
            sleeper.delays_ms.lock().unwrap().is_empty(),
            "must never sleep when max_retries == 0"
        );
    }

    #[tokio::test]
    async fn terminal_errors_never_retry_regardless_of_policy() {
        let policy = RetryPolicy::new(5, 1, 5);
        let sleeper = NoopSleeper::default();
        let calls = AtomicU32::new(0);

        let result = retry_with_backoff(&policy, &sleeper, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(ProviderError::Auth("bad key".into())) }
        })
        .await;

        assert!(matches!(result, Err(ProviderError::Auth(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(sleeper.delays_ms.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn retryable_errors_retry_up_to_the_cap_then_surface_the_last_error() {
        let policy = RetryPolicy::new(3, 1, 5);
        let sleeper = NoopSleeper::default();
        let calls = AtomicU32::new(0);

        let result = retry_with_backoff(&policy, &sleeper, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { Err::<(), _>(ProviderError::Transport(format!("attempt {n} failed"))) }
        })
        .await;

        // 1 initial attempt + 3 retries == 4 total calls.
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        match result {
            Err(ProviderError::Transport(msg)) => assert!(msg.contains("attempt 3")),
            other => panic!("expected the LAST transport error, got {other:?}"),
        }
        assert_eq!(
            sleeper.delays_ms.lock().unwrap().len(),
            3,
            "one sleep per retry"
        );
    }

    #[tokio::test]
    async fn succeeds_after_retries_and_reports_full_history() {
        let policy = RetryPolicy::new(5, 1, 5);
        let sleeper = NoopSleeper::default();
        let calls = AtomicU32::new(0);

        let outcome = retry_with_backoff(&policy, &sleeper, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(ProviderError::Transport(format!("flaky {n}")))
                } else {
                    Ok(42)
                }
            }
        })
        .await
        .expect("should eventually succeed");

        assert_eq!(outcome.value, 42);
        assert_eq!(outcome.attempts, 3);
        assert_eq!(outcome.retries.len(), 2);
        assert_eq!(outcome.retries[0].attempt, 1);
        assert!(outcome.retries[0].reason.contains("flaky 0"));
        assert_eq!(outcome.retries[1].attempt, 2);
        assert!(outcome.retries[1].reason.contains("flaky 1"));
    }

    #[tokio::test]
    async fn succeeds_on_the_first_try_reports_no_retries() {
        let policy = RetryPolicy::standard();
        let sleeper = NoopSleeper::default();

        let outcome =
            retry_with_backoff(&policy, &sleeper, || async { Ok::<_, ProviderError>("ok") })
                .await
                .expect("first try succeeds");

        assert_eq!(outcome.attempts, 1);
        assert!(outcome.retries.is_empty());
        assert!(sleeper.delays_ms.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rate_limited_retry_after_hint_is_honored_and_capped() {
        let policy = RetryPolicy::new(2, 100, 1_000);
        let sleeper = NoopSleeper::default();
        let calls = AtomicU32::new(0);

        let _ = retry_with_backoff(&policy, &sleeper, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(ProviderError::RateLimited {
                        message: "slow down".into(),
                        retry_after_ms: Some(50_000), // far above the policy cap
                    })
                } else {
                    Ok(())
                }
            }
        })
        .await;

        let delays = sleeper.delays_ms.lock().unwrap();
        assert_eq!(delays.len(), 1);
        assert_eq!(delays[0], 1_000, "hint must be capped at max_delay_ms");
    }
}
