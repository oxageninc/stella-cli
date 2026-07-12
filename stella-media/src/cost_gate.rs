//! The video cost gate (`08-multimodal.md` §6). Video generation spends real
//! money, so before a job is submitted its estimated cost is checked against a
//! configurable USD threshold; above the threshold, the job requires explicit
//! confirmation through a [`CostGate`] port. Headless callers deny by default
//! unless they pass an explicit bypass (the `--yes` flag).
//!
//! The gate is a port so the CLI can wire an interactive stdin prompt and the
//! agent-tool path can wire a budget-aware policy, without this crate
//! depending on either. [`evaluate_video_cost`] is the pure decision function
//! the caller runs before `generate_video`.

use crate::error::MediaError;
use crate::provider::CostEstimate;

/// Default confirmation threshold (`08-multimodal.md` §6: "default: any
/// video"). At `$0.00`, every video with a positive estimate consults the
/// gate; a caller can raise it to auto-approve cheap jobs.
pub const DEFAULT_VIDEO_COST_THRESHOLD_USD: f64 = 0.0;

/// A confirm/deny decision for a cost-gated job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CostDecision {
    Approve,
    Deny,
}

/// The confirmation port. An implementation decides whether a job whose
/// estimate exceeds the threshold may proceed. Kept synchronous: an
/// interactive implementation reads stdin (the caller can wrap it in
/// `spawn_blocking` if it must not block an async runtime).
pub trait CostGate: Send + Sync {
    fn confirm(&self, estimate: &CostEstimate) -> CostDecision;
}

/// Headless gate: approves only when constructed with `bypass = true` (the
/// `--yes` flag). The safe default — an unattended run never silently spends
/// on video (`08-multimodal.md` §6).
#[derive(Clone, Copy, Debug)]
pub struct HeadlessCostGate {
    bypass: bool,
}

impl HeadlessCostGate {
    /// `bypass = true` corresponds to `--yes`; `false` denies every gated job.
    pub fn new(bypass: bool) -> Self {
        Self { bypass }
    }
}

impl CostGate for HeadlessCostGate {
    fn confirm(&self, _estimate: &CostEstimate) -> CostDecision {
        if self.bypass {
            CostDecision::Approve
        } else {
            CostDecision::Deny
        }
    }
}

/// The pure gate decision (`08-multimodal.md` §6): if the estimate is at or
/// below `threshold_usd`, the job passes without consulting the gate;
/// otherwise the gate decides. A denial is a terminal
/// [`MediaError::CostDenied`] carrying the numbers.
pub fn evaluate_video_cost(
    estimate: &CostEstimate,
    threshold_usd: f64,
    gate: &dyn CostGate,
) -> Result<(), MediaError> {
    if estimate.estimated_usd <= threshold_usd {
        return Ok(());
    }
    match gate.confirm(estimate) {
        CostDecision::Approve => Ok(()),
        CostDecision::Deny => Err(MediaError::CostDenied {
            estimated_usd: estimate.estimated_usd,
            threshold_usd,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::MediaKind;

    fn estimate(usd: f64) -> CostEstimate {
        CostEstimate {
            kind: MediaKind::Video,
            model: "cogvideox".into(),
            estimated_usd: usd,
            detail: "10s video".into(),
        }
    }

    #[test]
    fn below_or_at_threshold_passes_without_consulting_the_gate() {
        // A DenyAll gate would reject if consulted; it must not be consulted.
        let deny = HeadlessCostGate::new(false);
        assert!(evaluate_video_cost(&estimate(0.10), 0.10, &deny).is_ok());
        assert!(evaluate_video_cost(&estimate(0.05), 0.10, &deny).is_ok());
    }

    #[test]
    fn above_threshold_headless_denies_by_default() {
        let deny = HeadlessCostGate::new(false);
        let err = evaluate_video_cost(&estimate(0.40), 0.0, &deny).unwrap_err();
        match err {
            MediaError::CostDenied {
                estimated_usd,
                threshold_usd,
            } => {
                assert!((estimated_usd - 0.40).abs() < 1e-9);
                assert!((threshold_usd - 0.0).abs() < 1e-9);
            }
            other => panic!("expected CostDenied, got {other:?}"),
        }
    }

    #[test]
    fn above_threshold_with_bypass_approves() {
        let approve = HeadlessCostGate::new(true);
        assert!(evaluate_video_cost(&estimate(5.0), 0.0, &approve).is_ok());
    }

    #[test]
    fn default_threshold_gates_any_positive_cost_video() {
        let deny = HeadlessCostGate::new(false);
        // The default threshold is 0.0, so any positive estimate is gated.
        assert!(
            evaluate_video_cost(&estimate(0.01), DEFAULT_VIDEO_COST_THRESHOLD_USD, &deny).is_err()
        );
        // A zero-cost job (free tier / estimate unavailable) is not gated.
        assert!(
            evaluate_video_cost(&estimate(0.0), DEFAULT_VIDEO_COST_THRESHOLD_USD, &deny).is_ok()
        );
    }
}
