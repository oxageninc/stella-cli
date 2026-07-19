//! Project scripts index — static, deterministic detection of
//! package-manager scripts across ecosystems, mapped onto six canonical
//! verbs (`install`, `build`, `start`, `test`, `lint`, `format`).
//!
//! Spec: `docs/design/scripts-index.md`. Three surfaces share this module:
//! the byte-stable `## Project scripts` prompt section, the
//! `list_scripts`/`run_script` tools, and the `stella scripts` subcommand.
//! Detection never invokes a package-manager binary and persists nothing —
//! it is a handful of manifest reads, cheap enough to recompute on every
//! call, so there is no cache file, no database, and no watcher to go
//! stale. Same workspace state ⇒ byte-identical output (entries sort by
//! `(dir, id)`, verbs render in fixed order).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::exec;
use crate::registry::Tool;

/// The canonical verbs, in the fixed render/resolution order.
pub const VERBS: [&str; 6] = ["install", "build", "start", "test", "lint", "format"];

const DEFAULT_TIMEOUT_SECS: u64 = 600;
/// Workspace-member enumeration cap — overflow is counted, not enumerated.
const MAX_WORKSPACE_MEMBERS: usize = 50;
/// Hard cap on the prompt section (it rides the byte-stable cached prefix).
const PROMPT_SECTION_CHAR_CAP: usize = 1_500;
/// Per-command cap inside the prompt section.
const PROMPT_COMMAND_CHAR_CAP: usize = 120;

/// One indexed script: a qualified id (`<runner>:<name>`), the exact command
/// `run_script` executes (cwd = `dir`), and where it came from.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScriptEntry {
    /// Qualified id, unique per package dir: `pnpm:build`, `make:lint`.
    pub id: String,
    /// The tool that runs it: `cargo`, `pnpm`, `uv`, `make`, …
    pub runner: &'static str,
    /// The script/target/recipe name inside its ecosystem.
    pub name: String,
    /// Exact command executed, with cwd = `dir`.
    pub command: String,
    /// Workspace-relative package dir; `"."` = root.
    pub dir: String,
    /// Workspace-relative manifest path, or `"synthesized"` for ecosystem
    /// defaults (e.g. `cargo build --workspace`).
    pub source: String,
    /// Present only on the entry each canonical verb binds to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verb: Option<&'static str>,
    /// The manifest's own definition (e.g. the package.json script body).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl ScriptEntry {
    fn synthesized(&self) -> bool {
        self.source == "synthesized"
    }
}

/// The detected index: all entries sorted by `(dir, id)` plus the canonical
/// verb → qualified-id bindings (root package only).
#[derive(Debug, Clone, Default)]
pub struct ScriptIndex {
    pub scripts: Vec<ScriptEntry>,
    pub verbs: BTreeMap<&'static str, String>,
    /// Workspace members beyond [`MAX_WORKSPACE_MEMBERS`] that were skipped.
    pub truncated_members: usize,
}

/// Marker-order rank of a runner's ecosystem — the precedence used both for
/// verb binding and for the "Detected:" line. npm-family runners share one
/// rank: a package has exactly one of them.
fn runner_rank(runner: &str) -> u8 {
    match runner {
        "cargo" => 1,
        "npm" | "pnpm" | "yarn" | "bun" => 2,
        "deno" => 3,
        "uv" | "poetry" => 4,
        "go" => 5,
        "make" => 6,
        "just" => 7,
        "task" => 8,
        "composer" => 9,
        _ => 10,
    }
}

fn is_npm_family(runner: &str) -> bool {
    matches!(runner, "npm" | "pnpm" | "yarn" | "bun")
}

/// Explicit script names each verb matches, in priority order.
fn verb_aliases(verb: &str) -> &'static [&'static str] {
    match verb {
        "install" => &["install", "setup", "bootstrap"],
        "build" => &["build", "compile", "dist"],
        "start" => &["start", "dev", "serve"],
        "test" => &["test", "tests"],
        "lint" => &["lint"],
        "format" => &["format", "fmt"],
        _ => &[],
    }
}

/// Names that are never implicitly verb-bound: a canonical verb must not
/// trigger a watcher or an outward-facing/destructive action.
fn verb_eligible(name: &str) -> bool {
    !name.contains("watch") && !matches!(name, "publish" | "deploy" | "release" | "clean")
}

impl ScriptIndex {
    /// Detect the workspace's scripts synchronously. Small manifest reads
    /// only — safe at session start next to the memory loading, which does
    /// the same kind of I/O.
    pub fn detect_blocking(root: &Path) -> Self {
        let mut entries: Vec<ScriptEntry> = Vec::new();
        let root_pm = node_pm(root, None);
        detect_package(root, ".", true, root_pm, &mut entries);
        let (members, truncated_members) = workspace_members(root);
        for dir in &members {
            detect_package(root, dir, false, root_pm, &mut entries);
        }
        // Stable sort + first-wins dedup: detectors push explicit entries
        // before synthesized ones, so an alias/script named e.g. `install`
        // beats the synthesized `install` at the same (dir, id).
        entries
            .sort_by(|a, b| (a.dir.as_str(), a.id.as_str()).cmp(&(b.dir.as_str(), b.id.as_str())));
        entries.dedup_by(|a, b| a.dir == b.dir && a.id == b.id);
        let verbs = resolve_verbs(&mut entries);
        ScriptIndex {
            scripts: entries,
            verbs,
            truncated_members,
        }
    }

    /// [`ScriptIndex::detect_blocking`] on the blocking pool — the form tool
    /// `execute()` methods use so manifest reads never block a runtime
    /// worker thread (#64).
    pub async fn detect(root: &Path) -> Self {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || Self::detect_blocking(&root))
            .await
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty()
    }

    /// Distinct runners across all entries, in ecosystem-rank order.
    pub fn detected_runners(&self) -> Vec<&'static str> {
        let mut runners: Vec<&'static str> = self.scripts.iter().map(|e| e.runner).collect();
        runners.sort_by_key(|r| (runner_rank(r), *r));
        runners.dedup();
        runners
    }

    /// The root-package entry a canonical verb binds to, if any.
    pub fn verb_entry(&self, verb: &str) -> Option<&ScriptEntry> {
        let id = self.verbs.get(verb)?;
        self.scripts.iter().find(|e| e.dir == "." && &e.id == id)
    }

    /// The rank-lowest runner among root-package entries — the workspace's
    /// primary ecosystem (what `run_tests` keys its kind mapping on).
    pub fn primary_runner(&self) -> Option<&'static str> {
        self.scripts
            .iter()
            .filter(|e| e.dir == ".")
            .min_by_key(|e| runner_rank(e.runner))
            .map(|e| e.runner)
    }

    /// Explicit (non-synthesized) script names at the root for `runner`.
    pub fn root_script_names(&self, runner: &str) -> BTreeSet<&str> {
        self.scripts
            .iter()
            .filter(|e| e.dir == "." && e.runner == runner && !e.synthesized())
            .map(|e| e.name.as_str())
            .collect()
    }

    /// Resolve a `run_script` input: a canonical verb, a qualified id, or a
    /// unique bare name. `dir` narrows to one package when the same id
    /// exists in several.
    pub fn resolve(&self, script: &str, dir: Option<&str>) -> Result<&ScriptEntry, String> {
        if self.is_empty() {
            return Err(no_scripts_message());
        }
        if VERBS.contains(&script) {
            return self.verb_entry(script).ok_or_else(|| {
                format!("no `{script}` script detected in this workspace — run list_scripts for what exists")
            });
        }
        let dir_matches = |e: &&ScriptEntry| dir.is_none_or(|d| e.dir == d.trim_end_matches('/'));
        let mut pool: Vec<&ScriptEntry> = self
            .scripts
            .iter()
            .filter(|e| e.id == script)
            .filter(dir_matches)
            .collect();
        if pool.is_empty() {
            pool = self
                .scripts
                .iter()
                .filter(|e| e.name == script)
                .filter(dir_matches)
                .collect();
        }
        match pool.len() {
            1 => Ok(pool[0]),
            0 => {
                let near: Vec<&str> = self
                    .scripts
                    .iter()
                    .filter(|e| e.id.contains(script) || script.contains(e.name.as_str()))
                    .map(|e| e.id.as_str())
                    .take(5)
                    .collect();
                let hint = if near.is_empty() {
                    String::new()
                } else {
                    format!(" Close matches: {}.", near.join(", "))
                };
                Err(format!(
                    "unknown script `{script}` — nothing indexed matches.{hint} Run list_scripts for the full list."
                ))
            }
            _ => {
                let ids: Vec<String> = pool
                    .iter()
                    .map(|e| format!("{} ({})", e.id, e.dir))
                    .collect();
                Err(format!(
                    "`{script}` is ambiguous — matches {}; pass the qualified id and a `dir`",
                    ids.join(", ")
                ))
            }
        }
    }

    /// The `## Project scripts` block for the byte-stable system prompt:
    /// the verb bindings inline, everything else as a count + teaser.
    /// `None` when nothing was detected — no section, no noise.
    pub fn render_prompt_section(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut s = String::from("## Project scripts\n\nDetected: ");
        s.push_str(&self.detected_runners().join(", "));
        s.push_str(
            ". Run these with the run_script tool — do not rediscover them with bash/cat.\n\n",
        );
        for verb in VERBS {
            if let Some(entry) = self.verb_entry(verb) {
                let mut command = entry.command.clone();
                if command.chars().count() > PROMPT_COMMAND_CHAR_CAP {
                    command = command.chars().take(PROMPT_COMMAND_CHAR_CAP - 1).collect();
                    command.push('…');
                }
                s.push_str(&format!("{verb} → {command}\n"));
            }
        }
        let unbound: Vec<&str> = self
            .scripts
            .iter()
            .filter(|e| e.verb.is_none())
            .map(|e| e.id.as_str())
            .collect();
        if !unbound.is_empty() {
            let teaser = unbound
                .iter()
                .take(3)
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            let ellipsis = if unbound.len() > 3 { ", …" } else { "" };
            s.push_str(&format!(
                "\n{} more scripts ({teaser}{ellipsis}): call list_scripts.\n",
                unbound.len()
            ));
        }
        if s.chars().count() > PROMPT_SECTION_CHAR_CAP {
            s = s.chars().take(PROMPT_SECTION_CHAR_CAP - 1).collect();
            s.push('…');
        }
        Some(s.trim_end().to_string())
    }

    /// The human/model list frame — shared verbatim between `list_scripts`
    /// and `stella scripts list`. `dir_filter` narrows the entry listing to
    /// one package.
    pub fn render_list(&self, dir_filter: Option<&str>) -> String {
        if self.is_empty() {
            return no_scripts_message();
        }
        let mut s = format!(
            "Project scripts — detected: {} (static manifest parse)\n",
            self.detected_runners().join(", ")
        );
        if dir_filter.is_none() {
            let mut any = false;
            for verb in VERBS {
                if let Some(entry) = self.verb_entry(verb) {
                    if !any {
                        s.push_str("\nCanonical verbs (run_script accepts these names):\n");
                        any = true;
                    }
                    s.push_str(&format!(
                        "  {verb:<8} {:<44} [{}]\n",
                        entry.command, entry.id
                    ));
                }
            }
        }
        s.push_str("\nAll scripts (id · command · source):\n");
        let mut listed = 0usize;
        for entry in &self.scripts {
            if dir_filter.is_some_and(|d| entry.dir != d.trim_end_matches('/')) {
                continue;
            }
            listed += 1;
            let loc = if entry.dir == "." {
                String::new()
            } else {
                format!("{} › ", entry.dir)
            };
            s.push_str(&format!(
                "  {loc}{:<24} {:<44} {}\n",
                entry.id, entry.command, entry.source
            ));
        }
        if listed == 0 {
            s.push_str("  (none in that dir)\n");
        }
        if self.truncated_members > 0 {
            s.push_str(&format!(
                "\n({} workspace members beyond the {MAX_WORKSPACE_MEMBERS}-member cap were not indexed)\n",
                self.truncated_members
            ));
        }
        s.trim_end().to_string()
    }

    /// The machine frame (`stella scripts list --json`), schema_version 1 —
    /// shape pinned by `docs/design/scripts-index.md`.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "schema_version": 1,
            "verbs": self.verbs,
            "scripts": self.scripts,
        })
    }
}

fn no_scripts_message() -> String {
    "no project scripts detected (looked for Cargo.toml, package.json, deno.json(c), \
     pyproject.toml, go.mod, Makefile, justfile, Taskfile.yml, composer.json)"
        .to_string()
}

/// Compose the command actually executed: the entry's command plus
/// shell-quoted extra args, joined runner-natively (`--` separator for
/// npm-family script runs, plain append otherwise).
pub fn compose_command(entry: &ScriptEntry, args: &[String]) -> String {
    if args.is_empty() {
        return entry.command.clone();
    }
    let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    let sep = if is_npm_family(entry.runner) && !entry.synthesized() {
        " -- "
    } else {
        " "
    };
    format!("{}{sep}{}", entry.command, quoted.join(" "))
}

/// Single-quote an argument unless it is a plain word — the composed line
/// runs through `bash -c` (crate::exec), so args must never re-tokenize.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/:=@%+,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Resolve `run_script`'s command line for the registry's `command.started`
/// policy chain — best-effort: `None` (no gating) when the input or index
/// can't resolve, in which case the tool itself returns the named error.
pub(crate) async fn resolve_command_for_gate(root: &Path, input: &Value) -> Option<String> {
    let script = input.get("script")?.as_str()?;
    let dir = input.get("dir").and_then(|v| v.as_str());
    let args = string_args(input);
    let index = ScriptIndex::detect(root).await;
    let entry = index.resolve(script, dir).ok()?;
    Some(compose_command(entry, &args))
}

/// Resolve and run one script — the single execution path shared by the
/// `run_script` tool and `stella scripts run`.
pub async fn run_by_name(
    root: &Path,
    script: &str,
    dir: Option<&str>,
    args: &[String],
    timeout_secs: u64,
) -> ToolOutput {
    let index = ScriptIndex::detect(root).await;
    let entry = match index.resolve(script, dir) {
        Ok(entry) => entry,
        Err(message) => return ToolOutput::Error { message },
    };
    let command = compose_command(entry, args);
    let cwd = if entry.dir == "." {
        root.to_path_buf()
    } else {
        root.join(&entry.dir)
    };
    exec::run_and_report(&command, &cwd, timeout_secs).await
}

fn string_args(input: &Value) -> Vec<String> {
    input
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Run every ecosystem detector over one package dir. Cargo synthesizes at
/// the workspace root only — per-crate cargo entries would be noise (the
/// `--workspace` commands already cover the members).
fn detect_package(
    root: &Path,
    rel: &str,
    is_root: bool,
    root_pm: &'static str,
    out: &mut Vec<ScriptEntry>,
) {
    let dir = if rel == "." {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    if is_root {
        detect_cargo(&dir, out);
    }
    detect_node(
        &dir,
        rel,
        if is_root {
            root_pm
        } else {
            node_pm(&dir, Some(root_pm))
        },
        out,
    );
    detect_deno(&dir, rel, out);
    detect_python(&dir, rel, out);
    detect_go(&dir, rel, out);
    detect_make(&dir, rel, out);
    detect_just(&dir, rel, out);
    detect_taskfile(&dir, rel, out);
    detect_composer(&dir, rel, out);
}

fn manifest_path(rel: &str, name: &str) -> String {
    if rel == "." {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

fn entry(
    runner: &'static str,
    name: &str,
    command: String,
    rel: &str,
    source: String,
    raw: Option<String>,
) -> ScriptEntry {
    ScriptEntry {
        id: format!("{runner}:{name}"),
        runner,
        name: name.to_string(),
        command,
        dir: rel.to_string(),
        source,
        verb: None,
        raw,
    }
}

fn synthesized(runner: &'static str, name: &str, command: &str, rel: &str) -> ScriptEntry {
    entry(
        runner,
        name,
        command.to_string(),
        rel,
        "synthesized".into(),
        None,
    )
}

/// A script/target name that would re-tokenize under `bash -c` gets quoted
/// inside the composed command.
fn quoted_name(name: &str) -> String {
    shell_quote(name)
}

/// The package manager for a dir with `package.json`, from its lockfile —
/// falling back to the workspace root's pm for lockfile-less members
/// (hoisted-lockfile monorepos).
fn node_pm(dir: &Path, inherited: Option<&'static str>) -> &'static str {
    if dir.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if dir.join("yarn.lock").exists() {
        "yarn"
    } else if dir.join("bun.lock").exists() || dir.join("bun.lockb").exists() {
        "bun"
    } else if dir.join("package-lock.json").exists() {
        "npm"
    } else {
        inherited.unwrap_or("npm")
    }
}

fn detect_node(dir: &Path, rel: &str, pm: &'static str, out: &mut Vec<ScriptEntry>) {
    let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
        return;
    };
    let Ok(pkg) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let source = manifest_path(rel, "package.json");
    if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
        for (name, body) in scripts {
            out.push(entry(
                pm,
                name,
                format!("{pm} run {}", quoted_name(name)),
                rel,
                source.clone(),
                body.as_str().map(String::from),
            ));
        }
    }
    out.push(synthesized(pm, "install", &format!("{pm} install"), rel));
}

fn detect_deno(dir: &Path, rel: &str, out: &mut Vec<ScriptEntry>) {
    let (text, name) = match std::fs::read_to_string(dir.join("deno.json")) {
        Ok(t) => (t, "deno.json"),
        Err(_) => match std::fs::read_to_string(dir.join("deno.jsonc")) {
            Ok(t) => (strip_jsonc_comments(&t), "deno.jsonc"),
            Err(_) => return,
        },
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let source = manifest_path(rel, name);
    if let Some(tasks) = doc.get("tasks").and_then(|t| t.as_object()) {
        for (task, body) in tasks {
            out.push(entry(
                "deno",
                task,
                format!("deno task {}", quoted_name(task)),
                rel,
                source.clone(),
                body.as_str().map(String::from),
            ));
        }
    }
    out.push(synthesized("deno", "install", "deno install", rel));
}

fn detect_python(dir: &Path, rel: &str, out: &mut Vec<ScriptEntry>) {
    let Ok(text) = std::fs::read_to_string(dir.join("pyproject.toml")) else {
        return;
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
        return;
    };
    let tool = |name: &str| doc.get("tool").and_then(|t| t.get(name));
    let runner: &'static str = if dir.join("uv.lock").exists() || tool("uv").is_some() {
        "uv"
    } else if dir.join("poetry.lock").exists() || tool("poetry").is_some() {
        "poetry"
    } else {
        "uv"
    };
    let source = manifest_path(rel, "pyproject.toml");
    if let Some(scripts) = doc
        .get("project")
        .and_then(|p| p.get("scripts"))
        .and_then(|s| s.as_table())
    {
        for (name, target) in scripts {
            out.push(entry(
                runner,
                name,
                format!("{runner} run {}", quoted_name(name)),
                rel,
                source.clone(),
                target.as_str().map(String::from),
            ));
        }
    }
    let install = if runner == "poetry" {
        "poetry install"
    } else {
        "uv sync"
    };
    out.push(synthesized(runner, "install", install, rel));
    // Test/lint/format synthesize only when the tool is actually declared —
    // `uv run pytest` in a pytest-less project is a guess, not a script.
    if has_python_dep(&doc, "pytest") {
        out.push(synthesized(
            runner,
            "test",
            &format!("{runner} run pytest"),
            rel,
        ));
    }
    if has_python_dep(&doc, "ruff") {
        out.push(synthesized(
            runner,
            "lint",
            &format!("{runner} run ruff check"),
            rel,
        ));
        out.push(synthesized(
            runner,
            "format",
            &format!("{runner} run ruff format"),
            rel,
        ));
    }
}

/// Whether `pyproject.toml` declares `tool_name` anywhere dependencies live:
/// `[project] dependencies`, `[project.optional-dependencies]`,
/// `[dependency-groups]`, `[tool.poetry.dependencies]`, or
/// `[tool.poetry.group.*.dependencies]`.
fn has_python_dep(doc: &toml::Value, tool_name: &str) -> bool {
    let mut names: Vec<String> = Vec::new();
    let mut push_reqs = |value: Option<&toml::Value>| {
        if let Some(reqs) = value.and_then(|v| v.as_array()) {
            names.extend(reqs.iter().filter_map(|r| r.as_str()).map(req_name));
        }
    };
    let project = doc.get("project");
    push_reqs(project.and_then(|p| p.get("dependencies")));
    if let Some(extras) = project
        .and_then(|p| p.get("optional-dependencies"))
        .and_then(|o| o.as_table())
    {
        for group in extras.values() {
            if let Some(reqs) = group.as_array() {
                names.extend(reqs.iter().filter_map(|r| r.as_str()).map(req_name));
            }
        }
    }
    if let Some(groups) = doc.get("dependency-groups").and_then(|g| g.as_table()) {
        for group in groups.values() {
            if let Some(reqs) = group.as_array() {
                // Non-string items are `{include-group = …}` tables — skip.
                names.extend(reqs.iter().filter_map(|r| r.as_str()).map(req_name));
            }
        }
    }
    let poetry = doc.get("tool").and_then(|t| t.get("poetry"));
    if let Some(deps) = poetry
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        names.extend(deps.keys().map(|k| req_name(k)));
    }
    if let Some(groups) = poetry
        .and_then(|p| p.get("group"))
        .and_then(|g| g.as_table())
    {
        for group in groups.values() {
            if let Some(deps) = group.get("dependencies").and_then(|d| d.as_table()) {
                names.extend(deps.keys().map(|k| req_name(k)));
            }
        }
    }
    let want = req_name(tool_name);
    names.contains(&want)
}

/// PEP 508-ish requirement → normalized distribution name (`Pytest>=8` →
/// `pytest`, `ruff == 0.4` → `ruff`).
fn req_name(req: &str) -> String {
    req.trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect::<String>()
        .to_lowercase()
        .replace('_', "-")
}

fn detect_go(dir: &Path, rel: &str, out: &mut Vec<ScriptEntry>) {
    if !dir.join("go.mod").exists() {
        return;
    }
    out.push(synthesized("go", "install", "go mod download", rel));
    out.push(synthesized("go", "build", "go build ./...", rel));
    out.push(synthesized("go", "test", "go test ./...", rel));
    out.push(synthesized("go", "lint", "go vet ./...", rel));
    out.push(synthesized("go", "format", "go fmt ./...", rel));
}

fn detect_cargo(root: &Path, out: &mut Vec<ScriptEntry>) {
    let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) else {
        return;
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
        return;
    };
    // Aliases first: pushed before the synthesized set so an alias named
    // e.g. `install` wins the (dir, id) dedup.
    if let Ok(config) = std::fs::read_to_string(root.join(".cargo/config.toml"))
        && let Ok(config) = toml::from_str::<toml::Value>(&config)
        && let Some(aliases) = config.get("alias").and_then(|a| a.as_table())
    {
        for (alias, value) in aliases {
            let raw = match value {
                toml::Value::String(s) => Some(s.clone()),
                toml::Value::Array(parts) => Some(
                    parts
                        .iter()
                        .filter_map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => None,
            };
            out.push(entry(
                "cargo",
                alias,
                format!("cargo {}", quoted_name(alias)),
                ".",
                ".cargo/config.toml".into(),
                raw,
            ));
        }
    }
    out.push(synthesized("cargo", "install", "cargo fetch", "."));
    out.push(synthesized(
        "cargo",
        "build",
        "cargo build --workspace",
        ".",
    ));
    out.push(synthesized("cargo", "test", "cargo test --workspace", "."));
    out.push(synthesized(
        "cargo",
        "lint",
        "cargo clippy --workspace --all-targets",
        ".",
    ));
    out.push(synthesized("cargo", "format", "cargo fmt", "."));
    // `start` only when there is something to run: an explicit default-run
    // binary, or a root src/main.rs.
    let has_default_run = doc
        .get("package")
        .and_then(|p| p.get("default-run"))
        .is_some();
    if has_default_run || root.join("src/main.rs").exists() {
        out.push(synthesized("cargo", "start", "cargo run", "."));
    }
}

fn detect_make(dir: &Path, rel: &str, out: &mut Vec<ScriptEntry>) {
    let text = match std::fs::read_to_string(dir.join("Makefile")) {
        Ok(t) => t,
        Err(_) => match std::fs::read_to_string(dir.join("makefile")) {
            Ok(t) => t,
            Err(_) => return,
        },
    };
    let source = manifest_path(rel, "Makefile");
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for line in text.lines() {
        // Recipe lines are tab-indented; comments, directives, and
        // assignments (`:=`, `VAR = x`) are not targets.
        if line.starts_with([' ', '\t', '#']) || line.is_empty() {
            continue;
        }
        let Some(colon) = line.find(':') else {
            continue;
        };
        // `:=` / `::=` assignments, and `=` before the colon, are variables.
        if line[colon + 1..].starts_with('=') || line[..colon].contains('=') {
            continue;
        }
        for name in line[..colon].split_whitespace() {
            let valid = !name.is_empty()
                && !name.starts_with('.')
                && !name.contains('%')
                && !name.contains('$')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/'));
            if valid && seen.insert(name.to_string()) {
                out.push(entry(
                    "make",
                    name,
                    format!("make {}", quoted_name(name)),
                    rel,
                    source.clone(),
                    None,
                ));
            }
        }
    }
}

fn detect_just(dir: &Path, rel: &str, out: &mut Vec<ScriptEntry>) {
    let (text, name) = match std::fs::read_to_string(dir.join("justfile")) {
        Ok(t) => (t, "justfile"),
        Err(_) => match std::fs::read_to_string(dir.join(".justfile")) {
            Ok(t) => (t, ".justfile"),
            Err(_) => return,
        },
    };
    let source = manifest_path(rel, name);
    for line in text.lines() {
        // Recipe headers are unindented `name params…: deps` lines; skip
        // comments, attributes (`[private]`), directives, assignments
        // (`x := y`), and `_`-prefixed (hidden-by-convention) recipes.
        if line.starts_with([' ', '\t', '#', '[']) || line.is_empty() {
            continue;
        }
        let Some(first) = line.split_whitespace().next() else {
            continue;
        };
        if matches!(first, "export" | "set" | "import" | "mod" | "alias") {
            continue;
        }
        let recipe = first.strip_suffix(':').unwrap_or(first);
        let rest = line[first.len()..].trim_start();
        // `x := y` is an assignment; a header either ends its first token
        // with `:` or carries params before a later colon.
        if rest.starts_with(":=") || (first.len() == recipe.len() && !rest.contains(':')) {
            continue;
        }
        let valid = recipe
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && recipe
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
        if valid {
            out.push(entry(
                "just",
                recipe,
                format!("just {}", quoted_name(recipe)),
                rel,
                source.clone(),
                None,
            ));
        }
    }
}

fn detect_taskfile(dir: &Path, rel: &str, out: &mut Vec<ScriptEntry>) {
    let (text, name) = match std::fs::read_to_string(dir.join("Taskfile.yml")) {
        Ok(t) => (t, "Taskfile.yml"),
        Err(_) => match std::fs::read_to_string(dir.join("Taskfile.yaml")) {
            Ok(t) => (t, "Taskfile.yaml"),
            Err(_) => return,
        },
    };
    let source = manifest_path(rel, name);
    // Minimal YAML walk: task names are the keys one indent level under the
    // top-level `tasks:` key; deeper lines are task bodies.
    let mut in_tasks = false;
    let mut task_indent: Option<usize> = None;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        let indent = trimmed.len() - trimmed.trim_start().len();
        if indent == 0 {
            in_tasks = trimmed == "tasks:";
            task_indent = None;
            continue;
        }
        if !in_tasks {
            continue;
        }
        let level = *task_indent.get_or_insert(indent);
        if indent != level {
            continue;
        }
        let key = trimmed.trim_start();
        let Some(colon) = key.find(':') else {
            continue;
        };
        let task = &key[..colon];
        let valid = !task.is_empty()
            && !task.starts_with('-')
            && task
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'));
        if valid {
            out.push(entry(
                "task",
                task,
                format!("task {}", quoted_name(task)),
                rel,
                source.clone(),
                None,
            ));
        }
    }
}

fn detect_composer(dir: &Path, rel: &str, out: &mut Vec<ScriptEntry>) {
    let Ok(text) = std::fs::read_to_string(dir.join("composer.json")) else {
        return;
    };
    let Ok(pkg) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let source = manifest_path(rel, "composer.json");
    if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
        for (name, body) in scripts {
            let raw = match body {
                Value::String(s) => Some(s.clone()),
                Value::Array(parts) => Some(
                    parts
                        .iter()
                        .filter_map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
                _ => None,
            };
            out.push(entry(
                "composer",
                name,
                format!("composer run {}", quoted_name(name)),
                rel,
                source.clone(),
                raw,
            ));
        }
    }
    out.push(synthesized("composer", "install", "composer install", rel));
}

/// Strip `//` and `/* */` comments from JSONC, string-aware.
fn strip_jsonc_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for next in chars.by_ref() {
                    if prev == '*' && next == '/' {
                        break;
                    }
                    prev = next;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Workspace members
// ---------------------------------------------------------------------------

/// Member dirs declared by the root manifests: `package.json` `workspaces`,
/// `pnpm-workspace.yaml` `packages`, `[workspace] members` in `Cargo.toml`.
/// Sorted, deduplicated, capped — the overflow count is reported, never
/// silently dropped.
fn workspace_members(root: &Path) -> (Vec<String>, usize) {
    let mut patterns: Vec<String> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("package.json"))
        && let Ok(pkg) = serde_json::from_str::<Value>(&text)
    {
        let globs = pkg
            .get("workspaces")
            .map(|w| w.get("packages").unwrap_or(w))
            .and_then(|w| w.as_array());
        if let Some(globs) = globs {
            patterns.extend(globs.iter().filter_map(|g| g.as_str()).map(String::from));
        }
    }
    if let Ok(text) = std::fs::read_to_string(root.join("pnpm-workspace.yaml")) {
        patterns.extend(pnpm_workspace_globs(&text));
    }
    if let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml"))
        && let Ok(doc) = toml::from_str::<toml::Value>(&text)
        && let Some(members) = doc
            .get("workspace")
            .and_then(|w| w.get("members"))
            .and_then(|m| m.as_array())
    {
        patterns.extend(members.iter().filter_map(|m| m.as_str()).map(String::from));
    }

    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for pattern in &patterns {
        expand_member_pattern(root, pattern, &mut dirs);
    }
    dirs.remove(".");
    let truncated = dirs.len().saturating_sub(MAX_WORKSPACE_MEMBERS);
    (
        dirs.into_iter().take(MAX_WORKSPACE_MEMBERS).collect(),
        truncated,
    )
}

/// The `packages:` globs of a `pnpm-workspace.yaml` — a minimal line-based
/// parse (top-level key, then `- pattern` items); negations are skipped.
fn pnpm_workspace_globs(text: &str) -> Vec<String> {
    let mut globs = Vec::new();
    let mut in_packages = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        if !trimmed.starts_with([' ', '\t']) {
            in_packages = trimmed == "packages:";
            continue;
        }
        if !in_packages {
            continue;
        }
        if let Some(item) = trimmed.trim_start().strip_prefix("- ") {
            let item = item.trim().trim_matches('"').trim_matches('\'');
            if !item.is_empty() && !item.starts_with('!') {
                globs.push(item.to_string());
            }
        }
    }
    globs
}

/// Expand one member pattern: a literal dir, or a `base/*` / `base/**`
/// one-level glob. Anything fancier is skipped — deterministically, not
/// approximately.
fn expand_member_pattern(root: &Path, pattern: &str, dirs: &mut BTreeSet<String>) {
    let pattern = pattern.trim().trim_end_matches('/');
    if pattern.is_empty() {
        return;
    }
    let base = pattern
        .strip_suffix("/*")
        .or_else(|| pattern.strip_suffix("/**"));
    if let Some(base) = base {
        let Ok(read) = std::fs::read_dir(root.join(base)) else {
            return;
        };
        for child in read.filter_map(|e| e.ok()) {
            let name = child.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            if child.path().is_dir() {
                dirs.insert(format!("{base}/{name}"));
            }
        }
    } else if !pattern.contains('*') && root.join(pattern).is_dir() {
        dirs.insert(pattern.to_string());
    }
}

// ---------------------------------------------------------------------------
// Verb resolution
// ---------------------------------------------------------------------------

/// Bind the six canonical verbs over the sorted entries (root package only):
/// (1) an explicit script in the first-ranked ecosystem, (2) that
/// ecosystem's synthesized default, (3) explicit scripts of later-ranked
/// ecosystems. Synthesized defaults of later ecosystems never bind.
fn resolve_verbs(entries: &mut [ScriptEntry]) -> BTreeMap<&'static str, String> {
    let mut map = BTreeMap::new();
    let mut ranks: Vec<u8> = entries
        .iter()
        .filter(|e| e.dir == ".")
        .map(|e| runner_rank(e.runner))
        .collect();
    ranks.sort_unstable();
    ranks.dedup();
    let Some(&first) = ranks.first() else {
        return map;
    };
    for verb in VERBS {
        let winner = find_explicit(entries, first, verb)
            .or_else(|| find_synthesized(entries, first, verb))
            .or_else(|| {
                ranks
                    .iter()
                    .skip(1)
                    .find_map(|&rank| find_explicit(entries, rank, verb))
            });
        if let Some(i) = winner {
            entries[i].verb = Some(verb);
            map.insert(verb, entries[i].id.clone());
        }
    }
    map
}

fn find_explicit(entries: &[ScriptEntry], rank: u8, verb: &str) -> Option<usize> {
    verb_aliases(verb).iter().find_map(|alias| {
        entries.iter().position(|e| {
            e.dir == "."
                && !e.synthesized()
                && runner_rank(e.runner) == rank
                && e.name == *alias
                && verb_eligible(&e.name)
        })
    })
}

fn find_synthesized(entries: &[ScriptEntry], rank: u8, verb: &str) -> Option<usize> {
    entries.iter().position(|e| {
        e.dir == "." && e.synthesized() && runner_rank(e.runner) == rank && e.name == verb
    })
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

pub struct ListScripts;

#[async_trait]
impl Tool for ListScripts {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_scripts".into(),
            description: "List the project's indexed package-manager scripts and their \
                          canonical verb bindings (install/build/start/test/lint/format). \
                          Static manifest detection — nothing is executed."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "dir": { "type": "string", "description": "Narrow to one workspace package dir" }
                }
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let index = ScriptIndex::detect(root).await;
        let dir = input.get("dir").and_then(|v| v.as_str());
        ToolOutput::Ok {
            content: index.render_list(dir),
        }
    }
}

pub struct RunScript;

#[async_trait]
impl Tool for RunScript {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "run_script".into(),
            description: "Run an indexed project script by canonical verb \
                          (install|build|start|test|lint|format) or qualified id (e.g. \
                          pnpm:build, make:lint — see list_scripts). Executes only indexed \
                          entries; for arbitrary shell, use bash."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["script"],
                "properties": {
                    "script": { "type": "string", "description": "Canonical verb or qualified id like pnpm:build" },
                    "dir": { "type": "string", "description": "Package dir when the id exists in several packages" },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Appended runner-natively (after `--` for npm-family)" },
                    "timeout_secs": { "type": "integer" }
                }
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let Some(script) = input.get("script").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "`script` is required — a canonical verb or qualified id".into(),
            };
        };
        let dir = input.get("dir").and_then(|v| v.as_str());
        let args = string_args(input);
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        run_by_name(root, script, dir, &args, timeout_secs).await
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

    #[test]
    fn node_scripts_and_lockfile_pm_bind_verbs() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"build": "next build", "dev": "next dev", "test": "vitest",
                "test:watch": "vitest --watch", "deploy": "vercel deploy"}}"#,
        );
        write(dir.path(), "pnpm-lock.yaml", "");
        let index = ScriptIndex::detect_blocking(dir.path());

        assert_eq!(index.verbs.get("build").unwrap(), "pnpm:build");
        assert_eq!(index.verbs.get("test").unwrap(), "pnpm:test");
        assert_eq!(index.verbs.get("start").unwrap(), "pnpm:dev");
        assert_eq!(index.verbs.get("install").unwrap(), "pnpm:install");
        let build = index.verb_entry("build").unwrap();
        assert_eq!(build.command, "pnpm run build");
        assert_eq!(build.raw.as_deref(), Some("next build"));
        assert_eq!(index.verb_entry("install").unwrap().command, "pnpm install",);
        // watch/deploy names are listed but never verb-bound.
        for entry in &index.scripts {
            if entry.name == "test:watch" || entry.name == "deploy" {
                assert!(entry.verb.is_none(), "{} must not bind a verb", entry.id);
            }
        }
    }

    #[test]
    fn cargo_root_synthesizes_verbs_and_reads_aliases() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        );
        write(
            dir.path(),
            ".cargo/config.toml",
            "[alias]\nxt = \"test --workspace\"\n",
        );
        let index = ScriptIndex::detect_blocking(dir.path());

        assert_eq!(index.verbs.get("install").unwrap(), "cargo:install");
        assert_eq!(index.verb_entry("install").unwrap().command, "cargo fetch");
        assert_eq!(
            index.verb_entry("build").unwrap().command,
            "cargo build --workspace"
        );
        // No default-run bin and no src/main.rs → no start verb.
        assert!(!index.verbs.contains_key("start"));
        let alias = index.scripts.iter().find(|e| e.id == "cargo:xt").unwrap();
        assert_eq!(alias.command, "cargo xt");
        assert_eq!(alias.raw.as_deref(), Some("test --workspace"));
        assert_eq!(alias.source, ".cargo/config.toml");
    }

    #[test]
    fn make_targets_parse_and_skip_variables_and_patterns() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Makefile",
            ".PHONY: build test\nVAR := x\nOTHER = y\n\nbuild:\n\tcargo build\n\
             test: build\n\techo test\n%.o: %.c\n\tcc\n.hidden:\n\ttrue\n",
        );
        let index = ScriptIndex::detect_blocking(dir.path());
        let ids: Vec<&str> = index.scripts.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["make:build", "make:test"]);
        // make is the only ecosystem → its explicit targets bind the verbs.
        assert_eq!(index.verbs.get("build").unwrap(), "make:build");
        assert_eq!(index.verbs.get("test").unwrap(), "make:test");
        assert!(!index.verbs.contains_key("install"));
    }

    #[test]
    fn justfile_recipes_skip_hidden_and_assignments() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "justfile",
            "version := \"1\"\n\n# comment\nbuild target=\"debug\":\n    cargo build\n\
             _helper:\n    true\nfmt:\n    cargo fmt\n",
        );
        let index = ScriptIndex::detect_blocking(dir.path());
        let ids: Vec<&str> = index.scripts.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["just:build", "just:fmt"]);
        assert_eq!(index.verbs.get("format").unwrap(), "just:fmt");
    }

    #[test]
    fn pyproject_uv_synthesizes_only_declared_tools() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            "[project]\nname = \"x\"\ndependencies = [\"requests>=2\"]\n\n\
             [dependency-groups]\ndev = [\"pytest>=8\", \"ruff\"]\n\n\
             [project.scripts]\nserve = \"x.app:main\"\n",
        );
        write(dir.path(), "uv.lock", "");
        let index = ScriptIndex::detect_blocking(dir.path());

        assert_eq!(index.verb_entry("install").unwrap().command, "uv sync");
        assert_eq!(index.verb_entry("test").unwrap().command, "uv run pytest");
        assert_eq!(
            index.verb_entry("lint").unwrap().command,
            "uv run ruff check"
        );
        assert_eq!(
            index.verb_entry("format").unwrap().command,
            "uv run ruff format"
        );
        // The [project.scripts] entry point is explicit and binds `start`
        // via its `serve` alias.
        assert_eq!(index.verbs.get("start").unwrap(), "uv:serve");
        assert_eq!(index.verb_entry("start").unwrap().command, "uv run serve");

        // Without pytest/ruff declared, none of test/lint/format synthesize.
        let bare = tempfile::tempdir().unwrap();
        write(bare.path(), "pyproject.toml", "[project]\nname = \"y\"\n");
        let bare_index = ScriptIndex::detect_blocking(bare.path());
        assert!(!bare_index.verbs.contains_key("test"));
        assert!(!bare_index.verbs.contains_key("lint"));
    }

    #[test]
    fn poetry_marker_selects_poetry_runner() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            "[tool.poetry]\nname = \"x\"\n\n[tool.poetry.dependencies]\npytest = \"^8\"\n",
        );
        let index = ScriptIndex::detect_blocking(dir.path());
        assert_eq!(
            index.verb_entry("install").unwrap().command,
            "poetry install"
        );
        assert_eq!(
            index.verb_entry("test").unwrap().command,
            "poetry run pytest"
        );
    }

    #[test]
    fn multi_ecosystem_rank_one_wins_then_later_explicit_fills_gaps() {
        // Node (rank 2) is first: its scripts win; the Makefile (rank 6)
        // fills verbs node doesn't define.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"test": "vitest"}}"#,
        );
        write(dir.path(), "Makefile", "build:\n\ttrue\ntest:\n\ttrue\n");
        let index = ScriptIndex::detect_blocking(dir.path());
        assert_eq!(index.verbs.get("test").unwrap(), "npm:test");
        assert_eq!(index.verbs.get("build").unwrap(), "make:build");
        // Synthesized install of the first ecosystem still binds.
        assert_eq!(index.verbs.get("install").unwrap(), "npm:install");
    }

    #[test]
    fn workspace_members_are_indexed_with_their_dir() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"workspaces": ["packages/*"], "scripts": {"build": "true"}}"#,
        );
        write(dir.path(), "pnpm-lock.yaml", "");
        write(
            dir.path(),
            "packages/app/package.json",
            r#"{"scripts": {"dev": "vite"}}"#,
        );
        let index = ScriptIndex::detect_blocking(dir.path());
        let member = index
            .scripts
            .iter()
            .find(|e| e.dir == "packages/app" && e.name == "dev")
            .expect("member script indexed");
        assert_eq!(member.id, "pnpm:dev");
        assert_eq!(member.source, "packages/app/package.json");
        // Verbs bind at the root only.
        assert_eq!(index.verbs.get("start"), None);
        assert_eq!(index.verbs.get("build").unwrap(), "pnpm:build");
    }

    #[test]
    fn pnpm_workspace_yaml_and_taskfile_and_composer_and_deno_parse() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pnpm-workspace.yaml",
            "# comment\npackages:\n  - docs\n  - '!excluded'\nother:\n  - not-a-package\n",
        );
        write(dir.path(), "package.json", r#"{"scripts": {}}"#);
        write(dir.path(), "pnpm-lock.yaml", "");
        write(
            dir.path(),
            "docs/package.json",
            r#"{"scripts": {"dev": "next dev"}}"#,
        );
        write(
            dir.path(),
            "Taskfile.yml",
            "version: '3'\ntasks:\n  greet:\n    cmds:\n      - echo hi\n  lint:\n    cmds:\n      - true\n",
        );
        write(
            dir.path(),
            "composer.json",
            r#"{"scripts": {"post-install-cmd": ["A\\B::hook"], "check": "phpstan"}}"#,
        );
        write(
            dir.path(),
            "deno.jsonc",
            "{\n  // a comment\n  \"tasks\": { \"bench\": \"deno bench\" }\n}\n",
        );
        let index = ScriptIndex::detect_blocking(dir.path());
        let has = |id: &str| index.scripts.iter().any(|e| e.id == id);
        assert!(has("task:greet"), "{:?}", index.scripts);
        assert!(has("task:lint"));
        assert!(has("composer:check"));
        assert!(has("deno:bench"));
        assert!(
            index
                .scripts
                .iter()
                .any(|e| e.dir == "docs" && e.id == "pnpm:dev"),
            "pnpm-workspace member indexed"
        );
        assert!(
            !index.scripts.iter().any(|e| e.dir == "not-a-package"),
            "keys outside packages: must not enumerate"
        );
    }

    #[test]
    fn go_synthesizes_the_full_verb_set() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/x\n");
        let index = ScriptIndex::detect_blocking(dir.path());
        assert_eq!(
            index.verb_entry("install").unwrap().command,
            "go mod download"
        );
        assert_eq!(index.verb_entry("test").unwrap().command, "go test ./...");
        assert_eq!(index.verb_entry("lint").unwrap().command, "go vet ./...");
    }

    #[test]
    fn detection_is_byte_stable() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"b": "1", "a": "2", "c": "3"}}"#,
        );
        write(dir.path(), "Makefile", "x:\n\ttrue\n");
        let a = ScriptIndex::detect_blocking(dir.path());
        let b = ScriptIndex::detect_blocking(dir.path());
        assert_eq!(a.render_prompt_section(), b.render_prompt_section());
        assert_eq!(a.render_list(None), b.render_list(None));
        assert_eq!(a.to_json().to_string(), b.to_json().to_string());
    }

    #[test]
    fn prompt_section_lists_verbs_and_counts_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"build": "next build", "docs:gen": "typedoc"}}"#,
        );
        let index = ScriptIndex::detect_blocking(dir.path());
        let section = index.render_prompt_section().unwrap();
        assert!(section.starts_with("## Project scripts"));
        assert!(section.contains("build → npm run build"), "{section}");
        assert!(section.contains("install → npm install"));
        assert!(section.contains("more scripts"), "{section}");
        assert!(section.contains("npm:docs:gen"), "{section}");
        assert!(section.chars().count() <= PROMPT_SECTION_CHAR_CAP);

        let empty = tempfile::tempdir().unwrap();
        assert!(
            ScriptIndex::detect_blocking(empty.path())
                .render_prompt_section()
                .is_none()
        );
    }

    #[test]
    fn resolve_accepts_verb_id_and_unique_name_and_names_near_misses() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"build": "true", "typecheck": "tsc"}}"#,
        );
        let index = ScriptIndex::detect_blocking(dir.path());
        assert_eq!(index.resolve("build", None).unwrap().id, "npm:build");
        assert_eq!(index.resolve("npm:build", None).unwrap().id, "npm:build");
        assert_eq!(
            index.resolve("typecheck", None).unwrap().id,
            "npm:typecheck"
        );
        let err = index.resolve("typechek", None).unwrap_err();
        assert!(err.contains("unknown script"), "{err}");
        let err = index.resolve("lint", None).unwrap_err();
        assert!(err.contains("no `lint` script detected"), "{err}");
    }

    #[test]
    fn compose_command_quotes_args_and_uses_npm_family_separator() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"test": "vitest"}}"#,
        );
        write(dir.path(), "Makefile", "lint:\n\ttrue\n");
        let index = ScriptIndex::detect_blocking(dir.path());

        let test = index.resolve("test", None).unwrap();
        assert_eq!(
            compose_command(test, &["--run".into(), "my file".into()]),
            "npm run test -- --run 'my file'"
        );
        let lint = index.resolve("make:lint", None).unwrap();
        assert_eq!(compose_command(lint, &["V=1".into()]), "make lint V=1");
        // Synthesized npm install takes plain args (no `--`).
        let install = index.resolve("install", None).unwrap();
        assert_eq!(
            compose_command(install, &["--frozen-lockfile".into()]),
            "npm install --frozen-lockfile"
        );
    }

    #[test]
    fn strip_jsonc_preserves_strings_and_removes_comments() {
        let src = "{ // c\n \"a\": \"http://x/*y*/\", /* b\n */ \"t\": 1 }";
        let stripped = strip_jsonc_comments(src);
        let doc: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(doc.get("a").unwrap(), "http://x/*y*/");
        assert_eq!(doc.get("t").unwrap(), 1);
    }

    #[tokio::test]
    async fn run_script_tool_executes_indexed_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Makefile", "greet:\n\t@echo hello-from-make\n");

        let out = RunScript
            .execute(&serde_json::json!({"script": "make:greet"}), dir.path())
            .await;
        match &out {
            ToolOutput::Ok { content } => {
                assert!(content.contains("PASSED"), "{content}");
                assert!(content.contains("hello-from-make"), "{content}");
            }
            other => panic!("{other:?}"),
        }

        let out = RunScript
            .execute(&serde_json::json!({"script": "rm -rf /"}), dir.path())
            .await;
        assert!(out.is_error(), "non-indexed input must be refused: {out:?}");

        let out = ListScripts
            .execute(&serde_json::json!({}), dir.path())
            .await;
        match &out {
            ToolOutput::Ok { content } => assert!(content.contains("make:greet"), "{content}"),
            other => panic!("{other:?}"),
        }
    }
}
