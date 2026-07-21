//! Issue #272 — generated/minified bundles must never enter the index, and
//! therefore must never surface as recall candidates. These are witness
//! tests: every one of them fails on the pre-#272 code (no
//! `dist-standalone` deny-dir, no `.gitattributes`/`*.min.*`/content-shape
//! filter) and passes with it.

use std::fs;

use stella_graph::{CodeGraph, ContextQuery};
use tempfile::TempDir;

fn query(text: &str) -> ContextQuery {
    ContextQuery {
        goal: text.to_string(),
        query_text: Some(text.to_string()),
        embedding: None,
        kinds: Vec::new(),
        anchors: Vec::new(),
        max_frames: 50,
        max_tokens: 20_000,
        as_of: None,
    }
}

/// The evidence tree from issue #272: a real source file sitting next to a
/// checked-in minified bundle nested inside a monorepo app directory (not at
/// the workspace root — `apps/cli/dist-standalone/…`).
fn fixture_with_dist_standalone() -> (TempDir, TempDir, CodeGraph) {
    let ws = TempDir::new().unwrap();
    let dbdir = TempDir::new().unwrap();
    fs::create_dir_all(ws.path().join("apps/cli/dist-standalone")).unwrap();
    fs::write(
        ws.path().join("apps/cli/dist-standalone/oxagen.mjs"),
        format!("function refresh(){{{}}}\n", "console.log(1);".repeat(300)),
    )
    .unwrap();
    fs::create_dir_all(ws.path().join("apps/cli/src")).unwrap();
    fs::write(
        ws.path().join("apps/cli/src/main.rs"),
        "pub fn run_turn() {\n    // real, hand-written source\n}\n",
    )
    .unwrap();
    let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();
    graph.index_all().unwrap();
    (ws, dbdir, graph)
}

#[test]
fn dist_standalone_bundle_is_never_indexed() {
    let (_ws, _db, graph) = fixture_with_dist_standalone();
    let files = graph.all_files().unwrap();
    assert!(
        !files.iter().any(|f| f.contains("dist-standalone")),
        "dist-standalone must never be walked: {files:?}"
    );
    assert!(files.iter().any(|f| f.ends_with("main.rs")));
}

#[test]
fn dist_standalone_bundle_produces_no_recall_frames() {
    let (_ws, _db, graph) = fixture_with_dist_standalone();

    // Direct definition/reference lookups for the symbol the bundle defines.
    assert!(
        graph.definitions("refresh").unwrap().is_empty(),
        "no definition frame should cite the minified bundle"
    );
    assert!(
        graph.references("refresh").unwrap().is_empty(),
        "no reference frame should cite the minified bundle"
    );

    // The OCP `query` entrypoint — the actual path `stella-cli`'s recall
    // fans a turn's context request through (`stella-cli/src/ocp.rs`,
    // `GraphProvider::query`).
    let frames = graph.query(&query("why did refresh regress")).unwrap();
    assert!(
        frames.iter().all(|f| !f
            .citation_label
            .as_deref()
            .unwrap_or("")
            .contains("dist-standalone")),
        "no frame in a fused query result may cite the bundle: {frames:#?}"
    );
    assert!(
        frames
            .iter()
            .all(|f| !f.uri.as_deref().unwrap_or("").contains("dist-standalone")),
        "no frame's uri may point into the bundle: {frames:#?}"
    );
}

#[test]
fn a_minified_bundle_outside_any_denied_directory_is_still_excluded() {
    // Not every generated bundle lives under a conventionally-named
    // directory — the content-shape heuristic is the backstop for those.
    let ws = TempDir::new().unwrap();
    let dbdir = TempDir::new().unwrap();
    fs::write(
        ws.path().join("bundle.js"),
        format!("function refresh(){{{}}}\n", "x".repeat(3000)),
    )
    .unwrap();
    let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();
    graph.index_all().unwrap();

    assert!(graph.all_files().unwrap().is_empty());
    assert!(graph.definitions("refresh").unwrap().is_empty());
}

#[test]
fn gitattributes_linguist_generated_files_are_excluded_from_recall() {
    let ws = TempDir::new().unwrap();
    let dbdir = TempDir::new().unwrap();
    fs::write(
        ws.path().join(".gitattributes"),
        "codegen/*.ts linguist-generated=true\n",
    )
    .unwrap();
    fs::create_dir_all(ws.path().join("codegen")).unwrap();
    fs::write(
        ws.path().join("codegen/client.ts"),
        "export function generatedClient() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        ws.path().join("hand_written.ts"),
        "export function handWritten() { return 2; }\n",
    )
    .unwrap();
    let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();
    graph.index_all().unwrap();

    assert!(graph.definitions("generatedClient").unwrap().is_empty());
    assert!(!graph.definitions("handWritten").unwrap().is_empty());
}

/// An index built before this exclusion existed (simulated here by indexing
/// once, then adding the `.gitattributes` rule with the file's bytes
/// untouched) must not keep serving frames from it forever — the very next
/// `index_all` (what `stella init` re-run drives) has to retroactively drop
/// the stale row, not just decline to add new ones.
#[test]
fn a_previously_indexed_file_stops_surfacing_once_declared_generated() {
    let ws = TempDir::new().unwrap();
    let dbdir = TempDir::new().unwrap();
    fs::write(
        ws.path().join("legacy.js"),
        "function legacyThing() { return 1; }\n",
    )
    .unwrap();
    let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();
    graph.index_all().unwrap();
    assert!(
        !graph.definitions("legacyThing").unwrap().is_empty(),
        "sanity: indexed normally before any exclusion rule exists"
    );

    fs::write(
        ws.path().join(".gitattributes"),
        "legacy.js linguist-generated=true\n",
    )
    .unwrap();
    graph.index_all().unwrap();

    assert!(
        graph.definitions("legacyThing").unwrap().is_empty(),
        "a stale pre-fix row must not survive an unchanged-bytes re-index"
    );
}
