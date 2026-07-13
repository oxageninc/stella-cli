//! Deterministic live-indexing coverage: drives the REAL debounce →
//! `apply_changes` pipeline by injecting paths directly into the watch
//! channel (`CodeGraph::watch_pipeline_for_tests`), replacing OS event
//! *delivery* only — every injected path is a real file (or a real deletion)
//! in the tempdir workspace, read from disk by the production code path.
//!
//! Two things make these tests timing-independent:
//!
//! 1. `#[tokio::test(start_paused = true)]`: tokio's paused clock only
//!    auto-advances while **every** task is idle, so the debounce window can
//!    never elapse between two consecutive `inject` calls — a burst cannot be
//!    split by scheduling, and no wall-clock time is spent waiting it out.
//! 2. The injector's applied-batch signal (`applied_past`): assertions run
//!    after an awaited completion event, never after a sleep/poll. Its
//!    internal timeout is a failsafe against a wedged pipeline, not something
//!    any assertion depends on.
//!
//! The real-FS smoke test (notify watcher end to end) stays in `watcher.rs`,
//! opt-in via `--ignored`.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use stella_graph::{CodeGraph, WatchInjector};
use tempfile::TempDir;

/// A fixture workspace + store (db kept in a separate tempdir so it is never
/// indexed), opened and fully indexed synchronously, with the live pipeline
/// running and an injector standing in for the OS watcher.
struct Live {
    _ws: TempDir,
    _dbdir: TempDir,
    /// Canonicalized workspace root — the same form the real watcher's
    /// events use (it watches `graph.root()`, so e.g. on macOS events carry
    /// `/private/var/...`, not the tempdir's `/var/...` alias).
    root: PathBuf,
    graph: CodeGraph,
    injector: WatchInjector,
}

impl Live {
    fn build(files: &[(&str, &str)]) -> Live {
        let ws = TempDir::new().unwrap();
        let dbdir = TempDir::new().unwrap();
        for (rel, content) in files {
            let path = ws.path().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
        let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();
        graph.index_all().unwrap();
        // Production debounce width: free under the paused clock.
        let injector = graph.watch_pipeline_for_tests(Duration::from_millis(200));
        Live {
            _ws: ws,
            _dbdir: dbdir,
            root: graph.root().to_path_buf(),
            graph,
            injector,
        }
    }

    /// Absolute path of a workspace-relative file, in canonical root form.
    fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.abs(rel);
        fs::write(&path, content).unwrap();
        path
    }

    fn has_def(&self, name: &str) -> bool {
        !self.graph.definitions(name).unwrap().is_empty()
    }
}

#[tokio::test(start_paused = true)]
async fn injected_create_indexes_new_file() {
    let mut fx = Live::build(&[("a.rs", "pub fn alpha() {}\n")]);
    assert_eq!(fx.graph.file_count().unwrap(), 1);

    let new = fx.write("b.rs", "pub struct Fresh;\n");
    assert!(fx.injector.inject(&new));

    let (seq, stats) = fx.injector.applied_past(0).await.unwrap();
    assert_eq!(seq, 1);
    assert_eq!(stats.files_parsed, 1);

    assert_eq!(fx.graph.file_count().unwrap(), 2);
    let defs = fx.graph.definitions("Fresh").unwrap();
    assert!(!defs.is_empty(), "new file's symbols should be visible");
    // The live-indexed symbol carries the same citation label a full index
    // would produce.
    assert!(
        defs[0]
            .citation_label
            .as_deref()
            .unwrap()
            .starts_with("struct Fresh (")
    );

    fx.graph.shutdown();
}

#[tokio::test(start_paused = true)]
async fn injected_modify_replaces_symbols() {
    let mut fx = Live::build(&[("a.rs", "pub fn old_sym() {}\n")]);
    assert!(fx.has_def("old_sym"));

    let path = fx.write("a.rs", "pub fn new_sym() {}\n");
    assert!(fx.injector.inject(&path));

    let (seq, stats) = fx.injector.applied_past(0).await.unwrap();
    assert_eq!(seq, 1);
    assert_eq!(stats.files_parsed, 1);

    assert!(
        !fx.has_def("old_sym"),
        "stale symbols must be replaced, not accumulated"
    );
    assert!(fx.has_def("new_sym"));
    assert_eq!(fx.graph.file_count().unwrap(), 1, "same file, re-indexed");

    fx.graph.shutdown();
}

#[tokio::test(start_paused = true)]
async fn sha_identical_write_is_skipped() {
    let content = "pub fn stable() {}\n";
    let mut fx = Live::build(&[("a.rs", content)]);

    // Byte-identical rewrite: the batch runs, but the parse is skipped (L-C2).
    let path = fx.write("a.rs", content);
    assert!(fx.injector.inject(&path));

    let (seq, stats) = fx.injector.applied_past(0).await.unwrap();
    assert_eq!(seq, 1);
    assert_eq!(stats.files_skipped_unchanged, 1);
    assert_eq!(
        stats.files_parsed, 0,
        "identical content is never re-parsed"
    );

    // And the graph is untouched, not wrongly pruned or duplicated.
    assert_eq!(fx.graph.definitions("stable").unwrap().len(), 1);

    fx.graph.shutdown();
}

#[tokio::test(start_paused = true)]
async fn injected_delete_prunes_file_and_symbols() {
    let mut fx = Live::build(&[
        ("keep.rs", "pub fn kept() {}\n"),
        ("gone.rs", "pub struct Doomed;\npub fn doomed_fn() {}\n"),
    ]);
    assert_eq!(fx.graph.file_count().unwrap(), 2);

    let path = fx.abs("gone.rs");
    fs::remove_file(&path).unwrap();
    assert!(fx.injector.inject(&path));

    let (seq, stats) = fx.injector.applied_past(0).await.unwrap();
    assert_eq!(seq, 1);
    assert_eq!(stats.files_pruned, 1);

    assert_eq!(fx.graph.file_count().unwrap(), 1);
    // Symbol rows go with the file row (ON DELETE CASCADE).
    assert!(!fx.has_def("Doomed"));
    assert!(!fx.has_def("doomed_fn"));
    assert!(fx.has_def("kept"), "unrelated files stay indexed");

    fx.graph.shutdown();
}

#[tokio::test(start_paused = true)]
async fn burst_applies_as_one_batch() {
    let mut fx = Live::build(&[("a.rs", "pub fn alpha() {}\n")]);

    // A save-storm: two new files plus a deletion, all injected within one
    // debounce window (guaranteed: the paused clock cannot advance while the
    // test task is still running between injections).
    let b = fx.write("b.rs", "pub fn beta() {}\n");
    let c = fx.write("c.rs", "pub fn gamma() {}\n");
    let a = fx.abs("a.rs");
    fs::remove_file(&a).unwrap();
    assert!(fx.injector.inject(&b));
    assert!(fx.injector.inject(&c));
    assert!(fx.injector.inject(&a));

    let (seq, stats) = fx.injector.applied_past(0).await.unwrap();
    assert_eq!(seq, 1, "the whole burst must coalesce into a single batch");
    assert_eq!(fx.injector.batches_applied(), 1);
    assert_eq!(stats.files_parsed, 2);
    assert_eq!(stats.files_pruned, 1);

    assert!(fx.has_def("beta"));
    assert!(fx.has_def("gamma"));
    assert!(!fx.has_def("alpha"));
    assert_eq!(fx.graph.file_count().unwrap(), 2);

    // A change injected *after* the batch signal starts a new batch.
    let d = fx.write("d.rs", "pub fn delta() {}\n");
    assert!(fx.injector.inject(&d));
    let (seq, _) = fx.injector.applied_past(1).await.unwrap();
    assert_eq!(seq, 2);
    assert!(fx.has_def("delta"));

    fx.graph.shutdown();
}

#[tokio::test(start_paused = true)]
async fn irrelevant_paths_are_filtered_at_injection() {
    let fx = Live::build(&[("a.rs", "pub fn alpha() {}\n")]);

    // Same filter as the real notify callback: non-source extensions,
    // ignored directories, and paths outside the workspace root never enter
    // the pipeline.
    let readme = fx.write("README.md", "# not source\n");
    assert!(!fx.injector.inject(&readme));

    fs::create_dir_all(fx.abs("node_modules")).unwrap();
    let vendored = fx.write("node_modules/dep.js", "export const x = 1;\n");
    assert!(!fx.injector.inject(&vendored));

    let outside = std::env::temp_dir().join("stella-live-index-outside.rs");
    assert!(!fx.injector.inject(&outside));

    assert_eq!(fx.injector.batches_applied(), 0, "nothing was enqueued");
    assert_eq!(fx.graph.file_count().unwrap(), 1);

    fx.graph.shutdown();
}
