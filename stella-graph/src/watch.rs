//! Live, in-process incremental re-index while a session runs
//! (`02-architecture.md` §6: "The code-graph watcher is an in-process
//! `notify` task alive only while a session runs" — no daemon, no background
//! network listener). Filesystem events are debounced so an editor's
//! save-storm collapses into a single re-index batch.
//!
//! The watcher only *feeds paths*; the actual re-index is one transactional
//! [`crate::store::apply_changes`] batch (L-L1), run on the blocking pool so
//! it never stalls async workers.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::error::GraphError;
use crate::graph::Inner;
use crate::lang::Language;
use crate::walk;

/// Debounce window: filesystem bursts within this interval coalesce into one
/// re-index batch.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(200);

/// Arm a recursive watcher on the workspace root and spawn the debounce loop.
/// Returns the watcher handle, which the caller must keep alive — dropping it
/// stops watching (and closes the channel, so the debounce task exits
/// cleanly, which is exactly how [`crate::graph::CodeGraph::shutdown`] tears
/// it down).
pub(crate) fn spawn(
    inner: Arc<Inner>,
    debounce: Duration,
) -> Result<RecommendedWatcher, GraphError> {
    let root = inner.root.clone();
    let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();

    let event_root = root.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        let Ok(event) = result else {
            return;
        };
        for path in event.paths {
            // Only source files we index, and never inside ignored dirs.
            if Language::from_path(&path).is_some() && !walk::rel_is_ignored(&event_root, &path) {
                let _ = tx.send(path);
            }
        }
    })
    .map_err(|e| GraphError::Watch(e.to_string()))?;

    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| GraphError::Watch(e.to_string()))?;

    let handle = tokio::spawn(debounce_loop(inner.clone(), rx, debounce));
    inner.push_task(handle);

    Ok(watcher)
}

/// Collect a burst of changed paths, then apply them in one batch. Exits when
/// the channel closes (watcher dropped) or shutdown is requested.
async fn debounce_loop(
    inner: Arc<Inner>,
    mut rx: mpsc::UnboundedReceiver<PathBuf>,
    debounce: Duration,
) {
    loop {
        let first = match rx.recv().await {
            Some(path) => path,
            None => break, // watcher dropped
        };
        let mut pending: HashSet<PathBuf> = HashSet::new();
        pending.insert(first);

        // Drain everything that arrives within the debounce window.
        loop {
            match tokio::time::timeout(debounce, rx.recv()).await {
                Ok(Some(path)) => {
                    pending.insert(path);
                }
                Ok(None) => break, // channel closed mid-burst
                Err(_) => break,   // window elapsed
            }
        }

        if inner.shutdown.load(Ordering::Relaxed) {
            break;
        }

        let paths: Vec<PathBuf> = pending.into_iter().collect();
        let batch_inner = inner.clone();
        // SQLite work is blocking + CPU-bound (re-parse) → off the async pool.
        let _ =
            tokio::task::spawn_blocking(move || batch_inner.apply_changes_blocking(&paths)).await;
    }
}
