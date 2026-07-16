//! Shared HTTP plumbing for the vendor media adapters: one classification
//! policy that maps an HTTP status + body into a [`MediaError`] category.
//!
//! Each adapter still *owns* the decision to route through this helper (they
//! all mirror the same `ProviderError`-shaped categories), but the mapping
//! lives once so a 401 means [`MediaError::Auth`] identically across Z.ai and
//! OpenAI rather than drifting per adapter. This is the direct analog of the
//! per-adapter status handling in `stella-model`'s chat adapters
//! (`zai.rs`/`openai.rs`), kept DRY here because three adapters share it.

use std::time::Duration;

use crate::error::MediaError;

/// How long to wait for the initial TCP/TLS connection before giving up —
/// mirrors `stella-model`'s policy: a black-holed endpoint must fail fast
/// and retryably, not block the turn on the OS connect timeout.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-read stall bound. Media responses are single JSON bodies or binary
/// downloads (no long-lived SSE gaps), so a connection that goes silent for
/// this long is dead. Generous enough for slow generation endpoints that
/// stream their body slowly; it bounds silence, not total transfer time.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// A `reqwest::Client` bounded by [`CONNECT_TIMEOUT`] and [`READ_TIMEOUT`].
/// Every media adapter must use this instead of `reqwest::Client::new()`:
/// an unbounded client turns a stalled provider into an agent turn that
/// hangs forever (there is no outer tool-level timeout on the media path).
/// Falls back to the default client only if the builder itself fails
/// (broken TLS backend — catastrophic and unrelated to any one request).
pub(crate) fn client() -> reqwest::Client {
    client_with(CONNECT_TIMEOUT, READ_TIMEOUT)
}

/// Builder core behind [`client`], parameterized so the stall path is
/// unit-testable in milliseconds; adapters always go through [`client`].
pub(crate) fn client_with(connect: Duration, read: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .read_timeout(read)
        .build()
        .unwrap_or_default()
}

/// Parse a `Retry-After` header (delta-seconds, RFC 9110 §10.2.3) into
/// milliseconds. HTTP-date form is not handled (providers send seconds on
/// 429s); an unparseable value yields `None` rather than an error.
pub(crate) fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let text = value.to_str().ok()?;
    let secs: u64 = text.trim().parse().ok()?;
    Some(secs.saturating_mul(1000))
}

/// Map a non-success HTTP response into a typed [`MediaError`]. `provider`
/// names the vendor for the message; `retry_after_ms` is threaded through
/// on 429s. The content-policy detection is a documented heuristic: a 400 /
/// 422 whose body mentions safety/policy/moderation is surfaced as
/// [`MediaError::ContentPolicy`] (a refusal the user can act on) rather than
/// a generic terminal error (`08-multimodal.md` §7).
pub(crate) fn classify_http_error(
    provider: &str,
    status: reqwest::StatusCode,
    retry_after_ms: Option<u64>,
    body: &str,
) -> MediaError {
    use reqwest::StatusCode;
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            MediaError::Auth(format!("{provider} rejected the API key (HTTP {status})"))
        }
        StatusCode::TOO_MANY_REQUESTS => MediaError::RateLimited {
            message: format!("{provider} rate limit"),
            retry_after_ms,
        },
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY if looks_like_policy(body) => {
            MediaError::ContentPolicy(format!("{provider}: {}", first_line(body)))
        }
        _ => MediaError::Terminal(format!("{provider} HTTP {status}: {}", first_line(body))),
    }
}

/// Download the bytes of a generated asset from a provider-returned URL
/// (Z.ai image/video endpoints hand back a URL rather than inline base64).
/// A non-success status is classified with [`classify_http_error`]; transport
/// failures become [`MediaError::Transport`].
pub(crate) async fn download_bytes(
    client: &reqwest::Client,
    url: &str,
    provider: &str,
) -> Result<Vec<u8>, MediaError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| MediaError::Transport(e.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let retry_after_ms = parse_retry_after_ms(response.headers());
        let body = response.text().await.unwrap_or_default();
        return Err(classify_http_error(provider, status, retry_after_ms, &body));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| MediaError::Transport(e.to_string()))?;
    Ok(bytes.to_vec())
}

/// Heuristic: does the error body read like a content-policy refusal?
/// Case-insensitive substring match on the vocabulary providers use for
/// moderation rejections. Documented and testable so a fixture can assert
/// a refusal is classified as [`MediaError::ContentPolicy`].
fn looks_like_policy(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    const NEEDLES: [&str; 6] = [
        "content policy",
        "content_policy",
        "safety",
        "moderation",
        "policy violation",
        "prohibited",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

/// Trim a body to its first non-empty line, bounded, so error messages stay
/// single-line (the `stream-json` event interface is one line per event).
fn first_line(body: &str) -> String {
    let line = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let trimmed = line.trim();
    if trimmed.len() > 300 {
        // Truncate on a UTF-8 char boundary: the body is provider-supplied and
        // may be non-ASCII, so byte 300 can land mid-codepoint — a `&s[..300]`
        // slice there panics, which wire data must never do.
        format!("{}…", &trimmed[..floor_char_boundary(trimmed, 300)])
    } else {
        trimmed.to_string()
    }
}

/// The largest byte index `<= max` that is a UTF-8 char boundary of `s`. Lets
/// us bound a string without slicing through a multibyte codepoint. (`std`'s
/// `str::floor_char_boundary` is still unstable, so we spell it out.)
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn unauthorized_and_forbidden_map_to_auth() {
        assert!(matches!(
            classify_http_error("zai", StatusCode::UNAUTHORIZED, None, "bad key"),
            MediaError::Auth(_)
        ));
        assert!(matches!(
            classify_http_error("openai", StatusCode::FORBIDDEN, None, "no entitlement"),
            MediaError::Auth(_)
        ));
    }

    #[test]
    fn too_many_requests_maps_to_rate_limited_and_threads_retry_after() {
        let err = classify_http_error("zai", StatusCode::TOO_MANY_REQUESTS, Some(2000), "slow");
        match err {
            MediaError::RateLimited { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(2000));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(err_is_retryable(StatusCode::TOO_MANY_REQUESTS));
    }

    fn err_is_retryable(status: StatusCode) -> bool {
        classify_http_error("x", status, None, "").is_retryable()
    }

    #[test]
    fn bad_request_mentioning_policy_is_content_policy_otherwise_terminal() {
        let refusal = classify_http_error(
            "openai",
            StatusCode::BAD_REQUEST,
            None,
            "{\"error\":\"Your request was rejected by our safety system\"}",
        );
        assert!(matches!(refusal, MediaError::ContentPolicy(_)));

        let generic = classify_http_error(
            "openai",
            StatusCode::BAD_REQUEST,
            None,
            "invalid size param",
        );
        assert!(matches!(generic, MediaError::Terminal(_)));
    }

    #[test]
    fn server_error_is_terminal_and_body_is_first_line_only() {
        let err = classify_http_error(
            "zai",
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
            "upstream boom\nstack line 2\nstack line 3",
        );
        let msg = err.to_string();
        assert!(msg.contains("upstream boom"));
        assert!(!msg.contains("stack line 2"));
    }

    #[test]
    fn parse_retry_after_reads_delta_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(parse_retry_after_ms(&headers), Some(3000));

        let mut bad = HeaderMap::new();
        bad.insert(
            reqwest::header::RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2099 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after_ms(&bad), None);
    }

    #[tokio::test]
    async fn client_read_timeout_turns_a_stalled_response_into_an_error_not_a_hang() {
        // Failure-mode simulation for the timeout this module adds: the
        // server accepts the request and then goes silent longer than the
        // read timeout. An unbounded client (the old Client::new()) would
        // hang here forever; the bounded one must surface a timeout error.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(5)),
            )
            .mount(&server)
            .await;

        let client = client_with(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(100),
        );
        let err = client
            .get(server.uri())
            .send()
            .await
            .expect_err("stalled response must error, not hang");
        assert!(err.is_timeout(), "expected a timeout error, got: {err:?}");
    }

    #[test]
    fn first_line_truncates_on_a_char_boundary_for_multibyte_bodies() {
        // 299 ASCII bytes then a run of 3-byte codepoints: byte 300 lands in the
        // middle of the first `€`, so a naive `&s[..300]` slice would panic.
        // `first_line` must instead truncate at the boundary before it.
        let body = format!("{}{}", "a".repeat(299), "€".repeat(10));
        assert!(
            !body.is_char_boundary(300),
            "test setup: byte 300 is mid-char"
        );

        let out = first_line(&body);

        assert!(out.ends_with('…'));
        assert_eq!(out, format!("{}…", "a".repeat(299)));
    }
}
