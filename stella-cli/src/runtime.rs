//! Production implementations of `stella-core`'s time ports. The engine
//! exports only the [`Clock`] and [`Sleeper`] traits (ports, not
//! concretions — `02-architecture.md` §3), so the binary owns the concrete
//! wall-clock/tokio impls and wires them at construction. This is what
//! keeps `stella-core` free of production `tokio::time` calls.

use async_trait::async_trait;
use stella_core::ports::Clock;
use stella_core::retry::Sleeper;

/// The production clock: monotonic milliseconds since construction.
pub struct SystemClock {
    origin: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

/// The production [`Sleeper`]: a thin wrapper over `tokio::time::sleep`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration_ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_starts_near_zero_and_advances_monotonically() {
        let clock = SystemClock::new();
        let first = clock.now_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = clock.now_ms();
        assert!(second >= first, "clock must never go backwards");
        assert!(
            second - first >= 4,
            "clock must actually advance with wall time"
        );
    }

    #[test]
    fn default_constructs_a_fresh_clock() {
        let clock = SystemClock::default();
        assert!(
            clock.now_ms() < 1000,
            "a freshly constructed clock starts near zero"
        );
    }
}
