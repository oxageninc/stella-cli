//! `glob` — find files matching a glob pattern. Shells to `fd` when present,
//! falls back to `find` otherwise.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};
use tokio::process::Command;

use crate::registry::Tool;

const MAX_RESULTS: usize = 500;

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "glob".into(),
            description:
                "Find files matching a glob pattern. Returns relative paths from workspace root."
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
                ToolOutput::Ok { content }
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
                            ToolOutput::Ok { content }
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
        let result = Glob
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
    async fn no_files_returns_ok() {
        let dir = std::env::temp_dir();
        let result = Glob
            .execute(&serde_json::json!({"pattern": "*.nonexistent"}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => assert!(content.contains("no files")),
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
    }
}
