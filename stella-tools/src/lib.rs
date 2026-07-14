//! `stella-tools` — the built-in tool set the agent loop calls.
//!
//! Every tool implements [`Tool`], takes a JSON input from the model, and
//! returns a [`ToolOutput`] (success or a typed, named error — never a bare
//! string). Tools are workspace-root-pinned: file paths resolve against the
//! configured root, and `cd` inside `bash` cannot silently diverge the root
//! for subsequent file operations (L-S2).

pub mod bash;
pub mod ci;
pub mod custom;
pub mod delete;
pub mod edit;
pub mod exec;
pub mod exploration;
pub mod glob;
pub mod grep;
pub mod issues;
pub mod memory;
pub mod project;
pub mod read;
pub mod registry;
pub mod schema_gate;
pub mod screenshot;
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

    // File doesn't exist yet (write/edit).  Walk up to the deepest existing
    // ancestor, canonicalise it, then re-append the non-existent tail.
    // This resolves any `..` components in the existing portion of the path.
    let mut existing = joined.clone();
    let mut tail: Vec<std::path::PathBuf> = Vec::new();
    while !existing.exists() {
        let parent = existing.parent()?;
        let name = existing.file_name()?;
        tail.push(std::path::PathBuf::from(name));
        existing = parent.to_path_buf();
        if existing == canon_root || existing == std::path::Path::new("/") {
            break;
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
}
