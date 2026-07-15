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
//! - **`workspace-memory`** — the bi-temporal store (`stella-context`):
//!   reflections, episodes, facts, fused by its own recall pipeline and
//!   scoped to the workspace's domain taxonomy.
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
use stella_context::ContextStore;

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

/// The workspace memory store behind the OCP provider trait. Domain scoping
/// is provider-internal: OCP's `ContextQuery` is workspace-agnostic, and
/// which taxonomy applies is exactly the kind of local knowledge a provider
/// owns.
struct MemoryProvider {
    store: Arc<ContextStore>,
    domains: Vec<String>,
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
            .store
            .recall_scoped(query, &self.domains)
            .await
            .map_err(|e| HostError::Transport {
                id: "workspace-memory".to_string(),
                message: e.to_string(),
            })?;
        // The store's recall does not honor `query.kinds`, so a kind-filtered
        // query (now routed here because we advertise those kinds) must be
        // filtered before returning — otherwise a `kinds: [Symbol]` request
        // could surface memory/fact frames. Frames removed here count toward
        // the truncation metadata alongside the store's own drops.
        let mut frames = result.frames;
        let mut dropped = result.dropped.len();
        if !query.kinds.is_empty() {
            let before = frames.len();
            frames.retain(|f| query.kinds.contains(&f.kind));
            dropped += before - frames.len();
        }
        Ok(ContextQueryResult {
            truncated: dropped > 0,
            dropped_estimate: u32::try_from(dropped).ok(),
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
        store,
        domains,
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
}
