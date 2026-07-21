//! `stella memory` — the human window into the memory-citation loop.
//!
//! `list` ranks the project's memories most-cited first, joining the
//! citation feedback the agent recorded (`cite_memory` → `.stella/private/store.db`)
//! with the memories themselves (`.stella/private/context.db`), and flags the ones
//! that have earned rule promotion. `promote <id>` turns an eligible memory
//! into a project rule at `.stella/rules/<slug>.md` — the rules engine's own
//! authoring format (`stella_core::rules`), so the promoted file is loaded
//! and parsed exactly like a hand-written rule.
//!
//! Like `stella stats`, both subcommands read local state only and need no
//! API key; `list` never creates a database as a side effect. Promotion is
//! deliberately explicit and human-invoked: eligibility is computed
//! automatically, the write only happens here.

use clap::ValueEnum;
use colored::Colorize;
use serde::Serialize;
use stella_context::{ContextStore, NodeKind, NodeRow};
use stella_core::rules::{self, PromoteStatus, RuleCandidate};
use stella_store::{MemoryCitationStats, PROMOTION_CITATIONS_REQUIRED, Store};

/// Output format for `stella memory list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MemoryFormat {
    /// Aligned human-readable columns (default).
    Table,
    /// Pretty-printed JSON array of per-memory rows.
    Json,
}

/// One memory in the inspection view: the stable id citations tie to, the
/// citation aggregates (zeroed when never cited), and the memory text.
/// Field order is the `--format json` serialization contract — append new
/// fields at the end, never reorder.
#[derive(Debug, Clone, Serialize)]
struct MemoryListRow {
    id: String,
    citations: i64,
    avg_score: f64,
    truthful_rate: f64,
    positive_streak: i64,
    eligible: bool,
    #[serde(skip_serializing_if = "is_false")]
    quarantined: bool,
    memory: String,
}

fn is_false(b: &bool) -> bool {
    !b
}

/// Entry point for `stella memory list`.
pub fn run_memory_list(format: MemoryFormat) -> Result<(), String> {
    let workspace_root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;
    let rows = list_rows(&workspace_root)?;

    if rows.is_empty() {
        let message = "No memories recorded yet — run `stella chat`/`stella run` and let the \
                       post-turn reflection populate .stella/private/context.db.";
        match format {
            MemoryFormat::Table => println!("{message}"),
            MemoryFormat::Json => {
                eprintln!("{message}");
                println!("[]");
            }
        }
        return Ok(());
    }

    match format {
        MemoryFormat::Table => {
            print!("{}", render_table(&rows));
            let eligible = rows.iter().filter(|r| r.eligible).count();
            if eligible > 0 {
                println!(
                    "\n{} {eligible} memory(ies) eligible for rule promotion — \
                     `stella memory promote <id>` writes .stella/rules/<slug>.md",
                    "✦".magenta()
                );
            }
            let quarantined = rows.iter().filter(|r| r.quarantined).count();
            if quarantined > 0 {
                println!(
                    "\n{} {quarantined} quarantined memor{them} excluded from recall — \
                     cited untruthful {threshold}+ times. Review with `stella memory list --json`.",
                    "⚠".yellow(),
                    them = if quarantined == 1 { "y" } else { "ies" },
                    threshold = stella_store::QUARANTINE_NEGATIVES_THRESHOLD,
                );
            }
        }
        MemoryFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|e| format!("serialize: {e}"))?
        ),
    }
    Ok(())
}

/// The inspection rows for a workspace — [`run_memory_list`] minus the cwd
/// resolution and rendering, so tests can drive it against a temp root.
/// Read-only politeness (the `stella stats` discipline): opening either
/// store would create `.stella/` and empty databases as a side effect, so
/// a missing file reads as "nothing yet", never a write.
fn list_rows(workspace_root: &std::path::Path) -> Result<Vec<MemoryListRow>, String> {
    let Some(context_db) =
        stella_store::existing_workspace_private_sqlite_path(workspace_root, "context.db")
            .map_err(|e| format!("cannot resolve context store: {e}"))?
    else {
        return Ok(Vec::new());
    };
    let context =
        ContextStore::open(&context_db).map_err(|e| format!("cannot open context store: {e}"))?;
    let memories = context
        .memory_nodes()
        .map_err(|e| format!("cannot read memories: {e}"))?;
    let stats = citation_stats(workspace_root)?;
    Ok(join_rows(memories, &stats))
}

/// Entry point for `stella memory promote <id>`.
pub fn run_memory_promote(id: &str) -> Result<(), String> {
    let workspace_root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;
    promote_in(&workspace_root, id)
}

/// The promotion flow for a workspace — [`run_memory_promote`] minus the cwd
/// resolution, so tests can drive it end-to-end against a temp root.
fn promote_in(workspace_root: &std::path::Path, id: &str) -> Result<(), String> {
    let stats = citation_stats(workspace_root)?;
    let Some(stat) = stats.iter().find(|s| s.memory_id == id) else {
        return Err(format!(
            "memory `{id}` has never been cited — only memories the agent cited successfully \
             more than {PROMOTION_CITATIONS_REQUIRED} times can be promoted \
             (`stella memory list` shows citation counts)"
        ));
    };
    if !stat.eligible {
        return Err(format!(
            "memory `{id}` is not promotion-eligible: {} consecutive positive citation(s) since \
             its last negative remark, and strictly more than {PROMOTION_CITATIONS_REQUIRED} are \
             required ({} citation(s) total, {} negative). One negative remark resets the streak \
             until it is re-earned.",
            stat.positive_streak, stat.citations, stat.negatives
        ));
    }

    let context_db =
        stella_store::existing_workspace_private_sqlite_path(workspace_root, "context.db")
            .map_err(|e| format!("cannot resolve context store: {e}"))?
            .ok_or_else(|| {
                "no private context.db in this workspace — nothing to promote".to_string()
            })?;
    let context =
        ContextStore::open(&context_db).map_err(|e| format!("cannot open context store: {e}"))?;
    let node = context
        .node_by_public_id(id)
        .map_err(|e| format!("cannot read memory `{id}`: {e}"))?
        .ok_or_else(|| format!("memory `{id}` is not in the context store (cited id is stale?)"))?;
    if node.kind != NodeKind::Memory {
        return Err(format!(
            "`{id}` is a {} node, not a memory — only memories can become rules",
            node.kind.as_str()
        ));
    }

    let candidate = promotion_candidate(&node, stat);
    let rules_dir = workspace_root.join(".stella").join("rules");
    let path = rules_dir.join(format!("{}.md", candidate.id));
    // The pure promotion decision is the rules engine's (`decide_promotion`);
    // this command owns only the I/O halves: the exists-check and the write.
    match rules::decide_promotion(true, path.exists()) {
        PromoteStatus::AlreadyExists => Err(format!(
            "{} already exists — not clobbering a possibly hand-edited rule; delete it first \
             to re-promote",
            path.display()
        )),
        PromoteStatus::Declined => unreachable!("promote is always approved here"),
        PromoteStatus::Written => {
            std::fs::create_dir_all(&rules_dir)
                .map_err(|e| format!("could not create {}: {e}", rules_dir.display()))?;
            std::fs::write(&path, rules::render_rule_markdown(&candidate))
                .map_err(|e| format!("could not write {}: {e}", path.display()))?;
            println!(
                "  {} promoted memory {} → {}",
                "✦".magenta().bold(),
                id.bright_magenta(),
                path.display()
            );
            println!(
                "  {}",
                "the rule now loads with the workspace rules (.stella/rules/)".dimmed()
            );
            Ok(())
        }
    }
}

/// Citation stats from `.stella/private/store.db`, or empty when the store does not
/// exist yet (never create it from a read-only command).
fn citation_stats(workspace_root: &std::path::Path) -> Result<Vec<MemoryCitationStats>, String> {
    if stella_store::existing_workspace_private_sqlite_path(workspace_root, "store.db")
        .map_err(|e| format!("cannot resolve local store: {e}"))?
        .is_none()
    {
        return Ok(Vec::new());
    }
    let store = Store::open(workspace_root).map_err(|e| format!("cannot open local store: {e}"))?;
    store
        .memory_citation_stats()
        .map_err(|e| format!("cannot read memory citations: {e}"))
}

/// Join memories with their citation stats: cited memories first (already
/// most-cited-first from the store), then never-cited ones newest first (the
/// order `memory_nodes` returns). Stats for ids that no longer resolve to a
/// live memory node (superseded, or a fabricated citation) are dropped — the
/// view shows the project's memories, not raw citation rows.
fn join_rows(memories: Vec<NodeRow>, stats: &[MemoryCitationStats]) -> Vec<MemoryListRow> {
    let mut cited: Vec<MemoryListRow> = Vec::new();
    let mut uncited: Vec<MemoryListRow> = Vec::new();
    for node in &memories {
        match stats.iter().find(|s| s.memory_id == node.public_id) {
            Some(s) => cited.push(MemoryListRow {
                id: node.public_id.clone(),
                citations: s.citations,
                avg_score: s.avg_score,
                truthful_rate: s.truthful_rate,
                positive_streak: s.positive_streak,
                eligible: s.eligible,
                quarantined: s.quarantined,
                memory: node.content.trim().to_string(),
            }),
            None => uncited.push(MemoryListRow {
                id: node.public_id.clone(),
                citations: 0,
                avg_score: 0.0,
                truthful_rate: 0.0,
                positive_streak: 0,
                eligible: false,
                quarantined: false,
                memory: node.content.trim().to_string(),
            }),
        }
    }
    cited.sort_by(|a, b| b.citations.cmp(&a.citations).then_with(|| a.id.cmp(&b.id)));
    cited.extend(uncited);
    cited
}

/// One row in the validation report (Proposal 5).
#[derive(Debug, Clone, Serialize)]
struct MemoryValidationRow {
    id: String,
    status: &'static str,
    anchors_found: usize,
    anchors_missing: usize,
    memory: String,
}

/// Entry point for `stella memory validate` (Proposal 5).
///
/// Scans each memory for file-path anchors (workspace-relative paths like
/// `stella-cli/src/agent.rs`), checks whether those paths still exist, and
/// reports stale memories — ones whose referenced files have been moved or
/// deleted, a strong signal the memory is about refactored-away code.
pub fn run_memory_validate() -> Result<(), String> {
    let workspace_root =
        std::env::current_dir().map_err(|e| format!("cannot determine workspace root: {e}"))?;
    let rows = validate_memories(&workspace_root)?;
    if rows.is_empty() {
        println!("No memories to validate — .stella/private/context.db is empty or missing.");
        return Ok(());
    }

    let stale = rows.iter().filter(|r| r.status == "stale").count();
    let ok = rows.iter().filter(|r| r.status == "ok").count();
    let no_anchors = rows.iter().filter(|r| r.status == "no-anchors").count();

    for row in &rows {
        let symbol = match row.status {
            "stale" => "⚠".yellow(),
            "ok" => "✓".green(),
            _ => "·".dimmed(),
        };
        let detail = match row.status {
            "stale" => format!(
                " — {} of {} anchor path(s) missing",
                row.anchors_missing, row.anchors_found
            ),
            "ok" => format!(" — {} anchor(s) verified", row.anchors_found),
            _ => String::new(),
        };
        println!(
            "  {symbol} {} [{status}{detail}] {memory}",
            row.id,
            status = row.status,
            memory = row.memory.chars().take(60).collect::<String>()
        );
    }

    println!("\n  {ok} ok, {stale} stale, {no_anchors} no path anchors");
    if stale > 0 {
        println!(
            "\n  {} {stale} stale memor{them} reference paths that no longer exist — \
             cite them untruthful (or delete) to quarantine them from recall.",
            "⚠".yellow(),
            them = if stale == 1 { "y" } else { "ies" },
        );
    }
    Ok(())
}

/// Extract workspace-relative file-path anchors from memory text. A path
/// anchor is a token matching the pattern `word/word[/word…]` with at least
/// one slash and a recognized source extension, e.g. `stella-cli/src/agent.rs`,
/// `src/lib.rs`, `docs/hooks.md`.
fn extract_path_anchors(text: &str) -> Vec<String> {
    let exts = [
        "rs", "py", "ts", "tsx", "js", "jsx", "go", "md", "sql", "toml",
    ];
    text.split(|c: char| !(c.is_alphanumeric() || c == '/' || c == '.' || c == '-' || c == '_'))
        .filter(|tok| {
            // Trim trailing dots — a sentence like "see src/lib.rs." would
            // otherwise produce token "src/lib.rs." with extension "rs.".
            let tok = tok.trim_end_matches('.');
            tok.contains('/')
                && tok
                    .rsplit('.')
                    .next()
                    .is_some_and(|ext| exts.contains(&ext))
        })
        .map(|tok| tok.trim_end_matches('.').to_string())
        .collect()
}

/// Validate all memories in a workspace against the current file tree.
fn validate_memories(workspace_root: &std::path::Path) -> Result<Vec<MemoryValidationRow>, String> {
    let Some(context_db) =
        stella_store::existing_workspace_private_sqlite_path(workspace_root, "context.db")
            .map_err(|e| format!("cannot resolve context store: {e}"))?
    else {
        return Ok(Vec::new());
    };
    let context =
        ContextStore::open(&context_db).map_err(|e| format!("cannot open context store: {e}"))?;
    let memories = context
        .memory_nodes()
        .map_err(|e| format!("cannot read memories: {e}"))?;

    let mut rows = Vec::new();
    for node in &memories {
        let anchors = extract_path_anchors(&node.content);
        if anchors.is_empty() {
            rows.push(MemoryValidationRow {
                id: node.public_id.clone(),
                status: "no-anchors",
                anchors_found: 0,
                anchors_missing: 0,
                memory: node.content.trim().to_string(),
            });
            continue;
        }
        let missing = anchors
            .iter()
            .filter(|p| !workspace_root.join(p).exists())
            .count();
        let status = if missing > 0 { "stale" } else { "ok" };
        rows.push(MemoryValidationRow {
            id: node.public_id.clone(),
            status,
            anchors_found: anchors.len(),
            anchors_missing: missing,
            memory: node.content.trim().to_string(),
        });
    }
    Ok(rows)
}

/// The promoted rule as a mining candidate, so `render_rule_markdown`
/// produces the exact frontmatter shape `rule_from_file` parses back —
/// promotion never invents its own rule format. Prompt-only (no guard): the
/// citation loop scores usefulness, it carries no file evidence to infer a
/// safe deny-glob from.
fn promotion_candidate(node: &NodeRow, stat: &MemoryCitationStats) -> RuleCandidate {
    RuleCandidate {
        id: slugify(&node.display_name),
        description: format!(
            "Promoted from memory {} after {} citation(s) ({} consecutive positive).",
            node.public_id, stat.citations, stat.positive_streak
        ),
        text: node.content.trim().to_string(),
        occurrences: stat.citations as usize,
        salient: false,
        evidence: Vec::new(),
        guard: None,
        score: 0,
    }
}

/// Filename-safe slug from a memory's label: lowercase alphanumeric runs
/// joined by `-`, capped at 48 chars, `memory-rule` when nothing survives.
fn slugify(label: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        "memory-rule".to_string()
    } else {
        slug
    }
}

/// Column headers, in `MemoryListRow` field order (`memory` last, so the
/// one unbounded column never breaks alignment).
const TABLE_HEADERS: [&str; 7] = [
    "ID", "CITES", "AVG", "TRUTHFUL", "STREAK", "STATUS", "MEMORY",
];

fn table_cells(row: &MemoryListRow) -> [String; 7] {
    let mut memory: String = row.memory.chars().take(60).collect();
    if row.memory.chars().count() > 60 {
        memory.push('…');
    }
    // Keep the free-text column single-line so alignment holds.
    let memory = memory.replace(['\n', '\r'], " ");
    let status = if row.quarantined {
        "QUARANTINED"
    } else if row.eligible {
        "eligible"
    } else {
        "-"
    };
    [
        row.id.clone(),
        row.citations.to_string(),
        if row.citations > 0 {
            format!("{:.1}", row.avg_score)
        } else {
            "-".into()
        },
        if row.citations > 0 {
            format!("{:.0}%", row.truthful_rate * 100.0)
        } else {
            "-".into()
        },
        row.positive_streak.to_string(),
        status.to_string(),
        memory,
    ]
}

/// Aligned table, `stella stats` style: left-aligned strings, right-aligned
/// numbers, two-space gutter, no trailing whitespace.
fn render_table(rows: &[MemoryListRow]) -> String {
    let body: Vec<[String; 7]> = rows.iter().map(table_cells).collect();
    let mut widths: Vec<usize> = TABLE_HEADERS.iter().map(|h| h.len()).collect();
    for cells in &body {
        for (w, cell) in widths.iter_mut().zip(cells.iter()) {
            *w = (*w).max(cell.len());
        }
    }
    let render_line = |cells: &[String]| -> String {
        let cols: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                // Numeric middle columns right-align; ID and MEMORY stay left.
                if (1..=4).contains(&i) {
                    format!("{cell:>width$}", width = widths[i])
                } else {
                    format!("{cell:<width$}", width = widths[i])
                }
            })
            .collect();
        cols.join("  ").trim_end().to_string()
    };
    let mut out = String::new();
    let headers: Vec<String> = TABLE_HEADERS.iter().map(|h| h.to_string()).collect();
    out.push_str(&render_line(&headers));
    out.push('\n');
    for cells in &body {
        out.push_str(&render_line(cells));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn memory_node(public_id: &str, label: &str, content: &str) -> NodeRow {
        NodeRow {
            id: 1,
            public_id: public_id.into(),
            kind: NodeKind::Memory,
            display_name: label.into(),
            content: content.into(),
            content_hash: "h".into(),
            uri: None,
            valid_from: None,
            recorded_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn stat(memory_id: &str, citations: i64, streak: i64, eligible: bool) -> MemoryCitationStats {
        MemoryCitationStats {
            memory_id: memory_id.into(),
            citations,
            avg_score: 4.5,
            truthful_rate: 1.0,
            negatives: 0,
            positive_streak: streak,
            eligible,
            quarantined: false,
        }
    }

    #[test]
    fn promoted_markdown_is_parsed_back_by_the_rules_engine() {
        let node = memory_node(
            "nod_0123456789abcdef01234567",
            "Never edit generated files",
            "Never edit generated files under gen/ — regenerate them instead.",
        );
        let candidate = promotion_candidate(&node, &stat(node.public_id.as_str(), 12, 12, true));
        let markdown = rules::render_rule_markdown(&candidate);

        // The promotion contract: the file must parse through the rules
        // engine's own frontmatter parser and loader, exactly like a
        // hand-authored .stella/rules/*.md file.
        let fm = rules::parse_frontmatter(&markdown);
        assert_eq!(
            fm.data.get("description").unwrap(),
            "Promoted from memory nod_0123456789abcdef01234567 after 12 citation(s) \
             (12 consecutive positive)."
        );
        assert_eq!(fm.body, candidate.text);

        let path = format!(".stella/rules/{}.md", candidate.id);
        let rule = rules::rule_from_file(&path, &markdown).expect("a loadable rule");
        assert_eq!(rule.id, "never-edit-generated-files");
        assert_eq!(rule.text, candidate.text);
        assert!(rule.guard.is_none(), "promoted rules are prompt-only");
    }

    #[test]
    fn slugify_produces_filename_safe_stems() {
        assert_eq!(
            slugify("Never edit generated files"),
            "never-edit-generated-files"
        );
        assert_eq!(slugify("  ~~weird?? punctuation!!  "), "weird-punctuation");
        assert_eq!(slugify("///"), "memory-rule");
        assert!(slugify(&"long word ".repeat(20)).len() <= 48);
    }

    #[test]
    fn join_ranks_cited_memories_first_and_keeps_uncited_ones_visible() {
        let memories = vec![
            memory_node("nod_new", "newest, never cited", "newest, never cited"),
            memory_node("nod_two", "cited twice", "cited twice"),
            memory_node("nod_ten", "cited ten times", "cited ten times"),
        ];
        let stats = vec![stat("nod_two", 2, 2, false), stat("nod_ten", 10, 10, false)];
        let rows = join_rows(memories, &stats);
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["nod_ten", "nod_two", "nod_new"],
            "most-cited first, uncited (but inspectable) last"
        );
        assert_eq!(rows[0].citations, 10);
        assert!(!rows[0].eligible, "exactly 10 is not more than 10");
        assert_eq!(rows[2].citations, 0);
    }

    #[test]
    fn join_drops_stats_for_ids_that_are_not_live_memories() {
        let rows = join_rows(
            vec![memory_node("nod_live", "l", "l")],
            &[stat("nod_ghost", 99, 99, true)],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "nod_live");
    }

    /// The whole loop against real stores in a temp workspace: seed a
    /// memory (context.db), cite it (store.db), watch the strict >10 gate
    /// hold at exactly 10, tip it at 11, promote, and parse the written
    /// rule back through the rules engine. No cwd mutation — the `_in`/
    /// `list_rows` seams take the root directly.
    #[tokio::test]
    async fn promotion_gate_and_rule_write_work_end_to_end() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let lesson = "Never edit generated files under gen/ — regenerate them instead.";

        let context_db = stella_store::workspace_private_sqlite_path(root, "context.db").unwrap();
        let context = ContextStore::open(context_db).unwrap();
        context
            .upsert(stella_context::ContextDelta {
                memories: vec![stella_context::MemoryInput::new(
                    stella_context::MemoryKind::Note,
                    lesson,
                )],
                ..Default::default()
            })
            .await
            .unwrap();
        let id = context.memory_nodes().unwrap()[0].public_id.clone();
        drop(context);

        let cite = |store: &stella_store::Store| {
            let exec = store.begin_execution("run", "p", "zai", "glm-5.2").unwrap();
            store
                .record_memory_citations(
                    exec,
                    &[stella_store::MemoryCitationRow {
                        memory_id: id.clone(),
                        useful_score: 5,
                        truthful: true,
                        remark: "held".into(),
                    }],
                )
                .unwrap();
        };
        let store = stella_store::Store::open(root).unwrap();
        for _ in 0..10 {
            cite(&store);
        }
        // Exactly 10 successful citations: the strict gate holds.
        let err = promote_in(root, &id).unwrap_err();
        assert!(err.contains("not promotion-eligible"), "{err}");
        // The 11th tips it.
        cite(&store);
        drop(store);
        assert!(list_rows(root).unwrap()[0].eligible);
        promote_in(root, &id).unwrap();

        let rules_dir = root.join(".stella").join("rules");
        let written: Vec<std::path::PathBuf> = std::fs::read_dir(&rules_dir)
            .expect("rules dir created")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert_eq!(written.len(), 1, "exactly one rule written: {written:?}");
        let rule_path = &written[0];
        let raw = std::fs::read_to_string(rule_path).expect("promoted rule written");
        let rule = rules::rule_from_file(&rule_path.display().to_string(), &raw)
            .expect("the rules engine loads the promoted file");
        assert_eq!(rule.text, lesson);

        // Idempotence: re-promotion never clobbers the written rule.
        let err = promote_in(root, &id).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn table_flags_eligibility_and_blanks_scores_for_uncited_rows() {
        let rows = join_rows(
            vec![
                memory_node("nod_eligible", "e", "an eligible memory"),
                memory_node("nod_quiet", "q", "a never-cited memory"),
            ],
            &[stat("nod_eligible", 12, 12, true)],
        );
        let out = render_table(&rows);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("ID"));
        assert!(lines[1].contains("eligible"), "eligible row flagged: {out}");
        assert!(
            lines[2].contains("  -  "),
            "uncited rows show `-`, not fake zeros: {out}"
        );
    }

    #[test]
    fn extract_path_anchors_finds_source_paths() {
        let text = "graph_snapshot() in stella-cli/src/agent.rs computes the \
                    neighborhood. See also docs/hooks.md and src/lib.rs.";
        let anchors = extract_path_anchors(text);
        assert!(anchors.contains(&"stella-cli/src/agent.rs".to_string()));
        assert!(anchors.contains(&"docs/hooks.md".to_string()));
        assert!(anchors.contains(&"src/lib.rs".to_string()));
        // No false positives from bare words.
        assert!(!anchors.iter().any(|a| a == "graph_snapshot"));
    }

    #[test]
    fn validate_flags_stale_path_anchors() {
        let root = tempdir().unwrap();
        // Create the stores + a memory node referencing a path that exists
        // and one that does not.
        let context_db =
            stella_store::workspace_private_sqlite_path(root.path(), "context.db").unwrap();
        std::fs::create_dir_all(root.path().join("stella-cli/src")).unwrap();
        std::fs::write(root.path().join("stella-cli/src/agent.rs"), "pub fn x() {}").unwrap();
        // Write memories via the context store's async upsert.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let context = stella_context::ContextStore::open(&context_db).unwrap();
            use stella_context::{ContextDelta, MemoryInput};
            let delta = ContextDelta {
                memories: vec![
                    MemoryInput::reflection(
                        "agent.rs at stella-cli/src/agent.rs does X",
                        Vec::<String>::new(),
                    ),
                    MemoryInput::reflection(
                        "old.rs at deleted/old.rs was removed",
                        Vec::<String>::new(),
                    ),
                    MemoryInput::reflection("a memory with no paths at all", Vec::<String>::new()),
                ],
                ..Default::default()
            };
            context.upsert(delta).await.unwrap();
        });

        let rows = validate_memories(root.path()).unwrap();
        assert_eq!(rows.len(), 3);
        let stale = rows.iter().find(|r| r.status == "stale");
        assert!(stale.is_some(), "must flag the deleted path");
        let ok = rows.iter().find(|r| r.status == "ok");
        assert!(ok.is_some(), "must verify the existing path");
        let no_anchors = rows.iter().find(|r| r.status == "no-anchors");
        assert!(no_anchors.is_some(), "must flag no-anchor memories");
    }
}
