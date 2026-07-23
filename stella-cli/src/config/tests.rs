use super::*;

/// Every provider's `default_model` here must resolve against
/// `stella_model::catalog::Catalog::seed()`, or `build_provider`'s
/// catalog check (`agent.rs`) would hard-error on first use of a
/// provider whose default was never added to the seed. Uses the
/// provider-scoped resolver, same as `build_provider`, so a default that
/// only exists under a *different* provider's row still fails here.
#[test]
fn every_provider_default_model_resolves_against_the_catalog_seed() {
    let catalog = stella_model::catalog::Catalog::seed();
    for provider in PROVIDERS {
        catalog
            .resolve_for(provider.id, provider.default_model)
            .unwrap_or_else(|e| {
                panic!(
                    "provider `{}`'s default_model `{}` is not in the catalog seed: {e}",
                    provider.id, provider.default_model
                )
            });
    }
}

#[test]
fn provider_ids_are_unique() {
    let mut ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
    ids.push(LOCAL_PROVIDER.id);
    let before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), before, "duplicate provider id in PROVIDERS");
}

/// Every seeded provider must declare its prompt-cache posture in
/// stella-model's parity matrix — the guard born from OpenRouter
/// silently running Claude with zero caching. A new provider cannot
/// land without stating how caching is engaged and naming the witness
/// test that proves it.
#[test]
fn every_seeded_provider_declares_a_cache_posture() {
    for provider in PROVIDERS.iter().chain(std::iter::once(&LOCAL_PROVIDER)) {
        assert!(
            stella_model::provider_parity::cache_posture(provider.id).is_some(),
            "provider `{}` has no row in stella-model/src/provider_parity.rs — \
             add its CachePosture (with a witness test) in this PR",
            provider.id
        );
    }
}

/// The reasoning-axis sibling of the cache-posture guard: every seeded
/// provider must declare how its reasoning/thinking budget is controlled (or
/// that the shared adapter deliberately drops it). Born from the same silent
/// per-provider divergence — a pinned effort reaching only Z.ai and OpenRouter
/// and being dropped everywhere else with nothing enforcing the omission stays
/// deliberate. A new provider cannot land without stating its reasoning
/// posture and naming the witness that proves a `Controllable` control on the
/// wire.
#[test]
fn every_seeded_provider_declares_a_reasoning_posture() {
    for provider in PROVIDERS.iter().chain(std::iter::once(&LOCAL_PROVIDER)) {
        assert!(
            stella_model::provider_parity::reasoning_posture(provider.id).is_some(),
            "provider `{}` has no ReasoningPosture row in \
             stella-model/src/provider_parity.rs — add it (with a witness test for a \
             Controllable control, or a note for a no-control posture) in this PR",
            provider.id
        );
    }
}

#[test]
fn alias_env_var_resolves_when_the_primary_is_unset() {
    // Synthetic provider with unique var names so parallel tests can't
    // race on shared env state (the convention credential.rs's own
    // tests follow).
    let provider = ProviderConfig {
        id: "alias-test",
        env_var: "STELLA_TEST_ALIAS_PRIMARY_KEY",
        env_var_aliases: &["STELLA_TEST_ALIAS_SECONDARY_KEY"],
        display_name: "Alias Test",
        default_model: "m",
        base_url: "",
        dialect: Dialect::OpenaiCompatible,
        seeded: false,
    };
    // SAFETY: test-only env mutation, unique var names per test — and
    // serialized behind the binary-wide env lock, because setenv racing
    // any concurrent getenv is UB on POSIX regardless of var names.
    let _env = crate::test_env::lock();
    unsafe {
        std::env::remove_var("STELLA_TEST_ALIAS_PRIMARY_KEY");
        std::env::set_var("STELLA_TEST_ALIAS_SECONDARY_KEY", "sk-from-alias");
    }
    let file = CredentialsFile::load(std::env::temp_dir().join(format!(
        "stella-test-alias-credentials-{}.toml",
        std::process::id()
    )))
    .unwrap();

    let (key, source) = resolve_provider_key(&provider, None, None, &file, false).unwrap();
    assert_eq!(key.reveal(), "sk-from-alias");
    assert_eq!(source, stella_model::credential::CredentialSource::EnvVar);

    unsafe {
        std::env::remove_var("STELLA_TEST_ALIAS_SECONDARY_KEY");
    }
}

#[test]
fn config_debug_never_leaks_the_api_key() {
    // H3: with `api_key: ApiKey`, the whole Config's derived Debug must
    // redact the secret — no `{:?}` (logs, panics, traces) can leak it.
    let secret = "sk-super-secret-do-not-log-XYZ";
    let cfg = Config {
        provider: PROVIDERS[0].clone(),
        model_id: "glm-5.2".to_string(),
        api_key: ApiKey::new(secret),
        workspace_root: std::path::PathBuf::from("/tmp/ws"),
        base_url_override: None,
        hooks: None,
        engine_settings: None,
        tools_bash: false,
        enable_recap: false,
        tools_web: false,
        authority: crate::settings::AuthorityPolicy::default(),
        credential_source: Some(stella_model::credential::CredentialSource::EnvVar),
    };
    let dbg = format!("{cfg:?}");
    assert!(!dbg.contains(secret), "Config Debug leaked the key: {dbg}");
    assert!(dbg.contains("redacted"));
}

#[test]
fn resolved_config_carries_the_authority_computed_during_settings_load() {
    let authority = crate::settings::AuthorityPolicy {
        project_prompts_allowed: true,
        project_custom_tools_allowed: false,
        bash_allowed: false,
        web_allowed: true,
        media_requires_host_approval: true,
    };
    let mut settings = crate::settings::Settings::default();
    settings.authority_policy = authority;

    let cfg = Config::load_with_settings(
        Some("local/test-model"),
        None,
        Some("http://localhost:11434/v1"),
        &settings,
        std::path::PathBuf::from("/tmp/ws"),
    )
    .unwrap();

    assert_eq!(cfg.authority, authority);
}

/// Helper: a Settings value parsed from JSON, as the scope-merge would
/// produce it — the seam for exercising resolution without touching
/// `$HOME`, `/etc`, or a real workspace.
fn settings_from(json: &str) -> crate::settings::Settings {
    serde_json::from_str(json).expect("test settings JSON must parse")
}

#[test]
fn trusted_engine_json_replaces_adversarial_project_engine_settings() {
    use crate::settings::EngineAgentKind;
    use stella_protocol::ReasoningEffort;

    let settings = settings_from(
        r#"{
            "providers": {"openrouter": {"api_key": "sk-or-test"}},
            "agent_engine_config": {
                "default_model": "anthropic/task-chosen-model",
                "pipeline_worker_model": "zai/task-worker",
                "auto_mode": "on",
                "effort_auto": "on",
                "reasoning_auto": "on",
                "agents": {
                    "default": {"provider": "anthropic", "model": "task-default"},
                    "worker": {"provider": "zai", "model": "task-worker"},
                    "judge": {"provider": "openai", "model": "task-judge"},
                    "triage": {"provider": "deepseek", "model": "task-triage"}
                }
            }
        }"#,
    );
    let trusted = r#"{
        "default_model":"openrouter/deepseek/deepseek-v4-pro",
        "allowed_models":["openrouter/deepseek/deepseek-v4-pro"],
        "auto_mode":"off",
        "effort_auto":"off",
        "reasoning_auto":"off",
        "agents":{
            "default":{"effort":"high","reasoning":"on"},
            "worker":{"effort":"high","reasoning":"on"},
            "judge":{"effort":"high","reasoning":"on"},
            "triage":{"effort":"low","reasoning":"off"}
        }
    }"#;

    let _env = crate::test_env::lock();
    // SAFETY: test-only process environment mutation serialized by test_env.
    unsafe { std::env::set_var(TRUSTED_ENGINE_CONFIG_ENV, trusted) };
    let cfg = Config::load_with_settings(
        Some("openrouter/deepseek/deepseek-v4-pro"),
        None,
        Some("https://openrouter.ai/api/v1"),
        &settings,
        std::path::PathBuf::from("/tmp/ws"),
    )
    .expect("trusted engine posture should replace project settings");
    unsafe { std::env::remove_var(TRUSTED_ENGINE_CONFIG_ENV) };

    let engine = cfg
        .engine_settings
        .expect("trusted engine config is stamped");
    assert_eq!(
        engine.default_model.as_deref(),
        Some("openrouter/deepseek/deepseek-v4-pro")
    );
    assert!(engine.pipeline_worker_model.is_none());
    assert!(engine.pipeline_judge_model.is_none());
    assert!(engine.pipeline_triage_model.is_none());
    assert!(!engine.auto_mode_on());
    assert!(!engine.effort_auto_on());
    assert!(!engine.reasoning_auto_on());
    for kind in EngineAgentKind::ALL {
        let spec = crate::engine_config::model_spec_for(&engine, kind, &|id| id == "openrouter")
            .expect("every role inherits the trusted default model");
        assert_eq!(spec.provider, "openrouter");
        assert_eq!(spec.model, "deepseek/deepseek-v4-pro");
        let tuning = crate::engine_config::tuning_for(&engine, kind);
        if kind == EngineAgentKind::Triage {
            assert_eq!(tuning.effort, Some(ReasoningEffort::Low));
            assert_eq!(tuning.reasoning, Some(false));
        } else {
            assert_eq!(tuning.effort, Some(ReasoningEffort::High));
            assert_eq!(tuning.reasoning, Some(true));
        }
        let agent = engine.agent(kind).expect("all four tuning rows are pinned");
        assert!(agent.model.is_none());
        assert!(agent.provider.is_none());
        assert!(agent.prompt.is_none());
        assert!(agent.params.is_none());
    }
}

#[test]
fn benchmark_mode_skips_malformed_filesystem_credentials_but_keeps_engine_override() {
    let _env = crate::test_env::lock();
    let dir = tempfile::tempdir().unwrap();
    let credential_dir = dir.path().join(".stella");
    std::fs::create_dir_all(&credential_dir).unwrap();
    std::fs::write(
        credential_dir.join("credentials.toml"),
        "this is deliberately [not valid TOML",
    )
    .unwrap();
    let trusted = r#"{
        "default_model":"openrouter/deepseek/deepseek-v4-pro",
        "allowed_models":["openrouter/deepseek/deepseek-v4-pro"],
        "auto_mode":"off",
        "effort_auto":"off",
        "reasoning_auto":"off"
    }"#;
    let old_home = std::env::var_os("HOME");

    // SAFETY: the binary-wide environment lock covers mutation, resolution,
    // and restoration. STELLA_NO_SETTINGS is the adapter-pinned benchmark
    // isolation mode; the trusted engine JSON remains a later explicit seam.
    unsafe {
        std::env::set_var("HOME", dir.path());
        std::env::set_var(TRUSTED_ENGINE_CONFIG_ENV, trusted);
    }
    let _isolation = crate::settings::test_filesystem_isolation(true);
    let cfg = Config::load_with_settings(
        Some("openrouter/deepseek/deepseek-v4-pro"),
        Some("test-key-from-trusted-handoff-seam"),
        Some("https://openrouter.ai/api/v1"),
        &crate::settings::Settings::default(),
        dir.path().join("workspace"),
    )
    .expect("benchmark mode must never parse the hostile credential file");
    unsafe {
        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        std::env::remove_var(TRUSTED_ENGINE_CONFIG_ENV);
    }

    assert_eq!(cfg.provider.id, "openrouter");
    assert_eq!(
        cfg.engine_settings
            .and_then(|settings| settings.default_model),
        Some("openrouter/deepseek/deepseek-v4-pro".to_string())
    );
}

#[test]
fn malformed_trusted_engine_json_fails_closed_without_echoing_value() {
    let secret_marker = "DO-NOT-ECHO-BENCHMARK-CONTENT";
    let malformed = format!(
        r#"{{"default_model":"{secret_marker}","agents":{{"worker":{{"efort":"high"}}}}}}"#
    );
    let _env = crate::test_env::lock();
    // SAFETY: test-only process environment mutation serialized by test_env.
    unsafe { std::env::set_var(TRUSTED_ENGINE_CONFIG_ENV, &malformed) };
    let error = Config::load_with_settings(
        Some("openrouter/deepseek/deepseek-v4-pro"),
        Some("test-key"),
        Some("https://openrouter.ai/api/v1"),
        &crate::settings::Settings::default(),
        std::path::PathBuf::from("/tmp/ws"),
    )
    .unwrap_err();
    unsafe { std::env::remove_var(TRUSTED_ENGINE_CONFIG_ENV) };

    assert!(error.contains(TRUSTED_ENGINE_CONFIG_ENV));
    assert!(!error.contains(secret_marker));
    assert!(!error.contains("efort"));
}

#[test]
fn a_settings_defined_provider_resolves_via_model_flag_with_its_literal_key() {
    let _env = crate::test_env::lock();
    // The issue #44 acceptance criterion: a provider that is NOT
    // built-in, added purely via settings.json, usable via
    // --model <id>/<model> with no code change.
    let settings = settings_from(
        r#"{"providers": {"together": {
            "name": "Together AI",
            "base_url": "https://api.together.xyz/v1",
            "api_key": "sk-together-test",
            "default_model": "meta-llama/Llama-3.3-70B-Instruct-Turbo"
        }}}"#,
    );
    let cfg = Config::load_with_settings(
        Some("together/meta-llama/Llama-3.3-70B-Instruct-Turbo"),
        None,
        None,
        &settings,
        std::path::PathBuf::from("/tmp/ws"),
    )
    .expect("config-defined provider should resolve");
    assert_eq!(cfg.provider.id, "together");
    assert_eq!(cfg.provider.display_name, "Together AI");
    assert_eq!(cfg.model_id, "meta-llama/Llama-3.3-70B-Instruct-Turbo");
    assert_eq!(cfg.effective_base_url(), "https://api.together.xyz/v1");
    assert_eq!(cfg.api_key.reveal(), "sk-together-test");
    assert_eq!(cfg.provider.dialect, Dialect::OpenaiCompatible);
    assert!(
        !cfg.provider.seeded,
        "config-defined providers must bypass the catalog check"
    );
}

#[test]
fn a_custom_provider_without_base_url_is_a_named_error() {
    let _env = crate::test_env::lock();
    let settings = settings_from(r#"{"providers": {"fireworks": {"api_key": "sk-x"}}}"#);
    let err = Config::load_with_settings(
        Some("fireworks/some-model"),
        None,
        None,
        &settings,
        std::path::PathBuf::from("/tmp/ws"),
    )
    .unwrap_err();
    assert!(err.contains("fireworks"), "{err}");
    assert!(err.contains("base_url"), "{err}");
}

#[test]
fn custom_providers_cannot_claim_the_vertex_or_bedrock_dialects() {
    let _env = crate::test_env::lock();
    let settings = settings_from(
        r#"{"providers": {"myvertex": {
            "base_url": "https://example.com",
            "dialect": "vertex"
        }}}"#,
    );
    let err = Config::load_with_settings(
        Some("myvertex/some-model"),
        None,
        None,
        &settings,
        std::path::PathBuf::from("/tmp/ws"),
    )
    .unwrap_err();
    assert!(err.contains("reserved for the built-in provider"), "{err}");
}

#[test]
fn a_settings_override_reshapes_a_builtin_without_changing_its_dialect() {
    let _env = crate::test_env::lock();
    // The pre-#44 override use case (e.g. the Z.ai coding plan): move a
    // built-in's base URL and key out of provider-specific env vars.
    let settings = settings_from(
        r#"{"providers": {"zai": {
            "name": "ZAI Provider",
            "base_url": "https://api.z.ai/api/coding/paas/v4"
        }}}"#,
    );
    // Key via --api-key (outranks everything) so this test can't be
    // perturbed by an ambient ZAI_API_KEY on the host.
    let cfg = Config::load_with_settings(
        Some("zai/glm-5.2"),
        Some("sk-cli-flag"),
        None,
        &settings,
        std::path::PathBuf::from("/tmp/ws"),
    )
    .expect("built-in override should resolve");
    assert_eq!(cfg.provider.id, "zai");
    assert_eq!(cfg.provider.display_name, "ZAI Provider");
    assert_eq!(
        cfg.effective_base_url(),
        "https://api.z.ai/api/coding/paas/v4"
    );
    assert_eq!(cfg.api_key.reveal(), "sk-cli-flag");
    assert_eq!(cfg.provider.dialect, Dialect::OpenaiCompatible);
    assert!(
        cfg.provider.seeded,
        "built-in overrides keep the catalog check"
    );
}

#[test]
fn a_stale_default_pin_does_not_mangle_a_qualified_engine_default_model() {
    let _env = crate::test_env::lock();
    // Regression: `agents.default.provider: "zai"` alongside the flat
    // `default_model: "openrouter/openrouter/auto"` (a provider-qualified
    // spec, the shape every TUI save writes) used to stitch the phantom
    // slug `zai/openrouter/openrouter/auto` and die on the catalog
    // check. The qualified spec's own provider must win over the stale
    // seeded pin.
    let settings = settings_from(
        r#"{
            "providers": {"openrouter": {"api_key": "sk-or-test"}},
            "agent_engine_config": {
                "default_model": "openrouter/openrouter/auto",
                "agents": {"default": {"provider": "zai"}}
            }
        }"#,
    );
    let cfg = Config::load_with_settings(
        None,
        None,
        None,
        &settings,
        std::path::PathBuf::from("/tmp/ws"),
    )
    .expect("the qualified engine default must resolve");
    assert_eq!(cfg.provider.id, "openrouter");
    assert_eq!(cfg.model_id, "openrouter/auto");
}

#[test]
fn an_openrouter_pin_over_the_tui_qualified_default_does_not_double_the_wire_slug() {
    let _env = crate::test_env::lock();
    // Regression: the pin naming the qualified spec's OWN provider —
    // `agents.default.provider: "openrouter"` plus the TUI-written
    // `default_model: "openrouter/openrouter/auto"`. OpenRouter is
    // unseeded, so the catalog arbitration that saves a stale seeded
    // pin never ran, verbatim routing kept the doubled slug, and every
    // call died on `openrouter/openrouter/auto is not a valid model ID`
    // (HTTP 400). The wire slug must come out de-qualified.
    let settings = settings_from(
        r#"{
            "providers": {"openrouter": {"api_key": "sk-or-test"}},
            "agent_engine_config": {
                "default_model": "openrouter/openrouter/auto",
                "agents": {"default": {"provider": "openrouter"}}
            }
        }"#,
    );
    let cfg = Config::load_with_settings(
        None,
        None,
        None,
        &settings,
        std::path::PathBuf::from("/tmp/ws"),
    )
    .expect("the pinned qualified default must resolve");
    assert_eq!(cfg.provider.id, "openrouter");
    assert_eq!(cfg.model_id, "openrouter/auto");
}

#[test]
fn env_var_outranks_the_settings_literal_key() {
    // Chain order: env var above settings.json api_key. Unique var name
    // so parallel tests can't race on shared env state.
    let settings = settings_from(
        r#"{"providers": {"envrank": {
            "base_url": "https://envrank.example/v1",
            "api_key": "sk-from-settings",
            "api_key_env": "STELLA_TEST_ENVRANK_KEY",
            "default_model": "m1"
        }}}"#,
    );
    // SAFETY: test-only env mutation, unique var name per test — and
    // serialized behind the binary-wide env lock (setenv racing any
    // concurrent getenv is UB on POSIX regardless of var names).
    let _env = crate::test_env::lock();
    unsafe {
        std::env::set_var("STELLA_TEST_ENVRANK_KEY", "sk-from-env");
    }
    let cfg = Config::load_with_settings(
        Some("envrank/m1"),
        None,
        None,
        &settings,
        std::path::PathBuf::from("/tmp/ws"),
    )
    .unwrap();
    assert_eq!(cfg.api_key.reveal(), "sk-from-env");
    assert!(
        stella_tools::subprocess_env::is_sensitive_env_name(std::ffi::OsStr::new(
            "STELLA_TEST_ENVRANK_KEY"
        )),
        "trusted custom api_key_env must be registered for child-process scrubbing"
    );
    unsafe {
        std::env::remove_var("STELLA_TEST_ENVRANK_KEY");
    }
}

#[test]
fn a_bare_slug_matches_a_custom_providers_default_model() {
    let _env = crate::test_env::lock();
    let settings = settings_from(
        r#"{"providers": {"slugmatch": {
            "base_url": "https://slugmatch.example/v1",
            "api_key": "sk-slug",
            "default_model": "totally-custom-slug"
        }}}"#,
    );
    let cfg = Config::load_with_settings(
        Some("totally-custom-slug"),
        None,
        None,
        &settings,
        std::path::PathBuf::from("/tmp/ws"),
    )
    .unwrap();
    assert_eq!(cfg.provider.id, "slugmatch");
    assert_eq!(cfg.model_id, "totally-custom-slug");
}

#[test]
fn discovery_style_resolution_accepts_the_settings_literal_key() {
    let _env = crate::test_env::lock();
    // resolve_provider_key with a settings literal and nothing else:
    // resolves non-interactively as SettingsJson — this is what puts
    // config-defined providers into auto-detection and judge discovery.
    let provider = ProviderConfig {
        id: "settings-key-test",
        env_var: "STELLA_TEST_SETTINGS_KEY_UNSET",
        env_var_aliases: &[],
        display_name: "Settings Key Test",
        default_model: "m",
        base_url: "https://x.example/v1",
        dialect: Dialect::OpenaiCompatible,
        seeded: false,
    };
    let file = CredentialsFile::load(std::env::temp_dir().join(format!(
        "stella-test-settings-key-credentials-{}.toml",
        std::process::id()
    )))
    .unwrap();
    let (key, source) =
        resolve_provider_key(&provider, None, Some("sk-settings"), &file, false).unwrap();
    assert_eq!(key.reveal(), "sk-settings");
    assert_eq!(
        source,
        stella_model::credential::CredentialSource::SettingsJson
    );
}

/// Issue #249's source-display requirement: a settings.json literal and a
/// real credentials.toml entry are two DIFFERENT stores, and a caller
/// showing "where did this come from" must be able to tell them apart. A
/// single `CredentialSource::ConfigFile` for both would conflate "declared
/// in settings.json" with "stored in credentials.toml". This constructs a
/// provider with BOTH present (the settings literal must win per the
/// documented precedence) and asserts the resolved source is the
/// settings-specific variant, not the file one — the file's differing value
/// proves which one actually won.
#[test]
fn settings_json_literal_is_reported_distinctly_from_a_real_credentials_toml_entry() {
    let provider = ProviderConfig {
        id: "settings-vs-file-test",
        env_var: "STELLA_TEST_SETTINGS_VS_FILE_UNSET",
        env_var_aliases: &[],
        display_name: "Settings vs File Test",
        default_model: "m",
        base_url: "https://x.example/v1",
        dialect: Dialect::OpenaiCompatible,
        seeded: false,
    };
    let mut file = CredentialsFile::load(std::env::temp_dir().join(format!(
        "stella-test-settings-vs-file-credentials-{}.toml",
        std::process::id()
    )))
    .unwrap();
    file.set("settings-vs-file-test", "sk-from-credentials-file");

    let (key, source) =
        resolve_provider_key(&provider, None, Some("sk-from-settings-json"), &file, false).unwrap();
    assert_eq!(key.reveal(), "sk-from-settings-json");
    assert_eq!(
        source,
        stella_model::credential::CredentialSource::SettingsJson,
        "a settings.json literal must be reported as SettingsJson, distinct from ConfigFile"
    );
}

#[test]
fn derived_env_var_uppercases_and_folds_punctuation() {
    assert_eq!(derived_env_var("together"), "TOGETHER_API_KEY");
    assert_eq!(derived_env_var("my-gateway"), "MY_GATEWAY_API_KEY");
}

#[test]
fn local_provider_requires_base_url_and_defaults_its_key() {
    let _env = crate::test_env::lock();
    let err = Config::load(Some("local/llama3.3"), None, None).unwrap_err();
    assert!(err.contains("--base-url"), "{err}");

    let cfg = Config::load(
        Some("local/llama3.3"),
        None,
        Some("http://localhost:11434/v1"),
    )
    .expect("local provider with --base-url should resolve");
    assert_eq!(cfg.provider.id, "local");
    assert_eq!(cfg.model_id, "llama3.3");
    assert_eq!(cfg.effective_base_url(), "http://localhost:11434/v1");
    // No LOCAL_API_KEY set: the placeholder key is used (local servers
    // generally ignore auth, but the OpenAI-compatible client always
    // sends a bearer token).
    assert_eq!(cfg.api_key.reveal(), "local");
}

/// The trusted seam rejects the WHOLE override on one unknown key — that is
/// its point ("a misspelled benchmark control must fail closed"). The cost is
/// that adding a field to `AgentEngineConfig` without also allowing it here
/// stops the benchmark booting, with a message that deliberately does not
/// echo the value, so the offending key is invisible. That happened once:
/// `headless_scope_bypass` shipped in the struct and the posture, and every
/// trial died at startup with a single line of output.
///
/// So: the exact shape the harbor adapter emits must survive the seam.
#[test]
fn the_benchmark_engine_posture_survives_the_trusted_launcher_seam() {
    let posture = serde_json::json!({
        "default_model": "openrouter/z-ai/glm-5.2",
        "allowed_models": ["openrouter/z-ai/glm-5.2"],
        "auto_mode": "off",
        "effort_auto": "off",
        "reasoning_auto": "off",
        "headless_scope_bypass": "on",
        "agents": {
            "default": {"effort": "high", "reasoning": "on"},
            "worker": {"effort": "high", "reasoning": "on"},
            "judge": {"effort": "high", "reasoning": "on"},
            "triage": {"effort": "low", "reasoning": "off"},
        },
    });
    assert!(
        super::trusted_engine_config_shape_is_strict(&posture),
        "bench/harbor_adapter's posture must pass the strict seam"
    );
    let parsed: crate::settings::AgentEngineConfig =
        serde_json::from_value(posture).expect("and deserialize into the settings type");
    assert!(
        parsed.headless_scope_bypass_on(),
        "the flag must survive the round trip, not just be tolerated"
    );
}
