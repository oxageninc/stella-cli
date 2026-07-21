use stella_protocol::AgentEvent;
use stella_store::enterprise_telemetry::{
    EnqueueOutcome, EnterpriseTelemetrySpool, ManagedModelDimension, OperationalEventContext,
    OperationalIdentity, SpoolLimits, StellaOperationalEventV1, load_or_create_installation_uuid,
};
use stella_store::usage::ExecutionRollupRow;
use stella_store::{FileTouchRow, Store, TelemetryRow};

fn rollup(execution_id: i64) -> ExecutionRollupRow {
    ExecutionRollupRow {
        project_id: "local-project-hash-must-not-escape".into(),
        project_name: "secret-project-name".into(),
        project_root: "/secret/source/path".into(),
        execution_id,
        kind: "run".into(),
        prompt_digest: "secret-prompt-digest".into(),
        prompt_preview: "secret prompt source args results reasoning errors git memory rules"
            .into(),
        model: "anthropic/claude-sonnet-4".into(),
        provider: "anthropic".into(),
        outcome: "completed".into(),
        cost_usd: 0.125,
        input_tokens: 11,
        output_tokens: 7,
        duration_ms: 42,
        tool_calls: 3,
        files_written: 2,
        produced_output: true,
        self_rating: Some(5),
        started_at: "2026-07-21 12:00:00".into(),
        day: "2026-07-21".into(),
        tool_histogram: Vec::new(),
    }
}

fn context() -> OperationalEventContext {
    OperationalEventContext::new(
        "enroll_01",
        "org_01",
        "workspace_01",
        OperationalIdentity::new(
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        )
        .unwrap(),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        [ManagedModelDimension::new("anthropic", "anthropic/claude-sonnet-4").unwrap()],
    )
    .unwrap()
}

const SINK_A: &str = "sink_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SINK_B: &str = "sink_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn sqlite_integer_writes_reject_u64_overflow() {
    let store = Store::in_memory().unwrap();
    let id = store
        .begin_execution("run", "overflow", "zai", "glm")
        .unwrap();
    assert!(
        store
            .record_event(id, u64::MAX, &AgentEvent::Text { delta: "x".into() })
            .is_err()
    );
    let telemetry = TelemetryRow {
        step: 0,
        provider: "zai".into(),
        model: "glm".into(),
        input_tokens: u64::MAX,
        estimated_input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_miss_tokens: 0,
        cache_write_tokens: 0,
        cost_usd: 0.0,
        duration_ms: 0,
        retries: 0,
        tool_calls: 0,
    };
    assert!(store.record_telemetry(id, &telemetry).is_err());
    assert!(
        store
            .record_files_touched(
                id,
                &[FileTouchRow {
                    path: "x".into(),
                    ops: "U".into(),
                    lines_added: u64::MAX,
                    lines_removed: 0,
                    events_json: "[]".into(),
                }]
            )
            .is_err()
    );
}

#[test]
fn event_is_deterministic_and_serializes_only_content_free_fields() {
    let a = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(7)).unwrap();
    let b = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(7)).unwrap();
    let different =
        StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(8)).unwrap();

    assert_eq!(a.event_id(), b.event_id());
    assert_ne!(a.event_id(), different.event_id());

    let json = serde_json::to_string(&a).unwrap();
    for forbidden in [
        "secret",
        "source",
        "args",
        "results",
        "reasoning",
        "errors",
        "git",
        "memory",
        "rules",
        "local-project-hash",
        "execution_id",
        "project_id",
        "prompt",
        "path",
    ] {
        assert!(!json.contains(forbidden), "leaked {forbidden}: {json}");
    }
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["schema"], "stella.operational.v1");
    assert_eq!(value["event_class"], "execution_rollup");
    assert_eq!(value["cost_microusd"], 125_000);
    assert_eq!(value["changed_file_count"], 2);
    assert_eq!(value["provider"], "anthropic");
    assert_eq!(value["model"], "anthropic/claude-sonnet-4");

    let mut unknown = value.clone();
    unknown["prompt"] = serde_json::json!("forbidden");
    assert!(serde_json::from_value::<StellaOperationalEventV1>(unknown).is_err());
    let mut invalid_provider = value;
    invalid_provider["provider"] = serde_json::json!("evil/path");
    assert!(serde_json::from_value::<StellaOperationalEventV1>(invalid_provider).is_err());
    let mut invalid_id: serde_json::Value = serde_json::from_str(&json).unwrap();
    invalid_id["event_id"] = serde_json::json!("local-execution-7");
    assert!(serde_json::from_value::<StellaOperationalEventV1>(invalid_id).is_err());
}

#[test]
fn event_rejects_unfinished_or_unbounded_rollups() {
    let mut unfinished = rollup(1);
    unfinished.outcome.clear();
    assert!(StellaOperationalEventV1::from_finalized_rollup(&context(), &unfinished).is_err());

    let invalid = OperationalEventContext::new(
        "enroll 01",
        "org_01",
        "workspace_01",
        OperationalIdentity::new(
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        )
        .unwrap(),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        [],
    );
    assert!(invalid.is_err());

    let mut path_like_model = rollup(2);
    path_like_model.model = "../../secret/model".into();
    let event =
        StellaOperationalEventV1::from_finalized_rollup(&context(), &path_like_model).unwrap();
    assert_eq!(serde_json::to_value(event).unwrap()["model"], "other");

    let mut rounded_upper_edge = rollup(3);
    rounded_upper_edge.cost_usd = (u64::MAX as f64) / 1_000_000.0;
    assert!(
        StellaOperationalEventV1::from_finalized_rollup(&context(), &rounded_upper_edge).is_err(),
        "the f64 value equal to the rounded u64 upper boundary must be rejected before cast"
    );
}

#[test]
fn every_runtime_terminal_outcome_has_a_closed_operational_variant() {
    for outcome in [
        "completed",
        "error",
        "failed",
        "aborted",
        "cancelled",
        "indeterminate",
        "verification_failed",
        "goal_met",
        "goal_unmet",
    ] {
        let mut row = rollup(11);
        row.outcome = outcome.to_string();
        let event = StellaOperationalEventV1::from_finalized_rollup(&context(), &row)
            .unwrap_or_else(|error| panic!("terminal outcome {outcome} rejected: {error}"));
        assert_eq!(serde_json::to_value(event).unwrap()["outcome"], outcome);
    }
}

#[test]
fn event_ids_are_domain_separated_framed_and_bound_to_host_and_store() {
    let identity_a = OperationalIdentity::new(
        "11111111-1111-4111-8111-111111111111",
        "22222222-2222-4222-8222-222222222222",
    )
    .unwrap();
    let identity_b = OperationalIdentity::new(
        "33333333-3333-4333-8333-333333333333",
        "22222222-2222-4222-8222-222222222222",
    )
    .unwrap();
    let identity_c = OperationalIdentity::new(
        "11111111-1111-4111-8111-111111111111",
        "44444444-4444-4444-8444-444444444444",
    )
    .unwrap();
    let make = |enrollment: &str, organization: &str, identity| {
        OperationalEventContext::new(
            enrollment,
            organization,
            "workspace_01",
            identity,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            [ManagedModelDimension::new("anthropic", "anthropic/claude-sonnet-4").unwrap()],
        )
        .unwrap()
    };
    let event = |ctx: &OperationalEventContext| {
        StellaOperationalEventV1::from_finalized_rollup(ctx, &rollup(7)).unwrap()
    };

    assert_ne!(
        event(&make("a", "bc", identity_a.clone())).event_id(),
        event(&make("ab", "c", identity_a.clone())).event_id(),
        "length framing prevents container ambiguity"
    );
    assert_ne!(
        event(&make("enroll", "org", identity_a)).event_id(),
        event(&make("enroll", "org", identity_b)).event_id(),
        "installation identity separates hosts/containers"
    );
    assert_ne!(
        event(&make("enroll", "org", identity_c)).event_id(),
        event(&context()).event_id(),
        "store reset identity changes event ids"
    );
}

#[test]
fn unknown_provider_and_model_are_normalized_to_closed_other_dimensions() {
    let mut custom = rollup(9);
    custom.provider = "attacker-controlled-provider".into();
    custom.model = "attacker-controlled-model".into();
    let event = StellaOperationalEventV1::from_finalized_rollup(&context(), &custom).unwrap();
    let value = serde_json::to_value(event).unwrap();
    assert_eq!(value["provider"], "other");
    assert_eq!(value["model"], "other");
}

#[test]
fn spool_is_idempotent_bounded_and_evicts_oldest_with_durable_drop_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    let spool = EnterpriseTelemetrySpool::open_at(
        &path,
        SpoolLimits {
            max_rows: 2,
            max_bytes: 64 * 1024,
        },
    )
    .unwrap();
    let first = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(1)).unwrap();
    let second = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(2)).unwrap();
    let third = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(3)).unwrap();

    assert_eq!(
        spool.enqueue(SINK_A, &first, 10).unwrap(),
        EnqueueOutcome::Retained
    );
    assert_eq!(
        spool.enqueue(SINK_A, &first, 11).unwrap(),
        EnqueueOutcome::Duplicate
    );
    assert_eq!(
        spool.enqueue(SINK_A, &second, 20).unwrap(),
        EnqueueOutcome::Retained
    );
    assert_eq!(
        spool.enqueue(SINK_A, &third, 30).unwrap(),
        EnqueueOutcome::Retained
    );

    let status = spool.status().unwrap();
    assert_eq!(status.pending_rows, 2);
    assert_eq!(status.dropped_rows, 1);
    let claimed = spool
        .claim_batch(SINK_A, "worker", 40, 1_000, 10, 64 * 1024)
        .unwrap();
    let ids: Vec<_> = claimed.iter().map(|item| item.event.event_id()).collect();
    assert!(
        !ids.contains(&first.event_id()),
        "oldest event was not evicted"
    );

    drop(spool);
    let reopened = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();
    assert_eq!(reopened.status().unwrap().dropped_rows, 1);
}

#[test]
fn claims_are_transactional_retryable_and_expired_leases_recover() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    let spool = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();
    for id in 1..=2 {
        let event =
            StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(id)).unwrap();
        spool.enqueue(SINK_A, &event, id).unwrap();
    }

    let a = spool
        .claim_batch(SINK_A, "worker-a", 10, 50, 1, 64 * 1024)
        .unwrap();
    assert_eq!(a.len(), 1);
    let b = spool
        .claim_batch(SINK_A, "worker-b", 10, 50, 10, 64 * 1024)
        .unwrap();
    assert_eq!(b.len(), 1);
    assert_ne!(a[0].event.event_id(), b[0].event.event_id());
    assert!(spool.ack(SINK_A, "wrong-owner", &a).is_err());
    assert!(spool.retry(SINK_A, "wrong-owner", &a, 20).is_err());

    spool.retry(SINK_A, "worker-a", &a, 20).unwrap();
    assert!(
        spool
            .claim_batch(SINK_A, "worker-c", 20, 50, 10, 64 * 1024)
            .unwrap()
            .is_empty(),
        "backoff keeps a failed request retryable but not hot-looping"
    );
    let recovered = spool
        .claim_batch(SINK_A, "worker-d", 100, 50, 10, 64 * 1024)
        .unwrap();
    assert_eq!(recovered.len(), 1, "worker-b lease recovered after expiry");
    spool.ack(SINK_A, "worker-d", &recovered).unwrap();
    let retried = spool
        .claim_batch(SINK_A, "worker-c", 2_000, 50, 10, 64 * 1024)
        .unwrap();
    assert_eq!(retried.len(), 1);
    spool.ack(SINK_A, "worker-c", &retried).unwrap();
    assert_eq!(spool.status().unwrap().pending_rows, 0);
}

#[test]
fn claim_api_rejects_unbounded_batch_requests() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    let spool = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();

    assert!(
        spool
            .claim_batch(SINK_A, "worker", 10, 1_000, 1_001, 64 * 1024)
            .is_err()
    );
    assert!(
        spool
            .claim_batch(SINK_A, "worker", 10, 1_000, 10, 16 * 1024 * 1024 + 1)
            .is_err()
    );
}

#[test]
fn sink_rotation_strands_old_rows_until_explicit_discard() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    let spool = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();
    let old = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(1)).unwrap();
    let current = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(2)).unwrap();
    assert_eq!(
        spool.enqueue(SINK_A, &old, 1).unwrap(),
        EnqueueOutcome::Retained
    );
    assert_eq!(
        spool.enqueue(SINK_B, &current, 2).unwrap(),
        EnqueueOutcome::Retained
    );

    let claimed = spool
        .claim_batch(SINK_B, "worker", 10, 1_000, 10, 64 * 1024)
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].event.event_id(), current.event_id());
    let status = spool.status_for_sink(SINK_B).unwrap();
    assert_eq!(status.pending_rows, 1);
    assert_eq!(status.stranded_rows, 1);
    assert!(status.physical_bytes > 0);

    let discarded = spool.discard_stranded(SINK_B).unwrap();
    assert_eq!(discarded, 1);
    let status = spool.status_for_sink(SINK_B).unwrap();
    assert_eq!(status.stranded_rows, 0);
    assert_eq!(status.rollover_discarded_rows, 1);
}

#[test]
fn legacy_unbound_spool_rows_migrate_as_stranded_never_current() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE operational_spool (
                event_id TEXT PRIMARY KEY, payload BLOB NOT NULL,
                payload_bytes INTEGER NOT NULL, created_at_ms INTEGER NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt_ms INTEGER NOT NULL DEFAULT 0,
                leased_by TEXT, lease_until_ms INTEGER
             );
             CREATE TABLE operational_spool_meta (
                singleton INTEGER PRIMARY KEY, dropped_rows INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO operational_spool_meta VALUES (1, 0);",
        )
        .unwrap();
        let event =
            StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(1)).unwrap();
        let payload = serde_json::to_vec(&event).unwrap();
        conn.execute(
            "INSERT INTO operational_spool(event_id,payload,payload_bytes,created_at_ms)
             VALUES (?1,?2,?3,1)",
            rusqlite::params![event.event_id(), payload, 1_i64],
        )
        .unwrap();
    }

    let spool = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();
    let status = spool.status_for_sink(SINK_A).unwrap();
    assert_eq!(status.pending_rows, 0);
    assert_eq!(status.stranded_rows, 1);
    assert!(
        spool
            .claim_batch(SINK_A, "worker", 10, 1_000, 10, 64 * 1024)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn an_oversized_new_event_reports_dropped_new_not_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    let event = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(1)).unwrap();
    let spool = EnterpriseTelemetrySpool::open_at(
        &path,
        SpoolLimits {
            max_rows: 10,
            max_bytes: 1,
        },
    )
    .unwrap();
    assert_eq!(
        spool.enqueue(SINK_A, &event, 1).unwrap(),
        EnqueueOutcome::DroppedNew
    );
}

#[test]
fn capacity_never_evicts_rows_owned_by_another_sink() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    let spool = EnterpriseTelemetrySpool::open_at(
        &path,
        SpoolLimits {
            max_rows: 2,
            max_bytes: 64 * 1024,
        },
    )
    .unwrap();
    let first = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(1)).unwrap();
    let second = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(2)).unwrap();
    let rotated = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(3)).unwrap();
    assert_eq!(
        spool.enqueue(SINK_A, &first, 1).unwrap(),
        EnqueueOutcome::Retained
    );
    assert_eq!(
        spool.enqueue(SINK_A, &second, 2).unwrap(),
        EnqueueOutcome::Retained
    );

    assert_eq!(
        spool.enqueue(SINK_B, &rotated, 3).unwrap(),
        EnqueueOutcome::DroppedNew,
        "a newly rotated sink cannot consume capacity by evicting the old sink"
    );
    assert_eq!(spool.status_for_sink(SINK_A).unwrap().pending_rows, 2);
    assert_eq!(spool.status_for_sink(SINK_B).unwrap().pending_rows, 0);
}

#[test]
fn clock_rollback_rebases_once_without_clearing_a_live_lease() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    let spool = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();
    let event = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(1)).unwrap();
    spool.enqueue(SINK_A, &event, 100_000).unwrap();
    assert_eq!(
        spool
            .claim_batch(SINK_A, "future-worker", 100_000, 30_000, 1, 64 * 1024)
            .unwrap()
            .len(),
        1
    );
    let concurrent = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();

    assert!(
        concurrent
            .claim_batch(SINK_A, "rolled-back-a", 1_000, 1_000, 1, 64 * 1024)
            .unwrap()
            .is_empty(),
        "rollback repair must preserve the original owner's rebased live lease"
    );
    assert!(
        spool
            .claim_batch(SINK_A, "rolled-back-b", 1_000, 1_000, 1, 64 * 1024)
            .unwrap()
            .is_empty(),
        "a concurrent caller at the same repaired epoch must not rebase again"
    );
    assert_eq!(
        concurrent
            .claim_batch(SINK_A, "after-expiry", 31_000, 1_000, 1, 64 * 1024)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn retry_deadline_never_exceeds_the_inclusive_375_second_horizon() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    let spool = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();
    let event = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(1)).unwrap();
    spool.enqueue(SINK_A, &event, 0).unwrap();
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let mut now = 0_i64;
    for attempt in 0..10 {
        let owner = format!("worker-{attempt}");
        let claimed = spool
            .claim_batch(SINK_A, &owner, now, 1_000, 1, 64 * 1024)
            .unwrap();
        assert_eq!(claimed.len(), 1);
        spool.retry(SINK_A, &owner, &claimed, now).unwrap();
        let deadline: i64 = inspect
            .query_row(
                "SELECT next_attempt_ms FROM operational_spool WHERE sink_fingerprint = ?1",
                [SINK_A],
                |row| row.get(0),
            )
            .unwrap();
        assert!(deadline >= now);
        assert!(
            deadline <= now + 375_000,
            "attempt {attempt}: {deadline} > {now}"
        );
        now = deadline;
    }
}

#[test]
fn malformed_spool_row_is_quarantined_before_lease_and_does_not_block_good_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    let spool = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();
    let corrupt_id = format!("evt_{}", "c".repeat(64));
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute(
        "INSERT INTO operational_spool
         (event_id, sink_fingerprint, payload, payload_bytes, created_at_ms)
         VALUES (?1, ?2, ?3, 1, 0)",
        rusqlite::params![corrupt_id, SINK_A, vec![b'{']],
    )
    .unwrap();
    drop(raw);
    let good = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(9)).unwrap();
    spool.enqueue(SINK_A, &good, 1).unwrap();

    let claimed = spool
        .claim_batch(SINK_A, "worker", 10, 1_000, 1, 64 * 1024)
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].event.event_id(), good.event_id());
    let status = spool.status_for_sink(SINK_A).unwrap();
    assert_eq!(status.corrupt_dropped_rows, 1);
    assert_eq!(status.pending_rows, 1);
}

#[test]
fn separate_connections_cannot_claim_the_same_event_concurrently() {
    use std::sync::{Arc, Barrier};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enterprise-telemetry.db");
    let first = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();
    let event = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(1)).unwrap();
    first.enqueue(SINK_A, &event, 1).unwrap();
    let second = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default()).unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let a_barrier = barrier.clone();
    let a = std::thread::spawn(move || {
        a_barrier.wait();
        first
            .claim_batch(SINK_A, "a", 10, 1_000, 1, 64 * 1024)
            .unwrap()
    });
    let b_barrier = barrier.clone();
    let b = std::thread::spawn(move || {
        b_barrier.wait();
        second
            .claim_batch(SINK_A, "b", 10, 1_000, 1, 64 * 1024)
            .unwrap()
    });
    barrier.wait();
    let claimed = a.join().unwrap().len() + b.join().unwrap().len();
    assert_eq!(claimed, 1);
}

#[test]
fn byte_limit_and_owner_only_file_mode_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("host-data/enterprise-telemetry.db");
    let event = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(1)).unwrap();
    let one_event_bytes = serde_json::to_vec(&event).unwrap().len() as u64;
    let spool = EnterpriseTelemetrySpool::open_at(
        &path,
        SpoolLimits {
            max_rows: 10,
            max_bytes: one_event_bytes + 8,
        },
    )
    .unwrap();
    spool.enqueue(SINK_A, &event, 1).unwrap();
    let second = StellaOperationalEventV1::from_finalized_rollup(&context(), &rollup(2)).unwrap();
    spool.enqueue(SINK_A, &second, 2).unwrap();
    let status = spool.status().unwrap();
    assert_eq!(status.pending_rows, 1);
    assert_eq!(status.dropped_rows, 1);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::symlink_metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn installation_and_store_identities_persist_and_reset_on_their_real_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let host_a = dir.path().join("host-a");
    let host_b = dir.path().join("host-b");
    let install_a = load_or_create_installation_uuid(&host_a).unwrap();
    assert_eq!(
        install_a,
        load_or_create_installation_uuid(&host_a).unwrap()
    );
    assert_ne!(
        install_a,
        load_or_create_installation_uuid(&host_b).unwrap()
    );

    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = stella_store::Store::open(&workspace).unwrap();
    let first = store.enterprise_store_uuid().unwrap();
    assert_eq!(first, store.enterprise_store_uuid().unwrap());
    drop(store);
    let reopened = stella_store::Store::open(&workspace).unwrap();
    assert_eq!(first, reopened.enterprise_store_uuid().unwrap());
    drop(reopened);

    let db = workspace.join(".stella/private/store.db");
    std::fs::remove_file(&db).unwrap();
    let reset = stella_store::Store::open(&workspace).unwrap();
    assert_ne!(first, reset.enterprise_store_uuid().unwrap());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(host_a.join("installation-id"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn cloned_store_uses_a_fresh_persistent_export_nonce_per_ledger_row() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let store = Store::open(&source).unwrap();
    store.begin_enterprise_enrollment(SINK_A).unwrap();
    let execution = store
        .begin_execution("run", "same", "anthropic", "model")
        .unwrap();
    store.finish_execution(execution, "completed", 0.0).unwrap();
    let store_uuid = store.enterprise_store_uuid().unwrap();
    drop(store);

    let source_db = source.join(".stella/private/store.db");
    let clone_a = dir.path().join("clone-a");
    let clone_b = dir.path().join("clone-b");
    let db_a = stella_store::workspace_private_sqlite_path(&clone_a, "store.db").unwrap();
    let db_b = stella_store::workspace_private_sqlite_path(&clone_b, "store.db").unwrap();
    std::fs::copy(&source_db, &db_a).unwrap();
    std::fs::copy(&source_db, &db_b).unwrap();

    let a = Store::open(&clone_a).unwrap();
    let b = Store::open(&clone_b).unwrap();
    assert_eq!(a.enterprise_store_uuid().unwrap(), store_uuid);
    assert_eq!(b.enterprise_store_uuid().unwrap(), store_uuid);
    let nonce_a = a
        .mark_enterprise_export_pending(SINK_A, execution)
        .unwrap()
        .unwrap();
    let nonce_b = b
        .mark_enterprise_export_pending(SINK_A, execution)
        .unwrap()
        .unwrap();
    assert_ne!(
        nonce_a, nonce_b,
        "clones must not derive the same export id"
    );
    assert_eq!(
        a.mark_enterprise_export_pending(SINK_A, execution)
            .unwrap()
            .unwrap(),
        nonce_a,
        "retrying one ledger row must reuse its persisted nonce"
    );

    let identity =
        OperationalIdentity::new("11111111-1111-4111-8111-111111111111", &store_uuid).unwrap();
    let event = |nonce: &str| {
        let context = OperationalEventContext::new(
            "enroll_01",
            "org_01",
            "workspace_01",
            identity.clone(),
            nonce,
            [ManagedModelDimension::new("anthropic", "anthropic/claude-sonnet-4").unwrap()],
        )
        .unwrap();
        StellaOperationalEventV1::from_finalized_rollup(&context, &rollup(execution)).unwrap()
    };
    assert_ne!(event(&nonce_a).event_id(), event(&nonce_b).event_id());
}

#[test]
fn export_ledger_backfills_only_post_enrollment_pending_executions() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = stella_store::Store::open(&workspace).unwrap();
    let old = store
        .begin_execution("run", "old", "anthropic", "model")
        .unwrap();
    store.finish_execution(old, "completed", 0.0).unwrap();
    store.begin_enterprise_enrollment(SINK_A).unwrap();
    assert!(
        store
            .mark_enterprise_export_pending(SINK_A, old)
            .unwrap()
            .is_none()
    );

    let new = store
        .begin_execution("run", "new", "anthropic", "model")
        .unwrap();
    store.finish_execution(new, "completed", 0.0).unwrap();
    assert!(
        store
            .mark_enterprise_export_pending(SINK_A, new)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store
            .pending_enterprise_export_page(SINK_A, None, 256)
            .unwrap()[0]
            .execution_id,
        new
    );
    store.mark_enterprise_export_spooled(SINK_A, new).unwrap();
    assert!(
        store
            .pending_enterprise_export_page(SINK_A, None, 256)
            .unwrap()
            .is_empty()
    );

    drop(store);
    let reopened = stella_store::Store::open(&workspace).unwrap();
    assert!(
        reopened
            .pending_enterprise_export_page(SINK_A, None, 256)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn pending_export_backfill_is_hard_paged_across_a_ten_thousand_row_outage() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = Store::open(&workspace).unwrap();
    store.begin_enterprise_enrollment(SINK_A).unwrap();
    for index in 0..10_050 {
        let execution = store
            .begin_execution("run", &format!("outage-{index}"), "anthropic", "model")
            .unwrap();
        store.finish_execution(execution, "completed", 0.0).unwrap();
        store
            .mark_enterprise_export_pending(SINK_A, execution)
            .unwrap()
            .unwrap();
    }

    assert!(
        store
            .pending_enterprise_export_page(SINK_A, None, 257)
            .is_err(),
        "callers cannot request an unbounded startup page"
    );
    let first = store
        .pending_enterprise_export_page(SINK_A, None, 256)
        .unwrap();
    assert_eq!(first.len(), 256);
    let second = store
        .pending_enterprise_export_page(SINK_A, Some(first.last().unwrap().execution_id), 256)
        .unwrap();
    assert_eq!(second.len(), 256);
    assert!(second[0].execution_id > first[255].execution_id);
}

#[test]
fn completed_export_ledger_compacts_with_a_durable_idempotency_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = Store::open(&workspace).unwrap();
    store.begin_enterprise_enrollment(SINK_A).unwrap();
    let mut rows = Vec::new();
    for index in 0..300 {
        let execution = store
            .begin_execution("run", &format!("done-{index}"), "anthropic", "model")
            .unwrap();
        store.finish_execution(execution, "completed", 0.0).unwrap();
        let nonce = store
            .mark_enterprise_export_pending(SINK_A, execution)
            .unwrap()
            .unwrap();
        store
            .mark_enterprise_export_spooled(SINK_A, execution)
            .unwrap();
        rows.push((execution, nonce));
    }

    assert_eq!(
        store.compact_enterprise_export_ledger(SINK_A, 32).unwrap(),
        268
    );
    assert!(
        store
            .mark_enterprise_export_pending(SINK_A, rows[0].0)
            .unwrap()
            .is_none(),
        "the compacted-through boundary prevents a new nonce for old closeout"
    );
    assert_eq!(
        store
            .mark_enterprise_export_pending(SINK_A, rows[299].0)
            .unwrap()
            .unwrap(),
        rows[299].1
    );
}
