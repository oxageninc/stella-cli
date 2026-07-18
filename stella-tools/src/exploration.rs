//! The session exploration store — shared codebase maps under
//! `.stella/explorations/`.
//!
//! An "exploration" is a high-fidelity, end-to-end map of one slice of a
//! codebase (a module, a subsystem, a data flow) produced by an agent while
//! working. Producing one is expensive — many reads, greps, and reasoning
//! tokens — and its value is almost entirely reusable: five parallel
//! feature builds over the same slice need one map, not five.
//!
//! Two tools expose the store to the model:
//!
//! - [`Explorations`] (read-only): no input lists every saved map with
//!   title, summary, and staleness hints; `{"slice": "..."}` returns one
//!   map's full content.
//! - [`SaveExploration`] (mutating): persists a map for a named slice,
//!   stamping save time and the repo's git HEAD so later readers can judge
//!   staleness. Saving an existing slice overwrites it — the newest map of
//!   a slice is the truth.
//!
//! Records are plain JSON files, one per slice, so they are inspectable,
//! diffable, and shareable through the filesystem with zero coordination:
//! any concurrently-running agent in the session (or a later session, with
//! the staleness hints in view) sees a map the moment it is saved.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::registry::Tool;

/// Subdirectory of the workspace root holding one JSON record per slice.
const EXPLORATIONS_DIR: &str = ".stella/explorations";

/// One persisted exploration.
#[derive(Debug, Serialize, Deserialize)]
struct ExplorationRecord {
    /// Slug identifying the slice, e.g. `cli`, `billing-webhooks`.
    slice: String,
    /// One-line human title.
    title: String,
    /// A few sentences: what the slice does and what the map covers.
    summary: String,
    /// The map itself — markdown, as detailed as the exploring agent made it.
    content: String,
    /// Key files the map covers, workspace-relative.
    #[serde(default)]
    files: Vec<String>,
    /// Unix epoch milliseconds at save time.
    created_at_ms: u64,
    /// `git rev-parse HEAD` at save time, when the workspace is a git repo —
    /// lets readers detect that the code may have moved under the map.
    #[serde(default)]
    git_head: Option<String>,
}

/// Validate a slice slug: path-safe, no traversal, predictable filenames.
fn valid_slice(slice: &str) -> bool {
    !slice.is_empty()
        && slice.len() <= 64
        && slice
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

async fn git_head(root: &std::path::Path) -> Option<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["rev-parse", "HEAD"]).current_dir(root);
    // Hook-exported GIT_* vars must not re-target this at another repo.
    for var in crate::exec::GIT_REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    let output = cmd.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if head.is_empty() { None } else { Some(head) }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn age_human(created_at_ms: u64) -> String {
    let age_ms = now_ms().saturating_sub(created_at_ms);
    let mins = age_ms / 60_000;
    if mins < 1 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{mins}m ago")
    } else if mins < 60 * 24 {
        format!("{}h ago", mins / 60)
    } else {
        format!("{}d ago", mins / (60 * 24))
    }
}

/// Read every record in the store, skipping unreadable/corrupt files
/// (they are reported in the listing rather than failing the whole call).
async fn read_all(dir: &std::path::Path) -> (Vec<ExplorationRecord>, Vec<String>) {
    let mut records = Vec::new();
    let mut corrupt = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return (records, corrupt);
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match tokio::fs::read_to_string(&path).await {
            Ok(raw) => match serde_json::from_str::<ExplorationRecord>(&raw) {
                Ok(record) => records.push(record),
                Err(_) => corrupt.push(path.display().to_string()),
            },
            Err(_) => corrupt.push(path.display().to_string()),
        }
    }
    records.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
    (records, corrupt)
}

/// Read-only lookup over the exploration store.
pub struct Explorations;

#[async_trait]
impl Tool for Explorations {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "explorations".into(),
            description: "Saved codebase maps. No args = list all; {slice} = read one. Check \
                          before exploring a module."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "slice": { "type": "string", "description": "Slice slug to read in full (omit to list all)" }
                }
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let dir = root.join(EXPLORATIONS_DIR);
        let (records, corrupt) = read_all(&dir).await;

        if let Some(slice) = input.get("slice").and_then(|v| v.as_str()) {
            let Some(record) = records.iter().find(|r| r.slice == slice) else {
                let available: Vec<&str> = records.iter().map(|r| r.slice.as_str()).collect();
                return ToolOutput::Error {
                    message: format!(
                        "no exploration saved for slice `{slice}` — available: [{}]",
                        available.join(", ")
                    ),
                };
            };
            let head = git_head(root).await;
            let staleness = match (&record.git_head, &head) {
                (Some(saved), Some(current)) if saved != current => format!(
                    "\n⚠ saved at commit {} but the repo is now at {} — verify details \
                     against the current code before relying on them",
                    &saved[..saved.len().min(8)],
                    &current[..current.len().min(8)]
                ),
                _ => String::new(),
            };
            return ToolOutput::Ok {
                content: format!(
                    "# {} ({})\nsaved {}{}\nfiles: {}\n\n{}\n\n{}",
                    record.title,
                    record.slice,
                    age_human(record.created_at_ms),
                    staleness,
                    record.files.join(", "),
                    record.summary,
                    record.content
                ),
            };
        }

        if records.is_empty() && corrupt.is_empty() {
            return ToolOutput::Ok {
                content: "no explorations saved yet in this workspace — after you map a slice \
                          of the codebase end-to-end, persist it with save_exploration so other \
                          agents can reuse it"
                    .into(),
            };
        }
        let mut lines: Vec<String> = records
            .iter()
            .map(|r| {
                format!(
                    "- `{}` — {} ({}, {} files): {}",
                    r.slice,
                    r.title,
                    age_human(r.created_at_ms),
                    r.files.len(),
                    r.summary
                )
            })
            .collect();
        for path in corrupt {
            lines.push(format!("- (unreadable exploration record at {path})"));
        }
        lines.push(String::new());
        lines.push("read one in full with {\"slice\": \"<name>\"}".into());
        ToolOutput::Ok {
            content: lines.join("\n"),
        }
    }
}

/// Persist an exploration map for a slice.
pub struct SaveExploration;

#[async_trait]
impl Tool for SaveExploration {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "save_exploration".into(),
            description: "Persist an end-to-end map of a codebase slice for other agents to \
                          reuse. Save after substantial exploration; same slice overwrites."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "slice": { "type": "string", "description": "Short slug for the slice, e.g. `cli`, `billing-webhooks` (lowercase letters, digits, - and _)" },
                    "title": { "type": "string", "description": "One-line title" },
                    "summary": { "type": "string", "description": "2-4 sentences: what the slice does and what this map covers" },
                    "content": { "type": "string", "description": "The full map, markdown: entry points, data flow, key types, file-by-file notes, gotchas" },
                    "files": { "type": "array", "items": { "type": "string" }, "description": "Key files covered, workspace-relative" }
                },
                "required": ["slice", "title", "summary", "content"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let Some(slice) = input.get("slice").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "missing required field `slice`".into(),
            };
        };
        if !valid_slice(slice) {
            return ToolOutput::Error {
                message: format!(
                    "invalid slice slug `{slice}` — use 1-64 lowercase letters, digits, - or _"
                ),
            };
        }
        let (Some(title), Some(summary), Some(content)) = (
            input.get("title").and_then(|v| v.as_str()),
            input.get("summary").and_then(|v| v.as_str()),
            input.get("content").and_then(|v| v.as_str()),
        ) else {
            return ToolOutput::Error {
                message: "missing required field(s): `title`, `summary`, and `content` are all \
                          required"
                    .into(),
            };
        };
        let files: Vec<String> = input
            .get("files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let record = ExplorationRecord {
            slice: slice.to_string(),
            title: title.to_string(),
            summary: summary.to_string(),
            content: content.to_string(),
            files,
            created_at_ms: now_ms(),
            git_head: git_head(root).await,
        };

        let dir = root.join(EXPLORATIONS_DIR);
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            return ToolOutput::Error {
                message: format!("could not create {}: {e}", dir.display()),
            };
        }
        let path = dir.join(format!("{slice}.json"));
        let existed = path.exists();
        let json = match serde_json::to_string_pretty(&record) {
            Ok(json) => json,
            Err(e) => {
                return ToolOutput::Error {
                    message: format!("could not serialize exploration record: {e}"),
                };
            }
        };
        if let Err(e) = tokio::fs::write(&path, json).await {
            return ToolOutput::Error {
                message: format!("could not write {}: {e}", path.display()),
            };
        }
        ToolOutput::Ok {
            content: format!(
                "{} exploration `{slice}` ({} chars) — other agents can now read it via \
                 explorations",
                if existed { "updated" } else { "saved" },
                record.content.chars().count()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stella_exploration_{tag}_{}_{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn save_then_list_then_read_roundtrip() {
        let root = temp_root("roundtrip");
        let save = SaveExploration;
        let input = serde_json::json!({
            "slice": "cli",
            "title": "CLI command surface",
            "summary": "Maps the argument parser and REPL loop.",
            "content": "## Entry\nmain.rs parses with clap…",
            "files": ["src/main.rs", "src/agent.rs"]
        });
        let saved = save.execute(&input, &root).await;
        assert!(!saved.is_error(), "{saved:?}");

        let list = Explorations.execute(&serde_json::json!({}), &root).await;
        match &list {
            ToolOutput::Ok { content } => {
                assert!(content.contains("`cli`"), "{content}");
                assert!(content.contains("CLI command surface"), "{content}");
                assert!(content.contains("2 files"), "{content}");
            }
            other => panic!("list failed: {other:?}"),
        }

        let read = Explorations
            .execute(&serde_json::json!({"slice": "cli"}), &root)
            .await;
        match &read {
            ToolOutput::Ok { content } => {
                assert!(content.contains("main.rs parses with clap"), "{content}");
                assert!(content.contains("src/agent.rs"), "{content}");
            }
            other => panic!("read failed: {other:?}"),
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn missing_slice_lists_what_exists() {
        let root = temp_root("missing");
        let out = Explorations
            .execute(&serde_json::json!({"slice": "nope"}), &root)
            .await;
        assert!(out.is_error());

        let empty_list = Explorations.execute(&serde_json::json!({}), &root).await;
        match empty_list {
            ToolOutput::Ok { content } => {
                assert!(content.contains("no explorations saved yet"), "{content}")
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn saving_same_slice_overwrites_not_duplicates() {
        let root = temp_root("overwrite");
        let save = SaveExploration;
        let mk = |content: &str| {
            serde_json::json!({
                "slice": "api", "title": "API", "summary": "s", "content": content
            })
        };
        assert!(!save.execute(&mk("first map"), &root).await.is_error());
        let second = save.execute(&mk("second map"), &root).await;
        match &second {
            ToolOutput::Ok { content } => assert!(content.starts_with("updated"), "{content}"),
            other => panic!("{other:?}"),
        }

        let read = Explorations
            .execute(&serde_json::json!({"slice": "api"}), &root)
            .await;
        match read {
            ToolOutput::Ok { content } => {
                assert!(content.contains("second map"));
                assert!(!content.contains("first map"));
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn invalid_slugs_and_missing_fields_are_named_errors() {
        let root = temp_root("invalid");
        let save = SaveExploration;
        for bad in [
            "",
            "UPPER",
            "has space",
            "dot.dot",
            "../escape",
            &"x".repeat(65),
        ] {
            let out = save
                .execute(
                    &serde_json::json!({"slice": bad, "title": "t", "summary": "s", "content": "c"}),
                    &root,
                )
                .await;
            assert!(out.is_error(), "slug `{bad}` must be rejected");
        }
        let out = save
            .execute(&serde_json::json!({"slice": "ok-slug"}), &root)
            .await;
        assert!(out.is_error(), "missing fields must be a named error");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn corrupt_records_are_reported_not_fatal() {
        let root = temp_root("corrupt");
        let dir = root.join(EXPLORATIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.json"), "not json at all").unwrap();

        let save = SaveExploration;
        let input = serde_json::json!({
            "slice": "good", "title": "Good", "summary": "s", "content": "map"
        });
        assert!(!save.execute(&input, &root).await.is_error());

        let list = Explorations.execute(&serde_json::json!({}), &root).await;
        match list {
            ToolOutput::Ok { content } => {
                assert!(content.contains("`good`"), "{content}");
                assert!(
                    content.contains("unreadable exploration record"),
                    "{content}"
                );
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn schema_marks_lookup_read_only_and_save_mutating() {
        assert!(Explorations.schema().read_only);
        assert!(!SaveExploration.schema().read_only);
    }
}
