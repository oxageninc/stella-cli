use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::Sha256;
use stella_protocol::ToolOutput;
use stella_store::enterprise_telemetry::StellaOperationalEventV1;
use stella_store::usage::ExecutionRollupRow;

use crate::enterprise_telemetry::{
    BatchSender, ExecutionSurface, StartupAuthoritySnapshot, activate_process_free_authority_with,
    authorize_execution_surface, authorize_execution_surface_with, build_runtime_from_managed,
    canonical_enrollment_bytes, host_spool_path, process_free_authority_active,
    prove_process_free_surface, reset_process_free_authority_for_test, validate_response_status,
    verify_managed_enrollment,
};
use crate::settings::Settings;
use crate::{Cli, Command, TelemetryCmd};
use clap::Parser;

struct EnvRestore(Vec<(String, Option<std::ffi::OsString>)>);

struct AuthorityReset;

impl AuthorityReset {
    fn new() -> Self {
        reset_process_free_authority_for_test();
        Self
    }
}

impl Drop for AuthorityReset {
    fn drop(&mut self) {
        reset_process_free_authority_for_test();
    }
}

impl EnvRestore {
    fn capture(names: &[&str]) -> Self {
        Self(
            names
                .iter()
                .map(|name| ((*name).to_string(), std::env::var_os(name)))
                .collect(),
        )
    }
}

#[test]
fn process_free_surface_enumeration_omits_every_spawn_and_extension_action() {
    use stella_core::ports::ToolExecutor;
    use stella_tools::media::HostDataIsolation;

    let dir = tempfile::tempdir().unwrap();
    prove_process_free_surface(dir.path()).unwrap();
    let registry = stella_tools::ToolRegistry::with_backends_and_options(
        dir.path().to_path_buf(),
        None,
        None,
        stella_tools::RegistryOptions {
            bash: true,
            web: false,
            media_host_data_isolation: Some(HostDataIsolation::ProcessFree),
            ..Default::default()
        },
    );
    assert!(registry.is_process_free());
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let interactive = crate::interactive::InteractiveToolSet::new(
        &registry,
        events,
        Box::new(crate::interactive::HeadlessAskUserIo),
    );
    let names: std::collections::BTreeSet<String> = interactive
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    for forbidden in [
        "bash",
        "grep",
        "glob",
        "gather_context",
        "process_start",
        "process_write",
        "process_poll",
        "test_start",
        "test_poll",
        "search_skills",
        "install_skill",
    ] {
        assert!(
            !names.contains(forbidden),
            "process action exposed: {forbidden}"
        );
    }
}

#[test]
fn process_free_authority_allows_only_the_registry_only_raw_one_shot_surface() {
    let allowed = authorize_execution_surface_with(ExecutionSurface::RawOneShot, true);
    assert!(
        allowed.is_ok(),
        "raw one-shot is the sole process-free engine path"
    );

    for surface in [
        ExecutionSurface::PipelineOneShot,
        ExecutionSurface::Goal,
        ExecutionSurface::Fleet,
        ExecutionSurface::Deck,
        ExecutionSurface::Interactive,
        ExecutionSurface::WorkspacePorts,
        ExecutionSurface::CandidateWorkspace,
    ] {
        let error = authorize_execution_surface_with(surface, true).unwrap_err();
        assert!(error.contains("enterprise telemetry process-free authority"));
        assert!(error.contains(surface.as_str()), "{surface:?}: {error}");
    }
}

#[test]
fn production_process_free_surface_matrix_enumerates_every_constructor() {
    assert_eq!(
        ExecutionSurface::ALL,
        [
            ExecutionSurface::RawOneShot,
            ExecutionSurface::PipelineOneShot,
            ExecutionSurface::Goal,
            ExecutionSurface::Fleet,
            ExecutionSurface::Deck,
            ExecutionSurface::Interactive,
            ExecutionSurface::WorkspacePorts,
            ExecutionSurface::CandidateWorkspace,
        ]
    );
    for surface in ExecutionSurface::ALL {
        assert!(authorize_execution_surface_with(surface, false).is_ok());
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        unsafe {
            for (name, value) in self.0.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

#[derive(Clone, Serialize)]
struct TestClaims {
    schema: &'static str,
    issuer: &'static str,
    audience: &'static str,
    enrollment_id: &'static str,
    organization_id: &'static str,
    workspace_id: &'static str,
    endpoint: &'static str,
    credential_env: &'static str,
    event_classes: Vec<&'static str>,
    host_data_isolation: &'static str,
    model_catalog: Vec<TestModelDimension>,
    issued_at_unix_s: i64,
    expires_at_unix_s: i64,
}

#[derive(Clone, Serialize)]
struct TestModelDimension {
    provider: &'static str,
    model: &'static str,
}

fn signed_managed(secret_env: &str, secret: &[u8], claims: TestClaims) -> Value {
    let claims_value = serde_json::to_value(&claims).unwrap();
    let bytes = canonical_enrollment_bytes(&claims_value).unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
    mac.update(&bytes);
    let signature_hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    json!({
        "verification_secret_env": secret_env,
        "allowed_issuers": ["oxagen-enterprise"],
        "allowed_audiences": ["stella-cli"],
        "allowed_endpoints": ["https://telemetry.oxagen.test/v1/events"],
        "host_data_isolation": "process_free",
        "enrollment": {
            "claims": claims,
            "signature_hex": signature_hex
        }
    })
}

fn valid_claims() -> TestClaims {
    TestClaims {
        schema: "stella.enterprise.telemetry.enrollment.v1",
        issuer: "oxagen-enterprise",
        audience: "stella-cli",
        enrollment_id: "enroll_01",
        organization_id: "org_01",
        workspace_id: "workspace_01",
        endpoint: "https://telemetry.oxagen.test/v1/events",
        credential_env: "STELLA_TEST_TELEMETRY_TOKEN",
        event_classes: vec!["execution_rollup"],
        host_data_isolation: "process_free",
        model_catalog: vec![TestModelDimension {
            provider: "anthropic",
            model: "anthropic/claude-sonnet-4",
        }],
        issued_at_unix_s: 1_700_000_000,
        expires_at_unix_s: 1_700_003_600,
    }
}

fn rollup(id: i64) -> ExecutionRollupRow {
    ExecutionRollupRow {
        usage_complete: true,
        project_id: "local-project-id".into(),
        project_name: "private-name".into(),
        project_root: "/private/path".into(),
        execution_id: id,
        kind: "run".into(),
        prompt_digest: "private-digest".into(),
        prompt_preview: "private prompt".into(),
        model: "anthropic/claude-sonnet-4".into(),
        provider: "anthropic".into(),
        outcome: "completed".into(),
        cost_usd: 0.01,
        input_tokens: 10,
        output_tokens: 5,
        duration_ms: 12,
        tool_calls: 1,
        files_written: 1,
        produced_output: true,
        self_rating: None,
        started_at: "2026-07-21 12:00:00".into(),
        day: "2026-07-21".into(),
        tool_histogram: Vec::new(),
    }
}

struct Sender {
    attempts: AtomicUsize,
    fail: Mutex<bool>,
}

#[async_trait]
impl BatchSender for Sender {
    async fn send(
        &self,
        _endpoint: &reqwest::Url,
        _bearer_token: &str,
        _events: &[StellaOperationalEventV1],
    ) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if *self.fail.lock().unwrap() {
            Err("simulated HTTP failure".into())
        } else {
            Ok(())
        }
    }
}

#[test]
fn absent_enrollment_builds_no_client_and_creates_no_host_state() {
    let _env = crate::test_env::lock();
    let _restore = EnvRestore::capture(&["STELLA_DATA_DIR"]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let data = dir.path().join("host-data");
    std::fs::create_dir_all(&workspace).unwrap();
    unsafe { std::env::set_var("STELLA_DATA_DIR", &data) };
    let builds = AtomicUsize::new(0);

    let runtime = build_runtime_from_managed(None, &workspace, 1_700_000_001, || {
        builds.fetch_add(1, Ordering::SeqCst);
        unreachable!("disabled telemetry must not construct an HTTP client")
    })
    .unwrap();

    assert!(runtime.is_none());
    assert_eq!(builds.load(Ordering::SeqCst), 0);
    assert!(!data.exists());
    unsafe { std::env::remove_var("STELLA_DATA_DIR") };
}

#[test]
fn telemetry_status_and_flush_are_explicit_provider_free_commands() {
    for (name, expected) in [
        ("status", TelemetryCmd::Status),
        ("flush", TelemetryCmd::Flush),
        ("rollover-discard", TelemetryCmd::RolloverDiscard),
    ] {
        let cli = Cli::try_parse_from(["stella", "telemetry", name]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Telemetry { cmd }) if cmd == expected
        ));
    }
}

#[test]
fn redirects_and_non_success_http_responses_remain_retryable_failures() {
    assert!(validate_response_status(reqwest::StatusCode::TEMPORARY_REDIRECT).is_err());
    assert!(validate_response_status(reqwest::StatusCode::TOO_MANY_REQUESTS).is_err());
    assert!(validate_response_status(reqwest::StatusCode::NO_CONTENT).is_ok());
}

#[test]
fn enrollment_is_strict_signed_current_https_and_operational_only() {
    let _env = crate::test_env::lock();
    let _restore =
        EnvRestore::capture(&["STELLA_TEST_VERIFY_SECRET", "STELLA_TEST_TELEMETRY_TOKEN"]);
    let secret = b"0123456789abcdef0123456789abcdef";
    unsafe {
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "separate-bearer-token")
    };
    let now = 1_700_000_001;

    let valid = signed_managed("STELLA_TEST_VERIFY_SECRET", secret, valid_claims());
    assert!(verify_managed_enrollment(&valid, now).is_ok());

    let mut no_confinement = valid.clone();
    no_confinement["host_data_isolation"] = json!("process_capable");
    assert!(verify_managed_enrollment(&no_confinement, now).is_err());

    let mut malformed_allowlist = valid.clone();
    malformed_allowlist["allowed_endpoints"] = json!([
        "https://telemetry.oxagen.test/v1/events",
        "http://telemetry.oxagen.test/v1/events"
    ]);
    assert!(
        verify_managed_enrollment(&malformed_allowlist, now).is_err(),
        "every allowlist entry must satisfy the strict endpoint policy"
    );

    let mut expired = valid_claims();
    expired.expires_at_unix_s = now;
    assert!(
        verify_managed_enrollment(
            &signed_managed("STELLA_TEST_VERIFY_SECRET", secret, expired),
            now
        )
        .is_err()
    );

    let wrong_signature = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"abcdef0123456789abcdef0123456789",
        valid_claims(),
    );
    assert!(verify_managed_enrollment(&wrong_signature, now).is_err());

    for claims in [
        TestClaims {
            endpoint: "http://telemetry.oxagen.test/v1/events",
            ..valid_claims()
        },
        TestClaims {
            endpoint: "https://evil.example/v1/events",
            ..valid_claims()
        },
        TestClaims {
            issuer: "evil-issuer",
            ..valid_claims()
        },
        TestClaims {
            audience: "other-client",
            ..valid_claims()
        },
        TestClaims {
            schema: "stella.enterprise.telemetry.enrollment.v2",
            ..valid_claims()
        },
        TestClaims {
            event_classes: vec!["compliance_audit"],
            ..valid_claims()
        },
    ] {
        let managed = signed_managed("STELLA_TEST_VERIFY_SECRET", secret, claims);
        assert!(verify_managed_enrollment(&managed, now).is_err());
    }

    let mut unknown = valid;
    unknown["enrollment"]["claims"]["prompt"] = json!("must reject unknown content");
    assert!(verify_managed_enrollment(&unknown, now).is_err());
    unsafe {
        std::env::remove_var("STELLA_TEST_VERIFY_SECRET");
        std::env::remove_var("STELLA_TEST_TELEMETRY_TOKEN");
    }
}

#[test]
fn identical_signing_and_bearer_rotation_domains_are_rejected() {
    let _env = crate::test_env::lock();
    let shared = "STELLA_TEST_SHARED_TELEMETRY_SECRET";
    let separate = "STELLA_TEST_SEPARATE_TELEMETRY_TOKEN";
    let _restore = EnvRestore::capture(&[shared, separate]);
    let secret = b"0123456789abcdef0123456789abcdef";
    unsafe {
        std::env::set_var(shared, std::str::from_utf8(secret).unwrap());
        std::env::set_var(separate, std::str::from_utf8(secret).unwrap());
    }
    let same_ref = signed_managed(
        shared,
        secret,
        TestClaims {
            credential_env: shared,
            ..valid_claims()
        },
    );
    assert!(verify_managed_enrollment(&same_ref, 1_700_000_001).is_err());
    let same_value = signed_managed(
        shared,
        secret,
        TestClaims {
            credential_env: separate,
            ..valid_claims()
        },
    );
    assert!(verify_managed_enrollment(&same_value, 1_700_000_001).is_err());
}

#[test]
fn pre_dotenv_snapshot_restores_every_privileged_control_and_credential_ref() {
    let _env = crate::test_env::lock();
    let names = [
        "STELLA_DATA_DIR",
        "STELLA_TRUST_PROJECT",
        "STELLA_BASH_SANDBOX",
        "STELLA_SKILLS_SEARCH_CMD",
        "HTTPS_PROXY",
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
    ];
    let _restore = EnvRestore::capture(&names);
    for name in names {
        unsafe { std::env::remove_var(name) };
    }
    let managed = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"0123456789abcdef0123456789abcdef",
        valid_claims(),
    );
    let snapshot = StartupAuthoritySnapshot::capture(Some(&managed));
    for name in names {
        unsafe { std::env::set_var(name, format!("project-{name}")) };
    }
    let rejected = snapshot.restore_after_project_env(&names.map(str::to_string));
    assert_eq!(rejected.len(), names.len());
    for name in names {
        assert!(
            std::env::var_os(name).is_none(),
            "project value survived: {name}"
        );
    }
}

#[test]
fn invalid_enrollment_cannot_register_arbitrary_scrub_names() {
    let arbitrary = "STELLA_ATTACKER_CHOSEN_SCRUB_TARGET";
    assert!(!stella_tools::exec::is_sensitive_env_name(arbitrary));
    let invalid = json!({
        "verification_secret_env": arbitrary,
        "allowed_issuers": [],
        "allowed_audiences": [],
        "allowed_endpoints": [],
        "host_data_isolation": "process_free",
        "enrollment": {"claims": {}, "signature_hex": "bad"}
    });
    assert!(verify_managed_enrollment(&invalid, 1_700_000_001).is_err());
    assert!(!stella_tools::exec::is_sensitive_env_name(arbitrary));
}

#[test]
fn proof_activation_failure_scrubs_credentials_and_fails_every_execution_surface_closed() {
    let _env = crate::test_env::lock();
    let _authority = AuthorityReset::new();
    let names = ["STELLA_TEST_VERIFY_SECRET", "STELLA_TEST_TELEMETRY_TOKEN"];
    let _restore = EnvRestore::capture(&names);
    unsafe {
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "separate-bearer-token");
    }
    let managed = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"0123456789abcdef0123456789abcdef",
        valid_claims(),
    );
    let enrollment = verify_managed_enrollment(&managed, 1_700_000_001).unwrap();

    let error = activate_process_free_authority_with(&enrollment, || {
        Err("simulated registry proof failure".into())
    })
    .unwrap_err();
    assert!(error.contains("simulated registry proof failure"));
    assert!(
        process_free_authority_active(),
        "failed-closed authority must keep process-free restrictions armed"
    );
    for name in names {
        assert!(
            stella_tools::exec::is_sensitive_env_name(name),
            "verified credential was not scrub-registered: {name}"
        );
    }
    for surface in ExecutionSurface::ALL {
        let error = authorize_execution_surface(surface).unwrap_err();
        assert!(error.contains("failed closed"), "{surface:?}: {error}");
    }
}

#[test]
fn post_verification_setup_errors_never_restore_full_execution_authority() {
    let _env = crate::test_env::lock();
    let _authority = AuthorityReset::new();
    let names = [
        "STELLA_DATA_DIR",
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
    ];
    let _restore = EnvRestore::capture(&names);
    unsafe {
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "separate-bearer-token");
    }
    let managed = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"0123456789abcdef0123456789abcdef",
        valid_claims(),
    );

    for seam in [
        "host_spool_path",
        "store_open",
        "identity",
        "spool_open",
        "sender",
    ] {
        reset_process_free_authority_for_test();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let data = dir.path().join("host-data");
        std::fs::create_dir_all(&workspace).unwrap();
        unsafe { std::env::set_var("STELLA_DATA_DIR", &data) };
        match seam {
            "host_spool_path" => unsafe {
                std::env::set_var("STELLA_DATA_DIR", workspace.join("model-visible"));
            },
            "store_open" => {
                std::fs::create_dir_all(workspace.join(".stella")).unwrap();
                std::fs::write(workspace.join(".stella/private"), b"not a directory").unwrap();
            }
            "identity" => {
                std::fs::create_dir_all(&data).unwrap();
                std::fs::write(data.join("installation-id"), b"malformed-legacy-id").unwrap();
            }
            "spool_open" => {
                std::fs::create_dir_all(data.join("enterprise-telemetry.db")).unwrap();
            }
            "sender" => {}
            _ => unreachable!(),
        }
        let result = build_runtime_from_managed(Some(&managed), &workspace, 1_700_000_001, || {
            if seam == "sender" {
                Err("simulated sender construction failure".into())
            } else {
                Ok(Arc::new(Sender {
                    attempts: AtomicUsize::new(0),
                    fail: Mutex::new(false),
                }) as Arc<dyn BatchSender>)
            }
        });
        assert!(result.is_err(), "seam unexpectedly succeeded: {seam}");
        assert!(
            process_free_authority_active(),
            "authority downgraded after {seam}"
        );
        assert!(
            authorize_execution_surface(ExecutionSurface::PipelineOneShot).is_err(),
            "full execution authority returned after {seam}"
        );
        for name in &names[1..] {
            assert!(
                stella_tools::exec::is_sensitive_env_name(name),
                "credential scrub registration was lost after {seam}: {name}"
            );
        }
    }
}

#[test]
fn managed_identifiers_are_bounded_before_any_store_or_ledger_mutation() {
    let _env = crate::test_env::lock();
    let _authority = AuthorityReset::new();
    let names = [
        "STELLA_DATA_DIR",
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
    ];
    let _restore = EnvRestore::capture(&names);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let data = dir.path().join("host-data");
    std::fs::create_dir_all(&workspace).unwrap();
    unsafe {
        std::env::set_var("STELLA_DATA_DIR", &data);
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "separate-bearer-token");
    }
    let too_long: &'static str = Box::leak("x".repeat(129).into_boxed_str());
    let managed = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"0123456789abcdef0123456789abcdef",
        TestClaims {
            organization_id: too_long,
            ..valid_claims()
        },
    );
    let sender_builds = AtomicUsize::new(0);

    assert!(
        build_runtime_from_managed(Some(&managed), &workspace, 1_700_000_001, || {
            sender_builds.fetch_add(1, Ordering::SeqCst);
            unreachable!("invalid managed identifiers must fail before sender construction")
        })
        .is_err()
    );
    assert_eq!(sender_builds.load(Ordering::SeqCst), 0);
    assert!(!workspace.join(".stella/private/store.db").exists());
    assert!(!data.exists());
}

#[test]
fn malformed_legacy_pending_rows_are_skipped_without_blocking_later_valid_rows() {
    let _env = crate::test_env::lock();
    let _authority = AuthorityReset::new();
    let names = [
        "STELLA_DATA_DIR",
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
    ];
    let _restore = EnvRestore::capture(&names);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let data = dir.path().join("host-data");
    std::fs::create_dir_all(&workspace).unwrap();
    unsafe {
        std::env::set_var("STELLA_DATA_DIR", &data);
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "separate-bearer-token");
    }
    let managed = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"0123456789abcdef0123456789abcdef",
        valid_claims(),
    );
    let enrollment = verify_managed_enrollment(&managed, 1_700_000_001).unwrap();
    let store = stella_store::Store::open(&workspace).unwrap();
    store
        .begin_enterprise_enrollment(enrollment.sink_fingerprint())
        .unwrap();
    let malformed = store
        .begin_execution("run", "malformed", "anthropic", "anthropic/claude-sonnet-4")
        .unwrap();
    store.finish_execution(malformed, "completed", 0.0).unwrap();
    store
        .mark_enterprise_export_pending(enrollment.sink_fingerprint(), malformed)
        .unwrap()
        .unwrap();
    let valid = store
        .begin_execution("run", "valid", "anthropic", "anthropic/claude-sonnet-4")
        .unwrap();
    store.finish_execution(valid, "completed", 0.0).unwrap();
    store
        .mark_enterprise_export_pending(enrollment.sink_fingerprint(), valid)
        .unwrap()
        .unwrap();
    drop(store);
    let path = stella_store::workspace_private_sqlite_path(&workspace, "store.db").unwrap();
    let raw = rusqlite::Connection::open(path).unwrap();
    raw.execute(
        "UPDATE enterprise_export_ledger SET export_nonce = 'malformed' WHERE execution_id = ?1",
        rusqlite::params![malformed],
    )
    .unwrap();
    drop(raw);

    let sender = Arc::new(Sender {
        attempts: AtomicUsize::new(0),
        fail: Mutex::new(false),
    });
    let runtime = build_runtime_from_managed(Some(&managed), &workspace, 1_700_000_001, || {
        Ok(sender as Arc<dyn BatchSender>)
    })
    .unwrap()
    .unwrap();
    assert_eq!(runtime.status().unwrap().pending_rows, 1);
    let reopened = stella_store::Store::open(&workspace).unwrap();
    let status = reopened
        .enterprise_export_ledger_status(enrollment.sink_fingerprint())
        .unwrap();
    assert_eq!(status.skipped_rows, 1);
    assert_eq!(status.malformed_nonce_rows, 1);
    assert_eq!(status.malformed_rollup_rows, 0);
    assert_eq!(status.missing_rollup_rows, 0);
    assert!(
        reopened
            .pending_enterprise_export_page(enrollment.sink_fingerprint(), None, 256)
            .unwrap()
            .is_empty(),
        "malformed first row must be skipped and later valid row spooled"
    );
}

#[test]
fn project_dotenv_cannot_supply_either_managed_credential() {
    let _env = crate::test_env::lock();
    let verify_ref = "STELLA_PROJECT_DOTENV_VERIFY_SECRET";
    let token_ref = "STELLA_PROJECT_DOTENV_BEARER_TOKEN";
    let _restore = EnvRestore::capture(&[verify_ref, token_ref]);
    let secret = b"fedcba9876543210fedcba9876543210";
    let managed = signed_managed(
        verify_ref,
        secret,
        TestClaims {
            credential_env: token_ref,
            ..valid_claims()
        },
    );
    unsafe {
        std::env::remove_var(verify_ref);
        std::env::remove_var(token_ref);
    }
    let snapshot = StartupAuthoritySnapshot::capture(Some(&managed));
    unsafe {
        std::env::set_var(verify_ref, "fedcba9876543210fedcba9876543210");
        std::env::set_var(token_ref, "project-controlled-token");
    }
    let rejected =
        snapshot.restore_after_project_env(&[verify_ref.to_string(), token_ref.to_string()]);
    assert_eq!(rejected.len(), 2);
    let Err(error) = verify_managed_enrollment(&managed, 1_700_000_001) else {
        panic!("project dotenv credentials were accepted");
    };
    assert!(!error.contains(verify_ref));
    assert!(!error.contains(token_ref));
}

#[test]
fn host_spool_path_rejects_workspace_and_symlinked_data_roots() {
    let _env = crate::test_env::lock();
    let _restore = EnvRestore::capture(&["STELLA_DATA_DIR"]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    unsafe { std::env::set_var("STELLA_DATA_DIR", workspace.join(".stella")) };
    assert!(host_spool_path(&workspace).is_err());

    #[cfg(unix)]
    {
        let link = outside.join("linked-data");
        std::os::unix::fs::symlink(&workspace, &link).unwrap();
        unsafe { std::env::set_var("STELLA_DATA_DIR", &link) };
        assert!(host_spool_path(&workspace).is_err());
    }

    unsafe { std::env::set_var("STELLA_DATA_DIR", &outside) };
    let spool = host_spool_path(&workspace).unwrap();
    assert!(spool.starts_with(outside.canonicalize().unwrap()));
    assert!(!spool.starts_with(workspace.canonicalize().unwrap()));
    unsafe { std::env::remove_var("STELLA_DATA_DIR") };
}

#[test]
fn only_the_managed_settings_snapshot_can_supply_enrollment() {
    let _env = crate::test_env::lock();
    let _restore = EnvRestore::capture(&["HOME", "STELLA_MANAGED_SETTINGS"]);
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let workspace = dir.path().join("workspace");
    let project_dir = workspace.join(".stella");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        project_dir.join("settings.json"),
        r#"{"enterprise_telemetry":{"source":"project"}}"#,
    )
    .unwrap();
    let absent_managed = dir.path().join("absent-managed.json");
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("STELLA_MANAGED_SETTINGS", &absent_managed);
    }
    let settings = Settings::load(&workspace).unwrap();
    assert!(settings.managed_enterprise_telemetry().is_none());

    let managed = dir.path().join("managed.json");
    std::fs::write(&managed, r#"{"enterprise_telemetry":{"source":"managed"}}"#).unwrap();
    unsafe { std::env::set_var("STELLA_MANAGED_SETTINGS", &managed) };
    let settings = Settings::load(&workspace).unwrap();
    assert_eq!(
        settings.managed_enterprise_telemetry().unwrap()["source"],
        "managed"
    );
    unsafe {
        std::env::remove_var("HOME");
        std::env::remove_var("STELLA_MANAGED_SETTINGS");
    }
}

#[cfg(unix)]
#[test]
fn managed_settings_reject_symlinks_and_group_or_other_writable_files() {
    use std::os::unix::fs::PermissionsExt;

    let _env = crate::test_env::lock();
    let _restore = EnvRestore::capture(&["HOME", "STELLA_MANAGED_SETTINGS"]);
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let managed = dir.path().join("managed.json");
    std::fs::write(&managed, "{}").unwrap();
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o666)).unwrap();
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("STELLA_MANAGED_SETTINGS", &managed);
    }
    assert!(Settings::load(&workspace).is_err());

    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o600)).unwrap();
    let linked = dir.path().join("managed-link.json");
    std::os::unix::fs::symlink(&managed, &linked).unwrap();
    unsafe { std::env::set_var("STELLA_MANAGED_SETTINGS", &linked) };
    assert!(Settings::load(&workspace).is_err());
}

#[test]
fn failed_delivery_stays_retryable_and_success_acks_the_same_event() {
    let _env = crate::test_env::lock();
    let _restore = EnvRestore::capture(&[
        "STELLA_DATA_DIR",
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let data = dir.path().join("host-data");
    std::fs::create_dir_all(&workspace).unwrap();
    unsafe {
        std::env::set_var("STELLA_DATA_DIR", &data);
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "bearer-secret");
    }
    let managed = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"0123456789abcdef0123456789abcdef",
        valid_claims(),
    );
    let sender = Arc::new(Sender {
        attempts: AtomicUsize::new(0),
        fail: Mutex::new(true),
    });
    let sender_for_runtime = sender.clone();
    let runtime = build_runtime_from_managed(Some(&managed), &workspace, 1_700_000_001, || {
        Ok(sender_for_runtime as Arc<dyn BatchSender>)
    })
    .unwrap()
    .unwrap();
    runtime.enqueue_rollup(&rollup(7), 10).unwrap();

    let handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        handle
            .block_on(runtime.flush_with_retry_clock(20, || Ok(20)))
            .is_err()
    );
    assert_eq!(runtime.status().unwrap().pending_rows, 1);
    *sender.fail.lock().unwrap() = false;
    let flushed = handle
        .block_on(runtime.flush_with_retry_clock(2_000, || Ok(2_000)))
        .unwrap();
    assert_eq!(flushed, 1);
    assert_eq!(runtime.status().unwrap().pending_rows, 0);
    assert_eq!(sender.attempts.load(Ordering::SeqCst), 2);

    unsafe {
        std::env::remove_var("STELLA_DATA_DIR");
        std::env::remove_var("STELLA_TEST_VERIFY_SECRET");
        std::env::remove_var("STELLA_TEST_TELEMETRY_TOKEN");
    }
}

#[test]
fn delivery_retry_reads_a_fresh_wall_clock_after_the_claim() {
    let _env = crate::test_env::lock();
    let _restore = EnvRestore::capture(&[
        "STELLA_DATA_DIR",
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    unsafe {
        std::env::set_var("STELLA_DATA_DIR", dir.path().join("host-data"));
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "bearer-secret");
    }
    let managed = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"0123456789abcdef0123456789abcdef",
        valid_claims(),
    );
    let sender = Arc::new(Sender {
        attempts: AtomicUsize::new(0),
        fail: Mutex::new(true),
    });
    let sender_for_runtime = sender.clone();
    let runtime = build_runtime_from_managed(Some(&managed), &workspace, 1_700_000_001, || {
        Ok(sender_for_runtime as Arc<dyn BatchSender>)
    })
    .unwrap()
    .unwrap();
    runtime.enqueue_rollup(&rollup(81), 100_000).unwrap();
    let handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        handle
            .block_on(runtime.flush_with_retry_clock(100_000, || Ok(1_000)))
            .is_err()
    );
    *sender.fail.lock().unwrap() = false;
    assert_eq!(
        handle
            .block_on(runtime.flush_with_retry_clock(5_000, || Ok(5_000)))
            .unwrap(),
        1,
        "retry must be eligible in the repaired clock epoch"
    );
}

#[test]
fn startup_backfill_processes_one_bounded_page_and_progresses_across_runs() {
    let _env = crate::test_env::lock();
    let _restore = EnvRestore::capture(&[
        "STELLA_DATA_DIR",
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    unsafe {
        std::env::set_var("STELLA_DATA_DIR", dir.path().join("host-data"));
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "separate-token");
    }
    let secret = b"0123456789abcdef0123456789abcdef";
    let managed = signed_managed("STELLA_TEST_VERIFY_SECRET", secret, valid_claims());
    let enrollment = verify_managed_enrollment(&managed, 1_700_000_001).unwrap();
    let store = stella_store::Store::open(&workspace).unwrap();
    store
        .begin_enterprise_enrollment(enrollment.sink_fingerprint())
        .unwrap();
    for index in 0..600 {
        let id = store
            .begin_execution(
                "run",
                &format!("outage-{index}"),
                "anthropic",
                "anthropic/claude-sonnet-4",
            )
            .unwrap();
        store.finish_execution(id, "completed", 0.0).unwrap();
        store
            .mark_enterprise_export_pending(enrollment.sink_fingerprint(), id)
            .unwrap()
            .unwrap();
    }
    drop(store);
    let pending_count = || {
        let store = stella_store::Store::open(&workspace).unwrap();
        let mut after = None;
        let mut count = 0;
        loop {
            let page = store
                .pending_enterprise_export_page(enrollment.sink_fingerprint(), after, 256)
                .unwrap();
            count += page.len();
            let Some(last) = page.last() else {
                break count;
            };
            after = Some(last.execution_id);
        }
    };
    let sender = Arc::new(Sender {
        attempts: AtomicUsize::new(0),
        fail: Mutex::new(false),
    });
    build_runtime_from_managed(Some(&managed), &workspace, 1_700_000_001, || {
        Ok(sender.clone() as Arc<dyn BatchSender>)
    })
    .unwrap();
    assert_eq!(pending_count(), 344);
    build_runtime_from_managed(Some(&managed), &workspace, 1_700_000_001, || {
        Ok(sender.clone() as Arc<dyn BatchSender>)
    })
    .unwrap();
    assert_eq!(pending_count(), 88);
}

#[test]
fn credential_rotation_failure_releases_the_claim_to_retry_state() {
    let _env = crate::test_env::lock();
    let names = [
        "STELLA_DATA_DIR",
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
    ];
    let _restore = EnvRestore::capture(&names);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    unsafe {
        std::env::set_var("STELLA_DATA_DIR", dir.path().join("host-data"));
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "bearer-secret");
    }
    let managed = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"0123456789abcdef0123456789abcdef",
        valid_claims(),
    );
    let sender = Arc::new(Sender {
        attempts: AtomicUsize::new(0),
        fail: Mutex::new(false),
    });
    let runtime = build_runtime_from_managed(Some(&managed), &workspace, 1_700_000_001, || {
        Ok(sender.clone() as Arc<dyn BatchSender>)
    })
    .unwrap()
    .unwrap();
    runtime.enqueue_rollup(&rollup(8), 10).unwrap();
    unsafe { std::env::remove_var("STELLA_TEST_VERIFY_SECRET") };
    let handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        handle
            .block_on(runtime.flush_with_retry_clock(20, || Ok(20)))
            .is_err()
    );
    assert_eq!(runtime.status().unwrap().pending_rows, 1);
    assert_eq!(sender.attempts.load(Ordering::SeqCst), 0);
}

#[test]
fn enrolled_host_can_flush_but_run_tests_cannot_observe_its_credentials() {
    let _env = crate::test_env::lock();
    let _restore = EnvRestore::capture(&[
        "STELLA_DATA_DIR",
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let data = dir.path().join("host-data");
    std::fs::create_dir_all(&workspace).unwrap();
    unsafe {
        std::env::set_var("STELLA_DATA_DIR", &data);
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "bearer-secret-value");
    }
    let managed = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"0123456789abcdef0123456789abcdef",
        valid_claims(),
    );
    let sender = Arc::new(Sender {
        attempts: AtomicUsize::new(0),
        fail: Mutex::new(false),
    });
    let runtime = build_runtime_from_managed(Some(&managed), &workspace, 1_700_000_001, || {
        Ok(sender as Arc<dyn BatchSender>)
    })
    .unwrap()
    .unwrap();
    runtime.enqueue_rollup(&rollup(77), 10).unwrap();
    let handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_eq!(
        handle
            .block_on(runtime.flush_with_retry_clock(20, || Ok(20)))
            .unwrap(),
        1
    );

    let registry = stella_tools::ToolRegistry::with_backends(workspace, None, None);
    let output = handle.block_on(registry.execute("run_tests", &json!({"command": "env"})));
    let ToolOutput::Ok { content } = output else {
        panic!("run_tests failed: {output:?}");
    };
    for forbidden in [
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
        "0123456789abcdef0123456789abcdef",
        "bearer-secret-value",
    ] {
        assert!(!content.contains(forbidden), "credential leaked: {content}");
    }
}

#[test]
fn finalization_stays_successful_when_telemetry_host_state_is_rejected() {
    let _env = crate::test_env::lock();
    let _restore = EnvRestore::capture(&[
        "HOME",
        "STELLA_MANAGED_SETTINGS",
        "STELLA_DATA_DIR",
        "STELLA_TEST_VERIFY_SECRET",
        "STELLA_TEST_TELEMETRY_TOKEN",
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let managed_value = signed_managed(
        "STELLA_TEST_VERIFY_SECRET",
        b"0123456789abcdef0123456789abcdef",
        TestClaims {
            issued_at_unix_s: now - 1,
            expires_at_unix_s: now + 3_600,
            ..valid_claims()
        },
    );
    let managed_path = dir.path().join("managed.json");
    std::fs::write(
        &managed_path,
        serde_json::to_vec(&json!({
            "enterprise_telemetry": managed_value
        }))
        .unwrap(),
    )
    .unwrap();
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("STELLA_MANAGED_SETTINGS", &managed_path);
        std::env::set_var("STELLA_DATA_DIR", workspace.join("model-visible-data"));
        std::env::set_var(
            "STELLA_TEST_VERIFY_SECRET",
            "0123456789abcdef0123456789abcdef",
        );
        std::env::set_var("STELLA_TEST_TELEMETRY_TOKEN", "separate-bearer-token");
    }
    let store = stella_store::Store::open(&workspace).unwrap();
    let enrollment = verify_managed_enrollment(&managed_value, now).unwrap();
    store
        .begin_enterprise_enrollment(enrollment.sink_fingerprint())
        .unwrap();
    let id = store
        .begin_execution("run", "private prompt", "anthropic", "claude-sonnet-4")
        .unwrap();
    let registry = stella_tools::ToolRegistry::with_backends(workspace.clone(), None, None);

    assert!(crate::agent::record_execution_end(
        &store,
        id,
        &registry,
        0,
        "completed",
        0.01,
        true,
    ));
    assert_eq!(
        store
            .execution_rollup(id, &workspace)
            .unwrap()
            .unwrap()
            .outcome,
        "completed"
    );
    assert!(
        !workspace
            .join("model-visible-data/enterprise-telemetry.db")
            .exists()
    );
    let pending = store
        .pending_enterprise_export_page(enrollment.sink_fingerprint(), None, 256)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].execution_id, id,
        "fail-open spool rejection must remain durably visible for backfill"
    );
    unsafe {
        std::env::remove_var("HOME");
        std::env::remove_var("STELLA_MANAGED_SETTINGS");
        std::env::remove_var("STELLA_DATA_DIR");
        std::env::remove_var("STELLA_TEST_VERIFY_SECRET");
        std::env::remove_var("STELLA_TEST_TELEMETRY_TOKEN");
    }
}
