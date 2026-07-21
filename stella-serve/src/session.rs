//! A single engine session: one turn driven on a dedicated OS thread.
//!
//! The engine's turn future is deliberately `!Send` (it holds provider futures
//! and the retry-jitter RNG across awaits — `stella-cli::fleet_cmd`), so it
//! cannot be `tokio::spawn`ed onto the server's multi-thread runtime. Instead
//! each session gets its own OS thread running a **current-thread** runtime that
//! `block_on`s `run_turn`; the server side communicates with it purely through
//! `Send` channels — an outbound [`ServerFrame`] stream and the shared
//! [`Pending`] one-shot registry. This is the fleet's bridge, reused for a
//! long-lived server instead of a batch worker.
//!
//! Scope note: one [`Session`] drives one turn. Multi-turn sessions (retaining
//! the message history across turns) and per-step checkpointing layer on top of
//! this without changing the transport — they are the next slice.

use std::thread::JoinHandle;

use stella_core::{BudgetGuard, Engine, EngineConfig};
use stella_protocol::{
    AgentEvent, CompletionMessage, CompletionResult, ProviderError, ToolOutput, ToolSchema,
};
use tokio::runtime::Builder;
use tokio::sync::mpsc;

use crate::error::ServeError;
use crate::frame::{ServerFrame, TurnOutcomeWire};
use crate::pending::Pending;
use crate::remote::{RemoteProvider, RemoteToolExecutor, TokioSleeper};

/// Everything needed to run one turn. The host assembles this — it owns prompt
/// construction, recall, model selection, and the tool set (advertised as
/// schemas here; execution is remoted). The engine only orchestrates.
pub struct SessionSpec {
    /// Stable provider id echoed on `Provider::id()` (telemetry/labeling only —
    /// the model call itself is remoted to the host).
    pub provider_id: String,
    /// Tool schemas the model may call, including `read_only` flags.
    pub tools: Vec<ToolSchema>,
    /// The already-assembled conversation to run this turn.
    pub messages: Vec<CompletionMessage>,
    /// Engine knobs (step cap, compaction budget, sampling). `Default` is a
    /// sane starting point.
    pub config: EngineConfig,
    /// Spend guard for the turn.
    pub budget: BudgetGuard,
}

/// A running session. The host drives it by reading [`ServerFrame`]s with
/// [`Session::next_frame`] and answering reverse-RPC requests with
/// [`Session::resolve_tool`] / [`Session::resolve_provider`].
pub struct Session {
    frames: mpsc::UnboundedReceiver<ServerFrame>,
    pending: Pending,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Spawn the session thread and begin the turn. Frames start arriving
    /// immediately; the terminal one is [`ServerFrame::TurnComplete`].
    pub fn start(spec: SessionSpec) -> Session {
        let (frame_tx, frame_rx) = mpsc::unbounded_channel::<ServerFrame>();
        let pending = Pending::default();
        let thread = {
            let pending = pending.clone();
            std::thread::spawn(move || run_session(spec, frame_tx, pending))
        };
        Session {
            frames: frame_rx,
            pending,
            thread: Some(thread),
        }
    }

    /// Await the next frame from the engine. `None` once the session thread has
    /// finished and dropped its sender (i.e. after `TurnComplete`).
    pub async fn next_frame(&mut self) -> Option<ServerFrame> {
        self.frames.recv().await
    }

    /// A handle to this session's reverse-RPC registry, cloneable and `Send`.
    /// The HTTP layer keeps one alongside the session so a POST resolve handler
    /// can answer a reverse request without holding the (exclusively-streamed)
    /// session itself.
    pub fn pending(&self) -> Pending {
        self.pending.clone()
    }

    /// Answer a [`ServerFrame::ToolRequest`] with the host-run tool output.
    pub fn resolve_tool(&self, request_id: &str, output: ToolOutput) -> Result<(), ServeError> {
        self.pending.resolve_tool(request_id, output)
    }

    /// Answer a [`ServerFrame::ProviderRequest`] with a completed model result.
    pub fn resolve_provider(
        &self,
        request_id: &str,
        result: CompletionResult,
    ) -> Result<(), ServeError> {
        self.pending.resolve_provider(request_id, Ok(result))
    }

    /// Answer a [`ServerFrame::ProviderRequest`] with a classified failure.
    pub fn fail_provider(&self, request_id: &str, error: ProviderError) -> Result<(), ServeError> {
        self.pending.resolve_provider(request_id, Err(error))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Unblock any parked reverse request so the engine turn aborts cleanly
        // and the thread can finish, rather than leaking a thread wedged on a
        // host that will never answer. We do not join (a still-running turn
        // could take arbitrarily long) — the thread detaches and drains itself.
        self.pending.clear();
        drop(self.thread.take());
    }
}

/// The body of the session thread: build a current-thread runtime, construct the
/// remoted ports, and drive one turn to its outcome.
fn run_session(spec: SessionSpec, frame_tx: mpsc::UnboundedSender<ServerFrame>, pending: Pending) {
    let runtime = match Builder::new_current_thread().enable_time().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = frame_tx.send(ServerFrame::TurnComplete {
                outcome: TurnOutcomeWire::Aborted {
                    reason: ServeError::RuntimeBuild(err.to_string()).to_string(),
                    cost_usd: 0.0,
                },
            });
            return;
        }
    };

    runtime.block_on(async move {
        let provider = RemoteProvider::new(spec.provider_id, frame_tx.clone(), pending.clone());
        let tools = RemoteToolExecutor::new(spec.tools, frame_tx.clone(), pending.clone());
        let sleeper = TokioSleeper;
        let engine = Engine::with_sleeper(&provider, &tools, spec.config, &sleeper);

        // The engine emits `AgentEvent`s; wrap each as a frame for the host. The
        // reverse-RPC request frames bypass this and go straight to `frame_tx`
        // from the ports, so a starved forwarder never stalls the turn — it only
        // affects UI-event latency.
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let forward_tx = frame_tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if forward_tx.send(ServerFrame::Event { event }).is_err() {
                    break;
                }
            }
        });

        let mut messages = spec.messages;
        let mut budget = spec.budget;
        let outcome = engine.run_turn(&mut messages, &mut budget, &event_tx).await;

        // Close the event channel so the forwarder drains and exits before the
        // terminal frame — a well-behaved host thus sees every event first.
        drop(event_tx);
        let _ = forwarder.await;

        let _ = frame_tx.send(ServerFrame::TurnComplete {
            outcome: outcome.into(),
        });
        // A clean turn leaves nothing parked; belt-and-suspenders for an aborted
        // one, so no reverse-RPC one-shot outlives the session.
        pending.clear();
    });
}
