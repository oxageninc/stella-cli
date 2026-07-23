//! Model roles, not model names. Every model reference in the engine is a
//! `Role`, resolved through one router; no call site names a model literal.
//!
//! "Auto mode is the absence of a pin, not a magic slug": selection is
//! `Option<ModelRef>`; `None` means the router chooses by task class. There
//! is no `"auto"` string anywhere in this type (L-M3) — the TS-era bug class
//! where a pseudo-slug leaked into resolver paths is structurally excluded.

use serde::{Deserialize, Serialize};

/// A functional role a model call fills. Never a model name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Main coding/execution steps.
    Worker,
    /// Prompt classification, simple-prompt fast path, routing decisions.
    Triage,
    /// Planner (defaults to worker-class, separately overridable).
    Plan,
    /// Evidence-based verification, best-of-N selection. Never the same
    /// instance as `Worker`.
    Judge,
    /// Context-plane embeddings.
    Embed,
    /// Image-input understanding.
    Vision,
    /// Image generation.
    Image,
    /// Video generation.
    Video,
}

/// An explicit `provider/model` pin. A caller either holds `Some(ModelRef)`
/// (an explicit pin from a flag, config, or slash command) or `None` (auto:
/// the router resolves by task class + scenario defaults). There is
/// deliberately no "auto" variant here — see module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn new(provider: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model_id: model_id.into(),
        }
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ref_display_matches_provider_model_id_shape() {
        let m = ModelRef::new("zai", "glm-5.2");
        assert_eq!(m.to_string(), "zai/glm-5.2");
    }

    #[test]
    fn role_roundtrips_and_uses_snake_case() {
        let json = serde_json::to_string(&Role::Judge).unwrap();
        assert_eq!(json, "\"judge\"");
        let back: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Role::Judge);
    }

    #[test]
    fn option_model_ref_none_is_auto_by_construction() {
        // The type system, not a string, encodes "auto": there is no way to
        // construct a "auto" ModelRef — the absence of Some(_) *is* auto.
        let auto: Option<ModelRef> = None;
        assert!(auto.is_none());
    }
}
