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
//!    web framework would be bloat for a handful of read-only JSON routes.
//!
//! The server speaks just enough HTTP for every browser: request line +
//! headers (discarded beyond the path), `Connection: close`, explicit
//! `Content-Length`.
//!
//! Every `/api/*` route accepts `?project=<id>`: the id is resolved against
//! the cross-project rollup's `projects` table ([`global`]) and, when known,
//! that project's workspace root replaces the serving root for the request —
//! the dashboard's project switcher. Unknown ids fall back to the serving
//! workspace rather than erroring, so a stale dropdown never breaks the page.

mod codegraph;
mod db;
mod fsview;
mod global;

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
    // `?project=<id>` re-points the whole request at another registered
    // workspace (resolved from the rollup's own table — never a raw path
    // from the client). Unknown or vanished projects fall back to the
    // serving workspace.
    let effective_root = query_param(query, "project")
        .and_then(|id| global::resolve_project_root(&id))
        .unwrap_or_else(|| workspace_root.to_path_buf());
    let root = effective_root.as_path();
    let obs = Observatory::new(root);
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
        "/api/execution" => match query_param(query, "id").and_then(|v| v.parse::<i64>().ok()) {
            Some(id) => obs.execution(id),
            None => return Response::error("400 Bad Request", "missing ?id=<execution id>"),
        },
        "/api/models" => obs.models(),
        "/api/tools" => obs.tools(),
        "/api/files" => obs.files(),
        "/api/memory" => obs.memory(),
        "/api/mcp" => obs.mcp(),
        "/api/fleet" => obs.fleet(),
        "/api/activity" => obs.activity(),
        "/api/projects" => Ok(global::projects(workspace_root)),
        "/api/codegraph" => Ok(codegraph::snapshot(root)),
        "/api/skills" => Ok(fsview::skills(root)),
        "/api/mcp-servers" => Ok(fsview::mcp_servers(root)),
        "/api/config" => Ok(fsview::config(root)),
        "/api/memories" => Ok(fsview::memories(root)),
        "/api/explorations" => Ok(fsview::explorations(root)),
        "/api/rules" => obs.memory().map(|m| {
            serde_json::json!({
                "db": m["rules"].clone(),
                "files": fsview::rules_files(root),
            })
        }),
        "/api/reflections" => obs.reflection_ratings().map(|ratings| {
            serde_json::json!({
                "lessons": fsview::lessons(root),
                "ratings": ratings,
            })
        }),
        _ => return Response::error("404 Not Found", "no such route"),
    };
    match result {
        Ok(value) => Response::json(value),
        Err(e) => Response::error("500 Internal Server Error", &e.to_string()),
    }
}

/// Pull one `key=value` pair out of a query string.
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix(prefix.as_str()))
        .filter(|v| !v.is_empty())
        .map(str::to_string)
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

/// True when the request's `Host` header names a loopback address. Any other
/// Host (e.g. an attacker domain rebound to 127.0.0.1) is refused — the standard
/// DNS-rebinding defense for a header-less localhost server. A missing Host is
/// allowed: a browser `fetch` always sends one, so its absence means the request
/// did not originate from the web attack this guards against (raw curl, tests).
fn host_is_local(head: &str) -> bool {
    let host = head.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("host") {
            Some(value.trim())
        } else {
            None
        }
    });
    let Some(h) = host else {
        return true;
    };
    // Strip an optional :port, keeping bracketed IPv6 literals intact.
    let hostname = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        h.rsplit_once(':').map(|(hn, _)| hn).unwrap_or(h)
    };
    hostname == "localhost" || hostname == "::1" || hostname.starts_with("127.")
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
    let response = if !host_is_local(&head) {
        // DNS-rebinding defense: a web page that resolves an attacker domain to
        // 127.0.0.1 can otherwise read this loopback dashboard cross-origin
        // (prompts, touched-file paths, memory, code graph). A rebound request
        // carries the attacker's hostname in Host; refuse anything non-loopback.
        Response::error("403 Forbidden", "forbidden Host header")
    } else if method == "GET" {
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

    #[test]
    fn host_header_gates_dns_rebinding() {
        let local = [
            "GET /api/executions HTTP/1.1\r\nHost: 127.0.0.1:7787\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: localhost:7787\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: [::1]:7787\r\n\r\n",
            "GET / HTTP/1.1\r\n\r\n", // no Host (raw socket) — allowed
        ];
        for h in local {
            assert!(host_is_local(h), "should allow: {h:?}");
        }
        let remote = [
            "GET / HTTP/1.1\r\nHost: attacker.example:7787\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: evil.com\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 127evil.com\r\n\r\n",
        ];
        for h in remote {
            assert!(!host_is_local(h), "should refuse: {h:?}");
        }
    }

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
               (1, 'src/lib.rs', 'RU', 4, 1, '[]');
             CREATE TABLE execution_reflection (
               execution_id INTEGER PRIMARY KEY,
               prompt TEXT NOT NULL DEFAULT '',
               delivered INTEGER, self_rating INTEGER,
               what_went_well TEXT NOT NULL DEFAULT '',
               what_to_improve TEXT NOT NULL DEFAULT '',
               critique TEXT NOT NULL DEFAULT '',
               produced_output INTEGER NOT NULL DEFAULT 0,
               wrote_files INTEGER NOT NULL DEFAULT 0,
               truncated INTEGER NOT NULL DEFAULT 0,
               recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             INSERT INTO execution_reflection
               (execution_id, delivered, self_rating, what_to_improve)
             VALUES (1, 1, 8, 'read the failing test first');
             CREATE TABLE rules (
               rule_id TEXT PRIMARY KEY, contents TEXT NOT NULL,
               source TEXT NOT NULL,
               created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
               updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             INSERT INTO rules (rule_id, contents, source)
             VALUES ('no-vacuous-fixes', 'a fix must change production code', 'reflection');",
        )
        .unwrap();
        dir
    }

    /// Layer the filesystem-backed surfaces (skills, memories, rules,
    /// lessons, mcp.toml, settings.json, codegraph.db) onto a seeded
    /// workspace.
    fn seed_fs_surfaces(dir: &TempDir) {
        let dot = dir.path().join(".stella");
        std::fs::create_dir_all(dot.join("skills/my-skill")).unwrap();
        std::fs::write(
            dot.join("skills/my-skill/SKILL.md"),
            "---\nname: my-skill\ndescription: does the thing\n---\n# body",
        )
        .unwrap();
        std::fs::create_dir_all(dot.join("skills/learned")).unwrap();
        std::fs::write(
            dot.join("skills/learned/hard-won.md"),
            "---\nname: hard-won\ndescription: extracted from a failure\n---\n# lesson",
        )
        .unwrap();
        std::fs::create_dir_all(dot.join("memories")).unwrap();
        std::fs::write(
            dot.join("memories/build-quirk.md"),
            "The build needs -p flags.",
        )
        .unwrap();
        std::fs::create_dir_all(dot.join("rules")).unwrap();
        std::fs::write(dot.join("rules/style.md"), "Prefer witness tests.").unwrap();
        std::fs::write(
            dot.join("reflections.jsonl"),
            r#"{"lesson":"check the test's invariant first","domains":["testing"],"occurred_at":1700000000}
{"lesson":"verify dependency chains before citing them","domains":["docs"],"occurred_at":1700000100}
"#,
        )
        .unwrap();
        std::fs::write(
            dot.join("mcp.toml"),
            "[servers.github]\ntransport = \"stdio\"\ncmd = \"gh-mcp\"\nargs = [\"--stdio\"]\n\
             [servers.github.env]\nGITHUB_TOKEN = \"ghp_supersecret\"\n",
        )
        .unwrap();
        std::fs::write(
            dot.join("settings.json"),
            r#"{"providers":{"zai":{"api_key":"sk-live-topsecret","api_key_env":"ZAI_KEY"}},"agent_engine_config":{"agents":{"judge":{"model":"glm-5.2"}}}}"#,
        )
        .unwrap();
        let graph = Connection::open(dot.join("codegraph.db")).unwrap();
        graph
            .execute_batch(
                "CREATE TABLE code_graph_files (
                   id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                   language TEXT NOT NULL, content_sha256 TEXT NOT NULL,
                   mtime_ns INTEGER NOT NULL, indexed_at INTEGER NOT NULL);
                 CREATE TABLE code_graph_symbols (
                   id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL,
                   name TEXT NOT NULL, kind TEXT NOT NULL,
                   start_line INTEGER NOT NULL, end_line INTEGER NOT NULL);
                 CREATE TABLE code_graph_imports (
                   id INTEGER PRIMARY KEY, from_file_id INTEGER NOT NULL,
                   specifier TEXT NOT NULL, to_path TEXT, kind TEXT NOT NULL);
                 INSERT INTO code_graph_files VALUES
                   (1, 'crate-a/src/lib.rs',  'rust', 'x', 0, 0),
                   (2, 'crate-a/src/util.rs', 'rust', 'x', 0, 0),
                   (3, 'crate-b/src/lib.rs',  'rust', 'x', 0, 0),
                   (4, 'tools/x.py', 'python', 'x', 0, 0),
                   (5, 'tools/y.py', 'python', 'x', 0, 0);
                 INSERT INTO code_graph_symbols VALUES
                   (1, 1, 'run', 'function', 1, 9),
                   (2, 2, 'Util', 'struct', 1, 5);
                 INSERT INTO code_graph_imports VALUES
                   (1, 3, 'crate_a::util::Util', NULL, 'absolute'),
                   (2, 2, 'crate::run',          NULL, 'absolute'),
                   (3, 2, 'std::fs',             NULL, 'absolute'),
                   (4, 4, './y', 'tools/y.py',   'relative');",
            )
            .unwrap();
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
        assert_eq!(v["reflection"]["self_rating"], 8);
        let none = respond(ws.path(), "/api/execution?id=2");
        let v: serde_json::Value = serde_json::from_slice(&none.body).unwrap();
        assert_eq!(
            v["reflection"],
            serde_json::Value::Null,
            "unreflected runs stay null"
        );
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
            "/api/activity",
            "/api/projects",
            "/api/codegraph",
            "/api/skills",
            "/api/mcp-servers",
            "/api/config",
            "/api/memories",
            "/api/rules",
            "/api/reflections",
        ] {
            let response = respond(ws.path(), route);
            assert_eq!(response.status, "200 OK", "route {route}");
        }
    }

    #[test]
    fn activity_rolls_up_runs_tokens_and_tool_calls_by_day() {
        let ws = seeded_workspace();
        let response = respond(ws.path(), "/api/activity");
        let v: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        let days = v.as_array().unwrap();
        assert_eq!(days.len(), 1, "everything seeded lands on today");
        let d = &days[0];
        assert_eq!(d["runs"], 2);
        assert_eq!(d["resolved"], 1);
        assert_eq!(d["input_tokens"], 3000);
        assert_eq!(d["tool_calls"], 3);
        assert_eq!(d["tool_errors"], 1);
    }

    #[test]
    fn codegraph_resolves_rust_specifiers_and_relative_imports() {
        let ws = seeded_workspace();
        seed_fs_surfaces(&ws);
        let response = respond(ws.path(), "/api/codegraph");
        let v: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        let nodes = v["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 5);
        assert_eq!(v["total_symbols"], 2);
        let path_of = |i: usize| nodes[i]["path"].as_str().unwrap();
        let edges: Vec<(usize, usize)> = v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e[0].as_u64().unwrap() as usize,
                    e[1].as_u64().unwrap() as usize,
                )
            })
            .collect();
        let has = |from: &str, to: &str| {
            edges
                .iter()
                .any(|&(a, b)| path_of(a) == from && path_of(b) == to)
        };
        // Cross-crate `crate_a::util::Util`, crate-local `crate::run`, and
        // the indexer-resolved Python relative import.
        assert!(has("crate-b/src/lib.rs", "crate-a/src/util.rs"));
        assert!(has("crate-a/src/util.rs", "crate-a/src/lib.rs"));
        assert!(has("tools/x.py", "tools/y.py"));
        // `std::fs` resolves to nothing.
        assert_eq!(edges.len(), 3);
    }

    #[test]
    fn skills_list_project_scope_and_learned_flags() {
        let ws = seeded_workspace();
        seed_fs_surfaces(&ws);
        let response = respond(ws.path(), "/api/skills");
        let v: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        let rows = v.as_array().unwrap();
        let find = |name: &str| rows.iter().find(|r| r["name"] == name);
        let skill = find("my-skill").expect("project skill listed");
        assert_eq!(skill["scope"], "project");
        assert_eq!(skill["learned"], false);
        assert_eq!(skill["description"], "does the thing");
        let learned = find("hard-won").expect("learned skill listed");
        assert_eq!(learned["learned"], true);
    }

    #[test]
    fn explorations_route_reports_per_map_freshness() {
        let ws = seeded_workspace();
        let dir = ws.path().join(".stella/explorations");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(ws.path().join("covered.rs"), "fn mapped() {}").unwrap();
        // sha256("fn mapped() {}") — computed with the same encoding the
        // producer uses; freshness must verify against the live file.
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"fn mapped() {}");
        let fresh_hash: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        std::fs::write(
            dir.join("zone.json"),
            serde_json::json!({
                "slice": "zone", "title": "Zone", "summary": "s", "content": "body",
                "files": ["covered.rs"], "created_at_ms": 2u64,
                "manifest": { "covered.rs": fresh_hash, "gone.rs": "0000" },
                "status": "complete", "pid": 42u32
            })
            .to_string(),
        )
        .unwrap();
        // A pre-manifest (v1) record must report "unknown", not crash.
        std::fs::write(
            dir.join("legacy.json"),
            serde_json::json!({
                "slice": "legacy", "title": "Old", "summary": "s", "content": "b",
                "files": [], "created_at_ms": 1u64
            })
            .to_string(),
        )
        .unwrap();

        let response = respond(ws.path(), "/api/explorations");
        let v: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        // Newest first.
        assert_eq!(rows[0]["slice"], "zone");
        assert_eq!(rows[0]["freshness"], "drifted");
        assert_eq!(rows[0]["missing"][0], "gone.rs");
        assert!(rows[0]["changed"].as_array().unwrap().is_empty());
        assert_eq!(rows[0]["manifest_files"], 2);
        assert_eq!(rows[1]["slice"], "legacy");
        assert_eq!(rows[1]["freshness"], "unknown");
        // Bodies are sized, never served in the listing.
        assert!(rows[0]["content"].is_null());
        assert_eq!(rows[0]["content_chars"], 4);
    }

    #[test]
    fn mcp_servers_expose_names_but_never_credential_values() {
        let ws = seeded_workspace();
        seed_fs_surfaces(&ws);
        let response = respond(ws.path(), "/api/mcp-servers");
        let body = String::from_utf8(response.body).unwrap();
        assert!(body.contains("github"));
        assert!(body.contains("GITHUB_TOKEN"), "env var names are shown");
        assert!(
            !body.contains("ghp_supersecret"),
            "env var values must never be served"
        );
    }

    #[test]
    fn config_serves_scope_chain_with_secrets_redacted() {
        let ws = seeded_workspace();
        seed_fs_surfaces(&ws);
        let response = respond(ws.path(), "/api/config");
        let body = String::from_utf8(response.body).unwrap();
        assert!(!body.contains("sk-live-topsecret"), "api keys are redacted");
        assert!(body.contains("ZAI_KEY"), "env var *names* survive");
        assert!(body.contains("glm-5.2"), "engine config is visible");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let scopes = v["scopes"].as_array().unwrap();
        assert_eq!(scopes.len(), 3);
        let project = scopes.iter().find(|s| s["scope"] == "project").unwrap();
        assert_eq!(project["exists"], true);
    }

    #[test]
    fn memories_rules_and_reflections_come_from_disk_and_db() {
        let ws = seeded_workspace();
        seed_fs_surfaces(&ws);
        let memories: serde_json::Value =
            serde_json::from_slice(&respond(ws.path(), "/api/memories").body).unwrap();
        assert_eq!(memories[0]["name"], "build-quirk");
        let rules: serde_json::Value =
            serde_json::from_slice(&respond(ws.path(), "/api/rules").body).unwrap();
        assert_eq!(rules["db"][0]["rule_id"], "no-vacuous-fixes");
        assert_eq!(rules["files"][0]["name"], "style");
        let refl: serde_json::Value =
            serde_json::from_slice(&respond(ws.path(), "/api/reflections").body).unwrap();
        assert_eq!(refl["lessons"].as_array().unwrap().len(), 2);
        let ratings = refl["ratings"].as_array().unwrap();
        assert_eq!(ratings.len(), 1);
        assert_eq!(ratings[0]["self_rating"], 8);
        assert_eq!(ratings[0]["what_to_improve"], "read the failing test first");
    }

    #[test]
    fn project_param_drills_into_another_registered_workspace() {
        // Two workspaces: `home` is empty, `other` is seeded. A usage.db in a
        // private STELLA_DATA_DIR registers `other`; ?project= must re-point
        // the request at it, and unknown ids must fall back to `home`.
        let home = TempDir::new().unwrap();
        let other = seeded_workspace();
        let data = TempDir::new().unwrap();
        let usage = Connection::open(data.path().join("usage.db")).unwrap();
        usage
            .execute_batch(&format!(
                "CREATE TABLE projects (
                   project_id TEXT PRIMARY KEY, name TEXT NOT NULL,
                   root_path TEXT NOT NULL, first_seen_at TEXT NOT NULL,
                   last_seen_at TEXT NOT NULL);
                 CREATE TABLE execution_rollup (
                   project_id TEXT NOT NULL, execution_id INTEGER NOT NULL,
                   kind TEXT NOT NULL, prompt_digest TEXT NOT NULL,
                   prompt_preview TEXT NOT NULL DEFAULT '',
                   model TEXT NOT NULL, provider TEXT NOT NULL,
                   outcome TEXT NOT NULL, cost_usd REAL NOT NULL,
                   input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
                   duration_ms INTEGER NOT NULL, tool_calls INTEGER NOT NULL,
                   files_written INTEGER NOT NULL, produced_output INTEGER NOT NULL,
                   self_rating INTEGER, started_at TEXT NOT NULL,
                   PRIMARY KEY (project_id, execution_id));
                 INSERT INTO projects VALUES
                   ('feedbeef00000001', 'other', '{}', '2026-01-01', '2026-01-02');",
                other.path().display()
            ))
            .unwrap();
        // SAFETY: env mutation in tests — this is the only test that sets
        // STELLA_DATA_DIR, and every assertion runs before it's removed.
        unsafe { std::env::set_var("STELLA_DATA_DIR", data.path()) };
        let drilled = respond(home.path(), "/api/overview?project=feedbeef00000001");
        let unknown = respond(home.path(), "/api/overview?project=doesnotexist");
        let listed = respond(home.path(), "/api/projects");
        unsafe { std::env::remove_var("STELLA_DATA_DIR") };
        let v: serde_json::Value = serde_json::from_slice(&drilled.body).unwrap();
        assert_eq!(v["runs"], 2, "?project= reads the other workspace");
        let v: serde_json::Value = serde_json::from_slice(&unknown.body).unwrap();
        assert_eq!(v["runs"], 0, "unknown ids fall back to the serving root");
        let v: serde_json::Value = serde_json::from_slice(&listed.body).unwrap();
        assert_eq!(v["available"], true);
        assert_eq!(v["projects"][0]["name"], "other");
        assert_eq!(v["projects"][0]["has_store"], true);
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
            // A loopback Host: the DNS-rebinding gate (host_is_local, its
            // own unit test above) would 403 a non-local name like the bare
            // `x` this test sent before the gate existed.
            .write_all(b"GET /api/overview HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).await.unwrap();
        assert!(body.starts_with("HTTP/1.1 200 OK"));
        assert!(body.contains("\"runs\":2"));
        server.abort();
    }
}
