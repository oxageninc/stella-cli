//! Best-effort code-map footer for text-search results.
//!
//! `grep` and `glob` answer "where does this string/file live" — but the
//! agent's next question is almost always structural: what's defined there,
//! who imports it, where do I go next. The code graph already knows
//! (Field Manual Part 4: "code is a graph, not text"), yet agents keep
//! grepping instead of asking it. So the search tools bring the graph to the
//! agent: every successful result carries a compact per-file map (symbols,
//! import edges) drawn from `.stella/private/codegraph.db`. A search whose *pattern is
//! symbol-shaped* — a definition/reference hunt graph_query serves better —
//! additionally carries a one-line pointer at the tool ([`is_symbol_shaped`]
//! decides). Not once-per-session: the nudge fires on every symbol-shaped
//! search, because that is exactly the moment it corrects, and stays silent on
//! free-text scans (grep's own job) so it never decays into noise.
//!
//! Strictly additive and strictly best-effort: no index, an unopenable
//! store, or files the graph doesn't know all mean "no footer", never an
//! error and never a changed search result. Open → query → shutdown per
//! call, the same no-held-handles discipline as `graph_query`.

use std::collections::HashSet;
use std::path::Path;

/// Files mapped per search result — enough to orient, small enough that the
/// footer never rivals the matches themselves.
const MAX_FILES: usize = 5;
/// Symbols listed per file before eliding the tail.
const MAX_SYMBOLS: usize = 6;
/// Importer paths listed per file before collapsing to a count.
const MAX_IMPORTERS: usize = 3;

/// The one-line pointer at `graph_query`, appended to a footer (and to a
/// bash result carrying a symbol-shaped `grep`/`rg`) when the search looks
/// like a definition/reference hunt. Shared so every surface teaches the
/// same tool the same way.
pub const GRAPH_QUERY_TIP: &str = "tip: this looks like a symbol/dependency lookup — graph_query answers it \
     directly (op=definitions|references|imports|importers|neighbors), precise \
     and cheaper than a text scan.";

/// Declaration keywords that, leading a query, mark it a definition hunt
/// (`struct Foo`, `pub fn bar`, `class Baz`, `def qux`). Cross-language on
/// purpose — this is a nudge heuristic, not a parser.
const DECL_KEYWORDS: &[&str] = &[
    "struct",
    "enum",
    "trait",
    "impl",
    "fn",
    "func",
    "function",
    "class",
    "def",
    "interface",
    "type",
    "const",
    "static",
    "mod",
    "module",
    "pub",
    "public",
    "private",
    "protected",
    "use",
    "import",
    "let",
    "var",
    "val",
    "namespace",
];

/// Does this regex look like the agent hunting for where a symbol is defined
/// or used — the case `graph_query` serves better than a text scan? Applied
/// to every `|`-alternation branch (so a multi-symbol `struct A|struct B`
/// still counts), each branch must be one of two shapes:
///   * a lone identifier or `::`/`.`-path — `ReadOnlyTools`, `stella::graph::Foo`
///   * a declaration-keyword-led identifier — `struct Foo`, `pub fn bar`
///
/// A single leading `^` / trailing `$` anchor is tolerated (the everyday
/// `^pub fn` definition search); any other regex machinery — quantifiers,
/// char classes, wildcards, embedded metacharacters — means a genuine text
/// pattern, grep's own job, and does NOT trip the nudge. Deliberately errs
/// toward missing over false-firing; a stray tip line is cheap, but nudging
/// on a real text scan is the noise the once-latch existed to avoid.
pub fn is_symbol_shaped(pattern: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    // `\|` (escaped, from a shell-quoted rg) and `|` (raw regex) both mean
    // alternation here; normalize, then every branch must be a symbol.
    let normalized = pattern.replace("\\|", "|");
    let mut branches = 0usize;
    for branch in normalized.split('|') {
        branches += 1;
        if !branch_is_symbol(branch) {
            return false;
        }
    }
    branches > 0
}

/// One alternation branch: an optional run of declaration keywords followed
/// by a single identifier/path. `^`/`$` anchors are stripped first.
fn branch_is_symbol(branch: &str) -> bool {
    let branch = branch
        .trim()
        .trim_start_matches('^')
        .trim_end_matches('$')
        .trim();
    let tokens: Vec<&str> = branch.split_whitespace().collect();
    let Some((ident, keywords)) = tokens.split_last() else {
        return false;
    };
    keywords
        .iter()
        .all(|k| DECL_KEYWORDS.contains(&k.to_ascii_lowercase().as_str()))
        && is_identifier_or_path(ident)
}

/// A bare identifier or a `::`/`.`-separated path of them — how a symbol name
/// is written. Each segment starts alphabetic/underscore and is otherwise
/// alphanumeric/underscore; the whole must contain at least one letter, so a
/// bare number never counts as a symbol.
fn is_identifier_or_path(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.split("::").flat_map(|p| p.split('.')).all(|seg| {
        let mut chars = seg.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
            && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }) && s.chars().any(|c| c.is_ascii_alphabetic())
}

/// Render the code-map footer for the files a search touched, or `None`
/// when there is nothing to say (no index, store unopenable, or none of the
/// candidates are in the graph). `candidates` may repeat and may mix
/// absolute and root-relative paths — both resolve; order of first
/// appearance is kept, so the map follows the result's own ranking.
pub fn for_files<'a, I>(root: &Path, candidates: I, show_tip: bool) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    if !matches!(crate::graph::graph_available(root), Ok(true)) {
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
    if show_tip {
        out.push('\n');
        out.push_str(GRAPH_QUERY_TIP);
    }
    Some(out)
}

/// One file's line, or `None` when the graph has nothing on it (unindexed
/// language, or a path the index doesn't know) — an empty entry would be
/// noise, not a map.
fn render_file(hood: &stella_graph::FileNeighborhood) -> Option<String> {
    // Columns are per-table detail (SQL files index one symbol per column);
    // at file granularity they drown the tables they belong to.
    let symbols: Vec<&stella_graph::NeighborhoodSymbol> =
        hood.symbols.iter().filter(|s| s.kind != "column").collect();
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
        assert_eq!(for_files(dir.path(), ["lib.rs"], true), None);
    }

    #[test]
    fn maps_an_indexed_file_with_symbols() {
        let dir = indexed_workspace();
        let map = for_files(dir.path(), ["lib.rs"], false).expect("footer for an indexed file");
        assert!(map.contains("code map"), "labeled: {map}");
        assert!(map.contains("lib.rs"), "names the file: {map}");
        assert!(map.contains("function greet:1"), "cites the symbol: {map}");
        assert!(map.contains("struct Greeter:2"), "cites the struct: {map}");
    }

    /// `show_tip` is the only thing that gates the pointer — the map itself is
    /// unconditional. A symbol-shaped search (tip on) teaches; a free-text one
    /// (tip off) gets the map bare.
    #[test]
    fn the_tip_follows_show_tip_not_a_latch() {
        let dir = indexed_workspace();
        let with = for_files(dir.path(), ["lib.rs"], true).expect("footer");
        assert!(with.contains("graph_query"), "tip on nudges: {with}");
        // Persistent, not once-per-session: a second symbol-shaped search
        // teaches again — no shared latch survives between calls.
        let again = for_files(dir.path(), ["lib.rs"], true).expect("footer");
        assert!(again.contains("graph_query"), "tip fires again: {again}");
        let without = for_files(dir.path(), ["lib.rs"], false).expect("footer");
        assert!(without.contains("code map"), "map stays: {without}");
        assert!(
            !without.contains("graph_query"),
            "tip off stays quiet: {without}"
        );
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
        let map = for_files(dir.path(), [abs.as_str(), "lib.rs"], false).expect("footer");
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
            for_files(dir.path(), ["README.md", "no/such/file.rs"], true),
            None,
            "files the graph doesn't know produce no footer at all"
        );
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
        let map = for_files(dir.path(), names.iter().map(String::as_str), false).expect("footer");
        assert_eq!(
            map.matches("- m").count(),
            MAX_FILES,
            "caps at {MAX_FILES}: {map}"
        );
    }

    #[test]
    fn symbol_shaped_accepts_definition_hunts() {
        // Lone identifiers and paths — the "where is X / who calls X" case.
        for p in [
            "ReadOnlyTools",
            "DeckProviderResolver",
            "stella::graph::CodeGraph",
            "graph.file_neighborhood",
        ] {
            assert!(is_symbol_shaped(p), "identifier should count: {p}");
        }
        // Declaration-keyword-led, including anchored and multi-symbol.
        for p in [
            "struct DeckProviderResolver",
            "pub fn run_goal",
            "pub mod ports",
            "^impl Tool",
            "^pub fn resolve$",
            "struct GoalConfig|fn run_goal|GoalOutcome",
        ] {
            assert!(is_symbol_shaped(p), "declaration hunt should count: {p}");
        }
    }

    #[test]
    fn symbol_shaped_rejects_text_patterns() {
        for p in [
            "",
            "   ",
            "hello world",
            "foo.*bar",
            "[a-z]+",
            "TODO:",
            "->",
            "unwrap\\(\\)",
            "0x1234",
            "a|.*b",
            "let mut x =",
        ] {
            assert!(!is_symbol_shaped(p), "text pattern should not count: {p:?}");
        }
    }
}
