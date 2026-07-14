//! `screenshot` — capture the screen (or a window/region) to a PNG under
//! `.stella/screenshots/`, for work verification: a judge or reviewer can
//! demand visual evidence that a UI change actually rendered.
//!
//! The capture lands on disk and the tool returns its path + size. Vision
//! (attaching the image bytes to a model call) arrives with multimodal
//! message support (spec `08-multimodal.md`); until then the artifact is
//! still valuable — humans open it, and agents cite it as evidence.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::exec;
use crate::registry::Tool;

pub struct Screenshot;

#[async_trait]
impl Tool for Screenshot {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "screenshot".into(),
            description: "Capture the screen to a PNG in .stella/screenshots/ as verification \
                          evidence. Returns the file path."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "Short slug for the filename" }
                }
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let label = input
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("capture")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(48)
            .collect::<String>();
        let dir = root.join(".stella/screenshots");
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            return ToolOutput::Error {
                message: format!("could not create {}: {e}", dir.display()),
            };
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file = dir.join(format!("{stamp}-{label}.png"));
        let path = file.display();

        // Platform capture chain: macOS screencapture; Linux grim (Wayland)
        // then import (X11/ImageMagick). Each is silent + non-interactive.
        let command = if cfg!(target_os = "macos") {
            format!("screencapture -x '{path}'")
        } else {
            format!(
                "grim '{path}' 2>/dev/null || import -window root '{path}' 2>/dev/null || \
                 (echo 'no capture backend (grim or imagemagick import)' >&2; exit 1)"
            )
        };
        match exec::run(&command, root, 30).await {
            Ok((0, _)) => {
                let size = tokio::fs::metadata(&file)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                if size == 0 {
                    return ToolOutput::Error {
                        message: "capture produced an empty file — is screen recording \
                                  permission granted?"
                            .into(),
                    };
                }
                let rel = file
                    .strip_prefix(root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| file.display().to_string());
                ToolOutput::Ok {
                    content: format!("captured {rel} ({size} bytes)"),
                }
            }
            Ok((code, output)) => ToolOutput::Error {
                message: format!("screen capture failed (exit {code}): {output}"),
            },
            Err(e) => ToolOutput::Error { message: e },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_mutating_and_named() {
        let schema = Screenshot.schema();
        assert_eq!(schema.name, "screenshot");
        assert!(!schema.read_only);
    }

    #[tokio::test]
    async fn label_is_sanitized_into_the_filename_path() {
        // Can't actually capture in CI — assert the failure path is a named
        // error, not a panic, when no backend/permission exists. On a dev
        // Mac with permission this passes through the success path instead;
        // both are acceptable shapes.
        let root = std::env::temp_dir().join(format!("stella_shot_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let out = Screenshot
            .execute(&serde_json::json!({"label": "../weird label!!"}), &root)
            .await;
        match out {
            ToolOutput::Ok { content } => assert!(content.contains("weirdlabel")),
            ToolOutput::Error { message } => assert!(!message.is_empty()),
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
