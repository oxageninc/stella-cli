//! Goal-driven turns: judged rounds until a judge confirms the goal is met.
//!
//! `run_goal_cmd` drives either the staged pipeline (default) or the raw
//! `Engine::run_goal` step-loop. The goal judge is independent of the worker
//! model and answers "does the whole effort meet the goal?" — distinct from
//! the pipeline's per-change verification judge.

use super::*;

/// Run a one-shot prompt through the raw step-loop (Engine::run_turn).
/// Selected via `--no-pipeline`.
pub(crate) async fn run_raw_one_shot(
    cfg: &Config,
    prompt: &str,
    budget_limit: Option<f64>,
    format: OutputFormat,
) -> Result<(), String> {
    let provider = build_provider(cfg)?;
    let registry_options = registry_options(cfg);
    // Concrete `Arc<ToolRegistry>` (not `Arc<dyn ToolExecutor>`) so the
    // files-touched ledger is reachable after the turn — the trait object
    // hides it. It still coerces to `&dyn ToolExecutor` for the engine.
    let registry: std::sync::Arc<ToolRegistry> = std::sync::Arc::new(
        ToolRegistry::new_detected(cfg.workspace_root.clone(), registry_options.clone()).await,
    );
    populate_schema_index(&registry, &cfg.workspace_root)?;
    let active_rules =
        crate::rules::enforce_workspace_rules(&registry, &cfg.workspace_root, &cfg.authority);
    // Auto-build + live-refresh the code graph in the background so a
    // multi-step one-shot turn can reach for `graph_query` once the index is
    // ready. Status goes to stderr — stdout may be machine-readable JSON.
    let (_session_graph, _graph_build) = spawn_session_graph(
        &cfg.workspace_root,
        registry.clone(),
        Box::new(|line| eprintln!("  {line}")),
        Box::new(|| {}),
    );
    let process_free = crate::enterprise_telemetry::process_free_authority_active();
    let mcp = if process_free {
        None
    } else {
        connect_mcp(
            cfg,
            registry.clone(),
            Some(registry.mcp_usage_ledger()),
            format == OutputFormat::Text,
        )
        .await?
    };
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set.as_ref(),
        None => &*registry,
    };
    let custom_tools = if process_free {
        Vec::new()
    } else {
        discover_custom_tools(cfg, format == OutputFormat::Text).await
    };
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);
    let calibration = seed_calibration(&store, cfg);

    if format == OutputFormat::Text {
        tui::section_header("Stella");
        println!("  {}\n", prompt.dimmed());
    }

    let mut messages = vec![
        CompletionMessage::system(
            with_session_hook_context(
                build_system_prompt(cfg, &cfg.workspace_root, &active_rules),
                cfg,
            )
            .await,
        ),
        crate::attachments::user_message(prompt),
    ];

    // The self-improvement loop (memory.rs): recall relevant memories +
    // skills into a volatile block after the stable system prefix (L-E8)…
    let mut memory = SessionMemory::open_with_authority(
        &cfg.workspace_root,
        format == OutputFormat::Text,
        &cfg.authority,
    );
    if let Some(m) = &memory {
        inject_recall_block(&mut messages, m.recall_block(prompt).await);
    }

    let started_unix = crate::memory::unix_now_secs();
    // Machine-wide presence: findable in the deck's SESSIONS overlay and
    // replayable from its journal after this process exits.
    let mut presence = SessionPresence::announce(cfg, prompt);
    let outcome = run_turn(
        &*provider,
        base_tools,
        &custom_tools,
        &registry,
        &mut messages,
        &mut budget,
        &calibration,
        cfg,
        format,
        &store,
        "run",
        prompt,
        Some(presence.id()),
        &crate::discovery::new_activation(),
    )
    .await;
    // Episodic memory first (works even for a failed turn — failures are
    // exactly the episodes worth recalling)…
    if let Some(m) = &memory {
        let files = registry.files_touched();
        if turn_warrants_reflection(&messages) || !files.is_empty() {
            let episode_outcome = if outcome.is_ok() {
                EpisodeOutcome::Success
            } else {
                EpisodeOutcome::Failure
            };
            m.record_episode(prompt, episode_outcome, &files, started_unix)
                .await;
        }
    }
    // …and reflect on the completed turn, recording domain-tagged lessons
    // (recurring ones auto-promote to SKILL.md files). Best-effort: never
    // fails or slows the turn that just ran. Reflect on success AND failure —
    // a failed one-shot is a prime learning signal (root-cause prompt via
    // `succeeded=false`). Gated on `turn_warrants_reflection` so a tool-free
    // turn (nothing to mine, failure almost certainly external) never spends a
    // model call. The report is surfaced so a model-call error is never silent.
    // The raw one-shot closes its execution inside `run_turn`; unlike the
    // staged pipeline it has no post-turn event phase before that terminal
    // barrier. Keep machine streams strict by not dispatching an unframed
    // reflection call after `Complete` (text retains the best-effort loop).
    if format == OutputFormat::Text
        && turn_warrants_reflection(&messages)
        && let Some(m) = &mut memory
    {
        let mut report = m
            .reflect_and_record(
                &*provider,
                &cfg.model_id,
                &messages,
                format != OutputFormat::Text,
                outcome.is_ok(),
                crate::agent::remaining_budget(&budget),
            )
            .await;
        settle_reflection_budget(&mut report, &mut budget);
        surface_reflection(&report, format);
    }
    if let Some(set) = &mcp {
        set.close_all().await;
    }
    // Terminal registry status + the headless → `/inbox` flow (failure
    // always notifies; success only past the looked-away threshold).
    let run_secs = crate::memory::unix_now_secs().saturating_sub(started_unix);
    let notify = if outcome.is_err() {
        Some((
            format!("{}: run FAILED", presence.name()),
            crate::command_deck::prompt_line(prompt, 160),
        ))
    } else if run_secs >= 60 {
        Some((
            format!("{}: run finished ({run_secs}s)", presence.name()),
            crate::command_deck::prompt_line(prompt, 160),
        ))
    } else {
        None
    };
    presence.finish(outcome.is_ok(), notify);
    outcome
}

/// Run a one-shot goal loop (non-interactive): work in judged rounds until
/// a judge model assesses the goal as met (`stella goal "…"`, and `stella
/// monitor` composed on top of it). The judge is routed by role: when a
/// second provider family is configured (BYOK), `run_goal_turn` builds a
/// role `Router` and resolves `Role::Judge` to a DIFFERENT family than the
/// worker for bias-resistant assessment; with a
/// single family it stays the worker provider, identical to before. The
/// worker turns get the full tool stack (MCP + custom + interactive +
/// skills), same as `run_one_shot`.
/// Work in judged rounds until a judge model confirms the goal is met.
/// `use_pipeline` (the default) runs each working round through the staged
/// pipeline (triage → recall → plan → witness → execute → verify → judge);
/// `false` falls back to the raw `Engine::run_goal` step-loop.
pub async fn run_goal_cmd(
    cfg: &Config,
    goal: &str,
    budget_limit: Option<f64>,
    use_pipeline: bool,
) -> Result<(), String> {
    crate::enterprise_telemetry::authorize_execution_surface(
        crate::enterprise_telemetry::ExecutionSurface::Goal,
    )?;
    let provider = build_provider(cfg)?;
    let registry_options = registry_options(cfg);
    let registry: std::sync::Arc<ToolRegistry> = std::sync::Arc::new(
        ToolRegistry::new_detected(cfg.workspace_root.clone(), registry_options.clone()).await,
    );
    populate_schema_index(&registry, &cfg.workspace_root)?;
    let active_rules =
        crate::rules::enforce_workspace_rules(&registry, &cfg.workspace_root, &cfg.authority);
    // Auto-build + live-refresh the code-graph index in the background so
    // `graph_query` is available for the goal loop without a manual `stella
    // init`. Non-blocking; status to stderr. Kept alive until the goal returns.
    let (_session_graph, _graph_build) = spawn_session_graph(
        &cfg.workspace_root,
        registry.clone(),
        Box::new(|line| eprintln!("  {line}")),
        Box::new(|| {}),
    );
    let mcp = connect_mcp(
        cfg,
        registry.clone(),
        Some(registry.mcp_usage_ledger()),
        true,
    )
    .await?;
    let base_tools: &dyn ToolExecutor = match &mcp {
        Some(set) => set.as_ref(),
        None => &*registry,
    };
    let custom_tools = discover_custom_tools(cfg, true).await;
    let mut budget = build_budget_guard(budget_limit);
    let store = open_store(&cfg.workspace_root);
    let calibration = seed_calibration(&store, cfg);

    tui::section_header("Stella — goal mode");
    println!("  {}\n", goal.dimmed());

    let mut messages = vec![CompletionMessage::system(
        with_session_hook_context(
            build_system_prompt(cfg, &cfg.workspace_root, &active_rules),
            cfg,
        )
        .await,
    )];
    let mut memory = SessionMemory::open_with_authority(&cfg.workspace_root, true, &cfg.authority);
    if let Some(m) = &memory {
        inject_recall_block(&mut messages, m.recall_block(goal).await);
    }

    let started_unix = crate::memory::unix_now_secs();
    // Machine-wide presence: a goal run is exactly the long-lived headless
    // session the SESSIONS overlay + replay exist for.
    let mut presence = SessionPresence::announce(cfg, goal);
    let outcome = if use_pipeline {
        run_goal_pipeline_turn(
            &*provider,
            base_tools,
            &custom_tools,
            &registry,
            &mut messages,
            &mut budget,
            &calibration,
            cfg,
            &store,
            goal,
            Some(presence.id()),
            registry_options.clone(),
            active_rules.clone(),
            mcp.clone(),
        )
        .await
    } else {
        run_goal_turn(
            &*provider,
            base_tools,
            &custom_tools,
            &registry,
            &mut messages,
            &mut budget,
            &calibration,
            cfg,
            &store,
            goal,
            Some(presence.id()),
        )
        .await
    };
    if let Some(m) = &memory {
        let files = registry.files_touched();
        if turn_warrants_reflection(&messages) || !files.is_empty() {
            let episode_outcome = if outcome.is_ok() {
                EpisodeOutcome::Success
            } else {
                EpisodeOutcome::Failure
            };
            m.record_episode(goal, episode_outcome, &files, started_unix)
                .await;
        }
    }
    if outcome.is_ok()
        && turn_warrants_reflection(&messages)
        && let Some(m) = &mut memory
    {
        let mut report = m
            .reflect_and_record(
                &*provider,
                &cfg.model_id,
                &messages,
                false,
                true,
                crate::agent::remaining_budget(&budget),
            )
            .await;
        settle_reflection_budget(&mut report, &mut budget);
        surface_reflection(&report, OutputFormat::Text);
    }
    if let Some(set) = &mcp {
        set.close_all().await;
    }
    // Goal runs are long by construction — always land the inbox
    // notification (Enter on it replays this session's journal).
    let goal_secs = crate::memory::unix_now_secs().saturating_sub(started_unix);
    let notify = if outcome.is_ok() {
        format!("{}: goal met ({goal_secs}s)", presence.name())
    } else {
        format!("{}: goal run FAILED", presence.name())
    };
    presence.finish(
        outcome.is_ok(),
        Some((notify, crate::command_deck::prompt_line(goal, 160))),
    );
    outcome
}

/// Run one goal loop through `stella_core::Engine::run_goal`: working turns
/// interleaved with judge assessments until the judge passes it (or a
/// backstop — rounds, budget, abort — ends it with a named reason). The
/// worker gets the full tool stack (MCP + custom + interactive + skills) and
/// the judge a read-only view of that same stack.
///
/// The judge is routed by role (`resolve_cross_family_judge`): when a second
/// provider family is configured and the `Router` selects it, the judge runs
/// on a DIFFERENT model family than the worker (bias-resistant assessment,
///) and a one-line notice is printed. With a single
/// configured family — or on any discovery/build failure — the judge is the
/// worker provider itself, identical to before: no second provider is built
/// and no extra cost is incurred. Text-mode rendering only — goal and
/// monitor never take `--output-format`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_goal_turn(
    provider: &dyn Provider,
    base_tools: &dyn ToolExecutor,
    custom_tools: &[CustomTool],
    registry: &ToolRegistry,
    messages: &mut Vec<CompletionMessage>,
    budget: &mut BudgetGuard,
    calibration: &CalibrationMap,
    cfg: &Config,
    store: &Option<Arc<Store>>,
    goal: &str,
    session: Option<&str>,
) -> Result<(), String> {
    let turn_start = Instant::now();
    let execution = begin_execution(store, "goal", goal, cfg, session);

    // Route the JUDGE role. `Some` only when a distinct-family judge was
    // selected AND built; the boxed provider must outlive the `run_goal`
    // call below, so it is bound here. `None` → the judge is the worker
    // provider (single-family/failure fallback — the v1 behavior).
    let configured = crate::config::discover_configured_providers();
    let routed_judge = resolve_cross_family_judge(cfg.provider.id, &cfg.model_id, &configured);
    if let Some((_, judge_id)) = &routed_judge {
        println!(
            "  {} cross-family judge: {} worker · {} judge — independent, bias-resistant \
             assessment\n",
            "◆".bright_cyan(),
            cfg.provider.id.bright_magenta(),
            judge_id.bright_green(),
        );
    }
    let judge: &dyn Provider = match &routed_judge {
        Some((boxed, _)) => &**boxed,
        None => provider,
    };

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let renderer = spawn_renderer(
        rx,
        OutputFormat::Text,
        execution.clone(),
        cfg.provider.id.to_string(),
    );

    let outcome = {
        let customs = CustomToolSet::new(
            base_tools,
            custom_tools.to_vec(),
            cfg.workspace_root.clone(),
        );
        let interactive = InteractiveToolSet::new(&customs, tx.clone(), default_ask_io(true))
            .with_skill_registry(SkillRegistry::from_env(cfg.workspace_root.clone()));
        let tools =
            crate::discovery::DiscoveryToolSet::new(&interactive, cfg.workspace_root.clone())
                .with_project_prompts_allowed(cfg.authority.project_prompts_allowed);
        let hook_runner = ShellHookRunner;
        let mut engine =
            Engine::with_sleeper(provider, &tools, engine_config_for(cfg), &TokioSleeper)
                .with_calibration(calibration);
        if let Some(hooks) = &cfg.hooks {
            engine = engine.with_hooks(hooks, &hook_runner);
        }
        engine
            .run_goal(judge, goal, messages, budget, &tx, &GoalConfig::default())
            .await
    };
    drop(tx);
    let persistence_complete = renderer.await.unwrap_or_default().persistence_complete;

    let files = registry.files_touched();
    if let Some((store, id)) = &execution {
        let (outcome_label, cost) = match &outcome {
            GoalOutcome::Met { cost_usd, .. } => ("goal_met", *cost_usd),
            GoalOutcome::Unmet { cost_usd, .. } => ("goal_unmet", *cost_usd),
        };
        if !record_execution_end(
            store,
            *id,
            registry,
            outcome_label,
            cost,
            persistence_complete,
        ) {
            warn_store_write_failed(
                "the audit record (files touched / memory citations / outcome)",
            );
        }
    }
    tui::files_touched_panel(&files);

    match outcome {
        GoalOutcome::Met {
            rounds,
            verdict,
            cost_usd,
        } => {
            println!(
                "\n  {} goal met after {rounds} round{}: {}",
                "✓".green().bold(),
                if rounds == 1 { "" } else { "s" },
                verdict
            );
            tui::cost_summary(
                cost_usd,
                &format!("{}/{}", cfg.provider.id, cfg.model_id),
                turn_start.elapsed(),
            );
            println!();
            Ok(())
        }
        GoalOutcome::Unmet {
            rounds,
            reason,
            cost_usd,
        } => {
            tui::cost_summary(
                cost_usd,
                &format!("{}/{}", cfg.provider.id, cfg.model_id),
                turn_start.elapsed(),
            );
            Err(format!("goal not met after {rounds} round(s): {reason}"))
        }
    }
}

/// One staged-pipeline goal turn: keep running the pipeline (triage → recall →
/// plan → witness → execute → verify → judge) until an independent goal judge
/// assesses the goal as met, or a backstop ends the loop. This is the pipeline
/// analogue of [`run_goal_turn`] — same goal-loop structure, same judgment,
/// but each working round goes through the staged pipeline instead of the raw
/// `Engine::run_turn`.
///
/// The goal-loop judge is distinct from the pipeline's verify judge: the verify
/// judge (inside [`Pipeline::run`]) answers "did this change pass its tests?",
/// while the goal judge here answers "does the whole effort meet the goal?".
/// Both are independent of the worker model.
#[allow(clippy::too_many_arguments)]
async fn run_goal_pipeline_turn(
    provider: &dyn Provider,
    base_tools: &dyn ToolExecutor,
    custom_tools: &[CustomTool],
    registry: &ToolRegistry,
    messages: &mut Vec<CompletionMessage>,
    budget: &mut BudgetGuard,
    calibration: &CalibrationMap,
    cfg: &Config,
    store: &Option<Arc<Store>>,
    goal: &str,
    session: Option<&str>,
    registry_options: stella_tools::RegistryOptions,
    active_rules: crate::rules::ResolvedRules,
    mcp: Option<Arc<stella_mcp::McpToolSet>>,
) -> Result<(), String> {
    let turn_start = Instant::now();
    let execution = begin_execution(store, "goal", goal, cfg, session);
    let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());

    // Role wiring from `agent_engine_config` — the pinned/auto judge (when
    // configured) also serves as the goal loop's round judge below.
    let configured = crate::config::discover_configured_providers();
    let wiring = resolve_engine_wiring(cfg, &model_ref, &configured);
    for notice in &wiring.notices {
        eprintln!("  ! {notice}");
    }

    // Route the goal-loop judge. The pipeline's verify judge is the same role;
    // both want independence from the worker. An engine-config judge pin
    // (explicit or auto_mode) wins; otherwise the discovery-based
    // cross-family routing applies exactly as before.
    let configured = crate::config::discover_configured_providers();
    let engine_judge: Option<(&dyn Provider, String)> = wiring
        .pins
        .get(Role::Judge)
        .and_then(|pinned| {
            wiring
                .extra_providers
                .iter()
                .find(|(model_ref, _)| model_ref == pinned)
        })
        .map(|(model_ref, provider)| (&**provider, model_ref.provider.clone()));
    let routed_judge = if engine_judge.is_none() {
        resolve_cross_family_judge(cfg.provider.id, &cfg.model_id, &configured)
    } else {
        None
    };
    let (judge, judge_id): (&dyn Provider, Option<String>) = match (&engine_judge, &routed_judge) {
        (Some((provider, id)), _) => (*provider, Some(id.clone())),
        (None, Some((boxed, id))) => (&**boxed, Some(id.clone())),
        (None, None) => (provider, None),
    };
    if let Some(judge_id) = &judge_id {
        println!(
            "  {} cross-family judge: {} worker · {} judge — independent, bias-resistant \
             assessment\n",
            "◆".bright_cyan(),
            cfg.provider.id.bright_magenta(),
            judge_id.bright_green(),
        );
    }

    let goal_config = GoalConfig::default();
    let resolver = RoleProviderResolver::new(provider, model_ref.clone(), &wiring.extra_providers);

    let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
    let renderer = spawn_renderer(
        rx,
        OutputFormat::Text,
        execution.clone(),
        cfg.provider.id.to_string(),
    );

    // Run the loop; the result is folded into `goal_result` so there is exactly
    // one teardown path (drop tx → await renderer → record execution → return).
    let goal_result = {
        let customs = CustomToolSet::new(
            base_tools,
            custom_tools.to_vec(),
            cfg.workspace_root.clone(),
        );
        let interactive = InteractiveToolSet::new(&customs, tx.clone(), default_ask_io(true))
            .with_skill_registry(SkillRegistry::from_env(cfg.workspace_root.clone()));
        let tools =
            crate::discovery::DiscoveryToolSet::new(&interactive, cfg.workspace_root.clone())
                .with_project_prompts_allowed(cfg.authority.project_prompts_allowed);

        let breaker = CircuitBreaker::new(Box::new(SystemClock::new()));
        let router = Router::new(wiring.pins.clone(), wiring.profiles.clone(), breaker);

        let ws_ports = workspace_ports(
            cfg.workspace_root.clone(),
            cfg,
            registry_options,
            active_rules.clone(),
            mcp,
        )?;
        let no_recall = NoContextRecall;
        let recall: &dyn ContextRecallPort = &no_recall;
        let hook_runner = ShellHookRunner;

        // The goal judge engine, built once and reused across rounds — shares
        // the session calibration (keyed per model, so a cross-family judge
        // learns its own drift) and a read-only view of the same tools.
        let read_only = stella_core::ports::ReadOnlyTools::new(&tools);
        let judge_engine = Engine::with_sleeper(
            judge,
            &read_only,
            judge_engine_config_for(cfg),
            &TokioSleeper,
        )
        .with_calibration(calibration);

        let mut total_cost_usd = 0.0f64;
        let mut result: Option<Result<(), String>> = None;
        let mut goal_met = false;

        for round in 1..=goal_config.max_rounds {
            budget.begin_turn();
            let pipeline_config = PipelineConfig {
                engine: pipeline_engine_config_for(cfg, &wiring.worker_model),
                role_overrides: wiring.role_overrides.clone(),
                headless: true,
                headless_bypass_scope_review: HEADLESS_SCOPE_REVIEW_BYPASS,
                ..PipelineConfig::default()
            };
            let ports = PipelinePorts {
                router: &router,
                providers: &resolver,
                tools: &tools,
                recall,
                repo: &ws_ports.repo_structure,
                repo_status: &ws_ports.repo_status,
                diagnostics: &ws_ports.diagnostic_runner,
                tests: &ws_ports.test_runner,
                approvals: &HEADLESS_APPROVAL_GATE,
                sleeper: &TokioSleeper,
                hooks: cfg
                    .hooks
                    .as_ref()
                    .map(|h| (h, &hook_runner as &dyn stella_core::hooks::HookRunner)),
                candidate_workspaces: Some(&ws_ports.candidate_workspaces),
                mcp_prefetch: ws_ports
                    .mcp_prefetch
                    .as_ref()
                    .map(|p| p as &dyn McpPrefetchPort),
                // Goal pipeline rounds run without an interactive steer tap.
                steering: None,
            };
            let pipeline = Pipeline::new(ports, tx.clone(), pipeline_config);
            let round_goal = format!(
                "GOAL: {goal}\n\nWork toward this goal. An independent judge will assess the \
                 result after each working round from the transcript evidence; keep your work \
                 verifiable (run tests, show outputs)."
            );
            match pipeline.run(&round_goal, messages, budget).await {
                Ok(outcome) => {
                    total_cost_usd += outcome.total_cost_usd;
                    match outcome.status {
                        PipelineStatus::Completed => {}
                        PipelineStatus::VerificationFailed { verdict } => {
                            result = Some(Err(format!(
                                "goal not met: verification failed: {}",
                                verdict.summary
                            )));
                            break;
                        }
                        PipelineStatus::Aborted { reason } => {
                            result = Some(Err(format!(
                                "goal not met: working round aborted: {reason}"
                            )));
                            break;
                        }
                    }
                }
                Err(e) => {
                    result = Some(Err(e.to_string()));
                    break;
                }
            }

            // Goal assessment (same judge + read-only tools as the raw goal loop).
            let _ = tx.send(AgentEvent::Stage {
                name: stella_protocol::StageKind::Judge,
            });
            let (verdict, judge_cost) = match judge_engine
                .assess(judge, goal, messages, budget, &tx, &goal_config)
                .await
            {
                Ok(pair) => pair,
                Err(reason) => {
                    result = Some(Err(format!("goal not met: judge unavailable: {reason}")));
                    break;
                }
            };
            total_cost_usd += judge_cost;
            let _ = tx.send(AgentEvent::GoalVerdict {
                round,
                met: verdict.met,
                reasoning: verdict.reasoning.clone(),
                cost_usd: judge_cost,
            });

            if verdict.met {
                tui::files_touched_panel(&registry.files_touched());
                println!(
                    "\n  {} goal met after {round} round{}: {}",
                    "✓".green().bold(),
                    if round == 1 { "" } else { "s" },
                    verdict.reasoning
                );
                tui::cost_summary(
                    total_cost_usd,
                    &format!("{}/{}", cfg.provider.id, cfg.model_id),
                    turn_start.elapsed(),
                );
                println!();
                goal_met = true;
                break;
            }

            let feedback = if verdict.feedback.trim().is_empty() {
                verdict.reasoning.clone()
            } else {
                verdict.feedback.clone()
            };
            messages.push(CompletionMessage::user(format!(
                "The judge assessed the goal as NOT yet met.\nJudge feedback: {feedback}\n\n\
                 Continue working toward the goal: {goal}"
            )));
        }

        // An explicit break result (abort/error/judge-down) stands. If the goal
        // was met, success. Otherwise the round cap was reached unmet.
        match (result, goal_met) {
            (Some(r), _) => r,
            (None, true) => Ok(()),
            (None, false) => {
                tui::cost_summary(
                    total_cost_usd,
                    &format!("{}/{}", cfg.provider.id, cfg.model_id),
                    turn_start.elapsed(),
                );
                Err(format!(
                    "goal not met after {} round(s): round cap reached without a passing verdict",
                    goal_config.max_rounds
                ))
            }
        }
    };

    drop(tx);
    let persistence_complete = renderer.await.unwrap_or_default().persistence_complete;
    // The shared guard is the settled ledger, including a judge turn that
    // aborted after spending and therefore returned no `judge_cost` value.
    let total_cost_usd = budget.session_spent_usd();
    let files = registry.files_touched();
    if let Some((store, id)) = &execution {
        let outcome_label = match &goal_result {
            Ok(()) => "goal_met",
            Err(_) => "goal_unmet",
        };
        if !record_execution_end(
            store,
            *id,
            registry,
            outcome_label,
            total_cost_usd,
            persistence_complete,
        ) {
            warn_store_write_failed(
                "the audit record (files touched / memory citations / outcome)",
            );
        }
    }
    tui::files_touched_panel(&files);
    goal_result
}
