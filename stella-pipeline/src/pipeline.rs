//! The orchestrator: the staged turn flow that sits
//! *above* `stella-core::Engine`. It sequences evaluate → enhance → route →
//! witness → execute → verify → judge → revise over the injected ports,
//! emitting a `Stage` event at every boundary and owning terminal
//! success-or-failure signaling for outcome-producing runs (`Complete` or a
//! non-retryable `Error`). Hard infrastructure failures return out of band as
//! [`PipelineRunError`].
//!
//! Everything here is I/O sequencing; every *decision* it makes is delegated
//! to a pure function in a sibling module (`triage`, `plan`, `scope`,
//! `verify`, `candidate`) so the hard logic stays synchronous and
//! property-testable. The orchestrator's own job is only to call the ports in
//! the right order and thread the budget through.
//!
//! # Event ownership
//!
//! `stella-core::Engine::run_turn` emits its own `Stage { Execute }`, a
//! terminal `Stage { Complete }`, and a `Complete` — correct for *one turn*,
//! but a multi-step plan or a revise loop runs several turns. The pipeline is
//! the **single authority** for stage boundaries and the terminal event on an
//! outcome-producing run: it gives each `run_turn` a private channel, then
//! forwards every event to the consumer *except* the engine's
//! `Stage`/`Complete` (which would otherwise falsely signal "done" after step
//! one). The pipeline emits `Complete` for success or a non-retryable `Error`
//! for terminal failure; hard [`PipelineRunError`] exits remain typed return
//! values for the caller to close out. This mirrors the one-emission-point
//! discipline of L-E1/L-T5.
//!
//! # Cache discipline (L-E8)
//!
//! Recalled context rides as a **volatile message after the byte-stable system
//! prefix**, never mutated into the system block itself, so prompt-cache hits
//! on the stable prefix survive across turns. See [`assemble_user_message`].
//!
//! # Breaker feedback boundary
//!
//! The pipeline holds `&Router` (per its constructor contract) and so *reads*
//! resolutions and surfaces `ProviderFallback` events, but does not feed
//! call outcomes back into the breaker (`record_success`/`record_failure`
//! need `&mut Router`). That feedback is the responsibility of the glue that
//! owns the `Router` — documented here so the boundary is explicit.

use std::collections::HashMap;
use std::time::Duration;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use stella_core::hooks::{HookRunner, Hooks};
use stella_core::retry::{RetryPolicy, Sleeper};
use stella_core::router::FallbackInfo;
use stella_core::{BudgetGuard, Engine, EngineConfig, EventSender, Router, TurnOutcome};
use stella_protocol::{
    AgentEvent, CompletionMessage, ContextFrameRef, JudgeEvidence, ModelCallRole, ModelRef,
    Provider, ProviderShare, Role, StageKind,
};

use crate::candidate::{
    CandidateScore, CandidateSummary, score_from_verification, select_best_candidate,
};
use crate::plan::{PlanStep, build_planner_prompt, parse_plan, plan_repair_prompt};
use crate::ports::{
    ApprovalGate, CandidateWorkspace, CandidateWorkspacePort, ContextRecallPort,
    DiagnosticInvocation, DiagnosticRunner, McpPrefetchPort, PipelinePorts, ProviderResolver,
    RecalledFrame, RepoStatusPort, RepoStructurePort, ScopeDecision, TestInvocation, TestRunner,
    WorkspaceError,
};
use crate::scope::{ScopeEstimate, apply_trim, build_proposal, needs_scope_review};
use crate::triage::{
    TaskAssessment, TaskClass, parse_triage_response, resolve_task_class, triage_prompt,
};
use crate::verify::{
    FlipOracle, JudgeVerdict as ModelJudgeVerdict, LadderDecision, LadderInputs,
    deterministic_fail_evidence, deterministic_pass_evidence, guidance_prompt, heuristic_fallback,
    judge_prompt, ladder_decision, model_verdict_evidence, parse_judge_response,
};
use crate::witness::{
    Witness, parse_test_invocation, parse_witness_command, validate_witness_artifact,
    validate_witness_identity, validate_witness_invocation, witness_identity_matches,
    witness_prompt, witness_repair_prompt,
};
mod raw_usage;
mod run_error;
mod stage_budget;
mod task5;
use raw_usage::{RawCall, RawCallError};
use run_error::RoleResolveError;
pub use run_error::{PipelineError, PipelineRunError};
use stage_budget::{PipelineBudgetAbort, budget_abort};
use task5::BoundHookRunner;
/// Minimal fallback when the caller supplies no stable system prefix.
const DEFAULT_SYSTEM_PROMPT: &str =
    "You are a precise, careful software engineering agent. Make the smallest correct change.";

/// Small fixed system prompt for the independent witness author.
const WITNESS_SYSTEM_PROMPT: &str = "You are a precise test author. You write minimal failing tests that pin down intended \
     behavior. You never modify production code and never fix the problem yourself.";

/// Per-role request overrides for the pipeline's raw completion calls
/// (triage / judge / guidance), resolved by the caller from
/// `agent_engine_config`. Every field is optional and falls through to the
/// engine config's value — the worker's settings are the pipeline-wide
/// base; these refine one role. `prompt`, when set, is prepended as a
/// system message to the role's built-in task prompt (the task prompt
/// carries the output contract — `PASS`/`FAIL`, the triage token — so it
/// is never replaced outright).
#[derive(Debug, Clone, Default)]
pub struct RoleCallOverrides {
    pub prompt: Option<String>,
    pub effort: Option<stella_protocol::ReasoningEffort>,
    pub reasoning: Option<bool>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub params: Option<stella_protocol::GenerationParams>,
}

/// The pipeline's per-role override set. Worker (and plan/witness, which
/// ride the worker's tier) is configured through
/// [`PipelineConfig::engine`] directly; only the two roles with their own
/// models get their own request shaping.
#[derive(Debug, Clone, Default)]
pub struct PipelineRoleOverrides {
    pub triage: RoleCallOverrides,
    pub judge: RoleCallOverrides,
}

/// Tuning for the whole staged flow.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Passed to `stella-core::Engine` for every execute turn.
    pub engine: EngineConfig,
    /// Per-role request overrides (`agent_engine_config`) for the raw
    /// triage/judge completion calls.
    pub role_overrides: PipelineRoleOverrides,
    /// Decision latency ceiling on the triage classification call (L-M4): if
    /// it doesn't answer within this, its eventual response is ignored and
    /// triage falls through to the full path. The paid call is still awaited
    /// so usage cannot disappear from accounting through cancellation.
    pub triage_latency_ceiling: Duration,
    /// Thresholds above which a plan triggers interactive scope review (L-E5).
    pub scope_thresholds: crate::scope::ScopeThresholds,
    /// Whether this run is headless (no interactive approver available).
    pub headless: bool,
    /// If headless and a plan crosses the scope-review thresholds, this must
    /// be explicitly `true` to proceed (via [`crate::ports::AutoApproveGate`]);
    /// otherwise the run is a named error rather than a silent auto-approve.
    pub headless_bypass_scope_review: bool,
    /// The test command the flip oracle tracks (run before and after execute).
    /// `None` hands the flip oracle to the witness author (when
    /// `witness_writer` is on) — an explicit user command always wins over an
    /// authored one.
    pub test_command: Option<String>,
    /// Witness authoring (L-E11 front half): when no `test_command` is
    /// configured and the task class verifies unconditionally, an independent
    /// model authors a failing witness test whose command arms the flip
    /// oracle, with tamper exclusion at verify time ([`crate::witness`]).
    /// Costs one engine turn + up to two test runs per pipeline run.
    pub witness_writer: bool,
    /// Distress-triggered course-correction: on the *second consecutive*
    /// deterministic verification failure, spend one judge call for guidance
    /// that rides with the next revision prompt ([`crate::verify::guidance_prompt`]).
    /// Event-triggered by design — never a fixed mid-run checkpoint. Bounded
    /// by `max_revisions` (at most `max_revisions - 1` guidance calls per
    /// candidate).
    pub distress_guidance: bool,
    /// The closed diagnostic that reports what the turn changed. `None`
    /// disables diff-size and zero-diff inspection.
    pub diff_diagnostic: Option<DiagnosticInvocation>,
    /// The diff-size budget in changed lines: a diff at or under this is
    /// "small enough" to trust deterministic evidence without a judge (L-E11).
    pub diff_budget_lines: u32,
    /// Maximum revision turns per candidate when verification fails.
    pub max_revisions: u32,
    /// Best-of-N (L-E7). `None` or `Some(1)` is single-shot (the default);
    /// `Some(n)` generates n candidate executions — each in an isolated
    /// snapshot of the current tree state when a
    /// [`crate::ports::CandidateWorkspacePort`] is wired — and selects the
    /// best, adopting only the winner's changes into the real tree. Paid for
    /// with n× the execution cost — opt-in only.
    pub candidates: Option<u32>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            role_overrides: PipelineRoleOverrides::default(),
            triage_latency_ceiling: Duration::from_secs(10),
            scope_thresholds: crate::scope::ScopeThresholds::default(),
            headless: false,
            headless_bypass_scope_review: false,
            test_command: None,
            witness_writer: true,
            distress_guidance: true,
            diff_diagnostic: Some(DiagnosticInvocation::GitDiff),
            diff_budget_lines: 400,
            max_revisions: 2,
            candidates: None,
        }
    }
}

impl PipelineConfig {
    /// The effective candidate count (`candidates`, floored at 1).
    fn candidate_count(&self) -> u32 {
        self.candidates.unwrap_or(1).max(1)
    }
}

/// The final verification verdict a pipeline run produced, if verification
/// ran. `deterministic` distinguishes a flip-oracle/ladder verdict from a
/// model/heuristic judge's opinion (never conflated, L-E11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub passed: bool,
    pub deterministic: bool,
    pub summary: String,
}

impl Verdict {
    fn from_evidence(passed: bool, evidence: &JudgeEvidence) -> Self {
        Self {
            passed,
            deterministic: evidence.deterministic,
            summary: evidence.summary.clone(),
        }
    }
}

/// How a pipeline run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineStatus {
    Completed,
    /// Verification remained red after the revision budget was exhausted.
    VerificationFailed {
        verdict: Verdict,
    },
    /// The run ended early: a step aborted (budget/loop/step-cap), or the user
    /// aborted at scope review.
    Aborted {
        reason: String,
    },
}

/// The result of one [`Pipeline::run`].
// No `Eq`: `total_cost_usd` is an `f64`. `PartialEq` is enough for assertions.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineOutcome {
    pub status: PipelineStatus,
    /// The task class triage resolved (after the deterministic floor).
    pub task_class: TaskClass,
    /// The final assistant text of the selected candidate.
    pub final_text: String,
    /// Total spend across every stage of this run, in USD.
    pub total_cost_usd: f64,
    /// The final verification verdict, if verification ran.
    pub verdict: Option<Verdict>,
    /// How many revision turns the selected candidate took.
    pub revisions: u32,
    /// How many candidates were generated (1 for single-shot).
    pub candidates_run: u32,
}

/// A role resolved to a concrete provider.
struct ResolvedRole<'a> {
    model_ref: ModelRef,
    provider: &'a dyn Provider,
    fallback: Option<FallbackInfo>,
}

/// The outcome of running one candidate (execute + verify + bounded revise).
struct CandidateResult {
    messages: Vec<CompletionMessage>,
    final_text: String,
    /// `Some(reason)` if a turn aborted (budget/loop/step-cap).
    aborted: Option<String>,
    /// Whether this abort is a degradable **infrastructure** setup failure —
    /// no isolation port, or a workspace that could not be snapshotted. `run`
    /// degrades these to a bare worker run rather than ending the turn with
    /// zero work (the "never choose nothing" rule). Deliberately does NOT
    /// cover a witness-integrity rejection (symlink artifact, language
    /// mismatch, a witness author touching production code): those are
    /// fail-closed security decisions and keep their abort. `false` on every
    /// executed and every verified/unverified result.
    degradable: bool,
    /// The verification verdict, if verification ran.
    verdict: Option<Verdict>,
    /// This candidate's verification score, for best-of-N selection.
    score: CandidateScore,
    diff_lines: u32,
    revisions: u32,
}

impl CandidateResult {
    /// A candidate that aborted for a reason that is NOT a degradable
    /// infrastructure setup failure: an execution abort (budget, loop,
    /// step-cap — the worker ran) or a fail-closed security rejection (a
    /// poisoned witness). These keep their stop.
    fn aborted(messages: Vec<CompletionMessage>, reason: String) -> Self {
        Self {
            messages,
            final_text: String::new(),
            aborted: Some(reason),
            degradable: false,
            verdict: None,
            score: CandidateScore::Failed,
            diff_lines: 0,
            revisions: 0,
        }
    }

    /// A candidate that aborted on a degradable **infrastructure** setup
    /// failure — no isolation port, or a tree that could not be snapshotted.
    /// Flagged so `run` degrades it to a bare worker run rather than ending
    /// the turn having done nothing.
    fn setup_aborted(messages: Vec<CompletionMessage>, reason: String) -> Self {
        Self {
            degradable: true,
            ..Self::aborted(messages, reason)
        }
    }
}

/// The candidate-local mutable state one execute+verify+revise pass threads
/// through its phases — grouped so [`Pipeline::run_candidate`]'s sub-methods
/// take one argument instead of seven. Every exit path moves it into the
/// returned [`CandidateResult`].
struct CandidateState {
    messages: Vec<CompletionMessage>,
    final_text: String,
    /// `FileChange` events observed across this candidate's engine turns —
    /// one half of the zero-diff guard's "touched files" signal (L-E2).
    file_changes: u32,
    oracle: FlipOracle,
    /// Untracked-file fingerprints snapshotted before the first turn, so
    /// every diff gather can exclude pre-existing dirty state.
    untracked_before: HashMap<String, String>,
    diff_lines: u32,
    diff_text: String,
    revisions: u32,
}

impl CandidateState {
    /// Finish this candidate with a verification verdict — every verified
    /// exit (deterministic or judged, passed or failed) funnels through here.
    fn into_verified(
        self,
        passed: bool,
        evidence: &JudgeEvidence,
        score: CandidateScore,
    ) -> CandidateResult {
        CandidateResult {
            messages: self.messages,
            final_text: self.final_text,
            aborted: None,
            degradable: false,
            verdict: Some(Verdict::from_evidence(passed, evidence)),
            score,
            diff_lines: self.diff_lines,
            revisions: self.revisions,
        }
    }

    /// Finish a clean lookup: verification skipped, no verdict to report.
    fn into_unverified(self) -> CandidateResult {
        CandidateResult {
            messages: self.messages,
            final_text: self.final_text,
            aborted: None,
            degradable: false,
            verdict: None,
            score: CandidateScore::Unverified,
            diff_lines: self.diff_lines,
            revisions: self.revisions,
        }
    }
}

/// The working-tree surface one candidate executes and verifies against: the
/// session ports (the real tree) on single-shot and shared-tree runs, an
/// isolated snapshot's ports under best-of-N isolation. Grouped and `Copy`
/// so the candidate phases thread one value instead of two borrows.
#[derive(Clone, Copy)]
struct CandidateSurface<'c> {
    diagnostics: &'c dyn DiagnosticRunner,
    tests: &'c dyn TestRunner,
    repo_status: &'c dyn RepoStatusPort,
    cwd: Option<&'c str>,
    hook_runner: Option<&'c dyn HookRunner>,
    workspace: Option<&'c dyn CandidateWorkspace>,
}

/// The staged orchestrator. Holds only borrowed ports + an owned event sender
/// and config; carries no per-run state (the caller owns the message history
/// and budget, exactly as `stella-core::Engine` does).
pub struct Pipeline<'a> {
    router: &'a Router,
    providers: &'a dyn ProviderResolver,
    tools: &'a dyn stella_core::ToolExecutor,
    recall: &'a dyn ContextRecallPort,
    repo: &'a dyn RepoStructurePort,
    repo_status: &'a dyn RepoStatusPort,
    diagnostics: &'a dyn DiagnosticRunner,
    tests: &'a dyn TestRunner,
    approvals: &'a dyn ApprovalGate,
    sleeper: &'a dyn Sleeper,
    hooks: Option<(&'a Hooks, &'a dyn HookRunner)>,
    candidate_workspaces: Option<&'a dyn CandidateWorkspacePort>,
    mcp_prefetch: Option<&'a dyn McpPrefetchPort>,
    steering: Option<&'a dyn stella_core::ports::TurnSteering>,
    events: EventSender,
    config: PipelineConfig,
    configured_test: Result<Option<TestInvocation>, crate::witness::TestInvocationError>,
}

impl<'a> Pipeline<'a> {
    /// Construct a pipeline over the given ports, event sink, and config.
    pub fn new(
        ports: PipelinePorts<'a>,
        events: impl Into<EventSender>,
        config: PipelineConfig,
    ) -> Self {
        let configured_test = config
            .test_command
            .as_deref()
            .map(parse_test_invocation)
            .transpose();
        Self {
            router: ports.router,
            providers: ports.providers,
            tools: ports.tools,
            recall: ports.recall,
            repo: ports.repo,
            repo_status: ports.repo_status,
            diagnostics: ports.diagnostics,
            tests: ports.tests,
            approvals: ports.approvals,
            sleeper: ports.sleeper,
            hooks: ports.hooks,
            candidate_workspaces: ports.candidate_workspaces,
            mcp_prefetch: ports.mcp_prefetch,
            steering: ports.steering,
            events: events.into(),
            config,
            configured_test,
        }
    }

    /// Replace the ordinary channel wrapper with a caller-supplied ordered
    /// sender. Benchmark mode uses this to journal+flush every event before
    /// the same event enters the renderer queue.
    pub fn with_event_sender(mut self, events: EventSender) -> Self {
        self.events = events;
        self
    }

    /// Drive one prompt through the full staged flow. `messages` is the
    /// caller-owned history: seed it with the stable system prefix (the cached
    /// prompt prefix, L-E8); the pipeline appends the volatile recall+goal
    /// message and every execution turn, and on return holds the selected
    /// candidate's trajectory. `budget` accumulates spend across every stage.
    pub async fn run(
        &self,
        goal: &str,
        messages: &mut Vec<CompletionMessage>,
        budget: &mut BudgetGuard,
    ) -> Result<PipelineOutcome, PipelineRunError> {
        let mut total_cost = 0.0f64;
        if let Err(error) = &self.configured_test {
            return Err(PipelineRunError::new(
                PipelineError::InvalidTestCommand(error.to_string()),
                total_cost,
            ));
        }
        if messages.is_empty() {
            messages.push(CompletionMessage::system(DEFAULT_SYSTEM_PROMPT));
        }

        // --- 1+2. Evaluate (triage) + context recall, overlapped. ----------
        // Triage's class first gates stage 3 and recall consumes only the
        // goal — no data dependency — so the triage model call and the
        // recall embedding/store scan run concurrently instead of paying
        // both latencies back-to-back on every prompt. Stage-event order is
        // unchanged: join polls triage first (it emits Stage::Triage before
        // its first await), then the recall future emits
        // Stage::ContextRecall before its own first await.
        let recall_future = async {
            self.emit(AgentEvent::Stage {
                name: StageKind::ContextRecall,
            });
            self.recall.recall(goal).await
        };
        let (assessment, frames) =
            tokio::join!(self.triage(goal, budget, &mut total_cost), recall_future);
        self.emit_context_recall(&frames);
        let assessment = match assessment {
            Ok(assessment) => assessment,
            Err(abort) => {
                return Ok(self.aborted_before_execute(
                    resolve_task_class(None, goal),
                    total_cost,
                    &abort.reason,
                ));
            }
        };
        let task_class = assessment.class;
        // The volatile recall+goal message rides AFTER the stable system
        // prefix (L-E8) — see assemble_user_message.
        messages.push(CompletionMessage::user(assemble_user_message(
            goal, &frames,
        )));

        // --- 3. Plan (skipped for simple/single-task). ---------------------
        let plan: Option<Vec<PlanStep>> = if task_class.plans() {
            let repo_structure = self.repo.structure_summary().await;
            match self
                .plan_stage(goal, &frames, &repo_structure, budget, &mut total_cost)
                .await
            {
                Ok(plan) => Some(plan),
                Err(abort) => {
                    return Ok(self.aborted_before_execute(task_class, total_cost, &abort.reason));
                }
            }
        } else {
            None
        };
        // --- 4. Scope review (only for planned work above thresholds). -----
        let plan = match plan {
            Some(steps) => match self.scope_review(goal, steps).await {
                Ok(Some(steps)) => Some(steps),
                Ok(None) => {
                    // User aborted (or trimmed to nothing) at the gate.
                    return Ok(self.aborted_before_execute(
                        task_class,
                        total_cost,
                        "aborted at scope review",
                    ));
                }
                Err(cause) => return Err(PipelineRunError::new(cause, total_cost)),
            },
            None => None,
        };

        // --- 5. Witness + execute + verify (single-shot or best-of-N). ------
        let n = self.config.candidate_count();
        let base_messages = messages.clone();
        // Decided here, before the single-shot/best-of-N split, because an
        // authored witness is the *only* reason a single candidate needs
        // disposable isolation. Resolving independence later would commit the
        // run to snapshot machinery it then discovers it cannot use — and
        // candidate isolation requires a git working tree, so on a plain
        // directory that is a hard failure rather than an unused cost.
        let authored_witness = self.config.test_command.is_none()
            && self.config.witness_writer
            && assessment.wants_witness()
            && task_class.verifies_unconditionally()
            && self.can_author_independent_witness();
        // Single-shot (the default) runs directly over the session ports —
        // zero snapshot/adoption machinery only when the user supplied the
        // test invocation (or witness authoring is otherwise disabled).
        // Authored witnesses always require a disposable candidate, even at
        // N=1, so authoring can never mutate the session tree.
        // Best-of-N runs every candidate in an isolated snapshot of the
        // current tree state and adopts only the winner's changes (L-E7).
        let (best, worker_model_label) = if n == 1 && !authored_witness {
            let worker = match self.resolve_provider(Role::Worker) {
                Ok(worker) => worker,
                Err(error) => {
                    return Err(PipelineRunError::new(
                        error.into_pipeline_error(),
                        total_cost,
                    ));
                }
            };
            if let Some(fallback) = &worker.fallback {
                self.emit_fallback(fallback);
            }
            let worker_model_label = worker.model_ref.to_string();
            let mut single = self
                .run_shared_candidates(
                    goal,
                    &base_messages,
                    plan.as_deref(),
                    assessment,
                    None,
                    &worker,
                    1,
                    budget,
                    &mut total_cost,
                )
                .await;
            let best = single
                .pop()
                .expect("run_shared_candidates returns one result per requested candidate");
            (best, Some(worker_model_label))
        } else {
            match self
                .run_best_of_n(
                    goal,
                    &base_messages,
                    plan.as_deref(),
                    assessment,
                    n,
                    &frames,
                    authored_witness,
                    budget,
                    &mut total_cost,
                )
                .await
            {
                Ok(result) => result,
                Err(cause) => return Err(PipelineRunError::new(cause, total_cost)),
            }
        };

        // The "never choose nothing" backstop. A candidate that aborted
        // BEFORE the worker ever ran — a setup failure (no isolation port, no
        // independent witness author, a tree that could not be snapshotted) —
        // must not end the turn having executed nothing. The fancy path being
        // unavailable is a reason to do LESS, never a reason to do nothing:
        // degrade to a bare worker run on the working tree. Execution aborts
        // (budget, loop, step-cap) and true resource limits keep their stop —
        // there the worker DID run, so the abort is honest.
        let best = if best.aborted.is_some() && best.degradable {
            match self
                .degrade_to_bare_execution(
                    goal,
                    &base_messages,
                    plan.as_deref(),
                    assessment,
                    budget,
                    &mut total_cost,
                )
                .await
            {
                Some(executed) => executed,
                // Even the bare run could not start (no resolvable worker) —
                // that is a genuine impossibility, so keep the setup abort.
                None => best,
            }
        } else {
            best
        };

        // Adopt the winning candidate's trajectory.
        *messages = best.messages;

        // --- 6. Complete. --------------------------------------------------
        if let Some(reason) = best.aborted {
            self.emit(AgentEvent::Error {
                message: reason.clone(),
                retryable: false,
            });
            return Ok(PipelineOutcome {
                status: PipelineStatus::Aborted { reason },
                task_class,
                final_text: best.final_text,
                total_cost_usd: total_cost,
                verdict: best.verdict,
                revisions: best.revisions,
                candidates_run: n,
            });
        }

        self.emit(AgentEvent::Stage {
            name: StageKind::Complete,
        });
        let status = match &best.verdict {
            Some(verdict) if !verdict.passed => PipelineStatus::VerificationFailed {
                verdict: verdict.clone(),
            },
            _ => PipelineStatus::Completed,
        };
        match &status {
            PipelineStatus::Completed => self.emit(AgentEvent::Complete {
                model: worker_model_label.unwrap_or_default(),
                cost_usd: total_cost,
            }),
            PipelineStatus::VerificationFailed { verdict } => {
                self.emit(AgentEvent::Error {
                    message: format!("verification failed: {}", verdict.summary),
                    retryable: false,
                });
            }
            PipelineStatus::Aborted { .. } => {
                unreachable!("aborted candidates return before terminal verification")
            }
        }
        Ok(PipelineOutcome {
            status,
            task_class,
            final_text: best.final_text,
            total_cost_usd: total_cost,
            verdict: best.verdict,
            revisions: best.revisions,
            candidates_run: n,
        })
    }

    // ------------------------------------------------------------------
    // Stage: triage
    // ------------------------------------------------------------------

    async fn triage(
        &self,
        goal: &str,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> Result<TaskAssessment, PipelineBudgetAbort> {
        self.emit(AgentEvent::Stage {
            name: StageKind::Triage,
        });
        let resolved = match self.resolve_provider(Role::Triage) {
            Ok(r) => r,
            // Triage resolution failure is soft: fall through to the full path
            // via the deterministic floor. Never fail the run on triage.
            Err(_) => {
                return Ok(TaskAssessment::from_class(resolve_task_class(None, goal)));
            }
        };
        if let Some(fb) = &resolved.fallback {
            self.emit_fallback(fb);
        }

        let assessment = match self
            .metered_raw_call(
                RawCall {
                    role: ModelCallRole::Triage,
                    resolved: &resolved,
                    messages: vec![CompletionMessage::user(triage_prompt(goal))],
                    policy: RetryPolicy::deterministic(),
                    overrides: &self.config.role_overrides.triage,
                    timeout: Some(self.config.triage_latency_ceiling),
                },
                budget,
                total,
            )
            .await
        {
            Ok(result) => parse_triage_response(&result.text),
            Err(RawCallError::Budget(abort)) => return Err(abort),
            Err(RawCallError::Provider | RawCallError::Timeout) => None,
        };
        // The class still goes through `resolve_task_class` so a failed or
        // unparseable triage lands on the deterministic floor exactly as
        // before; a real assessment keeps its own assurance flags.
        Ok(match assessment {
            Some(assessment) => TaskAssessment {
                class: resolve_task_class(Some(assessment.class), goal),
                ..assessment
            },
            None => TaskAssessment::from_class(resolve_task_class(None, goal)),
        })
    }

    // ------------------------------------------------------------------
    // Stage: plan
    // ------------------------------------------------------------------

    async fn plan_stage(
        &self,
        goal: &str,
        recall: &[RecalledFrame],
        repo_structure: &str,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> Result<Vec<PlanStep>, PipelineBudgetAbort> {
        self.emit(AgentEvent::Stage {
            name: StageKind::Plan,
        });
        let fallback_plan = || vec![PlanStep::new(goal)];

        let resolved = match self.resolve_provider(Role::Plan) {
            Ok(r) => r,
            Err(_) => return Ok(fallback_plan()),
        };
        if let Some(fb) = &resolved.fallback {
            self.emit_fallback(fb);
        }

        let prompt = build_planner_prompt(goal, recall, repo_structure);
        // Plan rides the worker's settings (same router tier, same tuning).
        let worker_overrides = RoleCallOverrides::default();
        let result = match self
            .metered_raw_call(
                RawCall {
                    role: ModelCallRole::Plan,
                    resolved: &resolved,
                    messages: vec![CompletionMessage::user(prompt)],
                    policy: RetryPolicy::standard(),
                    overrides: &worker_overrides,
                    timeout: None,
                },
                budget,
                total,
            )
            .await
        {
            Ok(r) => r,
            Err(RawCallError::Budget(abort)) => return Err(abort),
            Err(RawCallError::Provider | RawCallError::Timeout) => return Ok(fallback_plan()),
        };

        if let Some(steps) = parse_plan(&result.text) {
            return Ok(steps);
        }

        // One bounded JSON-repair retry (L-V2), deterministic (no retry-hang).
        match self
            .metered_raw_call(
                RawCall {
                    role: ModelCallRole::PlanRepair,
                    resolved: &resolved,
                    messages: vec![CompletionMessage::user(plan_repair_prompt(&result.text))],
                    policy: RetryPolicy::deterministic(),
                    overrides: &worker_overrides,
                    timeout: None,
                },
                budget,
                total,
            )
            .await
        {
            Ok(repair) => {
                if let Some(steps) = parse_plan(&repair.text) {
                    return Ok(steps);
                }
            }
            Err(RawCallError::Budget(abort)) => return Err(abort),
            Err(RawCallError::Provider | RawCallError::Timeout) => {}
        }

        // Degrade to a single-step plan rather than failing — a planner that
        // won't produce a parseable plan must still let the work proceed.
        Ok(fallback_plan())
    }

    // ------------------------------------------------------------------
    // Stage: scope review
    // ------------------------------------------------------------------

    /// Returns `Ok(Some(plan))` to proceed (possibly trimmed), `Ok(None)` if
    /// the user aborted/emptied the plan, or `Err` if headless without bypass.
    async fn scope_review(
        &self,
        goal: &str,
        plan: Vec<PlanStep>,
    ) -> Result<Option<Vec<PlanStep>>, PipelineError> {
        // Coarse blast-radius estimate: one file per step is a deliberately
        // rough proxy until the planner emits per-step file hints (a
        // documented follow-up). Cost estimate is left to the caller/planner.
        let estimate = ScopeEstimate {
            estimated_files: plan.len() as u32,
            estimated_cost_usd: None,
        };
        if !needs_scope_review(&plan, &estimate, &self.config.scope_thresholds) {
            return Ok(Some(plan));
        }

        if self.config.headless && self.config.headless_bypass_scope_review {
            // Bypass means proceed, not "ask a gate that always says no".
            // The headless approval port is `AlwaysAbortGate`, so running the
            // review anyway would empty the plan and end the turn having done
            // nothing — the same zero-work outcome as the error below, just
            // spelled differently.
            return Ok(Some(plan));
        }
        if self.config.headless {
            // Say why the run is ending. This error leaves through the
            // `Result`, not the event stream, so without this the stream just
            // stops mid-plan: a consumer reading only events sees a run that
            // vanished with no terminal event and no explanation.
            self.emit(AgentEvent::Error {
                message: format!(
                    "this plan needs scope review ({} steps, ~{} files) and a headless \
                     run has nobody to ask; set `headless_scope_bypass: on` where the \
                     working tree is disposable, or raise the scope thresholds",
                    plan.len(),
                    estimate.estimated_files
                ),
                retryable: false,
            });
            return Err(PipelineError::ScopeReviewRequiredHeadless);
        }

        self.emit(AgentEvent::Stage {
            name: StageKind::ScopeReview,
        });
        let proposal = build_proposal(goal, &plan, &estimate);
        self.emit(AgentEvent::ScopeReview {
            proposal: proposal.clone(),
        });

        match self.approvals.review(&proposal).await {
            ScopeDecision::Approve => Ok(Some(plan)),
            ScopeDecision::Trim { keep_steps } => {
                let trimmed = apply_trim(&plan, &keep_steps);
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed))
                }
            }
            ScopeDecision::Abort => Ok(None),
        }
    }

    // ------------------------------------------------------------------
    // Stage: execute + verify — candidate generation and selection
    // ------------------------------------------------------------------

    /// Run `n` candidates sequentially over the session ports (the real
    /// working tree): the single-shot path, and the shared-tree degradation
    /// of best-of-N when no [`CandidateWorkspacePort`] is wired.
    #[allow(clippy::too_many_arguments)]
    /// Last-resort execution when candidate setup failed before the worker
    /// ever ran. Runs exactly one worker turn on the session tree — no
    /// isolation, no authored witness, the simplest path that still does the
    /// work. Returns `None` only when there is no resolvable worker provider
    /// (a true impossibility, not a degradable setup failure), in which case
    /// the caller keeps the original setup abort.
    async fn degrade_to_bare_execution(
        &self,
        goal: &str,
        base_messages: &[CompletionMessage],
        plan: Option<&[PlanStep]>,
        assessment: TaskAssessment,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> Option<CandidateResult> {
        let worker = self.resolve_provider(Role::Worker).ok()?;
        if let Some(fallback) = &worker.fallback {
            self.emit_fallback(fallback);
        }
        self.warn(
            "candidate setup failed before any execution; running a bare worker turn on the \
             working tree so the turn still does the work it was asked to"
                .to_string(),
        );
        self.run_shared_candidates(
            goal,
            base_messages,
            plan,
            assessment,
            None,
            &worker,
            1,
            budget,
            total,
        )
        .await
        .pop()
    }

    async fn run_shared_candidates(
        &self,
        goal: &str,
        base_messages: &[CompletionMessage],
        plan: Option<&[PlanStep]>,
        assessment: TaskAssessment,
        witness: Option<&Witness>,
        worker: &ResolvedRole<'a>,
        n: u32,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> Vec<CandidateResult> {
        let mut engine = Engine::with_sleeper(
            worker.provider,
            self.tools,
            self.config.engine.clone(),
            self.sleeper,
        );
        if let Some((hooks, runner)) = self.hooks {
            engine = engine.with_hooks(hooks, runner);
        }
        if let Some(steering) = self.steering {
            engine = engine.with_steering(steering);
        }
        let surface = CandidateSurface {
            diagnostics: self.diagnostics,
            tests: self.tests,
            repo_status: self.repo_status,
            cwd: None,
            hook_runner: None,
            workspace: None,
        };
        let mut results: Vec<CandidateResult> = Vec::with_capacity(n as usize);
        for _ in 0..n {
            results.push(
                self.run_candidate(
                    goal,
                    base_messages,
                    plan,
                    assessment,
                    witness,
                    &engine,
                    surface,
                    budget,
                    total,
                )
                .await,
            );
        }
        results
    }

    /// Best-of-N over isolated workspaces (L-E7): each candidate executes and
    /// verifies inside its own snapshot of the current tree state, so
    /// siblings never see each other's edits and losers leave no residue —
    /// only the winner's changes are applied to the real tree. Cleanup is
    /// unconditional (success or failure) with one deliberate exception: a
    /// winner whose adoption failed keeps its workspace, named in the error,
    /// so completed work is never destroyed.
    #[allow(clippy::too_many_arguments)]
    async fn run_best_of_n(
        &self,
        goal: &str,
        base_messages: &[CompletionMessage],
        plan: Option<&[PlanStep]>,
        assessment: TaskAssessment,
        n: u32,
        frames: &[RecalledFrame],
        author_witness: bool,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> Result<(CandidateResult, Option<String>), PipelineError> {
        // Orchestrator pre-fetch (issue #248 Phase 1) — see `crate::mcp_prefetch::fold`.
        let prefetched = crate::mcp_prefetch::fold(self.mcp_prefetch, goal, n, base_messages).await;
        let base_messages: &[CompletionMessage] = prefetched.as_deref().unwrap_or(base_messages);
        let Some(port) = self.candidate_workspaces else {
            if author_witness {
                return Ok((
                    CandidateResult::setup_aborted(
                        base_messages.to_vec(),
                        "authored witness requires candidate isolation, but no candidate \
                         workspace port is available"
                            .to_string(),
                    ),
                    None,
                ));
            }
            // No isolation port (non-git workspace, or a caller that never
            // wired one): the historical shared-tree behavior, made loud —
            // candidates see each other's residue and losers' edits stay on
            // disk.
            self.warn(format!(
                "best-of-N candidate isolation is unavailable (no candidate workspace port): \
                 {n} candidates will run sequentially in the shared working tree, and losing \
                 candidates' file changes will not be rolled back"
            ));
            let worker = self
                .resolve_provider(Role::Worker)
                .map_err(RoleResolveError::into_pipeline_error)?;
            if let Some(fallback) = &worker.fallback {
                self.emit_fallback(fallback);
            }
            let label = worker.model_ref.to_string();
            let candidates = self
                .run_shared_candidates(
                    goal,
                    base_messages,
                    plan,
                    assessment,
                    None,
                    &worker,
                    n,
                    budget,
                    total,
                )
                .await;
            let best_idx = best_index(&candidates);
            return Ok((
                candidates
                    .into_iter()
                    .nth(best_idx)
                    .expect("best_index returns an in-range index"),
                Some(label),
            ));
        };

        // Resolve both identities before creating a workspace or dispatching
        // the witness. Authored verification is meaningful only when its
        // author is actually independent from the worker.
        let worker = self
            .resolve_provider(Role::Worker)
            .map_err(RoleResolveError::into_pipeline_error)?;
        if let Some(fallback) = &worker.fallback {
            self.emit_fallback(fallback);
        }
        let worker_label = worker.model_ref.to_string();
        // Losing the independent author costs the run its authored witness —
        // it must never cost the run the whole task. A single-model
        // configuration (every role pinned to one model, as benchmark and
        // solo-provider setups do) previously aborted here after one model
        // call, having done no work at all. Degrade to the unauthored verify
        // ladder instead, and say so once.
        // `can_author_independent_witness` already gated `author_witness` and
        // announced any degradation, so this is the invariant guard for that
        // decision — silent on purpose, never a second warning.
        let mut author_witness = author_witness;
        let witness_author = match author_witness
            .then(|| self.resolve_provider(Role::Judge))
            .and_then(Result::ok)
            .filter(|author| author.model_ref != worker.model_ref)
        {
            Some(author) => {
                if let Some(fallback) = &author.fallback {
                    self.emit_fallback(fallback);
                }
                Some(author)
            }
            None => {
                author_witness = false;
                None
            }
        };

        let mut candidates: Vec<CandidateResult> = Vec::with_capacity(n as usize);
        let mut workspaces: Vec<Option<Box<dyn CandidateWorkspace>>> =
            Vec::with_capacity(n as usize);
        for i in 0..n {
            let ws = match port.create().await {
                Ok(ws) => ws,
                Err(e) => {
                    // Isolation was promised, so a candidate that cannot be
                    // isolated is never run in the shared tree instead: it
                    // scores as aborted and the remaining candidates go on.
                    self.warn(format!("candidate {}/{n} skipped: {e}", i + 1));
                    candidates.push(CandidateResult::setup_aborted(
                        base_messages.to_vec(),
                        format!("candidate isolation failed: {e}"),
                    ));
                    workspaces.push(None);
                    continue;
                }
            };
            let result = {
                let bound_hook_runner = self.hooks.map(|(_, runner)| BoundHookRunner {
                    inner: runner,
                    cwd: ws.root(),
                });
                let surface = CandidateSurface {
                    diagnostics: ws.diagnostics(),
                    tests: ws.tests(),
                    repo_status: ws.repo_status(),
                    cwd: Some(ws.root()),
                    hook_runner: bound_hook_runner
                        .as_ref()
                        .map(|runner| runner as &dyn HookRunner),
                    workspace: Some(ws.as_ref()),
                };
                let witness = if author_witness {
                    let author = witness_author
                        .as_ref()
                        .expect("authored witness identity is resolved before dispatch");
                    match self
                        .witness_stage(goal, frames, author, surface, budget, total)
                        .await
                    {
                        Ok(witness) => witness,
                        Err(abort) => {
                            // A witness that couldn't be AUTHORED degrades to a
                            // bare run (the task needs no witness). An
                            // artifact-INTEGRITY violation stays fail-closed,
                            // so a poisoned witness is surfaced, never silently
                            // traded for an unverified run.
                            let candidate = if abort.degradable {
                                CandidateResult::setup_aborted(base_messages.to_vec(), abort.reason)
                            } else {
                                CandidateResult::aborted(base_messages.to_vec(), abort.reason)
                            };
                            candidates.push(candidate);
                            workspaces.push(Some(ws));
                            continue;
                        }
                    }
                } else {
                    None
                };
                let mut engine = Engine::with_sleeper(
                    worker.provider,
                    ws.tools(),
                    self.engine_config_for(surface),
                    self.sleeper,
                );
                if let Some((hooks, runner)) = self.hooks {
                    engine = engine.with_hooks(hooks, surface.hook_runner.unwrap_or(runner));
                }
                if let Some(steering) = self.steering {
                    engine = engine.with_steering(steering);
                }
                self.run_candidate(
                    goal,
                    base_messages,
                    plan,
                    assessment,
                    witness.as_ref(),
                    &engine,
                    surface,
                    budget,
                    total,
                )
                .await
            };
            candidates.push(result);
            workspaces.push(Some(ws));
        }

        let best_idx = best_index(&candidates);
        // Winner adoption + cleanup. An aborted winner adopts nothing — an
        // aborted best-of-N run leaves the real tree untouched.
        let mut adopt_failure: Option<WorkspaceError> = None;
        for (i, ws) in workspaces.into_iter().enumerate() {
            let Some(ws) = ws else { continue };
            if i == best_idx
                && candidates[best_idx]
                    .verdict
                    .as_ref()
                    .is_some_and(|verdict| verdict.passed)
            {
                match ws.adopt().await {
                    Ok(adopted) => {
                        // Surface the adopted paths on the event stream: the
                        // winner's edits happened inside the snapshot, so no
                        // FileChange was emitted for the real tree yet.
                        for change in adopted {
                            self.emit(AgentEvent::FileChange {
                                path: change.path,
                                kind: change.kind,
                                diff: None,
                            });
                        }
                        ws.remove().await;
                    }
                    // Keep the workspace: the error names its path and the
                    // conflicting files; the winning work stays recoverable.
                    Err(e) => adopt_failure = Some(e),
                }
            } else {
                ws.remove().await;
            }
        }
        let mut best = candidates
            .into_iter()
            .nth(best_idx)
            .expect("best_index returns an in-range index");
        if let Some(e) = adopt_failure {
            best.aborted = Some(e.to_string());
        }
        Ok((best, Some(worker_label)))
    }

    // ------------------------------------------------------------------
    // Stages: execute + verify + revise (one candidate)
    // ------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn run_candidate(
        &self,
        goal: &str,
        base_messages: &[CompletionMessage],
        plan: Option<&[PlanStep]>,
        assessment: TaskAssessment,
        witness: Option<&Witness>,
        engine: &Engine<'_>,
        surface: CandidateSurface<'_>,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> CandidateResult {
        // Flip oracle: for classes we always verify, take a pre-execute
        // baseline of the test command so a later pass counts as a genuine
        // fail→pass flip (L-E11). Simple lookups skip the baseline — they are
        // only verified at all if the zero-diff guard trips, and then the
        // absence of a baseline correctly leaves the oracle unflipped.
        // The baseline runs per candidate even though the witness stage
        // already saw its command fail once: the observation must come from
        // this candidate's own surface (its isolated snapshot under
        // best-of-N, or a shared tree an earlier candidate may have
        // modified), and a seeded observation would be a fabricated one.
        let mut oracle = FlipOracle::new();
        if assessment.class.verifies_unconditionally()
            && let Some(cmd) = self.effective_test_command(witness)
        {
            let pre = surface.tests.run_test(cmd.invocation).await;
            oracle.observe(cmd.command, pre.passed());
        }

        // Snapshot untracked files (with content fingerprints) BEFORE
        // executing so `gather_diff` can tell files this turn created OR
        // modified from pre-existing dirty state — a stale untracked file with
        // an unchanged fingerprint is not this turn's work, but one the turn
        // edited (fingerprint changed) is.
        let untracked_before = surface.repo_status.untracked_fingerprints().await;

        let mut state = CandidateState {
            messages: base_messages.to_vec(),
            final_text: String::new(),
            file_changes: 0,
            oracle,
            untracked_before,
            diff_lines: 0,
            diff_text: String::new(),
            revisions: 0,
        };

        if let Err(reason) = self
            .execute_plan(plan, engine, budget, total, &mut state)
            .await
        {
            return CandidateResult::aborted(state.messages, reason);
        }

        // Decide whether to verify: unconditional for single/multi; for a
        // simple lookup, only if the turn unexpectedly touched files (the
        // zero-diff guard, L-E2). "Touched files" = FileChange events observed
        // OR a non-empty diff.
        (state.diff_lines, state.diff_text) =
            self.gather_diff(surface, &state.untracked_before).await;
        let files_touched = state.file_changes > 0 || !state.diff_text.trim().is_empty();
        let should_verify = assessment.class.verifies_unconditionally()
            || (assessment.class == TaskClass::SimpleLookup && files_touched);
        if !should_verify {
            // A clean lookup: nothing to verify.
            return state.into_unverified();
        }

        self.verify_candidate(
            goal, assessment, witness, engine, surface, budget, total, state,
        )
        .await
    }

    /// Execute stage: one turn for simple/single-task; one turn per plan step
    /// for multi-step (each step guides a fresh engine turn). The last turn's
    /// text lands in `state.final_text`; `Err` is the first aborted turn's
    /// reason.
    async fn execute_plan(
        &self,
        plan: Option<&[PlanStep]>,
        engine: &Engine<'_>,
        budget: &mut BudgetGuard,
        total: &mut f64,
        state: &mut CandidateState,
    ) -> Result<(), String> {
        self.emit(AgentEvent::Stage {
            name: StageKind::Execute,
        });
        let steps: Vec<&PlanStep> = plan.map(|p| p.iter().collect()).unwrap_or_default();
        if steps.is_empty() {
            match self
                .run_engine_turn(engine, &mut state.messages, budget, &mut state.file_changes)
                .await
            {
                TurnOutcome::Completed { text, cost_usd } => {
                    state.final_text = text;
                    *total += cost_usd;
                }
                TurnOutcome::Aborted { reason, cost_usd } => {
                    *total += cost_usd;
                    return Err(reason);
                }
            }
        } else {
            let n = steps.len();
            for (i, step) in steps.iter().enumerate() {
                state.messages.push(CompletionMessage::user(format!(
                    "Step {}/{}: {}",
                    i + 1,
                    n,
                    step.description
                )));
                match self
                    .run_engine_turn(engine, &mut state.messages, budget, &mut state.file_changes)
                    .await
                {
                    TurnOutcome::Completed { text, cost_usd } => {
                        state.final_text = text;
                        *total += cost_usd;
                    }
                    TurnOutcome::Aborted { reason, cost_usd } => {
                        *total += cost_usd;
                        return Err(reason);
                    }
                }
            }
        }
        Ok(())
    }

    /// Verify + bounded revise loop over an executed candidate: observe the
    /// tests, take the deterministic ladder decision (L-E11), and either
    /// finish with a verdict, escalate to the model judge, or spend one of
    /// `max_revisions` on a revise pass and re-observe. Owns `state` because
    /// every exit moves it into the returned [`CandidateResult`].
    #[allow(clippy::too_many_arguments)]
    async fn verify_candidate(
        &self,
        goal: &str,
        assessment: TaskAssessment,
        witness: Option<&Witness>,
        engine: &Engine<'_>,
        surface: CandidateSurface<'_>,
        budget: &mut BudgetGuard,
        total: &mut f64,
        mut state: CandidateState,
    ) -> CandidateResult {
        self.emit(AgentEvent::Stage {
            name: StageKind::Verify,
        });
        let effective_cmd = self.effective_test_command(witness);
        loop {
            if let Some(workspace) = surface.workspace
                && let Err(error) = workspace.seal().await
            {
                return CandidateResult::aborted(
                    state.messages,
                    format!("candidate could not be sealed for verification: {error}"),
                );
            }
            let (touched_tests_passed, test_tail) = self
                .observe_touched_tests(surface, effective_cmd, &mut state.oracle)
                .await;
            // Tamper exclusion is an authority boundary, not evidence for a
            // model to weigh. Any post-baseline witness mutation hard-fails
            // the candidate before a judge can override it.
            let mut tampered = Vec::new();
            if let Some(witness) = witness {
                for (path, expected) in &witness.files {
                    let current = surface.repo_status.artifact_identity(path).await;
                    if !witness_identity_matches(expected, current.as_ref()) {
                        tampered.push(path.clone());
                    }
                }
                tampered.sort();
            }
            if !tampered.is_empty() {
                return CandidateResult::aborted(
                    state.messages,
                    format!(
                        "witness artifact changed after its accepted baseline: {}",
                        tampered.join(", ")
                    ),
                );
            }
            if let Some(workspace) = surface.workspace {
                match workspace.sealed_is_unchanged().await {
                    Ok(true) => {}
                    Ok(false) => {
                        return CandidateResult::aborted(
                            state.messages,
                            "candidate worktree changed after verification".to_string(),
                        );
                    }
                    Err(error) => {
                        return CandidateResult::aborted(
                            state.messages,
                            format!("could not validate the verified candidate seal: {error}"),
                        );
                    }
                }
            }
            let inputs = LadderInputs {
                flip_achieved: state.oracle.is_flipped(),
                touched_tests_passed,
                diff_lines: state.diff_lines,
                diff_budget: self.config.diff_budget_lines,
            };

            match ladder_decision(&inputs) {
                LadderDecision::SubmitFast => {
                    // Deterministic pass — judge SKIPPED (L-E11).
                    let evidence = deterministic_pass_evidence(
                        state.oracle.tracked_command(),
                        state.diff_lines,
                    );
                    self.emit(AgentEvent::JudgeVerdict {
                        passed: true,
                        evidence: evidence.clone(),
                    });
                    return state.into_verified(
                        true,
                        &evidence,
                        score_from_verification(true, None),
                    );
                }
                LadderDecision::Revise => {
                    // Deterministic failure (touched tests red) — no judge.
                    let evidence = deterministic_fail_evidence(&test_tail);
                    self.emit(AgentEvent::JudgeVerdict {
                        passed: false,
                        evidence: evidence.clone(),
                    });
                    if state.revisions >= self.config.max_revisions {
                        return state.into_verified(
                            false,
                            &evidence,
                            score_from_verification(false, Some(false)),
                        );
                    }
                    // Distress trigger: a SECOND consecutive deterministic
                    // failure means the raw evidence alone didn't steer the
                    // worker — spend one judge call on course-correction
                    // (event-triggered, never a fixed midpoint checkpoint).
                    let mut reason = evidence.summary.clone();
                    if self.config.distress_guidance && state.revisions >= 1 {
                        match self
                            .judge_guidance(
                                goal,
                                &state.diff_text,
                                &evidence.summary,
                                budget,
                                total,
                            )
                            .await
                        {
                            Ok(Some(guidance)) => {
                                reason.push_str("\n\nIndependent reviewer course-correction:\n");
                                reason.push_str(&guidance);
                            }
                            Ok(None) => {}
                            Err(abort) => {
                                return CandidateResult::aborted(state.messages, abort.reason);
                            }
                        }
                    }
                    if let Err(reason) = self
                        .revise_candidate(engine, surface, budget, &reason, total, &mut state)
                        .await
                    {
                        return CandidateResult::aborted(state.messages, reason);
                    }
                }
                // Triage judged this result not worth a separate reviewer.
                // Record exactly that: a pass carrying no independent
                // evidence. Falling through to `heuristic_fallback` would
                // report "judge unavailable", which describes a judge that
                // broke — not one that was deliberately waived — and would
                // turn a task triage called simple into a verification
                // failure. The summary states plainly what was not done.
                LadderDecision::ModelJudge if !assessment.wants_judge() => {
                    let evidence = JudgeEvidence {
                        summary: "model review waived by triage; no independent \
                                  verification was performed"
                            .to_string(),
                        deterministic: false,
                        evidence_refs: vec![],
                    };
                    self.emit(AgentEvent::JudgeVerdict {
                        passed: true,
                        evidence: evidence.clone(),
                    });
                    return state.into_verified(
                        true,
                        &evidence,
                        score_from_verification(true, None),
                    );
                }
                LadderDecision::ModelJudge => {
                    // Inconclusive — escalate to the model judge (judge ≠
                    // worker; a judge-call failure falls back to a heuristic).
                    let evidence_summary = format!(
                        "flip_achieved={}; touched_tests={:?}; diff_lines={} (budget {})",
                        inputs.flip_achieved,
                        inputs.touched_tests_passed,
                        state.diff_lines,
                        self.config.diff_budget_lines,
                    );
                    let verdict = match self
                        .judge(
                            goal,
                            &state.diff_text,
                            &evidence_summary,
                            &inputs,
                            budget,
                            total,
                        )
                        .await
                    {
                        Ok(verdict) => verdict,
                        Err(abort) => {
                            return CandidateResult::aborted(state.messages, abort.reason);
                        }
                    };
                    let evidence = model_verdict_evidence(&verdict);
                    self.emit(AgentEvent::JudgeVerdict {
                        passed: verdict.passed,
                        evidence: evidence.clone(),
                    });
                    if verdict.passed {
                        return state.into_verified(
                            true,
                            &evidence,
                            score_from_verification(false, Some(true)),
                        );
                    }
                    if state.revisions >= self.config.max_revisions {
                        return state.into_verified(
                            false,
                            &evidence,
                            score_from_verification(false, Some(false)),
                        );
                    }
                    if let Err(reason) = self
                        .revise_candidate(
                            engine,
                            surface,
                            budget,
                            &verdict.reasoning,
                            total,
                            &mut state,
                        )
                        .await
                    {
                        return CandidateResult::aborted(state.messages, reason);
                    }
                }
            }
        }
    }

    /// Post-execute test observation for the flip oracle + the touched-tests
    /// signal: `(Some(passed), stderr tail)` when a test command is available
    /// (configured or witness-authored), `(None, "")` when there is nothing
    /// to run.
    async fn observe_touched_tests(
        &self,
        surface: CandidateSurface<'_>,
        cmd: Option<EffectiveTestCommand<'_>>,
        oracle: &mut FlipOracle,
    ) -> (Option<bool>, String) {
        match cmd {
            Some(cmd) => {
                let post = surface.tests.run_test(cmd.invocation).await;
                let passed = post.passed();
                oracle.observe(cmd.command, passed);
                (Some(passed), post.stderr_tail)
            }
            None => (None, String::new()),
        }
    }

    fn effective_test_command<'c>(
        &'c self,
        witness: Option<&'c Witness>,
    ) -> Option<EffectiveTestCommand<'c>> {
        if let Ok(Some(invocation)) = &self.configured_test {
            return self
                .config
                .test_command
                .as_deref()
                .map(|command| EffectiveTestCommand {
                    command,
                    invocation,
                });
        }
        witness.map(|w| EffectiveTestCommand {
            command: &w.command,
            invocation: &w.invocation,
        })
    }

    /// Spend one revision: run [`Pipeline::revise_turn`] with the failure
    /// evidence and fold the fresh diff back into `state`. `Err` is the abort
    /// reason of a turn that died mid-revision (budget/loop).
    async fn revise_candidate(
        &self,
        engine: &Engine<'_>,
        surface: CandidateSurface<'_>,
        budget: &mut BudgetGuard,
        reason: &str,
        total: &mut f64,
        state: &mut CandidateState,
    ) -> Result<(), String> {
        let (diff_lines, diff_text) = self
            .revise_turn(
                engine,
                surface,
                &mut state.messages,
                budget,
                reason,
                &mut state.file_changes,
                &mut state.final_text,
                total,
                &state.untracked_before,
            )
            .await?;
        state.diff_lines = diff_lines;
        state.diff_text = diff_text;
        state.revisions += 1;
        Ok(())
    }

    /// Run one revision turn: append an evidence-carrying instruction, execute,
    /// and re-gather the diff. Emits the `Execute`/`Verify` stage bookends so
    /// the stream shows the revise loop. Returns the fresh `(diff_lines,
    /// diff_text)` on success, or the abort reason on a budget/loop abort.
    #[allow(clippy::too_many_arguments)]
    async fn revise_turn(
        &self,
        engine: &Engine<'_>,
        surface: CandidateSurface<'_>,
        messages: &mut Vec<CompletionMessage>,
        budget: &mut BudgetGuard,
        reason: &str,
        file_changes: &mut u32,
        final_text: &mut String,
        total: &mut f64,
        untracked_before: &HashMap<String, String>,
    ) -> Result<(u32, String), String> {
        messages.push(CompletionMessage::user(revision_prompt(reason)));
        self.emit(AgentEvent::Stage {
            name: StageKind::Execute,
        });
        match self
            .run_engine_turn(engine, messages, budget, file_changes)
            .await
        {
            TurnOutcome::Completed { text, cost_usd } => {
                *final_text = text;
                *total += cost_usd;
            }
            TurnOutcome::Aborted { reason, cost_usd } => {
                *total += cost_usd;
                return Err(reason);
            }
        }
        let (dl, dt) = self.gather_diff(surface, untracked_before).await;
        self.emit(AgentEvent::Stage {
            name: StageKind::Verify,
        });
        Ok((dl, dt))
    }

    // ------------------------------------------------------------------
    // Stage: judge
    // ------------------------------------------------------------------

    /// One distress-guidance call ([`guidance_prompt`]): best-effort and
    /// never a verdict — the failure it reacts to is already deterministic,
    /// so the judge's job here is *steering*, not re-judging. A failed call
    /// (or an unresolvable judge) degrades to evidence-only revision.
    async fn judge_guidance(
        &self,
        goal: &str,
        diff: &str,
        evidence_summary: &str,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> Result<Option<String>, PipelineBudgetAbort> {
        let resolved = match self.resolve_provider(Role::Judge) {
            Ok(resolved) => resolved,
            Err(_) => return Ok(None),
        };
        if let Some(fb) = &resolved.fallback {
            self.emit_fallback(fb);
        }
        self.emit(AgentEvent::Stage {
            name: StageKind::Judge,
        });
        let prompt = guidance_prompt(goal, diff, evidence_summary);
        match self
            .metered_raw_call(
                RawCall {
                    role: ModelCallRole::DistressGuidance,
                    resolved: &resolved,
                    messages: vec![CompletionMessage::user(prompt)],
                    policy: RetryPolicy::deterministic(),
                    overrides: &self.config.role_overrides.judge,
                    timeout: None,
                },
                budget,
                total,
            )
            .await
        {
            Ok(result) => {
                let text = result.text.trim().to_string();
                if text.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(text))
                }
            }
            Err(RawCallError::Budget(abort)) => Err(abort),
            Err(RawCallError::Provider | RawCallError::Timeout) => Ok(None),
        }
    }

    async fn judge(
        &self,
        goal: &str,
        diff: &str,
        evidence_summary: &str,
        inputs: &LadderInputs,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> Result<ModelJudgeVerdict, PipelineBudgetAbort> {
        self.emit(AgentEvent::Stage {
            name: StageKind::Judge,
        });
        let resolved = match self.resolve_provider(Role::Judge) {
            Ok(r) => r,
            // Judge unresolvable → conservative heuristic verdict (L-E11).
            Err(_) => return Ok(heuristic_fallback(inputs)),
        };
        if let Some(fb) = &resolved.fallback {
            self.emit_fallback(fb);
        }

        let prompt = judge_prompt(goal, diff, evidence_summary);
        // Deterministic policy: a judge call that fails must not hang; it falls
        // back to the heuristic verdict rather than retrying.
        match self
            .metered_raw_call(
                RawCall {
                    role: ModelCallRole::Judge,
                    resolved: &resolved,
                    messages: vec![CompletionMessage::user(prompt)],
                    policy: RetryPolicy::deterministic(),
                    overrides: &self.config.role_overrides.judge,
                    timeout: None,
                },
                budget,
                total,
            )
            .await
        {
            Ok(result) => {
                let verdict = parse_judge_response(&result.text)
                    .unwrap_or_else(|| heuristic_fallback(inputs));
                Ok(verdict)
            }
            Err(RawCallError::Budget(abort)) => Err(abort),
            Err(RawCallError::Provider | RawCallError::Timeout) => Ok(heuristic_fallback(inputs)),
        }
    }

    // ------------------------------------------------------------------
    // Shared helpers
    // ------------------------------------------------------------------

    /// Resolve a role to a concrete provider via the router + provider
    /// resolver. Reads `self.providers` as a copy of its `&'a` reference so the
    /// returned provider borrow carries the full `'a` lifetime (long enough to
    /// build a per-candidate `Engine`).
    fn resolve_provider(&self, role: Role) -> Result<ResolvedRole<'a>, RoleResolveError> {
        let decision = self
            .router
            .resolve(role)
            .map_err(RoleResolveError::Router)?;
        let providers: &'a dyn ProviderResolver = self.providers;
        let provider = providers
            .provider_for(&decision.model_ref)
            .ok_or_else(|| RoleResolveError::NoProvider(decision.model_ref.clone()))?;
        Ok(ResolvedRole {
            model_ref: decision.model_ref,
            provider,
            fallback: decision.fallback,
        })
    }

    /// Run one engine turn, forwarding every event to the consumer **live**
    /// (a concurrent drain task, not a post-hoc flush — an execute turn can
    /// run tool loops for minutes, and buffering froze the renderer for the
    /// whole turn) **except** the engine's `Stage`/`Complete` (the pipeline
    /// owns those), tallying `FileChange`s into `file_changes` for the
    /// zero-diff guard.
    async fn run_engine_turn(
        &self,
        engine: &Engine<'_>,
        messages: &mut Vec<CompletionMessage>,
        budget: &mut BudgetGuard,
        file_changes: &mut u32,
    ) -> TurnOutcome {
        // The filtered sender is SYNCHRONOUS on purpose: when the outer
        // sender carries a durability boundary, a paid StepUsage cannot
        // return to the engine before append+flush completes. Draining a
        // channel from a spawned forwarder instead would let the engine make
        // another paid call before the previous one's metering row is durable.
        let seen_file_changes = Arc::new(AtomicU32::new(0));
        let count = seen_file_changes.clone();
        let consumer = self.events.clone();
        let filtered = EventSender::from_fn(move |event| {
            match &event {
                // The pipeline is the sole authority for stage boundaries and
                // the terminal event of an outcome-producing run — drop the
                // engine's per-turn copies.
                AgentEvent::Stage { .. } | AgentEvent::Complete { .. } => Ok(()),
                AgentEvent::FileChange { kind, .. } => {
                    // Reads ride the same event for the files panel but are
                    // not changes — counting them would defeat the zero-diff
                    // guard on read-only turns.
                    if kind.is_mutation() {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    consumer.send(event)
                }
                _ => consumer.send(event),
            }
        });
        let outcome = engine
            .run_turn_with_sender(messages, budget, &filtered)
            .await;
        *file_changes += seen_file_changes.load(Ordering::Relaxed);
        outcome
    }

    /// The real added-line count of an untracked file, via a no-index diff
    /// numstat (`<added>\t<deleted>\t<path>`). A binary file numstats as `-`
    /// and counts as one changed line (a change the ladder must see, but
    /// unmeasurable in lines). Counting real lines — not a flat 1 per file —
    /// is what keeps a large untracked file from slipping under the diff budget
    /// and taking `SubmitFast`. A single file's numstat is one line, so this is
    /// safe against the diagnostic runner's output truncation.
    async fn untracked_added_lines(&self, surface: CandidateSurface<'_>, path: &str) -> u32 {
        let out = surface
            .diagnostics
            .run_diagnostic(&DiagnosticInvocation::UntrackedNumstat {
                path: path.to_string(),
            })
            .await;
        out.stdout_tail
            .lines()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| l.split('\t').next())
            .and_then(|added| added.trim().parse::<u32>().ok())
            .unwrap_or(1)
    }

    /// Run the diff command and return `(changed_line_count, raw_diff)`.
    ///
    /// `git diff` cannot see untracked files, so a turn whose entire change is
    /// a NEW (or edited-untracked) file would read as "no diff" — skipping
    /// verification via the zero-diff guard and showing the judge an empty
    /// diff. When the configured command is git's, untracked files that were
    /// **created or modified this turn** — present now with either no
    /// `untracked_before` entry or a changed fingerprint — are appended with
    /// their real added-line counts. Untouched dirty files (same fingerprint)
    /// are excluded, so pre-existing state is never attributed to the turn, and
    /// a large untracked file cannot slip under the diff-size budget.
    async fn gather_diff(
        &self,
        surface: CandidateSurface<'_>,
        untracked_before: &HashMap<String, String>,
    ) -> (u32, String) {
        let Some(diagnostic) = &self.config.diff_diagnostic else {
            return (0, String::new());
        };
        let out = surface.diagnostics.run_diagnostic(diagnostic).await;
        let mut lines = count_diff_lines(&out.stdout_tail);
        let mut text = out.stdout_tail;
        if matches!(diagnostic, DiagnosticInvocation::GitDiff) {
            let after = surface.repo_status.untracked_fingerprints().await;
            // Created (absent before) OR modified (fingerprint changed) this
            // turn — never an untouched dirty file.
            let mut fresh: Vec<&str> = after
                .iter()
                .filter(|(path, fp)| untracked_before.get(*path) != Some(*fp))
                .map(|(path, _)| path.as_str())
                .collect();
            fresh.sort(); // deterministic order for the appended evidence
            for path in fresh {
                let added = self.untracked_added_lines(surface, path).await;
                lines += added;
                text.push_str(&format!("\n+ untracked change: {path} (+{added} lines)"));
            }
        }
        (lines, text)
    }

    fn emit_context_recall(&self, frames: &[RecalledFrame]) {
        if frames.is_empty() {
            return;
        }
        let tokens: u32 = frames.iter().map(|f| f.token_cost).sum();
        let mut mix: Vec<ProviderShare> = Vec::new();
        for f in frames {
            if let Some(share) = mix.iter_mut().find(|s| s.provider == f.provider) {
                share.frames += 1;
            } else {
                mix.push(ProviderShare {
                    provider: f.provider.clone(),
                    frames: 1,
                });
            }
        }
        let frame_refs = frames
            .iter()
            .map(|f| ContextFrameRef {
                id: f.id.clone(),
                citation_label: f.citation_label.clone(),
                provider: f.provider.clone(),
                source: f.source.clone(),
                kind: f.kind.clone(),
                uri: f.uri.clone(),
                method: f.method.clone(),
                token_cost: f.token_cost,
            })
            .collect();
        self.emit(AgentEvent::ContextRecall {
            frames: frame_refs,
            provider_mix: mix,
            tokens,
        });
    }

    /// Whether a witness author independent of the worker can be resolved.
    ///
    /// Losing the author costs the run its authored witness, never the task:
    /// a `false` here routes to the ordinary single-shot path and the
    /// deterministic/judge verify ladder. Announced once, at the one point
    /// that decides it, so the run never pays for isolation it cannot use.
    fn can_author_independent_witness(&self) -> bool {
        let Ok(worker) = self.resolve_provider(Role::Worker) else {
            // A worker that won't resolve fails later, on its own terms —
            // not here, disguised as a witness-independence verdict.
            return false;
        };
        match self.resolve_provider(Role::Judge) {
            Ok(judge) if judge.model_ref != worker.model_ref => true,
            Ok(_) => {
                self.warn(format!(
                    "no witness author independent of the worker (judge and worker both \
                     resolved to `{}`); continuing without an authored witness",
                    worker.model_ref
                ));
                false
            }
            Err(_) => {
                self.warn(
                    "no witness author independent of the worker (the judge role is \
                     unresolvable); continuing without an authored witness"
                        .to_string(),
                );
                false
            }
        }
    }

    fn emit_fallback(&self, fb: &FallbackInfo) {
        self.emit(AgentEvent::ProviderFallback {
            from: fb.from.clone(),
            to: fb.to.clone(),
            reason: fb.reason.clone(),
        });
    }

    /// A non-fatal degradation the user should see (witness discarded,
    /// guidance unavailable): a `retryable: true` error event — the deck
    /// folds it as a warning, never a failed turn.
    fn warn(&self, message: String) {
        self.emit(AgentEvent::Error {
            message,
            retryable: true,
        });
    }

    fn aborted_before_execute(
        &self,
        task_class: TaskClass,
        total_cost: f64,
        reason: &str,
    ) -> PipelineOutcome {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = stage_budget::aborted_before_execute(&tx, task_class, total_cost, reason);
        drop(tx);
        while let Ok(event) = rx.try_recv() {
            self.emit(event);
        }
        outcome
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }
}

/// The winning candidate's index: strongest verification evidence first,
/// smallest diff as the tiebreak, earliest index for reproducibility
/// ([`select_best_candidate`]).
fn best_index(candidates: &[CandidateResult]) -> usize {
    let summaries: Vec<CandidateSummary> = candidates
        .iter()
        .map(|c| CandidateSummary {
            score: c.score,
            diff_lines: c.diff_lines,
        })
        .collect();
    select_best_candidate(&summaries).unwrap_or(0)
}

/// The flip oracle's command for this run: an explicit `--test-command`
/// always wins; otherwise the witness author's (when one was validated).
#[derive(Clone, Copy)]
struct EffectiveTestCommand<'c> {
    command: &'c str,
    invocation: &'c TestInvocation,
}

/// Assemble the volatile recall+goal user message that rides *after* the
/// stable system prefix (L-E8 cache discipline). Keeping the system prefix
/// untouched preserves prompt-cache hits on it across turns; the recall and
/// goal — both volatile per turn — go here in one message.
fn assemble_user_message(goal: &str, frames: &[RecalledFrame]) -> String {
    if frames.is_empty() {
        return goal.to_string();
    }
    let mut s = String::from("## Recalled context\n");
    for f in frames {
        // Cite by human label (L-C4); include content as grounding.
        s.push_str("- [");
        s.push_str(&f.citation_label);
        s.push_str("] (");
        s.push_str(&f.source);
        s.push_str(")\n");
        if !f.content.trim().is_empty() {
            s.push_str("  ");
            s.push_str(f.content.trim());
            s.push('\n');
        }
    }
    s.push_str("\n## Task\n");
    s.push_str(goal.trim());
    s
}

/// The instruction appended to a revision turn, carrying the failing
/// verification evidence so the worker can fix it.
fn revision_prompt(reason: &str) -> String {
    format!(
        "Verification did not pass. Evidence:\n{}\n\nFix the issue and complete the task.",
        reason.trim()
    )
}

/// Count changed lines in a unified diff: lines beginning with `+`/`-` but not
/// the `+++`/`---` file headers. A coarse but deterministic size proxy for the
/// ladder's diff budget.
fn count_diff_lines(diff: &str) -> u32 {
    diff.lines()
        .filter(|l| {
            (l.starts_with('+') && !l.starts_with("+++"))
                || (l.starts_with('-') && !l.starts_with("---"))
        })
        .count() as u32
}

#[cfg(test)]
mod tests;
