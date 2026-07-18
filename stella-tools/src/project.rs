//! `build_project` and `run_tests` — toolchain-aware build/test execution.
//!
//! Both detect the workspace's toolchain (Cargo, package.json + its
//! package manager, Go, Make) and run its canonical command, or accept an
//! explicit `command` override for anything exotic. `run_tests` supports
//! `kind` (unit / e2e / all) and a `filter` mapped to the runner's native
//! filtering flag — module, package, file, or test-name granularity is the
//! runner's own semantics.

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
}
