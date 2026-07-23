//! The context-use / efficacy value types (lifecycle §8.x) — how a compiled
//! frame's records were used, whether they helped, and what context was missing.
//!
//! **Purity boundary (important):** these are pure value types. Two spec
//! invariants are inherently *referential* — "a `ContextUseFeedback` resolves to
//! exactly one identity-consistent `ContextUse`" and the `not_rendered`
//! selected-use back-reference resolution — and cannot be checked without the
//! record set. This module validates only the **intra-record** parts (required
//! ids present, internally consistent, non-empty evidence). Existence,
//! uniqueness, and cross-record resolution are the Phase 2/3 repository's job.

use serde::{Deserialize, Serialize};

use super::{Confidence, RecordValidationError};

/// How a context record was used within a task (lifecycle §). Closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextUseKind {
    Selected,
    Rendered,
    Cited,
}

impl ContextUseKind {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::Rendered => "rendered",
            Self::Cited => "cited",
        }
    }
}

/// A post-task judgement of whether a used record helped (lifecycle §). Closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextUseEvaluation {
    Helpful,
    NotHelpful,
    Neutral,
}

impl ContextUseEvaluation {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Helpful => "helpful",
            Self::NotHelpful => "not_helpful",
            Self::Neutral => "neutral",
        }
    }
}

/// Which stage of a task a record influenced (lifecycle §). `none` means it had
/// no opportunity to influence anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextInfluenceStage {
    Planning,
    Execution,
    Verification,
    FinalResponse,
    None,
}

impl ContextInfluenceStage {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Execution => "execution",
            Self::Verification => "verification",
            Self::FinalResponse => "final_response",
            Self::None => "none",
        }
    }
}

/// How a record related to the task outcome (lifecycle §). Closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextOutcomeRelation {
    Supported,
    Contradicted,
    Unrelated,
    Unknown,
}

impl ContextOutcomeRelation {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Contradicted => "contradicted",
            Self::Unrelated => "unrelated",
            Self::Unknown => "unknown",
        }
    }
}

/// The method by which a use was evaluated (lifecycle §). **Extensible** — a
/// writer must emit a non-empty registered or versioned identifier, so this is a
/// validated string newtype, not a closed enum. The seven recognized *core*
/// methods are exposed as constants.
///
/// Two-tier policy: *recognized* ≠ *pruning-eligible*. `agent_self_report` is a
/// recognized core method but is explicitly excluded from driving pruning, and
/// unregistered extensions never contribute to automatic suppression until host
/// policy trusts them. This type carries the recognition signal; the full
/// pruning-eligibility decision is Phase 9 policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ContextEvaluationMethod(String);

impl ContextEvaluationMethod {
    /// Deterministic validator produced the verdict.
    pub const DETERMINISTIC_VALIDATION: &'static str = "deterministic_validation";
    /// A user gave explicit feedback.
    pub const EXPLICIT_USER_FEEDBACK: &'static str = "explicit_user_feedback";
    /// An external outcome (CI, deploy, ticket) confirmed it.
    pub const EXTERNAL_OUTCOME: &'static str = "external_outcome";
    /// The accepted repository state reflects it.
    pub const ACCEPTED_REPOSITORY_STATE: &'static str = "accepted_repository_state";
    /// A controlled comparison (A/B) established it.
    pub const CONTROLLED_COMPARISON: &'static str = "controlled_comparison";
    /// Correlated against a use trace.
    pub const TRACE_CORRELATION: &'static str = "trace_correlation";
    /// The agent's own post-task report — inferred evidence only; cannot drive
    /// pruning.
    pub const AGENT_SELF_REPORT: &'static str = "agent_self_report";

    const RECOGNIZED_CORE: [&'static str; 7] = [
        Self::DETERMINISTIC_VALIDATION,
        Self::EXPLICIT_USER_FEEDBACK,
        Self::EXTERNAL_OUTCOME,
        Self::ACCEPTED_REPOSITORY_STATE,
        Self::CONTROLLED_COMPARISON,
        Self::TRACE_CORRELATION,
        Self::AGENT_SELF_REPORT,
    ];

    /// Construct a method identifier, rejecting an empty string (a writer must
    /// emit a non-empty registered or versioned identifier).
    pub fn new(identifier: impl Into<String>) -> Result<Self, RecordValidationError> {
        let identifier = identifier.into();
        if identifier.is_empty() {
            return Err(RecordValidationError::invariant(
                "evaluation_method.must_be_nonempty",
            ));
        }
        Ok(Self(identifier))
    }

    /// The identifier string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is one of the seven recognized core methods (an extension
    /// identifier returns `false` and is recognized only after host trust).
    pub fn is_recognized_core(&self) -> bool {
        Self::RECOGNIZED_CORE.contains(&self.0.as_str())
    }

    /// Whether this is `agent_self_report`, which alone cannot drive pruning.
    pub fn is_agent_self_report(&self) -> bool {
        self.0 == Self::AGENT_SELF_REPORT
    }
}

impl<'de> Deserialize<'de> for ContextEvaluationMethod {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let identifier = String::deserialize(deserializer)?;
        ContextEvaluationMethod::new(identifier).map_err(serde::de::Error::custom)
    }
}

/// A single missing-context kind (lifecycle §). Closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingContextKind {
    NotRetrieved,
    NotSelected,
    NotRendered,
    Unavailable,
    Unknown,
}

impl MissingContextKind {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRetrieved => "not_retrieved",
            Self::NotSelected => "not_selected",
            Self::NotRendered => "not_rendered",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }
}

/// A link from an observation to its supporting evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLink {
    /// The evidence record id.
    pub evidence_id: String,
    /// How the evidence relates to the observation.
    pub relation: String,
}

/// A record that a compiled frame's context record was used (lifecycle §). The
/// raw use event; the post-task judgement lives in [`ContextUseFeedback`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUse {
    /// How it was used.
    pub use_kind: ContextUseKind,
    /// The context record that was used.
    pub context_record_id: String,
    /// The use-trace id tying this use to a task/invocation.
    pub use_trace_id: String,
    /// The task this use belongs to.
    pub task_id: String,
    /// Which stage it influenced.
    pub influence_stage: ContextInfluenceStage,
    /// When the use was observed (RFC 3339 UTC).
    pub observed_at: String,
}

/// A post-task judgement of a [`ContextUse`] (lifecycle §).
///
/// `context_use_id` must resolve to exactly one identity-consistent `ContextUse`
/// — that resolution is **referential** and enforced by the repository, not
/// here. This type enforces the intra-record shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUseFeedback {
    /// The `ContextUse` this feedback is about (referentially resolved later).
    pub context_use_id: String,
    /// The use-trace id (must match the referenced use — checked in the repo).
    pub use_trace_id: String,
    /// The task this feedback belongs to.
    pub task_id: String,
    /// The judgement.
    pub evaluation: ContextUseEvaluation,
    /// Whether the record had a real opportunity to influence the task.
    pub had_opportunity: bool,
    /// Which stage it influenced.
    pub influence_stage: ContextInfluenceStage,
    /// How it related to the outcome.
    pub outcome_relation: ContextOutcomeRelation,
    /// Observable-effect evidence references (post-task, observable only).
    #[serde(default)]
    pub observable_effect_refs: Vec<String>,
    /// The evaluation method (required for a `not_helpful` verdict).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub evaluation_method: Option<ContextEvaluationMethod>,
    /// Attribution confidence (`0..=100`, un-bypassable via [`Confidence`]).
    pub attribution_confidence: Confidence,
    /// The outcome assessment (required when supported/contradicted).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub outcome_assessment_id: Option<String>,
    /// A bounded, post-task observable summary (≤ 500 Unicode scalar values).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub influence_statement: Option<String>,
    /// When the feedback was observed (RFC 3339 UTC).
    pub observed_at: String,
}

impl ContextUseFeedback {
    /// Validate every intra-record invariant. (Referential resolution of
    /// `context_use_id` is deferred to the repository.)
    pub fn validate(&self) -> Result<(), RecordValidationError> {
        // not_helpful requires opportunity + observable effect + a method.
        if self.evaluation == ContextUseEvaluation::NotHelpful {
            if !self.had_opportunity {
                return Err(RecordValidationError::invariant(
                    "not_helpful.requires_opportunity",
                ));
            }
            if self.observable_effect_refs.is_empty() {
                return Err(RecordValidationError::invariant(
                    "not_helpful.requires_observable_effect",
                ));
            }
            if self.evaluation_method.is_none() {
                return Err(RecordValidationError::invariant(
                    "not_helpful.requires_evaluation_method",
                ));
            }
        }
        // No opportunity constrains the other axes.
        if !self.had_opportunity {
            if self.influence_stage != ContextInfluenceStage::None {
                return Err(RecordValidationError::invariant(
                    "no_opportunity.requires_influence_stage_none",
                ));
            }
            if self.evaluation != ContextUseEvaluation::Neutral {
                return Err(RecordValidationError::invariant(
                    "no_opportunity.requires_neutral_evaluation",
                ));
            }
            if !matches!(
                self.outcome_relation,
                ContextOutcomeRelation::Unrelated | ContextOutcomeRelation::Unknown
            ) {
                return Err(RecordValidationError::invariant(
                    "no_opportunity.requires_unrelated_or_unknown_outcome",
                ));
            }
        }
        // supported/contradicted require an assessment and observable effects.
        if matches!(
            self.outcome_relation,
            ContextOutcomeRelation::Supported | ContextOutcomeRelation::Contradicted
        ) {
            if self.outcome_assessment_id.is_none() {
                return Err(RecordValidationError::invariant(
                    "outcome_relation.requires_assessment",
                ));
            }
            if self.observable_effect_refs.is_empty() {
                return Err(RecordValidationError::invariant(
                    "outcome_relation.requires_observable_effect",
                ));
            }
        }
        validate_influence_statement(self.influence_statement.as_deref())?;
        Ok(())
    }
}

/// A record that expected context was missing (lifecycle §).
///
/// The `not_rendered` selected-use back-references (`selected_context_use_id` /
/// `selected_use_trace_id`) are validated for shape only; their identity-
/// consistent resolution to the earlier selected `ContextUse`, and the
/// "at least one present when selected-use telemetry is available" rule, are
/// **referential** and enforced by the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingContextDetected {
    /// Which kind of missing-context this is.
    pub missing_context_kind: MissingContextKind,
    /// The record that was expected (required for not_retrieved/selected/rendered).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expected_context_record_id: Option<String>,
    /// The requirement that could not be met (required for `unavailable`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expected_requirement: Option<String>,
    /// How the miss was detected.
    pub detection_method: String,
    /// Supporting evidence (must be non-empty).
    #[serde(default)]
    pub evidence_links: Vec<EvidenceLink>,
    /// (not_rendered) the selected use that was not rendered.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub selected_context_use_id: Option<String>,
    /// (not_rendered) the selected use's trace id.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub selected_use_trace_id: Option<String>,
    /// The task this observation belongs to.
    pub task_id: String,
    /// When it was observed (RFC 3339 UTC).
    pub observed_at: String,
}

impl MissingContextDetected {
    /// Validate every intra-record invariant.
    pub fn validate(&self) -> Result<(), RecordValidationError> {
        // Every missing-context observation needs non-empty evidence.
        if self.evidence_links.is_empty() {
            return Err(RecordValidationError::invariant(
                "missing_context.requires_evidence_links",
            ));
        }
        match self.missing_context_kind {
            MissingContextKind::NotRetrieved
            | MissingContextKind::NotSelected
            | MissingContextKind::NotRendered => {
                if self.expected_context_record_id.is_none() {
                    return Err(RecordValidationError::invariant(
                        "missing_context.requires_expected_record",
                    ));
                }
            }
            MissingContextKind::Unavailable => {
                if self.expected_requirement.is_none() {
                    return Err(RecordValidationError::invariant(
                        "unavailable.requires_expected_requirement",
                    ));
                }
                if self.expected_context_record_id.is_some() {
                    return Err(RecordValidationError::invariant(
                        "unavailable.forbids_expected_record",
                    ));
                }
            }
            MissingContextKind::Unknown => {
                if self.expected_context_record_id.is_none() && self.expected_requirement.is_none()
                {
                    return Err(RecordValidationError::invariant(
                        "unknown.requires_record_or_requirement",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// An `influence_statement`, when present, is at most 500 Unicode scalar values
/// — one bounded, post-task observable sentence.
pub fn validate_influence_statement(statement: Option<&str>) -> Result<(), RecordValidationError> {
    if let Some(statement) = statement
        && statement.chars().count() > 500
    {
        return Err(RecordValidationError::invariant(
            "influence_statement.max_500_scalars",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_feedback() -> ContextUseFeedback {
        ContextUseFeedback {
            context_use_id: "cxu_1".into(),
            use_trace_id: "trace_1".into(),
            task_id: "task_1".into(),
            evaluation: ContextUseEvaluation::Helpful,
            had_opportunity: true,
            influence_stage: ContextInfluenceStage::Execution,
            outcome_relation: ContextOutcomeRelation::Unrelated,
            observable_effect_refs: vec![],
            evaluation_method: None,
            attribution_confidence: Confidence::new(80).unwrap(),
            outcome_assessment_id: None,
            influence_statement: None,
            observed_at: "2026-07-20T18:30:00Z".into(),
        }
    }

    #[test]
    fn enum_strings_are_canonical() {
        assert_eq!(
            serde_json::to_value(ContextUseKind::Cited).unwrap(),
            json!("cited")
        );
        assert_eq!(
            serde_json::to_value(ContextUseEvaluation::NotHelpful).unwrap(),
            json!("not_helpful")
        );
        assert_eq!(
            serde_json::to_value(ContextInfluenceStage::FinalResponse).unwrap(),
            json!("final_response")
        );
        assert_eq!(
            serde_json::to_value(ContextOutcomeRelation::Contradicted).unwrap(),
            json!("contradicted")
        );
        assert_eq!(
            serde_json::to_value(MissingContextKind::NotRendered).unwrap(),
            json!("not_rendered")
        );
    }

    #[test]
    fn evaluation_method_recognition_and_extensibility() {
        let core = ContextEvaluationMethod::new("deterministic_validation").unwrap();
        assert!(core.is_recognized_core());
        assert!(!core.is_agent_self_report());
        let asr = ContextEvaluationMethod::new(ContextEvaluationMethod::AGENT_SELF_REPORT).unwrap();
        assert!(asr.is_recognized_core());
        assert!(asr.is_agent_self_report());
        let ext = ContextEvaluationMethod::new("acme.custom_v2").unwrap();
        assert!(
            !ext.is_recognized_core(),
            "extensions are not core-recognized"
        );
        assert!(
            ContextEvaluationMethod::new("").is_err(),
            "empty method rejected"
        );
        // serializes transparently as the string.
        assert_eq!(
            serde_json::to_value(&core).unwrap(),
            json!("deterministic_validation")
        );
    }

    #[test]
    fn helpful_feedback_is_valid() {
        assert!(base_feedback().validate().is_ok());
    }

    #[test]
    fn not_helpful_requires_opportunity_effect_and_method() {
        let mut fb = base_feedback();
        fb.evaluation = ContextUseEvaluation::NotHelpful;
        // missing opportunity
        fb.had_opportunity = false;
        // had_opportunity=false also trips its own rules first — set stage none etc.
        fb.influence_stage = ContextInfluenceStage::None;
        fb.outcome_relation = ContextOutcomeRelation::Unknown;
        // not_helpful + no opportunity → not_helpful.requires_opportunity
        assert_eq!(
            fb.validate(),
            Err(RecordValidationError::invariant(
                "not_helpful.requires_opportunity"
            ))
        );
        // give opportunity but no observable effect
        fb.had_opportunity = true;
        fb.influence_stage = ContextInfluenceStage::Execution;
        fb.outcome_relation = ContextOutcomeRelation::Unrelated;
        assert_eq!(
            fb.validate(),
            Err(RecordValidationError::invariant(
                "not_helpful.requires_observable_effect"
            ))
        );
        // add effect but no method
        fb.observable_effect_refs = vec!["ev_1".into()];
        assert_eq!(
            fb.validate(),
            Err(RecordValidationError::invariant(
                "not_helpful.requires_evaluation_method"
            ))
        );
        // add method → valid
        fb.evaluation_method =
            Some(ContextEvaluationMethod::new("explicit_user_feedback").unwrap());
        assert!(fb.validate().is_ok());
    }

    #[test]
    fn no_opportunity_constrains_the_other_axes() {
        let mut fb = base_feedback();
        fb.had_opportunity = false;
        fb.influence_stage = ContextInfluenceStage::Execution; // wrong
        assert_eq!(
            fb.validate(),
            Err(RecordValidationError::invariant(
                "no_opportunity.requires_influence_stage_none"
            ))
        );
        fb.influence_stage = ContextInfluenceStage::None;
        fb.evaluation = ContextUseEvaluation::Helpful; // wrong
        assert_eq!(
            fb.validate(),
            Err(RecordValidationError::invariant(
                "no_opportunity.requires_neutral_evaluation"
            ))
        );
        fb.evaluation = ContextUseEvaluation::Neutral;
        fb.outcome_relation = ContextOutcomeRelation::Supported; // wrong
        assert_eq!(
            fb.validate(),
            Err(RecordValidationError::invariant(
                "no_opportunity.requires_unrelated_or_unknown_outcome"
            ))
        );
        fb.outcome_relation = ContextOutcomeRelation::Unrelated;
        assert!(fb.validate().is_ok());
    }

    #[test]
    fn supported_requires_assessment_and_effects() {
        let mut fb = base_feedback();
        fb.outcome_relation = ContextOutcomeRelation::Supported;
        assert_eq!(
            fb.validate(),
            Err(RecordValidationError::invariant(
                "outcome_relation.requires_assessment"
            ))
        );
        fb.outcome_assessment_id = Some("oa_1".into());
        assert_eq!(
            fb.validate(),
            Err(RecordValidationError::invariant(
                "outcome_relation.requires_observable_effect"
            ))
        );
        fb.observable_effect_refs = vec!["ev_1".into()];
        assert!(fb.validate().is_ok());
    }

    #[test]
    fn influence_statement_is_bounded_to_500_scalars() {
        let mut fb = base_feedback();
        fb.influence_statement = Some("x".repeat(500));
        assert!(fb.validate().is_ok(), "exactly 500 is allowed");
        fb.influence_statement = Some("x".repeat(501));
        assert_eq!(
            fb.validate(),
            Err(RecordValidationError::invariant(
                "influence_statement.max_500_scalars"
            ))
        );
    }

    fn base_missing() -> MissingContextDetected {
        MissingContextDetected {
            missing_context_kind: MissingContextKind::NotRetrieved,
            expected_context_record_id: Some("rec_1".into()),
            expected_requirement: None,
            detection_method: "coverage_gap".into(),
            evidence_links: vec![EvidenceLink {
                evidence_id: "ev_1".into(),
                relation: "supports".into(),
            }],
            selected_context_use_id: None,
            selected_use_trace_id: None,
            task_id: "task_1".into(),
            observed_at: "2026-07-20T18:30:00Z".into(),
        }
    }

    #[test]
    fn missing_context_per_kind_rules() {
        // evidence required for all kinds.
        let mut m = base_missing();
        m.evidence_links.clear();
        assert_eq!(
            m.validate(),
            Err(RecordValidationError::invariant(
                "missing_context.requires_evidence_links"
            ))
        );

        // not_retrieved needs expected record.
        let mut m = base_missing();
        m.expected_context_record_id = None;
        assert_eq!(
            m.validate(),
            Err(RecordValidationError::invariant(
                "missing_context.requires_expected_record"
            ))
        );

        // unavailable needs a requirement and forbids an expected record.
        let mut m = base_missing();
        m.missing_context_kind = MissingContextKind::Unavailable;
        m.expected_context_record_id = None;
        m.expected_requirement = None;
        assert_eq!(
            m.validate(),
            Err(RecordValidationError::invariant(
                "unavailable.requires_expected_requirement"
            ))
        );
        m.expected_requirement = Some("brand_kit".into());
        m.expected_context_record_id = Some("rec_1".into());
        assert_eq!(
            m.validate(),
            Err(RecordValidationError::invariant(
                "unavailable.forbids_expected_record"
            ))
        );
        m.expected_context_record_id = None;
        assert!(m.validate().is_ok());

        // unknown needs at least one of the two.
        let mut m = base_missing();
        m.missing_context_kind = MissingContextKind::Unknown;
        m.expected_context_record_id = None;
        m.expected_requirement = None;
        assert_eq!(
            m.validate(),
            Err(RecordValidationError::invariant(
                "unknown.requires_record_or_requirement"
            ))
        );
        m.expected_requirement = Some("something".into());
        assert!(m.validate().is_ok());
    }
}
