//! Custom extensions — user-authored **commands** and **agents** parsed from
//! markdown-with-frontmatter definition files, plus the init-time
//! **symlink-sync plan** that adopts definitions authored for other code
//! agents (`.claude/`, `.agents/`) into stella's own directories.
//!
//! A **command** is a reusable prompt template: invoking `/name args` in a
//! composer substitutes `args` into the command's markdown body and runs the
//! result as the model prompt. An **agent** is a named persona definition —
//! a system-prompt-shaped markdown body with a `name:`/`description:`
//! frontmatter header — listed by the `/agents` command (and the seam a
//! future fleet spawn will consume). Both are distinct from a
//! [`crate::skills`] *skill* (know-how selected into context automatically)
//! and a [`crate::rules`] *rule* (an enforceable guardrail).
//!
//! Directory conventions mirror the skills engine:
//! `<workspace>/.stella/commands/` and `<workspace>/.stella/agents/`
//! (workspace scope, wins on name collisions) and the user-global
//! `~/.config/stella/{commands,agents}/`. Files are flat `<slug>.md`; the
//! frontmatter `name:` overrides the filename stem.
//!
//! # Adoption from other agents' directories (the sync plan)
//!
//! Ecosystem tools already maintain `.claude/{commands,skills,agents}` and
//! `.agents/{commands,skills,agents}`. On `stella init`, entries found there
//! are **symlinked** into the matching `.stella/`-side directory so stella
//! sees them without copying (edits stay in one place). Two rules keep the
//! adoption sane, both encoded in [`plan_extension_sync`]:
//!
//!   1. **Never link a symlink.** Other agents symlink between these
//!      directories too (e.g. `.claude/skills/x -> ../../.agents/skills/x`);
//!      following one would adopt the same definition twice — once through
//!      the symlink, once from its real home, which is also scanned.
//!   2. **Never clobber.** A name already present in the destination —
//!      user-authored or linked by a previous init — is skipped, so the sync
//!      is idempotent and hand-written definitions always win.
//!
//! # No I/O in this module (`02-architecture.md` §1.3)
//!
//! Scanning directories, reading definition files, and creating symlinks are
//! real I/O — `stella-cli` owns all of it. This module only plans and
//! parses: [`plan_extension_sync`] maps already-scanned entries to typed
//! actions, and the `*_from_file` parsers operate on already-read content,
//! unit-tested below without touching a filesystem.

use crate::rules::parse_frontmatter;

// ============================================================================
// Kinds
// ============================================================================

/// The three definition kinds the sync adopts and the loaders read. The
/// directory name is identical across every convention involved
/// (`.claude/<dir>`, `.agents/<dir>`, `.stella/<dir>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtensionKind {
    Commands,
    Skills,
    Agents,
}

impl ExtensionKind {
    /// Every kind, in the order sync reports use.
    pub const ALL: [ExtensionKind; 3] = [
        ExtensionKind::Commands,
        ExtensionKind::Skills,
        ExtensionKind::Agents,
    ];

    /// The directory basename this kind lives under, in every convention.
    pub fn dir_name(self) -> &'static str {
        match self {
            ExtensionKind::Commands => "commands",
            ExtensionKind::Skills => "skills",
            ExtensionKind::Agents => "agents",
        }
    }
}

// ============================================================================
// Definitions + parsing
// ============================================================================

/// A custom slash command: a prompt template invoked as `/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDef {
    /// The bare command name, **without** the leading slash (frontmatter
    /// `name:` or the filename stem). `/`-prefixed on display.
    pub name: String,
    /// Menu description — frontmatter `description:`, falling back to the
    /// body's first line (see [`fallback_description`]).
    pub description: String,
    /// The markdown body: the prompt template ([`expand_command`]).
    pub body: String,
    /// The file this was loaded from.
    pub source_path: String,
}

/// A custom agent definition: a persona with a system-prompt-shaped body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    /// Agent name (frontmatter `name:` or the filename stem).
    pub name: String,
    /// One-line description for the `/agents` listing.
    pub description: String,
    /// The markdown body — the agent's instructions/system prompt.
    pub body: String,
    /// The file this was loaded from.
    pub source_path: String,
}

/// Why one definition file could not be loaded. Mirrors
/// [`crate::skills::SkillProblem`]: typed so the CLI can report it precisely
/// instead of silently dropping the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionProblem {
    /// No `name:` frontmatter and the path yields no usable slug.
    MissingName,
    /// The markdown body is empty — a template/persona with no content.
    EmptyBody,
}

/// A malformed definition file, skipped during loading (never fatal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionDiagnostic {
    pub path: String,
    pub problem: ExtensionProblem,
}

/// Cap on a description derived from the body's first line — menu rows are
/// one line; anything longer is noise there.
const FALLBACK_DESCRIPTION_MAX_CHARS: usize = 72;

/// The filename stem (or, for a nested `<slug>/<FILE>.md` layout, the parent
/// directory name) as the definition's slug — same derivation as
/// [`crate::skills`], generalized over the nested filename.
fn name_from_path(path: &str, nested_file: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.eq_ignore_ascii_case(nested_file) {
        return path.rsplit('/').nth(1).unwrap_or("").to_string();
    }
    base.strip_suffix(".md").unwrap_or(base).to_string()
}

/// A one-line description from a body when the frontmatter has none: the
/// first non-empty line, markdown-heading markers stripped, truncated on a
/// char boundary with an ellipsis.
fn fallback_description(body: &str) -> String {
    let line = body
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() <= FALLBACK_DESCRIPTION_MAX_CHARS {
        return line.to_string();
    }
    let truncated: String = line
        .chars()
        .take(FALLBACK_DESCRIPTION_MAX_CHARS - 1)
        .collect();
    format!("{}…", truncated.trim_end())
}

/// Parse one definition file into `(name, description, body)` — the shape
/// both [`command_from_file`] and [`agent_from_file`] share. `nested_file`
/// names the per-directory layout file (`COMMAND.md`/`AGENT.md`) accepted
/// alongside flat `<slug>.md`.
fn definition_from_file(
    path: &str,
    raw: &str,
    nested_file: &str,
) -> Result<(String, String, String), ExtensionDiagnostic> {
    let fm = parse_frontmatter(raw);
    let diag = |problem| ExtensionDiagnostic {
        path: path.to_string(),
        problem,
    };

    let name = fm
        .data
        .get("name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| name_from_path(path, nested_file));
    if name.trim().is_empty() {
        return Err(diag(ExtensionProblem::MissingName));
    }

    let body = fm.body.trim().to_string();
    if body.is_empty() {
        return Err(diag(ExtensionProblem::EmptyBody));
    }

    let description = fm
        .data
        .get("description")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_description(&body));

    Ok((name, description, body))
}

/// Parse one command file. Tolerant like the skills loader: `description:`
/// is optional (many ecosystem command files carry only a body), falling
/// back to the body's first line.
pub fn command_from_file(path: &str, raw: &str) -> Result<CommandDef, ExtensionDiagnostic> {
    let (name, description, body) = definition_from_file(path, raw, "COMMAND.md")?;
    Ok(CommandDef {
        name,
        description,
        body,
        source_path: path.to_string(),
    })
}

/// Parse one agent file.
pub fn agent_from_file(path: &str, raw: &str) -> Result<AgentDef, ExtensionDiagnostic> {
    let (name, description, body) = definition_from_file(path, raw, "AGENT.md")?;
    Ok(AgentDef {
        name,
        description,
        body,
        source_path: path.to_string(),
    })
}

// ============================================================================
// Command expansion
// ============================================================================

/// The placeholder a command body may use to position its arguments.
pub const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";

/// Expand a command body with the text the user typed after the command
/// name: every `$ARGUMENTS` occurrence is substituted; a body without the
/// placeholder gets non-empty arguments appended as a trailing paragraph
/// (the ecosystem convention, so plain prompt files still take arguments).
pub fn expand_command(body: &str, args: &str) -> String {
    let args = args.trim();
    if body.contains(ARGUMENTS_PLACEHOLDER) {
        return body.replace(ARGUMENTS_PLACEHOLDER, args);
    }
    if args.is_empty() {
        body.to_string()
    } else {
        format!("{body}\n\n{args}")
    }
}

// ============================================================================
// Name-merged loading (user-global, then workspace — workspace wins)
// ============================================================================

/// Merge definitions by `key`, later entries overriding earlier ones while
/// keeping the first-seen ordering position — the same semantics as
/// [`crate::skills::load_skills`], so "workspace beats user-global" works by
/// passing user-scope definitions first.
pub fn merge_by_name<T>(items: Vec<T>, key: impl Fn(&T) -> &str) -> Vec<T> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: std::collections::HashMap<String, T> = std::collections::HashMap::new();
    for item in items {
        let name = key(&item).to_string();
        if !by_name.contains_key(&name) {
            order.push(name.clone());
        }
        by_name.insert(name, item);
    }
    order
        .into_iter()
        .filter_map(|name| by_name.remove(&name))
        .collect()
}

// ============================================================================
// Symlink-sync planning
// ============================================================================

/// One directory entry found while scanning a source directory
/// (`.claude/<kind>` or `.agents/<kind>`), as reported by the CLI's scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncEntry {
    /// The entry's basename — also the link name in the destination.
    pub name: String,
    /// The entry's absolute path (the symlink target when planned).
    pub path: String,
    /// `true` when the entry is itself a symlink (`fs::symlink_metadata`).
    pub is_symlink: bool,
    /// The name the definition will *load* under — the frontmatter `name:`
    /// when the scanner could parse one, else the filename-derived slug.
    /// Collision precedence is decided on this, not on `name`: two files
    /// with different basenames but the same frontmatter name are the same
    /// definition, and linking both would leave the loader's merge order
    /// (destination-lexicographic) to pick a winner instead of source order.
    pub definition_name: String,
}

/// The scanned contents of one source directory for one kind, in the
/// caller's precedence order (earlier sources win on a name collision).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSource {
    pub kind: ExtensionKind,
    pub entries: Vec<SyncEntry>,
}

/// Why one scanned entry was not planned as a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncSkipReason {
    /// The source entry is itself a symlink — following it would duplicate
    /// the definition it points at, whose real home is also scanned.
    SourceIsSymlink,
    /// The destination already has this file name (a previous sync or a
    /// user-authored definition) — never clobbered, which is also what makes
    /// re-running the sync idempotent.
    DestinationExists,
    /// A definition with this *loaded* name is already claimed — by an
    /// earlier source directory this run, or by something already in the
    /// destination under a different file name.
    DuplicateName,
}

/// One symlink the CLI should create: `<dest>/<kind>/<name>` → `source_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedLink {
    pub kind: ExtensionKind,
    pub name: String,
    pub source_path: String,
}

/// One entry the plan skipped, with the typed reason (inspectable output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSkip {
    pub kind: ExtensionKind,
    pub name: String,
    pub source_path: String,
    pub reason: SyncSkipReason,
}

/// The full sync decision for one destination root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub links: Vec<PlannedLink>,
    pub skips: Vec<SyncSkip>,
}

impl SyncPlan {
    /// How many links the plan creates for `kind`.
    pub fn linked(&self, kind: ExtensionKind) -> usize {
        self.links.iter().filter(|l| l.kind == kind).count()
    }
}

/// What already occupies one kind's destination directory, as scanned by the
/// CLI: the entry basenames (the never-clobber check is filesystem-level) and
/// the names those entries' definitions load under (so a re-run — or a
/// differently-named file carrying the same frontmatter `name:` — is not
/// adopted a second time).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExistingTargets {
    pub file_names: Vec<String>,
    pub definition_names: Vec<String>,
}

/// Plan the adoption of scanned source entries into one destination root.
/// `sources` are in precedence order (e.g. `.claude/<kind>` before
/// `.agents/<kind>`); `existing` describes each kind's destination directory.
/// Deterministic: actions come out in scan order. See the module doc for the
/// two rules (never link a symlink, never clobber); collision precedence
/// between sources is decided on [`SyncEntry::definition_name`].
pub fn plan_extension_sync(
    sources: &[SyncSource],
    existing: &dyn Fn(ExtensionKind) -> ExistingTargets,
) -> SyncPlan {
    let mut plan = SyncPlan::default();
    let mut claimed: std::collections::HashSet<(ExtensionKind, String)> =
        std::collections::HashSet::new();
    let mut present_files: std::collections::HashMap<
        ExtensionKind,
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();
    for source in sources {
        if let std::collections::hash_map::Entry::Vacant(vacant) = present_files.entry(source.kind)
        {
            let targets = existing(source.kind);
            vacant.insert(targets.file_names.into_iter().collect());
            // Definitions already in the destination claim their names up
            // front, so a same-named-but-differently-filed source entry is
            // skipped instead of linked alongside.
            for name in targets.definition_names {
                claimed.insert((source.kind, name));
            }
        }
        let present = &present_files[&source.kind];
        for entry in &source.entries {
            let skip = |reason| SyncSkip {
                kind: source.kind,
                name: entry.name.clone(),
                source_path: entry.path.clone(),
                reason,
            };
            if entry.is_symlink {
                plan.skips.push(skip(SyncSkipReason::SourceIsSymlink));
            } else if present.contains(&entry.name) {
                plan.skips.push(skip(SyncSkipReason::DestinationExists));
            } else if !claimed.insert((source.kind, entry.definition_name.clone())) {
                plan.skips.push(skip(SyncSkipReason::DuplicateName));
            } else {
                plan.links.push(PlannedLink {
                    kind: source.kind,
                    name: entry.name.clone(),
                    source_path: entry.path.clone(),
                });
            }
        }
    }
    plan
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parsing ----

    #[test]
    fn command_parses_frontmatter_name_and_description() {
        let raw = "---\nname: fix-bug\ndescription: Fix the named bug end to end\n---\nFind and fix the bug: $ARGUMENTS";
        let cmd = command_from_file("/ws/.stella/commands/other.md", raw).unwrap();
        assert_eq!(cmd.name, "fix-bug");
        assert_eq!(cmd.description, "Fix the named bug end to end");
        assert!(cmd.body.contains("$ARGUMENTS"));
    }

    #[test]
    fn command_name_falls_back_to_the_filename_stem() {
        let cmd = command_from_file("/ws/.stella/commands/review-pr.md", "Review the PR.").unwrap();
        assert_eq!(cmd.name, "review-pr");
    }

    #[test]
    fn command_supports_the_nested_layout() {
        let cmd = command_from_file("/ws/.stella/commands/deploy/COMMAND.md", "Ship it.").unwrap();
        assert_eq!(cmd.name, "deploy");
    }

    #[test]
    fn command_description_falls_back_to_the_first_body_line() {
        let raw = "# Review the diff carefully\n\nThen report findings.";
        let cmd = command_from_file("/x/review.md", raw).unwrap();
        assert_eq!(cmd.description, "Review the diff carefully");
    }

    #[test]
    fn fallback_description_truncates_long_first_lines_on_a_char_boundary() {
        let long = "é".repeat(200);
        let desc = fallback_description(&long);
        assert!(desc.chars().count() <= FALLBACK_DESCRIPTION_MAX_CHARS);
        assert!(desc.ends_with('…'));
    }

    #[test]
    fn empty_body_is_a_typed_diagnostic() {
        let err = command_from_file("/x/cmd.md", "---\nname: cmd\n---\n").unwrap_err();
        assert_eq!(err.problem, ExtensionProblem::EmptyBody);
        let err = agent_from_file("/x/agent.md", "").unwrap_err();
        assert_eq!(err.problem, ExtensionProblem::EmptyBody);
    }

    #[test]
    fn missing_name_with_unusable_path_is_a_typed_diagnostic() {
        let err = command_from_file("COMMAND.md", "body").unwrap_err();
        assert_eq!(err.problem, ExtensionProblem::MissingName);
    }

    #[test]
    fn agent_parses_like_claude_code_agent_files() {
        let raw = "---\nname: code-reviewer\ndescription: Reviews diffs for correctness\ntools: Read, Grep\n---\nYou are a meticulous reviewer.";
        let agent = agent_from_file("/ws/.stella/agents/code-reviewer.md", raw).unwrap();
        assert_eq!(agent.name, "code-reviewer");
        assert_eq!(agent.description, "Reviews diffs for correctness");
        assert!(agent.body.contains("meticulous"));
    }

    // ---- expansion ----

    #[test]
    fn expand_substitutes_every_arguments_placeholder() {
        let body = "Fix $ARGUMENTS. When done, verify $ARGUMENTS is closed.";
        assert_eq!(
            expand_command(body, "issue-42"),
            "Fix issue-42. When done, verify issue-42 is closed."
        );
    }

    #[test]
    fn expand_appends_args_when_no_placeholder_exists() {
        assert_eq!(
            expand_command("Review the diff.", "focus on tests"),
            "Review the diff.\n\nfocus on tests"
        );
    }

    #[test]
    fn expand_without_args_leaves_the_body_alone_and_clears_placeholders() {
        assert_eq!(expand_command("Review the diff.", "  "), "Review the diff.");
        assert_eq!(expand_command("Fix $ARGUMENTS now", ""), "Fix  now");
    }

    // ---- merging ----

    #[test]
    fn merge_by_name_lets_later_entries_override_earlier_ones_in_place() {
        let items = vec![("a", 1), ("b", 2), ("a", 3)];
        let merged = merge_by_name(items, |i| i.0);
        assert_eq!(merged, vec![("a", 3), ("b", 2)]);
    }

    // ---- sync planning ----

    fn entry(name: &str, path: &str, is_symlink: bool) -> SyncEntry {
        SyncEntry {
            name: name.to_string(),
            path: path.to_string(),
            is_symlink,
            // Tests that exercise frontmatter-name collisions build their
            // own entries; everywhere else the definition name is the file
            // name minus a `.md` suffix, like the scanner's fallback.
            definition_name: name.strip_suffix(".md").unwrap_or(name).to_string(),
        }
    }

    fn no_existing(_: ExtensionKind) -> ExistingTargets {
        ExistingTargets::default()
    }

    #[test]
    fn plan_links_real_entries_and_skips_symlinked_ones() {
        // The exact shape found in the wild: `.claude/skills` holds a mix of
        // real directories and symlinks into `.agents/skills`.
        let sources = vec![
            SyncSource {
                kind: ExtensionKind::Skills,
                entries: vec![
                    entry("motion", "/h/.claude/skills/motion", false),
                    entry("ai-sdk", "/h/.claude/skills/ai-sdk", true),
                ],
            },
            SyncSource {
                kind: ExtensionKind::Skills,
                entries: vec![entry("ai-sdk", "/h/.agents/skills/ai-sdk", false)],
            },
        ];
        let plan = plan_extension_sync(&sources, &no_existing);
        let linked: Vec<(&str, &str)> = plan
            .links
            .iter()
            .map(|l| (l.name.as_str(), l.source_path.as_str()))
            .collect();
        // `ai-sdk` is adopted exactly once, from its REAL home in `.agents`.
        assert_eq!(
            linked,
            vec![
                ("motion", "/h/.claude/skills/motion"),
                ("ai-sdk", "/h/.agents/skills/ai-sdk"),
            ]
        );
        assert_eq!(plan.skips.len(), 1);
        assert_eq!(plan.skips[0].reason, SyncSkipReason::SourceIsSymlink);
    }

    #[test]
    fn plan_never_clobbers_an_existing_destination_name() {
        let sources = vec![SyncSource {
            kind: ExtensionKind::Commands,
            entries: vec![
                entry("deploy", "/ws/.claude/commands/deploy.md", false),
                entry("new-cmd", "/ws/.claude/commands/new-cmd.md", false),
            ],
        }];
        let existing = |kind: ExtensionKind| {
            assert_eq!(kind, ExtensionKind::Commands);
            ExistingTargets {
                file_names: vec!["deploy".to_string()],
                definition_names: vec!["deploy".to_string()],
            }
        };
        let plan = plan_extension_sync(&sources, &existing);
        assert_eq!(plan.links.len(), 1);
        assert_eq!(plan.links[0].name, "new-cmd");
        assert_eq!(plan.skips[0].reason, SyncSkipReason::DestinationExists);
    }

    #[test]
    fn plan_resolves_collisions_on_the_frontmatter_name_not_the_file_name() {
        // Two differently-named files whose frontmatter says `name: deploy`:
        // linking both would leave the loader's merge order to pick a winner.
        // Source precedence (`.claude` first) must decide instead.
        let deploy = |name: &str, path: &str| SyncEntry {
            name: name.to_string(),
            path: path.to_string(),
            is_symlink: false,
            definition_name: "deploy".to_string(),
        };
        let sources = vec![
            SyncSource {
                kind: ExtensionKind::Commands,
                entries: vec![deploy("ship.md", "/ws/.claude/commands/ship.md")],
            },
            SyncSource {
                kind: ExtensionKind::Commands,
                entries: vec![deploy("release.md", "/ws/.agents/commands/release.md")],
            },
        ];
        let plan = plan_extension_sync(&sources, &no_existing);
        assert_eq!(plan.links.len(), 1);
        assert_eq!(plan.links[0].source_path, "/ws/.claude/commands/ship.md");
        assert_eq!(plan.skips[0].reason, SyncSkipReason::DuplicateName);
    }

    #[test]
    fn plan_skips_a_source_whose_definition_name_is_already_in_the_destination() {
        // The destination holds `mine.md` with frontmatter `name: deploy`;
        // a source file `deploy.md` (same loaded name, different file name)
        // must not be linked alongside it.
        let sources = vec![SyncSource {
            kind: ExtensionKind::Commands,
            entries: vec![entry("deploy.md", "/ws/.claude/commands/deploy.md", false)],
        }];
        let existing = |_: ExtensionKind| ExistingTargets {
            file_names: vec!["mine.md".to_string()],
            definition_names: vec!["deploy".to_string()],
        };
        let plan = plan_extension_sync(&sources, &existing);
        assert!(plan.links.is_empty());
        assert_eq!(plan.skips[0].reason, SyncSkipReason::DuplicateName);
    }

    #[test]
    fn plan_gives_the_earlier_source_precedence_on_a_real_name_collision() {
        let sources = vec![
            SyncSource {
                kind: ExtensionKind::Agents,
                entries: vec![entry("reviewer", "/ws/.claude/agents/reviewer.md", false)],
            },
            SyncSource {
                kind: ExtensionKind::Agents,
                entries: vec![entry("reviewer", "/ws/.agents/agents/reviewer.md", false)],
            },
        ];
        let plan = plan_extension_sync(&sources, &no_existing);
        assert_eq!(plan.links.len(), 1);
        assert_eq!(plan.links[0].source_path, "/ws/.claude/agents/reviewer.md");
        assert_eq!(plan.skips[0].reason, SyncSkipReason::DuplicateName);
    }

    #[test]
    fn rerunning_the_plan_over_a_synced_destination_is_a_no_op() {
        let sources = vec![SyncSource {
            kind: ExtensionKind::Skills,
            entries: vec![entry("motion", "/h/.claude/skills/motion", false)],
        }];
        let first = plan_extension_sync(&sources, &no_existing);
        assert_eq!(first.links.len(), 1);
        // Second run: the destination now contains what the first run linked.
        let after = ExistingTargets {
            file_names: first.links.iter().map(|l| l.name.clone()).collect(),
            definition_names: first.links.iter().map(|l| l.name.clone()).collect(),
        };
        let second = plan_extension_sync(&sources, &move |_| after.clone());
        assert!(second.links.is_empty());
        assert_eq!(second.skips[0].reason, SyncSkipReason::DestinationExists);
    }

    #[test]
    fn plan_counts_links_per_kind() {
        let sources = vec![
            SyncSource {
                kind: ExtensionKind::Skills,
                entries: vec![entry("a", "/s/a", false), entry("b", "/s/b", false)],
            },
            SyncSource {
                kind: ExtensionKind::Commands,
                entries: vec![entry("c", "/c/c.md", false)],
            },
        ];
        let plan = plan_extension_sync(&sources, &no_existing);
        assert_eq!(plan.linked(ExtensionKind::Skills), 2);
        assert_eq!(plan.linked(ExtensionKind::Commands), 1);
        assert_eq!(plan.linked(ExtensionKind::Agents), 0);
    }
}
