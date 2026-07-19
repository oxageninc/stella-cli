//! `build_project`, `run_tests`, `run_lint`, `format_code` —
//! toolchain-aware build/test/lint/format execution.
//!
//! All four detect the workspace's toolchain (Cargo, package.json + its
//! package manager, Go, Make) and run its canonical command; build/test
//! also accept an explicit `command` override for anything exotic.
//! `run_tests` supports `kind` (unit / e2e / all) and a `filter` mapped to
//! the runner's native filtering flag — module, package, file, or
//! test-name granularity is the runner's own semantics. `run_lint` and
//! `format_code` take no free-form command at all: they run the resolved
//! toolchain verb via argv exec (no shell), and a workspace with no
//! recognized linter/formatter is a named error, not a guess.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::exec;
use crate::registry::Tool;

const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// What toolchain drives this workspace.
enum Toolchain {
    Cargo,
    Node {
        pm: &'static str,
        scripts: serde_json::Map<String, Value>,
    },
    Go,
    Make,
}

/// Async so the marker-file stats and the `package.json` read never block a
/// runtime worker thread (#64) — this runs inside the tools' async
/// `execute()` methods.
async fn detect(root: &std::path::Path) -> Option<Toolchain> {
    let exists = |name: &'static str| async move {
        tokio::fs::try_exists(root.join(name))
            .await
            .unwrap_or(false)
    };
    if exists("Cargo.toml").await {
        return Some(Toolchain::Cargo);
    }
    if exists("package.json").await {
        let pm = if exists("pnpm-lock.yaml").await {
            "pnpm"
        } else if exists("yarn.lock").await {
            "yarn"
        } else {
            "npm"
        };
        let scripts = tokio::fs::read_to_string(root.join("package.json"))
            .await
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|pkg| pkg.get("scripts").and_then(|s| s.as_object()).cloned())
            .unwrap_or_default();
        return Some(Toolchain::Node { pm, scripts });
    }
    if exists("go.mod").await {
        return Some(Toolchain::Go);
    }
    if exists("Makefile").await {
        return Some(Toolchain::Make);
    }
    None
}

fn no_toolchain_error() -> ToolOutput {
    ToolOutput::Error {
        message: "no recognized toolchain (looked for Cargo.toml, package.json, go.mod, \
                  Makefile) — pass `command` explicitly"
            .into(),
    }
}

async fn run_and_report(command: &str, root: &std::path::Path, timeout_secs: u64) -> ToolOutput {
    match exec::run(command, root, timeout_secs).await {
        Ok((0, output)) => ToolOutput::Ok {
            content: format!("`{command}` PASSED (exit 0)\n{output}"),
        },
        Ok((code, output)) => ToolOutput::Error {
            message: format!("`{command}` FAILED (exit {code})\n{output}"),
        },
        Err(e) => ToolOutput::Error { message: e },
    }
}

pub struct BuildProject;

#[async_trait]
impl Tool for BuildProject {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "build_project".into(),
            description: "Build the workspace with its own toolchain (cargo/npm/go/make), or a \
                          custom command."
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
        let command = match detect(root).await {
            Some(Toolchain::Cargo) => "cargo build --workspace".to_string(),
            Some(Toolchain::Node { pm, scripts }) => {
                if scripts.contains_key("build") {
                    format!("{pm} run build")
                } else {
                    return ToolOutput::Error {
                        message: "package.json has no `build` script — pass `command`".into(),
                    };
                }
            }
            Some(Toolchain::Go) => "go build ./...".to_string(),
            Some(Toolchain::Make) => "make build".to_string(),
            None => return no_toolchain_error(),
        };
        run_and_report(&command, root, timeout_secs).await
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

        let command = match detect(root).await {
            Some(Toolchain::Cargo) => match kind {
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
            Some(Toolchain::Node { pm, scripts }) => {
                let script = match kind {
                    "unit" if scripts.contains_key("test:unit") => "test:unit",
                    "e2e" if scripts.contains_key("test:e2e") => "test:e2e",
                    "e2e" if scripts.contains_key("e2e") => "e2e",
                    // NO generic-`test` fallback for e2e: running unit tests
                    // while reporting "e2e passed" is a lie.
                    "unit" | "all" if scripts.contains_key("test") => "test",
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
            Some(Toolchain::Go) => {
                if filter.is_empty() {
                    "go test ./...".to_string()
                } else {
                    format!("go test ./... -run {filter}")
                }
            }
            Some(Toolchain::Make) => "make test".to_string(),
            None => return no_toolchain_error(),
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

/// The linter argv for the detected toolchain, or a named error. Pure —
/// separable from process spawning so the command shape is unit-testable.
fn lint_argv(toolchain: Option<&Toolchain>, fix: bool) -> Result<Vec<String>, String> {
    match toolchain {
        Some(Toolchain::Cargo) => {
            let mut argv = vec!["cargo", "clippy", "--workspace", "--all-targets"];
            if fix {
                argv.push("--fix");
                argv.push("--allow-dirty");
            }
            Ok(argv.into_iter().map(String::from).collect())
        }
        Some(Toolchain::Node { pm, scripts }) if scripts.contains_key("lint") => {
            if fix {
                // The `lint` script's fix flag is linter-specific; guessing
                // (`-- --fix`) could rewrite files under a linter that
                // treats it as a filename. Named refusal instead.
                return Err("`fix` resolves to cargo only in this version — run the \
                            project's own fix script (e.g. `lint:fix`) via run_script"
                    .into());
            }
            Ok(vec![pm.to_string(), "run".into(), "lint".into()])
        }
        _ => Err(no_tool_error("linter", "a package.json `lint` script")),
    }
}

/// The formatter argv for the detected toolchain, or a named error. Pure,
/// like [`lint_argv`].
fn format_argv(toolchain: Option<&Toolchain>, check: bool) -> Result<Vec<String>, String> {
    match toolchain {
        Some(Toolchain::Cargo) => {
            let mut argv = vec!["cargo".to_string(), "fmt".to_string()];
            if check {
                argv.push("--check".into());
            }
            Ok(argv)
        }
        Some(Toolchain::Node { pm, scripts }) if scripts.contains_key("format") => {
            if check {
                return Err("`check` resolves to cargo only in this version — run the \
                            project's own check script (e.g. `format:check`) via run_script"
                    .into());
            }
            Ok(vec![pm.to_string(), "run".into(), "format".into()])
        }
        _ => Err(no_tool_error("formatter", "a package.json `format` script")),
    }
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
        match lint_argv(detect(root).await.as_ref(), fix) {
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
        match format_argv(detect(root).await.as_ref(), check) {
            Ok(argv) => run_argv_and_report(&argv, root, timeout_secs).await,
            Err(message) => ToolOutput::Error { message },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("stella_project_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn no_toolchain_is_a_named_error_and_command_overrides() {
        let root = temp_root("none");
        let out = BuildProject.execute(&serde_json::json!({}), &root).await;
        assert!(out.is_error());

        let out = BuildProject
            .execute(
                &serde_json::json!({"command": "echo built && exit 0"}),
                &root,
            )
            .await;
        assert!(!out.is_error(), "{out:?}");

        let out = RunTests
            .execute(&serde_json::json!({"command": "exit 1"}), &root)
            .await;
        match &out {
            ToolOutput::Error { message } => assert!(message.contains("FAILED"), "{message}"),
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn node_detection_uses_scripts_and_pm_lockfile() {
        let root = temp_root("node");
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts": {"test": "echo node-tests-ran", "build": "echo node-build-ran"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();

        // pnpm may not exist in the test environment — we only assert the
        // detection path constructs the right command shape, visible in the
        // success/error text either way.
        let out = RunTests
            .execute(&serde_json::json!({"kind": "all"}), &root)
            .await;
        let text = match &out {
            ToolOutput::Ok { content } => content.clone(),
            ToolOutput::Error { message } => message.clone(),
        };
        assert!(text.contains("pnpm run test"), "{text}");

        let out = RunTests
            .execute(&serde_json::json!({"kind": "e2e"}), &root)
            .await;
        assert!(out.is_error(), "no e2e script → named error: {out:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn lint_and_format_argv_shapes_are_toolchain_resolved() {
        let join = |argv: Vec<String>| argv.join(" ");
        assert_eq!(
            join(lint_argv(Some(&Toolchain::Cargo), false).unwrap()),
            "cargo clippy --workspace --all-targets"
        );
        assert_eq!(
            join(lint_argv(Some(&Toolchain::Cargo), true).unwrap()),
            "cargo clippy --workspace --all-targets --fix --allow-dirty"
        );
        assert_eq!(
            join(format_argv(Some(&Toolchain::Cargo), false).unwrap()),
            "cargo fmt"
        );
        assert_eq!(
            join(format_argv(Some(&Toolchain::Cargo), true).unwrap()),
            "cargo fmt --check"
        );

        let scripts: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{"lint": "eslint .", "format": "prettier -w ."}"#).unwrap();
        let node = Toolchain::Node {
            pm: "pnpm",
            scripts,
        };
        assert_eq!(
            join(lint_argv(Some(&node), false).unwrap()),
            "pnpm run lint"
        );
        assert_eq!(
            join(format_argv(Some(&node), false).unwrap()),
            "pnpm run format"
        );
        // fix/check don't guess a node flag — they refuse, pointing at
        // run_script for the project's own fix/check verbs.
        assert!(
            lint_argv(Some(&node), true)
                .unwrap_err()
                .contains("run_script")
        );
        assert!(
            format_argv(Some(&node), true)
                .unwrap_err()
                .contains("run_script")
        );

        // No toolchain, and node without the scripts: the error NAMES what
        // was looked for.
        let bare = Toolchain::Node {
            pm: "npm",
            scripts: serde_json::Map::new(),
        };
        for err in [
            lint_argv(None, false).unwrap_err(),
            lint_argv(Some(&bare), false).unwrap_err(),
        ] {
            assert!(err.contains("no linter configured"), "{err}");
            assert!(err.contains("package.json `lint` script"), "{err}");
        }
        let err = format_argv(Some(&bare), false).unwrap_err();
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
        let root = temp_root("lint_none");
        let out = RunLint.execute(&serde_json::json!({}), &root).await;
        match &out {
            ToolOutput::Error { message } => {
                assert!(message.contains("no linter configured"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        let out = FormatCode.execute(&serde_json::json!({}), &root).await;
        assert!(out.is_error(), "{out:?}");
        std::fs::remove_dir_all(&root).ok();
    }
}
