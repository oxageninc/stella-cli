//! Usage-accounting witnesses for staged pipeline calls.

use super::*;
use stella_protocol::ToolCall;

fn metered_result(
    mut result: CompletionResult,
    input_tokens: u64,
    output_tokens: u64,
) -> CompletionResult {
    result.usage = CompletionUsage {
        input_tokens,
        output_tokens,
        cached_input_tokens: input_tokens / 2,
        cache_write_tokens: 0,
        reported: true,
    };
    result
}

fn repeated_tool_result(input_tokens: u64, output_tokens: u64) -> CompletionResult {
    metered_result(
        CompletionResult {
            text: String::new(),
            tool_calls: vec![ToolCall {
                call_id: "same-call".into(),
                name: "read_output".into(),
                input: serde_json::json!({"handle": "proc-5"}),
            }],
            usage: CompletionUsage::default(),
            model: "scripted".into(),
            cost_usd: 0.0001,
            finish_reason: None,
        },
        input_tokens,
        output_tokens,
    )
}

/// Pipeline-owned triage/plan calls emit the same usage records as engine
/// calls, and an execute turn that aborts at a safe boundary still contributes
/// every landed call to the final authoritative cost.
#[tokio::test]
async fn aborted_pipeline_totals_match_every_management_and_execute_usage_record() {
    // Three identical no-progress calls draw the engine's stuck-loop
    // steering warning; the fourth (still identical) is what aborts.
    let provider = ScriptedProvider::new(vec![
        metered_result(text_result("multi"), 11, 1),
        metered_result(text_result(r#"["refactor the parser"]"#), 22, 2),
        repeated_tool_result(33, 3),
        repeated_tool_result(33, 3),
        repeated_tool_result(33, 3),
        repeated_tool_result(33, 3),
    ]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![false], "");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let pipeline = Pipeline::new(
        PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall: &recall,
            repo: &repo,
            repo_status: &repo_status,
            diagnostics: &runner,
            tests: &runner,
            approvals: &approvals,
            sleeper: &sleeper,
            hooks: None,
            candidate_workspaces: None,
            mcp_prefetch: None,
            steering: None,
        },
        tx,
        PipelineConfig {
            test_command: Some("cargo test".into()),
            ..PipelineConfig::default()
        },
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);

    let outcome = pipeline
        .run(
            "Refactor the parser and update all callers",
            &mut messages,
            &mut budget,
        )
        .await
        .expect("a loop abort is a clean pipeline outcome");
    assert!(matches!(
        &outcome.status,
        PipelineStatus::Aborted { reason } if reason.contains("stuck-loop")
    ));

    let events = drain(&mut rx);
    let usages: Vec<&AgentEvent> = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::StepUsage { .. }))
        .collect();
    assert_eq!(
        usages.len(),
        6,
        "triage + plan + four execute calls must all be machine-readable"
    );
    let roles: Vec<ModelCallRole> = usages
        .iter()
        .map(|event| match event {
            AgentEvent::StepUsage { role, .. } => *role,
            _ => unreachable!("filtered to StepUsage"),
        })
        .collect();
    assert_eq!(
        roles,
        [
            ModelCallRole::Triage,
            ModelCallRole::Plan,
            ModelCallRole::Worker,
            ModelCallRole::Worker,
            ModelCallRole::Worker,
            ModelCallRole::Worker,
        ]
    );
    assert!(matches!(
        usages[0],
        AgentEvent::StepUsage { output_text: Some(text), .. } if text == "multi"
    ));
    assert!(matches!(
        usages[1],
        AgentEvent::StepUsage { output_text: Some(text), .. }
            if text == r#"["refactor the parser"]"#
    ));
    assert!(usages[2..].iter().all(|event| matches!(
        event,
        AgentEvent::StepUsage {
            output_text: None,
            ..
        }
    )));
    let (input_tokens, output_tokens, usage_cost) = usages.iter().fold(
        (0_u64, 0_u64, 0.0_f64),
        |(input_total, output_total, cost_total), event| match event {
            AgentEvent::StepUsage {
                input_tokens,
                output_tokens,
                cost_usd,
                ..
            } => (
                input_total + input_tokens,
                output_total + output_tokens,
                cost_total + cost_usd,
            ),
            _ => unreachable!("filtered to StepUsage"),
        },
    );
    assert_eq!((input_tokens, output_tokens), (165, 15));
    assert!((usage_cost - 0.0006).abs() < 1e-12);
    assert!((outcome.total_cost_usd - usage_cost).abs() < 1e-12);
    assert!((budget.spent_usd() - usage_cost).abs() < 1e-12);
}
