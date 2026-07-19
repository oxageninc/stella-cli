//! `run_script` — run a verb the project itself declares, and nothing else.
//!
//! The tool's vocabulary is enumerated from the workspace's own manifests:
//! `Makefile` targets, `package.json` `"scripts"`, and cargo aliases from
//! `.cargo/config.toml`. A name outside that vocabulary is a named error
//! that LISTS the discovered verbs — the error itself is the discovery
//! surface, so the model learns the project's real commands instead of
//! guessing shell lines. Arguments are passed argv-style to the runner
//! (`make <target> <args…>`, `<pm> run <script> -- <args…>`,
//! `cargo <alias> <args…>`) — no `sh -c`, no shell interpretation anywhere.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::exec;
use crate::registry::Tool;

const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// Where a discovered verb came from — decides the runner argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptSource {
    Make,
    Package,
    CargoAlias,
}

/// The project's declared verbs, grouped by manifest. Resolution order on a
/// name collision is Makefile → package.json → cargo alias (first match
/// wins), matching the enumeration order below.
#[derive(Debug, Default)]
struct Vocabulary {
    make_targets: Vec<String>,
    package_scripts: Vec<String>,
    /// The package manager for `package_scripts` (lockfile-detected, same
    /// rule as [`crate::project`]).
    pm: &'static str,
    cargo_aliases: Vec<String>,
}

impl Vocabulary {
    fn is_empty(&self) -> bool {
        self.make_targets.is_empty()
            && self.package_scripts.is_empty()
            && self.cargo_aliases.is_empty()
    }

    /// First source declaring `name`, in resolution order.
    fn resolve(&self, name: &str) -> Option<ScriptSource> {
        if self.make_targets.iter().any(|t| t == name) {
            return Some(ScriptSource::Make);
        }
        if self.package_scripts.iter().any(|s| s == name) {
            return Some(ScriptSource::Package);
        }
        if self.cargo_aliases.iter().any(|a| a == name) {
            return Some(ScriptSource::CargoAlias);
        }
        None
    }

    /// The unknown-name error: the full vocabulary, grouped by source —
    /// this listing is the tool's discoverability surface.
    fn unknown_name_error(&self, name: &str) -> String {
        if self.is_empty() {
            return format!(
                "unknown script `{name}` — this project declares no runnable verbs \
                 (no Makefile targets, package.json scripts, or cargo aliases found)"
            );
        }
        let mut groups = Vec::new();
        if !self.make_targets.is_empty() {
            groups.push(format!(
                "Makefile targets: {}",
                self.make_targets.join(", ")
            ));
        }
        if !self.package_scripts.is_empty() {
            groups.push(format!(
                "package.json scripts: {}",
                self.package_scripts.join(", ")
            ));
        }
        if !self.cargo_aliases.is_empty() {
            groups.push(format!("cargo aliases: {}", self.cargo_aliases.join(", ")));
        }
        format!(
            "unknown script `{name}` — this project declares: {}",
            groups.join("; ")
        )
    }
}

/// Parse Makefile rule targets: lines opening with a plain
/// `^[A-Za-z0-9_.-]+:` target. Skipped on purpose: special/`.PHONY`-style
/// dot-targets, pattern rules (`%`), and variable assignments (`NAME :=`).
fn parse_make_targets(src: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for line in src.lines() {
        let Some(colon) = line.find(':') else {
            continue;
        };
        let name = &line[..colon];
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            || name.starts_with('.')
        {
            continue;
        }
        // `NAME := value` / `NAME ::= value` are assignments, not rules.
        let rest = &line[colon + 1..];
        if rest.starts_with('=') || rest.starts_with(":=") {
            continue;
        }
        if !targets.iter().any(|t| t == name) {
            targets.push(name.to_string());
        }
    }
    targets
}

/// Parse `[alias]` keys out of `.cargo/config.toml`.
fn parse_cargo_aliases(src: &str) -> Vec<String> {
    let Ok(value) = toml::from_str::<toml::Value>(src) else {
        return Vec::new();
    };
    value
        .get("alias")
        .and_then(|a| a.as_table())
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default()
}

/// Enumerate the workspace's declared verbs. Async file probes for the same
/// reason as [`crate::project`]'s `detect` — this runs inside `execute()`.
async fn discover(root: &std::path::Path) -> Vocabulary {
    let mut vocab = Vocabulary {
        pm: "npm",
        ..Vocabulary::default()
    };
    if let Ok(src) = tokio::fs::read_to_string(root.join("Makefile")).await {
        vocab.make_targets = parse_make_targets(&src);
    }
    if let Ok(raw) = tokio::fs::read_to_string(root.join("package.json")).await
        && let Ok(pkg) = serde_json::from_str::<Value>(&raw)
        && let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object())
    {
        vocab.package_scripts = scripts.keys().cloned().collect();
        let exists = |name: &'static str| async move {
            tokio::fs::try_exists(root.join(name))
                .await
                .unwrap_or(false)
        };
        vocab.pm = if exists("pnpm-lock.yaml").await {
            "pnpm"
        } else if exists("yarn.lock").await {
            "yarn"
        } else {
            "npm"
        };
    }
    if let Ok(src) = tokio::fs::read_to_string(root.join(".cargo").join("config.toml")).await {
        vocab.cargo_aliases = parse_cargo_aliases(&src);
    }
    vocab
}

/// The runner argv for a resolved verb — pure, so the shape is testable.
fn runner_argv(source: ScriptSource, pm: &str, name: &str, args: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = match source {
        ScriptSource::Make => vec!["make".into(), name.into()],
        ScriptSource::Package => {
            let mut v = vec![pm.to_string(), "run".into(), name.into()];
            if !args.is_empty() {
                v.push("--".into());
            }
            v
        }
        ScriptSource::CargoAlias => vec!["cargo".into(), name.into()],
    };
    argv.extend(args.iter().cloned());
    argv
}

/// `run_script` — see the module doc.
pub struct RunScript;

#[async_trait]
impl Tool for RunScript {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "run_script".into(),
            description: "Run a verb this project itself declares: a Makefile target, a \
                          package.json script, or a cargo alias. args are passed argv-style \
                          (never through a shell). An unknown name returns the full list of \
                          declared verbs — call it with any name to discover them."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "A declared verb (Makefile target, package.json script, or cargo alias)" },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Extra arguments, passed argv-style to the runner" },
                    "timeout_secs": { "type": "integer" }
                },
                "required": ["name"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let Some(name) = input.get("name").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "missing required field `name`".into(),
            };
        };
        let args: Vec<String> = input
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let vocab = discover(root).await;
        let Some(source) = vocab.resolve(name) else {
            return ToolOutput::Error {
                message: vocab.unknown_name_error(name),
            };
        };
        let argv = runner_argv(source, vocab.pm, name, &args);
        let display = argv.join(" ");
        match exec::run_argv(&argv[0], &argv[1..], root, timeout_secs).await {
            Ok((0, output)) => ToolOutput::Ok {
                content: format!("`{display}` PASSED (exit 0)\n{output}"),
            },
            Ok((code, output)) => ToolOutput::Error {
                message: format!("`{display}` FAILED (exit {code})\n{output}"),
            },
            Err(e) => ToolOutput::Error { message: e },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_targets_parse_rules_and_skip_specials_and_assignments() {
        let src = "\
.PHONY: build test\n\
build: deps\n\
\tcargo build\n\
test:\n\
\tcargo test\n\
%.o: %.c\n\
\tcc -c $<\n\
VERSION := 1.2.3\n\
NAME:=x\n\
gate-all.v2: build\n";
        assert_eq!(
            parse_make_targets(src),
            vec!["build", "test", "gate-all.v2"]
        );
    }

    #[test]
    fn cargo_aliases_parse_the_alias_table() {
        let src = "[alias]\ntc = \"test -p stella-core\"\ngate = [\"fmt\", \"--check\"]\n\n[build]\njobs = 4\n";
        let mut aliases = parse_cargo_aliases(src);
        aliases.sort();
        assert_eq!(aliases, vec!["gate", "tc"]);
        assert!(parse_cargo_aliases("not toml [").is_empty());
    }

    #[test]
    fn runner_argv_is_shell_free_and_source_shaped() {
        let args = vec!["--flag".to_string(), "a b".to_string()];
        assert_eq!(
            runner_argv(ScriptSource::Make, "npm", "gate", &args).join("|"),
            "make|gate|--flag|a b"
        );
        assert_eq!(
            runner_argv(ScriptSource::Package, "pnpm", "dev", &args).join("|"),
            "pnpm|run|dev|--|--flag|a b"
        );
        // No args → no dangling `--`.
        assert_eq!(
            runner_argv(ScriptSource::Package, "npm", "dev", &[]).join("|"),
            "npm|run|dev"
        );
        assert_eq!(
            runner_argv(ScriptSource::CargoAlias, "npm", "tc", &args).join("|"),
            "cargo|tc|--flag|a b"
        );
    }

    #[tokio::test]
    async fn unknown_name_lists_the_discovered_vocabulary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Makefile"),
            "build:\n\ttrue\ngate:\n\ttrue\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"dev": "vite", "lint": "eslint ."}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        std::fs::write(
            dir.path().join(".cargo/config.toml"),
            "[alias]\ntc = \"test -p stella-core\"\n",
        )
        .unwrap();

        let out = RunScript
            .execute(&serde_json::json!({"name": "nope"}), dir.path())
            .await;
        let ToolOutput::Error { message } = out else {
            panic!("unknown name must be an error");
        };
        for expected in [
            "unknown script `nope`",
            "Makefile targets: build, gate",
            "dev",
            "lint",
            "cargo aliases: tc",
        ] {
            assert!(message.contains(expected), "missing {expected}: {message}");
        }
    }

    #[tokio::test]
    async fn an_empty_project_names_every_manifest_it_looked_for() {
        let dir = tempfile::tempdir().unwrap();
        let out = RunScript
            .execute(&serde_json::json!({"name": "build"}), dir.path())
            .await;
        let ToolOutput::Error { message } = out else {
            panic!("expected error");
        };
        assert!(message.contains("declares no runnable verbs"), "{message}");
        assert!(message.contains("Makefile targets"), "{message}");
    }

    /// Whether `make` exists on PATH — the execution test skips cleanly
    /// without it (mirrors the real-git skip in stella-fleet).
    async fn make_available() -> bool {
        tokio::process::Command::new("make")
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn a_declared_make_target_runs_and_args_pass_argv_style() {
        if !make_available().await {
            eprintln!("skipping run_script make test: `make` not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // The target prints its first argument-variable-free marker; args
        // land as extra make goals, so use a no-op goal to prove they are
        // separate argv entries and never shell-joined.
        std::fs::write(
            dir.path().join("Makefile"),
            "hello:\n\t@echo script_ran_ok\nnoop:\n\t@true\n",
        )
        .unwrap();
        let out = RunScript
            .execute(
                &serde_json::json!({"name": "hello", "args": ["noop"]}),
                dir.path(),
            )
            .await;
        match out {
            ToolOutput::Ok { content } => {
                assert!(content.contains("script_ran_ok"), "{content}");
                assert!(content.contains("`make hello noop` PASSED"), "{content}");
            }
            ToolOutput::Error { message } => panic!("expected ok: {message}"),
        }
    }
}
