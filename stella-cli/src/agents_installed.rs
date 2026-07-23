//! Installed-agent management for the deck's INSTALLED AGENTS pane:
//! discovery across both config scopes, filesystem-first **versioning with
//! an explicit pin**, and the LLM-assisted create-from-prompt helpers.
//!
//! ## Where agents live
//!
//! Same directories the loaders (`crate::extensions`) read:
//!
//! - **project**: `<workspace>/.stella/agents/`
//! - **user**: `~/.stella/agents/`
//!
//! A definition is either a flat `<slug>.md` or a nested `<slug>/AGENT.md`
//! (markdown with `name:`/`description:`/`tools:` frontmatter, parsed by
//! `stella_core::extensions::agent_from_file`).
//!
//! ## The versioning + pin scheme (filesystem-first, inspectable)
//!
//! ```text
//! <agents-dir>/<slug>.md                  ← the LOADED definition: a
//!                                            materialized copy of the
//!                                            PINNED version (what every
//!                                            loader — and every other
//!                                            tool — reads, no code needed)
//! <agents-dir>/.versions/<slug>/v0001.md  ← immutable version files,
//!                                 v0002.md   oldest first
//!                                 …
//! <agents-dir>/.versions/<slug>/PINNED    ← one line: the pinned version
//!                                            number (e.g. `2`)
//! ```
//!
//! - An agent with **no** `.versions/<slug>/` directory is *implicitly
//!   version 1* — the canonical file is its only version. Nothing is
//!   created until the first edit, so hand-authored agents stay exactly as
//!   their authors wrote them.
//! - **Edit-save** ([`save_new_version`]): the current canonical content is
//!   archived first (as `v0001` if the agent was never versioned), the new
//!   content is written as `v<max+1>`, `PINNED` is set to it, and the
//!   canonical file is rewritten. Saving **always creates a new version and
//!   pins it**; no prior version file is ever modified or deleted.
//! - **Pin-set** ([`pin_version`]): `PINNED` is set to an *existing*
//!   version and the canonical file is rewritten from that version's file.
//!   **No new version is created** — pin changes never increment anything.
//! - The `.versions` directory is dot-prefixed, so every definition loader
//!   (which skips hidden entries) ignores it by construction.
//!
//! Symlinked definitions (adopted from `.claude/`/`.agents/` by the init
//! sync) are handled conservatively on the first save/pin: the link is
//! replaced by a real flat `<slug>.md` in the agents dir — editing through
//! the link would silently rewrite another tool's source file.

use std::path::{Path, PathBuf};

use stella_core::extensions::agent_from_file;
use stella_protocol::CompletionMessage;
use stella_tui::{AgentScope, AgentVersionInfo, InstalledAgentEntry};

/// The project-scope agents directory.
pub fn project_agents_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".stella").join("agents")
}

/// The user-scope agents directory (`~/.stella/agents`), or `None`
/// without a home directory.
pub fn user_agents_dir() -> Option<PathBuf> {
    crate::extensions::user_config_root().map(|root| root.join("agents"))
}

/// The agents directory for one scope.
pub fn agents_dir_for(scope: AgentScope, workspace_root: &Path) -> Result<PathBuf, String> {
    match scope {
        AgentScope::Project => Ok(project_agents_dir(workspace_root)),
        AgentScope::User => {
            user_agents_dir().ok_or_else(|| "no home directory for the user scope".to_string())
        }
    }
}

// Discovery

/// One definition entry found in an agents dir: its slug (the versioning
/// key) and the canonical file the loader reads.
struct FoundDef {
    slug: String,
    canonical: PathBuf,
}

/// Enumerate the definition entries in `dir`: flat `<slug>.md` plus nested
/// `<slug>/AGENT.md`, hidden entries skipped (which is exactly what keeps
/// `.versions/` invisible to discovery). Sorted by slug for determinism.
fn find_defs(dir: &Path) -> Vec<FoundDef> {
    let mut defs = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "md") {
            let slug = name.strip_suffix(".md").unwrap_or(&name).to_string();
            defs.push(FoundDef {
                slug,
                canonical: path,
            });
        } else if path.is_dir() && path.join("AGENT.md").symlink_metadata().is_ok() {
            defs.push(FoundDef {
                slug: name,
                canonical: path.join("AGENT.md"),
            });
        }
    }
    defs.sort_by(|a, b| a.slug.cmp(&b.slug));
    defs
}

/// Discover the installed agents in the two scope directories, project
/// first then user, each row tagged with its scope. Both scopes are listed
/// even on a name collision (they are separate installs; the load-time
/// merge is the loaders' concern, not the manager's).
pub fn discover(user_dir: Option<&Path>, project_dir: &Path) -> Vec<InstalledAgentEntry> {
    let mut entries = Vec::new();
    let mut scan = |dir: &Path, scope: AgentScope| {
        for def in find_defs(dir) {
            let Ok(raw) = std::fs::read_to_string(&def.canonical) else {
                continue; // dangling symlink etc. — the loaders diagnose it
            };
            let Ok(parsed) = agent_from_file(&def.canonical.display().to_string(), &raw) else {
                continue; // malformed — the loaders diagnose it
            };
            let versions = list_versions(dir, &def.slug, &def.canonical);
            let version = active_version(dir, &def.slug);
            entries.push(InstalledAgentEntry {
                name: parsed.name,
                description: parsed.description,
                tools: parsed.tools,
                scope,
                source_path: def.canonical.display().to_string(),
                version,
                versions,
                content: raw,
            });
        }
    };
    scan(project_dir, AgentScope::Project);
    if let Some(user_dir) = user_dir {
        scan(user_dir, AgentScope::User);
    }
    entries
}

/// The slug (versioning key) of the installed agent named `name` in `dir`,
/// resolved through the same discovery the list pane shows.
pub fn find_slug(dir: &Path, name: &str) -> Option<String> {
    find_defs(dir).into_iter().find_map(|def| {
        let raw = std::fs::read_to_string(&def.canonical).ok()?;
        let parsed = agent_from_file(&def.canonical.display().to_string(), &raw).ok()?;
        (parsed.name == name).then_some(def.slug)
    })
}

// Versions + pin

/// The `.versions/<slug>` directory for one agent.
fn versions_dir(agents_dir: &Path, slug: &str) -> PathBuf {
    agents_dir.join(".versions").join(slug)
}

/// The version numbers archived for `slug`, sorted ascending. Empty when
/// the agent was never versioned.
fn archived_versions(agents_dir: &Path, slug: &str) -> Vec<u32> {
    let mut versions: Vec<u32> = std::fs::read_dir(versions_dir(agents_dir, slug))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.strip_prefix('v')?
                .strip_suffix(".md")?
                .parse::<u32>()
                .ok()
        })
        .collect();
    versions.sort_unstable();
    versions
}

/// The version file path for `version` of `slug`.
fn version_file(agents_dir: &Path, slug: &str, version: u32) -> PathBuf {
    versions_dir(agents_dir, slug).join(format!("v{version:04}.md"))
}

/// The pinned (active) version of `slug`: the `PINNED` manifest when it
/// names an archived version, the newest archive as a fallback, and the
/// implicit `1` for a never-versioned agent.
pub fn active_version(agents_dir: &Path, slug: &str) -> u32 {
    let archived = archived_versions(agents_dir, slug);
    if archived.is_empty() {
        return 1;
    }
    let pinned = std::fs::read_to_string(versions_dir(agents_dir, slug).join("PINNED"))
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok());
    match pinned {
        Some(v) if archived.contains(&v) => v,
        // No / stale manifest: the newest archive is what the last save
        // materialized.
        _ => *archived.last().expect("non-empty"),
    }
}

/// The pinned version of the definition at `source_path` (the invocation
/// telemetry's version stamp). Resolves the agents dir + slug from the
/// canonical path shape (`<dir>/<slug>.md` or `<dir>/<slug>/AGENT.md`).
pub fn active_version_for_source(source_path: &str) -> u32 {
    let path = Path::new(source_path);
    let (dir, slug) = match (path.file_name().and_then(|n| n.to_str()), path.parent()) {
        (Some("AGENT.md"), Some(agent_dir)) => match (
            agent_dir.parent(),
            agent_dir.file_name().and_then(|n| n.to_str()),
        ) {
            (Some(dir), Some(slug)) => (dir, slug.to_string()),
            _ => return 1,
        },
        (Some(file), Some(dir)) => (dir, file.strip_suffix(".md").unwrap_or(file).to_string()),
        _ => return 1,
    };
    active_version(dir, &slug)
}

/// Every version of `slug` as picker rows, oldest first — the archived
/// files, or the implicit single version for a never-versioned agent
/// (labeled from the canonical file's mtime).
fn list_versions(agents_dir: &Path, slug: &str, canonical: &Path) -> Vec<AgentVersionInfo> {
    let archived = archived_versions(agents_dir, slug);
    if archived.is_empty() {
        return vec![AgentVersionInfo {
            version: 1,
            label: mtime_label(canonical),
        }];
    }
    archived
        .into_iter()
        .map(|version| AgentVersionInfo {
            version,
            label: mtime_label(&version_file(agents_dir, slug, version)),
        })
        .collect()
}

/// A short `YYYY-MM-DD HH:MM` label from a file's mtime, or empty when the
/// filesystem has nothing to say.
fn mtime_label(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else {
        return String::new();
    };
    let Ok(mtime) = meta.modified() else {
        return String::new();
    };
    let Ok(unix) = mtime.duration_since(std::time::UNIX_EPOCH) else {
        return String::new();
    };
    let rfc = stella_context::format_rfc3339(unix.as_secs() as i64);
    // `2026-07-16T10:12:00Z` → `2026-07-16 10:12`
    rfc.get(..16).unwrap_or(&rfc).replace('T', " ")
}

/// Where `slug`'s canonical definition currently lives, if anywhere:
/// `(path, entry)` where `entry` is the directory entry to inspect for a
/// symlink (the flat file itself, or the nested layout's directory).
fn canonical_of(agents_dir: &Path, slug: &str) -> Option<(PathBuf, PathBuf)> {
    let flat = agents_dir.join(format!("{slug}.md"));
    if flat.symlink_metadata().is_ok() {
        return Some((flat.clone(), flat));
    }
    let nested_dir = agents_dir.join(slug);
    let nested = nested_dir.join("AGENT.md");
    if nested.symlink_metadata().is_ok() {
        return Some((nested, nested_dir));
    }
    None
}

/// Rewrite `slug`'s canonical definition to `content`. A symlinked entry
/// (flat file or nested directory) is replaced by a real flat `<slug>.md` —
/// never written through, so an adopted `.claude/` source is never silently
/// rewritten. A real nested layout is written in place.
fn materialize(agents_dir: &Path, slug: &str, content: &str) -> Result<(), String> {
    let write =
        |path: &Path| std::fs::write(path, content).map_err(|e| format!("{}: {e}", path.display()));
    match canonical_of(agents_dir, slug) {
        Some((path, entry)) => {
            let is_symlink = entry
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if is_symlink {
                std::fs::remove_file(&entry).map_err(|e| format!("{}: {e}", entry.display()))?;
                write(&agents_dir.join(format!("{slug}.md")))
            } else {
                write(&path)
            }
        }
        None => {
            std::fs::create_dir_all(agents_dir).map_err(|e| e.to_string())?;
            write(&agents_dir.join(format!("{slug}.md")))
        }
    }
}

/// Save `content` as a NEW version of `slug` and pin it (the edit-save
/// path). The prior canonical content is preserved: archived as `v0001` if
/// the agent was never versioned. Returns the new version number.
pub fn save_new_version(agents_dir: &Path, slug: &str, content: &str) -> Result<u32, String> {
    let vdir = versions_dir(agents_dir, slug);
    std::fs::create_dir_all(&vdir).map_err(|e| format!("{}: {e}", vdir.display()))?;

    let mut archived = archived_versions(agents_dir, slug);
    if archived.is_empty() {
        // First edit of a hand-authored (never-versioned) agent: its current
        // content becomes v1 so the edit destroys nothing.
        if let Some((canonical, _)) = canonical_of(agents_dir, slug)
            && let Ok(current) = std::fs::read_to_string(&canonical)
        {
            let v1 = version_file(agents_dir, slug, 1);
            std::fs::write(&v1, current).map_err(|e| format!("{}: {e}", v1.display()))?;
            archived.push(1);
        }
    }
    let next = archived.last().copied().unwrap_or(0) + 1;
    let vfile = version_file(agents_dir, slug, next);
    std::fs::write(&vfile, content).map_err(|e| format!("{}: {e}", vfile.display()))?;
    write_pin(agents_dir, slug, next)?;
    materialize(agents_dir, slug, content)?;
    Ok(next)
}

/// Re-pin `slug` to an EXISTING `version` (the pin-set path): the canonical
/// file is rewritten from that version's archive and `PINNED` updated.
/// Never creates a version — pinning is not an edit.
pub fn pin_version(agents_dir: &Path, slug: &str, version: u32) -> Result<(), String> {
    let archived = archived_versions(agents_dir, slug);
    if archived.is_empty() {
        // A never-versioned agent is implicitly v1 — re-pinning it to 1 is
        // a no-op; anything else names a version that does not exist.
        return if version == 1 {
            Ok(())
        } else {
            Err(format!("{slug} has no version v{version}"))
        };
    }
    if !archived.contains(&version) {
        return Err(format!(
            "{slug} has no version v{version} (available: {})",
            archived
                .iter()
                .map(|v| format!("v{v}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let vfile = version_file(agents_dir, slug, version);
    let content =
        std::fs::read_to_string(&vfile).map_err(|e| format!("{}: {e}", vfile.display()))?;
    write_pin(agents_dir, slug, version)?;
    materialize(agents_dir, slug, &content)?;
    Ok(())
}

fn write_pin(agents_dir: &Path, slug: &str, version: u32) -> Result<(), String> {
    let pin = versions_dir(agents_dir, slug).join("PINNED");
    std::fs::write(&pin, format!("{version}\n")).map_err(|e| format!("{}: {e}", pin.display()))
}

// Create from prompt (LLM-assisted)

/// A parsed LLM-drafted agent definition, ready to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedAgent {
    /// The kebab-case slug the file is installed under.
    pub slug: String,
    /// The loaded definition name (frontmatter `name:`).
    pub name: String,
    /// The full file content (frontmatter + body).
    pub content: String,
}

/// The one-shot completion messages that draft an agent definition from the
/// user's short description — pure prompt assembly, unit-testable without a
/// provider (the driver hands the result to `Provider::complete`, the same
/// invocation path the reflection module uses).
pub fn creation_messages(description: &str) -> Vec<CompletionMessage> {
    let system = "You write agent definition files for the stella CLI. An agent definition \
                  is a markdown file with YAML frontmatter and a body, in exactly this shape:\n\
                  ---\n\
                  name: kebab-case-name\n\
                  description: one line, under 80 characters, saying what the agent does\n\
                  tools: Comma, Separated, Tool, Names\n\
                  ---\n\
                  The agent's system prompt: persona, instructions, constraints.\n\n\
                  Rules:\n\
                  - `name` must be short, kebab-case, and specific.\n\
                  - Include the `tools:` line ONLY when the agent should be restricted to a \
                  subset of tools; omit it entirely to grant all tools.\n\
                  - The body must be a complete, self-contained system prompt.\n\
                  - Respond with ONLY the complete markdown file — no code fences, no \
                  commentary before or after.";
    vec![
        CompletionMessage::system(system),
        CompletionMessage::user(format!("Write an agent definition for: {description}")),
    ]
}

/// Parse a model's drafted definition: code fences stripped if present, the
/// result validated through the real loader parser (`agent_from_file`) so
/// anything installed is guaranteed loadable.
pub fn parse_generated_agent(text: &str) -> Result<GeneratedAgent, String> {
    let content = strip_fences(text).trim().to_string();
    if content.is_empty() {
        return Err("the model returned an empty definition".to_string());
    }
    let parsed = agent_from_file("generated.md", &content).map_err(|diag| {
        format!(
            "the drafted definition is not loadable ({})",
            match diag.problem {
                stella_core::extensions::ExtensionProblem::MissingName => "no usable name",
                stella_core::extensions::ExtensionProblem::EmptyBody => "empty body",
            }
        )
    })?;
    let slug = slugify(&parsed.name);
    if slug.is_empty() {
        return Err(format!("`{}` yields no usable slug", parsed.name));
    }
    Ok(GeneratedAgent {
        slug,
        name: parsed.name,
        content: format!("{content}\n"),
    })
}

/// Install a freshly created agent as `<slug>.md` in `agents_dir`. Refuses
/// to clobber an existing definition — creation never overwrites. No
/// `.versions` bookkeeping yet: a new agent is implicitly v1 until its
/// first edit (see the module docs).
pub fn install_new_agent(agents_dir: &Path, agent: &GeneratedAgent) -> Result<PathBuf, String> {
    if canonical_of(agents_dir, &agent.slug).is_some() {
        return Err(format!(
            "an agent named `{}` already exists — edit it instead",
            agent.slug
        ));
    }
    std::fs::create_dir_all(agents_dir).map_err(|e| e.to_string())?;
    let path = agents_dir.join(format!("{}.md", agent.slug));
    std::fs::write(&path, &agent.content).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// `"Code Reviewer!"` → `"code-reviewer"`: lowercase, non-alphanumeric runs
/// collapsed to single dashes, trimmed.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash_pending = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if dash_pending && !out.is_empty() {
                out.push('-');
            }
            dash_pending = false;
            out.push(c.to_ascii_lowercase());
        } else {
            dash_pending = true;
        }
    }
    out
}

/// The fence-stripping half of [`parse_generated_agent`]: a response whose
/// first non-empty line opens a code fence has the fence lines removed
/// (info string tolerated); anything else passes through untouched.
fn strip_fences(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }
    let mut lines: Vec<&str> = trimmed.lines().collect();
    lines.remove(0); // the opening fence (``` or ```markdown)
    if lines.last().is_some_and(|l| l.trim().starts_with("```")) {
        lines.pop();
    }
    lines.join("\n")
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_md(name: &str, tools: Option<&str>, body: &str) -> String {
        let tools_line = tools.map(|t| format!("tools: {t}\n")).unwrap_or_default();
        format!("---\nname: {name}\ndescription: about {name}\n{tools_line}---\n{body}\n")
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    // discovery

    #[test]
    fn discover_lists_both_scopes_with_toolbelt_and_scope_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("ws/.stella/agents");
        let user = tmp.path().join("home/.stella/agents");
        write(
            &project.join("reviewer.md"),
            &agent_md("reviewer", Some("Read, Grep"), "Review diffs."),
        );
        // Nested layout at the user scope, no tools restriction.
        write(
            &user.join("planner/AGENT.md"),
            &agent_md("planner", None, "Plan the work."),
        );

        let entries = discover(Some(&user), &project);
        assert_eq!(entries.len(), 2);
        let reviewer = entries.iter().find(|e| e.name == "reviewer").unwrap();
        assert_eq!(reviewer.scope, AgentScope::Project);
        assert_eq!(
            reviewer.tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
        assert_eq!(reviewer.version, 1, "never versioned = implicit v1");
        assert_eq!(reviewer.versions.len(), 1);
        assert!(reviewer.content.contains("Review diffs."));

        let planner = entries.iter().find(|e| e.name == "planner").unwrap();
        assert_eq!(planner.scope, AgentScope::User);
        assert_eq!(planner.tools, None, "no tools key = all tools");
        assert!(planner.source_path.ends_with("planner/AGENT.md"));
    }

    #[test]
    fn discover_skips_hidden_entries_and_missing_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join(".stella/agents");
        write(&project.join("real.md"), &agent_md("real", None, "Body."));
        // The version store must never be listed as an agent.
        write(
            &project.join(".versions/real/v0001.md"),
            &agent_md("real", None, "Old body."),
        );
        let entries = discover(None, &project);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "real");
        // A nonexistent dir yields nothing rather than erroring.
        assert!(discover(None, &tmp.path().join("nope")).is_empty());
    }

    // versioning + pin

    #[test]
    fn save_creates_a_new_version_pins_it_and_preserves_the_old_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents");
        let original = agent_md("reviewer", Some("Read"), "Old body.");
        write(&dir.join("reviewer.md"), &original);

        let edited = agent_md("reviewer", Some("Read, Grep"), "New body.");
        let version = save_new_version(&dir, "reviewer", &edited).unwrap();
        assert_eq!(version, 2, "the original was archived as v1");

        // The new version is pinned/used: the canonical file carries it.
        assert_eq!(active_version(&dir, "reviewer"), 2);
        assert_eq!(
            std::fs::read_to_string(dir.join("reviewer.md")).unwrap(),
            edited
        );
        // The prior version is preserved, byte for byte.
        assert_eq!(
            std::fs::read_to_string(dir.join(".versions/reviewer/v0001.md")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".versions/reviewer/v0002.md")).unwrap(),
            edited
        );
        assert_eq!(archived_versions(&dir, "reviewer"), vec![1, 2]);
    }

    #[test]
    fn pin_set_changes_the_active_version_without_creating_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents");
        let v1 = agent_md("reviewer", None, "Version one.");
        write(&dir.join("reviewer.md"), &v1);
        let v2 = agent_md("reviewer", None, "Version two.");
        save_new_version(&dir, "reviewer", &v2).unwrap();
        assert_eq!(active_version(&dir, "reviewer"), 2);

        // Pin back to v1 WITHOUT editing.
        pin_version(&dir, "reviewer", 1).unwrap();
        assert_eq!(active_version(&dir, "reviewer"), 1, "the pin moved");
        assert_eq!(
            std::fs::read_to_string(dir.join("reviewer.md")).unwrap(),
            v1,
            "the canonical file is materialized from the pinned version"
        );
        assert_eq!(
            archived_versions(&dir, "reviewer"),
            vec![1, 2],
            "a pin change never increments the version count"
        );

        // …and a later edit continues the sequence from the max, not the pin.
        let v3 = agent_md("reviewer", None, "Version three.");
        assert_eq!(save_new_version(&dir, "reviewer", &v3).unwrap(), 3);
        assert_eq!(active_version(&dir, "reviewer"), 3);
    }

    #[test]
    fn pinning_a_nonexistent_version_is_a_typed_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents");
        write(&dir.join("a.md"), &agent_md("a", None, "Body."));
        assert!(pin_version(&dir, "a", 1).is_ok(), "implicit v1 re-pin = ok");
        assert!(pin_version(&dir, "a", 2).is_err());
        save_new_version(&dir, "a", &agent_md("a", None, "B2.")).unwrap();
        let err = pin_version(&dir, "a", 9).unwrap_err();
        assert!(err.contains("no version v9"), "{err}");
    }

    #[test]
    fn version_listing_reports_every_version_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents");
        write(&dir.join("a.md"), &agent_md("a", None, "One."));
        save_new_version(&dir, "a", &agent_md("a", None, "Two.")).unwrap();
        save_new_version(&dir, "a", &agent_md("a", None, "Three.")).unwrap();

        let entries = discover(None, &dir);
        let versions: Vec<u32> = entries[0].versions.iter().map(|v| v.version).collect();
        assert_eq!(versions, vec![1, 2, 3]);
        assert_eq!(entries[0].version, 3, "newest save is pinned");
    }

    #[test]
    fn saving_over_a_symlinked_definition_never_writes_through_the_link() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents");
        let foreign = tmp.path().join(".claude/agents/adopted.md");
        let original = agent_md("adopted", None, "Foreign body.");
        write(&foreign, &original);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&foreign, dir.join("adopted.md")).unwrap();

        let edited = agent_md("adopted", None, "Edited in stella.");
        save_new_version(&dir, "adopted", &edited).unwrap();

        assert_eq!(
            std::fs::read_to_string(&foreign).unwrap(),
            original,
            "the .claude source is untouched"
        );
        let canonical = dir.join("adopted.md");
        assert!(
            !canonical
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was replaced by a real file"
        );
        assert_eq!(std::fs::read_to_string(&canonical).unwrap(), edited);
        // The foreign content was still archived as v1 before the edit.
        assert_eq!(
            std::fs::read_to_string(dir.join(".versions/adopted/v0001.md")).unwrap(),
            original
        );
    }

    #[test]
    fn nested_layout_is_versioned_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents");
        let original = agent_md("nested", None, "One.");
        write(&dir.join("nested/AGENT.md"), &original);
        let edited = agent_md("nested", None, "Two.");
        save_new_version(&dir, "nested", &edited).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("nested/AGENT.md")).unwrap(),
            edited,
            "a real nested layout keeps its shape"
        );
        assert_eq!(active_version(&dir, "nested"), 2);
    }

    #[test]
    fn active_version_for_source_resolves_both_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents");
        write(&dir.join("flat.md"), &agent_md("flat", None, "One."));
        save_new_version(&dir, "flat", &agent_md("flat", None, "Two.")).unwrap();
        assert_eq!(
            active_version_for_source(&dir.join("flat.md").display().to_string()),
            2
        );
        write(
            &dir.join("nested/AGENT.md"),
            &agent_md("nested", None, "B."),
        );
        assert_eq!(
            active_version_for_source(&dir.join("nested/AGENT.md").display().to_string()),
            1
        );
    }

    #[test]
    fn find_slug_resolves_a_frontmatter_name_to_its_file_slug() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents");
        // The file slug and the loaded name differ.
        write(
            &dir.join("cr.md"),
            &agent_md("code-reviewer", None, "Body."),
        );
        assert_eq!(find_slug(&dir, "code-reviewer").as_deref(), Some("cr"));
        assert_eq!(find_slug(&dir, "nope"), None);
    }

    // creation

    #[test]
    fn creation_messages_carry_the_format_contract_and_the_description() {
        let messages = creation_messages("reviews rust diffs for unsafe code");
        assert_eq!(messages.len(), 2);
        let system = &messages[0].content;
        assert!(system.contains("name: kebab-case-name"), "{system}");
        assert!(system.contains("omit it entirely to grant all tools"));
        assert!(system.contains("no code fences"));
        assert!(
            messages[1]
                .content
                .contains("reviews rust diffs for unsafe code")
        );
    }

    #[test]
    fn parse_generated_agent_strips_fences_and_validates_via_the_loader() {
        let drafted = "```markdown\n---\nname: Unsafe Auditor\ndescription: Audits unsafe \
                       blocks\ntools: Read, Grep\n---\nYou audit unsafe Rust.\n```";
        let agent = parse_generated_agent(drafted).unwrap();
        assert_eq!(agent.slug, "unsafe-auditor");
        assert_eq!(agent.name, "Unsafe Auditor");
        assert!(!agent.content.contains("```"), "fences stripped");
        assert!(agent.content.ends_with("You audit unsafe Rust.\n"));
    }

    #[test]
    fn parse_generated_agent_rejects_unloadable_drafts() {
        let err = parse_generated_agent("---\nname: x\n---\n").unwrap_err();
        assert!(err.contains("empty body"), "{err}");
        assert!(parse_generated_agent("   ").is_err());
    }

    #[test]
    fn install_new_agent_writes_the_file_and_never_clobbers() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents");
        let agent = GeneratedAgent {
            slug: "fresh".into(),
            name: "fresh".into(),
            content: agent_md("fresh", None, "Body."),
        };
        let path = install_new_agent(&dir, &agent).unwrap();
        assert!(path.ends_with("fresh.md"));
        let err = install_new_agent(&dir, &agent).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn slugify_collapses_and_lowercases() {
        assert_eq!(slugify("Code Reviewer!"), "code-reviewer");
        assert_eq!(slugify("  A -- B  "), "a-b");
        assert_eq!(slugify("---"), "");
    }
}
