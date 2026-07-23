//! The SKILLS-tab disk half — the I/O the deck driver owns on behalf of the
//! `stella-tui` SKILLS tab (`command_deck.rs` routes the ops, this module does
//! the filesystem work). Pure `std::fs` over a workspace root + `$HOME`, so it
//! is unit-tested against tempdirs with no live npx / model.
//!
//! ## Two scopes
//!
//! Skills live in two directories the loader already reads
//! ([`crate::memory`]): the **project** scope `<workspace>/.stella/skills` and
//! the **user** scope `~/.stella/skills`. The tab shows both (a
//! same-named skill in each is listed twice, each independently manageable —
//! unlike the recall loader, which merges by name with the project winning).
//!
//! ## State that isn't in the `SKILL.md`
//!
//! `stella_core::skills::Skill` carries no enabled/version/pin state, so this
//! module keeps it in a per-scope sidecar `<skills_dir>/.stella-skills.json`
//! (invisible to the shallow recall walk, which only reads `*.md` and
//! `<slug>/SKILL.md`). Enabled/disabled and the pinned version live there;
//! version *history* lives under `<slug>/versions/vN/SKILL.md` (the versioning
//! layer). A disabled skill is excluded from recall/selection — the file stays
//! on disk (see [`retain_enabled`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use stella_core::skills::{Skill, SkillOrigin, skill_from_file_with_origin};
use stella_tui::{SkillRow, SkillScope};

/// The per-scope sidecar that records state the `SKILL.md` files don't: which
/// skills are disabled and (versioning layer) which version each is pinned to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScopeState {
    /// Skill names disabled in this scope (excluded from recall; file kept).
    #[serde(default)]
    pub disabled: Vec<String>,
    /// Skill name → pinned version (versioning layer). Absent ⇒ latest.
    #[serde(default)]
    pub pins: std::collections::BTreeMap<String, u32>,
}

const STATE_FILE: &str = ".stella-skills.json";

/// `<workspace>/.stella/skills` (project) or `~/.stella/skills` (user).
/// `None` for the user scope when `$HOME` is unset.
pub fn scope_root(scope: SkillScope, workspace_root: &Path) -> Option<PathBuf> {
    match scope {
        SkillScope::Project => Some(workspace_root.join(".stella").join("skills")),
        SkillScope::User => {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".stella").join("skills"))
        }
    }
}

/// The default [`SkillOrigin`] for a skill physically under `scope`.
fn origin_for(scope: SkillScope) -> SkillOrigin {
    match scope {
        SkillScope::Project => SkillOrigin::Workspace,
        SkillScope::User => SkillOrigin::User,
    }
}

/// A short provenance label for the tab row.
fn origin_label(origin: SkillOrigin) -> &'static str {
    match origin {
        SkillOrigin::Workspace => "workspace",
        SkillOrigin::User => "user",
        SkillOrigin::Installed => "installed",
        SkillOrigin::AutoCreated => "auto",
    }
}

// ── State file I/O ──────────────────────────────────────────────────────────

fn state_path(scope_root: &Path) -> PathBuf {
    scope_root.join(STATE_FILE)
}

/// Read a scope's sidecar state (a missing/corrupt file reads as default —
/// state is a convenience layer, never load-bearing enough to fail an op).
pub fn read_state(scope_root: &Path) -> ScopeState {
    std::fs::read_to_string(state_path(scope_root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_state(scope_root: &Path, state: &ScopeState) -> Result<(), String> {
    std::fs::create_dir_all(scope_root)
        .map_err(|e| format!("cannot create {}: {e}", scope_root.display()))?;
    let json = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(state_path(scope_root), json)
        .map_err(|e| format!("cannot write skills state: {e}"))
}

// ── Discovery (shallow, both layouts, per scope) ────────────────────────────

/// One on-disk skill entry: the direct child of the skills dir (a `<slug>.md`
/// file or a `<slug>/` directory) plus the parsed skill.
struct Entry {
    /// The direct child of the scope dir — the thing uninstall deletes.
    entry_path: PathBuf,
    skill: Skill,
}

/// List every skill directly under `scope_root`, honoring both the flat
/// `<slug>.md` and nested `<slug>/SKILL.md` layouts (shallow — never descends
/// into `<slug>/versions/`, matching the recall loader).
fn list_scope(scope: SkillScope, scope_root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(scope_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let (skill_file, content) = if path.extension().is_some_and(|e| e == "md") {
            match std::fs::read_to_string(&path) {
                Ok(c) => (path.clone(), c),
                Err(_) => continue,
            }
        } else if path.is_dir() {
            let nested = path.join("SKILL.md");
            match std::fs::read_to_string(&nested) {
                Ok(c) => (nested, c),
                Err(_) => continue,
            }
        } else {
            continue;
        };
        if let Ok(skill) = skill_from_file_with_origin(
            &skill_file.display().to_string(),
            &content,
            origin_for(scope),
        ) {
            out.push(Entry {
                entry_path: path,
                skill,
            });
        }
    }
    // Deterministic order for a stable tab + stable tests.
    out.sort_by(|a, b| a.skill.name.cmp(&b.skill.name));
    out
}

/// The highest version present under `<slug>/versions/vN/` (0 if none). The
/// versioning layer writes those; commit-1 skills report 1/1.
fn latest_version(entry_path: &Path) -> u32 {
    let versions = entry_path.join("versions");
    let Ok(entries) = std::fs::read_dir(&versions) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_prefix('v'))
                .and_then(|n| n.parse::<u32>().ok())
        })
        .max()
        .unwrap_or(0)
}

/// Enumerate every installed skill across BOTH scopes, with its enabled /
/// version / pin state, as the wire rows the SKILLS tab renders.
pub fn enumerate(workspace_root: &Path) -> Vec<SkillRow> {
    let mut rows = Vec::new();
    for scope in [SkillScope::Project, SkillScope::User] {
        let Some(root) = scope_root(scope, workspace_root) else {
            continue;
        };
        let state = read_state(&root);
        let disabled: HashSet<&String> = state.disabled.iter().collect();
        for entry in list_scope(scope, &root) {
            let name = entry.skill.name.clone();
            let latest = latest_version(&entry.entry_path).max(1);
            let version = state.pins.get(&name).copied().unwrap_or(latest).min(latest);
            rows.push(SkillRow {
                scope,
                enabled: !disabled.contains(&name),
                name,
                description: entry.skill.description,
                body: entry.skill.body,
                origin: origin_label(entry.skill.origin).to_string(),
                version,
                latest,
                // Everything under a managed scope dir is deletable — uninstall
                // is symlink-safe (it unlinks the entry, never the source).
                removable: true,
            });
        }
    }
    rows
}

// ── Enable / disable (persistent) ───────────────────────────────────────────

/// Persistently enable or disable `name` in `scope`. Returns a status string.
pub fn set_enabled(
    scope: SkillScope,
    name: &str,
    enabled: bool,
    workspace_root: &Path,
) -> Result<String, String> {
    let root = scope_root(scope, workspace_root)
        .ok_or_else(|| "no $HOME for the user scope".to_string())?;
    let mut state = read_state(&root);
    state.disabled.retain(|n| n != name);
    if !enabled {
        state.disabled.push(name.to_string());
    }
    write_state(&root, &state)?;
    Ok(format!(
        "{name} {} ({})",
        if enabled { "enabled" } else { "disabled" },
        scope.label()
    ))
}

/// The set of skill names disabled in `scope_root` (for the recall filter).
pub fn disabled_names(scope_root: &Path) -> HashSet<String> {
    read_state(scope_root).disabled.into_iter().collect()
}

/// Drop disabled skills from a merged skill list (used by the recall/selection
/// path so a disabled skill is never injected — its file stays on disk). A
/// skill is matched to its scope by `source_path` containment.
pub fn retain_enabled(skills: &mut Vec<Skill>, workspace_root: &Path) {
    let project = scope_root(SkillScope::Project, workspace_root);
    let user = scope_root(SkillScope::User, workspace_root);
    let project_disabled = project.as_deref().map(disabled_names).unwrap_or_default();
    let user_disabled = user.as_deref().map(disabled_names).unwrap_or_default();
    skills.retain(|s| {
        let under = |root: &Option<PathBuf>| {
            root.as_ref()
                .is_some_and(|r| Path::new(&s.source_path).starts_with(r))
        };
        if under(&project) {
            !project_disabled.contains(&s.name)
        } else if under(&user) {
            !user_disabled.contains(&s.name)
        } else {
            true
        }
    });
}

// ── Uninstall (symlink-safe delete, both scopes) ────────────────────────────

/// Delete `name` from `scope` entirely. Symlink-safe: it removes the entry
/// directly under the scope dir — unlinking a symlinked entry rather than
/// following it into a shared source — and refuses anything resolving outside
/// the scope dir.
pub fn uninstall(scope: SkillScope, name: &str, workspace_root: &Path) -> Result<String, String> {
    let root = scope_root(scope, workspace_root)
        .ok_or_else(|| "no $HOME for the user scope".to_string())?;
    let entry = list_scope(scope, &root)
        .into_iter()
        .find(|e| e.skill.name == name)
        .ok_or_else(|| format!("no skill named {name} in the {} scope", scope.label()))?;
    // Containment guard: the entry must be a direct child of the scope dir.
    let parent = entry.entry_path.parent();
    if parent != Some(root.as_path()) {
        return Err(format!(
            "{name} is not directly under {} — refusing to delete it",
            root.display()
        ));
    }
    let meta = std::fs::symlink_metadata(&entry.entry_path)
        .map_err(|e| format!("cannot read {}: {e}", entry.entry_path.display()))?;
    let outcome = if meta.file_type().is_symlink() || meta.is_file() {
        // Unlink a symlinked/flat entry — never follow into a shared source.
        std::fs::remove_file(&entry.entry_path)
    } else {
        std::fs::remove_dir_all(&entry.entry_path)
    };
    outcome.map_err(|e| format!("delete failed: {e}"))?;
    // Forget any lingering state for the deleted name.
    let mut state = read_state(&root);
    state.disabled.retain(|n| n != name);
    state.pins.remove(name);
    let _ = write_state(&root, &state);
    Ok(format!("deleted {name} ({})", scope.label()))
}

// ── Install adoption (from an external installer's output tree) ─────────────

/// Recursively find the first `SKILL.md` under `dir` (breadth-ish, shallow
/// dirs first via sort), so we adopt whatever `npx skills add` produced no
/// matter which subdirectory layout it used.
fn find_skill_md(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let direct = d.join("SKILL.md");
        if direct.is_file() {
            return Some(direct);
        }
        if let Ok(entries) = std::fs::read_dir(&d) {
            let mut subs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            subs.sort();
            stack.extend(subs);
        }
    }
    None
}

/// Adopt a skill produced by an external installer under `produced_dir` into
/// `scope`'s skills dir. Derives the slug from the produced `SKILL.md`'s parent
/// directory (falling back to `id`'s last path segment), copies it to
/// `<scope>/<slug>/SKILL.md`, and returns the adopted skill's name. Copying
/// (not moving) into our own dir means a user-scope install lands in
/// `~/.stella/skills` regardless of where the installer wrote — the
/// scope-destination problem the registry CLI's fixed cwd can't solve.
pub fn adopt_tree(
    scope: SkillScope,
    workspace_root: &Path,
    produced_dir: &Path,
    id: &str,
) -> Result<String, String> {
    let skill_md = find_skill_md(produced_dir)
        .ok_or_else(|| "the installer produced no SKILL.md".to_string())?;
    let content = std::fs::read_to_string(&skill_md).map_err(|e| e.to_string())?;
    let slug = skill_md
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .filter(|s| !s.eq_ignore_ascii_case("skills") && !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| id.rsplit(['/', '@']).next().unwrap_or(id).to_string());
    let dest_root = scope_root(scope, workspace_root)
        .ok_or_else(|| "no $HOME for the user scope".to_string())?;
    let dest = dest_root.join(&slug);
    // Write both the pinned `SKILL.md` the recall loader reads and a `versions/v1`
    // snapshot, so an installed skill is managed like a created one: the tab shows
    // a real version, and edit/pin start from a proper history rather than the
    // `.max(1)` fallback for a version-less entry.
    let v1 = dest.join("versions").join("v1");
    std::fs::create_dir_all(&v1).map_err(|e| format!("cannot create {}: {e}", v1.display()))?;
    std::fs::write(dest.join("SKILL.md"), &content)
        .map_err(|e| format!("cannot write skill: {e}"))?;
    std::fs::write(v1.join("SKILL.md"), &content).map_err(|e| e.to_string())?;
    let name = skill_from_file_with_origin(
        &dest.join("SKILL.md").display().to_string(),
        &content,
        SkillOrigin::Installed,
    )
    .map(|s| s.name)
    .unwrap_or(slug);
    Ok(name)
}

// ── Versioning: edit (new version), pin (no bump), create ───────────────────

/// The raw `---\n…\n---\n` frontmatter prefix of a `SKILL.md` (empty when there
/// is none / it is unterminated), so an edit can preserve the authored
/// name/description/domains verbatim and swap only the body.
fn frontmatter_prefix(content: &str) -> String {
    let s = content.strip_prefix('\u{feff}').unwrap_or(content);
    if !s.starts_with("---") {
        return String::new();
    }
    let mut prefix = String::new();
    let mut fences = 0;
    for line in s.split_inclusive('\n') {
        prefix.push_str(line);
        if line.trim_end() == "---" {
            fences += 1;
            if fences == 2 {
                return prefix;
            }
        }
    }
    String::new()
}

/// The managed directory for a skill, and the current `SKILL.md` content —
/// promoting a flat `<slug>.md` or an adopted **symlink** into a real managed
/// `<slug>/` directory with a `versions/v1/SKILL.md` snapshot first. Copy-on-
/// write: an adopted skill's symlink is broken and replaced by our own copy, so
/// editing never writes back through the link into a shared source.
fn promote_to_managed(scope: SkillScope, scope_root: &Path, name: &str) -> Result<PathBuf, String> {
    let entry = list_scope(scope, scope_root)
        .into_iter()
        .find(|e| e.skill.name == name)
        .ok_or_else(|| format!("no skill named {name} in the {} scope", scope.label()))?;
    let entry_path = entry.entry_path;
    let content = std::fs::read_to_string(&entry.skill.source_path)
        .map_err(|e| format!("cannot read {}: {e}", entry.skill.source_path))?;

    let sym = std::fs::symlink_metadata(&entry_path)
        .map_err(|e| format!("cannot read {}: {e}", entry_path.display()))?;
    let is_symlink = sym.file_type().is_symlink();
    let is_real_dir = !is_symlink && entry_path.is_dir();

    let slug = entry_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_suffix(".md").unwrap_or(n).to_string())
        .ok_or_else(|| "unusable skill path".to_string())?;
    let managed = scope_root.join(&slug);

    // Already a managed real dir with history — nothing to do.
    if is_real_dir && managed.join("versions").is_dir() {
        return Ok(managed);
    }
    // Break a symlink or delete a flat file so we own a fresh copy (CoW).
    if is_symlink || sym.is_file() {
        std::fs::remove_file(&entry_path)
            .map_err(|e| format!("cannot replace {}: {e}", entry_path.display()))?;
    }
    let v1 = managed.join("versions").join("v1");
    std::fs::create_dir_all(&v1).map_err(|e| format!("cannot create {}: {e}", v1.display()))?;
    std::fs::write(v1.join("SKILL.md"), &content).map_err(|e| e.to_string())?;
    std::fs::write(managed.join("SKILL.md"), &content).map_err(|e| e.to_string())?;
    Ok(managed)
}

/// Save an edited body as a NEW version: writes `versions/v{N+1}/SKILL.md`
/// (authored frontmatter preserved, body swapped), mirrors it to the pinned
/// `SKILL.md`, and records the new pin. Increments the version.
pub fn save_edit(
    scope: SkillScope,
    name: &str,
    new_body: &str,
    workspace_root: &Path,
) -> Result<String, String> {
    let root = scope_root(scope, workspace_root)
        .ok_or_else(|| "no $HOME for the user scope".to_string())?;
    let managed = promote_to_managed(scope, &root, name)?;
    let current = std::fs::read_to_string(managed.join("SKILL.md")).map_err(|e| e.to_string())?;
    let fm = frontmatter_prefix(&current);
    let content = if fm.is_empty() {
        format!(
            "---\nname: {name}\ndescription: {name}\n---\n{}\n",
            new_body.trim()
        )
    } else {
        format!("{fm}{}\n", new_body.trim())
    };
    let next = latest_version(&managed).max(1) + 1;
    let vdir = managed.join("versions").join(format!("v{next}"));
    std::fs::create_dir_all(&vdir).map_err(|e| e.to_string())?;
    std::fs::write(vdir.join("SKILL.md"), &content).map_err(|e| e.to_string())?;
    // The pinned SKILL.md the recall loader reads is the just-saved version.
    std::fs::write(managed.join("SKILL.md"), &content).map_err(|e| e.to_string())?;
    let mut state = read_state(&root);
    state.pins.insert(name.to_string(), next);
    write_state(&root, &state)?;
    Ok(format!("saved {name} v{next} ({})", scope.label()))
}

/// Pin `version` as the active one WITHOUT editing — mirrors that version's
/// content to `SKILL.md` and records the pin. No version bump.
pub fn set_pin(
    scope: SkillScope,
    name: &str,
    version: u32,
    workspace_root: &Path,
) -> Result<String, String> {
    let root = scope_root(scope, workspace_root)
        .ok_or_else(|| "no $HOME for the user scope".to_string())?;
    let entry = list_scope(scope, &root)
        .into_iter()
        .find(|e| e.skill.name == name)
        .ok_or_else(|| format!("no skill named {name} in the {} scope", scope.label()))?;
    let latest = latest_version(&entry.entry_path).max(1);
    if version == 0 || version > latest {
        return Err(format!("{name} has no v{version} (latest is v{latest})"));
    }
    // Mirror the chosen version's content to the pinned SKILL.md (managed skills
    // only; a single-version skill is already its own pin).
    let vfile = entry
        .entry_path
        .join("versions")
        .join(format!("v{version}"))
        .join("SKILL.md");
    if vfile.is_file() {
        let content = std::fs::read_to_string(&vfile).map_err(|e| e.to_string())?;
        std::fs::write(entry.entry_path.join("SKILL.md"), content).map_err(|e| e.to_string())?;
    }
    let mut state = read_state(&root);
    state.pins.insert(name.to_string(), version);
    write_state(&root, &state)?;
    Ok(format!("pinned {name} to v{version} ({})", scope.label()))
}

/// A filesystem-safe slug from a skill name (lowercase alnum, dashes elsewhere).
fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() { "skill".to_string() } else { s }
}

/// Create a new skill at version 1 in `scope` with the given `SKILL.md`
/// content. Errors if a skill directory of that slug already exists.
pub fn create(
    scope: SkillScope,
    name: &str,
    content: &str,
    workspace_root: &Path,
) -> Result<String, String> {
    let root = scope_root(scope, workspace_root)
        .ok_or_else(|| "no $HOME for the user scope".to_string())?;
    let slug = slugify(name);
    let managed = root.join(&slug);
    if managed.exists() {
        return Err(format!(
            "a skill dir named {slug} already exists in the {} scope",
            scope.label()
        ));
    }
    let v1 = managed.join("versions").join("v1");
    std::fs::create_dir_all(&v1).map_err(|e| format!("cannot create {}: {e}", v1.display()))?;
    std::fs::write(v1.join("SKILL.md"), content).map_err(|e| e.to_string())?;
    std::fs::write(managed.join("SKILL.md"), content).map_err(|e| e.to_string())?;
    // The parsed name (frontmatter) is what the tab/recall key on.
    let parsed = skill_from_file_with_origin(
        &managed.join("SKILL.md").display().to_string(),
        content,
        SkillOrigin::Installed,
    )
    .map(|s| s.name)
    .unwrap_or_else(|_| slug.clone());
    Ok(parsed)
}

/// A `skill name → pinned version` map across both scopes (project wins on a
/// name collision, matching the recall loader's merge) — for usage telemetry.
pub fn pinned_versions(workspace_root: &Path) -> std::collections::HashMap<String, u32> {
    let mut map = std::collections::HashMap::new();
    // User first, then project, so project overwrites on a collision.
    for scope in [SkillScope::User, SkillScope::Project] {
        if let Some(row_scope) = scope_root(scope, workspace_root) {
            let state = read_state(&row_scope);
            for entry in list_scope(scope, &row_scope) {
                let latest = latest_version(&entry.entry_path).max(1);
                let v = state
                    .pins
                    .get(&entry.skill.name)
                    .copied()
                    .unwrap_or(latest)
                    .min(latest);
                map.insert(entry.skill.name, v);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, slug: &str, desc: &str) {
        let sub = dir.join(slug);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("SKILL.md"),
            format!("---\nname: {slug}\ndescription: {desc}\n---\nbody of {slug}"),
        )
        .unwrap();
    }

    /// A tempdir workspace with `HOME` pointed inside it so the user scope is
    /// isolated. Holds the process-wide env lock for the whole test (returned
    /// as the third element) — `setenv`/`getenv` races are UB and the harness
    /// runs these on parallel threads. Returns (workspace_root, home, lock).
    fn scratch() -> (
        tempfile::TempDir,
        PathBuf,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let lock = crate::test_env::lock();
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: the env lock above serializes every HOME-mutating test in
        // this binary; the guard is held for the caller's whole test.
        unsafe {
            std::env::set_var("HOME", &home);
        }
        (td, home, lock)
    }

    #[test]
    fn enumerate_reads_both_scopes() {
        let (td, home, _lock) = scratch();
        let ws = td.path().join("ws");
        write_skill(&ws.join(".stella/skills"), "proj-skill", "a project skill");
        write_skill(&home.join(".stella/skills"), "user-skill", "a user skill");
        let rows = enumerate(&ws);
        assert!(rows.iter().any(|r| r.name == "proj-skill"
            && r.scope == SkillScope::Project
            && r.origin == "workspace"));
        assert!(
            rows.iter().any(|r| r.name == "user-skill"
                && r.scope == SkillScope::User
                && r.origin == "user")
        );
        assert!(rows.iter().all(|r| r.enabled), "all enabled by default");
    }

    #[test]
    fn disable_persists_and_recall_filter_drops_it() {
        let (td, _home, _lock) = scratch();
        let ws = td.path().join("ws");
        write_skill(&ws.join(".stella/skills"), "sql-style", "format sql");
        write_skill(&ws.join(".stella/skills"), "keep-me", "keep");

        set_enabled(SkillScope::Project, "sql-style", false, &ws).unwrap();
        // Persisted across a fresh read: the tab still lists it, marked off.
        let rows = enumerate(&ws);
        let sql = rows.iter().find(|r| r.name == "sql-style").unwrap();
        assert!(!sql.enabled, "disable persisted");
        assert!(rows.iter().find(|r| r.name == "keep-me").unwrap().enabled);

        // The recall/selection loader now filters disabled skills (the file
        // stays on disk — the tab still shows it above), keeping the enabled one.
        let recalled = crate::memory::load_workspace_skills_with_authority(&ws, true).skills;
        assert!(
            !recalled.iter().any(|s| s.name == "sql-style"),
            "disabled excluded from recall"
        );
        assert!(
            recalled.iter().any(|s| s.name == "keep-me"),
            "enabled kept in recall"
        );

        // Re-enable brings it back everywhere.
        set_enabled(SkillScope::Project, "sql-style", true, &ws).unwrap();
        assert!(
            enumerate(&ws)
                .iter()
                .find(|r| r.name == "sql-style")
                .unwrap()
                .enabled
        );
        assert!(
            crate::memory::load_workspace_skills_with_authority(&ws, true)
                .skills
                .iter()
                .any(|s| s.name == "sql-style")
        );
    }

    #[test]
    fn uninstall_deletes_the_directory() {
        let (td, _home, _lock) = scratch();
        let ws = td.path().join("ws");
        let dir = ws.join(".stella/skills");
        write_skill(&dir, "doomed", "delete me");
        assert!(dir.join("doomed/SKILL.md").exists());

        let msg = uninstall(SkillScope::Project, "doomed", &ws).unwrap();
        assert!(msg.contains("deleted doomed"));
        assert!(!dir.join("doomed").exists(), "directory removed");
        assert!(enumerate(&ws).iter().all(|r| r.name != "doomed"));
    }

    #[test]
    fn uninstall_a_symlinked_entry_unlinks_without_touching_the_source() {
        let (td, _home, _lock) = scratch();
        let ws = td.path().join("ws");
        let dir = ws.join(".stella/skills");
        std::fs::create_dir_all(&dir).unwrap();
        // A shared source outside the scope dir, adopted as a symlink.
        let source = ws.join("shared/adopted");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: adopted\ndescription: shared\n---\nbody",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, dir.join("adopted")).unwrap();

        #[cfg(unix)]
        {
            uninstall(SkillScope::Project, "adopted", &ws).unwrap();
            assert!(!dir.join("adopted").exists(), "symlink unlinked");
            assert!(
                source.join("SKILL.md").exists(),
                "shared source untouched — never followed"
            );
        }
    }

    #[test]
    fn uninstall_unknown_skill_is_an_error() {
        let (td, _home, _lock) = scratch();
        let ws = td.path().join("ws");
        write_skill(&ws.join(".stella/skills"), "present", "here");
        assert!(uninstall(SkillScope::Project, "absent", &ws).is_err());
    }

    #[test]
    fn edit_creates_a_new_version_pins_it_and_preserves_frontmatter() {
        let (td, _home, _lock) = scratch();
        let ws = td.path().join("ws");
        write_skill(&ws.join(".stella/skills"), "sql-style", "format sql");

        let msg = save_edit(SkillScope::Project, "sql-style", "New body v2", &ws).unwrap();
        assert!(msg.contains("v2"), "{msg}");

        let dir = ws.join(".stella/skills/sql-style");
        assert!(dir.join("versions/v1/SKILL.md").exists(), "v1 snapshotted");
        assert!(dir.join("versions/v2/SKILL.md").exists(), "v2 written");
        let pinned = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(pinned.contains("New body v2"), "pinned is the new version");
        assert!(
            pinned.contains("description: format sql"),
            "frontmatter preserved"
        );

        let row = enumerate(&ws)
            .into_iter()
            .find(|r| r.name == "sql-style")
            .unwrap();
        assert_eq!(row.version, 2);
        assert_eq!(row.latest, 2);

        // A second edit bumps to v3.
        save_edit(SkillScope::Project, "sql-style", "body v3", &ws).unwrap();
        let row = enumerate(&ws)
            .into_iter()
            .find(|r| r.name == "sql-style")
            .unwrap();
        assert_eq!(row.version, 3);
        assert_eq!(row.latest, 3);
    }

    #[test]
    fn pin_switches_the_active_version_without_bumping() {
        let (td, _home, _lock) = scratch();
        let ws = td.path().join("ws");
        write_skill(&ws.join(".stella/skills"), "s", "d");
        save_edit(SkillScope::Project, "s", "second", &ws).unwrap(); // now v2, pinned v2

        // Pin back to v1 — no new version is created.
        let msg = set_pin(SkillScope::Project, "s", 1, &ws).unwrap();
        assert!(msg.contains("v1"), "{msg}");
        let row = enumerate(&ws).into_iter().find(|r| r.name == "s").unwrap();
        assert_eq!(row.version, 1, "pinned back to v1");
        assert_eq!(row.latest, 2, "history unchanged — no bump");
        let pinned = std::fs::read_to_string(ws.join(".stella/skills/s/SKILL.md")).unwrap();
        assert!(pinned.contains("body of s"), "SKILL.md mirrors v1");

        // Pinning a nonexistent version errors.
        assert!(set_pin(SkillScope::Project, "s", 9, &ws).is_err());
    }

    #[test]
    fn create_writes_a_v1_managed_skill() {
        let (td, _home, _lock) = scratch();
        let ws = td.path().join("ws");
        let content = "---\nname: my-skill\ndescription: does things\n---\nthe body";
        let name = create(SkillScope::Project, "my-skill", content, &ws).unwrap();
        assert_eq!(name, "my-skill");
        assert!(
            ws.join(".stella/skills/my-skill/versions/v1/SKILL.md")
                .exists()
        );
        let row = enumerate(&ws)
            .into_iter()
            .find(|r| r.name == "my-skill")
            .unwrap();
        assert_eq!(row.version, 1);
        // Recreating the same slug is refused.
        assert!(create(SkillScope::Project, "my-skill", content, &ws).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn editing_an_adopted_symlink_is_copy_on_write() {
        let (td, _home, _lock) = scratch();
        let ws = td.path().join("ws");
        let dir = ws.join(".stella/skills");
        std::fs::create_dir_all(&dir).unwrap();
        let source = ws.join("shared/adopted");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: adopted\ndescription: shared\n---\noriginal body",
        )
        .unwrap();
        std::os::unix::fs::symlink(&source, dir.join("adopted")).unwrap();

        save_edit(SkillScope::Project, "adopted", "edited body", &ws).unwrap();

        // The shared source is untouched (never written through the link).
        let src = std::fs::read_to_string(source.join("SKILL.md")).unwrap();
        assert!(
            src.contains("original body"),
            "shared source preserved: {src}"
        );
        // Our copy owns the edit, and the symlink was replaced by a real dir.
        assert!(
            !dir.join("adopted")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let ours = std::fs::read_to_string(dir.join("adopted/SKILL.md")).unwrap();
        assert!(ours.contains("edited body"));
    }

    #[test]
    fn adopt_tree_pulls_a_produced_skill_into_the_user_scope() {
        let (td, home, _lock) = scratch();
        let ws = td.path().join("ws");
        // Simulate what `npx skills add acme/auth` leaves behind in its cwd.
        let produced = td.path().join("produced");
        write_skill(
            &produced.join(".claude/skills"),
            "auth-helper",
            "oauth helper",
        );

        let name = adopt_tree(SkillScope::User, &ws, &produced, "acme/auth").unwrap();
        assert_eq!(name, "auth-helper");
        // It landed in ~/.stella/skills, NOT the project — proving the
        // scope is honored regardless of the installer's cwd.
        assert!(home.join(".stella/skills/auth-helper/SKILL.md").exists());
        assert!(
            enumerate(&ws)
                .iter()
                .any(|r| r.name == "auth-helper" && r.scope == SkillScope::User)
        );
        // An adopted skill has a real v1 history (not the version-less fallback),
        // so the tab shows a version and edit/pin start from a proper baseline.
        assert!(
            home.join(".stella/skills/auth-helper/versions/v1/SKILL.md")
                .exists(),
            "adopted skill gets a versions/v1 snapshot"
        );
        let row = enumerate(&ws)
            .into_iter()
            .find(|r| r.name == "auth-helper")
            .unwrap();
        assert_eq!(row.version, 1);
        assert_eq!(row.latest, 1);
    }
}
