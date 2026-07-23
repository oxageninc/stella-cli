//! Plain, backend-agnostic types for the code-graph inspector.
//!
//! `stella-tui` does **not** depend on `stella-graph`. The caller (the CLI,
//! which already owns a `CodeGraph`) queries `CodeGraph::neighbors(file)` and
//! converts the result into a [`GraphSnapshot`] it hands the deck. This keeps
//! the TUI decoupled — it renders data given to it, never reaching into a
//! backend — and lets the scenario driver synthesize a snapshot for demos.
//!
//! The snapshot is one of the two labeled **out-of-band read-models**: it is
//! not folded from `AgentEvent`s (a graph's structure isn't in the per-session
//! event stream).

/// A queried neighborhood of the code graph, ready to draw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphSnapshot {
    /// The symbol/file the neighborhood is centered on (human label).
    pub focus: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Every indexed code file (root-relative, sorted) — the Graph tab's file
    /// picker lists these so any file can be re-rooted, not just the busiest
    /// one the caller seeds `focus` with. Rides along on the snapshot because
    /// `stella-tui` cannot reach the graph store itself (it renders data given
    /// to it); the caller fills it from [`stella_graph::CodeGraph::all_files`].
    pub files: Vec<String>,
}

/// One node — a symbol or file. Cited by human label, never a raw UUID (L-C4).
#[derive(Clone, Debug, PartialEq)]
pub struct GraphNode {
    /// Human, inspectable label (the primary on-screen identifier).
    pub label: String,
    /// e.g. `"function"`, `"struct"`, `"trait"`, `"file"`, `"module"`.
    pub kind: String,
    /// Optional source location for the detail panel (`"src/x.rs:42"`).
    pub location: Option<String>,
}

/// A directed edge between two [`GraphSnapshot::nodes`] by index.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphEdge {
    pub from: usize,
    pub to: usize,
    /// e.g. `"imports"`, `"calls"`, `"defines"`, `"references"`.
    pub kind: String,
}

impl GraphSnapshot {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Degree (edge count touching) of a node index — handy for sizing.
    pub fn degree(&self, node: usize) -> usize {
        self.edges
            .iter()
            .filter(|e| e.from == node || e.to == node)
            .count()
    }

    /// The [`files`](Self::files) that match a picker query — a case-insensitive
    /// substring test, preserving the sorted file order. An empty (or
    /// whitespace-only) query matches every file, so the picker opens on the
    /// full list. Both the picker's key handler (for selection bounds) and its
    /// renderer route through this one function so the highlighted row and the
    /// selected path can never disagree.
    pub fn matching_files(&self, query: &str) -> Vec<&str> {
        let needle = query.trim().to_lowercase();
        self.files
            .iter()
            .map(String::as_str)
            .filter(|f| needle.is_empty() || f.to_lowercase().contains(&needle))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_files(files: &[&str]) -> GraphSnapshot {
        GraphSnapshot {
            focus: "root".into(),
            nodes: Vec::new(),
            edges: Vec::new(),
            files: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn an_empty_query_matches_every_file_in_order() {
        let snap = snapshot_with_files(&["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert_eq!(
            snap.matching_files(""),
            vec!["src/a.rs", "src/b.rs", "src/c.rs"]
        );
        // Whitespace-only is treated as empty, not as a literal space search.
        assert_eq!(
            snap.matching_files("   "),
            vec!["src/a.rs", "src/b.rs", "src/c.rs"]
        );
    }

    #[test]
    fn a_query_narrows_case_insensitively_by_substring() {
        let snap = snapshot_with_files(&["src/Auth.rs", "src/db/pool.rs", "README.md"]);
        // Case-insensitive.
        assert_eq!(snap.matching_files("auth"), vec!["src/Auth.rs"]);
        // Matches anywhere in the path, not just the basename.
        assert_eq!(snap.matching_files("db/"), vec!["src/db/pool.rs"]);
        // No match yields an empty list (the picker then shows its empty hint).
        assert!(snap.matching_files("zzz").is_empty());
    }
}
