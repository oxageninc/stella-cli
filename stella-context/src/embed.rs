//! Embeddings: the [`Embedder`] trait (the seam), the [`EmbedderFingerprint`]
//! that stamps every stored vector, and [`HashEmbedder`] — the pure-Rust,
//! offline-capable default.
//!
//! # Why a hashing embedder is the default
//!
//! `06-context-protocol.md` §4 and risk register R14 make the *product*
//! default a local ONNX model (bge-small-class, ~130 MB, checksum-pinned
//! weight fetch + a native runtime). That is the tracked follow-up. Shipping
//! it is a supply-chain and binary-size decision that must not block the
//! context plane from working at all, offline, on first run with zero
//! downloads. So the **built-in** default is [`HashEmbedder`]: a deterministic
//! hashed-character-n-gram projection with no I/O, no weights, no network.
//!
//! [`Embedder`] is the seam that makes swapping in ONNX (or a hosted API
//! embedder — Z.ai, OpenAI, Gemini) trivial: the store keys every vector by
//! `(content_hash, fingerprint)`, so switching embedders is just a new
//! fingerprint and an incremental re-embed on next touch (`L-C2`); retrieval
//! never mixes fingerprints (`02-architecture.md` §6).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A failure from an embedding backend.
#[derive(Debug, Error)]
pub enum EmbedError {
    /// A caller asked to embed an empty batch or an empty string where the
    /// backend requires content.
    #[error("cannot embed empty input")]
    EmptyInput,

    /// A vector arrived with the wrong dimensionality for its fingerprint.
    #[error("embedding dimension mismatch: expected {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },

    /// A concrete backend (ONNX runtime, hosted API) failed. Unused by the
    /// hashing default; present so API/ONNX embedders slot in without
    /// widening this enum later.
    #[error("embedding backend error: {0}")]
    Backend(String),
}

/// Identifies exactly which embedder produced a vector. Stored beside every
/// vector; retrieval compares only vectors sharing the active fingerprint
/// (`02-architecture.md` §6, `L-C2`). Changing any field is a new fingerprint
/// and invalidates old vectors incrementally rather than in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedderFingerprint {
    /// Model identity, e.g. `"hash-ngram"` or `"bge-small-en-v1.5"`.
    pub model_id: String,
    /// A revision within the model id — bump to force re-embedding when the
    /// projection changes without the model id changing.
    pub revision: String,
    /// Vector dimensionality.
    pub dims: usize,
    /// How vectors are normalized, e.g. `"l2"` or `"none"`.
    pub normalization: String,
}

impl EmbedderFingerprint {
    /// The canonical string form stored in the DB and compared for equality:
    /// `model_id@revision/dims/normalization`. Fixed-shape so it is a stable
    /// primary-key component.
    pub fn id(&self) -> String {
        format!(
            "{}@{}/{}/{}",
            self.model_id, self.revision, self.dims, self.normalization
        )
    }
}

/// A single embedding vector tagged with the fingerprint that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    pub fingerprint: String,
    pub vector: Vec<f32>,
}

/// Produces vectors for text. Async because real backends (ONNX on a
/// threadpool, hosted APIs over HTTP) are; the hashing default resolves
/// immediately.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// The fingerprint every vector this embedder produces is stamped with.
    fn fingerprint(&self) -> EmbedderFingerprint;

    /// Embed a batch. Returns one vector per input, in order. An empty batch
    /// is an error, not an empty success — callers should not ask for nothing.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, EmbedError>;
}

/// The pure-Rust default embedder: the hashing trick over character n-grams.
///
/// For each length-`n` character window of the (lowercased) input we compute
/// two independent FNV-1a hashes — one selects a dimension bucket, one picks a
/// sign — and accumulate ±1 into that bucket (a signed random projection that
/// keeps the expected dot-product an unbiased similarity estimate). The result
/// is L2-normalized so cosine similarity is a plain dot product. Fully
/// deterministic and platform-independent: the same string always yields the
/// same vector, which is what makes the `(content_hash, fingerprint)` skip in
/// `L-C2` sound.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dims: usize,
    ngram: usize,
    revision: String,
}

impl Default for HashEmbedder {
    fn default() -> Self {
        // 256 dims / 3-grams is a reasonable default for CLI-local corpora:
        // small enough that brute-force cosine is cheap, wide enough that
        // character-trigram collisions stay low.
        Self {
            dims: 256,
            ngram: 3,
            revision: "1".to_string(),
        }
    }
}

impl HashEmbedder {
    /// Construct with an explicit revision (bumping it re-fingerprints, which
    /// forces re-embedding). `model_id`/`normalization` are fixed for this
    /// backend.
    pub fn with_revision(revision: impl Into<String>) -> Self {
        Self {
            revision: revision.into(),
            ..Self::default()
        }
    }

    /// Construct with an explicit dimension count (for tests that want a tiny,
    /// hand-checkable vector) and revision.
    pub fn with_dims_and_revision(dims: usize, revision: impl Into<String>) -> Self {
        Self {
            dims,
            ngram: 3,
            revision: revision.into(),
        }
    }

    /// The pure projection, exposed for direct testing and reuse by
    /// [`Embedder::embed`].
    pub fn project(&self, text: &str) -> Vec<f32> {
        let mut acc = vec![0.0f32; self.dims];
        let chars: Vec<char> = text.to_lowercase().chars().collect();
        if chars.is_empty() {
            return acc; // zero vector; cosine against it is defined as 0.
        }
        // Windows of `ngram` chars; for very short inputs fall back to the
        // whole string as one window so we never index empty content to zero.
        let window = self.ngram.min(chars.len());
        for start in 0..=(chars.len() - window) {
            let gram: String = chars[start..start + window].iter().collect();
            let h = fnv1a(gram.as_bytes());
            let bucket = (h % self.dims as u64) as usize;
            // A second, decorrelated hash chooses the sign.
            let sign = if fnv1a_seeded(gram.as_bytes(), 0x9E37_79B9_7F4A_7C15) & 1 == 0 {
                1.0
            } else {
                -1.0
            };
            acc[bucket] += sign;
        }
        l2_normalize(&mut acc);
        acc
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    fn fingerprint(&self) -> EmbedderFingerprint {
        EmbedderFingerprint {
            model_id: "hash-ngram".to_string(),
            revision: self.revision.clone(),
            dims: self.dims,
            normalization: "l2".to_string(),
        }
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, EmbedError> {
        if texts.is_empty() {
            return Err(EmbedError::EmptyInput);
        }
        let fingerprint = self.fingerprint().id();
        Ok(texts
            .iter()
            .map(|t| Embedding {
                fingerprint: fingerprint.clone(),
                vector: self.project(t),
            })
            .collect())
    }
}

/// L2-normalize in place; a zero vector is left as zero (its norm is 0).
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// FNV-1a 64-bit. Chosen over `std::hash::DefaultHasher` because the latter's
/// output is explicitly not stable across releases — and stored-vector
/// determinism must survive a compiler upgrade.
fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_seeded(bytes, 0xcbf2_9ce4_8422_2325)
}

fn fnv1a_seeded(bytes: &[u8], offset_basis: u64) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = offset_basis;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_id_is_canonical_and_stable() {
        let fp = EmbedderFingerprint {
            model_id: "hash-ngram".into(),
            revision: "1".into(),
            dims: 256,
            normalization: "l2".into(),
        };
        assert_eq!(fp.id(), "hash-ngram@1/256/l2");
    }

    #[test]
    fn hash_embedder_is_deterministic() {
        let e = HashEmbedder::default();
        assert_eq!(
            e.project("the quick brown fox"),
            e.project("the quick brown fox")
        );
    }

    #[test]
    fn hash_embedder_output_is_l2_normalized() {
        let e = HashEmbedder::default();
        let v = e.project("some representative content to embed");
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm was {norm}");
    }

    #[test]
    fn similar_text_scores_higher_than_dissimilar() {
        // The property that makes retrieval work at all: near-duplicate text
        // is closer in cosine than unrelated text.
        let e = HashEmbedder::default();
        let base = e.project("open the sqlite connection in wal mode");
        let near = e.project("open the sqlite connection using wal mode");
        let far = e.project("render a bar chart of quarterly revenue");
        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!(cos(&base, &near) > cos(&base, &far));
    }

    #[test]
    fn empty_string_projects_to_zero_vector() {
        let e = HashEmbedder::default();
        assert!(e.project("").iter().all(|&x| x == 0.0));
    }

    #[tokio::test]
    async fn embed_batch_returns_one_vector_per_input_tagged_with_fingerprint() {
        let e = HashEmbedder::default();
        let out = e
            .embed(&["alpha".to_string(), "beta".to_string()])
            .await
            .expect("batch embeds");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].fingerprint, "hash-ngram@1/256/l2");
        assert_eq!(out[0].vector.len(), 256);
    }

    #[tokio::test]
    async fn empty_batch_is_an_error_not_an_empty_ok() {
        let e = HashEmbedder::default();
        assert!(matches!(e.embed(&[]).await, Err(EmbedError::EmptyInput)));
    }

    #[test]
    fn bumping_revision_changes_the_fingerprint() {
        assert_ne!(
            HashEmbedder::with_revision("1").fingerprint().id(),
            HashEmbedder::with_revision("2").fingerprint().id(),
        );
    }
}
