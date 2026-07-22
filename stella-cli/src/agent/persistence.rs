//! Event/telemetry persistence and execution closeout.

use super::*;

#[derive(Default)]
pub(crate) struct RendererOutcome {
    pub(crate) events: Vec<AgentEvent>,
    pub(crate) persistence_complete: bool,
}

pub(crate) fn spawn_renderer(
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    format: OutputFormat,
    execution: Option<(Arc<Store>, i64)>,
    provider_id: String,
) -> tokio::task::JoinHandle<RendererOutcome> {
    tokio::spawn(async move {
        let mut tool_names: HashMap<String, String> = HashMap::new();
        let mut outcome = RendererOutcome {
            events: Vec::new(),
            persistence_complete: true,
        };
        let mut seq = 0u64;
        let mut store_warned = false;
        let mut stream_terminal = None;
        while let Some(event) = rx.recv().await {
            let event = if format == OutputFormat::StreamJson {
                let Some(event) = defer_stream_terminal(&mut stream_terminal, event) else {
                    continue;
                };
                event
            } else {
                event
            };
            let preview = matches!(event, AgentEvent::TextDelta { .. });
            if let Some((store, id)) = &execution
                && !preview
            {
                if !persist_event(store, *id, seq, &event, &provider_id) {
                    outcome.persistence_complete = false;
                    if !store_warned {
                        eprintln!(
                            "  {} store write failed — telemetry for this execution is incomplete",
                            "⚠".yellow()
                        );
                        store_warned = true;
                    }
                }
                seq += 1;
            }
            match format {
                OutputFormat::StreamJson => match serde_json::to_string(&event) {
                    Ok(line) => println!("{line}"),
                    Err(e) => eprintln!("{{\"type\":\"error\",\"message\":\"serialize: {e}\"}}"),
                },
                OutputFormat::Json => outcome.events.push(event),
                OutputFormat::Text => match &event {
                    AgentEvent::ToolStart { call } => {
                        tool_names.insert(call.call_id.clone(), call.name.clone());
                        tui::tool_call_card(&call.name, &call.input, "running");
                    }
                    AgentEvent::ToolResult {
                        call_id,
                        output,
                        duration_ms,
                        ..
                    } => {
                        let name = tool_names
                            .get(call_id)
                            .map(String::as_str)
                            .unwrap_or("tool");
                        let content = match output {
                            ToolOutput::Ok { content } => content.clone(),
                            ToolOutput::Error { message } => message.clone(),
                        };
                        tui::tool_result_card(
                            name,
                            &content,
                            output.is_error(),
                            Duration::from_millis(*duration_ms),
                        );
                    }
                    other => tui::render_event(other),
                },
            }
        }
        // `Complete` is a protocol terminator, not ordinary narration. Hold
        // it until every later accounting/reflection event has drained, and
        // persist/print exactly one terminal frame as the final stream item.
        if let Some(event) = stream_terminal {
            if let Some((store, id)) = &execution
                && !persist_event(store, *id, seq, &event, &provider_id)
            {
                outcome.persistence_complete = false;
            }
            if let Ok(line) = serde_json::to_string(&event) {
                println!("{line}");
            }
        }
        outcome
    })
}

fn defer_stream_terminal(
    pending: &mut Option<AgentEvent>,
    event: AgentEvent,
) -> Option<AgentEvent> {
    if matches!(event, AgentEvent::Complete { .. }) {
        *pending = Some(event);
        None
    } else {
        Some(event)
    }
}

pub(crate) fn record_execution_end(
    store: &Store,
    execution_id: i64,
    registry: &ToolRegistry,
    outcome_label: &str,
    cost_usd: f64,
    persistence_complete: bool,
) -> bool {
    let files_ok = store
        .record_files_touched(execution_id, &file_touch_rows(registry))
        .is_ok();
    let citations_ok = store
        .record_memory_citations(execution_id, &memory_citation_rows(registry))
        .is_ok();
    let uses: Vec<stella_store::AgentUseRow> = registry
        .drain_agent_uses()
        .into_iter()
        .map(|u| stella_store::AgentUseRow {
            agent: u.agent,
            version: u.version,
            reason: u.reason,
        })
        .collect();
    let uses_ok = uses.is_empty() || store.record_agent_uses(execution_id, &uses).is_ok();
    let mcp_usage = mcp_usage_rows(registry);
    let mcp_usage_ok = store.record_mcp_usage(execution_id, &mcp_usage).is_ok();
    // Cancellation can race a provider response after dispatch. Even when all
    // local writes succeed, the provider-side usage envelope is unknowable and
    // the execution must never become exportable.
    let terminal_usage_known = outcome_label != "cancelled";
    let audit_complete = persistence_complete
        && files_ok
        && citations_ok
        && uses_ok
        && mcp_usage_ok
        && terminal_usage_known;
    let finish_ok = store
        .finish_execution_accounted(execution_id, outcome_label, cost_usd, audit_complete)
        .is_ok();
    let _ = store.materialize_tool_calls(execution_id);
    let _ = store.finalize_execution_reflection(execution_id);
    let _ = store.sync_to_usage_default(execution_id);
    let _ = crate::enterprise_telemetry::enqueue_finalized_execution(store, execution_id);
    audit_complete && finish_ok
}

fn mcp_usage_rows(registry: &ToolRegistry) -> Vec<stella_store::McpUsageRow> {
    registry
        .take_mcp_usage()
        .into_iter()
        .map(|u| stella_store::McpUsageRow {
            server: u.server,
            tool: u.tool,
            reason: u.reason,
            called_at_ms: u.called_at_ms as i64,
        })
        .collect()
}

fn file_touch_rows(registry: &ToolRegistry) -> Vec<stella_store::FileTouchRow> {
    registry
        .file_touch_telemetry()
        .files_touched
        .into_iter()
        .map(|record| stella_store::FileTouchRow {
            ops: record.crud_events.iter().map(|op| op.letter()).collect(),
            lines_added: record.lines_added,
            lines_removed: record.lines_removed,
            events_json: serde_json::to_string(&record.events).unwrap_or_else(|_| "[]".into()),
            path: record.path,
        })
        .collect()
}

fn memory_citation_rows(registry: &ToolRegistry) -> Vec<stella_store::MemoryCitationRow> {
    registry
        .take_memory_citations()
        .into_iter()
        .map(|c| stella_store::MemoryCitationRow {
            memory_id: c.memory_id,
            useful_score: c.useful_score,
            truthful: c.truthful,
            remark: c.remark,
        })
        .collect()
}

pub(crate) fn warn_store_write_failed(what: &str) {
    eprintln!(
        "  {} store write failed — {what} for this execution is incomplete",
        "⚠".yellow()
    );
}

pub(crate) fn persist_event(
    store: &Store,
    execution_id: i64,
    seq: u64,
    event: &AgentEvent,
    legacy_provider_id: &str,
) -> bool {
    let recorded = store.record_event(execution_id, seq, event).is_ok();
    let mut telemetry_ok = true;
    let mut usage_complete = true;
    if let AgentEvent::StepUsage {
        role,
        provider,
        model,
        input_tokens,
        output_tokens,
        cached_input_tokens,
        cache_write_tokens,
        estimated_input_tokens,
        cost_usd,
        duration_ms,
        retries,
        tool_calls,
        complete,
        ..
    } = event
    {
        let actual_provider = if provider.is_empty() {
            legacy_provider_id
        } else {
            provider
        };
        telemetry_ok = store
            .record_telemetry(
                execution_id,
                &TelemetryRow {
                    // Event-stream seq is the execution-global call identity;
                    // engine-local `step` restarts on each pipeline turn.
                    step: seq,
                    provider: actual_provider.to_string(),
                    call_role: serde_json::to_value(role)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_else(|| "unknown".into()),
                    model: model.clone(),
                    input_tokens: *input_tokens,
                    estimated_input_tokens: *estimated_input_tokens,
                    output_tokens: *output_tokens,
                    cache_read_tokens: *cached_input_tokens,
                    cache_miss_tokens: input_tokens.saturating_sub(*cached_input_tokens),
                    cache_write_tokens: *cache_write_tokens,
                    cost_usd: *cost_usd,
                    duration_ms: *duration_ms,
                    retries: *retries,
                    tool_calls: *tool_calls as u64,
                    usage_complete: *complete,
                },
            )
            .is_ok();
        usage_complete = *complete;
        crate::model_catalog::note_wire_model(actual_provider, model);
    } else if matches!(event, AgentEvent::UsageIncomplete { .. }) {
        usage_complete = false;
    }
    let complete = recorded && telemetry_ok && usage_complete;
    if !complete {
        let _ = store.mark_execution_usage_incomplete(execution_id);
    }
    complete
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    #[test]
    fn complete_is_unique_and_final_even_when_later_events_arrive() {
        let events = vec![
            AgentEvent::Stage {
                name: stella_protocol::StageKind::Execute,
            },
            AgentEvent::Complete {
                model: "old".into(),
                cost_usd: 1.0,
            },
            AgentEvent::Stage {
                name: stella_protocol::StageKind::Reflect,
            },
            AgentEvent::Complete {
                model: "final".into(),
                cost_usd: 1.25,
            },
        ];
        let mut terminal = None;
        let mut ordered: Vec<_> = events
            .into_iter()
            .filter_map(|event| defer_stream_terminal(&mut terminal, event))
            .collect();
        ordered.extend(terminal);

        assert_eq!(
            ordered
                .iter()
                .filter(|event| matches!(event, AgentEvent::Complete { .. }))
                .count(),
            1
        );
        assert!(matches!(
            ordered.last(),
            Some(AgentEvent::Complete { model, cost_usd })
                if model == "final" && (*cost_usd - 1.25).abs() < f64::EPSILON
        ));
    }

    #[tokio::test]
    async fn stream_renderer_persists_reflection_before_one_terminal_complete() {
        let store = std::sync::Arc::new(stella_store::Store::in_memory().expect("store"));
        let execution_id = store
            .begin_execution("pipeline", "prompt", "anthropic", "claude")
            .expect("begin");
        store
            .set_execution_session(execution_id, "stream-order")
            .expect("session");
        let (tx, rx) = mpsc::unbounded_channel();
        let renderer = spawn_renderer(
            rx,
            OutputFormat::StreamJson,
            Some((store.clone(), execution_id)),
            "anthropic".into(),
        );
        tx.send(AgentEvent::Complete {
            model: "worker".into(),
            cost_usd: 1.0,
        })
        .unwrap();
        tx.send(AgentEvent::Stage {
            name: stella_protocol::StageKind::Reflect,
        })
        .unwrap();
        tx.send(AgentEvent::Complete {
            model: "worker+reflection".into(),
            cost_usd: 1.25,
        })
        .unwrap();
        drop(tx);

        let outcome = renderer.await.expect("renderer");
        assert!(outcome.persistence_complete);
        let journal = store.session_events("stream-order").expect("journal");
        assert_eq!(journal.events.len(), 2);
        assert!(matches!(
            journal.events.first().map(|record| &record.event),
            Some(AgentEvent::Stage {
                name: stella_protocol::StageKind::Reflect
            })
        ));
        assert!(matches!(
            journal.events.last().map(|record| &record.event),
            Some(AgentEvent::Complete { model, cost_usd })
                if model == "worker+reflection"
                    && (*cost_usd - 1.25).abs() < f64::EPSILON
        ));
    }
}
