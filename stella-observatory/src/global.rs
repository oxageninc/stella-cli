//! The user-tier view: every stella project on this machine, read from the
//! cross-project aggregate `usage.db` that `stella-store` keeps under the OS
//! data dir. This is what lets the dashboard's project switcher drill into
//! any workspace stella has ever touched — each request may carry
//! `?project=<id>`, resolved here to that project's root so the per-workspace
//! queries in [`crate::db`] run against *its* `.stella/private/store.db` instead.
//!
//! Same posture as everything else in the observatory: read-only, and a
//! missing `usage.db` is a state (empty project list), never an error.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

/// The user-tier stella data dir. Mirrors `stella_store::usage::data_dir` —
/// kept as a copy because the observatory deliberately does not link
/// `stella-store` (whose `Store::open` runs migrations, i.e. writes).
/// `STELLA_DATA_DIR` overrides, then `STELLA_HOME`; otherwise `~/.stella`
/// on every platform (`USERPROFILE` stands in for `HOME` on Windows).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("STELLA_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("STELLA_HOME") {
        return PathBuf::from(dir);
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(".stella");
    }
    PathBuf::from(".")
}

/// `data_dir()/usage.db` — the cross-project rollup.
pub fn usage_db_path() -> PathBuf {
    data_dir().join("usage.db")
}

/// FNV-1a/64 of the canonical workspace path — the same stable project id
/// `stella_store::usage::project_id_for` computes, so the dashboard's notion
/// of "current project" lines up with the rollup's rows.
pub fn project_id_for(root: &Path) -> String {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let s = canon.to_string_lossy();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn open_usage() -> Option<Connection> {
    let path = usage_db_path();
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

/// Every project `usage.db` knows about, with its rollup headline numbers.
/// The serving workspace is flagged `is_current` and listed first;
/// `has_store` says whether drilling in will find a live `.stella/private/store.db`.
pub fn projects(current_root: &Path) -> Value {
    let current_id = project_id_for(current_root);
    let Some(conn) = open_usage() else {
        return json!({
            "available": false,
            "current": current_id,
            "projects": [],
        });
    };
    let mut rows: Vec<Value> = Vec::new();
    let query = conn.prepare(
        "SELECT p.project_id, p.name, p.root_path, p.last_seen_at,
                coalesce(r.runs, 0), coalesce(r.cost, 0),
                coalesce(r.input_tokens, 0), coalesce(r.output_tokens, 0)
         FROM projects p
         LEFT JOIN (SELECT project_id, count(*) AS runs,
                           sum(cost_usd) AS cost,
                           sum(input_tokens) AS input_tokens,
                           sum(output_tokens) AS output_tokens
                    FROM execution_rollup GROUP BY project_id) r
           ON r.project_id = p.project_id
         ORDER BY p.last_seen_at DESC",
    );
    if let Ok(mut stmt) = query {
        let mapped = stmt.query_map([], |r| {
            let project_id: String = r.get(0)?;
            let root_path: String = r.get(2)?;
            let has_store = Path::new(&root_path)
                .join(".stella/private/store.db")
                .exists();
            Ok(json!({
                "project_id": project_id.clone(),
                "name": r.get::<_, String>(1)?,
                "root_path": root_path,
                "last_seen_at": r.get::<_, String>(3)?,
                "runs": r.get::<_, i64>(4)?,
                "cost_usd": r.get::<_, f64>(5)?,
                "input_tokens": r.get::<_, i64>(6)?,
                "output_tokens": r.get::<_, i64>(7)?,
                "has_store": has_store,
                "is_current": project_id == current_id,
            }))
        });
        if let Ok(iter) = mapped {
            rows = iter.flatten().collect();
        }
    }
    // Current project first, then most recently seen.
    rows.sort_by_key(|p| !p["is_current"].as_bool().unwrap_or(false) as u8);
    json!({
        "available": true,
        "current": current_id,
        "projects": rows,
    })
}

/// Resolve a `?project=<id>` to that project's workspace root, but only to a
/// path the rollup itself registered (never a caller-supplied path) and only
/// while the directory still exists.
pub fn resolve_project_root(project_id: &str) -> Option<PathBuf> {
    let conn = open_usage()?;
    let root: String = conn
        .query_row(
            "SELECT root_path FROM projects WHERE project_id = ?1",
            [project_id],
            |r| r.get(0),
        )
        .ok()?;
    let path = PathBuf::from(root);
    path.is_dir().then_some(path)
}
