//! Witness-isolation and failed-adoption regression tests.

use super::*;
use stella_core::hooks::{
    HookAction, HookExecError, HookExecResult, HookMatcher, HookRunner, Hooks,
};
use stella_protocol::ToolCall;

struct NeverTools;

#[async_trait]
impl ToolExecutor for NeverTools {
    fn schemas(&self) -> Vec<ToolSchema> {
        Vec::new()
    }

    async fn execute(&self, name: &str, _input: &Value) -> ToolOutput {
        panic!("the session ToolExecutor must not serve isolated candidates (got `{name}`)");
    }
}

fn tool_result(name: &str) -> CompletionResult {
    CompletionResult {
        text: String::new(),
        tool_calls: vec![ToolCall {
            call_id: format!("call-{name}"),
            name: name.into(),
            input: serde_json::json!({}),
        }],
        usage: CompletionUsage::default(),
        model: "scripted".into(),
        cost_usd: 0.0001,
        finish_reason: None,
    }
}

#[derive(Default)]
struct RecordingHookRunner {
    cwd: std::sync::Mutex<Vec<String>>,
    payload_cwd: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl HookRunner for RecordingHookRunner {
    async fn run(
        &self,
        _action: &HookAction,
        payload_json: &str,
        cwd: &str,
    ) -> Result<HookExecResult, HookExecError> {
        let mut calls = self.cwd.lock().unwrap();
        std::fs::create_dir_all(cwd).unwrap();
        std::fs::write(
            std::path::Path::new(cwd).join(format!("hook-{}", calls.len())),
            b"candidate hook",
        )
        .unwrap();
        calls.push(cwd.to_string());
        let payload: serde_json::Value = serde_json::from_str(payload_json).unwrap();
        self.payload_cwd
            .lock()
            .unwrap()
            .push(payload["cwd"].as_str().unwrap().to_string());
        Ok(HookExecResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

#[tokio::test]
async fn witness_worker_and_revision_hooks_are_bound_to_the_candidate_root() {
    let candidate_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        tool_result("write_file"),
        text_result(
            "TEST_COMMAND: cargo test --test authority_witness authority_witness -- --exact",
        ),
        tool_result("write_file"),
        text_result("worker done"),
        tool_result("write_file"),
        text_result("revision done"),
    ]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace = FakeWorkspace::new(0, vec![false, false, false, true], Ok(vec![]), log.clone())
        .with_root(candidate_root.path().display().to_string())
        .with_repo_status(SeqRepoStatus::new(vec![
            vec![],
            vec![("tests/authority_witness.rs", "sha256:test")],
        ]));
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log);
    let resolver = OneProvider(&provider);
    let session_runner = NeverRunner;
    let session_status = NeverRepoStatus;
    let tools = NeverTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let hooks = Hooks {
        pre_tool_use: Some(vec![HookMatcher {
            matcher: Some("*".into()),
            hooks: vec![HookAction::new("record cwd")],
        }]),
        post_tool_use: None,
        session_start: None,
    };
    let hook_runner = RecordingHookRunner::default();
    let (tx, _rx) = mpsc::unbounded_channel();
    let pipeline = Pipeline::new(
        PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall: &recall,
            repo: &repo,
            repo_status: &session_status,
            diagnostics: &session_runner,
            tests: &session_runner,
            approvals: &approvals,
            sleeper: &sleeper,
            hooks: Some((&hooks, &hook_runner)),
            candidate_workspaces: Some(&port),
            steering: None,
        },
        tx,
        PipelineConfig {
            max_revisions: 1,
            engine: EngineConfig {
                cwd: session_root.path().display().to_string(),
                ..EngineConfig::default()
            },
            ..PipelineConfig::default()
        },
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);

    let outcome = pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .expect("pipeline runs");
    assert_eq!(outcome.status, PipelineStatus::Completed);
    let cwd = hook_runner.cwd.lock().unwrap().clone();
    assert_eq!(
        cwd.len(),
        2,
        "witness authoring cannot run hooks; worker and revision each fire once"
    );
    assert!(
        cwd.iter()
            .all(|path| path == &candidate_root.path().display().to_string()),
        "{cwd:?}"
    );
    let payload_cwd = hook_runner.payload_cwd.lock().unwrap().clone();
    assert_eq!(payload_cwd, cwd, "hook payload and process cwd must agree");
    assert_eq!(std::fs::read_dir(candidate_root.path()).unwrap().count(), 2);
    assert_eq!(std::fs::read_dir(session_root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn authored_witness_fails_closed_before_workspace_creation_when_judge_is_worker() {
    let provider = ScriptedProvider::new(vec![text_result("single")]);
    let resolver = OneProvider(&provider);
    let diagnostics = NeverRunner;
    let repo_status = NeverRepoStatus;
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let port = FakeWorkspacePort::untouchable();
    let same = ModelRef::new("scripted", "same-model");
    let mut roles = RoleTable::new();
    roles.pin(Role::Worker, same.clone());
    roles.pin(Role::Judge, same);
    let router = Router::new(
        roles,
        vec![ProviderProfile::new(
            "scripted",
            ModelRef::new("scripted", "worker"),
            ModelRef::new("scripted", "triage"),
            ModelRef::new("scripted", "judge"),
        )],
        CircuitBreaker::new(Box::new(ZeroClock)),
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let pipeline = Pipeline::new(
        PipelinePorts {
            router: &router,
            providers: &resolver,
            tools: &tools,
            recall: &recall,
            repo: &repo,
            repo_status: &repo_status,
            diagnostics: &diagnostics,
            tests: &diagnostics,
            approvals: &approvals,
            sleeper: &sleeper,
            hooks: None,
            candidate_workspaces: Some(&port),
            steering: None,
        },
        tx,
        PipelineConfig::default(),
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);

    let outcome = pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .expect("independence failure is a truthful candidate abort");
    assert!(matches!(
        outcome.status,
        PipelineStatus::Aborted { ref reason }
            if reason.contains("independent witness author")
    ));
    assert!(!stages(&drain(&mut rx)).contains(&StageKind::Witness));
}

#[tokio::test]
async fn authored_witness_with_one_candidate_uses_one_disposable_snapshot() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result(
            "TEST_COMMAND: cargo test --test authority_witness authority_witness -- --exact",
        ),
        text_result("done"),
    ]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace = FakeWorkspace::new(0, vec![false, false, true], Ok(vec![]), log.clone())
        .with_repo_status(SeqRepoStatus::new(vec![
            vec![],
            vec![("tests/authority_witness.rs", "sha256:test")],
        ]));
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log.clone());

    let (outcome, _, _) = run_isolated(
        &provider,
        &port,
        PipelineConfig {
            candidates: Some(1),
            ..PipelineConfig::default()
        },
        "Fix the failing test",
    )
    .await;
    let outcome = outcome.expect("run succeeds inside the snapshot");
    assert_eq!(outcome.status, PipelineStatus::Completed);
    assert_eq!(
        *log.lock().unwrap(),
        vec!["create", "seal:0", "adopt:0", "remove:0"],
        "authoring, worker verification, and adoption share one workspace"
    );
}

#[tokio::test]
async fn sealed_witness_identity_survives_git_reclassification_out_of_untracked_files() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result(
            "TEST_COMMAND: cargo test --test authority_witness authority_witness -- --exact",
        ),
        text_result("done"),
    ]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let identity = ArtifactIdentity {
        fingerprint: "sha256:test".into(),
        kind: ArtifactKind::Regular,
        mode: 0o100644,
        link_count: 1,
    };
    let status = SeqRepoStatus::new(vec![
        vec![],
        vec![("tests/authority_witness.rs", "sha256:test")],
        vec![("tests/authority_witness.rs", "sha256:test")],
        vec![("tests/authority_witness.rs", "sha256:test")],
        // `git add -A && git commit` in `seal()` makes the accepted witness
        // tracked. Classification changes; filesystem identity does not.
        vec![],
    ])
    .with_artifact_identity(identity);
    let workspace = FakeWorkspace::new(0, vec![false, false, true], Ok(vec![]), log.clone())
        .with_repo_status(status);
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log.clone());

    let (outcome, _, _) = run_isolated(
        &provider,
        &port,
        PipelineConfig {
            candidates: Some(1),
            ..PipelineConfig::default()
        },
        "Fix the failing test",
    )
    .await;

    let outcome = outcome.expect("classification change is not witness tamper");
    assert_eq!(outcome.status, PipelineStatus::Completed);
    assert_eq!(
        *log.lock().unwrap(),
        vec!["create", "seal:0", "adopt:0", "remove:0"]
    );
}

#[tokio::test]
async fn authored_witness_isolation_failure_aborts_before_authoring() {
    let provider = ScriptedProvider::new(vec![text_result("single")]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = FakeWorkspacePort::new(
        vec![Err(WorkspaceError::Snapshot {
            reason: "no isolated worktree".into(),
        })],
        log.clone(),
    );

    let (outcome, events, _) = run_isolated(
        &provider,
        &port,
        PipelineConfig::default(),
        "Fix the failing test",
    )
    .await;
    let outcome = outcome.expect("isolation failure is a truthful abort");
    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    assert!(!stages(&events).contains(&StageKind::Witness));
    assert_eq!(*log.lock().unwrap(), vec!["create"]);
}

#[tokio::test]
async fn tracked_production_edit_by_witness_author_aborts_without_adoption() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result(
            "TEST_COMMAND: cargo test --test authority_witness authority_witness -- --exact",
        ),
    ]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let status = SeqRepoStatus::new(vec![
        vec![],
        vec![("tests/authority_witness.rs", "sha256:test")],
    ])
    .with_tracked(vec![vec![], vec![("src/lib.rs", "sha256:mutated")]]);
    let workspace = FakeWorkspace::new(0, vec![], Ok(vec![]), log.clone()).with_repo_status(status);
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log.clone());

    let (outcome, _, _) = run_isolated(
        &provider,
        &port,
        PipelineConfig::default(),
        "Fix the failing test",
    )
    .await;
    let outcome = outcome.expect("author mutation is an aborted candidate");
    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    let log = log.lock().unwrap().clone();
    assert!(!log.iter().any(|entry| entry.starts_with("adopt:")));
    assert!(log.contains(&"remove:0".to_string()));
}

#[tokio::test]
async fn witness_language_mismatch_aborts_before_worker_execution() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result(
            "TEST_COMMAND: cargo test --test authority_witness authority_witness -- --exact",
        ),
        text_result("worker done"),
    ]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace = FakeWorkspace::new(0, vec![false, false, true], Ok(vec![]), log.clone())
        .with_repo_status(SeqRepoStatus::new(vec![
            vec![],
            vec![("tests/test_authority.py", "sha256:test")],
        ]));
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log.clone());

    let (outcome, _, _) = run_isolated(
        &provider,
        &port,
        PipelineConfig::default(),
        "Fix the failing test",
    )
    .await;
    let outcome = outcome.expect("mismatch is a truthful candidate abort");
    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    assert_eq!(*log.lock().unwrap(), vec!["create", "remove:0"]);
}

#[tokio::test]
async fn symlink_witness_artifact_aborts_before_worker_execution() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result(
            "TEST_COMMAND: cargo test --test authority_witness authority_witness -- --exact",
        ),
        text_result("worker done"),
    ]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let status = SeqRepoStatus::new(vec![
        vec![],
        vec![("tests/authority_witness.rs", "sha256:symlink")],
    ])
    .with_artifact_identity(ArtifactIdentity {
        fingerprint: "sha256:symlink".into(),
        kind: ArtifactKind::Symlink,
        mode: 0o120777,
        link_count: 1,
    });
    let workspace = FakeWorkspace::new(0, vec![false, false, true], Ok(vec![]), log.clone())
        .with_repo_status(status);
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log.clone());

    let (outcome, _, _) = run_isolated(
        &provider,
        &port,
        PipelineConfig::default(),
        "Fix the failing test",
    )
    .await;
    let outcome = outcome.expect("symlink is a truthful candidate abort");
    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    assert_eq!(*log.lock().unwrap(), vec!["create", "remove:0"]);
}

#[tokio::test]
async fn post_baseline_witness_tamper_is_hard_failure_even_if_judge_would_pass() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("TEST_COMMAND: cargo test --test witness witness -- --exact"),
        text_result("worker done"),
        text_result("PASS override attempt"),
    ]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let status = SeqRepoStatus::new(vec![
        vec![],
        vec![("tests/witness.rs", "w1")],
        vec![("tests/witness.rs", "w1")],
        vec![("tests/witness.rs", "w1")],
        vec![("tests/witness.rs", "w2")],
    ])
    .with_artifact_identities(vec![
        Some(ArtifactIdentity {
            fingerprint: "w1".into(),
            kind: ArtifactKind::Regular,
            mode: 0o100644,
            link_count: 1,
        }),
        Some(ArtifactIdentity {
            fingerprint: "w2".into(),
            kind: ArtifactKind::Regular,
            mode: 0o100644,
            link_count: 1,
        }),
    ]);
    let workspace = FakeWorkspace::new(0, vec![false, false, true], Ok(vec![]), log.clone())
        .with_repo_status(status);
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log.clone());

    let (outcome, events, _) = run_isolated(
        &provider,
        &port,
        PipelineConfig {
            max_revisions: 0,
            ..PipelineConfig::default()
        },
        "Fix the retry bug",
    )
    .await;
    let outcome = outcome.expect("tamper is a truthful candidate failure");
    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    assert!(!stages(&events).contains(&StageKind::Judge));
    let log = log.lock().unwrap().clone();
    assert!(
        !log.iter().any(|entry| entry.starts_with("adopt:")),
        "{log:?}"
    );
    assert!(log.contains(&"remove:0".to_string()));
}

#[tokio::test]
async fn failed_final_verification_never_adopts_and_removes_all_candidates() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("candidate zero"),
        text_result("candidate one"),
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
                vec![false, false],
                Ok(vec![]),
                log.clone(),
            )),
        ],
        log.clone(),
    );

    let (outcome, _, _) =
        run_isolated(&provider, &port, isolated_config(2), "Fix the failing test").await;
    let outcome = outcome.expect("red verification is a terminal outcome");
    assert!(matches!(
        outcome.status,
        PipelineStatus::VerificationFailed { .. }
    ));
    let log = log.lock().unwrap().clone();
    assert!(
        !log.iter().any(|entry| entry.starts_with("adopt:")),
        "{log:?}"
    );
    assert!(log.contains(&"remove:0".to_string()));
    assert!(log.contains(&"remove:1".to_string()));
}

#[tokio::test]
async fn post_verification_candidate_drift_is_rejected_before_adoption() {
    let provider = ScriptedProvider::new(vec![text_result("single"), text_result("done")]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace = FakeWorkspace::new(0, vec![false, true], Ok(vec![]), log.clone())
        .with_post_verification_drift();
    let port = FakeWorkspacePort::new(
        vec![
            Ok(workspace),
            Err(WorkspaceError::Snapshot {
                reason: "second candidate unavailable".into(),
            }),
        ],
        log.clone(),
    );

    let (outcome, _, _) =
        run_isolated(&provider, &port, isolated_config(2), "Fix the failing test").await;
    let outcome = outcome.expect("drift is a truthful candidate failure");
    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    let log = log.lock().unwrap().clone();
    assert!(log.contains(&"seal:0".to_string()), "{log:?}");
    assert!(
        !log.iter().any(|entry| entry.starts_with("adopt:")),
        "{log:?}"
    );
    assert!(log.contains(&"remove:0".to_string()));
}
