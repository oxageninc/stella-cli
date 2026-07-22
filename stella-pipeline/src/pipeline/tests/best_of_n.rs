//! Best-of-N candidate isolation tests — split out of `tests.rs` to keep it
//! under the file-size ratchet; a child module, so it reaches `run_isolated`,
//! `isolated_config`, and the other fakes above via `super::*`. The shared
//! infrastructure itself (`run_isolated`, `isolated_config`,
//! `FakeWorkspacePort`, `FakeWorkspace`) stays in `tests.rs` — `mcp_prefetch`
//! and `task5` also depend on it, so it must live in the common ancestor.

use super::*;

/// The core best-of-N isolation contract: every candidate runs against
/// its own workspace surface (the session ports panic if touched), only
/// the winner is adopted, and every workspace is removed.
#[tokio::test]
async fn best_of_two_adopts_only_the_winner_and_removes_every_workspace() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("cand0 done"),
        text_result("cand1 done"),
    ]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = FakeWorkspacePort::new(
        vec![
            // Candidate 0: baseline fail, still failing → Failed.
            Ok(FakeWorkspace::new(
                0,
                vec![false, false],
                Ok(vec![]),
                log.clone(),
            )),
            // Candidate 1: fail→pass flip → DeterministicPass (winner).
            Ok(FakeWorkspace::new(
                1,
                vec![false, true],
                Ok(vec![AdoptedChange {
                    path: "src/x.rs".into(),
                    kind: FileChangeKind::Modified,
                }]),
                log.clone(),
            )),
        ],
        log.clone(),
    );

    let (outcome, events, messages) =
        run_isolated(&provider, &port, isolated_config(2), "Fix the failing test").await;
    let outcome = outcome.expect("run succeeds");
    assert_eq!(outcome.status, PipelineStatus::Completed);
    assert_eq!(outcome.candidates_run, 2);
    assert_eq!(outcome.final_text, "cand1 done");
    let verdict = outcome.verdict.expect("winner verified");
    assert!(verdict.passed && verdict.deterministic);
    // The winner's trajectory (not the loser's) was adopted.
    assert!(messages.iter().any(|m| m.content == "cand1 done"));

    let log = log.lock().unwrap().clone();
    assert_eq!(
        log.iter().filter(|e| *e == "create").count(),
        2,
        "one snapshot per candidate: {log:?}"
    );
    assert_eq!(
        log.iter()
            .filter(|e| e.starts_with("adopt"))
            .collect::<Vec<_>>(),
        vec!["adopt:1"],
        "only the winner is ever adopted: {log:?}"
    );
    assert!(
        log.contains(&"remove:0".to_string()) && log.contains(&"remove:1".to_string()),
        "every workspace is removed after the run: {log:?}"
    );
    // The adopted paths surface as FileChange events (the session's file
    // tracking never saw the winner's in-snapshot edits).
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::FileChange { path, kind: FileChangeKind::Modified, .. }
                if path == "src/x.rs"
        )),
        "winner adoption must emit FileChange for adopted paths"
    );
}

/// The default single-shot path must never touch the workspace port —
/// zero snapshot machinery when `candidates` is unset.
#[tokio::test]
async fn single_shot_never_touches_the_candidate_workspace_port() {
    let provider = ScriptedProvider::new(vec![text_result("single"), text_result("done")]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![false, true], "@@ -1 +1 @@\n-a\n+b");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let port = FakeWorkspacePort::untouchable();
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
            candidate_workspaces: Some(&port),
            mcp_prefetch: None,
            steering: None,
        },
        tx,
        PipelineConfig {
            test_command: Some("cargo test".into()),
            ..PipelineConfig::default()
        },
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let outcome = pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .expect("run succeeds");
    assert_eq!(outcome.status, PipelineStatus::Completed);
    assert_eq!(outcome.candidates_run, 1);
}

/// A candidate whose snapshot fails is scored as aborted — never run in
/// the shared tree instead — and the remaining candidates continue.
#[tokio::test]
async fn a_failed_snapshot_scores_an_aborted_candidate_and_the_run_continues() {
    // Only ONE worker turn is scripted: candidate 0 must never execute.
    let provider = ScriptedProvider::new(vec![text_result("single"), text_result("cand1 done")]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = FakeWorkspacePort::new(
        vec![
            Err(WorkspaceError::Snapshot {
                reason: "disk full".into(),
            }),
            Ok(FakeWorkspace::new(
                1,
                vec![false, true],
                Ok(vec![]),
                log.clone(),
            )),
        ],
        log.clone(),
    );

    let (outcome, events, _) =
        run_isolated(&provider, &port, isolated_config(2), "Fix the failing test").await;
    let outcome = outcome.expect("run succeeds");
    assert_eq!(outcome.status, PipelineStatus::Completed);
    assert_eq!(outcome.final_text, "cand1 done");
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Error { message, retryable: true } if message.contains("skipped")
        )),
        "the skipped candidate is warned about, never silent"
    );
    let log = log.lock().unwrap().clone();
    assert_eq!(
        log,
        vec!["create", "create", "seal:1", "adopt:1", "remove:1"],
        "no adoption or removal for the never-created workspace"
    );
}

/// A winner whose adoption conflicts (the user edited mid-run) aborts the
/// run loudly — naming the conflicting paths — and preserves the winner's
/// workspace while still removing the losers'.
#[tokio::test]
async fn an_adoption_conflict_aborts_loudly_and_preserves_the_winner_workspace() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("cand0 done"),
        text_result("cand1 done"),
    ]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = FakeWorkspacePort::new(
        vec![
            Ok(FakeWorkspace::new(
                0,
                vec![false, false],
                Ok(vec![]),
                log.clone(),
            )),
            Ok(FakeWorkspace::new(
                1,
                vec![false, true],
                Err(WorkspaceError::Adopt {
                    reason: "`git apply` rejected the patch".into(),
                    paths: vec!["src/conflict.rs".into()],
                    workspace: "/tmp/stella_candidate_ws1".into(),
                }),
                log.clone(),
            )),
        ],
        log.clone(),
    );

    let (outcome, _, _) =
        run_isolated(&provider, &port, isolated_config(2), "Fix the failing test").await;
    let outcome = outcome.expect("an adoption conflict is a loud abort, not a panic");
    match &outcome.status {
        PipelineStatus::Aborted { reason } => {
            assert!(
                reason.contains("src/conflict.rs"),
                "the abort names the conflicting paths: {reason}"
            );
            assert!(
                reason.contains("/tmp/stella_candidate_ws1"),
                "the abort names the preserved workspace: {reason}"
            );
        }
        other => panic!("expected an aborted run, got {other:?}"),
    }
    let log = log.lock().unwrap().clone();
    assert!(
        log.contains(&"remove:0".to_string()),
        "losers are removed: {log:?}"
    );
    assert!(
        !log.contains(&"remove:1".to_string()),
        "the winner's workspace is preserved for recovery: {log:?}"
    );
}

/// Without a workspace port, best-of-N degrades to the historical
/// shared-tree behavior — and says so out loud.
#[tokio::test]
async fn best_of_n_without_a_port_degrades_to_the_shared_tree_with_a_warning() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("cand0 done"),
        text_result("cand1 done"),
    ]);
    let resolver = OneProvider(&provider);
    // One shared runner serves both candidates back-to-back.
    let runner = ScriptedRunner::new(vec![false, false, false, true], "@@ -1 +1 @@\n-a\n+b");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let (tx, mut rx) = mpsc::unbounded_channel();
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
            mcp_prefetch: None,
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
        .expect("run succeeds");
    assert_eq!(outcome.status, PipelineStatus::Completed);
    assert_eq!(outcome.candidates_run, 2);
    assert_eq!(outcome.final_text, "cand1 done");
    assert!(
        drain(&mut rx).iter().any(|e| matches!(
            e,
            AgentEvent::Error { message, retryable: true }
                if message.contains("shared working tree")
        )),
        "shared-tree degradation must be loud"
    );
}
