//! Outcome-assessment value types (lifecycle §6.8 / §8.x). Two independent
//! dimensions — completion and correctness — each carrying a status and the
//! evidential level behind it.

use serde::{Deserialize, Serialize};

/// The evidential strength behind an assessment (lifecycle §6.8). Agent
/// reflection alone supports only `inferred`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeAssessmentLevel {
    Verified,
    UserConfirmed,
    ExternallyConfirmed,
    Inferred,
    Unknown,
}

impl OutcomeAssessmentLevel {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::UserConfirmed => "user_confirmed",
            Self::ExternallyConfirmed => "externally_confirmed",
            Self::Inferred => "inferred",
            Self::Unknown => "unknown",
        }
    }
}

/// Whether a task's deliverable was completed (lifecycle §).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    Complete,
    Incomplete,
    Unknown,
}

impl CompletionStatus {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Incomplete => "incomplete",
            Self::Unknown => "unknown",
        }
    }
}

/// Whether a task's deliverable was correct (lifecycle §). Independent of
/// [`CompletionStatus`] — a task can be complete but incorrect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectnessStatus {
    Correct,
    Incorrect,
    Unknown,
}

impl CorrectnessStatus {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Correct => "correct",
            Self::Incorrect => "incorrect",
            Self::Unknown => "unknown",
        }
    }
}

/// One assessed dimension: a status plus the level of evidence behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionAssessment {
    /// The completion verdict.
    pub status: CompletionStatus,
    /// The evidence level.
    pub assessment_level: OutcomeAssessmentLevel,
}

/// One assessed dimension: a status plus the level of evidence behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrectnessAssessment {
    /// The correctness verdict.
    pub status: CorrectnessStatus,
    /// The evidence level.
    pub assessment_level: OutcomeAssessmentLevel,
}

/// A post-task assessment of an outcome across the two independent dimensions
/// (lifecycle §). Referenced by a `ContextUseFeedback` when the outcome relation
/// is supported/contradicted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeAssessment {
    /// The completion dimension.
    pub completion_assessment: CompletionAssessment,
    /// The correctness dimension.
    pub correctness_assessment: CorrectnessAssessment,
    /// When it was observed (RFC 3339 UTC).
    pub observed_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn level_and_status_strings_are_canonical() {
        assert_eq!(
            serde_json::to_value(OutcomeAssessmentLevel::ExternallyConfirmed).unwrap(),
            json!("externally_confirmed")
        );
        assert_eq!(
            serde_json::to_value(OutcomeAssessmentLevel::UserConfirmed).unwrap(),
            json!("user_confirmed")
        );
        assert_eq!(
            serde_json::to_value(CompletionStatus::Incomplete).unwrap(),
            json!("incomplete")
        );
        assert_eq!(
            serde_json::to_value(CorrectnessStatus::Incorrect).unwrap(),
            json!("incorrect")
        );
    }

    #[test]
    fn the_two_dimensions_are_independent() {
        // Complete but incorrect is a representable, valid combination.
        let assessment = OutcomeAssessment {
            completion_assessment: CompletionAssessment {
                status: CompletionStatus::Complete,
                assessment_level: OutcomeAssessmentLevel::Verified,
            },
            correctness_assessment: CorrectnessAssessment {
                status: CorrectnessStatus::Incorrect,
                assessment_level: OutcomeAssessmentLevel::Inferred,
            },
            observed_at: "2026-07-20T18:30:00Z".into(),
        };
        let round: OutcomeAssessment =
            serde_json::from_value(serde_json::to_value(&assessment).unwrap()).unwrap();
        assert_eq!(round, assessment);
    }
}
