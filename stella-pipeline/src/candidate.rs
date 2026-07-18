//! Best-of-N candidate selection (L-E7). Best-of-N is **opt-in, never the
//! default**: disciplined single-shot beat multi-attempt configurations on
//! cost-per-resolve in head-to-head SWE-bench runs, so generating N
//! candidates is a quality *lever* the user pulls with `--candidates N`, paid
//! for with N× the execution cost. This module owns the pure selection
//! logic; generating the candidates (running execute + verify N times) is
//! orchestration in [`crate::pipeline`].
//!
//! Selection ranks candidates by the *strength of their verification
//! evidence* — a deterministically-verified candidate always beats a
//! judge-passed one, which beats an unverified one, which beats a failed one
//! — with diff size as the tiebreak (smaller, all else equal, is better).

/// How strongly a candidate execution was verified — the primary sort key for
/// best-of-N selection. Ordered worst → best so the derived `Ord` makes
/// `max` pick the strongest evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateScore {
    /// Verification failed (touched tests red, or judge FAIL).
    Failed,
    /// No verdict could be established (judge unavailable, no tests) — better
    /// than an outright failure, worse than any pass.
    Unverified,
    /// A model judge passed it (softer evidence than a deterministic flip).
    JudgePass,
    /// The deterministic ladder passed it (flip + green + budget) — the
    /// strongest evidence.
    DeterministicPass,
}

/// One candidate's summary for selection: its verification score and the size
/// of its diff (the tiebreak). Kept a plain owned struct so selection is a
/// pure function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateSummary {
    pub score: CandidateScore,
    /// Diff size in lines — the tiebreak when scores are equal (smaller wins).
    pub diff_lines: u32,
}

/// Select the index of the best candidate: highest [`CandidateScore`] first,
/// then the smallest `diff_lines` as a tiebreak, then the earliest index (a
/// stable choice so best-of-N is reproducible given identical evidence).
/// Returns `None` only for an empty slice.
///
/// The earliest-index tiebreak matters for determinism: candidate 0 is the
/// single-shot result, so an all-equal field selects it — best-of-N never
/// pays for a different answer than single-shot unless a *later* candidate is
/// strictly better.
pub fn select_best_candidate(candidates: &[CandidateSummary]) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    let mut best_idx = 0usize;
    for (i, cand) in candidates.iter().enumerate().skip(1) {
        let best = &candidates[best_idx];
        let better = match cand.score.cmp(&best.score) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => cand.diff_lines < best.diff_lines,
        };
        if better {
            best_idx = i;
        }
    }
    Some(best_idx)
}

/// Map a verification result to a [`CandidateScore`]. Centralizes the one
/// place the pipeline turns "how did verify go" into a comparable score, so
/// best-of-N and the single-shot verdict never drift apart.
pub fn score_from_verification(
    deterministic_pass: bool,
    judge_passed: Option<bool>,
) -> CandidateScore {
    if deterministic_pass {
        return CandidateScore::DeterministicPass;
    }
    match judge_passed {
        Some(true) => CandidateScore::JudgePass,
        Some(false) => CandidateScore::Failed,
        None => CandidateScore::Unverified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(score: CandidateScore, diff_lines: u32) -> CandidateSummary {
        CandidateSummary { score, diff_lines }
    }

    #[test]
    fn score_ordering_is_worst_to_best() {
        assert!(CandidateScore::Failed < CandidateScore::Unverified);
        assert!(CandidateScore::Unverified < CandidateScore::JudgePass);
        assert!(CandidateScore::JudgePass < CandidateScore::DeterministicPass);
    }

    #[test]
    fn empty_slice_selects_nothing() {
        assert_eq!(select_best_candidate(&[]), None);
    }

    #[test]
    fn single_candidate_selects_itself() {
        assert_eq!(
            select_best_candidate(&[c(CandidateScore::Unverified, 10)]),
            Some(0)
        );
    }

    #[test]
    fn strongest_evidence_wins_over_diff_size() {
        // Candidate 1 has a bigger diff but stronger evidence — it wins.
        let cands = [
            c(CandidateScore::JudgePass, 5),
            c(CandidateScore::DeterministicPass, 500),
        ];
        assert_eq!(select_best_candidate(&cands), Some(1));
    }

    #[test]
    fn equal_scores_break_ties_by_smaller_diff() {
        let cands = [
            c(CandidateScore::DeterministicPass, 80),
            c(CandidateScore::DeterministicPass, 20),
            c(CandidateScore::DeterministicPass, 200),
        ];
        assert_eq!(select_best_candidate(&cands), Some(1));
    }

    #[test]
    fn all_equal_selects_the_earliest_index_for_reproducibility() {
        let cands = [
            c(CandidateScore::JudgePass, 10),
            c(CandidateScore::JudgePass, 10),
            c(CandidateScore::JudgePass, 10),
        ];
        assert_eq!(
            select_best_candidate(&cands),
            Some(0),
            "an all-equal field must pick candidate 0 (the single-shot result)"
        );
    }

    #[test]
    fn a_later_strictly_better_candidate_is_chosen_over_index_zero() {
        let cands = [
            c(CandidateScore::Unverified, 10),
            c(CandidateScore::DeterministicPass, 10),
        ];
        assert_eq!(select_best_candidate(&cands), Some(1));
    }

    #[test]
    fn failed_candidates_are_the_floor() {
        let cands = [
            c(CandidateScore::Failed, 1),
            c(CandidateScore::Unverified, 100),
        ];
        // Even a large-diff unverified candidate beats a tiny failed one.
        assert_eq!(select_best_candidate(&cands), Some(1));
    }

    #[test]
    fn score_from_verification_prefers_deterministic_then_judge() {
        assert_eq!(
            score_from_verification(true, Some(false)),
            CandidateScore::DeterministicPass,
            "a deterministic pass wins even if a later judge disagreed"
        );
        assert_eq!(
            score_from_verification(false, Some(true)),
            CandidateScore::JudgePass
        );
        assert_eq!(
            score_from_verification(false, Some(false)),
            CandidateScore::Failed
        );
        assert_eq!(
            score_from_verification(false, None),
            CandidateScore::Unverified
        );
    }
}
