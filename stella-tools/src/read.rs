//! `read_file` — read a file with optional line range and a line cap.
//! Mirrors the TS `read_file` tool: 1-based line numbers, `offset`/`limit`
//! params, and a cap on the number of lines *returned* (`MAX_LINES`) so a huge
//! file can't flood the context. Note this bounds displayed lines, not bytes:
//! the file is read into memory first, so a single pathologically long line is
//! still read in full.
//!
//! The tool also keeps the session's read-state ledger ([`ReadLedger`]): every
//! successful read records a per-file tally (reported in the tool output) and
//! the sha256 of the bytes that were current at read time. The hash is the
//! read→edit drift oracle's baseline (#331): `edit_file` compares it against
//! current disk bytes to attribute a failed match to an out-of-band change
//! instead of a generic not-found. Reads are keyed by the file's normalized
//! workspace-relative path (so `src/./a.rs` and `src/a.rs` count as one
//! file). One ledger lives per registry, shared by `read_file`, `read_symbol`
//! (which reads through this same tool), `edit_file`, and `write_file` — so
//! the ledger tracks the content the model last *saw*, whichever surface
//! showed (or produced) it. The audit-grade equivalent (one `R` event per
//! read) lands in the registry's file-touch ledger.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::registry::Tool;

/// Crate-visible so `read_symbol` (which reads through this tool) can name
/// the cap honestly when a symbol's span exceeds it.
pub(crate) const MAX_LINES: usize = 2000;

/// Per-file record of what the model last saw: how many times the file was
/// read, and the sha256 of the file's full content the last time the model
/// saw it (via a read, or via its own successful edit/write).
#[derive(Debug, Clone, Default)]
struct ReadState {
    reads: u64,
    sha256: String,
}

/// Session-scoped ledger of the last file content the model has *seen*.
///
/// Updated by successful reads (`read_file` and `read_symbol`) and by the
/// model's own successful mutations (`edit_file`, `write_file` — the model
/// knows the content it just produced). A mismatch between an entry's hash
/// and current disk bytes therefore means the file changed out-of-band since
/// the model last looked — the read→edit drift signal (#331).
#[derive(Default)]
pub struct ReadLedger {
    states: Mutex<HashMap<String, ReadState>>,
}

impl ReadLedger {
    /// Record one successful read of `path` whose full content was `content`,
    /// returning the new per-file read count.
    pub fn record_read(&self, root: &std::path::Path, path: &str, content: &str) -> u64 {
        let mut states = self.states.lock().unwrap_or_else(|p| p.into_inner());
        let state = states.entry(normalized_key(root, path)).or_default();
        state.reads += 1;
        state.sha256 = crate::staleness::hex_sha256(content.as_bytes());
        state.reads
    }

    /// Record content the model produced or was shown for `path` without
    /// counting a read — a successful `edit_file`/`write_file`, or the fresh
    /// content echoed inside a drift-attributed error.
    pub fn record_known(&self, root: &std::path::Path, path: &str, content: &str) {
        let mut states = self.states.lock().unwrap_or_else(|p| p.into_inner());
        let state = states.entry(normalized_key(root, path)).or_default();
        state.sha256 = crate::staleness::hex_sha256(content.as_bytes());
    }

    /// How many times `path` (under any workspace-relative spelling) has been
    /// successfully read this session.
    pub fn read_count(&self, root: &std::path::Path, path: &str) -> u64 {
        self.states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&normalized_key(root, path))
            .map(|s| s.reads)
            .unwrap_or(0)
    }

    /// sha256 of the content the model last saw for `path` — `None` when the
    /// file was never read (or written) through the ledger this session.
    pub fn last_seen_sha(&self, root: &std::path::Path, path: &str) -> Option<String> {
        self.states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&normalized_key(root, path))
            .map(|s| s.sha256.clone())
            .filter(|sha| !sha.is_empty())
    }
}

#[derive(Default)]
pub struct ReadFile {
    ledger: Arc<ReadLedger>,
}

impl ReadFile {
    /// Construct sharing the registry's read-state ledger, so edits and
    /// writes see the hashes this tool records.
    pub fn with_ledger(ledger: Arc<ReadLedger>) -> Self {
        Self { ledger }
    }

    /// How many times `path` (under any workspace-relative spelling) has been
    /// successfully read this session.
    pub fn read_count(&self, root: &std::path::Path, path: &str) -> u64 {
        self.ledger.read_count(root, path)
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
                    "limit": { "type": "integer", "description": "Max lines to return (optional; default and ceiling 2000)" },
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
        // MAX_LINES is a ceiling, not just the default: the flood protection
        // the module header promises must hold for explicit limits too.
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(MAX_LINES))
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
                // ledger's one-R-event-per-successful-read rule. The hash is
                // of the FULL content (even for a ranged read): drift asks
                // "has the file changed since the model looked", and the
                // whole file was current at that moment.
                let reads = self.ledger.record_read(root, path, &content);
                let lines: Vec<&str> = content.lines().collect();
                let start = offset.unwrap_or(1).saturating_sub(1);
                let end = start.saturating_add(limit).min(lines.len());

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
    async fn read_records_last_seen_hash_even_for_ranged_reads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\n").unwrap();
        let ledger = Arc::new(ReadLedger::default());
        let tool = ReadFile::with_ledger(ledger.clone());

        assert_eq!(ledger.last_seen_sha(dir.path(), "a.rs"), None);
        let out = tool
            .execute(
                &serde_json::json!({"path": "a.rs", "offset": 2, "limit": 1}),
                dir.path(),
            )
            .await;
        assert!(!out.is_error(), "{out:?}");
        // The hash covers the FULL file content current at read time, not
        // just the displayed range — drift asks about the file, not the view.
        assert_eq!(
            ledger.last_seen_sha(dir.path(), "a.rs"),
            Some(crate::staleness::hex_sha256(b"one\ntwo\nthree\n")),
        );

        // An out-of-band change is visible as a hash mismatch…
        std::fs::write(dir.path().join("a.rs"), "rewritten\n").unwrap();
        assert_ne!(
            ledger.last_seen_sha(dir.path(), "a.rs"),
            Some(crate::staleness::hex_sha256(b"rewritten\n")),
        );
        // …until the next read refreshes the baseline.
        let again = tool
            .execute(&serde_json::json!({"path": "a.rs"}), dir.path())
            .await;
        assert!(!again.is_error());
        assert_eq!(
            ledger.last_seen_sha(dir.path(), "a.rs"),
            Some(crate::staleness::hex_sha256(b"rewritten\n")),
        );
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
