//! Handshake and capability negotiation types
//! (`docs/specs/oxagen-rust-cli/06-context-protocol.md` §3.2). `DataFlow` is
//! the security-critical field: hosts surface it at install/consent time,
//! and `egress: true` providers must never be auto-enabled (§3.5).

use serde::{Deserialize, Serialize};

/// Declares what a provider does with data, so a host can gate consent
/// before ever sending it a query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataFlow {
    /// Can see workspace content via query payloads.
    #[serde(default)]
    pub reads: bool,
    /// Persists `context/upsert` writes.
    #[serde(default)]
    pub writes: bool,
    /// Sends anything off the local machine. A host MUST require explicit,
    /// one-time consent before enabling a provider with `egress: true`.
    #[serde(default)]
    pub egress: bool,
}

/// Provider identity reported at `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub version: String,
    pub data_flow: DataFlow,
}

/// What a provider can do, negotiated at handshake time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub query: QueryCapability,
    #[serde(default)]
    pub upsert: bool,
    #[serde(default)]
    pub graph: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings_fingerprint: Option<String>,
    #[serde(default)]
    pub subscribe: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryCapability {
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub filters: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egress_provider_data_flow_roundtrips() {
        let flow = DataFlow {
            reads: true,
            writes: false,
            egress: true,
        };
        let json = serde_json::to_string(&flow).unwrap();
        let back: DataFlow = serde_json::from_str(&json).unwrap();
        assert_eq!(back, flow);
        assert!(
            back.egress,
            "egress providers must be inspectable by hosts before consent"
        );
    }

    #[test]
    fn provider_info_defaults_data_flow_to_no_egress() {
        let flow = DataFlow::default();
        assert!(
            !flow.egress,
            "default DataFlow must never imply egress consent"
        );
    }

    #[test]
    fn capabilities_roundtrip_with_defaults() {
        let caps = Capabilities {
            query: QueryCapability {
                kinds: vec!["snippet".into()],
                filters: vec![],
            },
            upsert: true,
            graph: false,
            embeddings_fingerprint: Some("bge-small-v1".into()),
            subscribe: false,
        };
        let json = serde_json::to_string(&caps).unwrap();
        let back: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(back, caps);
    }
}
