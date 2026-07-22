from __future__ import annotations

import copy
import hashlib
import json
from decimal import Decimal
from pathlib import Path

import pytest

import freeze_tb21_study_seed as freezer
from tb21_evidence_contract import (
    append_budget_authorization,
    append_candidate,
    append_intent,
    append_outcome,
    append_preregistration,
    append_publication,
    build_initial_ledger,
    build_task_partition,
    canonical_body_bytes,
    canonical_file_bytes,
    next_sequence,
    parse_canonical_object,
    required_public_subjects,
    stage_shape,
    validate_run_ledger,
    validate_task_partition,
)
from tb21_study_seed import TASK_IDENTITIES, TASK_SET_HASH_DOMAIN, task_set_sha256


def _write_json(path: Path, value: object) -> bytes:
    raw = (json.dumps(value, sort_keys=True) + "\n").encode()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(raw)
    return raw


def _freezer_comparator_fixture(
    root: Path, *, trial_count: int = 2
) -> tuple[Path, dict[str, dict[str, object]]]:
    comparator = root / "comparator"
    entries: list[dict[str, object]] = []
    trial_ids: list[str] = []
    for index in range(trial_count):
        trial_id = f"trial-{index}"
        trial_ids.append(trial_id)
        task_name = f"task-{index}"
        result = {field: None for field in freezer._RESULT_FIELDS}
        result.update(
            {
                "id": f"result-{index}",
                "task_checksum": hashlib.sha256(
                    f"checksum-{index}".encode()
                ).hexdigest(),
                "task_id": {
                    "org": "terminal-bench",
                    "name": task_name,
                    "ref": "sha256:"
                    + hashlib.sha256(f"ref-{index}".encode()).hexdigest(),
                },
                "task_name": f"terminal-bench/{task_name}",
                "trial_name": f"{task_name}__trial",
            }
        )
        result_raw = _write_json(
            comparator / "trials" / trial_id / "result.json", result
        )
        entry = {field: None for field in freezer._MANIFEST_ENTRY_FIELDS}
        entry.update(
            {
                "submitted_trial_id": trial_id,
                "result_id": result["id"],
                "trial_name": result["trial_name"],
                "task_name": result["task_name"],
                "result_sha256": hashlib.sha256(result_raw).hexdigest(),
                "result_bytes": len(result_raw),
            }
        )
        entries.append(entry)
    manifest: dict[str, object] = {
        "leaderboard_job_id": freezer.LEADERBOARD_JOB_ID,
        "submission_url": "https://invalid.example/pinned",
        "entries": entries,
        "failures": [],
    }
    submission: dict[str, object] = {"trials": trial_ids}
    return comparator, {
        "manifest.json": manifest,
        "submission.json": submission,
    }


def _refresh_split_digest(partition: dict[str, object], split: str) -> None:
    digests = partition["split_sha256"]
    assert isinstance(digests, dict)
    digests[split] = hashlib.sha256(canonical_body_bytes(partition[split])).hexdigest()


def _sha256(value: object) -> str:
    return hashlib.sha256(canonical_body_bytes(value)).hexdigest()


def candidate_record(
    *,
    sequence: int,
    candidate_id: str = "dev-r1-a",
    stage: str = "development_round_1",
    job_name: str | None = None,
    candidate_sha256: str | None = None,
    config_sha256: str = "4" * 64,
) -> dict[str, object]:
    shape = stage_shape(stage)
    identity = {
        "source_commit": "1" * 40,
        "binary_sha256": "2" * 64,
        "source_tree_sha256": "3" * 64,
        "config_sha256": config_sha256,
        "adapter_sha256": "5" * 64,
        "analyzer_sha256": "6" * 64,
        "harbor_sha256": "7" * 64,
        "evidence_contract_sha256": "8" * 64,
        "model": "openrouter/z-ai/glm-5.1",
        "provider_route_policy": {
            "provider": "openrouter",
            "model": "z-ai/glm-5.1",
            "allow_fallbacks": False,
            "data_collection": "deny",
        },
        "topology": "direct",
        "role_model": "openrouter/z-ai/glm-5.1",
        "effort": "max",
        "reasoning": True,
        "harbor_concurrency": shape["harbor_concurrency"],
        "per_trial_limit_usd": "0.30",
        "task_split": "development" if stage.startswith("development_") else stage,
        "attempts_per_task": shape["attempts"],
        "retry_max_retries": 0,
    }
    record: dict[str, object] = {
        "sequence": sequence,
        "candidate_id": candidate_id,
        "stage": stage,
        "candidate_sha256": candidate_sha256 or _sha256(identity),
        "record_sha256": "",
        **identity,
        "harbor_concurrency": shape["harbor_concurrency"],
        "per_trial_limit_usd": "0.30",
        "task_split": "development" if stage.startswith("development_") else stage,
        "task_partition_sha256": "a" * 64,
        "attempts_per_task": shape["attempts"],
        "retry_max_retries": 0,
        "job_name": job_name or f"stella-{candidate_id}-{stage}",
        "declared_at": "2026-07-21T12:01:00-07:00",
    }
    record["record_sha256"] = _sha256(
        {
            key: value
            for key, value in record.items()
            if key not in {"sequence", "record_sha256"}
        }
    )
    return record


def _refresh_candidate_record_sha256(record: dict[str, object]) -> None:
    record["record_sha256"] = _sha256(
        {
            key: value
            for key, value in record.items()
            if key not in {"sequence", "record_sha256"}
        }
    )


def _refresh_candidate_identity_and_record_sha256(record: dict[str, object]) -> None:
    identity_fields = (
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
    record["candidate_sha256"] = _sha256(
        {field: record[field] for field in identity_fields}
    )
    _refresh_candidate_record_sha256(record)


def preregistration_record(
    *,
    sequence: int,
    kind: str = "development_round_1",
    candidate_ids: list[str] | None = None,
) -> dict[str, object]:
    return {
        "sequence": sequence,
        "kind": kind,
        "subject_commit": "1" * 40,
        "candidate_ids": candidate_ids if candidate_ids is not None else ["dev-r1-a"],
        "study_manifest_sha256": None,
        "declared_at": "2026-07-21T12:02:00-07:00",
    }


def amendment_record(
    *, sequence: int, invalid_job_name: str, replacement_job_name: str
) -> dict[str, object]:
    candidate = candidate_record(sequence=3)
    return {
        "sequence": sequence,
        "kind": "development_amendment",
        "stage": "development_round_1",
        "candidate_id": "dev-r1-a",
        "invalid_job_name": invalid_job_name,
        "replacement_job_name": replacement_job_name,
        "artifact_tree_sha256": "9" * 64,
        "reason": "runner failed before the first canonical trial",
        "candidate_sha256": candidate["candidate_sha256"],
        "config_sha256": candidate["config_sha256"],
        "subject_commit": "1" * 40,
        "declared_at": "2026-07-21T12:08:00-07:00",
    }


def publication_record(
    *,
    sequence: int,
    subject_type: str,
    subject_id: str,
    ledger_preimage_sha256: str | None = None,
) -> dict[str, object]:
    return {
        "sequence": sequence,
        "subject_type": subject_type,
        "subject_id": subject_id,
        "ledger_preimage_sha256": ledger_preimage_sha256,
        "ledger_commit": "c" * 40,
        "public_url": f"https://github.com/macanderson/stella/commit/{'c' * 40}",
        "published_at": "2026-07-21T12:03:00-07:00",
    }


def intent_record(
    *,
    sequence: int,
    stage: str = "development_round_1",
    candidate_id: str = "dev-r1-a",
    intent_sha256: str = "b" * 64,
    job_name: str | None = None,
    provider_key_name: str = "stella-tb21-tuning-key-v1",
    provider_authorization_id: str = "tuning_provider_v1",
    infrastructure_authorization_id: str = "tuning_infrastructure_v1",
    candidate_sha256: str | None = None,
    config_sha256: str = "4" * 64,
) -> dict[str, object]:
    shape = stage_shape(stage)
    requested_trials = shape["tasks"] * shape["attempts"]
    candidate = candidate_record(
        sequence=3,
        candidate_id=candidate_id,
        stage=stage,
        job_name=job_name,
        candidate_sha256=candidate_sha256,
        config_sha256=config_sha256,
    )
    return {
        "sequence": sequence,
        "intent_sha256": intent_sha256,
        "stage": stage,
        "candidate_id": candidate_id,
        "candidate_sha256": candidate["candidate_sha256"],
        "config_sha256": candidate["config_sha256"],
        "model": candidate["model"],
        "provider_route_policy": candidate["provider_route_policy"],
        "topology": candidate["topology"],
        "role_model": candidate["role_model"],
        "effort": candidate["effort"],
        "reasoning": candidate["reasoning"],
        "job_name": candidate["job_name"],
        "task_split": candidate["task_split"],
        "requested_trials": requested_trials,
        "attempts_per_task": shape["attempts"],
        "retry_max_retries": 0,
        "harbor_concurrency": shape["harbor_concurrency"],
        "per_trial_limit_usd": "0.30",
        "maximum_spend_cents": requested_trials * 30,
        "provider_authorization_id": provider_authorization_id,
        "infrastructure_authorization_id": infrastructure_authorization_id,
        "provider_key_name": provider_key_name,
        "provider_usage_before_usd": "0.00",
        "provider_snapshot_at": "2026-07-21T12:03:30-07:00",
        "declared_at": "2026-07-21T12:04:00-07:00",
    }


def outcome_record(
    *,
    sequence: int,
    intent_sha256: str = "b" * 64,
    status: str = "complete",
    attempted_trials: int | None = None,
    job_name: str | None = None,
    promotion_eligible: bool | None = None,
    stage: str = "development_round_1",
    candidate_id: str = "dev-r1-a",
    candidate_sha256: str | None = None,
    config_sha256: str = "4" * 64,
) -> dict[str, object]:
    candidate = candidate_record(
        sequence=3,
        candidate_id=candidate_id,
        stage=stage,
        job_name=job_name,
        candidate_sha256=candidate_sha256,
        config_sha256=config_sha256,
    )
    attempted_trials = (
        stage_shape(stage)["tasks"] * stage_shape(stage)["attempts"]
        if attempted_trials is None
        else attempted_trials
    )
    expected_paid_call_ids = [] if attempted_trials == 0 else ["call-1"]
    call_envelopes = (
        []
        if attempted_trials == 0
        else [
            {
                "call_id": "call-1",
                "terminal_state": "successful",
                "input_tokens": 0,
                "output_tokens": 0,
                "cached_input_tokens": 0,
                "cost_usd": "0.00",
            }
        ]
    )
    return {
        "sequence": sequence,
        "intent_sha256": intent_sha256,
        "candidate_id": candidate_id,
        "candidate_sha256": candidate["candidate_sha256"],
        "config_sha256": candidate["config_sha256"],
        "job_name": candidate["job_name"],
        "status": status,
        "promotion_eligible": (
            status == "complete" if promotion_eligible is None else promotion_eligible
        ),
        "attempted_trials": attempted_trials,
        "artifact_tree_sha256": "9" * 64,
        "provider_usage_before_usd": "0.00",
        "provider_usage_after_usd": "0.00",
        "provider_usage_delta_usd": "0.00",
        "expected_paid_call_ids": expected_paid_call_ids,
        "call_envelopes": call_envelopes,
        "missing_paid_call_ids": [],
        "telemetry_input_tokens": 0,
        "telemetry_output_tokens": 0,
        "telemetry_cached_input_tokens": 0,
        "telemetry_normalized_tokens": 0,
        "telemetry_cost_sum_usd": "0.00",
        "reconciliation_tolerance_usd": "0.01",
        "completed_at": "2026-07-21T12:06:00-07:00",
        "recorded_at": "2026-07-21T12:07:00-07:00",
    }


def _set_call_evidence(
    outcome: dict[str, object], envelopes: list[dict[str, object]]
) -> None:
    outcome["expected_paid_call_ids"] = [envelope["call_id"] for envelope in envelopes]
    outcome["call_envelopes"] = envelopes
    outcome["missing_paid_call_ids"] = []
    outcome["telemetry_input_tokens"] = sum(
        int(envelope["input_tokens"]) for envelope in envelopes
    )
    outcome["telemetry_output_tokens"] = sum(
        int(envelope["output_tokens"]) for envelope in envelopes
    )
    outcome["telemetry_cached_input_tokens"] = sum(
        int(envelope["cached_input_tokens"]) for envelope in envelopes
    )
    outcome["telemetry_normalized_tokens"] = (
        outcome["telemetry_input_tokens"] + outcome["telemetry_output_tokens"]
    )
    total = sum(Decimal(str(envelope["cost_usd"])) for envelope in envelopes)
    outcome["telemetry_cost_sum_usd"] = format(total, "f")
    outcome["provider_usage_after_usd"] = format(total, "f")
    outcome["provider_usage_delta_usd"] = format(total, "f")


def confirmatory_authorization_record(sequence: int) -> dict[str, object]:
    return {
        "sequence": sequence,
        "authorization_id": "confirmatory_v1",
        "scope": "confirmatory",
        "provider_key_name": "stella-tb21-confirmatory-key-v1",
        "hard_limit_cents": 20_000,
        "provider_cap_cents": 15_000,
        "infrastructure_cap_cents": 5_000,
        "reserve_cents": 0,
        "authorization_commit": "d" * 40,
        "declared_at": "2026-07-21T13:00:00-07:00",
    }


def _development_intent_ledger() -> dict[str, object]:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    ledger = append_candidate(ledger, candidate_record(sequence=3))
    ledger = append_preregistration(
        ledger, preregistration_record(sequence=4, kind="development_round_1")
    )
    ledger = append_publication(
        ledger,
        publication_record(
            sequence=5,
            subject_type="preregistration",
            subject_id="development_round_1",
        ),
    )
    ledger = append_intent(ledger, intent_record(sequence=6))
    return append_publication(
        ledger,
        publication_record(sequence=7, subject_type="intent", subject_id="b" * 64),
    )


def _append_completed_stage(
    ledger: dict[str, object],
    *,
    stage: str,
    candidate_id: str,
    intent_sha256: str,
    promotion_eligible: bool = True,
) -> dict[str, object]:
    candidate = candidate_record(
        sequence=next_sequence(ledger), candidate_id=candidate_id, stage=stage
    )
    ledger = append_candidate(ledger, candidate)
    preregistration = preregistration_record(
        sequence=next_sequence(ledger), kind=stage, candidate_ids=[candidate_id]
    )
    if stage in {"screen", "confirmatory"}:
        preregistration["study_manifest_sha256"] = "e" * 64
    ledger = append_preregistration(ledger, preregistration)
    ledger = append_publication(
        ledger,
        publication_record(
            sequence=next_sequence(ledger),
            subject_type="preregistration",
            subject_id=stage,
        ),
    )
    intent = intent_record(
        sequence=next_sequence(ledger),
        stage=stage,
        candidate_id=candidate_id,
        intent_sha256=intent_sha256,
        candidate_sha256=candidate["candidate_sha256"],
        config_sha256=candidate["config_sha256"],
        provider_key_name=(
            "stella-tb21-confirmatory-key-v1"
            if stage == "confirmatory"
            else "stella-tb21-tuning-key-v1"
        ),
        provider_authorization_id=(
            "confirmatory_v1" if stage == "confirmatory" else "tuning_provider_v1"
        ),
        infrastructure_authorization_id=(
            "confirmatory_v1" if stage == "confirmatory" else "tuning_infrastructure_v1"
        ),
    )
    ledger = append_intent(ledger, intent)
    ledger = append_publication(
        ledger,
        publication_record(
            sequence=next_sequence(ledger),
            subject_type="intent",
            subject_id=intent_sha256,
        ),
    )
    return append_outcome(
        ledger,
        outcome_record(
            sequence=next_sequence(ledger),
            intent_sha256=intent_sha256,
            stage=stage,
            candidate_id=candidate_id,
            candidate_sha256=candidate["candidate_sha256"],
            config_sha256=candidate["config_sha256"],
            promotion_eligible=promotion_eligible,
        ),
    )


def _through_round_three() -> dict[str, object]:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    for stage, digest in (
        ("development_round_1", "b" * 64),
        ("development_round_2", "c" * 64),
        ("development_round_3", "d" * 64),
    ):
        ledger = _append_completed_stage(
            ledger,
            stage=stage,
            candidate_id="winner",
            intent_sha256=digest,
        )
    return ledger


def test_real_seed_and_partition_are_frozen() -> None:
    # This is an internal hash-domain label, not an artifact schema version.
    assert TASK_SET_HASH_DOMAIN == "stella-tb21-task-set-v1"
    assert len(TASK_IDENTITIES) == 89
    assert task_set_sha256(TASK_IDENTITIES) == (
        "7e495afe0a86eaf572be1c2da2b9929c24e502adc888e550385d915cc0125ece"
    )
    partition = build_task_partition(TASK_IDENTITIES)
    assert [
        len(partition[name]) for name in ("development", "screen", "untouched")
    ] == [10, 20, 59]
    assert partition["split_sha256"] == {
        "development": (
            "265ef7896a287493fd846b5835d8eecb83e0e1dd74036aebd4c8e603cf5d3105"
        ),
        "screen": ("48828ea2c4fab2b7791a1b4e76e7d764c18cc94efb631bc944325aa91ace9866"),
        "untouched": (
            "324cfb122eb8220b4f7a177a932f1af45e5e4948fc22c9294156477d157bc26e"
        ),
    }
    screen = partition["screen"]
    assert isinstance(screen, list)
    assert [item["task_name"] for item in screen] == [
        "extract-moves-from-video",
        "pytorch-model-recovery",
        "dna-assembly",
        "path-tracing-reverse",
        "extract-elf",
        "build-cython-ext",
        "polyglot-c-py",
        "sparql-university",
        "polyglot-rust-c",
        "sqlite-db-truncate",
        "password-recovery",
        "build-pmars",
        "qemu-startup",
        "largest-eigenval",
        "regex-chess",
        "model-extraction-relu-logits",
        "mailman",
        "git-multibranch",
        "nginx-request-logging",
        "protein-assembly",
    ]
    assert validate_task_partition(partition) == partition


def test_generated_seed_names_the_internal_hash_domain() -> None:
    source = freezer._source_bytes(TASK_IDENTITIES)

    assert b'TASK_SET_HASH_DOMAIN = "stella-tb21-task-set-v1"' in source
    assert b"Internal hash-domain label; not an artifact schema version." in source


def test_local_verification_rejects_comparator_reference_drift(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    checksums = {name: checksum for name, _task_ref, checksum in TASK_IDENTITIES}
    for task_name in checksums:
        (tmp_path / task_name).mkdir()

    class FakeTask:
        def __init__(self, task_dir: Path) -> None:
            self.checksum = checksums[task_dir.name]

    monkeypatch.setattr(freezer, "Task", FakeTask)
    drifted = list(TASK_IDENTITIES)
    name, _task_ref, checksum = drifted[0]
    drifted[0] = (name, "sha256:" + "0" * 64, checksum)

    with pytest.raises(RuntimeError, match="pinned task identity binding"):
        freezer._verify_local_dataset(tmp_path, tuple(drifted))


def test_freezer_trial_count_is_frozen() -> None:
    assert freezer.EXPECTED_TRIAL_COUNT == 445


def test_freezer_rejects_control_digest_drift(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    original: dict[str, bytes] = {}
    digests: dict[str, str] = {}
    for filename in freezer.CONTROL_SHA256:
        raw = _write_json(tmp_path / filename, {"file": filename})
        original[filename] = raw
        digests[filename] = hashlib.sha256(raw).hexdigest()
    monkeypatch.setattr(freezer, "CONTROL_SHA256", digests)

    assert set(freezer._load_control_files(tmp_path)) == set(digests)
    (tmp_path / "manifest.json").write_bytes(original["manifest.json"] + b" ")
    with pytest.raises(RuntimeError, match="control digest differs"):
        freezer._load_control_files(tmp_path)


@pytest.mark.parametrize("target", ["manifest", "entry", "result"])
def test_freezer_rejects_fields_outside_strict_allowlists(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, target: str
) -> None:
    comparator, controls = _freezer_comparator_fixture(tmp_path)
    monkeypatch.setattr(freezer, "EXPECTED_TRIAL_COUNT", 2)
    manifest = controls["manifest.json"]
    entries = manifest["entries"]
    assert isinstance(entries, list)
    first_entry = entries[0]
    assert isinstance(first_entry, dict)
    if target == "manifest":
        manifest["unexpected"] = True
    elif target == "entry":
        first_entry["unexpected"] = True
    else:
        trial_id = first_entry["submitted_trial_id"]
        assert isinstance(trial_id, str)
        result_path = comparator / "trials" / trial_id / "result.json"
        result = json.loads(result_path.read_bytes())
        result["unexpected"] = True
        result_raw = _write_json(result_path, result)
        first_entry["result_sha256"] = hashlib.sha256(result_raw).hexdigest()
        first_entry["result_bytes"] = len(result_raw)

    with pytest.raises(RuntimeError, match="fields differ"):
        freezer._manifest_trials(comparator, controls)


@pytest.mark.parametrize(
    ("mutation", "message"),
    [
        ("count", "exactly 2"),
        ("identity", "inconsistent result ID"),
        ("submission", "submitted trial allowlist"),
    ],
)
def test_freezer_rejects_count_and_identity_linkage_drift(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    mutation: str,
    message: str,
) -> None:
    comparator, controls = _freezer_comparator_fixture(tmp_path)
    monkeypatch.setattr(freezer, "EXPECTED_TRIAL_COUNT", 2)
    manifest = controls["manifest.json"]
    entries = manifest["entries"]
    submission = controls["submission.json"]
    trial_ids = submission["trials"]
    assert isinstance(entries, list)
    assert isinstance(trial_ids, list)
    if mutation == "count":
        entries.pop()
    elif mutation == "identity":
        entry = entries[0]
        assert isinstance(entry, dict)
        entry["result_id"] = "wrong-result"
    else:
        trial_ids[-1] = "unexpected-trial"

    with pytest.raises(RuntimeError, match=message):
        freezer._manifest_trials(comparator, controls)


@pytest.mark.parametrize("mutation", ["extra", "missing"])
def test_freezer_rejects_extra_or_missing_trial_directories(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    mutation: str,
) -> None:
    comparator, controls = _freezer_comparator_fixture(tmp_path)
    monkeypatch.setattr(freezer, "EXPECTED_TRIAL_COUNT", 2)
    if mutation == "extra":
        (comparator / "trials" / "unexpected").mkdir()
        message = "extra or missing trials"
    else:
        missing = comparator / "trials" / "trial-1" / "result.json"
        missing.unlink()
        message = "cannot read comparator result"

    with pytest.raises(RuntimeError, match=message):
        freezer._manifest_trials(comparator, controls)


def test_freezer_requires_harbor_061(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(freezer.harbor, "__version__", "0.6.2")

    with pytest.raises(RuntimeError, match="Harbor 0.6.1 is required"):
        freezer._verify_local_dataset(tmp_path, ())


def test_freezer_output_is_create_or_identical(tmp_path: Path) -> None:
    output = tmp_path / "seed.py"

    assert freezer._create_or_verify_identical(output, b"frozen\n") == "created"
    assert freezer._create_or_verify_identical(output, b"frozen\n") == "identical"
    output.write_bytes(b"drifted\n")
    with pytest.raises(RuntimeError, match="refusing to overwrite"):
        freezer._create_or_verify_identical(output, b"frozen\n")


def test_canonical_json_has_distinct_file_and_body_encodings() -> None:
    value = {"z": "café", "a": [1, True, None]}
    body = b'{"a":[1,true,null],"z":"caf\xc3\xa9"}'

    assert canonical_body_bytes(value) == body
    assert canonical_file_bytes(value) == body + b"\n"
    assert parse_canonical_object(body + b"\n", label="fixture") == value


@pytest.mark.parametrize(
    "raw",
    [
        b'{"a":1,"a":2}\n',
        b"[]\n",
        b'{"z":2,"a":1}\n',
        b'{"a":1}',
        b'{"a":NaN}\n',
        b'{"a":Infinity}\n',
        b'{"a":-Infinity}\n',
    ],
)
def test_strict_parser_rejects_ambiguous_or_noncanonical_json(raw: bytes) -> None:
    with pytest.raises(ValueError, match="fixture"):
        parse_canonical_object(raw, label="fixture")


def test_strict_parser_normalizes_lone_surrogate_reencoding_failure() -> None:
    raw = b'{"value":"\\ud800"}\n'

    with pytest.raises(ValueError, match="fixture contains invalid Unicode"):
        parse_canonical_object(raw, label="fixture")


@pytest.mark.parametrize("field", ["schema_version", "study_id", "development"])
def test_partition_rejects_missing_fields(field: str) -> None:
    partition = build_task_partition(TASK_IDENTITIES)
    del partition[field]

    with pytest.raises(ValueError):
        validate_task_partition(partition)


def test_partition_rejects_extra_top_level_and_record_fields() -> None:
    partition = build_task_partition(TASK_IDENTITIES)
    partition["unexpected"] = True
    with pytest.raises(ValueError):
        validate_task_partition(partition)

    partition = build_task_partition(TASK_IDENTITIES)
    development = partition["development"]
    assert isinstance(development, list)
    development[0]["unexpected"] = True
    _refresh_split_digest(partition, "development")
    with pytest.raises(ValueError):
        validate_task_partition(partition)


def test_partition_rejects_duplicate_task_names_and_references() -> None:
    partition = build_task_partition(TASK_IDENTITIES)
    development = partition["development"]
    assert isinstance(development, list)
    development[1]["task_name"] = development[0]["task_name"]
    _refresh_split_digest(partition, "development")
    with pytest.raises(ValueError, match="duplicate task name"):
        validate_task_partition(partition)

    partition = build_task_partition(TASK_IDENTITIES)
    development = partition["development"]
    assert isinstance(development, list)
    development[1]["canonical_task_reference"] = development[0][
        "canonical_task_reference"
    ]
    _refresh_split_digest(partition, "development")
    with pytest.raises(ValueError, match="duplicate task reference"):
        validate_task_partition(partition)


def test_partition_rejects_incorrect_split_digest() -> None:
    partition = build_task_partition(TASK_IDENTITIES)
    digests = partition["split_sha256"]
    assert isinstance(digests, dict)
    digests["screen"] = "0" * 64

    with pytest.raises(ValueError, match="split digest"):
        validate_task_partition(partition)


def test_partition_rejects_a_digest_consistent_nonfrozen_split() -> None:
    partition = copy.deepcopy(build_task_partition(TASK_IDENTITIES))
    untouched = partition["untouched"]
    assert isinstance(untouched, list)
    untouched[0]["task_checksum"] = "0" * 64
    _refresh_split_digest(partition, "untouched")

    with pytest.raises(ValueError, match="frozen seed"):
        validate_task_partition(partition)


def test_hybrid_lifecycle_accepts_many_intents_and_one_global_sequence() -> None:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    assert next_sequence(ledger) == 3
    candidate = candidate_record(sequence=3, candidate_id="dev-r1-a")
    ledger = append_candidate(ledger, candidate)
    ledger = append_preregistration(
        ledger,
        preregistration_record(sequence=4, kind="development_round_1"),
    )
    ledger = append_publication(
        ledger,
        publication_record(
            sequence=5,
            subject_type="preregistration",
            subject_id="development_round_1",
        ),
    )
    ledger = append_intent(
        ledger,
        intent_record(sequence=6, stage="development_round_1", candidate_id="dev-r1-a"),
    )
    ledger = append_publication(
        ledger,
        publication_record(
            sequence=7,
            subject_type="intent",
            subject_id="b" * 64,
        ),
    )
    assert validate_run_ledger(ledger) == ledger
    assert required_public_subjects(ledger) == (
        ("preregistration", "development_round_1"),
        ("intent", "b" * 64),
    )
    assert [
        record["sequence"]
        for field in (
            "budget_authorizations",
            "preregistrations",
            "candidates",
            "intents",
            "publications",
            "outcomes",
        )
        for record in ledger[field]
    ] != list(range(1, 8))
    assert sorted(
        record["sequence"]
        for field in (
            "budget_authorizations",
            "preregistrations",
            "candidates",
            "intents",
            "publications",
            "outcomes",
        )
        for record in ledger[field]
    ) == list(range(1, 8))


def test_initial_budget_and_stage_shapes_are_exact() -> None:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )

    assert ledger["budget_authorizations"] == [
        {
            "sequence": 1,
            "authorization_id": "tuning_provider_v1",
            "scope": "tuning_and_screen",
            "provider_key_name": "stella-tb21-tuning-key-v1",
            "hard_limit_cents": 10_000,
            "provider_cap_cents": 10_000,
            "infrastructure_cap_cents": 0,
            "reserve_cents": 1_500,
            "authorization_commit": "a" * 40,
            "declared_at": "2026-07-21T12:00:00-07:00",
        },
        {
            "sequence": 2,
            "authorization_id": "tuning_infrastructure_v1",
            "scope": "tuning_and_screen",
            "provider_key_name": None,
            "hard_limit_cents": 5_500,
            "provider_cap_cents": 0,
            "infrastructure_cap_cents": 5_500,
            "reserve_cents": 0,
            "authorization_commit": "a" * 40,
            "declared_at": "2026-07-21T12:00:00-07:00",
        },
    ]
    assert stage_shape("development_round_3") == {
        "tasks": 10,
        "attempts": 3,
        "max_candidates": 2,
        "max_intents": 4,
        "max_trials": 60,
        "max_spend_cents": 1_800,
        "harbor_concurrency": 3,
    }
    assert stage_shape("confirmatory")["max_spend_cents"] is None
    with pytest.raises(ValueError, match="stage"):
        stage_shape("calibration")


def test_confirmatory_accepts_only_a_new_explicit_authorization() -> None:
    ledger = _through_round_three()
    ledger = _append_completed_stage(
        ledger,
        stage="screen",
        candidate_id="winner",
        intent_sha256="e" * 64,
        promotion_eligible=True,
    )
    ledger = append_budget_authorization(
        ledger, confirmatory_authorization_record(sequence=next_sequence(ledger))
    )
    candidate = candidate_record(
        sequence=next_sequence(ledger), candidate_id="winner", stage="confirmatory"
    )
    ledger = append_candidate(ledger, candidate)
    preregistration = preregistration_record(
        sequence=next_sequence(ledger), kind="confirmatory", candidate_ids=["winner"]
    )
    preregistration["study_manifest_sha256"] = "e" * 64
    ledger = append_preregistration(ledger, preregistration)
    ledger = append_publication(
        ledger,
        publication_record(
            sequence=next_sequence(ledger),
            subject_type="preregistration",
            subject_id="confirmatory",
        ),
    )
    ledger = append_intent(
        ledger,
        intent_record(
            sequence=next_sequence(ledger),
            stage="confirmatory",
            candidate_id="winner",
            intent_sha256="f" * 64,
            provider_key_name="stella-tb21-confirmatory-key-v1",
            provider_authorization_id="confirmatory_v1",
            infrastructure_authorization_id="confirmatory_v1",
        ),
    )

    assert validate_run_ledger(ledger) == ledger


def test_published_amendment_allows_only_a_zero_trial_development_replacement() -> None:
    ledger = _development_intent_ledger()
    invalid_job_name = "stella-dev-r1-a-development_round_1"
    replacement_job_name = "stella-dev-r1-a-development_round_1-replacement"
    ledger = append_outcome(
        ledger,
        outcome_record(
            sequence=8,
            status="ineligible",
            attempted_trials=0,
            job_name=invalid_job_name,
        ),
    )
    ledger = append_preregistration(
        ledger,
        amendment_record(
            sequence=9,
            invalid_job_name=invalid_job_name,
            replacement_job_name=replacement_job_name,
        ),
    )
    ledger = append_publication(
        ledger,
        publication_record(
            sequence=10,
            subject_type="preregistration",
            subject_id=f"development_amendment:{invalid_job_name}",
        ),
    )
    ledger = append_intent(
        ledger,
        intent_record(
            sequence=11,
            intent_sha256="d" * 64,
            job_name=replacement_job_name,
        ),
    )

    assert validate_run_ledger(ledger) == ledger


@pytest.mark.parametrize(
    "mutation",
    [
        "sequence reuse",
        "booleans as integers",
        "naive timestamps",
        "publication-only delta drift",
        "candidate edits",
        "round overfill",
        "unamended development replacement",
        "incomplete-candidate promotion",
        "wrong key authorization",
        "reserve reuse",
        "confirmatory records without a new explicit authorization",
    ],
)
def test_lifecycle_mutation_matrix_is_rejected(mutation: str) -> None:
    if mutation == "naive timestamps":
        with pytest.raises(ValueError, match="timezone-aware"):
            build_initial_ledger(
                "a" * 64,
                authorization_commit="a" * 40,
                declared_at="2026-07-21T12:00:00",
            )
        return

    if mutation == "sequence reuse":
        ledger = _development_intent_ledger()
        with pytest.raises(ValueError, match="next global sequence"):
            append_outcome(ledger, outcome_record(sequence=7))
        return

    if mutation == "booleans as integers":
        ledger = build_initial_ledger(
            "a" * 64,
            authorization_commit="a" * 40,
            declared_at="2026-07-21T12:00:00-07:00",
        )
        candidate = candidate_record(sequence=3)
        candidate["attempts_per_task"] = True
        _refresh_candidate_identity_and_record_sha256(candidate)
        with pytest.raises(ValueError, match="integer"):
            append_candidate(ledger, candidate)
        return

    if mutation == "publication-only delta drift":
        ledger = build_initial_ledger(
            "a" * 64,
            authorization_commit="a" * 40,
            declared_at="2026-07-21T12:00:00-07:00",
        )
        ledger = append_candidate(ledger, candidate_record(sequence=3))
        ledger = append_preregistration(ledger, preregistration_record(sequence=4))
        publication = publication_record(
            sequence=5,
            subject_type="preregistration",
            subject_id="development_round_1",
            ledger_preimage_sha256=_sha256(ledger),
        )
        ledger["budget_authorizations"][0]["authorization_commit"] = "f" * 40
        ledger["budget_authorizations"][1]["authorization_commit"] = "f" * 40
        with pytest.raises(ValueError, match="preimage"):
            append_publication(ledger, publication)
        return

    if mutation == "candidate edits":
        ledger = _development_intent_ledger()
        ledger["candidates"][0]["config_sha256"] = "f" * 64
        with pytest.raises(ValueError, match=r"candidate (?:identity|record) digest"):
            validate_run_ledger(ledger)
        return

    if mutation == "round overfill":
        ledger = build_initial_ledger(
            "a" * 64,
            authorization_commit="a" * 40,
            declared_at="2026-07-21T12:00:00-07:00",
        )
        for index in range(8):
            ledger = append_candidate(
                ledger,
                candidate_record(
                    sequence=next_sequence(ledger), candidate_id=f"entrant-{index}"
                ),
            )
        with pytest.raises(ValueError, match="candidate cap"):
            append_candidate(
                ledger,
                candidate_record(
                    sequence=next_sequence(ledger), candidate_id="entrant-8"
                ),
            )
        return

    if mutation == "unamended development replacement":
        ledger = _development_intent_ledger()
        ledger = append_outcome(
            ledger,
            outcome_record(sequence=8, status="ineligible", attempted_trials=0),
        )
        with pytest.raises(ValueError, match="published development amendment"):
            append_intent(
                ledger,
                intent_record(
                    sequence=9,
                    intent_sha256="d" * 64,
                    job_name="stella-dev-r1-a-development_round_1-replacement",
                ),
            )
        return

    if mutation == "incomplete-candidate promotion":
        ledger = _development_intent_ledger()
        ledger = append_outcome(
            ledger,
            outcome_record(sequence=8, status="incomplete", attempted_trials=10),
        )
        prior = ledger["candidates"][0]
        with pytest.raises(ValueError, match="complete candidate"):
            append_candidate(
                ledger,
                candidate_record(
                    sequence=9,
                    stage="development_round_2",
                    candidate_sha256=prior["candidate_sha256"],
                    config_sha256=prior["config_sha256"],
                ),
            )
        return

    if mutation == "reserve reuse":
        ledger = build_initial_ledger(
            "a" * 64,
            authorization_commit="a" * 40,
            declared_at="2026-07-21T12:00:00-07:00",
        )
        ledger["budget_authorizations"][0]["reserve_cents"] = 0
        with pytest.raises(ValueError, match="exact approved caps"):
            validate_run_ledger(ledger)
        return

    if mutation == "wrong key authorization":
        ledger = build_initial_ledger(
            "a" * 64,
            authorization_commit="a" * 40,
            declared_at="2026-07-21T12:00:00-07:00",
        )
        ledger = append_candidate(ledger, candidate_record(sequence=3))
        ledger = append_preregistration(ledger, preregistration_record(sequence=4))
        ledger = append_publication(
            ledger,
            publication_record(
                sequence=5,
                subject_type="preregistration",
                subject_id="development_round_1",
            ),
        )
        intent = intent_record(sequence=6)
        intent["provider_key_name"] = "stella-tb21-confirmatory-key-v1"
        with pytest.raises(ValueError, match="provider key"):
            append_intent(ledger, intent)
        return

    if mutation == "confirmatory records without a new explicit authorization":
        ledger = build_initial_ledger(
            "a" * 64,
            authorization_commit="a" * 40,
            declared_at="2026-07-21T12:00:00-07:00",
        )
        with pytest.raises(ValueError, match="explicit confirmatory authorization"):
            append_candidate(ledger, candidate_record(sequence=3, stage="confirmatory"))
        return

    raise AssertionError(f"unhandled mutation {mutation!r}")


@pytest.mark.parametrize(
    "field",
    [
        "provider_usage_before_usd",
        "provider_usage_after_usd",
        "provider_usage_delta_usd",
        "telemetry_cost_sum_usd",
        "reconciliation_tolerance_usd",
    ],
)
def test_metered_values_require_bounded_nonnegative_decimal_strings(field: str) -> None:
    ledger = _development_intent_ledger()
    outcome = outcome_record(sequence=8)
    outcome[field] = 0.0
    with pytest.raises(ValueError, match="decimal string"):
        append_outcome(ledger, outcome)

    outcome[field] = "Infinity"
    with pytest.raises(ValueError, match="decimal string"):
        append_outcome(ledger, outcome)


@pytest.mark.parametrize(
    ("attempted_trials", "expected_paid_call_ids", "call_envelopes", "message"),
    [
        (10, [], [], "attempted trials require expected paid-call IDs"),
        (
            0,
            ["call-1"],
            [],
            "zero attempted trials cannot declare paid-call evidence",
        ),
        (
            0,
            [],
            [
                {
                    "call_id": "call-1",
                    "terminal_state": "successful",
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cached_input_tokens": 0,
                    "cost_usd": "0.00",
                }
            ],
            "zero attempted trials cannot declare paid-call evidence",
        ),
    ],
)
def test_paid_call_evidence_matches_attempted_trial_cardinality(
    attempted_trials: int,
    expected_paid_call_ids: list[str],
    call_envelopes: list[dict[str, object]],
    message: str,
) -> None:
    ledger = _development_intent_ledger()
    outcome = outcome_record(
        sequence=8,
        status="complete" if attempted_trials else "ineligible",
        attempted_trials=attempted_trials,
    )
    outcome["expected_paid_call_ids"] = expected_paid_call_ids
    outcome["call_envelopes"] = call_envelopes

    with pytest.raises(ValueError, match=message):
        append_outcome(ledger, outcome)


def test_call_envelopes_retain_all_terminal_states_and_derive_exact_aggregates() -> (
    None
):
    ledger = _development_intent_ledger()
    outcome = outcome_record(
        sequence=8,
        status="complete",
        promotion_eligible=False,
    )
    _set_call_evidence(
        outcome,
        [
            {
                "call_id": "call-success",
                "terminal_state": "successful",
                "input_tokens": 11,
                "output_tokens": 5,
                "cached_input_tokens": 3,
                "cost_usd": "0.01",
            },
            {
                "call_id": "call-failed",
                "terminal_state": "failed",
                "input_tokens": 7,
                "output_tokens": 2,
                "cached_input_tokens": 1,
                "cost_usd": "0.02",
            },
            {
                "call_id": "call-aborted",
                "terminal_state": "aborted",
                "input_tokens": 4,
                "output_tokens": 1,
                "cached_input_tokens": 0,
                "cost_usd": "0.03",
            },
        ],
    )

    ledger = append_outcome(ledger, outcome)
    recorded = ledger["outcomes"][0]
    assert [item["terminal_state"] for item in recorded["call_envelopes"]] == [
        "successful",
        "failed",
        "aborted",
    ]
    assert recorded["telemetry_input_tokens"] == 22
    assert recorded["telemetry_output_tokens"] == 8
    assert recorded["telemetry_cached_input_tokens"] == 4
    assert recorded["telemetry_normalized_tokens"] == 30
    assert recorded["telemetry_cost_sum_usd"] == "0.06"


def test_incomplete_call_accounting_is_retained_but_never_complete() -> None:
    ledger = _development_intent_ledger()
    outcome = outcome_record(sequence=8, status="incomplete")
    envelope = outcome["call_envelopes"][0]
    envelope["output_tokens"] = None
    envelope["cost_usd"] = None
    outcome["telemetry_input_tokens"] = None
    outcome["telemetry_output_tokens"] = None
    outcome["telemetry_cached_input_tokens"] = None
    outcome["telemetry_normalized_tokens"] = None
    outcome["telemetry_cost_sum_usd"] = None

    recorded = append_outcome(ledger, outcome)["outcomes"][0]
    assert recorded["call_envelopes"][0]["output_tokens"] is None
    assert recorded["call_envelopes"][0]["cost_usd"] is None

    outcome["status"] = "complete"
    with pytest.raises(ValueError, match="incomplete paid-call accounting.*complete"):
        append_outcome(ledger, outcome)


def test_missing_call_ids_are_exact_and_force_incomplete_status() -> None:
    ledger = _development_intent_ledger()
    outcome = outcome_record(sequence=8, status="incomplete")
    outcome["expected_paid_call_ids"] = ["call-1", "call-2"]
    outcome["missing_paid_call_ids"] = ["call-2"]

    recorded = append_outcome(ledger, outcome)["outcomes"][0]
    assert recorded["missing_paid_call_ids"] == ["call-2"]


@pytest.mark.parametrize(
    ("mutation", "message"),
    [
        ("duplicate", "duplicate paid-call ID"),
        ("unknown", "unknown paid-call ID"),
        ("missing_complete", "missing paid-call IDs.*complete"),
        ("missing_list_drift", "missing paid-call IDs differ"),
        ("token_aggregate", "token aggregates"),
        ("cost_aggregate", "cost aggregate"),
        ("incomplete_envelope", "fields differ"),
        ("float_cost", "decimal string"),
    ],
)
def test_call_envelope_mutations_fail_closed(mutation: str, message: str) -> None:
    ledger = _development_intent_ledger()
    outcome = outcome_record(sequence=8)
    envelope = outcome["call_envelopes"][0]
    if mutation == "duplicate":
        outcome["call_envelopes"].append(copy.deepcopy(envelope))
    elif mutation == "unknown":
        envelope["call_id"] = "unknown-call"
    elif mutation == "missing_complete":
        outcome["call_envelopes"] = []
        outcome["missing_paid_call_ids"] = ["call-1"]
    elif mutation == "missing_list_drift":
        outcome["call_envelopes"] = []
    elif mutation == "token_aggregate":
        outcome["telemetry_normalized_tokens"] = 1
    elif mutation == "cost_aggregate":
        outcome["telemetry_cost_sum_usd"] = "0.01"
    elif mutation == "incomplete_envelope":
        del envelope["output_tokens"]
    elif mutation == "float_cost":
        envelope["cost_usd"] = 0.0
    else:
        raise AssertionError(mutation)

    with pytest.raises(ValueError, match=message):
        append_outcome(ledger, outcome)


def test_ordering_pins_initial_authorization_ids_to_sequences_one_and_two() -> None:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    ledger["budget_authorizations"].reverse()
    ledger["budget_authorizations"][0]["sequence"] = 1
    ledger["budget_authorizations"][1]["sequence"] = 2

    with pytest.raises(ValueError, match="initial budget authorization order"):
        validate_run_ledger(ledger)


def test_ordering_rejects_a_reordered_record_array() -> None:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    ledger = append_candidate(
        ledger, candidate_record(sequence=3, candidate_id="entrant-a")
    )
    ledger = append_candidate(
        ledger, candidate_record(sequence=4, candidate_id="entrant-b")
    )
    ledger["candidates"].reverse()

    with pytest.raises(ValueError, match="strictly sequence-ordered"):
        validate_run_ledger(ledger)


def test_ordering_rejects_future_confirmatory_authorization() -> None:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    ledger["candidates"].append(
        candidate_record(sequence=3, candidate_id="winner", stage="confirmatory")
    )
    ledger["budget_authorizations"].append(
        confirmatory_authorization_record(sequence=4)
    )

    with pytest.raises(ValueError, match="authorization must precede confirmatory"):
        validate_run_ledger(ledger)


def test_promotion_eligibility_is_explicit_and_requires_a_complete_outcome() -> None:
    ledger = _development_intent_ledger()
    outcome = outcome_record(sequence=8, status="incomplete", promotion_eligible=True)

    with pytest.raises(ValueError, match="promotion eligibility requires complete"):
        append_outcome(ledger, outcome)

    outcome["promotion_eligible"] = False
    assert append_outcome(ledger, outcome)["outcomes"][0]["promotion_eligible"] is False


def test_promotion_rejects_screen_without_round_three_evidence() -> None:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )

    with pytest.raises(ValueError, match="screen.*development_round_3"):
        append_candidate(
            ledger, candidate_record(sequence=3, candidate_id="winner", stage="screen")
        )


def test_promotion_rejects_confirmatory_after_failed_complete_screen() -> None:
    ledger = _through_round_three()
    ledger = _append_completed_stage(
        ledger,
        stage="screen",
        candidate_id="winner",
        intent_sha256="e" * 64,
        promotion_eligible=False,
    )
    ledger = append_budget_authorization(
        ledger, confirmatory_authorization_record(next_sequence(ledger))
    )

    with pytest.raises(ValueError, match="confirmatory.*promotion-eligible screen"):
        append_candidate(
            ledger,
            candidate_record(
                sequence=next_sequence(ledger),
                candidate_id="winner",
                stage="confirmatory",
            ),
        )


def test_promotion_accepts_exact_eligible_screen_to_confirmatory_chain() -> None:
    ledger = _through_round_three()
    ledger = _append_completed_stage(
        ledger,
        stage="screen",
        candidate_id="winner",
        intent_sha256="e" * 64,
        promotion_eligible=True,
    )
    ledger = append_budget_authorization(
        ledger, confirmatory_authorization_record(next_sequence(ledger))
    )
    ledger = append_candidate(
        ledger,
        candidate_record(
            sequence=next_sequence(ledger),
            candidate_id="winner",
            stage="confirmatory",
        ),
    )

    assert validate_run_ledger(ledger) == ledger


def test_candidate_freeze_identity_includes_per_trial_and_execution_posture() -> None:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    candidate = candidate_record(sequence=3)
    candidate["per_trial_limit_usd"] = "0.29"
    _refresh_candidate_record_sha256(candidate)

    with pytest.raises(ValueError, match="candidate identity digest"):
        append_candidate(ledger, candidate)


def test_candidate_freeze_rejects_intent_per_trial_drift_with_unchanged_digests() -> (
    None
):
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    candidate = candidate_record(sequence=3)
    ledger = append_candidate(ledger, candidate)
    ledger = append_preregistration(ledger, preregistration_record(sequence=4))
    ledger = append_publication(
        ledger,
        publication_record(
            sequence=5,
            subject_type="preregistration",
            subject_id="development_round_1",
        ),
    )
    intent = intent_record(
        sequence=6,
        candidate_sha256=candidate["candidate_sha256"],
        config_sha256=candidate["config_sha256"],
    )
    intent["per_trial_limit_usd"] = "0.29"
    intent["maximum_spend_cents"] = 290

    with pytest.raises(ValueError, match="per_trial_limit_usd.*frozen candidate"):
        append_intent(ledger, intent)


def test_candidate_freeze_rejects_intent_model_posture_drift() -> None:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    candidate = candidate_record(sequence=3)
    ledger = append_candidate(ledger, candidate)
    ledger = append_preregistration(ledger, preregistration_record(sequence=4))
    ledger = append_publication(
        ledger,
        publication_record(
            sequence=5,
            subject_type="preregistration",
            subject_id="development_round_1",
        ),
    )
    intent = intent_record(
        sequence=6,
        candidate_sha256=candidate["candidate_sha256"],
        config_sha256=candidate["config_sha256"],
    )
    intent["topology"] = "fleet"

    with pytest.raises(ValueError, match="topology.*frozen candidate"):
        append_intent(ledger, intent)


def test_candidate_full_record_digest_rejects_nonidentity_job_name_edit() -> None:
    ledger = build_initial_ledger(
        "a" * 64,
        authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    candidate = candidate_record(sequence=3)
    original_identity_digest = candidate["candidate_sha256"]
    candidate["job_name"] = "edited-after-freeze"

    with pytest.raises(ValueError, match="candidate record digest"):
        append_candidate(ledger, candidate)
    assert candidate["candidate_sha256"] == original_identity_digest


def test_promotion_rejects_behavioral_identity_change_between_stages() -> None:
    ledger = _through_round_three()
    candidate = candidate_record(
        sequence=next_sequence(ledger), candidate_id="winner", stage="screen"
    )
    candidate["source_commit"] = "f" * 40
    _refresh_candidate_identity_and_record_sha256(candidate)

    with pytest.raises(ValueError, match="screen.*frozen candidate identity"):
        append_candidate(ledger, candidate)
