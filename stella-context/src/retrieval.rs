//! The hybrid, budgeted, cited retrieval pipeline (`06-context-protocol.md`
//! §2.3, `02-architecture.md` §7). [`ContextStore::recall`] fuses three signals
//! — vector similarity, recency, and 1-hop graph adjacency — via reciprocal-
//! rank fusion, dedupes by content hash, diversifies with MMR, then **packs to
//! the query's token budget and reports what was dropped** (silent truncation
//! is banned, `L-C5`). Every frame carries a human `citation_label` (`L-C4`).
//! When graph/vector coverage of the goal is weak, it falls back to bounded
//! lexical search and **labels those frames as lexical fallback** rather than
//! dressing weak context up as grounding (`L-C6`).
//!
//! The scoring/fusion/packing steps are plain synchronous functions over owned
//! data (`02-architecture.md` §1.3) — brute-force top-k cosine is fine at
//! CLI-local scale; an ANN accelerator is a size-threshold follow-up per
//! `02-architecture.md` §6. They are property-tested at the bottom of the file.

use std::collections::{HashMap, HashSet};

use ocp_types::frame::FrameEmbedding;
use ocp_types::{ContextFrame, ContextQuery, Provenance};

use crate::error::ContextError;
use crate::store::{
    ContextStore, NodeRow, domains_for_node, live_nodes, neighbors, node_ids_for_uris,
    node_ids_in_domains, vectors_for_fingerprint,
};

/// Provenance `kind` marking a frame's domain tag, so a citation view can show
/// the domains a frame belongs to (scope update: "tag data rides provenance").
pub(crate) const DOMAIN_PROVENANCE_KIND: &str = "domain";

/// Provider identity stamped into frame provenance.
pub(crate) const PROVIDER_ID: &str = "stella-context/0.1";
/// The lexical-fallback marker written into a frame's provenance chain so a
/// host can see the frame is a weak-coverage substitute, not graph grounding.
pub(crate) const LEXICAL_FALLBACK_METHOD: &str = "stella-context/lexical-fallback";

/// Reciprocal-rank-fusion constant (the standard 60).
const RRF_K: f64 = 60.0;
/// MMR relevance/diversity trade-off; 0.7 favors relevance while still
/// breaking up near-duplicate clusters.
const MMR_LAMBDA: f32 = 0.7;
/// Below this mean top-k cosine, retrieval is deemed low-coverage and falls
/// back to lexical search (`L-C6`).
const MIN_COVERAGE: f32 = 0.15;
/// How many top vector hits define the coverage estimate.
const COVERAGE_TOPK: usize = 5;
/// Graph expansion seeds beyond anchors: the strongest vector hits.
const MAX_VECTOR_SEEDS: usize = 8;
/// Cap on lexical-fallback frames added.
const LEXICAL_LIMIT: usize = 8;

/// Why a candidate frame did not make it into the assembled context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Keeping it would have exceeded the query's `max_tokens`.
    TokenBudget,
    /// The query's `max_frames` count was already reached.
    FrameCount,
}

/// A frame that was retrieved and scored but did not fit the budget. Reported
/// so assembly is never a silent truncation (`L-C5`).
#[derive(Debug, Clone)]
pub struct DroppedFrame {
    pub id: String,
    pub title: String,
    pub token_cost: u32,
    pub reason: DropReason,
}

/// The typed, inspectable result of a recall (`02-architecture.md` §1: typed
/// outputs, not stringly telemetry). Carries the packed frames, the dropped
/// report, the coverage score, and the honesty flag for lexical fallback.
#[derive(Debug, Clone)]
pub struct RecallResult {
    /// Budget-respecting, MMR-ordered frames ready to assemble into a prompt.
    pub frames: Vec<ContextFrame>,
    /// What was scored but dropped, and why (`L-C5`).
    pub dropped: Vec<DroppedFrame>,
    /// Mean top-k vector coverage of the goal, in `[0, 1]`.
    pub coverage: f32,
    /// True when coverage fell below threshold and lexical fallback ran
    /// (`L-C6`). Individual fallback frames are also marked in their provenance.
    pub used_lexical_fallback: bool,
}

impl RecallResult {
    /// Total token cost of the assembled frames — must never exceed the
    /// query's `max_tokens` (the invariant the packer guarantees).
    pub fn assembled_tokens(&self) -> u64 {
        self.frames.iter().map(|f| f.token_cost as u64).sum()
    }
}

impl ContextStore {
    /// Hybrid retrieval with no domain scope — grounding drawn from the whole
    /// workspace. The OCP-shaped `ContextProvider::query` adapts this down to
    /// `Vec<ContextFrame>`.
    pub async fn recall(&self, q: &ContextQuery) -> Result<RecallResult, ContextError> {
        self.recall_scoped(q, &[]).await
    }

    /// Hybrid retrieval scoped to `domains` (scope update): fuse → dedup →
    /// diversify → budget-pack → coverage gate. When `domains` is non-empty it
    /// **filters** candidates to nodes tagged with at least one of them AND
    /// **boosts** relevance by domain overlap (a frame sharing more of the
    /// query's domains ranks higher). An empty `domains` slice behaves exactly
    /// like [`Self::recall`]. Every returned frame carries its domains in
    /// provenance so a citation view can show them.
    pub async fn recall_scoped(
        &self,
        q: &ContextQuery,
        domains: &[String],
    ) -> Result<RecallResult, ContextError> {
        // 1. Query vector: reuse the caller's if it matches our dims, else
        //    embed the query text ourselves. This is the ONLY embedding recall
        //    ever does — it never embeds stored content inline; that is warm's
        //    job (`L-C1`). So a cold store degrades to lexical, it never blocks.
        let dims = self.fingerprint().dims;
        let query_vec = match &q.embedding {
            Some(v) if v.len() == dims => v.clone(),
            _ => {
                let text = q.query_text.clone().unwrap_or_else(|| q.goal.clone());
                self.embedder()
                    .embed(&[text])
                    .await?
                    .into_iter()
                    .next()
                    .map(|e| e.vector)
                    .unwrap_or_else(|| vec![0.0; dims])
            }
        };

        // 2. Gather candidates under one lock — no await is held here. The
        //    domain filter (if any) is applied here so every downstream signal
        //    sees only the in-scope nodes.
        let fp_id = self.fingerprint().id();
        let (nodes, vectors, anchor_ids, domains_by_id) = {
            let conn = self.conn();
            let mut nodes = live_nodes(&conn)?;
            let mut vectors = vectors_for_fingerprint(&conn, &fp_id)?;
            let anchor_ids = node_ids_for_uris(&conn, &q.anchors)?;
            if let Some(allowed) = node_ids_in_domains(&conn, domains)? {
                nodes.retain(|n| allowed.contains(&n.id));
                vectors.retain(|(id, _)| allowed.contains(id));
            }
            let mut domains_by_id: HashMap<i64, Vec<String>> = HashMap::new();
            for n in &nodes {
                domains_by_id.insert(n.id, domains_for_node(&conn, n.id)?);
            }
            (nodes, vectors, anchor_ids, domains_by_id)
        };

        let node_by_id: HashMap<i64, &NodeRow> = nodes.iter().map(|n| (n.id, n)).collect();
        let vector_by_id: HashMap<i64, &Vec<f32>> =
            vectors.iter().map(|(id, v)| (*id, v)).collect();
        let no_domains: Vec<String> = Vec::new();
        let frame_domains_for = |id: &i64| domains_by_id.get(id).unwrap_or(&no_domains).as_slice();

        // 3a. Vector-similarity ranking + the cosine values coverage reads.
        let mut cos_scored: Vec<(i64, f32)> = vectors
            .iter()
            .map(|(id, v)| (*id, cosine(&query_vec, v)))
            .collect();
        cos_scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let coverage = coverage_score(&cos_scored);

        // 3b. Domain-overlap ranking (only when the query is domain-scoped):
        //     nodes sharing more of the query's domains rank higher. Folded
        //     into RRF like any other signal.
        let query_domains: HashSet<&str> = domains.iter().map(String::as_str).collect();
        let domain_ranked: Vec<i64> = if query_domains.is_empty() {
            Vec::new()
        } else {
            let mut scored: Vec<(i64, usize)> = nodes
                .iter()
                .filter_map(|n| {
                    let overlap = frame_domains_for(&n.id)
                        .iter()
                        .filter(|d| query_domains.contains(d.as_str()))
                        .count();
                    (overlap > 0).then_some((n.id, overlap))
                })
                .collect();
            scored.sort_by_key(|entry| std::cmp::Reverse(entry.1));
            scored.into_iter().map(|(id, _)| id).collect()
        };

        // 4. Coverage gate (`L-C6`). Below threshold the vector signal is too
        //    weak to trust; rather than dress fused graph/recency hits up as
        //    grounding, serve bounded lexical matches, **explicitly labeled**.
        //    Above threshold, fuse the signals into real grounding.
        let used_lexical_fallback = coverage < MIN_COVERAGE;
        let candidates: Vec<ContextFrame> = if used_lexical_fallback {
            let terms = query_terms(q);
            let mut frames = Vec::new();
            for (id, score) in lexical_search(&nodes, &terms, LEXICAL_LIMIT) {
                if let Some(node) = node_by_id.get(&id) {
                    frames.push(frame_from_node(
                        node,
                        score,
                        &fp_id,
                        true,
                        frame_domains_for(&id),
                    )?);
                }
            }
            frames
        } else {
            let vector_ranked: Vec<i64> = cos_scored.iter().map(|(id, _)| *id).collect();

            // 4a. Recency ranking — recorded_at is fixed-width RFC-3339, so a
            //     descending string sort IS descending time order (no parsing).
            let mut recency: Vec<&NodeRow> = nodes.iter().collect();
            recency.sort_by(|a, b| b.recorded_at.cmp(&a.recorded_at));
            let recency_ranked: Vec<i64> = recency.iter().map(|n| n.id).collect();

            // 4b. Graph adjacency: 1-hop from anchors + strongest vector hits.
            let mut seeds: Vec<i64> = anchor_ids.clone();
            seeds.extend(vector_ranked.iter().take(MAX_VECTOR_SEEDS).copied());
            seeds.sort_unstable();
            seeds.dedup();
            let mut graph_weight: HashMap<i64, f64> = HashMap::new();
            for &s in &seeds {
                // Seeds themselves are relevant context (an open file, a
                // mentioned symbol), so they enter the list with a base weight.
                *graph_weight.entry(s).or_insert(0.0) += 1.0;
            }
            for (neighbor, weight) in neighbors(&self.conn(), &seeds, q.as_of.as_deref())? {
                *graph_weight.entry(neighbor).or_insert(0.0) += weight;
            }
            let mut graph_scored: Vec<(i64, f64)> = graph_weight.into_iter().collect();
            graph_scored.sort_by(|a, b| b.1.total_cmp(&a.1));
            let graph_ranked: Vec<i64> = graph_scored.iter().map(|(id, _)| *id).collect();

            // 4c. Fuse (RRF) → dedup by content hash → MMR diversity pass.
            let fused = rrf_fuse(
                &[vector_ranked, recency_ranked, graph_ranked, domain_ranked],
                RRF_K,
            );
            let ordered = dedup_by_content_hash(&fused, &node_by_id);
            let max_fused = ordered.first().map(|(_, s)| *s).unwrap_or(0.0);
            let mmr_items: Vec<MmrItem> = ordered
                .iter()
                .map(|(id, s)| MmrItem {
                    relevance: if max_fused > 0.0 {
                        (*s / max_fused) as f32
                    } else {
                        0.0
                    },
                    vector: vector_by_id.get(id).map(|v| (*v).clone()),
                })
                .collect();
            let mmr_order = mmr_select(&mmr_items, MMR_LAMBDA);

            let mut frames = Vec::with_capacity(mmr_order.len());
            for &idx in &mmr_order {
                let (id, _) = ordered[idx];
                if let Some(node) = node_by_id.get(&id) {
                    frames.push(frame_from_node(
                        node,
                        mmr_items[idx].relevance,
                        &fp_id,
                        false,
                        frame_domains_for(&id),
                    )?);
                }
            }
            frames
        };

        // 5. Budget-pack; report what was dropped (`L-C5`, never silent).
        let (kept, dropped) = pack_to_budget(candidates, q.max_tokens, q.max_frames);
        Ok(RecallResult {
            frames: kept,
            dropped,
            coverage,
            used_lexical_fallback,
        })
    }
}

/// Build a frame from a node. **Constructor-level enforcement of `L-C4`:** a
/// node without a human label yields `Err(MissingCitation)`, never a frame
/// with a bare id as its identifier.
pub(crate) fn frame_from_node(
    node: &NodeRow,
    score: f32,
    fingerprint: &str,
    lexical: bool,
    domains: &[String],
) -> Result<ContextFrame, ContextError> {
    let label = node.display_name.trim();
    if label.is_empty() {
        return Err(ContextError::MissingCitation {
            id: node.public_id.clone(),
        });
    }
    let mut provenance = vec![Provenance {
        kind: "node".into(),
        uri: node.uri.clone(),
        range: None,
        digest: Some(format!("sha256:{}", node.content_hash)),
        method: None,
        by: Some(PROVIDER_ID.into()),
    }];
    if lexical {
        provenance.push(Provenance {
            kind: "derivation".into(),
            uri: None,
            range: None,
            digest: None,
            method: Some(LEXICAL_FALLBACK_METHOD.into()),
            by: Some(PROVIDER_ID.into()),
        });
    }
    if !domains.is_empty() {
        // Domain tags ride provenance so citation views can show which
        // workspace domains a frame belongs to (user requirement: domains
        // tag all graph nodes/edges; recall scores domain overlap).
        provenance.push(Provenance {
            kind: DOMAIN_PROVENANCE_KIND.into(),
            uri: None,
            range: None,
            digest: None,
            method: Some(domains.join(",")),
            by: Some(PROVIDER_ID.into()),
        });
    }
    Ok(ContextFrame {
        id: node.public_id.clone(),
        kind: node.kind.to_frame_kind(),
        title: node.display_name.clone(),
        content: node.content.clone(),
        uri: node.uri.clone(),
        score: score.clamp(0.0, 1.0),
        token_cost: estimate_tokens(&node.content) + estimate_tokens(&node.display_name),
        valid_from: None,
        valid_to: None,
        recorded_at: Some(node.recorded_at.clone()),
        provenance,
        citation_label: Some(label.to_string()),
        // The vector payload is elided; the fingerprint tags provenance.
        embedding: Some(FrameEmbedding {
            fingerprint: fingerprint.to_string(),
            vector: None,
        }),
        relations: vec![],
    })
}

/// Whether a frame is a lexical-fallback frame (`L-C6`), by inspecting its
/// provenance chain. Lets a host label weak-coverage context honestly.
pub fn is_lexical_fallback(frame: &ContextFrame) -> bool {
    frame
        .provenance
        .iter()
        .any(|p| p.method.as_deref() == Some(LEXICAL_FALLBACK_METHOD))
}

/// A rough token estimate (~4 chars/token). Honest enough for budgeting; a
/// real tokenizer is a follow-up but would not change the packing invariants.
pub(crate) fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() as u32).div_ceil(4)
}

/// Cosine similarity, guarding zero-norm vectors (defined as 0 similarity).
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Mean of the top-k positive cosine values — the goal-coverage estimate.
fn coverage_score(cos_sorted: &[(i64, f32)]) -> f32 {
    if cos_sorted.is_empty() {
        return 0.0;
    }
    let k = COVERAGE_TOPK.min(cos_sorted.len());
    let sum: f32 = cos_sorted.iter().take(k).map(|(_, c)| c.max(0.0)).sum();
    sum / k as f32
}

/// Reciprocal-rank fusion over several ranked id lists.
fn rrf_fuse(lists: &[Vec<i64>], k: f64) -> HashMap<i64, f64> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        for (rank, &id) in list.iter().enumerate() {
            *scores.entry(id).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
        }
    }
    scores
}

/// Collapse fused scores to one entry per content hash (keep the strongest),
/// returning `(node_id, fused_score)` sorted by score descending. Dedup by
/// content hash is `06-context-protocol.md` §2.3 step 4.
fn dedup_by_content_hash(
    fused: &HashMap<i64, f64>,
    node_by_id: &HashMap<i64, &NodeRow>,
) -> Vec<(i64, f64)> {
    // content_hash -> (best node_id, best score)
    let mut best: HashMap<&str, (i64, f64)> = HashMap::new();
    for (&id, &score) in fused {
        let Some(node) = node_by_id.get(&id) else {
            continue;
        };
        let entry = best
            .entry(node.content_hash.as_str())
            .or_insert((id, f64::MIN));
        if score > entry.1 {
            *entry = (id, score);
        }
    }
    let mut out: Vec<(i64, f64)> = best.into_values().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

/// One candidate for the MMR pass.
struct MmrItem {
    relevance: f32,
    vector: Option<Vec<f32>>,
}

/// Maximal-marginal-relevance selection. Greedily picks the item maximizing
/// `λ·relevance − (1−λ)·max_similarity_to_already_selected`. Items without a
/// vector are treated as maximally diverse (similarity 0), so graph/recency-
/// only hits are never penalized for lacking an embedding. Returns indices in
/// selection order. O(n²), fine for CLI-local candidate counts.
fn mmr_select(items: &[MmrItem], lambda: f32) -> Vec<usize> {
    let n = items.len();
    let mut selected: Vec<usize> = Vec::with_capacity(n);
    let mut remaining: Vec<usize> = (0..n).collect();
    while !remaining.is_empty() {
        let mut best_pos = 0usize;
        let mut best_score = f32::MIN;
        for (pos, &idx) in remaining.iter().enumerate() {
            let diversity_penalty = selected
                .iter()
                .filter_map(|&s| match (&items[idx].vector, &items[s].vector) {
                    (Some(a), Some(b)) => Some(cosine(a, b)),
                    _ => None,
                })
                .fold(0.0f32, f32::max);
            let mmr = lambda * items[idx].relevance - (1.0 - lambda) * diversity_penalty;
            if mmr > best_score {
                best_score = mmr;
                best_pos = pos;
            }
        }
        selected.push(remaining.remove(best_pos));
    }
    selected
}

/// Pack frames (already in priority order) into the token and count budgets,
/// returning `(kept, dropped)`. **Invariants (property-tested):** kept token
/// sum ≤ `max_tokens`, `kept.len()` ≤ `max_frames`, and `kept + dropped` is a
/// partition of the input (nothing vanishes silently — `L-C5`). A frame that
/// individually exceeds the remaining budget is dropped, but packing continues
/// so a smaller later frame can still fit.
pub(crate) fn pack_to_budget(
    frames: Vec<ContextFrame>,
    max_tokens: u32,
    max_frames: u32,
) -> (Vec<ContextFrame>, Vec<DroppedFrame>) {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let mut spent: u64 = 0;
    for frame in frames {
        if kept.len() as u32 >= max_frames {
            dropped.push(dropped_from(&frame, DropReason::FrameCount));
            continue;
        }
        if spent + frame.token_cost as u64 > max_tokens as u64 {
            dropped.push(dropped_from(&frame, DropReason::TokenBudget));
            continue;
        }
        spent += frame.token_cost as u64;
        kept.push(frame);
    }
    (kept, dropped)
}

fn dropped_from(frame: &ContextFrame, reason: DropReason) -> DroppedFrame {
    DroppedFrame {
        id: frame.id.clone(),
        title: frame.title.clone(),
        token_cost: frame.token_cost,
        reason,
    }
}

/// Lowercased query terms (length > 2) for lexical fallback.
fn query_terms(q: &ContextQuery) -> Vec<String> {
    let text = q.query_text.clone().unwrap_or_else(|| q.goal.clone());
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2)
        .map(|t| t.to_string())
        .collect()
}

/// Bounded substring/term search over stored content — the honest fallback
/// when graph/vector coverage is weak (`L-C6`). Score is the fraction of query
/// terms found in the node's content or label.
fn lexical_search(nodes: &[NodeRow], terms: &[String], limit: usize) -> Vec<(i64, f32)> {
    if terms.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(i64, f32)> = Vec::new();
    for node in nodes {
        let haystack = format!("{} {}", node.display_name, node.content).to_lowercase();
        let hits = terms.iter().filter(|t| haystack.contains(*t)).count();
        if hits > 0 {
            scored.push((node.id, hits as f32 / terms.len() as f32));
        }
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocp_types::FrameKind;
    use proptest::prelude::*;

    fn frame(id: &str, token_cost: u32) -> ContextFrame {
        ContextFrame {
            id: id.into(),
            kind: FrameKind::Snippet,
            title: id.into(),
            content: String::new(),
            uri: None,
            score: 0.5,
            token_cost,
            valid_from: None,
            valid_to: None,
            recorded_at: None,
            provenance: vec![],
            citation_label: Some(id.into()),
            embedding: None,
            relations: vec![],
        }
    }

    #[test]
    fn cosine_is_one_for_identical_and_zero_for_orthogonal() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn rrf_rewards_appearing_high_in_multiple_lists() {
        let fused = rrf_fuse(&[vec![1, 2, 3], vec![1, 3, 2]], 60.0);
        // id 1 is rank-0 in both lists → strictly highest.
        assert!(fused[&1] > fused[&2]);
        assert!(fused[&1] > fused[&3]);
    }

    #[test]
    fn packing_respects_frame_count() {
        let frames = vec![frame("a", 1), frame("b", 1), frame("c", 1)];
        let (kept, dropped) = pack_to_budget(frames, 1000, 2);
        assert_eq!(kept.len(), 2);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].reason, DropReason::FrameCount);
    }

    #[test]
    fn packing_skips_an_oversized_frame_but_fits_a_later_small_one() {
        let frames = vec![frame("big", 500), frame("small", 10)];
        let (kept, dropped) = pack_to_budget(frames, 100, 10);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "small");
        assert_eq!(dropped[0].id, "big");
        assert_eq!(dropped[0].reason, DropReason::TokenBudget);
    }

    #[test]
    fn missing_citation_is_a_constructor_error() {
        let node = NodeRow {
            id: 1,
            public_id: "nod_x".into(),
            kind: crate::store::NodeKind::Concept,
            display_name: "   ".into(), // whitespace-only → no human label
            content: "body".into(),
            content_hash: "h".into(),
            uri: None,
            recorded_at: "2026-01-01T00:00:00Z".into(),
        };
        let err = frame_from_node(&node, 0.5, "fp", false, &[]).unwrap_err();
        assert!(matches!(err, ContextError::MissingCitation { .. }));
    }

    #[test]
    fn lexical_frames_are_labeled_in_provenance() {
        let node = NodeRow {
            id: 1,
            public_id: "nod_x".into(),
            kind: crate::store::NodeKind::Concept,
            display_name: "a concept".into(),
            content: "body".into(),
            content_hash: "h".into(),
            uri: None,
            recorded_at: "2026-01-01T00:00:00Z".into(),
        };
        let frame = frame_from_node(&node, 0.5, "fp", true, &["billing".to_string()]).unwrap();
        assert!(
            is_lexical_fallback(&frame),
            "fallback frames must be labeled"
        );
        let graph_frame = frame_from_node(&node, 0.5, "fp", false, &[]).unwrap();
        assert!(!is_lexical_fallback(&graph_frame));
    }

    proptest! {
        /// The core budgeting guarantee (`L-C5`): the packer never exceeds
        /// either budget, and no frame is silently lost — kept ⊎ dropped == in.
        #[test]
        fn packing_never_exceeds_budget_and_loses_nothing(
            costs in prop::collection::vec(0u32..300, 0..40),
            max_tokens in 0u32..500,
            max_frames in 0u32..20,
        ) {
            let n = costs.len();
            let frames: Vec<ContextFrame> =
                costs.iter().enumerate().map(|(i, c)| frame(&format!("f{i}"), *c)).collect();
            let (kept, dropped) = pack_to_budget(frames, max_tokens, max_frames);
            let kept_tokens: u64 = kept.iter().map(|f| f.token_cost as u64).sum();
            prop_assert!(kept_tokens <= max_tokens as u64);
            prop_assert!(kept.len() as u32 <= max_frames);
            prop_assert_eq!(kept.len() + dropped.len(), n);
        }
    }

    // --- End-to-end recall over a real store (public-API integration) -------

    use crate::clock::FixedClock;
    use crate::embed::HashEmbedder;
    use crate::store::{ContextStore, NodeInput, NodeKind};
    use crate::writeback::ContextDelta;
    use ocp_types::ContextQuery;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn base_query(goal: &str, query_text: &str) -> ContextQuery {
        ContextQuery {
            goal: goal.into(),
            query_text: Some(query_text.into()),
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames: 10,
            max_tokens: 4000,
            as_of: None,
        }
    }

    async fn seeded() -> (TempDir, ContextStore) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        let store = ContextStore::open_with(
            &path,
            Arc::new(HashEmbedder::default()),
            FixedClock::shared(1_000),
        )
        .unwrap();
        store
            .upsert(
                ContextDelta::new()
                    .with_node(
                        NodeInput::new(NodeKind::File, "src/store.rs")
                            .with_uri("file:///repo/src/store.rs")
                            .with_content(
                                "open the sqlite connection in wal mode with foreign keys on",
                            ),
                    )
                    .with_node(
                        NodeInput::new(NodeKind::Artifact, "notes").with_content(
                            "render a bar chart of quarterly revenue in the dashboard",
                        ),
                    )
                    .with_node(
                        NodeInput::new(NodeKind::Concept, "budgeting").with_content(
                            "pack context frames to the token budget and report drops",
                        ),
                    ),
            )
            .await
            .unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn recall_returns_cited_budget_respecting_frames() {
        let (_dir, store) = seeded().await;
        let q = base_query(
            "open the database",
            "open the sqlite connection in wal mode with foreign keys on",
        );
        let result = store.recall(&q).await.unwrap();
        assert!(!result.frames.is_empty(), "recall found grounding");
        assert!(
            result.assembled_tokens() <= q.max_tokens as u64,
            "packing respects the budget"
        );
        // The strongly-matching node is retrieved (coverage should be high).
        assert!(
            result.coverage >= MIN_COVERAGE,
            "coverage {} too low",
            result.coverage
        );
        assert!(
            !result.used_lexical_fallback,
            "strong coverage → real grounding, not fallback"
        );
        assert!(result.frames.iter().any(|f| f.content.contains("sqlite")));
        // Every frame is humanly citable (`L-C4`).
        assert!(result.frames.iter().all(|f| {
            f.citation_label
                .as_deref()
                .map(|l| !l.is_empty())
                .unwrap_or(false)
        }));
    }

    #[tokio::test]
    async fn recall_reports_dropped_frames_under_a_tight_frame_budget() {
        let (_dir, store) = seeded().await;
        let mut q = base_query(
            "open the database",
            "open the sqlite connection in wal mode",
        );
        q.max_frames = 1;
        let result = store.recall(&q).await.unwrap();
        assert_eq!(result.frames.len(), 1, "only one frame fits");
        assert!(
            !result.dropped.is_empty(),
            "the rest are reported dropped, never silent (L-C5)"
        );
        assert!(
            result
                .dropped
                .iter()
                .all(|d| d.reason == DropReason::FrameCount)
        );
    }

    #[tokio::test]
    async fn recall_falls_back_to_labeled_lexical_when_no_vectors_under_fingerprint() {
        // Seed vectors under fingerprint rev "1", then recall through a store
        // whose active embedder is rev "2": its vector index is empty for this
        // content, so coverage is 0 and retrieval honestly falls back to
        // lexical search — and labels those frames (`L-C6`). This also proves
        // retrieval never mixes fingerprints (`L-C2`).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        {
            let store_a = ContextStore::open_with(
                &path,
                Arc::new(HashEmbedder::with_revision("1")),
                FixedClock::shared(1_000),
            )
            .unwrap();
            store_a
                .upsert(
                    ContextDelta::new().with_node(
                        NodeInput::new(NodeKind::Concept, "flux capacitor note")
                            .with_content("the flux capacitor requires exactly gigawatts"),
                    ),
                )
                .await
                .unwrap();
        }
        let store_b = ContextStore::open_with(
            &path,
            Arc::new(HashEmbedder::with_revision("2")),
            FixedClock::shared(2_000),
        )
        .unwrap();
        let q = base_query("capacitor question", "flux capacitor");
        let result = store_b.recall(&q).await.unwrap();
        assert_eq!(result.coverage, 0.0, "no rev-2 vectors → zero coverage");
        assert!(result.used_lexical_fallback);
        assert!(
            !result.frames.is_empty(),
            "lexical search found the node by term"
        );
        assert!(
            result.frames.iter().all(is_lexical_fallback),
            "every fallback frame is labeled, never dressed up as grounding (L-C6)"
        );
    }
}
