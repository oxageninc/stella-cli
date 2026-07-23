//! A scripted multi-agent scenario for demos, screenshots, and full-deck render
//! tests. It emits a realistic [`Inbound`] sequence plus a sample
//! [`GraphSnapshot`] so the command deck is fully populated without a live
//! engine — the same models snap onto real fleet data once a supervisor exists.
//!
//! `demo_inbound` is deterministic and order-stable, so it can drive both the
//! timed example playback (`examples/deck_demo.rs`) and a render snapshot test.

use serde_json::json;
use stella_protocol::{
    AgentEvent, ContextFrameRef, FileChangeKind, JudgeEvidence, ModelCallRole, PrStatus,
    ProviderShare, ScopeProposal, StageKind, ToolCall, ToolOutput,
};

use crate::envelope::{AgentMeta, AgentStatus, Inbound};
use crate::graph::{GraphEdge, GraphNode, GraphSnapshot};

/// A sample code-graph neighborhood centered on the engine step driver.
pub fn demo_graph() -> GraphSnapshot {
    let nodes = vec![
        GraphNode {
            label: "run_turn".into(),
            kind: "function".into(),
            location: Some("stella-core/src/driver.rs:239".into()),
        },
        GraphNode {
            label: "Engine".into(),
            kind: "struct".into(),
            location: Some("stella-core/src/engine.rs:48".into()),
        },
        GraphNode {
            label: "Router".into(),
            kind: "struct".into(),
            location: Some("stella-core/src/router.rs:373".into()),
        },
        GraphNode {
            label: "AgentEvent".into(),
            kind: "enum".into(),
            location: Some("stella-protocol/src/event.rs:55".into()),
        },
        GraphNode {
            label: "Ledger".into(),
            kind: "struct".into(),
            location: Some("stella-fleet/src/ledger.rs:90".into()),
        },
        GraphNode {
            label: "driver.rs".into(),
            kind: "file".into(),
            location: Some("stella-core/src/driver.rs".into()),
        },
    ];
    let edges = vec![
        GraphEdge {
            from: 0,
            to: 1,
            kind: "calls".into(),
        },
        GraphEdge {
            from: 1,
            to: 2,
            kind: "uses".into(),
        },
        GraphEdge {
            from: 0,
            to: 3,
            kind: "emits".into(),
        },
        GraphEdge {
            from: 5,
            to: 0,
            kind: "defines".into(),
        },
        GraphEdge {
            from: 1,
            to: 4,
            kind: "writes".into(),
        },
    ];
    GraphSnapshot {
        focus: "run_turn — engine step driver".into(),
        nodes,
        edges,
        // The picker's file list — the demo's source files, so `/` in the
        // Graph tab opens a browsable, filterable list during playback.
        files: vec![
            "stella-core/src/driver.rs".into(),
            "stella-core/src/engine.rs".into(),
            "stella-core/src/router.rs".into(),
            "stella-fleet/src/ledger.rs".into(),
            "stella-protocol/src/event.rs".into(),
        ],
    }
}

fn tool_start(id: &str, name: &str, input: serde_json::Value) -> AgentEvent {
    AgentEvent::ToolStart {
        call: ToolCall {
            call_id: id.into(),
            name: name.into(),
            input,
        },
    }
}

/// The scripted event sequence, in play order. `self_pid` is stamped as every
/// agent's pid so the resource monitor shows real CPU/MEM for a live demo.
pub fn demo_inbound(started_ms: u64, self_pid: u32) -> Vec<Inbound> {
    let lead = "lead";
    let auth = "sub:auth";
    let ci = "sub:ci";

    let ev = |agent: &str, event: AgentEvent| Inbound::Event {
        agent: agent.into(),
        event,
    };
    let reg = |id: &str, title: &str, role: &str| {
        Inbound::Register(
            AgentMeta::new(id, title, started_ms)
                .with_role(role)
                .with_pid(self_pid),
        )
    };

    vec![
        // ── the lead agent boots and plans ──────────────────────────────
        reg(lead, "web-app-2.0 · phase 3 automations", "lead"),
        ev(lead, AgentEvent::Stage { name: StageKind::Triage }),
        ev(lead, AgentEvent::ContextRecall {
            frames: vec![
                ContextFrameRef { id: None, citation_label: "engine step-driver (driver.rs)".into(), provider: "code-graph".into(), source: "code-graph".into(), kind: "symbol".into(), uri: None, method: None, token_cost: 120, block_id: None, content_digest: None },
                ContextFrameRef { id: None, citation_label: "ADR-023 event-log REPL".into(), provider: "workspace-memory".into(), source: "stella-context".into(), kind: "memory".into(), uri: None, method: None, token_cost: 90, block_id: None, content_digest: None },
            ],
            provider_mix: vec![ProviderShare { provider: "code-graph".into(), frames: 1 }, ProviderShare { provider: "memory".into(), frames: 1 }],
            tokens: 210,
        }),
        ev(lead, AgentEvent::Stage { name: StageKind::Plan }),
        ev(lead, AgentEvent::Text { delta: "Planning the automations cluster: list, editor, triggers, workflows.".into() }),
        ev(lead, AgentEvent::StepUsage { output_text: None, step: 1, role: ModelCallRole::Worker, provider: "zai".into(), model: "glm-5.2".into(), input_tokens: 12_400, output_tokens: 640, cached_input_tokens: 9_000, cache_write_tokens: 0, estimated_input_tokens: 12_000, cost_usd: 0.021, duration_ms: 1830, retries: 0, tool_calls: 0, complete: true }),
        ev(lead, AgentEvent::BudgetTick { spent_usd: 0.021, limit_usd: Some(2.5), mode: stella_protocol::BudgetMode::Observed, session_spent_usd: None, session_limit_usd: None }),

        // ── two subagents are dispatched ────────────────────────────────
        reg(auth, "wire automations triggers API", "subagent"),
        reg(ci, "watch CI + open PR", "subagent"),
        Inbound::Status { agent: auth.into(), status: AgentStatus::Running },
        Inbound::Status { agent: ci.into(), status: AgentStatus::Running },

        // ── lead executes: reads, edits, commits ────────────────────────
        ev(lead, AgentEvent::Stage { name: StageKind::Execute }),
        ev(lead, tool_start("c1", "read_file", json!({ "path": "apps/app/automations/page.tsx" }))),
        ev(lead, AgentEvent::ToolResult { call_id: "c1".into(), output: ToolOutput::Ok { content: "312 lines".into() }, duration_ms: 42, speculated: false }),
        ev(lead, AgentEvent::FileChange { path: "apps/app/automations/page.tsx".into(), kind: FileChangeKind::Modified, diff: Some("@@ -10,3 +10,7 @@\n-  const items = []\n+  const items = useAutomations()\n+  const [q, setQ] = useState(\"\")\n+  const filtered = filter(items, q)\n+  const onNew = () => open()\n".into()) }),
        ev(lead, AgentEvent::StepUsage { output_text: None, step: 2, role: ModelCallRole::Worker, provider: "zai".into(), model: "glm-5.2".into(), input_tokens: 8_200, output_tokens: 900, cached_input_tokens: 6_000, cache_write_tokens: 0, estimated_input_tokens: 8_000, cost_usd: 0.018, duration_ms: 2100, retries: 0, tool_calls: 1, complete: true }),
        ev(lead, AgentEvent::BudgetTick { spent_usd: 0.039, limit_usd: Some(2.5), mode: stella_protocol::BudgetMode::Observed, session_spent_usd: None, session_limit_usd: None }),

        // ── auth subagent creates a file, then asks a question ──────────
        ev(auth, AgentEvent::Stage { name: StageKind::Execute }),
        ev(auth, tool_start("c2", "grep", json!({ "pattern": "trigger" }))),
        ev(auth, AgentEvent::FileChange { path: "apps/api/routes/v1/automations.ts".into(), kind: FileChangeKind::Created, diff: Some("+export const triggers = router()\n+  .post(\"/\", create)\n+  .get(\"/\", list)\n".into()) }),
        ev(auth, AgentEvent::StepUsage { output_text: None, step: 1, role: ModelCallRole::Worker, provider: "zai".into(), model: "glm-5.2".into(), input_tokens: 5_100, output_tokens: 420, cached_input_tokens: 3_000, cache_write_tokens: 0, estimated_input_tokens: 5_000, cost_usd: 0.011, duration_ms: 1400, retries: 0, tool_calls: 1, complete: true }),
        ev(auth, AgentEvent::BudgetTick { spent_usd: 0.011, limit_usd: Some(1.0), mode: stella_protocol::BudgetMode::Observed, session_spent_usd: None, session_limit_usd: None }),
        ev(auth, AgentEvent::AskUser { id: "q1".into(), question: "Which auth guard should the triggers route use?".into(), options: vec!["assertOrgMember".into(), "assertBillingManager".into()] }),

        // ── ci subagent verifies + opens a PR ───────────────────────────
        ev(ci, AgentEvent::Stage { name: StageKind::Verify }),
        ev(ci, AgentEvent::JudgeVerdict { passed: true, evidence: JudgeEvidence { summary: "flip oracle: fail→pass on `pnpm --filter app test:unit`".into(), deterministic: true, evidence_refs: vec![] } }),
        ev(ci, AgentEvent::Commit { sha: "a1b2c3d".into(), message: "feat(automations): triggers + workflows UI".into() }),
        ev(ci, AgentEvent::Pr { url: "https://github.com/macanderson/stella/pull/981".into(), status: PrStatus::Open, number: Some(981), ci: Some(stella_protocol::CiStatus::Running) }),
        ev(ci, AgentEvent::StepUsage { output_text: None, step: 1, role: ModelCallRole::Worker, provider: "zai".into(), model: "glm-5.2-air".into(), input_tokens: 3_000, output_tokens: 180, cached_input_tokens: 0, cache_write_tokens: 0, estimated_input_tokens: 2_900, cost_usd: 0.004, duration_ms: 900, retries: 0, tool_calls: 0, complete: true }),

        // ── lead proposes a larger scope change (gate) ──────────────────
        ev(lead, AgentEvent::ScopeReview { proposal: ScopeProposal {
            summary: "Refactor the automations store into a shared workspace package".into(),
            steps: vec!["extract types".into(), "move hooks".into(), "update 9 imports".into()],
            estimated_files: 11,
            estimated_cost_usd: Some(0.42),
        } }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck::WorkspaceModel;

    #[test]
    fn demo_inbound_folds_into_a_populated_workspace() {
        let mut model = WorkspaceModel::new();
        model.now_ms = 60_000;
        for inbound in demo_inbound(0, 1234) {
            model.apply_inbound(&inbound);
        }
        // Three agents, files in the ledger, routes observed, a pending gate.
        assert_eq!(model.agents.len(), 3);
        assert!(model.ledger.file_count() >= 2);
        assert!(model.ledger.total_added() > 0);
        assert!(model.latest_model().is_some());
        assert!(!model.trace.rows.is_empty());
        let lead = &model.agents[model.index_of("lead").unwrap()];
        assert!(lead.model.pending_scope_review.is_some());
        let auth = &model.agents[model.index_of("sub:auth").unwrap()];
        assert_eq!(auth.status, AgentStatus::WaitingInput);
    }

    #[test]
    fn demo_graph_is_non_empty_and_consistent() {
        let g = demo_graph();
        assert!(!g.is_empty());
        for e in &g.edges {
            assert!(e.from < g.nodes.len() && e.to < g.nodes.len());
        }
    }
}
