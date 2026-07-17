//! `custom` — developer-defined **script tools**, discovered from the
//! filesystem with zero registration ceremony.
//!
//! A developer gives the agent a new tool by dropping a TOML manifest next to
//! a script. At CLI startup every manifest under `.stella/tools/` (workspace)
//! and `~/.config/stella/tools/` (user-global) is discovered automatically and
//! advertised to the model alongside the native tools ([`crate::registry`]).
//! No flags, no registry edits — a bash script plus a ten-line manifest is a
//! working agent tool. Rich or stateful integrations use an MCP server
//! (`stella-mcp`) instead; this layer is the low-ceremony floor beneath it.
//!
//! # Manifest format (`<name>.toml`, one tool per file)
//!
//! ```toml
//! name = "lint_fix"                    # the name the model sees: ^[a-z][a-z0-9_]{1,63}$
//! description = "Run the lint auto-fixer on a path and report what changed"
//! command = ["./scripts/lint-fix.sh"] # argv array, spawned DIRECTLY (no shell)
//! timeout_ms = 60000                   # optional; default 30s, hard cap 10min
//!
//! [env]                                # optional extra env vars for the child
//! LINT_PROFILE = "strict"
//!
//! [input_schema]                       # JSON Schema written as TOML, converted verbatim
//! type = "object"
//! [input_schema.properties.path]
//! type = "string"
//! description = "Directory or file to fix"
//! [input_schema.properties.dry_run]
//! type = "boolean"
//! ```
//!
//! # Execution contract — this IS the developer API
//!
//! When the model calls a custom tool, the CLI:
//!
//! - Spawns `command` **directly** via [`tokio::process::Command`] from the
//!   argv array — never through a shell. Tool *input* therefore has no
//!   injection surface. The manifest itself is workspace-trusted, at the same
//!   trust level as a `package.json` script or a Makefile target: whoever can
//!   write `.stella/tools/` can already run code in the repo.
//! - Sets the child's working directory to the **workspace root**, so a
//!   relative `command[0]` like `./scripts/lint-fix.sh` resolves against it.
//! - Delivers the model's input JSON to the child **two ways**, so trivial
//!   scripts need no JSON parser:
//!   1. The whole input object is written to the child's **stdin** as one JSON
//!      document, then stdin is closed (the child sees EOF).
//!   2. Each top-level **scalar** property (string / number / bool) is exported
//!      as `STELLA_INPUT_<UPPER_SNAKE_KEY>` — e.g. `path` → `STELLA_INPUT_PATH`.
//!      Nested objects and arrays are delivered on stdin only.
//! - Applies the manifest's optional `[env]` table on top of the inherited
//!   environment.
//!
//! Outcome mapping:
//!
//! - **Exit 0** → [`ToolOutput::Ok`] with captured stdout (middle-out truncated
//!   past a byte cap, tail budget ≥ head budget per lesson L-S3).
//! - **Non-zero exit** → [`ToolOutput::Error`] carrying the exit code and the
//!   stderr tail (also truncated).
//! - **Timeout** → the process group is killed (SIGKILL to `-pid`, mirroring
//!   [`crate::bash`]) and a named error is returned.
//! - **Spawn failure** (e.g. missing script) → a named error naming the path
//!   that was tried, so the developer can fix the manifest.
//!
//! # Discovery precedence
//!
//! Workspace (`<root>/.stella/tools/`) is scanned before user-global
//! (`$HOME/.config/stella/tools/`); on a name collision the workspace tool
//! wins and the global one is reported as a [`ToolDiagnostic`]. A malformed
//! manifest never aborts discovery — it becomes a typed per-file diagnostic so
//! `stella tools` can show developers exactly which file is broken and why. A
//! manifest whose `name` collides with a reserved built-in (or `ask_user`) is
//! likewise skipped with a diagnostic.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use stella_core::ports::ToolExecutor;
use stella_protocol::tool::{ToolOutput, ToolSchema};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Tool names that a custom manifest may not claim. Mirrors every built-in
/// registered in [`crate::registry::ToolRegistry`] (including the conditional
/// issue tools) plus `ask_user`/`search_skills`/`install_skill` from the CLI's
/// `interactive` layer — a custom tool must never shadow one, or the decorator
/// chain (native ← custom ← mcp ← ask_user) would route the wrong executor and
/// a manifest named e.g. `verify_done` or `delete_file` could silently replace
/// a flagship built-in. Keep this in sync if either set changes.
pub const RESERVED_NAMES: &[&str] = &[
    // File CRUD
    "read_file",
    "write_file",
    "edit_file",
    "delete_file",
    // Exec & search
    "bash",
    "grep",
    "glob",
    // Codebase maps & memory
    "explorations",
    "save_exploration",
    "save_memory",
    "cite_memory",
    // The definition of done + build/test
    "verify_done",
    "build_project",
    "run_tests",
    // CI & evidence
    "ci_status",
    "screenshot",
    // Media generation: generate_svg is client-side and always registered.
    "generate_svg",
    // Conditionally registered tools: graph_query only when a code-graph index
    // exists, generate_image (and the video pair, when the key family has a
    // video adapter) only when a media key is configured. The registry-driven
    // drift test can't see these (a bare registry never advertises them), so
    // they must be listed here by hand.
    "graph_query",
    "generate_image",
    "generate_video",
    "poll_video",
    // Issue tracking (registered only when a backend is configured)
    "create_issue",
    "update_issue",
    "close_issue",
    "search_issues",
    "start_work_on_issue",
    // CLI interactive layer
    "ask_user",
    "search_skills",
    "install_skill",
];

/// Timeout applied when a manifest omits `timeout_ms`. Public so
/// [`crate::validate`] can explain the defaulting it mirrors.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling on `timeout_ms`; a manifest cannot ask for more. Public so
/// [`crate::validate`] can warn about the clamp it mirrors.
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// Byte cap on captured stdout/stderr before middle-out truncation kicks in.
/// Matches [`crate::bash`] so custom and native exec behave the same.
const MAX_OUTPUT_BYTES: usize = 100_000;

/// A parsed, validated custom tool ready to advertise and execute.
#[derive(Debug, Clone)]
pub struct CustomTool {
    /// Tool name the model sees; validated against `^[a-z][a-z0-9_]{1,63}$`
    /// and guaranteed not to be a [`RESERVED_NAMES`] entry.
    pub name: String,
    /// Human description advertised to the model.
    pub description: String,
    /// argv to spawn directly (no shell). `command[0]` is the program.
    pub command: Vec<String>,
    /// Resolved timeout (default applied, clamped to [`MAX_TIMEOUT_MS`]).
    pub timeout_ms: u64,
    /// JSON Schema for the tool's input, converted verbatim from TOML.
    pub input_schema: Value,
    /// Extra environment variables applied to the child process.
    pub env: HashMap<String, String>,
    /// Manifest file this tool was loaded from (for diagnostics / listing).
    pub source: PathBuf,
}

impl CustomTool {
    /// The schema advertised to the model — the same shape a built-in emits.
    pub fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            read_only: false,
        }
    }
}

/// A manifest that could not be loaded, kept so the CLI can surface it to the
/// developer instead of silently dropping the tool.
#[derive(Debug, Clone)]
pub struct ToolDiagnostic {
    /// The manifest file (or directory) the problem is about.
    pub path: PathBuf,
    /// Human-readable reason, safe to print in `stella tools`.
    pub reason: String,
}

/// The result of scanning the tool directories: everything that loaded, plus a
/// typed diagnostic for everything that did not.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryReport {
    pub tools: Vec<CustomTool>,
    pub diagnostics: Vec<ToolDiagnostic>,
}

/// Wire shape of a manifest file. Kept private — callers see [`CustomTool`],
/// which only exists once every field has been validated.
#[derive(Deserialize)]
struct RawManifest {
    name: String,
    description: String,
    command: Vec<String>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    env: HashMap<String, String>,
    // The `toml` deserializer populates this straight into a JSON value, so a
    // JSON-Schema-shaped TOML table converts verbatim (integers, floats,
    // bools, strings, arrays and nested tables all round-trip).
    #[serde(default)]
    input_schema: Option<Value>,
}

/// `true` iff `name` matches `^[a-z][a-z0-9_]{1,63}$` (a lowercase letter then
/// 1–63 of `[a-z0-9_]`). Hand-rolled to avoid pulling in `regex`.
fn is_valid_tool_name(name: &str) -> bool {
    let len = name.len();
    if !(2..=64).contains(&len) {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Parse and validate one manifest's text. `source` is only used to stamp the
/// resulting [`CustomTool`]; it is not read. Returns a human-readable error on
/// any validation failure — the caller turns it into a [`ToolDiagnostic`].
pub fn parse_manifest(text: &str, source: &Path) -> Result<CustomTool, String> {
    let raw: RawManifest = toml::from_str(text).map_err(|e| format!("invalid TOML: {e}"))?;

    if !is_valid_tool_name(&raw.name) {
        return Err(format!(
            "invalid tool name `{}` — must match ^[a-z][a-z0-9_]{{1,63}}$",
            raw.name
        ));
    }
    if RESERVED_NAMES.contains(&raw.name.as_str()) {
        return Err(format!(
            "tool name `{}` is reserved by a built-in and cannot be redefined",
            raw.name
        ));
    }
    if raw.description.trim().is_empty() {
        return Err("`description` is required and must be non-empty".to_string());
    }
    if raw.command.is_empty() || raw.command[0].trim().is_empty() {
        return Err("`command` must be a non-empty argv array".to_string());
    }

    let input_schema = match raw.input_schema {
        Some(v) if v.is_object() => v,
        Some(_) => {
            return Err("`input_schema` must be a table (JSON Schema object)".to_string());
        }
        None => serde_json::json!({ "type": "object", "properties": {} }),
    };

    let timeout_ms = match raw.timeout_ms {
        None | Some(0) => DEFAULT_TIMEOUT_MS,
        Some(t) => t.min(MAX_TIMEOUT_MS),
    };

    Ok(CustomTool {
        name: raw.name,
        description: raw.description,
        command: raw.command,
        timeout_ms,
        input_schema,
        env: raw.env,
        source: source.to_path_buf(),
    })
}

/// Discover custom tools for `workspace_root`, reading the user-global
/// location from `$HOME`. Thin env-reading wrapper over [`discover_in`] — tests
/// inject a home directory directly rather than mutating the process env.
pub fn discover(workspace_root: &Path) -> DiscoveryReport {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    discover_in(workspace_root, home.as_deref())
}

/// Discover custom tools from the workspace directory then the (optional)
/// user-global home. Workspace wins on name collisions; a `None` home skips the
/// global scan entirely. Never fails: unreadable or malformed manifests become
/// diagnostics, not errors.
pub fn discover_in(workspace_root: &Path, home: Option<&Path>) -> DiscoveryReport {
    let mut report = DiscoveryReport::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Workspace first so it wins collisions, then user-global.
    let mut dirs: Vec<PathBuf> = vec![workspace_root.join(".stella").join("tools")];
    if let Some(home) = home {
        dirs.push(home.join(".config").join("stella").join("tools"));
    }

    for dir in dirs {
        load_dir(&dir, &mut seen, &mut report);
    }
    report
}

/// Scan one directory for `*.toml` manifests, appending tools and diagnostics
/// to `report`. Absent directories are silently ignored (having no tools dir is
/// normal); files are processed in sorted order for deterministic output.
fn load_dir(
    dir: &Path,
    seen: &mut std::collections::HashSet<String>,
    report: &mut DiscoveryReport,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return, // no such directory → nothing to load, not an error
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    paths.sort();

    for path in paths {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                report.diagnostics.push(ToolDiagnostic {
                    path: path.clone(),
                    reason: format!("could not read manifest: {e}"),
                });
                continue;
            }
        };
        match parse_manifest(&text, &path) {
            Ok(tool) => {
                if seen.contains(&tool.name) {
                    report.diagnostics.push(ToolDiagnostic {
                        path: path.clone(),
                        reason: format!(
                            "tool `{}` already defined by an earlier (workspace) manifest — this \
                             one is ignored",
                            tool.name
                        ),
                    });
                    continue;
                }
                seen.insert(tool.name.clone());
                report.tools.push(tool);
            }
            Err(reason) => report.diagnostics.push(ToolDiagnostic { path, reason }),
        }
    }
}

/// Map a JSON input key to its `STELLA_INPUT_*` env var name: uppercased, with
/// any non-`[A-Z0-9_]` byte replaced by `_`.
fn env_var_name(key: &str) -> String {
    let mut out = String::with_capacity("STELLA_INPUT_".len() + key.len());
    out.push_str("STELLA_INPUT_");
    for c in key.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_uppercase());
        } else {
            out.push('_');
        }
    }
    out
}

/// Render a scalar JSON value as the string form exported to `STELLA_INPUT_*`.
/// Returns `None` for arrays, objects and null (delivered on stdin only).
fn scalar_env_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Middle-out truncation on a char boundary: keeps a head and a (larger) tail
/// with an explicit elision marker. Per lesson L-S3 the tail budget is ≥ the
/// head budget, because a failing command's signal is usually in its tail.
fn truncate_middle_out(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let head_budget = max_bytes * 2 / 5; // 40% head …
    let tail_budget = max_bytes - head_budget; // … 60% tail (≥ head, L-S3)
    let head_end = floor_boundary(s, head_budget);
    let tail_start = ceil_boundary(s, s.len().saturating_sub(tail_budget));
    let elided = tail_start.saturating_sub(head_end);
    format!(
        "{}\n... [truncated {elided} bytes] ...\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}

/// Largest char boundary `<= i`.
fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary `>= i`.
fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Run one custom tool against the model's `input`, from `workspace_root`.
/// Never returns `Err` — every failure mode is a named [`ToolOutput::Error`],
/// because tool failures are model-visible data, not engine faults.
async fn run_custom(tool: &CustomTool, input: &Value, workspace_root: &Path) -> ToolOutput {
    let mut cmd = Command::new(&tool.command[0]);
    cmd.args(&tool.command[1..]);
    cmd.current_dir(workspace_root);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // Manifest env (workspace-trusted) first …
    for (k, v) in &tool.env {
        cmd.env(k, v);
    }
    // … then each scalar input property as STELLA_INPUT_*. Keys come from the
    // model but are namespaced under STELLA_INPUT_, so they cannot clobber PATH
    // or any inherited variable.
    if let Some(obj) = input.as_object() {
        for (key, value) in obj {
            if let Some(v) = scalar_env_value(value) {
                cmd.env(env_var_name(key), v);
            }
        }
    }

    // New process group so a timeout kills the whole child tree (mirrors
    // `crate::bash`).
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ToolOutput::Error {
                message: format!(
                    "custom tool `{}` failed to spawn `{}` (cwd {}): {e}",
                    tool.name,
                    tool.command[0],
                    workspace_root.display()
                ),
            };
        }
    };

    #[cfg(unix)]
    let pid = child.id().unwrap_or(0) as i32;

    // Deliver the input as one JSON document on stdin, concurrently with
    // draining stdout/stderr, so a chatty child cannot deadlock the write.
    if let Some(mut stdin) = child.stdin.take() {
        let payload = serde_json::to_vec(input).unwrap_or_default();
        tokio::spawn(async move {
            let _ = stdin.write_all(&payload).await;
            let _ = stdin.shutdown().await;
            // stdin dropped here → child sees EOF.
        });
    }

    let timeout = Duration::from_millis(tool.timeout_ms);
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return ToolOutput::Error {
                message: format!("custom tool `{}` failed: {e}", tool.name),
            };
        }
        Err(_) => {
            #[cfg(unix)]
            unsafe {
                // Guard on a real pid: kill(-0, …) would SIGKILL Stella's OWN
                // process group.
                if pid > 0 {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            return ToolOutput::Error {
                message: format!(
                    "custom tool `{}` timed out after {}ms",
                    tool.name, tool.timeout_ms
                ),
            };
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        ToolOutput::Ok {
            content: truncate_middle_out(&stdout, MAX_OUTPUT_BYTES),
        }
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        let tail = truncate_middle_out(&stderr, MAX_OUTPUT_BYTES);
        ToolOutput::Error {
            message: format!(
                "custom tool `{}` exited with code {code}\n[stderr]\n{tail}",
                tool.name
            ),
        }
    }
}

/// A [`ToolExecutor`] that adds discovered custom tools on top of an inner
/// executor (the native [`crate::registry::ToolRegistry`], or the MCP-merged
/// set once that lands). Composes exactly like the CLI's `InteractiveToolSet`:
/// `schemas()` is the inner's plus the customs', and `execute()` routes an
/// exact custom-name match, otherwise falls through to the inner executor.
pub struct CustomToolSet<'a> {
    inner: &'a dyn ToolExecutor,
    tools: Vec<CustomTool>,
    workspace_root: PathBuf,
}

impl<'a> CustomToolSet<'a> {
    pub fn new(
        inner: &'a dyn ToolExecutor,
        tools: Vec<CustomTool>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            inner,
            tools,
            workspace_root,
        }
    }
}

#[async_trait]
impl ToolExecutor for CustomToolSet<'_> {
    fn schemas(&self) -> Vec<ToolSchema> {
        let mut schemas = self.inner.schemas();
        schemas.extend(self.tools.iter().map(CustomTool::schema));
        schemas
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        if let Some(tool) = self.tools.iter().find(|t| t.name == name) {
            return run_custom(tool, input, &self.workspace_root).await;
        }
        self.inner.execute(name, input).await
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    // ---- manifest parsing -------------------------------------------------

    const HAPPY: &str = r#"
name = "lint_fix"
description = "Run the lint auto-fixer on a path"
command = ["./scripts/lint-fix.sh", "--quiet"]
timeout_ms = 60000

[env]
LINT_PROFILE = "strict"

[input_schema]
type = "object"
[input_schema.properties.path]
type = "string"
description = "Directory or file to fix"
[input_schema.properties.dry_run]
type = "boolean"
"#;

    #[test]
    fn parses_a_complete_manifest() {
        let tool = parse_manifest(HAPPY, Path::new("/x/lint_fix.toml")).expect("valid manifest");
        assert_eq!(tool.name, "lint_fix");
        assert_eq!(tool.command, vec!["./scripts/lint-fix.sh", "--quiet"]);
        assert_eq!(tool.timeout_ms, 60000);
        assert_eq!(
            tool.env.get("LINT_PROFILE").map(String::as_str),
            Some("strict")
        );
        assert_eq!(tool.source, Path::new("/x/lint_fix.toml"));
    }

    #[test]
    fn input_schema_toml_converts_to_json_faithfully_including_nested() {
        let tool = parse_manifest(HAPPY, Path::new("x.toml")).unwrap();
        let schema = &tool.input_schema;
        assert_eq!(schema["type"], "object");
        // Nested table → nested JSON object, preserved verbatim.
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(
            schema["properties"]["path"]["description"],
            "Directory or file to fix"
        );
        assert_eq!(schema["properties"]["dry_run"]["type"], "boolean");
    }

    #[test]
    fn numeric_and_bool_schema_values_round_trip() {
        let src = r#"
name = "tt"
description = "d"
command = ["./x.sh"]
[input_schema]
type = "object"
additionalProperties = false
[input_schema.properties.count]
type = "integer"
minimum = 1
maximum = 10
"#;
        let tool = parse_manifest(src, Path::new("t.toml")).unwrap();
        assert_eq!(tool.input_schema["additionalProperties"], false);
        assert_eq!(tool.input_schema["properties"]["count"]["minimum"], 1);
        assert_eq!(tool.input_schema["properties"]["count"]["maximum"], 10);
    }

    #[test]
    fn missing_description_is_rejected() {
        let src = r#"name = "t"
command = ["./x.sh"]"#;
        let err = parse_manifest(src, Path::new("t.toml")).unwrap_err();
        // serde reports the missing field; message mentions `description`.
        assert!(err.contains("description"), "got: {err}");
    }

    #[test]
    fn empty_description_is_rejected() {
        let src = r#"name = "tt"
description = "   "
command = ["./x.sh"]"#;
        let err = parse_manifest(src, Path::new("t.toml")).unwrap_err();
        assert!(err.contains("description"), "got: {err}");
    }

    #[test]
    fn empty_command_is_rejected() {
        let src = r#"name = "tt"
description = "d"
command = []"#;
        let err = parse_manifest(src, Path::new("t.toml")).unwrap_err();
        assert!(err.contains("command"), "got: {err}");
    }

    #[test]
    fn bad_names_are_rejected() {
        for bad in [
            "Lint",
            "1tool",
            "a",
            "has-dash",
            "has space",
            "way_too_long_name_that_keeps_going_way_past_the_sixty_four_character_limit_x",
        ] {
            let src = format!("name = \"{bad}\"\ndescription = \"d\"\ncommand = [\"./x.sh\"]");
            assert!(
                parse_manifest(&src, Path::new("t.toml")).is_err(),
                "name `{bad}` should be rejected"
            );
        }
    }

    #[test]
    fn good_names_are_accepted() {
        for good in ["ab", "lint_fix", "run_tests_2", "a1"] {
            let src = format!("name = \"{good}\"\ndescription = \"d\"\ncommand = [\"./x.sh\"]");
            assert!(
                parse_manifest(&src, Path::new("t.toml")).is_ok(),
                "name `{good}` should be accepted"
            );
        }
    }

    #[test]
    fn reserved_names_are_rejected() {
        for reserved in RESERVED_NAMES {
            let src = format!("name = \"{reserved}\"\ndescription = \"d\"\ncommand = [\"./x.sh\"]");
            let err = parse_manifest(&src, Path::new("t.toml")).unwrap_err();
            assert!(err.contains("reserved"), "reserved `{reserved}` -> {err}");
        }
    }

    #[test]
    fn timeout_defaults_when_omitted_and_clamps_over_cap() {
        let base = "name = \"tt\"\ndescription = \"d\"\ncommand = [\"./x.sh\"]";
        let default = parse_manifest(base, Path::new("t.toml")).unwrap();
        assert_eq!(default.timeout_ms, DEFAULT_TIMEOUT_MS);

        let over = format!("{base}\ntimeout_ms = 99999999");
        let clamped = parse_manifest(&over, Path::new("t.toml")).unwrap();
        assert_eq!(clamped.timeout_ms, MAX_TIMEOUT_MS);

        let zero = format!("{base}\ntimeout_ms = 0");
        let zeroed = parse_manifest(&zero, Path::new("t.toml")).unwrap();
        assert_eq!(zeroed.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn absent_input_schema_defaults_to_object() {
        let src = "name = \"tt\"\ndescription = \"d\"\ncommand = [\"./x.sh\"]";
        let tool = parse_manifest(src, Path::new("t.toml")).unwrap();
        assert_eq!(tool.input_schema["type"], "object");
    }

    // ---- discovery --------------------------------------------------------

    fn write_manifest(dir: &Path, file: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(file), body).unwrap();
    }

    fn ws_tools(root: &Path) -> PathBuf {
        root.join(".stella").join("tools")
    }

    fn global_tools(home: &Path) -> PathBuf {
        home.join(".config").join("stella").join("tools")
    }

    #[test]
    fn discovers_workspace_and_global_tools() {
        let ws = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_manifest(
            &ws_tools(ws.path()),
            "a.toml",
            "name = \"a_tool\"\ndescription = \"d\"\ncommand = [\"./a.sh\"]",
        );
        write_manifest(
            &global_tools(home.path()),
            "b.toml",
            "name = \"b_tool\"\ndescription = \"d\"\ncommand = [\"./b.sh\"]",
        );

        let report = discover_in(ws.path(), Some(home.path()));
        let names: Vec<&str> = report.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"a_tool"), "names: {names:?}");
        assert!(names.contains(&"b_tool"), "names: {names:?}");
        assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    }

    #[test]
    fn absent_home_skips_global_scan_without_error() {
        let ws = tempfile::tempdir().unwrap();
        write_manifest(
            &ws_tools(ws.path()),
            "a.toml",
            "name = \"a_tool\"\ndescription = \"d\"\ncommand = [\"./a.sh\"]",
        );
        let report = discover_in(ws.path(), None);
        assert_eq!(report.tools.len(), 1);
        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn malformed_manifest_becomes_a_diagnostic_and_does_not_kill_discovery() {
        let ws = tempfile::tempdir().unwrap();
        let dir = ws_tools(ws.path());
        write_manifest(
            &dir,
            "good.toml",
            "name = \"good_tool\"\ndescription = \"d\"\ncommand = [\"./g.sh\"]",
        );
        write_manifest(&dir, "bad.toml", "this is not = valid toml [[[");

        let report = discover_in(ws.path(), None);
        assert_eq!(report.tools.len(), 1);
        assert_eq!(report.tools[0].name, "good_tool");
        assert_eq!(report.diagnostics.len(), 1);
        assert!(report.diagnostics[0].path.ends_with("bad.toml"));
    }

    #[test]
    fn reserved_name_collision_is_a_diagnostic_and_skipped() {
        let ws = tempfile::tempdir().unwrap();
        write_manifest(
            &ws_tools(ws.path()),
            "bash.toml",
            "name = \"bash\"\ndescription = \"d\"\ncommand = [\"./b.sh\"]",
        );
        let report = discover_in(ws.path(), None);
        assert!(report.tools.is_empty(), "reserved tool must be skipped");
        assert_eq!(report.diagnostics.len(), 1);
        assert!(report.diagnostics[0].reason.contains("reserved"));
    }

    #[test]
    fn workspace_wins_over_global_on_name_collision() {
        let ws = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_manifest(
            &ws_tools(ws.path()),
            "dup.toml",
            "name = \"dup\"\ndescription = \"WORKSPACE\"\ncommand = [\"./w.sh\"]",
        );
        write_manifest(
            &global_tools(home.path()),
            "dup.toml",
            "name = \"dup\"\ndescription = \"GLOBAL\"\ncommand = [\"./g.sh\"]",
        );

        let report = discover_in(ws.path(), Some(home.path()));
        assert_eq!(report.tools.len(), 1);
        assert_eq!(report.tools[0].description, "WORKSPACE");
        assert_eq!(report.diagnostics.len(), 1, "global dup must be flagged");
        assert!(report.diagnostics[0].path.starts_with(home.path()));
    }

    // ---- execution --------------------------------------------------------

    /// Write an executable `#!/bin/sh` script into `root` and return a
    /// [`CustomTool`] whose relative `command[0]` resolves against `root`.
    fn script_tool(root: &Path, file: &str, body: &str, timeout_ms: u64) -> CustomTool {
        let path = root.join(file);
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        CustomTool {
            name: "t".into(),
            description: "d".into(),
            command: vec![format!("./{file}")],
            timeout_ms,
            input_schema: serde_json::json!({ "type": "object" }),
            env: HashMap::new(),
            source: path,
        }
    }

    #[tokio::test]
    async fn exit_zero_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script_tool(dir.path(), "ok.sh", "#!/bin/sh\necho custom_ran\n", 5000);
        let out = run_custom(&tool, &serde_json::json!({}), dir.path()).await;
        match out {
            ToolOutput::Ok { content } => assert!(content.contains("custom_ran"), "{content}"),
            ToolOutput::Error { message } => panic!("expected ok: {message}"),
        }
    }

    #[tokio::test]
    async fn input_json_is_delivered_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        // `cat` echoes the JSON document written to stdin.
        let tool = script_tool(dir.path(), "stdin.sh", "#!/bin/sh\ncat\n", 5000);
        let out = run_custom(
            &tool,
            &serde_json::json!({ "path": "src/lib.rs" }),
            dir.path(),
        )
        .await;
        match out {
            ToolOutput::Ok { content } => {
                assert!(content.contains("\"path\""), "{content}");
                assert!(content.contains("src/lib.rs"), "{content}");
            }
            ToolOutput::Error { message } => panic!("expected ok: {message}"),
        }
    }

    #[tokio::test]
    async fn scalar_inputs_are_exported_as_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script_tool(
            dir.path(),
            "env.sh",
            "#!/bin/sh\necho \"path=$STELLA_INPUT_PATH dry=$STELLA_INPUT_DRY_RUN n=$STELLA_INPUT_COUNT\"\n",
            5000,
        );
        let input = serde_json::json!({ "path": "hello", "dry_run": true, "count": 7 });
        let out = run_custom(&tool, &input, dir.path()).await;
        match out {
            ToolOutput::Ok { content } => {
                assert!(content.contains("path=hello"), "{content}");
                assert!(content.contains("dry=true"), "{content}");
                assert!(content.contains("n=7"), "{content}");
            }
            ToolOutput::Error { message } => panic!("expected ok: {message}"),
        }
    }

    #[tokio::test]
    async fn nested_input_is_not_exported_as_env_but_still_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script_tool(
            dir.path(),
            "nested.sh",
            "#!/bin/sh\necho \"nested=[$STELLA_INPUT_NESTED]\"\ncat\n",
            5000,
        );
        let input = serde_json::json!({ "nested": { "a": 1 } });
        let out = run_custom(&tool, &input, dir.path()).await;
        match out {
            ToolOutput::Ok { content } => {
                assert!(
                    content.contains("nested=[]"),
                    "object must not export env: {content}"
                );
                assert!(content.contains("\"nested\""), "still on stdin: {content}");
            }
            ToolOutput::Error { message } => panic!("expected ok: {message}"),
        }
    }

    #[tokio::test]
    async fn manifest_env_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let mut tool = script_tool(
            dir.path(),
            "menv.sh",
            "#!/bin/sh\necho \"p=$LINT_PROFILE\"\n",
            5000,
        );
        tool.env.insert("LINT_PROFILE".into(), "strict".into());
        let out = run_custom(&tool, &serde_json::json!({}), dir.path()).await;
        match out {
            ToolOutput::Ok { content } => assert!(content.contains("p=strict"), "{content}"),
            ToolOutput::Error { message } => panic!("expected ok: {message}"),
        }
    }

    #[tokio::test]
    async fn nonzero_exit_becomes_error_with_code_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script_tool(
            dir.path(),
            "fail.sh",
            "#!/bin/sh\necho boom >&2\nexit 3\n",
            5000,
        );
        let out = run_custom(&tool, &serde_json::json!({}), dir.path()).await;
        match out {
            ToolOutput::Ok { content } => panic!("expected error: {content}"),
            ToolOutput::Error { message } => {
                assert!(message.contains("code 3"), "{message}");
                assert!(message.contains("boom"), "{message}");
            }
        }
    }

    #[tokio::test]
    async fn timeout_kills_and_returns_fast() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script_tool(dir.path(), "slow.sh", "#!/bin/sh\nsleep 30\n", 200);
        let start = std::time::Instant::now();
        let out = run_custom(&tool, &serde_json::json!({}), dir.path()).await;
        let elapsed = start.elapsed();
        assert!(out.is_error());
        if let ToolOutput::Error { message } = out {
            assert!(message.contains("timed out"), "{message}");
        }
        assert!(
            elapsed.as_secs() < 5,
            "should not wait for the full sleep: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn missing_script_names_the_path_tried() {
        let dir = tempfile::tempdir().unwrap();
        let tool = CustomTool {
            name: "t".into(),
            description: "d".into(),
            command: vec!["./does-not-exist.sh".into()],
            timeout_ms: 5000,
            input_schema: serde_json::json!({ "type": "object" }),
            env: HashMap::new(),
            source: dir.path().join("t.toml"),
        };
        let out = run_custom(&tool, &serde_json::json!({}), dir.path()).await;
        match out {
            ToolOutput::Ok { content } => panic!("expected error: {content}"),
            ToolOutput::Error { message } => {
                assert!(message.contains("./does-not-exist.sh"), "{message}");
                assert!(message.contains("failed to spawn"), "{message}");
            }
        }
    }

    #[tokio::test]
    async fn oversized_output_is_truncated_middle_out() {
        let dir = tempfile::tempdir().unwrap();
        // Emit ~200k bytes of 'X' (well past MAX_OUTPUT_BYTES).
        let tool = script_tool(
            dir.path(),
            "big.sh",
            "#!/bin/sh\nhead -c 200000 /dev/zero | tr '\\0' 'X'\n",
            5000,
        );
        let out = run_custom(&tool, &serde_json::json!({}), dir.path()).await;
        match out {
            ToolOutput::Ok { content } => {
                assert!(content.contains("truncated"), "elision marker present");
                assert!(
                    content.len() <= MAX_OUTPUT_BYTES + 200,
                    "capped: {}",
                    content.len()
                );
            }
            ToolOutput::Error { message } => panic!("expected ok: {message}"),
        }
    }

    #[tokio::test]
    async fn non_object_input_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script_tool(dir.path(), "cat.sh", "#!/bin/sh\ncat\n", 5000);
        // A bare array — no top-level object, so no env vars, but still on stdin.
        let out = run_custom(&tool, &serde_json::json!(["a", "b"]), dir.path()).await;
        match out {
            ToolOutput::Ok { content } => assert!(content.contains("\"a\""), "{content}"),
            ToolOutput::Error { message } => panic!("expected ok: {message}"),
        }
    }

    // ---- CustomToolSet composition ---------------------------------------

    struct FakeInner;
    #[async_trait]
    impl ToolExecutor for FakeInner {
        fn schemas(&self) -> Vec<ToolSchema> {
            vec![ToolSchema {
                name: "bash".into(),
                description: "run".into(),
                input_schema: serde_json::json!({ "type": "object" }),
                read_only: false,
            }]
        }
        async fn execute(&self, name: &str, _input: &Value) -> ToolOutput {
            ToolOutput::Ok {
                content: format!("inner ran {name}"),
            }
        }
    }

    #[tokio::test]
    async fn set_advertises_inner_plus_custom_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script_tool(dir.path(), "s.sh", "#!/bin/sh\necho hi\n", 5000);
        let inner = FakeInner;
        let set = CustomToolSet::new(&inner, vec![tool], dir.path().to_path_buf());
        let names: Vec<String> = set.schemas().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"t".to_string()));
    }

    #[tokio::test]
    async fn set_routes_custom_names_and_falls_through_for_others() {
        let dir = tempfile::tempdir().unwrap();
        let mut tool = script_tool(dir.path(), "s.sh", "#!/bin/sh\necho from_custom\n", 5000);
        tool.name = "my_tool".into();
        tool.command = vec!["./s.sh".into()];
        let inner = FakeInner;
        let set = CustomToolSet::new(&inner, vec![tool], dir.path().to_path_buf());

        let custom = set.execute("my_tool", &serde_json::json!({})).await;
        match custom {
            ToolOutput::Ok { content } => assert!(content.contains("from_custom"), "{content}"),
            ToolOutput::Error { message } => panic!("expected ok: {message}"),
        }

        // Unknown-to-custom name falls through to the inner executor.
        let fell = set.execute("bash", &serde_json::json!({})).await;
        match fell {
            ToolOutput::Ok { content } => assert_eq!(content, "inner ran bash"),
            ToolOutput::Error { message } => panic!("expected fallthrough: {message}"),
        }
    }

    #[test]
    fn env_var_name_uppercases_and_sanitizes() {
        assert_eq!(env_var_name("path"), "STELLA_INPUT_PATH");
        assert_eq!(env_var_name("dry_run"), "STELLA_INPUT_DRY_RUN");
        assert_eq!(env_var_name("weird-key.x"), "STELLA_INPUT_WEIRD_KEY_X");
    }
}
