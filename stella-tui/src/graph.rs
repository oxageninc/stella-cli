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
//! event stream). See `COMMAND_DECK_DESIGN.md` → "The purity boundary".

/// A queried neighborhood of the code graph, ready to draw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphSnapshot {
    /// The symbol/file the neighborhood is centered on (human label).
    pub focus: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
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
}
