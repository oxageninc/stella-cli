//! `edit_file` — replace an exact substring in a file. Surgical edits, not
//! full rewrites. Supports `replace_all` for multi-occurrence.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::registry::Tool;

pub struct EditFile;

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
            return ToolOutput::Error {
                message: format!(
                    "old_string not found in `{path}` — check for exact whitespace/newline differences"
                ),
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

    #[tokio::test]
    async fn replaces_unique_substring() {
        let dir = std::env::temp_dir();
        let path = format!("stella_edit_{}.rs", std::process::id());
        let full = dir.join(&path);
        tokio::fs::write(&full, "fn main() { old }").await.unwrap();

        let result = EditFile
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

        let result = EditFile
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

        let result = EditFile
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
    async fn not_found_gives_diagnostic() {
        let dir = std::env::temp_dir();
        let path = format!("stella_edit_nf_{}.rs", std::process::id());
        let full = dir.join(&path);
        tokio::fs::write(&full, "hello world").await.unwrap();

        let result = EditFile
            .execute(
                &serde_json::json!({"path": path, "old_string": "xyz", "new_string": "abc"}),
                &dir,
            )
            .await;
        assert!(result.is_error());
        let _ = tokio::fs::remove_file(&full).await;
    }
}
