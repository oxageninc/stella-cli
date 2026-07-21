//! `stella-serve` — the Stella engine as a headless, host-driven service.
//!
//! This crate lets a host (e.g. Oxagen's platform) run the Stella Rust engine
//! "under the hood": the host assembles a turn ([`SessionSpec`]), the engine
//! orchestrates it, and **every governed side effect — model calls and tool
//! calls — is remoted back to the host** over a wire protocol. The engine never
//! holds ambient authority; the host runs each effect through its own kernel and
//! metering and reports the result back. This is ADR-033 Option B (the Rust
//! sidecar), whose port surface is identical to the in-process embed, so the
//! transport is swappable.
//!
//! # Shape
//!
//! - [`Session`] drives one turn on a dedicated OS thread (the engine turn
//!   future is `!Send`), emitting a stream of [`ServerFrame`]s.
//! - Most frames are [`ServerFrame::Event`] (agent events for the UI). The
//!   reverse-RPC frames — [`ServerFrame::ProviderRequest`] and
//!   [`ServerFrame::ToolRequest`] — each carry a `request_id`; the host runs the
//!   effect and answers with [`Session::resolve_provider`] /
//!   [`Session::resolve_tool`], unblocking the parked engine step.
//! - The terminal frame is [`ServerFrame::TurnComplete`].
//!
//! The HTTP/SSE transport that exposes this over a socket is a thin layer on top
//! of [`Session`] (SSE = the frame stream; POST endpoints = the resolve calls);
//! it is deliberately a separate slice so the `!Send` concurrency bridge here is
//! provable in isolation.

mod error;
mod frame;
mod http;
mod pending;
mod remote;
mod server;
mod session;

pub use error::ServeError;
pub use frame::{
    ProviderErrorWire, ProviderOutcomeIn, ProviderResultIn, ServerFrame, ToolResultIn,
    TurnOutcomeWire,
};
pub use pending::Pending;
pub use server::{ServeConfig, serve};
pub use session::{Session, SessionSpec};
