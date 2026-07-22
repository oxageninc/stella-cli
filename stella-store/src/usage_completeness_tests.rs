//! Durable execution-accounting completeness witnesses.

use super::*;

const SINK: &str = "sink_0000000000000000000000000000000000000000000000000000000000000000";

#[test]
fn new_execution_is_pending_and_not_rollupable() {
    let store = Store::in_memory().unwrap();
    let execution = store
        .begin_execution("pipeline", "private", "anthropic", "claude")
        .unwrap();

    assert!(!store.execution_usage_complete(execution).unwrap());
    let status: String = store
        .lock()
        .query_row(
            "SELECT usage_status FROM executions WHERE id = ?1",
            params![execution],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "pending");
    assert!(
        store
            .execution_rollup(execution, std::path::Path::new("/tmp/project"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn incomplete_closeout_is_durable_and_not_rollupable() {
    let store = Store::in_memory().unwrap();
    let execution = store
        .begin_execution("pipeline", "private", "anthropic", "claude")
        .unwrap();

    store
        .finish_execution_accounted(execution, "aborted", 0.25, false)
        .unwrap();

    assert!(!store.execution_usage_complete(execution).unwrap());
    assert!(
        store
            .execution_rollup(execution, std::path::Path::new("/tmp/project"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn clean_finalization_is_the_only_rollupable_state() {
    let store = Store::in_memory().unwrap();
    let execution = store
        .begin_execution("pipeline", "private", "anthropic", "claude")
        .unwrap();
    store
        .finish_execution_accounted(execution, "completed", 0.25, true)
        .unwrap();

    assert!(store.execution_usage_complete(execution).unwrap());
    let rollup = store
        .execution_rollup(execution, std::path::Path::new("/tmp/project"))
        .unwrap()
        .expect("complete finalized rollup");
    assert!(rollup.usage_complete);
}

#[test]
fn pending_page_skips_incomplete_rows_without_consuming_them() {
    let store = Store::in_memory().unwrap();
    store.begin_enterprise_enrollment(SINK).unwrap();

    let mut incomplete = Vec::new();
    for _ in 0..256 {
        let id = store
            .begin_execution("pipeline", "private", "anthropic", "claude")
            .unwrap();
        store
            .finish_execution_accounted(id, "aborted", 0.25, false)
            .unwrap();
        assert!(
            store
                .mark_enterprise_export_pending(SINK, id)
                .unwrap()
                .is_some()
        );
        incomplete.push(id);
    }
    let complete = store
        .begin_execution("pipeline", "private", "anthropic", "claude")
        .unwrap();
    store
        .finish_execution_accounted(complete, "completed", 0.5, true)
        .unwrap();
    assert!(
        store
            .mark_enterprise_export_pending(SINK, complete)
            .unwrap()
            .is_some()
    );

    let page = store.pending_enterprise_export_page(SINK, None, 1).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].execution_id, complete);
    let retained: i64 = store
        .lock()
        .query_row(
            "SELECT COUNT(*) FROM enterprise_export_ledger WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, 257, "incomplete intents remain retryable");
    assert!(incomplete.iter().all(|id| *id < complete));
}

#[test]
fn marking_usage_incomplete_is_monotonic() {
    let store = Store::in_memory().unwrap();
    let execution = store
        .begin_execution("pipeline", "private", "anthropic", "claude")
        .unwrap();
    store.mark_execution_usage_incomplete(execution).unwrap();
    store
        .finish_execution(execution, "completed", 0.25)
        .unwrap();
    assert!(!store.execution_usage_complete(execution).unwrap());
}
