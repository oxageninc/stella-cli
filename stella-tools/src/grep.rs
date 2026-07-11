//! `grep` — search file contents with a regex. Shells to `rg` when present
//! for speed; falls back to a pure-Rust walk otherwise.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};
use tokio::process::Command;

use crate::registry::Tool;

const MAX_RESULTS: usize = 200;

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "grep".into(),
            description: "Search file contents with a regex. Returns matching file:line:text. Shells to ripgrep when available.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression to search for" },
                    "path": { "type": "string", "description": "Subdirectory to search (default: workspace root)" },
                    "glob": { "type": "string", "description": "Restrict to files matching this glob (e.g. *.rs)" }
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
        let glob_filter = input.get("glob").and_then(|v| v.as_str());

        // Try ripgrep first — it's the fast path.
        let mut rg = Command::new("rg");
        rg.arg("--line-number")
            .arg("--no-heading")
            .arg("--color")
            .arg("never");
        rg.arg("--max-count").arg(MAX_RESULTS.to_string());
        if let Some(g) = glob_filter {
            rg.arg("--glob").arg(g);
        }
        rg.arg(pattern).arg(root.join(search_path));
        rg.stdout(std::process::Stdio::piped());
        rg.stderr(std::process::Stdio::piped());

        match rg.output().await {
            Ok(output) if output.status.success() || !output.stdout.is_empty() => {
                let text = String::from_utf8_lossy(&output.stdout);
                if text.is_empty() {
                    return ToolOutput::Ok {
                        content: "(no matches)".into(),
                    };
                }
                let lines: Vec<&str> = text.lines().take(MAX_RESULTS).collect();
                let mut result = lines.join("\n");
                if lines.len() == MAX_RESULTS {
                    result.push_str(&format!("\n... (showing first {MAX_RESULTS} matches)"));
                }
                ToolOutput::Ok { content: result }
            }
            Ok(_) => ToolOutput::Ok {
                content: "(no matches)".into(),
            },
            Err(_) => {
                // rg not installed — fall back to grep
                let mut grep = Command::new("grep");
                grep.arg("-rn").arg("--color=never");
                if let Some(g) = glob_filter {
                    grep.arg("--include").arg(g);
                }
                grep.arg(pattern).arg(root.join(search_path));
                grep.stdout(std::process::Stdio::piped());
                grep.stderr(std::process::Stdio::piped());

                match grep.output().await {
                    Ok(output) => {
                        let text = String::from_utf8_lossy(&output.stdout);
                        if text.is_empty() {
                            ToolOutput::Ok {
                                content: "(no matches)".into(),
                            }
                        } else {
                            let lines: Vec<&str> = text.lines().take(MAX_RESULTS).collect();
                            ToolOutput::Ok {
                                content: lines.join("\n"),
                            }
                        }
                    }
                    Err(e) => ToolOutput::Error {
                        message: format!("grep failed: {e}"),
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
    async fn finds_matching_lines() {
        let dir = std::env::temp_dir();
        let path = format!("stella_grep_{}.txt", std::process::id());
        tokio::fs::write(dir.join(&path), "hello world\nfoo bar\nhello again\n")
            .await
            .unwrap();

        let result = Grep
            .execute(&serde_json::json!({"pattern": "hello", "path": path}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => {
                assert!(content.contains("hello world") || content.contains("hello"));
            }
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
        let _ = tokio::fs::remove_file(dir.join(&path)).await;
    }

    #[tokio::test]
    async fn no_matches_returns_ok_empty() {
        let dir = std::env::temp_dir();
        let result = Grep
            .execute(&serde_json::json!({"pattern": "zzz_not_found_xyz"}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => assert!(content.contains("no matches")),
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
    }
}
