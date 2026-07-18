//! The typed conformance report. Each check
//! carries a pass/fail/skip status and an evidence string, so "not
//! conformant" always says *why*. Serde-derivable so `ocp-inspect --json`
//! and CI can consume it.

use serde::{Deserialize, Serialize};

/// The verdict for a single conformance check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    /// Not applicable to this provider/transport (e.g. a wire-level probe
    /// against an in-process provider).
    Skipped,
}

/// One check's outcome: which check, its verdict, and human-readable evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub evidence: String,
}

impl CheckResult {
    pub fn pass(name: impl Into<String>, evidence: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Pass,
            evidence: evidence.into(),
        }
    }

    pub fn fail(name: impl Into<String>, evidence: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            evidence: evidence.into(),
        }
    }

    pub fn skip(name: impl Into<String>, evidence: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Skipped,
            evidence: evidence.into(),
        }
    }

    /// Build a pass or fail from a boolean — the common "check this predicate"
    /// shape.
    pub fn from_bool(name: impl Into<String>, passed: bool, evidence: impl Into<String>) -> Self {
        if passed {
            Self::pass(name, evidence)
        } else {
            Self::fail(name, evidence)
        }
    }
}

/// The result of a conformance run: every check, against a described target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConformanceReport {
    /// Human description of the provider under test.
    pub target: String,
    pub checks: Vec<CheckResult>,
}

impl ConformanceReport {
    /// True when no check failed (skips don't fail a run). This is the
    /// "OCP conformant for your declared capability set" verdict.
    pub fn passed(&self) -> bool {
        !self
            .checks
            .iter()
            .any(|check| check.status == CheckStatus::Fail)
    }

    /// The checks that failed, in order.
    pub fn failures(&self) -> impl Iterator<Item = &CheckResult> {
        self.checks
            .iter()
            .filter(|check| check.status == CheckStatus::Fail)
    }

    /// `(passed, failed, skipped)` tallies.
    pub fn tally(&self) -> (usize, usize, usize) {
        let mut passed = 0;
        let mut failed = 0;
        let mut skipped = 0;
        for check in &self.checks {
            match check.status {
                CheckStatus::Pass => passed += 1,
                CheckStatus::Fail => failed += 1,
                CheckStatus::Skipped => skipped += 1,
            }
        }
        (passed, failed, skipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_passes_only_when_nothing_failed() {
        let report = ConformanceReport {
            target: "in-process: t".into(),
            checks: vec![
                CheckResult::pass("handshake", "ok"),
                CheckResult::skip("malformed-input-tolerance", "n/a"),
            ],
        };
        assert!(report.passed());
        assert_eq!(report.tally(), (1, 0, 1));

        let broken = ConformanceReport {
            target: "in-process: t".into(),
            checks: vec![
                CheckResult::pass("handshake", "ok"),
                CheckResult::fail("budget-honesty", "over budget"),
            ],
        };
        assert!(!broken.passed());
        assert_eq!(broken.failures().count(), 1);
        assert_eq!(broken.tally(), (1, 1, 0));
    }

    #[test]
    fn report_is_serde_roundtrippable_for_json_output() {
        let report = ConformanceReport {
            target: "stdio: ocp-example-docs".into(),
            checks: vec![CheckResult::from_bool(
                "frame-validity",
                true,
                "3 frames ok",
            )],
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: ConformanceReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
    }
}
