//! `ocp-conformance` — the public Open Context Protocol conformance suite
//!.
//!
//! "OCP conformant" means *green on this suite for your declared capability
//! set* — a checkable claim, which is what makes third-party adoption safe.
//! [`run_conformance`] drives a provider through the protocol and returns a
//! typed [`ConformanceReport`] with a pass/fail verdict per check and an
//! evidence string for each, so a failure says exactly what was wrong.
//!
//! The checks (all against the frozen `ocp-types` contracts):
//!
//! - **handshake** — the provider completes the handshake and reports a
//!   non-empty identity + capabilities.
//! - **frame-validity** — queried frames pass `ocp-types` validation: score
//!   in `[0, 1]`, a non-empty title, a non-empty `citation_label` (§3.4 —
//!   "NEVER a bare uuid").
//! - **budget-honesty** — returned frames' summed `token_cost` never exceeds
//!   the query budget (§3.3 — "never lies about cost").
//! - **shutdown-clean** — the provider tears down without error.
//! - **malformed-input-tolerance** — a garbage line is ignored-or-errored,
//!   never crashing the host (§3.5, task deliverable). Wire-level, so it
//!   applies to stdio providers.
//!
//! The suite is deliberately adversarial: pointed at a provider that lies
//! about costs, emits an out-of-range score, omits a citation label, or dies
//! mid-query, the matching check fails loudly. The bundled `ocp-example-docs`
//! fixture has `--misbehave` flags that trip each one, proving the suite
//! catches a broken provider (task deliverable).

use ocp_host::{ConsentRecord, ContextProvider, Host, HostError, RawStdioConnection};
use ocp_types::{Capabilities, ContextQuery, ContextQueryResult, ProviderInfo};

mod report;

pub use report::{CheckResult, CheckStatus, ConformanceReport};

/// The stable check names, so reports and callers agree on identifiers.
pub const CHECK_HANDSHAKE: &str = "handshake";
pub const CHECK_FRAME_VALIDITY: &str = "frame-validity";
pub const CHECK_BUDGET_HONESTY: &str = "budget-honesty";
pub const CHECK_SHUTDOWN: &str = "shutdown-clean";
pub const CHECK_MALFORMED: &str = "malformed-input-tolerance";

/// How to reach the provider under test. `ocp-inspect` builds one of these
/// from its CLI arguments; tests build them directly.
pub enum ProviderTarget {
    /// A child-process provider: `program` plus `args`.
    Stdio { program: String, args: Vec<String> },
    /// A remote provider at `url`.
    Http { url: String },
    /// An already-constructed in-process provider (e.g. a built-in).
    InProcess(Box<dyn ContextProvider>),
}

impl ProviderTarget {
    /// A one-line human description of the target, for the report header.
    pub fn describe(&self) -> String {
        match self {
            ProviderTarget::Stdio { program, args } => {
                if args.is_empty() {
                    format!("stdio: {program}")
                } else {
                    format!("stdio: {program} {}", args.join(" "))
                }
            }
            ProviderTarget::Http { url } => format!("http: {url}"),
            ProviderTarget::InProcess(provider) => format!("in-process: {}", provider.id()),
        }
    }
}

/// Run the full conformance suite against a provider, returning a typed
/// report. Never panics: every failure mode becomes a failing check with
/// evidence.
pub async fn run_conformance(target: ProviderTarget) -> ConformanceReport {
    let description = target.describe();

    // Capture stdio spawn info before `target` is consumed — the malformed
    // probe needs a second, independent connection to the same program.
    let stdio_probe = match &target {
        ProviderTarget::Stdio { program, args } => Some((program.clone(), args.clone())),
        _ => None,
    };

    let mut checks = Vec::new();

    match build_host(target).await {
        Ok((host, id, info, caps)) => {
            if info.name.trim().is_empty() || info.version.trim().is_empty() {
                checks.push(CheckResult::fail(
                    CHECK_HANDSHAKE,
                    format!(
                        "provider identity incomplete: name='{}' version='{}'",
                        info.name, info.version
                    ),
                ));
            } else {
                checks.push(CheckResult::pass(
                    CHECK_HANDSHAKE,
                    describe_handshake(&info, &caps),
                ));
            }
            run_query_and_shutdown_checks(host, &id, &mut checks).await;
        }
        Err(error) => {
            checks.push(CheckResult::fail(
                CHECK_HANDSHAKE,
                format!("could not establish provider: {error}"),
            ));
            for name in [CHECK_FRAME_VALIDITY, CHECK_BUDGET_HONESTY, CHECK_SHUTDOWN] {
                checks.push(CheckResult::skip(name, "handshake failed"));
            }
        }
    }

    match stdio_probe {
        Some((program, args)) => checks.push(malformed_stdio_probe(&program, &args).await),
        None => checks.push(CheckResult::skip(
            CHECK_MALFORMED,
            "wire-level malformed-input probe applies to stdio providers only",
        )),
    }

    ConformanceReport {
        target: description,
        checks,
    }
}

/// Stand up a one-provider host for the target and read back the provider's
/// negotiated identity + capabilities. Records consent for an egress
/// provider under test — running the suite *is* the consent to its declared
/// flow, so it isn't spuriously gated.
async fn build_host(
    target: ProviderTarget,
) -> Result<(Host, String, ProviderInfo, Capabilities), HostError> {
    let mut host = Host::new();
    let (id, info, caps) = match target {
        ProviderTarget::Stdio { program, args } => {
            let id = "provider-under-test".to_string();
            host.add_stdio(id.clone(), &program, &args).await?;
            capture_identity(&host, &id)?
        }
        ProviderTarget::Http { url } => {
            let id = "provider-under-test".to_string();
            host.add_http(id.clone(), url).await?;
            capture_identity(&host, &id)?
        }
        ProviderTarget::InProcess(provider) => {
            let id = provider.id().to_string();
            let info = provider.info().clone();
            let caps = provider.capabilities().clone();
            host.register(provider);
            (id, info, caps)
        }
    };

    if info.data_flow.egress {
        host.record_consent(ConsentRecord::new(
            id.clone(),
            info.data_flow,
            "conformance run under test",
        ));
    }

    Ok((host, id, info, caps))
}

fn capture_identity(
    host: &Host,
    id: &str,
) -> Result<(String, ProviderInfo, Capabilities), HostError> {
    let provider = host
        .provider(id)
        .ok_or_else(|| HostError::UnknownProvider(id.to_string()))?;
    Ok((
        id.to_string(),
        provider.info().clone(),
        provider.capabilities().clone(),
    ))
}

async fn run_query_and_shutdown_checks(host: Host, id: &str, checks: &mut Vec<CheckResult>) {
    let query = sample_query();
    match host.query_provider(id, &query).await {
        Ok(result) => {
            let (ok, evidence) = check_frames(&result);
            checks.push(CheckResult::from_bool(CHECK_FRAME_VALIDITY, ok, evidence));

            if result.respects_budget(query.max_tokens) {
                checks.push(CheckResult::pass(
                    CHECK_BUDGET_HONESTY,
                    format!(
                        "{} frame(s) sum to {} token(s), within the {}-token budget",
                        result.frames.len(),
                        result.total_token_cost(),
                        query.max_tokens
                    ),
                ));
            } else {
                checks.push(CheckResult::fail(
                    CHECK_BUDGET_HONESTY,
                    format!(
                        "frames claim {} token(s), over the {}-token budget — token_cost is dishonest",
                        result.total_token_cost(),
                        query.max_tokens
                    ),
                ));
            }
        }
        Err(error) => {
            let evidence = format!("query failed: {error}");
            checks.push(CheckResult::fail(CHECK_FRAME_VALIDITY, evidence.clone()));
            checks.push(CheckResult::fail(CHECK_BUDGET_HONESTY, evidence));
        }
    }

    let results = host.shutdown().await;
    match results.iter().find(|(pid, _)| pid == id) {
        Some((_, Ok(()))) => checks.push(CheckResult::pass(
            CHECK_SHUTDOWN,
            "provider acknowledged shutdown and tore down cleanly",
        )),
        Some((_, Err(error))) => checks.push(CheckResult::fail(
            CHECK_SHUTDOWN,
            format!("shutdown error: {error}"),
        )),
        None => checks.push(CheckResult::fail(
            CHECK_SHUTDOWN,
            "provider vanished before shutdown could be attempted",
        )),
    }
}

/// Wire-level probe: complete the handshake on a fresh connection, inject a
/// malformed line, then send a valid query. A conforming provider ignores or
/// cleanly errors on the garbage and stays alive to answer the query; a
/// provider that dies on one bad line fails.
async fn malformed_stdio_probe(program: &str, args: &[String]) -> CheckResult {
    let mut conn = match RawStdioConnection::spawn(program, args).await {
        Ok(conn) => conn,
        Err(error) => {
            return CheckResult::fail(
                CHECK_MALFORMED,
                format!("could not spawn provider: {error}"),
            );
        }
    };
    if let Err(error) = conn.handshake().await {
        return CheckResult::fail(
            CHECK_MALFORMED,
            format!("handshake failed before the probe could run: {error}"),
        );
    }
    if let Err(error) = conn.send_raw_line("this is not valid json {{{\n").await {
        return CheckResult::fail(
            CHECK_MALFORMED,
            format!("provider closed its input on a malformed line: {error}"),
        );
    }
    if let Err(error) = conn
        .send(&ocp_host::Envelope::Query {
            query: sample_query(),
        })
        .await
    {
        return CheckResult::fail(
            CHECK_MALFORMED,
            format!("provider died after a malformed line (before a valid query): {error}"),
        );
    }
    match conn.recv().await {
        Ok(ocp_host::Envelope::Frames { .. }) => CheckResult::pass(
            CHECK_MALFORMED,
            "provider ignored a malformed line and still answered a valid query",
        ),
        Ok(ocp_host::Envelope::Error { message }) => CheckResult::pass(
            CHECK_MALFORMED,
            format!("provider errored cleanly on malformed input and stayed alive: {message}"),
        ),
        Ok(other) => CheckResult::fail(
            CHECK_MALFORMED,
            format!(
                "provider replied to a valid query with an unexpected `{}` envelope",
                ocp_host::envelope_kind(&other)
            ),
        ),
        Err(HostError::ProviderCrashed { .. }) => CheckResult::fail(
            CHECK_MALFORMED,
            "provider crashed on a malformed line — it must error-or-ignore, not die",
        ),
        Err(error) => CheckResult::fail(
            CHECK_MALFORMED,
            format!("provider mishandled malformed input: {error}"),
        ),
    }
}

/// The query the suite probes every provider with — no `kinds` filter, so any
/// provider is asked for its best frames.
pub fn sample_query() -> ContextQuery {
    ContextQuery {
        goal: "conformance probe: return your most relevant frames".into(),
        query_text: Some("conformance probe".into()),
        embedding: None,
        kinds: vec![],
        anchors: vec![],
        max_frames: 8,
        max_tokens: 4096,
        as_of: None,
    }
}

/// Validate a query result's frames against the `ContextFrame` contract
/// (§3.4). Returns `(passed, evidence)`. Zero frames is permitted — a
/// provider may simply have nothing relevant.
pub fn check_frames(result: &ContextQueryResult) -> (bool, String) {
    if result.frames.is_empty() {
        return (
            true,
            "provider returned 0 frames (permitted — nothing relevant to the probe)".into(),
        );
    }

    let mut problems = Vec::new();
    for (i, frame) in result.frames.iter().enumerate() {
        if !frame.has_valid_score() {
            problems.push(format!("frame[{i}] score {} is outside [0,1]", frame.score));
        }
        if frame.title.trim().is_empty() {
            problems.push(format!("frame[{i}] has an empty title"));
        }
        match &frame.citation_label {
            Some(label) if !label.trim().is_empty() => {}
            _ => problems.push(format!(
                "frame[{i}] is missing a citation_label (§3.4 — never a bare id)"
            )),
        }
    }

    if problems.is_empty() {
        (
            true,
            format!(
                "{} frame(s) — all scores in [0,1], titles + citation labels present",
                result.frames.len()
            ),
        )
    } else {
        (false, problems.join("; "))
    }
}

fn describe_handshake(info: &ProviderInfo, caps: &Capabilities) -> String {
    format!(
        "provider '{}' v{} — data-flow reads={} writes={} egress={}; query kinds={:?}, upsert={}, graph={}",
        info.name,
        info.version,
        info.data_flow.reads,
        info.data_flow.writes,
        info.data_flow.egress,
        caps.query.kinds,
        caps.upsert,
        caps.graph,
    )
}
