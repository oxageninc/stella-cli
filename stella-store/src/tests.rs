use super::*;

#[test]
fn execution_lifecycle_events_and_telemetry_roundtrip() {
    let store = Store::in_memory().unwrap();
    let id = store
        .begin_execution("goal", "make tests pass", "zai", "glm-5.2")
        .unwrap();

    // Full event stream including chain of thought.
    store
        .record_event(
            id,
            0,
            &AgentEvent::Reasoning {
                delta: "first I will read the failing test".into(),
            },
        )
        .unwrap();
    store
        .record_event(
            id,
            1,
            &AgentEvent::Text {
                delta: "done".into(),
            },
        )
        .unwrap();
    assert_eq!(store.count("events").unwrap(), 2);

    store
        .record_telemetry(
            id,
            &TelemetryRow {
                step: 0,
                provider: "zai".into(),
                call_role: "worker".into(),
                model: "glm-5.2".into(),
                input_tokens: 12_000,
                estimated_input_tokens: 11_000,
                output_tokens: 400,
                cache_read_tokens: 9_000,
                cache_miss_tokens: 3_000,
                cache_write_tokens: 0,
                cost_usd: 0.0042,
                duration_ms: 1_830,
                retries: 1,
                tool_calls: 3,
                usage_complete: true,
            },
        )
        .unwrap();
    store
        .record_files_touched(id, &[touch_row("src/main.rs", "RU", 18, 4)])
        .unwrap();
    store.finish_execution(id, "completed", 0.0042).unwrap();

    assert_eq!(store.count("telemetry").unwrap(), 1);
    assert_eq!(store.count("files_touched").unwrap(), 1);
    assert_eq!(store.count("executions").unwrap(), 1);
}

/// A file-touch row with a one-entry audit log carrying the same deltas.
fn touch_row(path: &str, ops: &str, added: u64, removed: u64) -> FileTouchRow {
    FileTouchRow {
        path: path.into(),
        ops: ops.into(),
        lines_added: added,
        lines_removed: removed,
        events_json: format!(
            "[{{\"event\":\"U\",\"reason\":\"test\",\
             \"lines_added\":{added},\"lines_removed\":{removed}}}]"
        ),
    }
}

#[test]
fn files_touched_rows_roundtrip_line_deltas_and_audit_log() {
    let store = Store::in_memory().unwrap();
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_files_touched(id, &[touch_row("src/render.rs", "RU", 18, 4)])
        .unwrap();

    let conn = store.lock();
    let (added, removed, events): (i64, i64, String) = conn
        .query_row(
            "SELECT lines_added, lines_removed, events FROM files_touched \
             WHERE execution_id = ? AND path = 'src/render.rs'",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((added, removed), (18, 4));
    let parsed: serde_json::Value = serde_json::from_str(&events).unwrap();
    assert_eq!(parsed[0]["event"], "U");
    assert_eq!(parsed[0]["lines_added"], 18);
}

#[test]
fn files_touched_rejects_a_duplicate_path_per_execution() {
    let store = Store::in_memory().unwrap();
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_files_touched(id, &[touch_row("src/a.rs", "R", 0, 0)])
        .unwrap();
    assert!(
        store
            .record_files_touched(id, &[touch_row("src/a.rs", "RU", 1, 0)])
            .is_err(),
        "a second session record for the same normalized path must violate UNIQUE"
    );
    // The same path under a DIFFERENT execution is a fresh session record.
    let other = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_files_touched(other, &[touch_row("src/a.rs", "R", 0, 0)])
        .unwrap();
}

/// A citation row with the fields the eligibility policy reads.
fn citation(memory_id: &str, score: i64, truthful: bool, remark: &str) -> MemoryCitationRow {
    MemoryCitationRow {
        memory_id: memory_id.into(),
        useful_score: score,
        truthful,
        remark: remark.into(),
    }
}

#[test]
fn memory_citations_roundtrip_and_reject_a_duplicate_per_execution() {
    let store = Store::in_memory().unwrap();
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_memory_citations(
            id,
            &[
                citation("nod_aaa", 5, true, "pinpointed the failing module"),
                citation("nod_bbb", 2, false, "path has moved since"),
            ],
        )
        .unwrap();
    assert!(
        store
            .record_memory_citations(id, &[citation("nod_aaa", 4, true, "again")])
            .is_err(),
        "a second citation of the same memory in one execution must violate UNIQUE"
    );
    // The same memory under a DIFFERENT execution is a fresh citation.
    let other = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_memory_citations(other, &[citation("nod_aaa", 4, true, "held again")])
        .unwrap();
    assert_eq!(store.count("memory_citations").unwrap(), 3);

    let stats = store.memory_citation_stats().unwrap();
    assert_eq!(
        stats
            .iter()
            .map(|s| s.memory_id.as_str())
            .collect::<Vec<_>>(),
        ["nod_aaa", "nod_bbb"],
        "most-cited first"
    );
    let aaa = &stats[0];
    assert_eq!(aaa.citations, 2);
    assert!((aaa.avg_score - 4.5).abs() < 1e-12);
    assert!((aaa.truthful_rate - 1.0).abs() < 1e-12);
    assert_eq!(aaa.negatives, 0);
    assert_eq!(aaa.positive_streak, 2);
    assert!(!aaa.eligible, "2 positives is nowhere near the >10 gate");
    let bbb = &stats[1];
    assert_eq!((bbb.negatives, bbb.positive_streak), (1, 0));
    assert!((bbb.truthful_rate - 0.0).abs() < 1e-12);
}

#[test]
fn mcp_usage_roundtrips_and_aggregates_per_server_tool() {
    let store = Store::in_memory().unwrap();
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    // Two calls to github/search_issues (one with a reason), one to fs/read.
    let usage = |server: &str, tool: &str, reason: &str, t: i64| McpUsageRow {
        server: server.into(),
        tool: tool.into(),
        reason: reason.into(),
        called_at_ms: t,
    };
    store
        .record_mcp_usage(
            id,
            &[
                usage("github", "search_issues", "", 100),
                usage("github", "search_issues", "find the flake", 200),
                usage("fs", "read", "", 150),
            ],
        )
        .unwrap();
    assert_eq!(store.count("mcp_usage").unwrap(), 3);

    // A different execution's calls are separate rows (per-call log — the
    // same server+tool is NOT a UNIQUE violation, unlike memory citations).
    let other = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_mcp_usage(other, &[usage("github", "search_issues", "", 300)])
        .unwrap();
    assert_eq!(store.count("mcp_usage").unwrap(), 4);

    let stats = store.mcp_usage_stats().unwrap();
    // Most-used first: github/search_issues (3) before fs/read (1).
    assert_eq!(stats[0].server, "github");
    assert_eq!(stats[0].tool, "search_issues");
    assert_eq!(stats[0].calls, 3);
    // The most recent non-empty reason is kept.
    assert_eq!(stats[0].last_reason, "find the flake");
    assert_eq!(stats[0].last_called_at_ms, 300);
    assert_eq!(stats[1].server, "fs");
    assert_eq!(stats[1].calls, 1);
    assert_eq!(stats[1].last_reason, "");
}

#[test]
fn promotion_eligibility_requires_strictly_more_than_ten_positive_citations() {
    let positives = |n: usize| -> Vec<MemoryCitationRow> {
        (0..n).map(|_| citation("nod_x", 5, true, "held")).collect()
    };
    // Exactly 10 all-positive: NOT eligible (spec: MORE THAN 10).
    assert!(!fold_citation_stats(&positives(10))[0].eligible);
    // 11 all-positive: eligible.
    assert!(fold_citation_stats(&positives(11))[0].eligible);
    // 11 with one negative anywhere: NOT eligible — one negative remark
    // resets the streak, wherever it lands.
    for negative_at in [0, 5, 10] {
        let mut rows = positives(11);
        rows[negative_at] = citation("nod_x", 1, true, "wasted the turn");
        let s = &fold_citation_stats(&rows)[0];
        assert!(
            !s.eligible,
            "negative at {negative_at} must disqualify (streak {})",
            s.positive_streak
        );
    }
    // An untruthful citation is negative regardless of its score.
    let mut rows = positives(11);
    rows[10] = citation("nod_x", 5, false, "the convention changed");
    assert!(!fold_citation_stats(&rows)[0].eligible);
    // Re-earned: after a negative, MORE THAN 10 fresh positives requalify.
    let mut rows = vec![citation("nod_x", 1, false, "stale")];
    rows.extend(positives(11));
    let s = &fold_citation_stats(&rows)[0];
    assert_eq!((s.citations, s.negatives, s.positive_streak), (12, 1, 11));
    assert!(
        s.eligible,
        "the streak since the last negative is what gates"
    );
}

#[test]
fn quarantine_triggers_at_two_untruthful_citations() {
    // One untruthful citation does not quarantine — it's a signal but
    // the memory might have been judged wrong in one context.
    let one_neg = vec![
        citation("nod_x", 4, true, "held"),
        citation("nod_x", 2, false, "stale path"),
    ];
    assert!(!fold_citation_stats(&one_neg)[0].quarantined);

    // Two untruthful citations quarantine regardless of score.
    let two_neg = vec![
        citation("nod_x", 4, true, "held"),
        citation("nod_x", 5, false, "stale"),
        citation("nod_x", 3, false, "also stale"),
    ];
    let s = &fold_citation_stats(&two_neg)[0];
    assert!(s.quarantined, "two negatives must quarantine");
    assert_eq!(s.negatives, 2);

    // A low score (truthful) is negative for promotion but does NOT
    // count toward quarantine — quarantine is about untruthfulness.
    let low_scores = vec![
        citation("nod_x", 1, true, "wasted"),
        citation("nod_x", 1, true, "wasted again"),
    ];
    assert!(!fold_citation_stats(&low_scores)[0].quarantined);
}

#[test]
fn v3_migration_adds_memory_citations_to_a_legacy_database() {
    let root = temp_root("v3_memory_citations");
    {
        let conn = Connection::open(root.join(".stella/store.db")).unwrap();
        conn.execute_batch(LEGACY_V0_SCHEMA).unwrap();
    }
    let store = Store::open(&root).unwrap();
    assert_eq!(user_version(&store), SCHEMA_VERSION);
    assert_eq!(store.count("memory_citations").unwrap(), 0);
    // The new-shape write path works on the migrated file.
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_memory_citations(id, &[citation("nod_aaa", 4, true, "held")])
        .unwrap();
    assert_eq!(store.count("memory_citations").unwrap(), 1);
    drop(store);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn data_plane_tables_roundtrip_and_tool_histogram() {
    let store = Store::in_memory().unwrap();
    let id = store
        .begin_execution("deck", "build the feature", "zai", "glm-5.2")
        .unwrap();

    store
        .record_tool_calls(
            id,
            &[
                ToolCallRow {
                    call_id: "c1".into(),
                    name: "grep".into(),
                    surface: "native".into(),
                    args_json: "{\"pattern\":\"foo\"}".into(),
                    args_digest: "d1".into(),
                    reason: "find foo".into(),
                    ok: true,
                    error: String::new(),
                    bytes_out: 120,
                    duration_ms: 14,
                },
                ToolCallRow {
                    call_id: "c2".into(),
                    name: "grep".into(),
                    surface: "native".into(),
                    args_json: "{}".into(),
                    args_digest: "d2".into(),
                    reason: String::new(),
                    ok: true,
                    error: String::new(),
                    bytes_out: 0,
                    duration_ms: 9,
                },
                ToolCallRow {
                    call_id: "c3".into(),
                    name: "read_file".into(),
                    surface: "native".into(),
                    args_json: "{}".into(),
                    args_digest: "d3".into(),
                    reason: String::new(),
                    ok: false,
                    error: "nope".into(),
                    bytes_out: 0,
                    duration_ms: 3,
                },
            ],
        )
        .unwrap();
    assert_eq!(store.count("tool_calls").unwrap(), 3);
    // The histogram powers the "grep a lot, graph_query never" signal.
    let counts = store.tool_call_name_counts().unwrap();
    assert_eq!(counts[0], ("grep".to_string(), 2));

    store
        .record_execution_reflection(
            id,
            &ExecutionReflectionRow {
                prompt: "build the feature".into(),
                delivered: Some(false),
                self_rating: Some(2),
                what_went_well: "explored the codebase".into(),
                what_to_improve: "actually implement".into(),
                critique: "over-explored, never wrote a line".into(),
                produced_output: false,
                wrote_files: false,
                truncated: true,
            },
        )
        .unwrap();
    assert_eq!(store.count("execution_reflection").unwrap(), 1);

    let rid = store
        .record_reflection(&ReflectionRow {
            execution_id: Some(id),
            kind: "lesson".into(),
            content: "read prod code before trusting a failing test".into(),
            domains: "[\"pipeline\"]".into(),
            occurred_at: 1_783_832_747,
        })
        .unwrap();
    assert!(rid > 0);
    assert_eq!(store.count("reflections").unwrap(), 1);
}

#[test]
fn producer_materializes_tool_calls_reflection_and_rolls_up_to_usage() {
    use stella_protocol::{ToolCall, ToolOutput};
    let store = Store::in_memory().unwrap();
    let id = store
        .begin_execution("deck", "add a feature", "zai", "glm-5.2")
        .unwrap();

    // Simulate a turn's event stream: a successful grep, a failed read, text.
    store
        .record_event(
            id,
            0,
            &AgentEvent::ToolStart {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "grep".into(),
                    input: serde_json::json!({"pattern": "foo"}),
                },
            },
        )
        .unwrap();
    store
        .record_event(
            id,
            1,
            &AgentEvent::ToolResult {
                call_id: "c1".into(),
                output: ToolOutput::Ok {
                    content: "hit\n".into(),
                },
                duration_ms: 12,
                speculated: false,
            },
        )
        .unwrap();
    store
        .record_event(
            id,
            2,
            &AgentEvent::ToolStart {
                call: ToolCall {
                    call_id: "c2".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "x"}),
                },
            },
        )
        .unwrap();
    store
        .record_event(
            id,
            3,
            &AgentEvent::ToolResult {
                call_id: "c2".into(),
                output: ToolOutput::Error {
                    message: "not found".into(),
                },
                duration_ms: 3,
                speculated: false,
            },
        )
        .unwrap();
    store
        .record_event(
            id,
            4,
            &AgentEvent::Text {
                delta: "done".into(),
            },
        )
        .unwrap();

    // Materialize the normalized tool_calls log from the events.
    let n = store.materialize_tool_calls(id).unwrap();
    assert_eq!(n, 2);
    assert_eq!(store.count("tool_calls").unwrap(), 2);

    // Objective self-reflection: produced output, wrote nothing, not truncated.
    store.finalize_execution_reflection(id).unwrap();
    let (po, wf, tr): (i64, i64, i64) = {
        let conn = store.lock();
        conn.query_row(
            "SELECT produced_output, wrote_files, truncated \
             FROM execution_reflection WHERE execution_id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    assert_eq!((po, wf, tr), (1, 0, 0));

    // Roll one turn up into the user-tier aggregate.
    let usage = crate::usage::UsageStore::in_memory().unwrap();
    let root = std::path::Path::new("/w/stella");
    assert!(
        !store.sync_to_usage(id, root, &usage).unwrap(),
        "a pending execution must not escape into usage aggregates"
    );
    store.finish_execution(id, "completed", 0.0).unwrap();
    assert!(store.sync_to_usage(id, root, &usage).unwrap());
    let pid = crate::usage::project_id_for(root);
    assert_eq!(usage.execution_count(&pid).unwrap(), 1);
    assert_eq!(
        usage
            .tool_totals()
            .unwrap()
            .iter()
            .map(|(_, c)| *c)
            .sum::<i64>(),
        2,
        "grep + read_file folded into the cross-project histogram"
    );
}

#[test]
fn v7_migration_adds_data_plane_tables_to_a_legacy_database() {
    let root = temp_root("v7_data_plane");
    {
        let conn = Connection::open(root.join(".stella/store.db")).unwrap();
        conn.execute_batch(LEGACY_V0_SCHEMA).unwrap();
    }
    let store = Store::open(&root).unwrap();
    assert_eq!(user_version(&store), SCHEMA_VERSION);
    assert_eq!(store.count("tool_calls").unwrap(), 0);
    assert_eq!(store.count("execution_reflection").unwrap(), 0);
    assert_eq!(store.count("reflections").unwrap(), 0);
    // New write paths work on the migrated file.
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_reflection(&ReflectionRow {
            execution_id: Some(id),
            kind: "lesson".into(),
            content: "x".into(),
            domains: "[]".into(),
            occurred_at: 1,
        })
        .unwrap();
    assert_eq!(store.count("reflections").unwrap(), 1);
    drop(store);
    std::fs::remove_dir_all(&root).ok();
}

/// A task-board item with just the fields the mirror stores.
fn task(id: &str, subject: &str, status: TaskStatus, owner: Option<&str>) -> TaskItem {
    TaskItem {
        id: id.into(),
        subject: subject.into(),
        description: None,
        status,
        owner: owner.map(str::to_string),
    }
}

#[test]
fn v8_migration_adds_the_session_plane_to_a_legacy_database() {
    // The chain's final step is v7 → v8: a legacy file upgraded through
    // the whole migration list must end at user_version 8 with
    // executions.session_id, its by-session index, and the tasks /
    // pull_requests tables present and writable.
    let root = temp_root("v8_session_plane");
    {
        let conn = Connection::open(root.join(".stella/store.db")).unwrap();
        conn.execute_batch(LEGACY_V0_SCHEMA).unwrap();
    }
    let store = Store::open(&root).unwrap();
    assert_eq!(user_version(&store), SCHEMA_VERSION);
    assert_eq!(store.count("tasks").unwrap(), 0);
    assert_eq!(store.count("pull_requests").unwrap(), 0);

    // The migrated executions table took the session link and its index.
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store.set_execution_session(id, "ses-8-1").unwrap();
    {
        let conn = store.lock();
        let session: Option<String> = conn
            .query_row(
                "SELECT session_id FROM executions WHERE id = ?",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(session.as_deref(), Some("ses-8-1"));
        let index_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'executions_by_session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);
    }
    // The new write paths work on the migrated file.
    store
        .record_task_board(
            id,
            Some("ses-8-1"),
            &[task("1", "t", TaskStatus::Pending, None)],
            1,
        )
        .unwrap();
    store
        .upsert_pull_request(
            Some("ses-8-1"),
            "https://example.com/pr/1",
            Some(1),
            "open",
            None,
            1,
        )
        .unwrap();
    assert_eq!(store.count("tasks").unwrap(), 1);
    assert_eq!(store.count("pull_requests").unwrap(), 1);
    drop(store);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn task_board_upserts_the_latest_snapshot_per_session() {
    let store = Store::in_memory().unwrap();
    let turn_one = store
        .begin_execution("deck", "plan", "zai", "glm-5.2")
        .unwrap();
    store.set_execution_session(turn_one, "ses-1-9").unwrap();
    store
        .record_task_board(
            turn_one,
            Some("ses-1-9"),
            &[
                task("1", "read the code", TaskStatus::InProgress, Some("lead")),
                task("2", "write the tests", TaskStatus::Pending, None),
                task("10", "ship it", TaskStatus::Pending, None),
            ],
            1_000,
        )
        .unwrap();

    // A later snapshot from a later turn: task 1 done, task 2 claimed
    // and elaborated — rows are REPLACED, never appended.
    let turn_two = store
        .begin_execution("deck", "do", "zai", "glm-5.2")
        .unwrap();
    store.set_execution_session(turn_two, "ses-1-9").unwrap();
    let mut claimed = task(
        "2",
        "write the tests",
        TaskStatus::InProgress,
        Some("worker-1"),
    );
    claimed.description = Some("unit tests for the board mirror".into());
    store
        .record_task_board(
            turn_two,
            Some("ses-1-9"),
            &[
                task("1", "read the code", TaskStatus::Completed, Some("lead")),
                claimed,
                task("10", "ship it", TaskStatus::Pending, None),
            ],
            2_000,
        )
        .unwrap();
    assert_eq!(store.count("tasks").unwrap(), 3, "upsert, not append");

    let board = store.list_session_tasks("ses-1-9").unwrap();
    assert_eq!(
        board.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        ["1", "2", "10"],
        "numeric task-id order, not lexicographic (10 after 2)"
    );
    assert_eq!(board[0].status, TaskStatus::Completed);
    assert_eq!(board[1].status, TaskStatus::InProgress);
    assert_eq!(board[1].owner.as_deref(), Some("worker-1"));
    assert_eq!(
        board[1].description.as_deref(),
        Some("unit tests for the board mirror")
    );

    // The stored status is the protocol's serde snake_case token, and
    // the newest snapshot's timestamp won the upsert.
    {
        let conn = store.lock();
        let (status, updated_at): (String, i64) = conn
            .query_row(
                "SELECT status, updated_at FROM tasks \
                 WHERE session_id = 'ses-1-9' AND task_id = '2'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "in_progress");
        assert_eq!(updated_at, 2_000);
    }
    // Another session's board is invisible here.
    assert!(store.list_session_tasks("ses-other").unwrap().is_empty());
}

#[test]
fn pull_request_upsert_is_keyed_by_url() {
    let store = Store::in_memory().unwrap();
    let url = "https://github.com/o/r/pull/7";
    store
        .upsert_pull_request(Some("ses-1"), url, Some(7), "open", None, 1_000)
        .unwrap();
    // A later observation of the SAME url updates status/CI in place —
    // and a session-less update must not erase the stored session link.
    store
        .upsert_pull_request(None, url, Some(7), "merged", Some("passing"), 2_000)
        .unwrap();
    store
        .upsert_pull_request(
            Some("ses-2"),
            "https://github.com/o/r/pull/8",
            None,
            "open",
            Some("running"),
            3_000,
        )
        .unwrap();
    assert_eq!(store.count("pull_requests").unwrap(), 2, "upsert by url");

    let all = store.list_pull_requests(None).unwrap();
    assert_eq!(
        all.iter().map(|p| p.url.as_str()).collect::<Vec<_>>(),
        ["https://github.com/o/r/pull/8", url],
        "freshest first"
    );
    let seven = &all[1];
    assert_eq!(seven.number, Some(7));
    assert_eq!(seven.status, "merged");
    assert_eq!(seven.ci_status.as_deref(), Some("passing"));
    assert_eq!(seven.updated_at, 2_000);
    assert_eq!(
        seven.session_id.as_deref(),
        Some("ses-1"),
        "COALESCE keeps the session link across a session-less update"
    );

    let ses2 = store.list_pull_requests(Some("ses-2")).unwrap();
    assert_eq!(ses2.len(), 1);
    assert_eq!(ses2[0].number, None);
}

#[test]
fn session_events_reassembles_the_journal_and_skips_corrupt_payloads() {
    let store = Store::in_memory().unwrap();
    let turn_one = store
        .begin_execution("run", "one", "zai", "glm-5.2")
        .unwrap();
    store.set_execution_session(turn_one, "ses-j").unwrap();
    store
        .record_event(
            turn_one,
            0,
            &AgentEvent::Reasoning {
                delta: "think".into(),
            },
        )
        .unwrap();
    store
        .record_event(turn_one, 1, &AgentEvent::Text { delta: "a".into() })
        .unwrap();
    let turn_two = store
        .begin_execution("run", "two", "zai", "glm-5.2")
        .unwrap();
    store.set_execution_session(turn_two, "ses-j").unwrap();
    store
        .record_event(turn_two, 0, &AgentEvent::Text { delta: "b".into() })
        .unwrap();
    // Another session's execution stays out of this journal.
    let elsewhere = store.begin_execution("run", "x", "zai", "glm-5.2").unwrap();
    store.set_execution_session(elsewhere, "ses-other").unwrap();
    store
        .record_event(elsewhere, 0, &AgentEvent::Text { delta: "z".into() })
        .unwrap();
    // A payload whose variant this build no longer knows — inserted raw,
    // exactly as an older stream would have left it on disk.
    {
        let conn = store.lock();
        conn.execute(
            "INSERT INTO events (execution_id, seq, event_type, payload) \
             VALUES (?, 2, 'ghost', '{\"type\":\"ghost\",\"volume\":11}')",
            params![turn_one],
        )
        .unwrap();
    }

    let journal = store.session_events("ses-j").unwrap();
    assert_eq!(
        journal.skipped, 1,
        "an unparseable row is counted, never fatal"
    );
    assert_eq!(
        journal
            .events
            .iter()
            .map(|r| (r.execution_id, r.seq))
            .collect::<Vec<_>>(),
        vec![(turn_one, 0), (turn_one, 1), (turn_two, 0)],
        "ordered by (execution_id, seq) across the session's turns"
    );
    match &journal.events[0].event {
        AgentEvent::Reasoning { delta } => assert_eq!(delta, "think"),
        other => panic!("unexpected first event: {other:?}"),
    }
    let empty = store.session_events("ses-unknown").unwrap();
    assert!(empty.events.is_empty());
    assert_eq!(empty.skipped, 0);
}

#[test]
fn v2_migration_rebuilds_files_touched_with_dedupe_and_backfill() {
    let root = temp_root("v2_files_touched");
    {
        let conn = Connection::open(root.join(".stella/store.db")).unwrap();
        conn.execute_batch(LEGACY_V0_SCHEMA).unwrap();
        // Historic double-write of the same path the v0/v1 shapes
        // accepted — the newest row must survive the rebuild.
        conn.execute_batch(
            "INSERT INTO executions (kind, prompt, provider, model)
               VALUES ('run', 'p', 'zai', 'glm-5.2');
             INSERT INTO files_touched (execution_id, path, ops) VALUES (1, 'src/a.rs', 'R');
             INSERT INTO files_touched (execution_id, path, ops) VALUES (1, 'src/a.rs', 'RU');
             INSERT INTO files_touched (execution_id, path, ops) VALUES (1, 'src/b.rs', 'D');",
        )
        .unwrap();
    }

    let store = Store::open(&root).unwrap();
    assert_eq!(user_version(&store), SCHEMA_VERSION);
    assert_eq!(store.count("files_touched").unwrap(), 2);
    {
        let conn = store.lock();
        let (ops, added, removed, events): (String, i64, i64, String) = conn
            .query_row(
                "SELECT ops, lines_added, lines_removed, events FROM files_touched \
                 WHERE execution_id = 1 AND path = 'src/a.rs'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(ops, "RU", "newest row per (execution_id, path) survives");
        assert_eq!((added, removed), (0, 0), "legacy rows backfill zero deltas");
        assert_eq!(events, "[]", "legacy rows backfill an empty audit log");
    }
    // The retrofitted key holds and new-shape writes work.
    assert!(
        store
            .record_files_touched(1, &[touch_row("src/a.rs", "R", 0, 0)])
            .is_err()
    );
    store
        .record_files_touched(1, &[touch_row("src/new.rs", "C", 7, 0)])
        .unwrap();
    drop(store);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn rules_upsert_list_delete_roundtrip() {
    let store = Store::in_memory().unwrap();
    store
        .upsert_rule("no-force-push", "Never force-push.", "ext:policy")
        .unwrap();
    store
        .upsert_rule("a-first", "Sort me first.", "ext:policy")
        .unwrap();
    // Re-publishing an id replaces contents and source, never duplicates.
    store
        .upsert_rule(
            "no-force-push",
            "---\nguard-tool: Bash\nguard-deny-command: git push --force*\n---\nNever force-push.",
            "ext:policy-v2",
        )
        .unwrap();

    let rules = store.list_rules().unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].rule_id, "a-first", "ordered by rule id");
    assert_eq!(rules[1].source, "ext:policy-v2");
    assert!(rules[1].contents.contains("guard-tool: Bash"));

    assert!(store.delete_rule("a-first").unwrap());
    assert!(
        !store.delete_rule("a-first").unwrap(),
        "a second delete reports no row"
    );
    assert_eq!(store.count("rules").unwrap(), 1);
}

#[test]
fn v3_migration_adds_the_rules_table_to_a_legacy_file() {
    // A legacy file upgraded through the whole migration chain must end
    // at SCHEMA_VERSION with the rules table present and writable.
    let root = temp_root("v3_rules");
    {
        let conn = Connection::open(root.join(".stella/store.db")).unwrap();
        conn.execute_batch(LEGACY_V0_SCHEMA).unwrap();
    }
    let store = Store::open(&root).unwrap();
    assert_eq!(user_version(&store), SCHEMA_VERSION);
    store.upsert_rule("r", "rule text", "ext").unwrap();
    assert_eq!(store.count("rules").unwrap(), 1);
    drop(store);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn agent_uses_log_one_row_per_invocation_never_aggregated() {
    let store = Store::in_memory().unwrap();
    let id = store
        .begin_execution("deck", "p", "zai", "glm-5.2")
        .unwrap();
    store
        .record_agent_uses(
            id,
            &[
                AgentUseRow {
                    agent: "reviewer".into(),
                    version: 2,
                    reason: "review the diff".into(),
                },
                // The SAME agent-version again in the same execution: a
                // second real invocation, a second row — the log carries
                // no UNIQUE key by design.
                AgentUseRow {
                    agent: "reviewer".into(),
                    version: 2,
                    reason: "second pass".into(),
                },
            ],
        )
        .unwrap();
    assert_eq!(store.count("agent_uses").unwrap(), 2);
    let conn = store.lock();
    let (agent, version, reason, ts): (String, i64, String, String) = conn
        .query_row(
            "SELECT agent, version, reason, ts FROM agent_uses \
             WHERE execution_id = ? ORDER BY rowid LIMIT 1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((agent.as_str(), version), ("reviewer", 2));
    assert_eq!(reason, "review the diff");
    assert!(!ts.is_empty(), "the insert stamps a timestamp");
}

#[test]
fn skill_usage_records_per_execution_version_rows() {
    let store = Store::in_memory().unwrap();
    assert_eq!(user_version(&store), SCHEMA_VERSION);
    // skill_usage lands at v5; mcp_usage takes v6; the data-plane tables
    // (tool_calls / execution_reflection / reflections) take v7; the
    // session plane (executions.session_id / tasks / pull_requests)
    // takes v8; v9 adds fail-closed call-role/completeness and v10 adds lifecycle
    // accounting for execution/telemetry rows.
    assert_eq!(SCHEMA_VERSION, 10);

    let id = store
        .begin_execution("deck", "format the sql", "zai", "glm-5.2")
        .unwrap();
    store
        .record_skill_usage(
            id,
            &[
                SkillUsageRow {
                    skill: "sql-style".into(),
                    version: 3,
                    reason: "matched: sql, format".into(),
                },
                SkillUsageRow {
                    skill: "prefer-tables".into(),
                    version: 1,
                    reason: "matched: tables".into(),
                },
            ],
        )
        .unwrap();
    assert_eq!(store.count("skill_usage").unwrap(), 2);
    let conn = store.lock();
    let (skill, version): (String, i64) = conn
        .query_row(
            "SELECT skill, version FROM skill_usage WHERE execution_id = ? \
             ORDER BY rowid LIMIT 1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((skill.as_str(), version), ("sql-style", 3));
}

#[test]
fn v4_migration_adds_agent_uses_to_a_pre_v4_file() {
    let root = temp_root("v4_agent_uses");
    {
        let conn = Connection::open(root.join(".stella/store.db")).unwrap();
        conn.execute_batch(LEGACY_V0_SCHEMA).unwrap();
    }
    let store = Store::open(&root).unwrap();
    assert_eq!(user_version(&store), SCHEMA_VERSION);
    assert_eq!(
        store.count("agent_uses").unwrap(),
        0,
        "the migrated file grew an empty agent_uses log"
    );
    let id = store
        .begin_execution("deck", "p", "zai", "glm-5.2")
        .unwrap();
    store
        .record_agent_uses(
            id,
            &[AgentUseRow {
                agent: "planner".into(),
                version: 1,
                reason: String::new(),
            }],
        )
        .unwrap();
    assert_eq!(store.count("agent_uses").unwrap(), 1);
    drop(store);
    std::fs::remove_dir_all(&root).ok();
}

/// A telemetry row shaped for drift tests — only the sample-relevant
/// fields vary.
fn drift_row(step: u64, provider: &str, model: &str, estimated: u64, actual: u64) -> TelemetryRow {
    TelemetryRow {
        step,
        provider: provider.into(),
        call_role: "worker".into(),
        model: model.into(),
        input_tokens: actual,
        estimated_input_tokens: estimated,
        output_tokens: 100,
        cache_read_tokens: 0,
        cache_miss_tokens: actual,
        cache_write_tokens: 0,
        cost_usd: 0.001,
        duration_ms: 500,
        retries: 0,
        tool_calls: 1,
        usage_complete: true,
    }
}

#[test]
fn drift_samples_roundtrip_the_estimated_column_oldest_first() {
    let store = Store::in_memory().unwrap();
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_telemetry(id, &drift_row(0, "zai", "glm-5.2", 1_000, 1_400))
        .unwrap();
    store
        .record_telemetry(id, &drift_row(1, "zai", "glm-5.2", 2_000, 2_900))
        .unwrap();

    let samples = store.drift_samples("zai", "glm-5.2", 10).unwrap();
    assert_eq!(
        samples,
        vec![(1_000, 1_400), (2_000, 2_900)],
        "oldest first — EWMA replay order"
    );
}

#[test]
fn drift_samples_are_keyed_by_provider_and_model_and_skip_signal_free_rows() {
    let store = Store::in_memory().unwrap();
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_telemetry(id, &drift_row(0, "zai", "glm-5.2", 1_000, 1_400))
        .unwrap();
    // Same model slug on a DIFFERENT provider: a different tokenizer,
    // never the same calibration.
    store
        .record_telemetry(id, &drift_row(1, "other", "glm-5.2", 1_000, 9_000))
        .unwrap();
    // Different model, same provider.
    store
        .record_telemetry(id, &drift_row(2, "zai", "glm-4", 1_000, 9_000))
        .unwrap();
    // No estimate recorded (pre-drift row) and no reported usage: both
    // signal-free.
    store
        .record_telemetry(id, &drift_row(3, "zai", "glm-5.2", 0, 1_400))
        .unwrap();
    store
        .record_telemetry(id, &drift_row(4, "zai", "glm-5.2", 1_000, 0))
        .unwrap();

    let samples = store.drift_samples("zai", "glm-5.2", 10).unwrap();
    assert_eq!(samples, vec![(1_000, 1_400)]);
}

#[test]
fn drift_samples_limit_keeps_the_most_recent_rows() {
    let store = Store::in_memory().unwrap();
    // Across two executions, so the ordering key spans sessions.
    let first = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    for step in 0..3u64 {
        store
            .record_telemetry(first, &drift_row(step, "zai", "glm-5.2", 100 + step, 200))
            .unwrap();
    }
    let second = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    for step in 0..3u64 {
        store
            .record_telemetry(second, &drift_row(step, "zai", "glm-5.2", 500 + step, 600))
            .unwrap();
    }

    let samples = store.drift_samples("zai", "glm-5.2", 4).unwrap();
    assert_eq!(
        samples,
        vec![(102, 200), (500, 600), (501, 600), (502, 600)],
        "the limit trims the OLDEST rows and keeps replay order"
    );
}

#[test]
fn migrate_adds_the_estimated_column_to_a_pre_drift_database() {
    let root = std::env::temp_dir().join(format!(
        "stella_store_migrate_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(root.join(".stella")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join(".stella"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
    }
    // Simulate a database created before drift correction: the telemetry
    // table exists WITHOUT estimated_input_tokens and already has a row.
    {
        let conn = Connection::open(root.join(".stella/store.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE telemetry (
               execution_id INTEGER NOT NULL,
               step INTEGER NOT NULL,
               ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
               provider TEXT NOT NULL,
               model TEXT NOT NULL,
               input_tokens INTEGER NOT NULL,
               output_tokens INTEGER NOT NULL,
               cache_read_tokens INTEGER NOT NULL,
               cache_miss_tokens INTEGER NOT NULL,
               cache_write_tokens INTEGER NOT NULL DEFAULT 0,
               cost_usd REAL NOT NULL,
               duration_ms INTEGER NOT NULL,
               retries INTEGER NOT NULL,
               tool_calls INTEGER NOT NULL
             );
             INSERT INTO telemetry (execution_id, step, provider, model, input_tokens,
               output_tokens, cache_read_tokens, cache_miss_tokens, cost_usd, duration_ms,
               retries, tool_calls)
             VALUES (1, 0, 'zai', 'glm-5.2', 1400, 100, 0, 1400, 0.001, 500, 0, 1);",
        )
        .unwrap();
    }
    // Store::open runs migrate() against the old schema…
    let store = Store::open(&root).unwrap();
    // …after which new-schema writes and reads work,
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_telemetry(id, &drift_row(0, "zai", "glm-5.2", 1_000, 1_400))
        .unwrap();
    // and the legacy row (estimated defaulted, no signal) is excluded.
    assert_eq!(store.count("telemetry").unwrap(), 2);
    assert_eq!(
        store.drift_samples("zai", "glm-5.2", 10).unwrap(),
        vec![(1_000, 1_400)]
    );
    drop(store);
    std::fs::remove_dir_all(&root).ok();
}

/// The COMPLETE pre-versioning (v0) schema, verbatim — what any
/// store.db written before `user_version` stamping looks like on disk:
/// no UNIQUE keys on events/telemetry, both non-unique indexes present.
/// Migration tests build their fixtures from this, never from the
/// current DDL.
const LEGACY_V0_SCHEMA: &str = "CREATE TABLE executions (
       id INTEGER PRIMARY KEY AUTOINCREMENT,
       kind TEXT NOT NULL,
       prompt TEXT NOT NULL,
       provider TEXT NOT NULL,
       model TEXT NOT NULL,
       started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
       finished_at TEXT,
       outcome TEXT,
       cost_usd REAL NOT NULL DEFAULT 0
     );
     CREATE TABLE events (
       execution_id INTEGER NOT NULL,
       seq INTEGER NOT NULL,
       ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
       event_type TEXT NOT NULL,
       payload TEXT NOT NULL
     );
     CREATE TABLE telemetry (
       execution_id INTEGER NOT NULL,
       step INTEGER NOT NULL,
       ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
       provider TEXT NOT NULL,
       model TEXT NOT NULL,
       input_tokens INTEGER NOT NULL,
       estimated_input_tokens INTEGER NOT NULL DEFAULT 0,
       output_tokens INTEGER NOT NULL,
       cache_read_tokens INTEGER NOT NULL,
       cache_miss_tokens INTEGER NOT NULL,
       cache_write_tokens INTEGER NOT NULL DEFAULT 0,
       cost_usd REAL NOT NULL,
       duration_ms INTEGER NOT NULL,
       retries INTEGER NOT NULL,
       tool_calls INTEGER NOT NULL
     );
     CREATE TABLE files_touched (
       execution_id INTEGER NOT NULL,
       path TEXT NOT NULL,
       ops TEXT NOT NULL
     );
     CREATE TABLE file_locks (
       path TEXT PRIMARY KEY,
       holder TEXT NOT NULL,
       acquired_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
     );
     CREATE TABLE graph_nodes (
       id TEXT PRIMARY KEY,
       label TEXT NOT NULL,
       properties TEXT NOT NULL DEFAULT '{}'
     );
     CREATE TABLE graph_edges (
       src TEXT NOT NULL,
       dst TEXT NOT NULL,
       edge_type TEXT NOT NULL,
       properties TEXT NOT NULL DEFAULT '{}'
     );
     CREATE INDEX telemetry_by_model
       ON telemetry(provider, model, execution_id, step);
     CREATE INDEX events_by_execution
       ON events(execution_id, seq);";

/// Unique-per-test workspace root with `.stella/` pre-created, cleaned
/// of any leftover from a previously crashed run.
fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "stella_store_{tag}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join(".stella")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join(".stella"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
    }
    root
}

fn user_version(store: &Store) -> i64 {
    store
        .lock()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn fresh_database_is_created_at_the_latest_schema_version() {
    let store = Store::in_memory().unwrap();
    assert_eq!(
        user_version(&store),
        SCHEMA_VERSION,
        "fresh files are stamped directly, no migration list"
    );

    // The fresh shape carries the UNIQUE keys the write paths assume:
    // a double-write of the same stream position / step errors instead
    // of silently corrupting replay and double-counting cost.
    let id = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
    store
        .record_event(id, 0, &AgentEvent::Text { delta: "a".into() })
        .unwrap();
    assert!(
        store
            .record_event(id, 0, &AgentEvent::Text { delta: "b".into() })
            .is_err(),
        "duplicate (execution_id, seq) must violate UNIQUE"
    );
    store
        .record_telemetry(id, &drift_row(0, "zai", "glm-5.2", 1_000, 1_400))
        .unwrap();
    assert!(
        store
            .record_telemetry(id, &drift_row(0, "zai", "glm-5.2", 2_000, 2_900))
            .is_err(),
        "duplicate (execution_id, step) must violate UNIQUE"
    );
    // Distinct positions still insert freely.
    store
        .record_event(id, 1, &AgentEvent::Text { delta: "c".into() })
        .unwrap();
    store
        .record_telemetry(id, &drift_row(1, "zai", "glm-5.2", 2_000, 2_900))
        .unwrap();
    assert_eq!(store.count("events").unwrap(), 2);
    assert_eq!(store.count("telemetry").unwrap(), 2);
}

#[test]
fn v1_migration_dedupes_a_v0_database_and_retrofits_the_unique_keys() {
    let root = temp_root("v0_dedupe");
    {
        let conn = Connection::open(root.join(".stella/store.db")).unwrap();
        conn.execute_batch(LEGACY_V0_SCHEMA).unwrap();
        // Historic double-writes the v0 schema accepted: the same
        // stream position / step recorded twice. Separate INSERTs pin
        // rowid (insertion) order — the newer row must survive.
        conn.execute_batch(
            "INSERT INTO executions (kind, prompt, provider, model)
               VALUES ('run', 'p', 'zai', 'glm-5.2');
             INSERT INTO events (execution_id, seq, event_type, payload)
               VALUES (1, 0, 'text', '{\"delta\":\"stale\"}');
             INSERT INTO events (execution_id, seq, event_type, payload)
               VALUES (1, 0, 'text', '{\"delta\":\"final\"}');
             INSERT INTO events (execution_id, seq, event_type, payload)
               VALUES (1, 1, 'text', '{\"delta\":\"tail\"}');
             INSERT INTO telemetry (execution_id, step, provider, model, input_tokens,
               estimated_input_tokens, output_tokens, cache_read_tokens,
               cache_miss_tokens, cost_usd, duration_ms, retries, tool_calls)
               VALUES (1, 0, 'zai', 'glm-5.2', 111, 100, 10, 0, 111, 0.1, 500, 0, 1);
             INSERT INTO telemetry (execution_id, step, provider, model, input_tokens,
               estimated_input_tokens, output_tokens, cache_read_tokens,
               cache_miss_tokens, cost_usd, duration_ms, retries, tool_calls)
               VALUES (1, 0, 'zai', 'glm-5.2', 222, 200, 20, 0, 222, 0.2, 600, 0, 1);
             INSERT INTO telemetry (execution_id, step, provider, model, input_tokens,
               estimated_input_tokens, output_tokens, cache_read_tokens,
               cache_miss_tokens, cost_usd, duration_ms, retries, tool_calls)
               VALUES (1, 1, 'zai', 'glm-5.2', 333, 300, 30, 0, 333, 0.3, 700, 0, 1);",
        )
        .unwrap();
    }

    let store = Store::open(&root).unwrap();
    assert_eq!(
        user_version(&store),
        SCHEMA_VERSION,
        "the migration stamps the version it produced"
    );

    // Duplicates collapsed to the NEWEST row per natural key.
    assert_eq!(store.count("events").unwrap(), 2);
    assert_eq!(store.count("telemetry").unwrap(), 2);
    {
        let conn = store.lock();
        let payload: String = conn
            .query_row(
                "SELECT payload FROM events WHERE execution_id = 1 AND seq = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            payload, "{\"delta\":\"final\"}",
            "replay position (1, 0) keeps the last write"
        );
        let input: i64 = conn
            .query_row(
                "SELECT input_tokens FROM telemetry WHERE execution_id = 1 AND step = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(input, 222, "model call (1, 0) keeps the last write");
        // The rebuild preserved drift_samples' hot-path index.
        let index_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'telemetry_by_model'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);
    }

    // The retrofitted constraints hold on the migrated tables…
    assert!(
        store
            .record_event(
                1,
                0,
                &AgentEvent::Text {
                    delta: "again".into()
                }
            )
            .is_err()
    );
    assert!(
        store
            .record_telemetry(1, &drift_row(0, "zai", "glm-5.2", 1_000, 1_400))
            .is_err()
    );
    // …while fresh positions and the normal readers keep working.
    store
        .record_event(
            1,
            2,
            &AgentEvent::Text {
                delta: "new".into(),
            },
        )
        .unwrap();
    store
        .record_telemetry(1, &drift_row(2, "zai", "glm-5.2", 400, 500))
        .unwrap();
    let completeness: Vec<(i64, bool)> = {
        let conn = store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT step, usage_complete FROM telemetry \
                 WHERE execution_id = 1 ORDER BY step",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        completeness,
        vec![(0, false), (1, false), (2, true)],
        "migrated rows fail closed while a fresh telemetry row is complete"
    );
    assert_eq!(
        store.drift_samples("zai", "glm-5.2", 10).unwrap(),
        vec![(400, 500)],
        "drift calibration excludes migrated rows whose usage is incomplete"
    );
    // A post-migration execution gets a fresh id, never execution 1's.
    assert_eq!(
        store.begin_execution("run", "p", "zai", "glm-5.2").unwrap(),
        2
    );

    // Reopening an already-migrated file is a no-op — nothing
    // re-collapsed, version unchanged.
    drop(store);
    let store = Store::open(&root).unwrap();
    assert_eq!(user_version(&store), SCHEMA_VERSION);
    assert_eq!(store.count("events").unwrap(), 3);
    assert_eq!(store.count("telemetry").unwrap(), 3);
    drop(store);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn v1_migration_seeds_the_execution_counter_past_orphaned_history() {
    let root = temp_root("v0_orphans");
    {
        // A partial v0 file: telemetry references executions 1..=3 but
        // the executions table never existed, so its fresh AUTOINCREMENT
        // counter would restart at 1 and mis-attribute the orphans.
        let conn = Connection::open(root.join(".stella/store.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE telemetry (
               execution_id INTEGER NOT NULL,
               step INTEGER NOT NULL,
               ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
               provider TEXT NOT NULL,
               model TEXT NOT NULL,
               input_tokens INTEGER NOT NULL,
               estimated_input_tokens INTEGER NOT NULL DEFAULT 0,
               output_tokens INTEGER NOT NULL,
               cache_read_tokens INTEGER NOT NULL,
               cache_miss_tokens INTEGER NOT NULL,
               cache_write_tokens INTEGER NOT NULL DEFAULT 0,
               cost_usd REAL NOT NULL,
               duration_ms INTEGER NOT NULL,
               retries INTEGER NOT NULL,
               tool_calls INTEGER NOT NULL
             );
             INSERT INTO telemetry (execution_id, step, provider, model, input_tokens,
               output_tokens, cache_read_tokens, cache_miss_tokens, cost_usd,
               duration_ms, retries, tool_calls)
               VALUES (3, 0, 'zai', 'glm-5.2', 100, 10, 0, 100, 0.1, 500, 0, 1);",
        )
        .unwrap();
    }
    let store = Store::open(&root).unwrap();
    assert_eq!(
        store.begin_execution("run", "p", "zai", "glm-5.2").unwrap(),
        4,
        "new executions must never reuse a historically referenced id"
    );
    // In particular, step 0 of the new execution cannot collide with
    // orphaned telemetry under the retrofitted UNIQUE key.
    store
        .record_telemetry(4, &drift_row(0, "zai", "glm-5.2", 1_000, 1_400))
        .unwrap();
    drop(store);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn refuses_a_database_stamped_by_a_newer_build() {
    let root = temp_root("newer_version");
    {
        let conn = Connection::open(root.join(".stella/store.db")).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
    }
    let err = match Store::open(&root) {
        Ok(_) => panic!("a newer-versioned file must refuse to open"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("schema version"),
        "downgrade must refuse, not silently write into a newer shape: {err}"
    );
    // The real cause is a stale binary, not a corrupt workspace — the
    // message must name that and point at an upgrade path, not just
    // refuse (#252).
    assert!(
        msg.contains("out of date"),
        "message must name the stale binary as the cause, not just refuse: {err}"
    );
    assert!(
        msg.contains("brew upgrade stella")
            && msg.contains("install.sh")
            && msg.contains("github.com/macanderson/stella/releases"),
        "message must name every supported upgrade path: {err}"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn file_locks_are_exclusive_and_reentrant() {
    let store = Store::in_memory().unwrap();
    assert!(store.acquire_file_lock("src/a.rs", "agent-1").unwrap());
    assert!(
        store.acquire_file_lock("src/a.rs", "agent-1").unwrap(),
        "re-entrant"
    );
    assert!(
        !store.acquire_file_lock("src/a.rs", "agent-2").unwrap(),
        "exclusive"
    );

    // Only the holder's release frees it.
    store.release_file_lock("src/a.rs", "agent-2").unwrap();
    assert!(!store.acquire_file_lock("src/a.rs", "agent-2").unwrap());
    store.release_file_lock("src/a.rs", "agent-1").unwrap();
    assert!(store.acquire_file_lock("src/a.rs", "agent-2").unwrap());
}

#[test]
fn file_lock_holder_names_the_current_holder() {
    let store = Store::in_memory().unwrap();
    assert_eq!(store.file_lock_holder("src/a.rs").unwrap(), None);
    store.acquire_file_lock("src/a.rs", "agent-1").unwrap();
    assert_eq!(
        store.file_lock_holder("src/a.rs").unwrap(),
        Some("agent-1".to_string()),
        "a loser's conflict error must be able to name the winner"
    );
    store.release_file_lock("src/a.rs", "agent-1").unwrap();
    assert_eq!(store.file_lock_holder("src/a.rs").unwrap(), None);
}

#[test]
fn holder_wide_release_drops_only_that_holders_claims() {
    let store = Store::in_memory().unwrap();
    store.acquire_file_lock("src/a.rs", "run-1/t1").unwrap();
    store.acquire_file_lock("src/b.rs", "run-1/t1").unwrap();
    store.acquire_file_lock("src/c.rs", "run-2/t9").unwrap();
    assert_eq!(store.release_file_locks_for_holder("run-1/t1").unwrap(), 2);
    assert!(
        store.acquire_file_lock("src/a.rs", "run-2/t9").unwrap(),
        "released paths are claimable again"
    );
    assert_eq!(
        store.file_lock_holder("src/c.rs").unwrap(),
        Some("run-2/t9".to_string()),
        "the other holder's claim survives"
    );
}

#[test]
fn stale_lock_sweep_releases_old_claims_only() {
    let store = Store::in_memory().unwrap();
    store.acquire_file_lock("src/fresh.rs", "live").unwrap();
    // A crashed process's leftover: backdate the claim past the sweep age.
    store
        .lock()
        .execute(
            "INSERT INTO file_locks (path, holder, acquired_at) \
             VALUES ('src/stale.rs', 'dead', datetime('now', '-2 hours'))",
            [],
        )
        .unwrap();
    assert_eq!(store.prune_stale_file_locks(3600).unwrap(), 1);
    assert!(
        store.acquire_file_lock("src/stale.rs", "live").unwrap(),
        "the swept path is claimable"
    );
    assert_eq!(
        store.file_lock_holder("src/fresh.rs").unwrap(),
        Some("live".to_string()),
        "fresh claims survive the sweep"
    );
}

#[test]
fn graph_seam_upserts_nodes_and_edges() {
    let store = Store::in_memory().unwrap();
    store
        .upsert_graph_node("doc:readme", "Document", r#"{"path":"README.md"}"#)
        .unwrap();
    store
        .upsert_graph_node("doc:readme", "Document", r#"{"path":"README.md","v":2}"#)
        .unwrap();
    store
        .insert_graph_edge("doc:readme", "sym:main", "mentions", "{}")
        .unwrap();
    assert_eq!(
        store.count("graph_nodes").unwrap(),
        1,
        "upsert, not duplicate"
    );
    assert_eq!(store.count("graph_edges").unwrap(), 1);
}

#[test]
fn graph_seam_rejects_non_json_properties_and_persists_valid_ones() {
    let store = Store::in_memory().unwrap();

    // Malformed JSON is refused at the seam by BOTH write methods — the
    // invariant the JSON-typed DuckDB column used to enforce — so nothing
    // unparseable lands in the plain SQLite TEXT column.
    assert!(
        store
            .upsert_graph_node("doc:readme", "Document", "not json")
            .is_err(),
        "node upsert must reject non-JSON properties"
    );
    assert!(
        store
            .insert_graph_edge("doc:readme", "sym:main", "mentions", "{oops")
            .is_err(),
        "edge insert must reject non-JSON properties"
    );
    assert_eq!(
        store.count("graph_nodes").unwrap(),
        0,
        "a rejected node must not be written"
    );
    assert_eq!(
        store.count("graph_edges").unwrap(),
        0,
        "a rejected edge must not be written"
    );

    // Valid JSON — including the empty default and a caller-supplied
    // object — is accepted and round-trips out of the column intact.
    store
        .upsert_graph_node("doc:readme", "Document", r#"{"path":"README.md"}"#)
        .unwrap();
    store
        .insert_graph_edge("doc:readme", "sym:main", "mentions", "{}")
        .unwrap();
    assert_eq!(store.count("graph_nodes").unwrap(), 1);
    assert_eq!(store.count("graph_edges").unwrap(), 1);

    let conn = store.lock();
    let node_props: String = conn
        .query_row(
            "SELECT properties FROM graph_nodes WHERE id = ?",
            params!["doc:readme"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(node_props, r#"{"path":"README.md"}"#);
    let edge_props: String = conn
        .query_row(
            "SELECT properties FROM graph_edges WHERE src = ? AND dst = ?",
            params!["doc:readme", "sym:main"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(edge_props, "{}");
}

/// Test-only shorthand: a telemetry row with just the analytics-relevant
/// fields set.
#[allow(clippy::too_many_arguments)]
fn telemetry(
    step: u64,
    provider: &str,
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    duration_ms: u64,
) -> TelemetryRow {
    TelemetryRow {
        step,
        provider: provider.into(),
        call_role: "worker".into(),
        model: model.into(),
        input_tokens: input,
        // This fixture predates drift correction and exercises
        // usage_stats, which ignores the estimate; 0 = "no estimate
        // taken".
        estimated_input_tokens: 0,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_miss_tokens: input.saturating_sub(cache_read),
        cache_write_tokens: cache_write,
        cost_usd: cost,
        duration_ms,
        retries: 0,
        tool_calls: 0,
        usage_complete: true,
    }
}

/// Fixture: three providers with mixed outcomes.
/// - anthropic: 1 aborted run, cost 0.05 → resolved = 0.
/// - zai: 2 completed (0.02 + 0.01), 1 aborted, 1 never finished.
/// - local: 1 completed at $0 → the off-grid cost tier.
fn seeded_store() -> Store {
    let store = Store::in_memory().unwrap();

    let a = store
        .begin_execution("run", "p1", "anthropic", "claude-fable-5")
        .unwrap();
    store.finish_execution(a, "aborted", 0.05).unwrap();

    let z1 = store
        .begin_execution("run", "p2", "zai", "glm-5.2")
        .unwrap();
    store
        .record_telemetry(
            z1,
            &telemetry(0, "zai", "glm-5.2", 1000, 100, 500, 10, 0.01, 1000),
        )
        .unwrap();
    store
        .record_telemetry(
            z1,
            &telemetry(1, "zai", "glm-5.2", 2000, 200, 500, 0, 0.01, 500),
        )
        .unwrap();
    store.finish_execution(z1, "completed", 0.02).unwrap();

    let z2 = store
        .begin_execution("run", "p3", "zai", "glm-5.2")
        .unwrap();
    store
        .record_telemetry(
            z2,
            &telemetry(0, "zai", "glm-5.2", 3000, 300, 1000, 0, 0.01, 1500),
        )
        .unwrap();
    store.finish_execution(z2, "completed", 0.01).unwrap();

    // Aborted with no telemetry (LEFT JOIN's zero path) and a run that
    // never finished (outcome NULL) — both count as runs, not resolved.
    let z3 = store
        .begin_execution("run", "p4", "zai", "glm-5.2")
        .unwrap();
    store.finish_execution(z3, "aborted", 0.0).unwrap();
    store
        .begin_execution("run", "p5", "zai", "glm-5.2")
        .unwrap();

    let l = store
        .begin_execution("run", "p6", "local", "llama-3.3")
        .unwrap();
    store
        .record_telemetry(
            l,
            &telemetry(0, "local", "llama-3.3", 500, 50, 0, 0, 0.0, 2000),
        )
        .unwrap();
    store.finish_execution(l, "completed", 0.0).unwrap();

    store
}

#[test]
fn usage_stats_aggregates_per_provider_model() {
    let store = seeded_store();
    let rows = store.usage_stats().unwrap();
    assert_eq!(rows.len(), 3);

    // Ordered by total cost desc: anthropic 0.05, zai 0.03, local 0.0.
    assert_eq!(
        rows.iter().map(|r| r.provider.as_str()).collect::<Vec<_>>(),
        ["anthropic", "zai", "local"]
    );

    let zai = &rows[1];
    assert_eq!(zai.model, "glm-5.2");
    assert_eq!(zai.division, "-");
    assert_eq!(zai.runs, 4);
    assert_eq!(zai.resolved, 2);
    assert!((zai.resolve_rate - 0.5).abs() < 1e-12);
    assert!((zai.total_cost_usd - 0.03).abs() < 1e-12);
    assert!((zai.cost_per_resolved_usd.unwrap() - 0.015).abs() < 1e-12);
    assert_eq!(zai.input_tokens, 6000);
    assert_eq!(zai.output_tokens, 600);
    assert_eq!(zai.cache_read_tokens, 2000);
    assert_eq!(zai.cache_write_tokens, 10);
    assert!((zai.avg_duration_ms - 750.0).abs() < 1e-12);
}

#[test]
fn usage_stats_never_divides_by_zero_resolved() {
    let store = seeded_store();
    let rows = store.usage_stats().unwrap();
    let anthropic = &rows[0];
    assert_eq!(anthropic.runs, 1);
    assert_eq!(anthropic.resolved, 0);
    assert_eq!(anthropic.resolve_rate, 0.0);
    assert!((anthropic.total_cost_usd - 0.05).abs() < 1e-12);
    assert_eq!(
        anthropic.cost_per_resolved_usd, None,
        "resolved = 0 must yield None, never a fake number"
    );
    // No telemetry rows at all → token/duration sums are zero.
    assert_eq!(anthropic.input_tokens, 0);
    assert_eq!(anthropic.avg_duration_ms, 0.0);
}

#[test]
fn usage_stats_maps_local_provider_to_off_grid_division() {
    let store = seeded_store();
    let rows = store.usage_stats().unwrap();
    let local = &rows[2];
    assert_eq!(local.provider, "local");
    assert_eq!(local.division, "off-grid");
    assert_eq!(local.resolve_rate, 1.0);
    assert_eq!(local.cost_per_resolved_usd, Some(0.0));
    assert_eq!(UsageStatsRow::division_for_provider("local"), "off-grid");
    assert_eq!(UsageStatsRow::division_for_provider("anthropic"), "-");
    assert_eq!(UsageStatsRow::division_for_provider("openrouter"), "-");
}

#[test]
fn usage_stats_empty_store_returns_no_rows() {
    let store = Store::in_memory().unwrap();
    assert!(store.usage_stats().unwrap().is_empty());
}

#[test]
fn usage_stats_row_serializes_with_stable_field_order() {
    let row = UsageStatsRow {
        provider: "anthropic".into(),
        model: "claude-fable-5".into(),
        division: "-".into(),
        runs: 1,
        resolved: 0,
        resolve_rate: 0.0,
        total_cost_usd: 0.05,
        cost_per_resolved_usd: None,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        avg_duration_ms: 0.0,
    };
    // Exact string: field ORDER is the machine contract for json/csv
    // receipts, and resolved = 0 must serialize as null (not 0 or NaN).
    assert_eq!(
        serde_json::to_string(&row).unwrap(),
        r#"{"provider":"anthropic","model":"claude-fable-5","division":"-","runs":1,"resolved":0,"resolve_rate":0.0,"total_cost_usd":0.05,"cost_per_resolved_usd":null,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"avg_duration_ms":0.0}"#
    );
}

#[test]
fn count_rejects_unknown_tables() {
    let store = Store::in_memory().unwrap();
    assert!(store.count("users; DROP TABLE executions").is_err());
}

#[test]
fn on_disk_store_persists_across_reopen() {
    let root = std::env::temp_dir().join(format!("stella_store_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    {
        let store = Store::open(&root).unwrap();
        store
            .begin_execution("run", "hello", "anthropic", "claude-fable-5")
            .unwrap();
    }
    {
        let store = Store::open(&root).unwrap();
        assert_eq!(store.count("executions").unwrap(), 1);
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn open_hardens_a_fresh_dot_stella_dir() {
    let root = std::env::temp_dir().join(format!("stella_harden_{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();
    let _store = Store::open(&root).unwrap();
    let dir = root.join(".stella");

    // Generated artifacts (transcript DBs, WAL siblings, reflections log)
    // must be gitignored so session transcripts can't be committed by
    // accident; user-authored files must NOT be (no bare `*`).
    let gitignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
    assert!(gitignore.contains("*.db"));
    assert!(gitignore.contains("reflections.jsonl"));
    assert!(gitignore.lines().any(|line| line.trim() == "private/"));
    assert!(!gitignore.lines().any(|l| l.trim() == "*"));

    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .status()
        .unwrap();
    let ignored = std::process::Command::new("git")
        .args(["check-ignore", ".stella/private/mcp_oauth.json"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        ignored.status.success(),
        "private OAuth tokens must never stage"
    );

    // A pre-existing customized ignore keeps its contents and gains the
    // private-state rule exactly once.
    std::fs::write(dir.join(".gitignore"), "custom\n").unwrap();
    drop(Store::open(&root).unwrap());
    assert_eq!(
        std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
        "custom\nprivate/\n"
    );
    drop(Store::open(&root).unwrap());
    assert_eq!(
        std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
        "custom\nprivate/\n"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "fresh .stella must be owner-only");
    }

    std::fs::remove_dir_all(&root).ok();
}
