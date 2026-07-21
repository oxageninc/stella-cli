//! The code-graph view: files and import edges out of the workspace's
//! `.stella/private/codegraph.db` (written by `stella-graph`'s tree-sitter indexer),
//! flattened into the `{nodes, edges}` shape a force-directed canvas wants.
//!
//! The indexer resolves TS/JS/Python *relative* imports to concrete files
//! (`to_path`), but Rust `use` paths are recorded as bare specifiers
//! (`crate::db`, `stella_store::usage::…`). Those are resolved here by
//! module-path lookup: every `<crate>/src/**.rs` file registers its Rust
//! module path (`stella_cli::agent`, dashes to underscores, `lib.rs`/
//! `main.rs`/`mod.rs` collapsing to their parent), and a specifier matches
//! the longest registered prefix. `std`/`super`/`self` and external crates
//! simply don't match — exactly right for a workspace graph.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

/// Hard cap on nodes shipped to the browser; the graph keeps the
/// highest-degree files and reports how many were folded away.
const MAX_NODES: usize = 600;

/// Build the graph snapshot, or an empty one while `codegraph.db` doesn't
/// exist (the dashboard renders a "run stella to index" empty state).
pub fn snapshot(workspace_root: &Path) -> Value {
    let path = workspace_root
        .join(".stella")
        .join("private")
        .join("codegraph.db");
    let empty = json!({
        "nodes": [], "edges": [], "groups": [],
        "total_files": 0, "total_symbols": 0, "truncated": 0,
    });
    if !path.exists() {
        return empty;
    }
    let Ok(conn) = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return empty;
    };

    // Files: id → (path, language), plus per-file symbol counts.
    let mut files: Vec<(i64, String, String)> = Vec::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id, path, language FROM code_graph_files")
        && let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
    {
        files = rows.flatten().collect();
    }
    if files.is_empty() {
        return empty;
    }
    let mut symbols: HashMap<i64, i64> = HashMap::new();
    let mut total_symbols = 0_i64;
    if let Ok(mut stmt) =
        conn.prepare("SELECT file_id, count(*) FROM code_graph_symbols GROUP BY file_id")
        && let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
    {
        for (file_id, count) in rows.flatten() {
            total_symbols += count;
            symbols.insert(file_id, count);
        }
    }

    // Index files two ways: by exact repo-relative path (for resolved
    // `to_path` imports) and by Rust module path (for bare specifiers).
    let by_path: HashMap<&str, usize> = files
        .iter()
        .enumerate()
        .map(|(i, (_, p, _))| (p.as_str(), i))
        .collect();
    let mut by_module: HashMap<String, usize> = HashMap::new();
    for (i, (_, path, lang)) in files.iter().enumerate() {
        if lang == "rust"
            && let Some(module) = rust_module_path(path)
        {
            // First writer wins so `lib.rs` (registered before a
            // sibling `main.rs` only by luck) doesn't matter: prefer
            // lib.rs explicitly.
            if path.ends_with("/lib.rs") {
                by_module.insert(module, i);
            } else {
                by_module.entry(module).or_insert(i);
            }
        }
    }

    // Import edges: resolved `to_path` first, module-path lookup otherwise.
    let mut edge_weight: BTreeMap<(usize, usize), i64> = BTreeMap::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT from_file_id, specifier, to_path FROM code_graph_imports")
        && let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
    {
        let by_id: HashMap<i64, usize> = files
            .iter()
            .enumerate()
            .map(|(i, (id, _, _))| (*id, i))
            .collect();
        for (from_id, specifier, to_path) in rows.flatten() {
            let Some(&from) = by_id.get(&from_id) else {
                continue;
            };
            let to = to_path
                .as_deref()
                .and_then(|p| by_path.get(p).copied())
                .or_else(|| resolve_rust_specifier(&specifier, &files[from].1, &by_module));
            if let Some(to) = to
                && to != from
            {
                *edge_weight.entry((from, to)).or_insert(0) += 1;
            }
        }
    }

    // Degrees, then (if oversized) keep the busiest nodes.
    let mut deg_in = vec![0_i64; files.len()];
    let mut deg_out = vec![0_i64; files.len()];
    for &(from, to) in edge_weight.keys() {
        deg_out[from] += 1;
        deg_in[to] += 1;
    }
    let mut keep: Vec<usize> = (0..files.len()).collect();
    let mut truncated = 0;
    if keep.len() > MAX_NODES {
        keep.sort_by_key(|&i| {
            let sym = symbols.get(&files[i].0).copied().unwrap_or(0);
            -(deg_in[i] + deg_out[i]) * 1000 - sym
        });
        truncated = keep.len() - MAX_NODES;
        keep.truncate(MAX_NODES);
    }
    let index_of: HashMap<usize, usize> = keep.iter().enumerate().map(|(n, &i)| (i, n)).collect();

    let mut group_files: BTreeMap<String, i64> = BTreeMap::new();
    let nodes: Vec<Value> = keep
        .iter()
        .map(|&i| {
            let (id, path, lang) = &files[i];
            let group = path.split('/').next().unwrap_or("").to_string();
            *group_files.entry(group.clone()).or_insert(0) += 1;
            json!({
                "path": path,
                "lang": lang,
                "group": group,
                "symbols": symbols.get(id).copied().unwrap_or(0),
                "in": deg_in[i],
                "out": deg_out[i],
            })
        })
        .collect();
    let edges: Vec<Value> = edge_weight
        .iter()
        .filter_map(|(&(from, to), &w)| {
            let f = index_of.get(&from)?;
            let t = index_of.get(&to)?;
            Some(json!([f, t, w]))
        })
        .collect();
    let mut groups: Vec<Value> = group_files
        .into_iter()
        .map(|(name, count)| json!({ "name": name, "files": count }))
        .collect();
    groups.sort_by_key(|g| -g["files"].as_i64().unwrap_or(0));

    json!({
        "nodes": nodes,
        "edges": edges,
        "groups": groups,
        "total_files": files.len(),
        "total_symbols": total_symbols,
        "truncated": truncated,
    })
}

/// `stella-cli/src/agent.rs` → `stella_cli::agent`;
/// `stella-graph/src/lib.rs` → `stella_graph`;
/// `crate/src/foo/mod.rs` → `crate::foo`. Non-`src` files get no module.
fn rust_module_path(path: &str) -> Option<String> {
    let mut parts = path.split('/');
    let crate_dir = parts.next()?;
    if parts.next()? != "src" {
        return None;
    }
    let crate_name = crate_dir.replace('-', "_");
    let mut segments = vec![crate_name];
    let rest: Vec<&str> = parts.collect();
    for (i, seg) in rest.iter().enumerate() {
        let is_last = i == rest.len() - 1;
        if is_last {
            let stem = seg.strip_suffix(".rs")?;
            if stem != "lib" && stem != "main" && stem != "mod" {
                segments.push(stem.to_string());
            }
        } else {
            segments.push((*seg).to_string());
        }
    }
    Some(segments.join("::"))
}

/// Match a Rust `use` specifier to a registered module, longest prefix
/// first. `crate::…` is rewritten to the importing file's own crate.
fn resolve_rust_specifier(
    specifier: &str,
    from_path: &str,
    by_module: &HashMap<String, usize>,
) -> Option<usize> {
    // `stella_graph::{CodeGraph, WatchInjector}` → `stella_graph`.
    let clean = specifier.split("::{").next().unwrap_or(specifier).trim();
    let mut segments: Vec<&str> = clean.split("::").collect();
    let own_crate;
    match *segments.first()? {
        "crate" => {
            let dir = from_path.split('/').next()?;
            own_crate = dir.replace('-', "_");
            segments[0] = &own_crate;
        }
        "super" | "self" | "std" | "core" | "alloc" => return None,
        _ => {}
    }
    // Longest prefix that names a real module wins; single-segment matches
    // land on the crate root (lib.rs).
    for end in (1..=segments.len()).rev() {
        if let Some(&idx) = by_module.get(&segments[..end].join("::")) {
            return Some(idx);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_paths_collapse_lib_main_mod() {
        assert_eq!(
            rust_module_path("stella-cli/src/agent.rs").as_deref(),
            Some("stella_cli::agent")
        );
        assert_eq!(
            rust_module_path("stella-graph/src/lib.rs").as_deref(),
            Some("stella_graph")
        );
        assert_eq!(
            rust_module_path("x/src/views/mod.rs").as_deref(),
            Some("x::views")
        );
        assert_eq!(rust_module_path("bench/run.py"), None);
    }

    #[test]
    fn specifiers_resolve_cross_crate_and_crate_local() {
        let mut modules = HashMap::new();
        modules.insert("stella_graph".to_string(), 0_usize);
        modules.insert("stella_cli::plan".to_string(), 1_usize);
        assert_eq!(
            resolve_rust_specifier(
                "stella_graph::{CodeGraph, WatchInjector}",
                "stella-cli/src/agent.rs",
                &modules
            ),
            Some(0)
        );
        assert_eq!(
            resolve_rust_specifier("crate::plan::PlanStep", "stella-cli/src/agent.rs", &modules),
            Some(1)
        );
        assert_eq!(
            resolve_rust_specifier("std::collections::HashMap", "x/src/lib.rs", &modules),
            None
        );
        assert_eq!(
            resolve_rust_specifier("super::*", "x/src/lib.rs", &modules),
            None
        );
    }
}
