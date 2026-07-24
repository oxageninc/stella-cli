//! Internal, replay-safe adaptive-context events (Phase 1 installment 5).
//!
//! These are stella-internal lifecycle events — they do **not** change the
//! Context Graph Exchange Protocol wire semantics. They carry only stable IDs
//! (as strings), never the `stella-core` record structs: `stella-core` depends on
//! `stella-protocol`, so importing core types here would be a dependency cycle.
//!
//! ## Forward compatibility (the whole point)
//!
//! The main [`crate::event::AgentEvent`] stream is an internally-tagged enum that
//! **rejects unknown variants**, and the JSONL replay reader treats an
//! unparseable interior line as fatal. So a NEW event must never be a new
//! `AgentEvent` variant — an older binary replaying a mixed stream would break.
//!
//! Instead these events ride a **versioned envelope**: [`LifecycleEventEnvelope`]
//! keeps the type-specific fields in a raw `payload`, so it always deserializes,
//! even for an `event_type` the reader has never seen. [`LifecycleEventEnvelope::decode`]
//! then maps a recognized `event_type` to a typed [`LifecycleEvent`], or preserves
//! an unrecognized one as [`LifecycleEvent::Unknown`] with its payload intact —
//! the exact tolerance the fatal `parse_jsonl` path lacks.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The canonical `event_type` tokens.
pub mod event_type {
    /// An observation was recorded.
    pub const OBSERVATION_RECORDED: &str = "observation_recorded";
    /// A record proposal was created.
    pub const RECORD_PROPOSAL_CREATED: &str = "record_proposal_created";
    /// A promotion event was recorded.
    pub const PROMOTION_RECORDED: &str = "promotion_recorded";
    /// A context use was recorded.
    pub const CONTEXT_USE_RECORDED: &str = "context_use_recorded";
    /// Feedback on a context use was recorded.
    pub const CONTEXT_USE_FEEDBACK_RECORDED: &str = "context_use_feedback_recorded";
    /// Missing context was detected.
    pub const MISSING_CONTEXT_DETECTED: &str = "missing_context_detected";
    /// An artifact contract was selected for a task.
    pub const ARTIFACT_CONTRACT_SELECTED: &str = "artifact_contract_selected";
    /// A contract validation completed.
    pub const CONTRACT_VALIDATION_COMPLETED: &str = "contract_validation_completed";
    /// An outcome was assessed.
    pub const OUTCOME_ASSESSED: &str = "outcome_assessed";
    /// A compiled context frame was built.
    pub const COMPILED_CONTEXT_FRAME_BUILT: &str = "compiled_context_frame_built";
}

/// The versioned envelope every lifecycle event rides. Always deserializes: the
/// event-specific fields live in `payload` as a raw value, so an unrecognized
/// `event_type` is preserved rather than rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LifecycleEventEnvelope {
    /// Envelope schema version (e.g. `1.0-draft`).
    pub schema_version: String,
    /// The event discriminator (see [`event_type`]).
    pub event_type: String,
    /// Unique id of this event.
    pub event_id: String,
    /// When the event was observed (RFC 3339 UTC).
    pub observed_at: String,
    /// The task this event belongs to, when applicable.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub task_id: Option<String>,
    /// The invocation this event belongs to, when applicable.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub invocation_id: Option<String>,
    /// Event-type-specific fields (stable IDs only). Raw so unknown event types
    /// round-trip losslessly.
    pub payload: Value,
}

impl LifecycleEventEnvelope {
    /// Decode the payload into a typed [`LifecycleEvent`]. An unrecognized
    /// `event_type`, or a payload that does not match a recognized type, is
    /// preserved as [`LifecycleEvent::Unknown`] — decoding never fails.
    pub fn decode(&self) -> LifecycleEvent {
        fn typed<T: for<'de> Deserialize<'de>>(payload: &Value) -> Option<T> {
            serde_json::from_value(payload.clone()).ok()
        }
        let unknown = || LifecycleEvent::Unknown {
            event_type: self.event_type.clone(),
            payload: self.payload.clone(),
        };
        match self.event_type.as_str() {
            event_type::OBSERVATION_RECORDED => {
                typed(&self.payload).map_or_else(unknown, LifecycleEvent::ObservationRecorded)
            }
            event_type::RECORD_PROPOSAL_CREATED => {
                typed(&self.payload).map_or_else(unknown, LifecycleEvent::RecordProposalCreated)
            }
            event_type::PROMOTION_RECORDED => {
                typed(&self.payload).map_or_else(unknown, LifecycleEvent::PromotionRecorded)
            }
            event_type::CONTEXT_USE_RECORDED => {
                typed(&self.payload).map_or_else(unknown, LifecycleEvent::ContextUseRecorded)
            }
            event_type::CONTEXT_USE_FEEDBACK_RECORDED => typed(&self.payload)
                .map_or_else(unknown, LifecycleEvent::ContextUseFeedbackRecorded),
            event_type::MISSING_CONTEXT_DETECTED => {
                typed(&self.payload).map_or_else(unknown, LifecycleEvent::MissingContextDetected)
            }
            event_type::ARTIFACT_CONTRACT_SELECTED => {
                typed(&self.payload).map_or_else(unknown, LifecycleEvent::ArtifactContractSelected)
            }
            event_type::CONTRACT_VALIDATION_COMPLETED => typed(&self.payload)
                .map_or_else(unknown, LifecycleEvent::ContractValidationCompleted),
            event_type::OUTCOME_ASSESSED => {
                typed(&self.payload).map_or_else(unknown, LifecycleEvent::OutcomeAssessed)
            }
            event_type::COMPILED_CONTEXT_FRAME_BUILT => {
                typed(&self.payload).map_or_else(unknown, LifecycleEvent::CompiledContextFrameBuilt)
            }
            _ => unknown(),
        }
    }
}

/// A decoded lifecycle event. `Unknown` carries a forward-compatible event whose
/// type this binary does not recognize, with its payload preserved.
#[derive(Debug, Clone, PartialEq)]
pub enum LifecycleEvent {
    /// An observation was recorded.
    ObservationRecorded(ObservationRecorded),
    /// A record proposal was created.
    RecordProposalCreated(RecordProposalCreated),
    /// A promotion event was recorded.
    PromotionRecorded(PromotionRecorded),
    /// A context use was recorded.
    ContextUseRecorded(ContextUseRecorded),
    /// Feedback on a context use was recorded.
    ContextUseFeedbackRecorded(ContextUseFeedbackRecorded),
    /// Missing context was detected.
    MissingContextDetected(MissingContextDetected),
    /// An artifact contract was selected.
    ArtifactContractSelected(ArtifactContractSelected),
    /// A contract validation completed.
    ContractValidationCompleted(ContractValidationCompleted),
    /// An outcome was assessed.
    OutcomeAssessed(OutcomeAssessed),
    /// A compiled context frame was built.
    CompiledContextFrameBuilt(CompiledContextFrameBuilt),
    /// A forward-compatible event this binary does not recognize.
    Unknown {
        /// The unrecognized event type.
        event_type: String,
        /// Its preserved payload.
        payload: Value,
    },
}

/// Stable-ID payload of `observation_recorded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationRecorded {
    /// The observation record id.
    pub observation_id: String,
    /// Its observation kind (string; the typed enum lives in stella-core).
    pub observation_kind: String,
}

/// Stable-ID payload of `record_proposal_created`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordProposalCreated {
    /// The proposal record id.
    pub proposal_id: String,
    /// Its proposal kind (string).
    pub proposal_kind: String,
}

/// Stable-ID payload of `promotion_recorded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionRecorded {
    /// The promotion event id.
    pub promotion_event_id: String,
    /// The source record.
    pub source_record_id: String,
    /// The resulting record (a new immutable revision).
    pub result_record_id: String,
    /// The promotion action (string).
    pub action: String,
}

/// Stable-ID payload of `context_use_recorded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUseRecorded {
    /// The context-use record id.
    pub context_use_id: String,
    /// The record that was used.
    pub context_record_id: String,
    /// The use-trace id.
    pub use_trace_id: String,
}

/// Stable-ID payload of `context_use_feedback_recorded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUseFeedbackRecorded {
    /// The feedback record id.
    pub context_use_feedback_id: String,
    /// The context use it is about.
    pub context_use_id: String,
}

/// Stable-ID payload of `missing_context_detected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingContextDetected {
    /// The missing-context observation id.
    pub missing_context_id: String,
    /// Its missing-context kind (string).
    pub missing_context_kind: String,
}

/// Stable-ID payload of `artifact_contract_selected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactContractSelected {
    /// The selected contract record id.
    pub contract_record_id: String,
    /// The exact contract version selected.
    pub contract_version: String,
}

/// Stable-ID payload of `contract_validation_completed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractValidationCompleted {
    /// The contract validation record id.
    pub contract_validation_id: String,
    /// The contract it validated.
    pub contract_record_id: String,
    /// The validation status (string).
    pub validation_status: String,
}

/// Stable-ID payload of `outcome_assessed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeAssessed {
    /// The outcome-assessment record id.
    pub outcome_assessment_id: String,
}

/// Stable-ID payload of `compiled_context_frame_built`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledContextFrameBuilt {
    /// The compiled frame id.
    pub compiled_frame_id: String,
    /// Its byte-stable frame hash (`sha256:<hex>`).
    pub frame_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(event_type: &str, payload: Value) -> LifecycleEventEnvelope {
        LifecycleEventEnvelope {
            schema_version: "1.0-draft".into(),
            event_type: event_type.into(),
            event_id: "evt_1".into(),
            observed_at: "2026-07-20T18:30:00Z".into(),
            task_id: Some("task_1".into()),
            invocation_id: None,
            payload,
        }
    }

    #[test]
    fn a_recognized_event_decodes_to_its_typed_form() {
        let env = envelope(
            event_type::COMPILED_CONTEXT_FRAME_BUILT,
            json!({ "compiled_frame_id": "cf_1", "frame_hash": "sha256:aa" }),
        );
        assert_eq!(
            env.decode(),
            LifecycleEvent::CompiledContextFrameBuilt(CompiledContextFrameBuilt {
                compiled_frame_id: "cf_1".into(),
                frame_hash: "sha256:aa".into(),
            })
        );
    }

    #[test]
    fn a_future_event_type_round_trips_without_error() {
        // THE witness: the exact failure mode Phase 0 found in parse_jsonl —
        // an unknown interior event must NOT break the reader.
        let wire = r#"{
            "schema_version": "2.0",
            "event_type": "future_event_v99",
            "event_id": "evt_future",
            "observed_at": "2027-01-01T00:00:00Z",
            "payload": { "some_new_field": [1, 2, 3] }
        }"#;
        let env: LifecycleEventEnvelope =
            serde_json::from_str(wire).expect("unknown event types must still deserialize");
        // decode preserves it as Unknown with its payload intact.
        match env.decode() {
            LifecycleEvent::Unknown {
                event_type,
                payload,
            } => {
                assert_eq!(event_type, "future_event_v99");
                assert_eq!(payload, json!({ "some_new_field": [1, 2, 3] }));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        // and re-serializes losslessly.
        let back = serde_json::to_value(&env).unwrap();
        assert_eq!(back["event_type"], json!("future_event_v99"));
        assert_eq!(back["payload"], json!({ "some_new_field": [1, 2, 3] }));
    }

    #[test]
    fn a_recognized_type_with_a_bad_payload_degrades_to_unknown_not_error() {
        // Recognized event_type but payload missing required fields → preserved
        // as Unknown rather than panicking the decoder.
        let env = envelope(event_type::OBSERVATION_RECORDED, json!({ "wrong": true }));
        assert!(matches!(env.decode(), LifecycleEvent::Unknown { .. }));
    }

    #[test]
    fn envelope_omits_absent_identity_fields() {
        let env = LifecycleEventEnvelope {
            schema_version: "1.0-draft".into(),
            event_type: event_type::OUTCOME_ASSESSED.into(),
            event_id: "evt_1".into(),
            observed_at: "2026-07-20T18:30:00Z".into(),
            task_id: None,
            invocation_id: None,
            payload: json!({ "outcome_assessment_id": "oa_1" }),
        };
        let wire = serde_json::to_value(&env).unwrap();
        assert!(wire.get("task_id").is_none(), "absent task_id omitted");
        assert!(wire.get("invocation_id").is_none());
    }
}
