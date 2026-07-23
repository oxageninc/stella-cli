//! `apply_edits` — transactional multi-file edits with a validate-first
//! preflight (#333). A batch of `old_string → new_string` edits — across
//! multiple files — either applies **in full** or not at all:
//!
//! 1. **Validate**: every edit is resolved against an in-memory simulation
//!    of the current files (edits to the same file compose in order). If ANY
//!    edit fails, a structured per-edit report comes back and **nothing** is
//!    written. Match failures reuse the read→edit drift attribution (#331):
//!    a stale `old_string` on a file that changed out-of-band is reported as
//!    drift, not a generic miss.
//! 2. **Apply**: only when every edit validated, each touched file is
//!    written once with its final content. If a write fails midway (disk
//!    full, permissions), already-written files are **rolled back** to their
//!    original bytes so the tree is never left half-applied.
//!
//! One transactional call also fits the engine's concurrency model better
//! than N serial `edit_file` barriers: mutating tools are never
//! parallelized, so a five-file rename is five sequential steps today and
//! one step through this tool.
//!
//! Storage-definition files (the schema gate's territory) are refused
//! loudly: the gate judges one simulated post-edit file per call, and
//! bypassing it through a batch would un-guard the storage map. `edit_file`
//! remains the (gated) path for schema files.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::read::ReadLedger;
use crate::registry::Tool;

/// Hard ceiling on edits per call — enough for a wide rename, small enough
/// that a runaway batch can't hold the whole tree in memory.
const MAX_EDITS: usize = 64;

#[derive(Default)]
pub struct ApplyEdits {
    ledger: Arc<ReadLedger>,
}

impl ApplyEdits {
    /// Construct sharing the registry's read-state ledger, so validate-phase
    /// match failures are drift-attributed against what the model last saw.
    pub fn with_ledger(ledger: Arc<ReadLedger>) -> Self {
        Self { ledger }
    }
}

struct ParsedEdit {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
}

/// One edit's validate-phase outcome, rendered into the per-edit report.
enum EditVerdict {
    /// Resolves cleanly; `occurrences` will be replaced.
    Ok {
        occurrences: usize,
    },
    Failed {
        reason: String,
    },
}

fn parse_edits(input: &Value) -> Result<Vec<ParsedEdit>, String> {
    let edits = input
        .get("edits")
        .and_then(|v| v.as_array())
        .ok_or("missing required field `edits` (array of {path, old_string, new_string})")?;
    if edits.is_empty() {
        return Err("`edits` must not be empty".into());
    }
    if edits.len() > MAX_EDITS {
        return Err(format!(
            "`edits` has {} entries — the ceiling is {MAX_EDITS} per call; split the batch",
            edits.len()
        ));
    }
    edits
        .iter()
        .enumerate()
        .map(|(i, edit)| {
            let field = |key: &str| -> Result<String, String> {
                edit.get(key)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .ok_or(format!("edit {i}: missing required field `{key}`"))
            };
            let parsed = ParsedEdit {
                path: field("path")?,
                old_string: field("old_string")?,
                new_string: field("new_string")?,
                replace_all: edit
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            };
            if parsed.old_string.is_empty() {
                // Same destructive-empty-match hazard `edit_file` refuses.
                return Err(format!(
                    "edit {i}: old_string must not be empty — use write_file to create or \
                     replace a whole file"
                ));
            }
            Ok(parsed)
        })
        .collect()
}

#[async_trait]
impl Tool for ApplyEdits {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "apply_edits".into(),
            description: "Apply multiple exact-substring edits — across multiple files — in one transactional call. Every edit is validated first; if any fails, NOTHING is written and a per-edit report explains why. Edits to the same file compose in order. Set dry_run to validate without writing.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "description": "Edits applied in order; each targets one file",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "File path relative to workspace root" },
                                "old_string": { "type": "string", "description": "Exact text to find" },
                                "new_string": { "type": "string", "description": "Replacement text" },
                                "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)" }
                            },
                            "required": ["path", "old_string", "new_string"]
                        }
                    },
                    "dry_run": { "type": "boolean", "description": "Validate every edit and report, but write nothing (default false)" },
                    "reason": { "type": "string", "description": "Why you are editing these files — recorded in the session's file-touch audit log" }
                },
                "required": ["edits"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let edits = match parse_edits(input) {
            Ok(edits) => edits,
            Err(message) => return ToolOutput::Error { message },
        };
        let dry_run = input
            .get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // ---- Phase 1: validate everything against an in-memory simulation.
        // `files` maps normalized-path key → (display path, full path,
        // original bytes, simulated current bytes). Edits to one file
        // compose: edit 3 sees edit 1's result, exactly as the apply will.
        let mut files: HashMap<String, (String, std::path::PathBuf, String, String)> =
            HashMap::new();
        let mut order: Vec<String> = Vec::new(); // first-touch order, for stable output
        let mut verdicts: Vec<EditVerdict> = Vec::new();
        let mut failures = 0usize;

        for edit in &edits {
            let verdict = 'verdict: {
                let Some(full) = crate::resolve_within_root(root, &edit.path) else {
                    break 'verdict EditVerdict::Failed {
                        reason: format!("path `{}` escapes workspace root", edit.path),
                    };
                };
                let key = crate::file_touch::normalize_workspace_path(root, &edit.path)
                    .unwrap_or_else(|| edit.path.clone());
                if !files.contains_key(&key) {
                    match tokio::fs::read_to_string(&full).await {
                        Ok(content) => {
                            order.push(key.clone());
                            files.insert(
                                key.clone(),
                                (edit.path.clone(), full.clone(), content.clone(), content),
                            );
                        }
                        Err(e) => {
                            break 'verdict EditVerdict::Failed {
                                reason: format!("failed to read `{}`: {e}", edit.path),
                            };
                        }
                    }
                }
                let (_, _, original, simulated) = files.get_mut(&key).expect("inserted above");
                let count = simulated.matches(&edit.old_string).count();
                if count == 0 {
                    // A prior edit in this batch consuming the match is a
                    // composition mistake, not drift — say which it is.
                    let attribution = if original.contains(&edit.old_string) {
                        " — a PRIOR edit in this batch already changed that region; edits to \
                         one file compose in order, so target the intermediate content"
                    } else {
                        // Reuse the #331 drift attribution against the
                        // original disk bytes the batch started from.
                        let original_sha = crate::staleness::hex_sha256(original.as_bytes());
                        match self.ledger.last_seen_sha(root, &edit.path) {
                            Some(seen) if seen != original_sha => {
                                " — the file CHANGED after you last read it (out-of-band \
                                 modification); re-read it before retrying"
                            }
                            Some(_) => {
                                " — the file is unchanged since you last saw it; check for \
                                 exact whitespace/newline differences"
                            }
                            None => {
                                " — no read of this file is recorded this session; read it first"
                            }
                        }
                    };
                    break 'verdict EditVerdict::Failed {
                        reason: format!("old_string not found in `{}`{attribution}", edit.path),
                    };
                }
                if count > 1 && !edit.replace_all {
                    break 'verdict EditVerdict::Failed {
                        reason: format!(
                            "old_string appears {count} times in `{}` — set replace_all=true or \
                             provide a more specific string",
                            edit.path
                        ),
                    };
                }
                *simulated = if edit.replace_all {
                    simulated.replace(&edit.old_string, &edit.new_string)
                } else {
                    simulated.replacen(&edit.old_string, &edit.new_string, 1)
                };
                EditVerdict::Ok { occurrences: count }
            };
            if matches!(verdict, EditVerdict::Failed { .. }) {
                failures += 1;
            }
            verdicts.push(verdict);
        }

        let report = |verdicts: &[EditVerdict]| -> String {
            verdicts
                .iter()
                .enumerate()
                .map(|(i, v)| match v {
                    EditVerdict::Ok { occurrences } => format!(
                        "  edit {i} ({}): ok — {occurrences} occurrence(s)",
                        edits[i].path
                    ),
                    EditVerdict::Failed { reason } => format!("  edit {i}: FAILED — {reason}"),
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        if failures > 0 {
            return ToolOutput::Error {
                message: format!(
                    "{failures} of {} edits failed validation — NOTHING was written \
                     (all-or-nothing):\n{}",
                    edits.len(),
                    report(&verdicts)
                ),
            };
        }

        // Storage-gate territory check: the registry's schema gate judges ONE
        // simulated post-edit file per `write_file`/`edit_file` call; a batch
        // that lands storage definitions would slip past it. Mirror the
        // gate's own firing condition (extraction of the proposed content is
        // non-empty) and refuse loudly — `edit_file` remains the gated path.
        // Plain code files extract empty (the adapters are marker-gated) and
        // pass through untouched.
        if let Some(extractor) = crate::schema_gate::extractor() {
            let storage_hits: Vec<&str> = order
                .iter()
                .filter_map(|key| {
                    let (path, _, _, simulated) = &files[key];
                    (crate::schema_gate::is_schema_file(path)
                        && !extractor.extract(path, simulated).is_empty())
                    .then_some(path.as_str())
                })
                .collect();
            if !storage_hits.is_empty() {
                return ToolOutput::Error {
                    message: format!(
                        "batch touches storage definitions in {} — the storage gate judges \
                         single edits only, so apply these through edit_file; NOTHING was \
                         written",
                        storage_hits
                            .iter()
                            .map(|p| format!("`{p}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                };
            }
        }
        if dry_run {
            return ToolOutput::Ok {
                content: format!(
                    "dry run: all {} edits validate cleanly across {} file(s) — nothing \
                     written:\n{}",
                    edits.len(),
                    files.len(),
                    report(&verdicts)
                ),
            };
        }

        // ---- Phase 2: apply — one write per touched file, in first-touch
        // order. On a mid-batch write failure, roll back every file already
        // written to its original bytes so the tree never stays half-applied.
        let mut written: Vec<&String> = Vec::new();
        for key in &order {
            let (path, full, _, simulated) = &files[key];
            if let Err(e) = tokio::fs::write(full, simulated).await {
                let mut rollback_note = String::new();
                for prior in &written {
                    let (prior_path, prior_full, original, _) = &files[*prior];
                    if tokio::fs::write(prior_full, original).await.is_err() {
                        rollback_note.push_str(&format!(
                            "\n  ROLLBACK FAILED for `{prior_path}` — restore it manually from \
                             the content you last read"
                        ));
                    }
                }
                if rollback_note.is_empty() && !written.is_empty() {
                    rollback_note = format!(
                        "\n  {} already-written file(s) rolled back to their original content",
                        written.len()
                    );
                }
                return ToolOutput::Error {
                    message: format!(
                        "failed to write `{path}`: {e} — batch aborted{rollback_note}"
                    ),
                };
            }
            written.push(key);
        }
        // The model knows the bytes it just produced (#331) — record every
        // final content so these writes are never misattributed as drift.
        for key in &order {
            let (path, _, _, simulated) = &files[key];
            self.ledger.record_known(root, path, simulated);
        }

        ToolOutput::Ok {
            content: format!(
                "applied {} edits across {} file(s):\n{}",
                edits.len(),
                files.len(),
                report(&verdicts)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::ReadFile;

    fn edit(path: &str, old: &str, new: &str) -> Value {
        serde_json::json!({ "path": path, "old_string": old, "new_string": new })
    }

    #[tokio::test]
    async fn applies_a_multi_file_batch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() { one }\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() { one }\n").unwrap();

        let result = ApplyEdits::default()
            .execute(
                &serde_json::json!({ "edits": [
                    edit("a.rs", "one", "two"),
                    edit("b.rs", "one", "two"),
                ]}),
                dir.path(),
            )
            .await;
        match result {
            ToolOutput::Ok { content } => {
                assert!(
                    content.contains("applied 2 edits across 2 file(s)"),
                    "{content}"
                );
            }
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "fn a() { two }\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.rs")).unwrap(),
            "fn b() { two }\n"
        );
    }

    /// The #333 witness: a two-edit batch where the second edit's
    /// `old_string` is absent must leave BOTH files unchanged and report
    /// which edit failed — failing before transactional apply existed.
    #[tokio::test]
    async fn failed_edit_leaves_every_file_unchanged_and_names_the_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() { one }\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() { one }\n").unwrap();

        let result = ApplyEdits::default()
            .execute(
                &serde_json::json!({ "edits": [
                    edit("a.rs", "one", "two"),
                    edit("b.rs", "MISSING", "two"),
                ]}),
                dir.path(),
            )
            .await;
        match result {
            ToolOutput::Error { message } => {
                assert!(message.contains("NOTHING was written"), "{message}");
                assert!(message.contains("edit 0 (a.rs): ok"), "{message}");
                assert!(message.contains("edit 1: FAILED"), "{message}");
            }
            ToolOutput::Ok { content } => panic!("expected error, got: {content}"),
        }
        // Both files untouched — including the one whose edit validated.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "fn a() { one }\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.rs")).unwrap(),
            "fn b() { one }\n"
        );
    }

    #[tokio::test]
    async fn edits_to_the_same_file_compose_in_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "alpha\n").unwrap();

        let result = ApplyEdits::default()
            .execute(
                &serde_json::json!({ "edits": [
                    edit("a.rs", "alpha", "beta"),
                    // Only matches AFTER the first edit applied.
                    edit("a.rs", "beta", "gamma"),
                ]}),
                dir.path(),
            )
            .await;
        assert!(!result.is_error(), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "gamma\n"
        );
    }

    #[tokio::test]
    async fn dry_run_validates_but_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one\n").unwrap();

        let result = ApplyEdits::default()
            .execute(
                &serde_json::json!({ "edits": [edit("a.rs", "one", "two")], "dry_run": true }),
                dir.path(),
            )
            .await;
        match result {
            ToolOutput::Ok { content } => {
                assert!(content.contains("dry run"), "{content}");
                assert!(content.contains("nothing"), "{content}");
            }
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "one\n"
        );
    }

    #[tokio::test]
    async fn validate_failure_is_drift_attributed_via_the_shared_ledger() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "original\n").unwrap();
        let ledger = Arc::new(ReadLedger::default());
        let read = ReadFile::with_ledger(ledger.clone());
        let apply = ApplyEdits::with_ledger(ledger.clone());

        let seen = read
            .execute(&serde_json::json!({"path": "a.rs"}), dir.path())
            .await;
        assert!(!seen.is_error());
        // Out-of-band change between the read and the batch.
        std::fs::write(dir.path().join("a.rs"), "rewritten\n").unwrap();

        let result = apply
            .execute(
                &serde_json::json!({ "edits": [edit("a.rs", "original", "x")] }),
                dir.path(),
            )
            .await;
        match result {
            ToolOutput::Error { message } => {
                assert!(
                    message.contains("CHANGED after you last read it"),
                    "drift must be attributed in the per-edit report: {message}"
                );
            }
            ToolOutput::Ok { content } => panic!("expected error, got: {content}"),
        }
    }

    #[tokio::test]
    async fn refuses_storage_definitions_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "migrations/001_init.sql";
        std::fs::create_dir_all(dir.path().join("migrations")).unwrap();
        std::fs::write(dir.path().join(rel), "CREATE TABLE t (id int);\n").unwrap();

        let result = ApplyEdits::default()
            .execute(
                &serde_json::json!({ "edits": [edit(rel, "int", "bigint")] }),
                dir.path(),
            )
            .await;
        match result {
            ToolOutput::Error { message } => {
                assert!(
                    message.contains("storage definitions"),
                    "storage DDL must be refused toward edit_file: {message}"
                );
            }
            ToolOutput::Ok { content } => panic!("expected refusal, got: {content}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join(rel)).unwrap(),
            "CREATE TABLE t (id int);\n"
        );
    }

    #[tokio::test]
    async fn plain_code_files_are_not_mistaken_for_storage() {
        // .ts/.py/.js are storage *candidates* (marker-gated adapters): a
        // plain code file extracts empty and must pass through the batch.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.ts"), "export const one = 1;\n").unwrap();

        let result = ApplyEdits::default()
            .execute(
                &serde_json::json!({ "edits": [edit("app.ts", "one = 1", "one = 2")] }),
                dir.path(),
            )
            .await;
        assert!(!result.is_error(), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("app.ts")).unwrap(),
            "export const one = 2;\n"
        );
    }

    #[tokio::test]
    async fn prior_batch_edit_consuming_a_match_is_named_as_composition() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "alpha\n").unwrap();

        let result = ApplyEdits::default()
            .execute(
                &serde_json::json!({ "edits": [
                    edit("a.rs", "alpha", "beta"),
                    edit("a.rs", "alpha", "gamma"),
                ]}),
                dir.path(),
            )
            .await;
        match result {
            ToolOutput::Error { message } => {
                assert!(
                    message.contains("PRIOR edit in this batch"),
                    "composition mistakes must not be blamed on drift: {message}"
                );
            }
            ToolOutput::Ok { content } => panic!("expected error, got: {content}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "alpha\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_match_without_replace_all_fails_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x x\n").unwrap();

        let result = ApplyEdits::default()
            .execute(
                &serde_json::json!({ "edits": [edit("a.rs", "x", "y")] }),
                dir.path(),
            )
            .await;
        match result {
            ToolOutput::Error { message } => {
                assert!(message.contains("appears 2 times"), "{message}");
            }
            ToolOutput::Ok { content } => panic!("expected error, got: {content}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "x x\n"
        );
    }

    #[tokio::test]
    async fn empty_and_oversized_batches_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let empty = ApplyEdits::default()
            .execute(&serde_json::json!({ "edits": [] }), dir.path())
            .await;
        assert!(empty.is_error());

        let oversized: Vec<Value> = (0..65).map(|i| edit("a.rs", "x", &i.to_string())).collect();
        let result = ApplyEdits::default()
            .execute(&serde_json::json!({ "edits": oversized }), dir.path())
            .await;
        match result {
            ToolOutput::Error { message } => assert!(message.contains("ceiling"), "{message}"),
            ToolOutput::Ok { content } => panic!("expected error, got: {content}"),
        }
    }

    #[tokio::test]
    async fn escaping_path_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let result = ApplyEdits::default()
            .execute(
                &serde_json::json!({ "edits": [edit("../../etc/passwd", "root", "x")] }),
                dir.path(),
            )
            .await;
        assert!(result.is_error());
    }
}
