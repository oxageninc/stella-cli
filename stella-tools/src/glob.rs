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

        // Try fd first — fast, respects .gitignore by default.
        // `--glob` tells fd to interpret the pattern as a glob, not a regex,
        // so `*.rs` matches the same way as `find -name "*.rs"`.
        let mut fd = Command::new("fd");
        fd.arg("--glob");
        fd.arg("--type").arg("f");
        fd.arg("--color").arg("never");
        fd.arg("--max-results").arg(MAX_RESULTS.to_string());
        fd.arg(pattern).arg(root.join(search_path));
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
                let lines: Vec<&str> = text.lines().take(MAX_RESULTS).collect();
                ToolOutput::Ok {
                    content: lines.join("\n"),
                }
            }
            Err(_) => {
                // fd not installed — fall back to find
                let mut find = Command::new("find");
                find.arg(root.join(search_path));
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
                            let lines: Vec<&str> = text.lines().take(MAX_RESULTS).collect();
                            ToolOutput::Ok {
                                content: lines.join("\n"),
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
