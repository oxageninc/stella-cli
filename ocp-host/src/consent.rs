//! Consent gating for egress providers (
//! point 4).
//!
//! The security-critical rule: a provider that declares `egress` — anything
//! that could send workspace content off the local machine — MUST NOT be
//! queried until the user has recorded explicit, one-time consent that
//! **names what leaves**. A host never auto-enables egress. Read/write-only
//! providers carry no such gate. The store is in-memory and serde-able so a
//! host can persist the user's decisions across runs (task deliverable 4).

use std::collections::HashMap;

use ocp_types::{DataFlow, ProviderInfo};
use serde::{Deserialize, Serialize};

/// A recorded consent decision for one provider. `granted_scope` is the
/// human-readable description of what data flows out, shown to the user at
/// consent time and retained as the audit of what they agreed to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsentRecord {
    /// The provider id this consent applies to (the host's routing key).
    pub provider_id: String,
    /// The data-flow direction the user consented to — names what leaves.
    pub data_flow: DataFlow,
    /// Human-readable scope: what content is permitted to leave the machine.
    pub granted_scope: String,
    /// When consent was granted (RFC 3339), if the host records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<String>,
}

impl ConsentRecord {
    /// Record consent for a provider, naming the data-flow direction and the
    /// scope of what may leave.
    pub fn new(
        provider_id: impl Into<String>,
        data_flow: DataFlow,
        granted_scope: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            data_flow,
            granted_scope: granted_scope.into(),
            granted_at: None,
        }
    }
}

/// The set of consent decisions a host holds, keyed by provider id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConsentStore {
    #[serde(default)]
    records: HashMap<String, ConsentRecord>,
}

impl ConsentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or replace) consent for a provider.
    pub fn record(&mut self, record: ConsentRecord) {
        self.records.insert(record.provider_id.clone(), record);
    }

    /// Withdraw consent for a provider, returning the prior record if any.
    pub fn revoke(&mut self, provider_id: &str) -> Option<ConsentRecord> {
        self.records.remove(provider_id)
    }

    /// The recorded decision for a provider, if consent was granted.
    pub fn get(&self, provider_id: &str) -> Option<&ConsentRecord> {
        self.records.get(provider_id)
    }

    /// Whether consent has been recorded for a provider.
    pub fn is_consented(&self, provider_id: &str) -> bool {
        self.records.contains_key(provider_id)
    }

    /// Whether a provider needs consent before any query: only egress
    /// providers do. A read/write-only provider is always permitted —
    /// nothing it can do leaves the machine.
    pub fn requires_consent(info: &ProviderInfo) -> bool {
        info.data_flow.egress
    }

    /// The gate the host consults before transmitting a query: may we send
    /// the payload to this provider right now? True unless it declares egress
    /// and lacks a recorded consent.
    pub fn permits(&self, id: &str, info: &ProviderInfo) -> bool {
        !Self::requires_consent(info) || self.is_consented(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn egress_info() -> ProviderInfo {
        ProviderInfo {
            name: "ocp-github".into(),
            version: "0.1.0".into(),
            data_flow: DataFlow {
                reads: true,
                writes: false,
                egress: true,
            },
        }
    }

    fn local_info() -> ProviderInfo {
        ProviderInfo {
            name: "ocp-docs".into(),
            version: "0.1.0".into(),
            data_flow: DataFlow {
                reads: true,
                writes: false,
                egress: false,
            },
        }
    }

    #[test]
    fn local_providers_never_need_consent() {
        let store = ConsentStore::new();
        let info = local_info();
        assert!(!ConsentStore::requires_consent(&info));
        assert!(store.permits("ocp-docs", &info));
    }

    #[test]
    fn egress_providers_are_gated_until_consent_is_recorded() {
        let mut store = ConsentStore::new();
        let info = egress_info();
        assert!(ConsentStore::requires_consent(&info));
        // No consent yet → the gate is shut.
        assert!(!store.permits("ocp-github", &info));

        store.record(ConsentRecord::new(
            "ocp-github",
            info.data_flow,
            "open issue titles + bodies leave to github.com",
        ));
        assert!(store.permits("ocp-github", &info));
        assert_eq!(
            store.get("ocp-github").map(|r| r.granted_scope.as_str()),
            Some("open issue titles + bodies leave to github.com")
        );
    }

    #[test]
    fn revoking_consent_reshuts_the_gate() {
        let mut store = ConsentStore::new();
        let info = egress_info();
        store.record(ConsentRecord::new("ocp-github", info.data_flow, "issues"));
        assert!(store.permits("ocp-github", &info));
        let revoked = store.revoke("ocp-github").expect("a record existed");
        assert_eq!(revoked.provider_id, "ocp-github");
        assert!(!store.permits("ocp-github", &info));
    }

    #[test]
    fn consent_store_is_serde_able_for_persistence() {
        let mut store = ConsentStore::new();
        store.record(ConsentRecord::new(
            "ocp-github",
            DataFlow {
                reads: true,
                writes: false,
                egress: true,
            },
            "issues + PRs",
        ));
        let json = serde_json::to_string(&store).unwrap();
        let back: ConsentStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back, store);
        assert!(back.is_consented("ocp-github"));
    }
}
