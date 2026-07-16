//! Filesystem glue for custom extensions (`stella_core::extensions`): the
//! init-time symlink sync that adopts `.claude/`- and `.agents/`-authored
//! commands/skills/agents into stella's own directories, and the loaders the
//! chat surfaces use to offer them (⚡ slash-menu rows, `/agents`).
//!
//! All planning and parsing is pure and lives in `stella-core`; this module
//! owns exactly the I/O halves (`02-architecture.md` §1.3): directory
//! scanning with symlink detection, symlink creation, and definition-file
//! reads.
//!
//! ## Scopes
//!
//! The sync runs at both scopes, mirroring the settings/skills chain:
//!
//! - **workspace**: `<root>/{.claude,.agents}/<kind>` → `<root>/.stella/<kind>`
//! - **user**: `~/{.claude,.agents}/<kind>` → `~/.config/stella/<kind>`
//!
//! so definitions installed for other agents at either level are visible to
//! stella after `stella init` (or `/init`). The loaders then read the
//! `.stella`-side directories only — user-global first, workspace last, so a
//! workspace definition wins a name collision (same precedence as skills).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use stella_core::extensions::{
    AgentDef, CommandDef, ExtensionKind, SyncEntry, SyncSource, agent_from_file, command_from_file,
    expand_command, merge_by_name, plan_extension_sync,
};
use stella_core::skills::Skill;
use stella_tui::SlashCommand;

/// The other-agent directories the sync adopts from, in precedence order
/// (an entry in an earlier directory wins a real name collision).
const SOURCE_DIRS: [&str; 2] = [".claude", ".agents"];

// ============================================================================
// Scanning + symlink sync
// ============================================================================

/// Scan one source directory for one kind (`<source_root>/<kind>`), with
/// per-entry symlink detection. Hidden entries (`.DS_Store`,
/// `.skill-lock.json`, …) are ignored. Sorted by name so plans — and
/// therefore init output — are deterministic.
fn scan_source(source_root: &Path, kind: ExtensionKind) -> SyncSource {
    let dir = source_root.join(kind.dir_name());
    let mut entries: Vec<SyncEntry> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                return None;
            }
            let is_symlink = entry
                .path()
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            Some(SyncEntry {
                name,
                path: entry.path().display().to_string(),
                is_symlink,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    SyncSource { kind, entries }
}

/// Every name already present in `dir` — real files, dirs, and symlinks
/// alike (a dangling symlink still occupies the name).
fn existing_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect()
}

/// The relative path from inside `from_dir` to `target` — symlinks are
/// created relative so a repo (or home) moved as a unit keeps working.
/// Falls back to the absolute target when the two share no common root.
fn relative_symlink_target(from_dir: &Path, target: &Path) -> PathBuf {
    let from: Vec<Component> = from_dir.components().collect();
    let to: Vec<Component> = target.components().collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return target.to_path_buf();
    }
    let mut rel = PathBuf::new();
    for _ in common..from.len() {
        rel.push("..");
    }
    for component in &to[common..] {
        rel.push(component);
    }
    rel
}

/// What one sync run did, for the init progress line. `errors` carries any
/// link that could not be created (permissions, non-unix platform) — the
/// sync is best-effort and never fails init.
#[derive(Debug, Default)]
pub struct SyncOutcome {
    /// Created links as `(kind, name)`.
    pub linked: Vec<(ExtensionKind, String)>,
    /// Entries skipped by the plan (symlink sources, existing names).
    pub skipped: usize,
    pub errors: Vec<String>,
}

impl SyncOutcome {
    /// `"2 commands, 12 skills"`-style summary of the created links, or
    /// `None` when nothing was linked.
    pub fn summary(&self) -> Option<String> {
        let parts: Vec<String> = ExtensionKind::ALL
            .iter()
            .filter_map(|kind| {
                let n = self.linked.iter().filter(|(k, _)| k == kind).count();
                (n > 0).then(|| format!("{n} {}", kind.dir_name()))
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Adopt every command/skill/agent found under `source_roots` (in precedence
/// order) into `dest_root` (`.stella` or `~/.config/stella`) as symlinks.
/// Idempotent: symlink sources, already-present names, and duplicate names
/// are skipped by the plan (`stella_core::extensions::plan_extension_sync`).
fn sync_into(dest_root: &Path, source_roots: &[PathBuf]) -> SyncOutcome {
    let sources: Vec<SyncSource> = ExtensionKind::ALL
        .iter()
        .flat_map(|kind| source_roots.iter().map(|root| scan_source(root, *kind)))
        .filter(|s| !s.entries.is_empty())
        .collect();
    let existing = |kind: ExtensionKind| existing_names(&dest_root.join(kind.dir_name()));
    let plan = plan_extension_sync(&sources, &existing);

    let mut outcome = SyncOutcome {
        skipped: plan.skips.len(),
        ..SyncOutcome::default()
    };
    for link in &plan.links {
        let dir = dest_root.join(link.kind.dir_name());
        if let Err(e) = std::fs::create_dir_all(&dir) {
            outcome.errors.push(format!("{}: {e}", dir.display()));
            continue;
        }
        let dest = dir.join(&link.name);
        let target = relative_symlink_target(&dir, Path::new(&link.source_path));
        match create_symlink(&target, &dest) {
            Ok(()) => outcome.linked.push((link.kind, link.name.clone())),
            Err(e) => outcome.errors.push(format!("{}: {e}", dest.display())),
        }
    }
    outcome
}

#[cfg(unix)]
fn create_symlink(target: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, dest)
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _dest: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "extension symlinks are only supported on unix platforms",
    ))
}

/// The user-global stella config root (`~/.config/stella`), or `None`
/// without a home directory.
fn user_config_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("stella"))
}

/// Run the sync at both scopes and report through `emit` — the shared init
/// hook (`agent::init_workspace`) calls this so `stella init` and `/init`
/// behave identically. Quiet when there is nothing to do.
pub fn sync_extensions(workspace_root: &Path, emit: &mut dyn FnMut(String)) {
    let mut scopes: Vec<(&str, PathBuf, Vec<PathBuf>)> = vec![(
        "workspace",
        workspace_root.join(".stella"),
        SOURCE_DIRS.iter().map(|d| workspace_root.join(d)).collect(),
    )];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Some(config_root) = user_config_root()
    {
        scopes.push((
            "user",
            config_root,
            SOURCE_DIRS.iter().map(|d| home.join(d)).collect(),
        ));
    }

    for (scope, dest, sources) in scopes {
        let outcome = sync_into(&dest, &sources);
        if let Some(summary) = outcome.summary() {
            emit(format!(
                "✓ adopted {summary} from .claude/.agents ({scope} scope)"
            ));
        }
        for error in &outcome.errors {
            emit(format!("! extension link failed: {error}"));
        }
    }
}

// ============================================================================
// Loading
// ============================================================================

/// Read one kind's definition files from `dir`: flat `<slug>.md` plus the
/// nested `<slug>/<nested_file>` layout, both read *through* symlinks (that
/// is the point of the sync). Returns `(path, content)` pairs.
fn read_definition_files(dir: &Path, nested_file: &str) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return files,
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_none_or(|n| n.starts_with('.'))
        {
            continue;
        }
        if path.extension().is_some_and(|e| e == "md") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                files.push((path.display().to_string(), content));
            }
        } else if path.is_dir() {
            let nested = path.join(nested_file);
            if let Ok(content) = std::fs::read_to_string(&nested) {
                files.push((nested.display().to_string(), content));
            }
        }
    }
    files
}

/// The `.stella`-side directories one kind is loaded from, lowest precedence
/// first (user-global, then workspace — workspace wins, like skills).
fn load_dirs(workspace_root: &Path, kind: ExtensionKind) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(config_root) = user_config_root() {
        dirs.push(config_root.join(kind.dir_name()));
    }
    dirs.push(workspace_root.join(".stella").join(kind.dir_name()));
    dirs
}

fn load_commands_from(dirs: &[PathBuf]) -> Vec<CommandDef> {
    let parsed = dirs
        .iter()
        .flat_map(|dir| read_definition_files(dir, "COMMAND.md"))
        .filter_map(|(path, raw)| command_from_file(&path, &raw).ok())
        .collect();
    merge_by_name(parsed, |c: &CommandDef| c.name.as_str())
}

fn load_agents_from(dirs: &[PathBuf]) -> Vec<AgentDef> {
    let parsed = dirs
        .iter()
        .flat_map(|dir| read_definition_files(dir, "AGENT.md"))
        .filter_map(|(path, raw)| agent_from_file(&path, &raw).ok())
        .collect();
    merge_by_name(parsed, |a: &AgentDef| a.name.as_str())
}

/// Everything custom a chat surface offers: commands (⚡ `/name`, prompt
/// templates), skills (⚡ `/name`, injected know-how — the same files the
/// recall engine auto-selects from), and agents (`/agents`).
#[derive(Debug, Default)]
pub struct CustomExtensions {
    pub commands: Vec<CommandDef>,
    pub skills: Vec<Skill>,
    pub agents: Vec<AgentDef>,
}

/// What a custom `/name` resolves to.
pub enum Invocation<'a> {
    Command(&'a CommandDef),
    Skill(&'a Skill),
}

impl CustomExtensions {
    /// Load every custom definition visible from `workspace_root`
    /// (user-global + workspace `.stella` directories, workspace wins).
    pub fn load(workspace_root: &Path) -> Self {
        Self {
            commands: load_commands_from(&load_dirs(workspace_root, ExtensionKind::Commands)),
            skills: crate::memory::load_workspace_skills(workspace_root),
            agents: load_agents_from(&load_dirs(workspace_root, ExtensionKind::Agents)),
        }
    }

    /// The ⚡ slash-menu rows: commands first, then skills, names prefixed
    /// with `/`. A custom name shadowed by a productized command in
    /// `reserved` is dropped (builtins always win), and a skill sharing a
    /// command's name is dropped (the command wins — it was authored as an
    /// invocation).
    pub fn slash_entries(&self, reserved: &[SlashCommand]) -> Vec<SlashCommand> {
        let mut taken: HashSet<String> = reserved.iter().map(|c| c.name.clone()).collect();
        let mut rows = Vec::new();
        let commands = self
            .commands
            .iter()
            .map(|c| (format!("/{}", c.name), c.description.clone()));
        let skills = self
            .skills
            .iter()
            .map(|s| (format!("/{}", s.name), s.description.clone()));
        for (name, description) in commands.chain(skills) {
            if taken.insert(name.clone()) {
                rows.push(SlashCommand::custom(name, description));
            }
        }
        rows
    }

    /// Resolve a custom invocation: `head` is the leading `/word` of the
    /// input (slash included). Commands shadow skills, matching
    /// [`Self::slash_entries`].
    pub fn lookup(&self, head: &str) -> Option<Invocation<'_>> {
        let name = head.strip_prefix('/')?;
        if let Some(cmd) = self.commands.iter().find(|c| c.name == name) {
            return Some(Invocation::Command(cmd));
        }
        self.skills
            .iter()
            .find(|s| s.name == name)
            .map(Invocation::Skill)
    }

    /// Expand `input` (`/name args…`) into the prompt the model runs, or
    /// `None` when the name matches no custom command/skill. A command's
    /// body is its template ([`expand_command`]); a skill's body rides as
    /// context above the user's task.
    pub fn expand(&self, input: &str) -> Option<String> {
        let trimmed = input.trim();
        let (head, args) = match trimmed.split_once(char::is_whitespace) {
            Some((head, args)) => (head, args),
            None => (trimmed, ""),
        };
        match self.lookup(head)? {
            Invocation::Command(cmd) => Some(expand_command(&cmd.body, args)),
            Invocation::Skill(skill) => Some(skill_invocation_prompt(skill, args)),
        }
    }

    /// The `/agents` listing: every custom agent's name, description, and
    /// source, or a hint when none are defined.
    pub fn render_agent_list(&self) -> String {
        if self.agents.is_empty() {
            return "no custom agents found — add markdown definitions to .stella/agents/ \
                    (or run /init to adopt .claude/.agents ones)"
                .to_string();
        }
        let mut out = format!("custom agents ({}):\n", self.agents.len());
        for agent in &self.agents {
            out.push_str(&format!(
                "  ⚡ {} — {}  ({})\n",
                agent.name, agent.description, agent.source_path
            ));
        }
        out
    }
}

/// The prompt a `/skill-name` invocation runs: the skill body as explicit
/// instructions, with any trailing text as the task to apply them to.
fn skill_invocation_prompt(skill: &Skill, args: &str) -> String {
    let mut out = format!(
        "Apply the following skill.\n\n# Skill: {}\n{}\n\n{}",
        skill.name, skill.description, skill.body
    );
    let args = args.trim();
    if !args.is_empty() {
        out.push_str(&format!("\n\n## Task\n{args}"));
    }
    out
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn skill_md(name: &str) -> String {
        format!("---\nname: {name}\ndescription: about {name}\n---\nbody of {name}\n")
    }

    #[test]
    fn sync_adopts_real_entries_and_skips_symlinked_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // `.agents/skills/shared` is the real home; `.claude/skills/shared`
        // is another agent's symlink to it — the exact in-the-wild shape.
        write(
            &root.join(".agents/skills/shared/SKILL.md"),
            &skill_md("shared"),
        );
        write(
            &root.join(".claude/skills/local/SKILL.md"),
            &skill_md("local"),
        );
        std::os::unix::fs::symlink(
            root.join(".agents/skills/shared"),
            root.join(".claude/skills/shared"),
        )
        .unwrap();
        write(
            &root.join(".claude/commands/deploy.md"),
            "---\ndescription: ship it\n---\nDeploy $ARGUMENTS now.",
        );
        write(
            &root.join(".claude/agents/reviewer.md"),
            "---\nname: reviewer\ndescription: reviews\n---\nYou review.",
        );

        let outcome = sync_into(
            &root.join(".stella"),
            &[root.join(".claude"), root.join(".agents")],
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let mut linked = outcome.linked.clone();
        linked.sort_by(|a, b| a.1.cmp(&b.1));
        // Flat files keep their `.md` basename so the loaders' stem-derived
        // slugs survive the link; skill directories link by directory name.
        assert_eq!(
            linked,
            vec![
                (ExtensionKind::Commands, "deploy.md".to_string()),
                (ExtensionKind::Skills, "local".to_string()),
                (ExtensionKind::Agents, "reviewer.md".to_string()),
                (ExtensionKind::Skills, "shared".to_string()),
            ]
        );

        // `shared` was adopted from its real `.agents` home, not through the
        // `.claude` symlink — and as a relative link.
        let link = root.join(".stella/skills/shared");
        let target = std::fs::read_link(&link).unwrap();
        assert!(target.is_relative());
        assert_eq!(
            std::fs::canonicalize(&link).unwrap(),
            std::fs::canonicalize(root.join(".agents/skills/shared")).unwrap()
        );
        // The adopted definitions are readable through the links.
        assert!(
            std::fs::read_to_string(root.join(".stella/skills/shared/SKILL.md"))
                .unwrap()
                .contains("body of shared")
        );
    }

    #[test]
    fn sync_is_idempotent_and_never_clobbers_user_definitions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join(".claude/commands/deploy.md"), "Ship.");
        // A user-authored definition already occupies the name.
        write(&root.join(".stella/commands/deploy.md"), "MY deploy.");

        let dest = root.join(".stella");
        let sources = vec![root.join(".claude"), root.join(".agents")];
        let first = sync_into(&dest, &sources);
        assert!(
            first.linked.is_empty(),
            "existing names are never clobbered"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".stella/commands/deploy.md")).unwrap(),
            "MY deploy."
        );

        write(&root.join(".claude/commands/fresh.md"), "New.");
        let second = sync_into(&dest, &sources);
        assert_eq!(second.linked.len(), 1);
        let third = sync_into(&dest, &sources);
        assert!(third.linked.is_empty(), "re-running links nothing new");
    }

    #[test]
    fn relative_target_walks_up_and_back_down() {
        assert_eq!(
            relative_symlink_target(
                Path::new("/ws/.stella/skills"),
                Path::new("/ws/.agents/skills/x"),
            ),
            PathBuf::from("../../.agents/skills/x")
        );
        assert_eq!(
            relative_symlink_target(Path::new("/a/b"), Path::new("/a/b/c")),
            PathBuf::from("c")
        );
    }

    #[test]
    fn loads_commands_and_agents_with_workspace_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user-scope");
        let ws = tmp.path().join("ws-scope");
        write(
            &user.join("deploy.md"),
            "---\ndescription: user version\n---\nuser body",
        );
        write(
            &ws.join("deploy.md"),
            "---\ndescription: workspace version\n---\nworkspace body",
        );
        write(&ws.join("review/COMMAND.md"), "Review the diff.");

        let commands = load_commands_from(&[user.clone(), ws.clone()]);
        let deploy = commands.iter().find(|c| c.name == "deploy").unwrap();
        assert_eq!(deploy.description, "workspace version");
        assert!(
            commands.iter().any(|c| c.name == "review"),
            "nested layout loads"
        );

        let agent_dir = tmp.path().join("ws-agents");
        write(
            &agent_dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: reviews\n---\nYou review.",
        );
        let agents = load_agents_from(&[agent_dir]);
        assert_eq!(agents.len(), 1, "flat .md files parse as agents");
        assert_eq!(agents[0].name, "reviewer");
    }

    fn custom_fixture() -> CustomExtensions {
        CustomExtensions {
            commands: vec![CommandDef {
                name: "fix-bug".to_string(),
                description: "fix the named bug".to_string(),
                body: "Fix $ARGUMENTS end to end.".to_string(),
                source_path: "x/fix-bug.md".to_string(),
            }],
            skills: vec![Skill {
                name: "sql-style".to_string(),
                description: "format sql".to_string(),
                domains: vec![],
                body: "Lowercase keywords.".to_string(),
                source_path: "x/sql-style/SKILL.md".to_string(),
                origin: stella_core::skills::SkillOrigin::Workspace,
            }],
            agents: vec![AgentDef {
                name: "reviewer".to_string(),
                description: "reviews diffs".to_string(),
                body: "You review.".to_string(),
                source_path: "x/reviewer.md".to_string(),
            }],
        }
    }

    #[test]
    fn slash_entries_are_custom_rows_that_never_shadow_builtins() {
        let mut custom = custom_fixture();
        custom.commands.push(CommandDef {
            name: "help".to_string(), // collides with the builtin /help
            description: "shadowed".to_string(),
            body: "body".to_string(),
            source_path: "x/help.md".to_string(),
        });
        let reserved = vec![SlashCommand::new("/help", "show commands")];
        let rows = custom.slash_entries(&reserved);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["/fix-bug", "/sql-style"]);
        assert!(rows.iter().all(|r| r.kind == stella_tui::SlashKind::Custom));
    }

    #[test]
    fn expand_substitutes_command_arguments() {
        let custom = custom_fixture();
        assert_eq!(
            custom.expand("/fix-bug issue-42").as_deref(),
            Some("Fix issue-42 end to end.")
        );
        assert!(custom.expand("/unknown thing").is_none());
    }

    #[test]
    fn expand_wraps_a_skill_invocation_with_its_body_and_task() {
        let custom = custom_fixture();
        let prompt = custom.expand("/sql-style tidy my query").unwrap();
        assert!(prompt.contains("# Skill: sql-style"));
        assert!(prompt.contains("Lowercase keywords."));
        assert!(prompt.contains("## Task\ntidy my query"));
        // Bare invocation: no task section.
        let bare = custom.expand("/sql-style").unwrap();
        assert!(!bare.contains("## Task"));
    }

    #[test]
    fn agent_list_names_every_agent_and_hints_when_empty() {
        let custom = custom_fixture();
        let list = custom.render_agent_list();
        assert!(list.contains("⚡ reviewer — reviews diffs"));
        let empty = CustomExtensions::default();
        assert!(empty.render_agent_list().contains("no custom agents"));
    }
}
