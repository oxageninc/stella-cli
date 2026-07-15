//! `code_graph` — query the workspace's code graph (tree-sitter symbols +
//! import edges, indexed by `stella init` into `.stella/codegraph.db`).
//!
//! This is the runtime retrieval surface of `stella-graph`: instead of
//! grepping for a symbol, the agent asks the graph — where is `run_turn`
//! defined, who imports `src/auth.rs`, what is this file's neighborhood
//! (Field Manual Part 4: "code is a graph, not text"). Registered only when
//! the index exists, mirroring the issue tools' conditional registration —
//! no index, no dead schema burning tokens.
//!
//! Read-only by construction: the tool opens the store, answers, and shuts
//! down per call (the same open/shutdown discipline as the schema gate),
//! so it never holds the SQLite file or a file watcher across turns.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::registry::Tool;

/// Frames rendered per query before eliding the tail — enough for every
/// realistic definition/importer list while bounding a pathological one
/// (e.g. `references` on a one-letter name) to a sane prompt size.
const MAX_FRAMES: usize = 30;

/// The index location `stella init` writes and the schema gate reads.
pub fn graph_db_path(root: &Path) -> PathBuf {
    root.join(".stella").join("codegraph.db")
}

/// Whether the workspace has an index — the registration condition.
pub fn graph_available(root: &Path) -> bool {
    graph_db_path(root).exists()
}

pub struct CodeGraphQuery;

#[async_trait]
impl Tool for CodeGraphQuery {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "code_graph".into(),
            description: "Query the indexed code graph instead of grepping: where a symbol is \
                          defined or referenced, what a file imports, which files import it, or \
                          a file's full graph neighborhood. Cheaper and more precise than \
                          grep for symbol/dependency questions. Re-index with `stella init`."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["definitions", "references", "imports", "importers", "neighbors"],
                        "description": "definitions/references take a symbol name; \
                                        imports/importers/neighbors take a workspace-relative \
                                        file path"
                    },
                    "target": {
                        "type": "string",
                        "description": "The symbol name or file path to query"
                    }
                },
                "required": ["op", "target"]
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let op = input.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let target = input.get("target").and_then(|v| v.as_str()).unwrap_or("");
        if target.is_empty() {
            return ToolOutput::Error {
                message: "`target` is required: a symbol name for definitions/references, a \
                          file path for imports/importers/neighbors"
                    .into(),
            };
        }
        run_query(root, op, target)
    }
}

/// Open → query → shutdown, entirely synchronous underneath (SQLite reads).
/// Shared by the tool and the `stella graph` subcommand so both render the
/// exact same frames.
pub fn run_query(root: &Path, op: &str, target: &str) -> ToolOutput {
    let db_path = graph_db_path(root);
    if !db_path.exists() {
        return ToolOutput::Error {
            message: "no code graph index — run `stella init` to build .stella/codegraph.db".into(),
        };
    }
    let graph = match stella_graph::CodeGraph::open(root, &db_path) {
        Ok(g) => g,
        Err(e) => {
            return ToolOutput::Error {
                message: format!("could not open the code graph: {e}"),
            };
        }
    };
    let result = match op {
        "definitions" => graph.definitions(target),
        "references" => graph.references(target),
        "imports" => graph.imports_of(Path::new(target)),
        "importers" => graph.importers_of(Path::new(target)),
        "neighbors" => graph.neighbors(Path::new(target)),
        other => {
            graph.shutdown();
            return ToolOutput::Error {
                message: format!(
                    "unknown op `{other}` — expected definitions, references, imports, \
                     importers, or neighbors"
                ),
            };
        }
    };
    graph.shutdown();

    match result {
        // Importer edges only exist where import resolution succeeds
        // (relative TS/JS and Python paths). Rust `use` paths and bare
        // package specifiers are indexed unresolved, so an empty importers
        // answer for such a file is a capability gap, not a stale index —
        // saying "re-index" would send the agent down a useless `stella
        // init` retry.
        Ok(frames) if frames.is_empty() && op == "importers" && !target.ends_with(".py") => {
            ToolOutput::Ok {
                content: format!(
                    "no importers found for `{target}` — importer edges exist only where \
                     import resolution succeeds (relative TS/JS/Python imports); Rust `use` \
                     paths are indexed unresolved. Try `references` on the module name instead."
                ),
            }
        }
        Ok(frames) if frames.is_empty() => ToolOutput::Ok {
            content: format!(
                "no {op} found for `{target}` (index may be stale — `stella init` re-indexes)"
            ),
        },
        Ok(frames) => ToolOutput::Ok {
            content: render_frames(&frames),
        },
        Err(e) => ToolOutput::Error {
            message: format!("code-graph query failed: {e}"),
        },
    }
}

/// Render frames as cited entries — always the human citation label, never a
/// raw id (stella-graph L-C4) — eliding the tail loudly, never silently.
fn render_frames(frames: &[stella_graph::ContextFrame]) -> String {
    let mut lines: Vec<String> = frames
        .iter()
        .take(MAX_FRAMES)
        .map(|f| {
            let label = f.citation_label.as_deref().unwrap_or(&f.title);
            let content = f.content.trim();
            if content.is_empty() {
                format!("- {label}")
            } else {
                format!("- {label}\n  {}", content.replace('\n', "\n  "))
            }
        })
        .collect();
    if frames.len() > MAX_FRAMES {
        lines.push(format!(
            "… (+{} more — narrow the query)",
            frames.len() - MAX_FRAMES
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn greet() -> &'static str { \"hi\" }\n",
        )
        .expect("write source");
        std::fs::write(dir.path().join("main.rs"), "mod lib;\nfn main() {}\n")
            .expect("write source");
        let db = graph_db_path(dir.path());
        std::fs::create_dir_all(db.parent().expect("parent")).expect("mkdir");
        let graph = stella_graph::CodeGraph::open(dir.path(), &db).expect("open graph");
        graph.index_all().expect("index");
        graph.shutdown();
        dir
    }

    #[test]
    fn schema_is_read_only_and_named() {
        let schema = CodeGraphQuery.schema();
        assert_eq!(schema.name, "code_graph");
        assert!(schema.read_only);
    }

    #[tokio::test]
    async fn missing_index_is_a_named_error_pointing_at_init() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = CodeGraphQuery
            .execute(
                &serde_json::json!({"op": "definitions", "target": "greet"}),
                dir.path(),
            )
            .await;
        match out {
            ToolOutput::Error { message } => assert!(message.contains("stella init")),
            ToolOutput::Ok { .. } => panic!("expected an error without an index"),
        }
    }

    #[tokio::test]
    async fn definitions_finds_an_indexed_symbol_with_a_citation() {
        let dir = indexed_workspace();
        let out = CodeGraphQuery
            .execute(
                &serde_json::json!({"op": "definitions", "target": "greet"}),
                dir.path(),
            )
            .await;
        match out {
            ToolOutput::Ok { content } => {
                assert!(content.contains("greet"), "cites the symbol: {content}")
            }
            ToolOutput::Error { message } => panic!("expected frames, got: {message}"),
        }
    }

    #[tokio::test]
    async fn unknown_op_and_missing_target_are_named_errors() {
        let dir = indexed_workspace();
        let bad_op = CodeGraphQuery
            .execute(
                &serde_json::json!({"op": "callers", "target": "greet"}),
                dir.path(),
            )
            .await;
        assert!(matches!(bad_op, ToolOutput::Error { .. }));
        let no_target = CodeGraphQuery
            .execute(&serde_json::json!({"op": "definitions"}), dir.path())
            .await;
        assert!(matches!(no_target, ToolOutput::Error { .. }));
    }

    #[tokio::test]
    async fn empty_result_is_ok_with_a_stale_index_hint() {
        let dir = indexed_workspace();
        let out = CodeGraphQuery
            .execute(
                &serde_json::json!({"op": "definitions", "target": "no_such_symbol_xyz"}),
                dir.path(),
            )
            .await;
        match out {
            ToolOutput::Ok { content } => assert!(content.contains("no definitions")),
            ToolOutput::Error { message } => panic!("empty is not an error: {message}"),
        }
    }
}
