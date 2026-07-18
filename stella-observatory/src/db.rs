//! Read-only queries over the workspace's `.stella` SQLite stores.
//!
//! The observatory deliberately does **not** go through [`stella-store`]'s
//! `Store::open`: opening the store creates `.stella/` and runs schema
//! migrations — writes — and an observer must never mutate what it observes.
//! Instead every request opens the database file with
//! `SQLITE_OPEN_READ_ONLY` and runs its own SQL, mirroring the semantics of
//! `Store::usage_stats` (resolved = outcome `completed`; division
//! `off-grid` for provider `local`). Missing files and missing tables are
//! states, not errors — each section degrades to an empty payload so a
//! fresh workspace renders an empty dashboard instead of a 500.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

/// Everything that can go wrong serving observatory data.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The underlying SQLite read failed in a way that isn't "the table
    /// doesn't exist yet" (those degrade to empty results instead).
    #[error("telemetry query failed: {0}")]
    Query(#[from] rusqlite::Error),
}

/// A read-only handle on the workspace's telemetry stores.
pub struct Observatory {
    store_db: PathBuf,
    fleet_db: PathBuf,
    workspace_root: PathBuf,
}

impl Observatory {
    /// Point the observatory at a workspace root (the directory that holds
    /// `.stella/`). Nothing is opened or created here.
    pub fn new(workspace_root: &Path) -> Self {
        let dot = workspace_root.join(".stella");
        Self {
            store_db: dot.join("store.db"),
            fleet_db: dot.join("fleet.db"),
            workspace_root: workspace_root.to_path_buf(),
        }
    }

    /// Open `store.db` read-only, or `None` while it doesn't exist yet.
    fn store(&self) -> Option<Connection> {
        open_read_only(&self.store_db)
    }

    /// Open `fleet.db` read-only, or `None` while it doesn't exist yet.
    fn fleet_conn(&self) -> Option<Connection> {
        open_read_only(&self.fleet_db)
    }

    /// Workspace identity + which stores exist — the dashboard header.
    pub fn meta(&self) -> Value {
        json!({
            "workspace": self.workspace_root.display().to_string(),
            "store_db": self.store_db.exists(),
            "fleet_db": self.fleet_db.exists(),
        })
    }

    /// Headline aggregates: runs, resolve rate, spend, tokens, cache traffic,
    /// and the per-execution timeline the overview chart draws.
    pub fn overview(&self) -> Result<Value, DbError> {
        let Some(conn) = self.store() else {
            return Ok(json!({ "runs": 0, "timeline": [] }));
        };
        let head = or_empty(conn.query_row(
            "SELECT count(*),
                    count(*) FILTER (WHERE outcome = 'completed'),
                    coalesce(sum(cost_usd), 0)
             FROM executions",
            [],
            |r| {
                Ok(json!({
                    "runs": r.get::<_, i64>(0)?,
                    "resolved": r.get::<_, i64>(1)?,
                    "cost_usd": r.get::<_, f64>(2)?,
                }))
            },
        ))?;
        let tokens = or_empty(conn.query_row(
            "SELECT coalesce(sum(input_tokens), 0),
                    coalesce(sum(output_tokens), 0),
                    coalesce(sum(cache_read_tokens), 0),
                    coalesce(sum(cache_write_tokens), 0),
                    coalesce(sum(cache_miss_tokens), 0),
                    coalesce(sum(duration_ms), 0),
                    count(*)
             FROM telemetry",
            [],
            |r| {
                Ok(json!({
                    "input_tokens": r.get::<_, i64>(0)?,
                    "output_tokens": r.get::<_, i64>(1)?,
                    "cache_read_tokens": r.get::<_, i64>(2)?,
                    "cache_write_tokens": r.get::<_, i64>(3)?,
                    "cache_miss_tokens": r.get::<_, i64>(4)?,
                    "model_ms": r.get::<_, i64>(5)?,
                    "steps": r.get::<_, i64>(6)?,
                }))
            },
        ))?;
        let timeline = collect_rows(
            &conn,
            "SELECT e.id, e.kind, e.provider, e.model, e.outcome, e.cost_usd,
                    e.started_at, e.finished_at,
                    coalesce(t.input_tokens, 0), coalesce(t.output_tokens, 0),
                    coalesce(t.duration_ms, 0)
             FROM executions e
             LEFT JOIN (SELECT execution_id,
                               sum(input_tokens)  AS input_tokens,
                               sum(output_tokens) AS output_tokens,
                               sum(duration_ms)   AS duration_ms
                        FROM telemetry GROUP BY execution_id) t
               ON t.execution_id = e.id
             ORDER BY e.id ASC",
            |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "kind": r.get::<_, String>(1)?,
                    "provider": r.get::<_, String>(2)?,
                    "model": r.get::<_, String>(3)?,
                    "outcome": r.get::<_, Option<String>>(4)?,
                    "cost_usd": r.get::<_, f64>(5)?,
                    "started_at": r.get::<_, String>(6)?,
                    "finished_at": r.get::<_, Option<String>>(7)?,
                    "input_tokens": r.get::<_, i64>(8)?,
                    "output_tokens": r.get::<_, i64>(9)?,
                    "model_ms": r.get::<_, i64>(10)?,
                }))
            },
        )?;
        let mut out = head;
        merge(&mut out, tokens);
        out["timeline"] = Value::Array(timeline);
        Ok(out)
    }

    /// The executions table: one row per run with its aggregates.
    pub fn executions(&self) -> Result<Value, DbError> {
        let Some(conn) = self.store() else {
            return Ok(json!([]));
        };
        let rows = collect_rows(
            &conn,
            "SELECT e.id, e.kind, e.prompt, e.provider, e.model, e.outcome,
                    e.cost_usd, e.started_at, e.finished_at,
                    coalesce(t.steps, 0), coalesce(t.input_tokens, 0),
                    coalesce(t.output_tokens, 0), coalesce(t.duration_ms, 0),
                    coalesce(tc.calls, 0), coalesce(ft.files, 0)
             FROM executions e
             LEFT JOIN (SELECT execution_id, count(*) AS steps,
                               sum(input_tokens)  AS input_tokens,
                               sum(output_tokens) AS output_tokens,
                               sum(duration_ms)   AS duration_ms
                        FROM telemetry GROUP BY execution_id) t
               ON t.execution_id = e.id
             LEFT JOIN (SELECT execution_id, count(*) AS calls
                        FROM tool_calls GROUP BY execution_id) tc
               ON tc.execution_id = e.id
             LEFT JOIN (SELECT execution_id, count(*) AS files
                        FROM files_touched GROUP BY execution_id) ft
               ON ft.execution_id = e.id
             ORDER BY e.id DESC",
            |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "kind": r.get::<_, String>(1)?,
                    "prompt": truncate(r.get::<_, String>(2)?, 160),
                    "provider": r.get::<_, String>(3)?,
                    "model": r.get::<_, String>(4)?,
                    "outcome": r.get::<_, Option<String>>(5)?,
                    "cost_usd": r.get::<_, f64>(6)?,
                    "started_at": r.get::<_, String>(7)?,
                    "finished_at": r.get::<_, Option<String>>(8)?,
                    "steps": r.get::<_, i64>(9)?,
                    "input_tokens": r.get::<_, i64>(10)?,
                    "output_tokens": r.get::<_, i64>(11)?,
                    "model_ms": r.get::<_, i64>(12)?,
                    "tool_calls": r.get::<_, i64>(13)?,
                    "files_touched": r.get::<_, i64>(14)?,
                }))
            },
        )?;
        Ok(Value::Array(rows))
    }

    /// One execution drilled all the way down: step telemetry, tool calls,
    /// files touched, and the post-turn self-reflection when one exists.
    pub fn execution(&self, id: i64) -> Result<Value, DbError> {
        let Some(conn) = self.store() else {
            return Ok(Value::Null);
        };
        let head = or_empty(conn.query_row(
            "SELECT id, kind, prompt, provider, model, outcome, cost_usd,
                    started_at, finished_at
             FROM executions WHERE id = ?1",
            [id],
            |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "kind": r.get::<_, String>(1)?,
                    "prompt": r.get::<_, String>(2)?,
                    "provider": r.get::<_, String>(3)?,
                    "model": r.get::<_, String>(4)?,
                    "outcome": r.get::<_, Option<String>>(5)?,
                    "cost_usd": r.get::<_, f64>(6)?,
                    "started_at": r.get::<_, String>(7)?,
                    "finished_at": r.get::<_, Option<String>>(8)?,
                }))
            },
        ))?;
        if head == json!({}) {
            return Ok(Value::Null);
        }
        let steps = collect_rows_for(
            &conn,
            id,
            "SELECT step, provider, model, input_tokens, output_tokens,
                    cache_read_tokens, cache_miss_tokens, cache_write_tokens,
                    cost_usd, duration_ms, retries, tool_calls
             FROM telemetry WHERE execution_id = ?1 ORDER BY step ASC",
            |r| {
                Ok(json!({
                    "step": r.get::<_, i64>(0)?,
                    "provider": r.get::<_, String>(1)?,
                    "model": r.get::<_, String>(2)?,
                    "input_tokens": r.get::<_, i64>(3)?,
                    "output_tokens": r.get::<_, i64>(4)?,
                    "cache_read_tokens": r.get::<_, i64>(5)?,
                    "cache_miss_tokens": r.get::<_, i64>(6)?,
                    "cache_write_tokens": r.get::<_, i64>(7)?,
                    "cost_usd": r.get::<_, f64>(8)?,
                    "duration_ms": r.get::<_, i64>(9)?,
                    "retries": r.get::<_, i64>(10)?,
                    "tool_calls": r.get::<_, i64>(11)?,
                }))
            },
        )?;
        let tools = collect_rows_for(
            &conn,
            id,
            "SELECT seq, name, surface, reason, ok, error, bytes_out,
                    duration_ms, ts
             FROM tool_calls WHERE execution_id = ?1 ORDER BY seq ASC",
            |r| {
                Ok(json!({
                    "seq": r.get::<_, i64>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "surface": r.get::<_, String>(2)?,
                    "reason": r.get::<_, String>(3)?,
                    "ok": r.get::<_, i64>(4)? != 0,
                    "error": r.get::<_, String>(5)?,
                    "bytes_out": r.get::<_, i64>(6)?,
                    "duration_ms": r.get::<_, i64>(7)?,
                    "ts": r.get::<_, String>(8)?,
                }))
            },
        )?;
        let files = collect_rows_for(
            &conn,
            id,
            "SELECT path, ops, lines_added, lines_removed
             FROM files_touched WHERE execution_id = ?1 ORDER BY path ASC",
            |r| {
                Ok(json!({
                    "path": r.get::<_, String>(0)?,
                    "ops": r.get::<_, String>(1)?,
                    "lines_added": r.get::<_, i64>(2)?,
                    "lines_removed": r.get::<_, i64>(3)?,
                }))
            },
        )?;
        let reflection = or_empty(conn.query_row(
            "SELECT delivered, self_rating, what_went_well, what_to_improve,
                    critique
             FROM execution_reflection WHERE execution_id = ?1",
            [id],
            |r| {
                Ok(json!({
                    "delivered": r.get::<_, Option<i64>>(0)?,
                    "self_rating": r.get::<_, Option<i64>>(1)?,
                    "what_went_well": r.get::<_, String>(2)?,
                    "what_to_improve": r.get::<_, String>(3)?,
                    "critique": r.get::<_, String>(4)?,
                }))
            },
        ))?;
        let mut out = head;
        out["steps"] = Value::Array(steps);
        out["tools"] = Value::Array(tools);
        out["files"] = Value::Array(files);
        out["reflection"] = if reflection == json!({}) {
            Value::Null
        } else {
            reflection
        };
        Ok(out)
    }

    /// Per-(provider, model) usage — the same rows `stella stats` prints,
    /// same semantics (`resolved` = outcome `completed`, `off-grid` = local).
    pub fn models(&self) -> Result<Value, DbError> {
        let Some(conn) = self.store() else {
            return Ok(json!([]));
        };
        let rows = collect_rows(
            &conn,
            "SELECT e.provider, e.model, count(*) AS runs,
                    count(*) FILTER (WHERE e.outcome = 'completed') AS resolved,
                    coalesce(sum(e.cost_usd), 0) AS total_cost_usd,
                    CAST(coalesce(sum(t.input_tokens), 0) AS INTEGER),
                    CAST(coalesce(sum(t.output_tokens), 0) AS INTEGER),
                    CAST(coalesce(sum(t.cache_read_tokens), 0) AS INTEGER),
                    CAST(coalesce(sum(t.cache_write_tokens), 0) AS INTEGER),
                    CAST(coalesce(sum(t.duration_ms), 0) AS INTEGER)
             FROM executions e
             LEFT JOIN (SELECT execution_id,
                               sum(input_tokens)       AS input_tokens,
                               sum(output_tokens)      AS output_tokens,
                               sum(cache_read_tokens)  AS cache_read_tokens,
                               sum(cache_write_tokens) AS cache_write_tokens,
                               sum(duration_ms)        AS duration_ms
                        FROM telemetry GROUP BY execution_id) t
               ON t.execution_id = e.id
             GROUP BY e.provider, e.model
             ORDER BY total_cost_usd DESC, e.provider ASC, e.model ASC",
            |r| {
                let provider: String = r.get(0)?;
                let runs: i64 = r.get(2)?;
                let resolved: i64 = r.get(3)?;
                let cost: f64 = r.get(4)?;
                let total_ms: i64 = r.get(9)?;
                Ok(json!({
                    "division": if provider == "local" { "off-grid" } else { "-" },
                    "provider": provider,
                    "model": r.get::<_, String>(1)?,
                    "runs": runs,
                    "resolved": resolved,
                    "resolve_rate": resolved as f64 / runs.max(1) as f64,
                    "total_cost_usd": cost,
                    "cost_per_resolved_usd":
                        if resolved > 0 { json!(cost / resolved as f64) } else { Value::Null },
                    "input_tokens": r.get::<_, i64>(5)?,
                    "output_tokens": r.get::<_, i64>(6)?,
                    "cache_read_tokens": r.get::<_, i64>(7)?,
                    "cache_write_tokens": r.get::<_, i64>(8)?,
                    "avg_duration_ms": total_ms as f64 / runs.max(1) as f64,
                }))
            },
        )?;
        Ok(Value::Array(rows))
    }

    /// Tool leaderboard: calls, failures, latency, bytes returned. The p50
    /// is computed here (SQLite has no percentile function).
    pub fn tools(&self) -> Result<Value, DbError> {
        let Some(conn) = self.store() else {
            return Ok(json!([]));
        };
        let raw = collect_rows(
            &conn,
            "SELECT name, ok, duration_ms, bytes_out FROM tool_calls",
            |r| {
                Ok(json!({
                    "name": r.get::<_, String>(0)?,
                    "ok": r.get::<_, i64>(1)? != 0,
                    "duration_ms": r.get::<_, i64>(2)?,
                    "bytes_out": r.get::<_, i64>(3)?,
                }))
            },
        )?;
        let mut by_name: std::collections::BTreeMap<String, (i64, i64, Vec<i64>, i64)> =
            std::collections::BTreeMap::new();
        for row in &raw {
            let name = row["name"].as_str().unwrap_or_default().to_string();
            let entry = by_name.entry(name).or_default();
            entry.0 += 1;
            if !row["ok"].as_bool().unwrap_or(true) {
                entry.1 += 1;
            }
            entry.2.push(row["duration_ms"].as_i64().unwrap_or(0));
            entry.3 += row["bytes_out"].as_i64().unwrap_or(0);
        }
        let mut rows: Vec<Value> = by_name
            .into_iter()
            .map(|(name, (calls, errors, mut durs, bytes))| {
                durs.sort_unstable();
                let p50 = durs.get(durs.len() / 2).copied().unwrap_or(0);
                let max = durs.last().copied().unwrap_or(0);
                json!({
                    "name": name,
                    "calls": calls,
                    "errors": errors,
                    "p50_ms": p50,
                    "max_ms": max,
                    "bytes_out": bytes,
                })
            })
            .collect();
        rows.sort_by_key(|r| -r["calls"].as_i64().unwrap_or(0));
        Ok(Value::Array(rows))
    }

    /// Files-touched leaderboard across all executions.
    pub fn files(&self) -> Result<Value, DbError> {
        let Some(conn) = self.store() else {
            return Ok(json!([]));
        };
        let rows = collect_rows(
            &conn,
            "SELECT path, count(*) AS execs,
                    coalesce(sum(lines_added), 0),
                    coalesce(sum(lines_removed), 0)
             FROM files_touched
             GROUP BY path
             ORDER BY execs DESC, path ASC
             LIMIT 100",
            |r| {
                Ok(json!({
                    "path": r.get::<_, String>(0)?,
                    "executions": r.get::<_, i64>(1)?,
                    "lines_added": r.get::<_, i64>(2)?,
                    "lines_removed": r.get::<_, i64>(3)?,
                }))
            },
        )?;
        Ok(Value::Array(rows))
    }

    /// The self-improvement plane: memory citations, mined reflections,
    /// promoted rules, and skill usage.
    pub fn memory(&self) -> Result<Value, DbError> {
        let Some(conn) = self.store() else {
            return Ok(json!({
                "citations": [], "reflections": [], "rules": [], "skills": []
            }));
        };
        let citations = collect_rows(
            &conn,
            "SELECT memory_id, count(*) AS cites,
                    avg(useful_score),
                    avg(truthful),
                    max(ts)
             FROM memory_citations
             GROUP BY memory_id ORDER BY cites DESC LIMIT 50",
            |r| {
                Ok(json!({
                    "memory_id": r.get::<_, String>(0)?,
                    "citations": r.get::<_, i64>(1)?,
                    "avg_useful": r.get::<_, f64>(2)?,
                    "truthful_rate": r.get::<_, f64>(3)?,
                    "last_cited": r.get::<_, String>(4)?,
                }))
            },
        )?;
        let reflections = collect_rows(
            &conn,
            "SELECT kind, content, domains, occurred_at
             FROM reflections ORDER BY occurred_at DESC LIMIT 100",
            |r| {
                Ok(json!({
                    "kind": r.get::<_, String>(0)?,
                    "content": r.get::<_, String>(1)?,
                    "domains": r.get::<_, String>(2)?,
                    "occurred_at": r.get::<_, i64>(3)?,
                }))
            },
        )?;
        let rules = collect_rows(
            &conn,
            "SELECT rule_id, contents, source, created_at
             FROM rules ORDER BY created_at DESC LIMIT 50",
            |r| {
                Ok(json!({
                    "rule_id": r.get::<_, String>(0)?,
                    "contents": truncate(r.get::<_, String>(1)?, 240),
                    "source": r.get::<_, String>(2)?,
                    "created_at": r.get::<_, String>(3)?,
                }))
            },
        )?;
        let skills = collect_rows(
            &conn,
            "SELECT skill, count(*) AS uses, max(version), max(ts)
             FROM skill_usage GROUP BY skill ORDER BY uses DESC LIMIT 50",
            |r| {
                Ok(json!({
                    "skill": r.get::<_, String>(0)?,
                    "uses": r.get::<_, i64>(1)?,
                    "version": r.get::<_, i64>(2)?,
                    "last_used": r.get::<_, String>(3)?,
                }))
            },
        )?;
        Ok(json!({
            "citations": citations,
            "reflections": reflections,
            "rules": rules,
            "skills": skills,
        }))
    }

    /// MCP tool traffic per (server, tool).
    pub fn mcp(&self) -> Result<Value, DbError> {
        let Some(conn) = self.store() else {
            return Ok(json!([]));
        };
        let rows = collect_rows(
            &conn,
            "SELECT server, tool, count(*) AS calls, max(called_at_ms)
             FROM mcp_usage GROUP BY server, tool ORDER BY calls DESC LIMIT 100",
            |r| {
                Ok(json!({
                    "server": r.get::<_, String>(0)?,
                    "tool": r.get::<_, String>(1)?,
                    "calls": r.get::<_, i64>(2)?,
                    "last_called_ms": r.get::<_, i64>(3)?,
                }))
            },
        )?;
        Ok(Value::Array(rows))
    }

    /// Fleet ledger: every fan-out run with its tasks, attempts, commits.
    pub fn fleet(&self) -> Result<Value, DbError> {
        let Some(conn) = self.fleet_conn() else {
            return Ok(json!([]));
        };
        let rows = collect_rows(
            &conn,
            "SELECT r.id, r.root_task_count, r.created_at_ms,
                    (SELECT count(*) FROM attempts a WHERE a.run_id = r.id),
                    (SELECT count(*) FROM attempts a
                      WHERE a.run_id = r.id AND a.success = 1),
                    (SELECT count(*) FROM commits c WHERE c.run_id = r.id)
             FROM runs r ORDER BY r.created_at_ms DESC LIMIT 50",
            |r| {
                Ok(json!({
                    "run_id": r.get::<_, String>(0)?,
                    "tasks": r.get::<_, i64>(1)?,
                    "created_at_ms": r.get::<_, i64>(2)?,
                    "attempts": r.get::<_, i64>(3)?,
                    "succeeded": r.get::<_, i64>(4)?,
                    "commits": r.get::<_, i64>(5)?,
                }))
            },
        )?;
        Ok(Value::Array(rows))
    }
}

/// Open a SQLite file strictly read-only; `None` when it doesn't exist.
fn open_read_only(path: &Path) -> Option<Connection> {
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

/// Run a query collecting every row; a missing table degrades to `[]` so a
/// dashboard section renders empty instead of failing the whole page.
fn collect_rows<F>(conn: &Connection, sql: &str, map: F) -> Result<Vec<Value>, DbError>
where
    F: Fn(&rusqlite::Row<'_>) -> rusqlite::Result<Value>,
{
    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(e) if is_missing_table(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let rows = stmt.query_map([], map)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// [`collect_rows`] scoped to one execution id, bound as a parameter.
fn collect_rows_for<F>(conn: &Connection, id: i64, sql: &str, map: F) -> Result<Vec<Value>, DbError>
where
    F: Fn(&rusqlite::Row<'_>) -> rusqlite::Result<Value>,
{
    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(e) if is_missing_table(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let rows = stmt.query_map([id], map)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Same degradation for single-row lookups: missing table or no row → `{}`.
fn or_empty(result: rusqlite::Result<Value>) -> Result<Value, DbError> {
    match result {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(json!({})),
        Err(e) if is_missing_table(&e) => Ok(json!({})),
        Err(e) => Err(e.into()),
    }
}

fn is_missing_table(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("no such table")
    )
}

/// Shallow-merge `extra`'s fields into `base` (both must be objects).
fn merge(base: &mut Value, extra: Value) {
    if let (Some(base_map), Value::Object(extra_map)) = (base.as_object_mut(), extra) {
        for (k, v) in extra_map {
            base_map.insert(k, v);
        }
    }
}

/// Cap a string at `max` chars on a char boundary, appending an ellipsis.
fn truncate(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}
