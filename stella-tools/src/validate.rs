//! `validate` — the **strict** counterpart to [`crate::custom`] discovery,
//! backing `stella tools --validate`.
//!
//! Discovery ([`crate::custom::discover`]) is deliberately lenient: a broken
//! manifest becomes a diagnostic and the session runs on, and a sloppy-but-
//! loadable one is silently normalized (timeouts defaulted and clamped,
//! duplicate names dropped). That is the right behavior mid-session, but it
//! means a developer only learns about a mistake when a run fails after it
//! has already consumed model budget.
//!
//! This module is the pre-flight check: it scans the same directories, reuses
//! [`crate::custom::parse_manifest`]'s exact error messages, and additionally
//! surfaces everything discovery normalizes without a word:
//!
//! - **Errors** (the manifest will not load): unreadable file, invalid TOML /
//!   missing fields, invalid or reserved tool name, empty description, empty
//!   `command`, non-table `input_schema`.
//! - **Warnings** (the manifest loads but will not behave as written):
//!   `timeout_ms` above the 10-minute cap (clamped at runtime), a tool name
//!   already claimed by an earlier manifest (that one wins, this one is
//!   ignored), and a `command[0]` that does not exist or is not executable —
//!   a warning rather than an error because the script may legitimately be
//!   provisioned later (checked out, installed, built) before the tool runs.
//! - **Info**: `timeout_ms` omitted or `0`, which silently becomes the 30s
//!   default.
//!
//! Validation never mutates discovery's behavior — `discover` stays the
//! lenient path, this is the strict one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::custom::{self, DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS};

/// How bad a [`ValidationIssue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The manifest will not load at all — discovery would skip the tool.
    Error,
    /// The manifest loads, but not the way it is written (clamped timeout,
    /// shadowed name, missing script).
    Warning,
    /// Worth knowing, nothing to fix (e.g. an implicit default applied).
    Info,
}

/// One finding about one manifest, printable as-is.
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub severity: Severity,
    pub message: String,
}

/// Everything validation found out about a single manifest file.
#[derive(Debug, Clone)]
pub struct ManifestReport {
    /// The manifest file the findings are about.
    pub path: PathBuf,
    /// The declared tool name, when the file parses far enough to have one —
    /// present even for manifests that fail full validation (e.g. a missing
    /// `description`), so reports can name the tool the developer meant.
    pub name: Option<String>,
    /// Findings, most severe first is *not* guaranteed — they are emitted in
    /// check order (parse, duplicate, timeout, command).
    pub issues: Vec<ValidationIssue>,
}

impl ManifestReport {
    fn push(&mut self, severity: Severity, message: String) {
        self.issues.push(ValidationIssue { severity, message });
    }

    /// `true` iff this manifest would fail to load at discovery.
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|i| i.severity == Severity::Error)
    }
}

/// The result of validating a set of tool directories: one entry per `*.toml`
/// file found, in scan order (workspace before user-global, files sorted).
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    pub manifests: Vec<ManifestReport>,
}

impl ValidationReport {
    /// Total error-severity findings across all manifests.
    pub fn error_count(&self) -> usize {
        self.count(Severity::Error)
    }

    /// Total warning-severity findings across all manifests.
    pub fn warning_count(&self) -> usize {
        self.count(Severity::Warning)
    }

    /// `true` iff at least one manifest would fail to load.
    pub fn has_errors(&self) -> bool {
        self.manifests.iter().any(ManifestReport::has_errors)
    }

    fn count(&self, severity: Severity) -> usize {
        self.manifests
            .iter()
            .flat_map(|m| &m.issues)
            .filter(|i| i.severity == severity)
            .count()
    }
}

/// Validate the default discovery locations for `workspace_root`: the
/// workspace `.stella/tools/` then the user-global `~/.config/stella/tools/`
/// (read from `$HOME`), exactly the directories and order
/// [`crate::custom::discover`] scans — so duplicate-name findings mirror the
/// real workspace-wins shadowing. Thin env-reading wrapper over
/// [`validate_default_in`], mirroring `discover`/`discover_in`.
pub fn validate_default(workspace_root: &Path) -> ValidationReport {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    validate_default_in(workspace_root, home.as_deref())
}

/// [`validate_default`] with the home directory injected (tests pass a
/// tempdir instead of mutating the process env). A `None` home skips the
/// user-global scan, like [`crate::custom::discover_in`].
pub fn validate_default_in(workspace_root: &Path, home: Option<&Path>) -> ValidationReport {
    let mut dirs: Vec<PathBuf> = vec![workspace_root.join(".stella").join("tools")];
    if let Some(home) = home {
        dirs.push(home.join(".config").join("stella").join("tools"));
    }
    validate_dirs(&dirs, workspace_root)
}

/// Validate every `*.toml` manifest in one explicit directory.
/// `workspace_root` is where the tools would *run* (custom tools always spawn
/// with the workspace root as cwd — see [`crate::custom`]), so relative
/// `command[0]` paths are checked against it, not against `dir`. An absent
/// `dir` yields an empty report; callers that require the directory to exist
/// should check first.
pub fn validate_dir(dir: &Path, workspace_root: &Path) -> ValidationReport {
    validate_dirs(std::slice::from_ref(&dir.to_path_buf()), workspace_root)
}

/// Scan `dirs` in order (earlier dirs win name collisions, matching
/// discovery) and validate every manifest found.
fn validate_dirs(dirs: &[PathBuf], workspace_root: &Path) -> ValidationReport {
    let mut report = ValidationReport::default();
    // name → first manifest that claimed it; discovery order means that
    // manifest is the one a session would actually load.
    let mut seen: HashMap<String, PathBuf> = HashMap::new();

    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => continue, // no such directory → nothing to validate
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
            .collect();
        paths.sort();

        for path in paths {
            report
                .manifests
                .push(validate_file(&path, workspace_root, &mut seen));
        }
    }
    report
}

/// Validate one manifest file. `seen` carries name→path across files so
/// duplicates are reported on the manifest that would be ignored.
fn validate_file(
    path: &Path,
    workspace_root: &Path,
    seen: &mut HashMap<String, PathBuf>,
) -> ManifestReport {
    let mut out = ManifestReport {
        path: path.to_path_buf(),
        name: None,
        issues: Vec::new(),
    };

    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            // Same wording discovery uses for an unreadable manifest.
            out.push(Severity::Error, format!("could not read manifest: {e}"));
            return out;
        }
    };

    // Lenient pre-parse: even a manifest that fails full validation (say, a
    // missing `description`) usually still parses as TOML, so we can report
    // it under the tool name the developer intended and read the raw
    // `timeout_ms` before parse_manifest normalizes it away.
    let lenient: Option<toml::Value> = toml::from_str(&text).ok();
    out.name = lenient
        .as_ref()
        .and_then(|v| v.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_string);

    let tool = match custom::parse_manifest(&text, path) {
        Ok(tool) => tool,
        Err(reason) => {
            // parse_manifest's message is already precise and printable.
            out.push(Severity::Error, reason);
            return out;
        }
    };
    out.name = Some(tool.name.clone());

    // Duplicate names: discovery keeps the first definition and silently
    // drops this one, so the finding goes on the loser.
    if let Some(first) = seen.get(&tool.name) {
        let relation = if first.parent() == path.parent() {
            "another manifest in the same directory"
        } else {
            "a workspace manifest that shadows this user-global one"
        };
        out.push(
            Severity::Warning,
            format!(
                "tool name `{}` is already defined by {relation} ({}) — at discovery that \
                 definition wins and this manifest is ignored",
                tool.name,
                first.display()
            ),
        );
    } else {
        seen.insert(tool.name.clone(), path.to_path_buf());
    }

    // Timeout semantics discovery silently normalizes (custom.rs applies the
    // default for None/0 and clamps to the cap).
    let raw_timeout = lenient
        .as_ref()
        .and_then(|v| v.get("timeout_ms"))
        .and_then(toml::Value::as_integer)
        .and_then(|t| u64::try_from(t).ok());
    match raw_timeout {
        None => out.push(
            Severity::Info,
            format!("timeout_ms is not set — the default of {DEFAULT_TIMEOUT_MS} ms (30s) applies"),
        ),
        Some(0) => out.push(
            Severity::Info,
            format!(
                "timeout_ms = 0 is treated as unset — the default of {DEFAULT_TIMEOUT_MS} ms \
                 (30s) applies"
            ),
        ),
        Some(t) if t > MAX_TIMEOUT_MS => out.push(
            Severity::Warning,
            format!(
                "timeout_ms = {t} exceeds the hard cap of {MAX_TIMEOUT_MS} ms (10 minutes) — it \
                 will be clamped to {MAX_TIMEOUT_MS} ms at runtime"
            ),
        ),
        Some(_) => {}
    }

    // command[0] resolvability. A warning, not an error: the script may be
    // provisioned later (generated, installed, built) before the tool runs.
    if let Some(message) = command_issue(&tool.command[0], workspace_root) {
        out.push(Severity::Warning, message);
    }

    out
}

/// Check whether `cmd0` would spawn, mirroring [`crate::custom`]'s runtime
/// semantics: the child runs with the **workspace root** as cwd, so a path
/// containing `/` resolves against it, while a bare name goes through `$PATH`.
/// Returns a printable finding, or `None` when the command looks runnable.
fn command_issue(cmd0: &str, workspace_root: &Path) -> Option<String> {
    if cmd0.contains('/') {
        let resolved = if Path::new(cmd0).is_absolute() {
            PathBuf::from(cmd0)
        } else {
            workspace_root.join(cmd0)
        };
        if !resolved.exists() {
            return Some(format!(
                "command[0] `{cmd0}` does not exist (resolves to {} — custom tools run from the \
                 workspace root); the tool will fail to spawn unless it is provisioned before use",
                resolved.display()
            ));
        }
        if resolved.is_dir() {
            return Some(format!(
                "command[0] `{cmd0}` is a directory, not an executable ({})",
                resolved.display()
            ));
        }
        if !is_executable(&resolved) {
            return Some(format!(
                "command[0] `{cmd0}` is not executable — try `chmod +x {}`",
                resolved.display()
            ));
        }
        None
    } else if find_on_path(cmd0) {
        None
    } else {
        Some(format!(
            "command[0] `{cmd0}` was not found on PATH; the tool will fail to spawn unless it \
             is installed before use"
        ))
    }
}

/// `true` iff `path` has any execute bit set (owner/group/other). On
/// non-unix targets there is no execute bit to inspect, so existence wins.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// `true` iff a bare command name resolves to an executable file via `$PATH`
/// (the lookup the OS would do at spawn time).
fn find_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        if dir.as_os_str().is_empty() {
            return false;
        }
        let candidate = dir.join(name);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

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

    /// Drop an executable script at `root/name` so command[0] checks pass.
    fn write_script(root: &Path, name: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }

    fn issues_with(report: &ManifestReport, severity: Severity) -> Vec<&str> {
        report
            .issues
            .iter()
            .filter(|i| i.severity == severity)
            .map(|i| i.message.as_str())
            .collect()
    }

    // ---- witness: clean pass ----------------------------------------------

    #[test]
    fn valid_manifest_passes_clean() {
        let ws = tempfile::tempdir().unwrap();
        write_script(ws.path(), "scripts/ok.sh");
        write_manifest(
            &ws_tools(ws.path()),
            "ok.toml",
            "name = \"ok_tool\"\ndescription = \"d\"\ncommand = [\"./scripts/ok.sh\"]\n\
             timeout_ms = 60000",
        );

        let report = validate_default_in(ws.path(), None);
        assert_eq!(report.manifests.len(), 1);
        let m = &report.manifests[0];
        assert_eq!(m.name.as_deref(), Some("ok_tool"));
        assert!(m.issues.is_empty(), "expected no findings: {:?}", m.issues);
        assert!(!report.has_errors());
        assert_eq!(report.error_count(), 0);
        assert_eq!(report.warning_count(), 0);
    }

    // ---- witness: malformed TOML -------------------------------------------

    #[test]
    fn malformed_toml_is_an_error_with_a_precise_message() {
        let ws = tempfile::tempdir().unwrap();
        write_manifest(&ws_tools(ws.path()), "bad.toml", "this is not = toml [[[");

        let report = validate_default_in(ws.path(), None);
        assert_eq!(report.manifests.len(), 1);
        let m = &report.manifests[0];
        assert!(m.path.ends_with("bad.toml"));
        assert!(m.has_errors());
        let errors = issues_with(m, Severity::Error);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("invalid TOML"), "got: {}", errors[0]);
    }

    #[test]
    fn missing_required_field_reports_the_field_and_keeps_the_tool_name() {
        let ws = tempfile::tempdir().unwrap();
        // Valid TOML, but no `description` — parse_manifest rejects it while
        // the lenient pre-parse can still recover the intended name.
        write_manifest(
            &ws_tools(ws.path()),
            "nodesc.toml",
            "name = \"my_tool\"\ncommand = [\"./x.sh\"]",
        );

        let report = validate_default_in(ws.path(), None);
        let m = &report.manifests[0];
        assert_eq!(m.name.as_deref(), Some("my_tool"));
        assert!(m.has_errors());
        assert!(
            issues_with(m, Severity::Error)[0].contains("description"),
            "{:?}",
            m.issues
        );
    }

    // ---- witness: reserved name ---------------------------------------------

    #[test]
    fn reserved_name_is_an_error() {
        let ws = tempfile::tempdir().unwrap();
        write_manifest(
            &ws_tools(ws.path()),
            "bash.toml",
            "name = \"bash\"\ndescription = \"d\"\ncommand = [\"/bin/true\"]",
        );

        let report = validate_default_in(ws.path(), None);
        let m = &report.manifests[0];
        assert!(m.has_errors());
        assert!(
            issues_with(m, Severity::Error)[0].contains("reserved"),
            "{:?}",
            m.issues
        );
    }

    // ---- witness: duplicate names -------------------------------------------

    #[test]
    fn same_dir_duplicate_is_a_warning_on_the_losing_manifest() {
        let ws = tempfile::tempdir().unwrap();
        write_script(ws.path(), "a.sh");
        let dir = ws_tools(ws.path());
        // Files scan in sorted order: 01_first.toml wins, 02_second.toml loses.
        write_manifest(
            &dir,
            "01_first.toml",
            "name = \"dup\"\ndescription = \"d\"\ncommand = [\"./a.sh\"]\ntimeout_ms = 1000",
        );
        write_manifest(
            &dir,
            "02_second.toml",
            "name = \"dup\"\ndescription = \"d\"\ncommand = [\"./a.sh\"]\ntimeout_ms = 1000",
        );

        let report = validate_default_in(ws.path(), None);
        assert_eq!(report.manifests.len(), 2);
        assert!(report.manifests[0].issues.is_empty());
        let loser = &report.manifests[1];
        assert!(!loser.has_errors(), "duplicate is a warning, not an error");
        let warnings = issues_with(loser, Severity::Warning);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("already defined"), "{}", warnings[0]);
        assert!(warnings[0].contains("same directory"), "{}", warnings[0]);
        assert!(warnings[0].contains("01_first.toml"), "{}", warnings[0]);
    }

    #[test]
    fn workspace_shadowing_a_global_manifest_is_a_warning_on_the_global_one() {
        let ws = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_script(ws.path(), "w.sh");
        write_manifest(
            &ws_tools(ws.path()),
            "dup.toml",
            "name = \"dup\"\ndescription = \"ws\"\ncommand = [\"./w.sh\"]\ntimeout_ms = 1000",
        );
        write_manifest(
            &global_tools(home.path()),
            "dup.toml",
            "name = \"dup\"\ndescription = \"global\"\ncommand = [\"./w.sh\"]\ntimeout_ms = 1000",
        );

        let report = validate_default_in(ws.path(), Some(home.path()));
        assert_eq!(report.manifests.len(), 2);
        // Workspace manifest scanned first — it wins, no findings.
        assert!(report.manifests[0].path.starts_with(ws.path()));
        assert!(report.manifests[0].issues.is_empty());
        // Global manifest is shadowed.
        let global = &report.manifests[1];
        assert!(global.path.starts_with(home.path()));
        let warnings = issues_with(global, Severity::Warning);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("shadows"), "{}", warnings[0]);
    }

    // ---- witness: timeout normalization ---------------------------------------

    #[test]
    fn over_cap_timeout_warns_about_clamping() {
        let ws = tempfile::tempdir().unwrap();
        write_script(ws.path(), "a.sh");
        write_manifest(
            &ws_tools(ws.path()),
            "slow.toml",
            "name = \"slow_tool\"\ndescription = \"d\"\ncommand = [\"./a.sh\"]\n\
             timeout_ms = 99999999",
        );

        let report = validate_default_in(ws.path(), None);
        let m = &report.manifests[0];
        assert!(!m.has_errors(), "clamp is a warning: {:?}", m.issues);
        let warnings = issues_with(m, Severity::Warning);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("clamped"), "{}", warnings[0]);
        assert!(warnings[0].contains("600000"), "{}", warnings[0]);
    }

    #[test]
    fn omitted_and_zero_timeouts_are_info_about_the_default() {
        let ws = tempfile::tempdir().unwrap();
        write_script(ws.path(), "a.sh");
        let dir = ws_tools(ws.path());
        write_manifest(
            &dir,
            "omitted.toml",
            "name = \"om_tool\"\ndescription = \"d\"\ncommand = [\"./a.sh\"]",
        );
        write_manifest(
            &dir,
            "zero.toml",
            "name = \"zero_tool\"\ndescription = \"d\"\ncommand = [\"./a.sh\"]\ntimeout_ms = 0",
        );

        let report = validate_default_in(ws.path(), None);
        assert!(!report.has_errors());
        assert_eq!(report.warning_count(), 0);
        for m in &report.manifests {
            let infos = issues_with(m, Severity::Info);
            assert_eq!(infos.len(), 1, "{:?}", m.issues);
            assert!(infos[0].contains("30"), "{}", infos[0]);
        }
    }

    // ---- witness: command[0] checks --------------------------------------------

    #[test]
    fn missing_relative_command_is_a_warning_naming_the_resolved_path() {
        let ws = tempfile::tempdir().unwrap();
        write_manifest(
            &ws_tools(ws.path()),
            "ghost.toml",
            "name = \"ghost_tool\"\ndescription = \"d\"\ncommand = [\"./no/such.sh\"]\n\
             timeout_ms = 1000",
        );

        let report = validate_default_in(ws.path(), None);
        let m = &report.manifests[0];
        assert!(!m.has_errors(), "missing script is a warning, not an error");
        let warnings = issues_with(m, Severity::Warning);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("./no/such.sh"), "{}", warnings[0]);
        assert!(warnings[0].contains("does not exist"), "{}", warnings[0]);
        assert!(
            warnings[0].contains(&ws.path().display().to_string()),
            "resolved path should name the workspace root: {}",
            warnings[0]
        );
    }

    #[test]
    fn non_executable_command_is_a_warning() {
        let ws = tempfile::tempdir().unwrap();
        let script = ws.path().join("noexec.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o644); // rw-, no execute bit
        std::fs::set_permissions(&script, perms).unwrap();
        write_manifest(
            &ws_tools(ws.path()),
            "noexec.toml",
            "name = \"noexec_tool\"\ndescription = \"d\"\ncommand = [\"./noexec.sh\"]\n\
             timeout_ms = 1000",
        );

        let report = validate_default_in(ws.path(), None);
        let warnings = issues_with(&report.manifests[0], Severity::Warning);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not executable"), "{}", warnings[0]);
    }

    #[test]
    fn bare_command_on_path_passes_and_unknown_bare_command_warns() {
        let ws = tempfile::tempdir().unwrap();
        let dir = ws_tools(ws.path());
        write_manifest(
            &dir,
            "onpath.toml",
            "name = \"onpath_tool\"\ndescription = \"d\"\ncommand = [\"sh\", \"-c\", \"true\"]\n\
             timeout_ms = 1000",
        );
        write_manifest(
            &dir,
            "offpath.toml",
            "name = \"offpath_tool\"\ndescription = \"d\"\n\
             command = [\"stella_no_such_binary_xyz\"]\ntimeout_ms = 1000",
        );

        let report = validate_default_in(ws.path(), None);
        assert_eq!(report.manifests.len(), 2);
        let by_name = |n: &str| {
            report
                .manifests
                .iter()
                .find(|m| m.name.as_deref() == Some(n))
                .unwrap()
        };
        let off = by_name("offpath_tool");
        let warnings = issues_with(off, Severity::Warning);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not found on PATH"), "{}", warnings[0]);
        assert!(issues_with(by_name("onpath_tool"), Severity::Warning).is_empty());
    }

    // ---- shape of the report ----------------------------------------------------

    #[test]
    fn absent_directories_yield_an_empty_report() {
        let ws = tempfile::tempdir().unwrap();
        let report = validate_default_in(ws.path(), None);
        assert!(report.manifests.is_empty());
        assert!(!report.has_errors());

        let report = validate_dir(&ws.path().join("nope"), ws.path());
        assert!(report.manifests.is_empty());
    }

    #[test]
    fn validate_dir_checks_an_explicit_directory_against_the_workspace_root() {
        let ws = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        write_script(ws.path(), "run.sh");
        write_manifest(
            elsewhere.path(),
            "tool.toml",
            "name = \"else_tool\"\ndescription = \"d\"\ncommand = [\"./run.sh\"]\n\
             timeout_ms = 1000",
        );

        // command[0] resolves against the workspace root, not the manifest dir.
        let report = validate_dir(elsewhere.path(), ws.path());
        assert_eq!(report.manifests.len(), 1);
        assert!(
            report.manifests[0].issues.is_empty(),
            "{:?}",
            report.manifests[0].issues
        );
    }

    #[test]
    fn mixed_directory_summarizes_errors_and_warnings() {
        let ws = tempfile::tempdir().unwrap();
        write_script(ws.path(), "a.sh");
        let dir = ws_tools(ws.path());
        write_manifest(
            &dir,
            "good.toml",
            "name = \"good_tool\"\ndescription = \"d\"\ncommand = [\"./a.sh\"]\ntimeout_ms = 1000",
        );
        write_manifest(&dir, "broken.toml", "not toml at all [[[");
        write_manifest(
            &dir,
            "shadow.toml",
            "name = \"good_tool\"\ndescription = \"d\"\ncommand = [\"./a.sh\"]\ntimeout_ms = 1000",
        );

        let report = validate_default_in(ws.path(), None);
        assert_eq!(report.manifests.len(), 3);
        assert!(report.has_errors());
        assert_eq!(report.error_count(), 1);
        assert_eq!(report.warning_count(), 1);
        let failed = report.manifests.iter().filter(|m| m.has_errors()).count();
        assert_eq!(failed, 1);
    }
}
