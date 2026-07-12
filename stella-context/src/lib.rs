//! `stella-context` — **the context plane**: the single door between the
//! engine and everything the agent knows that isn't already in the prompt
//! (`02-architecture.md` §7, `06-context-protocol.md` §2). One SQLite file, one
//! engine ([`ContextStore`]) holds a bi-temporal property graph, a fingerprinted
//! embedding index, and episodic memory; on top of it sits a hybrid, budgeted,
//! cited retrieval pipeline ([`ContextStore::recall`]) and a bi-temporal
//! write-back path ([`ContextStore::upsert`]). Built-in and external sources
//! register through one seam ([`ContextProvider`] / [`ProviderRegistry`]).
//!
//! # The four jobs this plane does that its providers don't (arch §7)
//!
//! 1. **Routing & fusion** — fan a query to capability-matching sources, fuse
//!    vector + recency + graph-adjacency via reciprocal-rank fusion, dedup by
//!    content hash ([`retrieval`]).
//! 2. **Budgeting** — every frame carries a `token_cost`; assembly packs to the
//!    caller's budget and reports what was dropped. Silent truncation is banned
//!    (`L-C5`).
//! 3. **Provenance** — every frame carries a human `citation_label` (a frame
//!    without one is a constructor error, `L-C4`) and a source chain.
//! 4. **Consent & isolation** — providers declare `reads`/`writes`/`egress`;
//!    the built-in store is `egress: false` (nothing leaves the machine).
//!
//! # Binding lessons realized here
//!
//! - `L-C1` warm-at-mount: [`ContextStore::open_and_warm`] kicks embedding
//!   catch-up in the background, not lazily on first query.
//! - `L-C2` byte-compat skip: vectors are keyed by `(content_hash,
//!   fingerprint)`; identical content is never re-embedded; retrieval never
//!   mixes fingerprints.
//! - `L-C3` bi-temporal facts: corrections close-and-supersede, never delete;
//!   [`ContextStore::facts_as_of`] answers "what did we believe at T1".
//! - `L-C6` coverage gate: weak graph/vector coverage falls back to bounded
//!   lexical search, **labeled as such** rather than dressed up as grounding.
//! - `L-L1` crash consistency: every write batch is one transaction; a kill
//!   mid-index rolls back to a consistent store.
//!
//! # Embedder policy
//!
//! The built-in default is the offline, pure-Rust [`HashEmbedder`]. The product
//! default — a local ONNX bge-small model — is the tracked follow-up
//! (`06-context-protocol.md` §4, risk R14); [`Embedder`] is the seam that makes
//! swapping it in trivial. Wire types are built on `ocp-types`, never
//! duplicated (principle #7 / `L-E1`).

mod clock;
mod embed;
mod error;
mod provider;
mod retrieval;
mod store;
mod writeback;

pub use clock::{Clock, FixedClock, SystemClock, format_rfc3339};
pub use embed::{EmbedError, Embedder, EmbedderFingerprint, Embedding, HashEmbedder};
pub use error::ContextError;
pub use provider::{ContextProvider, ProviderRegistry};
pub use retrieval::{DropReason, DroppedFrame, RecallResult, is_lexical_fallback};
pub use store::{ContextStore, NodeInput, NodeKind, NodeRow};
pub use writeback::{
    ContextDelta, EpisodeInput, EpisodeOutcome, FactAssertion, FactView, MemoryInput, MemoryKind,
    UpsertReceipt,
};

// Re-export the OCP wire types callers pass in/out, so a consumer needs only
// this crate for the common path (they remain `ocp-types`' definitions).
pub use ocp_types::{Capabilities, ContextFrame, ContextQuery, DataFlow, ProviderInfo};
