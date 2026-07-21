//! The HTTP/SSE transport over [`Session`].
//!
//! Endpoints (all under a bearer token except `/healthz`):
//!
//! | Method + path | Purpose |
//! |---|---|
//! | `GET /healthz` | liveness |
//! | `POST /v1/turns` | start a turn ([`TurnRequest`] body) → `{ "turn_id": … }` |
//! | `GET /v1/turns/{id}/events` | SSE stream of [`ServerFrame`]s until `turn_complete` |
//! | `POST /v1/turns/{id}/tool-result` | answer a `tool_request` ([`ToolResultIn`]) |
//! | `POST /v1/turns/{id}/provider-result` | answer a `provider_request` ([`ProviderResultIn`]) |
//!
//! The SSE stream is the engine → host direction; the two result POSTs are the
//! host → engine direction. Together they are the reverse tool-call protocol —
//! the engine never runs a model or tool call itself.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use stella_core::{BudgetGuard, EngineConfig};
use stella_protocol::{BudgetMode, CompletionMessage, ToolSchema};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::frame::{ProviderOutcomeIn, ProviderResultIn, ServerFrame, ToolResultIn};
use crate::http::{read_request, write_json, write_sse_frame, write_sse_head};
use crate::pending::Pending;
use crate::session::{Session, SessionSpec};

/// How to bind and authenticate the server.
pub struct ServeConfig {
    /// Address to bind. `127.0.0.1:0` picks a free loopback port (tests); a
    /// containerized deployment binds `0.0.0.0:<port>`.
    pub bind: SocketAddr,
    /// The bearer token every request (except `/healthz`) must present. This is
    /// the auth gate — the server may bind a non-loopback address behind the
    /// host's private network.
    pub token: String,
}

/// Body of `POST /v1/turns`. The host owns prompt assembly, model selection, and
/// the tool set; engine knobs are optional overrides on top of the defaults.
#[derive(Debug, Deserialize)]
struct TurnRequest {
    provider_id: String,
    #[serde(default)]
    tools: Vec<ToolSchema>,
    messages: Vec<CompletionMessage>,
    #[serde(default)]
    budget: BudgetSpec,
    #[serde(default)]
    max_steps: Option<usize>,
}

/// Spend policy for a turn — the serializable projection of a [`BudgetGuard`].
#[derive(Debug, Deserialize)]
struct BudgetSpec {
    #[serde(default = "budget_mode_off")]
    mode: BudgetMode,
    #[serde(default)]
    turn_limit_usd: Option<f64>,
    #[serde(default)]
    session_limit_usd: Option<f64>,
}

impl Default for BudgetSpec {
    fn default() -> Self {
        Self {
            mode: BudgetMode::Off,
            turn_limit_usd: None,
            session_limit_usd: None,
        }
    }
}

fn budget_mode_off() -> BudgetMode {
    BudgetMode::Off
}

/// Response to `POST /v1/turns`.
#[derive(Debug, Serialize)]
struct TurnCreated<'a> {
    turn_id: &'a str,
}

/// One registered turn. `pending` answers reverse requests (shared, always
/// available); `session` is taken exactly once by the SSE stream.
struct Entry {
    pending: Pending,
    session: Mutex<Option<Session>>,
}

/// Shared server state across connections.
struct ServerState {
    token: String,
    turns: Mutex<HashMap<String, Arc<Entry>>>,
    counter: AtomicU64,
}

impl ServerState {
    fn turns(&self) -> MutexGuard<'_, HashMap<String, Arc<Entry>>> {
        self.turns.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn lookup(&self, id: &str) -> Option<Arc<Entry>> {
        self.turns().get(id).cloned()
    }
}

/// Bind and serve until the accept loop errors. `on_ready` fires once with the
/// bound address (so a `:0` bind can report its port).
pub async fn serve(config: ServeConfig, on_ready: impl FnOnce(SocketAddr)) -> std::io::Result<()> {
    let listener = TcpListener::bind(config.bind).await?;
    on_ready(listener.local_addr()?);
    let state = Arc::new(ServerState {
        token: config.token,
        turns: Mutex::new(HashMap::new()),
        counter: AtomicU64::new(0),
    });
    loop {
        let (stream, _) = listener.accept().await?;
        let state = Arc::clone(&state);
        // Per-connection errors (client hangup, bad request) stay local to the
        // connection; the accept loop keeps serving.
        tokio::spawn(async move {
            let _ = handle_conn(stream, state).await;
        });
    }
}

async fn handle_conn(mut stream: TcpStream, state: Arc<ServerState>) -> std::io::Result<()> {
    let Some(req) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let path = req.path.split('?').next().unwrap_or(&req.path);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if req.method == "GET" && segs.as_slice() == ["healthz"] {
        return write_json(&mut stream, "200 OK", br#"{"status":"ok"}"#).await;
    }
    if req.bearer() != Some(state.token.as_str()) {
        return write_json(
            &mut stream,
            "401 Unauthorized",
            br#"{"error":"missing or invalid bearer token"}"#,
        )
        .await;
    }

    match (req.method.as_str(), segs.as_slice()) {
        ("POST", ["v1", "turns"]) => handle_create(&mut stream, &state, &req.body).await,
        ("GET", ["v1", "turns", id, "events"]) => handle_events(&mut stream, &state, id).await,
        ("POST", ["v1", "turns", id, "tool-result"]) => {
            handle_tool_result(&mut stream, &state, id, &req.body).await
        }
        ("POST", ["v1", "turns", id, "provider-result"]) => {
            handle_provider_result(&mut stream, &state, id, &req.body).await
        }
        _ => write_json(&mut stream, "404 Not Found", br#"{"error":"not found"}"#).await,
    }
}

async fn handle_create(
    stream: &mut TcpStream,
    state: &ServerState,
    body: &[u8],
) -> std::io::Result<()> {
    let turn: TurnRequest = match serde_json::from_slice(body) {
        Ok(turn) => turn,
        Err(err) => {
            return write_json(
                stream,
                "400 Bad Request",
                &error_body(&format!("invalid turn request: {err}")),
            )
            .await;
        }
    };

    let mut config = EngineConfig::default();
    if let Some(max_steps) = turn.max_steps {
        config.max_steps = max_steps;
    }
    let spec = SessionSpec {
        provider_id: turn.provider_id,
        tools: turn.tools,
        messages: turn.messages,
        config,
        budget: BudgetGuard::new(
            turn.budget.mode,
            turn.budget.turn_limit_usd,
            turn.budget.session_limit_usd,
        ),
    };

    let session = Session::start(spec);
    let id = format!("turn-{}", state.counter.fetch_add(1, Ordering::Relaxed));
    let entry = Arc::new(Entry {
        pending: session.pending(),
        session: Mutex::new(Some(session)),
    });
    state.turns().insert(id.clone(), entry);

    let body = serde_json::to_vec(&TurnCreated { turn_id: &id }).unwrap_or_default();
    write_json(stream, "200 OK", &body).await
}

async fn handle_events(
    stream: &mut TcpStream,
    state: &ServerState,
    id: &str,
) -> std::io::Result<()> {
    let Some(entry) = state.lookup(id) else {
        return write_json(stream, "404 Not Found", &error_body("unknown turn")).await;
    };
    // Take the session out in its own scope so the (non-`Send`) mutex guard is
    // dropped before any `.await` — the connection future must stay `Send`.
    let taken = {
        entry
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    };
    let mut session = match taken {
        Some(session) => session,
        None => {
            return write_json(
                stream,
                "409 Conflict",
                &error_body("events are already being streamed for this turn"),
            )
            .await;
        }
    };

    write_sse_head(stream).await?;
    while let Some(frame) = session.next_frame().await {
        let done = matches!(frame, ServerFrame::TurnComplete { .. });
        let json = serde_json::to_string(&frame).unwrap_or_else(|_| "{}".to_string());
        if write_sse_frame(stream, &json).await.is_err() {
            break;
        }
        if done {
            break;
        }
    }
    // The turn is finished streaming; drop it so its thread and registry entry
    // are reclaimed.
    state.turns().remove(id);
    stream.shutdown().await
}

async fn handle_tool_result(
    stream: &mut TcpStream,
    state: &ServerState,
    id: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let Some(entry) = state.lookup(id) else {
        return write_json(stream, "404 Not Found", &error_body("unknown turn")).await;
    };
    let result: ToolResultIn = match serde_json::from_slice(body) {
        Ok(result) => result,
        Err(err) => {
            return write_json(
                stream,
                "400 Bad Request",
                &error_body(&format!("invalid tool result: {err}")),
            )
            .await;
        }
    };
    match entry
        .pending
        .resolve_tool(&result.request_id, result.output)
    {
        Ok(()) => write_json(stream, "200 OK", br#"{"status":"ok"}"#).await,
        Err(err) => write_json(stream, "409 Conflict", &error_body(&err.to_string())).await,
    }
}

async fn handle_provider_result(
    stream: &mut TcpStream,
    state: &ServerState,
    id: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let Some(entry) = state.lookup(id) else {
        return write_json(stream, "404 Not Found", &error_body("unknown turn")).await;
    };
    let posted: ProviderResultIn = match serde_json::from_slice(body) {
        Ok(posted) => posted,
        Err(err) => {
            return write_json(
                stream,
                "400 Bad Request",
                &error_body(&format!("invalid provider result: {err}")),
            )
            .await;
        }
    };
    let result = match posted.outcome {
        ProviderOutcomeIn::Ok { result } => Ok(result),
        ProviderOutcomeIn::Error { error } => Err(error.into()),
    };
    match entry.pending.resolve_provider(&posted.request_id, result) {
        Ok(()) => write_json(stream, "200 OK", br#"{"status":"ok"}"#).await,
        Err(err) => write_json(stream, "409 Conflict", &error_body(&err.to_string())).await,
    }
}

fn error_body(message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "error": message })).unwrap_or_default()
}
