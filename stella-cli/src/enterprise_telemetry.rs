//! Managed-only Oxagen Enterprise operational telemetry adapter.
//!
//! Community/default construction returns before resolving a spool path or
//! constructing an HTTP client. An enrolled deployment must supply a signed
//! managed document, a pinned verification-secret environment reference, an
//! exact HTTPS endpoint allowlist, and a bearer-token environment reference.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::atomic::AtomicU8;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stella_store::enterprise_telemetry::{
    EnqueueOutcome, EnterpriseExportSkipReason, EnterpriseTelemetrySpool, ManagedModelDimension,
    OperationalEventContext, OperationalIdentity, SpoolLimits, SpoolStatus,
    StellaOperationalEventV1,
};
#[cfg(test)]
use stella_store::usage::ExecutionRollupRow;

use crate::TelemetryCmd;

const ENROLLMENT_SCHEMA: &str = "stella.enterprise.telemetry.enrollment.v1";
const MAX_POLICY_ENTRIES: usize = 16;
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_ENV_REF_BYTES: usize = 128;
const MAX_SECRET_BYTES: usize = 4 * 1024;
const MAX_BEARER_BYTES: usize = 8 * 1024;
const MAX_ENROLLMENT_LIFETIME_S: i64 = 90 * 24 * 60 * 60;
const MAX_CLOCK_SKEW_S: i64 = 5 * 60;
const MAX_BATCH_EVENTS: usize = 50;
const MAX_BACKFILL_ROWS_PER_RUNTIME: usize = 256;
const COMPLETED_LEDGER_RETENTION_ROWS: usize = 2_048;
const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const LEASE_MS: i64 = 30_000;
const AUTHORITY_COMMUNITY: u8 = 0;
const AUTHORITY_PROCESS_FREE: u8 = 1;
const AUTHORITY_FAILED_CLOSED: u8 = 2;
#[cfg(not(test))]
static PROCESS_FREE_AUTHORITY: AtomicU8 = AtomicU8::new(AUTHORITY_COMMUNITY);
#[cfg(test)]
std::thread_local! {
    static PROCESS_FREE_AUTHORITY: std::cell::Cell<u8> = const {
        std::cell::Cell::new(AUTHORITY_COMMUNITY)
    };
}

#[cfg(not(test))]
fn load_process_free_authority() -> u8 {
    PROCESS_FREE_AUTHORITY.load(Ordering::Acquire)
}

#[cfg(test)]
fn load_process_free_authority() -> u8 {
    PROCESS_FREE_AUTHORITY.get()
}

#[cfg(not(test))]
fn store_process_free_authority(value: u8) {
    PROCESS_FREE_AUTHORITY.store(value, Ordering::Release);
}

#[cfg(test)]
fn store_process_free_authority(value: u8) {
    PROCESS_FREE_AUTHORITY.set(value);
}

const PRIVILEGED_ENV_NAMES: &[&str] = &[
    "STELLA_MANAGED_SETTINGS",
    "STELLA_DATA_DIR",
    "STELLA_TRUST_PROJECT",
    "STELLA_PROJECT_HOOKS",
    "STELLA_BASH_SANDBOX",
    "STELLA_BASE_URL",
    "STELLA_BUDGET",
    "STELLA_WEB_AUTH_FILE",
    "STELLA_INTEGRATIONS_FILE",
    "STELLA_GITHUB_API_URL",
    "STELLA_LINEAR_API_URL",
    "STELLA_LINEAR_CLIENT_ID",
    "STELLA_LINEAR_CLIENT_SECRET",
    "STELLA_SKILLS_SEARCH_CMD",
    "STELLA_SKILLS_INSTALL_CMD",
    "STELLA_SKILLS_USE_CMD",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedTelemetrySettings {
    verification_secret_env: String,
    allowed_issuers: Vec<String>,
    allowed_audiences: Vec<String>,
    allowed_endpoints: Vec<String>,
    host_data_isolation: HostDataIsolation,
    enrollment: SignedEnrollment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedEnrollment {
    claims: EnrollmentClaims,
    signature_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentClaims {
    schema: String,
    issuer: String,
    audience: String,
    enrollment_id: String,
    organization_id: String,
    workspace_id: String,
    endpoint: String,
    credential_env: String,
    event_classes: Vec<EnrollmentEventClass>,
    host_data_isolation: HostDataIsolation,
    model_catalog: Vec<ManagedModelDimension>,
    issued_at_unix_s: i64,
    expires_at_unix_s: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HostDataIsolation {
    ProcessFree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EnrollmentEventClass {
    ExecutionRollup,
    ComplianceAudit,
}

fn frame(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), String> {
    let len = u32::try_from(value.len())
        .map_err(|_| "enterprise telemetry enrollment field is too large".to_string())?;
    bytes.extend_from_slice(&len.to_be_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

/// Stable, domain-separated signing bytes independent of serde map ordering.
pub(crate) fn canonical_enrollment_bytes(value: &Value) -> Result<Vec<u8>, String> {
    let claims: EnrollmentClaims = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid enterprise telemetry claims: {error}"))?;
    let mut bytes = b"stella.enterprise.telemetry.enrollment-signature.v1".to_vec();
    for scalar in [
        claims.schema.as_str(),
        claims.issuer.as_str(),
        claims.audience.as_str(),
        claims.enrollment_id.as_str(),
        claims.organization_id.as_str(),
        claims.workspace_id.as_str(),
        claims.endpoint.as_str(),
        claims.credential_env.as_str(),
    ] {
        frame(&mut bytes, scalar.as_bytes())?;
    }
    frame(
        &mut bytes,
        u32::try_from(claims.event_classes.len())
            .map_err(|_| "too many enrollment event classes".to_string())?
            .to_be_bytes()
            .as_slice(),
    )?;
    for class in &claims.event_classes {
        let label = match class {
            EnrollmentEventClass::ExecutionRollup => "execution_rollup",
            EnrollmentEventClass::ComplianceAudit => "compliance_audit",
        };
        frame(&mut bytes, label.as_bytes())?;
    }
    frame(&mut bytes, b"process_free")?;
    frame(
        &mut bytes,
        u32::try_from(claims.model_catalog.len())
            .map_err(|_| "too many enrollment model dimensions".to_string())?
            .to_be_bytes()
            .as_slice(),
    )?;
    for dimension in &claims.model_catalog {
        frame(&mut bytes, dimension.provider().as_bytes())?;
        frame(&mut bytes, dimension.model().as_bytes())?;
    }
    frame(&mut bytes, &claims.issued_at_unix_s.to_be_bytes())?;
    frame(&mut bytes, &claims.expires_at_unix_s.to_be_bytes())?;
    Ok(bytes)
}

#[derive(Clone)]
struct VerifiedIdentifier(String);

impl VerifiedIdentifier {
    fn parse(value: &str) -> Result<Self, String> {
        let valid = !value.is_empty()
            && value.len() <= MAX_IDENTIFIER_BYTES
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            });
        if !valid {
            return Err(format!(
                "enterprise telemetry identifier must be 1..={MAX_IDENTIFIER_BYTES} ASCII bytes from [A-Za-z0-9._:-]"
            ));
        }
        Ok(Self(value.to_string()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) struct VerifiedEnrollment {
    enrollment_id: VerifiedIdentifier,
    organization_id: VerifiedIdentifier,
    workspace_id: VerifiedIdentifier,
    model_catalog: Vec<ManagedModelDimension>,
    endpoint: Url,
    credential_env: String,
    verification_secret_env: String,
    expires_at_unix_s: i64,
    sink_fingerprint: String,
}

impl VerifiedEnrollment {
    fn context(
        &self,
        identity: OperationalIdentity,
        export_nonce: &str,
    ) -> Result<OperationalEventContext, String> {
        OperationalEventContext::new(
            self.enrollment_id.as_str().to_owned(),
            self.organization_id.as_str().to_owned(),
            self.workspace_id.as_str().to_owned(),
            identity,
            export_nonce,
            self.model_catalog.clone(),
        )
        .map_err(|error| error.to_string())
    }

    #[cfg(test)]
    pub(crate) fn sink_fingerprint(&self) -> &str {
        &self.sink_fingerprint
    }
}

/// Whether signed managed authority has displaced Community authority.
/// Failed proof remains restricted here even though execution is denied.
pub(crate) fn process_free_authority_active() -> bool {
    load_process_free_authority() != AUTHORITY_COMMUNITY
}

fn process_free_authority_proven() -> bool {
    load_process_free_authority() == AUTHORITY_PROCESS_FREE
}

#[cfg(test)]
pub(crate) fn reset_process_free_authority_for_test() {
    store_process_free_authority(AUTHORITY_COMMUNITY);
}

/// Every production constructor that can assemble an agent execution surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionSurface {
    RawOneShot,
    PipelineOneShot,
    Goal,
    Fleet,
    Deck,
    Interactive,
    WorkspacePorts,
    CandidateWorkspace,
}

impl ExecutionSurface {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 8] = [
        Self::RawOneShot,
        Self::PipelineOneShot,
        Self::Goal,
        Self::Fleet,
        Self::Deck,
        Self::Interactive,
        Self::WorkspacePorts,
        Self::CandidateWorkspace,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RawOneShot => "raw_one_shot",
            Self::PipelineOneShot => "pipeline_one_shot",
            Self::Goal => "goal",
            Self::Fleet => "fleet",
            Self::Deck => "deck",
            Self::Interactive => "interactive",
            Self::WorkspacePorts => "workspace_ports",
            Self::CandidateWorkspace => "candidate_workspace",
        }
    }
}

pub(crate) fn authorize_execution_surface(surface: ExecutionSurface) -> Result<(), String> {
    match load_process_free_authority() {
        AUTHORITY_FAILED_CLOSED => Err(format!(
            "enterprise telemetry process-free authority activation failed closed before `{}` execution surface",
            surface.as_str()
        )),
        AUTHORITY_PROCESS_FREE => authorize_execution_surface_with(surface, true),
        _ => authorize_execution_surface_with(surface, false),
    }
}

pub(crate) fn authorize_one_shot(use_pipeline: bool) -> Result<(), String> {
    authorize_execution_surface(if use_pipeline {
        ExecutionSurface::PipelineOneShot
    } else {
        ExecutionSurface::RawOneShot
    })
}

pub(crate) fn authorize_execution_surface_with(
    surface: ExecutionSurface,
    process_free: bool,
) -> Result<(), String> {
    if process_free && surface != ExecutionSurface::RawOneShot {
        return Err(format!(
            "enterprise telemetry process-free authority rejects `{}` execution surface",
            surface.as_str()
        ));
    }
    Ok(())
}

fn activate_process_free_authority(
    enrollment: &VerifiedEnrollment,
    workspace_root: &Path,
) -> Result<(), String> {
    activate_process_free_authority_with(enrollment, || prove_process_free_surface(workspace_root))
}

pub(crate) fn activate_process_free_authority_with<F>(
    enrollment: &VerifiedEnrollment,
    prove: F,
) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String>,
{
    register_verified_credentials(enrollment);
    if process_free_authority_proven() {
        return Ok(());
    }
    // Once signed managed authority exists, community/full-authority fallback
    // is no longer safe. Keep every execution surface denied until the
    // concrete registry proof succeeds.
    store_process_free_authority(AUTHORITY_FAILED_CLOSED);
    prove()?;
    store_process_free_authority(AUTHORITY_PROCESS_FREE);
    Ok(())
}

fn register_verified_credentials(enrollment: &VerifiedEnrollment) {
    stella_tools::exec::register_sensitive_env_names([
        enrollment.verification_secret_env.clone(),
        enrollment.credential_env.clone(),
    ]);
}

pub(crate) fn prove_process_free_surface(workspace_root: &Path) -> Result<(), String> {
    let registry = stella_tools::ToolRegistry::with_backends_and_options(
        workspace_root.to_path_buf(),
        None,
        None,
        stella_tools::RegistryOptions {
            bash: false,
            web: false,
            media_host_data_isolation: Some(stella_tools::media::HostDataIsolation::ProcessFree),
            ..Default::default()
        },
    );
    if !registry.is_process_free() {
        return Err("enterprise telemetry process-free registry proof failed".into());
    }
    let names: BTreeSet<String> = registry
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    let forbidden = [
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
    ];
    if let Some(name) = forbidden.iter().find(|name| names.contains(**name)) {
        return Err(format!(
            "enterprise telemetry process-free registry exposes `{name}`"
        ));
    }
    Ok(())
}

/// Host authority captured before a project dotenv file is allowed to run.
/// The snapshot is local process state only; invalid enrollment bytes never
/// mutate the global spawn scrub registry.
pub(crate) struct StartupAuthoritySnapshot {
    values: BTreeMap<String, Option<OsString>>,
}

impl StartupAuthoritySnapshot {
    pub(crate) fn capture(managed: Option<&Value>) -> Self {
        let mut names: BTreeSet<String> = PRIVILEGED_ENV_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        if let Some(raw) = managed {
            for pointer in [
                "/verification_secret_env",
                "/enrollment/claims/credential_env",
            ] {
                if let Some(name) = raw.pointer(pointer).and_then(Value::as_str)
                    && validate_env_ref(name, "credential").is_ok()
                {
                    names.insert(name.to_string());
                }
            }
        }
        Self {
            values: names
                .into_iter()
                .map(|name| {
                    let value = std::env::var_os(&name);
                    (name, value)
                })
                .collect(),
        }
    }

    pub(crate) fn restore_after_project_env(&self, loaded: &[String]) -> Vec<String> {
        let loaded: BTreeSet<&str> = loaded.iter().map(String::as_str).collect();
        let mut rejected = Vec::new();
        for (name, value) in &self.values {
            if loaded.contains(name.as_str()) {
                rejected.push(name.clone());
            }
            // Restoring all privileged values also closes loaders which do not
            // report a complete set of parsed names.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        rejected
    }
}

/// Verify one managed enrollment without constructing persistence or HTTP.
pub(crate) fn verify_managed_enrollment(
    raw: &Value,
    now_unix_s: i64,
) -> Result<VerifiedEnrollment, String> {
    let managed: ManagedTelemetrySettings = serde_json::from_value(raw.clone())
        .map_err(|error| format!("invalid managed enterprise telemetry settings: {error}"))?;
    validate_env_ref(&managed.verification_secret_env, "verification secret")?;
    validate_policy_list(&managed.allowed_issuers, "issuer")?;
    validate_policy_list(&managed.allowed_audiences, "audience")?;
    if managed.allowed_endpoints.is_empty() || managed.allowed_endpoints.len() > MAX_POLICY_ENTRIES
    {
        return Err("managed telemetry endpoint allowlist must contain 1..=16 entries".into());
    }

    let claims = &managed.enrollment.claims;
    validate_env_ref(&claims.credential_env, "bearer credential")?;
    if managed.verification_secret_env == claims.credential_env {
        return Err("enterprise telemetry signing and bearer references must be distinct".into());
    }
    if managed.host_data_isolation != HostDataIsolation::ProcessFree
        || claims.host_data_isolation != HostDataIsolation::ProcessFree
    {
        return Err("enterprise telemetry requires signed process-free host isolation".into());
    }
    if claims.schema != ENROLLMENT_SCHEMA {
        return Err("unsupported enterprise telemetry enrollment schema".into());
    }
    if !managed
        .allowed_issuers
        .iter()
        .any(|item| item == &claims.issuer)
    {
        return Err("enterprise telemetry enrollment issuer is not allowlisted".into());
    }
    if !managed
        .allowed_audiences
        .iter()
        .any(|item| item == &claims.audience)
    {
        return Err("enterprise telemetry enrollment audience is not allowlisted".into());
    }
    if claims.issued_at_unix_s > now_unix_s.saturating_add(MAX_CLOCK_SKEW_S)
        || claims.expires_at_unix_s <= now_unix_s
        || claims.expires_at_unix_s <= claims.issued_at_unix_s
        || claims
            .expires_at_unix_s
            .saturating_sub(claims.issued_at_unix_s)
            > MAX_ENROLLMENT_LIFETIME_S
    {
        return Err("enterprise telemetry enrollment is not currently valid".into());
    }
    if claims.event_classes != [EnrollmentEventClass::ExecutionRollup] {
        if claims
            .event_classes
            .contains(&EnrollmentEventClass::ComplianceAudit)
        {
            return Err("compliance_audit telemetry is unsupported in this phase".into());
        }
        return Err("enterprise telemetry enrollment event class is not supported".into());
    }
    if claims.model_catalog.is_empty() || claims.model_catalog.len() > 64 {
        return Err("enterprise telemetry model catalog must contain 1..=64 entries".into());
    }
    let distinct: BTreeSet<(&str, &str)> = claims
        .model_catalog
        .iter()
        .map(|item| (item.provider(), item.model()))
        .collect();
    if distinct.len() != claims.model_catalog.len() {
        return Err("enterprise telemetry model catalog contains duplicate entries".into());
    }

    let endpoint = strict_https_url(&claims.endpoint)?;
    let allowed_endpoints = managed
        .allowed_endpoints
        .iter()
        .map(|allowed| strict_https_url(allowed))
        .collect::<Result<Vec<_>, _>>()?;
    let endpoint_allowed = allowed_endpoints.iter().any(|allowed| allowed == &endpoint);
    if !endpoint_allowed {
        return Err("enterprise telemetry endpoint is not exactly allowlisted".into());
    }

    let secret = std::env::var(&managed.verification_secret_env)
        .map_err(|_| "enterprise telemetry verification secret is unavailable".to_string())?;
    if secret.len() < 32 || secret.len() > MAX_SECRET_BYTES {
        return Err("enterprise telemetry verification secret must be 32..=4096 bytes".into());
    }
    let bearer = std::env::var(&claims.credential_env)
        .map_err(|_| "enterprise telemetry bearer credential is unavailable".to_string())?;
    if bearer.is_empty() || bearer.len() > MAX_BEARER_BYTES {
        return Err("enterprise telemetry bearer credential is invalid".into());
    }
    if secret.as_bytes() == bearer.as_bytes() {
        return Err("enterprise telemetry signing and bearer values must be distinct".into());
    }
    let signature = decode_signature(&managed.enrollment.signature_hex)?;
    let canonical = canonical_enrollment_bytes(
        &serde_json::to_value(claims)
            .map_err(|error| format!("cannot canonicalize telemetry enrollment: {error}"))?,
    )?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| "invalid telemetry verification secret".to_string())?;
    mac.update(&canonical);
    mac.verify_slice(&signature)
        .map_err(|_| "enterprise telemetry enrollment signature mismatch".to_string())?;

    // Keep only identifiers which already satisfy the wire boundary. No
    // enrollment, ledger, identity, or spool mutation may precede this check.
    let enrollment_id = VerifiedIdentifier::parse(&claims.enrollment_id)?;
    let organization_id = VerifiedIdentifier::parse(&claims.organization_id)?;
    let workspace_id = VerifiedIdentifier::parse(&claims.workspace_id)?;

    let mut sink = Sha256::new();
    sink.update(b"stella.enterprise.telemetry.sink.v1");
    for value in [
        claims.schema.as_str(),
        claims.issuer.as_str(),
        claims.audience.as_str(),
        enrollment_id.as_str(),
        organization_id.as_str(),
        workspace_id.as_str(),
        claims.endpoint.as_str(),
    ] {
        let len = u32::try_from(value.len()).map_err(|_| "telemetry sink field too large")?;
        sink.update(len.to_be_bytes());
        sink.update(value.as_bytes());
    }
    let sink_fingerprint = format!(
        "sink_{}",
        sink.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    Ok(VerifiedEnrollment {
        enrollment_id,
        organization_id,
        workspace_id,
        model_catalog: claims.model_catalog.clone(),
        endpoint,
        credential_env: claims.credential_env.clone(),
        verification_secret_env: managed.verification_secret_env.clone(),
        expires_at_unix_s: claims.expires_at_unix_s,
        sink_fingerprint,
    })
}

fn validate_policy_list(values: &[String], label: &str) -> Result<(), String> {
    if values.is_empty() || values.len() > MAX_POLICY_ENTRIES {
        return Err(format!(
            "managed telemetry {label} allowlist must contain 1..={MAX_POLICY_ENTRIES} entries"
        ));
    }
    if values.iter().any(|value| {
        value.is_empty()
            || value.len() > 128
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
    }) {
        return Err(format!("managed telemetry {label} allowlist is invalid"));
    }
    Ok(())
}

fn validate_env_ref(value: &str, label: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= MAX_ENV_REF_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        && value.as_bytes()[0].is_ascii_uppercase();
    if valid {
        Ok(())
    } else {
        Err(format!(
            "enterprise telemetry {label} env reference is invalid"
        ))
    }
}

fn strict_https_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "enterprise telemetry endpoint is not a URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "enterprise telemetry endpoint must be credential-free HTTPS without query or fragment"
                .into(),
        );
    }
    Ok(url)
}

fn decode_signature(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("enterprise telemetry signature must be 64 hexadecimal characters".into());
    }
    let mut bytes = [0_u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| "enterprise telemetry signature is invalid")?;
    }
    Ok(bytes)
}

/// Resolve the separate host spool, refusing any path addressable from the workspace.
pub(crate) fn host_spool_path(workspace_root: &Path) -> Result<PathBuf, String> {
    let workspace = workspace_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve workspace for telemetry: {error}"))?;
    let data = std::path::absolute(stella_store::usage::data_dir())
        .map_err(|error| format!("cannot resolve host data directory: {error}"))?;
    if data.starts_with(&workspace) {
        return Err("enterprise telemetry host data directory is inside the workspace".into());
    }
    stella_store::enterprise_telemetry::ensure_trusted_host_data_dir(&data)
        .map_err(|error| error.to_string())?;
    let data = data
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize enterprise telemetry host data: {error}"))?;
    if data.starts_with(&workspace) {
        return Err("enterprise telemetry host data resolves inside the workspace".into());
    }
    Ok(data.join("enterprise-telemetry.db"))
}

#[async_trait]
pub(crate) trait BatchSender: Send + Sync {
    async fn send(
        &self,
        endpoint: &Url,
        bearer_token: &str,
        events: &[StellaOperationalEventV1],
    ) -> Result<(), String>;
}

struct ReqwestBatchSender {
    client: Client,
}

impl ReqwestBatchSender {
    fn new() -> Result<Self, String> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .user_agent(concat!("stella/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                format!("cannot construct enterprise telemetry HTTP client: {error}")
            })?;
        Ok(Self { client })
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct OperationalBatch<'a> {
    schema: &'static str,
    events: &'a [StellaOperationalEventV1],
}

#[async_trait]
impl BatchSender for ReqwestBatchSender {
    async fn send(
        &self,
        endpoint: &Url,
        bearer_token: &str,
        events: &[StellaOperationalEventV1],
    ) -> Result<(), String> {
        if events.is_empty() || events.len() > MAX_BATCH_EVENTS {
            return Err("enterprise telemetry batch count is out of bounds".into());
        }
        let body = serde_json::to_vec(&OperationalBatch {
            schema: "stella.operational.batch.v1",
            events,
        })
        .map_err(|error| format!("cannot serialize enterprise telemetry batch: {error}"))?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err("enterprise telemetry request body exceeds 256 KiB".into());
        }
        let response = self
            .client
            .post(endpoint.clone())
            .bearer_auth(bearer_token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| format!("enterprise telemetry request failed: {error}"))?;
        let status = response.status();
        validate_response_status(status)?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err("enterprise telemetry response exceeds 64 KiB".into());
        }
        let mut received = 0usize;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| format!("telemetry response read failed: {error}"))?;
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                return Err("enterprise telemetry response exceeds 64 KiB".into());
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_response_status(status: reqwest::StatusCode) -> Result<(), String> {
    if status.is_redirection() {
        return Err("enterprise telemetry redirects are refused".into());
    }
    if !status.is_success() {
        return Err(format!(
            "enterprise telemetry endpoint returned HTTP {status}"
        ));
    }
    Ok(())
}

/// Verified enrollment plus its bounded spool and delivery adapter.
pub(crate) struct EnterpriseTelemetryRuntime {
    enrollment: VerifiedEnrollment,
    identity: OperationalIdentity,
    spool: EnterpriseTelemetrySpool,
    sender: Arc<dyn BatchSender>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalizationEnqueueOutcome {
    Disabled,
    Ineligible,
    Retained,
    Duplicate,
    DroppedNew,
}

/// Build only after a managed enrollment exists; `None` performs zero I/O.
pub(crate) fn build_runtime_from_managed<F>(
    managed: Option<&Value>,
    workspace_root: &Path,
    now_unix_s: i64,
    build_sender: F,
) -> Result<Option<EnterpriseTelemetryRuntime>, String>
where
    F: FnOnce() -> Result<Arc<dyn BatchSender>, String>,
{
    let Some(managed) = managed else {
        return Ok(None);
    };
    let enrollment = verify_managed_enrollment(managed, now_unix_s)?;
    activate_process_free_authority(&enrollment, workspace_root)?;
    let spool_path = host_spool_path(workspace_root)?;
    let store = stella_store::Store::open(workspace_root).map_err(|error| error.to_string())?;
    store
        .begin_enterprise_enrollment(&enrollment.sink_fingerprint)
        .map_err(|error| error.to_string())?;
    let identity = operational_identity(&store, &spool_path)?;
    let spool = EnterpriseTelemetrySpool::open_at(&spool_path, SpoolLimits::default())
        .map_err(|error| error.to_string())?;
    let sender = build_sender()?;
    let runtime = EnterpriseTelemetryRuntime {
        enrollment,
        identity,
        spool,
        sender,
    };
    // Replay durable fail-open intents from earlier post-enrollment closeout.
    for pending in store
        .pending_enterprise_export_page(
            &runtime.enrollment.sink_fingerprint,
            None,
            MAX_BACKFILL_ROWS_PER_RUNTIME,
        )
        .map_err(|error| error.to_string())?
    {
        let Some(rollup) = store
            .execution_rollup(pending.execution_id, workspace_root)
            .map_err(|error| error.to_string())?
        else {
            store
                .mark_enterprise_export_skipped(
                    &runtime.enrollment.sink_fingerprint,
                    pending.execution_id,
                    EnterpriseExportSkipReason::MissingRollup,
                )
                .map_err(|error| error.to_string())?;
            continue;
        };
        let event = match project_backfill_event(
            &runtime.enrollment,
            runtime.identity.clone(),
            &pending.export_nonce,
            &rollup,
        ) {
            Ok(event) => event,
            Err(reason) => {
                // Legacy rows predate the closed nonce/rollup boundary. Mark
                // malformed candidates skipped so they cannot pin the first
                // page forever or prevent later valid work from advancing.
                store
                    .mark_enterprise_export_skipped(
                        &runtime.enrollment.sink_fingerprint,
                        pending.execution_id,
                        reason,
                    )
                    .map_err(|error| error.to_string())?;
                continue;
            }
        };
        match runtime
            .spool
            .enqueue(
                &runtime.enrollment.sink_fingerprint,
                &event,
                now_unix_s.saturating_mul(1_000),
            )
            .map_err(|error| error.to_string())?
        {
            EnqueueOutcome::Retained | EnqueueOutcome::Duplicate => store
                .mark_enterprise_export_spooled(
                    &runtime.enrollment.sink_fingerprint,
                    pending.execution_id,
                )
                .map_err(|error| error.to_string())?,
            EnqueueOutcome::DroppedNew => {}
        }
    }
    store
        .compact_enterprise_export_ledger(
            &runtime.enrollment.sink_fingerprint,
            COMPLETED_LEDGER_RETENTION_ROWS,
        )
        .map_err(|error| error.to_string())?;
    Ok(Some(runtime))
}

fn project_backfill_event(
    enrollment: &VerifiedEnrollment,
    identity: OperationalIdentity,
    export_nonce: &str,
    rollup: &stella_store::usage::ExecutionRollupRow,
) -> Result<StellaOperationalEventV1, EnterpriseExportSkipReason> {
    let context = enrollment
        .context(identity, export_nonce)
        .map_err(|_| EnterpriseExportSkipReason::MalformedNonce)?;
    StellaOperationalEventV1::from_finalized_rollup(&context, rollup)
        .map_err(|_| EnterpriseExportSkipReason::MalformedRollup)
}

fn operational_identity(
    store: &stella_store::Store,
    spool_path: &Path,
) -> Result<OperationalIdentity, String> {
    let installation_uuid = stella_store::enterprise_telemetry::load_or_create_installation_uuid(
        spool_path
            .parent()
            .ok_or_else(|| "enterprise telemetry spool has no parent".to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let store_uuid = store
        .enterprise_store_uuid()
        .map_err(|error| error.to_string())?;
    OperationalIdentity::new(&installation_uuid, &store_uuid).map_err(|error| error.to_string())
}

impl EnterpriseTelemetryRuntime {
    #[cfg(test)]
    pub(crate) fn enqueue_rollup(
        &self,
        rollup: &ExecutionRollupRow,
        now_ms: i64,
    ) -> Result<EnqueueOutcome, String> {
        let context = self
            .enrollment
            .context(self.identity.clone(), "00000000000000000000000000000000")?;
        let event = StellaOperationalEventV1::from_finalized_rollup(&context, rollup)
            .map_err(|error| error.to_string())?;
        self.spool
            .enqueue(&self.enrollment.sink_fingerprint, &event, now_ms)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn status(&self) -> Result<SpoolStatus, String> {
        self.spool
            .status_for_sink(&self.enrollment.sink_fingerprint)
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn flush(&self) -> Result<usize, String> {
        let (_, now_ms) = unix_time()?;
        self.flush_with_retry_clock(now_ms, || unix_time().map(|(_, millis)| millis))
            .await
    }

    pub(crate) async fn flush_with_retry_clock<F>(
        &self,
        now_ms: i64,
        retry_clock: F,
    ) -> Result<usize, String>
    where
        F: Fn() -> Result<i64, String>,
    {
        let now_s = now_ms.div_euclid(1_000);
        if now_s >= self.enrollment.expires_at_unix_s {
            return Err("enterprise telemetry enrollment expired before delivery".into());
        }
        static CLAIM_SEQUENCE: AtomicU64 = AtomicU64::new(1);
        let sequence = CLAIM_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let owner = format!("pid-{}-{now_ms}-{sequence}", std::process::id());
        let observed_clock = self
            .spool
            .observe_claim_clock(&self.enrollment.sink_fingerprint, now_ms)
            .map_err(|error| error.to_string())?;
        let claimed = self
            .spool
            .claim_batch(
                &self.enrollment.sink_fingerprint,
                &owner,
                observed_clock,
                LEASE_MS,
                MAX_BATCH_EVENTS,
                MAX_REQUEST_BYTES,
            )
            .map_err(|error| error.to_string())?;
        if claimed.is_empty() {
            return Ok(0);
        }
        let token = match std::env::var(&self.enrollment.credential_env) {
            Ok(value) if !value.is_empty() && value.len() <= MAX_BEARER_BYTES => value,
            _ => {
                self.spool
                    .retry(
                        &self.enrollment.sink_fingerprint,
                        &owner,
                        &claimed,
                        retry_clock()?,
                    )
                    .map_err(|error| error.to_string())?;
                return Err(
                    "enterprise telemetry bearer credential is unavailable or invalid".into(),
                );
            }
        };
        let signing_secret = match std::env::var(&self.enrollment.verification_secret_env) {
            Ok(value) if (32..=MAX_SECRET_BYTES).contains(&value.len()) => value,
            _ => {
                self.spool
                    .retry(
                        &self.enrollment.sink_fingerprint,
                        &owner,
                        &claimed,
                        retry_clock()?,
                    )
                    .map_err(|error| error.to_string())?;
                return Err("enterprise telemetry verification secret is unavailable".into());
            }
        };
        if signing_secret.as_bytes() == token.as_bytes() {
            self.spool
                .retry(
                    &self.enrollment.sink_fingerprint,
                    &owner,
                    &claimed,
                    retry_clock()?,
                )
                .map_err(|error| error.to_string())?;
            return Err("enterprise telemetry signing and bearer values must be distinct".into());
        }
        let events: Vec<_> = claimed.iter().map(|item| item.event.clone()).collect();
        match self
            .sender
            .send(&self.enrollment.endpoint, &token, &events)
            .await
        {
            Ok(()) => {
                self.spool
                    .ack(&self.enrollment.sink_fingerprint, &owner, &claimed)
                    .map_err(|error| error.to_string())?;
                Ok(claimed.len())
            }
            Err(error) => {
                self.spool
                    .retry(
                        &self.enrollment.sink_fingerprint,
                        &owner,
                        &claimed,
                        retry_clock()?,
                    )
                    .map_err(|retry_error| {
                        format!("{error}; retry persistence failed: {retry_error}")
                    })?;
                Err(error)
            }
        }
    }
}

fn production_sender() -> Result<Arc<dyn BatchSender>, String> {
    Ok(Arc::new(ReqwestBatchSender::new()?))
}

fn unix_time() -> Result<(i64, i64), String> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock precedes the Unix epoch".to_string())?;
    let seconds = i64::try_from(duration.as_secs())
        .map_err(|_| "system clock is outside telemetry range".to_string())?;
    let millis = i64::try_from(duration.as_millis())
        .map_err(|_| "system clock is outside telemetry range".to_string())?;
    Ok((seconds, millis))
}

fn enrolled_spool(
    managed: Option<&Value>,
    workspace_root: &Path,
    now_unix_s: i64,
) -> Result<Option<(VerifiedEnrollment, EnterpriseTelemetrySpool)>, String> {
    let Some(managed) = managed else {
        return Ok(None);
    };
    let enrollment = verify_managed_enrollment(managed, now_unix_s)?;
    activate_process_free_authority(&enrollment, workspace_root)?;
    let path = host_spool_path(workspace_root)?;
    let spool = EnterpriseTelemetrySpool::open_at(&path, SpoolLimits::default())
        .map_err(|error| error.to_string())?;
    Ok(Some((enrollment, spool)))
}

/// Provider-free `stella telemetry status|flush` entry point.
pub(crate) fn run_command(command: TelemetryCmd) -> Result<(), String> {
    let workspace =
        std::env::current_dir().map_err(|error| format!("cannot determine workspace: {error}"))?;
    let settings = crate::settings::Settings::load(&workspace)?;
    let (now_s, _) = unix_time()?;
    match command {
        TelemetryCmd::Status => {
            let Some((enrollment, spool)) =
                enrolled_spool(settings.managed_enterprise_telemetry(), &workspace, now_s)?
            else {
                println!("enterprise telemetry: disabled (no managed enrollment)");
                return Ok(());
            };
            let status = spool
                .status_for_sink(&enrollment.sink_fingerprint)
                .map_err(|error| error.to_string())?;
            let ledger = stella_store::Store::open(&workspace)
                .map_err(|error| error.to_string())?
                .enterprise_export_ledger_status(&enrollment.sink_fingerprint)
                .map_err(|error| error.to_string())?;
            println!(
                "enterprise telemetry: enrolled; pending={} ({} bytes); stranded={} ({} bytes); quarantine={} ({} metadata bytes); ledger_skipped={} (missing_rollup={} malformed_nonce={} malformed_rollup={}); physical={} bytes; dropped={}; corrupt_dropped={}; rollover_discarded={}",
                status.pending_rows,
                status.pending_payload_bytes,
                status.stranded_rows,
                status.stranded_payload_bytes,
                status.quarantine_diagnostic_rows,
                status.quarantine_diagnostic_bytes,
                ledger.skipped_rows,
                ledger.missing_rollup_rows,
                ledger.malformed_nonce_rows,
                ledger.malformed_rollup_rows,
                status.physical_bytes,
                status.dropped_rows,
                status.corrupt_dropped_rows,
                status.rollover_discarded_rows
            );
            Ok(())
        }
        TelemetryCmd::Flush => {
            let Some(runtime) = build_runtime_from_managed(
                settings.managed_enterprise_telemetry(),
                &workspace,
                now_s,
                production_sender,
            )?
            else {
                println!("enterprise telemetry: disabled (no managed enrollment)");
                return Ok(());
            };
            let runtime_handle = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("cannot start telemetry runtime: {error}"))?;
            let sent = runtime_handle.block_on(runtime.flush())?;
            let status = runtime.status()?;
            println!(
                "enterprise telemetry: flushed={sent}; pending={}; dropped={}",
                status.pending_rows, status.dropped_rows
            );
            Ok(())
        }
        TelemetryCmd::RolloverDiscard => {
            let Some((enrollment, spool)) =
                enrolled_spool(settings.managed_enterprise_telemetry(), &workspace, now_s)?
            else {
                println!("enterprise telemetry: disabled (no managed enrollment)");
                return Ok(());
            };
            let discarded = spool
                .discard_stranded(&enrollment.sink_fingerprint)
                .map_err(|error| error.to_string())?;
            println!(
                "enterprise telemetry: explicitly discarded {discarded} stranded rollover rows"
            );
            Ok(())
        }
    }
}

/// Fail-open post-finalization derivation. No HTTP client or socket is built.
pub(crate) fn enqueue_finalized_execution(
    store: &stella_store::Store,
    execution_id: i64,
) -> Result<FinalizationEnqueueOutcome, String> {
    let Some(workspace) = store.workspace_root() else {
        return Ok(FinalizationEnqueueOutcome::Disabled);
    };
    let settings = crate::settings::Settings::load(workspace)?;
    let (now_s, now_ms) = unix_time()?;
    let Some(managed) = settings.managed_enterprise_telemetry() else {
        return Ok(FinalizationEnqueueOutcome::Disabled);
    };
    let enrollment = verify_managed_enrollment(managed, now_s)?;
    activate_process_free_authority(&enrollment, workspace)?;
    // Durable intent precedes every host-spool filesystem operation.
    let Some(export_nonce) = store
        .mark_enterprise_export_pending(&enrollment.sink_fingerprint, execution_id)
        .map_err(|error| error.to_string())?
    else {
        return Ok(FinalizationEnqueueOutcome::Ineligible);
    };
    let Some(rollup) = store
        .execution_rollup(execution_id, workspace)
        .map_err(|error| error.to_string())?
    else {
        return Ok(FinalizationEnqueueOutcome::Ineligible);
    };
    let spool_path = host_spool_path(workspace)?;
    let spool = EnterpriseTelemetrySpool::open_at(&spool_path, SpoolLimits::default())
        .map_err(|error| error.to_string())?;
    let context = enrollment.context(operational_identity(store, &spool_path)?, &export_nonce)?;
    let event = StellaOperationalEventV1::from_finalized_rollup(&context, &rollup)
        .map_err(|error| error.to_string())?;
    let outcome = spool
        .enqueue(&enrollment.sink_fingerprint, &event, now_ms)
        .map_err(|error| error.to_string())?;
    if matches!(
        outcome,
        EnqueueOutcome::Retained | EnqueueOutcome::Duplicate
    ) {
        store
            .mark_enterprise_export_spooled(&enrollment.sink_fingerprint, execution_id)
            .map_err(|error| error.to_string())?;
    }
    Ok(match outcome {
        EnqueueOutcome::Retained => FinalizationEnqueueOutcome::Retained,
        EnqueueOutcome::Duplicate => FinalizationEnqueueOutcome::Duplicate,
        EnqueueOutcome::DroppedNew => FinalizationEnqueueOutcome::DroppedNew,
    })
}

/// Startup-only detached flush. It cannot delay execution or process exit.
pub(crate) fn start_best_effort_flush() {
    let Ok(workspace) = std::env::current_dir() else {
        return;
    };
    let Ok(settings) = crate::settings::Settings::load(&workspace) else {
        return;
    };
    let Some(managed) = settings.managed_enterprise_telemetry().cloned() else {
        return;
    };
    let Ok((startup_s, _)) = unix_time() else {
        return;
    };
    // Verify synchronously so sensitive env names are registered before any
    // model-controlled tool or hook can spawn. Network remains detached.
    let enrollment = match verify_managed_enrollment(&managed, startup_s) {
        Ok(enrollment) => enrollment,
        Err(error) => {
            eprintln!("warning: enterprise telemetry enrollment is inactive: {error}");
            return;
        }
    };
    if let Err(error) = activate_process_free_authority(&enrollment, &workspace) {
        eprintln!("warning: enterprise telemetry authority failed closed: {error}");
        return;
    }
    if let Err(error) = host_spool_path(&workspace) {
        eprintln!(
            "warning: enterprise telemetry delivery is unavailable; process-free authority remains active: {error}"
        );
        return;
    }
    let store = match stella_store::Store::open(&workspace) {
        Ok(store) => store,
        Err(error) => {
            eprintln!(
                "warning: enterprise telemetry delivery is unavailable; process-free authority remains active: {error}"
            );
            return;
        }
    };
    if let Err(error) = store.begin_enterprise_enrollment(&enrollment.sink_fingerprint) {
        eprintln!(
            "warning: enterprise telemetry delivery is unavailable; process-free authority remains active: {error}"
        );
        return;
    }
    std::thread::spawn(move || {
        let Ok((now_s, _)) = unix_time() else {
            return;
        };
        let Ok(Some(runtime)) =
            build_runtime_from_managed(Some(&managed), &workspace, now_s, production_sender)
        else {
            return;
        };
        let Ok(handle) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let _ = handle.block_on(runtime.flush());
    });
}
