//! Adaptive-context domain model — pure, I/O-free types and validators.
//!
//! This is the typed record taxonomy every later adaptive-context phase
//! (migration, compilation, learning, governance) builds on. It is deliberately
//! **pure**: no I/O, no persistence, no `stella-context` dependency — only value
//! types, cross-field validators, and canonical hashing. (Named `context_record`
//! rather than `context` to stay unambiguous next to the `stella-context` crate.)
//!
//! ## Scope of this installment
//!
//! Phase 1 is large; this is **installment 1 of N** — the foundational vertical:
//! the record taxonomy enums, [`Scope`]/[`SharingScope`], the temporal
//! primitives, canonical [`hash`]ing, and the cross-field validators that live
//! entirely on those types. **Deferred to later installments** (their types are
//! not here yet, so neither are the validators that span them): the
//! context-use / efficacy web, artifact contracts + validation, outcome
//! assessments, frame representation / content-fidelity, and the internal
//! replay-safe events in `stella-protocol`. This module does **not** yet satisfy
//! the full Phase 1 gate.
//!
//! ## Relationship to the legacy `rules::metadata` types (coexist, do not merge)
//!
//! `stella-core::rules::metadata` already carries types that drive the **live**
//! Markdown rules path. Those are left untouched here — editing them would be a
//! behavior change. The new types coexist; a later phase migrates the rules path
//! onto them. The intended subsumption mapping:
//!
//! | legacy (`rules::metadata`)                     | new (`context_record`)                          |
//! |------------------------------------------------|-------------------------------------------------|
//! | `RuleRecordKind::Directive`                    | `ContextRecordKind::Directive` + `DirectiveKind`|
//! | `RuleEnforcement` = {Informational,Advisory,Blocking} | `DirectiveEnforcement` = {Advisory,Blocking} |
//! | `RuleOrigin` = {User,System,Inferred,Imported} | `Origin` = {User,System,Observed,Inferred,Imported} |
//!
//! **Two mapping edges are NOT covered by the ratified decisions and are flagged
//! for confirmation before the legacy path is migrated:**
//!
//! 1. `RuleEnforcement::Informational` has no target in the 2-value
//!    `DirectiveEnforcement`. The ratified 4→2 mapping (ADR 0007) was over the
//!    `context-prs-spec` vocabulary (`observe|advisory|required|blocking`), a
//!    *different* set than `Informational|Advisory|Blocking`. `Informational →
//!    advisory` is the likely intent but is **unratified**.
//! 2. `Origin` is a uniform 5-value enum (ratified). However the directive
//!    record schema (lifecycle §, directive allowed-values) enumerates only
//!    `user, system, inferred, imported` — a per-family narrowing that forbids
//!    an `observed` directive. That narrowing is left as a **flagged validator,
//!    not implemented here**, because it refines the "uniform 5" resolution.

pub mod context_use;
pub mod contract;
pub mod hash;
pub mod kind;
pub mod outcome;
pub mod representation;
pub mod scope;
pub mod temporal;

pub use context_use::{
    ContextEvaluationMethod, ContextInfluenceStage, ContextOutcomeRelation, ContextUse,
    ContextUseEvaluation, ContextUseFeedback, ContextUseKind, EvidenceLink, MissingContextDetected,
    MissingContextKind,
};
pub use contract::{
    ArtifactContract, ContractValidation, Requirement, RequirementKind, RequirementResult,
    RequirementStatus, ValidationStatus,
};
pub use hash::{RecordHashError, record_hash};
pub use kind::{
    ConstraintEffect, ContextRecordKind, DirectiveEnforcement, DirectiveKind, DirectivePriority,
    EffectiveStatus, KnowledgeKind, MemoryKind, Origin, PromotionAction, RecordProposalKind,
    RecordProposalStatus, RecordStatus,
};
pub use outcome::{CompletionStatus, CorrectnessStatus, OutcomeAssessment, OutcomeAssessmentLevel};
pub use representation::{
    ContentFidelity, InlineContentRequirement, MinimumContentFidelity, Representation,
};
pub use scope::{Scope, SharingScope};
pub use temporal::{TemporalInterval, TemporalQuery};

/// A `0..=100` confidence score. The newtype makes the range invariant
/// un-bypassable: there is no way to hold an out-of-range confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct Confidence(u8);

impl Confidence {
    /// Construct a confidence, rejecting anything outside `0..=100`.
    pub fn new(value: u16) -> Result<Self, RecordValidationError> {
        if value > 100 {
            return Err(RecordValidationError::ConfidenceOutOfRange(value));
        }
        Ok(Self(value as u8))
    }

    /// The score as `0..=100`.
    pub fn get(self) -> u8 {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for Confidence {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u16::deserialize(deserializer)?;
        Confidence::new(value).map_err(serde::de::Error::custom)
    }
}

/// A cross-field domain invariant was violated. These are the "explicit
/// constructor / validation function" errors the plan requires so an invalid
/// record cannot be silently constructed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordValidationError {
    /// Confidence must be an integer in `0..=100`.
    #[error("confidence {0} is out of range (must be 0..=100)")]
    ConfidenceOutOfRange(u16),
    /// An inferred directive may not START blocking (lifecycle §: inferred
    /// directives begin advisory and never become blocking without explicit
    /// confirmation).
    #[error("an inferred directive may not be created with blocking enforcement")]
    InferredDirectiveBlocking,
    /// An inferred record must carry a non-empty [`Scope`].
    #[error("an inferred record must have a non-empty scope")]
    InferredRecordEmptyScope,
    /// A `constraint` directive's effect must be `require` or `forbid`. (The
    /// type system already excludes `allow`; this variant exists for callers
    /// that parse effects from untyped input.)
    #[error("a constraint effect must be require or forbid, never allow")]
    ConstraintEffectAllow,
    /// A temporal interval must be non-empty and forward: `until` strictly
    /// after `from`.
    #[error("valid_until ({until}) must be strictly later than valid_from ({from})")]
    NonForwardInterval {
        /// The interval start.
        from: String,
        /// The interval end (must be strictly greater).
        until: String,
    },
    /// A [`SharingScope`] audience requires the matching id to be present in the
    /// record's [`Scope`].
    #[error("sharing scope `{audience}` requires scope.{required_id} to be set")]
    SharingScopeMissingId {
        /// The audience that was declared.
        audience: &'static str,
        /// The `Scope` id it requires.
        required_id: &'static str,
    },
    /// A named cross-field invariant was violated. `rule` is a stable dotted
    /// identifier (e.g. `not_helpful.requires_observable_effect`) so the many
    /// intra-record rules stay self-documenting and testable without a variant
    /// each.
    #[error("invariant violated: {rule}")]
    Invariant {
        /// The stable rule identifier that failed.
        rule: &'static str,
    },
}

impl RecordValidationError {
    /// Shorthand for [`RecordValidationError::Invariant`].
    pub(crate) fn invariant(rule: &'static str) -> Self {
        Self::Invariant { rule }
    }
}
