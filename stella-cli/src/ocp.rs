//! The session's OCP host: the in-tree context sources served through the
//! real `ocp-host` runtime instead of ad-hoc in-process calls.
//!
//! Until now the protocol and its conformance suite shipped, but the
//! shipping CLI's own retrieval bypassed them — the workspace memory store
//! was called directly and the code graph was not consulted at all. This
//! module closes that gap: recall builds one [`ocp_host::Host`], registers
//! two in-process providers, and fans every query out through
//! [`Host::query_all`] — the same consent gate, per-provider timeout,
//! crash isolation, and budget-honesty audit any external OCP provider
//! gets. "Code is a graph, not text" is now the runtime path, not just a
//! wire spec.
//!
//! - **`workspace-memory`** — the context plane: a
//!   [`stella_context::ProviderRegistry`] fan-out with the bi-temporal store
//!   registered domain-scoped (issue #103's wire decision — the store is
//!   queried through the plane's own provider seam, never directly).
//!   Reflections, episodes, facts, fused by the store's recall pipeline.
//! - **`code-graph`** — the tree-sitter index (`stella-graph`), opened
//!   read-only per query (the schema-gate discipline) on the blocking pool.
//!
//! Both are local, `egress: false` sources — the consent store passes them
//! without a prompt; only an egress provider would gate.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ocp_host::{ContextProvider, Host, HostError, ProviderResult};
use ocp_types::{
    Capabilities, ContextFrame, ContextQuery, ContextQueryResult, DataFlow, ProviderInfo,
};
use stella_context::{
    ContextError, ContextProvider as PlaneProvider, ContextStore, ProviderRegistry,
};

/// Per-provider recall timeout. Recall runs before every turn, so a wedged
/// source must cost bounded latency — the host isolates it and the other
/// providers' frames still arrive.
const RECALL_TIMEOUT_MS: u64 = 2_000;

fn local_info(name: &str) -> ProviderInfo {
    ProviderInfo {
        name: name.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_flow: DataFlow {
            reads: true,
            writes: false,
            egress: false,
        },
    }
}

/// The built-in store registered at the context-plane seam, with the
/// session's domain scope applied. Domain scoping is provider-internal:
/// OCP's `ContextQuery` is workspace-agnostic, and which taxonomy applies is
/// exactly the kind of local knowledge a provider owns. Identity and
/// capabilities are the store's own provider declarations.
struct ScopedStore {
    store: Arc<ContextStore>,
    domains: Vec<String>,
}

#[async_trait]
impl PlaneProvider for ScopedStore {
    fn info(&self) -> ProviderInfo {
        PlaneProvider::info(self.store.as_ref())
    }
    fn capabilities(&self) -> Capabilities {
        PlaneProvider::capabilities(self.store.as_ref())
    }
    async fn query(&self, query: &ContextQuery) -> Result<ContextQueryResult, ContextError> {
        Ok(self.store.recall_scoped(query, &self.domains).await?.into())
    }
}

/// The context-plane registry behind `workspace-memory`: the seam every
/// in-plane source registers through (issue #103's wire decision). Today
/// that is the built-in store, domain-scoped; a further plane source (a
/// git-history provider, an adapted external OCP provider) lands by
/// registering here, not by editing the host adapter.
fn memory_plane(store: Arc<ContextStore>, domains: Vec<String>) -> ProviderRegistry {
    let mut plane = ProviderRegistry::new();
    plane.register(Arc::new(ScopedStore { store, domains }));
    plane
}

/// The workspace context plane behind the OCP provider trait: recall fans
/// through the plane's [`ProviderRegistry`] instead of hitting the store
/// directly, so the registry's capability routing, id-dedup, and aggregated
/// truncation report are the production path.
struct MemoryProvider {
    plane: ProviderRegistry,
    info: ProviderInfo,
    caps: Capabilities,
}

#[async_trait]
impl ContextProvider for MemoryProvider {
    fn id(&self) -> &str {
        "workspace-memory"
    }
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    async fn query(&self, query: &ContextQuery) -> Result<ContextQueryResult, HostError> {
        let result = self
            .plane
            .query_all(query)
            .await
            .map_err(|e| HostError::Transport {
                id: "workspace-memory".to_string(),
                message: e.to_string(),
            })?;
        // The registry routes `query.kinds` at provider granularity, but the
        // store's recall does not honor it per-frame, so a kind-filtered
        // query (routed here because we advertise those kinds) must still be
        // filtered before returning — otherwise a `kinds: [Symbol]` request
        // could surface memory/fact frames. This filtering is NOT truncation:
        // `ContextQueryResult.truncated`/`dropped_estimate` describe candidates
        // that matched the request but were cut for budget, so they reflect
        // only the plane's own drops — a non-matching kind was never a
        // candidate for this query in the first place.
        let mut frames = result.frames;
        if !query.kinds.is_empty() {
            frames.retain(|f| query.kinds.contains(&f.kind));
        }
        Ok(ContextQueryResult {
            truncated: result.truncated,
            dropped_estimate: result.dropped_estimate,
            frames,
        })
    }
}

/// The code graph behind the OCP provider trait: open → query → shutdown
/// per call, on the blocking pool (SQLite reads are synchronous I/O, #64).
/// An absent index is an empty answer, not an error — a workspace that
/// never ran `stella init` still recalls memories normally.
struct GraphProvider {
    workspace_root: PathBuf,
    info: ProviderInfo,
    caps: Capabilities,
}

#[async_trait]
impl ContextProvider for GraphProvider {
    fn id(&self) -> &str {
        "code-graph"
    }
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    async fn query(&self, query: &ContextQuery) -> Result<ContextQueryResult, HostError> {
        let db_path = stella_tools::graph::graph_db_path(&self.workspace_root);
        if !db_path.exists() {
            return Ok(ContextQueryResult {
                frames: Vec::new(),
                truncated: false,
                dropped_estimate: None,
            });
        }
        let root = self.workspace_root.clone();
        let query = query.clone();
        let frames = tokio::task::spawn_blocking(move || {
            let graph = stella_graph::CodeGraph::open(&root, &db_path)?;
            let frames = graph.query(&query);
            graph.shutdown();
            frames
        })
        .await
        .map_err(|e| HostError::Transport {
            id: "code-graph".to_string(),
            message: format!("blocking task failed: {e}"),
        })?
        .map_err(|e| HostError::Transport {
            id: "code-graph".to_string(),
            message: e.to_string(),
        })?;
        Ok(ContextQueryResult {
            frames,
            truncated: false,
            dropped_estimate: None,
        })
    }
}

/// The session host: both in-tree providers registered, ready for
/// [`recall_via_host`]. Built once per session by `SessionMemory`.
pub fn session_host(
    store: Arc<ContextStore>,
    domains: Vec<String>,
    workspace_root: PathBuf,
) -> Host {
    let mut host = Host::with_timeout(std::time::Duration::from_millis(RECALL_TIMEOUT_MS));
    // Both providers advertise the frame kinds they serve. Empty `kinds`
    // passes only kind-UNfiltered queries through `capability_matches` — a
    // caller that ever sets `ContextQuery.kinds` would silently route to
    // zero providers if these stayed empty.
    // The wire strings mirror each provider's `to_frame_kind` mapping (the
    // memory store serves every kind it mints; the graph serves symbols,
    // snippets, and graph frames).
    host.register(Box::new(MemoryProvider {
        plane: memory_plane(store, domains),
        info: local_info("workspace-memory"),
        caps: Capabilities {
            query: ocp_types::capability::QueryCapability {
                kinds: ["memory", "episode", "fact", "snippet", "symbol", "doc"]
                    .map(String::from)
                    .to_vec(),
                filters: Vec::new(),
            },
            ..Capabilities::default()
        },
    }));
    host.register(Box::new(GraphProvider {
        workspace_root,
        info: local_info("code-graph"),
        caps: Capabilities {
            graph: true,
            query: ocp_types::capability::QueryCapability {
                kinds: ["symbol", "snippet", "graph"].map(String::from).to_vec(),
                filters: Vec::new(),
            },
            ..Capabilities::default()
        },
    }));
    host
}

/// Fan `query` out through the host and fuse the surviving frames: highest
/// score first, deduped by frame id, re-capped to the query's own frame and
/// token budget (each provider already respected it individually; the merge
/// must too). Failed, timed-out, or budget-lying providers contribute
/// nothing — their isolation is the point of routing through the host.
pub async fn recall_via_host(host: &Host, query: &ContextQuery) -> Vec<ContextFrame> {
    let fanout = host.query_all(query).await;
    let mut frames: Vec<ContextFrame> = Vec::new();
    for outcome in fanout.outcomes {
        if let ProviderResult::Frames(result) = outcome.result {
            frames.extend(result.frames);
        }
    }
    frames.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<ContextFrame> = Vec::new();
    let mut spent_tokens: u32 = 0;
    for frame in frames {
        if kept.len() >= query.max_frames as usize {
            break;
        }
        if spent_tokens.saturating_add(frame.token_cost) > query.max_tokens {
            continue;
        }
        if !seen.insert(frame.id.clone()) {
            continue;
        }
        spent_tokens += frame.token_cost;
        kept.push(frame);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: &str, score: f32, token_cost: u32) -> ContextFrame {
        ContextFrame {
            id: id.to_string(),
            kind: ocp_types::FrameKind::Memory,
            title: id.to_string(),
            content: format!("content of {id}"),
            uri: None,
            score,
            token_cost,
            valid_from: None,
            valid_to: None,
            recorded_at: None,
            provenance: vec![],
            citation_label: Some(format!("[{id}]")),
            embedding: None,
            relations: vec![],
        }
    }

    /// A scripted provider for merge tests.
    struct Scripted {
        id: &'static str,
        frames: Vec<ContextFrame>,
        info: ProviderInfo,
        caps: Capabilities,
    }

    #[async_trait]
    impl ContextProvider for Scripted {
        fn id(&self) -> &str {
            self.id
        }
        fn info(&self) -> &ProviderInfo {
            &self.info
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        async fn query(&self, _q: &ContextQuery) -> Result<ContextQueryResult, HostError> {
            Ok(ContextQueryResult {
                frames: self.frames.clone(),
                truncated: false,
                dropped_estimate: None,
            })
        }
    }

    fn scripted(id: &'static str, frames: Vec<ContextFrame>) -> Box<Scripted> {
        Box::new(Scripted {
            id,
            frames,
            info: local_info(id),
            caps: Capabilities::default(),
        })
    }

    fn query(max_frames: u32, max_tokens: u32) -> ContextQuery {
        ContextQuery {
            goal: "the goal".to_string(),
            query_text: Some("the goal".to_string()),
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames,
            max_tokens,
            as_of: None,
        }
    }

    #[tokio::test]
    async fn merges_providers_by_score_and_dedupes_by_id() {
        let mut host = Host::new();
        host.register(scripted(
            "a",
            vec![frame("low", 0.2, 10), frame("shared", 0.5, 10)],
        ));
        host.register(scripted(
            "b",
            vec![frame("high", 0.9, 10), frame("shared", 0.5, 10)],
        ));
        let kept = recall_via_host(&host, &query(10, 1_000)).await;
        let ids: Vec<&str> = kept.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["high", "shared", "low"], "score-ordered, deduped");
    }

    #[tokio::test]
    async fn merge_respects_the_query_budget_across_providers() {
        let mut host = Host::new();
        host.register(scripted("a", vec![frame("a1", 0.9, 600)]));
        host.register(scripted("b", vec![frame("b1", 0.8, 600)]));
        // Each provider individually fits 1000 tokens; the merged set must
        // not exceed it either.
        let kept = recall_via_host(&host, &query(10, 1_000)).await;
        assert_eq!(kept.len(), 1, "second frame would blow the merged budget");
        assert_eq!(kept[0].id, "a1");
    }

    #[tokio::test]
    async fn an_absent_graph_index_yields_empty_frames_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = GraphProvider {
            workspace_root: dir.path().to_path_buf(),
            info: local_info("code-graph"),
            caps: Capabilities::default(),
        };
        let result = provider.query(&query(5, 500)).await.expect("empty ok");
        assert!(result.frames.is_empty());
    }

    #[tokio::test]
    async fn the_session_host_registers_both_in_tree_providers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ContextStore::open(dir.path().join("context.db")).expect("store");
        let host = session_host(Arc::new(store), vec![], dir.path().to_path_buf());
        let mut ids = host.provider_ids();
        ids.sort();
        assert_eq!(ids, vec!["code-graph", "workspace-memory"]);
    }

    use stella_context::{ContextDelta, NodeInput, NodeKind};

    /// A store with one strongly-matching node, for plane-routing tests.
    async fn seeded_store(dir: &tempfile::TempDir) -> Arc<ContextStore> {
        let store = Arc::new(ContextStore::open(dir.path().join("context.db")).expect("store"));
        store
            .upsert(
                ContextDelta::new().with_node(
                    NodeInput::new(NodeKind::File, "src/store.rs")
                        .with_uri("file:///repo/src/store.rs")
                        .with_content("open the sqlite connection in wal mode"),
                ),
            )
            .await
            .expect("seed");
        store
    }

    #[tokio::test]
    async fn recall_routes_through_the_plane_registry_to_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = seeded_store(&dir).await;
        let host = session_host(store, vec![], dir.path().to_path_buf());
        let mut q = query(5, 4_000);
        q.query_text = Some("open the sqlite connection in wal mode".to_string());
        // Host → workspace-memory → plane registry → store: the full
        // production path, end to end.
        let kept = recall_via_host(&host, &q).await;
        assert!(
            kept.iter().any(|f| f.content.contains("sqlite")),
            "the seeded node surfaces through the registry-routed path"
        );
    }

    /// A scripted context-plane provider (the `stella-context` seam, not the
    /// host trait) for plane fan-out tests.
    struct PlaneScripted {
        kinds: Vec<String>,
        frames: Vec<ContextFrame>,
        truncated: bool,
    }

    #[async_trait]
    impl PlaneProvider for PlaneScripted {
        fn info(&self) -> ProviderInfo {
            local_info("plane-scripted")
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                query: ocp_types::capability::QueryCapability {
                    kinds: self.kinds.clone(),
                    filters: Vec::new(),
                },
                ..Capabilities::default()
            }
        }
        async fn query(&self, _q: &ContextQuery) -> Result<ContextQueryResult, ContextError> {
            Ok(ContextQueryResult {
                frames: self.frames.clone(),
                truncated: self.truncated,
                dropped_estimate: None,
            })
        }
    }

    #[tokio::test]
    async fn the_plane_fans_out_and_kind_routes_across_registered_providers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut plane = memory_plane(seeded_store(&dir).await, vec![]);
        let mut graph_frame = frame("plane-graph", 0.9, 10);
        graph_frame.kind = ocp_types::FrameKind::Graph;
        plane.register(Arc::new(PlaneScripted {
            kinds: vec!["graph".to_string()],
            frames: vec![graph_frame],
            truncated: true,
        }));
        let provider = MemoryProvider {
            plane,
            info: local_info("workspace-memory"),
            caps: Capabilities::default(),
        };

        // Unfiltered: both plane providers answer — the store's frame and the
        // second provider's graph frame merge, and the second provider's
        // truncation survives the fan-out instead of being erased (L-C5).
        let mut q = query(10, 4_000);
        q.query_text = Some("open the sqlite connection in wal mode".to_string());
        let result = provider.query(&q).await.expect("fan-out");
        assert!(result.frames.iter().any(|f| f.id == "plane-graph"));
        assert!(result.frames.iter().any(|f| f.content.contains("sqlite")));
        assert!(result.truncated, "a plane provider's drop report survives");

        // Kind-filtered to `graph`: the registry routes the store away (it
        // never serves graph frames) and only the second provider answers.
        let mut q = query(10, 4_000);
        q.kinds = vec![ocp_types::FrameKind::Graph];
        let result = provider.query(&q).await.expect("kind routing");
        let ids: Vec<&str> = result.frames.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["plane-graph"]);
    }
}
