//! `gather_context` — one deterministic context sweep, persisted as a
//! structured, staleness-checkable pack other agents reuse instead of
//! re-issuing the same read-only queries against unchanged files.
//!
//! The context-gathering phase of a turn is normally a scatter of separate
//! `grep`/`glob`/`graph_query`/`read_file` calls — each a model round-trip,
//! each re-run from scratch by every agent that needs the same ground
//! truth. This tool folds that whole sweep into ONE call executed inside
//! the tool (sub-queries run concurrently), and saves the result as a
//! *context pack*: a JSON record under `.stella/context/packs/` carrying
//! the query inputs, every section of findings, and a manifest of
//! `path → sha256` for the files the findings came from.
//!
//! The manifest is the staleness oracle, and the pack's `request_key`
//! (a hash of the canonicalized inputs) is the identity of the sweep:
//!
//! - Re-gathering with the same inputs while every manifest file is
//!   byte-identical returns the saved pack without re-running anything —
//!   the deterministic dedup that stops N agents paying for one sweep N
//!   times.
//! - Reading a pack (`{"pack": "<slug>"}`) re-hashes its manifest and
//!   labels exactly which files moved, so a consumer knows what to trust
//!   and what to re-verify.
//!
//! Three modes on one schema slot (mirroring [`crate::exploration`]'s
//! list/read shape, so the model learns one convention):
//! - `{goal, symbols?/patterns?/globs?/files?, save_as?}` — gather + save.
//! - `{"pack": "<slug>"}` — read a saved pack with a per-file freshness
//!   report.
//! - `{}` — list saved packs.
//!
//! # Why `read_only: true` despite writing the pack file
//!
//! The flag's contract in this codebase is about *scheduling and
//! authority*: read-only calls may run concurrently with each other, may
//! run speculatively while the model still streams, and are the only
//! tools a judging context may use. All three are sound here because the
//! tool's only write is a content-addressed derived cache under
//! `.stella/`, outside every source path: re-running it on unchanged
//! inputs writes the same bytes (idempotent), racing gathers of the same
//! sweep converge on identical content, and no workspace file any other
//! tool treats as source of truth is ever touched. A discarded
//! speculative gather leaves only a cache entry a later call would have
//! created anyway.

use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::registry::Tool;
use crate::staleness::hex_sha256;

/// Pack store, sibling of `.stella/explorations`.
const PACKS_DIR: &str = ".stella/context/packs";

/// Excerpted files per pack — enough to seed real work, bounded so one
/// broad pattern can't turn a pack into a codebase dump.
const MAX_EXCERPT_FILES: usize = 12;
/// Context lines around each match line in an excerpt window.
const EXCERPT_CONTEXT_LINES: usize = 8;
/// Match windows excerpted per file.
const MAX_WINDOWS_PER_FILE: usize = 4;
/// Manifest cap — files beyond this are reported but not hash-tracked.
const MAX_MANIFEST_FILES: usize = 200;
/// Per-section render cap (chars), elided loudly.
const MAX_SECTION_CHARS: usize = 8_000;

/// One persisted context pack.
#[derive(Debug, Serialize, Deserialize)]
struct ContextPack {
    slug: String,
    /// What the gatherer was trying to learn — narrative, recorded for the
    /// next reader, deliberately NOT part of `request_key`.
    goal: String,
    /// Hash of the canonicalized (sorted, deduped) inputs — the identity of
    /// the sweep, independent of goal wording.
    request_key: String,
    inputs: PackInputs,
    sections: Vec<PackSection>,
    /// `workspace-relative path → sha256(content)` at gather time, for every
    /// file the findings came from (capped at [`MAX_MANIFEST_FILES`]). The
    /// staleness oracle: a pack is fresh exactly when every entry still
    /// hashes the same.
    manifest: BTreeMap<String, String>,
    created_at_ms: u64,
    #[serde(default)]
    git_head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PackInputs {
    #[serde(default)]
    symbols: Vec<String>,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    globs: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
}

impl PackInputs {
    fn is_empty(&self) -> bool {
        self.symbols.is_empty()
            && self.patterns.is_empty()
            && self.globs.is_empty()
            && self.files.is_empty()
    }

    /// Sort + dedup every list so the same logical sweep always produces
    /// the same `request_key`, regardless of argument order.
    fn canonicalize(&mut self) {
        for list in [
            &mut self.symbols,
            &mut self.patterns,
            &mut self.globs,
            &mut self.files,
        ] {
            list.sort();
            list.dedup();
        }
    }

    fn request_key(&self) -> String {
        let canonical = serde_json::to_string(self).unwrap_or_default();
        hex_sha256(canonical.as_bytes())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PackSection {
    title: String,
    body: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn age_human(created_at_ms: u64) -> String {
    let mins = now_ms().saturating_sub(created_at_ms) / 60_000;
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

async fn git_head(root: &Path) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if head.is_empty() { None } else { Some(head) }
}

/// Same slug rule as the exploration store: path-safe, predictable.
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Derive a slug from the goal text; falls back to a key prefix when the
/// goal has no usable characters.
fn derive_slug(goal: &str, request_key: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for c in goal.chars().flat_map(|c| c.to_lowercase()) {
        if slug.len() >= 48 {
            break;
        }
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        format!("ctx-{}", &request_key[..8])
    } else {
        slug
    }
}

fn pack_path(root: &Path, slug: &str) -> std::path::PathBuf {
    root.join(PACKS_DIR).join(format!("{slug}.json"))
}

async fn load_pack(root: &Path, slug: &str) -> Option<ContextPack> {
    let raw = tokio::fs::read_to_string(pack_path(root, slug))
        .await
        .ok()?;
    serde_json::from_str(&raw).ok()
}

/// Re-hash every manifest entry; returns the workspace-relative paths whose
/// content changed or vanished since the pack was gathered.
async fn stale_paths(root: &Path, manifest: &BTreeMap<String, String>) -> Vec<String> {
    let mut stale = Vec::new();
    for (path, saved_hash) in manifest {
        match tokio::fs::read(root.join(path)).await {
            Ok(bytes) if &hex_sha256(&bytes) == saved_hash => {}
            _ => stale.push(path.clone()),
        }
    }
    stale
}

/// Render a section body under the cap, eliding loudly, never silently.
fn cap_section(body: &str) -> String {
    if body.len() <= MAX_SECTION_CHARS {
        return body.to_string();
    }
    let mut capped: String = body.chars().take(MAX_SECTION_CHARS).collect();
    capped.push_str("\n… (section elided — narrow the sweep for full detail)");
    capped
}

/// Strip the canonical workspace root prefix from a path that a sub-tool
/// rendered absolute, so manifest keys and pack text are workspace-relative
/// and stable across machines.
fn relativize(path: &str, canon_root: Option<&Path>) -> String {
    match canon_root {
        Some(root) => Path::new(path)
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string()),
        None => path.to_string(),
    }
}

/// Parse `path:line:text` grep output rows into per-file match line lists.
fn match_lines_by_file(
    grep_output: &str,
    canon_root: Option<&Path>,
) -> BTreeMap<String, Vec<usize>> {
    let mut by_file: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for row in grep_output.lines() {
        // Split from the left: path (may itself contain `:` only on exotic
        // filesystems, accepted risk), then the line number.
        let mut parts = row.splitn(3, ':');
        let (Some(path), Some(line), Some(_text)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(line) = line.parse::<usize>() else {
            continue;
        };
        by_file
            .entry(relativize(path, canon_root))
            .or_default()
            .push(line);
    }
    by_file
}

/// Excerpt bounded windows of `path` around `match_lines` (1-based); an
/// empty list means "excerpt the head of the file" (explicitly-listed
/// files with no known anchor).
fn excerpt(content: &str, match_lines: &[usize]) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    // Merge match lines into disjoint windows.
    let mut windows: Vec<(usize, usize)> = Vec::new();
    let anchors: Vec<usize> = if match_lines.is_empty() {
        vec![1]
    } else {
        match_lines
            .iter()
            .take(MAX_WINDOWS_PER_FILE)
            .copied()
            .collect()
    };
    for anchor in anchors {
        let start = anchor.saturating_sub(EXCERPT_CONTEXT_LINES + 1);
        let end = (anchor + EXCERPT_CONTEXT_LINES).min(lines.len());
        match windows.last_mut() {
            Some((_, last_end)) if start <= *last_end => *last_end = end,
            _ => windows.push((start, end)),
        }
    }
    let mut out = String::new();
    for (i, (start, end)) in windows.iter().enumerate() {
        if i > 0 {
            out.push_str("  ⋮\n");
        }
        for (offset, line) in lines[*start..*end].iter().enumerate() {
            out.push_str(&format!("{:>5}  {line}\n", start + offset + 1));
        }
    }
    out
}

/// The gather/read/list tool. See the module docs for the three modes and
/// the read-only rationale.
pub struct GatherContext;

#[async_trait]
impl Tool for GatherContext {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "gather_context".into(),
            description: "Run one deterministic context sweep (grep patterns + file globs + \
                          code-graph symbol lookups + bounded excerpts) in a single call and \
                          save it as a reusable context pack. Prefer this over issuing \
                          several separate grep/glob/graph_query/read_file calls when mapping \
                          ground truth for a task. Re-running the same sweep on unchanged \
                          files returns the saved pack instantly. {pack} reads a saved pack \
                          with per-file freshness; no args lists packs."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "What you are trying to learn — recorded in the pack for later readers" },
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "Symbol names to resolve via the code graph (definitions + references)" },
                    "patterns": { "type": "array", "items": { "type": "string" }, "description": "Regexes to grep across the workspace" },
                    "globs": { "type": "array", "items": { "type": "string" }, "description": "File globs to enumerate (e.g. stella-core/**/*.rs)" },
                    "files": { "type": "array", "items": { "type": "string" }, "description": "Workspace-relative files to excerpt directly" },
                    "save_as": { "type": "string", "description": "Slug to save the pack under (default: derived from goal)" },
                    "pack": { "type": "string", "description": "Read this saved pack instead of gathering (freshness-checked)" }
                }
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, root: &Path) -> ToolOutput {
        // Read mode.
        if let Some(slug) = input.get("pack").and_then(|v| v.as_str()) {
            return read_pack(root, slug).await;
        }

        let str_list = |key: &str| -> Vec<String> {
            input
                .get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut inputs = PackInputs {
            symbols: str_list("symbols"),
            patterns: str_list("patterns"),
            globs: str_list("globs"),
            files: str_list("files"),
        };

        // List mode.
        if inputs.is_empty() {
            return list_packs(root).await;
        }

        // Gather mode.
        let goal = input
            .get("goal")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if goal.is_empty() {
            return ToolOutput::Error {
                message: "gathering needs a `goal` — one line on what you're trying to learn, \
                          recorded in the pack for the agents that reuse it"
                    .into(),
            };
        }
        inputs.canonicalize();
        let request_key = inputs.request_key();
        let slug = match input.get("save_as").and_then(|v| v.as_str()) {
            Some(s) if valid_slug(s) => s.to_string(),
            Some(s) => {
                return ToolOutput::Error {
                    message: format!(
                        "invalid save_as slug `{s}` — use 1-64 lowercase letters, digits, - or _"
                    ),
                };
            }
            None => derive_slug(&goal, &request_key),
        };

        // The dedup fast path: an identical sweep over byte-identical files
        // is the saved pack, no re-run.
        if let Some(existing) = load_pack(root, &slug).await
            && existing.request_key == request_key
            && stale_paths(root, &existing.manifest).await.is_empty()
        {
            return ToolOutput::Ok {
                content: render_pack(
                    &existing,
                    &format!(
                        "cache hit — identical sweep already gathered {} and every manifest \
                         file is unchanged; returned without re-running",
                        age_human(existing.created_at_ms)
                    ),
                ),
            };
        }

        gather(root, goal, slug, request_key, inputs).await
    }
}

/// Run the sweep's sub-queries concurrently, assemble + persist the pack,
/// and render it.
async fn gather(
    root: &Path,
    goal: String,
    slug: String,
    request_key: String,
    inputs: PackInputs,
) -> ToolOutput {
    let canon_root = root.canonicalize().ok();
    let mut sections: Vec<PackSection> = Vec::new();

    // ── Concurrent sub-queries ──────────────────────────────────────────
    // Globs and greps reuse the exact Tool impls the model would have
    // called one by one; graph queries reuse the `graph_query` core. All of
    // them are read-only and independent, so they run together.
    let glob_results = futures_util::future::join_all(inputs.globs.iter().map(|pattern| {
        let input = serde_json::json!({ "pattern": pattern });
        async move {
            (
                pattern.clone(),
                // `bare()`: no code-map footer inside packs — the pack runs
                // its own graph queries; a per-section footer is duplication.
                crate::glob::Glob::bare().execute(&input, root).await,
            )
        }
    }));
    let grep_results = futures_util::future::join_all(inputs.patterns.iter().map(|pattern| {
        let input = serde_json::json!({ "pattern": pattern });
        async move {
            (
                pattern.clone(),
                crate::grep::Grep::bare().execute(&input, root).await,
            )
        }
    }));
    let graph_on = crate::graph::graph_available(root);
    let graph_results = futures_util::future::join_all(inputs.symbols.iter().map(|symbol| {
        let symbol = symbol.clone();
        async move {
            if !graph_on {
                return (symbol, None);
            }
            let defs = crate::graph::run_query(root, "definitions", &symbol);
            let refs = crate::graph::run_query(root, "references", &symbol);
            (symbol, Some((defs, refs)))
        }
    }));
    let (glob_results, grep_results, graph_results) =
        futures_util::join!(glob_results, grep_results, graph_results);

    // ── Fold results into sections + the excerpt worklist ───────────────
    let mut match_files: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (pattern, output) in glob_results {
        let body = match output {
            ToolOutput::Ok { content } => content,
            ToolOutput::Error { message } => format!("(glob failed: {message})"),
        };
        sections.push(PackSection {
            title: format!("Files matching `{pattern}`"),
            body: cap_section(&body),
        });
    }
    for (pattern, output) in grep_results {
        let body = match output {
            ToolOutput::Ok { content } => {
                for (path, lines) in match_lines_by_file(&content, canon_root.as_deref()) {
                    match_files.entry(path).or_default().extend(lines);
                }
                // Persist the matches workspace-relative so the pack reads
                // the same from any checkout of this tree.
                content
                    .lines()
                    .map(|row| {
                        let mut parts = row.splitn(2, ':');
                        match (parts.next(), parts.next()) {
                            (Some(path), Some(rest)) if Path::new(path).is_absolute() => {
                                format!("{}:{rest}", relativize(path, canon_root.as_deref()))
                            }
                            _ => row.to_string(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            ToolOutput::Error { message } => format!("(grep failed: {message})"),
        };
        sections.push(PackSection {
            title: format!("Matches for `{pattern}`"),
            body: cap_section(&body),
        });
    }
    for (symbol, result) in graph_results {
        let body = match result {
            None => "(no code-graph index — run `stella init` to add symbol resolution to \
                     future packs)"
                .to_string(),
            Some((defs, refs)) => {
                let render = |label: &str, out: ToolOutput| match out {
                    ToolOutput::Ok { content } => format!("### {label}\n{content}"),
                    ToolOutput::Error { message } => format!("### {label}\n({message})"),
                };
                format!(
                    "{}\n\n{}",
                    render("definitions", defs),
                    render("references", refs)
                )
            }
        };
        sections.push(PackSection {
            title: format!("Symbol `{symbol}`"),
            body: cap_section(&body),
        });
    }

    // ── Excerpts: explicit files first, then the busiest match files ────
    let mut excerpt_targets: Vec<(String, Vec<usize>)> = Vec::new();
    for file in &inputs.files {
        let lines = match_files.get(file).cloned().unwrap_or_default();
        excerpt_targets.push((file.clone(), lines));
    }
    let mut ranked: Vec<(&String, &Vec<usize>)> = match_files
        .iter()
        .filter(|(path, _)| !inputs.files.contains(path))
        .collect();
    // Busiest first; path order breaks ties so the pack is deterministic.
    ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
    for (path, lines) in ranked {
        if excerpt_targets.len() >= MAX_EXCERPT_FILES {
            break;
        }
        excerpt_targets.push((path.clone(), lines.clone()));
    }

    let mut manifest: BTreeMap<String, String> = BTreeMap::new();
    let mut excerpt_body = String::new();
    let mut skipped: Vec<String> = Vec::new();
    for (path, mut lines) in excerpt_targets {
        let Some(resolved) = crate::resolve_within_root(root, &path) else {
            skipped.push(format!("{path} (escapes workspace root)"));
            continue;
        };
        let Ok(bytes) = tokio::fs::read(&resolved).await else {
            skipped.push(format!("{path} (unreadable)"));
            continue;
        };
        manifest.insert(path.clone(), hex_sha256(&bytes));
        let Ok(content) = String::from_utf8(bytes) else {
            skipped.push(format!("{path} (not utf-8 — hashed but not excerpted)"));
            continue;
        };
        lines.sort_unstable();
        lines.dedup();
        excerpt_body.push_str(&format!("## {path}\n{}\n", excerpt(&content, &lines)));
    }
    if !skipped.is_empty() {
        excerpt_body.push_str(&format!("\n(skipped: {})\n", skipped.join(", ")));
    }
    if !excerpt_body.is_empty() {
        sections.push(PackSection {
            title: "Excerpts".to_string(),
            body: cap_section(&excerpt_body),
        });
    }

    // Hash the remaining (non-excerpted) match files into the manifest so
    // staleness covers everything the pack's match lists came from.
    for path in match_files.keys() {
        if manifest.len() >= MAX_MANIFEST_FILES {
            sections.push(PackSection {
                title: "Manifest cap".to_string(),
                body: format!(
                    "manifest capped at {MAX_MANIFEST_FILES} files — staleness detection \
                     does not cover the overflow; narrow the sweep for full coverage"
                ),
            });
            break;
        }
        if !manifest.contains_key(path)
            && let Ok(bytes) = tokio::fs::read(root.join(path)).await
        {
            manifest.insert(path.clone(), hex_sha256(&bytes));
        }
    }

    let pack = ContextPack {
        slug: slug.clone(),
        goal,
        request_key,
        inputs,
        sections,
        manifest,
        created_at_ms: now_ms(),
        git_head: git_head(root).await,
    };

    // Persist — best-effort: a pack that can't be written still returns its
    // findings (the sweep's value), with the failure named.
    let dir = root.join(PACKS_DIR);
    let persist_note = match tokio::fs::create_dir_all(&dir).await {
        Err(e) => format!(
            "NOT saved ({}: {e}) — findings below are this call only",
            dir.display()
        ),
        Ok(()) => match serde_json::to_string_pretty(&pack) {
            Err(e) => format!("NOT saved (serialize: {e}) — findings below are this call only"),
            Ok(json) => match tokio::fs::write(pack_path(root, &slug), json).await {
                Err(e) => {
                    format!("NOT saved (write: {e}) — findings below are this call only")
                }
                Ok(()) => format!(
                    "saved — any agent can reuse it via gather_context {{\"pack\": \"{slug}\"}}"
                ),
            },
        },
    };

    ToolOutput::Ok {
        content: render_pack(&pack, &persist_note),
    }
}

async fn read_pack(root: &Path, slug: &str) -> ToolOutput {
    let Some(pack) = load_pack(root, slug).await else {
        let listing = match list_packs(root).await {
            ToolOutput::Ok { content } => content,
            ToolOutput::Error { message } => message,
        };
        return ToolOutput::Error {
            message: format!("no context pack `{slug}`.\n{listing}"),
        };
    };
    let stale = stale_paths(root, &pack.manifest).await;
    let freshness = if stale.is_empty() {
        format!(
            "fresh — all {} manifest files byte-identical to gather time",
            pack.manifest.len()
        )
    } else {
        format!(
            "⚠ STALE — {}/{} manifest files changed since gather: {}. Trust the rest, \
             re-verify these (or re-run the sweep).",
            stale.len(),
            pack.manifest.len(),
            stale.join(", ")
        )
    };
    ToolOutput::Ok {
        content: render_pack(&pack, &freshness),
    }
}

async fn list_packs(root: &Path) -> ToolOutput {
    let dir = root.join(PACKS_DIR);
    let mut packs: Vec<ContextPack> = Vec::new();
    let mut corrupt: Vec<String> = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(raw) => match serde_json::from_str::<ContextPack>(&raw) {
                    Ok(pack) => packs.push(pack),
                    Err(_) => corrupt.push(path.display().to_string()),
                },
                Err(_) => corrupt.push(path.display().to_string()),
            }
        }
    }
    if packs.is_empty() && corrupt.is_empty() {
        return ToolOutput::Ok {
            content: "no context packs in this workspace yet — gather one with {goal, \
                      patterns/globs/symbols/files} and every agent here can reuse it"
                .into(),
        };
    }
    packs.sort_by_key(|p| std::cmp::Reverse(p.created_at_ms));
    let mut lines: Vec<String> = packs
        .iter()
        .map(|p| {
            format!(
                "- `{}` — {} ({}, {} sections, {} files tracked)",
                p.slug,
                p.goal,
                age_human(p.created_at_ms),
                p.sections.len(),
                p.manifest.len()
            )
        })
        .collect();
    for path in corrupt {
        lines.push(format!("- (unreadable pack at {path})"));
    }
    lines.push(String::new());
    lines.push("read one in full with {\"pack\": \"<slug>\"}".into());
    ToolOutput::Ok {
        content: lines.join("\n"),
    }
}

fn render_pack(pack: &ContextPack, status: &str) -> String {
    let head = pack
        .git_head
        .as_deref()
        .map(|h| format!(" @ {}", &h[..h.len().min(8)]))
        .unwrap_or_default();
    let mut out = format!(
        "# Context pack `{}` — {}\ngathered {}{head} · {} files tracked · {status}\n",
        pack.slug,
        pack.goal,
        age_human(pack.created_at_ms),
        pack.manifest.len(),
    );
    for section in &pack.sections {
        out.push_str(&format!("\n## {}\n{}\n", section.title, section.body));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/auth.rs"),
            "pub fn login(user: &str) -> bool {\n    // TODO: rate limiting\n    !user.is_empty()\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "mod auth;\nfn main() {\n    auth::login(\"root\");\n}\n",
        )
        .unwrap();
        dir
    }

    async fn gather_login_pack(root: &Path) -> ToolOutput {
        GatherContext
            .execute(
                &serde_json::json!({
                    "goal": "Map the login flow",
                    "patterns": ["login"],
                    "globs": ["*.rs"],
                    "save_as": "login-flow"
                }),
                root,
            )
            .await
    }

    #[tokio::test]
    async fn gather_saves_a_pack_with_matches_excerpts_and_manifest() {
        let dir = workspace();
        let out = gather_login_pack(dir.path()).await;
        let content = match out {
            ToolOutput::Ok { content } => content,
            ToolOutput::Error { message } => panic!("gather failed: {message}"),
        };
        assert!(content.contains("Context pack `login-flow`"), "{content}");
        assert!(content.contains("Matches for `login`"), "{content}");
        assert!(content.contains("src/auth.rs"), "{content}");
        assert!(content.contains("saved"), "{content}");

        let raw =
            std::fs::read_to_string(pack_path(dir.path(), "login-flow")).expect("pack persisted");
        let pack: ContextPack = serde_json::from_str(&raw).expect("valid pack json");
        assert_eq!(pack.slug, "login-flow");
        assert!(
            pack.manifest.contains_key("src/auth.rs"),
            "manifest must hash the files the findings came from: {:?}",
            pack.manifest
        );
        assert!(
            pack.sections.iter().any(|s| s.title == "Excerpts"),
            "excerpts section expected"
        );
    }

    #[tokio::test]
    async fn identical_sweep_on_unchanged_files_is_a_cache_hit() {
        let dir = workspace();
        let first = gather_login_pack(dir.path()).await;
        assert!(!first.is_error());

        let second = gather_login_pack(dir.path()).await;
        match second {
            ToolOutput::Ok { content } => {
                assert!(content.contains("cache hit"), "{content}")
            }
            ToolOutput::Error { message } => panic!("{message}"),
        }
    }

    #[tokio::test]
    async fn changing_a_manifest_file_invalidates_the_cache_and_flags_staleness() {
        let dir = workspace();
        assert!(!gather_login_pack(dir.path()).await.is_error());

        std::fs::write(
            dir.path().join("src/auth.rs"),
            "pub fn login(_user: &str) -> bool { false }\n",
        )
        .unwrap();

        // Read mode reports exactly which file moved.
        let read = GatherContext
            .execute(&serde_json::json!({"pack": "login-flow"}), dir.path())
            .await;
        match read {
            ToolOutput::Ok { content } => {
                assert!(content.contains("STALE"), "{content}");
                assert!(content.contains("src/auth.rs"), "{content}");
            }
            ToolOutput::Error { message } => panic!("{message}"),
        }

        // Re-gathering re-runs (no cache hit) and refreshes the pack.
        let regathered = gather_login_pack(dir.path()).await;
        match regathered {
            ToolOutput::Ok { content } => {
                assert!(!content.contains("cache hit"), "{content}");
            }
            ToolOutput::Error { message } => panic!("{message}"),
        }
    }

    #[tokio::test]
    async fn list_and_read_modes_roundtrip() {
        let dir = workspace();
        assert!(!gather_login_pack(dir.path()).await.is_error());

        let list = GatherContext
            .execute(&serde_json::json!({}), dir.path())
            .await;
        match list {
            ToolOutput::Ok { content } => {
                assert!(content.contains("`login-flow`"), "{content}");
                assert!(content.contains("Map the login flow"), "{content}");
            }
            ToolOutput::Error { message } => panic!("{message}"),
        }

        let read = GatherContext
            .execute(&serde_json::json!({"pack": "login-flow"}), dir.path())
            .await;
        match read {
            ToolOutput::Ok { content } => {
                assert!(content.contains("fresh"), "{content}");
                assert!(content.contains("Matches for `login`"), "{content}");
            }
            ToolOutput::Error { message } => panic!("{message}"),
        }
    }

    #[tokio::test]
    async fn missing_goal_and_bad_slug_are_named_errors_and_unknown_pack_lists() {
        let dir = workspace();
        let no_goal = GatherContext
            .execute(&serde_json::json!({"patterns": ["x"]}), dir.path())
            .await;
        assert!(no_goal.is_error(), "gather without goal must error");

        let bad_slug = GatherContext
            .execute(
                &serde_json::json!({"goal": "g", "patterns": ["x"], "save_as": "../escape"}),
                dir.path(),
            )
            .await;
        assert!(bad_slug.is_error(), "path-unsafe slug must be rejected");

        let missing = GatherContext
            .execute(&serde_json::json!({"pack": "nope"}), dir.path())
            .await;
        assert!(missing.is_error(), "unknown pack is a named error");
    }

    #[test]
    fn request_key_is_order_insensitive_and_goal_independent() {
        let mut a = PackInputs {
            symbols: vec!["b".into(), "a".into()],
            patterns: vec!["p2".into(), "p1".into()],
            globs: vec![],
            files: vec![],
        };
        let mut b = PackInputs {
            symbols: vec!["a".into(), "b".into(), "a".into()],
            patterns: vec!["p1".into(), "p2".into()],
            globs: vec![],
            files: vec![],
        };
        a.canonicalize();
        b.canonicalize();
        assert_eq!(
            a.request_key(),
            b.request_key(),
            "the same logical sweep must be the same pack identity"
        );
    }

    #[test]
    fn schema_is_read_only_and_named() {
        let schema = GatherContext.schema();
        assert_eq!(schema.name, "gather_context");
        assert!(schema.read_only);
    }

    #[test]
    fn derive_slug_is_path_safe_or_key_prefixed() {
        assert_eq!(
            derive_slug("Map the Login Flow!", "abcdef1234"),
            "map-the-login-flow"
        );
        assert_eq!(derive_slug("!!!", "abcdef1234"), "ctx-abcdef12");
        assert!(valid_slug(&derive_slug(
            "Map the Login Flow!",
            "abcdef1234"
        )));
    }
}
