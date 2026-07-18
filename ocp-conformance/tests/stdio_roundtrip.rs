//! Full stdio round-trip against the real `ocp-example-docs` fixture,
//! exercising `ocp-host`'s `Host` + `StdioProvider` end-to-end (the external
//! child-process path), and the
//! crash-consistency guarantee: a crashing child never poisons a healthy
//! sibling in a fan-out (task deliverable 5).

use async_trait::async_trait;
use ocp_host::{ContextProvider, Host, HostError, ProviderResult};
use ocp_types::capability::QueryCapability;
use ocp_types::{
    Capabilities, ContextFrame, ContextQuery, ContextQueryResult, DataFlow, FrameKind, ProviderInfo,
};

fn fixture() -> String {
    env!("CARGO_BIN_EXE_ocp-example-docs").to_string()
}

fn query(goal: &str) -> ContextQuery {
    ContextQuery {
        goal: goal.into(),
        query_text: Some(goal.into()),
        embedding: None,
        kinds: vec![],
        anchors: vec![],
        max_frames: 8,
        max_tokens: 4096,
        as_of: None,
    }
}

#[tokio::test]
async fn host_queries_a_real_stdio_provider_end_to_end() {
    let mut host = Host::new();
    host.add_stdio("docs", &fixture(), &[])
        .await
        .expect("handshake with the fixture should succeed");

    assert_eq!(host.provider_ids(), vec!["docs"]);
    let caps = host.provider("docs").unwrap().capabilities();
    assert!(caps.query.kinds.contains(&"doc".to_string()));

    let fanout = host.query_all(&query("how do I configure it")).await;
    assert_eq!(fanout.outcomes.len(), 1);
    assert_eq!(fanout.accepted_frames().count(), 2);

    // Every frame is citable by a human label, never a bare id.
    for frame in fanout.accepted_frames() {
        assert!(
            frame
                .citation_label
                .as_deref()
                .map(|label| !label.trim().is_empty())
                .unwrap_or(false),
            "frame {} lacks a citation label",
            frame.id
        );
    }

    let shutdown = host.shutdown().await;
    assert!(
        shutdown.iter().all(|(_, result)| result.is_ok()),
        "clean shutdown expected, got {shutdown:?}"
    );
}

#[tokio::test]
async fn a_crashing_stdio_child_never_poisons_a_healthy_provider() {
    let mut host = Host::new();
    // The fixture acks the handshake, then exits on the first query.
    host.add_stdio(
        "crasher",
        &fixture(),
        &["--misbehave".into(), "crash-on-query".into()],
    )
    .await
    .expect("even a crash-on-query fixture handshakes cleanly");
    host.register(Box::new(HealthyProvider));

    let fanout = host.query_all(&query("anything")).await;
    assert_eq!(fanout.outcomes.len(), 2);

    // The healthy provider's frame is accepted…
    assert_eq!(fanout.accepted_frames().count(), 1);
    // …and the crasher is isolated to its own failed outcome.
    let failures: Vec<_> = fanout.failures().collect();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, "crasher");
    assert!(
        matches!(failures[0].1, HostError::ProviderCrashed { .. }),
        "expected ProviderCrashed, got {:?}",
        failures[0].1
    );

    // Sanity: the healthy provider's outcome really is accepted frames.
    let healthy = fanout
        .outcomes
        .iter()
        .find(|o| o.provider_id == "healthy")
        .expect("healthy provider outcome present");
    assert!(matches!(healthy.result, ProviderResult::Frames(_)));
}

/// A trivial in-process provider that always returns one honest frame.
struct HealthyProvider;

#[async_trait]
impl ContextProvider for HealthyProvider {
    fn id(&self) -> &str {
        "healthy"
    }
    fn info(&self) -> &ProviderInfo {
        static INFO: std::sync::OnceLock<ProviderInfo> = std::sync::OnceLock::new();
        INFO.get_or_init(|| ProviderInfo {
            name: "healthy".into(),
            version: "0.0.1".into(),
            data_flow: DataFlow {
                reads: true,
                writes: false,
                egress: false,
            },
        })
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: std::sync::OnceLock<Capabilities> = std::sync::OnceLock::new();
        CAPS.get_or_init(|| Capabilities {
            query: QueryCapability {
                kinds: vec!["doc".into()],
                filters: vec![],
            },
            ..Capabilities::default()
        })
    }
    async fn query(&self, _query: &ContextQuery) -> Result<ContextQueryResult, HostError> {
        Ok(ContextQueryResult {
            frames: vec![ContextFrame {
                id: "frm_healthy".into(),
                kind: FrameKind::Doc,
                title: "in-process frame".into(),
                content: "still here".into(),
                uri: None,
                score: 0.5,
                token_cost: 8,
                valid_from: None,
                valid_to: None,
                recorded_at: None,
                provenance: vec![],
                citation_label: Some("in-process frame".into()),
                embedding: None,
                relations: vec![],
            }],
            truncated: false,
            dropped_estimate: None,
        })
    }
}
