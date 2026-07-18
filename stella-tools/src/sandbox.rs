//! Opt-in OS-level sandboxing for the `bash` tool, gated on the
//! `STELLA_BASH_SANDBOX` environment variable.
//!
//! Modes:
//! - `off` (default, also unset/empty) — exactly today's behavior: plain
//!   `bash -c <command>`, no confinement.
//! - `workspace-write` — file writes are confined to the workspace root plus
//!   the standard tmp directories; reads and network are unrestricted.
//! - `restricted` — `workspace-write` plus a blanket network denial
//!   (including localhost, by design).
//!
//! Backends:
//! - **macOS**: `sandbox-exec` with a generated Seatbelt (SBPL) profile
//!   passed inline via `-p`. Apple has carried a deprecation notice on
//!   `sandbox-exec` for years, but it remains present and functional on
//!   every shipping macOS — it fronts the same Seatbelt engine App Sandbox
//!   uses, and other agent CLIs rely on it the same way.
//! - **Linux**: `bwrap` (bubblewrap) with a read-only bind of `/`, writable
//!   binds for the workspace root and `/tmp`, and `--unshare-net` under
//!   `restricted`. If `bwrap` is not on `PATH`, the tool call fails.
//!
//! Fail-closed: a requested sandbox that cannot be applied (unknown mode,
//! missing `bwrap`, unsupported platform) fails the tool call with an
//! instructive error. Silently degrading to unsandboxed execution would be
//! worse than an error — the operator asked for confinement and would not
//! get it.
//!
//! The tradeoff, and why this is opt-in: the sandbox blocks legitimate work
//! too — `cargo` writing `~/.cargo`, `npm`/`pip` caches under `$HOME`,
//! `git push` under `restricted` — so the default stays `off` and the
//! operator chooses per environment. The threat it mitigates is prompt
//! injection: instructions embedded in a file the agent reads can steer the
//! model into running arbitrary shell commands, and the sandbox bounds the
//! blast radius of that command to the workspace.
//!
//! Argv and profile construction here are pure functions (mode + workspace
//! root + platform in, argv/SBPL out) so they are unit-testable without
//! spawning anything; the actual spawn stays in `bash.rs`.

use std::fmt;
use std::path::Path;

/// Sandbox strength requested via `STELLA_BASH_SANDBOX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// No confinement — plain `bash -c` (today's default behavior).
    Off,
    /// Writes confined to workspace root + tmp dirs; network still allowed.
    WorkspaceWrite,
    /// `WorkspaceWrite` plus deny all network, including localhost.
    Restricted,
}

impl SandboxMode {
    /// Parse the raw `STELLA_BASH_SANDBOX` value. Unset and empty mean `Off`
    /// (the variable simply isn't in play). A set-but-unrecognized value is
    /// an error, not `Off`: the operator asked for *some* sandbox, so
    /// guessing — or worse, running unsandboxed — would betray that intent.
    pub fn from_env_value(value: Option<&str>) -> Result<Self, SandboxError> {
        let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(Self::Off);
        };
        match value.to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "restricted" => Ok(Self::Restricted),
            _ => Err(SandboxError::UnknownMode(value.to_string())),
        }
    }
}

impl fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SandboxMode::Off => "off",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::Restricted => "restricted",
        })
    }
}

/// Why a requested sandbox could not be applied. Every variant is a
/// fail-closed condition: the command was **not** run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// `STELLA_BASH_SANDBOX` is set to something unrecognized.
    UnknownMode(String),
    /// Linux sandbox requested but `bwrap` is not an executable on `PATH`.
    BwrapMissing { mode: SandboxMode },
    /// Sandbox requested on a platform with no supported backend.
    UnsupportedPlatform { mode: SandboxMode, os: &'static str },
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SandboxError::UnknownMode(value) => write!(
                f,
                "STELLA_BASH_SANDBOX=\"{value}\" is not a recognized sandbox mode — \
                 expected \"off\", \"workspace-write\", or \"restricted\". \
                 Refusing to run the command unsandboxed."
            ),
            SandboxError::BwrapMissing { mode } => write!(
                f,
                "STELLA_BASH_SANDBOX={mode} requires bwrap (bubblewrap) on PATH, and it \
                 was not found — install it (e.g. `apt install bubblewrap` / \
                 `dnf install bubblewrap`) or unset STELLA_BASH_SANDBOX. \
                 Refusing to run the command unsandboxed."
            ),
            SandboxError::UnsupportedPlatform { mode, os } => write!(
                f,
                "STELLA_BASH_SANDBOX={mode} has no sandbox backend on this platform \
                 ({os}) — supported: macos (sandbox-exec), linux (bwrap). Unset \
                 STELLA_BASH_SANDBOX to run unsandboxed. \
                 Refusing to run the command unsandboxed."
            ),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Host platform, as a value so argv construction stays a pure function of
/// its inputs (testable for every platform from any platform).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Other(&'static str),
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other(std::env::consts::OS)
        }
    }
}

/// Build the full spawn argv — `(program, args)` — for `command` under
/// `mode`. Pure: no environment reads, no filesystem probes, no spawning;
/// platform and `bwrap` availability arrive as parameters.
///
/// `workspace_root` must already be canonicalized by the caller (see
/// [`host_argv`]): Seatbelt subpath rules match symlink-resolved vnode
/// paths, so an uncanonicalized `/tmp/...` root would produce a macOS
/// profile that denies writes to the workspace itself.
pub fn build_argv(
    mode: SandboxMode,
    workspace_root: &Path,
    os: Os,
    bwrap_on_path: bool,
    command: &str,
) -> Result<(String, Vec<String>), SandboxError> {
    match (mode, os) {
        (SandboxMode::Off, _) => Ok((
            "bash".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )),
        (_, Os::MacOs) => Ok((
            "sandbox-exec".to_string(),
            vec![
                "-p".to_string(),
                sbpl_profile(mode, workspace_root),
                "bash".to_string(),
                "-c".to_string(),
                command.to_string(),
            ],
        )),
        (_, Os::Linux) => {
            if !bwrap_on_path {
                return Err(SandboxError::BwrapMissing { mode });
            }
            let root = workspace_root.to_string_lossy().into_owned();
            let mut args: Vec<String> = [
                // Everything read-only by default; later binds overlay
                // earlier ones, re-opening exactly the writable surface.
                "--ro-bind",
                "/",
                "/",
                // Fresh minimal /dev (null, zero, tty, random, …) and /proc
                // for the new mount namespace.
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--bind",
                &root,
                &root,
                "--bind",
                "/tmp",
                "/tmp",
                // The sandboxed tree must not outlive Stella — mirrors the
                // process-group kill in bash.rs.
                "--die-with-parent",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            if mode == SandboxMode::Restricted {
                args.push("--unshare-net".to_string());
            }
            args.extend(["--", "bash", "-c", command].iter().map(|s| s.to_string()));
            Ok(("bwrap".to_string(), args))
        }
        (_, Os::Other(os)) => Err(SandboxError::UnsupportedPlatform { mode, os }),
    }
}

/// Generate the Seatbelt (SBPL) profile for a sandboxed mode. Default-allow
/// (reads, exec, fork keep working), then deny all file writes, then
/// re-allow the workspace root, the standard macOS tmp locations, and the
/// device nodes shells and build tools open for writing — later SBPL rules
/// override earlier ones. `Restricted` appends a blanket network denial.
///
/// Only meaningful for `WorkspaceWrite`/`Restricted`; [`build_argv`] never
/// consults it for `Off`.
pub fn sbpl_profile(mode: SandboxMode, workspace_root: &Path) -> String {
    // Both symlink (/tmp, /var/…) and resolved (/private/…) forms: Seatbelt
    // matches resolved vnode paths, but keeping both is harmless and covers
    // callers that hand paths through either spelling.
    const TMP_SUBPATHS: [&str; 6] = [
        "/tmp",
        "/private/tmp",
        "/var/tmp",
        "/private/var/tmp",
        "/var/folders",
        "/private/var/folders",
    ];
    let root = sbpl_quote(&workspace_root.to_string_lossy());
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    profile.push_str("(allow file-write*\n");
    profile.push_str(&format!("  (subpath \"{root}\")\n"));
    for tmp in TMP_SUBPATHS {
        profile.push_str(&format!("  (subpath \"{tmp}\")\n"));
    }
    profile.push_str("  (literal \"/dev/null\")\n");
    profile.push_str("  (literal \"/dev/zero\")\n");
    // dtrace shim — opened for writing by many tools on process start.
    profile.push_str("  (literal \"/dev/dtracehelper\")\n");
    // Terminal device nodes — bash job control opens /dev/tty read-write.
    profile.push_str("  (regex #\"^/dev/tty\")\n");
    profile.push_str(")\n");
    if mode == SandboxMode::Restricted {
        profile.push_str("(deny network*)\n");
    }
    profile
}

/// Escape a path for embedding inside a double-quoted SBPL string literal.
/// Backslashes and quotes are the only metacharacters in that position.
fn sbpl_quote(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Whether `PATH` (passed in, so the miss path is unit-testable) contains an
/// executable named `bwrap`.
pub fn bwrap_on_path(path_var: Option<&std::ffi::OsStr>) -> bool {
    let Some(path_var) = path_var else {
        return false;
    };
    std::env::split_paths(path_var).any(|dir| is_executable(&dir.join("bwrap")))
}

/// A regular file with any execute bit set (on non-unix, existence suffices —
/// the bwrap backend is only ever selected on Linux anyway).
fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Assemble the spawn argv for the current host: canonicalize the workspace
/// root (Seatbelt matches symlink-resolved paths), detect the platform, and
/// probe `PATH` for `bwrap` — then delegate to the pure [`build_argv`].
/// For `Off` this returns plain `bash -c <command>` with no probing, so the
/// default path is byte-identical to the pre-sandbox behavior.
pub fn host_argv(
    mode: SandboxMode,
    workspace_root: &Path,
    command: &str,
) -> Result<(String, Vec<String>), SandboxError> {
    if mode == SandboxMode::Off {
        return build_argv(mode, workspace_root, Os::current(), false, command);
    }
    // Fall back to the raw path if canonicalization fails — the spawn itself
    // will then surface the real filesystem error.
    let canon = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let os = Os::current();
    let bwrap = os == Os::Linux && bwrap_on_path(std::env::var_os("PATH").as_deref());
    build_argv(mode, &canon, os, bwrap, command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ws() -> PathBuf {
        PathBuf::from("/work/space")
    }

    // -- mode parsing ------------------------------------------------------

    #[test]
    fn unset_empty_and_off_parse_as_off() {
        assert_eq!(SandboxMode::from_env_value(None).unwrap(), SandboxMode::Off);
        assert_eq!(
            SandboxMode::from_env_value(Some("")).unwrap(),
            SandboxMode::Off
        );
        assert_eq!(
            SandboxMode::from_env_value(Some("off")).unwrap(),
            SandboxMode::Off
        );
    }

    #[test]
    fn known_modes_parse_trimmed_and_case_insensitive() {
        assert_eq!(
            SandboxMode::from_env_value(Some("workspace-write")).unwrap(),
            SandboxMode::WorkspaceWrite
        );
        assert_eq!(
            SandboxMode::from_env_value(Some(" Restricted ")).unwrap(),
            SandboxMode::Restricted
        );
    }

    #[test]
    fn unknown_mode_is_an_error_not_off() {
        let err = SandboxMode::from_env_value(Some("on")).unwrap_err();
        assert_eq!(err, SandboxError::UnknownMode("on".to_string()));
        let msg = err.to_string();
        // The error must teach the fix: name the variable and every valid value.
        assert!(msg.contains("STELLA_BASH_SANDBOX"), "{msg}");
        assert!(msg.contains("workspace-write"), "{msg}");
        assert!(msg.contains("restricted"), "{msg}");
        assert!(msg.contains("off"), "{msg}");
    }

    // -- off mode: default behavior unchanged ------------------------------

    #[test]
    fn off_is_plain_bash_on_every_platform() {
        for os in [Os::MacOs, Os::Linux, Os::Other("windows")] {
            let (program, args) =
                build_argv(SandboxMode::Off, &ws(), os, false, "echo hi").unwrap();
            assert_eq!(program, "bash");
            assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
        }
    }

    // -- macOS: sandbox-exec + SBPL ----------------------------------------

    #[test]
    fn macos_argv_wraps_bash_in_sandbox_exec() {
        let (program, args) = build_argv(
            SandboxMode::WorkspaceWrite,
            &ws(),
            Os::MacOs,
            false,
            "echo hi",
        )
        .unwrap();
        assert_eq!(program, "sandbox-exec");
        assert_eq!(args[0], "-p");
        assert!(args[1].starts_with("(version 1)"));
        assert_eq!(&args[2..], ["bash", "-c", "echo hi"]);
    }

    #[test]
    fn sbpl_profile_embeds_workspace_and_tmp_write_allows() {
        let profile = sbpl_profile(SandboxMode::WorkspaceWrite, &ws());
        assert!(profile.contains("(allow default)"), "{profile}");
        assert!(profile.contains("(deny file-write*)"), "{profile}");
        assert!(profile.contains("(subpath \"/work/space\")"), "{profile}");
        assert!(profile.contains("(subpath \"/private/tmp\")"), "{profile}");
        assert!(
            profile.contains("(subpath \"/private/var/folders\")"),
            "{profile}"
        );
        assert!(profile.contains("(literal \"/dev/null\")"), "{profile}");
    }

    #[test]
    fn sbpl_network_rule_present_only_under_restricted() {
        let write = sbpl_profile(SandboxMode::WorkspaceWrite, &ws());
        assert!(!write.contains("network"), "{write}");
        let restricted = sbpl_profile(SandboxMode::Restricted, &ws());
        assert!(restricted.contains("(deny network*)"), "{restricted}");
    }

    #[test]
    fn sbpl_escapes_quotes_and_backslashes_in_workspace_path() {
        let tricky = PathBuf::from("/work/we\"ird\\dir");
        let profile = sbpl_profile(SandboxMode::WorkspaceWrite, &tricky);
        assert!(
            profile.contains("(subpath \"/work/we\\\"ird\\\\dir\")"),
            "{profile}"
        );
    }

    // -- Linux: bwrap -------------------------------------------------------

    #[test]
    fn linux_workspace_write_binds_workspace_and_tmp_keeps_network() {
        let (program, args) = build_argv(
            SandboxMode::WorkspaceWrite,
            &ws(),
            Os::Linux,
            true,
            "echo hi",
        )
        .unwrap();
        assert_eq!(program, "bwrap");
        let has = |window: &[&str]| {
            args.windows(window.len())
                .any(|w| w.iter().map(String::as_str).eq(window.iter().copied()))
        };
        assert!(has(&["--ro-bind", "/", "/"]), "{args:?}");
        assert!(has(&["--bind", "/work/space", "/work/space"]), "{args:?}");
        assert!(has(&["--bind", "/tmp", "/tmp"]), "{args:?}");
        assert!(args.contains(&"--die-with-parent".to_string()), "{args:?}");
        assert!(!args.contains(&"--unshare-net".to_string()), "{args:?}");
        assert!(has(&["--", "bash", "-c", "echo hi"]), "{args:?}");
    }

    #[test]
    fn linux_restricted_unshares_network() {
        let (_, args) =
            build_argv(SandboxMode::Restricted, &ws(), Os::Linux, true, "true").unwrap();
        assert!(args.contains(&"--unshare-net".to_string()), "{args:?}");
    }

    #[test]
    fn linux_missing_bwrap_fails_closed_with_instructions() {
        let err =
            build_argv(SandboxMode::WorkspaceWrite, &ws(), Os::Linux, false, "true").unwrap_err();
        assert_eq!(
            err,
            SandboxError::BwrapMissing {
                mode: SandboxMode::WorkspaceWrite
            }
        );
        let msg = err.to_string();
        assert!(msg.contains("bubblewrap"), "{msg}");
        assert!(msg.contains("PATH"), "{msg}");
        assert!(
            msg.contains("did NOT run") || msg.contains("Refusing"),
            "{msg}"
        );
    }

    // -- other platforms ----------------------------------------------------

    #[test]
    fn unsupported_platform_fails_closed() {
        let err = build_argv(
            SandboxMode::Restricted,
            &ws(),
            Os::Other("windows"),
            false,
            "true",
        )
        .unwrap_err();
        assert_eq!(
            err,
            SandboxError::UnsupportedPlatform {
                mode: SandboxMode::Restricted,
                os: "windows"
            }
        );
        assert!(err.to_string().contains("windows"), "{err}");
    }

    // -- bwrap PATH probe ---------------------------------------------------

    #[test]
    fn bwrap_on_path_requires_an_executable_file() {
        assert!(!bwrap_on_path(None));

        let dir = tempfile::tempdir().unwrap();
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        // Empty dir: no bwrap at all.
        assert!(!bwrap_on_path(Some(&path_var)));

        // Present but not executable: still a miss on unix.
        let bwrap = dir.path().join("bwrap");
        std::fs::write(&bwrap, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bwrap, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(!bwrap_on_path(Some(&path_var)));
            std::fs::set_permissions(&bwrap, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(bwrap_on_path(Some(&path_var)));
    }
}
