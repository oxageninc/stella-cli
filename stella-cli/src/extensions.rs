//! Filesystem glue for custom extensions (`stella_core::extensions`): the
//! init-time symlink sync that adopts `.claude/`- and `.agents/`-authored
//! commands/skills/agents into stella's own directories, and the loaders the
//! chat surfaces use to offer them (⚡ slash-menu rows, `/agents`).
//!
//! All planning and parsing is pure and lives in `stella-core`; this module
//! owns exactly the I/O halves: directory
//! scanning with symlink detection, symlink creation, and definition-file
//! reads.
//!
//! ## Scopes
//!
//! The sync runs at both scopes, mirroring the settings/skills chain:
//!
//! - **workspace**: `<root>/{.claude,.agents}/<kind>` → `<root>/.stella/<kind>`
//! - **user**: `~/{.claude,.agents}/<kind>` → `~/.stella/<kind>`
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

// Scanning + symlink sync

/// The per-directory definition file each kind accepts alongside flat
/// `<slug>.md` (the `npx skills` ecosystem layout, generalized).
fn nested_file(kind: ExtensionKind) -> &'static str {
    match kind {
        ExtensionKind::Commands => "COMMAND.md",
        ExtensionKind::Skills => "SKILL.md",
        ExtensionKind::Agents => "AGENT.md",
    }
}

/// The name a definition at `path` will *load* under: the frontmatter
/// `name:` when readable (a directory entry is read through its
/// `SKILL.md`-style nested file), else the filename-derived slug. Sync
/// collision precedence keys on this — see `SyncEntry::definition_name`.
fn definition_name_for(path: &Path, kind: ExtensionKind) -> String {
    let fallback = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .map(|n| n.strip_suffix(".md").unwrap_or(&n).to_string())
        .unwrap_or_default();
    let file = if path.is_dir() {
        path.join(nested_file(kind))
    } else {
        path.to_path_buf()
    };
    std::fs::read_to_string(&file)
        .ok()
        .and_then(|raw| {
            stella_core::rules::parse_frontmatter(&raw)
                .data
                .get("name")
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
        })
        .unwrap_or(fallback)
}

/// Whether the loader can read a definition out of this entry: a flat file
/// always can (`read_definition_files` matches any `.md` extension); a
/// directory only can when it holds the kind's nested definition file. This
/// mirrors, exactly, the condition under which `read_definition_files`
/// silently skips a directory — no nested file present at all, so nothing
/// loads and nothing is diagnosed. A namespace directory such as
/// `.claude/commands/vercel/` holding only `deploy.md` (no `COMMAND.md`) is
/// not loadable by this measure.
fn is_loadable_entry(path: &Path, kind: ExtensionKind) -> bool {
    if path.is_dir() {
        path.join(nested_file(kind)).symlink_metadata().is_ok()
    } else {
        true
    }
}

/// Scan one source directory for one kind (`<source_root>/<kind>`), with
/// per-entry symlink detection and best-effort frontmatter-name resolution.
/// Hidden entries (`.DS_Store`, `.skill-lock.json`, …) are ignored. Sorted by
/// name so plans — and therefore init output — are deterministic.
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
                definition_name: definition_name_for(&entry.path(), kind),
                is_loadable: is_loadable_entry(&entry.path(), kind),
                name,
                path: entry.path().display().to_string(),
                is_symlink,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    SyncSource { kind, entries }
}

/// What already occupies one kind's destination directory: every entry
/// basename — real files, dirs, and symlinks alike (a dangling symlink still
/// occupies the name) — plus the names those definitions load under.
fn existing_targets(dir: &Path, kind: ExtensionKind) -> stella_core::extensions::ExistingTargets {
    let mut targets = stella_core::extensions::ExistingTargets::default();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with('.') {
            targets
                .definition_names
                .push(definition_name_for(&entry.path(), kind));
        }
        targets.file_names.push(name);
    }
    targets
}

/// One human-readable line for a `NotLoadable` skip — named individually
/// (unlike the benign skip reasons, which are just a count) because the bug
/// this reason exists to fix (#104) is exactly a silently-adopted entry;
/// folding it into "N already present" would also misdescribe it, since
/// nothing with this name was actually present anywhere.
fn describe_unloadable_skip(skip: &stella_core::extensions::SyncSkip) -> String {
    format!(
        "{} ({}): namespaced directory — no {}.md or {} found, not loadable",
        skip.name,
        skip.kind.dir_name(),
        skip.name,
        nested_file(skip.kind)
    )
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
    /// Entries skipped for the benign, expected reasons (symlink sources,
    /// already-present names, duplicate loaded names) — folded into the
    /// summary's "N already present" rather than named individually.
    /// Deliberately excludes `NotLoadable` skips, which are named
    /// individually in `unloadable` instead (see its doc).
    pub skipped: usize,
    /// One human-readable line per entry skipped as `NotLoadable` (a
    /// namespace directory with no nested definition file — see issue
    /// #104). Unlike `skipped`, these are always surfaced by
    /// `sync_extensions`, even when nothing else in this scope was linked
    /// or already present: the bug this reason exists to fix is exactly a
    /// silently-adopted entry, so going quiet here would reintroduce it.
    pub unloadable: Vec<String>,
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
/// order) into `dest_root` (`.stella` or `~/.stella`) as symlinks.
/// Idempotent: symlink sources, already-present names, duplicate names, and
/// unloadable entries (namespace directories with no nested definition
/// file) are skipped by the plan
/// (`stella_core::extensions::plan_extension_sync`).
fn sync_into(dest_root: &Path, source_roots: &[PathBuf]) -> SyncOutcome {
    let sources: Vec<SyncSource> = ExtensionKind::ALL
        .iter()
        .flat_map(|kind| source_roots.iter().map(|root| scan_source(root, *kind)))
        .filter(|s| !s.entries.is_empty())
        .collect();
    let existing = |kind: ExtensionKind| existing_targets(&dest_root.join(kind.dir_name()), kind);
    let plan = plan_extension_sync(&sources, &existing);

    let mut outcome = SyncOutcome {
        skipped: plan
            .skips
            .iter()
            .filter(|s| s.reason != stella_core::extensions::SyncSkipReason::NotLoadable)
            .count(),
        unloadable: plan
            .skips
            .iter()
            .filter(|s| s.reason == stella_core::extensions::SyncSkipReason::NotLoadable)
            .map(describe_unloadable_skip)
            .collect(),
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

/// The user-global stella config root (`~/.stella`), or `None`
/// without a home directory.
pub(crate) fn user_config_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".stella"))
}

/// Run the sync at both scopes and report through `emit` — the shared init
/// hook (`agent::init_workspace`) calls this so `stella init` and `/init`
/// behave identically. Quiet when there is nothing to do — except a
/// `NotLoadable` skip (a namespace directory adopted from another agent's
/// dirs that stella's loader could never read), which always gets a line,
/// even in a scope where nothing else linked (issue #104: the entire point
/// is that this shape must never go unmentioned).
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
        emit_sync_outcome(scope, &outcome, emit);
    }
}

/// Emit one scope's progress lines: the `✓ adopted …` summary (only when
/// something linked), then a line for every `NotLoadable` skip — always,
/// even when nothing linked, so a scope holding only an unloadable
/// namespace directory is never silent (issue #104) — then any
/// link-creation errors.
fn emit_sync_outcome(scope: &str, outcome: &SyncOutcome, emit: &mut dyn FnMut(String)) {
    if let Some(summary) = outcome.summary() {
        let skipped = match outcome.skipped {
            0 => String::new(),
            n => format!(", {n} already present"),
        };
        emit(format!(
            "✓ adopted {summary} from .claude/.agents ({scope} scope{skipped})"
        ));
    }
    for note in &outcome.unloadable {
        emit(format!("! skipped {note} ({scope} scope)"));
    }
    for error in &outcome.errors {
        emit(format!("! extension link failed: {error}"));
    }
}

// Loading

/// Read one kind's definition files from `dir`: flat `<slug>.md` plus the
/// nested `<slug>/<nested_file>` layout, both read *through* symlinks (that
/// is the point of the sync). Returns `(path, content)` pairs; a file that
/// exists but cannot be read (e.g. a dangling symlink left by a deleted
/// source) lands in `problems` instead of vanishing.
fn read_definition_files(
    dir: &Path,
    nested_file: &str,
    problems: &mut Vec<String>,
) -> Vec<(String, String)> {
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
            match std::fs::read_to_string(&path) {
                Ok(content) => files.push((path.display().to_string(), content)),
                Err(e) => problems.push(format!("{}: {e}", path.display())),
            }
        } else if path.is_dir() {
            let nested = path.join(nested_file);
            match std::fs::read_to_string(&nested) {
                Ok(content) => files.push((nested.display().to_string(), content)),
                // A directory without its nested definition file is not an
                // error (other agents keep auxiliary dirs here); one whose
                // definition file exists but won't read is.
                Err(e) if nested.symlink_metadata().is_ok() => {
                    problems.push(format!("{}: {e}", nested.display()));
                }
                Err(_) => {}
            }
        }
    }
    files
}

/// One human-readable line for a parse diagnostic.
fn describe_diagnostic(diag: &stella_core::extensions::ExtensionDiagnostic) -> String {
    let why = match diag.problem {
        stella_core::extensions::ExtensionProblem::MissingName => "no usable name",
        stella_core::extensions::ExtensionProblem::EmptyBody => "empty body",
    };
    format!("{}: {why}", diag.path)
}

/// The `.stella`-side directories one kind is loaded from, lowest precedence
/// first (user-global, then workspace — workspace wins, like skills).
fn load_dirs(workspace_root: &Path, kind: ExtensionKind, include_workspace: bool) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(config_root) = user_config_root() {
        dirs.push(config_root.join(kind.dir_name()));
    }
    if include_workspace {
        dirs.push(workspace_root.join(".stella").join(kind.dir_name()));
    }
    dirs
}

fn load_commands_from(dirs: &[PathBuf], problems: &mut Vec<String>) -> Vec<CommandDef> {
    let mut parsed = Vec::new();
    for dir in dirs {
        for (path, raw) in read_definition_files(dir, "COMMAND.md", problems) {
            match command_from_file(&path, &raw) {
                Ok(cmd) => parsed.push(cmd),
                Err(diag) => problems.push(describe_diagnostic(&diag)),
            }
        }
    }
    merge_by_name(parsed, |c: &CommandDef| c.name.as_str())
}

fn load_agents_from(dirs: &[PathBuf], problems: &mut Vec<String>) -> Vec<AgentDef> {
    let mut parsed = Vec::new();
    for dir in dirs {
        for (path, raw) in read_definition_files(dir, "AGENT.md", problems) {
            match agent_from_file(&path, &raw) {
                Ok(agent) => parsed.push(agent),
                Err(diag) => problems.push(describe_diagnostic(&diag)),
            }
        }
    }
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
    /// Definition files that were found but skipped — unreadable, or
    /// malformed per the core parsers. One human-readable line each, so a
    /// missing `/name` or `/agents` row is diagnosable instead of silent.
    pub problems: Vec<String>,
}

/// What a custom `/name` resolves to.
pub enum Invocation<'a> {
    Command(&'a CommandDef),
    Skill(&'a Skill),
    /// An installed agent persona: `/agent-name task…` runs the task under
    /// the agent's system-prompt-shaped body (and its toolbelt note). This
    /// is the invocation seam the agent-usage telemetry records.
    Agent(&'a AgentDef),
}

impl CustomExtensions {
    /// Load user-global definitions plus project definitions permitted by the
    /// session's immutable authority snapshot.
    pub fn load_with_authority(
        workspace_root: &Path,
        authority: &crate::settings::AuthorityPolicy,
    ) -> Self {
        if crate::settings::filesystem_settings_disabled() {
            return Self::default();
        }
        Self::load_with_workspace_extensions(workspace_root, authority.project_prompts_allowed)
    }

    fn load_with_workspace_extensions(workspace_root: &Path, include_workspace: bool) -> Self {
        let mut problems = Vec::new();
        let commands = load_commands_from(
            &load_dirs(workspace_root, ExtensionKind::Commands, include_workspace),
            &mut problems,
        );
        let agents = load_agents_from(
            &load_dirs(workspace_root, ExtensionKind::Agents, include_workspace),
            &mut problems,
        );
        let loaded_skills =
            crate::memory::load_workspace_skills_with_authority(workspace_root, include_workspace);
        for diag in &loaded_skills.diagnostics {
            let why = match diag.problem {
                stella_core::skills::SkillProblem::MissingName => "no usable name",
                stella_core::skills::SkillProblem::MissingDescription => "no description",
                stella_core::skills::SkillProblem::EmptyBody => "empty body",
            };
            problems.push(format!("{}: {why}", diag.path));
        }
        Self {
            commands,
            skills: loaded_skills.skills,
            agents,
            problems,
        }
    }

    /// The skipped-definition report, one line per file, or `None` when
    /// everything on disk loaded. Both chat surfaces print this so a
    /// definition that fails to parse is visible, not silently absent.
    pub fn problems_report(&self) -> Option<String> {
        if self.problems.is_empty() {
            return None;
        }
        let mut out = format!("! {} custom definition(s) skipped:\n", self.problems.len());
        for problem in &self.problems {
            out.push_str(&format!("  ! {problem}\n"));
        }
        Some(out)
    }

    /// The ⚡ slash-menu rows: commands first, then skills, then agents,
    /// names prefixed with `/`. A custom name shadowed by a productized
    /// command in `reserved` is dropped (builtins always win), and later
    /// kinds sharing an earlier kind's name are dropped (a command was
    /// authored as an invocation; it wins).
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
        let agents = self
            .agents
            .iter()
            .map(|a| (format!("/{}", a.name), a.description.clone()));
        for (name, description) in commands.chain(skills).chain(agents) {
            if taken.insert(name.clone()) {
                rows.push(SlashCommand::custom(name, description));
            }
        }
        rows
    }

    /// Resolve a custom invocation: `head` is the leading `/word` of the
    /// input (slash included). Commands shadow skills shadow agents,
    /// matching [`Self::slash_entries`].
    pub fn lookup(&self, head: &str) -> Option<Invocation<'_>> {
        let name = head.strip_prefix('/')?;
        if let Some(cmd) = self.commands.iter().find(|c| c.name == name) {
            return Some(Invocation::Command(cmd));
        }
        if let Some(skill) = self.skills.iter().find(|s| s.name == name) {
            return Some(Invocation::Skill(skill));
        }
        self.agents
            .iter()
            .find(|a| a.name == name)
            .map(Invocation::Agent)
    }

    /// Expand `input` (`/name args…`) into the prompt the model runs, or
    /// `None` when the name is `reserved` (a productized command can never
    /// be shadowed — not in the menu, and not at invocation time either,
    /// argument-carrying forms included) or matches no custom
    /// command/skill. A command's body is its template ([`expand_command`]);
    /// a skill's body rides as context above the user's task.
    pub fn expand(&self, input: &str, reserved: &[&str]) -> Option<String> {
        let trimmed = input.trim();
        let (head, args) = match trimmed.split_once(char::is_whitespace) {
            Some((head, args)) => (head, args),
            None => (trimmed, ""),
        };
        if reserved.contains(&head) {
            return None;
        }
        match self.lookup(head)? {
            Invocation::Command(cmd) => Some(expand_command(&cmd.body, args)),
            Invocation::Skill(skill) => Some(skill_invocation_prompt(skill, args)),
            Invocation::Agent(agent) => Some(agent_invocation_prompt(agent, args)),
        }
    }

    /// The `/agents` listing: every custom agent's name, description, and
    /// source, or a hint when none are defined.
    pub fn render_agent_list(&self) -> String {
        if self.agents.is_empty() {
            return "no custom agents found — add markdown definitions to .stella/agents/ \
                    or ~/.stella/agents/ (or run /init to adopt .claude/.agents ones)"
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

/// The prompt a `/agent-name` invocation runs: the agent's persona body as
/// explicit instructions (with its toolbelt grant stated when the
/// definition restricts tools — prompt-level today; hard enforcement is the
/// fleet-spawn seam's concern), and any trailing text as the task.
fn agent_invocation_prompt(agent: &AgentDef, args: &str) -> String {
    let mut out = format!(
        "Adopt the following agent persona for this task.\n\n# Agent: {}\n{}\n\n{}",
        agent.name, agent.description, agent.body
    );
    if let Some(tools) = &agent.tools {
        out.push_str(&format!(
            "\n\nThis agent's toolbelt is restricted to: {}.",
            tools.join(", ")
        ));
    }
    let args = args.trim();
    if !args.is_empty() {
        out.push_str(&format!("\n\n## Task\n{args}"));
    }
    out
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

// Tests

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
    fn sync_skips_a_namespace_directory_with_no_nested_definition_file() {
        // The exact shape from issue #104: another agent's namespaced
        // command directory `.claude/commands/vercel/deploy.md`
        // (`/vercel:deploy` in that agent) has neither a flat `vercel.md`
        // nor a `vercel/COMMAND.md` — nothing stella's loader can read.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join(".claude/commands/vercel/deploy.md"),
            "---\nname: deploy\n---\nDeploy $ARGUMENTS.",
        );
        write(&root.join(".claude/commands/build.md"), "Build it.");

        let outcome = sync_into(
            &root.join(".stella"),
            &[root.join(".claude"), root.join(".agents")],
        );
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(
            outcome.linked,
            vec![(ExtensionKind::Commands, "build.md".to_string())],
            "the namespace directory is never linked"
        );
        // `NotLoadable` is reported through `unloadable`, not folded into
        // the benign `skipped` count (nothing here was "already present").
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.unloadable.len(), 1, "{:?}", outcome.unloadable);
        assert!(outcome.unloadable[0].contains("vercel"));
        assert!(outcome.unloadable[0].contains("not loadable"));
        assert!(!root.join(".stella/commands/vercel").exists());
    }

    #[test]
    fn sync_reports_an_unloadable_namespace_directory_even_when_nothing_else_is_found() {
        // A workspace whose ONLY entry under `.claude/commands/` is a
        // namespace directory: `outcome.summary()` is `None` since nothing
        // linked, so this must not go silent (issue #104's actual bug).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join(".claude/commands/vercel/deploy.md"), "Deploy.");

        let outcome = sync_into(
            &root.join(".stella"),
            &[root.join(".claude"), root.join(".agents")],
        );
        assert!(outcome.linked.is_empty());
        assert!(outcome.summary().is_none());
        assert_eq!(outcome.unloadable.len(), 1, "{:?}", outcome.unloadable);

        let mut lines = Vec::new();
        emit_sync_outcome("workspace", &outcome, &mut |line| lines.push(line));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("vercel"));
        assert!(lines[0].contains("not loadable"));
        assert!(lines[0].contains("workspace scope"));
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
    fn sync_resolves_frontmatter_name_collisions_by_source_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Different file names, same frontmatter `name: deploy` — only the
        // earlier source (.claude) is adopted, so the loaded `/deploy` is
        // decided by source precedence, not destination file-name order.
        write(
            &root.join(".claude/commands/ship.md"),
            "---\nname: deploy\ndescription: from claude\n---\nShip.",
        );
        write(
            &root.join(".agents/commands/release.md"),
            "---\nname: deploy\ndescription: from agents\n---\nRelease.",
        );
        let outcome = sync_into(
            &root.join(".stella"),
            &[root.join(".claude"), root.join(".agents")],
        );
        assert_eq!(
            outcome.linked,
            vec![(ExtensionKind::Commands, "ship.md".to_string())]
        );
        assert_eq!(outcome.skipped, 1);
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

        let mut problems = Vec::new();
        let commands = load_commands_from(&[user.clone(), ws.clone()], &mut problems);
        let deploy = commands.iter().find(|c| c.name == "deploy").unwrap();
        assert_eq!(deploy.description, "workspace version");
        assert!(
            commands.iter().any(|c| c.name == "review"),
            "nested layout loads"
        );
        assert!(problems.is_empty(), "{problems:?}");

        let agent_dir = tmp.path().join("ws-agents");
        write(
            &agent_dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: reviews\n---\nYou review.",
        );
        let agents = load_agents_from(&[agent_dir], &mut problems);
        assert_eq!(agents.len(), 1, "flat .md files parse as agents");
        assert_eq!(agents[0].name, "reviewer");
    }

    #[test]
    fn untrusted_project_extensions_are_excluded_while_user_extensions_remain() {
        let _env = crate::test_env::lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let workspace = tmp.path().join("workspace");
        write(
            &home.join(".stella/commands/user.md"),
            "---\ndescription: user command\n---\nUSER_COMMAND_BODY",
        );
        write(
            &workspace.join(".stella/commands/project.md"),
            "---\ndescription: project command\n---\nPROJECT_COMMAND_BODY",
        );
        write(
            &home.join(".stella/agents/user.md"),
            "---\nname: user-agent\ndescription: user agent\n---\nUSER_AGENT_BODY",
        );
        write(
            &workspace.join(".stella/agents/project.md"),
            "---\nname: project-agent\ndescription: project agent\n---\nPROJECT_AGENT_BODY",
        );
        // SAFETY: serialized behind the binary-wide environment lock.
        let previous_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };

        let custom = CustomExtensions::load_with_authority(
            &workspace,
            &crate::settings::AuthorityPolicy::default(),
        );
        let trusted = CustomExtensions::load_with_authority(
            &workspace,
            &crate::settings::AuthorityPolicy {
                project_prompts_allowed: true,
                ..crate::settings::AuthorityPolicy::default()
            },
        );

        match previous_home {
            Some(previous) => unsafe { std::env::set_var("HOME", previous) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let names: Vec<&str> = custom
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(names, vec!["user"], "loaded commands: {names:?}");
        let trusted_names: Vec<&str> = trusted
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(trusted_names, vec!["user", "project"]);
        let agent_names: Vec<&str> = custom
            .agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect();
        assert_eq!(agent_names, vec!["user-agent"]);
        let trusted_agent_names: Vec<&str> = trusted
            .agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect();
        assert_eq!(trusted_agent_names, vec!["user-agent", "project-agent"]);
    }

    #[test]
    fn loader_reports_malformed_and_unreadable_definitions() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commands");
        write(&dir.join("empty.md"), "---\nname: empty\n---\n");
        // A dangling symlink: the file exists as an entry but cannot be read.
        std::os::unix::fs::symlink(tmp.path().join("gone.md"), dir.join("dangling.md")).unwrap();

        let mut problems = Vec::new();
        let commands = load_commands_from(&[dir], &mut problems);
        assert!(commands.is_empty());
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("empty body")));
        assert!(problems.iter().any(|p| p.contains("dangling.md")));
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
                tools: None,
                body: "You review.".to_string(),
                source_path: "x/reviewer.md".to_string(),
            }],
            problems: Vec::new(),
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
        // Commands, then skills, then agents — the fixture's `reviewer`
        // agent is offered as an invocable ⚡ row too.
        assert_eq!(names, vec!["/fix-bug", "/sql-style", "/reviewer"]);
        assert!(rows.iter().all(|r| r.kind == stella_tui::SlashKind::Custom));
    }

    #[test]
    fn agent_invocations_expand_to_the_persona_prompt() {
        let custom = custom_fixture();
        let prompt = custom
            .expand("/reviewer check the diff", &[])
            .expect("the agent is invocable");
        assert!(prompt.contains("# Agent: reviewer"), "{prompt}");
        assert!(prompt.contains("You review."), "{prompt}");
        assert!(prompt.contains("## Task\ncheck the diff"), "{prompt}");
        // Commands and skills still shadow agents by name.
        match custom.lookup("/reviewer") {
            Some(Invocation::Agent(agent)) => assert_eq!(agent.name, "reviewer"),
            _ => panic!("expected the agent invocation"),
        }
    }

    #[test]
    fn expand_substitutes_command_arguments() {
        let custom = custom_fixture();
        assert_eq!(
            custom.expand("/fix-bug issue-42", &[]).as_deref(),
            Some("Fix issue-42 end to end.")
        );
        assert!(custom.expand("/unknown thing", &[]).is_none());
    }

    #[test]
    fn expand_never_runs_a_custom_definition_under_a_reserved_name() {
        let mut custom = custom_fixture();
        custom.commands.push(CommandDef {
            name: "help".to_string(),
            description: "shadowed".to_string(),
            body: "hijacked".to_string(),
            source_path: "x/help.md".to_string(),
        });
        // Hidden from the menu AND unreachable at invocation time — the
        // argument-carrying form included, which bypasses whole-input
        // builtin matching in both surfaces.
        assert!(custom.expand("/help", &["/help"]).is_none());
        assert!(custom.expand("/help topic", &["/help"]).is_none());
        // Unreserved names still expand.
        assert!(custom.expand("/fix-bug x", &["/help"]).is_some());
    }

    #[test]
    fn expand_wraps_a_skill_invocation_with_its_body_and_task() {
        let custom = custom_fixture();
        let prompt = custom.expand("/sql-style tidy my query", &[]).unwrap();
        assert!(prompt.contains("# Skill: sql-style"));
        assert!(prompt.contains("Lowercase keywords."));
        assert!(prompt.contains("## Task\ntidy my query"));
        // Bare invocation: no task section.
        let bare = custom.expand("/sql-style", &[]).unwrap();
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
