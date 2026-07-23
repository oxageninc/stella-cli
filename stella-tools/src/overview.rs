//! `project_overview` — one call that answers "what is this repository?".
//!
//! Every other orientation tool in this crate is a batch executor for
//! questions the caller has already formed: `graph_query` needs a symbol or
//! file, `gather_context` needs patterns and globs, `grep` needs a regex.
//! None of them can be the *first* move, so an agent opening an unfamiliar
//! tree has no choice but to glob-and-read its way to a mental model — the
//! 10-30 call orientation loop this collapses into one.
//!
//! Assembly, not new capability: every field comes from a deterministic
//! source that already exists — the script index (static manifest
//! detection), the code graph, the storage/schema snapshot, and the domain
//! taxonomy. No model call, no shell, no grep.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};
use stella_protocol::{ToolOutput, ToolSchema};

use crate::registry::Tool;
use crate::scripts::ScriptIndex;

/// Entry points reported at most, shallowest first. The derivation is one
/// SQL anti-join (`CodeGraph::entry_points`) whatever the repository size,
/// so this bounds the rendered list, never the scan — a monorepo past a few
/// hundred files gets the same roots a small tree does.
const MAX_ENTRY_POINTS: usize = 12;

/// Top-level directories named in the layout summary at most. Past this the
/// remainder collapses to a count, so a monorepo with hundreds of packages
/// still renders a few dozen deterministic tokens.
const MAX_TOP_LEVEL_DIRS: usize = 12;

pub struct ProjectOverview;

#[async_trait]
impl Tool for ProjectOverview {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "project_overview".into(),
            description: "CALL THIS FIRST on an unfamiliar repository. Returns one JSON \
                          object describing the whole project — language and frameworks, \
                          the build/test/lint commands, entry-point files, the storage \
                          schema, domain taxonomy, and index freshness — assembled from \
                          static manifests and the code graph. Takes no arguments and \
                          costs no model call. Replaces the usual opening burst of \
                          glob/grep/read_file: use it before those, then reach for \
                          graph_query or gather_context once you know what to ask about."
                .into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            // Read-only in the sense the flag means: it mutates no
            // workspace state, so speculative execution commutes with
            // everything around it. The index catch-up writes only to
            // Stella's own codegraph.db, which is invisible to the model and
            // serialized by the store's write guard.
            read_only: true,
        }
    }

    async fn execute(&self, _input: &Value, root: &Path) -> ToolOutput {
        ToolOutput::Ok {
            content: match serde_json::to_string_pretty(&build_overview(root)) {
                Ok(text) => text,
                Err(error) => {
                    return ToolOutput::Error {
                        message: format!("could not render the project overview: {error}"),
                    };
                }
            },
        }
    }
}

/// A compact, deterministic orientation block for the system prompt, or
/// `None` when there is no usable index.
///
/// Read-only on purpose: it opens an **existing** index and never builds one,
/// so it can be called during system-prompt assembly without ever blocking
/// the first response on an index build (which would defeat the point of a
/// fast first turn). When the index is absent — a fresh session before the
/// background build finishes — it returns `None` and the model can still call
/// `project_overview` explicitly.
///
/// Deliberately the complement of the script index (which the prompt already
/// injects separately): languages, top-level layout, entry points, and
/// storage — the slow-churning skeleton the model would otherwise spend
/// grep/glob/read_file turns discovering. Fine detail that changes within a
/// session stays with `graph_query`/`read_file`; everything here is derived
/// from `codegraph.db`, so the block is byte-stable for a given index state
/// and kept to a few lines so it stays cheap in the cache-stable system
/// prefix. Every line is bounded by construction (issue #328): entry points
/// come from one SQL anti-join and the layout collapses past
/// [`MAX_TOP_LEVEL_DIRS`], so a monorepo far beyond a few hundred files
/// renders the same useful map a small tree does.
pub fn render_orientation_block(root: &Path) -> Option<String> {
    let path = stella_store::existing_workspace_private_sqlite_path(root, "codegraph.db")
        .ok()
        .flatten()?;
    let graph = stella_graph::CodeGraph::open(root, &path).ok()?;

    let files = graph.file_count().unwrap_or(0);
    if files == 0 {
        return None;
    }

    let mut lines =
        vec!["## Project map (indexed — you do not need to grep/glob to find this)".to_string()];

    let all_files = graph.all_files().unwrap_or_default();
    let mut languages: BTreeSet<&'static str> = BTreeSet::new();
    for file in &all_files {
        if let Some(language) = language_of(file) {
            languages.insert(language);
        }
    }
    if !languages.is_empty() {
        lines.push(format!(
            "Languages: {}",
            languages.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }

    if let Some(layout) = top_level_summary(&all_files) {
        lines.push(layout);
    }

    let entry_points = graph.entry_points(MAX_ENTRY_POINTS).unwrap_or_default();
    if !entry_points.is_empty() {
        lines.push(format!("Entry points: {}", entry_points.join(", ")));
    }

    let storage = graph.storage_snapshot();
    if !storage.is_empty() {
        let layers: Vec<String> = storage.layers.iter().map(|l| l.key.clone()).collect();
        let layer_note = if layers.is_empty() {
            String::new()
        } else {
            format!(" across {}", layers.join(", "))
        };
        lines.push(format!(
            "Storage: {} relation(s){layer_note}",
            storage.relations.len()
        ));
    }

    // Only the header means nothing worth injecting.
    if lines.len() == 1 {
        return None;
    }
    Some(lines.join("\n"))
}

/// Assemble the overview. Total by construction: every source degrades to
/// its empty shape, because an orientation call that errors sends the agent
/// straight back to the glob loop this exists to replace.
pub fn build_overview(root: &Path) -> Value {
    let scripts = ScriptIndex::detect_blocking(root);
    let graph = open_graph(root);

    let mut out = json!({
        "workspace": root.display().to_string(),
        "scripts": scripts_section(&scripts),
        "domains": domains_section(root),
    });

    let map = out.as_object_mut().expect("object literal");
    match &graph {
        Some(graph) => {
            map.insert("index".into(), index_section(graph));
            map.insert("code".into(), code_section(graph));
            map.insert("storage".into(), storage_section(&graph.storage_snapshot()));
        }
        None => {
            // Say so plainly. A confident-looking object with silently empty
            // fields would read as "this project has no code".
            map.insert(
                "index".into(),
                json!({
                    "built": false,
                    "note": "no code graph index — run `stella init` to build one; \
                             language, entry points, and storage are unavailable until then",
                }),
            );
        }
    }
    out
}

fn open_graph(root: &Path) -> Option<stella_graph::CodeGraph> {
    // Build on first use, the same path `graph_query` takes: project_overview
    // is meant to be the FIRST call in a session, before the background index
    // build could possibly have finished, so it must be able to produce the
    // index it reports on rather than waiting for one to appear.
    crate::graph::open_or_build(root).ok()
}

fn index_section(graph: &stella_graph::CodeGraph) -> Value {
    json!({
        "built": true,
        "files": graph.file_count().unwrap_or(0),
        "symbols": graph.symbol_count().unwrap_or(0),
        "imports": graph.import_count().unwrap_or(0),
        // The index is a point-in-time build, so anything written since is
        // invisible here. Saying so is what keeps a stale answer from being
        // mistaken for a current one.
        "freshness": "caught up to the working tree when this call ran",
    })
}

fn code_section(graph: &stella_graph::CodeGraph) -> Value {
    let files = graph.all_files().unwrap_or_default();
    let mut languages: BTreeSet<String> = BTreeSet::new();
    for file in &files {
        if let Some(language) = language_of(file) {
            languages.insert(language.to_string());
        }
    }

    json!({
        "languages": languages.into_iter().collect::<Vec<_>>(),
        "busiest_file": graph.busiest_file().unwrap_or(None),
        "top_level": top_level_summary(&files),
        "entry_points": graph.entry_points(MAX_ENTRY_POINTS).unwrap_or_default(),
    })
}

fn storage_section(snapshot: &stella_graph::StorageSnapshot) -> Value {
    if snapshot.is_empty() {
        return json!({ "relations": 0 });
    }
    json!({
        "relations": snapshot.relations.len(),
        "layers": snapshot
            .layers
            .iter()
            .map(|layer| layer.key.clone())
            .collect::<Vec<_>>(),
        "relation_names": snapshot
            .relations
            .iter()
            .map(|relation| relation.address.clone())
            .collect::<Vec<_>>(),
    })
}

fn scripts_section(scripts: &ScriptIndex) -> Value {
    if scripts.is_empty() {
        return json!({ "detected": false });
    }
    let verbs: serde_json::Map<String, Value> =
        ["install", "build", "start", "test", "lint", "format"]
            .iter()
            .filter_map(|verb| {
                scripts
                    .verb_entry(verb)
                    .map(|entry| ((*verb).to_string(), json!(entry.command.clone())))
            })
            .collect();
    json!({
        "detected": true,
        "runners": scripts.detected_runners(),
        "primary_runner": scripts.primary_runner(),
        "verbs": verbs,
    })
}

/// The domain taxonomy `stella init` writes. Read straight off disk rather
/// than through `stella-cli`'s loader — this crate sits below it.
fn domains_section(root: &Path) -> Value {
    let path = root.join(".stella").join("domains.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return json!([]);
    };
    let Ok(parsed) = toml::from_str::<toml::Value>(&text) else {
        return json!([]);
    };
    let names: Vec<String> = parsed
        .get("domains")
        .and_then(|domains| domains.as_array())
        .map(|domains| {
            domains
                .iter()
                .filter_map(|domain| {
                    domain
                        .get("name")
                        .and_then(|name| name.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    json!(names)
}

/// One line summarizing where the code lives: top-level directories by
/// indexed-file count (largest first, ties by name), the remainder and any
/// root-level files collapsed to counts. Derived from the index's sorted
/// file list, so it is deterministic for a given index state — and it never
/// degrades: past [`MAX_TOP_LEVEL_DIRS`] the summary collapses instead of
/// disappearing, which is what keeps the injected map useful on a monorepo.
fn top_level_summary(files: &[String]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let mut dir_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut root_files = 0usize;
    for file in files {
        match file.split_once('/') {
            Some((dir, _)) => *dir_counts.entry(dir).or_default() += 1,
            None => root_files += 1,
        }
    }
    let mut dirs: Vec<(&str, usize)> = dir_counts.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let omitted = dirs.len().saturating_sub(MAX_TOP_LEVEL_DIRS);
    dirs.truncate(MAX_TOP_LEVEL_DIRS);

    let mut parts: Vec<String> = dirs
        .into_iter()
        .map(|(dir, count)| format!("{dir}/ ({count})"))
        .collect();
    if omitted > 0 {
        parts.push(format!("+{omitted} more dirs"));
    }
    if root_files > 0 {
        parts.push(format!("{root_files} at the root"));
    }
    Some(format!(
        "Layout ({} indexed files): {}",
        files.len(),
        parts.join(", ")
    ))
}

/// Extension → language label, matching the grammars the indexer actually
/// carries. Anything else contributes no label rather than a guess.
fn language_of(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?;
    Some(match extension {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "sql" => "sql",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A truly empty workspace (no source at all) still answers, with an
    /// index that built but found nothing — never an error that would send
    /// the agent back to the glob loop this replaces.
    #[test]
    fn an_empty_workspace_answers_with_a_built_but_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let out = build_overview(dir.path());

        // The tool builds the index on first use, so it exists — and reports
        // zero files honestly rather than pretending there is nothing to index.
        assert_eq!(out["index"]["built"], serde_json::json!(true));
        assert_eq!(out["index"]["files"], serde_json::json!(0));
    }

    /// With real source present, the first call builds the index and the
    /// overview reports it — no prior `stella init`.
    #[test]
    fn a_first_call_builds_the_index_and_reports_real_symbols() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn f() {}\npub struct S;\n").unwrap();

        let out = build_overview(dir.path());
        assert_eq!(out["index"]["built"], serde_json::json!(true));
        assert!(
            out["index"]["files"].as_u64().unwrap_or(0) >= 1,
            "the first call indexed the source: {}",
            out["index"]
        );
        assert!(
            out.get("code").is_some(),
            "a code section is present: {out}"
        );
    }

    #[test]
    fn build_scripts_are_reported_without_any_index() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let out = build_overview(dir.path());
        let scripts = &out["scripts"];
        assert_eq!(scripts["detected"], serde_json::json!(true));
        assert!(
            scripts["runners"]
                .as_array()
                .expect("runners")
                .iter()
                .any(|r| r == "cargo"),
            "cargo detected from the manifest alone: {scripts}"
        );
    }

    /// `domains.toml` is read straight off disk — this crate sits below the
    /// CLI that owns the loader, and the taxonomy is the agent's vocabulary
    /// for everything the graph tags.
    #[test]
    fn the_domain_taxonomy_is_read_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".stella")).unwrap();
        std::fs::write(
            dir.path().join(".stella").join("domains.toml"),
            "[[domains]]\nname = \"scheduling\"\n\n[[domains]]\nname = \"transport\"\n",
        )
        .unwrap();

        let out = build_overview(dir.path());
        assert_eq!(
            out["domains"],
            serde_json::json!(["scheduling", "transport"])
        );
    }

    #[test]
    fn orientation_block_is_none_without_an_index_and_never_builds_one() {
        // Read-only: it must not create an index during system-prompt
        // assembly, or it would block the first response on a build.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn f() {}\n").unwrap();
        assert!(render_orientation_block(dir.path()).is_none());
        assert!(
            !crate::graph::graph_db_path(dir.path()).exists(),
            "the read-only block must not build an index"
        );
    }

    #[test]
    fn orientation_block_reports_languages_and_entry_points_from_an_existing_index() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "mod helper;\npub fn main() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("helper.rs"), "pub fn help() {}\n").unwrap();
        // Build the index first (what the session background build / adapter
        // `stella init` does), THEN render read-only.
        let _ = build_overview(dir.path());

        let block = render_orientation_block(dir.path()).expect("an indexed workspace renders");
        assert!(block.contains("Project map"), "{block}");
        assert!(block.contains("Languages: rust"), "{block}");
        assert!(
            block.contains("Layout (2 indexed files): 2 at the root"),
            "the layout line summarizes where the code lives: {block}"
        );
        assert!(
            block.contains("Entry points:"),
            "a file nothing imports is an entry point: {block}"
        );
    }

    /// Issue #328 witness: past the old 400-file scan limit the map must stay
    /// useful — entry points are still derived (one SQL anti-join, no
    /// file-count cap) and the bounded layout line is still present, in both
    /// the tool output and the injected prompt block. Pre-#328, this fixture
    /// rendered no entry points anywhere and an "omitted" note in the tool.
    #[test]
    fn orientation_stays_useful_past_the_old_400_file_scan_limit() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(dir.path().join("main.rs"), "pub fn main() {}\n").unwrap();
        for i in 0..420 {
            std::fs::write(src.join(format!("f{i:03}.rs")), "pub fn f() {}\n").unwrap();
        }

        let out = build_overview(dir.path());
        assert!(
            out["index"]["files"].as_u64().unwrap_or(0) > 400,
            "the fixture must exceed the old scan limit: {}",
            out["index"]
        );
        let entry_points = out["code"]["entry_points"]
            .as_array()
            .expect("entry points stay a real list, never an 'omitted' note");
        assert_eq!(
            entry_points.first(),
            Some(&serde_json::json!("main.rs")),
            "shallowest first: {entry_points:?}"
        );

        let block = render_orientation_block(dir.path()).expect("an indexed workspace renders");
        assert!(block.contains("Entry points: main.rs"), "{block}");
        assert!(
            block.contains("Layout (421 indexed files): src/ (420), 1 at the root"),
            "{block}"
        );
    }

    #[test]
    fn a_malformed_taxonomy_degrades_to_empty_rather_than_failing_the_call() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".stella")).unwrap();
        std::fs::write(
            dir.path().join(".stella").join("domains.toml"),
            "not = [toml",
        )
        .unwrap();
        assert_eq!(build_overview(dir.path())["domains"], serde_json::json!([]));
    }
}
