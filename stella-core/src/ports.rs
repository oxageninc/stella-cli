//! The engine's port boundary (`02-architecture.md` §3). `stella-core`
//! never imports a provider SDK, a filesystem call, or a terminal library —
//! it drives through these traits, mirroring the TS engine's `ports.ts`.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::{ToolOutput, ToolSchema};

/// Executes one tool call. Implemented by `stella-tools::ToolRegistry` (and
/// by test doubles). The engine treats it as a black box that never panics.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Schemas advertised to the model.
    fn schemas(&self) -> Vec<ToolSchema>;

    /// Execute a tool by name. Unknown names return an error `ToolOutput`,
    /// never an Err — tool failures are model-visible data, not engine
    /// failures.
    async fn execute(&self, name: &str, input: &Value) -> ToolOutput;
}

/// A read-only view over another executor: advertises only the schemas
/// marked `read_only` and refuses to execute anything else. This is how a
/// judge gets real evidence-gathering power (read files, grep, check
/// saved explorations) with a structural guarantee it cannot mutate the
/// workspace it is judging — the restriction is enforced at execution
/// time, not just by prompt.
pub struct ReadOnlyTools<'a> {
    inner: &'a dyn ToolExecutor,
}

impl<'a> ReadOnlyTools<'a> {
    pub fn new(inner: &'a dyn ToolExecutor) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ToolExecutor for ReadOnlyTools<'_> {
    fn schemas(&self) -> Vec<ToolSchema> {
        self.inner
            .schemas()
            .into_iter()
            .filter(|s| s.read_only)
            .collect()
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        let allowed = self
            .inner
            .schemas()
            .iter()
            .any(|s| s.name == name && s.read_only);
        if !allowed {
            return ToolOutput::Error {
                message: format!(
                    "`{name}` is not available here: this context is read-only (verification/\
                     judging) and may only use read-only tools"
                ),
            };
        }
        self.inner.execute(name, input).await
    }
}

/// Time source, injectable for deterministic tests.
pub trait Clock: Send + Sync {
    /// Monotonic milliseconds since an arbitrary epoch.
    fn now_ms(&self) -> u64;
}

/// The production clock.
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
