//! The wire vocabulary between the engine (running in this server) and the
//! host that drives it (e.g. Oxagen).
//!
//! Two directions, deliberately asymmetric:
//!
//! - **Outbound** ([`ServerFrame`]) is the single engine → host stream. It
//!   carries [`AgentEvent`]s for UI, plus the *reverse-RPC requests* the
//!   engine raises when it needs the host to run a governed side effect (a
//!   model completion, a tool call). Each request carries a `request_id`.
//! - **Inbound** ([`ToolResultIn`], [`ProviderResultIn`]) is the host → engine
//!   direction: the host POSTs the result of a reverse request back, keyed by
//!   that `request_id`, which unblocks the engine step waiting on it.
//!
//! This is the "reverse tool-call protocol" of ADR-033 Option B. The engine
//! never executes a tool or calls a model with ambient authority — every such
//! effect re-enters the host, which runs it through `kernel.invoke()` /
//! `@oxagen/ai` and reports back.

use serde::{Deserialize, Serialize};
use stella_core::TurnOutcome;
use stella_protocol::{AgentEvent, CompletionRequest, CompletionResult, ProviderError, ToolOutput};

/// One frame emitted by the engine toward the host over the outbound stream.
///
/// Not `Clone`: every frame is produced once and moved onto the channel; the
/// reverse-request variants own borrowed-free payloads (the engine hands over
/// the request by value), so nothing here needs to be duplicated.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// A normal agent event — stream it to the UI. Never requires a response.
    Event { event: AgentEvent },
    /// The engine needs the host to run a tool call and POST the result back
    /// ([`ToolResultIn`]) keyed by `request_id`. The engine step that raised
    /// it is parked until then.
    ToolRequest {
        request_id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The engine needs the host to run a model completion and POST the result
    /// back ([`ProviderResultIn`]) keyed by `request_id`. The host owns the
    /// model call (metering, gateway, BYOK); the engine only orchestrates.
    ProviderRequest {
        request_id: String,
        request: CompletionRequest,
    },
    /// Terminal frame: the turn ended. No further frames follow for this turn.
    TurnComplete { outcome: TurnOutcomeWire },
}

/// Serializable projection of [`TurnOutcome`] for the wire. `TurnOutcome` lives
/// in `stella-core` and is not itself `Serialize`, so the boundary owns the
/// mapping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TurnOutcomeWire {
    Completed { text: String, cost_usd: f64 },
    Aborted { reason: String },
}

impl From<TurnOutcome> for TurnOutcomeWire {
    fn from(outcome: TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::Completed { text, cost_usd } => {
                TurnOutcomeWire::Completed { text, cost_usd }
            }
            TurnOutcome::Aborted { reason } => TurnOutcomeWire::Aborted { reason },
        }
    }
}

/// Host → engine: the result of a [`ServerFrame::ToolRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultIn {
    pub request_id: String,
    pub output: ToolOutput,
}

/// Host → engine: the result of a [`ServerFrame::ProviderRequest`] — either a
/// completed model response or a classified provider error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderResultIn {
    pub request_id: String,
    #[serde(flatten)]
    pub outcome: ProviderOutcomeIn,
}

/// The success-or-error half of a [`ProviderResultIn`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProviderOutcomeIn {
    Ok { result: CompletionResult },
    Error { error: ProviderErrorWire },
}

/// Serializable mirror of [`ProviderError`]'s taxonomy. The host classifies the
/// failure at its adapter (never re-derived here) and sends the class; the
/// engine reconstructs a real [`ProviderError`] so its retry logic behaves
/// exactly as it would with a local provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderErrorWire {
    Transport {
        message: String,
    },
    RateLimited {
        message: String,
        #[serde(default)]
        retry_after_ms: Option<u64>,
    },
    Auth {
        message: String,
    },
    UnknownModel {
        slug: String,
    },
    Malformed {
        message: String,
    },
    Cancelled,
    Terminal {
        message: String,
    },
}

impl From<ProviderErrorWire> for ProviderError {
    fn from(wire: ProviderErrorWire) -> Self {
        match wire {
            ProviderErrorWire::Transport { message } => ProviderError::Transport(message),
            ProviderErrorWire::RateLimited {
                message,
                retry_after_ms,
            } => ProviderError::RateLimited {
                message,
                retry_after_ms,
            },
            ProviderErrorWire::Auth { message } => ProviderError::Auth(message),
            ProviderErrorWire::UnknownModel { slug } => ProviderError::UnknownModel { slug },
            ProviderErrorWire::Malformed { message } => ProviderError::Malformed(message),
            ProviderErrorWire::Cancelled => ProviderError::Cancelled,
            ProviderErrorWire::Terminal { message } => ProviderError::Terminal(message),
        }
    }
}

impl From<&ProviderError> for ProviderErrorWire {
    fn from(err: &ProviderError) -> Self {
        match err {
            ProviderError::Transport(m) => ProviderErrorWire::Transport { message: m.clone() },
            ProviderError::RateLimited {
                message,
                retry_after_ms,
            } => ProviderErrorWire::RateLimited {
                message: message.clone(),
                retry_after_ms: *retry_after_ms,
            },
            ProviderError::Auth(m) => ProviderErrorWire::Auth { message: m.clone() },
            ProviderError::UnknownModel { slug } => {
                ProviderErrorWire::UnknownModel { slug: slug.clone() }
            }
            ProviderError::Malformed(m) => ProviderErrorWire::Malformed { message: m.clone() },
            ProviderError::Cancelled => ProviderErrorWire::Cancelled,
            ProviderError::Terminal(m) => ProviderErrorWire::Terminal { message: m.clone() },
        }
    }
}
