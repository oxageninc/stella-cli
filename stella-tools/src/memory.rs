//! `save_memory` — persist a lesson to `.stella/memories/` so future
//! sessions execute better. The self-improvement loop's write half.
//!
//! The read half is deliberately NOT a tool: memories are loaded once at
//! session start into the system prompt's stable prefix (see
//! `stella-cli`'s prompt assembly), where every model call considers them
//! at prompt-cache-hit prices. That placement is also why this tool's
//! contract says new memories take effect NEXT session — hot-injecting
//! them mid-session would mutate the cached prefix and re-bill it on
//! every subsequent call.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::registry::Tool;

/// Workspace-relative directory holding one markdown memory per file.
pub const MEMORIES_DIR: &str = ".stella/memories";

pub struct SaveMemory;

#[async_trait]
impl Tool for SaveMemory {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "save_memory".into(),
            description: "Persist a durable lesson (gotcha, convention, failed approach) to \
                          .stella/memories/. Loaded into every future session's prompt. \
                          Takes effect next session."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string", "description": "Filename slug (lowercase, -, _)" },
                    "memory": { "type": "string", "description": "The lesson, 1-6 lines of markdown" }
                },
                "required": ["slug", "memory"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let Some(slug) = input.get("slug").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "missing required field `slug`".into(),
            };
        };
        let Some(memory) = input.get("memory").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "missing required field `memory`".into(),
            };
        };
        if slug.is_empty()
            || slug.len() > 64
            || !slug
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return ToolOutput::Error {
                message: format!(
                    "invalid slug `{slug}` — use 1-64 lowercase letters, digits, - or _"
                ),
            };
        }
        if memory.trim().is_empty() || memory.chars().count() > 2_000 {
            return ToolOutput::Error {
                message: "memory must be non-empty and at most 2000 characters — memories are \
                          prompt-resident, keep them dense"
                    .into(),
            };
        }
        let dir = root.join(MEMORIES_DIR);
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            return ToolOutput::Error {
                message: format!("could not create {}: {e}", dir.display()),
            };
        }
        let path = dir.join(format!("{slug}.md"));
        let existed = path.exists();
        if let Err(e) = tokio::fs::write(&path, memory).await {
            return ToolOutput::Error {
                message: format!("could not write {}: {e}", path.display()),
            };
        }
        ToolOutput::Ok {
            content: format!(
                "{} memory `{slug}` — it will be in the prompt from the next session on",
                if existed { "updated" } else { "saved" }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn saves_validates_and_overwrites() {
        let root = std::env::temp_dir().join(format!("stella_memory_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        let ok = SaveMemory
            .execute(
                &serde_json::json!({"slug": "cargo-workspace", "memory": "Always build with --workspace."}),
                &root,
            )
            .await;
        assert!(!ok.is_error(), "{ok:?}");
        let saved =
            std::fs::read_to_string(root.join(MEMORIES_DIR).join("cargo-workspace.md")).unwrap();
        assert_eq!(saved, "Always build with --workspace.");

        for bad in [
            serde_json::json!({"slug": "UPPER", "memory": "x"}),
            serde_json::json!({"slug": "ok", "memory": ""}),
            serde_json::json!({"slug": "ok", "memory": "y".repeat(2001)}),
            serde_json::json!({"memory": "no slug"}),
        ] {
            assert!(SaveMemory.execute(&bad, &root).await.is_error(), "{bad}");
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
