from __future__ import annotations

from fractions import Fraction

import pytest

import tb21_hybrid_analysis as hybrid
from tb21_evidence_contract import (
    CONFIRMATORY_DOMAIN,
    SCREEN_DOMAIN,
    bootstrap_index_stream_sha256,
    sampler_preimage,
    sha256_counter_index,
)
from tb21_hybrid_analysis import (
    HybridAnalysisError,
    accuracy_threshold,
    analyze_confirmatory,
    analyze_screen,
    development_gate,
    lower_percentile,
    meets_threshold,
    rank_development_candidates,
    screen_gate,
    token_threshold,
)


def _records(prefix: str, count: int) -> list[dict[str, str]]:
    return [{"task_name": f"{prefix}-{index:02d}"} for index in range(count)]


def _partition() -> dict[str, list[dict[str, str]]]:
    return {
        "development": _records("development", 10),
        "screen": _records("screen", 20),
        "untouched": _records("untouched", 59),
    }


def _rows(
    records: list[dict[str, str]],
    *,
    attempts: int,
    passes: int,
    tokens: int,
) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    for record in records:
        for attempt in range(attempts):
            rows.append(
                {
                    "task_name": record["task_name"],
                    "attempt_index": attempt + 1,
                    "verifier_pass": int(attempt < passes),
                    "normalized_tokens": tokens,
                }
            )
    return rows


def test_frozen_sampler_vectors_and_stream_digests() -> None:
    assert sampler_preimage(SCREEN_DOMAIN, 20260721, 0, 0, 0) == (
        b"stella-tb21-screen-bootstrap-v1\x0020260721\x000\x000\x000"
    )
    assert [
        sha256_counter_index(SCREEN_DOMAIN, 20260721, 0, draw, 20) for draw in range(5)
    ] == [18, 19, 15, 8, 5]
    assert [
        sha256_counter_index(CONFIRMATORY_DOMAIN, 20260721, 0, draw, 59)
        for draw in range(5)
    ] == [32, 18, 34, 52, 32]
    assert bootstrap_index_stream_sha256(SCREEN_DOMAIN, 20, 50_000) == (
        "c2215fe72122fba82f0259ef751d970dd0a69eace5f067d45e4e2de7c36abe14"
    )
    assert bootstrap_index_stream_sha256(CONFIRMATORY_DOMAIN, 59, 50_000) == (
        "d192cb83b44eb6c9777b62bf9839e05d7f618b253c2156313c0c81efdc3f9152"
    )


@pytest.mark.parametrize(
    ("args", "message"),
    [
        (("unregistered", 20260721, 0, 0, 0), "domain"),
        ((SCREEN_DOMAIN, True, 0, 0, 0), "nonnegative integers"),
        ((SCREEN_DOMAIN, 20260721, -1, 0, 0), "nonnegative integers"),
    ],
)
def test_sampler_rejects_unregistered_domains_and_invalid_counters(
    args: tuple[object, ...], message: str
) -> None:
    with pytest.raises(ValueError, match=message):
        sampler_preimage(*args)


@pytest.mark.parametrize("n", [True, 0, 65_536])
def test_sampler_rejects_invalid_task_counts(n: object) -> None:
    with pytest.raises(ValueError, match="task count"):
        sha256_counter_index(SCREEN_DOMAIN, 20260721, 0, 0, n)


@pytest.mark.parametrize("replicate_count", [True, 0, 1 << 64])
def test_stream_digest_rejects_invalid_replicate_counts(
    replicate_count: object,
) -> None:
    with pytest.raises(ValueError, match="replicate count"):
        bootstrap_index_stream_sha256(SCREEN_DOMAIN, 20, replicate_count)


def test_development_gate_uses_task_balanced_three_vs_five_denominators() -> None:
    records = _partition()["development"]
    stella = _rows(records, attempts=3, passes=3, tokens=9)
    claude = _rows(records, attempts=5, passes=4, tokens=10)

    result = development_gate(stella, claude, records)

    assert result["accuracy"]["point_improvement"] == Fraction(1, 4)
    assert result["tokens"]["point_improvement"] == Fraction(1, 10)
    assert result["stella_attempts_per_task"] == 3
    assert result["claude_attempts_per_task"] == 5
    assert result["statistical_gate_passed"] is True
    assert result["wall_clock_claim_eligible"] is False


def test_development_ranking_is_complete_first_then_uses_frozen_tiebreakers() -> None:
    candidates = [
        {
            "candidate_id": "incomplete",
            "status": "incomplete",
            "promotion_eligible": False,
            "verifier_passes": 30,
            "telemetry_normalized_tokens": 1,
            "provider_usage_delta_usd": "0.01",
        },
        {
            "candidate_id": "b",
            "status": "complete",
            "promotion_eligible": True,
            "verifier_passes": 20,
            "telemetry_normalized_tokens": 100,
            "provider_usage_delta_usd": "1.00",
        },
        {
            "candidate_id": "a",
            "status": "complete",
            "promotion_eligible": True,
            "verifier_passes": 20,
            "telemetry_normalized_tokens": 100,
            "provider_usage_delta_usd": "1.00",
        },
    ]

    ranked = rank_development_candidates(candidates)

    assert [candidate["candidate_id"] for candidate in ranked] == [
        "a",
        "b",
        "incomplete",
    ]


def test_development_ranking_applies_each_numeric_tiebreaker() -> None:
    def candidate(
        candidate_id: str, passes: int, tokens: int, cost: str
    ) -> dict[str, object]:
        return {
            "candidate_id": candidate_id,
            "status": "complete",
            "promotion_eligible": True,
            "verifier_passes": passes,
            "telemetry_normalized_tokens": tokens,
            "provider_usage_delta_usd": cost,
        }

    ranked = rank_development_candidates(
        [
            candidate("fewer-passes", 19, 1, "0.01"),
            candidate("more-tokens", 20, 101, "0.01"),
            candidate("higher-cost", 20, 100, "1.01"),
            candidate("winner", 20, 100, "1.00"),
        ]
    )

    assert [item["candidate_id"] for item in ranked] == [
        "winner",
        "higher-cost",
        "more-tokens",
        "fewer-passes",
    ]


def test_development_advancement_fails_with_too_few_complete_candidates() -> None:
    candidates = [
        {
            "candidate_id": "complete",
            "status": "complete",
            "promotion_eligible": True,
            "verifier_passes": 20,
            "telemetry_normalized_tokens": 100,
            "provider_usage_delta_usd": "1.00",
        },
        {
            "candidate_id": "incomplete",
            "status": "incomplete",
            "promotion_eligible": False,
            "verifier_passes": 30,
            "telemetry_normalized_tokens": 1,
            "provider_usage_delta_usd": "0.01",
        },
    ]

    with pytest.raises(HybridAnalysisError, match="too few complete candidates"):
        rank_development_candidates(candidates, advancement_slots=2)


def test_exact_ten_percent_boundary_is_not_binary_float_dependent() -> None:
    assert meets_threshold(Fraction(1, 10), strict=False) is True
    assert meets_threshold(Fraction(1, 10), strict=True) is False
    assert accuracy_threshold(stella_passes=11, claude_passes=10, strict=False)
    assert not accuracy_threshold(stella_passes=11, claude_passes=10, strict=True)
    assert token_threshold(stella_tokens=9, claude_tokens=10, strict=False)
    assert not token_threshold(stella_tokens=9, claude_tokens=10, strict=True)


def test_screen_joint_pass_boundary_is_exactly_35_000() -> None:
    assert screen_gate(Fraction(1, 10), Fraction(1, 10), 35_000) is True
    assert screen_gate(Fraction(1, 10), Fraction(1, 10), 34_999) is False


def test_screen_zero_comparator_accuracy_replicates_are_conservative_misses() -> None:
    records = _partition()["screen"]
    stella = _rows(records, attempts=5, passes=5, tokens=8)
    claude = _rows(records, attempts=5, passes=0, tokens=10)
    for row in claude[:5]:
        row["verifier_pass"] = 1

    result = analyze_screen(stella, claude, records)

    assert result["accuracy"]["zero_comparator_replicates"] > 0
    assert (
        result["joint_threshold_passes"]
        + result["accuracy"]["zero_comparator_replicates"]
        == 50_000
    )
    assert result["statistical_gate_passed"] is False
    assert result["wall_clock_claim_eligible"] is False


def test_nonpositive_comparator_tokens_fail_analysis() -> None:
    records = _partition()["screen"]
    stella = _rows(records, attempts=5, passes=5, tokens=8)
    claude = _rows(records, attempts=5, passes=4, tokens=0)

    with pytest.raises(HybridAnalysisError, match="comparator token total"):
        analyze_screen(stella, claude, records)


def test_replicate_only_zero_comparator_tokens_fail_analysis() -> None:
    records = _partition()["screen"]
    stella = _rows(records, attempts=5, passes=5, tokens=8)
    claude = _rows(records, attempts=5, passes=4, tokens=0)
    for row in claude[-5:]:
        row["normalized_tokens"] = 10

    with pytest.raises(HybridAnalysisError, match="comparator token total"):
        analyze_screen(stella, claude, records)


def test_assigned_negative_infinity_sorts_below_every_fraction() -> None:
    values = [Fraction(1, 5)] * 50_000
    values[:1_250] = [hybrid._NEGATIVE_INFINITY] * 1_250

    assert lower_percentile(values) is hybrid._NEGATIVE_INFINITY
    values[1_249] = Fraction(-10_000, 1)
    assert lower_percentile(values) == Fraction(-10_000, 1)


@pytest.mark.parametrize("value", [float("-inf"), float("nan"), float("inf")])
def test_unexpected_nan_and_positive_infinity_fail_analysis(value: float) -> None:
    with pytest.raises(HybridAnalysisError, match="exact improvement"):
        meets_threshold(value, strict=False)


def test_confirmatory_requires_both_strict_lower_bounds_and_only_59_primary_tasks() -> (
    None
):
    partition = _partition()
    all_records = [
        *partition["development"],
        *partition["screen"],
        *partition["untouched"],
    ]
    stella = _rows(all_records, attempts=5, passes=4, tokens=8)
    claude = _rows(all_records, attempts=5, passes=3, tokens=10)

    result = analyze_confirmatory(stella, claude, partition)

    assert result["task_count"] == 59
    assert result["accuracy"]["point_improvement"] == Fraction(1, 3)
    assert result["tokens"]["point_improvement"] == Fraction(1, 5)
    assert result["accuracy"]["lower_bound_order_index"] == 1_249
    assert result["accuracy"]["lower_bound"] == Fraction(1, 3)
    assert result["tokens"]["lower_bound"] == Fraction(1, 5)
    assert result["statistical_claim_established"] is True
    assert result["descriptive_all_89"]["task_count"] == 89
    assert result["descriptive_all_89"]["accuracy"]["point_improvement"] == Fraction(
        1, 3
    )
    assert result["descriptive_all_89"]["tokens"]["point_improvement"] == Fraction(1, 5)
    assert result["wall_clock_claim_eligible"] is False


def test_confirmatory_observed_zero_comparator_accuracy_fails() -> None:
    partition = _partition()
    records = [
        *partition["development"],
        *partition["screen"],
        *partition["untouched"],
    ]
    stella = _rows(records, attempts=5, passes=4, tokens=8)
    claude = _rows(records, attempts=5, passes=0, tokens=10)

    with pytest.raises(HybridAnalysisError, match="comparator accuracy"):
        analyze_confirmatory(stella, claude, partition)


def test_confirmatory_requires_full_partition_and_all_445_rows() -> None:
    records = _partition()["untouched"]
    stella = _rows(records, attempts=5, passes=4, tokens=8)
    claude = _rows(records, attempts=5, passes=3, tokens=10)

    with pytest.raises(HybridAnalysisError, match="full task partition"):
        analyze_confirmatory(stella, claude, records)


def test_duplicate_attempt_slots_fail_analysis() -> None:
    records = _partition()["development"]
    stella = _rows(records, attempts=3, passes=3, tokens=9)
    claude = _rows(records, attempts=5, passes=4, tokens=10)
    stella[1]["attempt_index"] = 1

    with pytest.raises(HybridAnalysisError, match="attempt slots"):
        development_gate(stella, claude, records)


def test_confirmatory_lower_bound_equal_to_ten_percent_is_not_sufficient() -> None:
    partition = _partition()
    all_records = [
        *partition["development"],
        *partition["screen"],
        *partition["untouched"],
    ]
    stella = _rows(all_records, attempts=5, passes=4, tokens=9)
    claude = _rows(all_records, attempts=5, passes=3, tokens=10)

    result = analyze_confirmatory(stella, claude, partition)

    assert result["tokens"]["lower_bound"] == Fraction(1, 10)
    assert result["statistical_claim_established"] is False


def test_confirmatory_negative_infinity_lower_bound_stays_internal() -> None:
    partition = _partition()
    all_records = [
        *partition["development"],
        *partition["screen"],
        *partition["untouched"],
    ]
    stella = _rows(all_records, attempts=5, passes=5, tokens=8)
    claude = _rows(all_records, attempts=5, passes=0, tokens=10)
    first_untouched = partition["untouched"][0]["task_name"]
    for row in claude:
        if row["task_name"] == first_untouched:
            row["verifier_pass"] = 1

    result = analyze_confirmatory(stella, claude, partition)

    assert result["accuracy"]["negative_infinity_replicates"] > 1_249
    assert result["accuracy"]["lower_bound"] is None
    assert result["accuracy"]["lower_bound_marker"] == "negative_infinity"
    assert result["statistical_claim_established"] is False
