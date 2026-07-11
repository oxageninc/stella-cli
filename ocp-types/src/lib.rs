//! `ocp-types` — the Open Context Protocol wire types.
//!
//! This crate is the industry-facing artifact: **MIT licensed, zero
//! dependencies beyond `serde`**, publishable to crates.io on its own so a
//! third party can implement an OCP host or provider without pulling in any
//! Oxagen code. See `docs/specs/oxagen-rust-cli/06-context-protocol.md` §3
//! for the normative shape this crate binds to Rust types.
//!
//! Protocol version: `ocp/1.0-draft`.

pub mod capability;
pub mod frame;
pub mod query;

pub use capability::{Capabilities, DataFlow, ProviderInfo};
pub use frame::{ContextFrame, FrameKind, Provenance, Relation};
pub use query::{ContextQuery, ContextQueryResult};

/// The protocol version string this crate implements. Frozen to `ocp/1.0`
/// only at the public v1.0 release (`06-context-protocol.md` §3).
pub const PROTOCOL_VERSION: &str = "ocp/1.0-draft";
