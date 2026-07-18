//! The orchestrator: the staged turn flow that sits
//! *above* `stella-core::Engine`. It sequences evaluate → enhance → route →
//! witness → execute → verify → judge → revise over the injected ports,
//! emitting a `Stage` event at every boundary and exactly one terminal
//! `Complete`.
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
//! the **single authority** for stage boundaries and the one terminal
//! `Complete`: it gives each `run_turn` a private channel, then forwards every
//! event to the consumer *except* the engine's `Stage`/`Complete` (which would
//! otherwise falsely signal "done" after step one). This mirrors the
//! one-emission-point discipline of L-E1/L-T5.
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

use stella_core::hooks::{HookRunner, Hooks};
use stella_core::retry::{RetryPolicy, Sleeper, retry_with_backoff};
use stella_core::router::{FallbackInfo, RouterError};
use stella_core::{BudgetGuard, BudgetOutcome, Engine, EngineConfig, Router, TurnOutcome};
use stella_protocol::{
    AgentEvent, CompletionMessage, CompletionRequest, CompletionResult, ContextFrameRef,
    JudgeEvidence, ModelRef, Provider, ProviderShare, Role, StageKind,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::timeout;

use crate::candidate::{
    CandidateScore, CandidateSummary, score_from_verification, select_best_candidate,
};
use crate::plan::{PlanStep, build_planner_prompt, parse_plan, plan_repair_prompt};
use crate::ports::{
    ApprovalGate, CommandRunner, ContextRecallPort, ProviderResolver, RecalledFrame,
    RepoStatusPort, RepoStructurePort, ScopeDecision,
};
use crate::scope::{ScopeEstimate, apply_trim, build_proposal, needs_scope_review};
use crate::triage::{TaskClass, classify_triage_response, resolve_task_class, triage_prompt};
use crate::verify::{
    FlipOracle, JudgeVerdict as ModelJudgeVerdict, LadderDecision, LadderInputs,
    deterministic_fail_evidence, deterministic_pass_evidence, guidance_prompt, heuristic_fallback,
    judge_prompt, ladder_decision, model_verdict_evidence, parse_judge_response,
};
use crate::witness::{
    Witness, parse_witness_command, tampered_paths, witness_prompt, witness_repair_prompt,
    witness_watchlist,
};

/// A minimal default system prompt, used only when the caller hands `run` an
/// empty message vector. Real callers seed their own stable system prefix
/// (which becomes the cached prompt prefix, L-E8).
const DEFAULT_SYSTEM_PROMPT: &str =
    "You are a precise, careful software engineering agent. Make the smallest correct change.";

/// The witness author's system prompt — small and fixed. The task prompt
/// (goal + recall + repo structure, split context like the planner's) is
/// [`witness_prompt`].
const WITNESS_SYSTEM_PROMPT: &str = "You are a precise test author. You write minimal failing tests that pin down intended \
     behavior. You never modify production code and never fix the problem yourself.";

/// The ports the pipeline orchestrates over. The `stella-cli` glue fills this
/// with real subsystem adapters; tests fill it with scripted doubles. Grouped
/// into one struct so [`Pipeline::new`] stays a two-argument constructor
/// rather than a nine-parameter one.
pub struct PipelinePorts<'a> {
    /// Role → model resolution (`stella-core`). Held immutably; see the
    /// module's "breaker feedback boundary" note.
    pub router: &'a Router,
    /// Maps a resolved [`ModelRef`] to its concrete provider adapter.
    pub providers: &'a dyn ProviderResolver,
    /// The tool registry the execute engine drives.
    pub tools: &'a dyn stella_core::ToolExecutor,
    /// Context recall at turn start (L-E8).
    pub recall: &'a dyn ContextRecallPort,
    /// Repo-structure summary for the planner's split context (L-E6).
    pub repo: &'a dyn RepoStructurePort,
    /// Untracked-file snapshots for the zero-diff guard (`git diff` can't see
    /// untracked files; this makes new/modified untracked files visible).
    pub repo_status: &'a dyn RepoStatusPort,
    /// Runs the verification ladder's test and diff commands (L-E11).
    pub commands: &'a dyn CommandRunner,
    /// The interactive scope-review gate (L-E5).
    pub approvals: &'a dyn ApprovalGate,
    /// The delay port for retry backoff — the same testability seam
    /// `stella-core` uses; production passes the CLI's tokio-backed
    /// sleeper, tests a no-op.
    pub sleeper: &'a dyn Sleeper,
    /// Lifecycle hooks for the execute engine — the parsed config plus the
    /// runner that spawns hook commands (`stella_core::hooks`). `None` runs
    /// the exact pre-hooks pipeline; the CLI passes its settings-chain hooks
    /// so `PreToolUse` gating also covers the default `stella run` path.
    pub hooks: Option<(&'a Hooks, &'a dyn HookRunner)>,
}

/// Tuning for the whole staged flow.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Passed to `stella-core::Engine` for every execute turn.
    pub engine: EngineConfig,
    /// Hard latency ceiling on the triage classification call (L-M4): if it
    /// doesn't answer within this, triage falls through to the full path.
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
    /// The command that reports what the turn changed (default `git diff`),
    /// used for the diff-size budget and the zero-diff guard. `None` skips it.
    pub diff_command: Option<String>,
    /// The diff-size budget in changed lines: a diff at or under this is
    /// "small enough" to trust deterministic evidence without a judge (L-E11).
    pub diff_budget_lines: u32,
    /// Maximum revision turns per candidate when verification fails.
    pub max_revisions: u32,
    /// Best-of-N (L-E7). `None` or `Some(1)` is single-shot (the default);
    /// `Some(n)` generates n candidate executions and selects the best. Paid
    /// for with n× the execution cost — opt-in only.
    pub candidates: Option<u32>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            triage_latency_ceiling: Duration::from_secs(10),
            scope_thresholds: crate::scope::ScopeThresholds::default(),
            headless: false,
            headless_bypass_scope_review: false,
            test_command: None,
            witness_writer: true,
            distress_guidance: true,
            diff_command: Some("git diff".to_string()),
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
    /// The staged flow ran to completion. Inspect [`PipelineOutcome::verdict`]
    /// for whether the work verified — a `Completed` run with a failed verdict
    /// means the revise budget was exhausted without passing.
    Completed,
    /// The run ended early: a step aborted (budget/loop/step-cap), or the user
    /// aborted at scope review.
    Aborted { reason: String },
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

/// A hard, named failure of a pipeline run (as opposed to a clean
/// [`PipelineStatus::Aborted`], which is a normal outcome).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PipelineError {
    /// A plan crossed the scope-review thresholds while running headless with
    /// no approval bypass configured (L-E5): never silently auto-approve.
    #[error(
        "scope review is required for this plan, but the run is headless without an approval bypass — re-run interactively or enable the scope-review bypass"
    )]
    ScopeReviewRequiredHeadless,
    /// The router resolved a role to a model no configured adapter serves
    /// (L-M1): a loud error, never a silent fallback.
    #[error(
        "no provider adapter is configured for the resolved model `{0}` — configure the provider or refresh the catalog"
    )]
    NoProviderForModel(String),
    /// A required role (worker) could not be resolved at all.
    #[error(transparent)]
    Routing(#[from] RouterError),
}

/// A role resolved to a concrete provider.
struct ResolvedRole<'a> {
    model_ref: ModelRef,
    provider: &'a dyn Provider,
    fallback: Option<FallbackInfo>,
}

/// Internal error resolving a role to a usable provider.
enum RoleResolveError {
    Router(RouterError),
    NoProvider(ModelRef),
}

impl RoleResolveError {
    fn into_pipeline_error(self) -> PipelineError {
        match self {
            RoleResolveError::Router(e) => PipelineError::Routing(e),
            RoleResolveError::NoProvider(m) => PipelineError::NoProviderForModel(m.to_string()),
        }
    }
}

/// The outcome of running one candidate (execute + verify + bounded revise).
struct CandidateResult {
    messages: Vec<CompletionMessage>,
    final_text: String,
    /// `Some(reason)` if a turn aborted (budget/loop/step-cap).
    aborted: Option<String>,
    /// The verification verdict, if verification ran.
    verdict: Option<Verdict>,
    /// This candidate's verification score, for best-of-N selection.
    score: CandidateScore,
    diff_lines: u32,
    revisions: u32,
}

impl CandidateResult {
    fn aborted(messages: Vec<CompletionMessage>, reason: String) -> Self {
        Self {
            messages,
            final_text: String::new(),
            aborted: Some(reason),
            verdict: None,
            score: CandidateScore::Failed,
            diff_lines: 0,
            revisions: 0,
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
            verdict: None,
            score: CandidateScore::Unverified,
            diff_lines: self.diff_lines,
            revisions: self.revisions,
        }
    }
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
    commands: &'a dyn CommandRunner,
    approvals: &'a dyn ApprovalGate,
    sleeper: &'a dyn Sleeper,
    hooks: Option<(&'a Hooks, &'a dyn HookRunner)>,
    events: UnboundedSender<AgentEvent>,
    config: PipelineConfig,
}

impl<'a> Pipeline<'a> {
    /// Construct a pipeline over the given ports, event sink, and config.
    pub fn new(
        ports: PipelinePorts<'a>,
        events: UnboundedSender<AgentEvent>,
        config: PipelineConfig,
    ) -> Self {
        Self {
            router: ports.router,
            providers: ports.providers,
            tools: ports.tools,
            recall: ports.recall,
            repo: ports.repo,
            repo_status: ports.repo_status,
            commands: ports.commands,
            approvals: ports.approvals,
            sleeper: ports.sleeper,
            hooks: ports.hooks,
            events,
            config,
        }
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
    ) -> Result<PipelineOutcome, PipelineError> {
        let mut total_cost = 0.0f64;
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
        let (task_class, frames) =
            tokio::join!(self.triage(goal, budget, &mut total_cost), recall_future);
        self.emit_context_recall(&frames);
        // The volatile recall+goal message rides AFTER the stable system
        // prefix (L-E8) — see assemble_user_message.
        messages.push(CompletionMessage::user(assemble_user_message(
            goal, &frames,
        )));

        // --- 3. Plan (skipped for simple/single-task). ---------------------
        let plan: Option<Vec<PlanStep>> = if task_class.plans() {
            let repo_structure = self.repo.structure_summary().await;
            Some(
                self.plan_stage(goal, &frames, &repo_structure, budget, &mut total_cost)
                    .await,
            )
        } else {
            None
        };

        // --- 4. Scope review (only for planned work above thresholds). -----
        let plan = match plan {
            Some(steps) => match self.scope_review(goal, steps).await? {
                Some(steps) => Some(steps),
                None => {
                    // User aborted (or trimmed to nothing) at the gate.
                    return Ok(self.aborted_before_execute(
                        task_class,
                        total_cost,
                        "aborted at scope review",
                    ));
                }
            },
            None => None,
        };

        // --- 4.5. Witness authoring (L-E11 front half). ---------------------
        // After scope review (never author a witness for a plan the user may
        // abort), before the candidate loop (one witness is the shared
        // yardstick every candidate is measured by).
        let witness = self
            .witness_stage(goal, &frames, task_class, budget, &mut total_cost)
            .await;

        // --- 5. Execute + verify (single-shot or best-of-N). ---------------
        let worker = match self.resolve_provider(Role::Worker) {
            Ok(w) => w,
            Err(e) => return Err(e.into_pipeline_error()),
        };
        if let Some(fb) = &worker.fallback {
            self.emit_fallback(fb);
        }
        let worker_model_label = worker.model_ref.to_string();
        let mut engine = Engine::with_sleeper(
            worker.provider,
            self.tools,
            self.config.engine.clone(),
            self.sleeper,
        );
        if let Some((hooks, runner)) = self.hooks {
            engine = engine.with_hooks(hooks, runner);
        }

        let n = self.config.candidate_count();
        let base_messages = messages.clone();
        let mut candidates: Vec<CandidateResult> = Vec::with_capacity(n as usize);
        for _ in 0..n {
            candidates.push(
                self.run_candidate(
                    goal,
                    &base_messages,
                    plan.as_deref(),
                    task_class,
                    witness.as_ref(),
                    &engine,
                    budget,
                    &mut total_cost,
                )
                .await,
            );
        }

        let summaries: Vec<CandidateSummary> = candidates
            .iter()
            .map(|c| CandidateSummary {
                score: c.score,
                diff_lines: c.diff_lines,
            })
            .collect();
        let best_idx = select_best_candidate(&summaries).unwrap_or(0);
        let best = candidates
            .into_iter()
            .nth(best_idx)
            .expect("select_best_candidate returns an in-range index");

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
        self.emit(AgentEvent::Complete {
            model: worker_model_label,
            cost_usd: total_cost,
        });
        Ok(PipelineOutcome {
            status: PipelineStatus::Completed,
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

    async fn triage(&self, goal: &str, budget: &mut BudgetGuard, total: &mut f64) -> TaskClass {
        self.emit(AgentEvent::Stage {
            name: StageKind::Triage,
        });
        let resolved = match self.resolve_provider(Role::Triage) {
            Ok(r) => r,
            // Triage resolution failure is soft: fall through to the full path
            // via the deterministic floor. Never fail the run on triage.
            Err(_) => return resolve_task_class(None, goal),
        };
        if let Some(fb) = &resolved.fallback {
            self.emit_fallback(fb);
        }

        let messages = vec![CompletionMessage::user(triage_prompt(goal))];
        // Deterministic policy (L-M4: max_retries = 0) under a hard latency
        // ceiling: on provider error OR timeout, fall through — never hang,
        // never retry.
        let call = self.complete_once(resolved.provider, messages, RetryPolicy::deterministic());
        let model_class = match timeout(self.config.triage_latency_ceiling, call).await {
            Ok(Ok(result)) => {
                *total += result.cost_usd;
                self.record_and_tick(budget, result.cost_usd);
                classify_triage_response(&result.text)
            }
            Ok(Err(_)) | Err(_) => None,
        };
        resolve_task_class(model_class, goal)
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
    ) -> Vec<PlanStep> {
        self.emit(AgentEvent::Stage {
            name: StageKind::Plan,
        });
        let fallback_plan = || vec![PlanStep::new(goal)];

        let resolved = match self.resolve_provider(Role::Plan) {
            Ok(r) => r,
            Err(_) => return fallback_plan(),
        };
        if let Some(fb) = &resolved.fallback {
            self.emit_fallback(fb);
        }

        let prompt = build_planner_prompt(goal, recall, repo_structure);
        let result = match self
            .complete_once(
                resolved.provider,
                vec![CompletionMessage::user(prompt)],
                RetryPolicy::standard(),
            )
            .await
        {
            Ok(r) => r,
            Err(_) => return fallback_plan(),
        };
        *total += result.cost_usd;
        self.record_and_tick(budget, result.cost_usd);

        if let Some(steps) = parse_plan(&result.text) {
            return steps;
        }

        // One bounded JSON-repair retry (L-V2), deterministic (no retry-hang).
        if let Ok(repair) = self
            .complete_once(
                resolved.provider,
                vec![CompletionMessage::user(plan_repair_prompt(&result.text))],
                RetryPolicy::deterministic(),
            )
            .await
        {
            *total += repair.cost_usd;
            self.record_and_tick(budget, repair.cost_usd);
            if let Some(steps) = parse_plan(&repair.text) {
                return steps;
            }
        }

        // Degrade to a single-step plan rather than failing — a planner that
        // won't produce a parseable plan must still let the work proceed.
        fallback_plan()
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

        if self.config.headless && !self.config.headless_bypass_scope_review {
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
    // Stages: execute + verify + revise (one candidate)
    // ------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn run_candidate(
        &self,
        goal: &str,
        base_messages: &[CompletionMessage],
        plan: Option<&[PlanStep]>,
        task_class: TaskClass,
        witness: Option<&Witness>,
        engine: &Engine<'_>,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> CandidateResult {
        // Flip oracle: for classes we always verify, take a pre-execute
        // baseline of the test command so a later pass counts as a genuine
        // fail→pass flip (L-E11). Simple lookups skip the baseline — they are
        // only verified at all if the zero-diff guard trips, and then the
        // absence of a baseline correctly leaves the oracle unflipped.
        // The baseline is re-run per candidate even though the witness stage
        // already saw its command fail once: a later candidate runs over the
        // tree an earlier one modified, and a seeded observation would be a
        // fabricated one.
        let mut oracle = FlipOracle::new();
        if task_class.verifies_unconditionally()
            && let Some(cmd) = effective_test_command(&self.config, witness)
        {
            let pre = self.commands.run(cmd).await;
            oracle.observe(cmd, pre.passed());
        }

        // Snapshot untracked files (with content fingerprints) BEFORE
        // executing so `gather_diff` can tell files this turn created OR
        // modified from pre-existing dirty state — a stale untracked file with
        // an unchanged fingerprint is not this turn's work, but one the turn
        // edited (fingerprint changed) is.
        let untracked_before = self.repo_status.untracked_fingerprints().await;

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
        (state.diff_lines, state.diff_text) = self.gather_diff(&state.untracked_before).await;
        let files_touched = state.file_changes > 0 || !state.diff_text.trim().is_empty();
        let should_verify = task_class.verifies_unconditionally()
            || (task_class == TaskClass::SimpleLookup && files_touched);
        if !should_verify {
            // A clean lookup: nothing to verify.
            return state.into_unverified();
        }

        self.verify_candidate(goal, witness, engine, budget, total, state)
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
                TurnOutcome::Aborted { reason } => return Err(reason),
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
                    TurnOutcome::Aborted { reason } => return Err(reason),
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
    async fn verify_candidate(
        &self,
        goal: &str,
        witness: Option<&Witness>,
        engine: &Engine<'_>,
        budget: &mut BudgetGuard,
        total: &mut f64,
        mut state: CandidateState,
    ) -> CandidateResult {
        self.emit(AgentEvent::Stage {
            name: StageKind::Verify,
        });
        let effective_cmd = effective_test_command(&self.config, witness);
        loop {
            let (touched_tests_passed, test_tail) = self
                .observe_touched_tests(effective_cmd, &mut state.oracle)
                .await;
            // Tamper exclusion: a flip is credited only while the witness
            // files are byte-identical to what the witness author wrote. A
            // tampered witness degrades the flip to inconclusive — the judge
            // then decides, told exactly which paths were touched — it never
            // silently passes and never hard-fails work that may still be
            // correct.
            let tampered = match witness {
                Some(w) if !w.files.is_empty() => {
                    let current = self.repo_status.untracked_fingerprints().await;
                    tampered_paths(&w.files, &current)
                }
                _ => Vec::new(),
            };
            let inputs = LadderInputs {
                flip_achieved: state.oracle.is_flipped() && tampered.is_empty(),
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
                    let mut evidence = deterministic_fail_evidence(&test_tail);
                    if !tampered.is_empty() {
                        evidence.summary.push_str(&format!(
                            "; witness test file(s) were modified after authoring — restore \
                             them, the flip is not credited while they differ: {}",
                            tampered.join(", ")
                        ));
                    }
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
                    if self.config.distress_guidance
                        && state.revisions >= 1
                        && let Some(guidance) = self
                            .judge_guidance(
                                goal,
                                &state.diff_text,
                                &evidence.summary,
                                budget,
                                total,
                            )
                            .await
                    {
                        reason.push_str("\n\nIndependent reviewer course-correction:\n");
                        reason.push_str(&guidance);
                    }
                    if let Err(reason) = self
                        .revise_candidate(engine, budget, &reason, total, &mut state)
                        .await
                    {
                        return CandidateResult::aborted(state.messages, reason);
                    }
                }
                LadderDecision::ModelJudge => {
                    // Inconclusive — escalate to the model judge (judge ≠
                    // worker; a judge-call failure falls back to a heuristic).
                    let mut evidence_summary = format!(
                        "flip_achieved={}; touched_tests={:?}; diff_lines={} (budget {})",
                        inputs.flip_achieved,
                        inputs.touched_tests_passed,
                        state.diff_lines,
                        self.config.diff_budget_lines,
                    );
                    if !tampered.is_empty() {
                        evidence_summary.push_str(&format!(
                            "; witness test file(s) modified by the worker after authoring — \
                             flip evidence EXCLUDED: {}",
                            tampered.join(", ")
                        ));
                    }
                    let verdict = self
                        .judge(
                            goal,
                            &state.diff_text,
                            &evidence_summary,
                            &inputs,
                            budget,
                            total,
                        )
                        .await;
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
                        .revise_candidate(engine, budget, &verdict.reasoning, total, &mut state)
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
        cmd: Option<&str>,
        oracle: &mut FlipOracle,
    ) -> (Option<bool>, String) {
        match cmd {
            Some(cmd) => {
                let post = self.commands.run(cmd).await;
                let passed = post.passed();
                oracle.observe(cmd, passed);
                (Some(passed), post.stderr_tail)
            }
            None => (None, String::new()),
        }
    }

    /// Spend one revision: run [`Pipeline::revise_turn`] with the failure
    /// evidence and fold the fresh diff back into `state`. `Err` is the abort
    /// reason of a turn that died mid-revision (budget/loop).
    async fn revise_candidate(
        &self,
        engine: &Engine<'_>,
        budget: &mut BudgetGuard,
        reason: &str,
        total: &mut f64,
        state: &mut CandidateState,
    ) -> Result<(), String> {
        let (diff_lines, diff_text) = self
            .revise_turn(
                engine,
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
            TurnOutcome::Aborted { reason } => return Err(reason),
        }
        let (dl, dt) = self.gather_diff(untracked_before).await;
        self.emit(AgentEvent::Stage {
            name: StageKind::Verify,
        });
        Ok((dl, dt))
    }

    // ------------------------------------------------------------------
    // Stage: witness authoring (L-E11 front half)
    // ------------------------------------------------------------------

    /// Author the witness test when nothing else arms the flip oracle: one
    /// engine turn (with tools — the author writes a real test file) whose
    /// reply names a `TEST_COMMAND:`, checked to actually FAIL on the current
    /// code (a passing "witness" witnesses nothing; one bounded repair retry,
    /// then discard). Returns `None` — degrading verification to the model
    /// judge, exactly as if no test command existed — whenever any part
    /// doesn't hold; the witness stage can slow a run down but never fail it.
    ///
    /// Witness ≠ worker: the author rides the judge's resolution (the test
    /// that defines "done" comes from the same independent role that enforces
    /// it), falling back to the worker's model only when no judge resolves.
    async fn witness_stage(
        &self,
        goal: &str,
        frames: &[RecalledFrame],
        task_class: TaskClass,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> Option<Witness> {
        if self.config.test_command.is_some()
            || !self.config.witness_writer
            || !task_class.verifies_unconditionally()
        {
            return None;
        }
        self.emit(AgentEvent::Stage {
            name: StageKind::Witness,
        });
        let resolved = match self
            .resolve_provider(Role::Judge)
            .or_else(|_| self.resolve_provider(Role::Worker))
        {
            Ok(r) => r,
            Err(_) => return None,
        };
        if let Some(fb) = &resolved.fallback {
            self.emit_fallback(fb);
        }

        // Snapshot BEFORE the author runs: the fingerprint delta afterwards
        // is the tamper watchlist (observed, never the author's own claims).
        let before = self.repo_status.untracked_fingerprints().await;
        let structure = self.repo.structure_summary().await;
        let mut engine = Engine::with_sleeper(
            resolved.provider,
            self.tools,
            self.config.engine.clone(),
            self.sleeper,
        );
        if let Some((hooks, runner)) = self.hooks {
            engine = engine.with_hooks(hooks, runner);
        }

        let mut messages = vec![
            CompletionMessage::system(WITNESS_SYSTEM_PROMPT),
            CompletionMessage::user(witness_prompt(goal, frames, &structure)),
        ];
        let mut file_changes = 0u32;
        let text = match self
            .run_engine_turn(&engine, &mut messages, budget, &mut file_changes)
            .await
        {
            TurnOutcome::Completed { text, cost_usd } => {
                *total += cost_usd;
                text
            }
            TurnOutcome::Aborted { reason } => {
                self.warn(format!(
                    "witness author turn aborted ({reason}); verification falls back to the \
                     model judge"
                ));
                return None;
            }
        };
        let Some(mut command) = parse_witness_command(&text) else {
            self.warn(
                "witness author produced no TEST_COMMAND line; verification falls back to \
                 the model judge"
                    .to_string(),
            );
            return None;
        };

        // The witness must fail on the unmodified code — only a fail→pass
        // flip of it will count (L-E11). One bounded repair retry (the L-V2
        // pattern), then discard; never loop.
        if self.commands.run(&command).await.passed() {
            messages.push(CompletionMessage::user(witness_repair_prompt(&command)));
            let repaired = match self
                .run_engine_turn(&engine, &mut messages, budget, &mut file_changes)
                .await
            {
                TurnOutcome::Completed { text, cost_usd } => {
                    *total += cost_usd;
                    text
                }
                TurnOutcome::Aborted { reason } => {
                    self.warn(format!(
                        "witness repair turn aborted ({reason}); verification falls back to \
                         the model judge"
                    ));
                    return None;
                }
            };
            command = parse_witness_command(&repaired).unwrap_or(command);
            if self.commands.run(&command).await.passed() {
                self.warn(
                    "witness test still passes on the unmodified code after one repair — \
                     discarded; verification falls back to the model judge"
                        .to_string(),
                );
                return None;
            }
        }

        let after = self.repo_status.untracked_fingerprints().await;
        Some(Witness {
            command,
            files: witness_watchlist(&before, &after),
        })
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
    ) -> Option<String> {
        let resolved = self.resolve_provider(Role::Judge).ok()?;
        if let Some(fb) = &resolved.fallback {
            self.emit_fallback(fb);
        }
        self.emit(AgentEvent::Stage {
            name: StageKind::Judge,
        });
        let prompt = guidance_prompt(goal, diff, evidence_summary);
        match self
            .complete_once(
                resolved.provider,
                vec![CompletionMessage::user(prompt)],
                RetryPolicy::deterministic(),
            )
            .await
        {
            Ok(result) => {
                *total += result.cost_usd;
                self.record_and_tick(budget, result.cost_usd);
                let text = result.text.trim().to_string();
                if text.is_empty() { None } else { Some(text) }
            }
            Err(_) => None,
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
    ) -> ModelJudgeVerdict {
        self.emit(AgentEvent::Stage {
            name: StageKind::Judge,
        });
        let resolved = match self.resolve_provider(Role::Judge) {
            Ok(r) => r,
            // Judge unresolvable → conservative heuristic verdict (L-E11).
            Err(_) => return heuristic_fallback(inputs),
        };
        if let Some(fb) = &resolved.fallback {
            self.emit_fallback(fb);
        }

        let prompt = judge_prompt(goal, diff, evidence_summary);
        // Deterministic policy: a judge call that fails must not hang; it falls
        // back to the heuristic verdict rather than retrying.
        match self
            .complete_once(
                resolved.provider,
                vec![CompletionMessage::user(prompt)],
                RetryPolicy::deterministic(),
            )
            .await
        {
            Ok(result) => {
                *total += result.cost_usd;
                self.record_and_tick(budget, result.cost_usd);
                parse_judge_response(&result.text).unwrap_or_else(|| heuristic_fallback(inputs))
            }
            Err(_) => heuristic_fallback(inputs),
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

    /// One raw provider completion (triage/plan/judge — not the execute engine)
    /// under `policy`, emitting a `Retry` event per committed retry (L-E10).
    async fn complete_once(
        &self,
        provider: &dyn Provider,
        messages: Vec<CompletionMessage>,
        policy: RetryPolicy,
    ) -> Result<CompletionResult, stella_protocol::ProviderError> {
        let req = CompletionRequest {
            messages,
            max_output_tokens: self.config.engine.max_output_tokens,
            temperature: self.config.engine.temperature,
            effort: self.config.engine.effort,
            tools: Vec::new(),
        };
        let outcome =
            retry_with_backoff(&policy, self.sleeper, || provider.complete(req.clone())).await?;
        for attempt in &outcome.retries {
            self.emit(AgentEvent::Retry {
                attempt: attempt.attempt,
                reason: attempt.reason.clone(),
            });
        }
        Ok(outcome.value)
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let consumer = self.events.clone();
        let forwarder = tokio::spawn(async move {
            let mut seen_file_changes: u32 = 0;
            while let Some(event) = rx.recv().await {
                let forward = match &event {
                    // The pipeline is the sole authority for stage boundaries
                    // and the terminal Complete — drop the engine's per-turn
                    // copies.
                    AgentEvent::Stage { .. } | AgentEvent::Complete { .. } => false,
                    AgentEvent::FileChange { kind, .. } => {
                        // Reads ride the same event for the files panel but
                        // are not changes — counting them would defeat the
                        // zero-diff guard on read-only turns.
                        if kind.is_mutation() {
                            seen_file_changes += 1;
                        }
                        true
                    }
                    _ => true,
                };
                if forward {
                    let _ = consumer.send(event);
                }
            }
            seen_file_changes
        });
        let outcome = engine.run_turn(messages, budget, &tx).await;
        drop(tx); // close the channel so the forwarder terminates
        *file_changes += forwarder.await.unwrap_or(0);
        outcome
    }

    /// The real added-line count of an untracked file, via a no-index diff
    /// numstat (`<added>\t<deleted>\t<path>`). A binary file numstats as `-`
    /// and counts as one changed line (a change the ladder must see, but
    /// unmeasurable in lines). Counting real lines — not a flat 1 per file —
    /// is what keeps a large untracked file from slipping under the diff budget
    /// and taking `SubmitFast`. A single file's numstat is one line, so this is
    /// safe against the CommandRunner's output truncation.
    async fn untracked_added_lines(&self, path: &str) -> u32 {
        let out = self
            .commands
            .run(&format!(
                "git diff --no-index --numstat -- /dev/null {}",
                shell_single_quote(path)
            ))
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
    async fn gather_diff(&self, untracked_before: &HashMap<String, String>) -> (u32, String) {
        let Some(cmd) = &self.config.diff_command else {
            return (0, String::new());
        };
        let out = self.commands.run(cmd).await;
        let mut lines = count_diff_lines(&out.stdout_tail);
        let mut text = out.stdout_tail;
        if cmd.trim_start().starts_with("git diff") {
            let after = self.repo_status.untracked_fingerprints().await;
            // Created (absent before) OR modified (fingerprint changed) this
            // turn — never an untouched dirty file.
            let mut fresh: Vec<&str> = after
                .iter()
                .filter(|(path, fp)| untracked_before.get(*path) != Some(*fp))
                .map(|(path, _)| path.as_str())
                .collect();
            fresh.sort(); // deterministic order for the appended evidence
            for path in fresh {
                let added = self.untracked_added_lines(path).await;
                lines += added;
                text.push_str(&format!("\n+ untracked change: {path} (+{added} lines)"));
            }
        }
        (lines, text)
    }

    fn record_and_tick(&self, budget: &mut BudgetGuard, cost_usd: f64) -> BudgetOutcome {
        let outcome = budget.record_spend(cost_usd);
        self.emit(AgentEvent::BudgetTick {
            spent_usd: budget.spent_usd(),
            limit_usd: budget.turn_limit_usd(),
            mode: budget.mode(),
        });
        outcome
    }

    fn emit_context_recall(&self, frames: &[RecalledFrame]) {
        if frames.is_empty() {
            return;
        }
        let tokens: u32 = frames.iter().map(|f| f.token_cost).sum();
        let mut mix: Vec<ProviderShare> = Vec::new();
        for f in frames {
            if let Some(share) = mix.iter_mut().find(|s| s.provider == f.source) {
                share.frames += 1;
            } else {
                mix.push(ProviderShare {
                    provider: f.source.clone(),
                    frames: 1,
                });
            }
        }
        let frame_refs = frames
            .iter()
            .map(|f| ContextFrameRef {
                id: f.id.clone(),
                citation_label: f.citation_label.clone(),
                source: f.source.clone(),
                token_cost: f.token_cost,
            })
            .collect();
        self.emit(AgentEvent::ContextRecall {
            frames: frame_refs,
            provider_mix: mix,
            tokens,
        });
    }

    fn emit_fallback(&self, fb: &FallbackInfo) {
        self.emit(AgentEvent::ProviderFallback {
            from: fb.from.clone(),
            to: fb.to.clone(),
            reason: fb.reason.clone(),
        });
    }

    fn aborted_before_execute(
        &self,
        task_class: TaskClass,
        total_cost: f64,
        reason: &str,
    ) -> PipelineOutcome {
        self.emit(AgentEvent::Error {
            message: reason.to_string(),
            retryable: false,
        });
        PipelineOutcome {
            status: PipelineStatus::Aborted {
                reason: reason.to_string(),
            },
            task_class,
            final_text: String::new(),
            total_cost_usd: total_cost,
            verdict: None,
            revisions: 0,
            candidates_run: 0,
        }
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

    fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }
}

/// The flip oracle's command for this run: an explicit `--test-command`
/// always wins; otherwise the witness author's (when one was validated).
fn effective_test_command<'c>(
    config: &'c PipelineConfig,
    witness: Option<&'c Witness>,
) -> Option<&'c str> {
    config
        .test_command
        .as_deref()
        .or(witness.map(|w| w.command.as_str()))
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

/// POSIX single-quote a shell argument so an untracked file path (which the
/// user, not the pipeline, named) can never break out of the `git diff`
/// command string. Embedded single quotes become the standard `'\''`.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use async_trait::async_trait;
    use serde_json::Value;
    use stella_core::router::{CircuitBreaker, ProviderProfile, RoleTable};
    use stella_core::{Clock, ToolExecutor};
    use stella_protocol::event::BudgetMode;
    use stella_protocol::{
        CompletionRequest, CompletionUsage, ProviderError, ScopeProposal, ToolOutput, ToolSchema,
    };
    use tokio::sync::Mutex as TokioMutex;
    use tokio::sync::mpsc;

    use crate::ports::{
        AutoApproveGate, CmdOutcome, NoContextRecall, NoRepoStatus, NoRepoStructure,
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
            }
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
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResult, ProviderError> {
            let mut q = self.script.lock().await;
            q.pop_front()
                .ok_or_else(|| ProviderError::Terminal("scripted provider exhausted".into()))
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
    impl CommandRunner for ScriptedRunner {
        async fn run(&self, cmd: &str) -> CmdOutcome {
            // Order matters: the no-index numstat and ls-files probes both
            // contain "diff"/"git", so match them before the generic diff.
            if cmd.contains("ls-files") {
                let listing = self
                    .untracked
                    .iter()
                    .map(|(p, _)| p.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                return CmdOutcome {
                    exit_code: 0,
                    stdout_tail: listing,
                    stderr_tail: String::new(),
                };
            }
            if cmd.contains("--no-index") && cmd.contains("--numstat") {
                // Report `<added>\t0\t<path>` for the file named in the cmd.
                let numstat = self
                    .untracked
                    .iter()
                    .find(|(p, _)| cmd.contains(p.as_str()))
                    .map(|(p, n)| format!("{n}\t0\t{p}"))
                    .unwrap_or_default();
                return CmdOutcome {
                    exit_code: if numstat.is_empty() { 0 } else { 1 },
                    stdout_tail: numstat,
                    stderr_tail: String::new(),
                };
            }
            if cmd.contains("diff") {
                return CmdOutcome {
                    exit_code: 0,
                    stdout_tail: self.diff.clone(),
                    stderr_tail: String::new(),
                };
            }
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
            diff_command: Some("git diff".into()),
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
                commands: &runner,
                approvals: &approvals,
                sleeper: &sleeper,
                hooks: None,
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
            diff_command: Some("git diff".into()),
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
                commands: &runner,
                approvals: &approvals,
                sleeper: &sleeper,
                hooks: None,
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
        let provider =
            ScriptedProvider::new(vec![text_result("lookup"), text_result("the answer")]);
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
                commands: &runner,
                approvals: &approvals,
                sleeper: &sleeper,
                hooks: None,
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
    async fn headless_scope_review_without_bypass_is_a_named_error() {
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
                commands: &runner,
                approvals: &approvals,
                sleeper: &sleeper,
                hooks: None,
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
        assert_eq!(err, PipelineError::ScopeReviewRequiredHeadless);
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
                commands: &runner,
                approvals: &approvals,
                sleeper: &sleeper,
                hooks: None,
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

    #[test]
    fn shell_single_quote_neutralizes_metacharacters() {
        assert_eq!(shell_single_quote("a b"), "'a b'");
        // An embedded quote can't break out — the classic '\'' escape.
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
        assert_eq!(shell_single_quote("x; rm -rf ~"), "'x; rm -rf ~'");
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
                commands: &runner,
                approvals: &approvals,
                sleeper: &sleeper,
                hooks: None,
            },
            tx,
            PipelineConfig::default(),
        );

        // No baseline → the file is this turn's; its real 5000 lines count.
        let (lines, text) = pipeline.gather_diff(&HashMap::new()).await;
        assert_eq!(lines, 5000, "a new file counts its real added lines, not 1");
        assert!(text.contains("src/huge.rs"), "diff text names the new file");

        // Present before at the SAME fingerprint → untouched dirty state, not
        // attributed to the turn.
        let unchanged: HashMap<String, String> =
            std::iter::once(("src/huge.rs".to_string(), "v2".to_string())).collect();
        let (lines2, _) = pipeline.gather_diff(&unchanged).await;
        assert_eq!(
            lines2, 0,
            "a pre-existing untracked file is not this turn's change"
        );

        // Present before but at a DIFFERENT fingerprint → the turn edited an
        // already-untracked file; it must be visible (the P1 regression).
        let modified: HashMap<String, String> =
            std::iter::once(("src/huge.rs".to_string(), "v1".to_string())).collect();
        let (lines3, text3) = pipeline.gather_diff(&modified).await;
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
            source: "code-graph".into(),
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
            text_result("wrote the test.\nTEST_COMMAND: run-witness"),
            text_result("done"),
        ]);
        let resolver = OneProvider(&provider);
        // Test-command pops: witness fail-check (fail), per-candidate baseline
        // (fail), post-execute observation (pass) → a genuine flip.
        let runner = ScriptedRunner::new(vec![false, false, true], "@@ -1 +1 @@\n-old\n+new");
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
                commands: &runner,
                approvals: &approvals,
                sleeper: &sleeper,
                hooks: None,
            },
            tx,
            PipelineConfig::default(),
        );

        let mut messages = vec![CompletionMessage::system("sys")];
        let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
        let outcome = pipeline
            .run("Fix the retry bug", &mut messages, &mut budget)
            .await
            .expect("run succeeds");

        assert_eq!(outcome.status, PipelineStatus::Completed);
        let verdict = outcome.verdict.expect("verified");
        assert!(verdict.passed);
        assert!(
            verdict.deterministic,
            "a witness flip is deterministic evidence: {}",
            verdict.summary
        );
        assert!(
            verdict.summary.contains("run-witness"),
            "the evidence names the witness command: {}",
            verdict.summary
        );

        let s = stages(&drain(&mut rx));
        assert!(s.contains(&StageKind::Witness), "witness stage emitted");
        assert!(!s.contains(&StageKind::Judge), "judge skipped on the flip");
    }

    /// A witness whose test passes on the unmodified code proves nothing: one
    /// bounded repair retry, and if it still passes the witness is discarded —
    /// the run degrades to model-judge verification instead of failing.
    #[tokio::test]
    async fn a_witness_that_never_fails_is_discarded_and_the_judge_verifies() {
        let provider = ScriptedProvider::new(vec![
            text_result("single"),
            text_result("TEST_COMMAND: always-green"),
            // The repair attempt also yields a command that passes.
            text_result("TEST_COMMAND: still-green"),
            text_result("done"),                  // worker
            text_result("PASS matches the goal"), // judge (no flip evidence)
        ]);
        let resolver = OneProvider(&provider);
        // Pops: witness check (pass), repair check (pass) → discard. Then no
        // test command exists for the candidate, so no further pops.
        let runner = ScriptedRunner::new(vec![true, true], "@@ -1 +1 @@\n-a\n+b");
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
                commands: &runner,
                approvals: &approvals,
                sleeper: &sleeper,
                hooks: None,
            },
            tx,
            PipelineConfig::default(),
        );

        let mut messages = vec![CompletionMessage::system("sys")];
        let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
        let outcome = pipeline
            .run("Fix the retry bug", &mut messages, &mut budget)
            .await
            .expect("run succeeds");

        assert_eq!(outcome.status, PipelineStatus::Completed);
        let verdict = outcome.verdict.expect("verified");
        assert!(verdict.passed);
        assert!(
            !verdict.deterministic,
            "no flip → the model judge decided, not the ladder"
        );
        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Error { message, retryable: true } if message.contains("witness")
            )),
            "the discarded witness is warned about, never silent"
        );
        assert!(stages(&events).contains(&StageKind::Judge));
    }

    /// Tamper exclusion: the worker modified the witness test file after it
    /// was authored, so the observed fail→pass flip is NOT credited — the
    /// evidence degrades to inconclusive and the model judge decides.
    #[tokio::test]
    async fn a_tampered_witness_file_excludes_the_flip_from_evidence() {
        let provider = ScriptedProvider::new(vec![
            text_result("single"),
            text_result("TEST_COMMAND: run-witness"),
            text_result("done"),                     // worker
            text_result("FAIL the test was edited"), // judge on tampered evidence
        ]);
        let resolver = OneProvider(&provider);
        // witness check (fail), baseline (fail), post-execute (pass) → the
        // oracle flips — but the tamper check below must void the credit.
        let runner = ScriptedRunner::new(vec![false, false, true], "@@ -1 +1 @@\n-a\n+b");
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
        ]);
        let tools = EmptyTools;
        let recall = NoContextRecall;
        let repo = NoRepoStructure;
        let approvals = AutoApproveGate;
        let sleeper = NoopSleeper;
        let router = router();
        let (tx, mut rx) = mpsc::unbounded_channel();

        let config = PipelineConfig {
            max_revisions: 0, // judge FAIL ends the run — keeps the script short
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
                commands: &runner,
                approvals: &approvals,
                sleeper: &sleeper,
                hooks: None,
            },
            tx,
            config,
        );

        let mut messages = vec![CompletionMessage::system("sys")];
        let mut budget = BudgetGuard::new(BudgetMode::Off, None, None);
        let outcome = pipeline
            .run("Fix the retry bug", &mut messages, &mut budget)
            .await
            .expect("run succeeds");

        let verdict = outcome.verdict.expect("verified");
        assert!(!verdict.passed, "the judge failed the tampered work");
        assert!(
            !verdict.deterministic,
            "a tampered flip must never submit fast — it degrades to the judge"
        );
        assert!(
            stages(&drain(&mut rx)).contains(&StageKind::Judge),
            "tamper forces the model judge instead of SubmitFast"
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
                commands: &runner,
                approvals: &approvals,
                sleeper: &sleeper,
                hooks: None,
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
}
