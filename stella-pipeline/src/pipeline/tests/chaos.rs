//! The loop-invariant / chaos harness.
//!
//! Most of the "the agent died having done zero work" failures this codebase
//! has hit were not wrong *answers* — they were the orchestrator ending a
//! turn on a setup precondition (no isolation, no independent witness author,
//! a scope gate, a provider that 400s) instead of degrading. Volume never
//! finds a class like that reliably; a property over the config × environment
//! cross-product does, deterministically and in milliseconds.
//!
//! This harness enumerates that cross-product — provider behavior × candidate
//! isolation × task class × single/multi-model × witness on/off × budget ×
//! headless bypass — and asserts, for EVERY combination, the two invariants
//! that separate a healthy loop from a dying one:
//!
//! 1. **Named termination.** A run ends only by returning `Ok(outcome)` or a
//!    typed `Err(PipelineRunError)`; an `Ok(Aborted)` always carries a
//!    non-empty reason; and a terminal event (`Complete` or `Error`) reaches
//!    the stream on the `Ok` path. No run vanishes without an explanation.
//! 2. **Never choose nothing.** A run that ends having executed zero worker
//!    turns may only be a *recognized* zero-work cause — a budget/resource
//!    limit, a user scope abort, or an unresolvable provider. A setup failure
//!    must have degraded to a bare execution (the #345 contract), never a
//!    silent zero-work death.
//!
//! When a combination violates either, the panic message names the exact
//! scenario, so a regression points straight at the offending shape rather
//! than "some run somewhere aborted".

use super::*;
use std::collections::VecDeque;

use stella_core::router::{CircuitBreaker, ProviderProfile, RoleTable};
use tokio::sync::Mutex as TokioMutex;

/// A provider whose every call is scripted as an explicit `Result`, so a
/// scenario can inject a terminal/transport error at any position — not just
/// "runs out". Exhausting the script yields a terminal error, standing in for
/// a provider that fails partway through a turn.
struct ChaosProvider {
    script: TokioMutex<VecDeque<Result<CompletionResult, ProviderError>>>,
}

impl ChaosProvider {
    fn new(script: Vec<Result<CompletionResult, ProviderError>>) -> Self {
        Self {
            script: TokioMutex::new(script.into_iter().collect()),
        }
    }
}

#[async_trait]
impl Provider for ChaosProvider {
    fn id(&self) -> &str {
        "chaos"
    }
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        self.script
            .lock()
            .await
            .pop_front()
            .unwrap_or_else(|| Err(ProviderError::Terminal("chaos provider exhausted".into())))
    }
}

/// How a scenario's candidate-isolation port behaves.
#[derive(Clone, Copy)]
enum PortShape {
    /// No candidate workspace port wired at all (a caller in a plain dir).
    Absent,
    /// A port that fails to snapshot every candidate (isolation unavailable).
    Fails,
    /// A port that snapshots successfully.
    Works,
}

/// The provider call sequence a scenario feeds every role.
#[derive(Clone, Copy)]
enum ProviderShape {
    /// Enough responses for triage + a worker turn + a judge — a clean run.
    Healthy,
    /// Triage answers, then the worker call errors (script exhausts).
    WorkerErrors,
    /// The very first call (triage) errors.
    TriageErrors,
}

/// One point in the cross-product.
#[derive(Clone, Copy)]
struct Scenario {
    provider: ProviderShape,
    port: PortShape,
    /// The triage class token the model returns (`lookup` / `single` / `multi`).
    class: &'static str,
    single_model: bool,
    witness_writer: bool,
    tiny_budget: bool,
    headless_bypass: bool,
}

impl Scenario {
    fn label(&self) -> String {
        format!(
            "provider={} port={} class={} single_model={} witness={} tiny_budget={} bypass={}",
            match self.provider {
                ProviderShape::Healthy => "healthy",
                ProviderShape::WorkerErrors => "worker-errors",
                ProviderShape::TriageErrors => "triage-errors",
            },
            match self.port {
                PortShape::Absent => "absent",
                PortShape::Fails => "fails",
                PortShape::Works => "works",
            },
            self.class,
            self.single_model,
            self.witness_writer,
            self.tiny_budget,
            self.headless_bypass,
        )
    }

    fn provider_script(&self) -> Vec<Result<CompletionResult, ProviderError>> {
        // The first response is the triage classification (a bare token, so
        // it parses whatever the assurance flags do); the rest feed the
        // worker/judge turns. `Healthy` gives generous headroom; the error
        // shapes cut the script short at the intended point.
        let triage = || Ok(text_result(self.class));
        let done = || Ok(text_result("done"));
        let pass = || Ok(text_result("PASS looks right"));
        match self.provider {
            ProviderShape::TriageErrors => vec![Err(ProviderError::Terminal("triage down".into()))],
            ProviderShape::WorkerErrors => {
                vec![triage(), Err(ProviderError::Terminal("worker down".into()))]
            }
            ProviderShape::Healthy => vec![
                triage(),
                done(),
                pass(),
                // Spare responses so a revise/repair/judge never exhausts the
                // script by accident and masks a real invariant break.
                done(),
                pass(),
                done(),
                pass(),
            ],
        }
    }

    fn router(&self) -> Router {
        if self.single_model {
            let only = ModelRef::new("chaos", "only");
            Router::new(
                RoleTable::new(),
                vec![ProviderProfile::new(
                    "chaos",
                    only.clone(),
                    only.clone(),
                    only,
                )],
                CircuitBreaker::new(Box::new(ZeroClock)),
            )
        } else {
            Router::new(
                RoleTable::new(),
                vec![ProviderProfile::new(
                    "chaos",
                    ModelRef::new("chaos", "worker"),
                    ModelRef::new("chaos", "triage"),
                    ModelRef::new("chaos", "judge"),
                )],
                CircuitBreaker::new(Box::new(ZeroClock)),
            )
        }
    }

    fn config(&self) -> PipelineConfig {
        PipelineConfig {
            witness_writer: self.witness_writer,
            // A candidate count > 1 exercises the best-of-N isolation path.
            candidates: Some(if matches!(self.port, PortShape::Works) {
                2
            } else {
                1
            }),
            headless: true,
            headless_bypass_scope_review: self.headless_bypass,
            ..PipelineConfig::default()
        }
    }
}

/// A resolver that returns the one chaos provider for every model.
struct OneChaosProvider<'p>(&'p ChaosProvider);
impl ProviderResolver for OneChaosProvider<'_> {
    fn provider_for(&self, _model: &ModelRef) -> Option<&dyn Provider> {
        Some(self.0)
    }
}

/// Run one scenario over WORKING session ports (so execution can actually
/// happen when it should) and a scenario-shaped candidate port. Returns
/// everything the invariant checker needs.
async fn run_scenario(
    scenario: &Scenario,
) -> (
    Result<PipelineOutcome, PipelineRunError>,
    Vec<AgentEvent>,
    usize,
    usize,
) {
    let provider = ChaosProvider::new(scenario.provider_script());
    let resolver = OneChaosProvider(&provider);
    let runner = ScriptedRunner::new(vec![], "@@ -1 +1 @@\n-a\n+b");
    let repo_status = SeqRepoStatus::new(vec![vec![]]);
    let tools = EmptyTools;
    let recall = NoContextRecall;
    let repo = NoRepoStructure;
    let approvals = AutoApproveGate;
    let sleeper = NoopSleeper;
    let router = scenario.router();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Build the candidate port lazily so `Absent` wires none at all.
    let fails_port;
    let works_port;
    let candidate_workspaces: Option<&dyn CandidateWorkspacePort> = match scenario.port {
        PortShape::Absent => None,
        PortShape::Fails => {
            fails_port = FakeWorkspacePort::new(
                vec![
                    Err(WorkspaceError::Snapshot {
                        reason: "chaos: not isolable".into(),
                    }),
                    Err(WorkspaceError::Snapshot {
                        reason: "chaos: not isolable".into(),
                    }),
                ],
                log.clone(),
            );
            Some(&fails_port)
        }
        PortShape::Works => {
            works_port = FakeWorkspacePort::new(
                vec![
                    Ok(FakeWorkspace::new(0, vec![], Ok(vec![]), log.clone())
                        .with_repo_status(SeqRepoStatus::new(vec![vec![]]))),
                    Ok(FakeWorkspace::new(1, vec![], Ok(vec![]), log.clone())
                        .with_repo_status(SeqRepoStatus::new(vec![vec![]]))),
                ],
                log.clone(),
            );
            Some(&works_port)
        }
    };

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
            candidate_workspaces,
            mcp_prefetch: None,
            steering: None,
        },
        tx,
        scenario.config(),
    );

    let mut messages = vec![CompletionMessage::system("sys")];
    let base_len = messages.len();
    let mode = if scenario.tiny_budget {
        BudgetMode::Enforced
    } else {
        BudgetMode::Off
    };
    let limit = scenario.tiny_budget.then_some(0.0001);
    let mut budget = BudgetGuard::new(mode, limit, limit);
    let outcome = pipeline
        .run("Fix the retry bug", &mut messages, &mut budget)
        .await;
    let events = drain(&mut rx);
    (outcome, events, base_len, messages.len())
}

/// The heart of the harness: assert both invariants for one scenario's run.
fn assert_loop_invariant(
    scenario: &Scenario,
    outcome: &Result<PipelineOutcome, PipelineRunError>,
    events: &[AgentEvent],
    base_len: usize,
    final_len: usize,
) {
    let at = scenario.label();

    // A non-empty reason on every abort.
    if let Ok(o) = outcome
        && let PipelineStatus::Aborted { reason } = &o.status
    {
        assert!(
            !reason.trim().is_empty(),
            "[{at}] an abort must carry a reason"
        );
    }

    // Named termination: the Ok path must have emitted a terminal signal
    // (Complete or a non-retryable Error). An Err return IS the signal, so it
    // needs no event.
    if outcome.is_ok() {
        let terminal = events.iter().any(|e| {
            matches!(
                e,
                AgentEvent::Complete { .. }
                    | AgentEvent::Error {
                        retryable: false,
                        ..
                    }
            )
        });
        assert!(
            terminal,
            "[{at}] an Ok run must emit a terminal Complete/Error — none did: {events:?}"
        );
    }

    // Never choose nothing: a run that executed zero worker turns and did not
    // complete may only be a RECOGNIZED zero-work cause. The signal is the
    // event stream, NOT the transcript length — `run` appends the user
    // message before any execution, so message count would report "executed"
    // even on a setup death. A worker turn is the one thing a setup abort
    // never reaches: it emits a `StepUsage` with the `worker` purpose.
    let _ = (base_len, final_len);
    let worker_executed = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::StepUsage {
                role: stella_protocol::ModelCallRole::Worker,
                ..
            }
        )
    });
    if !worker_executed {
        match outcome {
            // A typed hard error (e.g. no resolvable provider) is a named
            // termination by construction.
            Err(_) => {}
            Ok(o) => match &o.status {
                // Completing with no worker turn is only legitimate when the
                // work is genuinely a no-op path; on these scenarios it would
                // be a lookup that the class routed to zero execution, which
                // is still a *named* completion, not a death.
                PipelineStatus::Completed | PipelineStatus::VerificationFailed { .. } => {}
                PipelineStatus::Aborted { reason } => {
                    let r = reason.to_ascii_lowercase();
                    let legitimate = r.contains("budget")
                        || r.contains("scope review")
                        || r.contains("provider")
                        || r.contains("no candidate workspace port")
                        // A witness artifact-integrity violation is a
                        // DELIBERATE fail-closed stop (the author modified
                        // tracked files / produced a bad artifact / a
                        // runner-identity mismatch): a named, intended
                        // zero-work abort, not a silent setup death. Witness
                        // AUTHORING failures degrade instead of reaching here.
                        || r.contains("modified tracked")
                        || r.contains("create exactly one")
                        || r.contains("does not match test runner")
                        || r.contains("unsafe or unstable filesystem");
                    assert!(
                        legitimate,
                        "[{at}] ZERO-WORK DEATH: the loop aborted before any worker turn for a \
                         reason that is not a recognized resource/impossibility limit — this is \
                         exactly the 'choose nothing' class. reason = {reason:?}"
                    );
                }
            },
        }
    }
}

/// The cross-product. Kept as an explicit enumeration rather than a random
/// proptest: the space is small enough to cover exhaustively, and a
/// deterministic failure names the exact shape.
fn matrix() -> Vec<Scenario> {
    let mut out = Vec::new();
    for provider in [
        ProviderShape::Healthy,
        ProviderShape::WorkerErrors,
        ProviderShape::TriageErrors,
    ] {
        for port in [PortShape::Absent, PortShape::Fails, PortShape::Works] {
            for class in ["lookup", "single", "multi"] {
                for single_model in [false, true] {
                    for witness_writer in [false, true] {
                        for tiny_budget in [false, true] {
                            for headless_bypass in [false, true] {
                                out.push(Scenario {
                                    provider,
                                    port,
                                    class,
                                    single_model,
                                    witness_writer,
                                    tiny_budget,
                                    headless_bypass,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// Every point in the provider × isolation × class × model × witness ×
/// budget × bypass cross-product must satisfy both loop invariants. A break
/// names the exact scenario.
#[tokio::test]
async fn the_loop_never_chooses_nothing_across_the_config_environment_matrix() {
    let scenarios = matrix();
    assert!(
        scenarios.len() >= 200,
        "the matrix should be broad: {}",
        scenarios.len()
    );
    for scenario in &scenarios {
        let (outcome, events, base_len, final_len) = run_scenario(scenario).await;
        // Reaching here at all proves the run neither panicked nor hung
        // (NoopSleeper + the step cap make a hang impossible).
        assert_loop_invariant(scenario, &outcome, &events, base_len, final_len);
    }
}
