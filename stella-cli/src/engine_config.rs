//! `agent_engine_config` resolution — turns the declarative settings
//! object ([`crate::settings::AgentEngineConfig`]) into concrete per-agent
//! tuning: which provider/model each engine agent runs on, its effective
//! prompt/effort/reasoning, and the request parameter overrides.
//!
//! Three consumers:
//! - `agent.rs` builds role routers and engine configs from
//!   [`AgentTuning`] / [`ModelSpec`] when a run starts;
//! - `config.rs` consults [`AgentEngineConfig::model_for`] (via a combined
//!   spec string) for the default agent's model precedence;
//! - `command_deck.rs` maps settings ↔ the TUI's `EngineConfigState`
//!   snapshot for the `/engine` overlay.
//!
//! Everything here is pure functions over owned data (no I/O), so the
//! precedence and auto-mode rules are unit-testable without a workspace.

use stella_protocol::{GenerationParams, ReasoningEffort, ServiceTier, Verbosity};
use stella_tui::{EngineAgentState, EngineConfigState, EngineRole};

use crate::settings::{
    AgentEngineAgent, AgentEngineAgents, AgentEngineConfig, AgentEngineParams, EngineAgentKind,
    Toggle,
};

// ---------------------------------------------------------------------
// Model specs
// ---------------------------------------------------------------------

/// A parsed model setting: which provider serves it (when known) and the
/// provider-native slug sent verbatim on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    /// Provider id (`"anthropic"`, `"openrouter"`, a settings-defined id).
    pub provider: String,
    /// Provider-native model slug (`"claude-fable-5"`, `"openai/gpt-5.5"`).
    pub model: String,
}

/// Parse one model string into a [`ModelSpec`], `--model` semantics:
/// `provider/slug` splits on the FIRST `/` when the prefix names a known
/// provider (so `openrouter/openai/gpt-5.5` routes `openai/gpt-5.5`
/// through OpenRouter); a bare slug resolves through the seed catalog
/// (`glm-5.2` → `zai`). `None` when neither matches — the caller decides
/// whether that's a loud error (an explicit setting) or a skip (an
/// `allowed_models` candidate).
pub fn parse_model_spec(raw: &str, is_provider: &dyn Fn(&str) -> bool) -> Option<ModelSpec> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some((prefix, rest)) = raw.split_once('/')
        && !rest.is_empty()
        && is_provider(prefix)
    {
        return Some(ModelSpec {
            provider: prefix.to_string(),
            model: rest.to_string(),
        });
    }
    stella_model::catalog::Catalog::seed()
        .resolve(raw)
        .ok()
        .map(|entry| ModelSpec {
            provider: entry.provider.to_string(),
            model: raw.to_string(),
        })
}

/// The effective [`ModelSpec`] for `kind`, honoring the agent's explicit
/// `provider` field: when set, the model string (from the same precedence
/// chain as [`AgentEngineConfig::model_for`]) is the verbatim slug for
/// THAT provider — no prefix splitting, which is what lets an OpenRouter
/// entry carry a slug that itself contains `/`.
///
/// One exception guards the pin: the flat model keys (`default_model`,
/// `pipeline_*_model`) hold `provider/slug` specs — the TUI writes them
/// that way on every save — and the same precedence chain hands them to a
/// pinned agent. For a SEEDED pin the catalog arbitrates the two readings:
/// when it rejects the verbatim one and the string parses as a qualified
/// spec, the spec's own provider wins (a stale `zai` pin over
/// `openrouter/openrouter/auto` must route to OpenRouter, not become the
/// phantom slug `zai/openrouter/openrouter/auto`). Unseeded pins
/// (openrouter, local, settings-defined) keep verbatim semantics — their
/// slug space is open, so the catalog has no veto.
pub fn model_spec_for(
    engine: &AgentEngineConfig,
    kind: EngineAgentKind,
    is_provider: &dyn Fn(&str) -> bool,
) -> Option<ModelSpec> {
    let pinned_provider = engine
        .agent(kind)
        .and_then(|a| a.provider.as_deref())
        .filter(|p| !p.trim().is_empty());
    let model = engine.model_for(kind);
    match (pinned_provider, model) {
        (Some(provider), Some(model)) => {
            let pin_seeded = crate::config::PROVIDERS
                .iter()
                .any(|p| p.id == provider && p.seeded);
            if pin_seeded
                && stella_model::catalog::Catalog::seed()
                    .resolve_for(provider, model)
                    .is_err()
                && let Some(spec) = parse_model_spec(model, is_provider)
            {
                return Some(spec);
            }
            Some(ModelSpec {
                provider: provider.to_string(),
                model: model.to_string(),
            })
        }
        // A provider pin with no model: the provider's own default model
        // applies — signalled with an empty slug the wiring layer fills
        // from `ProviderConfig::default_model`.
        (Some(provider), None) => Some(ModelSpec {
            provider: provider.to_string(),
            model: String::new(),
        }),
        (None, Some(model)) => parse_model_spec(model, is_provider),
        (None, None) => None,
    }
}

/// Cross-family grouping key for judge diversity. Same-vendor providers
/// count as ONE family (a Gemini judge over Vertex-served Gemini work
/// carries the same bias), and an OpenRouter spec's family is the routed
/// slug's vendor prefix — `openrouter/openai/gpt-5.5` IS an OpenAI model,
/// and treating it as family "openrouter" would defeat the judge's
/// cross-family preference.
pub fn spec_family(spec: &ModelSpec) -> String {
    match spec.provider.as_str() {
        "gemini" | "vertex" => "google".to_string(),
        "anthropic" | "bedrock" => "anthropic".to_string(),
        "openrouter" => spec
            .model
            .split_once('/')
            .map(|(vendor, _)| vendor.to_string())
            .unwrap_or_else(|| "openrouter".to_string()),
        other => other.to_string(),
    }
}

/// `auto_mode: on` — pick the judge from `allowed_models`, preferring a
/// family different from the worker's, then ranking by the seed catalog's
/// output list price (the closest objective capability proxy the catalog
/// carries; gateway-priced entries rank last at 0), then list order.
/// Candidates whose provider has no resolvable credential are skipped —
/// `configured` is the filter. `None` when nothing in the list is usable
/// (the caller falls back to the normal judge routing).
pub fn auto_judge_spec(
    engine: &AgentEngineConfig,
    worker_family: &str,
    configured: &dyn Fn(&str) -> bool,
) -> Option<ModelSpec> {
    let catalog = stella_model::catalog::Catalog::seed();
    let mut best: Option<(bool, f64, usize, ModelSpec)> = None;
    for (index, raw) in engine.allowed_models().iter().enumerate() {
        let Some(spec) = parse_model_spec(raw, &|id| configured(id)) else {
            continue;
        };
        if !configured(&spec.provider) {
            continue;
        }
        let cross_family = spec_family(&spec) != worker_family;
        let price = catalog
            .resolve_for(&spec.provider, &spec.model)
            .map(|entry| entry.pricing.output_usd_per_mtok)
            .unwrap_or(0.0);
        // Lexicographic max over (cross_family, price, earliest-in-list).
        let better = match &best {
            None => true,
            Some((bf, bp, bi, _)) => {
                (cross_family, price, std::cmp::Reverse(index)) > (*bf, *bp, std::cmp::Reverse(*bi))
            }
        };
        if better {
            best = Some((cross_family, price, index, spec));
        }
    }
    best.map(|(_, _, _, spec)| spec)
}

// ---------------------------------------------------------------------
// Per-agent tuning
// ---------------------------------------------------------------------

/// The resolved request-shaping settings for one agent: what lands on the
/// engine config / completion requests once the auto modes are applied.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentTuning {
    /// Custom system prompt (replaces the built-in base; memories/rules
    /// still append).
    pub prompt: Option<String>,
    pub effort: Option<ReasoningEffort>,
    pub reasoning: Option<bool>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub params: Option<GenerationParams>,
}

/// `effort_auto: on` — the per-agent effort nobody has to think about:
/// the judge deliberates hard, the worker balances cost and quality, and
/// triage (a one-token classification under a latency ceiling) stays low.
fn auto_effort(kind: EngineAgentKind) -> ReasoningEffort {
    match kind {
        EngineAgentKind::Judge => ReasoningEffort::High,
        EngineAgentKind::Triage => ReasoningEffort::Low,
        EngineAgentKind::Default | EngineAgentKind::Worker => ReasoningEffort::Medium,
    }
}

/// `reasoning_auto: on` — thinking on wherever deliberation pays (judge,
/// worker, default), off for triage (latency-bound, one bare token out).
fn auto_reasoning(kind: EngineAgentKind) -> bool {
    !matches!(kind, EngineAgentKind::Triage)
}

/// Resolve `kind`'s tuning from the merged config, applying the auto
/// modes (`effort_auto`/`reasoning_auto` override the per-agent settings
/// when on — that is their contract: "you never worry about it").
pub fn tuning_for(engine: &AgentEngineConfig, kind: EngineAgentKind) -> AgentTuning {
    let agent = engine.agent(kind);
    let effort = if engine.effort_auto_on() {
        Some(auto_effort(kind))
    } else {
        agent.and_then(|a| a.effort)
    };
    let reasoning = if engine.reasoning_auto_on() {
        Some(auto_reasoning(kind))
    } else {
        agent.and_then(|a| a.reasoning).map(Toggle::is_on)
    };
    let (temperature, max_output_tokens, params) = agent
        .and_then(|a| a.params.as_ref())
        .map(split_params)
        .unwrap_or((None, None, None));
    AgentTuning {
        prompt: agent.and_then(|a| a.prompt.clone()),
        effort,
        reasoning,
        temperature,
        max_output_tokens,
        params,
    }
}

/// Split the settings-level params into the request's direct fields
/// (`temperature`, `max_output_tokens`) and the [`GenerationParams`]
/// rider (`Some` only when at least one field is set, so an all-default
/// params object keeps the wire body byte-identical to no params at all).
fn split_params(
    params: &AgentEngineParams,
) -> (Option<f32>, Option<u32>, Option<GenerationParams>) {
    let rider = GenerationParams {
        top_p: params.top_p,
        top_k: params.top_k,
        frequency_penalty: params.frequency_penalty,
        presence_penalty: params.presence_penalty,
        repetition_penalty: params.repetition_penalty,
        seed: params.seed,
        verbosity: params.verbosity,
        service_tier: params.service_tier,
    };
    let rider = (rider != GenerationParams::default()).then_some(rider);
    (params.temperature, params.max_tokens, rider)
}

// ---------------------------------------------------------------------
// Enum ↔ string (the TUI edits strings; settings/serde hold enums)
// ---------------------------------------------------------------------

pub fn effort_to_str(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max => "max",
    }
}

pub fn effort_from_str(raw: &str) -> Option<ReasoningEffort> {
    match raw {
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::Xhigh),
        "max" => Some(ReasoningEffort::Max),
        _ => None,
    }
}

pub fn verbosity_to_str(verbosity: Verbosity) -> &'static str {
    match verbosity {
        Verbosity::Low => "low",
        Verbosity::Medium => "medium",
        Verbosity::High => "high",
    }
}

pub fn verbosity_from_str(raw: &str) -> Option<Verbosity> {
    match raw {
        "low" => Some(Verbosity::Low),
        "medium" => Some(Verbosity::Medium),
        "high" => Some(Verbosity::High),
        _ => None,
    }
}

pub fn service_tier_to_str(tier: ServiceTier) -> &'static str {
    match tier {
        ServiceTier::Auto => "auto",
        ServiceTier::Default => "default",
        ServiceTier::Flex => "flex",
        ServiceTier::Priority => "priority",
    }
}

pub fn service_tier_from_str(raw: &str) -> Option<ServiceTier> {
    match raw {
        "auto" => Some(ServiceTier::Auto),
        "default" => Some(ServiceTier::Default),
        "flex" => Some(ServiceTier::Flex),
        "priority" => Some(ServiceTier::Priority),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Settings ↔ TUI snapshot
// ---------------------------------------------------------------------

/// The TUI's role vocabulary mapped onto the settings' — same order, so
/// `EngineConfigState::agents[i]` is `EngineAgentKind::ALL[i]`.
fn kind_for(role: EngineRole) -> EngineAgentKind {
    match role {
        EngineRole::Default => EngineAgentKind::Default,
        EngineRole::Worker => EngineAgentKind::Worker,
        EngineRole::Judge => EngineAgentKind::Judge,
        EngineRole::Triage => EngineAgentKind::Triage,
    }
}

/// The flat per-role model field (no fallback to `default_model` — the
/// snapshot shows what THIS row sets, not what it inherits).
fn flat_model(engine: &AgentEngineConfig, kind: EngineAgentKind) -> Option<&str> {
    match kind {
        EngineAgentKind::Default => engine.default_model.as_deref(),
        EngineAgentKind::Worker => engine.pipeline_worker_model.as_deref(),
        EngineAgentKind::Judge => engine.pipeline_judge_model.as_deref(),
        EngineAgentKind::Triage => engine.pipeline_triage_model.as_deref(),
    }
}

/// Build the `/engine` overlay's snapshot from the merged settings plus
/// the picker vocabularies the driver knows (provider ids, catalog slugs).
pub fn state_from_settings(
    engine: &AgentEngineConfig,
    providers: Vec<String>,
    catalog_models: Vec<String>,
) -> EngineConfigState {
    let agents = EngineRole::ALL
        .iter()
        .map(|role| {
            let kind = kind_for(*role);
            let agent = engine.agent(kind);
            let params = agent.and_then(|a| a.params.as_ref());
            EngineAgentState {
                model: agent
                    .and_then(|a| a.model.clone())
                    .or_else(|| flat_model(engine, kind).map(str::to_string)),
                provider: agent.and_then(|a| a.provider.clone()),
                prompt: agent.and_then(|a| a.prompt.clone()),
                effort: agent
                    .and_then(|a| a.effort)
                    .map(|e| effort_to_str(e).to_string()),
                reasoning: agent.and_then(|a| a.reasoning).map(Toggle::is_on),
                temperature: params.and_then(|p| p.temperature),
                top_p: params.and_then(|p| p.top_p),
                top_k: params.and_then(|p| p.top_k),
                frequency_penalty: params.and_then(|p| p.frequency_penalty),
                presence_penalty: params.and_then(|p| p.presence_penalty),
                repetition_penalty: params.and_then(|p| p.repetition_penalty),
                max_tokens: params.and_then(|p| p.max_tokens),
                seed: params.and_then(|p| p.seed),
                verbosity: params
                    .and_then(|p| p.verbosity)
                    .map(|v| verbosity_to_str(v).to_string()),
                service_tier: params
                    .and_then(|p| p.service_tier)
                    .map(|t| service_tier_to_str(t).to_string()),
            }
        })
        .collect();
    EngineConfigState {
        auto_mode: engine.auto_mode_on(),
        effort_auto: engine.effort_auto_on(),
        reasoning_auto: engine.reasoning_auto_on(),
        allowed_models: engine.allowed_models().to_vec(),
        providers,
        catalog_models,
        agents,
    }
}

/// Rebuild the settings object from an edited snapshot. Models land on
/// the FLAT per-role keys (`default_model`, `pipeline_*_model`) — the
/// canonical simple form — with `agents.<k>.model` cleared, so a TUI save
/// never leaves the two sources disagreeing. Empty agent objects and an
/// empty allowed list are omitted entirely (the file stays minimal).
pub fn settings_from_state(state: &EngineConfigState) -> AgentEngineConfig {
    let mut engine = AgentEngineConfig {
        auto_mode: Some(if state.auto_mode {
            Toggle::On
        } else {
            Toggle::Off
        }),
        effort_auto: Some(if state.effort_auto {
            Toggle::On
        } else {
            Toggle::Off
        }),
        reasoning_auto: Some(if state.reasoning_auto {
            Toggle::On
        } else {
            Toggle::Off
        }),
        allowed_models: (!state.allowed_models.is_empty()).then(|| state.allowed_models.clone()),
        ..AgentEngineConfig::default()
    };
    let mut agents = AgentEngineAgents::default();
    let mut any_agent = false;
    for (index, role) in EngineRole::ALL.iter().enumerate() {
        let kind = kind_for(*role);
        let Some(edited) = state.agents.get(index) else {
            continue;
        };
        let model = edited
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        match kind {
            EngineAgentKind::Default => engine.default_model = model,
            EngineAgentKind::Worker => engine.pipeline_worker_model = model,
            EngineAgentKind::Judge => engine.pipeline_judge_model = model,
            EngineAgentKind::Triage => engine.pipeline_triage_model = model,
        }
        let params = AgentEngineParams {
            temperature: edited.temperature,
            top_p: edited.top_p,
            top_k: edited.top_k,
            frequency_penalty: edited.frequency_penalty,
            presence_penalty: edited.presence_penalty,
            repetition_penalty: edited.repetition_penalty,
            max_tokens: edited.max_tokens,
            seed: edited.seed,
            verbosity: edited.verbosity.as_deref().and_then(verbosity_from_str),
            service_tier: edited
                .service_tier
                .as_deref()
                .and_then(service_tier_from_str),
        };
        let agent = AgentEngineAgent {
            model: None,
            provider: edited
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string),
            prompt: edited.prompt.clone().filter(|p| !p.trim().is_empty()),
            effort: edited.effort.as_deref().and_then(effort_from_str),
            reasoning: edited
                .reasoning
                .map(|on| if on { Toggle::On } else { Toggle::Off }),
            params: (params != AgentEngineParams::default()).then_some(params),
        };
        if agent != AgentEngineAgent::default() {
            *agents.get_mut(kind) = Some(agent);
            any_agent = true;
        }
    }
    engine.agents = any_agent.then_some(agents);
    engine
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_builtin(id: &str) -> bool {
        crate::config::PROVIDERS.iter().any(|p| p.id == id)
    }

    fn engine_from_json(json: &str) -> AgentEngineConfig {
        serde_json::from_str(json).expect("valid engine config json")
    }

    #[test]
    fn parse_model_spec_splits_provider_prefixes_and_resolves_bare_slugs() {
        // provider/slug — including a routed slug that itself contains `/`.
        let spec = parse_model_spec("openrouter/openai/gpt-5.5", &is_builtin).unwrap();
        assert_eq!(spec.provider, "openrouter");
        assert_eq!(spec.model, "openai/gpt-5.5");
        // A bare seeded slug resolves through the catalog.
        let spec = parse_model_spec("glm-5.2", &is_builtin).unwrap();
        assert_eq!(spec.provider, "zai");
        // Unknown either way → None.
        assert!(parse_model_spec("no-such-model", &is_builtin).is_none());
        assert!(parse_model_spec("", &is_builtin).is_none());
    }

    #[test]
    fn model_spec_honors_the_explicit_provider_pin_verbatim() {
        let engine = engine_from_json(
            r#"{"agents": {"judge": {"provider": "openrouter", "model": "openai/gpt-5.5"}}}"#,
        );
        let spec = model_spec_for(&engine, EngineAgentKind::Judge, &is_builtin).unwrap();
        // No prefix splitting: the slug goes to the pinned provider whole.
        assert_eq!(spec.provider, "openrouter");
        assert_eq!(spec.model, "openai/gpt-5.5");

        // A provider pin with no model → empty slug (provider default).
        let engine = engine_from_json(r#"{"agents": {"triage": {"provider": "zai"}}}"#);
        let spec = model_spec_for(&engine, EngineAgentKind::Triage, &is_builtin).unwrap();
        assert_eq!(spec.provider, "zai");
        assert!(spec.model.is_empty());
    }

    #[test]
    fn seeded_pin_yields_to_a_qualified_spec_the_catalog_rejects() {
        // The flat keys hold `provider/slug` specs (the TUI writes them
        // that way), and the precedence chain hands them to pinned agents
        // too. A stale seeded pin must not swallow one into a phantom slug:
        // `zai` + `openrouter/openrouter/auto` routed verbatim produced the
        // unknown-model error `zai/openrouter/openrouter/auto`.
        let engine = engine_from_json(
            r#"{"default_model": "openrouter/openrouter/auto",
                "agents": {"default": {"provider": "zai"}}}"#,
        );
        let spec = model_spec_for(&engine, EngineAgentKind::Default, &is_builtin).unwrap();
        assert_eq!(spec.provider, "openrouter");
        assert_eq!(spec.model, "openrouter/auto");

        // The TUI-save shape — pin plus a flat spec naming the SAME seeded
        // provider — resolves to the catalog slug, not `provider/provider/…`.
        let engine = engine_from_json(
            r#"{"pipeline_judge_model": "anthropic/claude-fable-5",
                "agents": {"judge": {"provider": "anthropic"}}}"#,
        );
        let spec = model_spec_for(&engine, EngineAgentKind::Judge, &is_builtin).unwrap();
        assert_eq!(spec.provider, "anthropic");
        assert_eq!(spec.model, "claude-fable-5");

        // A seeded pin with a catalog-valid slug is untouched.
        let engine = engine_from_json(
            r#"{"default_model": "glm-5.2", "agents": {"default": {"provider": "zai"}}}"#,
        );
        let spec = model_spec_for(&engine, EngineAgentKind::Default, &is_builtin).unwrap();
        assert_eq!(spec.provider, "zai");
        assert_eq!(spec.model, "glm-5.2");

        // An UNSEEDED pin keeps verbatim semantics even for a flat-key
        // model whose prefix names another provider — that slug shape is
        // exactly how OpenRouter routes (`openai/gpt-5.5` IS its slug).
        let engine = engine_from_json(
            r#"{"pipeline_judge_model": "openai/gpt-5.5",
                "agents": {"judge": {"provider": "openrouter"}}}"#,
        );
        let spec = model_spec_for(&engine, EngineAgentKind::Judge, &is_builtin).unwrap();
        assert_eq!(spec.provider, "openrouter");
        assert_eq!(spec.model, "openai/gpt-5.5");
    }

    #[test]
    fn spec_family_groups_vendors_and_unwraps_openrouter_slugs() {
        let family = |provider: &str, model: &str| {
            spec_family(&ModelSpec {
                provider: provider.into(),
                model: model.into(),
            })
        };
        assert_eq!(family("vertex", "gemini-3-pro"), "google");
        assert_eq!(family("bedrock", "anything"), "anthropic");
        assert_eq!(family("openrouter", "openai/gpt-5.5"), "openai");
        assert_eq!(family("zai", "glm-5.2"), "zai");
    }

    #[test]
    fn auto_judge_prefers_cross_family_then_price_then_list_order() {
        let engine = engine_from_json(
            r#"{"allowed_models": [
                "zai/glm-5.2",
                "deepseek/deepseek-chat",
                "anthropic/claude-fable-5"
            ], "auto_mode": "on"}"#,
        );
        // Worker family zai → cross-family candidates are deepseek (1.10
        // output) and anthropic (15.00 output): price ranks Anthropic first.
        let spec = auto_judge_spec(&engine, "zai", &|_| true).unwrap();
        assert_eq!(spec.provider, "anthropic");
        // Worker family anthropic → zai (2.20) beats deepseek (1.10).
        let spec = auto_judge_spec(&engine, "anthropic", &|_| true).unwrap();
        assert_eq!(spec.provider, "zai");
        // Credentials filter: with anthropic unavailable, deepseek wins for
        // a zai worker.
        let spec = auto_judge_spec(&engine, "zai", &|id| id != "anthropic").unwrap();
        assert_eq!(spec.provider, "deepseek");
        // Nothing configured → None (caller falls back to normal routing).
        assert!(auto_judge_spec(&engine, "zai", &|_| false).is_none());
    }

    #[test]
    fn tuning_applies_auto_modes_over_per_agent_settings() {
        let engine = engine_from_json(
            r#"{"effort_auto": "on", "reasoning_auto": "on",
                "agents": {"triage": {"effort": "max", "reasoning": "on"},
                            "judge": {"prompt": "Judge hard."}}}"#,
        );
        // Auto overrides the explicit triage settings (that's its contract).
        let triage = tuning_for(&engine, EngineAgentKind::Triage);
        assert_eq!(triage.effort, Some(ReasoningEffort::Low));
        assert_eq!(triage.reasoning, Some(false));
        let judge = tuning_for(&engine, EngineAgentKind::Judge);
        assert_eq!(judge.effort, Some(ReasoningEffort::High));
        assert_eq!(judge.reasoning, Some(true));
        // Prompts are never auto-managed.
        assert_eq!(judge.prompt.as_deref(), Some("Judge hard."));

        // Autos off → the per-agent settings answer.
        let engine =
            engine_from_json(r#"{"agents": {"triage": {"effort": "max", "reasoning": "off"}}}"#);
        let triage = tuning_for(&engine, EngineAgentKind::Triage);
        assert_eq!(triage.effort, Some(ReasoningEffort::Max));
        assert_eq!(triage.reasoning, Some(false));
        // And an untouched agent has no opinions at all.
        assert_eq!(
            tuning_for(&engine, EngineAgentKind::Worker),
            AgentTuning::default()
        );
    }

    #[test]
    fn tuning_splits_params_between_direct_fields_and_the_rider() {
        let engine = engine_from_json(
            r#"{"agents": {"worker": {"params": {
                "temperature": 0.3, "max_tokens": 9000, "top_p": 0.9, "seed": 42
            }}}}"#,
        );
        let worker = tuning_for(&engine, EngineAgentKind::Worker);
        assert_eq!(worker.temperature, Some(0.3));
        assert_eq!(worker.max_output_tokens, Some(9000));
        let rider = worker.params.expect("rider");
        assert_eq!(rider.top_p, Some(0.9));
        assert_eq!(rider.seed, Some(42));
        // temperature/max_tokens alone → no rider at all (byte-stable wire).
        let engine =
            engine_from_json(r#"{"agents": {"worker": {"params": {"temperature": 0.3}}}}"#);
        assert!(
            tuning_for(&engine, EngineAgentKind::Worker)
                .params
                .is_none()
        );
    }

    #[test]
    fn snapshot_roundtrips_through_the_tui_state() {
        let engine = engine_from_json(
            r#"{"default_model": "anthropic/claude-fable-5",
                "pipeline_judge_model": "openrouter/openai/gpt-5.5",
                "allowed_models": ["anthropic/claude-fable-5"],
                "auto_mode": "off", "effort_auto": "on", "reasoning_auto": "off",
                "agents": {"judge": {"provider": "openrouter", "effort": "high",
                                      "reasoning": "on",
                                      "params": {"temperature": 0.2, "top_k": 40,
                                                  "verbosity": "low",
                                                  "service_tier": "flex"}}}}"#,
        );
        let state = state_from_settings(&engine, vec!["zai".into()], vec!["zai/glm-5.2".into()]);
        assert!(!state.auto_mode);
        assert!(state.effort_auto);
        let judge = state.agent(EngineRole::Judge).expect("judge state");
        assert_eq!(judge.model.as_deref(), Some("openrouter/openai/gpt-5.5"));
        assert_eq!(judge.provider.as_deref(), Some("openrouter"));
        assert_eq!(judge.effort.as_deref(), Some("high"));
        assert_eq!(judge.reasoning, Some(true));
        assert_eq!(judge.top_k, Some(40));
        assert_eq!(judge.verbosity.as_deref(), Some("low"));
        assert_eq!(judge.service_tier.as_deref(), Some("flex"));

        let back = settings_from_state(&state);
        // Models land on the flat keys, agents.model stays clear.
        assert_eq!(
            back.pipeline_judge_model.as_deref(),
            Some("openrouter/openai/gpt-5.5")
        );
        assert_eq!(
            back.default_model.as_deref(),
            Some("anthropic/claude-fable-5")
        );
        let judge = back.agent(EngineAgentKind::Judge).expect("judge");
        assert!(judge.model.is_none());
        assert_eq!(judge.provider.as_deref(), Some("openrouter"));
        assert_eq!(judge.effort, Some(ReasoningEffort::High));
        assert_eq!(judge.reasoning, Some(Toggle::On));
        let params = judge.params.expect("params");
        assert_eq!(params.temperature, Some(0.2));
        assert_eq!(params.service_tier, Some(ServiceTier::Flex));
        // The toggles are written explicitly (an "off" the user chose is
        // preserved, not dropped).
        assert_eq!(back.auto_mode, Some(Toggle::Off));
        assert_eq!(back.effort_auto, Some(Toggle::On));
        // The round trip is stable: state → settings → state is identity
        // for the fields the snapshot carries.
        let state2 = state_from_settings(&back, vec!["zai".into()], vec!["zai/glm-5.2".into()]);
        assert_eq!(state2.agents, state.agents);
        assert_eq!(state2.allowed_models, state.allowed_models);
    }
}
