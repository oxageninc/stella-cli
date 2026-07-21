//! Shared HTTP plumbing for every provider adapter: a `reqwest` client with a
//! bounded connect timeout, and an idle-timeout wrapper around per-chunk
//! stream reads. Centralized so every provider adapter gets identical
//! timeout-and-retry-classification behavior — a hung TCP connect or a
//! provider that opens a stream and then goes silent must surface as a
//! *retryable* `Transport` error, not an unbounded hang.

use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde::Deserialize;
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

/// Best-effort extraction of the human `message` from a provider's
/// structured error body (issue #250: 401/403 used to discard the body
/// entirely). OpenAI, Anthropic, and every OpenAI-compatible gateway
/// (OpenRouter, xAI, DeepSeek, Z.ai, Gemini direct) nest it under an `error`
/// object — `{"error": {"message": "...", ...}}`; Bedrock/AWS report the
/// same field flat at the top level — `{"message": "...", "__type": ...}`.
/// Tried in that order; `None` when the body is not JSON or carries no
/// non-empty `message`, in which case callers fall back to the raw body.
fn parse_error_reason(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Wrapped {
        error: Inner,
    }
    #[derive(Deserialize, Default)]
    struct Inner {
        #[serde(default)]
        message: Option<String>,
    }
    #[derive(Deserialize, Default)]
    struct Flat {
        #[serde(default)]
        message: Option<String>,
    }

    let reason = serde_json::from_str::<Wrapped>(body)
        .ok()
        .and_then(|w| w.error.message)
        .or_else(|| {
            serde_json::from_str::<Flat>(body)
                .ok()
                .and_then(|f| f.message)
        });

    reason
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
}

/// Bucket an HTTP 403's reason text into the right remediation hint. A 403
/// means the key itself is valid — the account/key/model combination is
/// refused for one of three provider-observed reasons, and each has a
/// different fix: billing/credits exhausted, this specific model not
/// enabled for the key/org, or a bare permission/scope refusal with neither
/// signal present. Matched on lowercase text since providers share no
/// stable machine code for this (the same body-sniffing tradeoff
/// `zai.rs`'s 429 billing classifier makes) — a false negative that falls
/// through to the generic hint beats a wrong diagnosis.
fn forbidden_hint(haystack: &str) -> &'static str {
    if haystack.contains("credit")
        || haystack.contains("balance")
        || haystack.contains("insufficient")
        || haystack.contains("quota")
        || haystack.contains("spend")
        || haystack.contains("billing")
    {
        "the key is valid but the account is out of credits or over its spend cap — add \
         credit or raise the cap, then retry"
    } else if haystack.contains("model")
        || haystack.contains("enable")
        || haystack.contains("access")
    {
        "the key is valid but isn't enabled for this model — enable it for this key/org, or \
         switch models on the SETTINGS tab / with `--model provider/slug`"
    } else {
        "the key is valid but lacks permission for this request — check the key's scopes \
         and organization on the provider's dashboard"
    }
}

/// Whether a non-success body appears to be about the model the caller
/// configured — either literally (the gateway echoes back the rejected
/// slug, e.g. OpenRouter's `"{slug} is not a valid model ID"`) or generically
/// (any mention of "model" in a 4xx we already know isn't an auth/rate-limit
/// failure). Used to gate the invalid-model recovery hint (issue #271) so it
/// fires on "unknown model" without also firing on an unrelated malformed
/// request that happens to 400 for some other reason.
fn mentions_configured_model(body: &str, model: &str) -> bool {
    (!model.trim().is_empty() && body.contains(model)) || body.to_lowercase().contains("model")
}

/// The mechanical non-success ladder shared by every adapter, applied AFTER
/// any vendor-specific pre-check (Z.ai's billing-encoded 429s, Google's
/// API_KEY_INVALID-on-400). `model` is the wire slug this call was sent
/// with, used only to sharpen the invalid-model hint below — every adapter
/// already holds it as `self.model`.
///
/// - 401 → non-retryable `Auth` ("authenticate": the key itself is rejected
///   — revoked, mistyped, or never loaded). The provider's own reason
///   ([`parse_error_reason`]) is folded in when present, plus a hint to
///   check the SETTINGS tab / `api_key`/`api_key_env` / `--base-url`.
/// - 403 → non-retryable `Auth` ("authorize": the key is valid but the
///   request is refused). [`forbidden_hint`] distinguishes credits/billing,
///   model-not-enabled, and bare permission refusal so the three don't read
///   as one undifferentiated "credentials failed" message (issue #250).
/// - 402 → non-retryable `Terminal`, called out explicitly as a billing
///   failure (some gateways use Payment Required for out-of-credits rather
///   than folding it into a 403).
/// - 429 → retryable `RateLimited` carrying the Retry-After hint.
/// - 5xx → retryable `Transport` (includes 529, which Anthropic and Z.ai
///   use for load shedding). Without this a momentary blip aborts the whole
///   turn (`Terminal.is_retryable() == false`).
/// - anything else (400/404/422/...) → non-retryable `Terminal` — this call
///   can never succeed by retrying it verbatim (issue #271), so the
///   step-driver's retry loop never re-derives a classification here, it
///   only ever asks `is_retryable()` and gets `false` on the first attempt.
///   When the body appears to name the configured model
///   ([`mentions_configured_model`]), a one-line recovery hint is appended:
///   change it on the SETTINGS tab, relaunch with `--model provider/slug`,
///   or edit settings.json.
pub(crate) fn classify_http_status(
    label: &str,
    status: reqwest::StatusCode,
    retry_after_ms: Option<u64>,
    body: &str,
    model: &str,
) -> ProviderError {
    use reqwest::StatusCode;
    let reason = parse_error_reason(body);
    let reason_suffix = reason
        .as_deref()
        .map(|r| format!(": {r}"))
        .unwrap_or_default();
    match status {
        StatusCode::UNAUTHORIZED => ProviderError::Auth(format!(
            "{label} rejected the credential (HTTP 401){reason_suffix} — the key looks \
             revoked, mistyped, or was never loaded; re-check it on the SETTINGS tab, the \
             `api_key`/`api_key_env` value, and that `--base-url` actually points at {label}"
        )),
        StatusCode::FORBIDDEN => {
            let haystack = reason.as_deref().unwrap_or(body).to_lowercase();
            let hint = forbidden_hint(&haystack);
            ProviderError::Auth(format!(
                "{label} rejected the credential (HTTP 403){reason_suffix} — {hint}"
            ))
        }
        StatusCode::PAYMENT_REQUIRED => ProviderError::Terminal(format!(
            "{label} rejected the request (HTTP 402: payment required){reason_suffix} — the \
             account is out of credits; add credit or switch to a funded provider/model"
        )),
        StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited {
            message: format!("{label} rate limit"),
            retry_after_ms,
        },
        s if s.is_server_error() => {
            ProviderError::Transport(format!("{label} HTTP {status}: {body}"))
        }
        _ => {
            let mut message = format!("{label} HTTP {status}: {body}");
            if mentions_configured_model(body, model) {
                message.push_str(
                    " — change the model on the deck's SETTINGS tab, relaunch with \
                     `--model provider/slug`, or edit settings.json",
                );
            }
            ProviderError::Terminal(message)
        }
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

    // ---- classify_http_status (issue #271: terminal-on-first-attempt +
    // invalid-model recovery hint; issue #250: sharper 401/403/402 auth
    // diagnostics) --------------------------------------------------------

    /// The real-world repro (issue #271): OpenRouter answers a doubled/typo'd
    /// model slug with HTTP 400 and a body naming the exact slug it rejected.
    /// On origin/main this classified fine as `Terminal` (so it was already
    /// non-retryable) but the message carried no pointer to a fix — this
    /// assertion is what's new.
    #[test]
    fn classify_http_status_400_invalid_model_names_a_recovery_hint() {
        let err = classify_http_status(
            "OpenRouter",
            reqwest::StatusCode::BAD_REQUEST,
            None,
            r#"{"error":{"message":"openrouter/openrouter/auto is not a valid model ID","code":400}}"#,
            "openrouter/openrouter/auto",
        );
        assert!(!err.is_retryable(), "a 400 must never be retried: {err:?}");
        assert!(matches!(err, ProviderError::Terminal(_)));
        let msg = err.to_string();
        assert!(msg.contains("is not a valid model ID"), "{msg}");
        assert!(msg.contains("SETTINGS tab"), "{msg}");
        assert!(msg.contains("--model provider/slug"), "{msg}");
        assert!(msg.contains("settings.json"), "{msg}");
    }

    /// A 404 "model not found" must get the same hint as a 400 — both are
    /// the "this model can't be reached" family the issue calls out
    /// together ("invalid model, invalid request shape, 404 model").
    #[test]
    fn classify_http_status_404_model_not_found_also_gets_the_hint() {
        let err = classify_http_status(
            "OpenAI",
            reqwest::StatusCode::NOT_FOUND,
            None,
            r#"{"error":{"message":"The model `gpt-nope` does not exist"}}"#,
            "gpt-nope",
        );
        assert!(!err.is_retryable());
        assert!(err.to_string().contains("SETTINGS tab"));
    }

    /// A 4xx that has nothing to do with the model (a malformed tool-call
    /// schema, say) must NOT get the model-recovery hint — it would be a
    /// non sequitur ("change your model" doesn't fix a bad JSON schema).
    /// Guards the precision of `mentions_configured_model` against the
    /// obvious failure mode of firing on every non-auth 4xx.
    #[test]
    fn classify_http_status_400_unrelated_to_the_model_has_no_recovery_hint() {
        let err = classify_http_status(
            "OpenAI",
            reqwest::StatusCode::BAD_REQUEST,
            None,
            r#"{"error":{"message":"messages: array is too long"}}"#,
            "gpt-5.5",
        );
        let msg = err.to_string();
        assert!(!msg.contains("SETTINGS tab"), "{msg}");
        assert!(!msg.contains("--model"), "{msg}");
    }

    /// Issue #250: on origin/main, 401/403 discarded the response body
    /// entirely (`format!("{label} rejected the credential (HTTP {status})")`,
    /// no `body` in scope at all) — the provider's own reason never reached
    /// the user. This is the fold-in.
    #[test]
    fn classify_http_status_401_folds_in_the_provider_reason_and_a_credential_hint() {
        let err = classify_http_status(
            "OpenAI",
            reqwest::StatusCode::UNAUTHORIZED,
            None,
            r#"{"error":{"message":"Incorrect API key provided: sk-***abcd"}}"#,
            "gpt-5.5",
        );
        assert!(!err.is_retryable());
        assert!(matches!(err, ProviderError::Auth(_)));
        let msg = err.to_string();
        assert!(msg.contains("Incorrect API key provided"), "{msg}");
        assert!(msg.contains("SETTINGS tab"), "{msg}");
        assert!(msg.contains("api_key"), "{msg}");
    }

    /// 403 with billing language ⇒ the credits/spend-cap hint, distinct from
    /// a bad key and from "model not enabled" (issue #250 acceptance: the
    /// three must read as different failures, not one "credentials failed").
    #[test]
    fn classify_http_status_403_with_billing_language_hints_at_credits() {
        let err = classify_http_status(
            "Anthropic",
            reqwest::StatusCode::FORBIDDEN,
            None,
            r#"{"error":{"message":"Your organization has insufficient credit balance"}}"#,
            "claude-opus",
        );
        assert!(!err.is_retryable());
        let msg = err.to_string();
        assert!(msg.contains("credits"), "{msg}");
        assert!(!msg.contains("enable it"), "{msg}");
    }

    /// 403 with "not enabled for this key" ⇒ the model-permission hint, not
    /// the credits hint and not the generic fallback.
    #[test]
    fn classify_http_status_403_model_not_enabled_hints_at_switching_models() {
        let err = classify_http_status(
            "OpenRouter",
            reqwest::StatusCode::FORBIDDEN,
            None,
            r#"{"error":{"message":"model 'x/y' not enabled for this key"}}"#,
            "x/y",
        );
        let msg = err.to_string();
        assert!(msg.contains("isn't enabled for this model"), "{msg}");
        assert!(msg.contains("enable it"), "{msg}");
        assert!(msg.contains("--model provider/slug"), "{msg}");
        assert!(!msg.contains("credits"), "{msg}");
    }

    /// A bare 403 with neither billing nor model language falls to the
    /// generic permission hint — never silently identical to the 401 (key
    /// revoked) message, since the key here is valid.
    #[test]
    fn classify_http_status_403_generic_falls_back_to_a_permission_hint() {
        let err = classify_http_status(
            "Anthropic",
            reqwest::StatusCode::FORBIDDEN,
            None,
            r#"{"error":{"message":"request blocked by organization policy"}}"#,
            "claude-opus",
        );
        let msg = err.to_string();
        assert!(msg.contains("lacks permission"), "{msg}");
        assert!(!msg.contains("credits"), "{msg}");
        assert!(!msg.contains("enable it"), "{msg}");
    }

    /// 402 (some gateways use Payment Required rather than folding
    /// out-of-credits into a 403) must read as a distinct billing failure,
    /// not the generic `Terminal("{label} HTTP {status}: {body}")` bucket —
    /// asserted against an EMPTY body so the message can only be coming from
    /// the new dedicated arm, never from an echoed body.
    #[test]
    fn classify_http_status_402_is_terminal_with_a_billing_hint() {
        let err = classify_http_status(
            "OpenAI",
            reqwest::StatusCode::PAYMENT_REQUIRED,
            None,
            "",
            "gpt-5.5",
        );
        assert!(!err.is_retryable());
        assert!(matches!(err, ProviderError::Terminal(_)));
        let msg = err.to_string();
        assert!(msg.contains("payment required"), "{msg}");
        assert!(msg.contains("out of credits"), "{msg}");
    }

    /// The rest of the ladder (429/5xx) is untouched by this change — pinned
    /// here so a future edit to the 401/403/402/model-hint arms can't
    /// silently regress the retryable side.
    #[test]
    fn classify_http_status_429_and_5xx_stay_retryable() {
        let rate_limited = classify_http_status(
            "OpenAI",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some(1_500),
            "",
            "gpt-5.5",
        );
        assert!(rate_limited.is_retryable());
        assert!(matches!(rate_limited, ProviderError::RateLimited { .. }));

        let server_error = classify_http_status(
            "OpenAI",
            reqwest::StatusCode::BAD_GATEWAY,
            None,
            "upstream down",
            "gpt-5.5",
        );
        assert!(server_error.is_retryable());
        assert!(matches!(server_error, ProviderError::Transport(_)));
    }
}
