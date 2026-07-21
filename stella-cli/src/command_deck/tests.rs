use super::skills::{
    build_skill_creation_prompt, extract_skill_md, extract_skill_md_from_use, parse_installs_count,
    parse_skill_hits, rank_hits,
};
use super::*;

#[test]
fn deck_arg_commands_parse_models_forms_and_leave_sentences_as_prompts() {
    assert!(matches!(
        parse_models_command("/models refresh"),
        Some(ModelsCommand::Refresh { force: false })
    ));
    assert!(matches!(
        parse_models_command("/models refresh --force"),
        Some(ModelsCommand::Refresh { force: true })
    ));
    assert!(matches!(
        parse_models_command("/models list"),
        Some(ModelsCommand::List)
    ));
    // One unrecognized token is a typo'd subcommand → usage, never a
    // model call; a sentence stays a prompt.
    assert!(matches!(
        parse_models_command("/models refrsh"),
        Some(ModelsCommand::Usage(_))
    ));
    assert!(parse_models_command("/models what can I use").is_none());
    // Bare forms and non-command paths are not arg commands — and the
    // removed `/model-<role>` heads no longer parse (model config lives
    // on the SETTINGS tab).
    assert!(parse_models_command("/models").is_none());
    assert!(parse_models_command("/model-default zai/glm-5.2").is_none());
    assert!(parse_models_command("/src/main.rs explain").is_none());
}

#[test]
fn parse_skill_hits_strips_ansi_and_extracts_id_installs_url() {
    // The real `npx skills find` shape: ANSI SGR codes, an "Install with"
    // instruction line, result rows, and `└ url` continuation lines.
    let out = "\n\u{1b}[38;5;102mInstall with\u{1b}[0m npx skills add <owner/repo@skill>\n\n\
\u{1b}[38;5;145mwshobson/agents@rust-async-patterns\u{1b}[0m \u{1b}[36m15.8K installs\u{1b}[0m\n\
\u{1b}[38;5;102m└ https://skills.sh/wshobson/agents/rust-async-patterns\u{1b}[0m\n\n\
\u{1b}[38;5;145mapollographql/skills@rust-best-practices\u{1b}[0m \u{1b}[36m13.9K installs\u{1b}[0m\n\
\u{1b}[38;5;102m└ https://skills.sh/apollographql/skills/rust-best-practices\u{1b}[0m\n";
    let hits = parse_skill_hits(out);
    assert_eq!(hits.len(), 2, "only the two result rows: {hits:?}");
    assert_eq!(hits[0].id, "wshobson/agents@rust-async-patterns");
    assert_eq!(hits[0].installs, "15.8K installs");
    assert_eq!(hits[0].installs_rank, 15_800);
    assert_eq!(
        hits[0].url,
        "https://skills.sh/wshobson/agents/rust-async-patterns"
    );
    assert_eq!(hits[1].id, "apollographql/skills@rust-best-practices");
    assert_eq!(hits[1].installs_rank, 13_900);
    // Never leak escape codes or the instruction line into a hit.
    for h in &hits {
        assert!(!h.id.contains('\u{1b}') && !h.id.contains('['), "{h:?}");
        assert!(
            !h.id.contains("Install"),
            "instruction line rejected: {h:?}"
        );
    }
}

#[test]
fn parse_skill_hits_rejects_rows_without_owner_repo_at_skill() {
    // A plain description line (no `@`) and the placeholder are not results.
    let out = "acme/auth  not a real hit\nInstall with npx skills add <owner/repo@skill>\n";
    assert!(parse_skill_hits(out).is_empty());
}

#[test]
fn parse_installs_count_handles_k_m_and_plain() {
    assert_eq!(parse_installs_count("15.8K installs"), 15_800);
    assert_eq!(parse_installs_count("9K installs"), 9_000);
    assert_eq!(parse_installs_count("2.5M installs"), 2_500_000);
    assert_eq!(parse_installs_count("342 installs"), 342);
    assert_eq!(parse_installs_count("installs"), 0);
}

#[test]
fn parse_skill_hits_caps_at_fifty() {
    let out = (0..100)
        .map(|i| format!("pkg/repo@skill-{i}  {i} installs"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(parse_skill_hits(&out).len(), 50);
}

#[test]
fn extract_skill_md_from_use_unwraps_the_wrapped_body() {
    let out = "You are being given a Skill.\n\nUse the following SKILL.md as your instructions:\n\n<SKILL.md>\n---\nname: rust-async\n---\n\n# Rust Async\n\nbody\n</SKILL.md>\n";
    let md = extract_skill_md_from_use(out);
    assert!(md.starts_with("---"), "starts at frontmatter: {md}");
    assert!(md.contains("# Rust Async"));
    assert!(
        !md.contains("You are being given"),
        "preamble dropped: {md}"
    );
    assert!(!md.contains("</SKILL.md>"), "close marker dropped: {md}");
}

#[test]
fn rank_hits_orders_by_relevance_then_popularity() {
    let hits = vec![
        SkillSearchHit {
            id: "a/pkg@pdf-extract".into(),
            installs: "120 installs".into(),
            installs_rank: 120,
            url: String::new(),
        },
        SkillSearchHit {
            id: "b/pkg@img-resize".into(),
            installs: "9K installs".into(),
            installs_rank: 9_000,
            url: String::new(),
        },
        SkillSearchHit {
            id: "c/pkg@pdf-reader".into(),
            installs: "5 installs".into(),
            installs_rank: 5,
            url: String::new(),
        },
    ];
    let ranked = rank_hits(&hits, "extract tables from pdf");
    assert!(ranked[0].contains("a/pkg@pdf-extract"), "{ranked:?}");
    assert!(
        ranked.iter().position(|l| l.contains("a/pkg"))
            < ranked.iter().position(|l| l.contains("b/pkg")),
        "relevance beats popularity: {ranked:?}"
    );
}

#[test]
fn build_skill_creation_prompt_includes_request_and_ranked_candidates() {
    let p = build_skill_creation_prompt(
        "format sql nicely",
        &["a/sql-fmt  sql formatter".to_string()],
    );
    assert!(p.contains("format sql nicely"));
    assert!(p.contains("a/sql-fmt"));
    assert!(p.contains("SINGLE skill"));
    let empty = build_skill_creation_prompt("do a thing", &[]);
    assert!(empty.contains("from scratch"));
}

#[test]
fn extract_skill_md_unwraps_a_fenced_block_or_frontmatter() {
    let fenced = "Here you go:\n```markdown\n---\nname: x\ndescription: d\n---\nbody\n```\ndone";
    let got = extract_skill_md(fenced);
    assert!(got.starts_with("---"), "{got}");
    assert!(got.ends_with("body"), "{got}");
    let bare = "prose\n---\nname: y\ndescription: d\n---\nbody";
    assert!(extract_skill_md(bare).starts_with("---\nname: y"));
}

/// A minimal inner executor that always succeeds (or always errors).
struct FakeInner {
    error: bool,
}

#[async_trait]
impl ToolExecutor for FakeInner {
    fn schemas(&self) -> Vec<ToolSchema> {
        vec![]
    }
    async fn execute(&self, name: &str, _input: &Value) -> ToolOutput {
        if self.error {
            ToolOutput::Error {
                message: format!("{name} failed"),
            }
        } else {
            ToolOutput::Ok {
                content: format!("{name} ok"),
            }
        }
    }
}

fn recv_file_change(rx: &mut UnboundedReceiver<AgentEvent>) -> Option<AgentEvent> {
    rx.try_recv().ok()
}

#[tokio::test]
async fn tap_emits_created_for_write_file_to_a_new_path() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let inner = FakeInner { error: false };
    let tap = FileChangeTap {
        inner: &inner,
        events: tx,
        root: dir.path().to_path_buf(),
    };
    let input = serde_json::json!({ "path": "src/new.rs", "content": "a\nb\n" });
    let out = tap.execute("write_file", &input).await;
    assert!(!out.is_error());
    match recv_file_change(&mut rx) {
        Some(AgentEvent::FileChange { path, kind, diff }) => {
            assert_eq!(path, "src/new.rs");
            assert_eq!(kind, FileChangeKind::Created);
            assert_eq!(diff.as_deref(), Some("+a\n+b\n"));
        }
        other => panic!("expected FileChange, got {other:?}"),
    }
}

#[tokio::test]
async fn tap_emits_modified_for_write_file_over_an_existing_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.txt"), "old").unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let inner = FakeInner { error: false };
    let tap = FileChangeTap {
        inner: &inner,
        events: tx,
        root: dir.path().to_path_buf(),
    };
    let input = serde_json::json!({ "path": "x.txt", "content": "new" });
    tap.execute("write_file", &input).await;
    match recv_file_change(&mut rx) {
        Some(AgentEvent::FileChange { kind, .. }) => {
            assert_eq!(kind, FileChangeKind::Modified)
        }
        other => panic!("expected FileChange, got {other:?}"),
    }
}

#[tokio::test]
async fn tap_builds_edit_file_diff_from_old_and_new_strings() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let inner = FakeInner { error: false };
    let tap = FileChangeTap {
        inner: &inner,
        events: tx,
        root: dir.path().to_path_buf(),
    };
    let input = serde_json::json!({
        "path": "src/lib.rs",
        "old_string": "fn a() {}",
        "new_string": "fn a() {}\nfn b() {}",
    });
    tap.execute("edit_file", &input).await;
    match recv_file_change(&mut rx) {
        Some(AgentEvent::FileChange { kind, diff, .. }) => {
            assert_eq!(kind, FileChangeKind::Modified);
            let diff = diff.expect("edit_file carries a pseudo-diff");
            assert!(diff.contains("-fn a() {}"));
            assert!(diff.contains("+fn b() {}"));
        }
        other => panic!("expected FileChange, got {other:?}"),
    }
}

#[tokio::test]
async fn tap_reads_the_file_before_delete_for_the_removed_diff() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("gone.txt"), "one\ntwo\n").unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let inner = FakeInner { error: false };
    let tap = FileChangeTap {
        inner: &inner,
        events: tx,
        root: dir.path().to_path_buf(),
    };
    let input = serde_json::json!({ "path": "gone.txt" });
    tap.execute("delete_file", &input).await;
    match recv_file_change(&mut rx) {
        Some(AgentEvent::FileChange { kind, diff, .. }) => {
            assert_eq!(kind, FileChangeKind::Deleted);
            assert_eq!(diff.as_deref(), Some("-one\n-two\n"));
        }
        other => panic!("expected FileChange, got {other:?}"),
    }
}

#[tokio::test]
async fn tap_stays_silent_for_errors_and_non_file_tools() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let failing = FakeInner { error: true };
    let tap = FileChangeTap {
        inner: &failing,
        events: tx,
        root: dir.path().to_path_buf(),
    };
    let input = serde_json::json!({ "path": "x", "content": "y" });
    let out = tap.execute("write_file", &input).await;
    assert!(out.is_error());
    assert!(recv_file_change(&mut rx).is_none(), "no event on error");

    let (tx, mut rx) = mpsc::unbounded_channel();
    let inner = FakeInner { error: false };
    let tap = FileChangeTap {
        inner: &inner,
        events: tx,
        root: dir.path().to_path_buf(),
    };
    tap.execute("grep", &serde_json::json!({ "pattern": "x" }))
        .await;
    assert!(
        recv_file_change(&mut rx).is_none(),
        "non-file read-only tools emit nothing"
    );
}

#[tokio::test]
async fn tap_emits_a_diffless_read_event_for_successful_reads_only() {
    // A successful read rides the FileChange path (kind Read, no diff) so
    // the Files tab counts reads; a failed read stays silent.
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let inner = FakeInner { error: false };
    let tap = FileChangeTap {
        inner: &inner,
        events: tx,
        root: dir.path().to_path_buf(),
    };
    tap.execute("read_file", &serde_json::json!({ "path": "src/lib.rs" }))
        .await;
    match recv_file_change(&mut rx) {
        Some(AgentEvent::FileChange { path, kind, diff }) => {
            assert_eq!(path, "src/lib.rs");
            assert_eq!(kind, FileChangeKind::Read);
            assert_eq!(diff, None, "reads never carry a diff");
        }
        other => panic!("expected FileChange, got {other:?}"),
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let failing = FakeInner { error: true };
    let tap = FileChangeTap {
        inner: &failing,
        events: tx,
        root: dir.path().to_path_buf(),
    };
    let out = tap
        .execute("read_file", &serde_json::json!({ "path": "ghost.rs" }))
        .await;
    assert!(out.is_error());
    assert!(recv_file_change(&mut rx).is_none(), "no event on error");
}

#[test]
fn mcp_outcome_report_lists_connected_servers_by_name() {
    let report = crate::mcp_cmd::mcp_outcome_report(&["files", "search"], &[]);
    assert_eq!(report, "2 MCP server(s) connected: files, search");
}

#[test]
fn mcp_outcome_report_names_each_failure_with_its_reason() {
    let failed = vec![(
        "slow".to_string(),
        "connect timed out after 10000ms".to_string(),
    )];
    let report = crate::mcp_cmd::mcp_outcome_report(&["files"], &failed);
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines[0], "1 MCP server(s) connected: files");
    assert_eq!(
        lines[1],
        "MCP server `slow` unavailable: connect timed out after 10000ms"
    );
}

#[test]
fn mcp_outcome_report_states_total_failure_outright() {
    let failed = vec![("a".to_string(), "spawn failed".to_string())];
    let report = crate::mcp_cmd::mcp_outcome_report(&[], &failed);
    assert!(
        report.starts_with("no MCP servers connected"),
        "the degraded mode is stated, not implied: {report}"
    );
    assert!(report.contains("MCP server `a` unavailable: spawn failed"));
}

#[test]
fn pseudo_diff_caps_each_side_with_an_uncounted_elision_line() {
    let big: String = (0..300).map(|i| format!("line {i}\n")).collect();
    let diff = pseudo_diff(&big, "");
    let minus = diff.lines().filter(|l| l.starts_with('-')).count();
    assert_eq!(minus, PSEUDO_DIFF_MAX_LINES);
    assert!(diff.contains("(100 more lines)"));
    // The elision marker must not read as a change line.
    assert!(diff.lines().any(|l| l.starts_with(' ')));
}

/// Drive [`DeckAskUserIo::prompt`] with a scripted answer and inspect the
/// Inbound stream it produces. The answer is sent only AFTER the AskUser
/// card appears: `prompt` drains stale answers before presenting (the
/// cancelled-turn contract), so a pre-sent answer would be swallowed and
/// the await would hang.
async fn run_prompt(options: &[&str], answer: &str) -> (Result<String, String>, Vec<Inbound>) {
    let (in_tx, mut in_rx) = mpsc::unbounded_channel();
    let (ans_tx, ans_rx) = mpsc::unbounded_channel();
    let io = DeckAskUserIo {
        agent: "lead".into(),
        inbound: in_tx,
        answers: Arc::new(tokio::sync::Mutex::new(ans_rx)),
    };
    let opts: Vec<String> = options.iter().map(|s| s.to_string()).collect();
    let asking = tokio::spawn(async move { io.prompt("which one?", &opts).await });
    let mut seen = Vec::new();
    seen.push(in_rx.recv().await.expect("the AskUser card is presented"));
    ans_tx.send(answer.to_string()).unwrap();
    let result = asking.await.expect("the prompt task settles");
    while let Ok(inbound) = in_rx.try_recv() {
        seen.push(inbound);
    }
    (result, seen)
}

#[tokio::test]
async fn deck_ask_io_strips_the_free_text_option_and_maps_answers_to_indices() {
    let free = format!("{FREE_TEXT_LABEL}…");
    let (result, seen) = run_prompt(&["postgres", "sqlite", free.as_str()], "sqlite").await;
    // The picked option maps to its 1-based index, the shape
    // execute_ask_user's numeric parser expects.
    assert_eq!(result.unwrap(), "2");
    match &seen[0] {
        Inbound::Event {
            event: AgentEvent::AskUser { options, .. },
            ..
        } => {
            assert_eq!(options, &vec!["postgres".to_string(), "sqlite".to_string()]);
        }
        other => panic!("expected the AskUser card first, got {other:?}"),
    }
}

#[tokio::test]
async fn deck_ask_io_echoes_the_clearing_tool_result_with_the_card_id() {
    let (_, seen) = run_prompt(&["a", "b"], "b").await;
    let card_id = match &seen[0] {
        Inbound::Event {
            event: AgentEvent::AskUser { id, .. },
            ..
        } => id.clone(),
        other => panic!("expected AskUser, got {other:?}"),
    };
    match &seen[1] {
        Inbound::Event {
            event: AgentEvent::ToolResult {
                call_id, output, ..
            },
            ..
        } => {
            assert_eq!(*call_id, card_id, "the echo clears the exact card");
            assert!(!output.is_error());
        }
        other => panic!("expected the echoed ToolResult, got {other:?}"),
    }
}

#[tokio::test]
async fn deck_ask_io_passes_free_text_through_verbatim() {
    let (result, _) = run_prompt(&["a", "b"], "actually do it my way").await;
    assert_eq!(result.unwrap(), "actually do it my way");
}

// ── Double-Esc hold ─────────────────────────────────────────────────────

/// Single Esc: the plain cancel retains the prompt but never parks
/// dispatch — "interrupt current, run next" is unchanged.
#[test]
fn plain_cancel_retains_without_holding() {
    let mut dispatch = HoldState::new();
    dispatch.cancelled("prompt a");
    assert!(!dispatch.held(), "single Esc must not park dispatch");
}

/// The pair with an empty backlog: the escalation lands at the idle recv
/// (its `Stop` was consumed first — the channel is FIFO), and must still
/// requeue the prompt that cancel dropped and park dispatch. This is the
/// sequence that used to fall into the stray-input arm and vanish.
#[test]
fn stop_and_hold_requeues_the_prompt_the_first_esc_cancelled() {
    let mut dispatch = HoldState::new();
    dispatch.cancelled("prompt a");
    assert_eq!(dispatch.stop_and_hold(None), vec!["prompt a".to_string()]);
    assert!(dispatch.held(), "double Esc parks dispatch");
    // The retention was consumed: a re-sent escalation has nothing more
    // to requeue.
    assert!(dispatch.stop_and_hold(None).is_empty());
}

/// The pair with a backlog: the gap between its two messages is where
/// the driver auto-dispatches the next queued prompt, so the escalation
/// cancels THAT turn. Both prompts return — the retained one in front of
/// the auto-dispatched one (push order is front-most last), the order
/// the user last saw.
#[test]
fn stop_and_hold_restores_the_backlog_order_the_user_saw() {
    let mut dispatch = HoldState::new();
    dispatch.cancelled("prompt a"); // first Esc: A dropped, B dispatched
    assert_eq!(
        dispatch.stop_and_hold(Some("prompt b")), // second Esc during B
        vec!["prompt b".to_string(), "prompt a".to_string()],
    );
    assert!(dispatch.held());
}

/// A submission releases the hold, and each plain cancel replaces the
/// retention — the escalation only ever requeues its own pair's prompt.
#[test]
fn release_and_overwrite_scope_retention_to_the_latest_pair() {
    let mut dispatch = HoldState::new();
    dispatch.cancelled("stale");
    dispatch.cancelled("fresh");
    assert_eq!(dispatch.stop_and_hold(None), vec!["fresh".to_string()]);
    dispatch.release();
    assert!(!dispatch.held(), "the next submission releases the hold");
}

/// A stray escalation with nothing retained and nothing in flight stays
/// the documented no-op — nothing to requeue, nothing to hold.
#[test]
fn stray_stop_and_hold_is_a_no_op() {
    let mut dispatch = HoldState::new();
    assert!(dispatch.stop_and_hold(None).is_empty());
    assert!(!dispatch.held());
}

// ── ISSUES tab: entity-hit assembly ─────────────────────────────────────

#[test]
fn member_and_label_hits_carry_kind_insert_and_description() {
    let hit = member_hit(MemberInfo {
        handle: "@octocat".into(),
        name: Some("Octo Cat".into()),
        email: None,
    });
    assert_eq!(
        (hit.kind.as_str(), hit.label.as_str()),
        ("Person", "@octocat")
    );
    assert_eq!(hit.insert, "@octocat");
    assert_eq!(hit.description, "Octo Cat");

    // A Linear member's handle IS the email — never repeated in the
    // description.
    let hit = member_hit(MemberInfo {
        handle: "mona@example.com".into(),
        name: Some("Mona Lisa".into()),
        email: Some("mona@example.com".into()),
    });
    assert_eq!(hit.description, "Mona Lisa");

    let hit = label_hit(LabelInfo {
        name: "bug".into(),
        color: Some("d73a4a".into()),
        description: Some("Something is broken".into()),
    });
    assert_eq!((hit.kind.as_str(), hit.insert.as_str()), ("Label", "bug"));
    assert_eq!(hit.description, "Something is broken");
    // No description → the color stands in.
    let hit = label_hit(LabelInfo {
        name: "ci".into(),
        color: Some("00ff00".into()),
        description: None,
    });
    assert_eq!(hit.description, "00ff00");
}

#[test]
fn agent_entity_hits_filter_by_name_or_description_case_insensitively() {
    let entries = vec![
        stella_tui::InstalledAgentEntry {
            name: "reviewer".into(),
            description: "Reviews diffs".into(),
            tools: None,
            scope: AgentScope::Project,
            source_path: String::new(),
            version: 1,
            versions: vec![],
            content: String::new(),
        },
        stella_tui::InstalledAgentEntry {
            name: "planner".into(),
            description: "Plans work".into(),
            tools: None,
            scope: AgentScope::User,
            source_path: String::new(),
            version: 1,
            versions: vec![],
            content: String::new(),
        },
    ];
    let hits = agent_entity_hits(&entries, "REVIEW");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].kind, "Agent");
    assert_eq!(hits[0].insert, "reviewer");
    // Description text matches too; the empty query matches all.
    assert_eq!(agent_entity_hits(&entries, "plans")[0].label, "planner");
    assert_eq!(agent_entity_hits(&entries, "").len(), 2);
}

#[test]
fn memory_hits_carry_the_preview_provenance_and_citation_suffixes() {
    let hit = memory_hit(
        "naming-convention",
        "Prefer kebab-case for  skill names\nand slugs.",
        "2026-07-01T00:00:00Z",
        Some("2026-06-15T00:00:00Z"),
        Some((12, 0.9)),
    );
    assert_eq!(hit.kind, "Memory");
    assert_eq!(hit.insert, "naming-convention");
    assert_eq!(
        hit.description,
        "Prefer kebab-case for skill names and slugs. · observed \
         2026-07-01T00:00:00Z · valid from 2026-06-15T00:00:00Z · cited 12× avg 0.9"
    );

    // No valid_from → observation time stands in; no citations → no
    // suffix; a long content truncates char-safe with an ellipsis.
    let long = "x".repeat(200);
    let hit = memory_hit("m", &long, "2026-07-01", None, None);
    assert!(
        hit.description
            .starts_with(&"x".repeat(MEMORY_PREVIEW_CHARS - 1))
    );
    assert!(
        hit.description
            .contains("… · observed 2026-07-01 · valid from 2026-07-01")
    );
    assert!(!hit.description.contains("cited"));
}

#[test]
fn symbol_hits_take_the_bare_name_and_the_file_location() {
    let frame = ocp_types::ContextFrame {
        id: "code-graph:sym:src/lib.rs:12:issue_row".into(),
        kind: ocp_types::FrameKind::Symbol,
        title: "fn issue_row".into(),
        content: "fn issue_row(...) { ... }".into(),
        uri: Some("file:///repo/src/lib.rs".into()),
        score: 0.9,
        token_cost: 10,
        valid_from: None,
        valid_to: None,
        recorded_at: None,
        provenance: vec![],
        citation_label: Some("fn issue_row (src/lib.rs:12)".into()),
        embedding: None,
        relations: vec![],
    };
    let hit = symbol_hit(&frame);
    assert_eq!(hit.kind, "Symbol");
    assert_eq!(hit.label, "fn issue_row");
    assert_eq!(hit.insert, "issue_row", "the bare name is what inserts");
    assert_eq!(hit.description, "src/lib.rs:12");

    // Without a citation label the frame's uri stands in.
    let mut bare = frame;
    bare.citation_label = None;
    assert_eq!(symbol_hit(&bare).description, "file:///repo/src/lib.rs");
}

#[test]
fn merge_assignee_hits_orders_tracker_agents_local_and_caps() {
    let person = |l: &str| EntityHit {
        kind: "Person".into(),
        label: l.into(),
        description: String::new(),
        insert: l.into(),
    };
    let tracker: Vec<EntityHit> = (0..3).map(|i| person(&format!("p{i}"))).collect();
    let agents: Vec<EntityHit> = (0..2).map(|i| person(&format!("a{i}"))).collect();
    let local: Vec<EntityHit> = (0..3).map(|i| person(&format!("m{i}"))).collect();
    let merged = merge_assignee_hits(tracker, agents, local, 6);
    let labels: Vec<&str> = merged.iter().map(|h| h.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["p0", "p1", "p2", "a0", "a1", "m0"],
        "tracker first, then agents, then local — capped"
    );
}

#[test]
fn issue_rows_map_field_for_field() {
    let row = issue_row(IssueSummary {
        key: "ENG-42".into(),
        title: "Fix".into(),
        state: "open".into(),
        labels: vec!["bug".into()],
        assignee: Some("mona@example.com".into()),
        url: "https://linear.app/x/issue/ENG-42".into(),
        updated_at: Some("2026-07-18T00:00:00Z".into()),
    });
    assert_eq!(row.key, "ENG-42");
    assert_eq!(row.labels, vec!["bug"]);
    assert_eq!(row.assignee.as_deref(), Some("mona@example.com"));
    assert_eq!(row.updated_at.as_deref(), Some("2026-07-18T00:00:00Z"));
}

#[test]
fn local_assignee_hits_read_as_empty_on_a_bare_workspace() {
    // Read-only politeness: no `.stella/` databases → no hits and, above
    // all, no directories/files created as a side effect.
    let dir = tempfile::tempdir().unwrap();
    assert!(local_assignee_hits(dir.path(), "anything").is_empty());
    assert!(
        !dir.path().join(".stella").exists(),
        "a lookup must never create the workspace store"
    );
}

/// `requeue_front` front-inserts in push order and mirrors every insert
/// to the deck as `PromptRequeued`, so the driver's backlog and the
/// deck's queue view (which front-inserts each mirror in turn) agree.
#[test]
fn requeue_front_mirrors_each_front_insert_to_the_deck() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let dir = std::env::temp_dir().join(format!("stella-requeue-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut queue = crate::session_persist::DurableQueue::fresh(dir.clone());
    queue.push_back("c".to_string());
    requeue_front(&mut queue, &tx, vec!["b".to_string(), "a".to_string()]);
    // The backlog is durable + write-through: the authoritative order is
    // ON DISK the moment the inserts return.
    assert_eq!(queue.len(), 3);
    assert_eq!(
        stella_store::journal::read_queue(&dir),
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
    let _ = std::fs::remove_dir_all(&dir);
    for expected in ["b", "a"] {
        match rx.try_recv() {
            Ok(Inbound::PromptRequeued { agent, text }) => {
                assert_eq!(agent, LEAD);
                assert_eq!(text, expected);
            }
            other => panic!("expected PromptRequeued({expected}), got {other:?}"),
        }
    }
    assert!(rx.try_recv().is_err(), "exactly one mirror per insert");
}
