//! Lossless OCP-to-pipeline recall projection.

use std::collections::HashSet;

use stella_pipeline::RecalledFrame;

/// Preserve host-owned provider identity and the frame's complete provenance.
/// Source is the origin-most actor; method is the latest derivation step.
pub(super) fn project_recalled_frame(
    attributed: crate::ocp::AttributedContextFrame,
) -> Option<RecalledFrame> {
    let frame = attributed.frame;
    let citation_label = frame.citation_label.clone()?;
    let source = frame
        .provenance
        .iter()
        .find_map(|entry| entry.by.clone())
        .unwrap_or_else(|| attributed.provider.clone());
    let method = frame
        .provenance
        .iter()
        .rev()
        .find_map(|entry| entry.method.clone());
    let uri = frame
        .uri
        .clone()
        .or_else(|| frame.provenance.iter().find_map(|entry| entry.uri.clone()));
    let kind = serde_json::to_value(frame.kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    Some(RecalledFrame {
        citation_label,
        provider: attributed.provider,
        source,
        kind,
        uri,
        method,
        content: frame.content.trim().to_string(),
        token_cost: frame.token_cost,
        id: Some(frame.id),
    })
}

pub(super) fn is_quarantined_local_memory(
    frame: &RecalledFrame,
    quarantined: &HashSet<String>,
) -> bool {
    frame.provider == "workspace-memory"
        && frame.kind == "memory"
        && frame.id.as_ref().is_some_and(|id| quarantined.contains(id))
}
