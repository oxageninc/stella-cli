//! `context/query` request/response shapes
//!. Budget-aware
//! by contract: every query carries `max_tokens`; a conforming provider
//! never returns more than the budget and never lies about cost.

use serde::{Deserialize, Serialize};

use crate::frame::{ContextFrame, FrameKind};

/// A request to an OCP provider for context frames relevant to a goal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextQuery {
    /// The task/turn goal driving retrieval.
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<FrameKind>,
    /// Anchor URIs (open files, mentioned symbols) used for graph-proximity
    /// scoring.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anchors: Vec<String>,
    pub max_frames: u32,
    pub max_tokens: u32,
    /// Pin retrieval to a point in time for bi-temporal facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
}

/// The response to a `context/query` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextQueryResult {
    pub frames: Vec<ContextFrame>,
    /// True if the provider had more candidates than fit the budget.
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_estimate: Option<u32>,
}

impl ContextQueryResult {
    /// Sum of `token_cost` across returned frames — must never exceed the
    /// query's `max_tokens` for a conforming provider (checked in
    /// `ocp-conformance`, phase 3; this is the cheap client-side sanity
    /// check any host can run today).
    pub fn total_token_cost(&self) -> u64 {
        self.frames.iter().map(|f| f.token_cost as u64).sum()
    }

    pub fn respects_budget(&self, max_tokens: u32) -> bool {
        self.total_token_cost() <= max_tokens as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::ContextFrame;

    fn frame_with_cost(id: &str, cost: u32) -> ContextFrame {
        ContextFrame {
            id: id.into(),
            kind: FrameKind::Snippet,
            title: id.into(),
            content: String::new(),
            uri: None,
            score: 0.5,
            token_cost: cost,
            valid_from: None,
            valid_to: None,
            recorded_at: None,
            provenance: vec![],
            citation_label: None,
            embedding: None,
            relations: vec![],
        }
    }

    #[test]
    fn context_query_roundtrips() {
        let query = ContextQuery {
            goal: "fix the failing test".into(),
            query_text: Some("failing test".into()),
            embedding: None,
            kinds: vec![FrameKind::Symbol, FrameKind::Doc],
            anchors: vec!["file:///repo/src/lib.rs".into()],
            max_frames: 20,
            max_tokens: 4000,
            as_of: None,
        };
        let json = serde_json::to_string(&query).unwrap();
        let back: ContextQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(back, query);
    }

    #[test]
    fn respects_budget_true_when_under_or_at_limit() {
        let result = ContextQueryResult {
            frames: vec![frame_with_cost("a", 100), frame_with_cost("b", 200)],
            truncated: false,
            dropped_estimate: None,
        };
        assert_eq!(result.total_token_cost(), 300);
        assert!(result.respects_budget(300));
        assert!(result.respects_budget(500));
    }

    #[test]
    fn respects_budget_false_when_provider_lies_about_cost() {
        let result = ContextQueryResult {
            frames: vec![frame_with_cost("a", 400)],
            truncated: false,
            dropped_estimate: None,
        };
        assert!(!result.respects_budget(300));
    }
}
