//! Lifecycle hooks engine (ported from
//! `apps/cli/src/settings/hooks.ts`).
//!
//! Hooks are shell commands declared in workspace settings that fire on
//! agent lifecycle events, receiving the event payload as JSON on stdin
//! (Claude Code parity). Three events are wired, each with distinct,
//! load-bearing behavior (mirrors `hooks.ts`'s header exactly):
//!
//!   - [`HookEvent::SessionStart`] — runs once before the turn. Anything a
//!     hook prints to stdout is appended to the system prompt as
//!     additional context.
//!   - [`HookEvent::PreToolUse`] — runs before a tool executes. A non-zero
//!     exit BLOCKS the tool — the model receives the hook's message
//!     instead of running it.
//!   - [`HookEvent::PostToolUse`] — runs after a tool executes (side
//!     effects only). Never blocks.
//!
//! Matchers are globs over the tool name for `PreToolUse`/`PostToolUse`;
//! `SessionStart` ignores the matcher and runs every action.
//!
//! # No I/O in this module
//!
//! Actually spawning a hook command (process creation, stdin/stdout
//! piping, timeout + `SIGKILL`, signal-based abort) is real I/O, which
//! `stella-core` never performs directly — that job belongs to
//! `stella-tools`/`stella-cli`, mirroring how [`crate::ports::ToolExecutor`]
//! is the injectable *tool*-execution port that `stella-tools::ToolRegistry`
//! implements. [`HookRunner`] is the equivalent execution port for hooks.
//! Config parsing (there is none here — the shapes already deserialize
//! straight off `settings.json` via `serde`), matcher-glob selection, and
//! the per-event blocking decision are the plain logic that stays in this
//! crate, unit-tested below against a fake `HookRunner` — no real process
//! spawning required.
//!
//! Unlike `hooks.ts`'s `execHook`, which collapses every failure mode
//! (spawn error, timeout, abort, non-zero exit) into one `{code, stdout,
//! stderr}` shape, [`HookRunner::run`] returns a `Result` so a genuine
//! execution failure (never ran) is structurally distinguishable from a
//! hook that ran and exited non-zero — see [`HookExecError`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Lifecycle events a hook can fire on (TS: `HookEvent`, `HOOK_EVENTS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    SessionStart,
    PreToolUse,
    PostToolUse,
}

/// Default per-hook timeout — 60s (TS: `DEFAULT_HOOK_TIMEOUT_MS`).
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 60_000;
/// Hard ceiling on a configured timeout — 10 minutes (TS: the
/// `Math.min(..., 600_000)` clamp in `execHook`).
pub const MAX_HOOK_TIMEOUT_MS: u64 = 600_000;

fn default_hook_kind() -> String {
    "command".to_string()
}

/// A single shell command a hook runs (TS: `HookAction`). Only `"command"`
/// hooks exist today; `kind` is kept (rather than assumed) so this type
/// round-trips a real `settings.json` — including a future second hook
/// kind — without a schema break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookAction {
    #[serde(rename = "type", default = "default_hook_kind")]
    pub kind: String,
    /// Shell command. Receives the event payload as JSON on stdin.
    pub command: String,
    #[serde(rename = "timeoutMs", skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl HookAction {
    /// Construct a plain command hook with the default timeout.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            kind: default_hook_kind(),
            command: command.into(),
            timeout_ms: None,
        }
    }

    /// The effective timeout: configured value clamped to
    /// [`MAX_HOOK_TIMEOUT_MS`], or [`DEFAULT_HOOK_TIMEOUT_MS`] when unset —
    /// the same clamp `execHook` applies before spawning.
    pub fn effective_timeout_ms(&self) -> u64 {
        self.timeout_ms
            .unwrap_or(DEFAULT_HOOK_TIMEOUT_MS)
            .min(MAX_HOOK_TIMEOUT_MS)
    }
}

/// Groups hook actions under a tool-name pattern (TS: `HookMatcher`). For
/// `PreToolUse`/`PostToolUse`, `matcher` is a glob over the tool name
/// (e.g. `"bash"`, `"write_file"`, `"*"`). For `SessionStart` it is
/// ignored — every action runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookMatcher {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub hooks: Vec<HookAction>,
}

/// Hooks keyed by lifecycle event (TS: `Hooks`). Field names are `rename`d
/// to the exact PascalCase keys `settings.json` uses, so this type
/// deserializes a real workspace settings file without a translation
/// layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hooks {
    #[serde(
        rename = "SessionStart",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub session_start: Option<Vec<HookMatcher>>,
    #[serde(
        rename = "PreToolUse",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub pre_tool_use: Option<Vec<HookMatcher>>,
    #[serde(
        rename = "PostToolUse",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub post_tool_use: Option<Vec<HookMatcher>>,
}

impl Hooks {
    /// The matchers registered for `event`, or an empty slice when none
    /// are configured.
    pub fn matchers_for(&self, event: HookEvent) -> &[HookMatcher] {
        let field = match event {
            HookEvent::SessionStart => &self.session_start,
            HookEvent::PreToolUse => &self.pre_tool_use,
            HookEvent::PostToolUse => &self.post_tool_use,
        };
        field.as_deref().unwrap_or(&[])
    }
}

/// The tool a `PreToolUse`/`PostToolUse` hook fires for (TS: `HookPayload["tool"]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookToolInfo {
    pub name: String,
    pub input: serde_json::Value,
}

/// The JSON payload fed to a hook command on stdin (TS: `HookPayload`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookPayload {
    pub event: HookEvent,
    pub cwd: String,
    /// Present for `PreToolUse` / `PostToolUse`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<HookToolInfo>,
    /// Present for `PostToolUse`: the (clipped) result string the tool
    /// returned.
    #[serde(rename = "toolResult", skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<String>,
}

impl HookPayload {
    /// A `SessionStart` payload — no tool, no result.
    pub fn session_start(cwd: impl Into<String>) -> Self {
        Self {
            event: HookEvent::SessionStart,
            cwd: cwd.into(),
            tool: None,
            tool_result: None,
        }
    }

    /// A `PreToolUse` payload for the given tool call.
    pub fn pre_tool_use(
        cwd: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        Self {
            event: HookEvent::PreToolUse,
            cwd: cwd.into(),
            tool: Some(HookToolInfo {
                name: name.into(),
                input,
            }),
            tool_result: None,
        }
    }

    /// A `PostToolUse` payload for the given tool call and its result.
    pub fn post_tool_use(
        cwd: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
        tool_result: impl Into<String>,
    ) -> Self {
        Self {
            event: HookEvent::PostToolUse,
            cwd: cwd.into(),
            tool: Some(HookToolInfo {
                name: name.into(),
                input,
            }),
            tool_result: Some(tool_result.into()),
        }
    }
}

/// One hook command's raw execution result — returned even on non-zero
/// exit; only genuine inability to run is a [`HookExecError`] (TS:
/// `HookResult`, minus the collapsed failure codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A hook that never produced an exit code — structurally distinct from
/// [`HookExecResult`] with a non-zero `exit_code` (TS collapses all three
/// of these into `{code: 124 | 130 | 1, ...}`; here the port makes the
/// distinction explicit so a caller — or a test — can tell "it ran and
/// failed" apart from "it never ran").
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HookExecError {
    #[error("hook `{command}` timed out after {timeout_ms}ms")]
    TimedOut { command: String, timeout_ms: u64 },
    #[error("hook `{command}` was aborted")]
    Aborted { command: String },
    #[error("hook `{command}` failed to start: {message}")]
    SpawnFailed { command: String, message: String },
}

/// The execution port for hooks (mirrors [`crate::ports::ToolExecutor`]).
/// A production implementation (owned by `stella-tools`/`stella-cli`)
/// spawns `action.command` under a shell, feeds `payload_json` on stdin,
/// runs in `cwd`, and enforces [`HookAction::effective_timeout_ms`] with a
/// process-group kill — the real I/O `stella-core` never performs
/// directly.
#[async_trait]
pub trait HookRunner: Send + Sync {
    async fn run(
        &self,
        action: &HookAction,
        payload_json: &str,
        cwd: &str,
    ) -> Result<HookExecResult, HookExecError>;
}

/// Which matchers apply for `event` + the tool under consideration.
/// `SessionStart` ignores the matcher entirely — every registered action
/// runs (TS: `selectMatchers`).
pub fn select_matchers<'a>(
    event: HookEvent,
    matchers: &'a [HookMatcher],
    tool_name: Option<&str>,
) -> Vec<&'a HookMatcher> {
    if event == HookEvent::SessionStart {
        return matchers.iter().collect();
    }
    let Some(name) = tool_name else {
        return Vec::new();
    };
    matchers
        .iter()
        .filter(|m| crate::glob::match_glob(m.matcher.as_deref().unwrap_or("*"), name))
        .collect()
}

/// What running a set of hooks for one event produced (TS:
/// `HookRunOutcome`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookRunOutcome {
    /// `PreToolUse` only: a hook blocked the tool.
    pub blocked: bool,
    /// Human-readable reason when blocked (hook stderr/stdout, a timeout
    /// message, or a spawn-failure message).
    pub reason: Option<String>,
    /// Concatenated stdout of every hook that ran to completion (used for
    /// `SessionStart` context).
    pub output: String,
    /// Non-blocking failures observed while running the event's hooks: a
    /// hook that failed to spawn, or exited non-zero on an event that
    /// cannot block (`PostToolUse`, `SessionStart`). Empty means every
    /// hook ran clean. Callers surface these as warnings — never as
    /// errors — so a typo'd hook is visible instead of silently
    /// contributing nothing all session.
    pub diagnostics: Vec<String>,
}

impl HookRunOutcome {
    fn none() -> Self {
        Self::default()
    }

    fn blocked(reason: String, output: String) -> Self {
        Self {
            blocked: true,
            reason: Some(reason),
            output,
            diagnostics: Vec::new(),
        }
    }
}

/// Run every hook registered for `payload.event`, in matcher order then
/// action order, via `runner`. Returns whether the action should be
/// blocked (`PreToolUse` only) and any captured stdout (`SessionStart`
/// context) — TS: `runHooks`.
///
/// Blocking semantics (ported exactly): only `PreToolUse` can block.
/// A hook that ran and exited non-zero blocks with its stderr (falling
/// back to stdout, then a generic message) as the reason. A hook that
/// never ran at all — [`HookExecError`] — blocks `PreToolUse` too, with
/// the error's own message as the reason; `PostToolUse` and
/// `SessionStart` never block on either failure mode, matching `hooks.ts`
/// checking `payload.event === "PreToolUse"` as the sole gate.
pub async fn run_hooks(
    runner: &dyn HookRunner,
    hooks: Option<&Hooks>,
    payload: &HookPayload,
) -> HookRunOutcome {
    let Some(hooks) = hooks else {
        return HookRunOutcome::none();
    };
    let matchers = hooks.matchers_for(payload.event);
    if matchers.is_empty() {
        return HookRunOutcome::none();
    }

    let tool_name = payload.tool.as_ref().map(|t| t.name.as_str());
    let selected = select_matchers(payload.event, matchers, tool_name);
    if selected.is_empty() {
        return HookRunOutcome::none();
    }

    let payload_json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    let mut outputs: Vec<String> = Vec::new();
    let mut diagnostics: Vec<String> = Vec::new();

    for matcher in selected {
        for action in &matcher.hooks {
            match runner.run(action, &payload_json, &payload.cwd).await {
                Ok(result) => {
                    let trimmed_stdout = result.stdout.trim();
                    if !trimmed_stdout.is_empty() {
                        outputs.push(trimmed_stdout.to_string());
                    }
                    if result.exit_code != 0 {
                        if payload.event == HookEvent::PreToolUse {
                            let trimmed_stderr = result.stderr.trim();
                            let reason = if !trimmed_stderr.is_empty() {
                                trimmed_stderr.to_string()
                            } else if !trimmed_stdout.is_empty() {
                                trimmed_stdout.to_string()
                            } else {
                                format!("hook `{}` exited {}", action.command, result.exit_code)
                            };
                            return HookRunOutcome::blocked(reason, outputs.join("\n"));
                        }
                        // Non-blocking event: the failure must still leave a
                        // trace (a broken hook silently contributing nothing
                        // is the defect), so it lands in `diagnostics`.
                        let trimmed_stderr = result.stderr.trim();
                        diagnostics.push(if trimmed_stderr.is_empty() {
                            format!("hook `{}` exited {}", action.command, result.exit_code)
                        } else {
                            format!(
                                "hook `{}` exited {}: {trimmed_stderr}",
                                action.command, result.exit_code
                            )
                        });
                    }
                }
                Err(err) => {
                    if payload.event == HookEvent::PreToolUse {
                        return HookRunOutcome::blocked(err.to_string(), outputs.join("\n"));
                    }
                    // PostToolUse/SessionStart never block, even on a hook
                    // that failed to run at all — keep going, but record
                    // the failure for the caller to surface as a warning.
                    diagnostics.push(format!("hook `{}` failed to run: {err}", action.command));
                }
            }
        }
    }

    HookRunOutcome {
        blocked: false,
        reason: None,
        output: outputs.join("\n"),
        diagnostics,
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A scripted, no-I/O `HookRunner`: keyed by exact command string,
    /// records every call it received so tests can assert on the payload
    /// actually handed to the port.
    struct FakeHookRunner {
        scripted: HashMap<String, Result<HookExecResult, HookExecError>>,
        calls: Mutex<Vec<(String, String, String)>>,
    }

    impl FakeHookRunner {
        fn new(scripted: HashMap<String, Result<HookExecResult, HookExecError>>) -> Self {
            Self {
                scripted,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, String, String)> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl HookRunner for FakeHookRunner {
        async fn run(
            &self,
            action: &HookAction,
            payload_json: &str,
            cwd: &str,
        ) -> Result<HookExecResult, HookExecError> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).push((
                action.command.clone(),
                payload_json.to_string(),
                cwd.to_string(),
            ));
            self.scripted
                .get(&action.command)
                .cloned()
                .unwrap_or(Ok(HookExecResult {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }))
        }
    }

    fn ok(exit_code: i32, stdout: &str, stderr: &str) -> Result<HookExecResult, HookExecError> {
        Ok(HookExecResult {
            exit_code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        })
    }

    // ---- select_matchers (pure) ----

    #[test]
    fn session_start_ignores_the_matcher_field() {
        let matchers = vec![HookMatcher {
            matcher: Some("write_file".to_string()),
            hooks: vec![HookAction::new("echo hi")],
        }];
        let selected = select_matchers(HookEvent::SessionStart, &matchers, None);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn pre_tool_use_filters_by_glob_over_the_tool_name() {
        let matchers = vec![HookMatcher {
            matcher: Some("write_file".to_string()),
            hooks: vec![HookAction::new("exit 1")],
        }];
        assert!(select_matchers(HookEvent::PreToolUse, &matchers, Some("bash")).is_empty());
        assert_eq!(
            select_matchers(HookEvent::PreToolUse, &matchers, Some("write_file")).len(),
            1
        );
    }

    #[test]
    fn a_missing_matcher_defaults_to_star() {
        let matchers = vec![HookMatcher {
            matcher: None,
            hooks: vec![HookAction::new("true")],
        }];
        assert_eq!(
            select_matchers(HookEvent::PreToolUse, &matchers, Some("anything")).len(),
            1
        );
    }

    #[test]
    fn no_tool_name_on_a_tool_scoped_event_selects_nothing() {
        let matchers = vec![HookMatcher {
            matcher: Some("*".to_string()),
            hooks: vec![HookAction::new("true")],
        }];
        assert!(select_matchers(HookEvent::PreToolUse, &matchers, None).is_empty());
    }

    // ---- run_hooks ----

    #[tokio::test]
    async fn returns_a_noop_outcome_with_no_hooks() {
        let runner = FakeHookRunner::new(HashMap::new());
        let out = run_hooks(&runner, None, &HookPayload::session_start("/proj")).await;
        assert_eq!(out, HookRunOutcome::default());
    }

    #[tokio::test]
    async fn captures_session_start_stdout_as_context() {
        let mut scripted = HashMap::new();
        scripted.insert(
            "echo 'on-call: alice'".to_string(),
            ok(0, "on-call: alice", ""),
        );
        let runner = FakeHookRunner::new(scripted);
        let hooks = Hooks {
            session_start: Some(vec![HookMatcher {
                matcher: None,
                hooks: vec![HookAction::new("echo 'on-call: alice'")],
            }]),
            ..Hooks::default()
        };
        let out = run_hooks(&runner, Some(&hooks), &HookPayload::session_start("/proj")).await;
        assert!(!out.blocked);
        assert!(out.output.contains("on-call: alice"));
    }

    #[tokio::test]
    async fn feeds_the_event_payload_to_the_runner_on_stdin() {
        let mut scripted = HashMap::new();
        scripted.insert("cat".to_string(), ok(0, "", ""));
        let runner = FakeHookRunner::new(scripted);
        let hooks = Hooks {
            pre_tool_use: Some(vec![HookMatcher {
                matcher: Some("*".to_string()),
                hooks: vec![HookAction::new("cat")],
            }]),
            ..Hooks::default()
        };
        let payload =
            HookPayload::pre_tool_use("/proj", "bash", serde_json::json!({"command": "ls"}));
        run_hooks(&runner, Some(&hooks), &payload).await;
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.contains("\"name\":\"bash\""));
        assert!(calls[0].1.contains("\"command\":\"ls\""));
        assert_eq!(calls[0].2, "/proj");
    }

    #[tokio::test]
    async fn blocks_pre_tool_use_on_nonzero_exit_surfacing_stderr() {
        let mut scripted = HashMap::new();
        scripted.insert(
            "echo no shell here 1>&2; exit 1".to_string(),
            ok(1, "", "no shell here"),
        );
        let runner = FakeHookRunner::new(scripted);
        let hooks = Hooks {
            pre_tool_use: Some(vec![HookMatcher {
                matcher: Some("bash".to_string()),
                hooks: vec![HookAction::new("echo no shell here 1>&2; exit 1")],
            }]),
            ..Hooks::default()
        };
        let payload = HookPayload::pre_tool_use("/proj", "bash", serde_json::json!({}));
        let out = run_hooks(&runner, Some(&hooks), &payload).await;
        assert!(out.blocked);
        assert!(out.reason.unwrap().contains("no shell here"));
    }

    #[tokio::test]
    async fn does_not_block_when_pre_tool_use_hook_exits_zero() {
        let mut scripted = HashMap::new();
        scripted.insert("true".to_string(), ok(0, "", ""));
        let runner = FakeHookRunner::new(scripted);
        let hooks = Hooks {
            pre_tool_use: Some(vec![HookMatcher {
                matcher: Some("bash".to_string()),
                hooks: vec![HookAction::new("true")],
            }]),
            ..Hooks::default()
        };
        let payload = HookPayload::pre_tool_use("/proj", "bash", serde_json::json!({}));
        let out = run_hooks(&runner, Some(&hooks), &payload).await;
        assert!(!out.blocked);
    }

    #[tokio::test]
    async fn only_runs_hooks_whose_matcher_globs_the_tool_name() {
        let mut scripted = HashMap::new();
        scripted.insert("exit 1".to_string(), ok(1, "", ""));
        let runner = FakeHookRunner::new(scripted);
        let hooks = Hooks {
            pre_tool_use: Some(vec![HookMatcher {
                matcher: Some("write_file".to_string()),
                hooks: vec![HookAction::new("exit 1")],
            }]),
            ..Hooks::default()
        };

        let bash = run_hooks(
            &runner,
            Some(&hooks),
            &HookPayload::pre_tool_use("/proj", "bash", serde_json::json!({})),
        )
        .await;
        assert!(!bash.blocked);

        let write = run_hooks(
            &runner,
            Some(&hooks),
            &HookPayload::pre_tool_use("/proj", "write_file", serde_json::json!({})),
        )
        .await;
        assert!(write.blocked);
    }

    #[tokio::test]
    async fn never_blocks_on_post_tool_use_even_on_nonzero_exit() {
        let mut scripted = HashMap::new();
        scripted.insert("echo done; exit 3".to_string(), ok(3, "done", ""));
        let runner = FakeHookRunner::new(scripted);
        let hooks = Hooks {
            post_tool_use: Some(vec![HookMatcher {
                matcher: Some("*".to_string()),
                hooks: vec![HookAction::new("echo done; exit 3")],
            }]),
            ..Hooks::default()
        };
        let payload = HookPayload::post_tool_use(
            "/proj",
            "write_file",
            serde_json::json!({}),
            "wrote 3 lines",
        );
        let out = run_hooks(&runner, Some(&hooks), &payload).await;
        assert!(!out.blocked);
        assert!(out.output.contains("done"));
        // Non-blocking, but never silent: the non-zero exit must leave a
        // diagnostic for the caller to surface (issue #373, item 6).
        assert_eq!(out.diagnostics.len(), 1);
        assert!(
            out.diagnostics[0].contains("exited 3"),
            "diagnostic must name the exit: {:?}",
            out.diagnostics
        );
    }

    #[tokio::test]
    async fn clean_hooks_produce_no_diagnostics() {
        let mut scripted = HashMap::new();
        scripted.insert("echo ok".to_string(), ok(0, "ok", ""));
        let runner = FakeHookRunner::new(scripted);
        let hooks = Hooks {
            session_start: Some(vec![HookMatcher {
                matcher: None,
                hooks: vec![HookAction::new("echo ok")],
            }]),
            ..Hooks::default()
        };
        let payload = HookPayload::session_start("/proj".to_string());
        let out = run_hooks(&runner, Some(&hooks), &payload).await;
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    #[tokio::test]
    async fn session_start_spawn_failure_lands_in_diagnostics_not_silence() {
        let mut scripted = HashMap::new();
        scripted.insert(
            "typo-cmd".to_string(),
            Err(HookExecError::SpawnFailed {
                command: "typo-cmd".to_string(),
                message: "No such file or directory".to_string(),
            }),
        );
        let runner = FakeHookRunner::new(scripted);
        let hooks = Hooks {
            session_start: Some(vec![HookMatcher {
                matcher: None,
                hooks: vec![HookAction::new("typo-cmd")],
            }]),
            ..Hooks::default()
        };
        let payload = HookPayload::session_start("/proj".to_string());
        let out = run_hooks(&runner, Some(&hooks), &payload).await;
        assert!(!out.blocked, "SessionStart never blocks");
        assert_eq!(out.diagnostics.len(), 1);
        assert!(
            out.diagnostics[0].contains("typo-cmd") && out.diagnostics[0].contains("failed to run"),
            "diagnostic must name the hook and the failure: {:?}",
            out.diagnostics
        );
    }

    #[tokio::test]
    async fn returns_a_noop_when_the_event_has_no_registered_matchers() {
        let runner = FakeHookRunner::new(HashMap::new());
        let hooks = Hooks {
            session_start: Some(vec![HookMatcher {
                matcher: None,
                hooks: vec![HookAction::new("echo x")],
            }]),
            ..Hooks::default()
        };
        let payload = HookPayload::pre_tool_use("/proj", "bash", serde_json::json!({}));
        let out = run_hooks(&runner, Some(&hooks), &payload).await;
        assert_eq!(out, HookRunOutcome::default());
    }

    #[tokio::test]
    async fn a_timed_out_hook_blocks_pre_tool_use_with_a_distinguishable_reason() {
        let mut scripted = HashMap::new();
        scripted.insert(
            "sleep 5".to_string(),
            Err(HookExecError::TimedOut {
                command: "sleep 5".to_string(),
                timeout_ms: 50,
            }),
        );
        let runner = FakeHookRunner::new(scripted);
        let hooks = Hooks {
            pre_tool_use: Some(vec![HookMatcher {
                matcher: Some("*".to_string()),
                hooks: vec![HookAction {
                    kind: default_hook_kind(),
                    command: "sleep 5".to_string(),
                    timeout_ms: Some(50),
                }],
            }]),
            ..Hooks::default()
        };
        let payload = HookPayload::pre_tool_use("/proj", "bash", serde_json::json!({}));
        let out = run_hooks(&runner, Some(&hooks), &payload).await;
        assert!(out.blocked);
        assert!(out.reason.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn a_spawn_failure_is_distinguishable_from_a_completed_nonzero_exit() {
        // Same PreToolUse blocking outcome, but the REASON text proves the
        // two failure modes are structurally different at the port level:
        // one never got an exit code at all.
        let mut scripted = HashMap::new();
        scripted.insert(
            "does-not-exist".to_string(),
            Err(HookExecError::SpawnFailed {
                command: "does-not-exist".to_string(),
                message: "No such file or directory".to_string(),
            }),
        );
        scripted.insert("exit 1".to_string(), ok(1, "", "boom"));
        let runner = FakeHookRunner::new(scripted);

        let spawn_fail_hooks = Hooks {
            pre_tool_use: Some(vec![HookMatcher {
                matcher: Some("*".to_string()),
                hooks: vec![HookAction::new("does-not-exist")],
            }]),
            ..Hooks::default()
        };
        let nonzero_exit_hooks = Hooks {
            pre_tool_use: Some(vec![HookMatcher {
                matcher: Some("*".to_string()),
                hooks: vec![HookAction::new("exit 1")],
            }]),
            ..Hooks::default()
        };
        let payload = HookPayload::pre_tool_use("/proj", "bash", serde_json::json!({}));

        let spawn_fail = run_hooks(&runner, Some(&spawn_fail_hooks), &payload).await;
        let nonzero_exit = run_hooks(&runner, Some(&nonzero_exit_hooks), &payload).await;

        assert!(spawn_fail.blocked && nonzero_exit.blocked);
        let spawn_reason = spawn_fail.reason.unwrap();
        let exit_reason = nonzero_exit.reason.unwrap();
        assert!(spawn_reason.contains("failed to start"));
        assert!(exit_reason.contains("boom"));
        assert_ne!(spawn_reason, exit_reason);
    }

    #[tokio::test]
    async fn an_aborted_or_spawn_failed_hook_never_blocks_post_tool_use() {
        let mut scripted = HashMap::new();
        scripted.insert(
            "does-not-exist".to_string(),
            Err(HookExecError::Aborted {
                command: "does-not-exist".to_string(),
            }),
        );
        let runner = FakeHookRunner::new(scripted);
        let hooks = Hooks {
            post_tool_use: Some(vec![HookMatcher {
                matcher: Some("*".to_string()),
                hooks: vec![HookAction::new("does-not-exist")],
            }]),
            ..Hooks::default()
        };
        let payload = HookPayload::post_tool_use("/proj", "bash", serde_json::json!({}), "result");
        let out = run_hooks(&runner, Some(&hooks), &payload).await;
        assert!(!out.blocked);
    }

    // ---- HookExecError Display + effective timeout ----

    #[test]
    fn hook_exec_error_variants_have_distinct_messages() {
        let timed_out = HookExecError::TimedOut {
            command: "sleep 5".to_string(),
            timeout_ms: 50,
        };
        let aborted = HookExecError::Aborted {
            command: "sleep 5".to_string(),
        };
        let spawn_failed = HookExecError::SpawnFailed {
            command: "sleep 5".to_string(),
            message: "ENOENT".to_string(),
        };
        assert!(timed_out.to_string().contains("timed out after 50ms"));
        assert!(aborted.to_string().contains("aborted"));
        assert!(spawn_failed.to_string().contains("ENOENT"));
        assert_ne!(timed_out.to_string(), aborted.to_string());
    }

    #[test]
    fn effective_timeout_defaults_and_clamps() {
        assert_eq!(
            HookAction::new("x").effective_timeout_ms(),
            DEFAULT_HOOK_TIMEOUT_MS
        );
        let mut action = HookAction::new("x");
        action.timeout_ms = Some(10_000_000);
        assert_eq!(action.effective_timeout_ms(), MAX_HOOK_TIMEOUT_MS);
        let mut action = HookAction::new("x");
        action.timeout_ms = Some(5_000);
        assert_eq!(action.effective_timeout_ms(), 5_000);
    }

    // ---- serde round trip against a real settings.json-shaped payload ----

    #[test]
    fn hooks_deserialize_from_the_real_settings_json_shape() {
        let json = r#"{
            "PreToolUse": [
                { "matcher": "bash", "hooks": [{ "type": "command", "command": "true", "timeoutMs": 5000 }] }
            ],
            "SessionStart": [
                { "hooks": [{ "command": "echo hi" }] }
            ]
        }"#;
        let hooks: Hooks = serde_json::from_str(json).unwrap();
        let pre = hooks.matchers_for(HookEvent::PreToolUse);
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].matcher.as_deref(), Some("bash"));
        assert_eq!(pre[0].hooks[0].command, "true");
        assert_eq!(pre[0].hooks[0].timeout_ms, Some(5000));

        let start = hooks.matchers_for(HookEvent::SessionStart);
        assert_eq!(start[0].hooks[0].kind, "command");
        assert!(hooks.matchers_for(HookEvent::PostToolUse).is_empty());
    }

    #[test]
    fn hook_payload_serializes_with_the_expected_json_shape() {
        let payload =
            HookPayload::pre_tool_use("/proj", "bash", serde_json::json!({"command": "ls"}));
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"event\":\"PreToolUse\""));
        assert!(json.contains("\"cwd\":\"/proj\""));
        assert!(json.contains("\"name\":\"bash\""));
        assert!(!json.contains("toolResult"));
    }
}
