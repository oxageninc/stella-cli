//! Throwaway diagnostic: print the exact native tool schemas the registry
//! advertises for a given workspace root — i.e. what feeds the model via the
//! live `McpToolSet.schemas()` delegation. Answers "is `graph_query` in the
//! payload for this repo?" definitively.
//!
//! Run: cargo run -p stella-tools --example dump_tools -- <workspace_root>

use stella_tools::registry::ToolRegistry;

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let root = root.canonicalize().unwrap_or(root);

    let cg = root.join(".stella").join("codegraph.db");
    eprintln!("workspace root       : {}", root.display());
    eprintln!("codegraph.db exists  : {}", cg.exists());
    eprintln!("graph_available()    : {}", stella_tools::graph::graph_available(&root));

    let reg = ToolRegistry::new(root);
    let mut names: Vec<String> = reg.schemas().into_iter().map(|s| s.name).collect();
    names.sort();
    eprintln!("--- advertised native tools ({}) ---", names.len());
    for n in &names {
        eprintln!("{}{}", if n == "graph_query" { ">>> " } else { "    " }, n);
    }
    eprintln!(
        "graph_query advertised: {}",
        names.iter().any(|n| n == "graph_query")
    );
}
