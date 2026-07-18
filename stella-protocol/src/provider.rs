//! The `Provider` port. Lives in `stella-protocol`,
//! not `stella-model`, so `stella-core` can drive every model call through
//! `&dyn Provider` without depending on any concrete adapter — `stella-model`
//! depends on this crate to implement the trait, never the reverse.

use async_trait::async_trait;

use crate::completion::{CompletionRequest, CompletionResult};
use crate::error::ProviderError;

/// One model provider adapter. `stella-core` drives every call through
/// `&dyn Provider` — no adapter-specific code ever lives outside
/// `stella-model`.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable id for this provider instance, e.g. `"zai"` or `"anthropic"`.
    fn id(&self) -> &str;

    /// Run one completion end-to-end (streams internally, aggregates the
    /// result). Returns a typed, retry-classified error on failure — never
    /// panics on a malformed/erroring HTTP response.
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResult, ProviderError>;
}
