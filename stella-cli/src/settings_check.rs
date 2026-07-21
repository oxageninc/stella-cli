//! Launch-time settings validation — correctness checks run once, right
//! after the config resolves and before the first turn, so a mis-formed
//! setting is a clear warning at startup instead of a provider `400` on the
//! first model call.
//!
//! Today the focus is **model slugs**: the class of bug where a settings
//! entry names a model the provider can't serve — an unknown provider, a
//! typo'd slug, or a provider-qualification that resolves to the wrong wire
//! id (e.g. an OpenRouter slug that ends up double-prefixed as
//! `openrouter/openrouter/auto`, which OpenRouter rejects). Each configured
//! reference is resolved to the exact WIRE slug the engine would send and
//! validated against the catalog ([`crate::model_catalog::validate_model_slug`]),
//! so the check sees precisely what the provider will see.
//!
//! Warnings never block launch — a run can proceed on a partially-valid
//! config (a bad judge pin falls back to the worker, etc.); the point is to
//! surface the problem where it's cheap to fix, not to gate.

use crate::config::{PROVIDERS, ProviderConfig};
use crate::engine_config::{ModelSpec, model_spec_for, parse_model_spec};
use crate::model_catalog::validate_model_slug;
use crate::settings::{AgentEngineConfig, EngineAgentKind};
use stella_model::catalog::Catalog;

/// One flagged settings problem — where it lives, the offending value, and
/// what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsIssue {
    /// The settings location, in the user's own vocabulary (`default_model`,
    /// `agents.judge.model`, `allowed_models[2]`, or `--model`).
    pub location: String,
    /// The configured value that failed.
    pub value: String,
    /// What is wrong and how to fix it.
    pub message: String,
}

impl SettingsIssue {
    /// The one-line form the launch path prints (and tests pin).
    pub fn line(&self) -> String {
        format!("{}: `{}` — {}", self.location, self.value, self.message)
    }
}

fn kind_label(kind: EngineAgentKind) -> &'static str {
    match kind {
        EngineAgentKind::Default => "default",
        EngineAgentKind::Worker => "worker",
        EngineAgentKind::Judge => "judge",
        EngineAgentKind::Triage => "triage",
    }
}

/// Whether this provider's seed-catalog ids are vendor-namespaced (carry a
/// `/`, like OpenRouter's `openrouter/auto` and `openai/gpt-5.5`) rather than
/// bare (like Z.ai's `glm-5.2`). For a namespaced provider the wire slug MUST
/// keep its namespace — a bare slug is the fingerprint of an over-eager
/// `provider/slug` split that stripped it.
fn provider_ids_namespaced(provider: &str) -> bool {
    let seed = Catalog::seed();
    let mut ids = seed
        .entries()
        .iter()
        .filter(|e| e.provider == provider)
        .map(|e| e.id.as_str())
        .peekable();
    ids.peek().is_some() && ids.all(|id| id.contains('/'))
}

/// The wire-exactness problem with `wire`, if any — checks INDEPENDENT of the
/// catalog's alias-tolerant `resolve`, which happily maps both a doubled and a
/// de-namespaced slug back to the right card and so masks exactly these bugs.
/// The precise fingerprints:
///
/// - **over-qualified**: the slug repeats the provider prefix
///   (`openrouter/openrouter/auto`) — the doubled form providers reject; and
/// - **de-namespaced**: a namespaced provider's slug lost its vendor prefix
///   (`openrouter/auto` mis-split to the wire slug `auto`).
fn wire_shape_issue(provider: &str, wire: &str) -> Option<String> {
    if wire.starts_with(&format!("{provider}/{provider}/")) {
        return Some(format!(
            "over-qualified — the id repeats `{provider}/`; drop one so the wire \
             slug matches the provider's catalog (e.g. `{provider}/auto`, not \
             `{provider}/{provider}/auto`)"
        ));
    }
    if !wire.contains('/') && provider_ids_namespaced(provider) {
        return Some(format!(
            "missing the vendor namespace — `{provider}` model ids carry a \
             `vendor/` prefix, so the wire slug should be e.g. \
             `{provider}/{wire}`, not the bare `{wire}`"
        ));
    }
    None
}

/// Validate an already-resolved [`ModelSpec`] — the wire slug exactly as the
/// engine would send it — against the provider catalog. `None` means "no
/// problem I can prove" (valid, a provider pin with no model — the provider
/// default answers — or a settings-defined provider whose endpoint is the
/// authority). `value` is the user's original string, echoed back in the
/// warning so it points at what they actually typed.
fn check_resolved_spec(location: &str, value: &str, spec: &ModelSpec) -> Option<SettingsIssue> {
    // A provider pin with no slug rides the provider's own default model.
    if spec.model.is_empty() {
        return None;
    }
    // Only built-in providers have a catalog to validate against; a
    // settings-defined custom endpoint is its own authority (mirrors
    // `validate_model_slug`'s local/never-synced posture) — `?` skips it.
    let provider_config = PROVIDERS.iter().find(|p| p.id == spec.provider)?;
    // Wire-shape checks first — they catch the over-qualified / de-namespaced
    // slugs the alias-tolerant catalog resolve would wave through.
    if let Some(message) = wire_shape_issue(&spec.provider, &spec.model) {
        return Some(SettingsIssue {
            location: location.to_string(),
            value: value.to_string(),
            message,
        });
    }
    match validate_model_slug(provider_config, &spec.model) {
        Ok(()) => None,
        Err(message) => Some(SettingsIssue {
            location: location.to_string(),
            value: value.to_string(),
            message,
        }),
    }
}

/// Validate one configured model STRING (a `provider/slug` or bare slug with
/// no separate `provider` field — `default_model`, `allowed_models`),
/// resolving it to its wire slug exactly as the engine would.
fn check_spec(
    location: &str,
    raw: &str,
    is_provider: &dyn Fn(&str) -> bool,
) -> Option<SettingsIssue> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Some(spec) = parse_model_spec(trimmed, is_provider) else {
        return Some(SettingsIssue {
            location: location.to_string(),
            value: trimmed.to_string(),
            message: "unrecognized model — use `provider/slug` (e.g. `zai/glm-5.2`) or a \
                      bare slug the seed catalog knows"
                .to_string(),
        });
    };
    check_resolved_spec(location, trimmed, &spec)
}

/// Validate every model reference in the engine settings. Per-agent `model`
/// entries are resolved through the engine's own [`model_spec_for`], so the
/// check honors the agent's explicit `provider` field (a set `provider` sends
/// `model` VERBATIM as the wire slug — no `provider/slug` split) and validates
/// against the exact provider the request will hit. `default_model` and each
/// `allowed_models` candidate are plain `provider/slug` strings, parsed as
/// `--model` semantics.
pub fn check_engine_settings(
    engine: &AgentEngineConfig,
    is_provider: &dyn Fn(&str) -> bool,
) -> Vec<SettingsIssue> {
    let mut issues = Vec::new();
    if let Some(model) = &engine.default_model
        && let Some(issue) = check_spec("default_model", model, is_provider)
    {
        issues.push(issue);
    }
    for kind in [
        EngineAgentKind::Default,
        EngineAgentKind::Worker,
        EngineAgentKind::Judge,
        EngineAgentKind::Triage,
    ] {
        // Only validate an agent's OWN explicit `model` pin here; the flat /
        // `default_model` fallbacks are covered by their own locations above.
        let Some(agent) = engine.agent(kind) else {
            continue;
        };
        let Some(raw) = agent.model.as_deref() else {
            continue;
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let location = format!("agents.{}.model", kind_label(kind));
        // Resolve exactly as the engine does — honoring `agent.provider`.
        match model_spec_for(engine, kind, is_provider) {
            Some(spec) => {
                if let Some(issue) = check_resolved_spec(&location, trimmed, &spec) {
                    issues.push(issue);
                }
            }
            // No pinned provider and the string names no known provider /
            // seed slug: fall back to the plain-string diagnostic so a typo'd
            // per-agent slug still surfaces as `unrecognized`.
            None => {
                if let Some(issue) = check_spec(&location, trimmed, is_provider) {
                    issues.push(issue);
                }
            }
        }
    }
    for (i, model) in engine.allowed_models().iter().enumerate() {
        if let Some(issue) = check_spec(&format!("allowed_models[{i}]"), model, is_provider) {
            issues.push(issue);
        }
    }
    issues
}

/// The launch entry point: validate every configured model reference plus
/// the resolved default model. Best-effort — a settings load failure yields
/// no issues here (the config path already surfaced it), and the caller
/// treats the result as advisory warnings, never a launch gate.
pub fn validate_at_launch(cfg: &crate::config::Config) -> Vec<SettingsIssue> {
    let mut issues = Vec::new();
    if let Ok(settings) = crate::settings::Settings::load(&cfg.workspace_root) {
        let ids: Vec<String> = PROVIDERS
            .iter()
            .map(|p| p.id.to_string())
            .chain(std::iter::once(
                crate::config::LOCAL_PROVIDER.id.to_string(),
            ))
            .chain(settings.providers.keys().cloned())
            .collect();
        let is_provider = |id: &str| ids.iter().any(|p| p == id);
        if let Some(engine) = &settings.agent_engine_config {
            issues.extend(check_engine_settings(engine, &is_provider));
        }
    }
    // The effective wire model — deduped against the settings checks so an
    // issue already reported for `default_model` isn't repeated here.
    if let Some(issue) = check_resolved_model(&cfg.provider, &cfg.model_id)
        && !issues.iter().any(|i| i.value == issue.value)
    {
        issues.push(issue);
    }
    issues
}

/// Validate the RESOLVED wire model the default agent will actually send —
/// the last line of defense, catching a bad slug however it was configured
/// (`--model`, auto-detect, or a settings path this module can't see).
pub fn check_resolved_model(provider: &ProviderConfig, model_id: &str) -> Option<SettingsIssue> {
    let issue = |message: String| SettingsIssue {
        location: "resolved model".to_string(),
        value: format!("{}/{}", provider.id, model_id),
        message,
    };
    if let Some(message) = wire_shape_issue(provider.id, model_id) {
        return Some(issue(message));
    }
    validate_model_slug(provider, model_id).err().map(issue)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The seed catalog is deterministic, so these assertions hold without any
    // synced store: `zai/glm-5.2` and `openrouter/auto` are seeded; `auto`
    // (bare) and doubled forms are not.
    fn is_seed_provider(id: &str) -> bool {
        PROVIDERS.iter().any(|p| p.id == id)
    }

    fn openrouter() -> &'static ProviderConfig {
        PROVIDERS.iter().find(|p| p.id == "openrouter").unwrap()
    }

    #[test]
    fn a_seeded_slug_passes() {
        assert!(check_spec("default_model", "zai/glm-5.2", &is_seed_provider).is_none());
    }

    #[test]
    fn the_correct_openrouter_qualified_form_passes() {
        // `openrouter/openrouter/auto` decodes to the wire slug
        // `openrouter/auto`, which the seed catalog knows — this is the
        // CORRECT setting form and must NOT be flagged.
        assert!(
            check_spec(
                "default_model",
                "openrouter/openrouter/auto",
                &is_seed_provider
            )
            .is_none()
        );
    }

    #[test]
    fn a_singly_qualified_openrouter_slug_is_flagged() {
        // `openrouter/auto` resolves to the wire slug `auto`, which OpenRouter
        // does not serve — the natural-looking but wrong form.
        let issue = check_spec("default_model", "openrouter/auto", &is_seed_provider)
            .expect("bare `auto` wire slug must be flagged");
        assert_eq!(issue.location, "default_model");
    }

    #[test]
    fn an_over_qualified_slug_gets_the_double_prefix_note() {
        // The doubled wire slug that actually reaches the provider as a 400.
        let issue = check_resolved_model(openrouter(), "openrouter/openrouter/auto")
            .expect("doubled wire slug must be flagged");
        assert!(
            issue.message.contains("over-qualified"),
            "expected the double-prefix note: {}",
            issue.message
        );
    }

    #[test]
    fn an_unknown_provider_qualification_is_unrecognized() {
        let issue = check_spec("agents.judge.model", "notaprovider/x", &is_seed_provider)
            .expect("unknown provider prefix must be flagged");
        assert!(issue.message.contains("unrecognized"), "{}", issue.message);
    }

    #[test]
    fn a_valid_resolved_model_is_not_flagged() {
        assert!(check_resolved_model(openrouter(), "openrouter/auto").is_none());
    }

    #[test]
    fn per_agent_provider_pin_is_sent_verbatim_not_split() {
        // A judge pinned to OpenRouter with a slug that itself contains `/`:
        // the engine sends `openai/gpt-6` VERBATIM to OpenRouter (unseeded →
        // its endpoint is the authority), so the check must NOT re-split the
        // slug and validate the phantom `openai/gpt-6` against the OpenAI
        // catalog (where `gpt-6` does not exist) — that was a false positive.
        let engine: AgentEngineConfig = serde_json::from_str(
            r#"{ "agents": { "judge": { "provider": "openrouter", "model": "openai/gpt-6" } } }"#,
        )
        .unwrap();
        assert!(
            check_engine_settings(&engine, &is_seed_provider).is_empty(),
            "an OpenRouter-pinned verbatim slug must not be flagged"
        );
    }

    #[test]
    fn per_agent_provider_pin_validates_the_pinned_provider() {
        // With no explicit `provider`, `openai/nope` splits to the OpenAI
        // catalog and is correctly flagged (the string carries its own
        // routing).
        let engine: AgentEngineConfig =
            serde_json::from_str(r#"{ "agents": { "judge": { "model": "openai/nope" } } }"#)
                .unwrap();
        let issues = check_engine_settings(&engine, &is_seed_provider);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].location, "agents.judge.model");
        assert_eq!(issues[0].value, "openai/nope");
    }

    #[test]
    fn issue_line_is_readable() {
        let issue = SettingsIssue {
            location: "default_model".into(),
            value: "openrouter/auto".into(),
            message: "not served".into(),
        };
        assert_eq!(
            issue.line(),
            "default_model: `openrouter/auto` — not served"
        );
    }
}
