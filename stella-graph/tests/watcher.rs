//! Watcher smoke test — mount, then create a file and see it indexed live
//! through the real OS event source (`notify`).
//!
//! `#[ignore]`d by default: filesystem-event *delivery* timing is
//! environment-dependent (CI containers, network filesystems, and macOS
//! FSEvents coalescing all vary), so this would be a flaky gate. It stays as
//! an opt-in end-to-end smoke of the `notify` wiring only — the live-index
//! pipeline itself (debounce → transactional apply, add/modify/delete,
//! batching) is covered deterministically in `live_index.rs` by injecting
//! events below the OS watcher. Run this one locally with
//! `cargo test -p stella-graph -- --ignored`.

use std::fs;
use std::time::{Duration, Instant};

use stella_graph::CodeGraph;
use tempfile::TempDir;

async fn poll_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cond()
}

#[tokio::test]
#[ignore = "real fs-event delivery is environment-dependent; deterministic live-index coverage lives in live_index.rs"]
async fn watcher_indexes_a_newly_created_file() {
    let ws = TempDir::new().unwrap();
    let dbdir = TempDir::new().unwrap();
    fs::write(ws.path().join("a.rs"), "pub fn a() {}\n").unwrap();

    let graph = CodeGraph::mount(ws.path(), &dbdir.path().join("context.db"))
        .await
        .unwrap();

    // Catch-up at mount should index the pre-existing file.
    assert!(
        poll_until(Duration::from_secs(5), || graph.file_count().unwrap_or(0)
            >= 1)
        .await,
        "catch-up should have indexed a.rs"
    );

    // A newly created file should be picked up by the live watcher.
    fs::write(ws.path().join("b.rs"), "pub struct Fresh;\n").unwrap();
    let seen = poll_until(Duration::from_secs(10), || {
        graph
            .definitions("Fresh")
            .map(|d| !d.is_empty())
            .unwrap_or(false)
    })
    .await;
    assert!(
        seen,
        "the watcher should have indexed the newly created file"
    );

    // And a query against the new symbol carries the right citation label.
    let defs = graph.definitions("Fresh").unwrap();
    assert!(
        defs[0]
            .citation_label
            .as_deref()
            .unwrap()
            .starts_with("struct Fresh (")
    );

    graph.shutdown();
}
