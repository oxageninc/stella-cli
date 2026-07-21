use super::*;
use crate::config::{ConfiguredProvider, PROVIDERS, ProviderConfig};
use stella_model::credential::ApiKey;

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
        step: 0,
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

    let first = assemble_system_prompt(SYSTEM_PROMPT, root.path());
    let second = assemble_system_prompt(SYSTEM_PROMPT, root.path());
    assert_eq!(first, second, "same workspace state ⇒ identical bytes");
    assert!(first.contains("## Project scripts"), "section present");
    assert!(first.contains("build → pnpm run build"), "{first}");
    assert!(first.contains("install → pnpm install"), "{first}");

    let empty = tempfile::tempdir().expect("tempdir");
    let bare = assemble_system_prompt(SYSTEM_PROMPT, empty.path());
    assert!(
        !bare.contains("## Project scripts"),
        "no scripts → no section, no noise"
    );
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
    std::fs::create_dir_all(root.path().join(".stella")).unwrap();
    let db = root.path().join(".stella").join("codegraph.db");
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

/// Auto-build on session start (task part A): a workspace with a source
/// file but NO `.stella/codegraph.db` does not advertise `graph_query` on
/// turn 1; once [`spawn_session_graph`]'s background build completes the
/// tool is advertised AND dispatchable — no manual `stella init`, no
/// restart. Awaiting the returned handle is the deterministic "index
/// ready" signal.
#[tokio::test]
async fn spawn_session_graph_auto_builds_and_enables_graph_query() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("lib.rs"), "pub fn find_me() {}\n").unwrap();

    let registry = Arc::new(ToolRegistry::with_issue_backend(root.clone(), None));
    let advertises = |r: &ToolRegistry| r.schemas().iter().any(|s| s.name == "graph_query");

    // Turn 1: absent — no index on disk yet.
    assert!(!stella_tools::graph::graph_available(&root));
    assert!(
        !advertises(&registry),
        "graph_query must be absent before the index is built"
    );

    let (session_graph, build) =
        spawn_session_graph(&root, registry.clone(), Box::new(|_| {}), Box::new(|| {}));
    build.await.expect("background build task");

    // After the build: the db exists, the tool is advertised, and it
    // dispatches against the freshly built index.
    assert!(
        stella_tools::graph::graph_available(&root),
        "the background build must create .stella/codegraph.db"
    );
    assert!(
        advertises(&registry),
        "graph_query must be advertised once the index is built"
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

    let prompt = build_system_prompt(&cfg_for("zai"), root.path());
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

    let prompt = build_system_prompt(&cfg_for("zai"), root.path());
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
    let empty = build_system_prompt(&cfg_for("zai"), bare.path());
    assert!(!empty.contains("## Workspace maps"));
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
        workspace_root: std::path::PathBuf::from("/tmp"),
        base_url_override: None,
        hooks: None,
        engine_settings: None,
        tools_bash: false,
        tools_web: false,
        credential_source: None,
    }
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
