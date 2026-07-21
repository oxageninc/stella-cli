//! Unit tests for [`super`] — split out of `pipeline.rs` to keep the
//! orchestrator under the file-size ratchet; a child module, so the
//! private surface (`CandidateSurface`, `Pipeline::gather_diff`, ...)
//! stays reachable via `super::*`.

use super::*;
use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use stella_core::router::{CircuitBreaker, ProviderProfile, RoleTable};
use stella_core::{Clock, ToolExecutor};
use stella_protocol::event::BudgetMode;
use stella_protocol::{
    CompletionRequest, CompletionUsage, FileChangeKind, MessageRole, ProviderError, ScopeProposal,
    ToolOutput, ToolSchema,
};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;

use crate::ports::{
    AdoptedChange, ArtifactIdentity, ArtifactKind, AutoApproveGate, CmdOutcome,
    DiagnosticInvocation, DiagnosticRunner, NoContextRecall, NoRepoStatus, NoRepoStructure,
    TestInvocation, TestRunner,
};

// ---- test doubles ---------------------------------------------------

/// A [`RepoStatusPort`] returning a fixed untracked `path -> fingerprint`
/// map — the "after execute" snapshot `gather_diff` diffs against a
/// caller-supplied before-snapshot.
struct FakeRepoStatus {
    files: HashMap<String, String>,
}
impl FakeRepoStatus {
    fn new(files: Vec<(&str, &str)>) -> Self {
        Self {
            files: files
                .into_iter()
                .map(|(p, fp)| (p.to_string(), fp.to_string()))
                .collect(),
        }
    }
}
#[async_trait]
impl RepoStatusPort for FakeRepoStatus {
    async fn untracked_fingerprints(&self) -> HashMap<String, String> {
        self.files.clone()
    }
}

/// A [`RepoStatusPort`] serving a scripted SEQUENCE of snapshots — one per
/// `untracked_fingerprints` call, holding the last once exhausted. Lets a
/// test make the working tree "change" between the witness stage, the
/// execute turn, and the tamper check.
struct SeqRepoStatus {
    snapshots: std::sync::Mutex<VecDeque<HashMap<String, String>>>,
    last: std::sync::Mutex<HashMap<String, String>>,
    tracked_snapshots: std::sync::Mutex<VecDeque<HashMap<String, String>>>,
    tracked_last: std::sync::Mutex<HashMap<String, String>>,
    artifact_identity: Option<ArtifactIdentity>,
    artifact_identities: std::sync::Mutex<VecDeque<Option<ArtifactIdentity>>>,
}
impl SeqRepoStatus {
    fn new(snapshots: Vec<Vec<(&str, &str)>>) -> Self {
        let mapped: VecDeque<HashMap<String, String>> = snapshots
            .into_iter()
            .map(|files| {
                files
                    .into_iter()
                    .map(|(p, fp)| (p.to_string(), fp.to_string()))
                    .collect()
            })
            .collect();
        Self {
            snapshots: std::sync::Mutex::new(mapped),
            last: std::sync::Mutex::new(HashMap::new()),
            tracked_snapshots: std::sync::Mutex::new(VecDeque::new()),
            tracked_last: std::sync::Mutex::new(HashMap::new()),
            artifact_identity: None,
            artifact_identities: std::sync::Mutex::new(VecDeque::new()),
        }
    }

    fn with_tracked(mut self, snapshots: Vec<Vec<(&str, &str)>>) -> Self {
        self.tracked_snapshots = std::sync::Mutex::new(
            snapshots
                .into_iter()
                .map(|files| {
                    files
                        .into_iter()
                        .map(|(p, fp)| (p.to_string(), fp.to_string()))
                        .collect()
                })
                .collect(),
        );
        self
    }

    fn with_artifact_identity(mut self, identity: ArtifactIdentity) -> Self {
        self.artifact_identity = Some(identity);
        self
    }

    fn with_artifact_identities(self, identities: Vec<Option<ArtifactIdentity>>) -> Self {
        *self.artifact_identities.lock().unwrap() = identities.into();
        self
    }
}
#[async_trait]
impl RepoStatusPort for SeqRepoStatus {
    async fn untracked_fingerprints(&self) -> HashMap<String, String> {
        let mut q = self.snapshots.lock().unwrap();
        match q.pop_front() {
            Some(next) => {
                *self.last.lock().unwrap() = next.clone();
                next
            }
            None => self.last.lock().unwrap().clone(),
        }
    }

    async fn tracked_fingerprints(&self) -> HashMap<String, String> {
        let mut q = self.tracked_snapshots.lock().unwrap();
        match q.pop_front() {
            Some(next) => {
                *self.tracked_last.lock().unwrap() = next.clone();
                next
            }
            None => self.tracked_last.lock().unwrap().clone(),
        }
    }

    async fn artifact_identity(&self, path: &str) -> Option<ArtifactIdentity> {
        if let Some(identity) = self.artifact_identities.lock().unwrap().pop_front() {
            return identity;
        }
        self.artifact_identity.clone().or_else(|| {
            self.last
                .lock()
                .unwrap()
                .get(path)
                .map(|fingerprint| ArtifactIdentity {
                    fingerprint: fingerprint.clone(),
                    kind: ArtifactKind::Regular,
                    mode: 0o100644,
                    link_count: 1,
                })
        })
    }
}

/// A scripted provider serving triage, plan, worker, and judge calls from
/// one ordered queue (the resolver hands the same provider to every role).
struct ScriptedProvider {
    script: TokioMutex<VecDeque<CompletionResult>>,
}
impl ScriptedProvider {
    fn new(results: Vec<CompletionResult>) -> Self {
        Self {
            script: TokioMutex::new(results.into_iter().collect()),
        }
    }
}
#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        let mut q = self.script.lock().await;
        q.pop_front()
            .ok_or_else(|| ProviderError::Terminal("scripted provider exhausted".into()))
    }
}

/// A [`TurnSteering`] that hands out its queue on the first drain and never
/// soft-stops — the witness that a queued steer reaches the execute engine.
#[derive(Default)]
struct SteeringOnce {
    queued: std::sync::Mutex<Vec<String>>,
    drains: std::sync::atomic::AtomicU32,
}
impl stella_core::ports::TurnSteering for SteeringOnce {
    fn drain_steering(&self) -> Vec<String> {
        self.drains
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::mem::take(&mut *self.queued.lock().unwrap())
    }
    fn soft_stop_requested(&self) -> bool {
        false
    }
}

/// A resolver that returns the one scripted provider for every model.
struct OneProvider<'p>(&'p ScriptedProvider);
impl ProviderResolver for OneProvider<'_> {
    fn provider_for(&self, _model: &ModelRef) -> Option<&dyn Provider> {
        Some(self.0)
    }
}

/// A command runner: the git diff command returns a fixed diff; `ls-files`
/// reports the scripted untracked set; a `--no-index --numstat` reports
/// that file's scripted added-line count; anything else pops the next
/// queued test result (`true` = pass) or defaults to pass.
struct ScriptedRunner {
    test_results: std::sync::Mutex<VecDeque<bool>>,
    diff: String,
    /// Untracked files this workspace reports, as `(path, added_lines)`.
    untracked: Vec<(String, u32)>,
}
impl ScriptedRunner {
    fn new(test_results: Vec<bool>, diff: &str) -> Self {
        Self {
            test_results: std::sync::Mutex::new(test_results.into_iter().collect()),
            diff: diff.to_string(),
            untracked: Vec::new(),
        }
    }
    fn with_untracked(mut self, untracked: Vec<(&str, u32)>) -> Self {
        self.untracked = untracked
            .into_iter()
            .map(|(p, n)| (p.to_string(), n))
            .collect();
        self
    }
}
#[async_trait]
impl DiagnosticRunner for ScriptedRunner {
    async fn run_diagnostic(&self, invocation: &DiagnosticInvocation) -> CmdOutcome {
        if let DiagnosticInvocation::UntrackedNumstat { path } = invocation {
            let numstat = self
                .untracked
                .iter()
                .find(|(candidate, _)| candidate == path)
                .map(|(p, n)| format!("{n}\t0\t{p}"))
                .unwrap_or_default();
            return CmdOutcome {
                exit_code: if numstat.is_empty() { 0 } else { 1 },
                stdout_tail: numstat,
                stderr_tail: String::new(),
            };
        }
        if matches!(invocation, DiagnosticInvocation::GitDiff) {
            return CmdOutcome {
                exit_code: 0,
                stdout_tail: self.diff.clone(),
                stderr_tail: String::new(),
            };
        }
        CmdOutcome {
            exit_code: 0,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
        }
    }
}

#[async_trait]
impl TestRunner for ScriptedRunner {
    async fn run_test(&self, _invocation: &TestInvocation) -> CmdOutcome {
        let passed = self
            .test_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(true);
        CmdOutcome {
            exit_code: if passed { 0 } else { 1 },
            stdout_tail: String::new(),
            stderr_tail: if passed {
                String::new()
            } else {
                "test failed".into()
            },
        }
    }
}

struct EmptyTools;
#[async_trait]
impl ToolExecutor for EmptyTools {
    fn schemas(&self) -> Vec<ToolSchema> {
        Vec::new()
    }
    async fn execute(&self, _name: &str, _input: &Value) -> ToolOutput {
        ToolOutput::Ok {
            content: String::new(),
        }
    }
}

/// Session ports that fail the test if touched — the witness that
/// isolated candidates only ever reach their own workspace's surface,
/// never the real tree's.
struct NeverRunner;
#[async_trait]
impl DiagnosticRunner for NeverRunner {
    async fn run_diagnostic(&self, invocation: &DiagnosticInvocation) -> CmdOutcome {
        panic!(
            "the session DiagnosticRunner must not serve isolated candidates (got {invocation:?})"
        );
    }
}
#[async_trait]
impl TestRunner for NeverRunner {
    async fn run_test(&self, invocation: &TestInvocation) -> CmdOutcome {
        panic!("the session TestRunner must not serve isolated candidates (got {invocation:?})");
    }
}
struct NeverRepoStatus;
#[async_trait]
impl RepoStatusPort for NeverRepoStatus {
    async fn untracked_fingerprints(&self) -> HashMap<String, String> {
        panic!("the session RepoStatusPort must not serve isolated candidates");
    }
}

/// A scripted [`CandidateWorkspace`]: per-candidate command results, a
/// canned adoption outcome, and a shared log of lifecycle calls.
struct FakeWorkspace {
    id: usize,
    root: String,
    tools: EmptyTools,
    diagnostics: ScriptedRunner,
    repo_status: SeqRepoStatus,
    adopt_result: Result<Vec<AdoptedChange>, WorkspaceError>,
    sealed_unchanged: bool,
    log: Arc<std::sync::Mutex<Vec<String>>>,
}

impl FakeWorkspace {
    fn new(
        id: usize,
        test_results: Vec<bool>,
        adopt_result: Result<Vec<AdoptedChange>, WorkspaceError>,
        log: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            id,
            root: "/candidate/workspace".into(),
            tools: EmptyTools,
            diagnostics: ScriptedRunner::new(test_results, "@@ -1 +1 @@\n-a\n+b"),
            repo_status: SeqRepoStatus::new(vec![]),
            adopt_result,
            sealed_unchanged: true,
            log,
        }
    }

    fn with_repo_status(mut self, repo_status: SeqRepoStatus) -> Self {
        self.repo_status = repo_status;
        self
    }

    fn with_post_verification_drift(mut self) -> Self {
        self.sealed_unchanged = false;
        self
    }

    fn with_root(mut self, root: impl Into<String>) -> Self {
        self.root = root.into();
        self
    }
}

#[async_trait]
impl CandidateWorkspace for FakeWorkspace {
    fn root(&self) -> &str {
        &self.root
    }
    fn tools(&self) -> &dyn ToolExecutor {
        &self.tools
    }
    fn witness_tools(&self) -> &dyn ToolExecutor {
        &self.tools
    }
    fn diagnostics(&self) -> &dyn DiagnosticRunner {
        &self.diagnostics
    }
    fn tests(&self) -> &dyn TestRunner {
        &self.diagnostics
    }
    fn repo_status(&self) -> &dyn RepoStatusPort {
        &self.repo_status
    }
    async fn seal(&self) -> Result<(), WorkspaceError> {
        self.log.lock().unwrap().push(format!("seal:{}", self.id));
        Ok(())
    }
    async fn sealed_is_unchanged(&self) -> Result<bool, WorkspaceError> {
        Ok(self.sealed_unchanged)
    }
    async fn adopt(&self) -> Result<Vec<AdoptedChange>, WorkspaceError> {
        self.log.lock().unwrap().push(format!("adopt:{}", self.id));
        self.adopt_result.clone()
    }
    async fn remove(&self) {
        self.log.lock().unwrap().push(format!("remove:{}", self.id));
    }
}

/// A scripted [`CandidateWorkspacePort`]: each `create` pops the next
/// canned workspace (or failure). `panic_on_create` proves a path never
/// touches isolation at all.
struct FakeWorkspacePort {
    script: std::sync::Mutex<VecDeque<Result<FakeWorkspace, WorkspaceError>>>,
    log: Arc<std::sync::Mutex<Vec<String>>>,
    panic_on_create: bool,
}

impl FakeWorkspacePort {
    fn new(
        script: Vec<Result<FakeWorkspace, WorkspaceError>>,
        log: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            script: std::sync::Mutex::new(script.into_iter().collect()),
            log,
            panic_on_create: false,
        }
    }

    fn untouchable() -> Self {
        Self {
            script: std::sync::Mutex::new(VecDeque::new()),
            log: Arc::new(std::sync::Mutex::new(Vec::new())),
            panic_on_create: true,
        }
    }
}

#[async_trait]
impl CandidateWorkspacePort for FakeWorkspacePort {
    async fn create(&self) -> Result<Box<dyn CandidateWorkspace>, WorkspaceError> {
        assert!(
            !self.panic_on_create,
            "the candidate workspace port must not be touched on this path"
        );
        self.log.lock().unwrap().push("create".into());
        match self.script.lock().unwrap().pop_front() {
            Some(Ok(ws)) => Ok(Box::new(ws)),
            Some(Err(e)) => Err(e),
            None => Err(WorkspaceError::Snapshot {
                reason: "workspace script exhausted".into(),
            }),
        }
    }
}

#[derive(Default)]
struct NoopSleeper;
#[async_trait]
impl Sleeper for NoopSleeper {
    async fn sleep(&self, _duration_ms: u64) {}
}

struct ZeroClock;
impl Clock for ZeroClock {
    fn now_ms(&self) -> u64 {
        0
    }
}

/// A scope gate scripted with a fixed decision.
struct FixedGate(ScopeDecision);
#[async_trait]
impl ApprovalGate for FixedGate {
    async fn review(&self, _proposal: &ScopeProposal) -> ScopeDecision {
        self.0.clone()
    }
}

fn text_result(text: &str) -> CompletionResult {
    CompletionResult {
        text: text.into(),
        tool_calls: vec![],
        usage: CompletionUsage::default(),
        model: "scripted".into(),
        cost_usd: 0.0001,
        finish_reason: None,
    }
}

fn router() -> Router {
    Router::new(
        RoleTable::new(),
        vec![ProviderProfile::new(
            "scripted",
            ModelRef::new("scripted", "worker"),
            ModelRef::new("scripted", "triage"),
            ModelRef::new("scripted", "judge"),
        )],
        CircuitBreaker::new(Box::new(ZeroClock)),
    )
}

fn drain(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

fn stages(events: &[AgentEvent]) -> Vec<StageKind> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Stage { name } => Some(*name),
            _ => None,
        })
        .collect()
}

// ---- tests ----------------------------------------------------------

/// A single-task goal whose test command flips fail→pass submits fast:
/// deterministic verdict, model judge SKIPPED.
#[tokio::test]
async fn single_task_with_a_flip_submits_fast_and_skips_the_judge() {
    // triage → "single"; worker turn → final text (no tool calls).
    let provider = ScriptedProvider::new(vec![text_result("single"), text_result("done")]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![false, true], "@@ -1 +1 @@\n-old\n+new");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let (tx, mut rx) = mpsc::unbounded_channel();

    let config = PipelineConfig {
        test_command: Some("cargo test -p x".into()),
        diff_diagnostic: Some(DiagnosticInvocation::GitDiff),
        ..PipelineConfig::default()
    };
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
            steering: None,
        },
        tx,
        config,
    );

    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let outcome = pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .expect("run succeeds");

    assert_eq!(outcome.task_class, TaskClass::SingleTask);
    assert_eq!(outcome.status, PipelineStatus::Completed);
    let verdict = outcome.verdict.expect("a verdict was produced");
    assert!(verdict.passed);
    assert!(verdict.deterministic, "flip → deterministic verdict");

    let events = drain(&mut rx);
    // Judge stage must NOT appear (submit-fast skips it).
    assert!(
        !stages(&events).contains(&StageKind::Judge),
        "the judge must be skipped on a deterministic pass"
    );
    // A deterministic JudgeVerdict event must be present.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::JudgeVerdict {
            passed: true,
            evidence
        } if evidence.deterministic
    )));
}

/// A mid-turn steer reaches the EXECUTE engine: a message queued on the
/// steering tap is injected as the execute turn's next observation and so
/// rides into the returned trajectory. Triage runs as a raw completion (no
/// engine), so the tap is drained only by the execute engine's step loop.
#[tokio::test]
async fn a_queued_steer_is_injected_into_the_execute_turn() {
    // triage → "single"; worker turn → final text (no tool calls).
    let provider = ScriptedProvider::new(vec![text_result("single"), text_result("done")]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![false, true], "@@ -1 +1 @@\n-old\n+new");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let steering = SteeringOnce {
        queued: std::sync::Mutex::new(vec!["also update the changelog".into()]),
        drains: std::sync::atomic::AtomicU32::new(0),
    };
    let (tx, _rx) = mpsc::unbounded_channel();

    let config = PipelineConfig {
        test_command: Some("cargo test -p x".into()),
        diff_diagnostic: Some(DiagnosticInvocation::GitDiff),
        ..PipelineConfig::default()
    };
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
            steering: Some(&steering),
        },
        tx,
        config,
    );

    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .expect("run succeeds");

    let injected = messages
        .iter()
        .filter(|m| m.role == MessageRole::User && m.content == "also update the changelog")
        .count();
    assert_eq!(
        injected, 1,
        "the steer must be injected exactly once, into the execute turn"
    );
    assert!(
        steering.drains.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the execute engine must have drained the steering tap"
    );
}

/// The zero-diff guard: triage misclassifies a file-touching task as a
/// lookup, but the non-empty diff revokes the judge-skip and the task is
/// verified via the model judge — it still completes ("correct downgrade").
#[tokio::test]
async fn misclassified_lookup_that_touches_files_still_gets_verified() {
    // triage → "lookup"; worker → "done"; judge → "PASS".
    let provider = ScriptedProvider::new(vec![
        text_result("lookup"),
        text_result("done"),
        text_result("PASS looks right"),
    ]);
    let resolver = OneProvider(&provider);
    // Non-empty diff → files were touched. No test command.
    let runner = ScriptedRunner::new(vec![], "@@ -1 +1 @@\n-a\n+b");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let (tx, mut rx) = mpsc::unbounded_channel();

    let config = PipelineConfig {
        test_command: None,
        diff_diagnostic: Some(DiagnosticInvocation::GitDiff),
        ..PipelineConfig::default()
    };
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
            steering: None,
        },
        tx,
        config,
    );

    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let outcome = pipeline
        .run("Explain the retry policy", &mut messages, &mut budget)
        .await
        .expect("run succeeds");

    assert_eq!(outcome.task_class, TaskClass::SimpleLookup);
    assert_eq!(outcome.status, PipelineStatus::Completed);
    let verdict = outcome
        .verdict
        .expect("zero-diff guard forced verification");
    assert!(verdict.passed);
    assert!(!verdict.deterministic, "verified via the model judge");

    let events = drain(&mut rx);
    assert!(
        stages(&events).contains(&StageKind::Judge),
        "the zero-diff guard must run the judge on an unexpected mutation"
    );
}

/// A clean lookup that touches nothing skips planning, verification, and
/// the judge entirely.
#[tokio::test]
async fn clean_lookup_skips_plan_verify_and_judge() {
    let provider = ScriptedProvider::new(vec![text_result("lookup"), text_result("the answer")]);
    let resolver = OneProvider(&provider);
    // Empty diff → nothing touched.
    let runner = ScriptedRunner::new(vec![], "");
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
            steering: None,
        },
        tx,
        PipelineConfig::default(),
    );

    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let outcome = pipeline
        .run("What does the flip oracle do?", &mut messages, &mut budget)
        .await
        .expect("run succeeds");

    assert_eq!(outcome.task_class, TaskClass::SimpleLookup);
    assert_eq!(outcome.status, PipelineStatus::Completed);
    assert!(
        outcome.verdict.is_none(),
        "no verification for a clean lookup"
    );
    assert_eq!(outcome.final_text, "the answer");

    let s = stages(&drain(&mut rx));
    assert!(!s.contains(&StageKind::Plan));
    assert!(!s.contains(&StageKind::Verify));
    assert!(!s.contains(&StageKind::Judge));
}

/// A multi-step plan above the scope-review thresholds, running headless
/// with no bypass, is a named error (never a silent auto-approve).
#[tokio::test]
async fn paid_headless_scope_review_error_retains_settled_cost() {
    // triage → "multi"; plan → a 6-step JSON array (default threshold 5).
    let provider = ScriptedProvider::new(vec![
        text_result("multi"),
        text_result(r#"["s1","s2","s3","s4","s5","s6"]"#),
    ]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![], "");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = FixedGate(ScopeDecision::Approve);
    let sleeper = NoopSleeper;
    let router = router();
    let (tx, _rx) = mpsc::unbounded_channel();

    let config = PipelineConfig {
        headless: true,
        headless_bypass_scope_review: false,
        ..PipelineConfig::default()
    };
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
            steering: None,
        },
        tx,
        config,
    );

    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let err = pipeline
        .run(
            "Refactor across the codebase and then update all callers",
            &mut messages,
            &mut budget,
        )
        .await
        .expect_err("headless scope review must be a named error");
    assert_eq!(err.cause, PipelineError::ScopeReviewRequiredHeadless);
    assert!(
        (err.total_cost_usd - 0.0002).abs() < 1e-9,
        "triage and plan spend must survive the hard error: {err:?}"
    );
}

/// The user aborting at the scope-review gate ends the run cleanly (not an
/// error), with an `Aborted` status.
#[tokio::test]
async fn user_abort_at_scope_review_is_a_clean_abort() {
    let provider = ScriptedProvider::new(vec![
        text_result("multi"),
        text_result(r#"["s1","s2","s3","s4","s5","s6"]"#),
    ]);
    let resolver = OneProvider(&provider);
    let runner = ScriptedRunner::new(vec![], "");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = FixedGate(ScopeDecision::Abort);
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
            steering: None,
        },
        tx,
        PipelineConfig::default(),
    );

    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let outcome = pipeline
        .run(
            "Refactor across the codebase and then rename all callers",
            &mut messages,
            &mut budget,
        )
        .await
        .expect("a user abort is a clean outcome, not an error");
    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    // Execution never started.
    let s = stages(&drain(&mut rx));
    assert!(!s.contains(&StageKind::Execute));
}

#[test]
fn count_diff_lines_ignores_headers() {
    let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n context";
    assert_eq!(count_diff_lines(diff), 2);
}

/// P1/P2 regression: a large NEW file must contribute its real added-line
/// count (not a flat 1, which slipped a 10k-line file under the diff
/// budget), and a file already untracked before the turn must not be
/// attributed to it.
#[tokio::test]
async fn gather_diff_counts_real_new_file_lines_and_excludes_pre_existing() {
    let provider = ScriptedProvider::new(vec![]);
    let resolver = OneProvider(&provider);
    // Empty tracked diff; the ScriptedRunner reports src/huge.rs's no-index
    // numstat as 5000 added lines. The repo status reports it as untracked
    // with fingerprint "v2".
    let runner = ScriptedRunner::new(vec![], "").with_untracked(vec![("src/huge.rs", 5000)]);
    let repo_status = FakeRepoStatus::new(vec![("src/huge.rs", "v2")]);
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
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
            steering: None,
        },
        tx,
        PipelineConfig::default(),
    );

    let surface = CandidateSurface {
        diagnostics: &runner,
        tests: &runner,
        repo_status: &repo_status,
        cwd: None,
        hook_runner: None,
        workspace: None,
    };
    // No baseline → the file is this turn's; its real 5000 lines count.
    let (lines, text) = pipeline.gather_diff(surface, &HashMap::new()).await;
    assert_eq!(lines, 5000, "a new file counts its real added lines, not 1");
    assert!(text.contains("src/huge.rs"), "diff text names the new file");

    // Present before at the SAME fingerprint → untouched dirty state, not
    // attributed to the turn.
    let unchanged: HashMap<String, String> =
        std::iter::once(("src/huge.rs".to_string(), "v2".to_string())).collect();
    let (lines2, _) = pipeline.gather_diff(surface, &unchanged).await;
    assert_eq!(
        lines2, 0,
        "a pre-existing untracked file is not this turn's change"
    );

    // Present before but at a DIFFERENT fingerprint → the turn edited an
    // already-untracked file; it must be visible (the P1 regression).
    let modified: HashMap<String, String> =
        std::iter::once(("src/huge.rs".to_string(), "v1".to_string())).collect();
    let (lines3, text3) = pipeline.gather_diff(surface, &modified).await;
    assert_eq!(
        lines3, 5000,
        "an edit to an already-untracked file is counted"
    );
    assert!(text3.contains("src/huge.rs"));
}

#[test]
fn assemble_user_message_puts_recall_before_the_task() {
    let frames = vec![RecalledFrame {
        citation_label: "driver.rs".into(),
        provider: "code-graph".into(),
        source: "code-graph".into(),
        kind: "symbol".into(),
        uri: None,
        method: None,
        content: "run_turn".into(),
        token_cost: 5,
        id: None,
    }];
    let msg = assemble_user_message("do the thing", &frames);
    let recall_idx = msg.find("Recalled context").unwrap();
    let task_idx = msg.find("do the thing").unwrap();
    assert!(recall_idx < task_idx, "recall rides before the goal");
}

#[test]
fn assemble_user_message_is_just_the_goal_when_no_recall() {
    assert_eq!(assemble_user_message("hello", &[]), "hello");
}

/// With no `--test-command`, the witness author arms the flip oracle: its
/// authored command is observed failing, the worker's change flips it, and
/// the run submits fast on deterministic evidence — judge skipped.
#[tokio::test]
async fn witness_authored_command_arms_the_flip_oracle_and_submits_fast() {
    // triage → "single"; witness author turn → marker line; worker → done.
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("wrote the test.\nTEST_COMMAND: cargo test --test witness witness -- --exact"),
        text_result("done"),
    ]);
    // Test-command pops: witness fail-check (fail), per-candidate baseline
    // (fail), post-execute observation (pass) → a genuine flip.
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace =
        FakeWorkspace::new(0, vec![false, false, true], Ok(vec![]), log.clone()).with_repo_status(
            SeqRepoStatus::new(vec![vec![], vec![("tests/witness.rs", "w1")]]),
        );
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log);
    let (outcome, events, _) = run_isolated(
        &provider,
        &port,
        PipelineConfig::default(),
        "Fix the retry bug",
    )
    .await;
    let outcome = outcome.expect("run succeeds");

    assert_eq!(outcome.status, PipelineStatus::Completed);
    let verdict = outcome.verdict.expect("verified");
    assert!(verdict.passed);
    assert!(
        verdict.deterministic,
        "a witness flip is deterministic evidence: {}",
        verdict.summary
    );
    assert!(
        verdict
            .summary
            .contains("cargo test --test witness witness -- --exact"),
        "the evidence names the witness command: {}",
        verdict.summary
    );

    let s = stages(&events);
    assert!(s.contains(&StageKind::Witness), "witness stage emitted");
    assert!(!s.contains(&StageKind::Judge), "judge skipped on the flip");
}

/// A witness whose test passes on the unmodified code proves nothing: one
/// bounded repair retry, and if it still passes the contaminated candidate is
/// discarded rather than letting author-written files reach adoption.
#[tokio::test]
async fn a_witness_that_never_fails_aborts_and_removes_the_candidate() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("TEST_COMMAND: cargo test --test witness always_green -- --exact"),
        // The repair attempt also yields a command that passes.
        text_result("TEST_COMMAND: cargo test --test witness still_green -- --exact"),
    ]);
    // Pops: witness check (pass), repair check (pass) → discard. Then no
    // test command exists for the candidate, so no further pops.
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace =
        FakeWorkspace::new(0, vec![true, true], Ok(vec![]), log.clone()).with_repo_status(
            SeqRepoStatus::new(vec![vec![], vec![("tests/witness.rs", "w1")]]),
        );
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log.clone());
    let (outcome, _, _) = run_isolated(
        &provider,
        &port,
        PipelineConfig::default(),
        "Fix the retry bug",
    )
    .await;
    let outcome = outcome.expect("invalid witness is a truthful abort");
    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    assert_eq!(*log.lock().unwrap(), vec!["create", "remove:0"]);
}

/// Tamper exclusion: the worker modified the witness test file after it
/// was authored, so the candidate hard-fails without judge override.
#[tokio::test]
async fn a_tampered_witness_file_hard_fails_before_judge_evaluation() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("TEST_COMMAND: cargo test --test witness witness -- --exact"),
        text_result("done"),                     // worker
        text_result("FAIL the test was edited"), // must remain unused
    ]);
    // witness check (fail), baseline (fail), post-execute (pass) → the
    // oracle flips — but the tamper check below must hard-fail the candidate.
    // untracked_fingerprints call order: witness-before (empty),
    // witness-after (test authored at w1 → watchlist), candidate
    // untracked_before, gather_diff after execute, tamper check — where
    // the file now reads w2: TAMPERED.
    let repo_status = SeqRepoStatus::new(vec![
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
    let config = PipelineConfig {
        max_revisions: 0, // judge FAIL ends the run — keeps the script short
        ..PipelineConfig::default()
    };
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let workspace = FakeWorkspace::new(0, vec![false, false, true], Ok(vec![]), log.clone())
        .with_repo_status(repo_status);
    let port = FakeWorkspacePort::new(vec![Ok(workspace)], log);
    let (outcome, events, _) = run_isolated(&provider, &port, config, "Fix the retry bug").await;
    let outcome = outcome.expect("run succeeds");

    assert!(matches!(outcome.status, PipelineStatus::Aborted { .. }));
    assert!(
        !stages(&events).contains(&StageKind::Judge),
        "tamper is not eligible for judge override"
    );
}

/// Distress guidance: the FIRST deterministic failure revises on raw
/// evidence alone; the SECOND spends one judge call whose course-correction
/// rides with the next revision prompt.
#[tokio::test]
async fn second_consecutive_red_verification_gets_judge_guidance() {
    let provider = ScriptedProvider::new(vec![
        text_result("single"),
        text_result("done"),      // worker
        text_result("first fix"), // revision 1 (no guidance)
        text_result("You are patching the symptom; fix the parser instead."), // guidance
        text_result("second fix"), // revision 2 (carries guidance)
    ]);
    let resolver = OneProvider(&provider);
    // baseline (fail), post-execute (fail) → revise; post-revision-1
    // (fail) → distress → guidance → revise; post-revision-2 (fail) →
    // revisions exhausted → deterministic failed verdict.
    let runner = ScriptedRunner::new(vec![false, false, false, false], "@@ -1 +1 @@\n-a\n+b");
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let repo_status = NoRepoStatus;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = router();
    let (tx, mut rx) = mpsc::unbounded_channel();

    let config = PipelineConfig {
        test_command: Some("cargo test -p x".into()),
        max_revisions: 2,
        ..PipelineConfig::default()
    };
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
            steering: None,
        },
        tx,
        config,
    );

    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let outcome = pipeline
        .run("Fix the failing test", &mut messages, &mut budget)
        .await
        .expect("run succeeds");

    let verdict = outcome.verdict.expect("verified");
    assert!(!verdict.passed);
    assert!(verdict.deterministic, "red tests are a deterministic fail");
    assert_eq!(outcome.revisions, 2);

    // The guidance text reached the worker's revision prompt.
    let carried = messages.iter().any(|m| {
        m.content.contains("Independent reviewer course-correction")
            && m.content.contains("fix the parser instead")
    });
    assert!(carried, "guidance rides with the second revision prompt");
    assert!(
        stages(&drain(&mut rx)).contains(&StageKind::Judge),
        "the guidance call is an honest Judge stage in the stream"
    );
}

// ---- best-of-N candidate isolation ----------------------------------

/// Build a pipeline over panicking session command/repo-status ports (so
/// any candidate I/O that escapes its workspace fails the test) and run
/// `goal` with the given port + config. Returns the outcome and events.
async fn run_isolated(
    provider: &ScriptedProvider,
    port: &FakeWorkspacePort,
    config: PipelineConfig,
    goal: &str,
) -> (
    Result<PipelineOutcome, PipelineRunError>,
    Vec<AgentEvent>,
    Vec<CompletionMessage>,
) {
    let resolver = OneProvider(provider);
    let diagnostics = NeverRunner;
    let repo_status = NeverRepoStatus;
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
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
            diagnostics: &diagnostics,
            tests: &diagnostics,
            approvals: &approvals,
            sleeper: &sleeper,
            hooks: None,
            candidate_workspaces: Some(port),
            steering: None,
        },
        tx,
        config,
    );
    let mut messages = vec![CompletionMessage::system("sys")];
    let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
    let outcome = pipeline.run(goal, &mut messages, &mut budget).await;
    (outcome, drain(&mut rx), messages)
}

fn isolated_config(n: u32) -> PipelineConfig {
    PipelineConfig {
        test_command: Some("cargo test".into()),
        // A red first candidate must fail immediately, not revise — keeps
        // the scripts to one worker turn per candidate.
        max_revisions: 0,
        candidates: Some(n),
        ..PipelineConfig::default()
    }
}

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

mod task4;
mod task5;
