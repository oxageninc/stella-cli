//! The engine's ports, remoted.
//!
//! Server-side the engine must never run a model call or a tool with ambient
//! authority — the whole containment story of ADR-033 Option B rests on this.
//! So [`RemoteProvider`] and [`RemoteToolExecutor`] implement the engine's
//! `Provider` / `ToolExecutor` ports by emitting a reverse-RPC request frame to
//! the host and awaiting the host's answer. The host runs the effect through
//! its own governance (`kernel.invoke()`, `@oxagen/ai`) and posts the result
//! back, which resolves the one-shot the port is parked on.
//!
//! Both ports are constructed on, and driven by, the session thread's
//! current-thread runtime; the one-shot receivers they await are fired from the
//! server runtime. That cross-runtime wake is the `!Send`-future bridge.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use stella_core::ports::ToolExecutor;
use stella_core::retry::Sleeper;
use stella_protocol::{
    CompletionRequest, CompletionResult, Provider, ProviderError, ToolOutput, ToolSchema,
};
use tokio::sync::{mpsc, oneshot};

use crate::frame::ServerFrame;
use crate::pending::Pending;

/// A Tokio-backed [`Sleeper`] for the session runtime's retry backoff. The
/// session runtime is built with the time driver enabled, so `sleep` resolves
/// there.
pub(crate) struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration_ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;
    }
}

/// The `Provider` port as a reverse-RPC to the host. `complete` emits a
/// [`ServerFrame::ProviderRequest`] and blocks the step on the host's answer.
pub(crate) struct RemoteProvider {
    id: String,
    frames: mpsc::UnboundedSender<ServerFrame>,
    pending: Pending,
    counter: AtomicU64,
}

impl RemoteProvider {
    pub(crate) fn new(
        id: String,
        frames: mpsc::UnboundedSender<ServerFrame>,
        pending: Pending,
    ) -> Self {
        Self {
            id,
            frames,
            pending,
            counter: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl Provider for RemoteProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        let request_id = format!("prov-{}", self.counter.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        // Register before emitting so the entry always exists by the time the
        // host can answer (register → send ordering closes the race).
        self.pending.register_provider(request_id.clone(), tx);
        if self
            .frames
            .send(ServerFrame::ProviderRequest {
                request_id,
                request: req,
            })
            .is_err()
        {
            // The host stream is gone; a disconnect mid-flight is a retryable
            // transport condition, classified here at the adapter as upstream.
            return Err(ProviderError::Transport(
                "serve host disconnected before the model call could be dispatched".to_string(),
            ));
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(ProviderError::Transport(
                "serve host dropped the model call without answering".to_string(),
            )),
        }
    }
}

/// The `ToolExecutor` port as a reverse-RPC to the host. `schemas` returns the
/// tool set the host advertised at session start (including `read_only` flags,
/// so the engine's partitioned concurrency still applies); `execute` emits a
/// [`ServerFrame::ToolRequest`] and blocks on the host's answer.
pub(crate) struct RemoteToolExecutor {
    schemas: Vec<ToolSchema>,
    frames: mpsc::UnboundedSender<ServerFrame>,
    pending: Pending,
    counter: AtomicU64,
}

impl RemoteToolExecutor {
    pub(crate) fn new(
        schemas: Vec<ToolSchema>,
        frames: mpsc::UnboundedSender<ServerFrame>,
        pending: Pending,
    ) -> Self {
        Self {
            schemas,
            frames,
            pending,
            counter: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl ToolExecutor for RemoteToolExecutor {
    fn schemas(&self) -> Vec<ToolSchema> {
        self.schemas.clone()
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        let request_id = format!("tool-{}", self.counter.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.register_tool(request_id.clone(), tx);
        if self
            .frames
            .send(ServerFrame::ToolRequest {
                request_id,
                name: name.to_string(),
                input: input.clone(),
            })
            .is_err()
        {
            // A tool failure is model-visible data, never an engine error — the
            // port contract is that `execute` never returns `Err`.
            return ToolOutput::Error {
                message: "serve host disconnected before the tool call could be dispatched"
                    .to_string(),
            };
        }
        match rx.await {
            Ok(output) => output,
            Err(_) => ToolOutput::Error {
                message: "serve host dropped the tool call without answering".to_string(),
            },
        }
    }
}
