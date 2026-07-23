//! Artifact-contract and contract-validation value types (lifecycle §8.12–8.13).
//!
//! Two spec points are handled per the flagged-decisions issue (#483) with a
//! documented, non-freezing choice: **requirement kinds and the validation
//! `method` are extensible strings**, not closed enums (the spec never names the
//! semantic-judge method token, and requirement kinds are explicitly extensible
//! with "unknown kinds fail closed"). `validation_status` keeps its four named
//! values; `requirement_status` gets its own enum.
//!
//! The "exactly one result for every requirement in the referenced contract
//! version, no unknown ids" rule is **referential** (it spans the contract and
//! the validation) — only the intra-record half (no *duplicate* result ids, and
//! duplicates force `error`) is enforced here; coverage/unknown-id checks are the
//! repository's job.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::RecordValidationError;
use super::kind::Origin;
use super::scope::{Scope, SharingScope};

/// A requirement kind. Extensible: the ten recognized core kinds are exposed as
/// constants; an unrecognized kind is non-executable and fails closed when
/// required (enforced by the executor in a later phase).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RequirementKind(String);

impl RequirementKind {
    /// A file exists at a path.
    pub const FILE_EXISTS: &'static str = "file_exists";
    /// A directory exists at a path.
    pub const DIRECTORY_EXISTS: &'static str = "directory_exists";
    /// At least N files match a glob.
    pub const GLOB_MIN_COUNT: &'static str = "glob_min_count";
    /// A file has an expected MIME type.
    pub const MIME_TYPE: &'static str = "mime_type";
    /// An image has expected dimensions.
    pub const IMAGE_DIMENSIONS: &'static str = "image_dimensions";
    /// A file is within a size bound.
    pub const FILE_SIZE: &'static str = "file_size";
    /// A JSON document matches a schema.
    pub const JSON_SCHEMA: &'static str = "json_schema";
    /// A Markdown document has required sections.
    pub const MARKDOWN_SECTIONS: &'static str = "markdown_sections";
    /// A command exits successfully (needs `execution_approval_ref`).
    pub const COMMAND: &'static str = "command";
    /// A semantic judge scores against a rubric.
    pub const SEMANTIC_JUDGE: &'static str = "semantic_judge";

    const RECOGNIZED: [&'static str; 10] = [
        Self::FILE_EXISTS,
        Self::DIRECTORY_EXISTS,
        Self::GLOB_MIN_COUNT,
        Self::MIME_TYPE,
        Self::IMAGE_DIMENSIONS,
        Self::FILE_SIZE,
        Self::JSON_SCHEMA,
        Self::MARKDOWN_SECTIONS,
        Self::COMMAND,
        Self::SEMANTIC_JUDGE,
    ];

    /// Construct a requirement kind, rejecting an empty identifier.
    pub fn new(identifier: impl Into<String>) -> Result<Self, RecordValidationError> {
        let identifier = identifier.into();
        if identifier.is_empty() {
            return Err(RecordValidationError::invariant(
                "requirement_kind.must_be_nonempty",
            ));
        }
        Ok(Self(identifier))
    }

    /// The identifier string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is one of the ten recognized core kinds. An unrecognized
    /// kind is non-executable (fails closed when required).
    pub fn is_recognized(&self) -> bool {
        Self::RECOGNIZED.contains(&self.0.as_str())
    }

    /// Whether this is the `command` kind (which forces
    /// `execution_approval_ref` on the contract).
    pub fn is_command(&self) -> bool {
        self.0 == Self::COMMAND
    }
}

impl<'de> Deserialize<'de> for RequirementKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let identifier = String::deserialize(deserializer)?;
        RequirementKind::new(identifier).map_err(serde::de::Error::custom)
    }
}

/// One requirement of an [`ArtifactContract`]. Kind-specific parameters
/// (`glob`/`minimum`, `argv`/`timeout_ms`, `rubric_ref`, …) are carried in
/// `params` and interpreted by the executor in a later phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    /// Stable id, unique within the contract.
    pub requirement_id: String,
    /// The requirement kind.
    pub requirement_kind: RequirementKind,
    /// Whether the deliverable must satisfy this to be complete.
    pub required: bool,
    /// Kind-specific parameters (opaque here; validated by the executor).
    #[serde(skip_serializing_if = "serde_json::Map::is_empty", default)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// A contract that a produced artifact must satisfy (lifecycle §8.12). Carries
/// completion authority when selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactContract {
    /// Human name.
    pub name: String,
    /// Contract version.
    pub version: String,
    /// What it produces / checks.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    /// Provenance.
    pub origin: Origin,
    /// Where it applies.
    pub scope: Scope,
    /// Who it is shared with.
    pub sharing_scope: SharingScope,
    /// Root under which outputs are checked.
    pub output_root: String,
    /// The requirements.
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    /// Required when any requirement is `command`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub execution_approval_ref: Option<String>,
    /// When it was observed (RFC 3339 UTC).
    pub observed_at: String,
    /// Validity start (RFC 3339 UTC).
    pub valid_from: String,
}

impl ArtifactContract {
    /// Validate intra-record invariants: a `command` requirement forces an
    /// `execution_approval_ref`.
    pub fn validate(&self) -> Result<(), RecordValidationError> {
        let has_command = self
            .requirements
            .iter()
            .any(|r| r.requirement_kind.is_command());
        if has_command && self.execution_approval_ref.is_none() {
            return Err(RecordValidationError::invariant(
                "contract.command_requires_execution_approval_ref",
            ));
        }
        Ok(())
    }
}

/// The verdict of a whole contract validation (lifecycle §8.13). Four named
/// values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    Passed,
    Failed,
    Error,
    Skipped,
}

impl ValidationStatus {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Error => "error",
            Self::Skipped => "skipped",
        }
    }
}

/// The verdict for a single requirement. Its own enum (per #483): `error` is a
/// validation-level aggregate, not a per-requirement verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementStatus {
    Passed,
    Failed,
    Skipped,
}

impl RequirementStatus {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// One requirement's result within a [`ContractValidation`]. `method` is an
/// extensible identifier (e.g. `deterministic`, or a semantic-judge token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementResult {
    /// The requirement this result is for.
    pub requirement_id: String,
    /// The per-requirement verdict.
    pub requirement_status: RequirementStatus,
    /// How it was checked (extensible identifier).
    pub method: String,
    /// Optional human message.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub message: Option<String>,
}

/// A validation of an [`ArtifactContract`] (lifecycle §8.13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractValidation {
    /// The contract this validated.
    pub contract_record_id: String,
    /// The exact contract version validated.
    pub contract_version: String,
    /// The overall verdict.
    pub validation_status: ValidationStatus,
    /// Per-requirement results.
    #[serde(default)]
    pub results: Vec<RequirementResult>,
    /// When it was observed (RFC 3339 UTC).
    pub observed_at: String,
}

impl ContractValidation {
    /// Validate the intra-record half of the coverage rule: duplicate result
    /// ids force `validation_status == error`. (Full coverage / unknown-id
    /// checks against the referenced contract are the repository's job.)
    pub fn validate(&self) -> Result<(), RecordValidationError> {
        let mut seen = HashSet::new();
        let has_duplicate = self
            .results
            .iter()
            .any(|r| !seen.insert(r.requirement_id.as_str()));
        if has_duplicate && self.validation_status != ValidationStatus::Error {
            return Err(RecordValidationError::invariant(
                "contract_validation.duplicate_result_requires_error",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contract(kinds: &[&str]) -> ArtifactContract {
        ArtifactContract {
            name: "brand-kit".into(),
            version: "1".into(),
            description: None,
            origin: Origin::User,
            scope: Scope {
                repository_id: Some("repo_1".into()),
                ..Default::default()
            },
            sharing_scope: SharingScope::Repository,
            output_root: "out/".into(),
            requirements: kinds
                .iter()
                .enumerate()
                .map(|(i, k)| Requirement {
                    requirement_id: format!("req_{i}"),
                    requirement_kind: RequirementKind::new(*k).unwrap(),
                    required: true,
                    params: serde_json::Map::new(),
                })
                .collect(),
            execution_approval_ref: None,
            observed_at: "2026-07-20T18:30:00Z".into(),
            valid_from: "2026-07-20T18:30:00Z".into(),
        }
    }

    #[test]
    fn requirement_kind_recognition_and_extensibility() {
        assert!(RequirementKind::new("file_exists").unwrap().is_recognized());
        assert!(RequirementKind::new("command").unwrap().is_command());
        assert!(!RequirementKind::new("acme.custom").unwrap().is_recognized());
        assert!(RequirementKind::new("").is_err());
    }

    #[test]
    fn command_requirement_forces_execution_approval_ref() {
        let mut c = contract(&["file_exists", "command"]);
        assert_eq!(
            c.validate(),
            Err(RecordValidationError::invariant(
                "contract.command_requires_execution_approval_ref"
            ))
        );
        c.execution_approval_ref = Some("approval_1".into());
        assert!(c.validate().is_ok());
        // No command → no approval needed.
        assert!(contract(&["file_exists", "json_schema"]).validate().is_ok());
    }

    #[test]
    fn duplicate_result_ids_force_error_status() {
        let dup = |status| ContractValidation {
            contract_record_id: "ctr_1".into(),
            contract_version: "1".into(),
            validation_status: status,
            results: vec![
                RequirementResult {
                    requirement_id: "req_0".into(),
                    requirement_status: RequirementStatus::Passed,
                    method: "deterministic".into(),
                    message: None,
                },
                RequirementResult {
                    requirement_id: "req_0".into(),
                    requirement_status: RequirementStatus::Failed,
                    method: "deterministic".into(),
                    message: None,
                },
            ],
            observed_at: "2026-07-20T18:30:00Z".into(),
        };
        assert_eq!(
            dup(ValidationStatus::Passed).validate(),
            Err(RecordValidationError::invariant(
                "contract_validation.duplicate_result_requires_error"
            ))
        );
        assert!(dup(ValidationStatus::Error).validate().is_ok());
    }

    #[test]
    fn status_strings_are_canonical() {
        assert_eq!(
            serde_json::to_value(ValidationStatus::Error).unwrap(),
            json!("error")
        );
        assert_eq!(
            serde_json::to_value(RequirementStatus::Skipped).unwrap(),
            json!("skipped")
        );
    }
}
