//! `diagnostics` — fast typecheck via the toolchain's native
//! machine-readable diagnostics.
//!
//! `build_project`/`run_tests` return prose that gets middle-truncated at
//! 30 KB; the model re-parses it and misses errors buried in the cut. This
//! tool runs the detected toolchain's own structured-output mode —
//! `cargo check --message-format=json`, `tsc --pretty false`,
//! `eslint --format json`, `ruff check --output-format=json` — and parses
//! the FULL stream into typed [`Diagnostic`] records rendered compactly:
//! grouped by file, error-bearing files first, capped with a loud elision.
//! The raw stream is never truncated (`exec::run_argv_untruncated`) — the
//! *render* is bounded, so no record is silently severed mid-JSON-line.
//!
//! Detection mirrors the script index's ecosystem ranks (cargo → node →
//! python) and reuses its primitives (`scripts::node_pm`,
//! `scripts::python_runner`, `scripts::has_python_dep`); the command is a
//! fixed argv per toolchain — no shell, no free-form command surface, same
//! posture as `run_lint`/`format_code`.

use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::exec;
use crate::registry::Tool;

const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// Rendered-record cap. The elision line carries the exact counts left out,
/// so a giant break is loud, never silently shortened.
const MAX_RENDERED_DIAGNOSTICS: usize = 100;

/// Severity of one record. `Error < Warning` so sorting puts errors first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One structured diagnostic: exactly the fields the model needs to act —
/// where (`file`, `line`, `col`), how bad (`severity`), which rule
/// (`code`), and what (`message`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub severity: Severity,
    pub code: Option<String>,
    pub message: String,
}

/// The machine-readable dialects this tool parses, one per toolchain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticsFormat {
    /// `cargo check --message-format=json` (clippy shares the shape).
    CargoJson,
    /// `tsc --pretty false`: line-stable `file(l,c): error TSnnnn: msg`.
    Tsc,
    /// `eslint --format json`.
    EslintJson,
    /// `ruff check --output-format=json`.
    RuffJson,
}

/// The resolved run: an exact argv (no shell anywhere) plus the parser for
/// its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsPlan {
    pub argv: Vec<String>,
    pub format: DiagnosticsFormat,
}

/// Resolve the workspace's diagnostics command, in the script index's
/// ecosystem-rank order (cargo → node → python). Static marker reads only —
/// nothing is executed here.
pub fn resolve_plan(root: &Path) -> Result<DiagnosticsPlan, String> {
    if root.join("Cargo.toml").exists() {
        return Ok(DiagnosticsPlan {
            argv: to_argv(&["cargo", "check", "--workspace", "--message-format=json"]),
            format: DiagnosticsFormat::CargoJson,
        });
    }
    if root.join("package.json").exists() {
        let pm = crate::scripts::node_pm(root, None);
        if root.join("tsconfig.json").exists() {
            let mut argv = pm_exec(pm);
            // `--noEmit` keeps the run non-mutating even when the project's
            // tsconfig would emit JS.
            argv.extend(to_argv(&["tsc", "--noEmit", "--pretty", "false"]));
            return Ok(DiagnosticsPlan {
                argv,
                format: DiagnosticsFormat::Tsc,
            });
        }
        if has_eslint_config(root) {
            let mut argv = pm_exec(pm);
            argv.extend(to_argv(&["eslint", "--format", "json", "."]));
            return Ok(DiagnosticsPlan {
                argv,
                format: DiagnosticsFormat::EslintJson,
            });
        }
    }
    if let Ok(text) = std::fs::read_to_string(root.join("pyproject.toml"))
        && let Ok(doc) = toml::from_str::<toml::Value>(&text)
        && crate::scripts::has_python_dep(&doc, "ruff")
    {
        let runner = crate::scripts::python_runner(root, &doc);
        return Ok(DiagnosticsPlan {
            argv: to_argv(&[runner, "run", "ruff", "check", "--output-format=json"]),
            format: DiagnosticsFormat::RuffJson,
        });
    }
    Err(
        "no diagnostics toolchain detected — looked for Cargo.toml (cargo check), \
         package.json + tsconfig.json (tsc), an ESLint config (eslint), and a \
         pyproject.toml declaring ruff; run the project's own check script via \
         run_script instead"
            .into(),
    )
}

fn to_argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

/// The package manager's binary-exec prefix (`pnpm exec tsc …`), matching
/// the lockfile-detected pm the script index uses.
fn pm_exec(pm: &str) -> Vec<String> {
    match pm {
        "pnpm" => to_argv(&["pnpm", "exec"]),
        "yarn" => to_argv(&["yarn"]),
        "bun" => to_argv(&["bunx"]),
        _ => to_argv(&["npx"]),
    }
}

/// Whether the workspace configures ESLint: a dedicated config file (flat
/// or legacy) or the `eslintConfig` key in `package.json`.
fn has_eslint_config(root: &Path) -> bool {
    const CONFIG_FILES: &[&str] = &[
        "eslint.config.js",
        "eslint.config.mjs",
        "eslint.config.cjs",
        "eslint.config.ts",
        ".eslintrc",
        ".eslintrc.js",
        ".eslintrc.cjs",
        ".eslintrc.json",
        ".eslintrc.yml",
        ".eslintrc.yaml",
    ];
    if CONFIG_FILES.iter().any(|name| root.join(name).exists()) {
        return true;
    }
    std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|pkg| pkg.get("eslintConfig").is_some())
}

/// Dispatch to the per-format parser. Each parser is a pure, deterministic
/// function of the captured output — unit-tested per format on captured
/// fixtures, no toolchain required.
pub fn parse(format: DiagnosticsFormat, output: &str) -> Vec<Diagnostic> {
    match format {
        DiagnosticsFormat::CargoJson => parse_cargo_json(output),
        DiagnosticsFormat::Tsc => parse_tsc(output),
        DiagnosticsFormat::EslintJson => parse_eslint_json(output),
        DiagnosticsFormat::RuffJson => parse_ruff_json(output),
    }
}

/// `cargo check --message-format=json`: one JSON object per line; rustc
/// diagnostics ride `reason: "compiler-message"`. Span-less messages
/// ("aborting due to N previous errors") are summaries, not locations —
/// skipped. Note/help levels ride as children of their parent and are
/// skipped as top-level records.
fn parse_cargo_json(output: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in output.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if record.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(message) = record.get("message") else {
            continue;
        };
        let severity = match message.get("level").and_then(|l| l.as_str()) {
            Some("warning") => Severity::Warning,
            // "error" and "error: internal compiler error".
            Some(level) if level.starts_with("error") => Severity::Error,
            _ => continue,
        };
        let Some(span) = message
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|spans| {
                spans.iter().find(|s| {
                    s.get("is_primary")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false)
                })
            })
        else {
            continue;
        };
        let Some(file) = span.get("file_name").and_then(|f| f.as_str()) else {
            continue;
        };
        out.push(Diagnostic {
            file: file.to_string(),
            line: span.get("line_start").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            col: span
                .get("column_start")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            severity,
            code: message
                .get("code")
                .and_then(|c| c.get("code"))
                .and_then(|c| c.as_str())
                .map(String::from),
            message: flatten(
                message
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or(""),
            ),
        });
    }
    out
}

/// `tsc --pretty false`: `src/a.ts(12,5): error TS2322: message`, plus
/// file-less project errors (`error TS18003: message`). Non-matching lines
/// (continuations, summaries) contribute nothing.
fn parse_tsc(output: &str) -> Vec<Diagnostic> {
    output
        .lines()
        .filter_map(|line| parse_tsc_line(line.trim_end()))
        .collect()
}

fn parse_tsc_line(line: &str) -> Option<Diagnostic> {
    if let Some(idx) = line.find("): ")
        && let Some(diag) = parse_tsc_located(&line[..idx], &line[idx + 3..])
    {
        return Some(diag);
    }
    let (severity, code, message) = parse_tsc_tail(line)?;
    Some(Diagnostic {
        file: String::new(),
        line: 0,
        col: 0,
        severity,
        code: Some(code),
        message,
    })
}

/// `head` = `src/a.ts(12,5` (closing paren stripped), `tail` = the
/// `error TSnnnn: message` remainder.
fn parse_tsc_located(head: &str, tail: &str) -> Option<Diagnostic> {
    let open = head.rfind('(')?;
    let (line, col) = head[open + 1..].split_once(',')?;
    let (severity, code, message) = parse_tsc_tail(tail)?;
    Some(Diagnostic {
        file: head[..open].to_string(),
        line: line.trim().parse().ok()?,
        col: col.trim().parse().ok()?,
        severity,
        code: Some(code),
        message,
    })
}

/// `error TS2322: message` → (severity, code, message); `None` for any
/// other shape.
fn parse_tsc_tail(tail: &str) -> Option<(Severity, String, String)> {
    let (word, rest) = tail.split_once(' ')?;
    let severity = match word {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => return None,
    };
    let (code, message) = rest.split_once(": ")?;
    let digits = code.strip_prefix("TS")?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((severity, code.to_string(), flatten(message)))
}

/// `eslint --format json`: one array of file entries, each with a
/// `messages` list (`severity` 2 = error, 1 = warning; `ruleId` null on
/// fatal parse errors).
fn parse_eslint_json(output: &str) -> Vec<Diagnostic> {
    let Some(files) = first_json_value(output) else {
        return Vec::new();
    };
    let Some(files) = files.as_array().cloned() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in &files {
        let Some(file) = entry.get("filePath").and_then(|f| f.as_str()) else {
            continue;
        };
        let Some(messages) = entry.get("messages").and_then(|m| m.as_array()) else {
            continue;
        };
        for m in messages {
            let severity = if m.get("severity").and_then(|s| s.as_u64()) == Some(2) {
                Severity::Error
            } else {
                Severity::Warning
            };
            out.push(Diagnostic {
                file: file.to_string(),
                line: m.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                col: m.get("column").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                severity,
                code: m.get("ruleId").and_then(|r| r.as_str()).map(String::from),
                message: flatten(m.get("message").and_then(|s| s.as_str()).unwrap_or("")),
            });
        }
    }
    out
}

/// `ruff check --output-format=json`: one array of violations with
/// `filename`, `location {row, column}`, `code` (null on syntax errors),
/// `message`. Ruff carries no per-record severity; every violation fails
/// the run (exit 1), so they map to errors.
fn parse_ruff_json(output: &str) -> Vec<Diagnostic> {
    let Some(list) = first_json_value(output) else {
        return Vec::new();
    };
    let Some(list) = list.as_array().cloned() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for v in &list {
        let Some(file) = v.get("filename").and_then(|f| f.as_str()) else {
            continue;
        };
        let location = v.get("location");
        out.push(Diagnostic {
            file: file.to_string(),
            line: location
                .and_then(|l| l.get("row"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            col: location
                .and_then(|l| l.get("column"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            severity: Severity::Error,
            code: v.get("code").and_then(|c| c.as_str()).map(String::from),
            message: flatten(v.get("message").and_then(|m| m.as_str()).unwrap_or("")),
        });
    }
    out
}

/// The first JSON value at or after the first `[`/`{` — package-manager
/// wrappers (yarn's `yarn run v1…` banner) and interleaved stderr surround
/// the formatter's document with non-JSON lines.
fn first_json_value(output: &str) -> Option<Value> {
    let start = output.find(['[', '{'])?;
    serde_json::Deserializer::from_str(&output[start..])
        .into_iter::<Value>()
        .next()?
        .ok()
}

/// Newlines inside a message would break the one-record-per-line render.
fn flatten(message: &str) -> String {
    message.replace('\n', " ").trim().to_string()
}

/// Make paths workspace-relative (eslint/ruff print absolute paths), then
/// sort and dedup — cargo emits the same diagnostic once per target
/// (lib + test), and duplicates would double-count.
fn normalize(diags: &mut Vec<Diagnostic>, root: &Path) {
    let mut prefixes = vec![root.display().to_string()];
    if let Ok(canonical) = root.canonicalize() {
        let canonical = canonical.display().to_string();
        if !prefixes.contains(&canonical) {
            prefixes.push(canonical);
        }
    }
    for diag in diags.iter_mut() {
        for prefix in &prefixes {
            if let Some(rest) = diag.file.strip_prefix(prefix.as_str()) {
                diag.file = rest.trim_start_matches('/').to_string();
                break;
            }
        }
    }
    diags.sort();
    diags.dedup();
}

/// Render the report: a PASSED/FAILED first line with exact counts, then
/// records grouped by file — error-bearing files first, errors before
/// warnings within a file — capped at [`MAX_RENDERED_DIAGNOSTICS`] with an
/// elision line that names what was left out.
fn render_report(display: &str, exit_code: i32, diags: &[Diagnostic]) -> String {
    let errors = diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diags.len() - errors;
    let verdict = if exit_code == 0 {
        format!("`{display}` PASSED (exit 0)")
    } else {
        format!("`{display}` FAILED (exit {exit_code})")
    };
    if diags.is_empty() {
        return format!("{verdict} — no diagnostics");
    }

    // Group by file; error-bearing files first (then by path), and within
    // a file errors before warnings (then by line) — so the cap can only
    // ever elide errors when there are more than the cap OF errors.
    let mut groups: BTreeMap<&str, Vec<&Diagnostic>> = BTreeMap::new();
    for diag in diags {
        groups.entry(diag.file.as_str()).or_default().push(diag);
    }
    let mut ordered: Vec<(&str, Vec<&Diagnostic>)> = groups.into_iter().collect();
    for (_, group) in ordered.iter_mut() {
        group.sort_by_key(|d| (d.severity, d.line, d.col));
    }
    ordered.sort_by_key(|(file, group)| (group[0].severity, *file));

    let files = ordered.len();
    let mut report =
        format!("{verdict} — {errors} error(s), {warnings} warning(s) in {files} file(s)\n");
    let mut rendered = 0usize;
    let mut rendered_errors = 0usize;
    'render: for (file, group) in &ordered {
        if rendered == MAX_RENDERED_DIAGNOSTICS {
            break 'render;
        }
        report.push('\n');
        report.push_str(if file.is_empty() { "(project)" } else { file });
        report.push('\n');
        for diag in group {
            if rendered == MAX_RENDERED_DIAGNOSTICS {
                break 'render;
            }
            let code = diag
                .code
                .as_deref()
                .map(|c| format!("[{c}]"))
                .unwrap_or_default();
            report.push_str(&format!(
                "  {}{code} {}:{} {}\n",
                diag.severity.label(),
                diag.line,
                diag.col,
                diag.message
            ));
            rendered += 1;
            if diag.severity == Severity::Error {
                rendered_errors += 1;
            }
        }
    }
    if rendered < diags.len() {
        let elided = diags.len() - rendered;
        let elided_errors = errors - rendered_errors;
        report.push_str(&format!(
            "\n[!] {elided} more diagnostic(s) NOT shown ({elided_errors} error(s), {} \
             warning(s)) — fix the ones above and rerun diagnostics\n",
            elided - elided_errors
        ));
    }
    report.trim_end().to_string()
}

/// `diagnostics` — the fast-typecheck tool. Read-only in the flag's sense:
/// it mutates no workspace state the model sees (cargo/tsc caches land in
/// their own build dirs; `--noEmit` pins tsc).
pub struct Diagnostics;

#[async_trait]
impl Tool for Diagnostics {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "diagnostics".into(),
            description: "Fast typecheck: run the toolchain's native machine-readable check \
                          (cargo check / tsc / eslint / ruff) and return structured \
                          file:line:col diagnostics with severity and rule code, grouped by \
                          file. Much cheaper than build_project — use it after edits to see \
                          exactly what broke."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "timeout_secs": { "type": "integer" }
                }
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let timeout_secs = crate::exec::timeout_from(input, DEFAULT_TIMEOUT_SECS);
        let plan = {
            // Manifest reads on the blocking pool, like `ScriptIndex::detect`.
            let root = root.to_path_buf();
            tokio::task::spawn_blocking(move || resolve_plan(&root))
                .await
                .unwrap_or_else(|_| Err("diagnostics detection was cancelled".into()))
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(message) => return ToolOutput::Error { message },
        };
        let display = plan.argv.join(" ");
        match exec::run_argv_untruncated(&plan.argv[0], &plan.argv[1..], root, timeout_secs).await {
            Ok((code, raw)) => {
                let mut diags = parse(plan.format, &raw);
                normalize(&mut diags, root);
                if code != 0 && diags.is_empty() {
                    // A failing run with nothing parseable (broken manifest,
                    // missing binary shim, tool crash) must not report an
                    // empty success-shaped frame — surface the raw evidence.
                    return ToolOutput::Error {
                        message: format!(
                            "`{display}` FAILED (exit {code}) — no structured diagnostics \
                             parsed; raw output:\n{}",
                            exec::truncate_middle(raw)
                        ),
                    };
                }
                let report = render_report(&display, code, &diags);
                if code == 0 {
                    ToolOutput::Ok { content: report }
                } else {
                    ToolOutput::Error { message: report }
                }
            }
            Err(e) => ToolOutput::Error { message: e },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// THE WITNESS for the structured-diagnostics tool: a known-bad crate
    /// through the real cargo yields a record with the exact
    /// {file, line, col, severity, code} — not a prose dump.
    #[tokio::test]
    async fn cargo_check_on_a_known_bad_crate_yields_exact_structured_records() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"known_bad\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            dir.path(),
            "src/lib.rs",
            "pub fn answer() -> u32 {\n    \"forty-two\"\n}\n",
        );
        let out = Diagnostics
            .execute(&serde_json::json!({}), dir.path())
            .await;
        match &out {
            ToolOutput::Error { message } => {
                assert!(message.contains("FAILED"), "{message}");
                assert!(message.contains("src/lib.rs"), "{message}");
                assert!(
                    message.contains("error[E0308] 2:5"),
                    "exact severity[code] line:col record: {message}"
                );
                assert!(message.contains("mismatched types"), "{message}");
            }
            other => panic!("a bad crate must FAIL with structured records, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cargo_check_on_a_clean_crate_passes_with_no_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"known_good\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            dir.path(),
            "src/lib.rs",
            "pub fn answer() -> u32 {\n    42\n}\n",
        );
        let out = Diagnostics
            .execute(&serde_json::json!({}), dir.path())
            .await;
        match &out {
            ToolOutput::Ok { content } => {
                assert!(content.contains("PASSED"), "{content}");
                assert!(content.contains("no diagnostics"), "{content}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_workspace_is_a_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let out = Diagnostics
            .execute(&serde_json::json!({}), dir.path())
            .await;
        match &out {
            ToolOutput::Error { message } => {
                assert!(
                    message.contains("no diagnostics toolchain detected"),
                    "{message}"
                );
                assert!(message.contains("Cargo.toml"), "{message}");
                assert!(message.contains("run_script"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plan_resolution_is_marker_driven_and_rank_ordered() {
        let join = |plan: &DiagnosticsPlan| plan.argv.join(" ");

        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        let plan = resolve_plan(dir.path()).unwrap();
        assert_eq!(join(&plan), "cargo check --workspace --message-format=json");
        assert_eq!(plan.format, DiagnosticsFormat::CargoJson);

        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "package.json", "{}");
        write(dir.path(), "pnpm-lock.yaml", "");
        write(dir.path(), "tsconfig.json", "{}");
        let plan = resolve_plan(dir.path()).unwrap();
        assert_eq!(join(&plan), "pnpm exec tsc --noEmit --pretty false");
        assert_eq!(plan.format, DiagnosticsFormat::Tsc);

        // No tsconfig, but an ESLint flat config: eslint via npx (no
        // lockfile → npm).
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "package.json", "{}");
        write(dir.path(), "eslint.config.js", "module.exports = [];");
        let plan = resolve_plan(dir.path()).unwrap();
        assert_eq!(join(&plan), "npx eslint --format json .");
        assert_eq!(plan.format, DiagnosticsFormat::EslintJson);

        // Ruff only when pyproject declares it; uv from its lockfile.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            "[project]\nname = \"y\"\n\n[dependency-groups]\ndev = [\"ruff\"]\n",
        );
        write(dir.path(), "uv.lock", "");
        let plan = resolve_plan(dir.path()).unwrap();
        assert_eq!(join(&plan), "uv run ruff check --output-format=json");
        assert_eq!(plan.format, DiagnosticsFormat::RuffJson);

        // A ruff-less pyproject resolves nothing.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"z\"\n");
        assert!(resolve_plan(dir.path()).is_err());
    }

    /// Captured `cargo check --message-format=json` shapes: the duplicate
    /// per-target record dedups, span-less summaries and note levels are
    /// skipped, and non-JSON lines contribute nothing.
    #[test]
    fn cargo_json_parses_dedups_and_skips_summaries() {
        let record = r#"{"reason":"compiler-message","package_id":"path+file:///tmp/x#0.1.0","message":{"$message_type":"diagnostic","message":"mismatched types","code":{"code":"E0308","explanation":null},"level":"error","spans":[{"file_name":"src/lib.rs","line_start":2,"line_end":2,"column_start":5,"column_end":16,"is_primary":true}],"children":[],"rendered":"error[E0308]: mismatched types\n"}}"#;
        let summary = r#"{"reason":"compiler-message","message":{"$message_type":"diagnostic","message":"aborting due to 1 previous error","code":null,"level":"error","spans":[],"children":[],"rendered":"error: aborting due to 1 previous error\n"}}"#;
        let note = r#"{"reason":"compiler-message","message":{"$message_type":"diagnostic","message":"required by a bound","code":null,"level":"note","spans":[{"file_name":"src/lib.rs","line_start":9,"line_end":9,"column_start":1,"column_end":2,"is_primary":true}],"children":[],"rendered":"note\n"}}"#;
        let finished = r#"{"reason":"build-finished","success":false}"#;
        let output = format!(
            "{record}\n{record}\n{summary}\n{note}\n{finished}\nerror: could not compile `x` (lib)\n"
        );

        let mut diags = parse(DiagnosticsFormat::CargoJson, &output);
        normalize(&mut diags, Path::new("/nonexistent-root"));
        assert_eq!(
            diags,
            vec![Diagnostic {
                file: "src/lib.rs".into(),
                line: 2,
                col: 5,
                severity: Severity::Error,
                code: Some("E0308".into()),
                message: "mismatched types".into(),
            }]
        );
    }

    /// Captured `tsc --pretty false` output: located records, a warning,
    /// a file-less project error, and noise lines.
    #[test]
    fn tsc_output_parses_located_and_project_level_records() {
        let output = "src/app.ts(12,5): error TS2322: Type 'string' is not assignable to type 'number'.\n\
                      src/util.ts(3,10): warning TS6133: 'x' is declared but its value is never read.\n\
                      error TS18003: No inputs were found in config file 'tsconfig.json'.\n\
                      Found 3 errors in 2 files.\n";
        let diags = parse(DiagnosticsFormat::Tsc, output);
        assert_eq!(diags.len(), 3, "{diags:?}");
        assert_eq!(
            diags[0],
            Diagnostic {
                file: "src/app.ts".into(),
                line: 12,
                col: 5,
                severity: Severity::Error,
                code: Some("TS2322".into()),
                message: "Type 'string' is not assignable to type 'number'.".into(),
            }
        );
        assert_eq!(diags[1].severity, Severity::Warning);
        assert_eq!(diags[1].code.as_deref(), Some("TS6133"));
        // The project-level error keeps its code but has no location.
        assert_eq!(diags[2].file, "");
        assert_eq!(diags[2].code.as_deref(), Some("TS18003"));
    }

    /// Captured `eslint --format json` output, wrapped in the yarn-classic
    /// banner noise the pm-exec path can prepend.
    #[test]
    fn eslint_json_parses_severities_and_null_rule_ids() {
        let output = "yarn run v1.22.19\n$ eslint --format json .\n\
            [{\"filePath\":\"/repo/src/a.js\",\"messages\":[\
            {\"ruleId\":\"no-unused-vars\",\"severity\":2,\"message\":\"'x' is defined but never used.\",\"line\":1,\"column\":7},\
            {\"ruleId\":\"eqeqeq\",\"severity\":1,\"message\":\"Expected '===' and instead saw '=='.\",\"line\":4,\"column\":9},\
            {\"ruleId\":null,\"severity\":2,\"fatal\":true,\"message\":\"Parsing error: Unexpected token\",\"line\":9,\"column\":1}\
            ],\"errorCount\":2,\"warningCount\":1}]\n";
        let diags = parse(DiagnosticsFormat::EslintJson, output);
        assert_eq!(diags.len(), 3, "{diags:?}");
        assert_eq!(
            diags[0],
            Diagnostic {
                file: "/repo/src/a.js".into(),
                line: 1,
                col: 7,
                severity: Severity::Error,
                code: Some("no-unused-vars".into()),
                message: "'x' is defined but never used.".into(),
            }
        );
        assert_eq!(diags[1].severity, Severity::Warning);
        assert_eq!(diags[2].code, None, "fatal parse errors have no ruleId");
    }

    /// Captured `ruff check --output-format=json` output — records carry no
    /// severity of their own; every violation fails the run.
    #[test]
    fn ruff_json_parses_location_and_null_codes() {
        let output = r#"[{"cell":null,"code":"F401","end_location":{"column":10,"row":1},"filename":"/repo/pkg/a.py","fix":null,"location":{"column":8,"row":1},"message":"`os` imported but unused","noqa_row":1,"url":"https://docs.astral.sh/ruff/rules/unused-import"},{"cell":null,"code":null,"end_location":{"column":1,"row":4},"filename":"/repo/pkg/b.py","fix":null,"location":{"column":5,"row":3},"message":"SyntaxError: Expected an expression","noqa_row":3,"url":null}]"#;
        let diags = parse(DiagnosticsFormat::RuffJson, output);
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert_eq!(
            diags[0],
            Diagnostic {
                file: "/repo/pkg/a.py".into(),
                line: 1,
                col: 8,
                severity: Severity::Error,
                code: Some("F401".into()),
                message: "`os` imported but unused".into(),
            }
        );
        assert_eq!(diags[1].code, None, "syntax errors carry a null code");
    }

    #[test]
    fn normalize_relativizes_absolute_paths_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let abs = dir.path().canonicalize().unwrap().join("src/a.py");
        let mut diags = vec![Diagnostic {
            file: abs.display().to_string(),
            line: 1,
            col: 1,
            severity: Severity::Error,
            code: None,
            message: "m".into(),
        }];
        normalize(&mut diags, dir.path());
        assert_eq!(diags[0].file, "src/a.py");
    }

    #[test]
    fn render_groups_by_file_orders_error_files_first_and_elides_loudly() {
        let diag = |file: &str, line: u32, severity: Severity| Diagnostic {
            file: file.into(),
            line,
            col: 1,
            severity,
            code: Some("X1".into()),
            message: "m".into(),
        };
        // a.rs has only a warning; z.rs has an error — z.rs must render
        // first so the cap can never elide errors in favor of warnings.
        let mut diags = vec![
            diag("a.rs", 1, Severity::Warning),
            diag("z.rs", 2, Severity::Error),
        ];
        diags.sort();
        let report = render_report("cmd", 101, &diags);
        assert!(report.contains("FAILED (exit 101) — 1 error(s), 1 warning(s) in 2 file(s)"));
        let z = report.find("z.rs").unwrap();
        let a = report.find("a.rs").unwrap();
        assert!(z < a, "error-bearing file renders first:\n{report}");

        // 40 errors in one file + 100 warnings in another: the render caps
        // at MAX_RENDERED_DIAGNOSTICS and the elision line counts the rest.
        let mut many = Vec::new();
        for line in 0..40 {
            many.push(diag("bad.rs", line, Severity::Error));
        }
        for line in 0..100 {
            many.push(diag("warn.rs", line, Severity::Warning));
        }
        many.sort();
        let report = render_report("cmd", 101, &many);
        assert!(
            report.contains("[!] 40 more diagnostic(s) NOT shown (0 error(s), 40 warning(s))"),
            "{report}"
        );
        // Every error is visible; only warnings were elided.
        assert_eq!(report.matches("error[X1]").count(), 40, "{report}");

        // Warnings-only with exit 0 is a PASS that still shows the records.
        let warn_only = vec![diag("w.rs", 1, Severity::Warning)];
        let report = render_report("cmd", 0, &warn_only);
        assert!(
            report.contains("PASSED (exit 0) — 0 error(s), 1 warning(s)"),
            "{report}"
        );
        assert!(report.contains("warning[X1] 1:1"), "{report}");
    }
}
