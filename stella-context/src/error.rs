//! Typed context-plane errors (`docs/specs/stella-rust-cli/02-architecture.md`
//! §1.5: "fail loud, recover gracefully" — errors are `thiserror`, never
//! `panic!` in the hot path). The retrieval/write-back pipelines classify
//! failures at the source so callers never re-derive a category downstream.

use thiserror::Error;

/// A failure inside the context plane — storage, embedding, or an invariant
/// the plane refuses to violate (a frame without a citation label, a
/// cross-fingerprint retrieval).
#[derive(Debug, Error)]
pub enum ContextError {
    /// A `rusqlite` storage failure (open, migrate, query, transaction).
    #[error("context store error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A JSON (de)serialization failure for a stored property bag or delta.
    #[error("context serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// An embedding backend failed.
    #[error("embedding error: {0}")]
    Embed(#[from] crate::embed::EmbedError),

    /// A frame was built without a human citation label. `L-C4` makes this a
    /// **constructor-level error, not a lint**: every frame that reaches a
    /// prompt must be citable by a human label, never a bare id.
    #[error("frame `{id}` has no citation label — every frame must be humanly citable (L-C4)")]
    MissingCitation { id: String },

    /// Retrieval was asked to mix embeddings from two different embedders.
    /// `02-architecture.md` §6 and `L-C2`: retrieval never mixes fingerprints
    /// — a stored vector under a stale fingerprint is invisible, never
    /// silently compared against a fresh query vector.
    #[error("embedder fingerprint mismatch: query is `{query}`, candidate is `{candidate}`")]
    FingerprintMismatch { query: String, candidate: String },

    /// The store failed its integrity check (`PRAGMA integrity_check`). A
    /// kill mid-index must never leave this state (`L-L1`); if it does, the
    /// plane reports it loudly rather than serving corrupt context.
    #[error("context store integrity check failed: {0}")]
    Corruption(String),

    /// A caller passed input the plane refuses (empty display name, a
    /// timestamp that isn't RFC-3339, a zero-dimension embedding).
    #[error("invalid context input: {0}")]
    InvalidInput(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_citation_names_the_frame_and_the_lesson() {
        let err = ContextError::MissingCitation {
            id: "frm_42".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("frm_42"), "{msg}");
        assert!(msg.contains("L-C4"), "{msg}");
    }

    #[test]
    fn fingerprint_mismatch_names_both_sides() {
        let err = ContextError::FingerprintMismatch {
            query: "a@1/256/l2".into(),
            candidate: "b@1/256/l2".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("a@1/256/l2"), "{msg}");
        assert!(msg.contains("b@1/256/l2"), "{msg}");
    }

    #[test]
    fn sqlite_errors_convert_via_from() {
        // The `#[from]` wiring is what lets `?` bubble a rusqlite failure into
        // a ContextError at every call site without a manual map_err.
        let sqlite = rusqlite::Error::InvalidQuery;
        let err: ContextError = sqlite.into();
        assert!(matches!(err, ContextError::Sqlite(_)));
    }
}
