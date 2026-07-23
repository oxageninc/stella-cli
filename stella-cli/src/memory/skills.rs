//! Filesystem-backed skill discovery for session recall and extension menus.

use std::path::Path;

#[cfg(test)]
use stella_core::skills::Skill;
use stella_core::skills::{self, LoadSkillsOptions, SkillSource};

/// Filesystem-backed [`SkillSource`] reading the workspace + user-global
/// skill directories. Outside consumers use the loading functions below.
struct FsSkillSource;

impl SkillSource for FsSkillSource {
    fn read_skill_files(&self, roots: &[String]) -> Vec<skills::SkillFile> {
        let mut files = Vec::new();
        for root in roots {
            // Flat layout: <root>/<slug>.md
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "md") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            files.push(skills::SkillFile {
                                path: path.display().to_string(),
                                content,
                            });
                        }
                    } else if path.is_dir() {
                        // Ecosystem layout: <root>/<slug>/SKILL.md
                        let nested = path.join("SKILL.md");
                        if let Ok(content) = std::fs::read_to_string(&nested) {
                            files.push(skills::SkillFile {
                                path: nested.display().to_string(),
                                content,
                            });
                        }
                    }
                }
            }
        }
        files
    }
}

/// `<workspace>/.stella/skills` — the workspace-scope skills directory.
pub(crate) fn workspace_skills_dir(workspace_root: &Path) -> String {
    workspace_root
        .join(".stella")
        .join("skills")
        .display()
        .to_string()
}

/// `~/.stella/skills` — the user-global skills directory (empty
/// string without a home, which the loader skips silently).
fn user_skills_dir() -> String {
    crate::settings::user_home_dir()
        .map(|home| home.join(".stella").join("skills").display().to_string())
        .unwrap_or_default()
}

/// Load user-global skills and, when permitted, workspace skill definitions.
pub(crate) fn load_workspace_skills_with_authority(
    workspace_root: &Path,
    include_workspace: bool,
) -> skills::LoadedSkills {
    if crate::settings::filesystem_settings_disabled() {
        return skills::LoadedSkills::default();
    }
    let mut loaded = skills::load_skills_with_diagnostics(
        &FsSkillSource,
        &LoadSkillsOptions {
            workspace_skills_dir: if include_workspace {
                workspace_skills_dir(workspace_root)
            } else {
                String::new()
            },
            user_skills_dir: user_skills_dir(),
        },
    );
    // A skill disabled from the SKILLS tab is excluded from recall/selection
    // and the ⚡ slash menu — its file stays on disk (see `crate::skill_manager`).
    crate::skill_manager::retain_enabled(&mut loaded.skills, workspace_root);
    loaded
}

/// Compatibility seam for callers that deliberately request the historical
/// user-plus-workspace skill view. Authority-aware session assembly uses
/// [`load_workspace_skills_with_authority`] directly.
#[cfg(test)]
pub(crate) fn load_workspace_skills(workspace_root: &Path) -> Vec<Skill> {
    load_workspace_skills_with_authority(workspace_root, true).skills
}
