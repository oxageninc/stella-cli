//! The `Provider` port. Lives in `stella-protocol`,
//! not `stella-model`, so `stella-core` can drive every model call through
//! `&dyn Provider` without depending on any concrete adapter — `stella-model`
//! depends on this crate to implement the trait, never the reverse.

use async_trait::async_trait;

use crate::completion::{CompletionRequest, CompletionResult};
use crate::error::ProviderError;
use crate::tool::ToolCall;

/// Observes tool calls as their blocks finish streaming, while the rest of
/// the completion is still in flight. This is the seam speculative tool
/// execution hangs on: `stella-core` hands an observer to
/// [`Provider::complete_observed`] and begins executing *read-only* calls
/// the moment they are announced, instead of waiting for the full response.
///
/// Strictly advisory: the definitive tool-call list is the returned
/// [`CompletionResult`] — an adapter may announce all, some, or none of the
/// calls it will return, but every announced call MUST be byte-identical
/// (same `call_id`, `name`, and parsed `input`) to the one in the final
/// result, because consumers match announced work back by exact equality.
/// An adapter must never announce a call whose input failed to parse.
pub trait ToolCallObserver: Send + Sync {
    /// One tool call's block has fully streamed: id, name, and complete,
    /// well-formed input are known.
    fn tool_call_streamed(&self, call: &ToolCall);

    /// One fragment of user-visible answer text arrived on the stream, in
    /// order. Only answer text — never thinking/reasoning content — and
    /// strictly best-effort: the definitive text is `CompletionResult::text`
    /// (a retried attempt re-streams from the start, and an adapter without
    /// mid-stream visibility calls this not at all). Default no-op so
    /// existing observers compile unchanged.
    fn text_delta(&self, delta: &str) {
        let _ = delta;
    }
}

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

    /// [`Provider::complete`], additionally announcing each tool call to
    /// `observer` as its block finishes streaming (see [`ToolCallObserver`]).
    /// The default ignores the observer and delegates to `complete`, so an
    /// adapter without mid-stream visibility keeps exactly its old behavior
    /// — the engine simply gets no speculation from it.
    async fn complete_observed(
        &self,
        req: CompletionRequest,
        observer: &dyn ToolCallObserver,
    ) -> Result<CompletionResult, ProviderError> {
        let _ = observer;
        self.complete(req).await
    }
}
