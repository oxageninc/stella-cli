//! The shared staleness oracle (`docs/design/exploration-sharing.md` §3b).
//!
//! One answer to "is this saved artifact still true of the code?", shared by
//! every context-caching surface: `gather_context` packs and exploration
//! maps today, anything added later (§11's audit rule). The oracle is a
//! per-file manifest of `workspace-relative path → sha256(content)` captured
//! at save time and re-checked at read time, so validity is decided *per
//! file* — a commit elsewhere in the repo degrades nothing, and a real
//! drift names exactly which files moved instead of poisoning the whole
//! artifact.
//!
//! Git hashes are deliberately NOT the oracle: content hashing works with a
//! dirty working tree and with files git never sees. `git rev-parse HEAD`
//! is kept as provenance and as a free short-circuit — when HEAD matches
//! the saved head and the working tree is clean, nothing can have moved and
//! no file needs re-hashing.

use std::collections::BTreeMap;
use std::path::Path;

use sha2::{Digest, Sha256};

/// Hex-encoded SHA-256 of raw bytes — the manifest entry format.
pub fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Hash every extant path into a manifest, silently skipping paths that do
/// not exist or cannot be read (they were never evidence). Order and dedup
/// come from the `BTreeMap`.
pub async fn build_manifest(
    root: &Path,
    paths: impl IntoIterator<Item = String>,
) -> BTreeMap<String, String> {
    let mut manifest = BTreeMap::new();
    for path in paths {
        if let Ok(bytes) = tokio::fs::read(root.join(&path)).await {
            manifest.insert(path, hex_sha256(&bytes));
        }
    }
    manifest
}

/// The per-file validity verdict for a manifest-carrying artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// Every manifest entry re-hashes to its saved value — the artifact is
    /// exactly as true as the day it was saved, whatever HEAD did meanwhile.
    Fresh,
    /// Some covered files moved or vanished. Degraded, not dead: everything
    /// outside `changed`/`missing` is still current.
    Drifted {
        changed: Vec<String>,
        missing: Vec<String>,
    },
    /// No manifest to check (a pre-manifest record). The only signal left is
    /// whether HEAD moved since save — the old coarse compare.
    Unknown { head_moved: bool },
}

impl Freshness {
    pub fn is_fresh(&self) -> bool {
        matches!(self, Freshness::Fresh)
    }

    /// One parenthetical for listings: `(fresh)`, `(drifted: 2/9 files
    /// changed)`, `(unverified — saved before manifests)`.
    pub fn label(&self, manifest_len: usize) -> String {
        match self {
            Freshness::Fresh => "fresh".to_string(),
            Freshness::Drifted { changed, missing } => {
                let moved = changed.len() + missing.len();
                format!("drifted: {moved}/{manifest_len} files changed")
            }
            Freshness::Unknown { head_moved: false } => "likely fresh (same commit)".to_string(),
            Freshness::Unknown { head_moved: true } => {
                "unverified — saved before manifests, repo has moved".to_string()
            }
        }
    }
}

/// Compute the verdict for a saved artifact.
///
/// Rules, in order (§3b):
/// 1. Empty manifest → [`Freshness::Unknown`] from the head compare alone.
/// 2. Saved head == current head AND clean working tree → [`Freshness::Fresh`]
///    without touching a single file.
/// 3. Otherwise re-hash every entry and report exact drift.
pub async fn freshness(
    root: &Path,
    manifest: &BTreeMap<String, String>,
    saved_head: Option<&str>,
    current_head: Option<&str>,
) -> Freshness {
    if manifest.is_empty() {
        let head_moved = match (saved_head, current_head) {
            (Some(saved), Some(current)) => saved != current,
            // No basis for comparison — treat as moved so the caller renders
            // the cautious wording rather than false confidence.
            _ => true,
        };
        return Freshness::Unknown { head_moved };
    }
    if let (Some(saved), Some(current)) = (saved_head, current_head)
        && saved == current
        && worktree_clean(root).await
    {
        return Freshness::Fresh;
    }
    let (mut changed, mut missing) = (Vec::new(), Vec::new());
    for (path, saved_hash) in manifest {
        match tokio::fs::read(root.join(path)).await {
            Ok(bytes) if &hex_sha256(&bytes) == saved_hash => {}
            Ok(_) => changed.push(path.clone()),
            Err(_) => missing.push(path.clone()),
        }
    }
    if changed.is_empty() && missing.is_empty() {
        Freshness::Fresh
    } else {
        Freshness::Drifted { changed, missing }
    }
}

/// Synchronous [`freshness`] for callers outside a runtime (registry
/// construction, system-prompt assembly). Identical rules; `std::fs` reads.
pub fn freshness_sync(
    root: &Path,
    manifest: &BTreeMap<String, String>,
    saved_head: Option<&str>,
    current_head: Option<&str>,
) -> Freshness {
    if manifest.is_empty() {
        let head_moved = match (saved_head, current_head) {
            (Some(saved), Some(current)) => saved != current,
            _ => true,
        };
        return Freshness::Unknown { head_moved };
    }
    if let (Some(saved), Some(current)) = (saved_head, current_head)
        && saved == current
        && worktree_clean_sync(root)
    {
        return Freshness::Fresh;
    }
    let (mut changed, mut missing) = (Vec::new(), Vec::new());
    for (path, saved_hash) in manifest {
        match std::fs::read(root.join(path)) {
            Ok(bytes) if &hex_sha256(&bytes) == saved_hash => {}
            Ok(_) => changed.push(path.clone()),
            Err(_) => missing.push(path.clone()),
        }
    }
    if changed.is_empty() && missing.is_empty() {
        Freshness::Fresh
    } else {
        Freshness::Drifted { changed, missing }
    }
}

/// Synchronous [`git_head`], same `GIT_*` hygiene.
pub fn git_head_sync(root: &Path) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["rev-parse", "HEAD"]).current_dir(root);
    for var in crate::exec::GIT_REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if head.is_empty() { None } else { Some(head) }
}

fn worktree_clean_sync(root: &Path) -> bool {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["status", "--porcelain"]).current_dir(root);
    for var in crate::exec::GIT_REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    match cmd.output() {
        Ok(out) => out.status.success() && out.stdout.iter().all(|b| b.is_ascii_whitespace()),
        Err(_) => false,
    }
}

/// `git rev-parse HEAD`, hardened against hook-exported `GIT_*` vars
/// re-targeting the query at another repo (same hygiene as
/// [`crate::exploration`], now shared).
pub async fn git_head(root: &Path) -> Option<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["rev-parse", "HEAD"]).current_dir(root);
    for var in crate::exec::GIT_REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    let output = cmd.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if head.is_empty() { None } else { Some(head) }
}

/// True iff `git status --porcelain` reports nothing — the precondition for
/// the head-match short-circuit. Any failure to ask counts as dirty.
async fn worktree_clean(root: &Path) -> bool {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["status", "--porcelain"]).current_dir(root);
    for var in crate::exec::GIT_REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    match cmd.output().await {
        Ok(out) => out.status.success() && out.stdout.iter().all(|b| b.is_ascii_whitespace()),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stella_staleness_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_is_hex_of_known_vector() {
        // sha256("") — the canonical empty-input vector.
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn manifest_skips_missing_and_hashes_present() {
        let root = temp_root("manifest");
        std::fs::write(root.join("a.txt"), "alpha").unwrap();
        let manifest = build_manifest(&root, ["a.txt".to_string(), "ghost.txt".to_string()]).await;
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest["a.txt"], hex_sha256(b"alpha"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn verdicts_fresh_drifted_missing() {
        let root = temp_root("verdicts");
        std::fs::write(root.join("a.txt"), "alpha").unwrap();
        std::fs::write(root.join("b.txt"), "beta").unwrap();
        let manifest = build_manifest(&root, ["a.txt".to_string(), "b.txt".to_string()]).await;

        // Untouched → Fresh (no git in this temp dir, so heads are None and
        // the hash path decides).
        assert_eq!(
            freshness(&root, &manifest, None, None).await,
            Freshness::Fresh
        );

        // Change one, delete the other → exact drift lists.
        std::fs::write(root.join("a.txt"), "alpha2").unwrap();
        std::fs::remove_file(root.join("b.txt")).unwrap();
        match freshness(&root, &manifest, None, None).await {
            Freshness::Drifted { changed, missing } => {
                assert_eq!(changed, vec!["a.txt".to_string()]);
                assert_eq!(missing, vec!["b.txt".to_string()]);
            }
            other => panic!("expected Drifted, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn empty_manifest_falls_back_to_head_compare() {
        let root = temp_root("v1");
        let empty = BTreeMap::new();
        assert_eq!(
            freshness(&root, &empty, Some("abc"), Some("abc")).await,
            Freshness::Unknown { head_moved: false }
        );
        assert_eq!(
            freshness(&root, &empty, Some("abc"), Some("def")).await,
            Freshness::Unknown { head_moved: true }
        );
        assert_eq!(
            freshness(&root, &empty, None, Some("def")).await,
            Freshness::Unknown { head_moved: true }
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn labels_read_as_designed() {
        assert_eq!(Freshness::Fresh.label(9), "fresh");
        let drifted = Freshness::Drifted {
            changed: vec!["a".into()],
            missing: vec!["b".into()],
        };
        assert_eq!(drifted.label(9), "drifted: 2/9 files changed");
    }
}
