//! `ContextFrame` — the unit of exchange between an OCP host and a provider.
//! fixes this exact
//! shape; frames, never blobs, carry relevance, cost, and provenance so a
//! budgeting, citing host can compose sources honestly.

use serde::{Deserialize, Serialize};

/// What kind of thing a frame represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    Snippet,
    Symbol,
    Fact,
    Doc,
    Memory,
    Episode,
    Graph,
}

/// One link in a frame's provenance chain, ordered closest-to-source first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// e.g. "file", "derivation", "episode".
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// A graph relation a frame participates in, surfaced with a human label —
/// raw ids are never the primary identifier (§3.3 "display_name mandatory").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub rel: String,
    pub target_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// The optional embedding carried by a frame. The vector itself is
/// elidable — a host may want the fingerprint without the payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameEmbedding {
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
}

/// One context frame returned from `context/query`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextFrame {
    /// Provider-scoped, stable for dedup across queries.
    pub id: String,
    pub kind: FrameKind,
    /// Human label — never a bare uuid.
    pub title: String,
    /// Text the host may quote into a prompt. Untrusted data: a conforming
    /// host delimits this as quoted material, never as instructions.
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Provider-normalized relevance in `[0, 1]`.
    pub score: f32,
    /// Honest, conformance-audited token cost.
    pub token_cost: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<Provenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citation_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<FrameEmbedding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<Relation>,
}

impl ContextFrame {
    /// Score must be normalized into `[0, 1]` per the protocol contract.
    /// Conformance suites assert this; providers should self-check too.
    pub fn has_valid_score(&self) -> bool {
        (0.0..=1.0).contains(&self.score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame() -> ContextFrame {
        ContextFrame {
            id: "frm_1".into(),
            kind: FrameKind::Snippet,
            title: "workspace.ts L120-160".into(),
            content: "export interface Workspace { ... }".into(),
            uri: Some("file:///repo/workspace.ts".into()),
            score: 0.83,
            token_cost: 412,
            valid_from: None,
            valid_to: None,
            recorded_at: Some("2026-07-10T00:00:00Z".into()),
            provenance: vec![Provenance {
                kind: "file".into(),
                uri: Some("file:///repo/workspace.ts".into()),
                range: Some("L120-160".into()),
                digest: Some("sha256:abc".into()),
                method: None,
                by: None,
            }],
            citation_label: Some("workspace.ts L120-160".into()),
            embedding: None,
            relations: vec![],
        }
    }

    #[test]
    fn context_frame_roundtrips_through_json() {
        let frame = sample_frame();
        let json = serde_json::to_string(&frame).unwrap();
        let back: ContextFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn score_out_of_range_fails_the_conformance_check() {
        let mut frame = sample_frame();
        assert!(frame.has_valid_score());
        frame.score = 1.5;
        assert!(!frame.has_valid_score());
    }

    #[test]
    fn optional_fields_are_omitted_when_absent() {
        let frame = sample_frame();
        let mut minimal = frame.clone();
        minimal.uri = None;
        minimal.valid_from = None;
        minimal.provenance.clear();
        let json = serde_json::to_string(&minimal).unwrap();
        assert!(!json.contains("\"uri\""));
        assert!(!json.contains("\"provenance\""));
    }
}
