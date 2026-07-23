//! Temporal primitives. Two axes, kept explicitly separate (lifecycle §7.5):
//! `known_at` = provider-local knowledge/transaction (belief) time; `valid_at` =
//! world validity. This is the typed successor to today's single `as_of` filter,
//! which the Phase 0 characterization pinned as **transaction-time only** — it
//! maps to `known_at`, never silently to `valid_at`.
//!
//! Intervals are **half-open `[from, until)`**: `from` inclusive, `until`
//! exclusive. This installment defines the value types and the ordering
//! invariant; the prefix-safe historical *reconstruction* over these lives in the
//! Phase 3 repository, not here.
//!
//! Timestamps are RFC 3339 strings. The ordering check compares them
//! lexicographically, which is correct **only for normalized UTC timestamps of
//! equal precision** (the form the canonicalization pipeline's UTC normalization
//! produces). Real instant parsing is deferred to Phase 3; callers must pass
//! normalized timestamps until then.

use serde::{Deserialize, Serialize};

use super::RecordValidationError;

/// A half-open time interval `[from, until)`. `until = None` is an open-ended
/// interval (e.g. a record valid from `from` with no known end).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalInterval {
    /// Inclusive start (RFC 3339, normalized UTC).
    pub from: String,
    /// Exclusive end (RFC 3339, normalized UTC). `None` = open-ended.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub until: Option<String>,
}

impl TemporalInterval {
    /// Construct a validated interval: when `until` is present it must be
    /// strictly after `from` (a non-empty forward interval). This is the
    /// `valid_until` > `valid_from` invariant.
    pub fn new(
        from: impl Into<String>,
        until: Option<String>,
    ) -> Result<Self, RecordValidationError> {
        let interval = Self {
            from: from.into(),
            until,
        };
        interval.validate()?;
        Ok(interval)
    }

    /// An open-ended interval starting at `from` (no end).
    pub fn open(from: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            until: None,
        }
    }

    /// Re-check the ordering invariant (useful after deserialization, which
    /// bypasses [`TemporalInterval::new`]).
    pub fn validate(&self) -> Result<(), RecordValidationError> {
        if let Some(until) = &self.until
            && *until <= self.from
        {
            return Err(RecordValidationError::NonForwardInterval {
                from: self.from.clone(),
                until: until.clone(),
            });
        }
        Ok(())
    }
}

/// The parameters of a bi-temporal query (lifecycle §7.5). A pure value type —
/// its *execution* (prefix-safe reconstruction) is the Phase 3 repository's job;
/// this installment only carries the parameters.
///
/// A provider that cannot reconstruct durable local ingestion history must
/// reject a query that sets `known_at`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalQuery {
    /// Knowledge/transaction-time cutoff: consider only what was known at or
    /// before this instant. (Maps to today's `as_of`.)
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub known_at: Option<String>,
    /// World-validity instant: select records whose validity interval contains
    /// this instant.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub valid_at: Option<String>,
    /// Filter on canonical-origin `observed_at ∈ [from, until)`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub observed: Option<TemporalInterval>,
    /// Select records whose validity overlaps this interval.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub valid_overlaps: Option<TemporalInterval>,
    /// Include event-only records that carry no validity when a `valid_at` /
    /// `valid_overlaps` filter is present (default `false` excludes them).
    #[serde(skip_serializing_if = "is_false", default)]
    pub include_records_without_validity: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T1: &str = "2026-02-01T00:00:00Z";
    const T2: &str = "2026-03-01T00:00:00Z";

    #[test]
    fn forward_interval_is_accepted_and_empty_or_reversed_is_rejected() {
        assert!(TemporalInterval::new(T1, Some(T2.into())).is_ok());
        assert!(TemporalInterval::open(T1).validate().is_ok());
        // until == from is empty (half-open) → rejected.
        assert_eq!(
            TemporalInterval::new(T1, Some(T1.into())),
            Err(RecordValidationError::NonForwardInterval {
                from: T1.into(),
                until: T1.into(),
            })
        );
        // until < from → rejected.
        assert!(TemporalInterval::new(T2, Some(T1.into())).is_err());
    }

    #[test]
    fn open_interval_omits_until_on_the_wire() {
        assert_eq!(
            serde_json::to_value(TemporalInterval::open(T1)).unwrap(),
            json!({ "from": T1 })
        );
    }

    #[test]
    fn empty_temporal_query_is_all_absent() {
        // Every field defaults absent / false, so the wire form is `{}`.
        assert_eq!(
            serde_json::to_value(TemporalQuery::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn temporal_query_carries_both_axes() {
        let q = TemporalQuery {
            known_at: Some(T2.into()),
            valid_at: Some(T1.into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&q).unwrap(),
            json!({ "known_at": T2, "valid_at": T1 })
        );
    }
}
