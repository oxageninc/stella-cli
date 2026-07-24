//! The versioned drain wire contract — what a cloud syncer ships off the hub.
//!
//! The hub (`~/.stella/usage.db`) stages org-scoped, content-free telemetry
//! rows for upload (`UsageStore::cloud_pending`). A drain — the native syncer
//! built in #404, or the OTLP encoder in #427 — pages those rows and ships them
//! to a remote intake. This module defines the *native* (`format = "stella"`)
//! wire payload and stamps it with a **schema version** so the intake can tell
//! client versions apart as the payload evolves.
//!
//! # Why version now
//!
//! The [`DrainRow`] field set is a contract between every deployed client and
//! the intake. Once real clients are in the field, renaming a key or changing a
//! field's meaning silently breaks older/newer peers. A version stamp is cheap
//! to add before the transport lands (#404) and expensive to retrofit after —
//! the same reasoning that shipped the drain config's `format` discriminator
//! ahead of the OTLP encoder (#427). Ship the seam now, fill it in later.
//!
//! # Compatibility contract
//!
//! - Every native batch carries [`DrainBatch::schema_version`]; the version
//!   this build speaks is [`DRAIN_SCHEMA_VERSION`].
//! - The OTLP export (#427) carries the same value as a resource/scope
//!   attribute keyed [`OTEL_SCHEMA_VERSION_ATTR`], so a collector reads the
//!   version the same way regardless of format.
//! - The intake accepts a **known version range** — [`schema_version_supported`]
//!   is the single source of truth. An unknown version is a **terminal,
//!   non-poison reject**: the batch is malformed-by-contract, not a transient
//!   failure, so the drain loop (#467) must classify it as terminal (do not
//!   retry forever) — yet distinct from a poison *row*, because the whole batch
//!   shares one version and bisecting rows cannot rescue it. This module
//!   supplies the predicate; the reject/quarantine handling lives with the
//!   drain loop that has a batch to act on.
//!
//! # Bump-on-change discipline
//!
//! Any change to [`DrainRow`]'s field set or a field's semantics **must**
//! increment [`DRAIN_SCHEMA_VERSION`] and widen [`schema_version_supported`]
//! to match. The `wire_contract_is_frozen` test pins the serialized key set to
//! this version: change the fields without bumping and it fails, forcing the
//! version bump into the same change rather than a silent wire break.
//!
//! # Content-free by construction (#466)
//!
//! [`DrainRow`] enumerates only identity + addressing + telemetry keys — never
//! prompt or completion text. It deliberately drops internal-only columns the
//! hub row carries: the cursor address (`hub_rowid`), execution grouping
//! (`execution_id`, `step`, `call_role`), and the local drift-analysis estimate
//! (`estimated_input_tokens`) are not part of the enterprise rollup contract.
//! Adding any of them is a v2 decision made on purpose, not a side effect of a
//! blanket `derive(Serialize)` over the internal struct.

use serde::{Deserialize, Serialize};

use crate::usage::CloudTelemetryEvent;

/// The native drain wire-format version this build speaks. Increment on any
/// change to [`DrainRow`]'s field set or a field's semantics (see the module
/// docs' bump-on-change discipline).
pub const DRAIN_SCHEMA_VERSION: u32 = 1;

/// Oldest wire version this build's intake contract still accepts.
pub const MIN_SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// Newest wire version this build understands — always [`DRAIN_SCHEMA_VERSION`].
pub const MAX_SUPPORTED_SCHEMA_VERSION: u32 = DRAIN_SCHEMA_VERSION;

/// OTLP resource/scope attribute key carrying [`DrainBatch::schema_version`] on
/// the `format = "otel"` export (#427). The native and OTLP encoders stamp the
/// same value under this key so a collector can tell client versions apart.
pub const OTEL_SCHEMA_VERSION_ATTR: &str = "stella.schema_version";

/// Whether the intake accepts wire version `v` — the single source of truth for
/// the "known version range" half of the compatibility contract. `false` is a
/// **terminal, non-poison** reject (see the module docs), never a transient
/// error the drain loop should retry.
pub fn schema_version_supported(v: u32) -> bool {
    (MIN_SUPPORTED_SCHEMA_VERSION..=MAX_SUPPORTED_SCHEMA_VERSION).contains(&v)
}

/// The native (`format = "stella"`) batch envelope: a schema-versioned run of
/// content-free telemetry rows. This is the wire payload a cloud syncer (#404)
/// POSTs to the org intake; the intake reads [`Self::schema_version`] first and
/// rejects an unsupported version before decoding [`Self::rows`].
///
/// The version is an **integer** so the intake can accept a *range*
/// ([`schema_version_supported`]) and bump-on-change is a simple increment —
/// distinct from the enterprise spool's string `schema` discriminator
/// (`"stella.operational.batch.v1"`), a different sink with a different
/// (single-id, not range) contract. `deny_unknown_fields` matches that sink's
/// wire posture: within a supported version the field set is exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrainBatch {
    /// Wire-format version — see [`DRAIN_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The staged hub rows, oldest first (the order `cloud_pending` returns).
    pub rows: Vec<DrainRow>,
}

impl DrainBatch {
    /// Wrap a page of hub rows in the current-version envelope — the native
    /// encoder the `format = "stella"` drain (#404) serializes and POSTs.
    pub fn from_events(events: &[CloudTelemetryEvent]) -> Self {
        Self {
            schema_version: DRAIN_SCHEMA_VERSION,
            rows: events.iter().map(DrainRow::from).collect(),
        }
    }
}

/// One telemetry row on the wire: org/workspace/repo/project identity, the
/// `(workspace_id, source_rowid)` idempotency key, `recorded_at`, and the
/// content-free per-call telemetry. Content-free by construction (#466) — this
/// field set *is* what [`DRAIN_SCHEMA_VERSION`] pins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrainRow {
    // --- identity ---
    pub org_id: String,
    pub workspace_id: Option<String>,
    pub repo_id: String,
    pub project_id: String,

    // --- addressing / idempotency ---
    /// Per-project source rowid. `(workspace_id, source_rowid)` is the intake's
    /// dedup key, so a re-send after a lost ack lands idempotently (#404).
    pub source_rowid: i64,
    pub recorded_at: String,

    // --- telemetry (content-free) ---
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_miss_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub retries: u32,
    pub tool_calls: u64,
    pub usage_complete: bool,
}

impl From<&CloudTelemetryEvent> for DrainRow {
    fn from(e: &CloudTelemetryEvent) -> Self {
        let t = &e.telemetry;
        Self {
            org_id: e.org_id.clone(),
            workspace_id: e.workspace_id.clone(),
            repo_id: e.repo_id.clone(),
            project_id: e.project_id.clone(),
            source_rowid: e.source_rowid,
            recorded_at: e.recorded_at.clone(),
            provider: t.provider.clone(),
            model: t.model.clone(),
            input_tokens: t.input_tokens,
            output_tokens: t.output_tokens,
            cache_read_tokens: t.cache_read_tokens,
            cache_miss_tokens: t.cache_miss_tokens,
            cache_write_tokens: t.cache_write_tokens,
            cost_usd: t.cost_usd,
            duration_ms: t.duration_ms,
            retries: t.retries,
            tool_calls: t.tool_calls,
            usage_complete: t.usage_complete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TelemetryRow;
    use serde_json::Value;

    /// A hub row whose internal-only ids differ from its wire ids, so the tests
    /// can prove the wire carries `source_rowid` (the dedup key), not the
    /// internal `hub_rowid`.
    fn sample_event() -> CloudTelemetryEvent {
        CloudTelemetryEvent {
            hub_rowid: 42,
            org_id: "acme".into(),
            workspace_id: Some("ws-1".into()),
            repo_id: "repo-1".into(),
            project_id: "proj-1".into(),
            execution_id: 7,
            source_rowid: 99,
            recorded_at: "2026-07-23T00:00:00Z".into(),
            telemetry: TelemetryRow {
                step: 3,
                provider: "zai".into(),
                call_role: "lead".into(),
                model: "glm-5.2".into(),
                input_tokens: 100,
                estimated_input_tokens: 90,
                output_tokens: 20,
                cache_read_tokens: 10,
                cache_miss_tokens: 5,
                cache_write_tokens: 2,
                cost_usd: 0.01,
                duration_ms: 1234,
                retries: 1,
                tool_calls: 4,
                usage_complete: true,
            },
        }
    }

    /// The frozen wire-contract field set for schema v1. Editing this list
    /// without bumping [`DRAIN_SCHEMA_VERSION`] is exactly what
    /// `wire_contract_is_frozen` catches. Compared as a set (order-independent).
    const V1_ROW_KEYS: &[&str] = &[
        "org_id",
        "workspace_id",
        "repo_id",
        "project_id",
        "source_rowid",
        "recorded_at",
        "provider",
        "model",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_miss_tokens",
        "cache_write_tokens",
        "cost_usd",
        "duration_ms",
        "retries",
        "tool_calls",
        "usage_complete",
    ];

    #[test]
    fn batch_carries_schema_version() {
        let batch = DrainBatch::from_events(&[sample_event()]);
        assert_eq!(batch.schema_version, DRAIN_SCHEMA_VERSION);
        let v: Value = serde_json::to_value(&batch).unwrap();
        assert_eq!(v["schema_version"], DRAIN_SCHEMA_VERSION);
        assert_eq!(v["rows"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn wire_contract_is_frozen() {
        // Serialize a row and assert its key set equals the frozen v1 allowlist.
        // This is the bump-on-change guard: add / remove / rename a DrainRow
        // field and this fails until V1_ROW_KEYS is updated *and* the version
        // bumped in the same change.
        let row = DrainRow::from(&sample_event());
        let obj = serde_json::to_value(&row).unwrap();
        let obj = obj.as_object().unwrap();
        let mut got: Vec<&str> = obj.keys().map(String::as_str).collect();
        got.sort_unstable();
        let mut want: Vec<&str> = V1_ROW_KEYS.to_vec();
        want.sort_unstable();
        assert_eq!(
            got, want,
            "DrainRow wire keys drifted from the v1 contract — bump \
             DRAIN_SCHEMA_VERSION and update V1_ROW_KEYS in the same change"
        );
    }

    #[test]
    fn drain_row_excludes_internal_fields() {
        // The internal columns the hub row carries but the contract excludes
        // must never leak onto the wire (cursor address, execution grouping,
        // local drift estimate).
        let obj = serde_json::to_value(DrainRow::from(&sample_event())).unwrap();
        let obj = obj.as_object().unwrap();
        for excluded in [
            "hub_rowid",
            "execution_id",
            "step",
            "call_role",
            "estimated_input_tokens",
        ] {
            assert!(
                !obj.contains_key(excluded),
                "internal field `{excluded}` leaked onto the wire"
            );
        }
    }

    #[test]
    fn source_rowid_is_the_dedup_key() {
        // The wire carries source_rowid (the intake's (workspace_id,
        // source_rowid) dedup key), not the hub's internal rowid.
        let e = sample_event();
        assert_ne!(
            e.source_rowid, e.hub_rowid,
            "fixture must distinguish the two ids for this test to mean anything"
        );
        assert_eq!(DrainRow::from(&e).source_rowid, e.source_rowid);
    }

    #[test]
    fn round_trips_through_json() {
        let batch = DrainBatch::from_events(&[sample_event(), sample_event()]);
        let bytes = serde_json::to_vec(&batch).unwrap();
        let back: DrainBatch = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(batch, back);
    }

    #[test]
    fn version_support_range_is_closed_and_current() {
        assert!(schema_version_supported(DRAIN_SCHEMA_VERSION));
        assert!(!schema_version_supported(0));
        assert!(
            !schema_version_supported(DRAIN_SCHEMA_VERSION + 1),
            "an unknown newer version must be an explicit (terminal) reject"
        );
    }
}
