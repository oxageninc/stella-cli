//! CLI argument-surface tests: the everything-global invariant on
//! [`GlobalArgs`](super::GlobalArgs) and the parse positions it exists to
//! guarantee. These are the regression fence for "`--budget` is an
//! unexpected argument after `stella fleet`" — the flag was defined at the
//! root without `global = true`, so clap only accepted it *before* the
//! subcommand while README and muscle memory put it after.

use clap::{CommandFactory, Parser};

use super::{AuthCmd, Cli, Command, ConnectCmd, OutputFormat, TelemetryCmd};

#[cfg(unix)]
fn legacy_codegraph_workspace(mode: u32) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let dot = dir.path().join(".stella");
    std::fs::create_dir_all(&dot).unwrap();
    std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(mode)).unwrap();
    let graph = stella_graph::CodeGraph::open(dir.path(), &dot.join("codegraph.db")).unwrap();
    graph.shutdown();
    dir
}

#[cfg(unix)]
#[test]
fn storage_snapshot_preflight_migrates_safe_legacy_codegraph() {
    let dir = legacy_codegraph_workspace(0o700);
    super::load_storage_snapshot_checked(dir.path()).unwrap();
    assert!(!dir.path().join(".stella/codegraph.db").exists());
    assert!(dir.path().join(".stella/private/codegraph.db").exists());
}

#[cfg(unix)]
#[test]
fn storage_snapshot_preflight_reports_unsafe_legacy_codegraph() {
    let dir = legacy_codegraph_workspace(0o777);
    let error = super::load_storage_snapshot_checked(dir.path())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("legacy") && error.contains("private"),
        "{error}"
    );
    assert!(dir.path().join(".stella/codegraph.db").exists());
}

#[cfg(unix)]
#[test]
fn observatory_preflight_migrates_safe_legacy_sqlite_stores() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let dot = dir.path().join(".stella");
    std::fs::create_dir_all(&dot).unwrap();
    std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["store.db", "fleet.db", "context.db", "codegraph.db"] {
        std::fs::write(dot.join(name), b"closed legacy sqlite file").unwrap();
    }

    super::preflight_observatory_stores(dir.path()).unwrap();
    for name in ["store.db", "fleet.db", "context.db", "codegraph.db"] {
        assert!(!dot.join(name).exists());
        assert!(dot.join("private").join(name).exists());
    }
}

#[cfg(unix)]
#[test]
fn observatory_preflight_reports_unsafe_legacy_store() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let dot = dir.path().join(".stella");
    std::fs::create_dir_all(&dot).unwrap();
    std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o777)).unwrap();
    std::fs::write(dot.join("store.db"), b"unsafe legacy sqlite file").unwrap();

    let error = super::preflight_observatory_stores(dir.path()).unwrap_err();
    assert!(
        error.contains("legacy") && error.contains("private"),
        "{error}"
    );
    assert!(dot.join("store.db").exists());
}

/// clap's own consistency audit (conflicting ids, broken defaults,
/// mis-typed value parsers) — panics on the first violation. Runs the same
/// checks release builds skip, so a bad arg table fails here instead of at
/// a user's first `--help`.
#[test]
fn clap_command_is_internally_consistent() {
    Cli::command().debug_assert();
}

#[test]
fn telemetry_status_remains_a_distinct_top_level_command() {
    let cli = Cli::try_parse_from(["stella", "telemetry", "status"])
        .expect("`telemetry status` must parse independently of adjacent auth commands");
    assert!(matches!(
        cli.command,
        Some(Command::Telemetry {
            cmd: TelemetryCmd::Status
        })
    ));
}

/// The load-bearing invariant: every flag defined at the root MUST be
/// `global = true`, or it is silently unaccepted after a subcommand token
/// (`stella fleet … --budget 5` → "unexpected argument"). Introspects the
/// built command rather than the source so any future root flag — added to
/// `GlobalArgs` or straight onto `Cli` — is covered without editing this
/// test. `help`/`version` are clap built-ins with their own propagation.
#[test]
fn every_root_flag_is_global() {
    let cmd = Cli::command();
    for arg in cmd.get_arguments() {
        let id = arg.get_id().as_str();
        if id == "help" || id == "version" {
            continue;
        }
        assert!(
            arg.is_global_set(),
            "root-level flag `--{id}` is not `global = true`: it will be rejected \
             when typed after a subcommand (e.g. `stella fleet … --{id}`). Add \
             `global = true` to its #[arg] in GlobalArgs (see the struct doc)."
        );
    }
}

/// The exact invocation shape README advertises for fleets — session flags
/// after the subcommand — must parse, and the values must land at the root.
#[test]
fn session_flags_parse_after_subcommand() {
    let cli = Cli::try_parse_from([
        "stella",
        "fleet",
        "fix the flaky auth test",
        "--max-concurrency",
        "2",
        "--budget",
        "5.0",
        "--model",
        "zai/glm-5.2",
    ])
    .expect("session flags after `fleet` must parse");
    assert_eq!(cli.globals.budget, Some(5.0));
    assert_eq!(cli.globals.model.as_deref(), Some("zai/glm-5.2"));
    match cli.command {
        Some(Command::Fleet {
            tasks,
            max_concurrency,
            ..
        }) => {
            assert_eq!(tasks, vec!["fix the flaky auth test".to_string()]);
            assert_eq!(max_concurrency, 2);
        }
        _ => panic!("expected the fleet subcommand"),
    }
}

/// The pre-subcommand position keeps working — global args are valid in
/// both positions, not moved.
#[test]
fn session_flags_parse_before_subcommand() {
    let cli = Cli::try_parse_from([
        "stella",
        "--budget",
        "2.5",
        "--output-format",
        "json",
        "run",
        "hi",
    ])
    .expect("session flags before the subcommand must keep parsing");
    assert_eq!(cli.globals.budget, Some(2.5));
    assert_eq!(cli.globals.output_format, OutputFormat::Json);
}

/// `--budget`'s value parser still guards after a subcommand: a
/// non-positive limit would silently disarm the hard spend cap, so it must
/// be a parse error in every position.
#[test]
fn budget_validation_applies_after_subcommand() {
    let err = Cli::try_parse_from(["stella", "fleet", "task", "--budget", "0"])
        .err()
        .expect("a zero budget must be rejected");
    assert!(
        err.to_string().contains("positive dollar amount"),
        "unexpected error: {err}"
    );
}

/// Global names are reserved CLI-wide. A subcommand flag with the same id
/// as a root global does NOT shadow it cleanly: clap propagates the
/// global's value slot into every subcommand, and the collision panics at
/// match-downcast time in debug builds and misbinds in release —
/// `debug_assert` does not catch it (this is exactly how `connect linear`'s
/// old `--api-key` bool blew up against the global credential string).
/// Walk every subcommand recursively so a reintroduction anywhere fails
/// here, not in a user's terminal.
#[test]
fn no_subcommand_flag_reuses_a_global_name() {
    fn check(cmd: &clap::Command, globals: &[String], path: &str) {
        for sub in cmd.get_subcommands() {
            let sub_path = format!("{path} {}", sub.get_name());
            for arg in sub.get_arguments() {
                let id = arg.get_id().as_str();
                // Propagated copies of the globals themselves are fine —
                // only a *local* redefinition collides.
                if !arg.is_global_set() && globals.iter().any(|g| g == id) {
                    panic!(
                        "`{sub_path}` defines its own `--{id}`, colliding with the \
                         session-wide global of that name — pick a different flag \
                         name (see the GlobalArgs doc)."
                    );
                }
            }
            check(sub, globals, &sub_path);
        }
    }
    let mut cmd = Cli::command();
    cmd.build();
    let globals: Vec<String> = cmd
        .get_arguments()
        .filter(|a| a.is_global_set())
        .map(|a| a.get_id().to_string())
        .collect();
    assert!(!globals.is_empty(), "expected at least one global flag");
    check(&cmd, &globals, "stella");
}

/// `connect linear --paste-key` (the rename that resolved the collision
/// above) binds to the subcommand's bool, and the session-wide `--api-key`
/// stays independently usable on the same command line.
#[test]
fn connect_linear_paste_key_parses() {
    let cli = Cli::try_parse_from(["stella", "connect", "linear", "--paste-key"])
        .expect("`connect linear --paste-key` must parse");
    assert_eq!(cli.globals.api_key, None);
    match cli.command {
        Some(Command::Connect {
            cmd: ConnectCmd::Linear { paste_key },
        }) => assert!(paste_key),
        _ => panic!("expected `connect linear`"),
    }

    let cli = Cli::try_parse_from(["stella", "connect", "linear", "--api-key", "sk-model"])
        .expect("the global --api-key must parse after `connect linear`");
    assert_eq!(cli.globals.api_key.as_deref(), Some("sk-model"));
    match cli.command {
        Some(Command::Connect {
            cmd: ConnectCmd::Linear { paste_key },
        }) => assert!(!paste_key),
        _ => panic!("expected `connect linear`"),
    }
}

/// `stella auth set <provider> --key <k>` parses, and the global `--api-key`
/// (a different secret — the model-provider credential) still parses
/// independently on the same command line, same shape as `connect linear`.
#[test]
fn auth_set_key_parses_and_stays_independent_of_the_global_api_key() {
    let cli = Cli::try_parse_from([
        "stella",
        "auth",
        "set",
        "zai",
        "--key",
        "sk-stored",
        "--api-key",
        "sk-model",
    ])
    .expect("`auth set <provider> --key <k>` must parse alongside the global --api-key");
    assert_eq!(cli.globals.api_key.as_deref(), Some("sk-model"));
    match cli.command {
        Some(Command::Auth {
            cmd:
                AuthCmd::Set {
                    provider,
                    key,
                    stdin,
                },
        }) => {
            assert_eq!(provider, "zai");
            assert_eq!(key.as_deref(), Some("sk-stored"));
            assert!(!stdin);
        }
        _ => panic!("expected `auth set`"),
    }
}

/// `--key` and `--stdin` are mutually exclusive — a user meaning to pipe a
/// secret in via `--stdin` must not silently also accept a (likely absent)
/// `--key` value.
#[test]
fn auth_set_rejects_key_and_stdin_together() {
    // `Cli` derives no `Debug` (it carries the model API key), so match
    // rather than `expect_err`/`unwrap_err` (both require `T: Debug`).
    match Cli::try_parse_from(["stella", "auth", "set", "zai", "--key", "sk-x", "--stdin"]) {
        Err(e) => assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict),
        Ok(_) => panic!("--key and --stdin must conflict"),
    }
}

#[test]
fn auth_remove_and_list_parse() {
    let cli = Cli::try_parse_from(["stella", "auth", "remove", "zai"])
        .expect("`auth remove <provider>` must parse");
    match cli.command {
        Some(Command::Auth {
            cmd: AuthCmd::Remove { provider },
        }) => assert_eq!(provider, "zai"),
        _ => panic!("expected `auth remove`"),
    }

    let cli = Cli::try_parse_from(["stella", "auth", "list"]).expect("`auth list` must parse");
    assert!(matches!(
        cli.command,
        Some(Command::Auth { cmd: AuthCmd::List })
    ));
}
