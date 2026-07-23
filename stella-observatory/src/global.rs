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

use rusqlite::{Connection, OpenFlags, ToSql};
use serde_json::{Value, json};

/// The scope value the dashboard passes to select the null bucket: a developer
/// who has not cloud-registered has `NULL` `org_id`/`workspace_id`, which no
/// `= ?` filter matches — the drill hands back `(local)` and we translate it to
/// `IS NULL` (for `repo_id`, whose column is `NOT NULL DEFAULT ''`, to `''`).
const LOCAL_SCOPE: &str = "(local)";

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

/// The hub `telemetry` table (the cross-project replica #398 keeps in
/// `usage.db`, scoped by `org_id`/`workspace_id`/`repo_id`) aggregated for the
/// org-telemetry dashboard: totals, a drill that steps org → workspace → repo →
/// project as filters narrow, per-provider/model rows (mirroring
/// `stella usage report`, plus a cache-read rate), per-repo rows, and a daily
/// activity series.
///
/// Hub-only and read-only: it works while project stores are locked by live
/// sessions, and a missing `usage.db` — or one predating #398, with no
/// `telemetry` table — is an empty state, never an error.
///
/// `org`/`workspace`/`repo` narrow the scope and pick the drill level; the
/// literal [`LOCAL_SCOPE`] selects the null bucket.
pub fn hub_telemetry(
    current_root: &Path,
    org: Option<&str>,
    workspace: Option<&str>,
    repo: Option<&str>,
) -> Value {
    let scope = json!({ "org": org, "workspace": workspace, "repo": repo });
    let level = drill_level(org, workspace, repo);
    let Some(conn) = open_usage() else {
        return json!({
            "available": false,
            "current": project_id_for(current_root),
            "scope": scope,
            "totals": telemetry_totals_zero(),
            "drill": { "level": level, "rows": [] },
            "by_provider_model": [],
            "by_repo": [],
            "by_day": [],
        });
    };

    let filter = build_scope_filter(org, workspace, repo);
    let bind: Vec<(&str, &dyn ToSql)> = filter
        .params
        .iter()
        .map(|(k, v)| (*k, v as &dyn ToSql))
        .collect();
    let where_sql = filter.where_sql.as_str();

    json!({
        "available": true,
        "current": project_id_for(current_root),
        "scope": scope,
        "totals": hub_totals(&conn, where_sql, &bind),
        "drill": { "level": level, "rows": hub_drill(&conn, level, where_sql, &bind) },
        "by_provider_model": hub_by_provider_model(&conn, where_sql, &bind),
        "by_repo": hub_by_repo(&conn, where_sql, &bind),
        "by_day": hub_by_day(&conn, where_sql, &bind),
    })
}

/// The dimension the drill breaks down for the current scope: the deepest set
/// filter selects the next level in.
fn drill_level(org: Option<&str>, workspace: Option<&str>, repo: Option<&str>) -> &'static str {
    if repo.is_some() {
        "project"
    } else if workspace.is_some() {
        "repo"
    } else if org.is_some() {
        "workspace"
    } else {
        "org"
    }
}

struct ScopeFilter {
    /// `" WHERE …"` (leading space) or empty.
    where_sql: String,
    params: Vec<(&'static str, String)>,
}

/// Translate the scope selectors into a `WHERE` clause. Nullable scope columns
/// take `IS NULL` for the [`LOCAL_SCOPE`] bucket (changing SQL text, not just a
/// bound value); `repo_id`'s null bucket is the empty string.
fn build_scope_filter(
    org: Option<&str>,
    workspace: Option<&str>,
    repo: Option<&str>,
) -> ScopeFilter {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<(&'static str, String)> = Vec::new();
    let mut nullable = |col: &str, name: &'static str, val: Option<&str>| match val {
        None => {}
        Some(LOCAL_SCOPE) => clauses.push(format!("{col} IS NULL")),
        Some(v) => {
            clauses.push(format!("{col} = {name}"));
            params.push((name, v.to_string()));
        }
    };
    nullable("org_id", ":org", org);
    nullable("workspace_id", ":workspace", workspace);
    if let Some(v) = repo {
        clauses.push("repo_id = :repo".to_string());
        params.push((
            ":repo",
            if v == LOCAL_SCOPE {
                String::new()
            } else {
                v.to_string()
            },
        ));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    ScopeFilter { where_sql, params }
}

/// Fraction of read+miss cache-eligible input tokens served from cache, guarded
/// against the no-cache-activity case.
fn cache_read_rate(read: i64, miss: i64) -> f64 {
    let total = read + miss;
    if total <= 0 {
        0.0
    } else {
        read as f64 / total as f64
    }
}

/// Run a read-only aggregate, tolerating a `usage.db` with no `telemetry` table
/// (`prepare` fails → empty), matching the observatory's never-an-error posture.
fn query_rows<F>(conn: &Connection, sql: &str, params: &[(&str, &dyn ToSql)], map: F) -> Vec<Value>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<Value>,
{
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let Ok(iter) = stmt.query_map(params, map) else {
        return Vec::new();
    };
    iter.flatten().collect()
}

fn telemetry_totals_zero() -> Value {
    json!({
        "calls": 0,
        "input_tokens": 0,
        "output_tokens": 0,
        "cache_read_tokens": 0,
        "cache_read_rate": 0.0,
        "cost_usd": 0.0,
        "projects": 0,
        "workspaces": 0,
        "orgs": 0,
    })
}

fn hub_totals(conn: &Connection, where_sql: &str, params: &[(&str, &dyn ToSql)]) -> Value {
    let sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0), \
                COALESCE(SUM(cache_read_tokens), 0), COALESCE(SUM(cache_miss_tokens), 0), \
                COALESCE(SUM(cost_usd), 0), COUNT(DISTINCT project_id), \
                COUNT(DISTINCT workspace_id), COUNT(DISTINCT org_id) \
         FROM telemetry{where_sql}"
    );
    query_rows(conn, &sql, params, |r| {
        let cache_read: i64 = r.get(3)?;
        let cache_miss: i64 = r.get(4)?;
        Ok(json!({
            "calls": r.get::<_, i64>(0)?,
            "input_tokens": r.get::<_, i64>(1)?,
            "output_tokens": r.get::<_, i64>(2)?,
            "cache_read_tokens": cache_read,
            "cache_read_rate": cache_read_rate(cache_read, cache_miss),
            "cost_usd": r.get::<_, f64>(5)?,
            "projects": r.get::<_, i64>(6)?,
            "workspaces": r.get::<_, i64>(7)?,
            "orgs": r.get::<_, i64>(8)?,
        }))
    })
    .into_iter()
    .next()
    .unwrap_or_else(telemetry_totals_zero)
}

fn hub_drill(
    conn: &Connection,
    level: &str,
    where_sql: &str,
    params: &[(&str, &dyn ToSql)],
) -> Vec<Value> {
    // The project leaf joins `projects` for a human name; the other levels group
    // a single (nullable) scope column. `projects` carries none of the scope
    // columns, so the unqualified `WHERE` stays unambiguous under the join.
    if level == "project" {
        let sql = format!(
            "SELECT t.project_id, COALESCE(p.name, ''), COUNT(*), \
                    COALESCE(SUM(t.input_tokens), 0), COALESCE(SUM(t.output_tokens), 0), \
                    COALESCE(SUM(t.cache_read_tokens), 0), COALESCE(SUM(t.cache_miss_tokens), 0), \
                    COALESCE(SUM(t.cost_usd), 0), COUNT(DISTINCT t.workspace_id) \
             FROM telemetry t LEFT JOIN projects p ON p.project_id = t.project_id{where_sql} \
             GROUP BY t.project_id ORDER BY SUM(t.cost_usd) DESC"
        );
        return query_rows(conn, &sql, params, |r| {
            let cache_read: i64 = r.get(5)?;
            let cache_miss: i64 = r.get(6)?;
            Ok(json!({
                "key": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "calls": r.get::<_, i64>(2)?,
                "input_tokens": r.get::<_, i64>(3)?,
                "output_tokens": r.get::<_, i64>(4)?,
                "cache_read_tokens": cache_read,
                "cache_read_rate": cache_read_rate(cache_read, cache_miss),
                "cost_usd": r.get::<_, f64>(7)?,
                "workspaces": r.get::<_, i64>(8)?,
            }))
        });
    }
    let group_col = match level {
        "workspace" => "workspace_id",
        "repo" => "repo_id",
        _ => "org_id",
    };
    let sql = format!(
        "SELECT {group_col}, COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0), \
                COALESCE(SUM(cache_read_tokens), 0), COALESCE(SUM(cache_miss_tokens), 0), \
                COALESCE(SUM(cost_usd), 0), COUNT(DISTINCT project_id), COUNT(DISTINCT workspace_id) \
         FROM telemetry{where_sql} GROUP BY {group_col} ORDER BY SUM(cost_usd) DESC"
    );
    query_rows(conn, &sql, params, |r| {
        let cache_read: i64 = r.get(4)?;
        let cache_miss: i64 = r.get(5)?;
        Ok(json!({
            // NULL org/workspace → JSON null so the client can render `(local)`.
            "key": r.get::<_, Option<String>>(0)?,
            "calls": r.get::<_, i64>(1)?,
            "input_tokens": r.get::<_, i64>(2)?,
            "output_tokens": r.get::<_, i64>(3)?,
            "cache_read_tokens": cache_read,
            "cache_read_rate": cache_read_rate(cache_read, cache_miss),
            "cost_usd": r.get::<_, f64>(6)?,
            "projects": r.get::<_, i64>(7)?,
            "workspaces": r.get::<_, i64>(8)?,
        }))
    })
}

fn hub_by_provider_model(
    conn: &Connection,
    where_sql: &str,
    params: &[(&str, &dyn ToSql)],
) -> Vec<Value> {
    let sql = format!(
        "SELECT provider, model, COUNT(*), COALESCE(SUM(input_tokens), 0), \
                COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_read_tokens), 0), \
                COALESCE(SUM(cache_miss_tokens), 0), COALESCE(SUM(cost_usd), 0), \
                COUNT(DISTINCT project_id) \
         FROM telemetry{where_sql} GROUP BY provider, model ORDER BY SUM(cost_usd) DESC"
    );
    query_rows(conn, &sql, params, |r| {
        let cache_read: i64 = r.get(5)?;
        let cache_miss: i64 = r.get(6)?;
        Ok(json!({
            "provider": r.get::<_, String>(0)?,
            "model": r.get::<_, String>(1)?,
            "calls": r.get::<_, i64>(2)?,
            "input_tokens": r.get::<_, i64>(3)?,
            "output_tokens": r.get::<_, i64>(4)?,
            "cache_read_tokens": cache_read,
            "cache_read_rate": cache_read_rate(cache_read, cache_miss),
            "cost_usd": r.get::<_, f64>(7)?,
            "projects": r.get::<_, i64>(8)?,
        }))
    })
}

fn hub_by_repo(conn: &Connection, where_sql: &str, params: &[(&str, &dyn ToSql)]) -> Vec<Value> {
    let sql = format!(
        "SELECT repo_id, COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0), \
                COALESCE(SUM(cost_usd), 0), COUNT(DISTINCT project_id) \
         FROM telemetry{where_sql} GROUP BY repo_id ORDER BY SUM(cost_usd) DESC"
    );
    query_rows(conn, &sql, params, |r| {
        Ok(json!({
            "repo_id": r.get::<_, String>(0)?,
            "calls": r.get::<_, i64>(1)?,
            "input_tokens": r.get::<_, i64>(2)?,
            "output_tokens": r.get::<_, i64>(3)?,
            "cost_usd": r.get::<_, f64>(4)?,
            "projects": r.get::<_, i64>(5)?,
        }))
    })
}

fn hub_by_day(conn: &Connection, where_sql: &str, params: &[(&str, &dyn ToSql)]) -> Vec<Value> {
    // `recorded_at` is `COALESCE(started_at, '')`, an ISO-8601 datetime; the
    // first ten chars are the day (empty strings — rows never assigned one —
    // are excluded so they don't fold into a bogus bucket).
    let guard = if where_sql.is_empty() {
        " WHERE recorded_at <> ''".to_string()
    } else {
        format!("{where_sql} AND recorded_at <> ''")
    };
    let sql = format!(
        "SELECT substr(recorded_at, 1, 10), COUNT(*), COALESCE(SUM(cost_usd), 0), \
                COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0) \
         FROM telemetry{guard} GROUP BY substr(recorded_at, 1, 10) ORDER BY 1"
    );
    query_rows(conn, &sql, params, |r| {
        Ok(json!({
            "day": r.get::<_, String>(0)?,
            "calls": r.get::<_, i64>(1)?,
            "cost_usd": r.get::<_, f64>(2)?,
            "input_tokens": r.get::<_, i64>(3)?,
            "output_tokens": r.get::<_, i64>(4)?,
        }))
    })
}
