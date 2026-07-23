//! Telemetry scoping identity: `org_id` / `workspace_id` / `repo_id`, and
//! the stub cloud registration they hang off.
//!
//! Every row replicated into the user-tier hub (and from there to a cloud
//! org) carries:
//!
//! - **`org_id`** — the enterprise organization this installation reports
//!   into. `NULL` until the user registers: `STELLA_ORG_ID` wins, else the
//!   `org_id` in `~/.stella/cloud.json` (written by `stella cloud register`
//!   — today a stub; an OAuth login will attach a token to the same file),
//!   else `None`.
//! - **`workspace_id`** — a durable UUID at `<root>/.stella/workspace.json`,
//!   written only by registration ([`register_workspace`]), never minted
//!   implicitly — an unregistered workspace reports `NULL`. It lives
//!   *outside* `.stella/private/`, so committing it shares one workspace
//!   identity across clones and machines.
//! - **`repo_id`** — the workspace is only *sorta* a repo: usually 1:1, not
//!   always. `repo_id` hashes the workspace's normalized `origin` remote URL
//!   (SSH and HTTPS forms of the same repo agree), falling back to the
//!   canonical-path hash when there is no git remote. Rows carry it
//!   separately so a multi-repo workspace still reports per-repo.
//!
//! The machine-local `project_id` (canonical-path hash) remains the
//! replication key — always present, registration or not. Resolution is
//! infallible by design: telemetry scoping must never fail a turn.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::usage::project_id_for;
use crate::{Result, StoreError};

/// The identity stamped onto every replicated telemetry row. `org_id` and
/// `workspace_id` are `None` until this installation / workspace is
/// registered to a cloud account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryScope {
    pub org_id: Option<String>,
    pub workspace_id: Option<String>,
    pub repo_id: String,
    pub project_id: String,
}

impl TelemetryScope {
    /// Resolve the full scope for a workspace. Never fails: unregistered
    /// ids resolve to `None`, and a missing git remote degrades `repo_id`
    /// to the path-derived project id.
    pub fn resolve(workspace_root: &Path) -> Self {
        let project_id = project_id_for(workspace_root);
        Self {
            org_id: org_id(),
            workspace_id: workspace_id(workspace_root),
            repo_id: repo_id_for(workspace_root).unwrap_or_else(|| project_id.clone()),
            project_id,
        }
    }
}

/// `~/.stella/cloud.json` — the stub cloud-account registration. The
/// `oauth_token` slot is reserved: the future login flow stores its
/// credential here so registration and auth stay one file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudRegistration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_token: Option<String>,
}

fn cloud_json_path() -> PathBuf {
    crate::usage::data_dir().join("cloud.json")
}

/// Load the registration, `Default` (all `None`) when absent or unreadable.
pub fn cloud_registration() -> CloudRegistration {
    std::fs::read_to_string(cloud_json_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persist the registration (owner-only, atomic enough for a single-user
/// file: write then rename).
pub fn save_cloud_registration(reg: &CloudRegistration) -> Result<()> {
    let path = cloud_json_path();
    if let Some(parent) = path.parent() {
        crate::ensure_private_dir(parent)?;
    }
    let body = serde_json::to_string_pretty(reg)
        .map_err(|e| StoreError(format!("cannot render cloud registration: {e}")))?
        + "\n";
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| StoreError(format!("cannot write cloud registration: {e}")))
}

/// The org this installation reports into, `None` until registered.
/// `STELLA_ORG_ID` overrides the stored registration.
pub fn org_id() -> Option<String> {
    if let Some(id) = std::env::var_os("STELLA_ORG_ID") {
        let id = id.to_string_lossy().trim().to_string();
        return (!id.is_empty()).then_some(id);
    }
    cloud_registration()
        .org_id
        .filter(|id| !id.trim().is_empty())
}

/// The registered workspace UUID from `<root>/.stella/workspace.json`,
/// `None` while unregistered. Never mints — registration is explicit.
pub fn workspace_id(workspace_root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(workspace_root.join(".stella/workspace.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    json.get("workspace_id")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Register this workspace: persist the given id (or mint a UUID) at
/// `<root>/.stella/workspace.json`. Idempotent — an already-registered
/// workspace keeps its id (one workspace must never fork into two
/// identities); pass through the existing id in that case.
pub fn register_workspace(workspace_root: &Path, id: Option<&str>) -> Result<String> {
    if let Some(existing) = workspace_id(workspace_root) {
        return Ok(existing);
    }
    let minted = match id {
        Some(given) if !given.trim().is_empty() => given.trim().to_string(),
        _ => crate::enterprise_telemetry::random_uuid_v4(),
    };
    let dot = workspace_root.join(".stella");
    std::fs::create_dir_all(&dot).map_err(|e| StoreError(format!("cannot create .stella: {e}")))?;
    let body = serde_json::json!({ "workspace_id": minted }).to_string() + "\n";
    // create_new: if a sibling process registered first, its id wins.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dot.join("workspace.json"))
    {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(body.as_bytes())
                .map_err(|e| StoreError(format!("cannot write workspace identity: {e}")))?;
            Ok(minted)
        }
        Err(_) => workspace_id(workspace_root)
            .ok_or_else(|| StoreError("workspace identity is unreadable".into())),
    }
}

/// Hash of the workspace's normalized `origin` remote URL, so every clone of
/// a repo reports under one `repo_id` regardless of transport or machine.
fn repo_id_for(workspace_root: &Path) -> Option<String> {
    let config = git_config_path(workspace_root)?;
    let raw = std::fs::read_to_string(config).ok()?;
    let url = origin_url(&raw)?;
    Some(crate::fnv_hex(&normalize_remote_url(&url)))
}

/// `<root>/.git/config`, following a worktree's `gitdir:` file (and its
/// `commondir`) to the main repository's config.
fn git_config_path(workspace_root: &Path) -> Option<PathBuf> {
    let dot_git = workspace_root.join(".git");
    if dot_git.is_dir() {
        let config = dot_git.join("config");
        return config.is_file().then_some(config);
    }
    // A worktree: `.git` is a file "gitdir: <path>".
    let raw = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = raw.strip_prefix("gitdir:")?.trim();
    let mut gitdir = PathBuf::from(gitdir);
    if gitdir.is_relative() {
        gitdir = workspace_root.join(gitdir);
    }
    // `commondir` (usually "../..") points from the worktree's gitdir back
    // to the shared .git dir that owns `config`.
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(rel) => {
            let rel = rel.trim();
            let common = PathBuf::from(rel);
            if common.is_relative() {
                gitdir.join(common)
            } else {
                common
            }
        }
        Err(_) => gitdir,
    };
    let config = common.join("config");
    config.is_file().then_some(config)
}

/// Extract `url` from the `[remote "origin"]` section of a git config.
fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.starts_with("[remote") && line.contains("\"origin\"");
            continue;
        }
        if in_origin
            && let Some(rest) = line.strip_prefix("url")
            && let Some(url) = rest.trim_start().strip_prefix('=')
        {
            return Some(url.trim().to_string());
        }
    }
    None
}

/// Reduce every spelling of a remote to one canonical `host/path` form:
/// scheme and `git@` stripped, the scp-style `host:path` colon folded to
/// `/`, a trailing `/` or `.git` dropped, lowercased.
fn normalize_remote_url(url: &str) -> String {
    let mut s = url.trim().to_string();
    for scheme in ["ssh://", "git://", "https://", "http://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.replacen(':', "/", 1);
    }
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    s.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_and_https_remotes_normalize_identically() {
        assert_eq!(
            normalize_remote_url("git@github.com:MacAnderson/Stella.git"),
            normalize_remote_url("https://github.com/macanderson/stella"),
        );
        assert_eq!(
            normalize_remote_url("ssh://git@github.com/org/repo.git"),
            "github.com/org/repo"
        );
    }

    #[test]
    fn origin_url_reads_only_the_origin_section() {
        let config = "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = https://example.com/up.git\n[remote \"origin\"]\n\turl = git@github.com:org/repo.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n";
        assert_eq!(
            origin_url(config).as_deref(),
            Some("git@github.com:org/repo.git")
        );
        assert_eq!(origin_url("[core]\n\tbare = false\n"), None);
    }

    #[test]
    fn unregistered_workspace_scopes_to_null_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = TelemetryScope::resolve(tmp.path());
        assert_eq!(scope.workspace_id, None, "no implicit workspace minting");
        assert_eq!(scope.project_id, project_id_for(tmp.path()));
        assert_eq!(
            scope.repo_id, scope.project_id,
            "no git remote → path-derived repo id"
        );
        assert!(
            !tmp.path().join(".stella").exists(),
            "resolving telemetry scope writes nothing"
        );
    }

    #[test]
    fn register_workspace_mints_once_then_sticks() {
        let tmp = tempfile::tempdir().unwrap();
        let first = register_workspace(tmp.path(), None).unwrap();
        let second = register_workspace(tmp.path(), Some("other-id")).unwrap();
        assert_eq!(first, second, "an existing registration is never forked");
        assert_eq!(workspace_id(tmp.path()).as_deref(), Some(first.as_str()));
        assert!(
            tmp.path().join(".stella/workspace.json").is_file(),
            "persisted outside .stella/private so it is committable"
        );
        let explicit = tempfile::tempdir().unwrap();
        let given = register_workspace(explicit.path(), Some("ws-from-cloud")).unwrap();
        assert_eq!(given, "ws-from-cloud", "a cloud-assigned id is honored");
    }

    #[test]
    fn repo_id_reads_the_origin_remote_from_git_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join(".git/config"),
            "[remote \"origin\"]\n\turl = git@github.com:org/repo.git\n",
        )
        .unwrap();
        let a = repo_id_for(tmp.path()).unwrap();

        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp2.path().join(".git")).unwrap();
        std::fs::write(
            tmp2.path().join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/org/repo\n",
        )
        .unwrap();
        assert_eq!(
            a,
            repo_id_for(tmp2.path()).unwrap(),
            "two clones, two transports, one repo_id"
        );
    }
}
