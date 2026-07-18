//! Streamable-HTTP transport: POST JSON-RPC to an endpoint URL and read the
//! response, which may arrive as either `application/json` (one message) or
//! `text/event-stream` (JSON-RPC messages carried as SSE `data:` lines). A
//! session id assigned by the server in the `Mcp-Session-Id` response header
//! (typically on `initialize`) is captured and replayed on every subsequent
//! request.
//!
//! Each POST is self-correlating — one request, one response — so there is no
//! pending-map here; when the server streams SSE, this transport scans the
//! stream for the message whose id echoes the request (progress
//! notifications interleaved before it are skipped).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::error::McpError;
use crate::protocol::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest};
use crate::sse::SseDecoder;
use crate::transport::Transport;

/// The MCP streamable-HTTP session header.
const SESSION_HEADER: &str = "Mcp-Session-Id";

/// A streamable-HTTP connection to one MCP server.
pub struct HttpTransport {
    client: Client,
    url: String,
    base_headers: HeaderMap,
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
    server_name: String,
}

impl HttpTransport {
    /// Build a transport for `url`, replaying `headers` on every request.
    /// `timeout` bounds each POST (including the SSE body read).
    pub fn new(
        server_name: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let mut base_headers = HeaderMap::new();
        for (key, value) in headers {
            let name = HeaderName::from_bytes(key.as_bytes())
                .map_err(|e| McpError::Config(format!("invalid header name `{key}`: {e}")))?;
            let value = HeaderValue::from_str(value)
                .map_err(|e| McpError::Config(format!("invalid header value for `{key}`: {e}")))?;
            base_headers.insert(name, value);
        }
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| McpError::Transport(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            url: url.to_string(),
            base_headers,
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
            server_name: server_name.to_string(),
        })
    }

    /// POST `body`, capture any assigned session id, and reject non-2xx.
    async fn send(&self, body: String) -> Result<reqwest::Response, McpError> {
        let mut builder = self
            .client
            .post(&self.url)
            .headers(self.base_headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body);
        if let Some(session) = self.session_id.lock().await.clone() {
            builder = builder.header(SESSION_HEADER, session);
        }

        let response = builder.send().await.map_err(|e| {
            McpError::Transport(format!(
                "HTTP request to `{}` failed: {e}",
                self.server_name
            ))
        })?;

        // Capture the session id the first time the server assigns one.
        if let Some(session) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            let mut guard = self.session_id.lock().await;
            if guard.is_none() {
                *guard = Some(session.to_string());
            }
        }

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(McpError::Transport(format!(
                "server `{}` returned HTTP {status}: {}",
                self.server_name,
                truncate(&text, 500)
            )));
        }
        Ok(response)
    }

    /// Read a `text/event-stream` body and return the JSON-RPC message that
    /// echoes `expected_id` (or, failing an id match, the first message that
    /// carries a `result`/`error`).
    async fn read_sse_message(
        &self,
        response: reqwest::Response,
        expected_id: u64,
    ) -> Result<JsonRpcMessage, McpError> {
        let mut decoder = SseDecoder::new();
        let mut stream = response.bytes_stream();
        let mut fallback: Option<JsonRpcMessage> = None;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                McpError::Transport(format!("SSE stream from `{}` broke: {e}", self.server_name))
            })?;
            decoder.push_bytes(&chunk).map_err(|e| {
                McpError::Transport(format!("SSE stream from `{}`: {e}", self.server_name))
            })?;
            for event in decoder.poll() {
                if event.data.trim().is_empty() {
                    continue;
                }
                // Tolerate non-JSON `data:` lines (comments/keep-alives).
                let message: JsonRpcMessage = match serde_json::from_str(event.data.trim()) {
                    Ok(message) => message,
                    Err(_) => continue,
                };
                if message.correlated_id() == Some(expected_id) {
                    return Ok(message);
                }
                if fallback.is_none() && (message.result.is_some() || message.error.is_some()) {
                    fallback = Some(message);
                }
                // Otherwise a server notification (e.g. progress) — keep reading.
            }
        }

        fallback.ok_or_else(|| {
            McpError::Protocol(format!(
                "SSE stream from `{}` ended without a response for request {expected_id}",
                self.server_name
            ))
        })
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest::new(id, method, params);
        let body =
            serde_json::to_string(&request).map_err(|e| McpError::Protocol(e.to_string()))?;
        let response = self.send(body).await?;

        let is_sse = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.to_ascii_lowercase().contains("text/event-stream"))
            .unwrap_or(false);

        let message = if is_sse {
            self.read_sse_message(response, id).await?
        } else {
            let text = response.text().await.map_err(|e| {
                McpError::Transport(format!(
                    "reading response body from `{}` failed: {e}",
                    self.server_name
                ))
            })?;
            serde_json::from_str::<JsonRpcMessage>(text.trim()).map_err(|e| {
                McpError::Protocol(format!("could not decode JSON-RPC response: {e}"))
            })?
        };
        message.into_result()
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let note = JsonRpcNotification::new(method, params);
        let body = serde_json::to_string(&note).map_err(|e| McpError::Protocol(e.to_string()))?;
        // A notification's response body (typically `202 Accepted`) is
        // irrelevant; we only need the POST to have succeeded.
        let _ = self.send(body).await?;
        Ok(())
    }

    async fn close(&self) -> Result<(), McpError> {
        // No persistent connection to tear down. (A future revision could
        // `DELETE` the session; v1 lets the server expire it.)
        Ok(())
    }
}

/// Char-boundary-safe truncation for diagnostic bodies.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_header_name_is_a_config_error() {
        let mut headers = BTreeMap::new();
        headers.insert("bad header".to_string(), "x".to_string());
        let result = HttpTransport::new("s", "https://h/mcp", &headers, Duration::from_secs(1));
        assert!(matches!(result, Err(McpError::Config(_))));
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        // Multi-byte chars must never be sliced mid-codepoint.
        let s = "áéíóú".repeat(200);
        let out = truncate(&s, 3);
        assert_eq!(out, "áéí…");
    }

    #[test]
    fn short_strings_are_untouched() {
        assert_eq!(truncate("hello", 500), "hello");
    }
}
