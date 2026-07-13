//! Write-back: memory flowing the other way (`06-context-protocol.md` §3.6).
//! [`ContextStore::upsert`] persists episode summaries, indexed content nodes,
//! and fact assertions with **bi-temporal supersession** — a correction closes
//! the prior belief's intervals and links the new edge with `SUPERSEDES`, so
//! "what did we believe at T1" still answers after a T2 correction (`L-C3`).
//! The whole delta is one transaction (`L-L1` crash consistency), and
//! byte-identical content under the active fingerprint is never re-embedded
//! (`L-C2`).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::ContextError;
use crate::store::{
    ContextStore, NodeInput, NodeKind, close_edge, currently_valid_edge, edges_as_of,
    embedding_exists, insert_edge, insert_episode, insert_memory, node_by_id, sha256_hex,
    store_embedding, tag_edge_domains, tag_node_domains, to_hex, upsert_domain, upsert_node,
};

/// How an episode turned out. Stored as its `as_str` form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    Success,
    Failure,
    Partial,
    Aborted,
}

impl EpisodeOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            EpisodeOutcome::Success => "success",
            EpisodeOutcome::Failure => "failure",
            EpisodeOutcome::Partial => "partial",
            EpisodeOutcome::Aborted => "aborted",
        }
    }
}

/// An episodic-memory write: a one-turn/one-session summary with the files it
/// touched and how it ended. It becomes both an `episode` row and a retrievable
/// `Episode` node (so recall can surface prior turns).
#[derive(Debug, Clone)]
pub struct EpisodeInput {
    pub summary: String,
    pub files_touched: Vec<String>,
    pub outcome: EpisodeOutcome,
    pub started_at: String,
    pub ended_at: String,
    pub salience: f32,
    /// Workspace domain tags carried onto the episode's mirror node.
    pub domains: Vec<String>,
}

impl EpisodeInput {
    /// A minimal successful episode with just a summary.
    pub fn new(
        summary: impl Into<String>,
        started_at: impl Into<String>,
        ended_at: impl Into<String>,
    ) -> Self {
        Self {
            summary: summary.into(),
            files_touched: Vec::new(),
            outcome: EpisodeOutcome::Success,
            started_at: started_at.into(),
            ended_at: ended_at.into(),
            salience: 0.0,
            domains: Vec::new(),
        }
    }

    /// Tag with one or more workspace domains.
    pub fn with_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.domains = domains.into_iter().map(Into::into).collect();
        self
    }

    /// Stable identity: the summary plus its time window.
    fn public_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.summary.as_bytes());
        h.update([0u8]);
        h.update(self.started_at.as_bytes());
        h.update([0u8]);
        h.update(self.ended_at.as_bytes());
        format!("epi_{}", &to_hex(&h.finalize())[..24])
    }

    /// The retrievable Episode node mirroring this episode (carries its domains).
    fn as_node(&self) -> NodeInput {
        let label = truncate_label(&self.summary);
        NodeInput::new(NodeKind::Episode, label)
            .with_uri(format!("episode://{}", self.public_id()))
            .with_content(self.summary.clone())
            .with_domains(self.domains.clone())
    }
}

/// A fact assertion: `subject —predicate→ object`. Single-valued by default:
/// asserting a new object for the same `(subject, predicate)` supersedes the
/// prior belief. Set `multivalued` for facts that legitimately have several
/// concurrent objects.
#[derive(Debug, Clone)]
pub struct FactAssertion {
    pub subject: NodeInput,
    pub predicate: String,
    pub object: NodeInput,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub weight: f64,
    pub properties: serde_json::Value,
    pub multivalued: bool,
    /// Domain tags applied to the resulting fact edge.
    pub domains: Vec<String>,
}

impl FactAssertion {
    /// A single-valued fact with default weight and no explicit validity.
    pub fn new(subject: NodeInput, predicate: impl Into<String>, object: NodeInput) -> Self {
        Self {
            subject,
            predicate: predicate.into(),
            object,
            valid_from: None,
            valid_to: None,
            weight: 1.0,
            properties: serde_json::json!({}),
            multivalued: false,
            domains: Vec::new(),
        }
    }

    /// Tag the fact edge with one or more workspace domains.
    pub fn with_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.domains = domains.into_iter().map(Into::into).collect();
        self
    }
}

/// The kind of a memory record. Reflections are the post-turn self-improvement
/// lessons the CLI/pipeline writes after every chat turn (generation is
/// stella-pipeline/CLI scope; storage + recall are this crate's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// A post-turn self-improvement lesson.
    Reflection,
    /// A durable note the agent chose to remember.
    Note,
    /// An extracted insight/preference.
    Insight,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Reflection => "reflection",
            MemoryKind::Note => "note",
            MemoryKind::Insight => "insight",
        }
    }
}

/// A memory to write — content, kind, domains, salience. It becomes a `memory`
/// record and a retrievable `Memory` node, so future turns recall it by
/// similarity + domain overlap + recency.
#[derive(Debug, Clone)]
pub struct MemoryInput {
    pub kind: MemoryKind,
    pub content: String,
    pub domains: Vec<String>,
    pub salience: f32,
}

impl MemoryInput {
    /// A reflection memory tagged with the given domains.
    pub fn reflection<I, S>(content: impl Into<String>, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            kind: MemoryKind::Reflection,
            content: content.into(),
            domains: domains.into_iter().map(Into::into).collect(),
            salience: 0.0,
        }
    }

    /// A memory of an explicit kind.
    pub fn new(kind: MemoryKind, content: impl Into<String>) -> Self {
        Self {
            kind,
            content: content.into(),
            domains: Vec::new(),
            salience: 0.0,
        }
    }

    /// Tag with one or more workspace domains.
    pub fn with_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.domains = domains.into_iter().map(Into::into).collect();
        self
    }

    /// Stable identity: kind + content.
    fn public_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.kind.as_str().as_bytes());
        h.update([0u8]);
        h.update(self.content.as_bytes());
        format!("mem_{}", &to_hex(&h.finalize())[..24])
    }

    /// The retrievable Memory node mirroring this memory (carries its domains).
    fn as_node(&self) -> NodeInput {
        let label = truncate_label(&self.content);
        NodeInput::new(NodeKind::Memory, label)
            .with_uri(format!("memory://{}", self.public_id()))
            .with_content(self.content.clone())
            .with_domains(self.domains.clone())
    }
}

/// An explicit domain definition (name + optional description). Writing bare
/// domain names on nodes/edges auto-creates them; this is how a caller attaches
/// a description (e.g. the `stella init` taxonomy).
#[derive(Debug, Clone)]
pub struct DomainInput {
    pub name: String,
    pub description: Option<String>,
}

impl DomainInput {
    pub fn new(name: impl Into<String>, description: Option<String>) -> Self {
        Self {
            name: name.into(),
            description,
        }
    }
}

/// A batch of context writes applied atomically.
#[derive(Debug, Clone, Default)]
pub struct ContextDelta {
    /// Explicit domain definitions (names + descriptions). Bare domain names on
    /// other records auto-create domains, so this is only needed to attach a
    /// description.
    pub domains: Vec<DomainInput>,
    /// Bare content nodes to index (docs, snippets, symbols).
    pub nodes: Vec<NodeInput>,
    /// Episodic-memory summaries.
    pub episodes: Vec<EpisodeInput>,
    /// Reflection/other memories.
    pub memories: Vec<MemoryInput>,
    /// Fact assertions with bi-temporal supersession.
    pub facts: Vec<FactAssertion>,
}

impl ContextDelta {
    /// An empty delta to build up fluently.
    pub fn new() -> Self {
        Self::default()
    }

    /// Define a domain (name + optional description).
    pub fn with_domain(mut self, domain: DomainInput) -> Self {
        self.domains.push(domain);
        self
    }

    /// Add a content node.
    pub fn with_node(mut self, node: NodeInput) -> Self {
        self.nodes.push(node);
        self
    }

    /// Add an episode.
    pub fn with_episode(mut self, ep: EpisodeInput) -> Self {
        self.episodes.push(ep);
        self
    }

    /// Add a memory (reflection etc.).
    pub fn with_memory(mut self, memory: MemoryInput) -> Self {
        self.memories.push(memory);
        self
    }

    /// Add a fact assertion.
    pub fn with_fact(mut self, fact: FactAssertion) -> Self {
        self.facts.push(fact);
        self
    }
}

/// What a write-back did — a typed, inspectable receipt (`02-architecture.md`
/// §1: typed outputs). `embeddings_reused` is the byte-compat skip count that
/// makes re-indexing cheap (`L-C2`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpsertReceipt {
    pub nodes_upserted: usize,
    pub episodes_written: usize,
    pub memories_written: usize,
    pub facts_asserted: usize,
    pub facts_superseded: usize,
    pub embeddings_computed: usize,
    pub embeddings_reused: usize,
    /// New domain-tag associations written across all records this batch.
    pub domain_tags_added: usize,
}

/// A fact resolved to human labels for point-in-time queries (`L-C4`: cite by
/// label, never a bare id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactView {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub recorded_at: String,
    pub superseded_at: Option<String>,
}

/// Truncate a summary into a node label without splitting a UTF-8 char.
fn truncate_label(s: &str) -> String {
    const MAX: usize = 80;
    let trimmed = s.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    let truncated: String = trimmed.chars().take(MAX - 1).collect();
    format!("{truncated}…")
}

impl ContextStore {
    /// Apply a delta atomically, returning a receipt. Embedding decisions
    /// happen before the transaction (async), then all node/episode/fact/vector
    /// writes commit together (`L-L1`).
    pub async fn upsert(&self, delta: ContextDelta) -> Result<UpsertReceipt, ContextError> {
        let now = self.clock().now_rfc3339();
        let fingerprint = self.fingerprint().id();

        // --- Phase A: decide what to embed (async, no lock held) ------------
        // Gather every distinct piece of embeddable content in this delta.
        let mut contents: Vec<(String, String)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let push =
            |content: &str, contents: &mut Vec<(String, String)>, seen: &mut HashSet<String>| {
                if content.is_empty() {
                    return;
                }
                let hash = sha256_hex(content);
                if seen.insert(hash.clone()) {
                    contents.push((hash, content.to_string()));
                }
            };
        for node in &delta.nodes {
            push(&node.content, &mut contents, &mut seen);
        }
        for ep in &delta.episodes {
            push(&ep.summary, &mut contents, &mut seen);
        }
        for memory in &delta.memories {
            push(&memory.content, &mut contents, &mut seen);
        }
        for fact in &delta.facts {
            push(&fact.subject.content, &mut contents, &mut seen);
            push(&fact.object.content, &mut contents, &mut seen);
        }

        // Partition into already-embedded (reused) vs missing (`L-C2`).
        let (missing, reused): (Vec<_>, Vec<_>) = {
            let conn = self.conn();
            let mut missing = Vec::new();
            let mut reused = Vec::new();
            for (hash, content) in contents {
                if embedding_exists(&conn, &hash, &fingerprint)? {
                    reused.push(hash);
                } else {
                    missing.push((hash, content));
                }
            }
            (missing, reused)
        };

        // Embed only the missing content. An empty batch would error, so guard.
        let mut new_vectors: Vec<(String, Vec<f32>)> = Vec::with_capacity(missing.len());
        if !missing.is_empty() {
            let texts: Vec<String> = missing.iter().map(|(_, c)| c.clone()).collect();
            let embeddings = self.embedder().embed(&texts).await?;
            for ((hash, _), emb) in missing.iter().zip(embeddings) {
                new_vectors.push((hash.clone(), emb.vector));
            }
        }

        // --- Phase B: one transaction for all writes (`L-L1`) ---------------
        let mut receipt = UpsertReceipt {
            embeddings_computed: new_vectors.len(),
            embeddings_reused: reused.len(),
            ..Default::default()
        };
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;

        // Explicit domain definitions first, so descriptions land even if the
        // same names are also referenced as bare tags below.
        for domain in &delta.domains {
            upsert_domain(&tx, &domain.name, domain.description.as_deref(), &now)?;
        }

        for node in &delta.nodes {
            let id = upsert_node(&tx, node, &now)?;
            receipt.domain_tags_added += tag_node_domains(&tx, id, &node.domains, &now)?;
            receipt.nodes_upserted += 1;
        }

        for ep in &delta.episodes {
            let files = serde_json::json!(ep.files_touched);
            insert_episode(
                &tx,
                &ep.public_id(),
                &ep.summary,
                &files,
                ep.outcome.as_str(),
                ep.salience as f64,
                &ep.started_at,
                &ep.ended_at,
                &now,
            )?;
            let node = ep.as_node();
            let id = upsert_node(&tx, &node, &now)?;
            receipt.domain_tags_added += tag_node_domains(&tx, id, &node.domains, &now)?;
            receipt.nodes_upserted += 1;
            receipt.episodes_written += 1;
        }

        for memory in &delta.memories {
            insert_memory(
                &tx,
                &memory.public_id(),
                memory.kind.as_str(),
                &memory.content,
                memory.salience as f64,
                &now,
            )?;
            let node = memory.as_node();
            let id = upsert_node(&tx, &node, &now)?;
            receipt.domain_tags_added += tag_node_domains(&tx, id, &node.domains, &now)?;
            receipt.nodes_upserted += 1;
            receipt.memories_written += 1;
        }

        for fact in &delta.facts {
            let src = upsert_node(&tx, &fact.subject, &now)?;
            let dst = upsert_node(&tx, &fact.object, &now)?;
            receipt.domain_tags_added += tag_node_domains(&tx, src, &fact.subject.domains, &now)?;
            receipt.domain_tags_added += tag_node_domains(&tx, dst, &fact.object.domains, &now)?;
            receipt.nodes_upserted += 2;
            apply_fact(&tx, fact, src, dst, &now, &mut receipt)?;
        }

        for (hash, vector) in &new_vectors {
            store_embedding(&tx, hash, &fingerprint, vector, &now)?;
        }

        tx.commit()?;
        Ok(receipt)
    }

    /// Fact edges as believed at a transaction-time instant, resolved to human
    /// labels. `as_of = None` returns currently-believed facts; `Some(t)`
    /// reconstructs the belief set at `t` (`L-C3` audit query).
    pub fn facts_as_of(&self, as_of: Option<&str>) -> Result<Vec<FactView>, ContextError> {
        let conn = self.conn();
        let edges = edges_as_of(&conn, as_of)?;
        let mut out = Vec::with_capacity(edges.len());
        for edge in edges {
            let subject = node_by_id(&conn, edge.src_id)?
                .map(|n| n.display_name)
                .unwrap_or_else(|| "<unknown>".to_string());
            let object = node_by_id(&conn, edge.dst_id)?
                .map(|n| n.display_name)
                .unwrap_or_else(|| "<unknown>".to_string());
            out.push(FactView {
                subject,
                predicate: edge.rel,
                object,
                recorded_at: edge.recorded_at,
                superseded_at: edge.superseded_at,
            });
        }
        Ok(out)
    }
}

/// Insert a fact edge, superseding a prior single-valued belief if the object
/// changed. Idempotent when the same `(subject, predicate, object)` is
/// re-asserted.
fn apply_fact(
    tx: &rusqlite::Connection,
    fact: &FactAssertion,
    src: i64,
    dst: i64,
    now: &str,
    receipt: &mut UpsertReceipt,
) -> Result<(), ContextError> {
    if fact.multivalued {
        // Multi-valued: coexist unless this exact triple is already live.
        if !live_triple_exists(tx, src, &fact.predicate, dst)? {
            let edge_id = insert_edge(
                tx,
                &fact.predicate,
                src,
                dst,
                fact.weight,
                &fact.properties,
                fact.valid_from.as_deref(),
                fact.valid_to.as_deref(),
                now,
                None,
            )?;
            receipt.domain_tags_added += tag_edge_domains(tx, edge_id, &fact.domains, now)?;
            receipt.facts_asserted += 1;
        }
        return Ok(());
    }

    // Single-valued: find the current belief for (subject, predicate).
    match currently_valid_edge(tx, src, &fact.predicate)? {
        Some((_, existing_dst)) if existing_dst == dst => {
            // Same object → idempotent no-op; the belief already holds.
        }
        Some((existing_edge, _)) => {
            // Object changed → close the old interval, link SUPERSEDES.
            let valid_to = fact.valid_from.as_deref().unwrap_or(now);
            close_edge(tx, existing_edge, now, valid_to)?;
            let edge_id = insert_edge(
                tx,
                &fact.predicate,
                src,
                dst,
                fact.weight,
                &fact.properties,
                fact.valid_from.as_deref(),
                fact.valid_to.as_deref(),
                now,
                Some(existing_edge),
            )?;
            receipt.domain_tags_added += tag_edge_domains(tx, edge_id, &fact.domains, now)?;
            receipt.facts_superseded += 1;
            receipt.facts_asserted += 1;
        }
        None => {
            let edge_id = insert_edge(
                tx,
                &fact.predicate,
                src,
                dst,
                fact.weight,
                &fact.properties,
                fact.valid_from.as_deref(),
                fact.valid_to.as_deref(),
                now,
                None,
            )?;
            receipt.domain_tags_added += tag_edge_domains(tx, edge_id, &fact.domains, now)?;
            receipt.facts_asserted += 1;
        }
    }
    Ok(())
}

/// Whether an exact live `(src, rel, dst)` edge already exists.
fn live_triple_exists(
    conn: &rusqlite::Connection,
    src: i64,
    rel: &str,
    dst: i64,
) -> Result<bool, ContextError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edge
         WHERE src_id = ?1 AND rel = ?2 AND dst_id = ?3 AND superseded_at IS NULL",
        rusqlite::params![src, rel, dst],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::embed::HashEmbedder;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn store_at(clock: Arc<FixedClock>) -> (TempDir, ContextStore) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        let store =
            ContextStore::open_with(&path, Arc::new(HashEmbedder::default()), clock).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn supersession_preserves_point_in_time_belief() {
        // L-C3: correct a fact at T2; querying belief-time T1 still answers
        // with the pre-correction value; history is never destroyed.
        let clock = FixedClock::shared(1_000);
        let (_dir, store) = store_at(clock.clone());

        // T1: the build system is make.
        let fact = FactAssertion::new(
            NodeInput::new(NodeKind::Concept, "build system"),
            "IS",
            NodeInput::new(NodeKind::Concept, "make"),
        );
        let r1 = store
            .upsert(ContextDelta::new().with_fact(fact))
            .await
            .unwrap();
        assert_eq!(r1.facts_asserted, 1);
        assert_eq!(r1.facts_superseded, 0);
        let t1 = store.clock().now_rfc3339();

        // T2: we learn it's actually bazel — supersede.
        clock.advance(1_000);
        let fact2 = FactAssertion::new(
            NodeInput::new(NodeKind::Concept, "build system"),
            "IS",
            NodeInput::new(NodeKind::Concept, "bazel"),
        );
        let r2 = store
            .upsert(ContextDelta::new().with_fact(fact2))
            .await
            .unwrap();
        assert_eq!(r2.facts_superseded, 1, "the make belief was superseded");
        assert_eq!(r2.facts_asserted, 1);

        // Now (currently believed): bazel.
        let now_beliefs = store.facts_as_of(None).unwrap();
        assert_eq!(now_beliefs.len(), 1);
        assert_eq!(now_beliefs[0].object, "bazel");

        // As believed at T1: make. History survived the correction.
        let then = store.facts_as_of(Some(&t1)).unwrap();
        assert_eq!(then.len(), 1, "exactly one belief held at T1");
        assert_eq!(then[0].object, "make");
        assert_eq!(then[0].subject, "build system");
    }

    #[tokio::test]
    async fn reasserting_the_same_fact_is_idempotent() {
        let clock = FixedClock::shared(1_000);
        let (_dir, store) = store_at(clock);
        let make_fact = || {
            FactAssertion::new(
                NodeInput::new(NodeKind::Concept, "lang"),
                "IS",
                NodeInput::new(NodeKind::Concept, "rust"),
            )
        };
        store
            .upsert(ContextDelta::new().with_fact(make_fact()))
            .await
            .unwrap();
        let r2 = store
            .upsert(ContextDelta::new().with_fact(make_fact()))
            .await
            .unwrap();
        assert_eq!(r2.facts_asserted, 0, "same object → no new edge");
        assert_eq!(r2.facts_superseded, 0);
        assert_eq!(store.facts_as_of(None).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn byte_identical_content_is_never_re_embedded() {
        // L-C2: the byte-compat skip. The receipt distinguishes computed from
        // reused; the second upsert of identical content computes nothing.
        let clock = FixedClock::shared(1_000);
        let (_dir, store) = store_at(clock);
        let node = || {
            NodeInput::new(NodeKind::Concept, "doc")
                .with_content("a paragraph of stable indexed content")
        };
        let r1 = store
            .upsert(ContextDelta::new().with_node(node()))
            .await
            .unwrap();
        assert_eq!(r1.embeddings_computed, 1);
        assert_eq!(r1.embeddings_reused, 0);
        let r2 = store
            .upsert(ContextDelta::new().with_node(node()))
            .await
            .unwrap();
        assert_eq!(
            r2.embeddings_computed, 0,
            "identical content is reused, not re-embedded"
        );
        assert_eq!(r2.embeddings_reused, 1);
    }

    #[tokio::test]
    async fn episodes_are_written_and_become_retrievable_nodes() {
        let clock = FixedClock::shared(1_000);
        let (_dir, store) = store_at(clock);
        let ep = EpisodeInput::new(
            "fixed the failing budget test by clamping the token estimate",
            "2026-07-01T10:00:00Z",
            "2026-07-01T10:05:00Z",
        );
        let receipt = store
            .upsert(ContextDelta::new().with_episode(ep))
            .await
            .unwrap();
        assert_eq!(receipt.episodes_written, 1);
        assert_eq!(receipt.nodes_upserted, 1, "the episode also indexed a node");
        assert!(store.node_count().unwrap() >= 1);
    }

    #[tokio::test]
    async fn multivalued_facts_coexist() {
        let clock = FixedClock::shared(1_000);
        let (_dir, store) = store_at(clock);
        let mut a = FactAssertion::new(
            NodeInput::new(NodeKind::Concept, "service"),
            "DEPENDS_ON",
            NodeInput::new(NodeKind::Concept, "postgres"),
        );
        a.multivalued = true;
        let mut b = FactAssertion::new(
            NodeInput::new(NodeKind::Concept, "service"),
            "DEPENDS_ON",
            NodeInput::new(NodeKind::Concept, "redis"),
        );
        b.multivalued = true;
        store
            .upsert(ContextDelta::new().with_fact(a).with_fact(b))
            .await
            .unwrap();
        let beliefs = store.facts_as_of(None).unwrap();
        assert_eq!(
            beliefs.len(),
            2,
            "both dependencies are concurrently believed"
        );
    }
}
