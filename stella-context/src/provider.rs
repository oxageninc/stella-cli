//! The provider registry seam (`02-architecture.md` §7, `06-context-protocol.md`
//! §2). One interface — [`ContextProvider`] — behind which every source
//! registers: the built-in [`ContextStore`], the code graph and git-history
//! providers (`stella-graph`), and external OCP providers spoken to over stdio
//! or HTTP (adapted onto this trait by the integration glue; this crate does
//! not depend on `ocp-host`). The store itself implements the trait, so it is
//! both the primary backend and a first-class provider.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use ocp_types::capability::QueryCapability;
use ocp_types::{Capabilities, ContextFrame, ContextQuery, DataFlow, ProviderInfo};

use crate::error::ContextError;
use crate::store::{ContextStore, NodeKind};

/// A source of context frames. Async because external providers are child
/// processes / HTTP endpoints; the built-in store resolves in-process.
#[async_trait]
pub trait ContextProvider: Send + Sync {
    /// Identity and data-flow declaration surfaced at install/consent time
    /// (`06-context-protocol.md` §3.2, §3.5). A host gates `egress` providers
    /// on explicit consent using this.
    fn info(&self) -> ProviderInfo;

    /// What this provider can answer, for query routing.
    fn capabilities(&self) -> Capabilities;

    /// Return frames relevant to the query. Providers return `Vec<ContextFrame>`;
    /// budgeting/fusion across providers is the host's job.
    async fn query(&self, q: &ContextQuery) -> Result<Vec<ContextFrame>, ContextError>;
}

/// Every frame kind the built-in store can serve.
fn store_kinds() -> Vec<String> {
    [
        NodeKind::File,
        NodeKind::Symbol,
        NodeKind::Concept,
        NodeKind::Fact,
        NodeKind::Episode,
        NodeKind::Person,
        NodeKind::Artifact,
        NodeKind::Task,
    ]
    .iter()
    .map(|k| k.to_frame_kind())
    // Serialize the FrameKind to its wire string via serde for one source of
    // truth on the spelling.
    .filter_map(|fk| {
        serde_json::to_value(fk)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
    })
    .collect::<HashSet<_>>()
    .into_iter()
    .collect()
}

#[async_trait]
impl ContextProvider for ContextStore {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "stella-context".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            // The local plane reads workspace content and persists upserts, but
            // nothing ever leaves the machine — no egress consent required.
            data_flow: DataFlow {
                reads: true,
                writes: true,
                egress: false,
            },
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            query: QueryCapability {
                kinds: store_kinds(),
                filters: vec!["anchors".to_string(), "as_of".to_string()],
            },
            upsert: true,
            graph: true,
            embeddings_fingerprint: Some(self.fingerprint().id()),
            subscribe: false,
        }
    }

    async fn query(&self, q: &ContextQuery) -> Result<Vec<ContextFrame>, ContextError> {
        // The OCP-shaped adapter over the rich `recall` pipeline.
        Ok(self.recall(q).await?.frames)
    }
}

/// A set of registered providers. Fans a query out to those whose capabilities
/// match, then concatenates (deduping by frame id). Cross-provider reciprocal-
/// rank fusion is the store's internal pipeline today; multi-provider fusion is
/// the tracked follow-up once external OCP providers register here.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn ContextProvider>>,
}

impl ProviderRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider.
    pub fn register(&mut self, provider: Arc<dyn ContextProvider>) {
        self.providers.push(provider);
    }

    /// Number of registered providers.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether no providers are registered.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// The registered providers (for status/inspection surfaces).
    pub fn providers(&self) -> &[Arc<dyn ContextProvider>] {
        &self.providers
    }

    /// Whether a provider's declared kinds satisfy the query's requested kinds.
    /// An empty `q.kinds` matches every provider (no kind filter).
    fn matches(caps: &Capabilities, q: &ContextQuery) -> bool {
        if q.kinds.is_empty() {
            return true;
        }
        let wanted: HashSet<String> = q
            .kinds
            .iter()
            .filter_map(|k| {
                serde_json::to_value(k)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
            })
            .collect();
        caps.query.kinds.iter().any(|k| wanted.contains(k))
    }

    /// Fan the query to capability-matching providers and concatenate their
    /// frames, deduping by frame id (first writer wins).
    pub async fn query_all(&self, q: &ContextQuery) -> Result<Vec<ContextFrame>, ContextError> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for provider in &self.providers {
            if !Self::matches(&provider.capabilities(), q) {
                continue;
            }
            for frame in provider.query(q).await? {
                if seen.insert(frame.id.clone()) {
                    out.push(frame);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ContextStore, NodeInput};
    use crate::writeback::ContextDelta;
    use ocp_types::FrameKind;
    use tempfile::TempDir;

    async fn seeded_store() -> (TempDir, Arc<ContextStore>) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("context.db");
        let store = Arc::new(ContextStore::open(&path).unwrap());
        store
            .upsert(
                ContextDelta::new().with_node(
                    NodeInput::new(NodeKind::File, "src/main.rs")
                        .with_uri("file:///repo/src/main.rs")
                        .with_content("fn main() { open the sqlite connection in wal mode }"),
                ),
            )
            .await
            .unwrap();
        (dir, store)
    }

    fn query(goal: &str) -> ContextQuery {
        ContextQuery {
            goal: goal.into(),
            query_text: None,
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames: 10,
            max_tokens: 4000,
            as_of: None,
        }
    }

    #[tokio::test]
    async fn store_advertises_local_no_egress_data_flow() {
        let (_dir, store) = seeded_store().await;
        let info = store.info();
        assert_eq!(info.name, "stella-context");
        assert!(info.data_flow.reads);
        assert!(info.data_flow.writes);
        assert!(!info.data_flow.egress, "the local plane never egresses");
    }

    #[tokio::test]
    async fn store_capabilities_report_upsert_graph_and_fingerprint() {
        let (_dir, store) = seeded_store().await;
        let caps = store.capabilities();
        assert!(caps.upsert);
        assert!(caps.graph);
        assert_eq!(
            caps.embeddings_fingerprint.as_deref(),
            Some("hash-ngram@1/256/l2")
        );
        assert!(caps.query.kinds.contains(&"snippet".to_string()));
    }

    #[tokio::test]
    async fn registry_fans_out_and_dedups_by_frame_id() {
        let (_dir, store) = seeded_store().await;
        let mut registry = ProviderRegistry::new();
        // Register the SAME store twice: the dedup must collapse duplicate ids.
        registry.register(store.clone());
        registry.register(store.clone());
        assert_eq!(registry.len(), 2);
        let frames = registry
            .query_all(&query("open the sqlite connection"))
            .await
            .unwrap();
        let ids: HashSet<_> = frames.iter().map(|f| f.id.clone()).collect();
        assert_eq!(
            ids.len(),
            frames.len(),
            "no duplicate frame ids across providers"
        );
    }

    #[tokio::test]
    async fn kind_filter_routes_away_non_matching_providers() {
        let (_dir, store) = seeded_store().await;
        let mut registry = ProviderRegistry::new();
        registry.register(store);
        // The store serves File→snippet frames; asking only for `graph` kind
        // (which the store's node kinds never map to) skips it.
        let mut q = query("anything");
        q.kinds = vec![FrameKind::Graph];
        let frames = registry.query_all(&q).await.unwrap();
        assert!(
            frames.is_empty(),
            "provider is routed away when kinds don't intersect"
        );
    }
}
