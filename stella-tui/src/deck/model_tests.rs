use super::*;
use stella_protocol::{StageKind, ToolCall, ToolOutput};

fn reg(id: &str) -> Inbound {
    Inbound::Register(AgentMeta::new(id, format!("goal for {id}"), 0))
}
fn ev(agent: &str, event: AgentEvent) -> Inbound {
    Inbound::Event {
        agent: agent.into(),
        event,
    }
}
fn prompt_started(agent: &str, text: &str) -> Inbound {
    Inbound::PromptStarted {
        agent: agent.into(),
        text: text.into(),
    }
}

#[test]
fn text_deltas_feed_the_preview_without_flooding_the_trace() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    let baseline = w.trace.rows.len();
    for _ in 0..100 {
        w.apply_inbound(&ev(
            "lead",
            AgentEvent::TextDelta {
                text: "tok ".into(),
            },
        ));
    }
    assert_eq!(
        w.trace.rows.len(),
        baseline,
        "per-token previews never land trace rows"
    );
    assert_eq!(
        w.agents[0].status,
        AgentStatus::Running,
        "streaming still reads as activity"
    );
    assert_eq!(
        w.agents[0].model.streaming_text.len(),
        400,
        "the per-agent fold accumulates the preview"
    );
}

#[test]
fn session_reset_blanks_transcript_zeroes_cost_and_stops_the_clock() {
    let mut w = WorkspaceModel::new();
    w.now_ms = 1_000;
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&prompt_started("lead", "hello"));
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::Text {
            delta: "hi there".into(),
        },
    ));
    if let Some(a) = w.agents.first_mut() {
        a.cost_usd = 0.42;
    }
    assert!(
        !w.agents[0].model.transcript.is_empty(),
        "precondition: content present"
    );
    assert!(
        w.agents[0].turn_started_ms.is_some(),
        "precondition: clock running"
    );

    // `/clear` sends this.
    w.apply_inbound(&Inbound::SessionReset {
        agent: "lead".into(),
    });

    let a = &w.agents[0];
    assert!(a.model.transcript.is_empty(), "transcript blanked");
    assert_eq!(a.cost_usd, 0.0, "cost stat zeroed");
    assert_eq!(w.total_cost(), 0.0, "workspace cost total zeroed");
    assert_eq!(a.turn_started_ms, None, "wall clock stopped");
    assert!(
        !a.model.hud.complete && a.model.hud.stage.is_none(),
        "hud/progress reset to idle"
    );
}

#[test]
fn deregister_removes_the_row_and_only_that_row() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&reg("req:1"));
    w.apply_inbound(&reg("req:2"));
    assert_eq!(w.agents.len(), 3, "precondition: three rows");

    w.apply_inbound(&Inbound::Deregister {
        agent: "req:1".into(),
    });

    assert_eq!(w.agents.len(), 2, "the deregistered row is gone");
    assert!(w.index_of("req:1").is_none(), "req:1 removed");
    assert_eq!(w.index_of("lead"), Some(0), "lead untouched");
    assert_eq!(w.index_of("req:2"), Some(1), "later rows shift down");
}

#[test]
fn deregister_of_an_unknown_id_is_a_noop() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&Inbound::Deregister {
        agent: "req:404".into(),
    });
    assert_eq!(w.agents.len(), 1, "unknown id disturbs nothing");
    assert_eq!(w.index_of("lead"), Some(0));

    // A stale repeat (the row already gone) is equally harmless.
    w.apply_inbound(&Inbound::Deregister {
        agent: "req:404".into(),
    });
    assert_eq!(w.agents.len(), 1);
}

#[test]
fn prompt_started_flips_a_finished_agent_back_to_running() {
    // A completed turn leaves the lead resting; the next submission must flip
    // it to Running (and clear any stale stage) so the progress bar leaves
    // the full-green complete state and restarts in-progress.
    let mut w = WorkspaceModel::new();
    w.now_ms = 1_000;
    w.apply_inbound(&reg("lead"));
    if let Some(a) = w.agents.first_mut() {
        a.status = AgentStatus::Done;
        a.model.hud.stage = Some(StageKind::Complete);
        a.model.hud.complete = true;
    }
    w.apply_inbound(&prompt_started("lead", "next"));
    let a = &w.agents[0];
    assert_eq!(a.status, AgentStatus::Running, "new turn ⇒ running");
    assert!(a.model.hud.stage.is_none(), "stale stage cleared on submit");
    assert!(!a.model.hud.complete, "stale completion cleared on submit");
    assert!(a.turn_started_ms.is_some(), "the header clock restarts");
}

#[test]
fn turn_clock_holds_zero_then_runs_freezes_and_resets() {
    let mut w = WorkspaceModel::new();
    w.now_ms = 1_000;
    w.apply_inbound(&reg("lead"));

    // Idle, pre-turn: the clock reads zero and is always defined.
    assert_eq!(w.agents[0].turn_clock_ms(w.now_ms), 0);
    assert_eq!(w.agents[0].turn_started_ms, None);

    // A prompt is dispatched — the clock starts from now_ms and runs live.
    w.now_ms = 5_000;
    w.apply_inbound(&prompt_started("lead", "do the thing"));
    assert_eq!(w.agents[0].turn_started_ms, Some(5_000));
    w.now_ms = 8_000;
    assert_eq!(
        w.agents[0].turn_clock_ms(w.now_ms),
        3_000,
        "3s elapsed, live"
    );

    // The turn completes — the clock freezes at its final elapsed and holds
    // it as later deck-clock frames advance.
    w.now_ms = 9_500;
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::Complete {
            model: "m".into(),
            cost_usd: 0.0,
        },
    ));
    assert_eq!(w.agents[0].turn_started_ms, None);
    assert_eq!(w.agents[0].last_turn_ms, Some(4_500)); // 9.5s − 5.0s
    w.now_ms = 60_000;
    assert_eq!(
        w.agents[0].turn_clock_ms(w.now_ms),
        4_500,
        "the completed turn's time is held, not still counting up"
    );

    // The next prompt resets: the prior turn's held time is dropped and the
    // clock runs anew from zero.
    w.apply_inbound(&prompt_started("lead", "again"));
    assert_eq!(w.agents[0].turn_started_ms, Some(60_000));
    assert_eq!(w.agents[0].last_turn_ms, None);
    assert_eq!(w.agents[0].turn_clock_ms(w.now_ms), 0);
}

#[test]
fn a_cancelled_turn_freezes_the_clock_but_a_retryable_error_does_not() {
    let mut w = WorkspaceModel::new();
    w.now_ms = 0;
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&prompt_started("lead", "go"));

    // A retryable error is mid-turn noise (it folds to `Running`) — the turn
    // continues, so the clock keeps ticking.
    w.now_ms = 2_000;
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::Error {
            message: "transient".into(),
            retryable: true,
        },
    ));
    assert_eq!(
        w.agents[0].turn_started_ms,
        Some(0),
        "a retryable error leaves the turn running"
    );
    assert_eq!(w.agents[0].turn_clock_ms(w.now_ms), 2_000);

    // A non-retryable error (user Stop / abort / double-Esc all fold to
    // this) ends the turn exactly like `Complete`.
    w.now_ms = 3_000;
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::Error {
            message: "turn stopped by user".into(),
            retryable: false,
        },
    ));
    assert_eq!(w.agents[0].turn_started_ms, None);
    assert_eq!(w.agents[0].last_turn_ms, Some(3_000));
}

#[test]
fn waiting_input_status_stops_the_clock_after_init_or_handled_command() {
    let mut w = WorkspaceModel::new();
    w.now_ms = 0;
    w.apply_inbound(&reg("lead"));

    // `/init` and other handled commands send `PromptStarted` (the deck
    // doesn't classify commands until after) then `Status { WaitingInput }`
    // — but no `Complete`/`Error` event. Before the fix the clock ran
    // forever; now `WaitingInput` via `Status` freezes it.
    w.now_ms = 1_000;
    w.apply_inbound(&prompt_started("lead", "/init"));
    assert_eq!(w.agents[0].turn_started_ms, Some(1_000));

    w.now_ms = 4_000;
    w.apply_inbound(&Inbound::Status {
        agent: "lead".into(),
        status: AgentStatus::WaitingInput,
    });
    assert_eq!(
        w.agents[0].turn_started_ms, None,
        "WaitingInput status must stop the turn clock"
    );
    assert_eq!(
        w.agents[0].last_turn_ms,
        Some(3_000),
        "the clock freezes at its elapsed, not reset to zero"
    );
    // And stays frozen as frames advance — no runaway.
    w.now_ms = 100_000;
    assert_eq!(w.agents[0].turn_clock_ms(w.now_ms), 3_000);
}

#[test]
fn diff_line_counts_ignore_headers_and_hunks() {
    let diff =
        "--- a/x.rs\n+++ b/x.rs\n@@ -1,3 +1,4 @@\n context\n-old line\n+new line\n+another add\n";
    assert_eq!(count_diff_lines(diff), (2, 1));
}

#[test]
fn diff_line_counts_are_robust_to_empty() {
    assert_eq!(count_diff_lines(""), (0, 0));
    assert_eq!(count_diff_lines("no markers here"), (0, 0));
}

#[test]
fn diff_body_lines_starting_with_extra_plus_or_minus_still_count() {
    // An added line whose content is `++i` arrives as `+++i`; a removed
    // line whose content is `--config` arrives as `---config`. Only real
    // file headers (`+++ b/…`, `--- a/…` — with the space) are skipped.
    let diff = "--- a/x.c\n+++ b/x.c\n@@ -1,2 +1,2 @@\n---config\n+++i\n";
    assert_eq!(count_diff_lines(diff), (1, 1));
}

#[test]
fn register_then_events_route_to_the_right_agent() {
    let mut w = WorkspaceModel::new();
    w.now_ms = 10;
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&reg("sub"));
    assert_eq!(w.agents.len(), 2);
    w.apply_inbound(&ev("sub", AgentEvent::Text { delta: "hi".into() }));
    // The event landed on "sub"'s pure fold, not "lead"'s.
    let sub = &w.agents[w.index_of("sub").unwrap()];
    assert_eq!(sub.model.transcript.len(), 1);
    let lead = &w.agents[w.index_of("lead").unwrap()];
    assert_eq!(lead.model.transcript.len(), 0);
    assert_eq!(sub.status, AgentStatus::Running);
}

#[test]
fn stray_event_auto_registers_its_agent() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&ev(
        "ghost",
        AgentEvent::Stage {
            name: StageKind::Plan,
        },
    ));
    assert!(w.index_of("ghost").is_some());
}

#[test]
fn step_usage_accumulates_tokens_and_file_change_fills_ledger() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::StepUsage {
            step: 1,
            role: stella_protocol::ModelCallRole::Worker,
            provider: "zai".into(),
            model: "glm-5.2".into(),
            input_tokens: 1200,
            output_tokens: 300,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            estimated_input_tokens: 1200,
            cost_usd: 0.01,
            duration_ms: 100,
            retries: 0,
            tool_calls: 1,
            complete: true,
        },
    ));
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::FileChange {
            path: "src/a.rs".into(),
            kind: FileChangeKind::Modified,
            diff: Some("+one\n+two\n-gone\n".into()),
        },
    ));
    let lead = &w.agents[0];
    assert_eq!(lead.tokens_in, 1200);
    assert_eq!(lead.tokens_out, 300);
    assert_eq!(lead.meta.model.as_deref(), Some("glm-5.2"));
    assert_eq!(w.ledger.total_added(), 2);
    assert_eq!(w.ledger.total_removed(), 1);
    assert_eq!(w.ledger.file_count(), 1);
    assert_eq!(w.latest_model(), Some("glm-5.2"));
}

#[test]
fn ledger_counts_reads_without_regressing_the_mutation_badge() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    let read = |path: &str| {
        ev(
            "lead",
            AgentEvent::FileChange {
                path: path.into(),
                kind: FileChangeKind::Read,
                diff: None,
            },
        )
    };
    // A read-only file shows up with an R badge and a read count.
    w.apply_inbound(&read("src/a.rs"));
    w.apply_inbound(&read("src/a.rs"));
    let rec = &w.ledger.records[0];
    assert_eq!(rec.kind, FileChangeKind::Read);
    assert_eq!((rec.changes, rec.reads), (0, 2));

    // A mutation owns the badge and ± totals; a later re-read only
    // counts.
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::FileChange {
            path: "src/a.rs".into(),
            kind: FileChangeKind::Modified,
            diff: Some("+one\n".into()),
        },
    ));
    w.apply_inbound(&read("src/a.rs"));
    let rec = &w.ledger.records[0];
    assert_eq!(rec.kind, FileChangeKind::Modified);
    assert_eq!((rec.changes, rec.reads), (1, 3));
    assert_eq!(w.ledger.total_reads(), 3);
    assert_eq!(w.ledger.total_added(), 1);
    assert_eq!(w.ledger.file_count(), 1);
}

#[test]
fn context_tokens_track_the_latest_window_not_the_cumulative_input() {
    // THE Ctx% P1: the gauge divided the CUMULATIVE input by the window, so
    // after a few turns it pinned at 100%. context_tokens must hold only the
    // most recent call's prompt size (current occupancy), while tokens_in
    // keeps the running total for the I/O column.
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    let step = |input: u64| AgentEvent::StepUsage {
        step: 1,
        role: stella_protocol::ModelCallRole::Worker,
        provider: "zai".into(),
        model: "glm-5.2".into(),
        input_tokens: input,
        output_tokens: 10,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        estimated_input_tokens: input,
        cost_usd: 0.0,
        duration_ms: 1,
        retries: 0,
        tool_calls: 0,
        complete: true,
    };
    // Three calls of 150k each: cumulative 450k dwarfs the 200k window, but
    // the window was only 150k full on the LAST call.
    w.apply_inbound(&ev("lead", step(150_000)));
    w.apply_inbound(&ev("lead", step(150_000)));
    w.apply_inbound(&ev("lead", step(150_000)));

    let lead = &w.agents[0];
    assert_eq!(lead.context_tokens, 150_000, "occupancy = latest call only");
    assert_eq!(lead.tokens_in, 450_000, "cumulative input is still summed");
    // Occupancy reads a real 75%, not a pinned 100% from 450k / 200k.
    assert!((lead.context_tokens as f64 / 200_000.0 - 0.75).abs() < 1e-9);
}

#[test]
fn budget_tick_sets_live_spend_without_double_counting_step_usage() {
    let step = |cost_usd: f64| AgentEvent::StepUsage {
        step: 1,
        role: stella_protocol::ModelCallRole::Worker,
        provider: "test".into(),
        model: "m".into(),
        input_tokens: 1,
        output_tokens: 1,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        estimated_input_tokens: 1,
        cost_usd,
        duration_ms: 1,
        retries: 0,
        tool_calls: 0,
        complete: true,
    };
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&ev("lead", step(0.10)));
    // Before any tick, step costs are the fallback spend — a stream
    // without BudgetTicks still shows real dollars.
    assert_eq!(w.agents[0].cost_usd, 0.10);
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::BudgetTick {
            spent_usd: 0.42,
            limit_usd: Some(2.0),
            mode: stella_protocol::BudgetMode::Observed,
        },
    ));
    assert_eq!(w.agents[0].cost_usd, 0.42, "the tick is authoritative");
    // Once ticked, later step costs no longer add on top (that would
    // double-count what the next tick already includes).
    w.apply_inbound(&ev("lead", step(5.0)));
    assert_eq!(w.agents[0].cost_usd, 0.42);
}

#[test]
fn supervisor_status_for_an_unknown_agent_auto_registers_it() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&Inbound::Status {
        agent: "ghost".into(),
        status: AgentStatus::Paused,
    });
    let i = w
        .index_of("ghost")
        .expect("status auto-registers, like Event");
    assert_eq!(w.agents[i].status, AgentStatus::Paused);
}

#[test]
fn scope_review_marks_the_agent_waiting_for_input() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::ScopeReview {
            proposal: stella_protocol::ScopeProposal {
                summary: "widen the refactor".into(),
                steps: vec![],
                estimated_files: 2,
                estimated_cost_usd: None,
            },
        },
    ));
    assert_eq!(w.agents[0].status, AgentStatus::WaitingInput);
}

#[test]
fn supervisor_status_and_terminal_kill_are_respected() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&Inbound::Status {
        agent: "lead".into(),
        status: AgentStatus::Killed,
    });
    // Even a fresh event cannot resurrect a killed agent's lifecycle.
    w.apply_inbound(&ev("lead", AgentEvent::Text { delta: "x".into() }));
    assert_eq!(w.agents[0].status, AgentStatus::Killed);
}

#[test]
fn complete_marks_done_and_records_final_cost() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::Complete {
            model: "glm".into(),
            cost_usd: 0.033,
        },
    ));
    assert_eq!(w.agents[0].status, AgentStatus::Done);
    assert!(w.agents[0].cost_usd >= 0.033);
}

#[test]
fn trace_captures_every_agent_and_filters_by_agent() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("a"));
    w.apply_inbound(&reg("b"));
    w.apply_inbound(&ev(
        "a",
        AgentEvent::Stage {
            name: StageKind::Execute,
        },
    ));
    w.apply_inbound(&ev(
        "b",
        AgentEvent::ToolStart {
            call: ToolCall {
                call_id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
            },
        },
    ));
    assert_eq!(w.trace.rows.len(), 2);
    assert_eq!(w.trace.for_agent("a").count(), 1);
    assert_eq!(w.trace.for_agent("b").count(), 1);
}

#[test]
fn ask_user_marks_waiting_then_a_later_event_resumes_running() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::AskUser {
            id: "q".into(),
            question: "which db?".into(),
            options: vec![],
        },
    ));
    assert_eq!(w.agents[0].status, AgentStatus::WaitingInput);
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::ToolResult {
            call_id: "q".into(),
            output: ToolOutput::Ok {
                content: "sqlite".into(),
            },
            duration_ms: 1,
            speculated: false,
        },
    ));
    assert_eq!(w.agents[0].status, AgentStatus::Running);
}

#[test]
fn prompt_queue_never_blocks_and_dispatches_fifo() {
    let mut q = PromptQueue::default();
    q.enqueue("first".into(), 1);
    q.enqueue("second".into(), 2);
    assert_eq!(q.pending(), 2);
    assert_eq!(q.take_next().as_deref(), Some("first"));
    assert_eq!(q.pending(), 1);
    assert_eq!(q.take_next().as_deref(), Some("second"));
    assert_eq!(q.pending(), 0);
    assert_eq!(q.take_next(), None);
}

#[test]
fn prompt_started_pops_the_front_of_the_queue_and_leaves_a_trace() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    // The shell enqueues on submit (its labeled out-of-band mutation)…
    w.queue.enqueue("first".into(), 1);
    w.queue.enqueue("second".into(), 2);
    // …and the dispatcher's PromptStarted drains it front-first.
    w.apply_inbound(&Inbound::PromptStarted {
        agent: "lead".into(),
        text: "first".into(),
    });
    assert_eq!(w.queue.pending(), 1);
    assert_eq!(
        w.queue.items.front().map(|q| q.text.as_str()),
        Some("second")
    );
    let row = w.trace.rows.back().expect("a trace row was recorded");
    assert_eq!(row.agent, "lead");
    assert!(row.summary.contains("first"), "{}", row.summary);
}

#[test]
fn prompt_started_with_an_unseen_text_never_drops_someone_elses_entry() {
    let mut w = WorkspaceModel::new();
    w.queue.enqueue("queued by the shell".into(), 1);
    // A dispatch the shell never enqueued (e.g. a driver-side prompt)
    // must not eat the front entry the user is still watching.
    w.apply_inbound(&Inbound::PromptStarted {
        agent: "lead".into(),
        text: "driver-side prompt".into(),
    });
    assert_eq!(w.queue.pending(), 1);
}

#[test]
fn prompt_requeued_returns_the_prompt_to_the_front_of_the_queue() {
    let mut w = WorkspaceModel::new();
    w.apply_inbound(&reg("lead"));
    w.queue.enqueue("second".into(), 1);
    w.queue.enqueue("third".into(), 2);
    // A double-Esc cancelled "first" mid-turn; the driver returned it to
    // the front of its backlog and mirrored that here.
    w.apply_inbound(&Inbound::PromptRequeued {
        agent: "lead".into(),
        text: "first".into(),
    });
    assert_eq!(w.queue.pending(), 3);
    assert_eq!(
        w.queue.items.front().map(|q| q.text.as_str()),
        Some("first")
    );
    let row = w.trace.rows.back().expect("a trace row was recorded");
    assert_eq!(row.agent, "lead");
    assert!(row.summary.contains("first"), "{}", row.summary);
}

#[test]
fn front_inserts_stack_so_the_newest_front_insert_runs_first() {
    // Double-Esc requeues the interrupted prompt at the front; the user's
    // next submission front-inserts ABOVE it — new prompt, then the
    // returned prompt, then the rest of the backlog.
    let mut q = PromptQueue::default();
    q.enqueue("rest".into(), 1);
    q.enqueue_front("returned".into(), 2);
    q.enqueue_front("new".into(), 3);
    assert_eq!(q.take_next().as_deref(), Some("new"));
    assert_eq!(q.take_next().as_deref(), Some("returned"));
    assert_eq!(q.take_next().as_deref(), Some("rest"));
}

#[test]
fn prompt_queue_edits_like_a_list() {
    let mut q = PromptQueue::default();
    q.enqueue("a".into(), 1);
    q.enqueue("b".into(), 2);
    q.enqueue("c".into(), 3);
    // Remove by position, not just from the front.
    assert_eq!(q.remove(1).as_deref(), Some("b"));
    assert_eq!(q.pending(), 2);
    assert_eq!(q.remove(9), None, "out of range is a no-op");
    q.clear();
    assert_eq!(q.pending(), 0);
}

#[test]
fn pr_events_fold_into_the_read_model_latest_wins_and_ci_updates_in_place() {
    let mut w = WorkspaceModel::new();
    assert_eq!(w.pr, None, "no PR story before any Pr event");
    w.apply_inbound(&reg("lead"));
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::Pr {
            url: "https://github.com/x/y/pull/183".into(),
            status: PrStatus::Open,
            number: Some(183),
            ci: None,
        },
    ));
    let pr = w.pr.as_ref().expect("the Pr event folded");
    assert_eq!(pr.number, Some(183));
    assert_eq!(pr.status, PrStatus::Open);
    assert_eq!(pr.ci, None, "not polled yet");

    // A CI re-poll on the same PR replaces the snapshot in place.
    w.apply_inbound(&ev(
        "lead",
        AgentEvent::Pr {
            url: "https://github.com/x/y/pull/183".into(),
            status: PrStatus::Open,
            number: Some(183),
            ci: Some(CiStatus::Failing),
        },
    ));
    let pr = w.pr.as_ref().expect("still present");
    assert_eq!(pr.ci, Some(CiStatus::Failing));
    assert_eq!(pr.number, Some(183), "same PR, updated verdict");

    // A later Pr event from ANY agent wins outright — latest wins.
    w.apply_inbound(&ev(
        "sub",
        AgentEvent::Pr {
            url: "https://github.com/x/y/pull/184".into(),
            status: PrStatus::Merged,
            number: Some(184),
            ci: Some(CiStatus::Passing),
        },
    ));
    let pr = w.pr.as_ref().expect("still present");
    assert_eq!(pr.number, Some(184));
    assert_eq!(pr.status, PrStatus::Merged);
    assert_eq!(pr.ci, Some(CiStatus::Passing));
}

#[test]
fn pipeline_toggle_folds_into_the_model() {
    // `/pipeline` flips routing driver-side; the stat box must track it
    // through the fold, both directions.
    let mut w = WorkspaceModel::new();
    assert!(!w.pipeline, "the deck starts on the raw engine loop");
    w.apply_inbound(&Inbound::Pipeline(true));
    assert!(w.pipeline);
    w.apply_inbound(&Inbound::Pipeline(false));
    assert!(!w.pipeline);
}

#[test]
fn deck_tab_cycles_both_ways() {
    assert_eq!(DeckTab::Session.next(), DeckTab::Agents);
    // Tab order ends …Skills → Mcp → Issues → Settings; Settings wraps to
    // Session and is Session's predecessor backward.
    assert_eq!(DeckTab::Files.next(), DeckTab::Skills);
    assert_eq!(DeckTab::Skills.next(), DeckTab::Mcp);
    assert_eq!(DeckTab::Mcp.next(), DeckTab::Issues);
    assert_eq!(DeckTab::Issues.next(), DeckTab::Settings);
    assert_eq!(DeckTab::Settings.next(), DeckTab::Session);
    assert_eq!(DeckTab::Session.prev(), DeckTab::Settings);
}

#[test]
fn deck_tab_all_round_trips_through_index() {
    // Every tab in ALL maps to a unique index and back; a full next()
    // walk visits each exactly once before wrapping.
    for (i, tab) in DeckTab::ALL.iter().enumerate() {
        assert_eq!(tab.index(), i);
        assert_eq!(DeckTab::from_index(i), *tab);
    }
    let mut tab = DeckTab::Session;
    for expected in DeckTab::ALL.iter().skip(1) {
        tab = tab.next();
        assert_eq!(tab, *expected);
    }
    assert_eq!(tab.next(), DeckTab::Session, "the cycle closes");
}

#[test]
fn activity_spark_pads_and_caps() {
    let mut s = ActivitySpark::new(4);
    s.push(10);
    s.push(20);
    assert_eq!(s.padded(), vec![0, 0, 10, 20]);
    for v in [1, 2, 3, 4] {
        s.push(v);
    }
    assert_eq!(s.padded(), vec![1, 2, 3, 4]); // capped at width 4
}
