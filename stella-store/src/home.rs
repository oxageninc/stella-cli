//! `~/.stella` — the single user-level stella home, on every platform.
//!
//! Everything user-global (the `usage.db` telemetry hub, `catalog.db`, the
//! session registry, notifications, the enterprise spool, settings, skills,
//! agents, rules, tools, credentials) lives directly under `~/.stella`, the
//! same way Claude Code uses `~/.claude`. No platform data-dir guessing:
//! macOS, Linux, and Windows all resolve to the home directory.
//!
//! Overrides: `STELLA_HOME` moves the whole home; the narrower
//! `STELLA_DATA_DIR` / `STELLA_CONFIG_DIR` overrides still win where they
//! always did (tests, sandboxes, org-managed layouts).
//!
//! [`migrate_legacy_global_dirs`] performs the one-time move from the old
//! split layout (platform data dir + `~/.config/stella`) into `~/.stella`.
//! It is best-effort, idempotent, per-entry (an entry that already exists at
//! the new home is never overwritten), and skipped entirely when any
//! override env var points resolution away from the defaults.

use std::path::PathBuf;
use std::sync::Once;

/// The user's home directory: `HOME`, else `USERPROFILE` (Windows).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The stella home: `$STELLA_HOME`, else `~/.stella`. `None` only when no
/// home directory is discoverable at all.
pub fn stella_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("STELLA_HOME") {
        return Some(PathBuf::from(dir));
    }
    home_dir().map(|home| home.join(".stella"))
}

/// The pre-`~/.stella` platform data dir (macOS Application Support,
/// `%APPDATA%`, XDG, `~/.local/share`) — where usage.db, catalog.db, the
/// session registry, notifications, and the enterprise spool used to live.
fn legacy_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home).join("Library/Application Support/stella"));
    }
    #[cfg(target_os = "windows")]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(appdata).join("stella"));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg).join("stella"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/stella"))
}

/// The pre-`~/.stella` user config dir (`~/.config/stella`) — settings,
/// skills, agents, rules, tools, credentials.
fn legacy_config_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".config").join("stella"))
}

/// Move every entry of `legacy` that does not already exist under `target`.
/// Prefers `fs::rename` (atomic, same-volume); on a cross-device (`EXDEV`)
/// or other rename failure it falls back to a recursive copy that leaves the
/// legacy original in place — so a failed move never strands the only copy
/// of a user's settings/credentials, and the next run retries. Returns the
/// number of entries it could neither move nor copy, so the caller can
/// surface a partial migration instead of silently reporting an empty home.
fn merge_dir_into(legacy: &std::path::Path, target: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(legacy) else {
        return 0;
    };
    if std::fs::create_dir_all(target).is_err() {
        return 1;
    }
    let mut failures = 0u64;
    for entry in entries.flatten() {
        let dest = target.join(entry.file_name());
        if dest.exists() {
            continue;
        }
        if std::fs::rename(entry.path(), &dest).is_ok() {
            continue;
        }
        // Cross-device or racing rename: copy instead, leaving the legacy
        // original untouched. A failed copy removes its own partial output
        // so the retry starts clean.
        if copy_into(&entry.path(), &dest).is_err() {
            let _ = std::fs::remove_dir_all(&dest);
            let _ = std::fs::remove_file(&dest);
            failures += 1;
        }
    }
    failures
}

/// Recursively copy a file or directory tree `src` → `dest`. Used only as
/// the cross-device fallback in [`merge_dir_into`].
fn copy_into(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(src)?.flatten() {
            copy_into(&entry.path(), &dest.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dest).map(|_| ())
    }
}

/// One-time (per process), best-effort migration of the legacy split layout
/// into `~/.stella`. A no-op when `STELLA_HOME`, `STELLA_DATA_DIR`, or
/// `STELLA_CONFIG_DIR` is set (resolution isn't pointing at the defaults, so
/// moving real user data would be wrong), or when there is nothing to move.
pub fn migrate_legacy_global_dirs() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for var in ["STELLA_HOME", "STELLA_DATA_DIR", "STELLA_CONFIG_DIR"] {
            if std::env::var_os(var).is_some() {
                return;
            }
        }
        let Some(target) = stella_home() else {
            return;
        };
        let mut failures = 0u64;
        for legacy in [legacy_data_dir(), legacy_config_dir()]
            .into_iter()
            .flatten()
        {
            if legacy != target && legacy.is_dir() {
                failures += merge_dir_into(&legacy, &target);
            }
        }
        if failures > 0 {
            // Never fail a turn, but don't let a partial migration look like
            // a clean empty home — the legacy originals are still in place.
            eprintln!(
                "stella: could not migrate {failures} legacy config/data \
                 entr{} into {} — the originals were left untouched; rerun to retry",
                if failures == 1 { "y" } else { "ies" },
                target.display()
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_moves_only_missing_entries_and_keeps_legacy_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy");
        let target = tmp.path().join("home/.stella");
        std::fs::create_dir_all(legacy.join("sessions")).unwrap();
        std::fs::write(legacy.join("usage.db"), b"legacy-usage").unwrap();
        std::fs::write(legacy.join("installation-id"), b"legacy-id").unwrap();
        std::fs::write(legacy.join("sessions/one.json"), b"{}").unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("installation-id"), b"new-id").unwrap();

        assert_eq!(
            merge_dir_into(&legacy, &target),
            0,
            "clean move, no failures"
        );

        assert_eq!(
            std::fs::read(target.join("usage.db")).unwrap(),
            b"legacy-usage",
            "missing entries move over"
        );
        assert_eq!(
            std::fs::read(target.join("sessions/one.json")).unwrap(),
            b"{}",
            "directories move wholesale"
        );
        assert_eq!(
            std::fs::read(target.join("installation-id")).unwrap(),
            b"new-id",
            "existing entries are never overwritten"
        );
        assert!(
            std::fs::read(legacy.join("installation-id")).unwrap() == b"legacy-id",
            "skipped entries stay in the legacy dir"
        );
        assert!(legacy.exists(), "legacy dir itself is preserved");

        // Idempotent: a second merge changes nothing.
        merge_dir_into(&legacy, &target);
        assert_eq!(
            std::fs::read(target.join("installation-id")).unwrap(),
            b"new-id"
        );
    }

    #[test]
    fn merge_with_missing_legacy_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".stella");
        assert_eq!(
            merge_dir_into(&tmp.path().join("does-not-exist"), &target),
            0
        );
        assert!(!target.exists(), "no legacy → nothing created");
    }

    #[test]
    fn copy_into_clones_a_nested_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("skills/a")).unwrap();
        std::fs::write(src.join("settings.json"), b"{}").unwrap();
        std::fs::write(src.join("skills/a/SKILL.md"), b"# a").unwrap();
        let dest = tmp.path().join("dest");

        copy_into(&src, &dest).unwrap();

        assert_eq!(std::fs::read(dest.join("settings.json")).unwrap(), b"{}");
        assert_eq!(
            std::fs::read(dest.join("skills/a/SKILL.md")).unwrap(),
            b"# a"
        );
        assert!(src.join("settings.json").exists(), "source is left intact");
    }

    #[test]
    fn stella_home_honors_the_override() {
        // SAFETY: single-threaded test; set and read one process env var.
        unsafe {
            std::env::set_var("STELLA_HOME", "/tmp/stella-home-test");
        }
        assert_eq!(stella_home(), Some(PathBuf::from("/tmp/stella-home-test")));
        unsafe {
            std::env::remove_var("STELLA_HOME");
        }
    }
}
