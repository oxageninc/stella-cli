//! End-to-end over a real socket: a mock host binds the server, POSTs a turn,
//! opens the SSE stream, and answers the engine's reverse-RPC requests with
//! `provider-result` / `tool-result` POSTs — exactly the protocol Oxagen's
//! client will speak. Proves the transport on top of the (separately proven)
//! `!Send` bridge.

use std::net::SocketAddr;

use serde_json::json;
use stella_protocol::{CompletionMessage, ToolSchema};
use stella_serve::{ServeConfig, serve};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const TOKEN: &str = "test-secret";

fn echo_tool() -> serde_json::Value {
    serde_json::to_value(ToolSchema {
        name: "echo".to_string(),
        description: "echo".to_string(),
        input_schema: json!({ "type": "object" }),
        read_only: false,
    })
    .unwrap()
}

/// Start the server on an ephemeral loopback port; returns its address.
async fn start_server() -> SocketAddr {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let config = ServeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: TOKEN.to_string(),
        };
        let _ = serve(config, move |addr| {
            let _ = tx.send(addr);
        })
        .await;
    });
    rx.await.expect("server reported its bound address")
}

/// POST a JSON body and read the whole response (server sends `Connection:
/// close`). Returns `(status_line, body)`.
async fn post_json(
    addr: SocketAddr,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: engine\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head.lines().next().unwrap_or_default().to_string();
    (status, body.to_string())
}

/// GET a plain endpoint (used for `/healthz`), returning `(status_line, body)`.
async fn get_json(addr: SocketAddr, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: engine\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    (
        head.lines().next().unwrap_or_default().to_string(),
        body.to_string(),
    )
}

/// Open the SSE stream and consume the HTTP response head, leaving the reader at
/// the first event.
async fn open_sse(addr: SocketAddr, path: &str, token: &str) -> BufReader<TcpStream> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: engine\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap();
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    reader
}

/// Read one SSE `data:` payload; `None` at end of stream.
async fn next_event(reader: &mut BufReader<TcpStream>) -> Option<serde_json::Value> {
    let mut data: Option<String> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.ok()?;
        if n == 0 {
            return data.map(|d| serde_json::from_str(&d).unwrap());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if let Some(d) = &data {
                return Some(serde_json::from_str(d).unwrap());
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("data: ") {
            data = Some(rest.to_string());
        }
    }
}

fn model_result(text: &str) -> serde_json::Value {
    json!({
        "text": text,
        "usage": { "input_tokens": 0, "output_tokens": 0 },
        "model": "mock",
        "cost_usd": 0.0,
    })
}

fn model_wants_echo() -> serde_json::Value {
    json!({
        "text": "",
        "tool_calls": [{ "call_id": "c1", "name": "echo", "input": { "text": "hi" } }],
        "usage": { "input_tokens": 0, "output_tokens": 0 },
        "model": "mock",
        "cost_usd": 0.0,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn healthz_needs_no_auth_and_missing_token_is_rejected() {
    let addr = start_server().await;

    let (status, body) = get_json(addr, "/healthz").await;
    assert!(status.contains("200"), "health status: {status}");
    assert!(body.contains("\"status\":\"ok\""), "health body: {body}");

    // A turn without the bearer token is refused.
    let (status, _) = post_json(addr, "/v1/turns", None, "{}").await;
    assert!(status.contains("401"), "unauthenticated status: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_turn_round_trips_over_http() {
    let addr = start_server().await;

    let create = json!({
        "provider_id": "mock",
        "tools": [echo_tool()],
        "messages": [serde_json::to_value(CompletionMessage::user("use echo then answer")).unwrap()],
    })
    .to_string();
    let (status, body) = post_json(addr, "/v1/turns", Some(TOKEN), &create).await;
    assert!(
        status.contains("200"),
        "create status: {status}, body: {body}"
    );
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let turn_id = created["turn_id"].as_str().unwrap().to_string();

    let mut sse = open_sse(addr, &format!("/v1/turns/{turn_id}/events"), TOKEN).await;

    let mut provider_calls = 0;
    let mut tool_calls = 0;
    let mut outcome = None;

    while let Some(event) = next_event(&mut sse).await {
        match event["type"].as_str().unwrap_or_default() {
            "provider_request" => {
                provider_calls += 1;
                let request_id = event["request_id"].as_str().unwrap();
                let result = if provider_calls == 1 {
                    model_wants_echo()
                } else {
                    model_result("done")
                };
                let body = json!({
                    "request_id": request_id,
                    "status": "ok",
                    "result": result,
                })
                .to_string();
                let (status, resp) = post_json(
                    addr,
                    &format!("/v1/turns/{turn_id}/provider-result"),
                    Some(TOKEN),
                    &body,
                )
                .await;
                assert!(status.contains("200"), "provider-result: {status} {resp}");
            }
            "tool_request" => {
                tool_calls += 1;
                assert_eq!(event["name"].as_str(), Some("echo"));
                let request_id = event["request_id"].as_str().unwrap();
                let body = json!({
                    "request_id": request_id,
                    "output": { "ok": { "content": "echoed" } },
                })
                .to_string();
                let (status, resp) = post_json(
                    addr,
                    &format!("/v1/turns/{turn_id}/tool-result"),
                    Some(TOKEN),
                    &body,
                )
                .await;
                assert!(status.contains("200"), "tool-result: {status} {resp}");
            }
            "turn_complete" => outcome = Some(event["outcome"].clone()),
            _ => {}
        }
    }

    assert_eq!(provider_calls, 2, "model called before and after the tool");
    assert_eq!(tool_calls, 1, "one tool call round-tripped over HTTP");
    let outcome = outcome.expect("turn produced a terminal outcome");
    assert_eq!(outcome["status"].as_str(), Some("completed"));
    assert_eq!(outcome["text"].as_str(), Some("done"));
}
