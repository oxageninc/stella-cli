//! End-to-end conformance-suite tests against the real `ocp-example-docs`
//! fixture. A well-behaved provider passes
//! every check; each `--misbehave` mode trips exactly the check it violates,
//! proving the suite catches a broken provider (task deliverable).

use ocp_conformance::{
    CHECK_BUDGET_HONESTY, CHECK_FRAME_VALIDITY, CHECK_HANDSHAKE, CHECK_MALFORMED, CHECK_SHUTDOWN,
    CheckStatus, ProviderTarget, run_conformance,
};

/// Path to the fixture binary, built automatically for integration tests.
fn fixture() -> String {
    env!("CARGO_BIN_EXE_ocp-example-docs").to_string()
}

fn target(args: &[&str]) -> ProviderTarget {
    ProviderTarget::Stdio {
        program: fixture(),
        args: args.iter().map(|s| s.to_string()).collect(),
    }
}

fn status_of(report: &ocp_conformance::ConformanceReport, name: &str) -> CheckStatus {
    report
        .checks
        .iter()
        .find(|check| check.name == name)
        .unwrap_or_else(|| panic!("report is missing the `{name}` check"))
        .status
}

#[tokio::test]
async fn a_well_behaved_provider_is_fully_conformant() {
    let report = run_conformance(target(&[])).await;
    assert!(
        report.passed(),
        "expected conformant; failures: {:?}",
        report.failures().collect::<Vec<_>>()
    );
    // All five checks ran and passed (none skipped for a stdio provider).
    assert_eq!(report.checks.len(), 5);
    for name in [
        CHECK_HANDSHAKE,
        CHECK_FRAME_VALIDITY,
        CHECK_BUDGET_HONESTY,
        CHECK_SHUTDOWN,
        CHECK_MALFORMED,
    ] {
        assert_eq!(status_of(&report, name), CheckStatus::Pass, "{name}");
    }
}

#[tokio::test]
async fn lying_about_token_cost_fails_budget_honesty() {
    let report = run_conformance(target(&["--misbehave", "lying-costs"])).await;
    assert!(!report.passed());
    assert_eq!(status_of(&report, CHECK_BUDGET_HONESTY), CheckStatus::Fail);
    // The handshake itself was fine — only the budget check caught the lie.
    assert_eq!(status_of(&report, CHECK_HANDSHAKE), CheckStatus::Pass);
}

#[tokio::test]
async fn an_out_of_range_score_fails_frame_validity() {
    let report = run_conformance(target(&["--misbehave", "bad-score"])).await;
    assert!(!report.passed());
    assert_eq!(status_of(&report, CHECK_FRAME_VALIDITY), CheckStatus::Fail);
}

#[tokio::test]
async fn an_empty_citation_label_fails_frame_validity() {
    let report = run_conformance(target(&["--misbehave", "empty-citation"])).await;
    assert!(!report.passed());
    assert_eq!(status_of(&report, CHECK_FRAME_VALIDITY), CheckStatus::Fail);
}

#[tokio::test]
async fn crashing_on_a_query_fails_the_frame_checks_but_not_the_handshake() {
    let report = run_conformance(target(&["--misbehave", "crash-on-query"])).await;
    assert!(!report.passed());
    // Handshake completed; the provider only died on the query.
    assert_eq!(status_of(&report, CHECK_HANDSHAKE), CheckStatus::Pass);
    assert_eq!(status_of(&report, CHECK_FRAME_VALIDITY), CheckStatus::Fail);
    assert_eq!(status_of(&report, CHECK_BUDGET_HONESTY), CheckStatus::Fail);
}

#[tokio::test]
async fn crashing_on_garbage_fails_malformed_input_tolerance() {
    let report = run_conformance(target(&["--misbehave", "crash-on-garbage"])).await;
    assert!(!report.passed());
    assert_eq!(status_of(&report, CHECK_MALFORMED), CheckStatus::Fail);
}

#[tokio::test]
async fn an_incompatible_protocol_version_fails_the_handshake() {
    let report = run_conformance(target(&["--misbehave", "bad-version"])).await;
    assert!(!report.passed());
    assert_eq!(status_of(&report, CHECK_HANDSHAKE), CheckStatus::Fail);
    // With no established provider, the behavioral checks are skipped.
    assert_eq!(
        status_of(&report, CHECK_FRAME_VALIDITY),
        CheckStatus::Skipped
    );
}
