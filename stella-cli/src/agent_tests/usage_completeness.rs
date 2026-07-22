use super::*;
use stella_protocol::ModelCallRole;

fn usage(provider: &str, model: &str, cost_usd: f64) -> AgentEvent {
    AgentEvent::StepUsage {
        step: 0,
        role: ModelCallRole::Worker,
        provider: provider.into(),
        model: model.into(),
        input_tokens: 10,
        output_tokens: 5,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        estimated_input_tokens: 9,
        cost_usd,
        duration_ms: 12,
        retries: 0,
        tool_calls: 0,
        complete: true,
    }
}

#[test]
fn multi_turn_usage_uses_event_identity_and_actual_fallback_provider() {
    let store = Store::in_memory().expect("store");
    let execution_id = store
        .begin_execution("run", "prompt", "configured", "configured-model")
        .expect("begin");

    assert!(persist_event(
        &store,
        execution_id,
        2,
        &usage("anthropic", "claude-fable-5", 0.01),
        "configured",
    ));
    assert!(persist_event(
        &store,
        execution_id,
        9,
        &usage("openai", "gpt-5", 0.02),
        "configured",
    ));
    store
        .finish_execution_accounted(execution_id, "completed", 0.03, true)
        .expect("finish");

    assert_eq!(store.count("telemetry").expect("count"), 2);
    let telemetry_json = store
        .export_all_json()
        .expect("export")
        .into_iter()
        .find_map(|(table, json)| (table == "telemetry").then_some(json))
        .expect("telemetry export");
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&telemetry_json).expect("telemetry json");
    let providers: Vec<&str> = rows
        .iter()
        .filter_map(|row| row["provider"].as_str())
        .collect();
    assert_eq!(providers, ["anthropic", "openai"]);
}

#[test]
fn event_and_telemetry_persistence_failure_downgrades_later_successful_closeout() {
    let store = Store::in_memory().expect("store");
    let execution_id = store
        .begin_execution("run", "prompt", "anthropic", "claude-fable-5")
        .expect("begin");
    let event = usage("anthropic", "claude-fable-5", 0.01);
    assert!(persist_event(&store, execution_id, 1, &event, "anthropic"));
    assert!(
        !persist_event(&store, execution_id, 1, &event, "anthropic"),
        "duplicate event seq and telemetry identity must surface both write failures"
    );

    let root = tempfile::tempdir().expect("root");
    let registry = ToolRegistry::with_issue_backend(root.path().to_path_buf(), None);
    assert!(!record_execution_end(
        &store,
        execution_id,
        &registry,
        "completed",
        0.01,
        false,
    ));
    assert!(
        !store
            .execution_usage_complete(execution_id)
            .expect("complete flag")
    );

    store
        .finish_execution_accounted(execution_id, "completed", 0.01, true)
        .expect("later finish");
    assert!(
        !store
            .execution_usage_complete(execution_id)
            .expect("complete flag"),
        "completion is monotonic: a later successful closeout cannot restore trust"
    );
}

#[test]
fn cancelled_closeout_is_incomplete_even_when_every_write_succeeds() {
    let store = Store::in_memory().expect("store");
    let execution_id = store
        .begin_execution("deck-sub", "prompt", "anthropic", "claude-fable-5")
        .expect("begin");
    let root = tempfile::tempdir().expect("root");
    let registry = ToolRegistry::with_issue_backend(root.path().to_path_buf(), None);

    assert!(!record_execution_end(
        &store,
        execution_id,
        &registry,
        "cancelled",
        0.0,
        true,
    ));
    assert!(!store.execution_usage_complete(execution_id).unwrap());
    assert!(
        store
            .execution_rollup(execution_id, root.path())
            .unwrap()
            .is_none()
    );
}
