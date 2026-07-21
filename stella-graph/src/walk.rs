//! Directory walk that yields indexable source files.
//!
//! L-S5 makes ripgrep-semantics (gitignore-aware,
//! hidden-excluded) the contract for search tooling. This module implements a
//! **deny-list approximation** rather than pulling ripgrep's `ignore` crate:
//! it skips hidden directories (leading `.`), the usual build/vendor/generated
//! caches (`target`, `node_modules`, `dist`, `build`, `out`, `.next`,
//! `dist-standalone`, `vendor`, `__pycache__`, `venv`, …), and never follows
//! symlinked directories (cycle safety), then keeps only files whose
//! extension [`Language::from_path`] recognizes.
//!
//! This is the cheap, structural half of generated/minified exclusion
//! (issue #272) — a whole directory of build output never even gets opened.
//! The other half — `.gitattributes` `linguist-generated=true`, the
//! `*.min.*` filename convention, and minified-content sniffing for files
//! that live outside any denied directory — runs per-file once content is
//! already in hand, in [`crate::generated`] via `crate::store::index_one`.
//!
//! **Documented gap (task brief, ignore-discipline clause):** custom
//! `.gitignore` patterns (per-directory ignore files, negations, glob rules)
//! are *not* honored — only the built-in deny-list is. Swapping this for the
//! `ignore` crate is a clean, self-contained upgrade (it would be a new
//! dependency in this crate's own manifest); it is deferred here to avoid
//! churning the shared `Cargo.lock` under parallel workstreams. The walk is
//! otherwise structurally identical, so the swap is behind this one function.

use std::path::{Path, PathBuf};

use crate::lang::Language;

/// Directory names that never contain first-party source worth indexing.
/// `dist`/`build`/`out`/`node_modules`/`vendor` are classic build/vendor
/// caches; `.next` and `dist-standalone` are generated-bundle directories
/// specifically called out by issue #272 (a checked-in minified
/// `dist-standalone/*.mjs` bundle was polluting recall with zero-signal
/// frames).
const DENY_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "dist-standalone",
    "build",
    "out",
    ".next",
    "__pycache__",
    "venv",
    ".venv",
    "coverage",
    "vendor",
];

/// Whether a directory should be skipped: hidden (leading `.`) or on the
/// build/vendor deny-list.
fn is_denied_dir(name: &str) -> bool {
    name.starts_with('.') || DENY_DIRS.contains(&name)
}

/// Whether a path *below `root`* falls in an ignored directory — the same
/// deny-list the walk applies, used to filter live watcher events. Only the
/// portion under `root` is examined, so a workspace that itself lives inside a
/// hidden or vendor directory (e.g. `~/.cache/proj`) is not wrongly excluded.
/// A path outside `root` is treated as ignored.
pub(crate) fn rel_is_ignored(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return true;
    };
    rel.components().any(|component| match component {
        std::path::Component::Normal(name) => {
            let name = name.to_string_lossy();
            name.starts_with('.') || DENY_DIRS.contains(&name.as_ref())
        }
        _ => false,
    })
}

/// Walk `root` and return every indexable source file (absolute paths).
/// Iterative (explicit stack) so deeply-nested trees cannot overflow the
/// call stack. Unreadable directories are skipped, never fatal.
pub(crate) fn walk_indexable(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            // Symlinks report as neither is_dir nor is_file here, so they are
            // ignored — no symlink-cycle risk, no double-indexing.
            if file_type.is_dir() {
                let name = entry.file_name();
                if !is_denied_dir(&name.to_string_lossy()) {
                    stack.push(entry.path());
                }
            } else if file_type.is_file() {
                let path = entry.path();
                if Language::from_path(&path).is_some()
                    || crate::storage::indexes_without_language(&path)
                {
                    out.push(path);
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Issue #272: a checked-in minified bundle under `dist-standalone/`
    /// (found nested inside a monorepo app directory, not at the workspace
    /// root) must never reach the indexable set.
    #[test]
    fn dist_standalone_and_next_directories_are_never_walked() {
        let ws = TempDir::new().unwrap();
        let root = ws.path();
        fs::create_dir_all(root.join("apps/cli/dist-standalone")).unwrap();
        fs::write(
            root.join("apps/cli/dist-standalone/oxagen.mjs"),
            "function refresh() {}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("apps/web/.next/static")).unwrap();
        fs::write(
            root.join("apps/web/.next/static/chunk.js"),
            "function chunk() {}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("apps/cli/src")).unwrap();
        fs::write(root.join("apps/cli/src/main.rs"), "fn main() {}\n").unwrap();

        let files = walk_indexable(root);
        assert_eq!(files.len(), 1, "only real source is walked: {files:?}");
        assert!(files[0].ends_with("apps/cli/src/main.rs"));
    }

    #[test]
    fn is_denied_dir_covers_generated_bundle_directories() {
        for name in [
            "dist",
            "dist-standalone",
            "build",
            "out",
            "vendor",
            "node_modules",
        ] {
            assert!(is_denied_dir(name), "{name} should be denied");
        }
        assert!(is_denied_dir(".next"), ".next is denied (hidden + listed)");
        assert!(!is_denied_dir("src"));
    }
}
