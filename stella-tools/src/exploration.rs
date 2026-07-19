//! The session exploration store — shared codebase maps under
//! `.stella/explorations/` (`docs/design/exploration-sharing.md`).
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
//!   title, summary, and a per-file freshness verdict; `{"slice": "..."}`
//!   returns one map's full content with exact drift lists.
//! - [`SaveExploration`] (mutating): persists a map for a named slice,
//!   stamping save time, the repo's git HEAD, the producing pid, and a
//!   `path → sha256` manifest of the files the map covers — the shared
//!   staleness oracle ([`crate::staleness`]). Saving an existing slice
//!   overwrites it — the newest map of a slice is the truth.
//!
//! Validity is per file, not per repo: a commit elsewhere degrades nothing,
//! and a real drift names exactly which covered files moved so a reader
//! re-verifies two files instead of re-exploring thirty.
//!
//! A map saved with `status: "draft"` doubles as the in-flight claim
//! (spec §4c): other sessions see `IN PROGRESS` with liveness derived from
//! the recorded pid — a crashed session leaves readable partial notes,
//! never a leaked lock. Records are plain JSON files, one per slice, so
//! they are inspectable, diffable, and shareable through the filesystem
//! with zero coordination.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::registry::Tool;
use crate::staleness::{self, Freshness};

/// Subdirectory of the workspace root holding one JSON record per slice.
const EXPLORATIONS_DIR: &str = ".stella/explorations";

/// Manifest cap — mirrors `gather_context`'s pack cap. A map whose evidence
/// exceeds this is really several slices; the save response says so.
const MAX_MANIFEST_FILES: usize = 200;

/// Internal input key the registry uses to pass the session's file-touch
/// read paths into a save (`docs/design/exploration-sharing.md` §3d). Not
/// advertised in the tool schema — the model never authors it.
pub const LEDGER_FILES_KEY: &str = "_session_read_files";

/// Lifecycle of a record: a draft is an in-flight claim with partial notes;
/// complete is a finished map.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExplorationStatus {
    Draft,
    #[default]
    Complete,
}

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
    /// `git rev-parse HEAD` at save time — provenance, and the free
    /// short-circuit for the freshness check.
    #[serde(default)]
    git_head: Option<String>,
    /// Per-file staleness oracle: workspace-relative path → sha256 of file
    /// bytes at save time. Superset of `files`. Empty on pre-manifest (v1)
    /// records, which fall back to the coarse HEAD compare.
    #[serde(default)]
    manifest: BTreeMap<String, String>,
    /// Draft (in-flight claim) or complete map.
    #[serde(default)]
    status: ExplorationStatus,
    /// Pid of the producing session — the liveness probe target for drafts.
    #[serde(default)]
    pid: Option<u32>,
    /// Key symbols the map covers (code-graph anchors). Optional.
    #[serde(default)]
    symbols: Vec<String>,
}

/// Read-model of one record for consumers outside the tool pair: the CLI's
/// startup index, the registry's coverage hints, the Observatory.
#[derive(Debug, Clone)]
pub struct ExplorationSummary {
    pub slice: String,
    pub title: String,
    pub summary: String,
    pub created_at_ms: u64,
    pub status: ExplorationStatus,
    pub pid: Option<u32>,
    pub freshness: Freshness,
    /// Covered paths (manifest keys; falls back to `files` for v1 records).
    pub covered: Vec<String>,
    pub manifest_len: usize,
}

impl ExplorationSummary {
    /// Is the producing process still alive? Only meaningful for drafts.
    pub fn producer_alive(&self) -> bool {
        self.pid.is_some_and(pid_alive)
    }
}

/// Whether `pid` is a live process. Unix: `kill(pid, 0)`; `EPERM` still
/// means "exists". Same probe as the session registry
/// (`stella-store/src/sessions.rs`).
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // A pid above i32::MAX would wrap negative under `as` and probe a
        // process group instead of a process — treat as dead.
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Validate a slice slug: path-safe, no traversal, predictable filenames.
fn valid_slice(slice: &str) -> bool {
    !slice.is_empty()
        && slice.len() <= 64
        && slice
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
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

/// Synchronous store scan with freshness verdicts — for the CLI's startup
/// index and the registry's coverage index, both built outside a runtime.
/// One `git_head` query serves every record.
pub fn summaries_sync(root: &std::path::Path) -> Vec<ExplorationSummary> {
    let dir = root.join(EXPLORATIONS_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let current_head = staleness::git_head_sync(root);
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(record) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<ExplorationRecord>(&raw).ok())
        else {
            continue;
        };
        let freshness = staleness::freshness_sync(
            root,
            &record.manifest,
            record.git_head.as_deref(),
            current_head.as_deref(),
        );
        let covered: Vec<String> = if record.manifest.is_empty() {
            record.files.clone()
        } else {
            record.manifest.keys().cloned().collect()
        };
        out.push(ExplorationSummary {
            slice: record.slice,
            title: record.title,
            summary: record.summary,
            created_at_ms: record.created_at_ms,
            status: record.status,
            pid: record.pid,
            freshness,
            manifest_len: record.manifest.len(),
            covered,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.created_at_ms));
    out
}

/// The draft slices a given process currently holds — the cheap read the
/// session registry refresh uses every turn (JSON parse only, no hashing,
/// no git).
pub fn draft_slices_for_pid(root: &std::path::Path, pid: u32) -> Vec<String> {
    let dir = root.join(EXPLORATIONS_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut slices: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| {
            let raw = std::fs::read_to_string(e.path()).ok()?;
            let record: ExplorationRecord = serde_json::from_str(&raw).ok()?;
            (record.status == ExplorationStatus::Draft && record.pid == Some(pid))
                .then_some(record.slice)
        })
        .collect();
    slices.sort();
    slices
}

/// Render the workspace-maps index for the system prompt (spec §4a):
/// metadata only, newest-first, dropped-with-notice past `budget_chars`.
/// Returns `None` when the store is empty (no section, no tokens).
pub fn render_index(summaries: &[ExplorationSummary], budget_chars: usize) -> Option<String> {
    if summaries.is_empty() {
        return None;
    }
    let footer = "Read one with the explorations tool ({\"slice\": ...}). Check this list \
                  BEFORE exploring any area; after mapping unmapped territory, persist it \
                  with save_exploration so the next session reuses your map.\n";
    let mut out = String::from("\n## Workspace maps (shared across all Stella sessions)\n");
    let mut dropped = 0usize;
    for s in summaries {
        let line = match s.status {
            ExplorationStatus::Draft => {
                let live = if s.producer_alive() {
                    format!(
                        "IN PROGRESS by pid {} (live), started {} — read the draft before duplicating",
                        s.pid.unwrap_or(0),
                        age_human(s.created_at_ms)
                    )
                } else {
                    "abandoned draft — partial notes usable, safe to take over".to_string()
                };
                format!("- `{}` — {} ({live})\n", s.slice, s.title)
            }
            ExplorationStatus::Complete => format!(
                "- `{}` — {} ({}, {}, {} files): {}\n",
                s.slice,
                s.title,
                s.freshness.label(s.manifest_len),
                age_human(s.created_at_ms),
                s.covered.len(),
                s.summary
            ),
        };
        if out.len() + line.len() + footer.len() > budget_chars {
            dropped += 1;
            continue;
        }
        out.push_str(&line);
    }
    if dropped > 0 {
        out.push_str(&format!(
            "- (+{dropped} older maps not shown — list all with the explorations tool)\n"
        ));
    }
    out.push_str(footer);
    Some(out)
}

/// Render one record's freshness for the model, affirmatively when fresh
/// (spec §3c) — an over-cautious warning on an untouched map trains agents
/// to distrust the store.
fn render_freshness(
    freshness: &Freshness,
    manifest_len: usize,
    record: &ExplorationRecord,
) -> String {
    match freshness {
        Freshness::Fresh if manifest_len > 0 => {
            format!("\n✓ fresh — all {manifest_len} covered files verified unchanged since save")
        }
        Freshness::Fresh => String::new(),
        Freshness::Drifted { changed, missing } => {
            let mut s = format!(
                "\n⚠ drifted — {}/{} covered files moved since save; re-verify sections touching:",
                changed.len() + missing.len(),
                manifest_len
            );
            if !changed.is_empty() {
                s.push_str(&format!("\n  changed: {}", changed.join(", ")));
            }
            if !missing.is_empty() {
                s.push_str(&format!("\n  missing: {}", missing.join(", ")));
            }
            s.push_str("\neverything else in the map is still current");
            s
        }
        Freshness::Unknown { head_moved } => {
            // Pre-manifest (v1) record: the old coarse HEAD compare wording.
            if *head_moved {
                let saved = record.git_head.as_deref().unwrap_or("?");
                format!(
                    "\n⚠ saved at commit {} but the repo has moved — verify details \
                     against the current code before relying on them",
                    &saved[..saved.len().min(8)]
                )
            } else {
                String::new()
            }
        }
    }
}

/// Read-only lookup over the exploration store.
pub struct Explorations;

#[async_trait]
impl Tool for Explorations {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "explorations".into(),
            description: "Saved codebase maps with per-file freshness. No args = list all; \
                          {slice} = read one. Check before exploring a module."
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
        let current_head = staleness::git_head(root).await;

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
            let freshness = staleness::freshness(
                root,
                &record.manifest,
                record.git_head.as_deref(),
                current_head.as_deref(),
            )
            .await;
            let draft_banner = match record.status {
                ExplorationStatus::Draft => {
                    if record.pid.is_some_and(pid_alive) {
                        format!(
                            "\n⚠ DRAFT — being mapped right now by pid {} (live); partial notes below",
                            record.pid.unwrap_or(0)
                        )
                    } else {
                        "\n⚠ DRAFT — producing session is gone; partial notes below, safe to take over"
                            .to_string()
                    }
                }
                ExplorationStatus::Complete => String::new(),
            };
            let covered = if record.manifest.is_empty() {
                record.files.join(", ")
            } else {
                record
                    .manifest
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return ToolOutput::Ok {
                content: format!(
                    "# {} ({})\nsaved {}{}{}\nfiles: {}\n\n{}\n\n{}",
                    record.title,
                    record.slice,
                    age_human(record.created_at_ms),
                    draft_banner,
                    render_freshness(&freshness, record.manifest.len(), record),
                    covered,
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
        let mut lines: Vec<String> = Vec::with_capacity(records.len());
        for r in &records {
            let line = match r.status {
                ExplorationStatus::Draft => {
                    let state = if r.pid.is_some_and(pid_alive) {
                        format!("IN PROGRESS by pid {} (live)", r.pid.unwrap_or(0))
                    } else {
                        "abandoned draft — safe to take over".to_string()
                    };
                    format!(
                        "- `{}` — {} ({state}, {}): {}",
                        r.slice,
                        r.title,
                        age_human(r.created_at_ms),
                        r.summary
                    )
                }
                ExplorationStatus::Complete => {
                    let freshness = staleness::freshness(
                        root,
                        &r.manifest,
                        r.git_head.as_deref(),
                        current_head.as_deref(),
                    )
                    .await;
                    // Pre-manifest records keep the quiet v1 line — a loud
                    // "unverified" on every legacy record is noise.
                    let verdict = match &freshness {
                        Freshness::Unknown { .. } => String::new(),
                        other => format!("{}, ", other.label(r.manifest.len())),
                    };
                    format!(
                        "- `{}` — {} ({verdict}{}, {} files): {}",
                        r.slice,
                        r.title,
                        age_human(r.created_at_ms),
                        if r.manifest.is_empty() {
                            r.files.len()
                        } else {
                            r.manifest.len()
                        },
                        r.summary
                    )
                }
            };
            lines.push(line);
        }
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
                          reuse. Save after substantial exploration; same slice overwrites. \
                          Save early with status \"draft\" to signal an in-progress mapping \
                          to concurrent sessions, then overwrite when complete."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "slice": { "type": "string", "description": "Short slug for the slice, e.g. `cli`, `billing-webhooks` (lowercase letters, digits, - and _)" },
                    "title": { "type": "string", "description": "One-line title" },
                    "summary": { "type": "string", "description": "2-4 sentences: what the slice does and what this map covers" },
                    "content": { "type": "string", "description": "The full map, markdown: entry points, data flow, key types, file-by-file notes, gotchas" },
                    "files": { "type": "array", "items": { "type": "string" }, "description": "Key files covered, workspace-relative" },
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Key symbols covered (optional graph anchors)" },
                    "status": { "type": "string", "enum": ["draft", "complete"], "description": "\"draft\" = in-progress claim visible to other sessions; default \"complete\"" }
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
        let str_list = |key: &str| -> Vec<String> {
            input
                .get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let files = str_list("files");
        let symbols = str_list("symbols");
        let status = match input.get("status").and_then(|v| v.as_str()) {
            Some("draft") => ExplorationStatus::Draft,
            _ => ExplorationStatus::Complete,
        };

        // Manifest evidence = declared files ∪ the session's read paths the
        // registry passed through (spec §3d) — the map's actual sources, at
        // zero model-token cost. Nonexistent paths drop out inside
        // build_manifest.
        let mut evidence: Vec<String> = files.clone();
        evidence.extend(str_list(LEDGER_FILES_KEY));
        evidence.sort();
        evidence.dedup();
        let over_cap = evidence.len().saturating_sub(MAX_MANIFEST_FILES);
        evidence.truncate(MAX_MANIFEST_FILES);
        let manifest = staleness::build_manifest(root, evidence).await;

        let record = ExplorationRecord {
            slice: slice.to_string(),
            title: title.to_string(),
            summary: summary.to_string(),
            content: content.to_string(),
            files,
            created_at_ms: now_ms(),
            git_head: staleness::git_head(root).await,
            manifest,
            status,
            pid: Some(std::process::id()),
            symbols,
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
        let mut note = format!(
            "{} exploration `{slice}` ({} chars, {} files tracked for staleness) — other \
             agents can now read it via explorations",
            if existed { "updated" } else { "saved" },
            record.content.chars().count(),
            record.manifest.len(),
        );
        if status == ExplorationStatus::Draft {
            note.push_str(
                "; marked DRAFT — concurrent sessions see it as in-progress; overwrite with \
                 status \"complete\" when done",
            );
        }
        if over_cap > 0 {
            note.push_str(&format!(
                "; {over_cap} evidence files beyond the {MAX_MANIFEST_FILES}-file manifest cap \
                 were not tracked — this slice may be worth splitting"
            ));
        }
        ToolOutput::Ok { content: note }
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
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        let save = SaveExploration;
        let input = serde_json::json!({
            "slice": "cli",
            "title": "CLI command surface",
            "summary": "Maps the argument parser and REPL loop.",
            "content": "## Entry\nmain.rs parses with clap…",
            "files": ["main.rs", "src/agent.rs"]
        });
        let saved = save.execute(&input, &root).await;
        assert!(!saved.is_error(), "{saved:?}");

        let list = Explorations.execute(&serde_json::json!({}), &root).await;
        match &list {
            ToolOutput::Ok { content } => {
                assert!(content.contains("`cli`"), "{content}");
                assert!(content.contains("CLI command surface"), "{content}");
                // Only main.rs exists → 1 manifest file; fresh since untouched.
                assert!(content.contains("fresh"), "{content}");
                assert!(content.contains("1 files"), "{content}");
            }
            other => panic!("list failed: {other:?}"),
        }

        let read = Explorations
            .execute(&serde_json::json!({"slice": "cli"}), &root)
            .await;
        match &read {
            ToolOutput::Ok { content } => {
                assert!(content.contains("main.rs parses with clap"), "{content}");
                assert!(content.contains("verified unchanged"), "{content}");
            }
            other => panic!("read failed: {other:?}"),
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn drift_names_exact_files_not_the_whole_map() {
        let root = temp_root("drift");
        std::fs::write(root.join("a.rs"), "a v1").unwrap();
        std::fs::write(root.join("b.rs"), "b v1").unwrap();
        let input = serde_json::json!({
            "slice": "pair", "title": "Pair", "summary": "s", "content": "map",
            "files": ["a.rs", "b.rs"]
        });
        assert!(!SaveExploration.execute(&input, &root).await.is_error());

        // Change one file; the other stays. Verdict must name only a.rs.
        std::fs::write(root.join("a.rs"), "a v2").unwrap();
        let read = Explorations
            .execute(&serde_json::json!({"slice": "pair"}), &root)
            .await;
        match &read {
            ToolOutput::Ok { content } => {
                assert!(content.contains("drifted — 1/2"), "{content}");
                assert!(content.contains("changed: a.rs"), "{content}");
                assert!(!content.contains("missing"), "{content}");
                assert!(content.contains("still current"), "{content}");
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn v1_record_without_manifest_still_reads_with_head_fallback() {
        let root = temp_root("v1compat");
        let dir = root.join(EXPLORATIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        // A v1 record: no manifest/status/pid/symbols fields at all.
        std::fs::write(
            dir.join("legacy.json"),
            serde_json::json!({
                "slice": "legacy", "title": "Legacy map", "summary": "old",
                "content": "v1 body", "files": ["x.rs"],
                "created_at_ms": 123u64, "git_head": "abcdef1234567890"
            })
            .to_string(),
        )
        .unwrap();

        let read = Explorations
            .execute(&serde_json::json!({"slice": "legacy"}), &root)
            .await;
        match &read {
            ToolOutput::Ok { content } => {
                assert!(content.contains("v1 body"), "{content}");
                // Not a git repo → head unknown → cautious wording, not a crash.
                assert!(content.contains("verify details"), "{content}");
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn draft_save_lists_as_in_progress_by_live_pid() {
        let root = temp_root("draft");
        let input = serde_json::json!({
            "slice": "wip", "title": "WIP slice", "summary": "mapping now",
            "content": "partial notes", "status": "draft"
        });
        let saved = SaveExploration.execute(&input, &root).await;
        match &saved {
            ToolOutput::Ok { content } => assert!(content.contains("DRAFT"), "{content}"),
            other => panic!("{other:?}"),
        }

        // Saved from this test process → pid is alive → IN PROGRESS.
        let list = Explorations.execute(&serde_json::json!({}), &root).await;
        match &list {
            ToolOutput::Ok { content } => {
                assert!(content.contains("IN PROGRESS by pid"), "{content}");
                assert!(content.contains("(live)"), "{content}");
            }
            other => panic!("{other:?}"),
        }

        let read = Explorations
            .execute(&serde_json::json!({"slice": "wip"}), &root)
            .await;
        match &read {
            ToolOutput::Ok { content } => {
                assert!(content.contains("DRAFT"), "{content}");
                assert!(content.contains("partial notes"), "{content}");
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn dead_pid_draft_reads_as_abandoned() {
        let root = temp_root("dead_draft");
        let dir = root.join(EXPLORATIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        // u32::MAX cannot be a live pid (also exercises the i32 overflow
        // guard in pid_alive).
        std::fs::write(
            dir.join("gone.json"),
            serde_json::json!({
                "slice": "gone", "title": "Gone", "summary": "s", "content": "partial",
                "files": [], "created_at_ms": 123u64, "status": "draft",
                "pid": u32::MAX
            })
            .to_string(),
        )
        .unwrap();
        let list = Explorations.execute(&serde_json::json!({}), &root).await;
        match &list {
            ToolOutput::Ok { content } => {
                assert!(content.contains("abandoned draft"), "{content}");
            }
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ledger_paths_flow_into_manifest_without_model_tokens() {
        let root = temp_root("ledger");
        std::fs::write(root.join("seen.rs"), "read by session").unwrap();
        std::fs::write(root.join("declared.rs"), "declared by model").unwrap();
        let input = serde_json::json!({
            "slice": "led", "title": "Ledger", "summary": "s", "content": "map",
            "files": ["declared.rs"],
            LEDGER_FILES_KEY: ["seen.rs", "ghost.rs"]
        });
        let saved = SaveExploration.execute(&input, &root).await;
        match &saved {
            // declared.rs + seen.rs exist; ghost.rs drops out.
            ToolOutput::Ok { content } => {
                assert!(content.contains("2 files tracked"), "{content}")
            }
            other => panic!("{other:?}"),
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
    fn summaries_and_index_render_within_budget() {
        let root = temp_root("index");
        let dir = root.join(EXPLORATIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(root.join("z.rs"), "zzz").unwrap();
        let manifest: BTreeMap<String, String> =
            [("z.rs".to_string(), crate::staleness::hex_sha256(b"zzz"))].into();
        for (i, slice) in ["one", "two", "three"].iter().enumerate() {
            std::fs::write(
                dir.join(format!("{slice}.json")),
                serde_json::to_string(&ExplorationRecord {
                    slice: slice.to_string(),
                    title: format!("Map {slice}"),
                    summary: "covers z".into(),
                    content: "body".into(),
                    files: vec!["z.rs".into()],
                    created_at_ms: 1000 + i as u64,
                    git_head: None,
                    manifest: manifest.clone(),
                    status: ExplorationStatus::Complete,
                    pid: None,
                    symbols: vec![],
                })
                .unwrap(),
            )
            .unwrap();
        }
        let summaries = summaries_sync(&root);
        assert_eq!(summaries.len(), 3);
        assert!(summaries.iter().all(|s| s.freshness.is_fresh()));
        // Newest first.
        assert_eq!(summaries[0].slice, "three");

        let full = render_index(&summaries, 4_000).expect("index");
        assert!(full.contains("## Workspace maps"));
        assert!(full.contains("`one`") && full.contains("`three`"));
        assert!(full.contains("save_exploration"));

        // A tight budget drops with notice, never silently. (Header+footer
        // ≈ 270 chars; ~65/line ⇒ 400 fits one or two lines, not three.)
        let tight = render_index(&summaries, 400).expect("index");
        assert!(tight.contains("older maps not shown"), "{tight}");
        assert!(tight.contains("`three`"), "newest survives: {tight}");

        assert!(render_index(&[], 4_000).is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn schema_marks_lookup_read_only_and_save_mutating() {
        assert!(Explorations.schema().read_only);
        assert!(!SaveExploration.schema().read_only);
    }
}
