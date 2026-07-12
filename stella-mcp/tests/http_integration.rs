//! Streamable-HTTP transport tests using `wiremock`. Covers both response
//! shapes a server may return (`application/json` and `text/event-stream`),
//! session-id capture + replay, and non-2xx mapping.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;
use stella_mcp::{HttpTransport, McpClient, McpError, McpServerConfig, McpTransport, Transport};
use stella_protocol::ToolOutput;
use wiremock::matchers::{body_partial_json, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn json_handshake_replays_the_session_id() {
    let server = MockServer::start().await;

    // `initialize` assigns a session id (no session header required on it).
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "initialize" })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("Mcp-Session-Id", "sess-xyz")
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "serverInfo": { "name": "remote", "version": "1" }
                    }
                })),
        )
        .mount(&server)
        .await;

    // The `notifications/initialized` notification (202, no body).
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({ "method": "notifications/initialized" }),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    // `tools/list` matches ONLY if the session id was replayed — proving
    // capture + replay end-to-end.
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/list" })))
        .and(header("mcp-session-id", "sess-xyz"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": 2,
                    "result": { "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] }
                })),
        )
        .mount(&server)
        .await;

    // `tools/call` — likewise requires the session id.
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/call" })))
        .and(header("mcp-session-id", "sess-xyz"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": 3,
                    "result": { "content": [{ "type": "text", "text": "remote ok" }] }
                })),
        )
        .mount(&server)
        .await;

    let cfg = McpServerConfig {
        name: "remote".into(),
        transport: McpTransport::Http {
            url: server.uri(),
            headers: BTreeMap::new(),
        },
    };
    let client = McpClient::connect(&cfg, Duration::from_secs(5))
        .await
        .unwrap();

    assert_eq!(client.negotiated_version(), "2025-06-18");
    assert_eq!(client.tools().len(), 1);

    let out = client.call_tool("echo", json!({})).await.unwrap();
    assert_eq!(
        out,
        ToolOutput::Ok {
            content: "remote ok".into()
        }
    );
}

#[tokio::test]
async fn sse_response_is_parsed() {
    let server = MockServer::start().await;

    // A progress notification (no id) interleaved before the real response —
    // the transport must skip it and return the correlated result.
    let sse_body = concat!(
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"p\":0.5}}\n",
        "\n",
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"sse ok\"}]}}\n",
        "\n",
    );
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/call" })))
        .respond_with(
            // `set_body_raw` sets the Content-Type cleanly (no duplicate
            // header from a body helper) so the transport detects SSE.
            ResponseTemplate::new(200).set_body_raw(sse_body.as_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let transport =
        HttpTransport::new("h", &server.uri(), &BTreeMap::new(), Duration::from_secs(5)).unwrap();
    // First request on a fresh transport is id 1 — matches the SSE payload.
    let result = transport
        .request("tools/call", json!({ "name": "echo", "arguments": {} }))
        .await
        .unwrap();
    assert_eq!(result["content"][0]["text"], "sse ok");
}

#[tokio::test]
async fn non_2xx_is_a_transport_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal boom"))
        .mount(&server)
        .await;

    let transport =
        HttpTransport::new("h", &server.uri(), &BTreeMap::new(), Duration::from_secs(5)).unwrap();
    let err = transport
        .request("tools/call", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::Transport(_)));
}

#[tokio::test]
async fn configured_headers_are_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } })),
        )
        .mount(&server)
        .await;

    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_string(), "Bearer tok".to_string());
    let transport =
        HttpTransport::new("h", &server.uri(), &headers, Duration::from_secs(5)).unwrap();
    // Succeeds only because the configured Authorization header was sent.
    let result = transport.request("ping", json!({})).await.unwrap();
    assert_eq!(result["ok"], true);
}
