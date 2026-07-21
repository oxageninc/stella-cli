//! A tiny hand-rolled HTTP/1.1 layer, following `stella-observatory`'s idiom
//! (no web-framework dependency) but extended for what an engine server needs
//! the read-only dashboard did not: request bodies (POST), bearer auth, and
//! long-lived Server-Sent-Events responses.
//!
//! Deliberately minimal: one request per connection, `Connection: close`, an
//! SSE writer that streams frames until the turn ends and then closes. Enough
//! for a governed sidecar behind the host, not a general-purpose server.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Cap on the request head + body we will buffer. A turn request carries an
/// assembled conversation, so it is larger than the dashboard's 8 KiB GET cap,
/// but still bounded — a host that needs more is misusing the endpoint.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// One parsed HTTP request.
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    /// Header names lowercased for case-insensitive lookup; values trimmed.
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The bearer token from `Authorization: Bearer <token>`, if present.
    pub fn bearer(&self) -> Option<&str> {
        self.header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
    }
}

/// Read and parse one request (head + `Content-Length` body). Returns `None` on
/// a clean early hangup or a malformed/over-cap request — the caller closes the
/// connection without a response, exactly as the observatory does.
pub(crate) async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0_u8; 8192];
    let head_end = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return Ok(None);
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut req_parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (req_parts.next(), req_parts.next()) else {
        return Ok(None);
    };

    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }

    let content_length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_REQUEST_BYTES {
        return Ok(None);
    }

    let body_start = head_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(Some(Request {
        method: method.to_string(),
        path: path.to_string(),
        headers,
        body,
    }))
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Write a one-shot response (status, JSON content type, body) and close.
pub(crate) async fn write_json(
    stream: &mut TcpStream,
    status: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len(),
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

/// Write the SSE response head, leaving the connection open to stream frames.
pub(crate) async fn write_sse_head(stream: &mut TcpStream) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).await
}

/// Write one SSE `data:` frame carrying a JSON payload.
pub(crate) async fn write_sse_frame(stream: &mut TcpStream, json: &str) -> std::io::Result<()> {
    stream.write_all(b"data: ").await?;
    stream.write_all(json.as_bytes()).await?;
    stream.write_all(b"\n\n").await
}
