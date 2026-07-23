//! `Scope` (where a record applies) and `SharingScope` (who may receive it) —
//! two independent concepts (lifecycle §7.2 / §7.3). Do not collapse them.

use serde::{Deserialize, Serialize};

use super::RecordValidationError;
use super::kind::Origin;

/// Where a record applies. A struct of optional identifiers, **conjunctive**
/// when populated (a record scoped to a repository AND a workspace applies only
/// where both match). `project` is intentionally not a core field — it is a
/// namespaced extension only. Scope never widens automatically.
///
/// Optional fields are omitted (never `null`) on the wire, which is what keeps
/// the `record_hash` preimage stable across present-vs-absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// The user the record is about / applies to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user_id: Option<String>,
    /// The organization the record applies to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub organization_id: Option<String>,
    /// The repository (VCS/Git identity) the record applies to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repository_id: Option<String>,
    /// The workspace (provider-managed security principal) the record applies to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub workspace_id: Option<String>,
    /// The environment the record applies to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub environment_id: Option<String>,
    /// The session the record applies to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
    /// The task the record applies to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub task_id: Option<String>,
}

impl Scope {
    /// True when no scope dimension is populated. An unscoped **inferred** record
    /// is invalid (see [`validate_inferred_scope`]).
    pub fn is_empty(&self) -> bool {
        self.user_id.is_none()
            && self.organization_id.is_none()
            && self.repository_id.is_none()
            && self.workspace_id.is_none()
            && self.environment_id.is_none()
            && self.session_id.is_none()
            && self.task_id.is_none()
    }
}

/// Who may receive or inherit a record (lifecycle §7.3) — a 4-value audience,
/// ratified 2026-07-23 (ADR 0002). NOT a linear hierarchy; every audience change
/// is explicit. Each audience requires its matching [`Scope`] id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingScope {
    User,
    Repository,
    Workspace,
    Organization,
}

impl SharingScope {
    /// The canonical `snake_case` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Repository => "repository",
            Self::Workspace => "workspace",
            Self::Organization => "organization",
        }
    }

    /// The `Scope` id this audience requires to be present.
    pub fn required_scope_id(self) -> &'static str {
        match self {
            Self::User => "user_id",
            Self::Repository => "repository_id",
            Self::Workspace => "workspace_id",
            Self::Organization => "organization_id",
        }
    }

    fn scope_id_present(self, scope: &Scope) -> bool {
        match self {
            Self::User => scope.user_id.is_some(),
            Self::Repository => scope.repository_id.is_some(),
            Self::Workspace => scope.workspace_id.is_some(),
            Self::Organization => scope.organization_id.is_some(),
        }
    }
}

/// A sharing audience requires its matching scope id (`user → user_id`,
/// `repository → repository_id`, `workspace → workspace_id`, `organization →
/// organization_id`).
pub fn validate_sharing_scope(
    sharing: SharingScope,
    scope: &Scope,
) -> Result<(), RecordValidationError> {
    if sharing.scope_id_present(scope) {
        Ok(())
    } else {
        Err(RecordValidationError::SharingScopeMissingId {
            audience: sharing.as_str(),
            required_id: sharing.required_scope_id(),
        })
    }
}

/// An inferred record must carry a non-empty scope (lifecycle §7.2).
pub fn validate_inferred_scope(origin: Origin, scope: &Scope) -> Result<(), RecordValidationError> {
    if origin == Origin::Inferred && scope.is_empty() {
        return Err(RecordValidationError::InferredRecordEmptyScope);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_scope_serializes_to_an_empty_object() {
        // Every field is None → omitted, so the wire form is `{}` (no nulls).
        let json = serde_json::to_value(Scope::default()).unwrap();
        assert_eq!(json, json!({}));
        assert!(Scope::default().is_empty());
    }

    #[test]
    fn populated_scope_omits_absent_ids() {
        let scope = Scope {
            repository_id: Some("repo_42".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&scope).unwrap(),
            json!({"repository_id": "repo_42"})
        );
        assert!(!scope.is_empty());
    }

    #[test]
    fn sharing_scope_strings_and_required_ids() {
        for (s, name, id) in [
            (SharingScope::User, "user", "user_id"),
            (SharingScope::Repository, "repository", "repository_id"),
            (SharingScope::Workspace, "workspace", "workspace_id"),
            (
                SharingScope::Organization,
                "organization",
                "organization_id",
            ),
        ] {
            assert_eq!(s.as_str(), name);
            assert_eq!(s.required_scope_id(), id);
            assert_eq!(serde_json::to_value(s).unwrap(), json!(name));
        }
    }

    #[test]
    fn sharing_requires_matching_scope_id() {
        let repo_scope = Scope {
            repository_id: Some("repo_1".into()),
            ..Default::default()
        };
        assert!(validate_sharing_scope(SharingScope::Repository, &repo_scope).is_ok());
        // Sharing to the workspace audience needs workspace_id, which is absent.
        assert_eq!(
            validate_sharing_scope(SharingScope::Workspace, &repo_scope),
            Err(RecordValidationError::SharingScopeMissingId {
                audience: "workspace",
                required_id: "workspace_id",
            })
        );
    }

    #[test]
    fn inferred_record_requires_nonempty_scope() {
        assert_eq!(
            validate_inferred_scope(Origin::Inferred, &Scope::default()),
            Err(RecordValidationError::InferredRecordEmptyScope)
        );
        let scoped = Scope {
            task_id: Some("task_9".into()),
            ..Default::default()
        };
        assert!(validate_inferred_scope(Origin::Inferred, &scoped).is_ok());
        // A non-inferred record may be unscoped.
        assert!(validate_inferred_scope(Origin::User, &Scope::default()).is_ok());
    }
}
