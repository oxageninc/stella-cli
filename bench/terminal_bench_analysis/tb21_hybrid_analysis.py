"""Pure exact estimands for the preregistered TB 2.1 hybrid study."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from decimal import Decimal, InvalidOperation
from fractions import Fraction

from tb21_evidence_contract import (
    BOOTSTRAP_REPLICATES,
    BOOTSTRAP_SEED,
    CONFIRMATORY_DOMAIN,
    SCREEN_DOMAIN,
    bootstrap_index_stream_sha256,
    sha256_counter_index,
)

_THRESHOLD_NUMERATOR = 1
_THRESHOLD_DENOMINATOR = 10
_SCREEN_JOINT_PASS_MINIMUM = 35_000
_CONFIRMATORY_LOWER_ORDER_INDEX = 1_249


class HybridAnalysisError(ValueError):
    """Raised when exact hybrid-study analysis cannot be completed."""


class _NegativeInfinity:
    __slots__ = ()

    def __repr__(self) -> str:
        return "NEGATIVE_INFINITY"


_NEGATIVE_INFINITY = _NegativeInfinity()
type ExactImprovement = Fraction | _NegativeInfinity


def _require_nonnegative_int(value: object, *, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise HybridAnalysisError(f"{label} must be a nonnegative integer")
    return value


def _require_positive_int(value: object, *, label: str) -> int:
    result = _require_nonnegative_int(value, label=label)
    if result == 0:
        raise HybridAnalysisError(f"{label} must be positive")
    return result


def is_exact_improvement(value: object) -> bool:
    return isinstance(value, Fraction) or value is _NEGATIVE_INFINITY


def exact_improvement_sort_key(value: ExactImprovement) -> tuple[int, Fraction]:
    if value is _NEGATIVE_INFINITY:
        return (0, Fraction(0))
    if not isinstance(value, Fraction):
        raise HybridAnalysisError("value is not an exact improvement")
    return (1, value)


def meets_threshold(value: ExactImprovement, *, strict: bool) -> bool:
    """Compare an exact improvement with ten percent without binary float."""
    if not is_exact_improvement(value):
        raise HybridAnalysisError("value is not an exact improvement")
    if value is _NEGATIVE_INFINITY:
        return False
    threshold = Fraction(_THRESHOLD_NUMERATOR, _THRESHOLD_DENOMINATOR)
    return value > threshold if strict else value >= threshold


def accuracy_improvement(stella_passes: int, claude_passes: int) -> Fraction:
    stella = _require_nonnegative_int(stella_passes, label="Stella pass total")
    claude = _require_positive_int(claude_passes, label="observed comparator accuracy")
    return Fraction(stella, claude) - 1


def token_improvement(stella_tokens: int, claude_tokens: int) -> Fraction:
    stella = _require_nonnegative_int(stella_tokens, label="Stella token total")
    claude = _require_positive_int(claude_tokens, label="comparator token total")
    return 1 - Fraction(stella, claude)


def accuracy_threshold(*, stella_passes: int, claude_passes: int, strict: bool) -> bool:
    """Apply the accuracy threshold by exact integer cross-product."""
    stella = _require_nonnegative_int(stella_passes, label="Stella pass total")
    claude = _require_positive_int(claude_passes, label="observed comparator accuracy")
    left = _THRESHOLD_DENOMINATOR * stella
    right = (_THRESHOLD_DENOMINATOR + _THRESHOLD_NUMERATOR) * claude
    return left > right if strict else left >= right


def token_threshold(*, stella_tokens: int, claude_tokens: int, strict: bool) -> bool:
    """Apply the token threshold by exact integer cross-product."""
    stella = _require_nonnegative_int(stella_tokens, label="Stella token total")
    claude = _require_positive_int(claude_tokens, label="comparator token total")
    left = _THRESHOLD_DENOMINATOR * stella
    right = (_THRESHOLD_DENOMINATOR - _THRESHOLD_NUMERATOR) * claude
    return left < right if strict else left <= right


def lower_percentile(values: Sequence[ExactImprovement]) -> ExactImprovement:
    """Return the frozen noninterpolated one-sided 97.5% lower bound."""
    if len(values) != BOOTSTRAP_REPLICATES or any(
        not is_exact_improvement(value) for value in values
    ):
        raise HybridAnalysisError("confirmatory bootstrap produced an invalid value")
    return sorted(values, key=exact_improvement_sort_key)[
        _CONFIRMATORY_LOWER_ORDER_INDEX
    ]


def _ordered_task_names(
    partition: object, *, split: str, expected_count: int
) -> list[str]:
    records = partition.get(split) if isinstance(partition, Mapping) else partition
    if not isinstance(records, Sequence) or isinstance(records, (str, bytes)):
        raise HybridAnalysisError(f"{split} partition must be an ordered sequence")
    names: list[str] = []
    for record in records:
        if not isinstance(record, Mapping):
            raise HybridAnalysisError(f"{split} partition record must be an object")
        name = record.get("task_name")
        if not isinstance(name, str) or not name:
            raise HybridAnalysisError(f"{split} partition task_name is invalid")
        names.append(name)
    if len(names) != expected_count or len(set(names)) != expected_count:
        raise HybridAnalysisError(
            f"{split} partition must contain {expected_count} unique tasks"
        )
    return names


def _all_confirmatory_task_names(partition: object) -> tuple[list[str], list[str]]:
    if not isinstance(partition, Mapping):
        raise HybridAnalysisError(
            "confirmatory analysis requires the full task partition"
        )
    primary = _ordered_task_names(partition, split="untouched", expected_count=59)
    development = _ordered_task_names(partition, split="development", expected_count=10)
    screen = _ordered_task_names(partition, split="screen", expected_count=20)
    all_names = [*development, *screen, *primary]
    if len(set(all_names)) != 89:
        raise HybridAnalysisError("confirmatory partition must contain 89 unique tasks")
    return primary, all_names


def _task_totals(
    rows: Sequence[Mapping[str, object]],
    task_names: Sequence[str],
    *,
    attempts: int,
    label: str,
) -> list[tuple[int, int]]:
    by_task: dict[str, dict[int, tuple[int, int]]] = {name: {} for name in task_names}
    if isinstance(rows, (str, bytes)):
        raise HybridAnalysisError(f"{label} rows must be a sequence")
    for row in rows:
        if not isinstance(row, Mapping):
            raise HybridAnalysisError(f"{label} trial row must be an object")
        task_name = row.get("task_name")
        if task_name not in by_task:
            raise HybridAnalysisError(f"{label} trial row has an unexpected task")
        attempt_index = row.get("attempt_index")
        if (
            isinstance(attempt_index, bool)
            or not isinstance(attempt_index, int)
            or not 1 <= attempt_index <= attempts
            or attempt_index in by_task[str(task_name)]
        ):
            raise HybridAnalysisError(
                f"{label} attempt slots must be unique integers 1..{attempts}"
            )
        verifier_pass = row.get("verifier_pass")
        if (
            isinstance(verifier_pass, bool)
            or not isinstance(verifier_pass, int)
            or verifier_pass not in {0, 1}
        ):
            raise HybridAnalysisError(
                f"{label} verifier_pass must be the integer zero or one"
            )
        tokens = _require_nonnegative_int(
            row.get("normalized_tokens"), label=f"{label} normalized_tokens"
        )
        by_task[str(task_name)][attempt_index] = (verifier_pass, tokens)
    expected_slots = set(range(1, attempts + 1))
    bad = [name for name, values in by_task.items() if set(values) != expected_slots]
    if bad:
        raise HybridAnalysisError(
            f"{label} must contain exactly {attempts} complete trials per task"
        )
    return [
        (
            sum(by_task[name][slot][0] for slot in range(1, attempts + 1)),
            sum(by_task[name][slot][1] for slot in range(1, attempts + 1)),
        )
        for name in task_names
    ]


def _point_result(
    stella: Sequence[tuple[int, int]], comparator: Sequence[tuple[int, int]]
) -> dict[str, dict[str, Fraction]]:
    stella_passes = sum(value[0] for value in stella)
    comparator_passes = sum(value[0] for value in comparator)
    stella_tokens = sum(value[1] for value in stella)
    comparator_tokens = sum(value[1] for value in comparator)
    return {
        "accuracy": {
            "point_improvement": accuracy_improvement(stella_passes, comparator_passes)
        },
        "tokens": {
            "point_improvement": token_improvement(stella_tokens, comparator_tokens)
        },
    }


def rank_development_candidates(
    candidates: Sequence[Mapping[str, object]],
    *,
    advancement_slots: int | None = None,
) -> list[dict[str, object]]:
    """Rank v3 outcome-derived statistics with the frozen tiebreakers.

    Each record combines validated v3 outcome fields with the integer
    ``verifier_passes`` derived from that outcome's normalized trial rows.
    """
    validated: list[dict[str, object]] = []
    seen: set[str] = set()
    for candidate in candidates:
        if not isinstance(candidate, Mapping):
            raise HybridAnalysisError("development candidate must be an object")
        candidate_id = candidate.get("candidate_id")
        if (
            not isinstance(candidate_id, str)
            or not candidate_id
            or candidate_id in seen
        ):
            raise HybridAnalysisError("development candidate_id is invalid or reused")
        seen.add(candidate_id)
        status = candidate.get("status")
        if status not in {"complete", "ineligible", "incomplete"}:
            raise HybridAnalysisError("development status is invalid")
        promotion_eligible = candidate.get("promotion_eligible")
        if not isinstance(promotion_eligible, bool):
            raise HybridAnalysisError("development promotion_eligible must be boolean")
        if promotion_eligible and status != "complete":
            raise HybridAnalysisError(
                "development promotion eligibility requires complete status"
            )
        passes = _require_nonnegative_int(
            candidate.get("verifier_passes"), label="development verifier_passes"
        )
        tokens = _require_nonnegative_int(
            candidate.get("telemetry_normalized_tokens"),
            label="development telemetry_normalized_tokens",
        )
        cost_text = candidate.get("provider_usage_delta_usd")
        try:
            cost = Decimal(cost_text) if isinstance(cost_text, str) else None
        except InvalidOperation:
            cost = None
        if cost is None or not cost.is_finite() or cost < 0:
            raise HybridAnalysisError(
                "development provider_usage_delta_usd must be a finite decimal string"
            )
        item = dict(candidate)
        item.update(
            {
                "candidate_id": candidate_id,
                "status": status,
                "promotion_eligible": promotion_eligible,
                "verifier_passes": passes,
                "telemetry_normalized_tokens": tokens,
                "provider_usage_delta_usd": cost_text,
            }
        )
        validated.append(item)

    def rank_key(item: dict[str, object]) -> tuple[object, ...]:
        complete = item["status"] == "complete" and item["promotion_eligible"] is True
        return (
            not complete,
            -int(item["verifier_passes"]),
            int(item["telemetry_normalized_tokens"]),
            Decimal(str(item["provider_usage_delta_usd"])),
            str(item["candidate_id"]),
        )

    ranked = sorted(
        validated,
        key=rank_key,
    )
    if advancement_slots is not None:
        slots = _require_positive_int(
            advancement_slots, label="development advancement_slots"
        )
        complete_count = sum(
            item["status"] == "complete" and item["promotion_eligible"] is True
            for item in ranked
        )
        if complete_count < slots:
            raise HybridAnalysisError(
                "development round has too few complete candidates to advance"
            )
    return ranked


def development_gate(
    stella_rows: Sequence[Mapping[str, object]],
    claude_rows: Sequence[Mapping[str, object]],
    partition: object,
) -> dict[str, object]:
    """Evaluate the exploratory Round 3 gate using task-balanced 3-vs-5 means."""
    names = _ordered_task_names(partition, split="development", expected_count=10)
    stella = _task_totals(stella_rows, names, attempts=3, label="Stella")
    claude = _task_totals(claude_rows, names, attempts=5, label="Claude")
    stella_accuracy_sum = sum(Fraction(value[0], 3) for value in stella)
    claude_accuracy_sum = sum(Fraction(value[0], 5) for value in claude)
    if claude_accuracy_sum <= 0:
        raise HybridAnalysisError("observed comparator accuracy must be positive")
    stella_token_sum = sum(Fraction(value[1], 3) for value in stella)
    claude_token_sum = sum(Fraction(value[1], 5) for value in claude)
    if claude_token_sum <= 0:
        raise HybridAnalysisError("comparator token total must be positive")
    accuracy = stella_accuracy_sum / claude_accuracy_sum - 1
    tokens = 1 - stella_token_sum / claude_token_sum
    return {
        "task_count": 10,
        "stella_attempts_per_task": 3,
        "claude_attempts_per_task": 5,
        "accuracy": {"point_improvement": accuracy},
        "tokens": {"point_improvement": tokens},
        "statistical_gate_passed": meets_threshold(accuracy, strict=False)
        and meets_threshold(tokens, strict=False),
        "wall_clock_claim_eligible": False,
    }


def screen_gate(
    accuracy_point: ExactImprovement,
    token_point: ExactImprovement,
    joint_threshold_passes: int,
) -> bool:
    passes = _require_nonnegative_int(
        joint_threshold_passes, label="screen joint threshold passes"
    )
    if passes > BOOTSTRAP_REPLICATES:
        raise HybridAnalysisError("screen joint threshold passes exceed draws")
    return (
        meets_threshold(accuracy_point, strict=False)
        and meets_threshold(token_point, strict=False)
        and passes >= _SCREEN_JOINT_PASS_MINIMUM
    )


def analyze_screen(
    stella_rows: Sequence[Mapping[str, object]],
    claude_rows: Sequence[Mapping[str, object]],
    partition: object,
) -> dict[str, object]:
    """Evaluate the sealed screen with paired exact task-cluster draws."""
    names = _ordered_task_names(partition, split="screen", expected_count=20)
    stella = _task_totals(stella_rows, names, attempts=5, label="Stella")
    claude = _task_totals(claude_rows, names, attempts=5, label="Claude")
    point = _point_result(stella, claude)
    joint_passes = 0
    zero_comparator = 0
    for replicate in range(BOOTSTRAP_REPLICATES):
        stella_passes = comparator_passes = 0
        stella_tokens = comparator_tokens = 0
        for draw in range(len(names)):
            index = sha256_counter_index(
                SCREEN_DOMAIN, BOOTSTRAP_SEED, replicate, draw, len(names)
            )
            stella_passes += stella[index][0]
            stella_tokens += stella[index][1]
            comparator_passes += claude[index][0]
            comparator_tokens += claude[index][1]
        if comparator_tokens <= 0:
            raise HybridAnalysisError("comparator token total must be positive")
        if comparator_passes <= 0:
            zero_comparator += 1
            continue
        if accuracy_threshold(
            stella_passes=stella_passes,
            claude_passes=comparator_passes,
            strict=False,
        ) and token_threshold(
            stella_tokens=stella_tokens,
            claude_tokens=comparator_tokens,
            strict=False,
        ):
            joint_passes += 1
    accuracy_point = point["accuracy"]["point_improvement"]
    token_point = point["tokens"]["point_improvement"]
    return {
        "task_count": 20,
        "bootstrap_replicates": BOOTSTRAP_REPLICATES,
        "sampler_seed": BOOTSTRAP_SEED,
        "sampler_domain": SCREEN_DOMAIN,
        "index_stream_sha256": bootstrap_index_stream_sha256(
            SCREEN_DOMAIN, len(names), BOOTSTRAP_REPLICATES
        ),
        "accuracy": {
            "point_improvement": accuracy_point,
            "zero_comparator_replicates": zero_comparator,
        },
        "tokens": {"point_improvement": token_point},
        "joint_threshold_passes": joint_passes,
        "statistical_gate_passed": screen_gate(
            accuracy_point, token_point, joint_passes
        ),
        "wall_clock_claim_eligible": False,
    }


def _select_totals(
    totals: Sequence[tuple[int, int]],
    all_names: Sequence[str],
    selected_names: Sequence[str],
) -> list[tuple[int, int]]:
    by_name = dict(zip(all_names, totals, strict=True))
    return [by_name[name] for name in selected_names]


def analyze_confirmatory(
    stella_rows: Sequence[Mapping[str, object]],
    claude_rows: Sequence[Mapping[str, object]],
    partition: object,
) -> dict[str, object]:
    """Evaluate the untouched 59-task claim and descriptive all-89 result."""
    primary_names, all_names = _all_confirmatory_task_names(partition)
    stella_all = _task_totals(stella_rows, all_names, attempts=5, label="Stella")
    claude_all = _task_totals(claude_rows, all_names, attempts=5, label="Claude")
    stella = _select_totals(stella_all, all_names, primary_names)
    claude = _select_totals(claude_all, all_names, primary_names)
    point = _point_result(stella, claude)
    accuracy_values: list[ExactImprovement] = []
    token_values: list[ExactImprovement] = []
    negative_infinity_count = 0
    for replicate in range(BOOTSTRAP_REPLICATES):
        stella_passes = comparator_passes = 0
        stella_tokens = comparator_tokens = 0
        for draw in range(len(primary_names)):
            index = sha256_counter_index(
                CONFIRMATORY_DOMAIN,
                BOOTSTRAP_SEED,
                replicate,
                draw,
                len(primary_names),
            )
            stella_passes += stella[index][0]
            stella_tokens += stella[index][1]
            comparator_passes += claude[index][0]
            comparator_tokens += claude[index][1]
        if comparator_tokens <= 0:
            raise HybridAnalysisError("comparator token total must be positive")
        if comparator_passes <= 0:
            accuracy_values.append(_NEGATIVE_INFINITY)
            negative_infinity_count += 1
        else:
            accuracy_values.append(
                accuracy_improvement(stella_passes, comparator_passes)
            )
        token_values.append(token_improvement(stella_tokens, comparator_tokens))
    accuracy_lower = lower_percentile(accuracy_values)
    token_lower = lower_percentile(token_values)
    accuracy_lower_is_negative_infinity = accuracy_lower is _NEGATIVE_INFINITY
    accuracy_point = point["accuracy"]["point_improvement"]
    token_point = point["tokens"]["point_improvement"]
    statistical_claim_established = (
        meets_threshold(accuracy_point, strict=False)
        and meets_threshold(token_point, strict=False)
        and meets_threshold(accuracy_lower, strict=True)
        and meets_threshold(token_lower, strict=True)
    )
    descriptive = _point_result(stella_all, claude_all)
    return {
        "task_count": 59,
        "bootstrap_replicates": BOOTSTRAP_REPLICATES,
        "sampler_seed": BOOTSTRAP_SEED,
        "sampler_domain": CONFIRMATORY_DOMAIN,
        "index_stream_sha256": bootstrap_index_stream_sha256(
            CONFIRMATORY_DOMAIN, len(primary_names), BOOTSTRAP_REPLICATES
        ),
        "accuracy": {
            "point_improvement": accuracy_point,
            "lower_bound": (
                None if accuracy_lower_is_negative_infinity else accuracy_lower
            ),
            "lower_bound_marker": (
                "negative_infinity" if accuracy_lower_is_negative_infinity else None
            ),
            "lower_bound_order_index": _CONFIRMATORY_LOWER_ORDER_INDEX,
            "negative_infinity_replicates": negative_infinity_count,
        },
        "tokens": {
            "point_improvement": token_point,
            "lower_bound": token_lower,
            "lower_bound_order_index": _CONFIRMATORY_LOWER_ORDER_INDEX,
        },
        "statistical_claim_established": statistical_claim_established,
        "descriptive_all_89": {
            "task_count": len(all_names),
            **descriptive,
            "wall_clock_claim_eligible": False,
        },
        "wall_clock_claim_eligible": False,
    }
