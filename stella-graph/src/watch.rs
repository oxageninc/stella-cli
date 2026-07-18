//! Live, in-process incremental re-index while a session runs
//! ("The code-graph watcher is an in-process
//! `notify` task alive only while a session runs" — no daemon, no background
//! network listener). Filesystem events are debounced so an editor's
//! save-storm collapses into a single re-index batch.
//!
//! The watcher only *feeds paths*; the actual re-index is one transactional
//! [`crate::store::apply_changes`] batch (L-L1), run on the blocking pool so
//! it never stalls async workers.
//!
//! # Structure: event source vs. pipeline
//!
//! Event *delivery* (the `notify` OS watcher) is deliberately decoupled from
//! the *pipeline* (channel → debounce → transactional apply). [`spawn`] wires
//! the real watcher to [`start_pipeline`]; tests drive the identical pipeline
//! through a [`WatchInjector`] instead, so live-index behavior is covered
//! without depending on OS event timing (FSEvents coalescing, inotify
//! batching). Both entry points share [`is_watch_relevant`], so the
//! language/ignore filtering under test is the production filter.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::error::GraphError;
use crate::graph::Inner;
use crate::lang::Language;
use crate::store::IndexStats;
use crate::walk;

/// Debounce window: filesystem bursts within this interval coalesce into one
/// re-index batch.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(200);

/// Failsafe bound on [`WatchInjector::applied_past`]. Assertions never depend
/// on it — it only turns a wedged pipeline into a fast test failure instead
/// of a hang (and under a paused tokio clock it is skipped instantly).
const APPLIED_FAILSAFE: Duration = Duration::from_secs(60);

/// The applied-batch feed: a monotonically increasing batch sequence number
/// plus the stats of the most recently applied batch.
type BatchTick = (u64, IndexStats);

/// Whether a watcher-delivered path is worth re-indexing: a source file we
/// recognize, not inside an ignored directory. The single relevance filter
/// shared by the real `notify` callback and [`WatchInjector::inject`].
fn is_watch_relevant(root: &Path, path: &Path) -> bool {
    Language::from_path(path).is_some() && !walk::rel_is_ignored(root, path)
}

/// Spawn the consumption side of live indexing: an unbounded path channel
/// whose receiver runs [`debounce_loop`]. Returns the sender that feeds it
/// and a receiver of [`BatchTick`]s, published after every applied batch —
/// the deterministic completion signal (no wall-clock polling).
fn start_pipeline(
    inner: Arc<Inner>,
    debounce: Duration,
) -> (
    mpsc::UnboundedSender<PathBuf>,
    tokio::sync::watch::Receiver<BatchTick>,
) {
    let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();
    let (tick_tx, tick_rx) = tokio::sync::watch::channel((0, IndexStats::default()));
    let handle = tokio::spawn(debounce_loop(inner.clone(), rx, debounce, tick_tx));
    inner.push_task(handle);
    (tx, tick_rx)
}

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
    // The tick receiver is dropped: production has no consumer for the
    // completion signal (the loop publishes via `send_modify`, which never
    // fails without receivers). If watcher creation fails below, `tx` is
    // dropped with the closure and the pipeline task exits on channel close.
    let (tx, _ticks) = start_pipeline(inner, debounce);

    let event_root = root.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        let Ok(event) = result else {
            return;
        };
        for path in event.paths {
            // Only source files we index, and never inside ignored dirs.
            if is_watch_relevant(&event_root, &path) {
                let _ = tx.send(path);
            }
        }
    })
    .map_err(|e| GraphError::Watch(e.to_string()))?;

    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| GraphError::Watch(e.to_string()))?;

    Ok(watcher)
}

/// Start the live-index pipeline **without** an OS watcher and return the
/// injector that stands in for it. See [`WatchInjector`]. Must be called from
/// within a tokio runtime, like [`spawn`].
pub(crate) fn spawn_injectable(inner: Arc<Inner>, debounce: Duration) -> WatchInjector {
    let root = inner.root.clone();
    let (tx, ticks) = start_pipeline(inner, debounce);
    WatchInjector { root, tx, ticks }
}

/// Test seam: stands in for the OS watcher, feeding synthetic events into the
/// **real** debounce → transactional-apply pipeline. Injection replaces event
/// *delivery* only — injected paths are read from disk by
/// [`crate::store::apply_changes`] exactly as in production, so tests write
/// real files into the workspace and then inject their paths instead of
/// waiting on environment-dependent fs-event timing.
#[doc(hidden)]
pub struct WatchInjector {
    root: PathBuf,
    tx: mpsc::UnboundedSender<PathBuf>,
    ticks: tokio::sync::watch::Receiver<BatchTick>,
}

impl WatchInjector {
    /// Inject `path` as if the OS watcher had delivered an event for it,
    /// applying the same relevance filter as the real `notify` callback.
    /// Returns whether the path passed the filter and was enqueued.
    pub fn inject(&self, path: &Path) -> bool {
        if !is_watch_relevant(&self.root, path) {
            return false;
        }
        self.tx.send(path.to_path_buf()).is_ok()
    }

    /// Sequence number of the most recently applied batch (0 = none yet).
    pub fn batches_applied(&self) -> u64 {
        self.ticks.borrow().0
    }

    /// Wait until a batch with sequence number strictly greater than `seen`
    /// has been applied; returns that batch's `(seq, stats)`. `None` means
    /// the pipeline shut down or the failsafe elapsed — never a normal
    /// outcome for a healthy pipeline.
    pub async fn applied_past(&mut self, seen: u64) -> Option<BatchTick> {
        let wait = self.ticks.wait_for(|(seq, _)| *seq > seen);
        match tokio::time::timeout(APPLIED_FAILSAFE, wait).await {
            Ok(Ok(tick)) => Some(tick.clone()),
            _ => None,
        }
    }
}

/// Collect a burst of changed paths, then apply them in one batch and publish
/// a [`BatchTick`]. Exits when the channel closes (watcher or injector
/// dropped) or shutdown is requested.
async fn debounce_loop(
    inner: Arc<Inner>,
    mut rx: mpsc::UnboundedReceiver<PathBuf>,
    debounce: Duration,
    ticks: tokio::sync::watch::Sender<BatchTick>,
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
        let outcome =
            tokio::task::spawn_blocking(move || batch_inner.apply_changes_blocking(&paths)).await;
        // A tick means "batch attempt finished", success or not (L-L1: one
        // bad batch must not wedge consumers awaiting the signal). Failures
        // publish default stats.
        let stats = match outcome {
            Ok(Ok(stats)) => stats,
            _ => IndexStats::default(),
        };
        ticks.send_modify(|(seq, last)| {
            *seq += 1;
            *last = stats;
        });
    }
}
