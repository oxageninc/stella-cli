//! Host-owned approval for paid media generation.
//!
//! Model tool arguments are untrusted requests, not authority. Every image
//! and video provider submission therefore crosses this asynchronous port
//! before it can spend money. Hosts may inject an interactive or governed
//! implementation; unattended callers receive [`DenyMediaSpendGate`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use stella_protocol::MediaKind;

/// A content-free description of a proposed provider submission.
///
/// Prompts and artifact labels are deliberately excluded: an approval host
/// needs the provider, media kind, and estimated charge, not user content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaSpendRequest {
    pub operation_id: String,
    pub kind: MediaKind,
    pub provider_id: String,
    pub estimated_usd: Option<f64>,
    pub detail: String,
}

impl MediaSpendRequest {
    pub fn new(
        operation_id: impl Into<String>,
        kind: MediaKind,
        provider_id: impl Into<String>,
        estimated_usd: Option<f64>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            kind,
            provider_id: provider_id.into(),
            estimated_usd,
            detail: detail.into(),
        }
    }
}

/// A host's decision for one proposed media submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostDecision {
    Approve,
    Deny,
}

/// Host approval port for paid media submissions.
#[async_trait]
pub trait MediaSpendGate: Send + Sync {
    async fn authorize(&self, request: &MediaSpendRequest) -> CostDecision;
}

/// Secure default for callers without an explicit host approval surface.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyMediaSpendGate;

#[async_trait]
impl MediaSpendGate for DenyMediaSpendGate {
    async fn authorize(&self, _request: &MediaSpendRequest) -> CostDecision {
        CostDecision::Deny
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> MediaSpendRequest {
        MediaSpendRequest::new(
            "mop_deadbeef",
            MediaKind::Video,
            "zai",
            Some(0.4),
            "5s video @ $0.0800/s",
        )
    }

    #[tokio::test]
    async fn default_gate_denies() {
        assert_eq!(
            DenyMediaSpendGate.authorize(&request()).await,
            CostDecision::Deny
        );
    }

    #[test]
    fn spend_request_round_trips_through_json_byte_stably() {
        let encoded = serde_json::to_vec(&request()).unwrap();
        assert_eq!(
            std::str::from_utf8(&encoded).unwrap(),
            r#"{"operation_id":"mop_deadbeef","kind":"video","provider_id":"zai","estimated_usd":0.4,"detail":"5s video @ $0.0800/s"}"#
        );
        let decoded: MediaSpendRequest = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, request());
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), encoded);
        assert!(
            serde_json::from_str::<MediaSpendRequest>(
                r#"{"operation_id":"mop_deadbeef","kind":"video","provider_id":"zai","estimated_usd":0.4,"detail":"x","unexpected":true}"#
            )
            .is_err(),
            "authority requests must reject unknown fields"
        );

        let decision = serde_json::to_vec(&CostDecision::Approve).unwrap();
        assert_eq!(std::str::from_utf8(&decision).unwrap(), r#""approve""#);
        assert_eq!(
            serde_json::from_slice::<CostDecision>(&decision).unwrap(),
            CostDecision::Approve
        );
    }
}
