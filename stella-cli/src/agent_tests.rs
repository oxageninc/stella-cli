use super::*;
use crate::config::{ConfiguredProvider, PROVIDERS, ProviderConfig};
use stella_model::credential::ApiKey;
use stella_pipeline::CandidateWorkspacePort;

#[test]
fn one_shot_reflection_defaults_on_for_every_output_format() {
    let _env = crate::test_env::lock();
    // SAFETY: the shared test env lock serializes every Stella test that
    // mutates or reads process environment state.
    unsafe { std::env::remove_var(DISABLE_REFLECTION_ENV) };

    assert!(one_shot_reflection_enabled(OutputFormat::Text));
    assert!(one_shot_reflection_enabled(OutputFormat::Json));
    assert!(one_shot_reflection_enabled(OutputFormat::StreamJson));
}

#[test]
fn explicit_reflection_opt_out_suppresses_every_one_shot_format() {
    let _env = crate::test_env::lock();
    // SAFETY: the shared test env lock serializes every Stella test that
    // mutates or reads process environment state.
    unsafe { std::env::set_var(DISABLE_REFLECTION_ENV, "  YeS  ") };

    assert!(!one_shot_reflection_enabled(OutputFormat::Text));
    assert!(!one_shot_reflection_enabled(OutputFormat::Json));
    assert!(!one_shot_reflection_enabled(OutputFormat::StreamJson));

    // SAFETY: still inside the shared test env critical section.
    unsafe { std::env::remove_var(DISABLE_REFLECTION_ENV) };
}

#[test]
fn reflection_opt_out_uses_explicit_truthy_values() {
    for value in ["1", "true", "TRUE", " yes ", "On"] {
        assert!(is_truthy_env_value(value), "{value:?} should be truthy");
    }
    for value in ["", "0", "false", "no", "off", "disabled", "2"] {
        assert!(!is_truthy_env_value(value), "{value:?} should be falsey");
    }
}

/// The store write path for `StepUsage`: every token field on the event
/// — cache writes included — lands in the telemetry row verbatim.
/// Regression for issue #97, where `cache_write_tokens` was hard-coded
/// to 0 at this exact seam while the schema and `stella stats` already
/// carried the column.
#[test]
fn persist_event_records_cache_write_tokens_from_step_usage() {
    let store = Store::in_memory().expect("in-memory store");
    let execution_id = store
        .begin_execution("run", "prompt", "anthropic", "claude-fable-5")
        .expect("begin execution");
    let event = AgentEvent::StepUsage {
        output_text: None,
        step: 0,
        role: stella_protocol::ModelCallRole::Worker,
        provider: "anthropic".into(),
        model: "claude-fable-5".into(),
        input_tokens: 1_000,
        output_tokens: 50,
        cached_input_tokens: 900,
        cache_write_tokens: 640,
        estimated_input_tokens: 980,
        cost_usd: 0.0042,
        duration_ms: 1_830,
        retries: 0,
        tool_calls: 1,
        complete: true,
    };

    assert!(persist_event(&store, execution_id, 0, &event, "anthropic"));
    store
        .finish_execution(execution_id, "completed", 0.0042)
        .expect("finish execution");

    let rows = store.usage_stats().expect("usage stats");
    let row = rows
        .iter()
        .find(|r| r.provider == "anthropic")
        .expect("anthropic row");
    assert_eq!(row.input_tokens, 1_000);
    assert_eq!(row.output_tokens, 50);
    assert_eq!(row.cache_read_tokens, 900);
    assert_eq!(
        row.cache_write_tokens, 640,
        "the event's cache-write count must reach the store, never a hard-coded 0"
    );
}

/// The scripts section rides the byte-stable prompt prefix: two
/// assemblies over the same workspace must be byte-identical, the verb
/// bindings must be present, and a scriptless workspace must add
/// nothing (docs/design/scripts-index.md).
#[test]
fn assemble_system_prompt_carries_a_byte_stable_scripts_section() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        root.path().join("package.json"),
        r#"{"scripts": {"build": "next build", "test": "vitest"}}"#,
    )
    .unwrap();
    std::fs::write(root.path().join("pnpm-lock.yaml"), "").unwrap();

    let authority = crate::settings::AuthorityPolicy {
        project_prompts_allowed: true,
        ..crate::settings::AuthorityPolicy::default()
    };
    let rules = crate::rules::ResolvedRules::default();
    let first = assemble_system_prompt(SYSTEM_PROMPT, root.path(), &authority, &rules);
    let second = assemble_system_prompt(SYSTEM_PROMPT, root.path(), &authority, &rules);
    assert_eq!(first, second, "same workspace state ⇒ identical bytes");
    assert!(first.contains("## Project scripts"), "section present");
    assert!(first.contains("build → pnpm run build"), "{first}");
    assert!(first.contains("install → pnpm install"), "{first}");

    let empty = tempfile::tempdir().expect("tempdir");
    let bare = assemble_system_prompt(SYSTEM_PROMPT, empty.path(), &authority, &rules);
    assert!(
        !bare.contains("## Project scripts"),
        "no scripts → no section, no noise"
    );
}

/// Zero-call orientation (issue #328): over a pre-indexed workspace, the
/// interactive system prompt carries the project map — languages, layout,
/// entry points — baked into the byte-stable prefix, so orientation costs no
/// model round-trip and is unconditional rather than left to the model's
/// discretion. Two assemblies over the same index state must be
/// byte-identical: that is the invariant that lets the whole prefix ride the
/// provider's prompt cache (AGENTS.md invariant #7).
#[test]
fn assemble_system_prompt_bakes_a_byte_stable_orientation_map() {
    let root = graph_fixture();

    let authority = crate::settings::AuthorityPolicy {
        project_prompts_allowed: true,
        ..crate::settings::AuthorityPolicy::default()
    };
    let rules = crate::rules::ResolvedRules::default();
    let first = assemble_system_prompt(SYSTEM_PROMPT, root.path(), &authority, &rules);
    let second = assemble_system_prompt(SYSTEM_PROMPT, root.path(), &authority, &rules);
    assert_eq!(
        first, second,
        "same index state ⇒ identical bytes (the prompt-cache invariant)"
    );
    assert!(first.contains("## Project map"), "{first}");
    assert!(first.contains("Languages: rust"), "{first}");
    assert!(
        first.contains("Layout (2 indexed files): 2 at the root"),
        "the slow-churning skeleton includes the top-level layout: {first}"
    );
    assert!(first.contains("Entry points:"), "{first}");
}

/// The #336 wave-1 steering-parity witness: `read_symbol` (#383) must be
/// advertised in BOTH static base personas the way its siblings `repo_diff`
/// (#381) and `diagnostics` (#384) are — a tool the prompt never mentions
/// loses to guessed read_file offsets no matter how good it is.
#[test]
fn both_static_prompts_carry_a_read_symbol_steering_line() {
    for (name, prompt) in [
        ("SYSTEM_PROMPT", SYSTEM_PROMPT),
        ("PIPELINE_SYSTEM_PROMPT", PIPELINE_SYSTEM_PROMPT),
    ] {
        assert!(
            prompt.contains("- read_symbol: "),
            "{name} must carry a read_symbol steering line"
        );
        assert!(
            prompt.contains("guessing read_file offsets after a graph_query"),
            "{name}'s read_symbol line must steer AWAY from offset-guessing — \
             that round-trip is the tool's reason to exist (issue #330)"
        );
    }
}

/// Build a real code-graph index in a tempdir: `hub.rs` (three symbols) is
/// busiest, `leaf.rs` (one) is not. Returns the workspace root tempdir.
fn graph_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        root.path().join("hub.rs"),
        "pub fn a() {}\npub fn b() {}\npub struct C;\n",
    )
    .unwrap();
    std::fs::write(root.path().join("leaf.rs"), "pub fn d() {}\n").unwrap();
    let db = stella_store::workspace_private_sqlite_path(root.path(), "codegraph.db").unwrap();
    let graph = stella_graph::CodeGraph::open(root.path(), &db).expect("open graph");
    graph.index_all().expect("index");
    graph.shutdown();
    root
}

/// The default snapshot roots on the busiest file and carries the full,
/// sorted file list the deck's picker browses — sourced straight from the
/// graph store, a superset of the rooted neighborhood.
#[test]
fn graph_snapshot_defaults_to_the_busiest_file_and_lists_all_files() {
    let root = graph_fixture();
    let snap = graph_snapshot(root.path()).expect("snapshot");
    assert_eq!(snap.focus, "hub.rs", "default focus is the busiest file");
    assert_eq!(
        snap.files,
        vec!["hub.rs".to_string(), "leaf.rs".to_string()],
        "the picker's file list is every indexed file, sorted"
    );
}

/// An explicit focus re-roots the neighborhood on that file — the picker's
/// selection path — while still shipping the same browsable file list.
#[test]
fn graph_snapshot_focus_re_roots_on_the_requested_file() {
    let root = graph_fixture();
    let snap = graph_snapshot_focus(root.path(), Some("leaf.rs")).expect("snapshot");
    assert_eq!(snap.focus, "leaf.rs", "re-rooted on the requested file");
    assert!(
        snap.nodes.iter().any(|n| n.label == "leaf.rs"),
        "the neighborhood is centered on leaf.rs, not the busiest file"
    );
    assert!(snap.files.contains(&"hub.rs".to_string()));
}

/// No index → no snapshot (the tab shows its "run stella init" hint).
#[test]
fn graph_snapshot_is_none_without_an_index() {
    let root = tempfile::tempdir().expect("tempdir");
    assert!(graph_snapshot(root.path()).is_none());
    assert!(graph_snapshot_focus(root.path(), Some("x.rs")).is_none());
}

#[cfg(unix)]
#[test]
fn schema_index_population_visibly_rejects_unsafe_legacy_codegraph() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("tempdir");
    let dot = root.path().join(".stella");
    std::fs::create_dir_all(&dot).unwrap();
    std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o777)).unwrap();
    std::fs::write(dot.join("codegraph.db"), b"unsafe legacy graph").unwrap();
    let registry = ToolRegistry::with_issue_backend(root.path().to_path_buf(), None);

    let error = populate_schema_index(&registry, root.path()).unwrap_err();
    assert!(
        error.contains("legacy") && error.contains("private"),
        "{error}"
    );
    assert!(dot.join("codegraph.db").exists());
}

/// Auto-build on session start (task part A): a workspace with a source
/// file. `graph_query` is now advertised from turn 1 regardless (it builds
/// its own index on first use), so this pins what [`spawn_session_graph`]
/// still adds: it builds `.stella/private/codegraph.db` EAGERLY in the
/// background, so the first real query harvests a ready index instead of
/// paying the build cost inline. Awaiting the returned handle is the
/// deterministic "index ready" signal.
#[tokio::test]
async fn spawn_session_graph_eagerly_builds_the_index_in_the_background() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("lib.rs"), "pub fn find_me() {}\n").unwrap();

    let registry = Arc::new(ToolRegistry::with_issue_backend(root.clone(), None));
    let advertises = |r: &ToolRegistry| r.schemas().iter().any(|s| s.name == "graph_query");

    // Turn 1: advertised already, and no index on disk yet — the tool does
    // not wait for one, it builds on first use.
    assert!(!stella_tools::graph::graph_available(&root).unwrap());
    assert!(
        advertises(&registry),
        "graph_query is advertised from the start, index or not"
    );

    let (session_graph, build) =
        spawn_session_graph(&root, registry.clone(), Box::new(|_| {}), Box::new(|| {}));
    build.await.expect("background build task");

    // After the build: the db exists, the tool is advertised, and it
    // dispatches against the freshly built index.
    assert!(
        stella_tools::graph::graph_available(&root).unwrap(),
        "the background build must create .stella/private/codegraph.db"
    );
    assert!(
        advertises(&registry),
        "graph_query stays advertised after the build"
    );
    let out = registry
        .execute(
            "graph_query",
            &serde_json::json!({"op": "definitions", "target": "find_me"}),
        )
        .await;
    assert!(!out.is_error(), "graph_query must dispatch: {out:?}");
    session_graph.shutdown();
}

/// Live freshness (task part B): after the session graph is up, a
/// brand-new source file the agent (or an external tool) writes is
/// incrementally re-indexed by the live `notify` watcher, so the very next
/// `graph_query` reflects it — the staleness that makes the model distrust
/// the graph is gone. Polls with a generous budget because the OS watcher
/// + debounce are asynchronous, and re-writes the file each iteration so a
/// create event lost during the watcher's async arming window is retried
/// (the un-indexed file re-parses on the first event that lands).
#[tokio::test]
async fn session_graph_live_refreshes_after_a_file_is_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("lib.rs"), "pub fn original() {}\n").unwrap();

    let registry = Arc::new(ToolRegistry::with_issue_backend(root.clone(), None));
    let (session_graph, build) =
        spawn_session_graph(&root, registry.clone(), Box::new(|_| {}), Box::new(|| {}));
    build.await.expect("background build task");

    // The new symbol is absent from the just-built index.
    let before = stella_tools::graph::run_query(&root, "definitions", "added_later");
    assert!(
        matches!(&before, ToolOutput::Ok { content } if content.contains("no definitions")),
        "the new symbol must not be indexed yet: {before:?}"
    );

    let added = root.join("added.rs");
    let mut reflected = false;
    for _ in 0..150 {
        std::fs::write(&added, "pub fn added_later() {}\n").unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let ToolOutput::Ok { content } =
            stella_tools::graph::run_query(&root, "definitions", "added_later")
            && content.contains("added_later")
        {
            reflected = true;
            break;
        }
    }
    assert!(
        reflected,
        "the live watcher must re-index the new file so graph_query reflects it"
    );
    session_graph.shutdown();
}

/// Tier-1 rule wiring (issue #103): a workspace rule renders into the
/// assembled system prompt, appended after the untouched base prefix.
#[test]
fn system_prompt_carries_the_workspace_rules_section() {
    let root = tempfile::tempdir().expect("tempdir");
    let rules_dir = root.path().join(".stella/rules");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
        rules_dir.join("no-force-push.md"),
        "---\nguard-tool: Bash\nguard-deny-command: git push --force*\n---\nNever force-push.",
    )
    .unwrap();

    let mut cfg = cfg_for("zai");
    cfg.authority.project_prompts_allowed = true;
    let rules = crate::rules::load_workspace_rules(root.path(), &cfg.authority);
    let prompt = build_system_prompt(&cfg, root.path(), &rules);
    assert!(
        prompt.starts_with(SYSTEM_PROMPT),
        "rules append to the prompt; the base prefix must stay intact"
    );
    assert!(prompt.contains("## Workspace rules"));
    assert!(
        prompt.contains("Never force-push.  [enforced]"),
        "a guarded rule must render with the enforced marker: {prompt}"
    );
}

/// An untrusted checkout cannot append repository-authored content to the
/// privileged system prompt. Explicit repository trust restores those
/// sources within the already-computed managed ceiling.
#[test]
fn untrusted_project_prompt_sources_are_absent_from_the_system_prompt() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        root.path().join("package.json"),
        r#"{"scripts": {"authority-marker": "echo project-script"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join(".stella/memories")).unwrap();
    std::fs::write(
        root.path().join(".stella/memories/project.md"),
        "PROJECT_MEMORY_AUTHORITY_MARKER",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join(".stella/rules")).unwrap();
    std::fs::write(
        root.path().join(".stella/rules/project.md"),
        "PROJECT_RULE_AUTHORITY_MARKER",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join(".stella/explorations")).unwrap();
    std::fs::write(
        root.path().join(".stella/explorations/project.json"),
        serde_json::json!({
            "slice": "authority-map",
            "title": "PROJECT_MAP_AUTHORITY_MARKER",
            "summary": "project map",
            "content": "body",
            "files": [],
            "created_at_ms": 1u64
        })
        .to_string(),
    )
    .unwrap();

    let mut cfg = cfg_for("zai");
    cfg.workspace_root = root.path().to_path_buf();
    cfg.authority.project_prompts_allowed = false;
    let untrusted_rules = crate::rules::load_workspace_rules(root.path(), &cfg.authority);
    let untrusted = build_system_prompt(&cfg, root.path(), &untrusted_rules);
    for marker in [
        "authority-marker",
        "PROJECT_MEMORY_AUTHORITY_MARKER",
        "PROJECT_RULE_AUTHORITY_MARKER",
        "PROJECT_MAP_AUTHORITY_MARKER",
    ] {
        assert!(
            !untrusted.contains(marker),
            "untrusted project marker reached system prompt: {marker}\n{untrusted}"
        );
    }

    cfg.authority.project_prompts_allowed = true;
    let trusted_rules = crate::rules::load_workspace_rules(root.path(), &cfg.authority);
    let trusted = build_system_prompt(&cfg, root.path(), &trusted_rules);
    for marker in [
        "authority-marker",
        "PROJECT_MEMORY_AUTHORITY_MARKER",
        "PROJECT_RULE_AUTHORITY_MARKER",
        "PROJECT_MAP_AUTHORITY_MARKER",
    ] {
        assert!(trusted.contains(marker), "trusted marker missing: {marker}");
    }
}

#[test]
fn system_prompt_carries_the_workspace_maps_index() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path().join(".stella/explorations");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("cli.json"),
        serde_json::json!({
            "slice": "cli", "title": "CLI surface", "summary": "maps the CLI",
            "content": "big body that must NOT be in the prompt",
            "files": [], "created_at_ms": 1u64
        })
        .to_string(),
    )
    .unwrap();

    let mut cfg = cfg_for("zai");
    cfg.authority.project_prompts_allowed = true;
    let rules = crate::rules::ResolvedRules::default();
    let prompt = build_system_prompt(&cfg, root.path(), &rules);
    assert!(
        prompt.contains("## Workspace maps"),
        "index section missing"
    );
    assert!(prompt.contains("`cli`") && prompt.contains("CLI surface"));
    assert!(
        !prompt.contains("big body"),
        "map bodies must stay pull-only, never in the prompt"
    );

    // No maps → no section, no tokens.
    let bare = tempfile::tempdir().expect("tempdir");
    let empty = build_system_prompt(
        &cfg_for("zai"),
        bare.path(),
        &crate::rules::ResolvedRules::default(),
    );
    assert!(!empty.contains("## Workspace maps"));
}

#[test]
fn benchmark_gate_excludes_hostile_filesystem_steering_and_extensions() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("home");
    let root = workspace.path();
    let dot_stella = root.join(".stella");

    for (path, body) in [
        (
            dot_stella.join("memories/hostile.md"),
            "HOSTILE_WORKSPACE_MEMORY",
        ),
        (dot_stella.join("rules/hostile.md"), "HOSTILE_STELLA_RULE"),
        (
            root.join(".claude/rules/hostile-claude.md"),
            "HOSTILE_CLAUDE_RULE",
        ),
        (
            dot_stella.join("skills/hostile/SKILL.md"),
            "---\nname: hostile-workspace-skill\ndescription: hostile workspace skill\n---\nHOSTILE_WORKSPACE_SKILL",
        ),
        (
            home.path().join(".stella/rules/hostile-user.md"),
            "HOSTILE_USER_RULE",
        ),
        (
            home.path().join(".stella/skills/hostile-user/SKILL.md"),
            "---\nname: hostile-user-skill\ndescription: hostile user skill\n---\nHOSTILE_USER_SKILL",
        ),
    ] {
        std::fs::create_dir_all(path.parent().expect("fixture parent")).unwrap();
        std::fs::write(path, body).unwrap();
    }

    for (path, name) in [
        (
            dot_stella.join("tools/hostile.toml"),
            "hostile_workspace_tool",
        ),
        (
            home.path().join(".stella/tools/hostile.toml"),
            "hostile_user_tool",
        ),
    ] {
        std::fs::create_dir_all(path.parent().expect("tool fixture parent")).unwrap();
        std::fs::write(
            path,
            format!(
                "name = \"{name}\"\ndescription = \"must not load\"\ncommand = [\"sh\", \"-c\", \"exit 99\"]\n"
            ),
        )
        .unwrap();
    }

    std::fs::write(
        dot_stella.join("mcp.toml"),
        "[servers.hostile]\ntransport = \"stdio\"\ncmd = \"sh\"\nargs = [\"-c\", \"exit 98\"]\n",
    )
    .unwrap();
    let context_bytes = b"HOSTILE_CONTEXT_DB";
    let store_bytes = b"HOSTILE_STORE_DB";
    std::fs::write(dot_stella.join("context.db"), context_bytes).unwrap();
    std::fs::write(dot_stella.join("store.db"), store_bytes).unwrap();

    let _home = crate::settings::test_user_home(home.path().to_path_buf());
    let isolation = crate::settings::test_filesystem_isolation(true);

    let mut cfg = cfg_for("zai");
    cfg.workspace_root = root.to_path_buf();
    cfg.authority.project_prompts_allowed = true;

    let rules = crate::rules::load_workspace_rules(root, &cfg.authority);
    let prompt = build_pipeline_system_prompt(&cfg, root, &rules);
    let skills = crate::memory::load_workspace_skills(root);
    let custom_tools = custom_tool_report_for_workspace(root).tools;
    let memory = SessionMemory::open(root, false);
    let store = open_store(root);
    let mcp = load_mcp_plan(&cfg);

    let registry = ToolRegistry::with_backends_and_options(
        root.to_path_buf(),
        None,
        None,
        registry_options(&cfg),
    );
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let interactive = InteractiveToolSet::new(&registry, event_tx, default_ask_io(false));
    let interactive = match skill_registry_for_run(root.to_path_buf()) {
        Some(registry) => interactive.with_skill_registry(registry),
        None => interactive,
    };
    let discovery = crate::discovery::DiscoveryToolSet::new(&interactive, root.to_path_buf());
    let schema_names: Vec<String> = discovery
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect();

    assert_eq!(prompt, PIPELINE_SYSTEM_PROMPT);
    assert!(rules.is_empty(), "rules loaded under benchmark isolation");
    assert!(skills.is_empty(), "skills loaded under benchmark isolation");
    assert!(
        custom_tools.is_empty(),
        "custom tools loaded under benchmark isolation"
    );
    assert!(memory.is_none(), "context memory opened under isolation");
    assert!(
        store.is_none(),
        "workspace telemetry store opened under isolation"
    );
    assert!(matches!(mcp, McpPlan::None));
    assert!(schema_names.iter().any(|name| name == "tool_search"));
    for forbidden in [
        "skill_search",
        "mcp_search",
        "search_skills",
        "install_skill",
        "hostile_workspace_tool",
        "hostile_user_tool",
    ] {
        assert!(
            !schema_names.iter().any(|name| name == forbidden),
            "{forbidden} leaked into the isolated tool schema: {schema_names:?}"
        );
    }
    assert_eq!(
        std::fs::read(dot_stella.join("context.db")).unwrap(),
        context_bytes
    );
    assert_eq!(
        std::fs::read(dot_stella.join("store.db")).unwrap(),
        store_bytes
    );

    // Dropping only the isolation signal proves normal product behavior is
    // unchanged against the exact same workspace/user fixtures.
    drop(isolation);
    let normal_rules = crate::rules::load_workspace_rules(root, &cfg.authority);
    let normal_prompt = build_pipeline_system_prompt(&cfg, root, &normal_rules);
    let normal_skills = crate::memory::load_workspace_skills(root);
    let normal_custom_tools = custom_tool_report_for_workspace(root).tools;
    assert!(normal_prompt.contains("HOSTILE_WORKSPACE_MEMORY"));
    assert!(normal_prompt.contains("HOSTILE_STELLA_RULE"));
    assert!(normal_prompt.contains("HOSTILE_CLAUDE_RULE"));
    assert!(normal_prompt.contains("HOSTILE_USER_RULE"));
    assert!(!normal_rules.is_empty());
    assert_eq!(normal_skills.len(), 2);
    assert_eq!(normal_custom_tools.len(), 2);
    assert!(skill_registry_for_run(root.to_path_buf()).is_some());
    assert!(matches!(load_mcp_plan(&cfg), McpPlan::Invalid(_)));
}

/// A `Config` selecting `provider_id` at its default model, with a dummy
/// key. `build_provider` only constructs the adapter (no network call),
/// so the key is never used.
fn cfg_for(provider_id: &str) -> Config {
    let provider = PROVIDERS
        .iter()
        .find(|p| p.id == provider_id)
        .unwrap_or_else(|| panic!("provider `{provider_id}` not in PROVIDERS"))
        .clone();
    let model_id = provider.default_model.to_string();
    Config {
        provider,
        model_id,
        api_key: ApiKey::new("dummy-key-unused-offline"),
        credential_source: None,
        workspace_root: std::path::PathBuf::from("/tmp"),
        base_url_override: None,
        hooks: None,
        engine_settings: None,
        tools_bash: false,
        enable_recap: false,
        tools_web: false,
        authority: crate::settings::AuthorityPolicy::default(),
    }
}

#[tokio::test]
async fn untrusted_project_custom_tools_are_absent_from_the_runtime_surface() {
    let workspace = tempfile::tempdir().unwrap();
    let workspace_tools = workspace.path().join(".stella/tools");
    std::fs::create_dir_all(&workspace_tools).unwrap();
    std::fs::write(
        workspace_tools.join("workspace.toml"),
        "name = \"workspace_tool\"\ndescription = \"d\"\ncommand = [\"./workspace.sh\"]",
    )
    .unwrap();
    let mut cfg = cfg_for("zai");
    cfg.workspace_root = workspace.path().to_path_buf();
    cfg.authority.project_custom_tools_allowed = false;

    let tools = discover_custom_tools(&cfg, false).await;

    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(
        !names.contains(&"workspace_tool"),
        "runtime tools: {names:?}"
    );
}

#[test]
fn non_tty_text_output_is_headless_without_losing_text_rendering() {
    let cfg = cfg_for("zai");
    let format = OutputFormat::Text;
    let worker_model = ModelRef::new(cfg.provider.id, cfg.model_id.clone());
    let non_tty = pipeline_config_for_approval_capability(
        &cfg,
        PipelineApprovalCapability::Unavailable,
        None,
        &worker_model,
    );
    assert!(
        non_tty.headless,
        "text redirected through a non-TTY host cannot prompt for approval"
    );
    assert!(
        !non_tty.headless_bypass_scope_review,
        "output serialization must never grant execution authority"
    );
    assert_eq!(format, OutputFormat::Text, "rendering remains text");

    let interactive = pipeline_config_for_approval_capability(
        &cfg,
        PipelineApprovalCapability::Stdio,
        None,
        &worker_model,
    );
    assert!(
        !interactive.headless,
        "an explicit interactive approval host retains scope review"
    );
    assert!(!interactive.headless_bypass_scope_review);
}

/// Issue: a squash-merge (#284 x #297/#276) silently dropped
/// `run_pipeline_one_shot`'s `approval_capability` computation, collapsing
/// its production call site to a bare `is_text` check with no test to catch
/// it — the helper above was already covered in isolation, but nothing
/// exercised the actual condition the call site computes. These three tests
/// pin `approval_capability_for` (the extracted, directly-testable seam)
/// against every input combination that matters.
#[test]
fn approval_capability_for_requires_both_terminal_handles_not_just_text_format() {
    // The exact regression: a redirected/piped text-format run (is_text,
    // stdout still a TTY, but stdin is NOT) must stay Unavailable — a bare
    // `is_text` check would wrongly select Stdio here and try to read an
    // approval decision from a pipe no one is at the other end of.
    assert_eq!(
        approval_capability_for(true, false, true),
        PipelineApprovalCapability::Unavailable,
        "text format alone must not select Stdio when stdin isn't a real terminal"
    );
    assert_eq!(
        approval_capability_for(true, true, false),
        PipelineApprovalCapability::Unavailable,
        "text format alone must not select Stdio when stdout isn't a real terminal"
    );
    assert_eq!(
        approval_capability_for(true, false, false),
        PipelineApprovalCapability::Unavailable
    );
}

#[test]
fn approval_capability_for_json_is_always_unavailable() {
    // Output serialization must never grant execution authority, regardless
    // of the terminal state — JSON output has nowhere to render a prompt.
    assert_eq!(
        approval_capability_for(false, true, true),
        PipelineApprovalCapability::Unavailable
    );
    assert_eq!(
        approval_capability_for(false, false, false),
        PipelineApprovalCapability::Unavailable
    );
}

#[test]
fn approval_capability_for_full_tty_text_is_stdio() {
    // Only the genuine interactive case — text format, real stdin, real
    // stdout — selects Stdio.
    assert_eq!(
        approval_capability_for(true, true, true),
        PipelineApprovalCapability::Stdio
    );
}

/// The composition gap the incident actually exploited: `approval_capability_for`
/// and `pipeline_config_for_approval_capability` were each covered above in
/// isolation, but nothing pinned them wired together the way
/// `run_pipeline_one_shot` actually wires them (agent.rs, around the
/// `pipeline_config` construction) — feeding one straight into the other. A
/// regression that breaks *that* composition (e.g. hardcoding
/// `PipelineApprovalCapability::Stdio` at the call site instead of using the
/// computed value) would pass every test above while still shipping the
/// scope-review bypass this incident (#284 x #297, fixed in #305) shipped.
#[test]
fn non_tty_text_run_wiring_stays_headless_and_json_run_wiring_never_bypasses_scope_review() {
    let cfg = cfg_for("zai");
    let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());

    // A non-TTY text-format run (e.g. `stella run` piped in a script or CI)
    // must not select the interactive stdio approval gate, and its wired
    // config must stay headless.
    let text_capability = approval_capability_for(true, false, false);
    let text_config =
        pipeline_config_for_approval_capability(&cfg, text_capability, None, &model_ref);
    assert_ne!(
        text_capability,
        PipelineApprovalCapability::Stdio,
        "a non-tty text run must not select the interactive stdio approval gate"
    );
    assert!(
        text_config.headless,
        "a non-tty text run's wired config must stay headless"
    );

    // A JSON-format one-shot run is headless by construction — and even with
    // both terminal handles real, its wired config must never bypass scope
    // review; JSON has nowhere to render a prompt regardless of TTY state.
    let json_capability = approval_capability_for(false, true, true);
    let json_config =
        pipeline_config_for_approval_capability(&cfg, json_capability, None, &model_ref);
    assert!(json_config.headless);
    assert!(
        !json_config.headless_bypass_scope_review,
        "a JSON-format run's wired config must never bypass scope review"
    );
}

#[tokio::test]
async fn candidate_rules_reuse_the_parent_snapshot_after_source_removal() {
    let root = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t.t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(root.path().join("base.txt"), "base\n").unwrap();
    git(&["add", "base.txt"]);
    git(&["commit", "-q", "-m", "base"]);

    let rule_path = root.path().join(".stella/rules/protect-session.md");
    std::fs::create_dir_all(rule_path.parent().unwrap()).unwrap();
    std::fs::write(
        &rule_path,
        "---\nguard-tool: Write\nguard-deny-path: protected/**\n---\nOriginal session guard.",
    )
    .unwrap();
    let mut cfg = cfg_for("zai");
    cfg.workspace_root = root.path().to_path_buf();
    cfg.authority.project_prompts_allowed = true;

    let parent_rules = crate::rules::load_workspace_rules(root.path(), &cfg.authority);
    let parent = ToolRegistry::with_issue_backend(root.path().to_path_buf(), None);
    crate::rules::attach_rule_guards(&parent, &parent_rules);
    let parent_denied = parent
        .execute(
            "write_file",
            &serde_json::json!({"path": "protected/parent.txt", "content": "no\n"}),
        )
        .await;
    assert!(parent_denied.is_error(), "parent guard was not attached");

    // Mutate the source after the parent session has resolved and attached
    // it. Candidate creation must retain that original session snapshot.
    std::fs::remove_file(&rule_path).unwrap();
    let prompt = build_system_prompt(&cfg, root.path(), &parent_rules);
    assert!(
        prompt.contains("Original session guard.  [enforced]"),
        "prompt rendering diverged from the parent rule snapshot: {prompt}"
    );
    let ws_ports = workspace_ports(
        root.path().to_path_buf(),
        &cfg,
        stella_tools::RegistryOptions::default(),
        parent_rules.clone(),
        None,
    )
    .unwrap();
    let candidate = ws_ports.candidate_workspaces.create().await.unwrap();
    let output = candidate
        .tools()
        .execute(
            "write_file",
            &serde_json::json!({"path": "protected/candidate.txt", "content": "no\n"}),
        )
        .await;
    candidate.seal().await.unwrap();
    let adopted = candidate.adopt().await.unwrap();
    let landed = root.path().join("protected/candidate.txt").exists();
    candidate.remove().await;

    assert!(
        output.is_error(),
        "candidate reloaded weakened sources instead of retaining the parent snapshot: {output:?}"
    );
    assert!(
        adopted.is_empty(),
        "prohibited candidate edit was adoptable: {adopted:?}"
    );
    assert!(!landed, "prohibited candidate edit reached the parent tree");
}

#[test]
fn existing_providers_still_route_to_their_current_adapter() {
    // Regression: switching the catalog check to resolve_for, the
    // (provider, id) dedup, and the inserted vertex/bedrock arms must NOT
    // change selection for any provider that worked before. `build_provider`
    // dispatches on `cfg.provider.id`: OpenAI/Anthropic/Gemini each get
    // their own native adapter, while the OpenAI-compatible gateways (xAI,
    // DeepSeek, OpenRouter) share the ZaiProvider implementation but are
    // re-identified via `with_identity`, so each adapter's `id()` is its own
    // provider name — i.e. every provider reports itself.
    for (provider_id, expected_adapter) in [
        ("openai", "openai"),
        ("anthropic", "anthropic"),
        ("zai", "zai"),
        ("xai", "xai"),
        ("deepseek", "deepseek"),
        ("gemini", "gemini"),
        ("openrouter", "openrouter"),
    ] {
        let provider = build_provider(&cfg_for(provider_id))
            .unwrap_or_else(|e| panic!("build_provider({provider_id}) failed: {e}"));
        assert_eq!(
            provider.id(),
            expected_adapter,
            "provider `{provider_id}` must still route to the `{expected_adapter}` adapter"
        );
    }
}

#[test]
fn vertex_and_bedrock_route_to_their_native_adapters_not_a_fallthrough() {
    // The new providers must construct their own native adapter (not the
    // shared ZaiProvider shim, id "zai", nor the anthropic branch). Both
    // arms read extra addressing/credentials from the environment; set
    // the minimum each requires. build_provider only constructs — no
    // network call. Env mutation is UB against concurrent getenv on
    // POSIX, so hold the binary-wide env lock for the whole
    // mutate-read-cleanup window; the missing-project error case shares
    // this test so the set/remove stays serialized.
    let _env = crate::test_env::lock();
    unsafe {
        std::env::set_var("VERTEX_PROJECT_ID", "test-project");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test-secret");
    }

    let vertex = build_provider(&cfg_for("vertex")).expect("vertex builds");
    assert_eq!(vertex.id(), "vertex", "vertex must route to VertexProvider");

    let bedrock = build_provider(&cfg_for("bedrock")).expect("bedrock builds");
    assert_eq!(
        bedrock.id(),
        "bedrock",
        "bedrock must route to BedrockProvider"
    );

    // A vertex selection with no project id must fail loudly with a named
    // error, never silently fall through to another adapter.
    unsafe {
        std::env::remove_var("VERTEX_PROJECT_ID");
        std::env::remove_var("GOOGLE_CLOUD_PROJECT");
    }
    // `.err()` (not `.unwrap_err()`) so the Ok type `Box<dyn Provider>`,
    // which is not `Debug`, is never required to be printed.
    let err = build_provider(&cfg_for("vertex"))
        .err()
        .expect("vertex without a project id must be an error");
    assert!(
        err.contains("VERTEX_PROJECT_ID"),
        "expected a named VERTEX_PROJECT_ID error, got: {err}"
    );

    unsafe {
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    }
}

/// A `ConfiguredProvider` for `provider_id` at its default model with a
/// dummy key — the offline analogue of `cfg_for` for judge routing. The
/// key is never sent anywhere: routing only constructs adapters and
/// reads `.id()`.
fn configured_provider(provider_id: &str) -> ConfiguredProvider {
    let config = PROVIDERS
        .iter()
        .find(|p| p.id == provider_id)
        .unwrap_or_else(|| panic!("provider `{provider_id}` not in PROVIDERS"))
        .clone();
    ConfiguredProvider {
        config,
        api_key: ApiKey::new("dummy-key-unused-offline"),
    }
}

#[test]
fn single_configured_provider_reuses_the_worker_as_judge() {
    // (a) Only the worker's own provider is configured: no distinct
    // family exists, so the router degrades to the worker and we build no
    // second provider — the judge IS the worker (identical to the
    // pre-routing behavior, no extra cost).
    let configured = vec![configured_provider("zai")];
    assert!(
        resolve_cross_family_judge("zai", "glm-5.2", &configured).is_none(),
        "a single configured family must leave the judge as the worker provider"
    );
}

#[test]
fn same_family_providers_reuse_the_worker_as_judge() {
    // Two providers but ONE family (Gemini and Gemini-via-Vertex both
    // group under `google`): still no bias-resistant judge available, so
    // it stays the worker — proves `provider_family` grouping gates the
    // cross-family judge, not the raw provider count.
    let configured = vec![configured_provider("gemini"), configured_provider("vertex")];
    assert!(
        resolve_cross_family_judge("gemini", "gemini-3-pro", &configured).is_none(),
        "same-vendor providers share a family and must not route a cross-family judge"
    );
}

#[test]
fn distinct_families_route_a_cross_family_judge() {
    // (b) Worker on Z.ai with Anthropic also configured: the router picks
    // the distinct family and we build that concrete adapter. No network
    // — only construction and `.id()`.
    let configured = vec![configured_provider("zai"), configured_provider("anthropic")];
    let (judge, judge_id) = resolve_cross_family_judge("zai", "glm-5.2", &configured)
        .expect("a distinct family must route a cross-family judge");
    assert_eq!(judge_id, "anthropic", "judge must be the distinct family");
    assert_eq!(judge.id(), "anthropic", "judge adapter must be Anthropic's");
    assert_ne!(
        judge.id(),
        "zai",
        "judge must differ from the worker's family"
    );
}

#[test]
fn judge_build_failure_falls_back_to_the_worker() {
    // (c) The router selects a distinct family, but building that judge
    // adapter fails (an unknown model slug the catalog rejects). Judge
    // routing must never break the loop: it falls back to the worker
    // provider (`None`). Fully offline and race-free — no shared env, no
    // network — unlike an env-gated Vertex/Bedrock build failure.
    let faux = ConfiguredProvider {
        config: ProviderConfig {
            id: "faux",
            env_var: "STELLA_TEST_FAUX_KEY",
            env_var_aliases: &[],
            display_name: "Faux (unbuildable)",
            default_model: "faux-model-not-in-catalog",
            base_url: "http://localhost:0",
            dialect: crate::config::Dialect::OpenaiCompatible,
            // Seeded on purpose: the catalog check must reject the
            // phantom slug, which is exactly the build failure this
            // test needs.
            seeded: true,
        },
        api_key: ApiKey::new("dummy-key-unused-offline"),
    };
    let configured = vec![configured_provider("zai"), faux];
    assert!(
        resolve_cross_family_judge("zai", "glm-5.2", &configured).is_none(),
        "a judge adapter that fails to build must fall back to the worker provider"
    );
}

#[test]
fn reflection_json_preserves_full_paid_call_envelope_and_cost() {
    let report = ReflectionReport {
        recorded: 1,
        model_error: None,
        cost_usd: 0.0042,
        events: vec![AgentEvent::StepUsage {
            output_text: None,
            step: 0,
            role: stella_protocol::ModelCallRole::Reflection,
            provider: "anthropic".into(),
            model: "claude-reflect".into(),
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 5,
            cache_write_tokens: 3,
            estimated_input_tokens: 90,
            cost_usd: 0.0042,
            duration_ms: 12,
            retries: 1,
            tool_calls: 0,
            complete: true,
        }],
    };

    let value = reflection_json(&report);
    assert_eq!(value["cost_usd"], 0.0042);
    assert_eq!(value["events"][0]["type"], "step_usage");
    assert_eq!(value["events"][0]["role"], "reflection");
    assert_eq!(value["events"][0]["provider"], "anthropic");
    assert_eq!(value["events"][0]["model"], "claude-reflect");
    assert_eq!(value["events"][0]["complete"], true);
}

#[test]
fn reflection_budget_tick_is_rebased_to_the_caller_session() {
    let mut guard = BudgetGuard::new(BudgetMode::Enforced, Some(1.0), None);
    let _ = guard.record_spend(0.8);
    let mut report = ReflectionReport {
        recorded: 0,
        model_error: None,
        cost_usd: 0.02,
        events: vec![AgentEvent::BudgetTick {
            spent_usd: 0.02,
            limit_usd: Some(0.2),
            mode: BudgetMode::Enforced,
        }],
    };

    settle_reflection_budget(&mut report, &mut guard);

    assert!((guard.spent_usd() - 0.82).abs() < f64::EPSILON);
    let ticks = report
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::BudgetTick {
                spent_usd,
                limit_usd,
                ..
            } => Some((*spent_usd, *limit_usd)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ticks.len(), 1);
    assert!((ticks[0].0 - 0.82).abs() < f64::EPSILON);
    assert_eq!(ticks[0].1, Some(1.0));
}

#[test]
fn budget_flag_configures_the_session_axis_not_the_turn_axis() {
    // `--budget` must cap the whole run, so its limit lives on the session
    // axis (which `begin_turn` never resets) and the turn axis stays unset.
    let guard = build_budget_guard(Some(5.0));
    assert_eq!(guard.mode(), BudgetMode::Enforced);
    assert_eq!(guard.session_limit_usd(), Some(5.0));
    assert_eq!(
        guard.turn_limit_usd(),
        None,
        "the CLI limit must not land on the per-turn axis"
    );

    // No flag still meters (observed) but never gates.
    let unbounded = build_budget_guard(None);
    assert_eq!(unbounded.mode(), BudgetMode::Observed);
    assert_eq!(unbounded.session_limit_usd(), None);
    assert_eq!(unbounded.turn_limit_usd(), None);
}

#[test]
fn budget_cap_holds_across_turns_rather_than_resetting_each_one() {
    use stella_core::BudgetOutcome;
    use stella_core::budget::BudgetAxis;

    // A multi-turn session (REPL, deck, or goal round) calls `begin_turn` at
    // the top of every turn. Each turn here is individually under the $1.00
    // limit, but their sum is not — the session axis must trip on the second
    // turn instead of the per-turn reset handing back the full limit again.
    let mut budget = build_budget_guard(Some(1.0));

    budget.begin_turn();
    assert_eq!(budget.record_spend(0.6), BudgetOutcome::Continue);

    budget.begin_turn();
    match budget.record_spend(0.6) {
        BudgetOutcome::AbortTurn {
            axis: BudgetAxis::Session,
            spent_usd,
            limit_usd,
        } => {
            assert!((spent_usd - 1.2).abs() < 1e-9);
            assert_eq!(limit_usd, 1.0);
        }
        other => panic!("expected a session-axis abort across turns, got {other:?}"),
    }
}

#[test]
fn remaining_budget_tracks_session_headroom() {
    let mut guard = build_budget_guard(Some(2.0));
    assert_eq!(remaining_budget(&guard), Some(2.0));

    guard.begin_turn();
    guard.record_spend(0.5);
    assert!((remaining_budget(&guard).unwrap() - 1.5).abs() < 1e-9);

    // Headroom survives a turn reset — it is session-scoped, not turn-scoped.
    guard.begin_turn();
    assert!((remaining_budget(&guard).unwrap() - 1.5).abs() < 1e-9);
    guard.record_spend(3.0);
    assert_eq!(remaining_budget(&guard), Some(0.0));

    // No configured limit means no headroom to report.
    assert_eq!(remaining_budget(&build_budget_guard(None)), None);
}

#[path = "agent_tests/usage_completeness.rs"]
mod usage_completeness;

/// A `Config` selecting `provider_id` at its default model, carrying
/// `engine_settings` — the `agent_engine_config` variant of [`cfg_for`] for
/// `resolve_engine_wiring` tests.
fn cfg_with_engine(provider_id: &str, engine_settings_json: &str) -> Config {
    let mut cfg = cfg_for(provider_id);
    cfg.engine_settings =
        Some(serde_json::from_str(engine_settings_json).expect("valid agent_engine_config json"));
    cfg
}

#[test]
fn pipeline_worker_model_is_inert_without_the_fix_but_routes_with_it() {
    // Issue #276: `pipeline_worker_model` (and `agents.worker.*`) must
    // actually change what `Role::Worker` resolves to — previously the
    // worker always rode `worker_ref` (the session default), no matter what
    // this setting said.
    let cfg = cfg_with_engine(
        "zai", // session default: zai/glm-5.2
        r#"{ "pipeline_worker_model": "anthropic/claude-fable-5" }"#,
    );
    let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());
    let configured = vec![configured_provider("zai"), configured_provider("anthropic")];

    let wiring = resolve_engine_wiring(&cfg, &model_ref, &configured);

    let overridden = ModelRef::new("anthropic", "claude-fable-5");
    assert_eq!(
        wiring.worker_model, overridden,
        "the wiring's own worker_model must reflect the pipeline_worker_model override"
    );
    assert_eq!(
        wiring.pins.get(Role::Worker),
        Some(&overridden),
        "Role::Worker must be pinned to the configured worker model"
    );
    assert_eq!(
        wiring.pins.get(Role::Plan),
        Some(&overridden),
        "Role::Plan shares the worker's tier and must follow the override too, or plan/witness \
         turns would silently keep running on the session default"
    );
    assert!(
        wiring
            .extra_providers
            .iter()
            .any(|(model_ref, _)| *model_ref == overridden),
        "an adapter for the overridden worker model must be built"
    );

    // The full round trip through the actual router (what `resolve_provider`
    // in `stella-pipeline` calls) must resolve BOTH roles to the override,
    // not just the raw pin table.
    let breaker = CircuitBreaker::new(Box::new(SystemClock::new()));
    let router = Router::new(wiring.pins.clone(), wiring.profiles.clone(), breaker);
    assert_eq!(
        router.resolve(Role::Worker).unwrap().model_ref,
        overridden,
        "the router must actually route worker turns to the override"
    );
    assert_eq!(
        router.resolve(Role::Plan).unwrap().model_ref,
        overridden,
        "the router must actually route plan turns to the override"
    );
}

/// Issue #272: `stella init`'s summary line must surface generated/minified
/// exclusion count, not let excluded files silently vanish from the totals.
/// Tested on the pure builder/output functions, never a live TTY.
#[test]
fn format_graph_stats_reports_generated_skip_count_when_nonzero() {
    let summary = GraphSummary {
        total_symbols: 12,
        total_imports: 4,
        total_files: 3,
        files_parsed: 2,
        files_unchanged: 1,
        files_skipped_generated: 5,
    };
    let line = format_graph_stats(&summary);
    assert!(
        line.contains("skipped 5 generated files"),
        "line should surface the skip count: {line}"
    );
    assert!(line.contains("12 symbols"), "{line}");
}

#[test]
fn format_graph_stats_omits_the_skip_clause_when_nothing_was_skipped() {
    let summary = GraphSummary {
        total_symbols: 1,
        total_imports: 0,
        total_files: 1,
        files_parsed: 1,
        files_unchanged: 0,
        files_skipped_generated: 0,
    };
    let line = format_graph_stats(&summary);
    assert!(
        !line.contains("skipped"),
        "no skip clause when nothing was excluded: {line}"
    );
}

#[test]
fn worker_model_unset_falls_back_to_the_session_default() {
    // No `pipeline_worker_model`/`agents.worker.*` configured at all: the
    // worker must behave exactly as before this fix — riding the session
    // default, no pin, no extra adapter.
    let cfg = cfg_with_engine(
        "zai",
        r#"{ "pipeline_judge_model": "anthropic/claude-fable-5" }"#,
    );
    let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());
    let configured = vec![configured_provider("zai"), configured_provider("anthropic")];

    let wiring = resolve_engine_wiring(&cfg, &model_ref, &configured);

    assert_eq!(
        wiring.worker_model, model_ref,
        "with no worker override, worker_model falls back to the session default"
    );
    assert!(
        wiring.pins.get(Role::Worker).is_none(),
        "no worker pin is recorded when unconfigured"
    );
    assert!(
        wiring.pins.get(Role::Plan).is_none(),
        "no plan pin is recorded when unconfigured"
    );
}

#[test]
fn worker_model_with_no_resolvable_credential_falls_back_and_notices() {
    // The configured worker provider has no credential available: the
    // override must degrade to the session default (soft failure), with a
    // human-readable notice — never a hard error, matching triage/judge's
    // existing posture. An explicit `agents.worker.provider` pin (rather
    // than a flat-key `provider/slug` string) is what exercises this path:
    // an unconfigured provider named only inside a flat-key string is not
    // even recognized as a provider prefix (`model_spec_for`'s `is_provider`
    // gate), so it never reaches `pin_role`'s credential lookup at all.
    let cfg = cfg_with_engine(
        "zai",
        r#"{ "agents": { "worker": { "provider": "anthropic" } } }"#,
    );
    let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());
    let configured = vec![configured_provider("zai")]; // anthropic NOT configured

    let wiring = resolve_engine_wiring(&cfg, &model_ref, &configured);

    assert_eq!(
        wiring.worker_model, model_ref,
        "an unroutable worker override must fall back to the session default"
    );
    assert!(wiring.pins.get(Role::Worker).is_none());
    assert!(
        wiring
            .notices
            .iter()
            .any(|n| n.contains("worker") && n.contains("anthropic")),
        "a skipped worker override must be reported: {:?}",
        wiring.notices
    );
}

#[test]
fn worker_override_equal_to_the_session_default_still_pins_without_a_duplicate_adapter() {
    // Configuring the worker to the SAME model the session already defaults
    // to must not build a redundant second adapter (mirrors the existing
    // triage/judge "same instance" optimization) — but the pin is still
    // recorded, matching that established behavior.
    let cfg = cfg_with_engine("zai", r#"{ "pipeline_worker_model": "zai/glm-5.2" }"#);
    let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());
    let configured = vec![configured_provider("zai")];

    let wiring = resolve_engine_wiring(&cfg, &model_ref, &configured);

    assert_eq!(wiring.worker_model, model_ref);
    assert_eq!(wiring.pins.get(Role::Worker), Some(&model_ref));
    assert!(
        wiring.extra_providers.is_empty(),
        "no extra adapter is needed when the override equals the session default"
    );
}

#[test]
fn worker_override_shifts_the_judges_cross_family_comparison() {
    // Issue #276's router-correctness corollary: once the worker is
    // overridden, auto-mode judge selection (and the router's own unpinned-
    // judge cross-family fallback) must compare against the model the
    // worker ACTUALLY resolves to, not the stale session default — else a
    // judge could silently collapse to the same family as the real worker.
    let cfg = cfg_with_engine(
        "zai",
        r#"{ "pipeline_worker_model": "anthropic/claude-fable-5",
             "auto_mode": "on",
             "allowed_models": ["anthropic/claude-fable-5", "zai/glm-5.2"] }"#,
    );
    let model_ref = ModelRef::new(cfg.provider.id, cfg.model_id.clone());
    let configured = vec![configured_provider("zai"), configured_provider("anthropic")];

    let wiring = resolve_engine_wiring(&cfg, &model_ref, &configured);

    // The worker now runs on Anthropic; auto-mode must pick the cross-family
    // candidate (zai) as judge, not Anthropic again (which the STALE
    // "worker family = zai" comparison would have wrongly treated as
    // cross-family).
    assert_eq!(
        wiring.pins.get(Role::Judge),
        Some(&ModelRef::new("zai", "glm-5.2")),
        "auto-mode judge selection must be cross-family from the OVERRIDDEN worker, not the \
         session default"
    );
}

#[test]
fn format_graph_stats_uses_singular_file_for_a_count_of_one() {
    let summary = GraphSummary {
        total_symbols: 0,
        total_imports: 0,
        total_files: 0,
        files_parsed: 0,
        files_unchanged: 0,
        files_skipped_generated: 1,
    };
    let line = format_graph_stats(&summary);
    assert!(line.contains("skipped 1 generated file"), "{line}");
    assert!(
        !line.contains("generated files"),
        "singular, not plural, for a count of one: {line}"
    );
}

/// End-to-end through the real builder `stella init` calls
/// ([`index_workspace_graph_blocking`]): a `*.min.*` file sitting at the
/// workspace root (no denied directory involved) must be excluded and
/// counted, while an ordinary file alongside it indexes normally.
#[test]
fn index_workspace_graph_blocking_reports_generated_skips_end_to_end() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("app.min.js"), "function refresh(){}\n").unwrap();
    std::fs::write(ws.path().join("main.rs"), "pub fn run() {}\n").unwrap();

    let summary = index_workspace_graph_blocking(ws.path()).expect("index build succeeds");
    assert_eq!(summary.total_files, 1, "the minified file is never indexed");
    assert_eq!(summary.files_skipped_generated, 1);

    let line = format_graph_stats(&summary);
    assert!(line.contains("skipped 1 generated file"), "{line}");
}
