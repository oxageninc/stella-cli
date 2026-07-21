//! Secure context-store path resolution and its visible degraded-mode policy.

use std::path::{Path, PathBuf};

pub(super) fn resolve_context_db_path(
    workspace_root: &Path,
    warn: bool,
    mut report: impl FnMut(String),
) -> Option<PathBuf> {
    match stella_store::workspace_private_sqlite_path(workspace_root, "context.db") {
        Ok(path) => Some(path),
        Err(error) => {
            if warn {
                report(format!(
                    "memory disabled this session: cannot resolve private context state: {error}"
                ));
            }
            None
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn secure_path_failure_warns_when_requested_instead_of_disappearing() {
        let dir = tempfile::tempdir().unwrap();
        let dot = dir.path().join(".stella");
        std::fs::create_dir_all(&dot).unwrap();
        std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::write(dot.join("context.db"), b"unsafe legacy context").unwrap();

        let mut warnings = Vec::new();
        assert!(resolve_context_db_path(dir.path(), true, |w| warnings.push(w)).is_none());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("legacy") && warnings[0].contains("private"));

        warnings.clear();
        assert!(resolve_context_db_path(dir.path(), false, |w| warnings.push(w)).is_none());
        assert!(warnings.is_empty(), "warn=false stays intentionally quiet");
    }
}
