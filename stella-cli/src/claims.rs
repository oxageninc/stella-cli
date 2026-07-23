//! Claim-on-first-write — coordinator-free write coordination over one
//! shared tree.
//!
//! Every writer in a workspace (the deck's lead, its sub-session workers,
//! shared-tree fleet workers — across processes and across fleets) wraps its
//! tool stack in a [`ClaimTap`]. The moment a tool would mutate a path, the
//! tap acquires that path's cooperative lock in the workspace store
//! (`file_locks` — a single atomic SQLite upsert, sub-millisecond). Two
//! rules make it invalidation-free:
//!
//! 1. **Lock on first write.** Nothing is declared up front; the claim set
//!    is discovered as the work unfolds, so emergent edits (the file the
//!    model realizes mid-task it must touch) are covered exactly like
//!    planned ones. A conflict fails FAST and NAMES the holder — the model
//!    reads the refusal and adapts (different file, or retry) instead of
//!    silently clobbering a sibling.
//! 2. **Hold to turn end, release in one statement.** Claims release when
//!    the holder's turn/worker settles (`release_all`, one DELETE by
//!    holder), never mid-edit — a file a worker is iterating on stays its
//!    own for the whole attempt. Crash hygiene is age-based: a claim older
//!    than [`STALE_CLAIM_MAX_AGE_SECS`] is swept at session start (a dead
//!    process cannot release its own).
//!
//! The build lane is the one **transient** claim: `run_tests` /
//! `build_project` / `diagnostics` — and the manifest-verb executors that
//! can rewrite the tree (`run_lint`, `format_code`, `run_script`) — serialize under the
//! well-known pseudo-path [`BUILD_CLAIM`] for the duration of the call
//! only (with a bounded wait, not a hard refusal — build contention is
//! routine, edit contention is signal). This kills the phantom-failure
//! class where one worker's test run observes a sibling's half-written
//! edit (or a sibling formatter's half-rewritten tree).
//!
//! A missing/failed store degrades to no coordination rather than no work —
//! the same observability-loss-not-work-stoppage contract as every other
//! store write.
//!
//! Coverage note: with `bash` OFF by default (settings `tools.bash`), the
//! historically documented "bash hole" — arbitrary shell writes invisible
//! to claim tracking — is closed in the default configuration; it exists
//! only in sessions that explicitly opted the shell back in. (The
//! manifest-verb executors still write outside per-path claims, but they
//! run the project's own declared verbs, not arbitrary shell, and they
//! serialize under the build lane below.)

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use stella_core::ports::ToolExecutor;
use stella_protocol::{ToolOutput, ToolSchema};
use stella_store::Store;

/// The pseudo-path serializing build/test runs across every writer in the
/// workspace. Not a real file — `//` is unrepresentable as a workspace-
/// relative path, so it can never collide with a genuine claim.
pub(crate) const BUILD_CLAIM: &str = "//build";

/// How long a build/test call waits for the build lane before giving up.
const BUILD_WAIT_MS: u64 = 60_000;
/// Poll cadence while waiting for the build lane.
const BUILD_POLL_MS: u64 = 500;

/// Claims older than this are swept at session start (crash hygiene).
pub(crate) const STALE_CLAIM_MAX_AGE_SECS: u64 = 6 * 3600;

/// The tools whose successful call mutates the path in their `path` input.
/// In the DEFAULT configuration this coverage is complete for file writes:
/// `bash` — historically the documented hole, since a shell can write
/// anything unattributably — is no longer registered unless the workspace
/// opts in via settings (`tools.bash: "on"`). Only in an opted-in session
/// does the hole reopen; claims cover the structured file tools, and the
/// witness/verify ladder covers the rest.
fn mutating_path<'i>(name: &str, input: &'i Value) -> Option<&'i str> {
    if matches!(name, "write_file" | "edit_file" | "delete_file") {
        input.get("path").and_then(Value::as_str)
    } else {
        None
    }
}

/// Wraps a tool executor with claim-on-first-write (see the module doc).
pub(crate) struct ClaimTap<'a> {
    pub(crate) inner: &'a dyn ToolExecutor,
    /// The workspace store carrying `file_locks`. `None` = coordination
    /// disabled (no store), tools pass straight through.
    store: Option<Arc<Store>>,
    /// This writer's lock-table identity (`<session>/<lane>` or
    /// `<run>/<task>`) — what a rival's conflict error names.
    holder: String,
    /// Paths this tap already claimed (skip the store round-trip on the
    /// second write to the same file).
    held: std::sync::Mutex<HashSet<String>>,
}

impl<'a> ClaimTap<'a> {
    pub(crate) fn new(
        inner: &'a dyn ToolExecutor,
        store: Option<Arc<Store>>,
        holder: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            store,
            holder: holder.into(),
            held: std::sync::Mutex::new(HashSet::new()),
        }
    }

    /// Release every claim this tap acquired — called when the turn/worker
    /// settles (including cancellation; the drop-at-await future never gets
    /// to release, so the owner must).
    pub(crate) fn release_all(&self) {
        if let Some(store) = &self.store {
            let _ = store.release_file_locks_for_holder(&self.holder);
        }
        self.held.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    fn already_held(&self, path: &str) -> bool {
        self.held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(path)
    }

    fn mark_held(&self, path: &str) {
        self.held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(path.to_string());
    }
}

#[async_trait]
impl ToolExecutor for ClaimTap<'_> {
    fn schemas(&self) -> Vec<ToolSchema> {
        self.inner.schemas()
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        let Some(store) = &self.store else {
            return self.inner.execute(name, input).await;
        };

        // Lock on first write: refuse (naming the holder) instead of
        // clobbering a sibling's in-flight work.
        if let Some(path) = mutating_path(name, input)
            && !self.already_held(path)
        {
            match store.acquire_file_lock(path, &self.holder) {
                Ok(true) => self.mark_held(path),
                Ok(false) => {
                    let rival = store
                        .file_lock_holder(path)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "(released meanwhile)".to_string());
                    return ToolOutput::Error {
                        message: format!(
                            "`{path}` is currently claimed by `{rival}` — another agent is \
                             editing it right now. Work on a different file, or retry in a \
                             moment; the claim releases when that agent's turn ends."
                        ),
                    };
                }
                // Store trouble is observability loss, never a work
                // stoppage — proceed uncoordinated.
                Err(_) => {}
            }
        }

        // The build lane: transient, bounded-wait serialization so a test
        // run never observes a sibling's half-written tree — and a sibling
        // never observes a formatter/linter/script mid-rewrite.
        if matches!(
            name,
            "run_tests"
                | "build_project"
                | "diagnostics"
                | "run_lint"
                | "format_code"
                | "run_script"
        ) {
            let mut waited = 0u64;
            let acquired = loop {
                match store.acquire_file_lock(BUILD_CLAIM, &self.holder) {
                    Ok(true) => break true,
                    Ok(false) if waited < BUILD_WAIT_MS => {
                        tokio::time::sleep(std::time::Duration::from_millis(BUILD_POLL_MS)).await;
                        waited += BUILD_POLL_MS;
                    }
                    Ok(false) => {
                        let rival = store
                            .file_lock_holder(BUILD_CLAIM)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| "(released meanwhile)".to_string());
                        return ToolOutput::Error {
                            message: format!(
                                "the build/test lane has been held by `{rival}` for over \
                                 {}s — retry shortly",
                                BUILD_WAIT_MS / 1000
                            ),
                        };
                    }
                    // Degrade to an unserialized run rather than blocking
                    // real work on a broken store.
                    Err(_) => break false,
                }
            };
            let output = self.inner.execute(name, input).await;
            if acquired {
                let _ = store.release_file_lock(BUILD_CLAIM, &self.holder);
            }
            return output;
        }

        self.inner.execute(name, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording fake: succeeds every call and remembers what ran.
    struct Passthrough(std::sync::Mutex<Vec<String>>);

    #[async_trait]
    impl ToolExecutor for Passthrough {
        fn schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        async fn execute(&self, name: &str, _input: &Value) -> ToolOutput {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(name.to_string());
            ToolOutput::Ok {
                content: "ok".into(),
            }
        }
    }

    fn store() -> Arc<Store> {
        Arc::new(Store::in_memory().unwrap())
    }

    #[tokio::test]
    async fn first_write_claims_and_a_rival_is_refused_with_the_holder_named() {
        let store = store();
        let inner_a = Passthrough(std::sync::Mutex::new(Vec::new()));
        let inner_b = Passthrough(std::sync::Mutex::new(Vec::new()));
        let a = ClaimTap::new(&inner_a, Some(store.clone()), "ses-1/lead");
        let b = ClaimTap::new(&inner_b, Some(store.clone()), "ses-1/req:1");
        let input = serde_json::json!({ "path": "src/lib.rs", "content": "x" });

        assert!(!a.execute("write_file", &input).await.is_error());
        let refusal = b.execute("edit_file", &input).await;
        match refusal {
            ToolOutput::Error { message } => {
                assert!(message.contains("ses-1/lead"), "{message}");
                assert!(message.contains("src/lib.rs"), "{message}");
            }
            other => panic!("rival write must be refused, got {other:?}"),
        }
        // The refused call never reached the inner executor.
        assert!(inner_b.0.lock().unwrap().is_empty());

        // Release frees the path for the rival.
        a.release_all();
        assert!(!b.execute("edit_file", &input).await.is_error());
    }

    #[tokio::test]
    async fn repeat_writes_by_the_holder_pass_and_reads_are_never_gated() {
        let store = store();
        let inner = Passthrough(std::sync::Mutex::new(Vec::new()));
        let tap = ClaimTap::new(&inner, Some(store.clone()), "ses-1/lead");
        let input = serde_json::json!({ "path": "src/lib.rs" });
        assert!(!tap.execute("edit_file", &input).await.is_error());
        assert!(!tap.execute("edit_file", &input).await.is_error());
        // A read-only tool with a path input is not a mutation.
        assert!(!tap.execute("read_file", &input).await.is_error());
        assert_eq!(inner.0.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn build_claim_is_transient_and_released_after_the_call() {
        let store = store();
        let inner = Passthrough(std::sync::Mutex::new(Vec::new()));
        let tap = ClaimTap::new(&inner, Some(store.clone()), "ses-1/lead");
        let input = serde_json::json!({});
        // Every tree-rewriting executor rides the lane, not just tests —
        // and `diagnostics`, which rewrites nothing but must never observe
        // a sibling's half-written tree (phantom errors).
        for tool in [
            "run_tests",
            "build_project",
            "diagnostics",
            "run_lint",
            "format_code",
            "run_script",
        ] {
            assert!(!tap.execute(tool, &input).await.is_error());
            // Released immediately — a rival takes the lane without waiting.
            assert!(
                store.acquire_file_lock(BUILD_CLAIM, "ses-1/req:2").unwrap(),
                "build lane must be free after `{tool}`"
            );
            store.release_file_lock(BUILD_CLAIM, "ses-1/req:2").unwrap();
        }
    }

    #[tokio::test]
    async fn no_store_means_no_gating() {
        let inner = Passthrough(std::sync::Mutex::new(Vec::new()));
        let tap = ClaimTap::new(&inner, None, "ses-1/lead");
        let input = serde_json::json!({ "path": "src/lib.rs" });
        assert!(!tap.execute("write_file", &input).await.is_error());
    }
}
