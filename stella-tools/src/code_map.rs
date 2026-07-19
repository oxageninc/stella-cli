//! Best-effort code-map footer for text-search results.
//!
//! `grep` and `glob` answer "where does this string/file live" — but the
//! agent's next question is almost always structural: what's defined there,
//! who imports it, where do I go next. The code graph already knows
//! (Field Manual Part 4: "code is a graph, not text"), yet agents keep
//! grepping instead of asking it. So the search tools bring the graph to the
//! agent: every successful result carries a compact per-file map (symbols,
//! import edges) drawn from `.stella/codegraph.db`. The session's FIRST
//! mapped search additionally carries a one-line pointer at `graph_query`
//! (the [`TipOnce`] latch, shared by grep and glob) — taught once, then out
//! of the way.
//!
//! Strictly additive and strictly best-effort: no index, an unopenable
//! store, or files the graph doesn't know all mean "no footer", never an
//! error and never a changed search result. Open → query → shutdown per
//! call, the same no-held-handles discipline as `graph_query`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Files mapped per search result — enough to orient, small enough that the
/// footer never rivals the matches themselves.
const MAX_FILES: usize = 5;
/// Symbols listed per file before eliding the tail.
const MAX_SYMBOLS: usize = 6;
/// Importer paths listed per file before collapsing to a count.
const MAX_IMPORTERS: usize = 3;

/// Once-per-session latch for the footer's `graph_query` tip line. The
/// registry constructs one and hands clones to `grep` AND `glob`, so the
/// tip appears on the session's first mapped search — whichever tool runs
/// it — and never again; repeating it on every result is teaching that
/// decays into noise. Consumed only when a footer actually renders: a
/// search before the index exists doesn't burn the one showing.
#[derive(Clone, Default)]
pub struct TipOnce(Arc<AtomicBool>);

impl TipOnce {
    /// True exactly once, on the first call — atomic, so concurrent
    /// read-only searches race to exactly one tip.
    fn take(&self) -> bool {
        !self.0.swap(true, Ordering::Relaxed)
    }
}

/// Render the code-map footer for the files a search touched, or `None`
/// when there is nothing to say (no index, store unopenable, or none of the
/// candidates are in the graph). `candidates` may repeat and may mix
/// absolute and root-relative paths — both resolve; order of first
/// appearance is kept, so the map follows the result's own ranking.
pub fn for_files<'a, I>(root: &Path, candidates: I, tip: &TipOnce) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    if !crate::graph::graph_available(root) {
        return None;
    }
    let graph = stella_graph::CodeGraph::open(root, &crate::graph::graph_db_path(root)).ok()?;

    // Two dedup layers: `seen` skips repeated candidate strings cheaply;
    // `mapped` keys on the graph-resolved path, so an absolute and a
    // relative spelling of the same file still collapse to one entry.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut mapped: HashSet<String> = HashSet::new();
    let mut lines: Vec<String> = Vec::new();
    for candidate in candidates {
        if lines.len() == MAX_FILES {
            break;
        }
        if candidate.is_empty() || !seen.insert(candidate) {
            continue;
        }
        let Ok(hood) = graph.file_neighborhood(Path::new(candidate)) else {
            continue;
        };
        if let Some(line) = render_file(&hood)
            && mapped.insert(hood.file)
        {
            lines.push(line);
        }
    }
    graph.shutdown();

    if lines.is_empty() {
        return None;
    }
    let mut out = String::from("\n\ncode map (from the code graph):\n");
    out.push_str(&lines.join("\n"));
    if tip.take() {
        out.push_str(
            "\ntip: graph_query answers symbol/dependency questions directly \
             (op=definitions|references|imports|importers|neighbors).",
        );
    }
    Some(out)
}

/// One file's line, or `None` when the graph has nothing on it (unindexed
/// language, or a path the index doesn't know) — an empty entry would be
/// noise, not a map.
fn render_file(hood: &stella_graph::FileNeighborhood) -> Option<String> {
    // Columns are per-table detail (SQL files index one symbol per column);
    // at file granularity they drown the tables they belong to.
    let symbols: Vec<&stella_graph::NeighborhoodSymbol> = hood
        .symbols
        .iter()
        .filter(|s| s.kind != "column")
        .collect();
    if symbols.is_empty() && hood.imports.is_empty() && hood.importers.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    if !symbols.is_empty() {
        let mut listed: Vec<String> = symbols
            .iter()
            .take(MAX_SYMBOLS)
            .map(|s| format!("{} {}:{}", s.kind, s.name, s.start_line))
            .collect();
        if symbols.len() > MAX_SYMBOLS {
            listed.push(format!("+{} more", symbols.len() - MAX_SYMBOLS));
        }
        parts.push(listed.join(", "));
    }
    if !hood.imports.is_empty() {
        parts.push(format!("imports {}", hood.imports.len()));
    }
    if !hood.importers.is_empty() {
        let mut listed: Vec<&str> = hood
            .importers
            .iter()
            .take(MAX_IMPORTERS)
            .map(String::as_str)
            .collect();
        let extra = hood.importers.len().saturating_sub(MAX_IMPORTERS);
        let mut s = format!("imported by {}", listed.join(", "));
        if extra > 0 {
            listed.truncate(MAX_IMPORTERS);
            s.push_str(&format!(" (+{extra} more)"));
        }
        parts.push(s);
    }
    Some(format!("- {} — {}", hood.file, parts.join(" · ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace with a built index: `lib.rs` defines `greet`, `main.rs`
    /// imports nothing resolvable — the same minimal fixture the graph
    /// tool's tests use.
    fn indexed_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn greet() -> &'static str { \"hi\" }\npub struct Greeter;\n",
        )
        .expect("write source");
        let db = crate::graph::graph_db_path(dir.path());
        std::fs::create_dir_all(db.parent().expect("parent")).expect("mkdir");
        let graph = stella_graph::CodeGraph::open(dir.path(), &db).expect("open graph");
        graph.index_all().expect("index");
        graph.shutdown();
        dir
    }

    #[test]
    fn no_index_means_no_footer() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("lib.rs"), "pub fn f() {}\n").expect("write");
        assert_eq!(for_files(dir.path(), ["lib.rs"], &TipOnce::default()), None);
    }

    #[test]
    fn maps_an_indexed_file_with_symbols_and_a_graph_query_tip() {
        let dir = indexed_workspace();
        let map = for_files(dir.path(), ["lib.rs"], &TipOnce::default())
            .expect("footer for an indexed file");
        assert!(map.contains("code map"), "labeled: {map}");
        assert!(map.contains("lib.rs"), "names the file: {map}");
        assert!(map.contains("function greet:1"), "cites the symbol: {map}");
        assert!(map.contains("struct Greeter:2"), "cites the struct: {map}");
        assert!(map.contains("graph_query"), "nudges at the tool: {map}");
    }

    #[test]
    fn absolute_and_relative_spellings_dedupe_to_one_entry() {
        let dir = indexed_workspace();
        let abs = dir
            .path()
            .canonicalize()
            .expect("canon")
            .join("lib.rs")
            .to_string_lossy()
            .into_owned();
        let map =
            for_files(dir.path(), [abs.as_str(), "lib.rs"], &TipOnce::default()).expect("footer");
        assert_eq!(
            map.matches("function greet:1").count(),
            1,
            "one entry, not one per spelling: {map}"
        );
    }

    #[test]
    fn unknown_files_are_skipped_not_rendered_empty() {
        let dir = indexed_workspace();
        assert_eq!(
            for_files(
                dir.path(),
                ["README.md", "no/such/file.rs"],
                &TipOnce::default()
            ),
            None,
            "files the graph doesn't know produce no footer at all"
        );
    }

    #[test]
    fn the_tip_renders_once_then_footers_stay_map_only() {
        let dir = indexed_workspace();
        let tip = TipOnce::default();
        let first = for_files(dir.path(), ["lib.rs"], &tip).expect("first footer");
        assert!(first.contains("tip: graph_query"), "teaches once: {first}");
        let second = for_files(dir.path(), ["lib.rs"], &tip).expect("second footer");
        assert!(second.contains("code map"), "the map stays: {second}");
        assert!(
            !second.contains("tip: graph_query"),
            "already taught: {second}"
        );
    }

    /// The one showing must land where it can teach: a search that renders
    /// no footer (nothing mapped) leaves the latch unspent.
    #[test]
    fn a_search_without_a_footer_does_not_consume_the_tip() {
        let dir = indexed_workspace();
        let tip = TipOnce::default();
        assert_eq!(for_files(dir.path(), ["README.md"], &tip), None);
        let mapped = for_files(dir.path(), ["lib.rs"], &tip).expect("footer");
        assert!(
            mapped.contains("tip: graph_query"),
            "still unspent after a footer-less search: {mapped}"
        );
    }

    /// The session-level contract: grep and glob share one latch through the
    /// registry, so whichever searches first carries the tip and the other
    /// never repeats it.
    #[tokio::test]
    async fn the_tip_shows_once_per_session_across_grep_and_glob() {
        let dir = indexed_workspace();
        let reg = crate::ToolRegistry::with_issue_backend(dir.path().to_path_buf(), None);
        let first = reg
            .execute("grep", &serde_json::json!({"pattern": "greet"}))
            .await;
        let second = reg
            .execute("glob", &serde_json::json!({"pattern": "*.rs"}))
            .await;
        match (first, second) {
            (
                stella_protocol::tool::ToolOutput::Ok { content: grep_out },
                stella_protocol::tool::ToolOutput::Ok { content: glob_out },
            ) => {
                assert!(
                    grep_out.contains("tip: graph_query"),
                    "the session's first mapped search teaches: {grep_out}"
                );
                assert!(glob_out.contains("code map"), "later maps stay: {glob_out}");
                assert!(
                    !glob_out.contains("tip: graph_query"),
                    "taught once per session, across tools: {glob_out}"
                );
            }
            other => panic!("expected two ok results, got: {other:?}"),
        }
    }

    #[test]
    fn the_footer_is_bounded_to_max_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..(MAX_FILES + 3) {
            std::fs::write(
                dir.path().join(format!("m{i}.rs")),
                format!("pub fn f{i}() {{}}\n"),
            )
            .expect("write");
        }
        let db = crate::graph::graph_db_path(dir.path());
        std::fs::create_dir_all(db.parent().expect("parent")).expect("mkdir");
        let graph = stella_graph::CodeGraph::open(dir.path(), &db).expect("open");
        graph.index_all().expect("index");
        graph.shutdown();

        let names: Vec<String> = (0..(MAX_FILES + 3)).map(|i| format!("m{i}.rs")).collect();
        let map = for_files(
            dir.path(),
            names.iter().map(String::as_str),
            &TipOnce::default(),
        )
        .expect("footer");
        assert_eq!(
            map.matches("- m").count(),
            MAX_FILES,
            "caps at {MAX_FILES}: {map}"
        );
    }
}
