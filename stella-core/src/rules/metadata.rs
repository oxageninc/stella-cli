//! Context-as-code rule metadata parsing, validation, and rendering.

use std::collections::HashMap;

use super::Frontmatter;

/// The record class published by a context-as-code rule file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleRecordKind {
    /// A rule is a normative directive, rather than a memory or observation.
    Directive,
}

impl RuleRecordKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "directive" => Some(Self::Directive),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Directive => "directive",
        }
    }
}

/// The declared enforcement level of a context-as-code rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleEnforcement {
    /// Inform reviewers without adding an enforcement expectation.
    Informational,
    /// Inject into the prompt as reviewable steering.
    Advisory,
    /// Declares blocking intent; current guard behavior remains independent.
    Blocking,
}

impl RuleEnforcement {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "informational" => Some(Self::Informational),
            "advisory" => Some(Self::Advisory),
            "blocking" => Some(Self::Blocking),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Informational => "informational",
            Self::Advisory => "advisory",
            Self::Blocking => "blocking",
        }
    }
}

/// The provenance class declared for a context-as-code rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleOrigin {
    /// Explicitly supplied by a user.
    User,
    /// Supplied by a system-owned policy source.
    System,
    /// Promoted from independently supported local evidence.
    Inferred,
    /// Imported from another compatible source.
    Imported,
}

impl RuleOrigin {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "system" => Some(Self::System),
            "inferred" => Some(Self::Inferred),
            "imported" => Some(Self::Imported),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
            Self::Inferred => "inferred",
            Self::Imported => "imported",
        }
    }
}

/// Safe, reviewable metadata associated with a published rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMetadata {
    /// Version of the context-as-code metadata schema.
    pub schema_version: String,
    /// Stable identifier for this immutable record revision.
    pub record_id: String,
    /// The published record's class.
    pub record_kind: RuleRecordKind,
    /// Repository-relative paths to which the rule applies.
    pub scope_paths: Vec<String>,
    /// Declared policy enforcement level.
    pub enforcement: RuleEnforcement,
    /// Confidence in the range 0 through 100.
    pub confidence: u8,
    /// Source classification for the published directive.
    pub origin: RuleOrigin,
    /// Stable evidence identifiers only; raw evidence remains private.
    pub supporting_evidence_ids: Vec<String>,
    /// When Stella recorded the supported directive.
    pub observed_at: String,
    /// When the directive became applicable.
    pub valid_from: String,
    /// Exclusive end of applicability, when known.
    pub valid_until: Option<String>,
}

/// A validation problem found in optional context-as-code metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleMetadataError {
    /// A metadata key was specified more than once in one frontmatter block.
    DuplicateKey(String),
    /// A metadata-bearing rule omitted a required field.
    MissingField(&'static str),
    /// The metadata schema is not supported by this reader.
    InvalidSchemaVersion(String),
    /// An enum field contains a value outside its published vocabulary.
    InvalidEnum { field: &'static str, value: String },
    /// Confidence must be an integer from 0 through 100.
    InvalidConfidence(String),
    /// A timestamp is not a canonical UTC RFC 3339 instant.
    InvalidTimestamp { field: &'static str, value: String },
    /// The exclusive end of applicability is not after its start.
    InvalidValidityRange,
    /// A list expected to contain stable identifiers or paths was empty.
    EmptyList { field: &'static str },
}

const METADATA_FIELDS: [&str; 11] = [
    "schema_version",
    "record_id",
    "record_kind",
    "scope_paths",
    "enforcement",
    "confidence",
    "origin",
    "supporting_evidence_ids",
    "observed_at",
    "valid_from",
    "valid_until",
];

fn required_value<'a>(
    data: &'a HashMap<String, String>,
    field: &'static str,
    errors: &mut Vec<RuleMetadataError>,
) -> Option<&'a str> {
    match data
        .get(field)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        Some(value) => Some(value),
        None => {
            errors.push(RuleMetadataError::MissingField(field));
            None
        }
    }
}

fn canonical_list(value: &str) -> Vec<String> {
    let mut values: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    values.sort();
    values.dedup();
    values
}

fn is_canonical_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .into_iter()
            .all(|index| bytes[index].is_ascii_digit())
}

fn validate_timestamp(
    field: &'static str,
    value: &str,
    errors: &mut Vec<RuleMetadataError>,
) -> Option<String> {
    if is_canonical_timestamp(value) {
        Some(value.to_string())
    } else {
        errors.push(RuleMetadataError::InvalidTimestamp {
            field,
            value: value.to_string(),
        });
        None
    }
}

pub(super) fn metadata_from_frontmatter(
    fm: &Frontmatter,
) -> Result<Option<RuleMetadata>, Vec<RuleMetadataError>> {
    let has_metadata = METADATA_FIELDS
        .iter()
        .any(|field| fm.data.contains_key(*field));
    if !has_metadata {
        return Ok(None);
    }

    let mut errors: Vec<RuleMetadataError> = fm
        .duplicate_keys
        .iter()
        .filter(|key| METADATA_FIELDS.contains(&key.as_str()))
        .cloned()
        .map(RuleMetadataError::DuplicateKey)
        .collect();

    let schema_version =
        required_value(&fm.data, "schema_version", &mut errors).and_then(|value| {
            if value == "1.0-draft" {
                Some(value.to_string())
            } else {
                errors.push(RuleMetadataError::InvalidSchemaVersion(value.to_string()));
                None
            }
        });
    let record_id = required_value(&fm.data, "record_id", &mut errors).map(ToOwned::to_owned);
    let record_kind = required_value(&fm.data, "record_kind", &mut errors).and_then(|value| {
        RuleRecordKind::parse(value).or_else(|| {
            errors.push(RuleMetadataError::InvalidEnum {
                field: "record_kind",
                value: value.to_string(),
            });
            None
        })
    });
    let scope_paths = required_value(&fm.data, "scope_paths", &mut errors).map(|value| {
        let values = canonical_list(value);
        if values.is_empty() {
            errors.push(RuleMetadataError::EmptyList {
                field: "scope_paths",
            });
        }
        values
    });
    let enforcement = required_value(&fm.data, "enforcement", &mut errors).and_then(|value| {
        RuleEnforcement::parse(value).or_else(|| {
            errors.push(RuleMetadataError::InvalidEnum {
                field: "enforcement",
                value: value.to_string(),
            });
            None
        })
    });
    let confidence = required_value(&fm.data, "confidence", &mut errors).and_then(|value| {
        value
            .parse::<u8>()
            .ok()
            .filter(|confidence| *confidence <= 100)
            .or_else(|| {
                errors.push(RuleMetadataError::InvalidConfidence(value.to_string()));
                None
            })
    });
    let origin = required_value(&fm.data, "origin", &mut errors).and_then(|value| {
        RuleOrigin::parse(value).or_else(|| {
            errors.push(RuleMetadataError::InvalidEnum {
                field: "origin",
                value: value.to_string(),
            });
            None
        })
    });
    let supporting_evidence_ids = required_value(&fm.data, "supporting_evidence_ids", &mut errors)
        .map(|value| {
            let values = canonical_list(value);
            if values.is_empty() {
                errors.push(RuleMetadataError::EmptyList {
                    field: "supporting_evidence_ids",
                });
            }
            values
        });
    let observed_at = required_value(&fm.data, "observed_at", &mut errors)
        .and_then(|value| validate_timestamp("observed_at", value, &mut errors));
    let valid_from = required_value(&fm.data, "valid_from", &mut errors)
        .and_then(|value| validate_timestamp("valid_from", value, &mut errors));
    let valid_until = fm
        .data
        .get("valid_until")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .and_then(|value| validate_timestamp("valid_until", value, &mut errors));

    if let (Some(valid_from), Some(valid_until)) = (&valid_from, &valid_until)
        && valid_until <= valid_from
    {
        errors.push(RuleMetadataError::InvalidValidityRange);
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    match (
        schema_version,
        record_id,
        record_kind,
        scope_paths,
        enforcement,
        confidence,
        origin,
        supporting_evidence_ids,
        observed_at,
        valid_from,
    ) {
        (
            Some(schema_version),
            Some(record_id),
            Some(record_kind),
            Some(scope_paths),
            Some(enforcement),
            Some(confidence),
            Some(origin),
            Some(supporting_evidence_ids),
            Some(observed_at),
            Some(valid_from),
        ) => Ok(Some(RuleMetadata {
            schema_version,
            record_id,
            record_kind,
            scope_paths,
            enforcement,
            confidence,
            origin,
            supporting_evidence_ids,
            observed_at,
            valid_from,
            valid_until,
        })),
        _ => Err(errors),
    }
}

/// Render valid context-as-code metadata in its canonical frontmatter order.
/// The result omits the surrounding `---` fence so callers can compose it
/// with rule name, description, and legacy-compatible guard fields.
pub fn render_rule_metadata(metadata: &RuleMetadata) -> String {
    let mut lines = vec![
        format!("schema_version: {}", metadata.schema_version),
        format!("record_id: {}", metadata.record_id),
        format!("record_kind: {}", metadata.record_kind.as_str()),
        "scope_paths:".to_string(),
    ];
    for path in canonical_list(&metadata.scope_paths.join(",")) {
        lines.push(format!("  - {path}"));
    }
    lines.extend([
        format!("enforcement: {}", metadata.enforcement.as_str()),
        format!("confidence: {}", metadata.confidence),
        format!("origin: {}", metadata.origin.as_str()),
        "supporting_evidence_ids:".to_string(),
    ]);
    for evidence_id in canonical_list(&metadata.supporting_evidence_ids.join(",")) {
        lines.push(format!("  - {evidence_id}"));
    }
    lines.push(format!("observed_at: {}", metadata.observed_at));
    lines.push(format!("valid_from: {}", metadata.valid_from));
    if let Some(valid_until) = &metadata.valid_until {
        lines.push(format!("valid_until: {valid_until}"));
    }
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::rule_from_file;

    #[test]
    fn rule_from_file_parses_and_canonically_renders_context_metadata() {
        let raw = "---\nname: api-coverage\nschema_version: 1.0-draft\nrecord_id: dir_api_coverage_v1\nrecord_kind: directive\nscope_paths:\n  - src/api/**\n  - docs/api/**\nenforcement: advisory\nconfidence: 91\norigin: inferred\nsupporting_evidence_ids:\n  - ev_task_118\n  - ev_task_101\nobserved_at: 2026-07-20T18:30:00Z\nvalid_from: 2026-07-20T18:30:00Z\n---\nAPI endpoint changes require integration coverage.";

        let rule = rule_from_file(".stella/rules/api-coverage.md", raw).unwrap();
        let metadata = rule.metadata.as_ref().unwrap();

        assert!(rule.metadata_errors.is_empty());
        assert_eq!(metadata.record_id, "dir_api_coverage_v1");
        assert_eq!(metadata.record_kind, RuleRecordKind::Directive);
        assert_eq!(metadata.scope_paths, vec!["docs/api/**", "src/api/**"]);
        assert_eq!(metadata.enforcement, RuleEnforcement::Advisory);
        assert_eq!(metadata.confidence, 91);
        assert_eq!(metadata.origin, RuleOrigin::Inferred);
        assert_eq!(
            metadata.supporting_evidence_ids,
            vec!["ev_task_101", "ev_task_118"]
        );
        assert_eq!(
            render_rule_metadata(metadata),
            "schema_version: 1.0-draft\nrecord_id: dir_api_coverage_v1\nrecord_kind: directive\nscope_paths:\n  - docs/api/**\n  - src/api/**\nenforcement: advisory\nconfidence: 91\norigin: inferred\nsupporting_evidence_ids:\n  - ev_task_101\n  - ev_task_118\nobserved_at: 2026-07-20T18:30:00Z\nvalid_from: 2026-07-20T18:30:00Z\n"
        );
    }

    #[test]
    fn rule_from_file_keeps_invalid_metadata_distinguishable_for_linting() {
        let raw = "---\nschema_version: 1.0-draft\nrecord_id: dir_api_coverage_v1\nrecord_kind: directive\nscope_paths: src/api/**\nenforcement: required\nconfidence: 101\norigin: inferred\nsupporting_evidence_ids: ev_task_101\nobserved_at: 2026-07-20T18:30:00Z\nvalid_from: 2026-07-20T18:30:00Z\nrecord_id: dir_api_coverage_v2\n---\nAPI endpoint changes require integration coverage.";

        let rule = rule_from_file(".stella/rules/api-coverage.md", raw).unwrap();

        assert!(rule.metadata.is_none());
        assert_eq!(
            rule.metadata_errors,
            vec![
                RuleMetadataError::DuplicateKey("record_id".to_string()),
                RuleMetadataError::InvalidEnum {
                    field: "enforcement",
                    value: "required".to_string(),
                },
                RuleMetadataError::InvalidConfidence("101".to_string()),
            ]
        );
        assert_eq!(
            rule.text,
            "API endpoint changes require integration coverage."
        );
    }
}
