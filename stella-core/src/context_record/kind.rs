//! The record-taxonomy enums. Every variant's canonical wire form is the
//! lowercase `snake_case` token asserted by the tests at the bottom of this
//! file — those strings are load-bearing (they enter `record_hash` preimages),
//! so `as_str()` and the serde form are pinned to each other.

use serde::{Deserialize, Serialize};

use super::RecordValidationError;

/// Top-level `record_kind` discriminator (lifecycle §6.1) — a flat discriminated
/// union: the value selects the type-specific schema whose fields live at the
/// record top level. Only `directive` carries instruction authority;
/// `artifact_contract` carries completion authority when selected; all others
/// carry none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRecordKind {
    Observation,
    Knowledge,
    Memory,
    Directive,
    RecordProposal,
    Evidence,
    ArtifactContract,
    ContractValidation,
    OutcomeAssessment,
    PromotionEvent,
    ContextUse,
    ContextUseFeedback,
}

impl ContextRecordKind {
    /// The canonical `snake_case` string stored on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::Knowledge => "knowledge",
            Self::Memory => "memory",
            Self::Directive => "directive",
            Self::RecordProposal => "record_proposal",
            Self::Evidence => "evidence",
            Self::ArtifactContract => "artifact_contract",
            Self::ContractValidation => "contract_validation",
            Self::OutcomeAssessment => "outcome_assessment",
            Self::PromotionEvent => "promotion_event",
            Self::ContextUse => "context_use",
            Self::ContextUseFeedback => "context_use_feedback",
        }
    }
}

/// `knowledge` sub-kind (lifecycle §6.3). "Do not create a new kind for every
/// noun" — the set is deliberately tiny.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeKind {
    Fact,
    Assumption,
    Decision,
}

impl KnowledgeKind {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Assumption => "assumption",
            Self::Decision => "decision",
        }
    }
}

/// `memory` sub-kind (lifecycle §6.4).
///
/// NOTE: this is the spec taxonomy (`episode`, `summary`), which differs from
/// the current `stella-context` `MemoryKind` (`Reflection`/`Note`/`Insight`).
/// The two coexist; migration is a later phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// A bounded recollection of one task, session, or event.
    Episode,
    /// A lossy synthesis across multiple episodes.
    Summary,
}

impl MemoryKind {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Episode => "episode",
            Self::Summary => "summary",
        }
    }
}

/// `directive` sub-kind (lifecycle §6.5). `memory` and `fact` are explicitly
/// forbidden as directive kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveKind {
    Preference,
    Rule,
    Constraint,
    Procedure,
}

impl DirectiveKind {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preference => "preference",
            Self::Rule => "rule",
            Self::Constraint => "constraint",
            Self::Procedure => "procedure",
        }
    }
}

/// A `constraint` directive's effect (lifecycle §6.5). `allow` is **deliberately
/// excluded** — learned context cannot grant authorization. The type therefore
/// makes an `allow` constraint unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintEffect {
    Require,
    Forbid,
}

impl ConstraintEffect {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Require => "require",
            Self::Forbid => "forbid",
        }
    }
}

/// Semantic source class for a record (lifecycle §7.1). Ratified as a uniform
/// 5-value set across all families.
///
/// NOTE (flagged, not enforced here): the directive record schema narrows a
/// directive's origin to `user, system, inferred, imported` (no `observed`).
/// That per-family validator is deferred pending confirmation — see the module
/// docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    User,
    System,
    Observed,
    Inferred,
    Imported,
}

impl Origin {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
            Self::Observed => "observed",
            Self::Inferred => "inferred",
            Self::Imported => "imported",
        }
    }
}

/// A directive's enforcement level (lifecycle §). Exactly two values (the
/// ratified 4→2 mapping, ADR 0007); any richer UI vocabulary is a label over
/// these two, never a second enforcement enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveEnforcement {
    Advisory,
    Blocking,
}

impl DirectiveEnforcement {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Advisory => "advisory",
            Self::Blocking => "blocking",
        }
    }
}

/// A directive's priority band (lifecycle §, directive allowed-values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectivePriority {
    Low,
    Normal,
    High,
    Critical,
}

impl DirectivePriority {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// The STORED lifecycle status of a record (lifecycle §7.6, §15.1). Exactly
/// three; every change creates a new immutable revision. Staleness is NOT a
/// status (it is a separate derived selection-health value). Contrast
/// [`EffectiveStatus`], which is a query-time derivation and is excluded from
/// `record_hash`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    Active,
    Retracted,
    Archived,
}

impl RecordStatus {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retracted => "retracted",
            Self::Archived => "archived",
        }
    }
}

/// The DERIVED effective status computed at query time from the historical
/// prefix (lifecycle §15.1). Excluded from `record_hash` — it is a projection,
/// not stored canonical state. `superseded` comes from a later revision's
/// `supersedes_record_id`; `expired` from `valid_until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveStatus {
    Active,
    Superseded,
    Retracted,
    Archived,
    Expired,
}

impl EffectiveStatus {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Retracted => "retracted",
            Self::Archived => "archived",
            Self::Expired => "expired",
        }
    }
}

/// The kind of record a proposal proposes (lifecycle §6.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordProposalKind {
    Knowledge,
    Directive,
    ContractAmendment,
}

impl RecordProposalKind {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Knowledge => "knowledge",
            Self::Directive => "directive",
            Self::ContractAmendment => "contract_amendment",
        }
    }
}

/// A proposal's status (lifecycle §8.9). Activation and rejection are
/// PromotionEvent outcomes, NOT proposal states — do not add them here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordProposalStatus {
    Collecting,
    Eligible,
    Dismissed,
    Expired,
}

impl RecordProposalStatus {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Collecting => "collecting",
            Self::Eligible => "eligible",
            Self::Dismissed => "dismissed",
            Self::Expired => "expired",
        }
    }
}

/// The action recorded on an immutable `promotion_event` (lifecycle §6.10).
/// `auto_activated` is permitted only for an advisory directive under the active
/// governance policy. Promotion is not assumed to be one linear state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionAction {
    Proposed,
    AutoActivated,
    Confirmed,
    Published,
    Rejected,
    Retired,
    Reverted,
}

impl PromotionAction {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::AutoActivated => "auto_activated",
            Self::Confirmed => "confirmed",
            Self::Published => "published",
            Self::Rejected => "rejected",
            Self::Retired => "retired",
            Self::Reverted => "reverted",
        }
    }
}

/// Validate the enforcement a directive is *created* with against its origin: an
/// inferred directive may not START blocking (lifecycle §). Spans `origin` and
/// `enforcement`.
pub fn validate_directive_creation_enforcement(
    origin: Origin,
    enforcement: DirectiveEnforcement,
) -> Result<(), RecordValidationError> {
    if origin == Origin::Inferred && enforcement == DirectiveEnforcement::Blocking {
        return Err(RecordValidationError::InferredDirectiveBlocking);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Assert that a variant's `as_str()` equals its serde wire form, and that
    /// the wire form round-trips back to the same variant. This double-locks the
    /// canonical string used in `record_hash` preimages.
    fn assert_canonical<T>(variant: T, expected: &str)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug + Copy,
    {
        let wire = serde_json::to_value(variant).unwrap();
        assert_eq!(wire, json!(expected), "serde wire form");
        let back: T = serde_json::from_value(Value::String(expected.to_string())).unwrap();
        assert_eq!(back, variant, "round-trip");
    }

    #[test]
    fn context_record_kind_strings_are_canonical() {
        use ContextRecordKind::*;
        for (v, s) in [
            (Observation, "observation"),
            (Knowledge, "knowledge"),
            (Memory, "memory"),
            (Directive, "directive"),
            (RecordProposal, "record_proposal"),
            (Evidence, "evidence"),
            (ArtifactContract, "artifact_contract"),
            (ContractValidation, "contract_validation"),
            (OutcomeAssessment, "outcome_assessment"),
            (PromotionEvent, "promotion_event"),
            (ContextUse, "context_use"),
            (ContextUseFeedback, "context_use_feedback"),
        ] {
            assert_eq!(v.as_str(), s);
            assert_canonical(v, s);
        }
    }

    #[test]
    fn small_taxonomy_strings_are_canonical() {
        assert_canonical(KnowledgeKind::Fact, "fact");
        assert_canonical(KnowledgeKind::Assumption, "assumption");
        assert_canonical(KnowledgeKind::Decision, "decision");
        assert_canonical(MemoryKind::Episode, "episode");
        assert_canonical(MemoryKind::Summary, "summary");
        assert_canonical(DirectiveKind::Preference, "preference");
        assert_canonical(DirectiveKind::Rule, "rule");
        assert_canonical(DirectiveKind::Constraint, "constraint");
        assert_canonical(DirectiveKind::Procedure, "procedure");
        assert_canonical(ConstraintEffect::Require, "require");
        assert_canonical(ConstraintEffect::Forbid, "forbid");
        assert_canonical(Origin::User, "user");
        assert_canonical(Origin::System, "system");
        assert_canonical(Origin::Observed, "observed");
        assert_canonical(Origin::Inferred, "inferred");
        assert_canonical(Origin::Imported, "imported");
        assert_canonical(DirectiveEnforcement::Advisory, "advisory");
        assert_canonical(DirectiveEnforcement::Blocking, "blocking");
        assert_canonical(DirectivePriority::Low, "low");
        assert_canonical(DirectivePriority::Critical, "critical");
    }

    #[test]
    fn status_and_promotion_strings_are_canonical() {
        assert_canonical(RecordStatus::Active, "active");
        assert_canonical(RecordStatus::Retracted, "retracted");
        assert_canonical(RecordStatus::Archived, "archived");
        assert_canonical(EffectiveStatus::Superseded, "superseded");
        assert_canonical(EffectiveStatus::Expired, "expired");
        assert_canonical(RecordProposalKind::ContractAmendment, "contract_amendment");
        assert_canonical(RecordProposalStatus::Collecting, "collecting");
        assert_canonical(RecordProposalStatus::Eligible, "eligible");
        assert_canonical(PromotionAction::AutoActivated, "auto_activated");
        assert_canonical(PromotionAction::Reverted, "reverted");
    }

    #[test]
    fn constraint_effect_cannot_represent_allow() {
        // `allow` is not a variant, so it cannot even deserialize.
        let parsed: Result<ConstraintEffect, _> = serde_json::from_value(json!("allow"));
        assert!(
            parsed.is_err(),
            "constraint effect `allow` must be unrepresentable"
        );
    }

    #[test]
    fn inferred_directive_may_not_start_blocking() {
        assert_eq!(
            validate_directive_creation_enforcement(
                Origin::Inferred,
                DirectiveEnforcement::Blocking
            ),
            Err(RecordValidationError::InferredDirectiveBlocking)
        );
        // Inferred + advisory is fine; user + blocking is fine.
        assert!(
            validate_directive_creation_enforcement(
                Origin::Inferred,
                DirectiveEnforcement::Advisory
            )
            .is_ok()
        );
        assert!(
            validate_directive_creation_enforcement(Origin::User, DirectiveEnforcement::Blocking)
                .is_ok()
        );
    }
}
