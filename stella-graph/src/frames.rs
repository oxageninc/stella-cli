//! Query → [`ContextFrame`] assembly: the code graph as a built-in CGP
//! provider.
//!
//! Two rules from the lessons registry are load-bearing here:
//! - **L-C4 (cite by human label):** every frame's `title` and mandatory
//!   `citation_label` are human strings like `fn run_turn
//!   (stella-core/src/driver.rs:160)`; the structured `id` is never presented
//!   as the primary identifier, and graph relations carry a `display_name`.
//! - **Provenance carries the provider id `code-graph`** (task brief): every
//!   frame's provenance chain ends in a derivation entry attributing it to
//!   this provider.
//!
//! These are synchronous SQLite reads plus small, bounded file reads (to quote
//! a definition's source or a reference's line); `stella-context` wraps them
//! for its async `ContextPlane`. Budget-packing here mirrors the retrieval
//! contract ( / L-C5): frames declare a
//! `token_cost` and assembly never exceeds `max_tokens`/`max_frames`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use contextgraph_types::{ContextFrame, ContextQuery, FrameKind, Provenance, Relation};
use rusqlite::Connection;

use crate::error::GraphError;
use crate::import::{self, ImportKind};
use crate::store::{self, DefRow, ImportRow};

/// The CGP provider id this crate reports in every frame's provenance.
pub const PROVIDER_ID: &str = "code-graph";

/// Semantic version of the extractor, embedded in derivation provenance.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Max lines of source quoted into a definition frame's content.
const SNIPPET_MAX_LINES: usize = 60;
/// Max reference occurrences scanned/returned by [`references`].
const MAX_REFERENCES: usize = 50;
/// Max chars of a reference line quoted into a frame.
const REF_LINE_MAX_CHARS: usize = 200;

// Score bands (all in `[0,1]`, `ContextFrame::has_valid_score`): a direct
// definition match is the strongest code-graph signal, a graph neighborhood
// medium, a best-effort textual reference the weakest.
const SCORE_DEFINITION: f32 = 0.9;
const SCORE_NEIGHBOR_SYMBOL: f32 = 0.7;
const SCORE_GRAPH_EDGE: f32 = 0.6;
const SCORE_REFERENCE: f32 = 0.5;

/// Frames for every definition of `name`.
pub(crate) fn definitions(
    conn: &Connection,
    root: &Path,
    name: &str,
) -> Result<Vec<ContextFrame>, GraphError> {
    let rows = store::definitions(conn, name)?;
    Ok(rows
        .into_iter()
        .map(|row| symbol_frame(root, &row, SCORE_DEFINITION))
        .collect())
}

/// Best-effort textual references: whole-word occurrences of `name` across
/// indexed files. Linear in the indexed corpus and capped at
/// [`MAX_REFERENCES`] — the spec labels this "best-effort" retrieval, not an
/// index ( CLI surface note).
pub(crate) fn references(
    conn: &Connection,
    root: &Path,
    name: &str,
) -> Result<Vec<ContextFrame>, GraphError> {
    let files = store::all_files(conn)?;
    let mut out = Vec::new();
    'outer: for rel in files {
        let abs = root.join(&rel);
        let content = match std::fs::read_to_string(&abs) {
            Ok(content) => content,
            Err(_) => continue,
        };
        for (idx, line) in content.lines().enumerate() {
            if out.len() >= MAX_REFERENCES {
                break 'outer;
            }
            if !line_contains_word(line, name) {
                continue;
            }
            let line_no = idx as u32 + 1;
            let snippet = truncate_chars(line.trim(), REF_LINE_MAX_CHARS);
            let citation = format!("{rel}:{line_no}");
            let provenance = vec![
                file_provenance(&file_uri(root, &rel), Some(format!("L{line_no}")), None),
                derivation("tree-sitter/reference-scan"),
            ];
            out.push(frame(
                format!("{PROVIDER_ID}:ref:{rel}:{line_no}:{name}"),
                FrameKind::Snippet,
                format!("reference to {name}"),
                snippet,
                Some(file_uri(root, &rel)),
                SCORE_REFERENCE,
                citation,
                provenance,
                Vec::new(),
            ));
        }
    }
    Ok(out)
}

/// A single graph frame listing the imports out of `rel` (empty if the file
/// has no recorded imports).
pub(crate) fn imports_of(
    conn: &Connection,
    root: &Path,
    rel: &str,
) -> Result<Vec<ContextFrame>, GraphError> {
    let rows = store::imports_from(conn, rel)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![edge_frame(
        format!("{PROVIDER_ID}:imports:{rel}"),
        format!("imports of {rel}"),
        &edge_listing(&rows),
        file_uri(root, rel),
        import_relations(root, &rows, "IMPORTS"),
    )])
}

/// A single graph frame listing the files that import `rel`.
pub(crate) fn importers_of(
    conn: &Connection,
    root: &Path,
    rel: &str,
) -> Result<Vec<ContextFrame>, GraphError> {
    let importers = store::importers_of(conn, rel)?;
    if importers.is_empty() {
        return Ok(Vec::new());
    }
    let content = importers.join("\n");
    let relations = importers
        .iter()
        .map(|p| Relation {
            rel: "IMPORTED_BY".into(),
            target_uri: file_uri(root, p),
            display_name: Some(p.clone()),
        })
        .collect();
    Ok(vec![edge_frame(
        format!("{PROVIDER_ID}:importers:{rel}"),
        format!("importers of {rel}"),
        &content,
        file_uri(root, rel),
        relations,
    )])
}

/// The immediate neighborhood of a file: its defined symbols plus its import
/// and importer edges.
pub(crate) fn neighbors(
    conn: &Connection,
    root: &Path,
    rel: &str,
) -> Result<Vec<ContextFrame>, GraphError> {
    let mut out = Vec::new();
    for row in store::symbols_in_file(conn, rel)? {
        out.push(symbol_frame(root, &row, SCORE_NEIGHBOR_SYMBOL));
    }
    out.extend(imports_of(conn, root, rel)?);
    out.extend(importers_of(conn, root, rel)?);
    Ok(out)
}

/// The CGP provider entrypoint: map a [`ContextQuery`] onto code-graph
/// lookups, fuse + dedup, and budget-pack to the query's limits.
pub(crate) fn query(
    conn: &Connection,
    root: &Path,
    q: &ContextQuery,
) -> Result<Vec<ContextFrame>, GraphError> {
    let mut collected: Vec<ContextFrame> = Vec::new();

    // Identifier-like tokens in the query drive definition lookups.
    let text = q.query_text.as_deref().unwrap_or(&q.goal);
    for token in candidate_identifiers(text) {
        collected.extend(definitions(conn, root, &token)?);
    }
    // Anchors (open files / mentioned symbols) drive neighborhood lookups.
    for anchor in &q.anchors {
        if let Some(rel) = anchor_to_rel(anchor, root) {
            collected.extend(neighbors(conn, root, &rel)?);
        }
    }

    // Filter by requested kinds, dedup by id (highest score wins).
    let mut by_id: HashMap<String, ContextFrame> = HashMap::new();
    for frame in collected {
        if !q.kinds.is_empty() && !q.kinds.contains(&frame.kind) {
            continue;
        }
        match by_id.get(&frame.id) {
            Some(existing) if existing.score >= frame.score => {}
            _ => {
                by_id.insert(frame.id.clone(), frame);
            }
        }
    }

    Ok(budget_pack(
        by_id.into_values().collect(),
        q.max_frames,
        q.max_tokens,
    ))
}

// ---- frame construction --------------------------------------------------

fn symbol_frame(root: &Path, row: &DefRow, score: f32) -> ContextFrame {
    let citation = format!(
        "{} {} ({}:{})",
        row.kind.keyword(),
        row.name,
        row.path,
        row.start_line
    );
    let content = read_snippet(root, &row.path, row.start_line, row.end_line)
        .unwrap_or_else(|| citation.clone());
    let uri = file_uri(root, &row.path);
    let provenance = vec![
        file_provenance(
            &uri,
            Some(format!("L{}-{}", row.start_line, row.end_line)),
            Some(&row.sha),
        ),
        derivation("tree-sitter/symbol-extract"),
    ];
    frame(
        format!(
            "{PROVIDER_ID}:sym:{}:{}:{}",
            row.path, row.start_line, row.name
        ),
        FrameKind::Symbol,
        format!("{} {}", row.kind.keyword(), row.name),
        content,
        Some(uri),
        score,
        citation,
        provenance,
        Vec::new(),
    )
}

fn edge_frame(
    id: String,
    label: String,
    content: &str,
    uri: String,
    relations: Vec<Relation>,
) -> ContextFrame {
    let provenance = vec![
        file_provenance(&uri, None, None),
        derivation("code-graph/import-edges"),
    ];
    frame(
        id,
        FrameKind::Graph,
        label.clone(),
        content.to_string(),
        Some(uri),
        SCORE_GRAPH_EDGE,
        label,
        provenance,
        relations,
    )
}

#[allow(clippy::too_many_arguments)]
fn frame(
    id: String,
    kind: FrameKind,
    title: String,
    content: String,
    uri: Option<String>,
    score: f32,
    citation_label: String,
    provenance: Vec<Provenance>,
    relations: Vec<Relation>,
) -> ContextFrame {
    let token_cost = estimate_tokens(&content);
    ContextFrame {
        id,
        kind,
        title,
        content,
        uri,
        score,
        token_cost,
        valid_from: None,
        valid_to: None,
        recorded_at: None,
        provenance,
        citation_label: Some(citation_label),
        embedding: None,
        relations,
    }
}

fn import_relations(root: &Path, rows: &[ImportRow], rel: &str) -> Vec<Relation> {
    rows.iter()
        .map(|row| {
            let (target_uri, display_name) = match &row.to_path {
                Some(path) => (file_uri(root, path), path.clone()),
                None => (
                    format!("unresolved:{}", row.specifier),
                    row.specifier.clone(),
                ),
            };
            Relation {
                rel: rel.into(),
                target_uri,
                display_name: Some(display_name),
            }
        })
        .collect()
}

fn edge_listing(rows: &[ImportRow]) -> String {
    rows.iter()
        .map(|row| match (&row.to_path, row.kind) {
            (Some(path), _) => format!("{} → {}", row.specifier, path),
            (None, ImportKind::Bare) => format!("{} (external)", row.specifier),
            (None, _) => format!("{} (unresolved)", row.specifier),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn file_provenance(uri: &str, range: Option<String>, sha: Option<&str>) -> Provenance {
    Provenance {
        kind: "file".into(),
        uri: Some(uri.to_string()),
        range,
        digest: sha.map(|s| format!("sha256:{s}")),
        method: None,
        by: None,
    }
}

fn derivation(method: &str) -> Provenance {
    Provenance {
        kind: "derivation".into(),
        uri: None,
        range: None,
        digest: None,
        method: Some(format!("{method} ({VERSION})")),
        by: Some(PROVIDER_ID.into()),
    }
}

// ---- helpers -------------------------------------------------------------

fn file_uri(root: &Path, rel: &str) -> String {
    format!("file://{}", root.join(rel).display())
}

fn estimate_tokens(text: &str) -> u32 {
    // ~4 chars per token is the conventional cheap approximation; documented
    // as an estimate so budget accounting stays honest (L-C5).
    text.len().div_ceil(4).max(1) as u32
}

fn read_snippet(root: &Path, rel: &str, start: u32, end: u32) -> Option<String> {
    let content = std::fs::read_to_string(root.join(rel)).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start_idx = start.saturating_sub(1) as usize;
    if start_idx >= lines.len() {
        return None;
    }
    let end_idx = (end as usize)
        .min(lines.len())
        .min(start_idx + SNIPPET_MAX_LINES);
    Some(lines[start_idx..end_idx].join("\n"))
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn line_contains_word(line: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let bytes = line.as_bytes();
    let len = word.len();
    let mut from = 0;
    while let Some(pos) = line[from..].find(word) {
        let start = from + pos;
        let end = start + len;
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = end;
        if from >= line.len() {
            break;
        }
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn candidate_identifiers(text: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "fix", "add", "use", "let", "get", "set", "new", "all", "not", "are",
        "can", "you", "this", "that", "with", "from", "into", "how", "why", "what", "where",
        "does", "run", "test", "code", "file", "function", "class",
    ];
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for token in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if token.len() < 3 || STOPWORDS.contains(&token) {
            continue;
        }
        if !token
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        {
            continue;
        }
        if seen.insert(token.to_string()) {
            out.push(token.to_string());
        }
    }
    out
}

/// Turn a `file://…` (or plain path) anchor into a root-relative path, if it
/// resolves inside the workspace.
fn anchor_to_rel(anchor: &str, root: &Path) -> Option<String> {
    let raw = anchor.strip_prefix("file://").unwrap_or(anchor);
    let path = Path::new(raw);
    if let Ok(rel) = path.strip_prefix(root) {
        return Some(import::rel_to_slash(rel));
    }
    if let Ok(canonical) = path.canonicalize()
        && let Ok(rel) = canonical.strip_prefix(root)
    {
        return Some(import::rel_to_slash(rel));
    }
    // Fall back to treating the remainder as already workspace-relative; a
    // path that matches nothing just yields no frames.
    Some(raw.trim_start_matches('/').to_string())
}

fn budget_pack(frames: Vec<ContextFrame>, max_frames: u32, max_tokens: u32) -> Vec<ContextFrame> {
    let mut frames = frames;
    frames.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut out = Vec::new();
    let mut tokens: u64 = 0;
    for frame in frames {
        if out.len() as u32 >= max_frames {
            break;
        }
        let cost = frame.token_cost as u64;
        if tokens + cost > max_tokens as u64 {
            continue;
        }
        tokens += cost;
        out.push(frame);
    }
    out
}
