//! Candidate-local witness authoring for the isolated typed-execution boundary.

use super::*;

/// Candidate-bound hook execution: both the hook process and the engine's
/// payload use the isolated root, never the session root.
pub(super) struct BoundHookRunner<'a> {
    pub(super) inner: &'a dyn HookRunner,
    pub(super) cwd: &'a str,
}

#[async_trait::async_trait]
impl HookRunner for BoundHookRunner<'_> {
    async fn run(
        &self,
        action: &stella_core::hooks::HookAction,
        payload_json: &str,
        _cwd: &str,
    ) -> Result<stella_core::hooks::HookExecResult, stella_core::hooks::HookExecError> {
        self.inner.run(action, payload_json, self.cwd).await
    }
}

impl<'a> Pipeline<'a> {
    pub(super) fn engine_config_for(&self, surface: CandidateSurface<'_>) -> EngineConfig {
        let mut config = self.config.engine.clone();
        if let Some(cwd) = surface.cwd {
            config.cwd = cwd.to_string();
        }
        config
    }

    /// Author one failing witness inside the same disposable candidate that
    /// will execute, revise, verify, and (only on pass) adopt it.
    pub(super) async fn witness_stage(
        &self,
        goal: &str,
        frames: &[RecalledFrame],
        author: &ResolvedRole<'a>,
        surface: CandidateSurface<'_>,
        budget: &mut BudgetGuard,
        total: &mut f64,
    ) -> Result<Option<Witness>, String> {
        self.emit(AgentEvent::Stage {
            name: StageKind::Witness,
        });

        let tracked_before = surface.repo_status.tracked_fingerprints().await;
        let untracked_before = surface.repo_status.untracked_fingerprints().await;
        let structure = self.repo.structure_summary().await;
        let witness_tools = surface
            .workspace
            .expect("witness authoring requires a candidate workspace")
            .witness_tools();
        let engine = Engine::with_sleeper(
            author.provider,
            witness_tools,
            self.engine_config_for(surface),
            self.sleeper,
        );

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
            TurnOutcome::Aborted { reason, cost_usd } => {
                *total += cost_usd;
                if let Some(abort) = budget_abort(budget.evaluate()) {
                    return Err(abort.reason);
                }
                return Err(format!("witness author turn aborted: {reason}"));
            }
        };
        let Some(mut command) = parse_witness_command(&text) else {
            return Err("witness author produced no TEST_COMMAND line".to_string());
        };

        let Ok(mut invocation) = parse_test_invocation(&command) else {
            return Err(format!(
                "witness author produced an unsafe or unsupported test command `{command}`"
            ));
        };
        if surface.tests.run_test(&invocation).await.passed() {
            messages.push(CompletionMessage::user(witness_repair_prompt(&command)));
            let repaired = match self
                .run_engine_turn(&engine, &mut messages, budget, &mut file_changes)
                .await
            {
                TurnOutcome::Completed { text, cost_usd } => {
                    *total += cost_usd;
                    text
                }
                TurnOutcome::Aborted { reason, cost_usd } => {
                    *total += cost_usd;
                    if let Some(abort) = budget_abort(budget.evaluate()) {
                        return Err(abort.reason);
                    }
                    return Err(format!("witness repair turn aborted: {reason}"));
                }
            };
            command = parse_witness_command(&repaired).unwrap_or(command);
            let Ok(repaired_invocation) = parse_test_invocation(&command) else {
                return Err(format!(
                    "witness repair produced an unsafe or unsupported test command `{command}`"
                ));
            };
            invocation = repaired_invocation;
            if surface.tests.run_test(&invocation).await.passed() {
                return Err(
                    "witness test still passes on the unmodified code after one repair".to_string(),
                );
            }
        }

        let tracked_after = surface.repo_status.tracked_fingerprints().await;
        let untracked_after = surface.repo_status.untracked_fingerprints().await;
        let fingerprints = validate_witness_artifact(
            &tracked_before,
            &tracked_after,
            &untracked_before,
            &untracked_after,
        )
        .map_err(|error| error.to_string())?;
        let path = fingerprints
            .keys()
            .next()
            .expect("validated witness artifact contains exactly one path");
        validate_witness_invocation(path, &invocation).map_err(|error| error.to_string())?;
        let identity = surface.repo_status.artifact_identity(path).await;
        validate_witness_identity(path, &fingerprints[path], identity.as_ref())
            .map_err(|error| error.to_string())?;
        let files = HashMap::from([(
            path.clone(),
            identity.expect("validated witness identity is present"),
        )]);
        Ok(Some(Witness {
            command,
            invocation,
            files,
        }))
    }
}
