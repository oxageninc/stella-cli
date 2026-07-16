//! Production wiring for the workspace rules engine (`stella_core::rules`):
//! the [`RuleSource`] backed by real directory reads and the workspace
//! store, plus the session glue that hard-enforces Tier-2 guards at the
//! tool boundary.
//!
//! All rule semantics — frontmatter parsing, precedence merging, Tier-1
//! rendering, Tier-2 guard evaluation — live in `stella-core` (no I/O,
//! `02-architecture.md` §1.3); this module owns exactly the I/O half:
//! walking the rule directories, reading extension-authored rules out of
//! `.stella/store.db` (`stella_store::Store::list_rules`), and registering
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
//! `.claude/rules/` and `~/.config/stella/rules/`) — the format promoted
//! rules are written in. The SAME markdown, as one string, is what
//! extension providers publish through `stella_store::Store::upsert_rule`.

use std::path::{Path, PathBuf};

use stella_core::bus::{HookBus, HookDecision, names as hook_names};
use stella_core::rules::{
    LoadRulesOptions, ProposedAction, Rule, RuleFile, RuleSource, evaluate_guards, load_rules,
};
use stella_tools::ToolRegistry;

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

/// Extension-authored rules from the workspace store (`.stella/store.db`),
/// rendered as the [`RuleFile`]s the engine's merge consumes. The synthetic
/// `store://rules/<id>.md` path both labels the rule's provenance
/// ([`Rule::source`]) and yields the stored id as the filename stem, so a
/// rule without `name:` frontmatter still merges under the id it was
/// published as. Read only when the database already exists — rule loading
/// must never create one as a side effect — and best-effort like all
/// persistence: any failure means no store rules this session, never an
/// error.
fn store_rule_files(workspace_root: &Path) -> Vec<RuleFile> {
    if !workspace_root.join(".stella").join("store.db").exists() {
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
/// (lowest precedence), then the on-disk rule files in directory order —
/// the engine's merge lets any rule FILE (user, `.claude`, `.stella`)
/// override a store rule with the same id, so the human's checked-in files
/// keep final say over machine-written rules.
struct SessionRuleSource {
    store_files: Vec<RuleFile>,
}

impl RuleSource for SessionRuleSource {
    fn read_rule_files(&self, dirs: &[String]) -> Vec<RuleFile> {
        let mut out = self.store_files.clone();
        out.extend(FsRuleSource.read_rule_files(dirs));
        out
    }
}

/// Load every workspace rule visible from `workspace_root`, merged by id
/// through the engine's precedence merging: store rules, then the user
/// (`~/.config/stella/rules`), `.claude/rules`, and `.stella/rules`
/// directories — later wins (`stella_core::rules::rule_search_dirs`).
/// Called once per session: the result feeds both the Tier-1 prompt
/// section and the Tier-2 guards, so the two enforcement surfaces can
/// never disagree about what was loaded.
pub(crate) fn load_workspace_rules(workspace_root: &Path) -> Vec<Rule> {
    let user_rules_dir = std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("stella")
            .join("rules")
    });
    load_rules_from(workspace_root, user_rules_dir)
}

/// [`load_workspace_rules`] over an explicit user rules directory — the
/// seam tests use so precedence is provable without touching `$HOME`.
/// `None` (no home directory) degrades to workspace + store rules only.
fn load_rules_from(workspace_root: &Path, user_rules_dir: Option<PathBuf>) -> Vec<Rule> {
    let opts = LoadRulesOptions {
        cwd: workspace_root.display().to_string(),
        user_rules_dir: user_rules_dir
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
    };
    let source = SessionRuleSource {
        store_files: store_rule_files(workspace_root),
    };
    load_rules(&source, &opts)
}

/// Session wiring shorthand: load the workspace rules and attach their
/// Tier-2 guards to `registry`. Every session driver calls this right after
/// constructing its registry (same rhythm as `populate_schema_index`).
pub(crate) fn enforce_workspace_rules(registry: &ToolRegistry, workspace_root: &Path) {
    attach_rule_guards(registry, load_workspace_rules(workspace_root));
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
pub(crate) fn attach_rule_guards(registry: &ToolRegistry, rules: Vec<Rule>) {
    if rules.iter().all(|rule| rule.guard.is_none()) {
        return;
    }
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

    fn write_rule(dir: &Path, name: &str, contents: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), contents).unwrap();
    }

    // ---- FsRuleSource ----

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

    // ---- load_rules_from: the production directories + precedence ----

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
        let rules = load_rules_from(root.path(), None);
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
        let rules = load_rules_from(root.path(), Some(user.path().join("rules")));
        assert_eq!(rules.len(), 1, "one id merges across all three dirs");
        assert_eq!(rules[0].text, "stella text");
    }

    #[test]
    fn no_rule_dirs_at_all_loads_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert!(load_rules_from(root.path(), None).is_empty());
        assert!(
            !root.path().join(".stella").exists(),
            "rule loading must not create .stella (or a store.db) as a side effect"
        );
    }

    // ---- store-backed rules (the extension write path) ----

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

        let rules = load_rules_from(root.path(), None);
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
        enforce_workspace_rules(&registry, root.path());

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

    // ---- Tier-2 enforcement through the wired tool boundary ----

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
        enforce_workspace_rules(&registry, root.path());

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

        let registry = ToolRegistry::with_issue_backend(root.path().to_path_buf(), None);
        enforce_workspace_rules(&registry, root.path());

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
}
