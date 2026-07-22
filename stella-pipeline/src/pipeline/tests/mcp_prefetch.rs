//! The orchestrator MCP pre-fetch hook (issue #248 Phase 1): [`McpPrefetchPort::prefetch`]
//! is consulted once at the top of `run_best_of_n`, and its result — when
//! `Some` — rides in every candidate's shared message history rather than
//! each candidate independently paying to look it up.

use super::*;

/// A [`McpPrefetchPort`] that always returns a fixed sentinel string —
/// proves the orchestrator calls it and folds the result into the shared
/// history, independent of how the real CLI adapter gathers context.
struct FixedPrefetch(&'static str);

#[async_trait]
impl McpPrefetchPort for FixedPrefetch {
    async fn prefetch(&self, _goal: &str) -> Option<String> {
        Some(self.0.to_string())
    }
}

#[tokio::test]
async fn best_of_n_folds_the_mcp_prefetch_into_every_candidate() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("cand0 done"),
        text_result("cand1 done"),
    ]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![false, false, false, true], "@@ -1 +1 @@\n-a\n+b");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let prefetch = FixedPrefetch("SENTINEL-SHARED-CONTEXT");
    let (tx, _rx) = mpsc::unbounded_channel();
    let pipeline = Pipeline::new(
        PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall: &recall,
            repo: &repo,
            repo_status: &repo_status,
            diagnostics: &runner,
            tests: &runner,
            approvals: &approvals,
            sleeper: &sleeper,
            hooks: None,
            candidate_workspaces: None,
            mcp_prefetch: Some(&prefetch),
            steering: None,
        },
        tx,
        isolated_config(2),
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .expect("run succeeds");
    assert!(
        messages
            .iter()
            .any(|m| m.content.contains("SENTINEL-SHARED-CONTEXT")),
        "the once-fetched context must ride in the winning candidate's history: {messages:?}"
    );
}

/// A prefetch miss (`None`) must never abort the run — best-of-N proceeds
/// exactly as if no [`McpPrefetchPort`] were wired at all.
#[tokio::test]
async fn a_prefetch_miss_never_aborts_the_run() {
    struct EmptyPrefetch;
    #[async_trait]
    impl McpPrefetchPort for EmptyPrefetch {
        async fn prefetch(&self, _goal: &str) -> Option<String> {
            None
        }
    }

    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("cand0 done"),
        text_result("cand1 done"),
    ]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![false, false, false, true], "@@ -1 +1 @@\n-a\n+b");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let prefetch = EmptyPrefetch;
    let (tx, _rx) = mpsc::unbounded_channel();
    let pipeline = Pipeline::new(
        PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall: &recall,
            repo: &repo,
            repo_status: &repo_status,
            diagnostics: &runner,
            tests: &runner,
            approvals: &approvals,
            sleeper: &sleeper,
            hooks: None,
            candidate_workspaces: None,
            mcp_prefetch: Some(&prefetch),
            steering: None,
        },
        tx,
        isolated_config(2),
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let outcome = pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .expect("a prefetch miss must not fail the run");
    assert_eq!(outcome.status, PipelineStatus::Completed);
}
