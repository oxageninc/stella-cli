//! Per-session file-touch telemetry: the CRUD event model, the ledger the
//! registry aggregates touches into, path normalization, line-diff counting,
//! and payload validation.
//!
//! Every successful file operation the agent performs through the registry
//! becomes one [`FileTouchEvent`] (its CRUD letter, the reason the agent gave,
//! and the line delta it caused). Events aggregate by normalized
//! workspace-relative path into one [`FileTouchRecord`] per file, and the
//! session payload — [`FileTouchTelemetry`] — serializes to the documented
//! JSON Schema (see `stella-docs/content/docs/telemetry/files-touched.mdx`):
//!
//! - `C` — the file went from nonexistent to existing (`lines_added` = the
//!   new file's full line count).
//! - `R` — content or metadata was successfully read (zero deltas).
//! - `U` — an existing file's content changed (deltas from a line diff of
//!   pre- vs post-write content).
//! - `D` — the file went from existing to nonexistent (`lines_removed` = the
//!   full pre-deletion line count).
//!
//! Failed operations never produce an event. Per-file `crud_events` is
//! deduplicated in first-occurrence order; `events` is the full ordered audit
//! log and is never deduplicated.

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

/// One file-lifecycle op, in CRUD display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    Create,
    Read,
    Update,
    Delete,
}

impl FileOp {
    pub fn letter(self) -> char {
        match self {
            FileOp::Create => 'C',
            FileOp::Read => 'R',
            FileOp::Update => 'U',
            FileOp::Delete => 'D',
        }
    }

    pub fn from_letter(letter: char) -> Option<Self> {
        match letter {
            'C' => Some(FileOp::Create),
            'R' => Some(FileOp::Read),
            'U' => Some(FileOp::Update),
            'D' => Some(FileOp::Delete),
            _ => None,
        }
    }
}

impl Serialize for FileOp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.letter().encode_utf8(&mut [0u8; 4]))
    }
}

impl<'de> Deserialize<'de> for FileOp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        let mut chars = s.chars();
        match (chars.next().and_then(FileOp::from_letter), chars.next()) {
            (Some(op), None) => Ok(op),
            _ => Err(serde::de::Error::custom(format!(
                "invalid CRUD event `{s}` — expected one of C, R, U, D"
            ))),
        }
    }
}

/// One entry in a file's audit log: what happened, why, and the line delta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTouchEvent {
    pub event: FileOp,
    pub reason: String,
    pub lines_added: u64,
    pub lines_removed: u64,
}

/// The session-level record for one file: its normalized path, deduplicated
/// CRUD events (first-occurrence order), line-delta totals, and the full
/// ordered audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTouchRecord {
    pub path: String,
    pub crud_events: Vec<FileOp>,
    pub lines_added: u64,
    pub lines_removed: u64,
    pub events: Vec<FileTouchEvent>,
}

/// The full session payload: one record per file touched, ordered by first
/// touch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTouchTelemetry {
    pub files_touched: Vec<FileTouchRecord>,
}

impl FileTouchTelemetry {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({"files_touched": []}))
    }

    /// Check every invariant the telemetry schema and counting rules impose.
    /// Returns all violations, not just the first — a payload assembled by
    /// [`FileTouchLedger`] always passes; this is the independent check for
    /// payloads that crossed a serialization boundary.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut violations = Vec::new();
        let mut seen_paths = std::collections::HashSet::new();
        for record in &self.files_touched {
            let path = &record.path;
            if path.is_empty() {
                violations.push("record with empty path".into());
            }
            if !seen_paths.insert(path.clone()) {
                violations.push(format!("duplicate record for path `{path}`"));
            }
            if path.starts_with('/')
                || path.contains('\\')
                || path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
            {
                violations.push(format!(
                    "path `{path}` is not a normalized workspace-relative POSIX path"
                ));
            }
            if record.events.is_empty() {
                violations.push(format!("`{path}`: events must not be empty"));
            }
            // crud_events must be exactly the first-occurrence dedup of the
            // audit log — which also implies non-empty and unique.
            let mut first_occurrence = Vec::new();
            for event in &record.events {
                if !first_occurrence.contains(&event.event) {
                    first_occurrence.push(event.event);
                }
            }
            if record.crud_events != first_occurrence {
                violations.push(format!(
                    "`{path}`: crud_events {:?} != first-occurrence order {:?} of the audit log",
                    record.crud_events, first_occurrence
                ));
            }
            let mut sum_added = 0u64;
            let mut sum_removed = 0u64;
            for event in &record.events {
                sum_added += event.lines_added;
                sum_removed += event.lines_removed;
                if event.reason.is_empty() {
                    violations.push(format!(
                        "`{path}`: event {:?} has empty reason",
                        event.event
                    ));
                }
                let zero_rule = match event.event {
                    FileOp::Read => event.lines_added == 0 && event.lines_removed == 0,
                    FileOp::Create => event.lines_removed == 0,
                    FileOp::Delete => event.lines_added == 0,
                    FileOp::Update => true,
                };
                if !zero_rule {
                    violations.push(format!(
                        "`{path}`: {:?} event violates its zero-delta rule (+{} -{})",
                        event.event, event.lines_added, event.lines_removed
                    ));
                }
            }
            if record.lines_added != sum_added || record.lines_removed != sum_removed {
                violations.push(format!(
                    "`{path}`: totals (+{} -{}) != event sums (+{sum_added} -{sum_removed})",
                    record.lines_added, record.lines_removed
                ));
            }
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

/// The session's file-touch ledger. One instance lives behind the registry's
/// mutex; every mutation happens under that single lock, so an appended event
/// and its aggregate updates land atomically even when tools run
/// concurrently.
#[derive(Debug, Default)]
pub struct FileTouchLedger {
    records: Vec<FileTouchRecord>,
    /// Total events appended across every record — the ledger's revision.
    /// Assigned under the registry's lock, so `files_touched.updated`
    /// consumers can order aggregate snapshots even when the emissions of
    /// concurrent touches interleave after the lock is released.
    events_total: u64,
}

impl FileTouchLedger {
    /// Append one successful touch for a normalized path: grows that path's
    /// audit log, its first-occurrence CRUD set, and its line-delta totals in
    /// one step. Aggregation is strictly by the normalized path string.
    pub fn record(&mut self, path: String, event: FileTouchEvent) {
        let record = match self.records.iter_mut().find(|r| r.path == path) {
            Some(existing) => existing,
            None => {
                self.records.push(FileTouchRecord {
                    path,
                    crud_events: Vec::new(),
                    lines_added: 0,
                    lines_removed: 0,
                    events: Vec::new(),
                });
                self.records.last_mut().expect("just pushed")
            }
        };
        if !record.crud_events.contains(&event.event) {
            record.crud_events.push(event.event);
        }
        record.lines_added += event.lines_added;
        record.lines_removed += event.lines_removed;
        record.events.push(event);
        self.events_total += 1;
    }

    /// The ledger revision: total events ever appended, monotonic under the
    /// owning lock.
    pub fn revision(&self) -> u64 {
        self.events_total
    }

    /// The aggregate record for one normalized path, if touched.
    pub fn get(&self, path: &str) -> Option<&FileTouchRecord> {
        self.records.iter().find(|r| r.path == path)
    }

    /// The full session payload, records ordered by first touch.
    pub fn snapshot(&self) -> FileTouchTelemetry {
        FileTouchTelemetry {
            files_touched: self.records.clone(),
        }
    }

    /// Legacy compact view: `(path, ops)` pairs with the ops letters in CRUD
    /// display order (e.g. `"RU"`) — what the TUI panel and episodic memory
    /// have always consumed.
    pub fn compact(&self) -> Vec<(String, String)> {
        self.records
            .iter()
            .map(|record| {
                let ordered = [FileOp::Create, FileOp::Read, FileOp::Update, FileOp::Delete]
                    .into_iter()
                    .filter(|op| record.crud_events.contains(op))
                    .map(FileOp::letter)
                    .collect::<String>();
                (record.path.clone(), ordered)
            })
            .collect()
    }

    /// Number of distinct files touched so far.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Normalize a model-supplied path to the workspace-relative POSIX form the
/// ledger aggregates by: resolved against the workspace root (collapsing `.`
/// and `..`, following the same escape rules every tool enforces), stripped
/// back to relative, and joined with `/`. Returns `None` when the path
/// escapes the workspace or resolves to the root itself.
pub fn normalize_workspace_path(root: &Path, raw: &str) -> Option<String> {
    let full = crate::resolve_within_root(root, raw)?;
    let canon_root = root.canonicalize().ok()?;
    let rel = full.strip_prefix(&canon_root).ok()?;
    let mut parts: Vec<&str> = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(name) => parts.push(name.to_str()?),
            // resolve_within_root output is an absolute path under the
            // canonical root; anything else here means it wasn't resolvable
            // to a plain file path.
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Line count as the telemetry defines it: the number of `str::lines()`
/// items, so `""` is 0 lines and a trailing newline does not add one.
pub fn count_lines(content: &str) -> u64 {
    content.lines().count() as u64
}

/// Beyond this many cell comparisons the exact LCS falls back to the
/// replace-everything upper bound — deterministic and cheap on
/// pathologically large single-file rewrites.
const LCS_AREA_CAP: usize = 4_000_000;

/// Line-based diff counts between two file contents: `(added, removed)`.
/// Common prefix/suffix lines are trimmed first; the middle is compared with
/// an exact longest-common-subsequence, so `added = new − LCS` and
/// `removed = old − LCS` — the minimal line edit script.
pub fn line_diff(old: &str, new: &str) -> (u64, u64) {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let mut start = 0;
    while start < old_lines.len() && start < new_lines.len() && old_lines[start] == new_lines[start]
    {
        start += 1;
    }
    let mut old_end = old_lines.len();
    let mut new_end = new_lines.len();
    while old_end > start && new_end > start && old_lines[old_end - 1] == new_lines[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }

    let old_mid = &old_lines[start..old_end];
    let new_mid = &new_lines[start..new_end];
    if old_mid.is_empty() || new_mid.is_empty() {
        return (new_mid.len() as u64, old_mid.len() as u64);
    }
    if old_mid.len().saturating_mul(new_mid.len()) > LCS_AREA_CAP {
        return (new_mid.len() as u64, old_mid.len() as u64);
    }

    // Two-row LCS length DP over the trimmed middle.
    let mut prev = vec![0usize; new_mid.len() + 1];
    let mut curr = vec![0usize; new_mid.len() + 1];
    for old_line in old_mid {
        for (j, new_line) in new_mid.iter().enumerate() {
            curr[j + 1] = if old_line == new_line {
                prev[j] + 1
            } else {
                prev[j + 1].max(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    let lcs = prev[new_mid.len()];
    ((new_mid.len() - lcs) as u64, (old_mid.len() - lcs) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_lines_matches_str_lines_semantics() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("a\nb\nc\n"), 3);
        assert_eq!(count_lines("\n\n"), 2);
    }

    #[test]
    fn line_diff_counts_minimal_edits() {
        assert_eq!(line_diff("", ""), (0, 0));
        assert_eq!(line_diff("a\nb\n", "a\nb\n"), (0, 0));
        // Pure additions and removals.
        assert_eq!(line_diff("", "a\nb\nc\n"), (3, 0));
        assert_eq!(line_diff("a\nb\nc\n", ""), (0, 3));
        assert_eq!(line_diff("a\nc\n", "a\nb\nc\n"), (1, 0));
        assert_eq!(line_diff("a\nb\nc\n", "a\nc\n"), (0, 1));
        // Replacement in the middle: one line becomes two.
        assert_eq!(line_diff("a\nb\nc\n", "a\nx\ny\nc\n"), (2, 1));
        // A moved line is one removal plus one addition, not zero.
        assert_eq!(line_diff("a\nb\nc\n", "b\na\nc\n"), (1, 1));
        // Repeated lines: LCS keeps the common pair.
        assert_eq!(line_diff("x\nx\n", "x\nx\nx\n"), (1, 0));
    }

    #[test]
    fn line_diff_huge_input_falls_back_to_replace_bound() {
        // Middles larger than the LCS area cap take the deterministic
        // upper-bound path: everything unshared counts as replaced.
        let old: String = (0..3000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..3000).map(|i| format!("new {i}\n")).collect();
        assert_eq!(line_diff(&old, &new), (3000, 3000));
    }

    #[test]
    fn ledger_aggregates_by_path_and_orders_crud_by_first_occurrence() {
        let mut ledger = FileTouchLedger::default();
        let touch = |op, added, removed| FileTouchEvent {
            event: op,
            reason: "test".into(),
            lines_added: added,
            lines_removed: removed,
        };
        ledger.record("a.rs".into(), touch(FileOp::Read, 0, 0));
        ledger.record("a.rs".into(), touch(FileOp::Update, 3, 1));
        ledger.record("a.rs".into(), touch(FileOp::Read, 0, 0));
        ledger.record("a.rs".into(), touch(FileOp::Update, 2, 2));
        ledger.record("b.rs".into(), touch(FileOp::Create, 10, 0));

        let payload = ledger.snapshot();
        payload.validate().expect("ledger output is always valid");
        assert_eq!(payload.files_touched.len(), 2);
        let a = &payload.files_touched[0];
        assert_eq!(a.path, "a.rs");
        assert_eq!(a.crud_events, vec![FileOp::Read, FileOp::Update]);
        assert_eq!(a.events.len(), 4, "audit log is never deduplicated");
        assert_eq!((a.lines_added, a.lines_removed), (5, 3));
        assert_eq!(
            ledger.compact(),
            vec![
                ("a.rs".to_string(), "RU".to_string()),
                ("b.rs".to_string(), "C".to_string()),
            ]
        );
    }

    #[test]
    fn serializes_to_the_documented_schema_shape() {
        let mut ledger = FileTouchLedger::default();
        ledger.record(
            "src/render.rs".into(),
            FileTouchEvent {
                event: FileOp::Update,
                reason: "Add status indicator".into(),
                lines_added: 18,
                lines_removed: 4,
            },
        );
        let json = ledger.snapshot().to_json();
        assert_eq!(
            json,
            serde_json::json!({
                "files_touched": [{
                    "path": "src/render.rs",
                    "crud_events": ["U"],
                    "lines_added": 18,
                    "lines_removed": 4,
                    "events": [{
                        "event": "U",
                        "reason": "Add status indicator",
                        "lines_added": 18,
                        "lines_removed": 4
                    }]
                }]
            })
        );
        // And the payload round-trips through its serialized form.
        let back: FileTouchTelemetry = serde_json::from_value(json).unwrap();
        assert_eq!(back, ledger.snapshot());
    }

    #[test]
    fn validate_flags_every_schema_violation() {
        let event = |op, reason: &str, added, removed| FileTouchEvent {
            event: op,
            reason: reason.into(),
            lines_added: added,
            lines_removed: removed,
        };
        let record = |path: &str, crud: Vec<FileOp>, added, removed, events| FileTouchRecord {
            path: path.into(),
            crud_events: crud,
            lines_added: added,
            lines_removed: removed,
            events,
        };
        let cases: Vec<(FileTouchTelemetry, &str)> = vec![
            (
                FileTouchTelemetry {
                    files_touched: vec![record(
                        "",
                        vec![FileOp::Read],
                        0,
                        0,
                        vec![event(FileOp::Read, "r", 0, 0)],
                    )],
                },
                "empty path",
            ),
            (
                FileTouchTelemetry {
                    files_touched: vec![
                        record(
                            "a.rs",
                            vec![FileOp::Read],
                            0,
                            0,
                            vec![event(FileOp::Read, "r", 0, 0)],
                        ),
                        record(
                            "a.rs",
                            vec![FileOp::Read],
                            0,
                            0,
                            vec![event(FileOp::Read, "r", 0, 0)],
                        ),
                    ],
                },
                "duplicate record",
            ),
            (
                FileTouchTelemetry {
                    files_touched: vec![record(
                        "../a.rs",
                        vec![FileOp::Read],
                        0,
                        0,
                        vec![event(FileOp::Read, "r", 0, 0)],
                    )],
                },
                "not a normalized",
            ),
            (
                FileTouchTelemetry {
                    files_touched: vec![record("a.rs", vec![], 0, 0, vec![])],
                },
                "events must not be empty",
            ),
            (
                // crud_events in CRUD-display order instead of first-occurrence.
                FileTouchTelemetry {
                    files_touched: vec![record(
                        "a.rs",
                        vec![FileOp::Read, FileOp::Update],
                        1,
                        0,
                        vec![
                            event(FileOp::Update, "u", 1, 0),
                            event(FileOp::Read, "r", 0, 0),
                        ],
                    )],
                },
                "first-occurrence order",
            ),
            (
                // Read carrying a delta.
                FileTouchTelemetry {
                    files_touched: vec![record(
                        "a.rs",
                        vec![FileOp::Read],
                        1,
                        0,
                        vec![event(FileOp::Read, "r", 1, 0)],
                    )],
                },
                "zero-delta rule",
            ),
            (
                // Totals disagree with the event sums.
                FileTouchTelemetry {
                    files_touched: vec![record(
                        "a.rs",
                        vec![FileOp::Update],
                        5,
                        0,
                        vec![event(FileOp::Update, "u", 1, 0)],
                    )],
                },
                "event sums",
            ),
            (
                FileTouchTelemetry {
                    files_touched: vec![record(
                        "a.rs",
                        vec![FileOp::Read],
                        0,
                        0,
                        vec![event(FileOp::Read, "", 0, 0)],
                    )],
                },
                "empty reason",
            ),
        ];
        for (payload, expected) in cases {
            let violations = payload.validate().expect_err(expected);
            assert!(
                violations.iter().any(|v| v.contains(expected)),
                "expected a violation containing `{expected}`, got {violations:?}"
            );
        }
    }

    #[test]
    fn normalize_collapses_dots_and_rejects_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

        for equivalent in [
            "src/main.rs",
            "src/./main.rs",
            "src/../src/main.rs",
            "./src/main.rs",
        ] {
            assert_eq!(
                normalize_workspace_path(root, equivalent).as_deref(),
                Some("src/main.rs"),
                "`{equivalent}` should normalize to src/main.rs"
            );
        }
        // Nonexistent files normalize too (write_file targets).
        assert_eq!(
            normalize_workspace_path(root, "src/./new_file.rs").as_deref(),
            Some("src/new_file.rs")
        );
        // Escapes and the root itself do not.
        assert_eq!(normalize_workspace_path(root, "../outside.txt"), None);
        assert_eq!(normalize_workspace_path(root, "src/../.."), None);
        assert_eq!(normalize_workspace_path(root, "."), None);
    }
}
