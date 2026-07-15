//! The query → [`ContextFrame`] contract: frames carry mandatory human
//! citation labels (L-C4), `code-graph` provenance (task brief), valid scores,
//! and respect the query's token budget (L-C5).

use std::fs;

use stella_graph::{CodeGraph, ContextQuery, FrameKind, PROVIDER_ID};
use tempfile::TempDir;

fn query(text: &str, max_frames: u32, max_tokens: u32) -> ContextQuery {
    ContextQuery {
        goal: text.to_string(),
        query_text: Some(text.to_string()),
        embedding: None,
        kinds: Vec::new(),
        anchors: Vec::new(),
        max_frames,
        max_tokens,
        as_of: None,
    }
}

fn fixture() -> (TempDir, TempDir, CodeGraph) {
    let ws = TempDir::new().unwrap();
    let dbdir = TempDir::new().unwrap();
    fs::write(
        ws.path().join("driver.rs"),
        "pub fn run_turn() {\n    // drive one turn\n}\npub struct Engine;\n",
    )
    .unwrap();
    let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();
    graph.index_all().unwrap();
    (ws, dbdir, graph)
}

#[test]
fn definition_frames_have_human_citation_labels_and_provenance() {
    let (_ws, _db, graph) = fixture();
    let frames = graph.definitions("run_turn").unwrap();
    assert_eq!(frames.len(), 1);
    let frame = &frames[0];

    // L-C4: cited by a human label like `fn run_turn (driver.rs:1)`, never a
    // raw id (the id is structured but is not the citation surface).
    let label = frame
        .citation_label
        .as_deref()
        .expect("citation label is mandatory");
    assert!(
        label.starts_with("fn run_turn ("),
        "unexpected label: {label}"
    );
    assert!(
        label.contains("driver.rs:1"),
        "label should cite path:line: {label}"
    );
    assert_ne!(label, frame.id, "the citation label must not be the raw id");

    assert_eq!(frame.kind, FrameKind::Symbol);
    assert!(frame.has_valid_score());

    // Provenance carries the provider id `code-graph` and a file entry.
    assert!(
        frame
            .provenance
            .iter()
            .any(|p| p.by.as_deref() == Some(PROVIDER_ID)),
        "provenance must attribute the frame to the code-graph provider"
    );
    assert!(frame.provenance.iter().any(|p| p.kind == "file"));
}

#[test]
fn query_returns_frames_and_respects_the_budget() {
    let (_ws, _db, graph) = fixture();

    // Ample budget: the definition for `run_turn` comes back.
    let frames = graph
        .query(&query("look at run_turn and Engine", 20, 4000))
        .unwrap();
    assert!(!frames.is_empty());
    assert!(frames.iter().all(|f| f.citation_label.is_some()));
    assert!(frames.iter().all(|f| f.has_valid_score()));
    assert!(frames.iter().any(|f| {
        f.citation_label
            .as_deref()
            .unwrap()
            .starts_with("fn run_turn (")
    }));

    // Tight budget: total token cost never exceeds the cap.
    let tight = graph
        .query(&query("look at run_turn and Engine", 20, 8))
        .unwrap();
    let total: u32 = tight.iter().map(|f| f.token_cost).sum();
    assert!(
        total <= 8,
        "budget-pack must not exceed max_tokens; got {total}"
    );
}

#[test]
fn kind_filter_limits_returned_frames() {
    let (_ws, _db, graph) = fixture();
    let mut q = query("run_turn", 20, 4000);
    q.kinds = vec![FrameKind::Graph]; // no graph frames for a bare definition query
    let frames = graph.query(&q).unwrap();
    assert!(
        frames.iter().all(|f| f.kind == FrameKind::Graph),
        "kind filter must exclude non-matching frames"
    );
}

#[test]
fn file_neighborhood_returns_the_files_symbols() {
    // The structured neighborhood the deck's Graph tab renders (as opposed to
    // the prose frames above): the file's own symbols, with kinds and lines.
    let (_ws, _db, graph) = fixture();
    let hood = graph
        .file_neighborhood(std::path::Path::new("driver.rs"))
        .unwrap();
    assert_eq!(hood.file, "driver.rs");
    let names: Vec<&str> = hood.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"run_turn"), "symbols: {names:?}");
    assert!(names.contains(&"Engine"), "symbols: {names:?}");
    assert!(
        hood.symbols.iter().any(|s| s.kind == "function"),
        "a function symbol should carry the persisted `function` kind tag"
    );
}

#[test]
fn busiest_file_names_the_most_connected_file() {
    let (_ws, _db, graph) = fixture();
    // The one-file fixture makes driver.rs the only (thus busiest) candidate.
    assert_eq!(graph.busiest_file().unwrap().as_deref(), Some("driver.rs"));
}

#[test]
fn file_neighborhood_round_trips_through_serde() {
    // The type crosses the crate boundary (the deck consumes it), so it must
    // survive a serde round-trip like every other public wire type.
    let (_ws, _db, graph) = fixture();
    let hood = graph
        .file_neighborhood(std::path::Path::new("driver.rs"))
        .unwrap();
    let json = serde_json::to_string(&hood).unwrap();
    let back: stella_graph::FileNeighborhood = serde_json::from_str(&json).unwrap();
    assert_eq!(hood, back);
}
