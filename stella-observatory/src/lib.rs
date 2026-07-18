//! The Stella Observatory — a local, loopback-only dashboard over the
//! workspace's own telemetry (`.stella/store.db`, `.stella/fleet.db`).
//!
//! Design constraints, in order:
//!
//! 1. **No phone-home, structurally.** The listener binds `127.0.0.1` and
//!    nothing here constructs an outbound connection. The page is a single
//!    embedded HTML file with zero external references (no CDN, no fonts,
//!    no analytics) — it renders fully offline.
//! 2. **Observer never mutates.** Every database open is
//!    `SQLITE_OPEN_READ_ONLY` (see [`db`]); a live `stella` session writing
//!    telemetry is never blocked or altered by a dashboard tab.
//! 3. **No new dependencies.** The HTTP layer is a deliberately tiny
//!    GET-only HTTP/1.1 responder over `tokio`'s `TcpListener` — the
//!    workspace already ships everything required. A router the size of a
//!    web framework would be bloat for nine read-only JSON routes.
//!
//! The server speaks just enough HTTP for every browser: request line +
//! headers (discarded beyond the path), `Connection: close`, explicit
//! `Content-Length`.

mod db;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub use db::{DbError, Observatory};

/// The dashboard page, embedded so the binary is self-contained.
const INDEX_HTML: &str = include_str!("assets/index.html");
/// The Stella mark, served for the header + favicon.
const MARK_SVG: &str = include_str!("assets/mark.svg");

/// Errors starting or running the observatory server.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The loopback listener could not be created (port in use, etc.).
    #[error("cannot bind 127.0.0.1:{port}: {source}")]
    Bind {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    /// Accepting a connection failed fatally.
    #[error("accept failed: {0}")]
    Accept(#[from] std::io::Error),
}

/// A minimal HTTP response: status line, content type, body bytes.
pub struct Response {
    pub status: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    fn json(value: serde_json::Value) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/json",
            body: value.to_string().into_bytes(),
        }
    }

    fn error(status: &'static str, message: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::json!({ "error": message })
                .to_string()
                .into_bytes(),
        }
    }
}

/// Route a request path to a response. Pure function of (workspace, path) —
/// the unit tests drive this directly, no sockets involved.
pub fn respond(workspace_root: &Path, path: &str) -> Response {
    let (route, query) = match path.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (path, None),
    };
    let obs = Observatory::new(workspace_root);
    let result = match route {
        "/" | "/index.html" => {
            return Response {
                status: "200 OK",
                content_type: "text/html; charset=utf-8",
                body: INDEX_HTML.as_bytes().to_vec(),
            };
        }
        "/assets/mark.svg" | "/favicon.svg" => {
            return Response {
                status: "200 OK",
                content_type: "image/svg+xml",
                body: MARK_SVG.as_bytes().to_vec(),
            };
        }
        "/api/meta" => Ok(obs.meta()),
        "/api/overview" => obs.overview(),
        "/api/executions" => obs.executions(),
        "/api/execution" => match query_id(query) {
            Some(id) => obs.execution(id),
            None => return Response::error("400 Bad Request", "missing ?id=<execution id>"),
        },
        "/api/models" => obs.models(),
        "/api/tools" => obs.tools(),
        "/api/files" => obs.files(),
        "/api/memory" => obs.memory(),
        "/api/mcp" => obs.mcp(),
        "/api/fleet" => obs.fleet(),
        _ => return Response::error("404 Not Found", "no such route"),
    };
    match result {
        Ok(value) => Response::json(value),
        Err(e) => Response::error("500 Internal Server Error", &e.to_string()),
    }
}

/// Parse `id=<i64>` out of a query string.
fn query_id(query: Option<&str>) -> Option<i64> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("id="))
        .and_then(|v| v.parse().ok())
}

/// Bind the observatory on `127.0.0.1:port` and serve until the process
/// exits. `port` 0 picks a free port. Calls `on_ready` once with the bound
/// address (the CLI prints the URL from it).
pub async fn serve(
    workspace_root: PathBuf,
    port: u16,
    on_ready: impl FnOnce(SocketAddr),
) -> Result<(), ServeError> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|source| ServeError::Bind { port, source })?;
    let addr = listener.local_addr().map_err(ServeError::Accept)?;
    on_ready(addr);
    loop {
        let (stream, _) = listener.accept().await?;
        let root = workspace_root.clone();
        tokio::spawn(async move {
            // Per-connection errors (bad request line, client hangup) only
            // affect that connection; the accept loop keeps serving.
            let _ = handle(stream, &root).await;
        });
    }
}

/// Read one request head, answer it, close. GET only, 8 KiB head cap.
async fn handle(mut stream: TcpStream, workspace_root: &Path) -> std::io::Result<()> {
    let mut buf = vec![0_u8; 8192];
    let mut read = 0;
    // Read until the end of the request head (or the cap — a GET with no
    // body never legitimately exceeds it).
    while read < buf.len() {
        let n = stream.read(&mut buf[read..]).await?;
        if n == 0 {
            break;
        }
        read += n;
        if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf[..read]);
    let mut parts = head.split_whitespace();
    let (method, path) = match (parts.next(), parts.next()) {
        (Some(m), Some(p)) => (m, p),
        _ => return Ok(()),
    };
    let response = if method == "GET" {
        respond(workspace_root, path)
    } else {
        Response::error("405 Method Not Allowed", "GET only")
    };
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len(),
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;

    /// Build a workspace with a seeded `.stella/store.db` shaped like the
    /// real schema (the subset the observatory reads).
    fn seeded_workspace() -> TempDir {
        let dir = TempDir::new().unwrap();
        let dot = dir.path().join(".stella");
        std::fs::create_dir_all(&dot).unwrap();
        let conn = Connection::open(dot.join("store.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE executions (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               kind TEXT NOT NULL, prompt TEXT NOT NULL,
               provider TEXT NOT NULL, model TEXT NOT NULL,
               started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
               finished_at TEXT, outcome TEXT,
               cost_usd REAL NOT NULL DEFAULT 0);
             CREATE TABLE telemetry (
               execution_id INTEGER NOT NULL, step INTEGER NOT NULL,
               ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
               provider TEXT NOT NULL, model TEXT NOT NULL,
               input_tokens INTEGER NOT NULL,
               estimated_input_tokens INTEGER NOT NULL DEFAULT 0,
               output_tokens INTEGER NOT NULL,
               cache_read_tokens INTEGER NOT NULL,
               cache_miss_tokens INTEGER NOT NULL,
               cache_write_tokens INTEGER NOT NULL DEFAULT 0,
               cost_usd REAL NOT NULL, duration_ms INTEGER NOT NULL,
               retries INTEGER NOT NULL, tool_calls INTEGER NOT NULL);
             CREATE TABLE tool_calls (
               execution_id INTEGER NOT NULL, seq INTEGER NOT NULL,
               call_id TEXT NOT NULL DEFAULT '', name TEXT NOT NULL,
               surface TEXT NOT NULL DEFAULT 'native',
               args_json TEXT NOT NULL DEFAULT '{}',
               args_digest TEXT NOT NULL DEFAULT '',
               reason TEXT NOT NULL DEFAULT '',
               ok INTEGER NOT NULL DEFAULT 1,
               error TEXT NOT NULL DEFAULT '',
               bytes_out INTEGER NOT NULL DEFAULT 0,
               duration_ms INTEGER NOT NULL DEFAULT 0,
               ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             CREATE TABLE files_touched (
               execution_id INTEGER NOT NULL, path TEXT NOT NULL,
               ops TEXT NOT NULL, lines_added INTEGER NOT NULL DEFAULT 0,
               lines_removed INTEGER NOT NULL DEFAULT 0,
               events TEXT NOT NULL DEFAULT '[]');
             INSERT INTO executions
               (kind, prompt, provider, model, outcome, cost_usd)
             VALUES
               ('run', 'add a function', 'zai', 'glm-5.2', 'completed', 0.03),
               ('goal', 'make tests pass', 'local', 'llama', 'goal_unmet', 0.0);
             INSERT INTO telemetry VALUES
               (1, 1, CURRENT_TIMESTAMP, 'zai', 'glm-5.2',
                1000, 0, 200, 400, 600, 50, 0.03, 1500, 0, 2),
               (2, 1, CURRENT_TIMESTAMP, 'local', 'llama',
                2000, 0, 100, 0, 2000, 0, 0.0, 900, 1, 1);
             INSERT INTO tool_calls
               (execution_id, seq, name, ok, error, bytes_out, duration_ms)
             VALUES
               (1, 1, 'read_file', 1, '', 2048, 12),
               (1, 2, 'edit_file', 1, '', 64, 3),
               (2, 1, 'bash', 0, 'exit 1', 0, 40);
             INSERT INTO files_touched VALUES
               (1, 'src/lib.rs', 'RU', 4, 1, '[]');",
        )
        .unwrap();
        dir
    }

    #[test]
    fn overview_aggregates_runs_cost_and_tokens() {
        let ws = seeded_workspace();
        let response = respond(ws.path(), "/api/overview");
        let v: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(v["runs"], 2);
        assert_eq!(v["resolved"], 1);
        assert_eq!(v["input_tokens"], 3000);
        assert_eq!(v["output_tokens"], 300);
        assert_eq!(v["timeline"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn models_mirror_stats_semantics() {
        let ws = seeded_workspace();
        let response = respond(ws.path(), "/api/models");
        let v: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        // Ordered by cost desc: zai first, then local (off-grid).
        assert_eq!(rows[0]["provider"], "zai");
        assert_eq!(rows[0]["resolve_rate"], 1.0);
        assert_eq!(rows[1]["division"], "off-grid");
        assert_eq!(rows[1]["cost_per_resolved_usd"], serde_json::Value::Null);
    }

    #[test]
    fn execution_detail_includes_steps_tools_files() {
        let ws = seeded_workspace();
        let response = respond(ws.path(), "/api/execution?id=1");
        let v: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["steps"].as_array().unwrap().len(), 1);
        assert_eq!(v["tools"].as_array().unwrap().len(), 2);
        assert_eq!(v["files"][0]["path"], "src/lib.rs");
        assert_eq!(v["reflection"], serde_json::Value::Null);
    }

    #[test]
    fn tool_leaderboard_counts_errors() {
        let ws = seeded_workspace();
        let response = respond(ws.path(), "/api/tools");
        let v: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        let bash = v
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "bash")
            .unwrap();
        assert_eq!(bash["calls"], 1);
        assert_eq!(bash["errors"], 1);
    }

    #[test]
    fn empty_workspace_degrades_to_empty_payloads_not_errors() {
        let ws = TempDir::new().unwrap();
        for route in [
            "/api/overview",
            "/api/executions",
            "/api/models",
            "/api/tools",
            "/api/files",
            "/api/memory",
            "/api/mcp",
            "/api/fleet",
        ] {
            let response = respond(ws.path(), route);
            assert_eq!(response.status, "200 OK", "route {route}");
        }
    }

    #[test]
    fn unknown_route_is_404_and_missing_id_is_400() {
        let ws = TempDir::new().unwrap();
        assert_eq!(respond(ws.path(), "/api/nope").status, "404 Not Found");
        assert_eq!(
            respond(ws.path(), "/api/execution").status,
            "400 Bad Request"
        );
        assert_eq!(
            respond(ws.path(), "/api/execution?id=abc").status,
            "400 Bad Request"
        );
    }

    #[test]
    fn index_and_mark_are_embedded() {
        let ws = TempDir::new().unwrap();
        let index = respond(ws.path(), "/");
        assert_eq!(index.content_type, "text/html; charset=utf-8");
        assert!(
            String::from_utf8(index.body)
                .unwrap()
                .contains("Observatory")
        );
        let mark = respond(ws.path(), "/assets/mark.svg");
        assert_eq!(mark.content_type, "image/svg+xml");
    }

    /// The page must be fully self-contained: any http(s) URL in the HTML
    /// would be an outbound fetch from the user's browser — a phone-home.
    #[test]
    fn dashboard_html_has_no_external_references() {
        for needle in ["http://", "https://", "//cdn", "@import", "integrity="] {
            assert!(
                !INDEX_HTML.contains(needle),
                "embedded dashboard must not reference {needle}"
            );
        }
    }

    #[tokio::test]
    async fn serves_over_a_real_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ws = seeded_workspace();
        let root = ws.path().to_path_buf();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let _ = serve(root, 0, move |addr| {
                let _ = tx.send(addr);
            })
            .await;
        });
        let addr = rx.await.unwrap();
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /api/overview HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).await.unwrap();
        assert!(body.starts_with("HTTP/1.1 200 OK"));
        assert!(body.contains("\"runs\":2"));
        server.abort();
    }
}
