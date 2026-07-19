//! `build_project`, `run_tests`, `run_lint`, `format_code` —
//! toolchain-aware build/test/lint/format execution.
//!
//! All four are thin verb shortcuts over the project scripts index
//! (`crate::scripts`, spec: `docs/design/scripts-index.md`): detection is
//! the index's one code path. `build_project` runs the `build` verb
//! binding; `run_tests` layers its `kind` (unit / e2e / all) and `filter`
//! semantics on top — mapped to the runner's native filtering flag, or to
//! the project's own `test:unit`/`test:e2e` scripts; both accept an
//! explicit `command` override for anything exotic. `run_lint` and
//! `format_code` take no free-form command at all: they run the index's
//! `lint`/`format` verb binding via argv exec (no shell), and a workspace
//! with no recognized linter/formatter is a named error, not a guess.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::exec::{self, run_and_report};
use crate::registry::Tool;
use crate::scripts::ScriptIndex;

const DEFAULT_TIMEOUT_SECS: u64 = 600;

fn no_toolchain_error() -> ToolOutput {
    ToolOutput::Error {
        message: "no recognized toolchain (looked for Cargo.toml, package.json, deno.json, \
                  pyproject.toml, go.mod, Makefile, justfile, Taskfile.yml, composer.json) — \
                  pass `command` explicitly"
            .into(),
    }
}

pub struct BuildProject;

#[async_trait]
impl Tool for BuildProject {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "build_project".into(),
            description: "Build the workspace with its own toolchain (cargo/npm/go/make/…), or \
                          a custom command."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Override the detected build command" },
                    "timeout_secs": { "type": "integer" }
                }
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
            return run_and_report(command, root, timeout_secs).await;
        }
        let index = ScriptIndex::detect(root).await;
        if index.is_empty() {
            return no_toolchain_error();
        }
        match index.verb_entry("build") {
            Some(entry) => run_and_report(&entry.command, root, timeout_secs).await,
            None => ToolOutput::Error {
                message: "no `build` script detected in this workspace (see list_scripts) — \
                          pass `command`"
                    .into(),
            },
        }
    }
}

pub struct RunTests;

#[async_trait]
impl Tool for RunTests {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "run_tests".into(),
            description: "Run tests with the workspace's own runner. kind: unit|e2e|all. \
                          filter: module, package, file, or test name (runner-native)."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["unit", "e2e", "all"] },
                    "filter": { "type": "string", "description": "Narrow to a module/file/test" },
                    "command": { "type": "string", "description": "Override the detected test command" },
                    "timeout_secs": { "type": "integer" }
                }
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
            return run_and_report(command, root, timeout_secs).await;
        }
        let kind = input.get("kind").and_then(|v| v.as_str()).unwrap_or("all");
        let filter = input.get("filter").and_then(|v| v.as_str()).unwrap_or("");

        let index = ScriptIndex::detect(root).await;
        let Some(primary) = index.primary_runner() else {
            return no_toolchain_error();
        };
        let command = match primary {
            "cargo" => match kind {
                // Unit tests live beside the code; e2e = the integration
                // test targets under tests/.
                "unit" => format!("cargo test --workspace --lib --bins {filter}"),
                "e2e" => {
                    if filter.is_empty() {
                        "cargo test --workspace --test '*'".to_string()
                    } else {
                        format!("cargo test --workspace --test '*' {filter}")
                    }
                }
                _ => format!("cargo test --workspace {filter}"),
            },
            pm @ ("npm" | "pnpm" | "yarn" | "bun") => {
                let scripts = index.root_script_names(pm);
                let script = match kind {
                    "unit" if scripts.contains("test:unit") => "test:unit",
                    "e2e" if scripts.contains("test:e2e") => "test:e2e",
                    "e2e" if scripts.contains("e2e") => "e2e",
                    // NO generic-`test` fallback for e2e: running unit tests
                    // while reporting "e2e passed" is a lie.
                    "unit" | "all" if scripts.contains("test") => "test",
                    _ => {
                        return ToolOutput::Error {
                            message: format!(
                                "package.json has no matching test script for kind `{kind}` — \
                                 pass `command`"
                            ),
                        };
                    }
                };
                if filter.is_empty() {
                    format!("{pm} run {script}")
                } else {
                    format!("{pm} run {script} -- {filter}")
                }
            }
            "go" => {
                if filter.is_empty() {
                    "go test ./...".to_string()
                } else {
                    format!("go test ./... -run {filter}")
                }
            }
            runner => {
                // uv/poetry/make/just/task/composer/deno: the project's own
                // e2e script or nothing (same no-lying rule as above); unit
                // and all ride the index's `test` verb binding. pytest takes
                // the filter as a positional; the task runners have no
                // native filter flag, so a filter there is ignored.
                let scripts = index.root_script_names(runner);
                if kind == "e2e" {
                    let script = ["test:e2e", "e2e"]
                        .into_iter()
                        .find(|s| scripts.contains(s));
                    match script.and_then(|s| index.resolve(s, Some(".")).ok()) {
                        Some(entry) => entry.command.clone(),
                        None => {
                            return ToolOutput::Error {
                                message: "no e2e test script detected for kind `e2e` — pass \
                                          `command`"
                                    .into(),
                            };
                        }
                    }
                } else {
                    match index.verb_entry("test") {
                        Some(entry) if matches!(runner, "uv" | "poetry") && !filter.is_empty() => {
                            format!("{} {filter}", entry.command)
                        }
                        Some(entry) => entry.command.clone(),
                        None => {
                            return ToolOutput::Error {
                                message: "no test script detected in this workspace (see \
                                          list_scripts) — pass `command`"
                                    .into(),
                            };
                        }
                    }
                }
            }
        };
        run_and_report(&command, root, timeout_secs).await
    }
}

/// Named "nothing configured" error for `run_lint` / `format_code`: says
/// exactly what was looked for, so the model can fix the project or pick a
/// different tool instead of retrying blind.
fn no_tool_error(what: &str, node_hint: &str) -> String {
    format!(
        "no {what} configured — looked for Cargo.toml (cargo) and {node_hint}; \
         declare one, or run a project-declared verb via run_script"
    )
}

/// The linter argv for the index's `lint` verb binding, or a named error.
/// Pure — separable from process spawning so the command shape is
/// unit-testable. Verb-bound commands are runner-composed from validated
/// script names (never free text), so whitespace-splitting one into argv
/// is lossless.
fn lint_argv(index: &ScriptIndex, fix: bool) -> Result<Vec<String>, String> {
    let Some(entry) = index.verb_entry("lint") else {
        return Err(no_tool_error("linter", "a package.json `lint` script"));
    };
    let mut argv: Vec<String> = entry.command.split_whitespace().map(String::from).collect();
    if fix {
        if entry.runner == "cargo" && entry.source == "synthesized" {
            argv.push("--fix".into());
            argv.push("--allow-dirty".into());
        } else {
            // The `lint` script's fix flag is linter-specific; guessing
            // (`-- --fix`) could rewrite files under a linter that
            // treats it as a filename. Named refusal instead.
            return Err("`fix` resolves to cargo only in this version — run the \
                        project's own fix script (e.g. `lint:fix`) via run_script"
                .into());
        }
    }
    Ok(argv)
}

/// The formatter argv for the index's `format` verb binding, or a named
/// error. Pure, like [`lint_argv`].
fn format_argv(index: &ScriptIndex, check: bool) -> Result<Vec<String>, String> {
    let Some(entry) = index.verb_entry("format") else {
        return Err(no_tool_error("formatter", "a package.json `format` script"));
    };
    let mut argv: Vec<String> = entry.command.split_whitespace().map(String::from).collect();
    if check {
        if entry.runner == "cargo" && entry.source == "synthesized" {
            argv.push("--check".into());
        } else {
            return Err("`check` resolves to cargo only in this version — run the \
                        project's own check script (e.g. `format:check`) via run_script"
                .into());
        }
    }
    Ok(argv)
}

/// Count rustc-style diagnostics in captured output — lines opening with
/// `warning:`/`warning[` or `error:`/`error[`. The "counts where parseable"
/// half of the structured report; toolchains with other formats just get
/// the exit status and truncated output.
fn diagnostic_counts(output: &str) -> (usize, usize) {
    let mut warnings = 0;
    let mut errors = 0;
    for line in output.lines() {
        let t = line.trim_start();
        if t.starts_with("warning:") || t.starts_with("warning[") {
            warnings += 1;
        }
        if t.starts_with("error:") || t.starts_with("error[") {
            errors += 1;
        }
    }
    (warnings, errors)
}

/// Spawn `argv` directly (no shell) and fold the result into the standard
/// PASSED/FAILED report, appending diagnostic counts when any were parsed.
/// Output is already middle-truncated by the runner.
async fn run_argv_and_report(
    argv: &[String],
    root: &std::path::Path,
    timeout_secs: u64,
) -> ToolOutput {
    let display = argv.join(" ");
    match exec::run_argv(&argv[0], &argv[1..], root, timeout_secs).await {
        Ok((code, output)) => {
            let (warnings, errors) = diagnostic_counts(&output);
            let counts = if warnings > 0 || errors > 0 {
                format!(" [{errors} error(s), {warnings} warning(s)]")
            } else {
                String::new()
            };
            if code == 0 {
                ToolOutput::Ok {
                    content: format!("`{display}` PASSED (exit 0){counts}\n{output}"),
                }
            } else {
                ToolOutput::Error {
                    message: format!("`{display}` FAILED (exit {code}){counts}\n{output}"),
                }
            }
        }
        Err(e) => ToolOutput::Error { message: e },
    }
}

/// `run_lint` — the project's own linter, resolved from the detected
/// toolchain and spawned argv-style: `cargo clippy --workspace
/// --all-targets` (plus `--fix --allow-dirty` when `fix`), or the
/// package.json `lint` script. No free-form command surface.
pub struct RunLint;

#[async_trait]
impl Tool for RunLint {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "run_lint".into(),
            description: "Run the project's own linter (cargo clippy, or the package.json \
                          `lint` script). fix: apply automatic fixes (cargo only). Prefer \
                          this over invoking a linter by hand."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "fix": { "type": "boolean", "description": "Apply automatic fixes (cargo only)" },
                    "timeout_secs": { "type": "integer" }
                }
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let fix = input.get("fix").and_then(|v| v.as_bool()).unwrap_or(false);
        match lint_argv(&ScriptIndex::detect(root).await, fix) {
            Ok(argv) => run_argv_and_report(&argv, root, timeout_secs).await,
            Err(message) => ToolOutput::Error { message },
        }
    }
}

/// `format_code` — the project's own formatter, resolved and spawned like
/// [`RunLint`]: `cargo fmt` (plus `--check` when `check`), or the
/// package.json `format` script.
pub struct FormatCode;

#[async_trait]
impl Tool for FormatCode {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "format_code".into(),
            description: "Run the project's own formatter (cargo fmt, or the package.json \
                          `format` script). check: verify formatting without rewriting \
                          (cargo only)."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "check": { "type": "boolean", "description": "Verify without rewriting (cargo only)" },
                    "timeout_secs": { "type": "integer" }
                }
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let check = input
            .get("check")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        match format_argv(&ScriptIndex::detect(root).await, check) {
            Ok(argv) => run_argv_and_report(&argv, root, timeout_secs).await,
            Err(message) => ToolOutput::Error { message },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_toolchain_is_a_named_error_and_command_overrides() {
        let root = tempfile::tempdir().unwrap();
        let out = BuildProject
            .execute(&serde_json::json!({}), root.path())
            .await;
        assert!(out.is_error());

        let out = BuildProject
            .execute(
                &serde_json::json!({"command": "echo built && exit 0"}),
                root.path(),
            )
            .await;
        assert!(!out.is_error(), "{out:?}");

        let out = RunTests
            .execute(&serde_json::json!({"command": "exit 1"}), root.path())
            .await;
        match &out {
            ToolOutput::Error { message } => assert!(message.contains("FAILED"), "{message}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn node_detection_uses_scripts_and_pm_lockfile() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts": {"test": "echo node-tests-ran", "build": "echo node-build-ran"}}"#,
        )
        .unwrap();
        std::fs::write(root.path().join("pnpm-lock.yaml"), "").unwrap();

        // pnpm may not exist in the test environment — we only assert the
        // detection path constructs the right command shape, visible in the
        // success/error text either way.
        let out = RunTests
            .execute(&serde_json::json!({"kind": "all"}), root.path())
            .await;
        let text = match &out {
            ToolOutput::Ok { content } => content.clone(),
            ToolOutput::Error { message } => message.clone(),
        };
        assert!(text.contains("pnpm run test"), "{text}");

        let out = RunTests
            .execute(&serde_json::json!({"kind": "e2e"}), root.path())
            .await;
        assert!(out.is_error(), "no e2e script → named error: {out:?}");
    }

    #[tokio::test]
    async fn make_targets_drive_build_and_tests_via_the_index() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Makefile"),
            "build:\n\t@echo make-build-ran\ntest:\n\t@echo make-test-ran\n",
        )
        .unwrap();

        let out = BuildProject
            .execute(&serde_json::json!({}), root.path())
            .await;
        match &out {
            ToolOutput::Ok { content } => assert!(content.contains("make-build-ran"), "{content}"),
            other => panic!("{other:?}"),
        }

        let out = RunTests
            .execute(&serde_json::json!({"kind": "all"}), root.path())
            .await;
        match &out {
            ToolOutput::Ok { content } => assert!(content.contains("make-test-ran"), "{content}"),
            other => panic!("{other:?}"),
        }

        // A bare `test` target must NOT pass itself off as e2e — the same
        // no-lying rule the npm-family mapping enforces.
        let out = RunTests
            .execute(&serde_json::json!({"kind": "e2e"}), root.path())
            .await;
        assert!(out.is_error(), "no e2e target → named error: {out:?}");
    }

    #[tokio::test]
    async fn lint_and_format_argv_shapes_are_index_resolved() {
        let join = |argv: Vec<String>| argv.join(" ");

        // Cargo workspace: the synthesized clippy/fmt verb bindings, with
        // the cargo-only fix/check flags appended.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let cargo = ScriptIndex::detect(root.path()).await;
        assert_eq!(
            join(lint_argv(&cargo, false).unwrap()),
            "cargo clippy --workspace --all-targets"
        );
        assert_eq!(
            join(lint_argv(&cargo, true).unwrap()),
            "cargo clippy --workspace --all-targets --fix --allow-dirty"
        );
        assert_eq!(join(format_argv(&cargo, false).unwrap()), "cargo fmt");
        assert_eq!(
            join(format_argv(&cargo, true).unwrap()),
            "cargo fmt --check"
        );

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts": {"lint": "eslint .", "format": "prettier -w ."}}"#,
        )
        .unwrap();
        std::fs::write(root.path().join("pnpm-lock.yaml"), "").unwrap();
        let node = ScriptIndex::detect(root.path()).await;
        assert_eq!(join(lint_argv(&node, false).unwrap()), "pnpm run lint");
        assert_eq!(join(format_argv(&node, false).unwrap()), "pnpm run format");
        // fix/check don't guess a node flag — they refuse, pointing at
        // run_script for the project's own fix/check verbs.
        assert!(lint_argv(&node, true).unwrap_err().contains("run_script"));
        assert!(format_argv(&node, true).unwrap_err().contains("run_script"));

        // No verb binding — empty index, and node without the scripts: the
        // error NAMES what was looked for.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("package.json"), r#"{"name": "bare"}"#).unwrap();
        let bare = ScriptIndex::detect(root.path()).await;
        for err in [
            lint_argv(&ScriptIndex::default(), false).unwrap_err(),
            lint_argv(&bare, false).unwrap_err(),
        ] {
            assert!(err.contains("no linter configured"), "{err}");
            assert!(err.contains("package.json `lint` script"), "{err}");
        }
        let err = format_argv(&bare, false).unwrap_err();
        assert!(err.contains("no formatter configured"), "{err}");
        assert!(err.contains("package.json `format` script"), "{err}");
    }

    #[test]
    fn diagnostic_counts_parse_rustc_style_lines() {
        let out = "warning: unused variable `x`\n  --> src/a.rs:1:1\n\
                   error[E0308]: mismatched types\nsome other line\n";
        assert_eq!(diagnostic_counts(out), (1, 1));
        assert_eq!(diagnostic_counts("all clean\n"), (0, 0));
    }

    #[tokio::test]
    async fn run_lint_names_the_missing_linter_in_an_empty_workspace() {
        let root = tempfile::tempdir().unwrap();
        let out = RunLint.execute(&serde_json::json!({}), root.path()).await;
        match &out {
            ToolOutput::Error { message } => {
                assert!(message.contains("no linter configured"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        let out = FormatCode
            .execute(&serde_json::json!({}), root.path())
            .await;
        assert!(out.is_error(), "{out:?}");
    }
}
