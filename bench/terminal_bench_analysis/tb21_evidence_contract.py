"""Canonical task-partition contract for the preregistered TB 2.1 study."""

from __future__ import annotations

import hashlib
import json
import re
from collections.abc import Sequence
from copy import deepcopy
from datetime import datetime
from decimal import Decimal, InvalidOperation
from functools import cache

from tb21_study_seed import TASK_IDENTITIES, TaskIdentity

STUDY_ID = "stella-tb21-hybrid-study-v1"
TASK_PARTITION_SCHEMA = "stella-tb21-task-partition-v1"
BOOTSTRAP_SEED = 20260721
BOOTSTRAP_REPLICATES = 50_000
SCREEN_DOMAIN = "stella-tb21-screen-bootstrap-v1"
CONFIRMATORY_DOMAIN = "stella-tb21-confirmatory-bootstrap-v1"
BOOTSTRAP_INDEX_STREAM_DOMAIN = "stella-tb21-bootstrap-index-stream-v1"
DEVELOPMENT_TASK_NAMES = (
    "fix-git",
    "filter-js-from-html",
    "kv-store-grpc",
    "large-scale-text-editing",
    "regex-log",
    "schemelike-metacircular-eval",
    "sqlite-with-gcov",
    "bn-fit-modify",
    "make-mips-interpreter",
    "train-fasttext",
)

_SPLIT_NAMES = ("development", "screen", "untouched")
_PARTITION_FIELDS = {
    "schema_version",
    "study_id",
    *_SPLIT_NAMES,
    "split_sha256",
}
_RECORD_FIELDS = {
    "task_name",
    "canonical_task_reference",
    "task_checksum",
}
_SHA256_RE = re.compile(r"[0-9a-f]{64}")
_TASK_REFERENCE_RE = re.compile(r"sha256:[0-9a-f]{64}")

RUN_LEDGER_SCHEMA = "stella-tb21-run-ledger-v3"
RUN_LEDGER_FIELDS = frozenset(
    {
        "schema_version",
        "study_id",
        "paths",
        "task_partition_sha256",
        "budget_authorizations",
        "prior_exploration_disclosure",
        "preregistrations",
        "candidates",
        "intents",
        "publications",
        "outcomes",
    }
)
STAGE_SHAPES = {
    "readiness": {
        "tasks": 1,
        "attempts": 1,
        "max_candidates": 1,
        "max_intents": 1,
        "max_trials": 1,
        "max_spend_cents": 100,
        "harbor_concurrency": 1,
    },
    "development_round_1": {
        "tasks": 10,
        "attempts": 1,
        "max_candidates": 8,
        "max_intents": 16,
        "max_trials": 80,
        "max_spend_cents": 2_400,
        "harbor_concurrency": 3,
    },
    "development_round_2": {
        "tasks": 10,
        "attempts": 1,
        "max_candidates": 4,
        "max_intents": 8,
        "max_trials": 40,
        "max_spend_cents": 1_200,
        "harbor_concurrency": 3,
    },
    "development_round_3": {
        "tasks": 10,
        "attempts": 3,
        "max_candidates": 2,
        "max_intents": 4,
        "max_trials": 60,
        "max_spend_cents": 1_800,
        "harbor_concurrency": 3,
    },
    "screen": {
        "tasks": 20,
        "attempts": 5,
        "max_candidates": 1,
        "max_intents": 1,
        "max_trials": 100,
        "max_spend_cents": 3_000,
        "harbor_concurrency": 1,
    },
    "confirmatory": {
        "tasks": 89,
        "attempts": 5,
        "max_candidates": 1,
        "max_intents": 1,
        "max_trials": 445,
        "max_spend_cents": None,
        "harbor_concurrency": 1,
    },
}

FIXED_PATHS = {
    "task_partition": "bench/evidence/stella-tb21-task-partition.json",
    "run_ledger": "bench/evidence/stella-tb21-run-ledger.json",
    "study_manifest": "bench/evidence/stella-tb21-study-manifest.json",
    "host_attestations": "bench/evidence/host-attestations",
}
PRIOR_EXPLORATION_DISCLOSURE = {
    "excluded_historical_job_ids": [
        "9b704487-9d21-46a7-8103-e5396cb7d4ea",
        "0c44d9ee-4389-4c7a-8445-ea4be2404115",
        "c5686c41-1d2d-41cf-a275-177c2e6878b3",
        "37ee4276-8595-4ff9-8507-be21adb891cc",
        "7e59ed1e-2abe-40b9-bf7e-6b24c7f9a350",
    ],
    "eligibility_statement": (
        "No v6 paid readiness, calibration, or primary call is eligible for "
        "the hybrid study."
    ),
}

_LEDGER_ARRAY_FIELDS = (
    "budget_authorizations",
    "preregistrations",
    "candidates",
    "intents",
    "publications",
    "outcomes",
)
_BUDGET_FIELDS = {
    "sequence",
    "authorization_id",
    "scope",
    "provider_key_name",
    "hard_limit_cents",
    "provider_cap_cents",
    "infrastructure_cap_cents",
    "reserve_cents",
    "authorization_commit",
    "declared_at",
}
_CANDIDATE_FIELDS = {
    "sequence",
    "candidate_id",
    "stage",
    "candidate_sha256",
    "record_sha256",
    "source_commit",
    "binary_sha256",
    "source_tree_sha256",
    "config_sha256",
    "adapter_sha256",
    "analyzer_sha256",
    "harbor_sha256",
    "evidence_contract_sha256",
    "model",
    "provider_route_policy",
    "topology",
    "role_model",
    "effort",
    "reasoning",
    "harbor_concurrency",
    "per_trial_limit_usd",
    "task_split",
    "task_partition_sha256",
    "attempts_per_task",
    "retry_max_retries",
    "job_name",
    "declared_at",
}
_CANDIDATE_IDENTITY_FIELDS = (
    "source_commit",
    "binary_sha256",
    "source_tree_sha256",
    "config_sha256",
    "adapter_sha256",
    "analyzer_sha256",
    "harbor_sha256",
    "evidence_contract_sha256",
    "model",
    "provider_route_policy",
    "topology",
    "role_model",
    "effort",
    "reasoning",
    "harbor_concurrency",
    "per_trial_limit_usd",
    "task_split",
    "attempts_per_task",
    "retry_max_retries",
)
_PROMOTION_IDENTITY_FIELDS = (
    "source_commit",
    "binary_sha256",
    "source_tree_sha256",
    "config_sha256",
    "adapter_sha256",
    "analyzer_sha256",
    "harbor_sha256",
    "evidence_contract_sha256",
    "model",
    "provider_route_policy",
    "topology",
    "role_model",
    "effort",
    "reasoning",
    "per_trial_limit_usd",
    "task_partition_sha256",
)
_PREREGISTRATION_FIELDS = {
    "sequence",
    "kind",
    "subject_commit",
    "candidate_ids",
    "study_manifest_sha256",
    "declared_at",
}
_AMENDMENT_FIELDS = {
    "sequence",
    "kind",
    "stage",
    "candidate_id",
    "invalid_job_name",
    "replacement_job_name",
    "artifact_tree_sha256",
    "reason",
    "candidate_sha256",
    "config_sha256",
    "subject_commit",
    "declared_at",
}
_PUBLICATION_FIELDS = {
    "sequence",
    "subject_type",
    "subject_id",
    "ledger_preimage_sha256",
    "ledger_commit",
    "public_url",
    "published_at",
}
_INTENT_FIELDS = {
    "sequence",
    "intent_sha256",
    "stage",
    "candidate_id",
    "candidate_sha256",
    "config_sha256",
    "model",
    "provider_route_policy",
    "topology",
    "role_model",
    "effort",
    "reasoning",
    "job_name",
    "task_split",
    "requested_trials",
    "attempts_per_task",
    "retry_max_retries",
    "harbor_concurrency",
    "per_trial_limit_usd",
    "maximum_spend_cents",
    "provider_authorization_id",
    "infrastructure_authorization_id",
    "provider_key_name",
    "provider_usage_before_usd",
    "provider_snapshot_at",
    "declared_at",
}
_OUTCOME_FIELDS = {
    "sequence",
    "intent_sha256",
    "candidate_id",
    "candidate_sha256",
    "config_sha256",
    "job_name",
    "status",
    "promotion_eligible",
    "attempted_trials",
    "artifact_tree_sha256",
    "provider_usage_before_usd",
    "provider_usage_after_usd",
    "provider_usage_delta_usd",
    "expected_paid_call_ids",
    "call_envelopes",
    "missing_paid_call_ids",
    "telemetry_input_tokens",
    "telemetry_output_tokens",
    "telemetry_cached_input_tokens",
    "telemetry_normalized_tokens",
    "telemetry_cost_sum_usd",
    "reconciliation_tolerance_usd",
    "completed_at",
    "recorded_at",
}
_CALL_ENVELOPE_FIELDS = {
    "call_id",
    "terminal_state",
    "input_tokens",
    "output_tokens",
    "cached_input_tokens",
    "cost_usd",
}
_ROUTE_POLICY = {
    "provider": "openrouter",
    "model": "z-ai/glm-5.1",
    "allow_fallbacks": False,
    "data_collection": "deny",
}
_DECIMAL_RE = re.compile(r"(?:0|[1-9][0-9]*)(?:\.[0-9]{1,6})?")
_MAX_METERED_USD = Decimal("1000000")


def canonical_file_bytes(value: object) -> bytes:
    """Encode a JSON file using the study's byte-canonical representation."""
    return (
        json.dumps(
            value,
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
            allow_nan=False,
        )
        + "\n"
    ).encode()


def canonical_body_bytes(value: object) -> bytes:
    """Encode a JSON value canonically without the file-ending newline."""
    return json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
        allow_nan=False,
    ).encode()


def sampler_preimage(
    domain: str, seed: int, replicate: int, draw: int, retry: int
) -> bytes:
    """Return the exact NUL-delimited SHA-256 counter-sampler preimage."""
    if domain not in {SCREEN_DOMAIN, CONFIRMATORY_DOMAIN}:
        raise ValueError("sampler domain is not registered")
    integers = (seed, replicate, draw, retry)
    if any(
        not isinstance(value, int) or isinstance(value, bool) or value < 0
        for value in integers
    ):
        raise ValueError("sampler counters must be nonnegative integers")
    return b"\x00".join(
        [domain.encode("ascii"), *(str(value).encode("ascii") for value in integers)]
    )


def sha256_counter_index(
    domain: str, seed: int, replicate: int, draw: int, n: int
) -> int:
    """Select one unbiased task index from the frozen SHA-256 counter stream."""
    if not isinstance(n, int) or isinstance(n, bool) or not 0 < n <= 65_535:
        raise ValueError("sampler task count is invalid")
    retry = 0
    limit = (1 << 64) // n * n
    while True:
        raw = sampler_preimage(domain, seed, replicate, draw, retry)
        value = int.from_bytes(hashlib.sha256(raw).digest()[:8], "big")
        if value < limit:
            return value % n
        retry += 1


@cache
def bootstrap_index_stream_sha256(domain: str, n: int, replicate_count: int) -> str:
    """Digest the frozen replicate-major, draw-minor u16 index stream."""
    if not isinstance(n, int) or isinstance(n, bool) or not 0 < n <= 65_535:
        raise ValueError("sampler task count is invalid")
    if (
        not isinstance(replicate_count, int)
        or isinstance(replicate_count, bool)
        or not 0 < replicate_count < 1 << 64
    ):
        raise ValueError("sampler replicate count is invalid")
    sampler_preimage(domain, BOOTSTRAP_SEED, 0, 0, 0)
    digest = hashlib.sha256()
    digest.update(BOOTSTRAP_INDEX_STREAM_DOMAIN.encode("ascii"))
    digest.update(b"\x00")
    digest.update(domain.encode("ascii"))
    digest.update(b"\x00")
    digest.update(BOOTSTRAP_SEED.to_bytes(8, "big"))
    digest.update(n.to_bytes(4, "big"))
    digest.update(replicate_count.to_bytes(8, "big"))
    for replicate in range(replicate_count):
        for draw in range(n):
            index = sha256_counter_index(domain, BOOTSTRAP_SEED, replicate, draw, n)
            digest.update(index.to_bytes(2, "big"))
    return digest.hexdigest()


def _reject_duplicate_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    value: dict[str, object] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON key {key!r}")
        value[key] = item
    return value


def _reject_nonfinite(value: str) -> object:
    raise ValueError(f"non-finite JSON number {value!r}")


def parse_canonical_object(raw: bytes, *, label: str) -> dict[str, object]:
    """Parse one canonical JSON object, rejecting alternate byte encodings."""
    try:
        value = json.loads(
            raw,
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_nonfinite,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as exc:
        raise ValueError(f"{label} is not strict JSON: {exc}") from exc
    if not isinstance(value, dict):
        raise ValueError(f"{label} must contain a JSON object")
    try:
        expected = canonical_file_bytes(value)
    except UnicodeEncodeError as exc:
        raise ValueError(
            f"{label} contains invalid Unicode for canonical JSON"
        ) from exc
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{label} cannot be represented canonically: {exc}") from exc
    if raw != expected:
        raise ValueError(f"{label} bytes are not canonical JSON")
    return value


def build_task_partition(
    identities: Sequence[TaskIdentity],
) -> dict[str, object]:
    """Build the approved deterministic development/screen/untouched split."""
    by_name = {name: (name, ref, checksum) for name, ref, checksum in identities}
    missing = set(DEVELOPMENT_TASK_NAMES) - set(by_name)
    if missing:
        raise ValueError(f"task identities omit development tasks: {sorted(missing)!r}")
    if len(by_name) != len(identities):
        raise ValueError("task identities contain duplicate task names")

    development = [by_name[name] for name in DEVELOPMENT_TASK_NAMES]
    remaining = [item for item in identities if item[0] not in DEVELOPMENT_TASK_NAMES]
    remaining.sort(
        key=lambda item: (
            hashlib.sha256((STUDY_ID + "\0" + item[1]).encode()).digest(),
            item[1],
        )
    )

    def record(item: TaskIdentity) -> dict[str, str]:
        return {
            "task_name": item[0],
            "canonical_task_reference": item[1],
            "task_checksum": item[2],
        }

    splits = {
        "development": [record(item) for item in development],
        "screen": [record(item) for item in remaining[:20]],
        "untouched": [record(item) for item in remaining[20:]],
    }
    return {
        "schema_version": TASK_PARTITION_SCHEMA,
        "study_id": STUDY_ID,
        **splits,
        "split_sha256": {
            name: hashlib.sha256(canonical_body_bytes(records)).hexdigest()
            for name, records in splits.items()
        },
    }


def _validate_fields(
    value: dict[str, object], expected: set[str], *, label: str
) -> None:
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        raise ValueError(f"{label} fields differ: missing={missing!r}, extra={extra!r}")


def validate_task_partition(value: object) -> dict[str, object]:
    """Validate a task partition against its schema, digests, and frozen seed."""
    if not isinstance(value, dict):
        raise ValueError("task partition must be an object")
    _validate_fields(value, _PARTITION_FIELDS, label="task partition")
    if value["schema_version"] != TASK_PARTITION_SCHEMA:
        raise ValueError("task partition schema_version is not approved")
    if value["study_id"] != STUDY_ID:
        raise ValueError("task partition study_id is not approved")

    names: set[str] = set()
    references: set[str] = set()
    for split_name in _SPLIT_NAMES:
        records = value[split_name]
        if not isinstance(records, list):
            raise ValueError(f"task partition {split_name} must be an array")
        for index, record in enumerate(records):
            label = f"task partition {split_name}[{index}]"
            if not isinstance(record, dict):
                raise ValueError(f"{label} must be an object")
            _validate_fields(record, _RECORD_FIELDS, label=label)
            task_name = record["task_name"]
            reference = record["canonical_task_reference"]
            checksum = record["task_checksum"]
            if not isinstance(task_name, str) or not task_name:
                raise ValueError(f"{label} has an invalid task name")
            if task_name in names:
                raise ValueError(
                    f"task partition has duplicate task name {task_name!r}"
                )
            names.add(task_name)
            if not isinstance(reference, str) or not _TASK_REFERENCE_RE.fullmatch(
                reference
            ):
                raise ValueError(f"{label} has an invalid task reference")
            if reference in references:
                raise ValueError(
                    f"task partition has duplicate task reference {reference!r}"
                )
            references.add(reference)
            if not isinstance(checksum, str) or not _SHA256_RE.fullmatch(checksum):
                raise ValueError(f"{label} has an invalid task checksum")

    digests = value["split_sha256"]
    if not isinstance(digests, dict):
        raise ValueError("task partition split_sha256 must be an object")
    _validate_fields(digests, set(_SPLIT_NAMES), label="task partition split_sha256")
    for split_name in _SPLIT_NAMES:
        digest = digests[split_name]
        expected_digest = hashlib.sha256(
            canonical_body_bytes(value[split_name])
        ).hexdigest()
        if digest != expected_digest:
            raise ValueError(f"task partition {split_name} split digest is incorrect")

    expected = build_task_partition(TASK_IDENTITIES)
    if any(value[split_name] != expected[split_name] for split_name in _SPLIT_NAMES):
        raise ValueError("task partition splits do not equal the frozen seed")
    return value


def _require_object(value: object, *, label: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ValueError(f"{label} must be an object")
    return value


def _require_array(value: object, *, label: str) -> list[object]:
    if not isinstance(value, list):
        raise ValueError(f"{label} must be an array")
    return value


def _require_string(value: object, *, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError(f"{label} must be a nonempty string")
    return value


def _require_sha256(value: object, *, label: str) -> str:
    if not isinstance(value, str) or _SHA256_RE.fullmatch(value) is None:
        raise ValueError(f"{label} must be a lowercase SHA-256 digest")
    return value


def _require_commit(value: object, *, label: str) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{40}", value) is None:
        raise ValueError(f"{label} must be a lowercase 40-hex commit")
    return value


def _require_int(
    value: object, *, label: str, minimum: int = 0, maximum: int | None = None
) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{label} must be an integer")
    if value < minimum or (maximum is not None and value > maximum):
        raise ValueError(f"{label} integer is outside the approved bounds")
    return value


def _require_timestamp(value: object, *, label: str) -> datetime:
    text = _require_string(value, label=label)
    try:
        parsed = datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError as exc:
        raise ValueError(
            f"{label} must be a timezone-aware ISO-8601 timestamp"
        ) from exc
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise ValueError(f"{label} must be a timezone-aware ISO-8601 timestamp")
    return parsed


def _require_decimal(value: object, *, label: str) -> Decimal:
    if not isinstance(value, str) or _DECIMAL_RE.fullmatch(value) is None:
        raise ValueError(f"{label} must be a bounded nonnegative decimal string")
    try:
        parsed = Decimal(value)
    except InvalidOperation as exc:
        raise ValueError(
            f"{label} must be a bounded nonnegative decimal string"
        ) from exc
    if not parsed.is_finite() or parsed < 0 or parsed > _MAX_METERED_USD:
        raise ValueError(f"{label} must be a bounded nonnegative decimal string")
    return parsed


def _record_sequence(record: dict[str, object], *, label: str) -> int:
    return _require_int(record.get("sequence"), label=f"{label} sequence", minimum=1)


def _canonical_sha256(value: object) -> str:
    return hashlib.sha256(canonical_body_bytes(value)).hexdigest()


def stage_shape(stage: str) -> dict[str, int | None]:
    """Return a detached copy of one exact paid-stage shape."""
    try:
        return dict(STAGE_SHAPES[stage])
    except (KeyError, TypeError) as exc:
        raise ValueError(f"unknown paid stage {stage!r}") from exc


def _initial_budget_authorizations(
    *, authorization_commit: str, declared_at: str
) -> list[dict[str, object]]:
    _require_commit(authorization_commit, label="authorization commit")
    _require_timestamp(declared_at, label="authorization declaration")
    common = {
        "scope": "tuning_and_screen",
        "authorization_commit": authorization_commit,
        "declared_at": declared_at,
    }
    return [
        {
            "sequence": 1,
            "authorization_id": "tuning_provider_v1",
            **common,
            "provider_key_name": "stella-tb21-tuning-key-v1",
            "hard_limit_cents": 10_000,
            "provider_cap_cents": 10_000,
            "infrastructure_cap_cents": 0,
            "reserve_cents": 1_500,
        },
        {
            "sequence": 2,
            "authorization_id": "tuning_infrastructure_v1",
            **common,
            "provider_key_name": None,
            "hard_limit_cents": 5_500,
            "provider_cap_cents": 0,
            "infrastructure_cap_cents": 5_500,
            "reserve_cents": 0,
        },
    ]


def build_initial_ledger(
    partition_sha256: str, *, authorization_commit: str, declared_at: str
) -> dict[str, object]:
    """Build the immutable v3 ledger prefix with only tuning authorization."""
    _require_sha256(partition_sha256, label="task partition digest")
    ledger: dict[str, object] = {
        "schema_version": RUN_LEDGER_SCHEMA,
        "study_id": STUDY_ID,
        "paths": dict(FIXED_PATHS),
        "task_partition_sha256": partition_sha256,
        "budget_authorizations": _initial_budget_authorizations(
            authorization_commit=authorization_commit, declared_at=declared_at
        ),
        "prior_exploration_disclosure": deepcopy(PRIOR_EXPLORATION_DISCLOSURE),
        "preregistrations": [],
        "candidates": [],
        "intents": [],
        "publications": [],
        "outcomes": [],
    }
    return validate_run_ledger(ledger)


def _validate_budget_authorizations(
    records: list[object],
) -> dict[str, dict[str, object]]:
    if len(records) < 2:
        raise ValueError("run ledger must preserve both initial budget authorizations")
    validated: list[dict[str, object]] = []
    by_id: dict[str, dict[str, object]] = {}
    key_names: set[str] = set()
    for index, item in enumerate(records):
        label = f"budget authorization[{index}]"
        record = _require_object(item, label=label)
        _validate_fields(record, _BUDGET_FIELDS, label=label)
        _record_sequence(record, label=label)
        authorization_id = _require_string(
            record["authorization_id"], label=f"{label} authorization_id"
        )
        if authorization_id in by_id:
            raise ValueError(f"duplicate budget authorization {authorization_id!r}")
        scope = _require_string(record["scope"], label=f"{label} scope")
        if scope not in {"tuning_and_screen", "confirmatory"}:
            raise ValueError(f"{label} has an unapproved scope")
        provider_key_name = record["provider_key_name"]
        if provider_key_name is not None:
            provider_key_name = _require_string(
                provider_key_name, label=f"{label} provider_key_name"
            )
            if provider_key_name in key_names:
                raise ValueError(f"provider key name {provider_key_name!r} is reused")
            key_names.add(provider_key_name)
        hard_limit = _require_int(
            record["hard_limit_cents"], label=f"{label} hard_limit_cents", minimum=1
        )
        provider_cap = _require_int(
            record["provider_cap_cents"],
            label=f"{label} provider_cap_cents",
            minimum=0,
        )
        infrastructure_cap = _require_int(
            record["infrastructure_cap_cents"],
            label=f"{label} infrastructure_cap_cents",
            minimum=0,
        )
        reserve = _require_int(
            record["reserve_cents"], label=f"{label} reserve_cents", minimum=0
        )
        if hard_limit != provider_cap + infrastructure_cap:
            raise ValueError(f"{label} hard limit must equal its finite component caps")
        if reserve > hard_limit:
            raise ValueError(f"{label} reserve exceeds its hard limit")
        _require_commit(record["authorization_commit"], label=f"{label} commit")
        _require_timestamp(record["declared_at"], label=f"{label} declared_at")
        if scope == "confirmatory" and (
            index < 2
            or provider_key_name is None
            or provider_cap <= 0
            or infrastructure_cap <= 0
            or reserve != 0
            or provider_key_name == "stella-tb21-tuning-key-v1"
        ):
            raise ValueError(
                "confirmatory authorization must be later, explicit, finite, "
                "and use its own provider key"
            )
        validated.append(record)
        by_id[authorization_id] = record

    first = validated[0]
    second = validated[1]
    if (
        first["sequence"],
        first["authorization_id"],
        second["sequence"],
        second["authorization_id"],
    ) != (1, "tuning_provider_v1", 2, "tuning_infrastructure_v1"):
        raise ValueError(
            "initial budget authorization order must pin provider at sequence 1 "
            "and infrastructure at sequence 2"
        )
    common_matches = (
        first["scope"] == second["scope"] == "tuning_and_screen"
        and first["authorization_commit"] == second["authorization_commit"]
        and first["declared_at"] == second["declared_at"]
    )
    if (
        not common_matches
        or {
            key: first[key]
            for key in (
                "authorization_id",
                "provider_key_name",
                "hard_limit_cents",
                "provider_cap_cents",
                "infrastructure_cap_cents",
                "reserve_cents",
            )
        }
        != {
            "authorization_id": "tuning_provider_v1",
            "provider_key_name": "stella-tb21-tuning-key-v1",
            "hard_limit_cents": 10_000,
            "provider_cap_cents": 10_000,
            "infrastructure_cap_cents": 0,
            "reserve_cents": 1_500,
        }
        or {
            key: second[key]
            for key in (
                "authorization_id",
                "provider_key_name",
                "hard_limit_cents",
                "provider_cap_cents",
                "infrastructure_cap_cents",
                "reserve_cents",
            )
        }
        != {
            "authorization_id": "tuning_infrastructure_v1",
            "provider_key_name": None,
            "hard_limit_cents": 5_500,
            "provider_cap_cents": 0,
            "infrastructure_cap_cents": 5_500,
            "reserve_cents": 0,
        }
    ):
        raise ValueError(
            "initial budget authorizations differ from the exact approved caps"
        )
    return by_id


def _candidate_identity(record: dict[str, object]) -> dict[str, object]:
    return {field: record[field] for field in _CANDIDATE_IDENTITY_FIELDS}


def _promotion_identity(record: dict[str, object]) -> dict[str, object]:
    return {field: record[field] for field in _PROMOTION_IDENTITY_FIELDS}


def _validate_candidates(
    records: list[object],
    *,
    partition_sha256: str,
    authorizations: dict[str, dict[str, object]],
) -> list[dict[str, object]]:
    validated: list[dict[str, object]] = []
    ids_by_stage: dict[str, set[str]] = {stage: set() for stage in STAGE_SHAPES}
    jobs: set[str] = set()
    for index, item in enumerate(records):
        label = f"candidate[{index}]"
        record = _require_object(item, label=label)
        _validate_fields(record, _CANDIDATE_FIELDS, label=label)
        _record_sequence(record, label=label)
        candidate_id = _require_string(
            record["candidate_id"], label=f"{label} candidate_id"
        )
        stage = _require_string(record["stage"], label=f"{label} stage")
        shape = stage_shape(stage)
        if stage == "confirmatory" and not any(
            authorization["scope"] == "confirmatory"
            and authorization["sequence"] < record["sequence"]
            for authorization in authorizations.values()
        ):
            raise ValueError(
                f"{label} authorization must precede confirmatory record; "
                "a new explicit confirmatory authorization is required"
            )
        if candidate_id in ids_by_stage[stage]:
            raise ValueError(f"duplicate candidate {candidate_id!r} in {stage}")
        ids_by_stage[stage].add(candidate_id)
        if len(ids_by_stage[stage]) > shape["max_candidates"]:
            raise ValueError(f"{stage} candidate cap is exceeded")

        for field in (
            "binary_sha256",
            "source_tree_sha256",
            "config_sha256",
            "adapter_sha256",
            "analyzer_sha256",
            "harbor_sha256",
            "evidence_contract_sha256",
        ):
            _require_sha256(record[field], label=f"{label} {field}")
        _require_commit(record["source_commit"], label=f"{label} source_commit")
        candidate_sha = _require_sha256(
            record["candidate_sha256"], label=f"{label} candidate_sha256"
        )
        if candidate_sha != _canonical_sha256(_candidate_identity(record)):
            raise ValueError(f"{label} candidate identity digest is incorrect")
        record_sha = _require_sha256(
            record["record_sha256"], label=f"{label} record_sha256"
        )
        record_body = {
            key: value
            for key, value in record.items()
            if key not in {"sequence", "record_sha256"}
        }
        if record_sha != _canonical_sha256(record_body):
            raise ValueError(f"{label} candidate record digest is incorrect")
        if record["model"] != "openrouter/z-ai/glm-5.1":
            raise ValueError(f"{label} model is not the exact GLM-5.1 route")
        if record["provider_route_policy"] != _ROUTE_POLICY:
            raise ValueError(f"{label} provider route policy differs")
        if record["topology"] not in {"direct", "pipeline", "fleet"}:
            raise ValueError(f"{label} topology is not approved")
        if (
            record["role_model"] != "openrouter/z-ai/glm-5.1"
            or record["effort"] != "max"
            or record["reasoning"] is not True
        ):
            raise ValueError(f"{label} role/effort/reasoning posture differs")
        if (
            _require_int(
                record["harbor_concurrency"],
                label=f"{label} harbor_concurrency",
                minimum=1,
            )
            != shape["harbor_concurrency"]
        ):
            raise ValueError(f"{label} Harbor concurrency differs from its stage")
        per_trial = _require_decimal(
            record["per_trial_limit_usd"], label=f"{label} per_trial_limit_usd"
        )
        if per_trial <= 0 or per_trial > Decimal("0.30"):
            raise ValueError(f"{label} per-trial limit exceeds $0.30")
        expected_split = "development" if stage.startswith("development_") else stage
        if record["task_split"] != expected_split:
            raise ValueError(f"{label} task split differs from its stage")
        if record["task_partition_sha256"] != partition_sha256:
            raise ValueError(f"{label} task partition digest differs")
        if (
            _require_int(
                record["attempts_per_task"],
                label=f"{label} attempts_per_task",
                minimum=1,
            )
            != shape["attempts"]
        ):
            raise ValueError(f"{label} attempts differ from its stage")
        if (
            _require_int(
                record["retry_max_retries"], label=f"{label} retry_max_retries"
            )
            != 0
        ):
            raise ValueError(f"{label} must freeze zero Harbor retries")
        job_name = _require_string(record["job_name"], label=f"{label} job_name")
        if job_name in jobs:
            raise ValueError(f"duplicate candidate job name {job_name!r}")
        jobs.add(job_name)
        _require_timestamp(record["declared_at"], label=f"{label} declared_at")
        validated.append(record)
    return validated


def _preregistration_subject(record: dict[str, object]) -> str:
    if record["kind"] == "development_amendment":
        return f"development_amendment:{record['invalid_job_name']}"
    return str(record["kind"])


def _validate_preregistrations(
    records: list[object],
    *,
    candidates: list[dict[str, object]],
    outcomes: list[dict[str, object]],
    authorizations: dict[str, dict[str, object]],
) -> list[dict[str, object]]:
    validated: list[dict[str, object]] = []
    normal_kinds: set[str] = set()
    amendment_jobs: set[str] = set()
    for index, item in enumerate(records):
        label = f"preregistration[{index}]"
        record = _require_object(item, label=label)
        kind = _require_string(record.get("kind"), label=f"{label} kind")
        if kind == "development_amendment":
            _validate_fields(record, _AMENDMENT_FIELDS, label=label)
            _record_sequence(record, label=label)
            stage = _require_string(record["stage"], label=f"{label} stage")
            if not stage.startswith("development_"):
                raise ValueError("development amendment must name a development stage")
            candidate_id = _require_string(
                record["candidate_id"], label=f"{label} candidate_id"
            )
            invalid_job = _require_string(
                record["invalid_job_name"], label=f"{label} invalid_job_name"
            )
            replacement_job = _require_string(
                record["replacement_job_name"],
                label=f"{label} replacement_job_name",
            )
            if invalid_job == replacement_job:
                raise ValueError("development amendment must name a fresh job")
            if invalid_job in amendment_jobs:
                raise ValueError(f"duplicate development amendment for {invalid_job!r}")
            amendment_jobs.add(invalid_job)
            _require_sha256(
                record["artifact_tree_sha256"], label=f"{label} artifact_tree_sha256"
            )
            _require_string(record["reason"], label=f"{label} reason")
            _require_sha256(
                record["candidate_sha256"], label=f"{label} candidate_sha256"
            )
            _require_sha256(record["config_sha256"], label=f"{label} config_sha256")
            matching = [
                outcome
                for outcome in outcomes
                if outcome["job_name"] == invalid_job
                and outcome["candidate_id"] == candidate_id
                and outcome["status"] == "ineligible"
                and outcome["sequence"] < record["sequence"]
            ]
            if len(matching) != 1:
                raise ValueError(
                    "development amendment must name one completed ineligible outcome"
                )
            outcome = matching[0]
            if (
                outcome["artifact_tree_sha256"] != record["artifact_tree_sha256"]
                or outcome["candidate_sha256"] != record["candidate_sha256"]
                or outcome["config_sha256"] != record["config_sha256"]
            ):
                raise ValueError(
                    "development amendment does not preserve exact artifacts"
                )
            candidate = next(
                (
                    candidate
                    for candidate in candidates
                    if candidate["stage"] == stage
                    and candidate["candidate_id"] == candidate_id
                ),
                None,
            )
            if candidate is None or (
                candidate["candidate_sha256"] != record["candidate_sha256"]
                or candidate["config_sha256"] != record["config_sha256"]
            ):
                raise ValueError("development amendment changes the frozen candidate")
        else:
            _validate_fields(record, _PREREGISTRATION_FIELDS, label=label)
            _record_sequence(record, label=label)
            stage_shape(kind)
            if kind == "confirmatory" and not any(
                authorization["scope"] == "confirmatory"
                and authorization["sequence"] < record["sequence"]
                for authorization in authorizations.values()
            ):
                raise ValueError(
                    f"{label} authorization must precede confirmatory preregistration"
                )
            if kind in normal_kinds:
                raise ValueError(f"duplicate preregistration for {kind!r}")
            normal_kinds.add(kind)
            candidate_ids_raw = _require_array(
                record["candidate_ids"], label=f"{label} candidate_ids"
            )
            candidate_ids = [
                _require_string(value, label=f"{label} candidate_ids")
                for value in candidate_ids_raw
            ]
            if not candidate_ids or len(candidate_ids) != len(set(candidate_ids)):
                raise ValueError(f"{label} candidate_ids must be unique and nonempty")
            available = {
                candidate["candidate_id"]
                for candidate in candidates
                if candidate["stage"] == kind
                and candidate["sequence"] < record["sequence"]
            }
            if set(candidate_ids) != available:
                raise ValueError(f"{label} does not freeze every exact stage candidate")
            manifest_digest = record["study_manifest_sha256"]
            if kind in {"screen", "confirmatory"}:
                _require_sha256(manifest_digest, label=f"{label} study manifest")
            elif manifest_digest is not None:
                raise ValueError(f"{label} must not bind a phase manifest")
        _require_commit(record["subject_commit"], label=f"{label} subject_commit")
        _require_timestamp(record["declared_at"], label=f"{label} declared_at")
        validated.append(record)
    return validated


def _ledger_prefix(ledger: dict[str, object], sequence: int) -> dict[str, object]:
    prefix = deepcopy(ledger)
    for field in _LEDGER_ARRAY_FIELDS:
        prefix[field] = [
            record for record in prefix[field] if record["sequence"] < sequence
        ]
    return prefix


def _validate_publications(
    records: list[object],
    *,
    ledger: dict[str, object],
    preregistrations: list[dict[str, object]],
    intents: list[dict[str, object]],
) -> list[dict[str, object]]:
    validated: list[dict[str, object]] = []
    subjects: set[tuple[str, str]] = set()
    prereg_by_subject = {
        _preregistration_subject(record): record for record in preregistrations
    }
    intent_by_subject = {str(record["intent_sha256"]): record for record in intents}
    for index, item in enumerate(records):
        label = f"publication[{index}]"
        record = _require_object(item, label=label)
        _validate_fields(record, _PUBLICATION_FIELDS, label=label)
        sequence = _record_sequence(record, label=label)
        subject_type = _require_string(
            record["subject_type"], label=f"{label} subject_type"
        )
        subject_id = _require_string(record["subject_id"], label=f"{label} subject_id")
        subject = (subject_type, subject_id)
        if subject in subjects:
            raise ValueError(f"duplicate publication subject {subject!r}")
        subjects.add(subject)
        if subject_type == "preregistration":
            target = prereg_by_subject.get(subject_id)
        elif subject_type == "intent":
            target = intent_by_subject.get(subject_id)
        else:
            raise ValueError(f"{label} has an unapproved subject type")
        if target is None or target["sequence"] >= sequence:
            raise ValueError(f"{label} subject does not exist before publication")
        preimage = _require_sha256(
            record["ledger_preimage_sha256"], label=f"{label} ledger preimage"
        )
        expected_preimage = _canonical_sha256(_ledger_prefix(ledger, sequence))
        if preimage != expected_preimage:
            raise ValueError(
                f"{label} ledger preimage differs; publication-only delta drift"
            )
        commit = _require_commit(
            record["ledger_commit"], label=f"{label} ledger_commit"
        )
        if (
            record["public_url"]
            != f"https://github.com/macanderson/stella/commit/{commit}"
        ):
            raise ValueError(f"{label} public URL is not the immutable ledger commit")
        _require_timestamp(record["published_at"], label=f"{label} published_at")
        validated.append(record)
    return validated


def _is_published(
    publications: list[dict[str, object]],
    *,
    subject_type: str,
    subject_id: str,
    before_sequence: int,
) -> bool:
    return any(
        publication["subject_type"] == subject_type
        and publication["subject_id"] == subject_id
        and publication["sequence"] < before_sequence
        for publication in publications
    )


def _validate_intent_records(
    records: list[object],
    *,
    candidates: list[dict[str, object]],
    authorizations: dict[str, dict[str, object]],
) -> list[dict[str, object]]:
    validated: list[dict[str, object]] = []
    intent_ids: set[str] = set()
    job_names: set[str] = set()
    for index, item in enumerate(records):
        label = f"intent[{index}]"
        record = _require_object(item, label=label)
        _validate_fields(record, _INTENT_FIELDS, label=label)
        _record_sequence(record, label=label)
        intent_sha = _require_sha256(
            record["intent_sha256"], label=f"{label} intent_sha256"
        )
        if intent_sha in intent_ids:
            raise ValueError(f"duplicate intent digest {intent_sha!r}")
        intent_ids.add(intent_sha)
        stage = _require_string(record["stage"], label=f"{label} stage")
        shape = stage_shape(stage)
        candidate_id = _require_string(
            record["candidate_id"], label=f"{label} candidate_id"
        )
        candidate = next(
            (
                candidate
                for candidate in candidates
                if candidate["stage"] == stage
                and candidate["candidate_id"] == candidate_id
                and candidate["sequence"] < record["sequence"]
            ),
            None,
        )
        if candidate is None:
            raise ValueError(f"{label} does not name a frozen stage candidate")
        for field in ("candidate_sha256", "config_sha256"):
            _require_sha256(record[field], label=f"{label} {field}")
            if record[field] != candidate[field]:
                raise ValueError(f"{label} changes the frozen candidate {field}")
        for field in (
            "model",
            "provider_route_policy",
            "topology",
            "role_model",
            "effort",
            "reasoning",
            "harbor_concurrency",
            "per_trial_limit_usd",
            "task_split",
            "attempts_per_task",
            "retry_max_retries",
        ):
            if record[field] != candidate[field]:
                raise ValueError(f"{label} {field} differs from its frozen candidate")
        job_name = _require_string(record["job_name"], label=f"{label} job_name")
        if job_name in job_names:
            raise ValueError(f"duplicate intent job name {job_name!r}")
        job_names.add(job_name)
        expected_split = "development" if stage.startswith("development_") else stage
        if record["task_split"] != expected_split:
            raise ValueError(f"{label} task split differs from its stage")
        requested_trials = _require_int(
            record["requested_trials"], label=f"{label} requested_trials", minimum=1
        )
        if requested_trials != shape["tasks"] * shape["attempts"]:
            raise ValueError(f"{label} requested trials differ from its stage")
        for field, expected in (
            ("attempts_per_task", shape["attempts"]),
            ("retry_max_retries", 0),
            ("harbor_concurrency", shape["harbor_concurrency"]),
        ):
            if _require_int(record[field], label=f"{label} {field}") != expected:
                raise ValueError(f"{label} {field} differs from its stage")
        per_trial = _require_decimal(
            record["per_trial_limit_usd"], label=f"{label} per_trial_limit_usd"
        )
        cents_per_trial = per_trial * 100
        if (
            per_trial <= 0
            or per_trial > Decimal("0.30")
            or cents_per_trial != cents_per_trial.to_integral_value()
        ):
            raise ValueError(
                f"{label} per-trial spend must be exact cents at most $0.30"
            )
        maximum_spend = _require_int(
            record["maximum_spend_cents"],
            label=f"{label} maximum_spend_cents",
            minimum=1,
        )
        if maximum_spend != requested_trials * int(cents_per_trial):
            raise ValueError(
                f"{label} maximum spend differs from its exact trial budget"
            )
        provider_id = _require_string(
            record["provider_authorization_id"],
            label=f"{label} provider_authorization_id",
        )
        infrastructure_id = _require_string(
            record["infrastructure_authorization_id"],
            label=f"{label} infrastructure_authorization_id",
        )
        provider_auth = authorizations.get(provider_id)
        infrastructure_auth = authorizations.get(infrastructure_id)
        if provider_auth is None or infrastructure_auth is None:
            raise ValueError(f"{label} names an unknown budget authorization")
        if (
            provider_auth["sequence"] >= record["sequence"]
            or infrastructure_auth["sequence"] >= record["sequence"]
        ):
            raise ValueError(f"{label} authorization must precede its paid intent")
        provider_key_name = _require_string(
            record["provider_key_name"], label=f"{label} provider_key_name"
        )
        if provider_auth["provider_key_name"] != provider_key_name:
            raise ValueError(f"{label} provider key differs from its authorization")
        if stage == "confirmatory" and (
            provider_auth["scope"] != "confirmatory"
            or infrastructure_auth["scope"] != "confirmatory"
            or provider_auth["provider_cap_cents"] <= 0
            or infrastructure_auth["infrastructure_cap_cents"] <= 0
        ):
            raise ValueError(
                f"{label} requires a new explicit confirmatory authorization"
            )
        if maximum_spend > provider_auth["provider_cap_cents"]:
            raise ValueError(f"{label} spend exceeds the provider authorization")
        if infrastructure_auth["infrastructure_cap_cents"] <= 0:
            raise ValueError(f"{label} lacks an infrastructure authorization")
        snapshot = _require_timestamp(
            record["provider_snapshot_at"], label=f"{label} provider_snapshot_at"
        )
        declared = _require_timestamp(
            record["declared_at"], label=f"{label} declared_at"
        )
        if snapshot > declared:
            raise ValueError(f"{label} provider snapshot postdates its declaration")
        _require_decimal(
            record["provider_usage_before_usd"],
            label=f"{label} provider_usage_before_usd",
        )
        if stage != "confirmatory" and (
            provider_id != "tuning_provider_v1"
            or infrastructure_id != "tuning_infrastructure_v1"
            or provider_key_name != "stella-tb21-tuning-key-v1"
        ):
            raise ValueError(
                f"{label} uses the wrong tuning provider key authorization"
            )
        validated.append(record)
    return validated


def _validate_outcome_records(
    records: list[object], *, intents: list[dict[str, object]]
) -> list[dict[str, object]]:
    validated: list[dict[str, object]] = []
    intent_by_sha = {str(record["intent_sha256"]): record for record in intents}
    used_intents: set[str] = set()
    for index, item in enumerate(records):
        label = f"outcome[{index}]"
        record = _require_object(item, label=label)
        _validate_fields(record, _OUTCOME_FIELDS, label=label)
        sequence = _record_sequence(record, label=label)
        intent_sha = _require_sha256(
            record["intent_sha256"], label=f"{label} intent_sha256"
        )
        intent = intent_by_sha.get(intent_sha)
        if intent is None or intent["sequence"] >= sequence:
            raise ValueError(f"{label} does not name an earlier intent")
        if intent_sha in used_intents:
            raise ValueError(f"duplicate outcome for intent {intent_sha!r}")
        used_intents.add(intent_sha)
        for field in ("candidate_id", "candidate_sha256", "config_sha256", "job_name"):
            if record[field] != intent[field]:
                raise ValueError(f"{label} {field} differs from its intent")
        status = _require_string(record["status"], label=f"{label} status")
        if status not in {"complete", "ineligible", "incomplete"}:
            raise ValueError(f"{label} has an unapproved status")
        promotion_eligible = record["promotion_eligible"]
        if not isinstance(promotion_eligible, bool):
            raise ValueError(f"{label} promotion_eligible must be a boolean")
        if promotion_eligible and status != "complete":
            raise ValueError(
                f"{label} promotion eligibility requires complete outcome status"
            )
        attempted = _require_int(
            record["attempted_trials"], label=f"{label} attempted_trials"
        )
        if attempted > intent["requested_trials"]:
            raise ValueError(f"{label} attempted more than the canonical trial count")
        if status == "complete" and attempted != intent["requested_trials"]:
            raise ValueError(f"{label} complete status lacks every canonical trial")
        _require_sha256(
            record["artifact_tree_sha256"], label=f"{label} artifact_tree_sha256"
        )
        expected_raw = _require_array(
            record["expected_paid_call_ids"],
            label=f"{label} expected_paid_call_ids",
        )
        expected_ids = [
            _require_string(value, label=f"{label} expected paid-call ID")
            for value in expected_raw
        ]
        if len(expected_ids) != len(set(expected_ids)):
            raise ValueError(f"{label} has a duplicate paid-call ID in expectations")
        envelopes_raw = _require_array(
            record["call_envelopes"], label=f"{label} call_envelopes"
        )
        if attempted > 0 and not expected_ids:
            raise ValueError(f"{label} attempted trials require expected paid-call IDs")
        if attempted == 0 and (expected_ids or envelopes_raw):
            raise ValueError(
                f"{label} zero attempted trials cannot declare paid-call evidence"
            )
        observed_ids: list[str] = []
        input_total = 0
        output_total = 0
        cached_total = 0
        cost_total = Decimal(0)
        envelopes_complete = True
        for envelope_index, envelope_item in enumerate(envelopes_raw):
            envelope_label = f"{label} call_envelopes[{envelope_index}]"
            envelope = _require_object(envelope_item, label=envelope_label)
            _validate_fields(envelope, _CALL_ENVELOPE_FIELDS, label=envelope_label)
            call_id = _require_string(
                envelope["call_id"], label=f"{envelope_label} call_id"
            )
            if call_id in observed_ids:
                raise ValueError(f"{label} has a duplicate paid-call ID {call_id!r}")
            if call_id not in expected_ids:
                raise ValueError(f"{label} has an unknown paid-call ID {call_id!r}")
            observed_ids.append(call_id)
            terminal_state = _require_string(
                envelope["terminal_state"],
                label=f"{envelope_label} terminal_state",
            )
            if terminal_state not in {"successful", "failed", "aborted"}:
                raise ValueError(f"{envelope_label} has an unknown terminal state")
            components: dict[str, int] = {}
            for field in ("input_tokens", "output_tokens", "cached_input_tokens"):
                component = envelope[field]
                if component is None:
                    envelopes_complete = False
                    continue
                components[field] = _require_int(
                    component, label=f"{envelope_label} {field}"
                )
            cost_value = envelope["cost_usd"]
            if cost_value is None:
                envelopes_complete = False
            else:
                cost_total += _require_decimal(
                    cost_value, label=f"{envelope_label} cost_usd"
                )
            if len(components) == 3:
                if components["cached_input_tokens"] > components["input_tokens"]:
                    raise ValueError(
                        f"{envelope_label} cached input exceeds total input"
                    )
                input_total += components["input_tokens"]
                output_total += components["output_tokens"]
                cached_total += components["cached_input_tokens"]
        missing_ids = [
            call_id for call_id in expected_ids if call_id not in observed_ids
        ]
        supplied_missing_raw = _require_array(
            record["missing_paid_call_ids"],
            label=f"{label} missing_paid_call_ids",
        )
        supplied_missing = [
            _require_string(value, label=f"{label} missing paid-call ID")
            for value in supplied_missing_raw
        ]
        if supplied_missing != missing_ids:
            raise ValueError(
                f"{label} missing paid-call IDs differ from exact evidence"
            )
        accounting_complete = envelopes_complete and not missing_ids
        if status == "complete" and missing_ids:
            raise ValueError(f"{label} missing paid-call IDs cannot be complete")
        if status == "complete" and not accounting_complete:
            raise ValueError(
                f"{label} incomplete paid-call accounting cannot be complete"
            )
        aggregate_fields = (
            "telemetry_input_tokens",
            "telemetry_output_tokens",
            "telemetry_cached_input_tokens",
            "telemetry_normalized_tokens",
        )
        if envelopes_complete:
            expected_aggregates = (
                input_total,
                output_total,
                cached_total,
                input_total + output_total,
            )
            observed_aggregates = tuple(
                _require_int(record[field], label=f"{label} {field}")
                for field in aggregate_fields
            )
            if observed_aggregates != expected_aggregates:
                raise ValueError(f"{label} token aggregates differ from call envelopes")
            telemetry = _require_decimal(
                record["telemetry_cost_sum_usd"],
                label=f"{label} telemetry_cost_sum_usd",
            )
            if telemetry != cost_total:
                raise ValueError(f"{label} cost aggregate differs from call envelopes")
        else:
            if (
                any(record[field] is not None for field in aggregate_fields)
                or record["telemetry_cost_sum_usd"] is not None
            ):
                raise ValueError(
                    f"{label} incomplete accounting aggregates must remain null"
                )
            telemetry = None
        before = _require_decimal(
            record["provider_usage_before_usd"],
            label=f"{label} provider_usage_before_usd",
        )
        after = _require_decimal(
            record["provider_usage_after_usd"],
            label=f"{label} provider_usage_after_usd",
        )
        delta = _require_decimal(
            record["provider_usage_delta_usd"],
            label=f"{label} provider_usage_delta_usd",
        )
        tolerance = _require_decimal(
            record["reconciliation_tolerance_usd"],
            label=f"{label} reconciliation_tolerance_usd",
        )
        if before != _require_decimal(
            intent["provider_usage_before_usd"],
            label=f"{label} intent provider_usage_before_usd",
        ):
            raise ValueError(f"{label} provider usage preimage differs from its intent")
        if after - before != delta:
            raise ValueError(f"{label} provider usage delta does not reconcile exactly")
        if tolerance > Decimal("0.01") or (
            telemetry is not None and abs(delta - telemetry) > tolerance
        ):
            raise ValueError(f"{label} telemetry reconciliation exceeds $0.01")
        if delta * 100 > intent["maximum_spend_cents"]:
            raise ValueError(f"{label} provider spend exceeds the intent maximum")
        completed = _require_timestamp(
            record["completed_at"], label=f"{label} completed_at"
        )
        recorded = _require_timestamp(
            record["recorded_at"], label=f"{label} recorded_at"
        )
        if completed > recorded:
            raise ValueError(f"{label} was recorded before completion")
        validated.append(record)
    return validated


def _validate_promotions(
    candidates: list[dict[str, object]],
    *,
    intents: list[dict[str, object]],
    outcomes: list[dict[str, object]],
) -> None:
    outcome_by_intent = {str(outcome["intent_sha256"]): outcome for outcome in outcomes}
    previous_stage = {
        "development_round_2": "development_round_1",
        "development_round_3": "development_round_2",
        "screen": "development_round_3",
        "confirmatory": "screen",
    }
    for candidate in candidates:
        stage = str(candidate["stage"])
        prior_stage = previous_stage.get(stage)
        if prior_stage is None:
            continue
        prior_intents = [
            intent
            for intent in intents
            if intent["stage"] == prior_stage
            and intent["candidate_id"] == candidate["candidate_id"]
            and intent["sequence"] < candidate["sequence"]
        ]
        eligible: list[dict[str, object]] = []
        for intent in prior_intents:
            outcome = outcome_by_intent.get(str(intent["intent_sha256"]))
            prior_candidates = [
                prior_candidate
                for prior_candidate in candidates
                if prior_candidate["stage"] == prior_stage
                and prior_candidate["candidate_id"] == candidate["candidate_id"]
                and prior_candidate["candidate_sha256"] == intent["candidate_sha256"]
                and prior_candidate["sequence"] < intent["sequence"]
            ]
            if (
                outcome is not None
                and outcome["status"] == "complete"
                and outcome["promotion_eligible"] is True
                and outcome["sequence"] < candidate["sequence"]
                and len(prior_candidates) == 1
                and _promotion_identity(prior_candidates[0])
                == _promotion_identity(candidate)
            ):
                eligible.append(intent)
        if len(eligible) != 1:
            if stage in {"screen", "confirmatory"}:
                raise ValueError(
                    f"{stage} requires exactly one promotion-eligible {prior_stage} "
                    "outcome with the frozen candidate identity"
                )
            raise ValueError(
                f"{stage} may promote only one complete candidate from {prior_stage}"
            )


def _validate_intent_lifecycle(
    intents: list[dict[str, object]],
    *,
    candidates: list[dict[str, object]],
    preregistrations: list[dict[str, object]],
    publications: list[dict[str, object]],
    outcomes: list[dict[str, object]],
) -> None:
    outcomes_by_intent = {
        str(outcome["intent_sha256"]): outcome for outcome in outcomes
    }
    for intent in intents:
        stage = str(intent["stage"])
        sequence = int(intent["sequence"])
        candidate_id = str(intent["candidate_id"])
        preregistration = next(
            (
                record
                for record in preregistrations
                if record["kind"] == stage
                and candidate_id in record["candidate_ids"]
                and record["sequence"] < sequence
            ),
            None,
        )
        if preregistration is None or not _is_published(
            publications,
            subject_type="preregistration",
            subject_id=stage,
            before_sequence=sequence,
        ):
            raise ValueError(f"{stage} intent lacks its published preregistration")
        candidate = next(
            candidate
            for candidate in candidates
            if candidate["stage"] == stage and candidate["candidate_id"] == candidate_id
        )
        earlier = [
            prior
            for prior in intents
            if prior["stage"] == stage
            and prior["candidate_id"] == candidate_id
            and prior["sequence"] < sequence
        ]
        if not earlier:
            if intent["job_name"] != candidate["job_name"]:
                raise ValueError(f"{stage} first intent changes its frozen job name")
            continue
        if not stage.startswith("development_") or len(earlier) != 1:
            raise ValueError(f"{stage} does not admit replacement intents")
        prior = earlier[0]
        outcome = outcomes_by_intent.get(str(prior["intent_sha256"]))
        amendment = next(
            (
                record
                for record in preregistrations
                if record["kind"] == "development_amendment"
                and record["stage"] == stage
                and record["candidate_id"] == candidate_id
                and record["invalid_job_name"] == prior["job_name"]
                and record["replacement_job_name"] == intent["job_name"]
                and record["sequence"] < sequence
            ),
            None,
        )
        amendment_subject = f"development_amendment:{prior['job_name']}"
        if (
            outcome is None
            or outcome["status"] != "ineligible"
            or amendment is None
            or not _is_published(
                publications,
                subject_type="preregistration",
                subject_id=amendment_subject,
                before_sequence=sequence,
            )
        ):
            raise ValueError(
                f"{stage} replacement requires a completed ineligible outcome "
                "and published development amendment"
            )
        if (
            amendment["candidate_sha256"] != intent["candidate_sha256"]
            or amendment["config_sha256"] != intent["config_sha256"]
            or amendment["artifact_tree_sha256"] != outcome["artifact_tree_sha256"]
        ):
            raise ValueError(f"{stage} replacement changes frozen amendment evidence")


def _validate_outcome_lifecycle(
    outcomes: list[dict[str, object]],
    *,
    publications: list[dict[str, object]],
) -> None:
    for outcome in outcomes:
        if not _is_published(
            publications,
            subject_type="intent",
            subject_id=str(outcome["intent_sha256"]),
            before_sequence=int(outcome["sequence"]),
        ):
            raise ValueError("outcome intent was not published before execution")


def _validate_stage_capacity(
    intents: list[dict[str, object]], outcomes: list[dict[str, object]]
) -> None:
    outcomes_by_intent = {
        str(outcome["intent_sha256"]): outcome for outcome in outcomes
    }
    tuning_provider_spend = Decimal(0)
    for stage, shape in STAGE_SHAPES.items():
        stage_intents = [intent for intent in intents if intent["stage"] == stage]
        if len(stage_intents) > shape["max_intents"]:
            raise ValueError(f"{stage} intent cap is exceeded")
        if (
            len({intent["candidate_id"] for intent in stage_intents})
            > shape["max_candidates"]
        ):
            raise ValueError(f"{stage} distinct entrant cap is exceeded")
        trials = 0
        spend_cents = Decimal(0)
        for intent in stage_intents:
            outcome = outcomes_by_intent.get(str(intent["intent_sha256"]))
            if outcome is None:
                trials += int(intent["requested_trials"])
                spend_cents += Decimal(int(intent["maximum_spend_cents"]))
            else:
                trials += int(outcome["attempted_trials"])
                spend_cents += (
                    _require_decimal(
                        outcome["provider_usage_delta_usd"],
                        label="stage provider usage delta",
                    )
                    * 100
                )
        if trials > shape["max_trials"]:
            raise ValueError(f"{stage} attempted-trial cap is exceeded")
        stage_cap = shape["max_spend_cents"]
        if stage_cap is not None and spend_cents > stage_cap:
            raise ValueError(f"{stage} spend cap is exceeded")
        if stage != "confirmatory":
            tuning_provider_spend += spend_cents
    if tuning_provider_spend > Decimal(8_500):
        raise ValueError("the 1,500-cent tuning reserve is not authorized for use")


def validate_run_ledger(value: object) -> dict[str, object]:
    """Validate the exact append-only v3 study ledger and lifecycle."""
    ledger = _require_object(value, label="run ledger")
    _validate_fields(ledger, set(RUN_LEDGER_FIELDS), label="run ledger")
    if ledger["schema_version"] != RUN_LEDGER_SCHEMA:
        raise ValueError("run ledger schema_version is not approved")
    if ledger["study_id"] != STUDY_ID:
        raise ValueError("run ledger study_id is not approved")
    if ledger["paths"] != FIXED_PATHS:
        raise ValueError("run ledger paths differ from the fixed evidence paths")
    partition_sha = _require_sha256(
        ledger["task_partition_sha256"], label="run ledger task partition digest"
    )
    if ledger["prior_exploration_disclosure"] != PRIOR_EXPLORATION_DISCLOSURE:
        raise ValueError("run ledger prior-exploration disclosure differs")
    arrays = {
        field: _require_array(ledger[field], label=f"run ledger {field}")
        for field in _LEDGER_ARRAY_FIELDS
    }
    for field, records in arrays.items():
        array_sequences = [
            _record_sequence(
                _require_object(record, label=f"run ledger {field}"), label=field
            )
            for record in records
        ]
        if array_sequences != sorted(array_sequences):
            raise ValueError(f"run ledger {field} must be strictly sequence-ordered")
    authorizations = _validate_budget_authorizations(arrays["budget_authorizations"])
    candidates = _validate_candidates(
        arrays["candidates"],
        partition_sha256=partition_sha,
        authorizations=authorizations,
    )
    intents = _validate_intent_records(
        arrays["intents"], candidates=candidates, authorizations=authorizations
    )
    outcomes = _validate_outcome_records(arrays["outcomes"], intents=intents)
    preregistrations = _validate_preregistrations(
        arrays["preregistrations"],
        candidates=candidates,
        outcomes=outcomes,
        authorizations=authorizations,
    )
    publications = _validate_publications(
        arrays["publications"],
        ledger=ledger,
        preregistrations=preregistrations,
        intents=intents,
    )

    sequences = sorted(
        _record_sequence(record, label=field)
        for field in _LEDGER_ARRAY_FIELDS
        for record in arrays[field]
    )
    if sequences != list(range(1, len(sequences) + 1)):
        raise ValueError("records must use each next global sequence exactly once")
    _validate_promotions(candidates, intents=intents, outcomes=outcomes)
    _validate_intent_lifecycle(
        intents,
        candidates=candidates,
        preregistrations=preregistrations,
        publications=publications,
        outcomes=outcomes,
    )
    _validate_outcome_lifecycle(outcomes, publications=publications)
    _validate_stage_capacity(intents, outcomes)
    return ledger


def next_sequence(ledger: object) -> int:
    """Return the only legal sequence for the next append."""
    validated = validate_run_ledger(ledger)
    return sum(len(validated[field]) for field in _LEDGER_ARRAY_FIELDS) + 1


def _append_record(ledger: object, record: object, *, field: str) -> dict[str, object]:
    validated = validate_run_ledger(ledger)
    item = deepcopy(_require_object(record, label=field.removesuffix("s")))
    expected = next_sequence(validated)
    if _record_sequence(item, label=field.removesuffix("s")) != expected:
        raise ValueError(f"record must use next global sequence {expected}")
    result = deepcopy(validated)
    result[field].append(item)
    return validate_run_ledger(result)


def append_candidate(ledger: object, candidate: object) -> dict[str, object]:
    """Append one immutable candidate record."""
    return _append_record(ledger, candidate, field="candidates")


def append_budget_authorization(
    ledger: object, authorization: object
) -> dict[str, object]:
    """Append one explicit later budget authorization."""
    return _append_record(ledger, authorization, field="budget_authorizations")


def append_preregistration(
    ledger: object, preregistration: object
) -> dict[str, object]:
    """Append one stage freeze or development amendment."""
    return _append_record(ledger, preregistration, field="preregistrations")


def append_intent(ledger: object, intent: object) -> dict[str, object]:
    """Append one paid intent without dropping earlier attempts."""
    return _append_record(ledger, intent, field="intents")


def append_publication(ledger: object, publication: object) -> dict[str, object]:
    """Append a publication that changes only the exact ledger preimage."""
    validated = validate_run_ledger(ledger)
    item = deepcopy(_require_object(publication, label="publication"))
    expected = next_sequence(validated)
    if _record_sequence(item, label="publication") != expected:
        raise ValueError(f"record must use next global sequence {expected}")
    preimage = _canonical_sha256(validated)
    supplied = item.get("ledger_preimage_sha256")
    if supplied is not None and supplied != preimage:
        raise ValueError(
            "publication ledger preimage differs; publication-only delta drift"
        )
    item["ledger_preimage_sha256"] = preimage
    result = deepcopy(validated)
    result["publications"].append(item)
    return validate_run_ledger(result)


def append_outcome(ledger: object, outcome: object) -> dict[str, object]:
    """Append one complete, incomplete, or ineligible attempted-job outcome."""
    return _append_record(ledger, outcome, field="outcomes")


def required_public_subjects(ledger: object) -> tuple[tuple[str, str], ...]:
    """Return every preregistration and intent subject in global sequence order."""
    validated = validate_run_ledger(ledger)
    subjects = [
        (int(record["sequence"]), "preregistration", _preregistration_subject(record))
        for record in validated["preregistrations"]
    ]
    subjects.extend(
        (int(record["sequence"]), "intent", str(record["intent_sha256"]))
        for record in validated["intents"]
    )
    return tuple(
        (subject_type, subject_id) for _, subject_type, subject_id in sorted(subjects)
    )
