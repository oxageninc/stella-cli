//! `read_file` — read a file with optional line range and a line cap.
//! Mirrors the TS `read_file` tool: 1-based line numbers, `offset`/`limit`
//! params, and a cap on the number of lines *returned* (`MAX_LINES`) so a huge
//! file can't flood the context. Note this bounds displayed lines, not bytes:
//! the file is read into memory first, so a single pathologically long line is
//! still read in full.
//!
//! The tool also keeps the session's per-file read tally: every successful
//! read increments a counter keyed by the file's normalized
//! workspace-relative path (so `src/./a.rs` and `src/a.rs` count as one
//! file), and the running count is reported in the tool output. One
//! [`ReadFile`] instance lives per registry, so the tally is per session by
//! construction; the audit-grade equivalent (one `R` event per read) lands in
//! the registry's file-touch ledger.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::registry::Tool;

const MAX_LINES: usize = 2000;

#[derive(Default)]
pub struct ReadFile {
    /// Per-session read counts by normalized workspace-relative path.
    counts: Mutex<HashMap<String, u64>>,
}

impl ReadFile {
    /// How many times `path` (under any workspace-relative spelling) has been
    /// successfully read this session.
    pub fn read_count(&self, root: &std::path::Path, path: &str) -> u64 {
        self.counts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&normalized_key(root, path))
            .copied()
            .unwrap_or(0)
    }

    /// Record one successful read of `path`, returning the new total.
    fn tally(&self, root: &std::path::Path, path: &str) -> u64 {
        let mut counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        let count = counts.entry(normalized_key(root, path)).or_insert(0);
        *count += 1;
        *count
    }
}

/// The aggregation key for a read: the same normalized workspace-relative
/// path the file-touch ledger uses, falling back to the raw spelling when
/// normalization fails (it can't for a path that resolved and read OK).
fn normalized_key(root: &std::path::Path, path: &str) -> String {
    crate::file_touch::normalize_workspace_path(root, path).unwrap_or_else(|| path.to_string())
}

#[async_trait]
impl Tool for ReadFile {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".into(),
            description: "Read a file from the workspace. Returns content with 1-based line numbers. Use offset and limit for ranges.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to workspace root" },
                    "offset": { "type": "integer", "description": "1-based start line (optional)" },
                    "limit": { "type": "integer", "description": "Max lines to return (optional, default 2000)" },
                    "reason": { "type": "string", "description": "Why you are reading this file — recorded in the session's file-touch audit log" }
                },
                "required": ["path"]
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolOutput::Error {
                    message: "missing required field `path`".into(),
                };
            }
        };

        let offset = input
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(MAX_LINES);

        let full_path = match crate::resolve_within_root(root, path) {
            Some(p) => p,
            None => {
                return ToolOutput::Error {
                    message: format!("path `{path}` escapes workspace root"),
                };
            }
        };

        match tokio::fs::read_to_string(&full_path).await {
            Ok(content) => {
                // Count every successful read — including a past-end offset,
                // which still read the file — so the tally matches the
                // ledger's one-R-event-per-successful-read rule.
                let reads = self.tally(root, path);
                let lines: Vec<&str> = content.lines().collect();
                let start = offset.unwrap_or(1).saturating_sub(1);
                let end = (start + limit).min(lines.len());

                if start >= lines.len() {
                    return ToolOutput::Ok {
                        content: format!(
                            "(file has {} lines, offset {start} is past end)",
                            lines.len()
                        ),
                    };
                }

                let mut numbered = String::new();
                for (i, line) in lines[start..end].iter().enumerate() {
                    let line_num = start + i + 1;
                    numbered.push_str(&format!("{line_num:>6}\t{line}\n"));
                }
                let total = lines.len();
                let shown = end - start;
                numbered.push_str(&format!(
                    "\n({shown}/{total} lines shown · read {reads}× this session)"
                ));
                ToolOutput::Ok { content: numbered }
            }
            Err(e) => ToolOutput::Error {
                message: format!("failed to read `{path}`: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_file_with_line_numbers() {
        let dir = std::env::temp_dir();
        let path = format!("stella_test_read_{}.txt", std::process::id());
        let full = dir.join(&path);
        tokio::fs::write(&full, "line one\nline two\nline three\n")
            .await
            .unwrap();

        let result = ReadFile::default()
            .execute(&serde_json::json!({"path": path}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => {
                assert!(content.contains("1\tline one"));
                assert!(content.contains("2\tline two"));
                assert!(content.contains("3/3 lines shown"));
            }
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
        let _ = tokio::fs::remove_file(&full).await;
    }

    #[tokio::test]
    async fn respects_offset_and_limit() {
        let dir = std::env::temp_dir();
        let path = format!("stella_test_range_{}.txt", std::process::id());
        let full = dir.join(&path);
        tokio::fs::write(&full, "a\nb\nc\nd\ne\n").await.unwrap();

        let result = ReadFile::default()
            .execute(
                &serde_json::json!({"path": path, "offset": 2, "limit": 2}),
                &dir,
            )
            .await;
        match result {
            ToolOutput::Ok { content } => {
                assert!(content.contains("2\tb"));
                assert!(content.contains("3\tc"));
                assert!(!content.contains("4\td"));
                assert!(content.contains("2/5 lines shown"));
            }
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
        let _ = tokio::fs::remove_file(&full).await;
    }

    #[tokio::test]
    async fn counts_reads_per_file_and_reports_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "one\n").unwrap();
        let tool = ReadFile::default();

        // Two reads of the same file under different spellings aggregate.
        for spelling in ["src/a.rs", "src/./a.rs"] {
            let out = tool
                .execute(&serde_json::json!({"path": spelling}), dir.path())
                .await;
            assert!(!out.is_error(), "{out:?}");
        }
        let third = tool
            .execute(&serde_json::json!({"path": "src/a.rs"}), dir.path())
            .await;
        match third {
            ToolOutput::Ok { content } => {
                assert!(
                    content.contains("read 3× this session"),
                    "third read reports its count: {content}"
                );
            }
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
        assert_eq!(tool.read_count(dir.path(), "src/a.rs"), 3);
        assert_eq!(tool.read_count(dir.path(), "src/./a.rs"), 3);

        // Other files and failed reads don't inflate the tally.
        let other = tool
            .execute(&serde_json::json!({"path": "b.rs"}), dir.path())
            .await;
        assert!(!other.is_error());
        assert_eq!(tool.read_count(dir.path(), "b.rs"), 1);
        let missing = tool
            .execute(&serde_json::json!({"path": "ghost.rs"}), dir.path())
            .await;
        assert!(missing.is_error());
        assert_eq!(tool.read_count(dir.path(), "ghost.rs"), 0);
    }

    #[tokio::test]
    async fn missing_file_returns_error() {
        let dir = std::env::temp_dir();
        let result = ReadFile::default()
            .execute(
                &serde_json::json!({"path": "nonexistent_xyz_123.txt"}),
                &dir,
            )
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn path_escape_returns_error() {
        let dir = std::env::temp_dir();
        let result = ReadFile::default()
            .execute(&serde_json::json!({"path": "../../etc/passwd"}), &dir)
            .await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn missing_path_field_returns_error() {
        let dir = std::env::temp_dir();
        let result = ReadFile::default()
            .execute(&serde_json::json!({}), &dir)
            .await;
        assert!(result.is_error());
    }
}
