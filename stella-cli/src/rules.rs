//! Production wiring for the workspace rules engine (`stella_core::rules`):
//! the [`RuleSource`] backed by real directory reads and the workspace
//! store, plus the session glue that hard-enforces Tier-2 guards at the
//! tool boundary.
//!
//! All rule semantics — frontmatter parsing, precedence merging, Tier-1
//! rendering, Tier-2 guard evaluation — live in `stella-core` (no I/O,
//!); this module owns exactly the I/O half:
//! walking the rule directories, reading extension-authored rules out of
//! `.stella/private/store.db` (`stella_store::Store::list_rules`), and registering
//! the blocking policy handler that threads [`evaluate_guards`] into
//! `ToolRegistry::execute`'s `tool.call.requested` chain
//! (`stella_core::bus`). Tier-1 rendering into the system prompt happens in
//! `agent::assemble_system_prompt`, next to the workspace memories it rides
//! the prompt cache with.
//!
//! # Rule authoring format (the contract every writer targets)
//!
//! A rule is one markdown document: optional single-line `key: value`
//! frontmatter between `---` fences, then the rule statement as the body
//! (parsed by `stella_core::rules::rule_from_file`). Recognized keys:
//!
//! - `name:` — rule id (defaults to the filename stem)
//! - `description:` — one-line summary
//! - `guard-tool:` — the tool a Tier-2 guard pins: canonical `Bash` /
//!   `Write` / `Edit` / `Read` / `Delete` (see [`canonical_tool`]), an
//!   exact registry tool name, or `*` for any
//! - `guard-deny-path:` — glob blocking a file tool by path
//! - `guard-deny-command:` — glob blocking a `bash` command
//!
//! Any `guard-*` key makes the rule Tier 2 (hard-enforced); none keeps it
//! Tier 1 (prompt-only). Rule files live in `.stella/rules/*.md` (also
//! `.claude/rules/` and `~/.stella/rules/`) — the format promoted
//! rules are written in. The SAME markdown, as one string, is what
//! extension providers publish through `stella_store::Store::upsert_rule`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use stella_core::bus::{HookBus, HookDecision, names as hook_names};
use stella_core::rules::{
    LoadRulesOptions, ProposedAction, Rule, RuleFile, RuleSource, evaluate_guards, load_rules,
};
use stella_tools::ToolRegistry;

/// One immutable rule snapshot resolved at session assembly. Cloning this
/// value clones only the [`Arc`], so prompt rendering, the parent registry,
/// and every candidate registry observe the exact same source resolution.
#[derive(Clone, Default)]
pub(crate) struct ResolvedRules(Arc<[Rule]>);

impl ResolvedRules {
    pub(crate) fn as_slice(&self) -> &[Rule] {
        &self.0
    }
}

impl std::ops::Deref for ResolvedRules {
    type Target = [Rule];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

/// The production [`RuleSource`]: real `std::fs` reads over each directory
/// in `dirs`, in the given order — every `.md` file's contents, name-sorted
/// within a directory, absent directories skipped silently (exactly the
/// contract the port documents).
pub(crate) struct FsRuleSource;

impl RuleSource for FsRuleSource {
    fn read_rule_files(&self, dirs: &[String]) -> Vec<RuleFile> {
        let mut out = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|x| x.to_str()) == Some("md"))
                .collect();
            paths.sort();
            for path in paths {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    out.push(RuleFile {
                        path: path.display().to_string(),
                        contents,
                    });
                }
            }
        }
        out
    }
}

/// Extension-authored rules from the workspace store (`.stella/private/store.db`),
/// rendered as the [`RuleFile`]s the engine's merge consumes. The synthetic
/// `store://rules/<id>.md` path both labels the rule's provenance
/// ([`Rule::source`]) and yields the stored id as the filename stem, so a
/// rule without `name:` frontmatter still merges under the id it was
/// published as. Read only when the database already exists — rule loading
/// must never create one as a side effect — and best-effort like all
/// persistence: any failure means no store rules this session, never an
/// error.
fn store_rule_files(workspace_root: &Path) -> Vec<RuleFile> {
    if !matches!(
        stella_store::existing_workspace_private_sqlite_path(workspace_root, "store.db"),
        Ok(Some(_))
    ) {
        return Vec::new();
    }
    let Ok(store) = stella_store::Store::open(workspace_root) else {
        return Vec::new();
    };
    store
        .list_rules()
        .unwrap_or_default()
        .into_iter()
        .map(|row| RuleFile {
            path: format!("store://rules/{}.md", row.rule_id),
            contents: row.contents,
        })
        .collect()
}

/// The session's full rule source: extension-authored store rules first
/// (lowest precedence), then the on-disk rule files in directory order.
struct SessionRuleSource {
    store_files: Vec<RuleFile>,
    include_project: bool,
}

impl RuleSource for SessionRuleSource {
    fn read_rule_files(&self, dirs: &[String]) -> Vec<RuleFile> {
        let mut out = self.store_files.clone();
        let visible_dirs = if self.include_project {
            dirs
        } else {
            dirs.get(..1).unwrap_or(dirs)
        };
        out.extend(FsRuleSource.read_rule_files(visible_dirs));
        out
    }
}

/// Load the user rule directory and, when the effective authority permits
/// project prompts, the workspace store plus `.claude/rules` and
/// `.stella/rules`. Prompt-only rules retain the historical latest-wins
/// precedence, while user-global hard guards are monotonic: a project rule
/// with the same id can never remove them, and a distinct project guard is
/// added under a provenance-qualified id so both constraints remain armed.
/// Called once per session: the result feeds both the Tier-1 prompt
/// section and the Tier-2 guards, so the two enforcement surfaces can
/// never disagree about what was loaded.
pub(crate) fn load_workspace_rules(
    workspace_root: &Path,
    authority: &crate::settings::AuthorityPolicy,
) -> ResolvedRules {
    if crate::settings::filesystem_settings_disabled() {
        return ResolvedRules::default();
    }
    let user_rules_dir =
        crate::settings::user_home_dir().map(|home| home.join(".stella").join("rules"));
    ResolvedRules(
        load_rules_from_with_authority(
            workspace_root,
            user_rules_dir,
            authority.project_prompts_allowed,
        )
        .into(),
    )
}

/// [`load_workspace_rules`] over an explicit user rules directory — the seam
/// tests use so precedence and both authority branches are provable without
/// touching `$HOME`. `None` omits user rules; project sources still follow
/// `include_project`.
fn load_rules_from_with_authority(
    workspace_root: &Path,
    user_rules_dir: Option<PathBuf>,
    include_project: bool,
) -> Vec<Rule> {
    let user_opts = LoadRulesOptions {
        cwd: workspace_root.display().to_string(),
        user_rules_dir: user_rules_dir
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
    };
    let user_rules = load_rules(
        &SessionRuleSource {
            store_files: Vec::new(),
            include_project: false,
        },
        &user_opts,
    );
    if !include_project {
        return user_rules;
    }

    // Load the project tier independently. An empty first directory keeps
    // RuleSource's directory contract intact while preventing user files
    // from entering the lower-trust merge a second time.
    let project_opts = LoadRulesOptions {
        cwd: workspace_root.display().to_string(),
        user_rules_dir: String::new(),
    };
    let project_rules = load_rules(
        &SessionRuleSource {
            store_files: store_rule_files(workspace_root),
            include_project: true,
        },
        &project_opts,
    );
    merge_rule_trust_tiers(user_rules, project_rules)
}

/// Merge already-resolved trust tiers without allowing a lower-trust rule to
/// weaken a user hard guard. Prompt-only user rules retain the established
/// project-overrides-user behavior. When both tiers carry different guards
/// under one id, keep the user rule at the canonical id and add the project
/// rule under a stable qualified id so guard evaluation sees both.
fn merge_rule_trust_tiers(mut user_rules: Vec<Rule>, project_rules: Vec<Rule>) -> Vec<Rule> {
    for mut project_rule in project_rules {
        let same_id = user_rules
            .iter()
            .position(|user_rule| user_rule.id == project_rule.id);
        let Some(index) = same_id else {
            user_rules.push(project_rule);
            continue;
        };
        if user_rules[index].guard.is_none() {
            user_rules[index] = project_rule;
            continue;
        }
        if project_rule.guard.is_some() && project_rule.guard != user_rules[index].guard {
            let base = format!("{}@project", project_rule.id);
            let mut qualified = base.clone();
            let mut suffix = 2;
            while user_rules.iter().any(|rule| rule.id == qualified) {
                qualified = format!("{base}-{suffix}");
                suffix += 1;
            }
            project_rule.id = qualified;
            user_rules.push(project_rule);
        }
    }
    user_rules
}

/// Session wiring shorthand: load the workspace rules and attach their
/// Tier-2 guards to `registry`. Every session driver calls this right after
/// constructing its registry (same rhythm as `populate_schema_index`).
pub(crate) fn enforce_workspace_rules(
    registry: &ToolRegistry,
    workspace_root: &Path,
    authority: &crate::settings::AuthorityPolicy,
) -> ResolvedRules {
    let rules = load_workspace_rules(workspace_root, authority);
    attach_rule_guards(registry, &rules);
    rules
}

/// Map a registry tool name to the canonical vocabulary guards are authored
/// in (`guard-tool: Bash|Write|Edit|Read|Delete`, `stella_core::rules::
/// RuleGuard`). Unmapped names pass through unchanged, so a guard can still
/// pin an exact registry tool (`guard-tool: grep`) and `*` matches all.
fn canonical_tool(name: &str) -> &str {
    match name {
        "bash" => "Bash",
        "read_file" => "Read",
        "write_file" => "Write",
        "edit_file" => "Edit",
        "delete_file" => "Delete",
        other => other,
    }
}

/// Thread Tier-2 rule guards into the registry's tool boundary. Guarded
/// rules become one blocking policy handler on the `tool.call.requested`
/// chain, which `ToolRegistry::execute` consults before anything runs: a
/// violating call is denied with the rule's id + text as the reason, and
/// the registry returns that as the tool error so the model self-corrects.
/// With no guarded rules no bus is attached at all — a rule-less (or
/// Tier-1-only) session keeps the exact pre-hooks execution path.
pub(crate) fn attach_rule_guards(registry: &ToolRegistry, rules: &ResolvedRules) {
    if rules.as_slice().iter().all(|rule| rule.guard.is_none()) {
        return;
    }
    let rules = Arc::clone(&rules.0);
    let bus = HookBus::new(format!("rules-{}", std::process::id()));
    bus.on_blocking(hook_names::TOOL_CALL_REQUESTED, move |event| {
        let action = ProposedAction {
            tool: canonical_tool(event.payload["tool"].as_str().unwrap_or_default()),
            path: event.payload["input"].get("path").and_then(|v| v.as_str()),
            command: event.payload["input"]
                .get("command")
                .and_then(|v| v.as_str()),
        };
        match evaluate_guards(&rules, &action).primary() {
            Some(violation) => HookDecision::Deny {
                reason: violation.reason.clone(),
            },
            None => HookDecision::Allow,
        }
    })
    .detach();
    registry.attach_bus(bus);
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::ToolOutput;

    fn trusted_project_authority() -> crate::settings::AuthorityPolicy {
        crate::settings::AuthorityPolicy {
            project_prompts_allowed: true,
            ..crate::settings::AuthorityPolicy::default()
        }
    }

    fn write_rule(dir: &Path, name: &str, contents: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), contents).unwrap();
    }

    // FsRuleSource

    #[test]
    fn fs_source_reads_md_files_name_sorted_and_skips_absent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        write_rule(&rules_dir, "b.md", "rule b");
        write_rule(&rules_dir, "a.md", "rule a");
        std::fs::write(rules_dir.join("notes.txt"), "not a rule").unwrap();
        let files = FsRuleSource.read_rule_files(&[
            dir.path().join("missing").display().to_string(),
            rules_dir.display().to_string(),
        ]);
        assert_eq!(files.len(), 2, "only .md files load; absent dirs skip");
        assert!(files[0].path.ends_with("a.md"), "name-sorted within a dir");
        assert!(files[1].path.ends_with("b.md"));
        assert_eq!(files[1].contents, "rule b");
    }

    #[test]
    fn fs_source_preserves_directory_order_across_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let low = dir.path().join("low");
        let high = dir.path().join("high");
        write_rule(&low, "r.md", "low text");
        write_rule(&high, "r.md", "high text");
        let files =
            FsRuleSource.read_rule_files(&[low.display().to_string(), high.display().to_string()]);
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].contents, "high text",
            "later dirs come later, so load_rules' merge lets them win by id"
        );
    }

    // load_rules_from: the production directories + precedence

    /// The authoring-format contract (module docs): every frontmatter key a
    /// writer — hand authoring, rule promotion, or an extension via the
    /// store — can rely on, parsed off a real file.
    #[test]
    fn loads_a_guarded_rule_from_stella_rules() {
        let root = tempfile::tempdir().unwrap();
        write_rule(
            &root.path().join(".stella/rules"),
            "no-applied-migration.md",
            "---\ndescription: Never edit an applied migration\nguard-tool: Edit\nguard-deny-path: migrations/*-applied/**\n---\nAdd a new forward migration instead of editing an applied one.",
        );
        let rules = load_rules_from_with_authority(root.path(), None, true);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "no-applied-migration", "id = filename stem");
        assert_eq!(rules[0].description, "Never edit an applied migration");
        assert_eq!(
            rules[0].text,
            "Add a new forward migration instead of editing an applied one."
        );
        assert_eq!(
            rules[0].guard,
            Some(stella_core::rules::RuleGuard {
                tool: Some("Edit".to_string()),
                deny_path_glob: Some("migrations/*-applied/**".to_string()),
                deny_command_glob: None,
            })
        );
    }

    #[test]
    fn workspace_stella_rules_override_user_and_claude_rules_by_id() {
        let root = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        write_rule(&user.path().join("rules"), "r.md", "user text");
        write_rule(&root.path().join(".claude/rules"), "r.md", "claude text");
        write_rule(&root.path().join(".stella/rules"), "r.md", "stella text");
        let rules =
            load_rules_from_with_authority(root.path(), Some(user.path().join("rules")), true);
        assert_eq!(rules.len(), 1, "one id merges across all three dirs");
        assert_eq!(rules[0].text, "stella text");
    }

    #[test]
    fn untrusted_project_rules_are_excluded_while_user_rules_remain() {
        let root = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        {
            let store = stella_store::Store::open(root.path()).unwrap();
            store
                .upsert_rule("store", "untrusted store rule", "ext:policy")
                .unwrap();
        }
        write_rule(&user.path().join("rules"), "user.md", "trusted user rule");
        write_rule(
            &root.path().join(".claude/rules"),
            "claude.md",
            "untrusted claude rule",
        );
        write_rule(
            &root.path().join(".stella/rules"),
            "project.md",
            "untrusted project rule",
        );

        let rules =
            load_rules_from_with_authority(root.path(), Some(user.path().join("rules")), false);
        let texts: Vec<&str> = rules.iter().map(|rule| rule.text.as_str()).collect();
        assert_eq!(texts, vec!["trusted user rule"], "loaded rules: {texts:?}");
    }

    #[tokio::test]
    async fn untrusted_project_guard_is_not_armed_at_the_tool_boundary() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_rule(
            &root.path().join(".stella/rules"),
            "deny-write.md",
            "---\nguard-tool: Write\nguard-deny-path: blocked/**\n---\nProject guard.",
        );
        let registry = ToolRegistry::with_issue_backend(root.path().to_path_buf(), None);
        {
            let _env = crate::test_env::lock();
            let previous_home = std::env::var_os("HOME");
            // SAFETY: serialized behind the binary-wide environment lock.
            unsafe { std::env::set_var("HOME", home.path()) };
            enforce_workspace_rules(
                &registry,
                root.path(),
                &crate::settings::AuthorityPolicy::default(),
            );
            match previous_home {
                Some(previous) => unsafe { std::env::set_var("HOME", previous) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }

        let result = registry
            .execute(
                "write_file",
                &serde_json::json!({"path": "blocked/allowed.txt", "content": "ok\n"}),
            )
            .await;

        assert!(
            !result.is_error(),
            "untrusted guard blocked the tool: {result:?}"
        );
        assert!(root.path().join("blocked/allowed.txt").exists());
    }

    #[tokio::test]
    async fn trusted_project_same_id_cannot_weaken_a_user_guard() {
        let root = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        write_rule(
            &user.path().join("rules"),
            "protect-secrets.md",
            "---\nguard-tool: Write\nguard-deny-path: secrets/**\n---\nUser guard must remain binding.",
        );
        write_rule(
            &root.path().join(".stella/rules"),
            "protect-secrets.md",
            "Project text deliberately removes the hard guard.",
        );

        let registry = ToolRegistry::with_issue_backend(root.path().to_path_buf(), None);
        let rules =
            load_rules_from_with_authority(root.path(), Some(user.path().join("rules")), true);
        attach_rule_guards(&registry, &ResolvedRules(rules.into()));

        let denied = registry
            .execute(
                "write_file",
                &serde_json::json!({"path": "secrets/token.txt", "content": "x\n"}),
            )
            .await;
        let ToolOutput::Error { message } = denied else {
            panic!("a project rule with the same id must not weaken the user guard");
        };
        assert!(message.contains("protect-secrets"), "{message}");
        assert!(
            message.contains("User guard must remain binding."),
            "{message}"
        );
        assert!(!root.path().join("secrets/token.txt").exists());
    }

    #[tokio::test]
    async fn same_id_user_and_project_guards_are_additive() {
        let root = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        write_rule(
            &user.path().join("rules"),
            "protected.md",
            "---\nguard-tool: Write\nguard-deny-path: user-only/**\n---\nUser path is protected.",
        );
        write_rule(
            &root.path().join(".stella/rules"),
            "protected.md",
            "---\nguard-tool: Write\nguard-deny-path: project-only/**\n---\nProject path is protected.",
        );
        let rules =
            load_rules_from_with_authority(root.path(), Some(user.path().join("rules")), true);
        assert_eq!(rules.len(), 2, "both trust-tier guards must remain active");

        let registry = ToolRegistry::with_issue_backend(root.path().to_path_buf(), None);
        attach_rule_guards(&registry, &ResolvedRules(rules.into()));
        for path in ["user-only/blocked.txt", "project-only/blocked.txt"] {
            let output = registry
                .execute(
                    "write_file",
                    &serde_json::json!({"path": path, "content": "x\n"}),
                )
                .await;
            assert!(
                output.is_error(),
                "same-id guard did not block {path}: {output:?}"
            );
            assert!(!root.path().join(path).exists());
        }
    }

    #[test]
    fn no_rule_dirs_at_all_loads_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert!(load_rules_from_with_authority(root.path(), None, false).is_empty());
        assert!(
            !root.path().join(".stella").exists(),
            "rule loading must not create .stella (or a store.db) as a side effect"
        );
    }

    // store-backed rules (the extension write path)

    #[test]
    fn store_rules_load_and_any_rule_file_overrides_them_by_id() {
        let root = tempfile::tempdir().unwrap();
        {
            let store = stella_store::Store::open(root.path()).unwrap();
            store
                .upsert_rule("from-ext", "Extension-published rule.", "ext:policy")
                .unwrap();
            store
                .upsert_rule("shadowed", "store text", "ext:policy")
                .unwrap();
        }
        write_rule(
            &root.path().join(".stella/rules"),
            "shadowed.md",
            "file text",
        );

        let rules = load_rules_from_with_authority(root.path(), None, true);
        assert_eq!(rules.len(), 2, "store + file rules merge by id");
        let ext = rules.iter().find(|r| r.id == "from-ext").unwrap();
        assert_eq!(ext.text, "Extension-published rule.");
        assert_eq!(
            ext.source, "store://rules/from-ext.md",
            "store provenance must be visible on the merged rule"
        );
        assert_eq!(
            rules.iter().find(|r| r.id == "shadowed").unwrap().text,
            "file text",
            "a rule file must override a store rule with the same id"
        );
    }

    #[tokio::test]
    async fn a_store_published_guard_blocks_through_the_wired_boundary() {
        let root = tempfile::tempdir().unwrap();
        {
            let store = stella_store::Store::open(root.path()).unwrap();
            store
                .upsert_rule(
                    "no-secret-writes",
                    "---\nguard-deny-path: secrets/**\n---\nNever write under secrets/.",
                    "ext:policy",
                )
                .unwrap();
        }
        let registry = ToolRegistry::with_issue_backend(root.path().to_path_buf(), None);
        enforce_workspace_rules(&registry, root.path(), &trusted_project_authority());

        let denied = registry
            .execute(
                "write_file",
                &serde_json::json!({"path": "secrets/token.txt", "content": "x\n"}),
            )
            .await;
        let ToolOutput::Error { message } = denied else {
            panic!("a store-published guard must block the write");
        };
        assert!(message.contains("no-secret-writes"), "{message}");
        assert!(message.contains("Never write under secrets/."), "{message}");
        assert!(
            !root.path().join("secrets/token.txt").exists(),
            "a blocked write must never land"
        );
    }

    // Tier-2 enforcement through the wired tool boundary

    #[tokio::test]
    async fn a_tier2_guard_blocks_a_violating_edit_through_the_wired_boundary() {
        let root = tempfile::tempdir().unwrap();
        write_rule(
            &root.path().join(".stella/rules"),
            "no-applied-migration.md",
            "---\nguard-tool: Edit\nguard-deny-path: migrations/*-applied/**\n---\nAdd a new forward migration instead of editing an applied one.",
        );
        std::fs::create_dir_all(root.path().join("migrations/0001-applied")).unwrap();
        let target = root.path().join("migrations/0001-applied/up.sql");
        std::fs::write(&target, "SELECT 1;\n").unwrap();

        let registry = ToolRegistry::with_issue_backend(root.path().to_path_buf(), None);
        enforce_workspace_rules(&registry, root.path(), &trusted_project_authority());

        let denied = registry
            .execute(
                "edit_file",
                &serde_json::json!({
                    "path": "migrations/0001-applied/up.sql",
                    "old_string": "SELECT 1;",
                    "new_string": "SELECT 2;",
                }),
            )
            .await;
        let ToolOutput::Error { message } = denied else {
            panic!("the guard must block the edit");
        };
        assert!(message.contains("no-applied-migration"), "{message}");
        assert!(
            message.contains("Add a new forward migration"),
            "the rule text must reach the model so it can self-correct: {message}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "SELECT 1;\n",
            "a blocked edit must never land"
        );

        // The same registry still executes calls no guard matches.
        let allowed = registry
            .execute(
                "write_file",
                &serde_json::json!({"path": "migrations/0002/up.sql", "content": "SELECT 2;\n"}),
            )
            .await;
        assert!(!allowed.is_error(), "an unguarded path must run normally");
    }

    #[tokio::test]
    async fn a_command_guard_blocks_bash_and_a_tier1_rule_never_does() {
        let root = tempfile::tempdir().unwrap();
        write_rule(
            &root.path().join(".stella/rules"),
            "no-force-push.md",
            "---\nguard-tool: Bash\nguard-deny-command: git push --force*\n---\nNever force-push.",
        );
        // Tier-1 (prompt-only) rule alongside: must not block anything.
        write_rule(
            &root.path().join(".stella/rules"),
            "style.md",
            "Match the surrounding code style.",
        );

        // Command guards apply to `bash`, which is settings opt-in — this
        // fixture opts it in the way an enabled session would.
        let registry = ToolRegistry::with_backends_and_options(
            root.path().to_path_buf(),
            None,
            None,
            stella_tools::RegistryOptions {
                bash: true,
                web: false,
                ..Default::default()
            },
        );
        enforce_workspace_rules(&registry, root.path(), &trusted_project_authority());

        let denied = registry
            .execute(
                "bash",
                &serde_json::json!({"command": "git push --force origin main"}),
            )
            .await;
        let ToolOutput::Error { message } = denied else {
            panic!("the command guard must block the call");
        };
        assert!(message.contains("no-force-push"), "{message}");
        assert!(message.contains("Never force-push."), "{message}");

        let allowed = registry
            .execute("bash", &serde_json::json!({"command": "echo ok"}))
            .await;
        assert!(!allowed.is_error(), "a non-matching command runs normally");
    }

    // Phase 0: representative `.stella/rules/*.md` fixtures must parse under the
    // CURRENT `stella_core::rules` parser — the format Phase 2's importer will
    // read back as read-only mirrors. Captures both a promoted guard rule and a
    // full context-as-code directive (inferred, advisory).
    const GUARD_RULE_FIXTURE: &str =
        include_str!("../tests/fixtures/rules/no-hand-edited-migrations.md");
    const DIRECTIVE_RULE_FIXTURE: &str =
        include_str!("../tests/fixtures/rules/api-integration-coverage.md");

    #[test]
    fn guard_rule_fixture_parses_as_a_guarded_rule_without_metadata() {
        let rule = stella_core::rules::rule_from_file(
            ".stella/rules/no-hand-edited-migrations.md",
            GUARD_RULE_FIXTURE,
        )
        .expect("guard rule parses");
        assert_eq!(rule.id, "no-hand-edited-migrations");
        assert_eq!(
            rule.description,
            "Never hand-edit an already-applied migration file."
        );
        let guard = rule.guard.expect("guard frontmatter present");
        assert_eq!(guard.tool.as_deref(), Some("Edit"));
        assert_eq!(
            guard.deny_path_glob.as_deref(),
            Some("migrations/*-applied/**")
        );
        assert!(guard.deny_command_glob.is_none());
        // A pure guard rule declares no lifecycle metadata and so has no errors.
        assert!(rule.metadata.is_none());
        assert!(rule.metadata_errors.is_empty());
    }

    #[test]
    fn directive_rule_fixture_parses_with_context_metadata() {
        let rule = stella_core::rules::rule_from_file(
            ".stella/rules/api-integration-coverage.md",
            DIRECTIVE_RULE_FIXTURE,
        )
        .expect("directive rule parses");
        assert_eq!(rule.id, "api-integration-coverage");
        // Empty errors proves every enum (record_kind/enforcement/origin) and
        // timestamp in the frontmatter parsed to a valid value.
        assert!(
            rule.metadata_errors.is_empty(),
            "unexpected metadata errors: {:?}",
            rule.metadata_errors
        );
        let meta = rule.metadata.expect("context metadata present");
        assert_eq!(meta.record_id, "dir_api_integration_coverage_v1");
        assert_eq!(meta.confidence, 88);
        // `scope_paths` is canonicalized (sorted, deduped) by the parser.
        assert_eq!(meta.scope_paths, vec!["docs/api/**", "src/api/**"]);
        assert_eq!(meta.supporting_evidence_ids.len(), 3);
    }
}
