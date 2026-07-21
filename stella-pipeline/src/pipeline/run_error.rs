use stella_core::router::RouterError;
use stella_protocol::ModelRef;

/// A hard, named failure of a pipeline run (as opposed to a clean
/// [`super::PipelineStatus::Aborted`], which is a normal outcome).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PipelineError {
    /// A user-supplied test command did not fit the typed test vocabulary.
    #[error("invalid test command: {0}")]
    InvalidTestCommand(String),
    /// A plan crossed the scope-review thresholds while running headless with
    /// no approval bypass configured (L-E5): never silently auto-approve.
    #[error(
        "scope review is required for this plan, but the run is headless without an approval bypass — re-run interactively or enable the scope-review bypass"
    )]
    ScopeReviewRequiredHeadless,
    /// The router resolved a role to a model no configured adapter serves.
    #[error(
        "no provider adapter is configured for the resolved model `{0}` — configure the provider or refresh the catalog"
    )]
    NoProviderForModel(String),
    /// A required role (worker) could not be resolved at all.
    #[error(transparent)]
    Routing(#[from] RouterError),
}

/// A hard pipeline failure paired with every paid stage that settled before
/// the failure boundary.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{cause}")]
pub struct PipelineRunError {
    pub cause: PipelineError,
    pub total_cost_usd: f64,
}

impl PipelineRunError {
    pub(super) fn new(cause: PipelineError, total_cost_usd: f64) -> Self {
        Self {
            cause,
            total_cost_usd,
        }
    }
}

pub(super) enum RoleResolveError {
    Router(RouterError),
    NoProvider(ModelRef),
}

impl RoleResolveError {
    pub(super) fn into_pipeline_error(self) -> PipelineError {
        match self {
            Self::Router(error) => PipelineError::Routing(error),
            Self::NoProvider(model) => PipelineError::NoProviderForModel(model.to_string()),
        }
    }
}
