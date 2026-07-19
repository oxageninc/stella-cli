//! `stella-tools` — the built-in tool set the agent loop calls.
//!
//! Every tool implements [`Tool`], takes a JSON input from the model, and
//! returns a [`ToolOutput`] (success or a typed, named error — never a bare
//! string). Tools are workspace-root-pinned: file paths resolve against the
//! configured root, and `cd` inside `bash` cannot silently diverge the root
//! for subsequent file operations (L-S2).

pub mod agent_use;
pub mod bash;
pub mod ci;
pub mod custom;
pub mod delete;
pub mod edit;
pub mod exec;
pub mod exploration;
pub mod file_touch;
pub mod gather;
pub mod github_rest;
pub mod glob;
pub mod graph;
pub mod grep;
pub mod hook_runner;
pub mod issue_ops;
pub mod issues;
pub mod media;
pub mod memory;
pub mod project;
pub mod read;
pub mod registry;
pub mod sandbox;
pub mod schema_gate;
pub mod screenshot;
pub mod tasks;
pub mod tracker_auth;
pub mod validate;
pub mod verify;
pub mod write;

pub use registry::ToolRegistry;

/// Resolve `path` against `root` and verify the result stays inside `root`.
///
/// `Path::starts_with` is a *lexical* component comparison — it does not
/// resolve `..`.  On Linux where `std::env::temp_dir()` is `/tmp`, the path
/// `../../etc/passwd` joined to `/tmp` produces `/tmp/../../etc/passwd` which
/// *lexically* starts with `/tmp` but *semantically* resolves to `/etc/passwd`.
///
/// This helper canonicalises `root` (which must exist) and then normalises
/// the joined path.  If the file already exists, it is canonicalised
/// directly.  If it does not exist (e.g. `write_file` creating a new file),
/// the path is normalised by walking up to the deepest existing ancestor,
/// canonicalising that, and re-appending the remaining non-existent
/// components.  If the normalised full path does not start with the
/// canonicalised root, the path escapes the workspace and the caller must
/// reject it.
///
/// Returns the resolved full path on success, or `None` if the path
/// escapes the workspace root or `root` itself cannot be canonicalised.
pub fn resolve_within_root(root: &std::path::Path, path: &str) -> Option<std::path::PathBuf> {
    // Canonicalise root — it must exist (it's the workspace root).
    let canon_root = root.canonicalize().ok()?;

    let joined = canon_root.join(path);

    // If the file already exists, canonicalise it directly.
    if let Ok(canon) = joined.canonicalize() {
        return if canon.starts_with(&canon_root) {
            Some(canon)
        } else {
            None
        };
    }

    // The path doesn't fully resolve (write/edit creating a new file, or a
    // component is missing). Walk up to the deepest *existing* component —
    // detected with `symlink_metadata` (lstat), NOT `exists()`. `exists()`
    // follows symlinks and so returns `false` for a DANGLING symlink, which
    // would let this loop treat the link's own name as a brand-new file
    // inside root and hand it back — the OS then follows the link on write
    // and escapes the workspace. lstat stops at the link itself so we can
    // resolve or reject it.
    let mut existing = joined.clone();
    let mut tail: Vec<std::path::PathBuf> = Vec::new();
    loop {
        match existing.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => {
                // A symlink is the deepest existing component. Resolve its
                // target: if it dangles or points outside root, reject; if
                // it stays inside, continue resolving from the target so any
                // remaining tail is appended to the real in-root location.
                let target = existing.canonicalize().ok()?;
                if !target.starts_with(&canon_root) {
                    return None;
                }
                existing = target;
                break;
            }
            // Ordinary existing dir/file — deepest real ancestor found.
            Ok(_) => break,
            // This component doesn't exist yet: peel it into the tail and
            // keep walking up.
            Err(_) => {
                let parent = existing.parent()?;
                let name = existing.file_name()?;
                tail.push(std::path::PathBuf::from(name));
                existing = parent.to_path_buf();
                if existing == canon_root || existing == std::path::Path::new("/") {
                    break;
                }
            }
        }
    }

    let canon_existing = existing.canonicalize().ok()?;
    let mut full = canon_existing;
    for name in tail.into_iter().rev() {
        full = full.join(name);
    }

    if full.starts_with(&canon_root) {
        Some(full)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_within_root_rejects_dotdot_escape() {
        let dir = std::env::temp_dir();
        // Create a subdir so canonicalize works
        let subdir = dir.join(format!("stella_resolve_test_{}", std::process::id()));
        std::fs::create_dir_all(&subdir).unwrap();

        // A safe path inside the root
        let safe = resolve_within_root(&subdir, "file.txt");
        assert!(safe.is_some());

        // An escape attempt — on Linux /tmp is shallow so ../../etc/passwd
        // resolves outside the root.
        let escaped = resolve_within_root(&subdir, "../../etc/passwd");
        assert!(escaped.is_none(), "path escape should be rejected");

        std::fs::remove_dir_all(&subdir).ok();
    }

    #[test]
    fn resolve_within_root_allows_nested_safe_path() {
        let dir = std::env::temp_dir();
        let subdir = dir.join(format!("stella_resolve_safe_{}", std::process::id()));
        std::fs::create_dir_all(subdir.join("sub/deep")).unwrap();

        let safe = resolve_within_root(&subdir, "sub/deep/file.txt");
        assert!(safe.is_some());

        std::fs::remove_dir_all(&subdir).ok();
    }

    /// A DANGLING symlink inside the root must not be a valid write target:
    /// the OS follows it on write and escapes the workspace. `exists()`
    /// returns false for a dangling link, so the old walk handed back
    /// `root/link` — this is the witness that it no longer does.
    #[cfg(unix)]
    #[test]
    fn resolve_within_root_rejects_write_through_a_dangling_symlink() {
        let dir = std::env::temp_dir();
        let root = dir.join(format!("stella_resolve_dangling_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        // `link` -> a target that does not exist (yet). Writing "link"
        // would create that outside-the-root target via symlink follow.
        let outside = dir.join(format!("stella_resolve_outside_{}", std::process::id()));
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        assert_eq!(
            resolve_within_root(&root, "link"),
            None,
            "writing through a dangling symlink must be rejected"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// A symlink pointing at an existing directory OUTSIDE the root must not
    /// launder a nested path back inside the confinement check.
    #[cfg(unix)]
    #[test]
    fn resolve_within_root_rejects_write_through_a_symlink_to_outside_dir() {
        let dir = std::env::temp_dir();
        let root = dir.join(format!(
            "stella_resolve_outlink_root_{}",
            std::process::id()
        ));
        let outside = dir.join(format!("stella_resolve_outlink_out_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();

        assert_eq!(
            resolve_within_root(&root, "out/evil.txt"),
            None,
            "a nested path through an outward symlink must be rejected"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// An in-root symlink is fine: writes through it stay confined.
    #[cfg(unix)]
    #[test]
    fn resolve_within_root_allows_write_through_an_in_root_symlink() {
        let dir = std::env::temp_dir();
        let root = dir.join(format!("stella_resolve_inlink_{}", std::process::id()));
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();

        let resolved =
            resolve_within_root(&root, "alias/new.txt").expect("in-root symlink write allowed");
        assert!(resolved.starts_with(root.canonicalize().unwrap()));

        std::fs::remove_dir_all(&root).ok();
    }
}
