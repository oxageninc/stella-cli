//! The provider registry seam (
//! §2). One interface — [`ContextProvider`] — behind which context sources
//! register: the built-in [`ContextStore`] implements it (so the store is both
//! the primary backend and a first-class provider), and the shipping CLI's
//! session recall flows through this registry (`stella-cli/src/contextgraph.rs` wraps
//! it as the `workspace-memory` CGP provider, registering the store domain-
//! scoped). Wiring further sources through this seam — a `stella-graph`
//! code-graph provider at this layer, a git-history provider, and external CGP
//! providers adapted from `contextgraph-host` — is designed here but not yet built;
//! this crate does not depend on `contextgraph-host`.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use contextgraph_types::capability::QueryCapability;
use contextgraph_types::{Capabilities, ContextQuery, ContextQueryResult, DataFlow, ProviderInfo};

use crate::error::ContextError;
use crate::store::{ContextStore, NodeKind};

/// A source of context frames. Async because external providers are child
/// processes / HTTP endpoints; the built-in store resolves in-process.
#[async_trait]
pub trait ContextProvider: Send + Sync {
    /// Identity and data-flow declaration surfaced at install/consent time
    /// A host gates `egress` providers
    /// on explicit consent using this.
    fn info(&self) -> ProviderInfo;

    /// What this provider can answer, for query routing.
    fn capabilities(&self) -> Capabilities;

    /// Return frames relevant to the query, as the CGP wire result so a
    /// provider's budget drops stay visible across the seam (`L-C5` — a bare
    /// frame list would erase the truncation report). Budgeting/fusion across
    /// providers is the host's job.
    async fn query(&self, q: &ContextQuery) -> Result<ContextQueryResult, ContextError>;
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
        // The kind the store exists for — omitting it routed kind-filtered
        // memory queries away from the one provider that stores memories.
        NodeKind::Memory,
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

    async fn query(&self, q: &ContextQuery) -> Result<ContextQueryResult, ContextError> {
        // The CGP-shaped adapter over the rich `recall` pipeline.
        Ok(self.recall(q).await?.into())
    }
}

/// A set of registered providers. Fans a query out to those whose capabilities
/// match, then concatenates (deduping by frame id). Cross-provider reciprocal-
/// rank fusion is the store's internal pipeline today; multi-provider fusion is
/// the tracked follow-up once external CGP providers register here.
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
    /// frames, deduping by frame id (first writer wins). The truncation report
    /// aggregates across providers — `truncated` if any provider truncated,
    /// `dropped_estimate` summed over the providers that reported one — so the
    /// fan-out never erases a provider's honest drop report (`L-C5`).
    pub async fn query_all(&self, q: &ContextQuery) -> Result<ContextQueryResult, ContextError> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut frames = Vec::new();
        let mut truncated = false;
        let mut dropped_estimate: Option<u32> = None;
        for provider in &self.providers {
            if !Self::matches(&provider.capabilities(), q) {
                continue;
            }
            let result = provider.query(q).await?;
            truncated |= result.truncated;
            if let Some(d) = result.dropped_estimate {
                dropped_estimate = Some(dropped_estimate.unwrap_or(0).saturating_add(d));
            }
            for frame in result.frames {
                if seen.insert(frame.id.clone()) {
                    frames.push(frame);
                }
            }
        }
        Ok(ContextQueryResult {
            frames,
            truncated,
            dropped_estimate,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ContextStore, NodeInput};
    use crate::writeback::ContextDelta;
    use contextgraph_types::{ContextFrame, FrameKind};
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
        let result = registry
            .query_all(&query("open the sqlite connection"))
            .await
            .unwrap();
        let ids: HashSet<_> = result.frames.iter().map(|f| f.id.clone()).collect();
        assert_eq!(
            ids.len(),
            result.frames.len(),
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
        let result = registry.query_all(&q).await.unwrap();
        assert!(
            result.frames.is_empty(),
            "provider is routed away when kinds don't intersect"
        );
        assert!(!result.truncated, "a routed-away provider reports no drops");
    }

    /// A scripted provider for aggregation tests.
    struct Scripted {
        frames: Vec<ContextFrame>,
        truncated: bool,
        dropped_estimate: Option<u32>,
    }

    #[async_trait]
    impl ContextProvider for Scripted {
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: "scripted".to_string(),
                version: "0".to_string(),
                data_flow: DataFlow {
                    reads: true,
                    writes: false,
                    egress: false,
                },
            }
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                query: QueryCapability {
                    kinds: store_kinds(),
                    filters: vec![],
                },
                ..Capabilities::default()
            }
        }

        async fn query(&self, _q: &ContextQuery) -> Result<ContextQueryResult, ContextError> {
            Ok(ContextQueryResult {
                frames: self.frames.clone(),
                truncated: self.truncated,
                dropped_estimate: self.dropped_estimate,
            })
        }
    }

    #[tokio::test]
    async fn fan_out_aggregates_the_truncation_report_instead_of_erasing_it() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(Scripted {
            frames: vec![],
            truncated: false,
            dropped_estimate: None,
        }));
        registry.register(Arc::new(Scripted {
            frames: vec![],
            truncated: true,
            dropped_estimate: Some(2),
        }));
        registry.register(Arc::new(Scripted {
            frames: vec![],
            truncated: true,
            dropped_estimate: Some(3),
        }));
        let result = registry.query_all(&query("anything")).await.unwrap();
        assert!(result.truncated, "one truncating provider taints the merge");
        assert_eq!(
            result.dropped_estimate,
            Some(5),
            "estimates sum over the providers that reported one (L-C5)"
        );
    }
}
