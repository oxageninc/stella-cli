//! Canonical record hashing — `record_hash = sha256:<64 hex>` over the RFC 8785
//! (JCS) canonical bytes of the record's preimage.
//!
//! The preimage pipeline (the part that must be deterministic across producers):
//!
//! 1. serialize the record to a JSON object;
//! 2. **drop `record_hash`** — a record never hashes its own hash;
//! 3. **input-null normalization** — recursively remove `null`-valued object
//!    keys, so an explicit `null` and an absent field hash identically (this
//!    complements the `#[serde(skip_serializing_if = "Option::is_none")]`
//!    absent-option omission on the record types);
//! 4. **RFC 8785 JCS** — sort keys, minimal whitespace, canonical numbers, via
//!    `serde_json_canonicalizer` (the same crate + version CGEP uses, so the
//!    preimage bytes are byte-identical to CGEP's for export interop);
//! 5. sha256, lowercase hex, `sha256:` prefix.
//!
//! Two normalization steps are the *caller's* responsibility before hashing and
//! are intentionally NOT redone here: **alias normalization** (legacy field
//! aliases like `recorded_at`→`observed_at`, `valid_to`→`valid_until` are mapped
//! at ingestion) and **UTC timestamp normalization** (timestamps are stored in
//! canonical UTC form). Records built from the typed model already satisfy both.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Hashing failed. Distinct from validation: a record can be valid yet fail to
/// serialize/canonicalize (or not be an object).
#[derive(Debug, thiserror::Error)]
pub enum RecordHashError {
    /// The record could not be serialized to JSON.
    #[error("record could not be serialized to JSON: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The record's JSON could not be RFC 8785-canonicalized.
    #[error("record could not be canonicalized (RFC 8785): {0}")]
    Canonicalize(String),
    /// Records must serialize to a JSON object; this one did not.
    #[error("a context record must serialize to a JSON object")]
    NotAnObject,
}

/// The canonical `record_hash` for `record`: `sha256:<64 lowercase hex>` over the
/// JCS preimage. Deterministic for identical logical content regardless of field
/// order or a pre-existing `record_hash`.
pub fn record_hash<T: Serialize>(record: &T) -> Result<String, RecordHashError> {
    let preimage = canonical_preimage(record)?;
    let digest = Sha256::digest(preimage.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        // infallible on String
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(format!("sha256:{hex}"))
}

/// The exact bytes that get hashed — the RFC 8785 canonical JSON of the record
/// after dropping `record_hash` and normalizing nulls. Private, but exercised
/// directly by the golden tests so the preimage is pinned, not just the digest.
fn canonical_preimage<T: Serialize>(record: &T) -> Result<String, RecordHashError> {
    let mut value = serde_json::to_value(record)?;
    match &mut value {
        Value::Object(map) => {
            map.remove("record_hash");
        }
        _ => return Err(RecordHashError::NotAnObject),
    }
    strip_nulls(&mut value);
    serde_json_canonicalizer::to_string(&value)
        .map_err(|e| RecordHashError::Canonicalize(e.to_string()))
}

/// Recursively remove `null`-valued keys from every object (and recurse into
/// arrays), so an explicit `null` is indistinguishable from an absent field.
fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for v in map.values_mut() {
                strip_nulls(v);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                strip_nulls(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preimage_sorts_keys_drops_record_hash_and_omits_nulls() {
        // A focused, hand-verifiable pipeline vector: unsorted keys, a nested
        // object, an explicit null, and a bogus record_hash.
        let record = json!({
            "b_num": 88,
            "a_str": "x",
            "nested": { "y": "2", "x": "1" },
            "z_null": null,
            "record_hash": "sha256:deadbeef"
        });
        // record_hash dropped, z_null omitted, all keys sorted, no whitespace.
        assert_eq!(
            canonical_preimage(&record).unwrap(),
            r#"{"a_str":"x","b_num":88,"nested":{"x":"1","y":"2"}}"#
        );
    }

    #[test]
    fn hash_has_the_canonical_shape() {
        let h = record_hash(&json!({"a": 1})).unwrap();
        assert!(h.starts_with("sha256:"), "prefix: {h}");
        let hex = h.strip_prefix("sha256:").unwrap();
        assert_eq!(hex.len(), 64, "digest is 64 hex chars: {h}");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "lowercase hex only: {h}"
        );
    }

    #[test]
    fn hash_is_independent_of_key_order() {
        let a = record_hash(&json!({"a": 1, "b": 2})).unwrap();
        let b = record_hash(&json!({"b": 2, "a": 1})).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn hash_ignores_a_preexisting_record_hash_field() {
        let without = record_hash(&json!({"a": 1})).unwrap();
        let with_bogus = record_hash(&json!({"a": 1, "record_hash": "sha256:0000"})).unwrap();
        assert_eq!(without, with_bogus, "record_hash must not hash itself");
    }

    #[test]
    fn hash_treats_explicit_null_as_absent() {
        let absent = record_hash(&json!({"a": 1})).unwrap();
        let explicit_null = record_hash(&json!({"a": 1, "b": null})).unwrap();
        assert_eq!(absent, explicit_null);
    }

    #[test]
    fn non_object_records_are_rejected() {
        assert!(matches!(
            record_hash(&json!([1, 2, 3])),
            Err(RecordHashError::NotAnObject)
        ));
    }

    #[test]
    fn golden_directive_record_hash_is_stable() {
        // A representative directive record run through the whole pipeline. The
        // canonical preimage is hand-verifiable (keys sorted, record_hash and
        // absent fields gone); the digest is pinned so any drift is caught.
        let record = json!({
            "schema_version": "1.0-draft",
            "record_kind": "directive",
            "record_id": "dir_example_v1",
            "lineage_id": "lin_example",
            "directive_kind": "rule",
            "origin": "inferred",
            "enforcement": "advisory",
            "confidence": 88,
            "scope": { "repository_id": "repo_1" },
            "sharing_scope": "repository",
            "observed_at": "2026-07-20T18:30:00Z",
            "valid_from": "2026-07-20T18:30:00Z",
            "record_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        });
        assert_eq!(
            canonical_preimage(&record).unwrap(),
            r#"{"confidence":88,"directive_kind":"rule","enforcement":"advisory","lineage_id":"lin_example","observed_at":"2026-07-20T18:30:00Z","origin":"inferred","record_id":"dir_example_v1","record_kind":"directive","schema_version":"1.0-draft","scope":{"repository_id":"repo_1"},"sharing_scope":"repository","valid_from":"2026-07-20T18:30:00Z"}"#
        );
        assert_eq!(record_hash(&record).unwrap(), GOLDEN_DIRECTIVE_HASH);
    }

    // Pinned from the canonical preimage asserted just above. Any change to the
    // hashing pipeline or the record's canonical bytes will break this.
    const GOLDEN_DIRECTIVE_HASH: &str =
        "sha256:761d3d24d908aa31946a09d2fab3ab329c7504cfdb508b5f7d4832d23bd18499";
}
