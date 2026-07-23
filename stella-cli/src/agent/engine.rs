//! Engine tuning and pipeline provider wiring.
//!
//! `EngineConfig` construction per agent kind, and the role->provider
//! resolution for pipeline runs. Every wiring failure here is soft: an
//! unroutable role rides the worker provider, so configuration can never
//! turn a runnable pipeline into an error.

use super::*;

/// EngineConfig for `kind`: defaults + the workspace root as hook `cwd`,
/// with the agent's `agent_engine_config` tuning applied — temperature and
/// max_tokens override the engine defaults only when set (the "Include"
/// contract), effort/reasoning/params land verbatim (they default to
/// `None` anyway).
///
/// `catalog_ref` is the `(provider_id, model_id)` the catalog-based clamps
/// below (context-window compaction budget, reasoning capability) are
/// computed against — the model THIS kind's calls actually land on, not
/// necessarily `cfg`'s. For `Default`/`Judge` that is `cfg.provider.id`/
/// `cfg.model_id`; for `Worker` it is the wiring's resolved worker model
/// (issue #276 — honoring `pipeline_worker_model`/`agents.worker.*` must
/// also clamp against the model it actually routes to, or a worker pinned to
/// a smaller-context or non-reasoning model still gets the DEFAULT model's
/// clamps, which is exactly the wire-shape/400 class issue #273 exists to
/// warn about).
fn tuned_engine_config(
    cfg: &Config,
    kind: crate::settings::EngineAgentKind,
    catalog_ref: (&str, &str),
) -> EngineConfig {
    let (provider_id, model_id) = catalog_ref;
    let mut engine = EngineConfig {
        cwd: cfg.workspace_root.display().to_string(),
        ..EngineConfig::default()
    };
    // Compaction must fire BEFORE the provider's context window overflows:
    // the engine default (150k) exceeds some catalog windows (deepseek-chat
    // is 128k), where provider-side overflow would land before compaction
    // ever triggered. The window only ever LOWERS the default — 3/4 leaves
    // headroom for the estimator's error band plus the next step's output.
    if let Ok(entry) = stella_model::catalog::Catalog::current().resolve_for(provider_id, model_id)
    {
        let window = entry.context_window as u64;
        if window > 0 {
            engine.compaction_budget_tokens = engine
                .compaction_budget_tokens
                .min(window.saturating_mul(3) / 4);
        }
    }
    if let Some(settings) = &cfg.engine_settings {
        let tuning = crate::engine_config::tuning_for(settings, kind);
        if tuning.temperature.is_some() {
            engine.temperature = tuning.temperature;
        }
        if tuning.max_output_tokens.is_some() {
            engine.max_output_tokens = tuning.max_output_tokens;
        }
        engine.effort = tuning.effort;
        engine.reasoning = tuning.reasoning;
        engine.params = tuning.params;
    }
    // Capability clamp: a catalog-confirmed non-reasoning model must not
    // carry effort/reasoning onto the wire — providers reject or silently
    // ignore them, and both outcomes are worse than omitting the fields
    // (the auto modes set effort for every role without knowing the
    // model). Unknown capability passes through: the provider stays the
    // authority.
    if crate::engine_config::model_supports_reasoning(provider_id, model_id) == Some(false) {
        engine.effort = None;
        engine.reasoning = None;
    }
    engine
}

/// EngineConfig for a session's default (interactive/step-loop) agent.
pub(crate) fn engine_config_for(cfg: &Config) -> EngineConfig {
    tuned_engine_config(
        cfg,
        crate::settings::EngineAgentKind::Default,
        (cfg.provider.id, &cfg.model_id),
    )
}

/// EngineConfig for a pipeline's execute turns — the WORKER agent's tuning
/// (plan and witness ride it too, matching the router's tiering).
/// `worker_model` is [`EngineWiring::worker_model`]: the model the worker
/// role actually resolves to, honoring `pipeline_worker_model`/
/// `agents.worker.*` when set (issue #276), falling back to the session
/// default (`cfg.provider`/`cfg.model_id`) when unset.
pub(crate) fn pipeline_engine_config_for(cfg: &Config, worker_model: &ModelRef) -> EngineConfig {
    tuned_engine_config(
        cfg,
        crate::settings::EngineAgentKind::Worker,
        (&worker_model.provider, &worker_model.model_id),
    )
}

/// CLI-owned headless surfaces have no host approval port, so scope expansion
/// always stops at the named pipeline error. Output modes never alter this.
pub(crate) const HEADLESS_SCOPE_REVIEW_BYPASS: bool = false;
pub(crate) const HEADLESS_APPROVAL_GATE: AlwaysAbortGate = AlwaysAbortGate;

/// Approval port the one-shot host can actually service. This is explicit so
/// output serialization cannot silently stand in for execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipelineApprovalCapability {
    Stdio,
    Unavailable,
}

/// Which approval capability a one-shot host run can actually service, given
/// the output format and whether stdin/stdout are real terminals. A pure
/// function over already-observed booleans (rather than calling
/// `IsTerminal` itself) so the exact condition is directly unit-testable.
/// Stdio approval requires a text-safe renderer PLUS both terminal handles:
/// stdin must accept the
/// decision and stdout must present the prompt. A redirected/piped
/// text-format run is still rendered as text, but must stay headless and
/// fail closed at scope review — never read stdin for a decision no one is
/// there to give.
pub(crate) fn approval_capability_for(
    is_text: bool,
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
) -> PipelineApprovalCapability {
    if is_text && stdin_is_terminal && stdout_is_terminal {
        PipelineApprovalCapability::Stdio
    } else {
        PipelineApprovalCapability::Unavailable
    }
}

/// Build the one-shot pipeline config from the host's approval capability.
/// Rendering remains a separate concern owned by the event renderer.
/// `worker_model` is [`EngineWiring::worker_model`], threaded through to
/// `pipeline_engine_config_for` so the worker's clamps key off the model the
/// role actually resolves to (issue #276).
pub(crate) fn pipeline_config_for_approval_capability(
    cfg: &Config,
    approval: PipelineApprovalCapability,
    test_command: Option<&str>,
    worker_model: &ModelRef,
) -> PipelineConfig {
    PipelineConfig {
        engine: pipeline_engine_config_for(cfg, worker_model),
        headless: approval == PipelineApprovalCapability::Unavailable,
        // The constant is the safe default; a workspace may opt out of it
        // where the tree is disposable (see `headless_scope_bypass`).
        headless_bypass_scope_review: cfg
            .engine_settings
            .as_ref()
            .is_some_and(|engine| engine.headless_scope_bypass_on())
            || HEADLESS_SCOPE_REVIEW_BYPASS,
        test_command: test_command.map(str::to_string),
        ..Default::default()
    }
}

/// EngineConfig for the goal loop's standalone judge engine — the JUDGE
/// agent's tuning.
pub(crate) fn judge_engine_config_for(cfg: &Config) -> EngineConfig {
    tuned_engine_config(
        cfg,
        crate::settings::EngineAgentKind::Judge,
        (cfg.provider.id, &cfg.model_id),
    )
}

/// Fire `SessionStart` hooks once and return their stdout — the additional
/// session context `stella_core::hooks` documents. `None` when no hooks are
/// configured or they printed nothing. Called once per session by each
/// driver, never per turn.
pub(crate) async fn session_start_hook_context(cfg: &Config) -> Option<String> {
    let hooks = cfg.hooks.as_ref()?;
    let outcome = stella_core::hooks::run_hooks(
        &ShellHookRunner,
        Some(hooks),
        &stella_core::hooks::HookPayload::session_start(cfg.workspace_root.display().to_string()),
    )
    .await;
    // A SessionStart hook that failed stays non-fatal, but must not vanish:
    // without this line a typo'd hook silently contributes no context all
    // session and nothing ever says why (issue #373, item 6).
    for diagnostic in &outcome.diagnostics {
        eprintln!("  ! SessionStart {diagnostic}");
    }
    (!outcome.output.is_empty()).then_some(outcome.output)
}

/// Append any `SessionStart` hook context to an assembled system prompt.
/// The result is still byte-stable for the session: hooks fire once, here,
/// and the prompt never changes afterwards.
pub(crate) async fn with_session_hook_context(mut system_prompt: String, cfg: &Config) -> String {
    if let Some(context) = session_start_hook_context(cfg).await {
        system_prompt.push_str("\n\nSession context (from SessionStart hooks):\n");
        system_prompt.push_str(&context);
    }
    system_prompt
}

// Pipeline port adapters

/// Everything `agent_engine_config` resolves for one pipeline run: the
/// role router inputs (profiles + pins), owned adapters for roles routed
/// to a model other than the worker's, the per-role request overrides,
/// and human-readable notices about wiring decisions (a provider without
/// a credential, an adapter that failed to build). Every failure is soft:
/// the affected role rides the worker, exactly as before this config
/// existed — configuration must never turn a runnable pipeline into an
/// error.
pub(crate) struct EngineWiring {
    pub(crate) profiles: Vec<ProviderProfile>,
    pub(crate) pins: RoleTable,
    /// Adapters for pinned off-worker models, keyed by the exact
    /// [`ModelRef`] the pins route to (adapters bind their model id at
    /// construction, so each distinct ref needs its own instance).
    pub(crate) extra_providers: Vec<(ModelRef, Box<dyn Provider>)>,
    pub(crate) role_overrides: stella_pipeline::PipelineRoleOverrides,
    /// The model `Role::Worker`/`Role::Plan` actually resolve to: the
    /// worker's own `pipeline_worker_model`/`agents.worker.*` pin when one
    /// is configured and its provider is credentialed (issue #276), else
    /// the session default this wiring was built with. Callers building the
    /// worker's own [`EngineConfig`] (catalog-based context-window and
    /// reasoning-capability clamps) must key off THIS, not `cfg` directly —
    /// see `pipeline_engine_config_for`.
    pub(crate) worker_model: ModelRef,
    pub(crate) notices: Vec<String>,
}

/// Resolve one role's already-computed [`ModelSpec`] into a pin: find the
/// credentialed provider, and build its adapter unless the pin names the
/// exact same model the primary resolver entry already serves (`base_ref` —
/// always the literal session-default `ModelRef` the pre-built primary
/// provider is bound to, never an already-overridden ref, so this check
/// stays "does this need a NEW adapter instance" regardless of which role is
/// being pinned). `roles` lets one resolved model pin more than one router
/// role at once (`Role::Plan` shares `Role::Worker`'s tier — see
/// `resolve_engine_wiring`'s worker-override handling). Every failure here
/// is soft — a missing credential or a build error pushes a notice and
/// leaves every role in `roles` unpinned, degrading to `fallback` in the
/// router, never a hard error. Returns the resolved [`ModelRef`] on success.
fn pin_role(
    wiring: &mut EngineWiring,
    roles: &[Role],
    label: &str,
    spec: &crate::engine_config::ModelSpec,
    base_ref: &ModelRef,
    configured: &[crate::config::ConfiguredProvider],
    fallback: &str,
) -> Option<ModelRef> {
    let Some(entry) = configured.iter().find(|c| c.config.id == spec.provider) else {
        wiring.notices.push(format!(
            "engine config: {label} model `{}/{}` skipped — no resolvable credential for \
             provider `{}`; {label} {fallback}",
            spec.provider, spec.model, spec.provider
        ));
        return None;
    };
    // An empty slug is the "provider pin without a model" form — the
    // provider's own default model.
    let slug = if spec.model.is_empty() {
        entry.config.default_model.to_string()
    } else {
        spec.model.clone()
    };
    let pinned = ModelRef::new(entry.config.id, slug.clone());
    if pinned == *base_ref {
        // Same instance the primary resolver entry already serves: no new
        // adapter needed, the pin(s) still record the explicit choice.
        for &role in roles {
            wiring.pins.pin(role, pinned.clone());
        }
        return Some(pinned);
    }
    match build_provider_parts(
        &entry.config,
        &slug,
        entry.api_key.clone(),
        entry.config.base_url.to_string(),
        None,
    ) {
        Ok(provider) => {
            for &role in roles {
                wiring.pins.pin(role, pinned.clone());
            }
            // A profile for the routed provider keeps the router's provider
            // list honest (breaker bookkeeping, `providers()` introspection,
            // and — critically — the router's own unpinned-judge cross-
            // family lookup, which matches `resolve(Worker)`'s result against
            // a profile's `worker_model` field) even though the pin itself
            // short-circuits normal tiered resolution.
            wiring.profiles.push(
                ProviderProfile::new(
                    entry.config.id,
                    pinned.clone(),
                    pinned.clone(),
                    pinned.clone(),
                )
                .with_family(provider_family(entry.config.id)),
            );
            wiring.extra_providers.push((pinned.clone(), provider));
            Some(pinned)
        }
        Err(e) => {
            wiring.notices.push(format!(
                "engine config: {label} model `{}/{slug}` skipped — {e}; {label} {fallback}",
                entry.config.id
            ));
            None
        }
    }
}

/// Resolve the engine wiring for a pipeline run whose session-default worker
/// is `worker_ref` (already resolved by `Config` — an explicit `--model`
/// flag beats the settings, see `Config::load_with_settings`). `configured`
/// is the caller's own [`crate::config::discover_configured_providers`]
/// snapshot — injected rather than rediscovered here so this function is a
/// plain, testable one over owned data.
///
/// Routing rules, in order:
/// - WORKER (and `Role::Plan`, which shares the worker's tier when unpinned
///   — `resolve_tier` in `stella-core`'s router) honors
///   `pipeline_worker_model`/`agents.worker.*`
///   ([`crate::engine_config::model_spec_for`]) when configured and its
///   provider is credentialed; unset or unroutable falls back to the
///   session default `worker_ref` (issue #276).
/// - TRIAGE and JUDGE pins come from their configured model specs the same
///   way, but always fall back to the (possibly worker-overridden) worker
///   model on any failure — the pre-existing behavior.
/// - `auto_mode: on` replaces the judge spec with
///   [`crate::engine_config::auto_judge_spec`]'s pick from
///   `allowed_models` (cross-family from the ACTUAL worker model, then
///   price tier); when the allowed list yields nothing usable it falls back
///   to the explicit judge spec, then to normal router degradation.
/// - A pin equal to the session-default model needs no extra adapter — the
///   primary resolver entry already serves it.
///
/// Pins deliberately bypass the circuit breaker (`RoleTable` semantics —
/// an explicit pin wins unconditionally). If a pinned judge's provider
/// fails, the pipeline's judge call degrades to its heuristic verdict,
/// the same soft path an unreachable judge always took.
pub(crate) fn resolve_engine_wiring(
    cfg: &Config,
    worker_ref: &ModelRef,
    configured: &[crate::config::ConfiguredProvider],
) -> EngineWiring {
    use crate::engine_config::{
        ModelSpec, auto_judge_spec, model_spec_for, spec_family, tuning_for,
    };
    use crate::settings::EngineAgentKind;

    let worker_profile = ProviderProfile::new(
        worker_ref.provider.clone(),
        worker_ref.clone(),
        worker_ref.clone(),
        worker_ref.clone(),
    )
    .with_family(provider_family(&worker_ref.provider));

    let mut wiring = EngineWiring {
        profiles: vec![worker_profile],
        pins: RoleTable::new(),
        extra_providers: Vec::new(),
        role_overrides: stella_pipeline::PipelineRoleOverrides::default(),
        worker_model: worker_ref.clone(),
        notices: Vec::new(),
    };
    let Some(engine) = cfg.engine_settings.clone() else {
        return wiring;
    };

    // Credentialed providers only — a model spec naming a provider without
    // a resolvable key is reported and skipped, never a hard error.
    let is_provider = |id: &str| configured.iter().any(|c| c.config.id == id);

    // Issue #276: resolve the WORKER's own override first, before anything
    // that needs to know the worker's actual model (judge cross-family
    // selection, the capability clamp's "rides the worker" fallback below).
    // `Role::Plan` is pinned alongside `Role::Worker` to the same model —
    // unpinned, it shares the worker's tier (`resolve_tier` treats
    // `Worker`/`Plan` identically), so leaving it out would silently revert
    // plan/witness turns to the session default the moment the worker is
    // overridden, defeating "plan rides the worker" (`pipeline_engine_config_for`'s
    // doc comment).
    let worker_spec = model_spec_for(&engine, EngineAgentKind::Worker, &is_provider);
    let effective_worker_ref = match &worker_spec {
        Some(spec) => pin_role(
            &mut wiring,
            &[Role::Worker, Role::Plan],
            "worker",
            spec,
            worker_ref,
            configured,
            &format!("rides the session default (`{worker_ref}`)"),
        )
        .unwrap_or_else(|| worker_ref.clone()),
        None => worker_ref.clone(),
    };
    wiring.worker_model = effective_worker_ref.clone();

    let triage_tuning = tuning_for(&engine, EngineAgentKind::Triage);
    let judge_tuning = tuning_for(&engine, EngineAgentKind::Judge);
    wiring.role_overrides.triage = stella_pipeline::RoleCallOverrides {
        prompt: triage_tuning.prompt,
        effort: triage_tuning.effort,
        reasoning: triage_tuning.reasoning,
        temperature: triage_tuning.temperature,
        max_output_tokens: triage_tuning.max_output_tokens,
        params: triage_tuning.params,
    };
    wiring.role_overrides.judge = stella_pipeline::RoleCallOverrides {
        prompt: judge_tuning.prompt,
        effort: judge_tuning.effort,
        reasoning: judge_tuning.reasoning,
        temperature: judge_tuning.temperature,
        max_output_tokens: judge_tuning.max_output_tokens,
        params: judge_tuning.params,
    };

    // The judge's cross-family preference must compare against the model the
    // worker ACTUALLY resolves to — comparing against the stale session
    // default here would let auto-mode pick a judge that turns out to share
    // the overridden worker's family (or vice versa), defeating the
    // bias-resistance the family comparison exists for.
    let worker_family = spec_family(&ModelSpec {
        provider: effective_worker_ref.provider.clone(),
        model: effective_worker_ref.model_id.clone(),
    });
    let judge_spec = if engine.auto_mode_on() {
        auto_judge_spec(&engine, &worker_family, &is_provider)
            .or_else(|| model_spec_for(&engine, EngineAgentKind::Judge, &is_provider))
    } else {
        model_spec_for(&engine, EngineAgentKind::Judge, &is_provider)
    };
    let triage_spec = model_spec_for(&engine, EngineAgentKind::Triage, &is_provider);

    // Capability clamp, mirroring `tuned_engine_config`: a role whose
    // model (pinned, provider-default, or riding the worker) is a
    // catalog-confirmed non-reasoning model must not carry effort or
    // reasoning onto the wire. Unknown capability passes through. "Riding
    // the worker" means the ACTUAL (possibly overridden) worker model.
    {
        let clamp = |overrides: &mut stella_pipeline::RoleCallOverrides,
                     spec: Option<&ModelSpec>| {
            let resolved: Option<(String, String)> = match spec {
                Some(s) if !s.model.is_empty() => Some((s.provider.clone(), s.model.clone())),
                // Provider pin without a model → the provider's default.
                Some(s) => crate::config::PROVIDERS
                    .iter()
                    .find(|p| p.id == s.provider && !p.default_model.is_empty())
                    .map(|p| (s.provider.clone(), p.default_model.to_string())),
                None => Some((
                    effective_worker_ref.provider.clone(),
                    effective_worker_ref.model_id.clone(),
                )),
            };
            if let Some((provider, model)) = resolved
                && crate::engine_config::model_supports_reasoning(&provider, &model) == Some(false)
            {
                overrides.effort = None;
                overrides.reasoning = None;
            }
        };
        clamp(&mut wiring.role_overrides.triage, triage_spec.as_ref());
        clamp(&mut wiring.role_overrides.judge, judge_spec.as_ref());
    }

    let role_specs = [
        (Role::Triage, "triage", triage_spec),
        (Role::Judge, "judge", judge_spec),
    ];

    for (role, label, spec) in role_specs {
        let Some(spec) = spec else { continue };
        pin_role(
            &mut wiring,
            &[role],
            label,
            &spec,
            worker_ref,
            configured,
            "rides the worker",
        );
    }
    wiring
}

/// Maps each pinned [`ModelRef`] to its adapter: the primary (worker)
/// provider plus the wiring's extra per-role adapters. The worker entry is
/// borrowed (the caller owns it — boxed in one-shot, `&dyn` in the deck
/// and goal paths); the extras are borrowed from the [`EngineWiring`].
pub(crate) struct RoleProviderResolver<'p> {
    primary: &'p dyn Provider,
    primary_ref: ModelRef,
    extra: &'p [(ModelRef, Box<dyn Provider>)],
}

impl<'p> RoleProviderResolver<'p> {
    pub(crate) fn new(
        primary: &'p dyn Provider,
        primary_ref: ModelRef,
        extra: &'p [(ModelRef, Box<dyn Provider>)],
    ) -> Self {
        Self {
            primary,
            primary_ref,
            extra,
        }
    }
}

impl ProviderResolver for RoleProviderResolver<'_> {
    fn provider_for(&self, model: &ModelRef) -> Option<&dyn Provider> {
        if *model == self.primary_ref {
            return Some(self.primary);
        }
        self.extra
            .iter()
            .find(|(model_ref, _)| model_ref == model)
            .map(|(_, provider)| &**provider)
    }
}

pub(crate) fn build_provider(cfg: &Config) -> Result<Box<dyn Provider>, String> {
    build_provider_parts(
        &cfg.provider,
        &cfg.model_id,
        // `cfg.api_key` is already an `ApiKey` (H3) — clone it rather than
        // reconstructing one from a revealed string.
        cfg.api_key.clone(),
        cfg.effective_base_url().to_string(),
        cfg.base_url_override.as_deref(),
    )
}

/// The per-dialect provider factory, over already-resolved parts rather than
/// a whole [`Config`]. Both the worker path ([`build_provider`]) and the
/// goal loop's routed judge ([`resolve_cross_family_judge`]) go through this
/// one match, so the wire-dialect selection — and the anti-phantom-slug
/// catalog check — live in exactly one place. `effective_base_url` is the
/// base URL requests go to (override-or-default); `base_url_override` is the
/// raw `--base-url`, which only the Vertex/Bedrock arms consume (they build
/// region/project-scoped URLs themselves). See [`build_provider`]'s note on
/// the catalog check and the shared Chat Completions arm.
fn build_provider_parts(
    provider_config: &crate::config::ProviderConfig,
    model_id: &str,
    api_key: ApiKey,
    effective_base_url: String,
    base_url_override: Option<&str>,
) -> Result<Box<dyn Provider>, String> {
    use crate::config::Dialect;

    let provider_id = provider_config.id;
    let display_name = provider_config.display_name;
    // The anti-invalid-slug gate, for EVERY provider (not just seeded
    // ones): the seed floor always passes; a provider whose master-list
    // rows are synced (`stella models refresh`) gets hard validation with
    // suggestions; `local` and never-synced custom endpoints keep their
    // endpoint-is-the-authority posture. See
    // `crate::model_catalog::validate_model_slug` for the full ladder.
    crate::model_catalog::validate_model_slug(provider_config, model_id)?;

    match provider_config.dialect {
        Dialect::OpenaiResponses => {
            let provider = stella_model::openai::OpenAiProvider::new(api_key, model_id.to_string())
                .with_base_url(effective_base_url);
            Ok(Box::new(provider))
        }
        Dialect::Anthropic => {
            let provider =
                stella_model::anthropic::AnthropicProvider::new(api_key, model_id.to_string())
                    .with_base_url(effective_base_url);
            Ok(Box::new(provider))
        }
        Dialect::Gemini => {
            let provider = stella_model::gemini::GeminiProvider::new(api_key, model_id.to_string())
                .with_base_url(effective_base_url);
            Ok(Box::new(provider))
        }
        Dialect::Vertex => {
            // The access token is `api_key` (VERTEX_ACCESS_TOKEN via the
            // credential chain); project and location are Vertex-specific
            // addressing, resolved here with named errors rather than
            // burying a doomed request.
            let project = std::env::var("VERTEX_PROJECT_ID")
                .or_else(|_| std::env::var("GOOGLE_CLOUD_PROJECT"))
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    "Vertex AI needs a project id — set VERTEX_PROJECT_ID (or \
                     GOOGLE_CLOUD_PROJECT)"
                        .to_string()
                })?;
            let location = std::env::var("VERTEX_LOCATION")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "global".to_string());
            let mut provider = stella_model::vertex::VertexProvider::new(
                api_key,
                model_id.to_string(),
                project,
                location,
            );
            if let Some(override_url) = base_url_override {
                provider = provider.with_base_url(override_url.to_string());
            }
            Ok(Box::new(provider))
        }
        Dialect::Bedrock => {
            // `api_key` is AWS_ACCESS_KEY_ID via the credential chain; the
            // rest of the standard AWS env set is read here. Secret
            // resolution failure is a named error pointing at the exact
            // var, not a doomed unsigned request.
            let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    "Bedrock needs AWS_SECRET_ACCESS_KEY alongside AWS_ACCESS_KEY_ID".to_string()
                })?;
            let session_token = std::env::var("AWS_SESSION_TOKEN")
                .ok()
                .filter(|v| !v.is_empty())
                .map(ApiKey::new);
            let region = std::env::var("AWS_REGION")
                .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "us-east-1".to_string());
            let mut provider = stella_model::bedrock::BedrockProvider::new(
                api_key,
                ApiKey::new(secret),
                session_token,
                region,
                model_id.to_string(),
            );
            if let Some(override_url) = base_url_override {
                provider = provider.with_base_url(override_url.to_string());
            }
            Ok(Box::new(provider))
        }
        // Z.ai, xAI, DeepSeek, OpenRouter, local, and config-defined
        // providers (settings.json) — the shared Chat Completions adapter,
        // re-identified per provider so its `Provider::id()` and error
        // messages name the surface actually being called.
        Dialect::OpenaiCompatible => {
            let label = match provider_id {
                "zai" => "Z.ai",
                "xai" => "xAI",
                "deepseek" => "DeepSeek",
                "openrouter" => "OpenRouter",
                "local" => "the local endpoint",
                _ => display_name,
            };
            let mut provider = stella_model::zai::ZaiProvider::new(api_key, model_id.to_string())
                .with_base_url(effective_base_url)
                .with_identity(provider_id, label);
            if provider_id == "openrouter" {
                // First-class OpenRouter: app attribution on every request,
                // and the gateway's own usage accounting so
                // `CompletionResult::cost_usd` is the routed call's real
                // price (its slugs are unseeded — see config.rs — so there
                // is no catalog list price to fall back on).
                provider = provider
                    .with_attribution("https://stella.oxagen.sh", "Stella")
                    .with_usage_accounting();
            }
            Ok(Box::new(provider))
        }
    }
}

/// Cross-family grouping key for judge selection. Same-vendor providers must
/// count as the SAME family so a routed judge is genuinely a different model
/// : a Gemini judge assessing Gemini-via-Vertex work
/// carries the same bias, as does an Anthropic Claude judge over Bedrock
/// Claude. Anything without a known sibling is its own family (its id).
pub(crate) fn provider_family(provider_id: &str) -> String {
    match provider_id {
        "gemini" | "vertex" => "google".to_string(),
        "anthropic" | "bedrock" => "anthropic".to_string(),
        other => other.to_string(),
    }
}

/// A `ProviderProfile` for a discovered provider, using its `default_model`
/// for all three role tiers (the finest model this layer knows without a
/// per-role catalog) and [`provider_family`] for cross-family grouping.
fn profile_for(config: &crate::config::ProviderConfig) -> ProviderProfile {
    let model = ModelRef::new(config.id, config.default_model);
    ProviderProfile::new(config.id, model.clone(), model.clone(), model)
        .with_family(provider_family(config.id))
}

/// Resolve the JUDGE role for the goal loop. Builds a role [`Router`] whose
/// most-preferred provider is the active worker (`worker_id`/`worker_model`,
/// so the `--model` pin is honored) followed by every OTHER configured
/// provider, then resolves `Role::Judge`. The router prefers a healthy
/// provider whose family differs from the worker's (`resolve_judge`), so:
///
/// - Only the worker's family configured → the router degrades to the worker
///   provider; `model_ref.provider == worker_id`, so we return `None` and no
///   second provider is built (behavior identical to before).
/// - A distinct family is selected → the concrete `ModelRef` is returned.
///
/// Returns `None` (→ caller reuses the worker as judge) on ANY failure —
/// same-family degradation, a resolve error, an unknown judge provider, or a
/// judge-adapter build failure — so judge routing can never break the loop.
/// On success returns the built judge provider and its id (for the notice).
pub(crate) fn resolve_cross_family_judge(
    worker_id: &str,
    worker_model: &str,
    configured: &[crate::config::ConfiguredProvider],
) -> Option<(Box<dyn Provider>, String)> {
    let worker_ref = ModelRef::new(worker_id, worker_model);
    let worker_profile = ProviderProfile::new(
        worker_id,
        worker_ref.clone(),
        worker_ref.clone(),
        worker_ref,
    )
    .with_family(provider_family(worker_id));

    let mut profiles = vec![worker_profile];
    for entry in configured {
        if entry.config.id == worker_id {
            continue; // the worker is already the preferred profile
        }
        profiles.push(profile_for(&entry.config));
    }

    let router = Router::new(
        RoleTable::new(),
        profiles,
        CircuitBreaker::new(Box::new(SystemClock::new())),
    );
    let decision = router.resolve(Role::Judge).ok()?;

    // Same provider as the worker → single-family/degraded: reuse the worker
    // provider directly, never build a duplicate.
    if decision.model_ref.provider == worker_id {
        return None;
    }

    // Build the concrete judge from the discovered credential for the chosen
    // provider. A missing entry or a build error falls back to the worker.
    let entry = configured
        .iter()
        .find(|c| c.config.id == decision.model_ref.provider)?;
    let judge = build_provider_parts(
        &entry.config,
        &decision.model_ref.model_id,
        entry.api_key.clone(),
        entry.config.base_url.to_string(),
        None,
    )
    .ok()?;
    Some((judge, decision.model_ref.provider))
}
