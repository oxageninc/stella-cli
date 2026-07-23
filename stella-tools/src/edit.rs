//! `edit_file` — replace an exact substring in a file. Surgical edits, not
//! full rewrites. Supports `replace_all` for multi-occurrence.
//!
//! The tool shares the session's read-state ledger (#331): when `old_string`
//! fails to match, it compares current disk bytes against the hash of what
//! the model last saw (recorded by `read_file`/`read_symbol` and by the
//! model's own edits/writes) and *attributes* the failure — a drifted file
//! gets a drift-named error carrying the fresh content so the model can
//! re-issue the edit against current bytes, instead of a generic not-found
//! that sends it back into a read→edit-fail thrash. Because the drift echo
//! embeds the changed content, a legitimate recovery never produces
//! byte-identical outputs, so the loop detector (which requires identical
//! outputs to flag a loop) keeps treating it as progress.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::read::ReadLedger;
use crate::registry::Tool;

/// Ceiling on the fresh-content echo inside a drift-attributed error, so a
/// huge drifted file doesn't flood the context through an error message.
const DRIFT_ECHO_MAX_LINES: usize = 400;

#[derive(Default)]
pub struct EditFile {
    ledger: Arc<ReadLedger>,
}

impl EditFile {
    /// Construct sharing the registry's read-state ledger, so match failures
    /// can be attributed against what the model last saw.
    pub fn with_ledger(ledger: Arc<ReadLedger>) -> Self {
        Self { ledger }
    }
}

/// Render the fresh content echoed inside a drift-attributed error:
/// line-numbered like `read_file` output (so the model can re-anchor
/// edits), capped at [`DRIFT_ECHO_MAX_LINES`].
fn drift_echo(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let shown = lines.len().min(DRIFT_ECHO_MAX_LINES);
    let mut numbered = String::new();
    for (i, line) in lines[..shown].iter().enumerate() {
        numbered.push_str(&format!("{:>6}\t{line}\n", i + 1));
    }
    if shown < lines.len() {
        numbered.push_str(&format!(
            "(first {shown} of {} lines — use read_file for the rest)\n",
            lines.len()
        ));
    }
    numbered
}

#[async_trait]
impl Tool for EditFile {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "edit_file".into(),
            description: "Replace an exact substring in a file. By default the old_string must appear exactly once; set replace_all to replace every occurrence.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to workspace root" },
                    "old_string": { "type": "string", "description": "Exact text to find" },
                    "new_string": { "type": "string", "description": "Replacement text" },
                    "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)" },
                    "reason": { "type": "string", "description": "Why you are editing this file — recorded in the session's file-touch audit log" },
                    "storage_intent": { "type": "string", "description": "Only when creating a database table/column that the storage gate flagged as similar to an existing one: one sentence of purpose plus why the existing objects don't fit. Recorded in stella.storage.toml." }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            read_only: false,
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
        let old_string = match input.get("old_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return ToolOutput::Error {
                    message: "missing required field `old_string`".into(),
                };
            }
        };
        // An empty `old_string` is destructive: `"".matches("")` reports
        // char_count+1 hits, so the tool would tell the model to set
        // replace_all=true and then `replace("", new)` interleaves `new` at
        // every char boundary — shredding the file (and allocating O(len^2)).
        // On an empty file it would silently overwrite. Refuse it outright.
        if old_string.is_empty() {
            return ToolOutput::Error {
                message: "old_string must not be empty — use write_file to create or replace a \
                          whole file"
                    .into(),
            };
        }
        let new_string = match input.get("new_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return ToolOutput::Error {
                    message: "missing required field `new_string`".into(),
                };
            }
        };
        let replace_all = input
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let full_path = match crate::resolve_within_root(root, path) {
            Some(p) => p,
            None => {
                return ToolOutput::Error {
                    message: format!("path `{path}` escapes workspace root"),
                };
            }
        };

        let content = match tokio::fs::read_to_string(&full_path).await {
            Ok(c) => c,
            Err(e) => {
                return ToolOutput::Error {
                    message: format!("failed to read `{path}`: {e}"),
                };
            }
        };

        let count = content.matches(old_string).count();
        if count == 0 {
            // Attribute the miss (#331): compare current bytes against what
            // the model last saw. Three distinguishable causes, three
            // different recoveries — a generic not-found forces the model to
            // guess which one it is.
            let current_sha = crate::staleness::hex_sha256(content.as_bytes());
            return match self.ledger.last_seen_sha(root, path) {
                Some(seen) if seen != current_sha => {
                    // Drift: the file changed after the model last saw it.
                    // Echo the fresh content so the model can re-issue the
                    // edit without a round-trip — and record it as seen, so
                    // a repeat failure against these same bytes is reported
                    // as unchanged (not re-attributed as drift forever).
                    // The echo is capped, so the recorded hash may cover
                    // more than was shown; the unchanged-file message below
                    // still steers a confused model back to read_file.
                    self.ledger.record_known(root, path, &content);
                    ToolOutput::Error {
                        message: format!(
                            "old_string not found in `{path}` — the file CHANGED after you last \
                             read it (out-of-band modification); the copy in your context is \
                             stale. Current content follows — re-issue the edit against these \
                             bytes.\n\n--- {path} (current) ---\n{}",
                            drift_echo(&content)
                        ),
                    }
                }
                Some(_) => ToolOutput::Error {
                    message: format!(
                        "old_string not found in `{path}` — the file is unchanged since you last \
                         saw it, so the copy in your context matches disk; check for exact \
                         whitespace/newline differences"
                    ),
                },
                None => ToolOutput::Error {
                    message: format!(
                        "old_string not found in `{path}` — no read of this file is recorded \
                         this session; read it first and copy old_string byte-exact"
                    ),
                },
            };
        }
        if count > 1 && !replace_all {
            return ToolOutput::Error {
                message: format!(
                    "old_string appears {count} times in `{path}` — set replace_all=true or provide a more specific string"
                ),
            };
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        match tokio::fs::write(&full_path, &new_content).await {
            Ok(()) => {
                // The model knows the bytes it just produced — record them so
                // its own edit is never later misattributed as drift.
                self.ledger.record_known(root, path, &new_content);
                let replaced = if replace_all { count } else { 1 };
                ToolOutput::Ok {
                    content: format!("replaced {replaced} occurrence(s) in {path}"),
                }
            }
            Err(e) => ToolOutput::Error {
                message: format!("failed to write `{path}`: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::ReadFile;

    #[tokio::test]
    async fn replaces_unique_substring() {
        let dir = std::env::temp_dir();
        let path = format!("stella_edit_{}.rs", std::process::id());
        let full = dir.join(&path);
        tokio::fs::write(&full, "fn main() { old }").await.unwrap();

        let result = EditFile::default()
            .execute(
                &serde_json::json!({"path": path, "old_string": "old", "new_string": "new"}),
                &dir,
            )
            .await;
        match result {
            ToolOutput::Ok { content } => assert!(content.contains("replaced 1")),
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
        let after = tokio::fs::read_to_string(&full).await.unwrap();
        assert_eq!(after, "fn main() { new }");
        let _ = tokio::fs::remove_file(&full).await;
    }

    #[tokio::test]
    async fn errors_on_multiple_without_replace_all() {
        let dir = std::env::temp_dir();
        let path = format!("stella_edit_multi_{}.rs", std::process::id());
        let full = dir.join(&path);
        tokio::fs::write(&full, "a a a").await.unwrap();

        let result = EditFile::default()
            .execute(
                &serde_json::json!({"path": path, "old_string": "a", "new_string": "b"}),
                &dir,
            )
            .await;
        assert!(result.is_error());
        let _ = tokio::fs::remove_file(&full).await;
    }

    #[tokio::test]
    async fn replace_all_works() {
        let dir = std::env::temp_dir();
        let path = format!("stella_edit_all_{}.rs", std::process::id());
        let full = dir.join(&path);
        tokio::fs::write(&full, "a a a").await.unwrap();

        let result = EditFile::default()
            .execute(
                &serde_json::json!({"path": path, "old_string": "a", "new_string": "b", "replace_all": true}),
                &dir,
            )
            .await;
        match result {
            ToolOutput::Ok { content } => assert!(content.contains("replaced 3")),
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
        let after = tokio::fs::read_to_string(&full).await.unwrap();
        assert_eq!(after, "b b b");
        let _ = tokio::fs::remove_file(&full).await;
    }

    #[tokio::test]
    async fn not_found_without_a_read_names_the_missing_read() {
        let dir = std::env::temp_dir();
        let path = format!("stella_edit_nf_{}.rs", std::process::id());
        let full = dir.join(&path);
        tokio::fs::write(&full, "hello world").await.unwrap();

        let result = EditFile::default()
            .execute(
                &serde_json::json!({"path": path, "old_string": "xyz", "new_string": "abc"}),
                &dir,
            )
            .await;
        match result {
            ToolOutput::Error { message } => {
                assert!(message.contains("old_string not found"), "got: {message}");
                assert!(
                    message.contains("no read of this file is recorded"),
                    "an unread file must be attributed as such: {message}"
                );
            }
            ToolOutput::Ok { content } => panic!("expected error, got: {content}"),
        }
        let _ = tokio::fs::remove_file(&full).await;
    }

    /// The #331 witness: read a file, mutate it out-of-band, then edit with
    /// an `old_string` that no longer matches — the error must name the
    /// concurrent change (not the generic not-found) and carry the fresh
    /// content so the model can re-issue the edit without a round-trip.
    #[tokio::test]
    async fn drift_is_attributed_and_fresh_content_echoed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "original contents\n").unwrap();
        let ledger = Arc::new(ReadLedger::default());
        let read = ReadFile::with_ledger(ledger.clone());
        let edit = EditFile::with_ledger(ledger.clone());

        let seen = read
            .execute(&serde_json::json!({"path": "a.rs"}), dir.path())
            .await;
        assert!(!seen.is_error(), "{seen:?}");

        // Out-of-band change (another process, the user, a subagent).
        std::fs::write(dir.path().join("a.rs"), "rewritten elsewhere\n").unwrap();

        let result = edit
            .execute(
                &serde_json::json!({"path": "a.rs", "old_string": "original", "new_string": "x"}),
                dir.path(),
            )
            .await;
        match result {
            ToolOutput::Error { message } => {
                assert!(
                    message.contains("CHANGED after you last read it"),
                    "drift must be attributed: {message}"
                );
                assert!(
                    message.contains("rewritten elsewhere"),
                    "fresh content must be echoed: {message}"
                );
            }
            ToolOutput::Ok { content } => panic!("expected drift error, got: {content}"),
        }

        // The echo counts as seen: a repeat failure against the SAME bytes is
        // reported as unchanged, not re-attributed as drift forever.
        let repeat = edit
            .execute(
                &serde_json::json!({"path": "a.rs", "old_string": "original", "new_string": "x"}),
                dir.path(),
            )
            .await;
        match repeat {
            ToolOutput::Error { message } => {
                assert!(
                    message.contains("unchanged since you last saw it"),
                    "got: {message}"
                );
            }
            ToolOutput::Ok { content } => panic!("expected error, got: {content}"),
        }

        // And the recovery works: an edit against current bytes succeeds.
        let recovered = edit
            .execute(
                &serde_json::json!({"path": "a.rs", "old_string": "rewritten", "new_string": "fixed"}),
                dir.path(),
            )
            .await;
        assert!(!recovered.is_error(), "{recovered:?}");
    }

    #[tokio::test]
    async fn unchanged_file_failure_is_not_drift_attributed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "hello world\n").unwrap();
        let ledger = Arc::new(ReadLedger::default());
        let read = ReadFile::with_ledger(ledger.clone());
        let edit = EditFile::with_ledger(ledger.clone());

        let seen = read
            .execute(&serde_json::json!({"path": "a.rs"}), dir.path())
            .await;
        assert!(!seen.is_error());

        let result = edit
            .execute(
                &serde_json::json!({"path": "a.rs", "old_string": "helo world", "new_string": "x"}),
                dir.path(),
            )
            .await;
        match result {
            ToolOutput::Error { message } => {
                assert!(
                    message.contains("unchanged since you last saw it"),
                    "an unchanged file must not be blamed on drift: {message}"
                );
                assert!(
                    message.contains("whitespace/newline"),
                    "the classic hint stays: {message}"
                );
            }
            ToolOutput::Ok { content } => panic!("expected error, got: {content}"),
        }
    }

    #[tokio::test]
    async fn own_successful_edit_is_not_later_misattributed_as_drift() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one two three\n").unwrap();
        let ledger = Arc::new(ReadLedger::default());
        let read = ReadFile::with_ledger(ledger.clone());
        let edit = EditFile::with_ledger(ledger.clone());

        let seen = read
            .execute(&serde_json::json!({"path": "a.rs"}), dir.path())
            .await;
        assert!(!seen.is_error());

        // The model's own edit changes the file relative to the read…
        let first = edit
            .execute(
                &serde_json::json!({"path": "a.rs", "old_string": "two", "new_string": "2"}),
                dir.path(),
            )
            .await;
        assert!(!first.is_error(), "{first:?}");

        // …but a subsequent bad old_string is the model's mistake, not drift.
        let second = edit
            .execute(
                &serde_json::json!({"path": "a.rs", "old_string": "bogus", "new_string": "x"}),
                dir.path(),
            )
            .await;
        match second {
            ToolOutput::Error { message } => {
                assert!(
                    message.contains("unchanged since you last saw it"),
                    "own edits must update the seen hash: {message}"
                );
            }
            ToolOutput::Ok { content } => panic!("expected error, got: {content}"),
        }
    }

    #[tokio::test]
    async fn drift_echo_is_capped_for_huge_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), "seed\n").unwrap();
        let ledger = Arc::new(ReadLedger::default());
        let read = ReadFile::with_ledger(ledger.clone());
        let edit = EditFile::with_ledger(ledger.clone());

        let seen = read
            .execute(&serde_json::json!({"path": "big.txt"}), dir.path())
            .await;
        assert!(!seen.is_error());

        let big: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();

        let result = edit
            .execute(
                &serde_json::json!({"path": "big.txt", "old_string": "seed", "new_string": "x"}),
                dir.path(),
            )
            .await;
        match result {
            ToolOutput::Error { message } => {
                assert!(message.contains("CHANGED after you last read it"));
                assert!(message.contains("line 400"), "echo shows the cap window");
                assert!(
                    !message.contains("line 401"),
                    "echo must stop at the cap: {}",
                    &message[message.len().saturating_sub(200)..]
                );
                assert!(
                    message.contains("first 400 of 1000 lines"),
                    "truncation must be named: {message}"
                );
            }
            ToolOutput::Ok { content } => panic!("expected drift error, got: {content}"),
        }
    }
}
