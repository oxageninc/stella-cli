//! `glob` — find files matching a glob pattern. Shells to `fd` when present,
//! falls back to `find` otherwise.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};
use tokio::process::Command;

use crate::registry::Tool;

const MAX_RESULTS: usize = 500;

pub struct Glob {
    /// Code-map footer state; `None` disables the footer entirely (the
    /// embedded sweeps in `gather_context` — see [`crate::grep::Grep`]).
    code_map: Option<crate::code_map::TipOnce>,
}

impl Glob {
    /// The model-facing tool: footer on, `graph_query` tip latch shared
    /// with `grep` (the registry passes both clones of one
    /// [`crate::code_map::TipOnce`]).
    pub fn with_code_map(tip: crate::code_map::TipOnce) -> Self {
        Self {
            code_map: Some(tip),
        }
    }

    /// Footer-less instance for embedded use.
    pub fn bare() -> Self {
        Self { code_map: None }
    }
}

impl Default for Glob {
    /// Footer on with a private latch — the standalone shape tests use.
    fn default() -> Self {
        Self::with_code_map(crate::code_map::TipOnce::default())
    }
}

#[async_trait]
impl Tool for Glob {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "glob".into(),
            description: "Find files matching a glob pattern. Returns relative paths from \
                          workspace root. When the code-graph index exists, results carry a \
                          code-map footer (matched files' symbols and import edges)."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern (e.g. **/*.rs)" },
                    "path": { "type": "string", "description": "Subdirectory to search (default: workspace root)" }
                },
                "required": ["pattern"]
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let pattern = match input.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolOutput::Error {
                    message: "missing required field `pattern`".into(),
                };
            }
        };
        let search_path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        // Confine `path` to the workspace root — `Path::join` would otherwise
        // let an absolute or `../..` path escape it. `.`/empty resolve to the
        // root itself, so existing default-scoped calls keep working.
        let search_dir = match crate::resolve_within_root(root, search_path) {
            Some(p) => p,
            None => {
                return ToolOutput::Error {
                    message: format!("path `{search_path}` escapes workspace root"),
                };
            }
        };
        // Results are rendered relative to the (canonical) workspace root, per
        // this tool's advertised contract.
        let canon_root = root.canonicalize().ok();

        // Try fd first — fast, respects .gitignore by default.
        // `--glob` tells fd to interpret the pattern as a glob, not a regex,
        // so `*.rs` matches the same way as `find -name "*.rs"`.
        let mut fd = Command::new("fd");
        fd.arg("--glob");
        fd.arg("--type").arg("f");
        fd.arg("--color").arg("never");
        fd.arg("--max-results").arg(MAX_RESULTS.to_string());
        fd.arg("--").arg(pattern).arg(&search_dir);
        fd.stdout(std::process::Stdio::piped());
        fd.stderr(std::process::Stdio::piped());

        match fd.output().await {
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stdout);
                if text.is_empty() {
                    return ToolOutput::Ok {
                        content: "(no files found)".into(),
                    };
                }
                let content = text
                    .lines()
                    .take(MAX_RESULTS)
                    .map(|l| relativize(l, canon_root.as_deref()))
                    .collect::<Vec<_>>()
                    .join("\n");
                ToolOutput::Ok {
                    content: with_code_map(content, root, self.code_map.as_ref()),
                }
            }
            Err(_) => {
                // fd not installed — fall back to find (same pinned dir).
                let mut find = Command::new("find");
                find.arg(&search_dir);
                find.arg("-type").arg("f");
                find.arg("-name").arg(pattern);
                find.stdout(std::process::Stdio::piped());
                find.stderr(std::process::Stdio::piped());

                match find.output().await {
                    Ok(output) => {
                        let text = String::from_utf8_lossy(&output.stdout);
                        if text.is_empty() {
                            ToolOutput::Ok {
                                content: "(no files found)".into(),
                            }
                        } else {
                            let content = text
                                .lines()
                                .take(MAX_RESULTS)
                                .map(|l| relativize(l, canon_root.as_deref()))
                                .collect::<Vec<_>>()
                                .join("\n");
                            ToolOutput::Ok {
                                content: with_code_map(content, root, self.code_map.as_ref()),
                            }
                        }
                    }
                    Err(e) => ToolOutput::Error {
                        message: format!("find failed: {e}"),
                    },
                }
            }
        }
    }
}

/// Append the code-map footer for these matched paths, when a footer is
/// enabled and the graph has one to give (see [`crate::code_map`]). Bound
/// separately from the `if let` so the `content.lines()` borrow ends before
/// `content` is mutated.
fn with_code_map(
    mut content: String,
    root: &std::path::Path,
    tip: Option<&crate::code_map::TipOnce>,
) -> String {
    let Some(tip) = tip else {
        return content;
    };
    let map = crate::code_map::for_files(root, content.lines(), tip);
    if let Some(map) = map {
        content.push_str(&map);
    }
    content
}

/// Render an absolute match path relative to the workspace root, honoring the
/// tool's advertised "relative paths from workspace root" contract. Falls back
/// to the original string if the root is unknown or the path lies outside it.
fn relativize(line: &str, canon_root: Option<&std::path::Path>) -> String {
    match canon_root {
        Some(root) => std::path::Path::new(line)
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| line.to_string()),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_files_matching_pattern() {
        let dir = std::env::temp_dir();
        let subdir = format!("stella_glob_{}", std::process::id());
        tokio::fs::create_dir_all(dir.join(&subdir)).await.unwrap();
        tokio::fs::write(dir.join(&subdir).join("a.rs"), "fn main(){}")
            .await
            .unwrap();
        tokio::fs::write(dir.join(&subdir).join("b.txt"), "hello")
            .await
            .unwrap();

        // Use a glob pattern (not regex) so both `fd` and `find -name`
        // handle it identically across platforms.
        let result = Glob::default()
            .execute(
                &serde_json::json!({"pattern": "*.rs", "path": &subdir}),
                &dir,
            )
            .await;
        match result {
            ToolOutput::Ok { content } => {
                assert!(content.contains("a.rs"), "expected a.rs in: {content}");
                assert!(
                    !content.contains("b.txt"),
                    "should not contain b.txt: {content}"
                );
            }
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
        let _ = tokio::fs::remove_dir_all(dir.join(&subdir)).await;
    }

    #[tokio::test]
    async fn results_carry_a_code_map_footer_when_an_index_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("lib.rs"), "pub fn greet() {}\n").expect("write");
        let db = crate::graph::graph_db_path(dir.path());
        std::fs::create_dir_all(db.parent().expect("parent")).expect("mkdir");
        let graph = stella_graph::CodeGraph::open(dir.path(), &db).expect("open graph");
        graph.index_all().expect("index");
        graph.shutdown();

        let result = Glob::default()
            .execute(&serde_json::json!({"pattern": "*.rs"}), dir.path())
            .await;
        match result {
            ToolOutput::Ok { content } => {
                assert!(content.contains("lib.rs"), "the file list stays: {content}");
                assert!(content.contains("code map"), "footer attached: {content}");
                assert!(
                    content.contains("function greet:1"),
                    "maps the listed file's symbols: {content}"
                );
            }
            ToolOutput::Error { message } => panic!("expected files, got: {message}"),
        }
    }

    #[tokio::test]
    async fn no_files_returns_ok() {
        let dir = std::env::temp_dir();
        let result = Glob::default()
            .execute(&serde_json::json!({"pattern": "*.nonexistent"}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => assert!(content.contains("no files")),
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
    }
}
