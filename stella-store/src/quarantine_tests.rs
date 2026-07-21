use super::*;

#[test]
fn quarantine_query_reports_schema_corruption() {
    let store = Store::in_memory().expect("in-memory store");
    store
        .lock()
        .execute("DROP TABLE memory_citations", [])
        .expect("remove quarantine source table");

    let error = store
        .quarantined_memory_ids()
        .expect_err("missing quarantine table must be explicit");
    assert!(
        error.to_string().contains("memory_citations"),
        "diagnostic names the failed quarantine source: {error}"
    );
}

#[test]
fn empty_quarantine_query_is_distinct_from_query_failure() {
    let store = Store::in_memory().expect("in-memory store");

    assert!(
        store
            .quarantined_memory_ids()
            .expect("valid quarantine query")
            .is_empty()
    );
}
