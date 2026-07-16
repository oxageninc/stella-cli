//! Shared HTTP plumbing for every provider adapter: a `reqwest` client with a
//! bounded connect timeout, and an idle-timeout wrapper around per-chunk
//! stream reads. Centralized so every provider adapter gets identical
//! timeout-and-retry-classification behavior — a hung TCP connect or a
//! provider that opens a stream and then goes silent must surface as a
//! *retryable* `Transport` error, not an unbounded hang.

use std::time::Duration;

use futures_util::{Stream, StreamExt};
use stella_protocol::ProviderError;

/// How long to wait for the initial TCP/TLS connection before giving up. A
/// dead or black-holed provider endpoint should fail fast and retryably, not
/// block the turn on the OS default connect timeout (minutes).
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for the *next* chunk of an already-open stream before
/// treating the connection as dead. Generous enough for slow model thinking
/// between tokens, short enough that a silently-dropped stream doesn't hang
/// the turn forever.
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// A `reqwest::Client` with [`CONNECT_TIMEOUT`] applied, plus a per-read
/// stall bound of [`STREAM_IDLE_TIMEOUT`]. The read timeout closes the gap
/// [`next_with_timeout`] cannot see: the wait between a successful connect
/// and the first response byte (headers). Without it, a provider LB that
/// accepts the connection and then black-holes hangs `.send()` forever and
/// the retry engine never fires. The bound matches the stream-idle policy,
/// so it can never kill a stream the idle timeout would have allowed.
/// Falls back to the default client if the builder fails (only possible on
/// a broken TLS backend, which is catastrophic and unrelated to any single
/// request) — never panics on the construction path.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(STREAM_IDLE_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// Parse a `Retry-After` header (delta-seconds form, RFC 9110 §10.2.3) into
/// a millisecond hint for the retry policy — `stella-core/src/retry.rs`
/// honors `RateLimited.retry_after_ms` when present. The HTTP-date form is
/// not handled (providers send seconds on 429s); an absent or unparseable
/// value yields `None` rather than an error.
pub(crate) fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let seconds: u64 = value.to_str().ok()?.trim().parse().ok()?;
    Some(seconds.saturating_mul(1000))
}

/// The mechanical non-success ladder shared by every adapter, applied AFTER
/// any vendor-specific pre-check (Z.ai's billing-encoded 429s, Google's
/// API_KEY_INVALID-on-400):
///
/// - 401/403 → non-retryable `Auth`. A 403 (permission-denied key, model
///   not enabled for the account) is a credential problem the user must fix
///   — pointing them at their key beats a generic terminal error, and the
///   step driver must not retry it.
/// - 429 → retryable `RateLimited` carrying the Retry-After hint.
/// - 5xx → retryable `Transport` (includes 529, which Anthropic and Z.ai
///   use for load shedding). Without this a momentary blip aborts the whole
///   turn (`Terminal.is_retryable() == false`).
/// - anything else → `Terminal`, with the body for diagnosis.
pub(crate) fn classify_http_status(
    label: &str,
    status: reqwest::StatusCode,
    retry_after_ms: Option<u64>,
    body: &str,
) -> ProviderError {
    use reqwest::StatusCode;
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            ProviderError::Auth(format!("{label} rejected the credential (HTTP {status})"))
        }
        StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited {
            message: format!("{label} rate limit"),
            retry_after_ms,
        },
        s if s.is_server_error() => {
            ProviderError::Transport(format!("{label} HTTP {status}: {body}"))
        }
        _ => ProviderError::Terminal(format!("{label} HTTP {status}: {body}")),
    }
}

/// Await the next stream item, bounded by `idle`. Maps a stalled stream (no
/// item within `idle`) and any transport error to a **retryable**
/// `ProviderError::Transport`, and a clean end-of-stream to `Ok(None)`.
///
/// `idle` is a parameter (rather than reading [`STREAM_IDLE_TIMEOUT`]
/// directly) purely so the timeout path is unit-testable in milliseconds;
/// adapters always pass [`STREAM_IDLE_TIMEOUT`].
pub(crate) async fn next_with_timeout<S, T>(
    stream: &mut S,
    idle: Duration,
) -> Result<Option<T>, ProviderError>
where
    S: Stream<Item = reqwest::Result<T>> + Unpin,
{
    match tokio::time::timeout(idle, stream.next()).await {
        Ok(Some(Ok(item))) => Ok(Some(item)),
        Ok(Some(Err(e))) => Err(ProviderError::Transport(e.to_string())),
        Ok(None) => Ok(None),
        Err(_elapsed) => Err(ProviderError::Transport(format!(
            "stream idle timeout: no data for {}s",
            idle.as_secs()
        ))),
    }
}

/// Longest raw-snippet prefix (in characters) echoed back in a truncated
/// tool-input error — enough to identify the call without dumping a
/// multi-kilobyte partial payload into the log.
const TOOL_INPUT_SNIPPET_CHARS: usize = 200;

/// Build the terminal error for a streamed tool call whose argument JSON was
/// cut off at the output-token limit before it finished. Shared by every
/// adapter that assembles tool calls from stream fragments: `provider` names
/// the concrete backend, `limit_signal` the wire-level evidence
/// (`stop_reason=max_tokens`, `finish_reason=length`). Non-retryable:
/// re-issuing the identical request re-truncates identically (the very
/// "stuck-loop" this error replaces), so the actionable fix — a larger
/// `max_output_tokens` or a smaller payload — is surfaced in the message
/// along with a bounded snippet of the RAW accumulated string.
pub(crate) fn truncated_tool_input_error(
    provider: &str,
    name: &str,
    raw: &str,
    limit_signal: &str,
) -> ProviderError {
    let total_chars = raw.chars().count();
    let snippet: String = raw.chars().take(TOOL_INPUT_SNIPPET_CHARS).collect();
    let ellipsis = if total_chars > TOOL_INPUT_SNIPPET_CHARS {
        "…"
    } else {
        ""
    };
    ProviderError::Terminal(format!(
        "{provider} tool call `{name}` was cut off at the output-token limit \
         ({limit_signal}) before its arguments finished streaming — the \
         accumulated JSON is incomplete and cannot be parsed ({total_chars} chars: \
         `{snippet}{ellipsis}`). Raise max_output_tokens or have the model emit a \
         smaller payload, then retry."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_without_panicking() {
        // Smoke test: the connect-timeout client constructs on this platform.
        let _ = client();
    }

    #[tokio::test]
    async fn next_with_timeout_maps_a_stalled_stream_to_a_retryable_transport_error() {
        // A stream that never yields must time out and surface as retryable,
        // not hang. `pending()` models a connection that opened and then went
        // silent — the exact failure the idle timeout exists to bound.
        let mut stalled = futures_util::stream::pending::<reqwest::Result<Vec<u8>>>();
        let err = next_with_timeout(&mut stalled, Duration::from_millis(20))
            .await
            .expect_err("a stalled stream must error, not hang");
        assert!(
            err.is_retryable(),
            "idle timeout must be retryable: {err:?}"
        );
        assert!(matches!(err, ProviderError::Transport(_)));
    }

    #[tokio::test]
    async fn next_with_timeout_passes_through_a_ready_item_and_end_of_stream() {
        let mut ready = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(vec![1u8, 2, 3])]);
        let first = next_with_timeout(&mut ready, Duration::from_millis(50))
            .await
            .expect("a ready item is not an error");
        assert_eq!(first, Some(vec![1u8, 2, 3]));
        let end = next_with_timeout(&mut ready, Duration::from_millis(50))
            .await
            .expect("clean end of stream is Ok(None)");
        assert_eq!(end, None);
    }
}
