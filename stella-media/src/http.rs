//! Shared HTTP plumbing for the vendor media adapters: one classification
//! policy that maps an HTTP status + body into a [`MediaError`] category.
//!
//! Each adapter still *owns* the decision to route through this helper (they
//! all mirror the same `ProviderError`-shaped categories), but the mapping
//! lives once so a 401 means [`MediaError::Auth`] identically across Z.ai and
//! OpenAI rather than drifting per adapter. This is the direct analog of the
//! per-adapter status handling in `stella-model`'s chat adapters
//! (`zai.rs`/`openai.rs`), kept DRY here because three adapters share it.

use crate::error::MediaError;

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
        format!("{}…", &trimmed[..300])
    } else {
        trimmed.to_string()
    }
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
}
