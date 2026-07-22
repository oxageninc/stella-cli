from __future__ import annotations

import csv
import hashlib
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

import tb21_analysis as analysis_module
from tb21_analysis import (
    ANALYSIS_CONTENT_SHA256,
    CALIBRATION_MODEL_ORDER,
    CALIBRATION_TASKS,
    CANONICAL_DATASET_NAME,
    CANONICAL_DATASET_REF,
    CANONICAL_HARBOR_DATASET_SETTINGS,
    CANONICAL_HARBOR_JOB_SETTINGS,
    CANONICAL_HARBOR_SETTINGS,
    CANONICAL_OPENROUTER_BASE_URL,
    CANONICAL_PROVIDER_ROUTE_POLICY,
    STUDY_MANIFEST_VERSION,
    build_report,
    ingest_job,
    load_comparator_inputs,
    task_cluster_bootstrap,
    validate_study,
    write_outputs,
)

_MODEL = analysis_module.PRIMARY_MODEL
_READINESS_MODEL = CALIBRATION_MODEL_ORDER[0]
_CALIBRATION_WINNER = CALIBRATION_MODEL_ORDER[0]


def _full_tasks() -> list[str]:
    return [*CALIBRATION_TASKS, *(f"task-{index:03d}" for index in range(79))]


def _task_ref(task: str) -> str:
    if task == analysis_module.READINESS_TASK:
        return analysis_module.READINESS_TASK_REF
    if task in analysis_module.CALIBRATION_TASK_REFS:
        return analysis_module.CALIBRATION_TASK_REFS[task]
    return "sha256:" + hashlib.sha256(f"ref:{task}".encode()).hexdigest()


def _task_checksum(task: str) -> str:
    if task == analysis_module.READINESS_TASK:
        return analysis_module.READINESS_TASK_SHA256
    if task in analysis_module.CALIBRATION_TASK_CHECKSUMS:
        return analysis_module.CALIBRATION_TASK_CHECKSUMS[task]
    return hashlib.sha256(f"checksum:{task}".encode()).hexdigest()


def _task_set_digest(tasks: list[str]) -> str:
    return analysis_module._task_set_sha256(
        {
            task: {
                "task_ref": _task_ref(task),
                "task_checksum": _task_checksum(task),
            }
            for task in tasks
        }
    )


def _write_json(path: Path, payload: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload), encoding="utf-8")


def _job_config(
    tasks: list[str],
    *,
    attempts: int = 1,
    n_concurrent_trials: int = 1,
    job_name: str = "synthetic-job",
    jobs_dir: str = "jobs",
) -> dict:
    return {
        "job_name": job_name,
        "jobs_dir": jobs_dir,
        "n_attempts": attempts,
        "n_concurrent_trials": n_concurrent_trials,
        "debug": False,
        "quiet": False,
        "artifacts": [],
        "metrics": [],
        "tasks": [],
        "retry": {
            "max_retries": 0,
            "include_exceptions": None,
            "exclude_exceptions": [
                "VerifierOutputParseError",
                "RewardFileEmptyError",
                "RewardFileNotFoundError",
                "AgentTimeoutError",
                "VerifierTimeoutError",
            ],
            "wait_multiplier": 1.0,
            "min_wait_sec": 1.0,
            "max_wait_sec": 60.0,
        },
        "agents": [
            {
                "name": None,
                "import_path": "stella_harbor:StellaAgent",
                "model_name": _MODEL,
                "override_timeout_sec": None,
                "override_setup_timeout_sec": None,
                "max_timeout_sec": None,
                "kwargs": {},
                "env": {},
            }
        ],
        "datasets": [
            {
                "path": None,
                "name": CANONICAL_DATASET_NAME,
                "version": None,
                "ref": CANONICAL_DATASET_REF,
                "registry_url": None,
                "registry_path": None,
                "overwrite": False,
                "download_dir": None,
                "task_names": [f"terminal-bench/{task}" for task in tasks],
                "exclude_task_names": None,
                "n_tasks": None,
            }
        ],
        "timeout_multiplier": 1.0,
        "agent_timeout_multiplier": None,
        "verifier_timeout_multiplier": None,
        "agent_setup_timeout_multiplier": None,
        "environment_build_timeout_multiplier": None,
        "environment": {
            "type": "docker",
            "import_path": None,
            "force_build": False,
            "delete": True,
            "override_cpus": None,
            "override_memory_mb": None,
            "override_storage_mb": None,
            "override_gpus": None,
            "suppress_override_warnings": False,
            "mounts_json": None,
            "env": {},
            "kwargs": {},
        },
        "verifier": {
            "override_timeout_sec": None,
            "max_timeout_sec": None,
            "env": {},
            "disable": False,
        },
    }


def _trial_config(task: str, trial_name: str, *, trials_dir: str = "jobs") -> dict:
    return {
        "task": {
            "path": None,
            "git_url": None,
            "git_commit_id": None,
            "name": f"terminal-bench/{task}",
            "ref": _task_ref(task),
            "overwrite": False,
            "download_dir": None,
            "source": CANONICAL_DATASET_NAME,
        },
        "trial_name": trial_name,
        "trials_dir": trials_dir,
        "artifacts": [],
        "agent": {
            "name": None,
            "import_path": "stella_harbor:StellaAgent",
            "model_name": _MODEL,
            "override_timeout_sec": None,
            "override_setup_timeout_sec": None,
            "max_timeout_sec": None,
            "kwargs": {},
            "env": {},
        },
        "timeout_multiplier": 1.0,
        "agent_timeout_multiplier": None,
        "verifier_timeout_multiplier": None,
        "agent_setup_timeout_multiplier": None,
        "environment_build_timeout_multiplier": None,
        "environment": {
            "type": "docker",
            "import_path": None,
            "force_build": False,
            "delete": True,
            "override_cpus": None,
            "override_memory_mb": None,
            "override_storage_mb": None,
            "override_gpus": None,
            "suppress_override_warnings": False,
            "mounts_json": None,
            "env": {},
            "kwargs": {},
        },
        "verifier": {
            "override_timeout_sec": None,
            "max_timeout_sec": None,
            "env": {},
            "disable": False,
        },
        "job_id": "job-id",
    }


def _valid_atif() -> dict:
    posture, posture_json, posture_sha256 = analysis_module.canonical_engine_posture(
        _MODEL
    )
    return {
        "schema_version": "ATIF-v1.7",
        "session_id": "session-id",
        "agent": {
            "name": "stella",
            "version": "stella 0.4.47",
            "extra": {
                "engine_posture_version": (
                    analysis_module.CANONICAL_ENGINE_POSTURE_VERSION
                ),
                "engine_posture": posture,
                "engine_posture_json": posture_json,
                "engine_posture_sha256": posture_sha256,
                "host_credential_source": (
                    analysis_module.CANONICAL_HOST_CREDENTIAL_SOURCE
                ),
                "host_credential_name": (
                    analysis_module.CANONICAL_HOST_CREDENTIAL_NAME
                ),
                "host_credential_bundle_count": 1,
                "container_credential_absence_verified": True,
            },
        },
        "steps": [{"step_id": 1, "source": "user", "message": "task"}],
    }


def test_ingest_enumerates_success_error_and_uninstantiated(tmp_path: Path) -> None:
    job = tmp_path / "job"
    _write_json(job / "config.json", _job_config(["success", "error", "missing"]))
    _write_json(job / "result.json", {"id": "job-id", "n_total_trials": 3})

    success = job / "success__one"
    config = _trial_config("success", "success__one")
    _write_json(success / "config.json", config)
    _write_json(
        success / "result.json",
        {
            "id": "trial-success",
            "task_name": "terminal-bench/success",
            "trial_name": "success__one",
            "task_id": {"ref": "sha256:success"},
            "task_checksum": "checksum",
            "config": config,
            "agent_result": {
                "n_input_tokens": 100,
                "n_cache_tokens": 80,
                "n_output_tokens": 20,
                "cost_usd": 0.05,
                "metadata": {"stella_return_code": 0},
            },
            "verifier_result": {"rewards": {"reward": 1.0}},
            "exception_info": None,
            "started_at": "2026-01-01T00:00:00Z",
            "finished_at": "2026-01-01T00:00:07Z",
            "agent_execution": {
                "started_at": "2026-01-01T00:00:01Z",
                "finished_at": "2026-01-01T00:00:06Z",
            },
        },
    )
    _write_json(success / "agent" / "trajectory.json", _valid_atif())

    error = job / "error__one"
    config = _trial_config("error", "error__one")
    _write_json(error / "config.json", config)
    _write_json(
        error / "result.json",
        {
            "id": "trial-error",
            "task_name": "terminal-bench/error",
            "trial_name": "error__one",
            "config": config,
            "agent_result": {
                "n_input_tokens": None,
                "n_cache_tokens": None,
                "n_output_tokens": None,
                "cost_usd": None,
            },
            "verifier_result": {"rewards": {}},
            "exception_info": {
                "exception_type": "AgentTimeoutError",
                "exception_message": 'stdout: {"cost_usd": 0.03125, "events": []}',
            },
            "started_at": "2026-01-01T00:00:00Z",
            "finished_at": "2026-01-01T00:00:07Z",
            "agent_execution": {
                "started_at": "2026-01-01T00:00:02Z",
                "finished_at": "2026-01-01T00:00:07Z",
            },
        },
    )

    rows, warnings = ingest_job(job)

    assert warnings == []
    assert len(rows) == 3
    by_task = {row["task"]: row for row in rows}
    assert by_task["success"]["token_spend"] == 120
    assert by_task["success"]["cache_tokens"] == 80
    assert by_task["success"]["agent_wall_seconds"] == 5.0
    assert by_task["success"]["atif_valid"] is True
    assert by_task["error"]["status"] == "error"
    assert by_task["error"]["accuracy_value"] == 0.0
    assert by_task["error"]["token_spend"] is None
    assert by_task["error"]["cost_usd"] == pytest.approx(0.03125)
    assert by_task["error"]["cost_source"] == "stella_envelope_in_exception"
    assert by_task["missing"]["status"] == "not_instantiated"
    assert by_task["missing"]["attempted"] is False


def test_ingest_preserves_multiagent_calibration_job_controls(tmp_path: Path) -> None:
    job = tmp_path / "calibration"
    config = _job_config(list(CALIBRATION_TASKS), attempts=2)
    config["job_name"] = analysis_module.CALIBRATION_JOB_NAME
    config["agents"] = [
        {
            "name": None,
            "import_path": analysis_module.CANONICAL_AGENT_IMPORT_PATH,
            "model_name": model,
            "override_timeout_sec": None,
            "override_setup_timeout_sec": None,
            "max_timeout_sec": None,
            "kwargs": {},
            "env": {},
        }
        for model in CALIBRATION_MODEL_ORDER
    ]
    _write_json(job / "config.json", config)

    rows, warnings = ingest_job(job, product="calibration")

    assert warnings == []
    assert len(rows) == 60
    assert rows[0]["job_agent_count"] == 3
    assert json.loads(rows[0]["job_agent_models_json"]) == list(CALIBRATION_MODEL_ORDER)
    assert rows[0]["job_harbor_missing_fields"] == ""
    for name, value in CANONICAL_HARBOR_SETTINGS.items():
        assert rows[0][f"job_{name}"] == value


def test_readiness_accepts_realistic_path_only_harbor_config(tmp_path: Path) -> None:
    job = tmp_path / analysis_module.READINESS_JOB_NAME
    task_path = (
        tmp_path.parent / analysis_module.READINESS_TASK_RELATIVE_PATH
    ).resolve()
    job_config = _job_config(
        [],
        job_name=analysis_module.READINESS_JOB_NAME,
        jobs_dir=str(tmp_path.resolve()),
    )
    job_config["datasets"] = []
    job_config["tasks"] = [
        {
            "path": str(task_path),
            "git_url": None,
            "git_commit_id": None,
            "name": None,
            "ref": None,
            "overwrite": False,
            "download_dir": None,
            "source": None,
        }
    ]
    trial_config = _trial_config(
        analysis_module.READINESS_TASK,
        "synthetic-adapter-sentinel__1",
        trials_dir=str(job.resolve()),
    )
    trial_config["task"] = dict(job_config["tasks"][0])
    row = {
        "source_input": str(job.resolve()),
        "job_name": analysis_module.READINESS_JOB_NAME,
        **analysis_module._job_study_metadata(job_config),
        **analysis_module._trial_study_metadata(trial_config),
    }

    assert row["job_dataset_count"] == 0
    assert row["job_task_count"] == 1
    assert analysis_module._readiness_harbor_reasons(row) == []


def test_ingest_real_path_only_readiness_preserves_null_ref(tmp_path: Path) -> None:
    job = tmp_path / analysis_module.READINESS_JOB_NAME
    task_path = (
        tmp_path.parent / analysis_module.READINESS_TASK_RELATIVE_PATH
    ).resolve()
    task_config = {
        "path": str(task_path),
        "git_url": None,
        "git_commit_id": None,
        "name": None,
        "ref": None,
        "overwrite": False,
        "download_dir": None,
        "source": None,
    }
    job_config = _job_config(
        [],
        job_name=analysis_module.READINESS_JOB_NAME,
        jobs_dir=str(tmp_path.resolve()),
    )
    job_config["datasets"] = []
    job_config["tasks"] = [task_config]
    _write_json(job / "config.json", job_config)
    _write_json(job / "result.json", {"id": "readiness-real-job"})

    trial = job / "synthetic-adapter-sentinel__1"
    trial_config = _trial_config(
        analysis_module.READINESS_TASK,
        trial.name,
        trials_dir=str(job.resolve()),
    )
    trial_config["task"] = task_config
    trial_config["job_id"] = "readiness-real-job"
    _write_json(trial / "config.json", trial_config)
    _write_json(
        trial / "result.json",
        {
            "id": "readiness-real-trial",
            "task_name": analysis_module.READINESS_TASK_NAME,
            "trial_name": trial.name,
            "task_id": {"path": str(task_path)},
            "task_checksum": analysis_module.READINESS_TASK_SHA256,
            "config": trial_config,
            "agent_result": {
                "n_input_tokens": 1,
                "n_cache_tokens": 0,
                "n_output_tokens": 1,
                "cost_usd": 0.001,
                "metadata": {},
            },
            "verifier_result": {"rewards": {"reward": 1.0}},
            "exception_info": None,
            "started_at": "2026-01-01T00:00:00Z",
            "finished_at": "2026-01-01T00:00:02Z",
            "agent_execution": {
                "started_at": "2026-01-01T00:00:00Z",
                "finished_at": "2026-01-01T00:00:02Z",
            },
        },
    )

    rows, warnings = ingest_job(job, product="calibration_excluded")

    assert warnings == []
    attempted = [row for row in rows if row["attempted"]]
    assert len(attempted) == 1
    assert attempted[0]["task_ref"] is None
    assert attempted[0]["task_checksum"] == analysis_module.READINESS_TASK_SHA256
    assert attempted[0]["trial_task_path"] == str(task_path)


def test_retry_exclude_exception_order_is_set_semantic() -> None:
    expected = analysis_module.CANONICAL_HARBOR_JOB_SETTINGS[
        "retry_exclude_exceptions_json"
    ]
    reordered = json.dumps(
        sorted(analysis_module.CANONICAL_RETRY_EXCLUDE_EXCEPTIONS, reverse=True)
    )

    assert analysis_module._matches_harbor_setting(
        "retry_exclude_exceptions_json", reordered, expected
    )
    for invalid in (
        "null",
        "{}",
        json.dumps(["VerifierTimeoutError"]),
        json.dumps([*analysis_module.CANONICAL_RETRY_EXCLUDE_EXCEPTIONS, "Extra"]),
        json.dumps([*analysis_module.CANONICAL_RETRY_EXCLUDE_EXCEPTIONS, 1]),
    ):
        assert not analysis_module._matches_harbor_setting(
            "retry_exclude_exceptions_json", invalid, expected
        )


def _metric_rows(
    *, reward: float, tokens: int, wall: float, product: str
) -> list[dict]:
    return [
        {
            "product": product,
            "task": task,
            "attempted": True,
            "accuracy_value": reward,
            "token_spend": tokens,
            "agent_wall_seconds": wall,
        }
        for task in ("task-a", "task-b")
        for _ in range(2)
    ]


_BINARY_SHA256 = "a" * 64
_ADAPTER_SHA256 = "d" * 64
_HARBOR_SHA256 = "e" * 64
_SOURCE_COMMIT = "b" * 40
_FREEZE_COMMIT = "c" * 40
_RUN_LEDGER_RELATIVE = "bench/evidence/stella-tb21-run-ledger.json"
_AGENT_VERSION = f"stella 0.4.47 [binary-sha256:{_BINARY_SHA256}]"
_POSTURE, _POSTURE_JSON, _POSTURE_SHA256 = analysis_module.canonical_engine_posture(
    _MODEL
)
(
    _READINESS_POSTURE,
    _READINESS_POSTURE_JSON,
    _READINESS_POSTURE_SHA256,
) = analysis_module.canonical_engine_posture(_READINESS_MODEL)
assert _POSTURE_SHA256 == (
    "98511188b8338637afe0f2ffde1998c26f048db2f9c936549f75bd222600cf76"
)


def _study_manifest(
    *,
    tasks: int = 89,
    attempts: int = 5,
    calibration_rows: list[dict] | None = None,
    calibration_ledger_rows: list[dict] | None = None,
) -> dict:
    calibration_rows = calibration_rows or []
    calibration_ledger_rows = calibration_ledger_rows or []
    return {
        "schema_version": STUDY_MANIFEST_VERSION,
        "preregistration": {
            "study_id": "stella-tb21-scientific-study-v1",
            "run_ledger_path": _RUN_LEDGER_RELATIVE,
            "readiness_commit": _SOURCE_COMMIT,
            "calibration_commit": _SOURCE_COMMIT,
        },
        "sut": {
            "model": _MODEL,
            "allowed_call_models": list(analysis_module.REGISTERED_CALL_MODELS[_MODEL]),
            "binary_sha256": _BINARY_SHA256,
            "source_commit": _SOURCE_COMMIT,
            "source_commit_embedded": True,
            "agent_version": _AGENT_VERSION,
            "adapter_version": "0.6.0",
            "adapter_sha256": _ADAPTER_SHA256,
            "budget_usd": 0.17,
            "disable_reflection": True,
            "base_url": CANONICAL_OPENROUTER_BASE_URL,
            "provider_route_policy": CANONICAL_PROVIDER_ROUTE_POLICY,
            "host_credential_source": (
                analysis_module.CANONICAL_HOST_CREDENTIAL_SOURCE
            ),
            "host_credential_name": analysis_module.CANONICAL_HOST_CREDENTIAL_NAME,
            "host_credential_bundle_count": 1,
            "engine_posture_version": (
                analysis_module.CANONICAL_ENGINE_POSTURE_VERSION
            ),
            "engine_posture": _POSTURE,
            "engine_posture_sha256": _POSTURE_SHA256,
        },
        "analysis": {
            "sha256": ANALYSIS_CONTENT_SHA256,
            "public_timing_sha256": analysis_module.PUBLIC_TIMING_CONTENT_SHA256,
        },
        "dataset": {
            "name": CANONICAL_DATASET_NAME,
            "ref": CANONICAL_DATASET_REF,
            "task_set_sha256": _task_set_digest(_full_tasks()),
            **CANONICAL_HARBOR_DATASET_SETTINGS,
        },
        "design": {"tasks": tasks, "attempts_per_task": attempts},
        "harbor": {
            "version": analysis_module.CANONICAL_HARBOR_VERSION,
            "sha256": _HARBOR_SHA256,
            **CANONICAL_HARBOR_SETTINGS,
            **CANONICAL_HARBOR_JOB_SETTINGS,
        },
        "comparator": {
            "public_job_id": analysis_module.COMPARATOR_PUBLIC_JOB_ID,
            "manifest_sha256": analysis_module.COMPARATOR_MANIFEST_SHA256,
            "trial_data_sha256": analysis_module.COMPARATOR_TRIAL_DATA_SHA256,
            "submission": {
                "repository": analysis_module.COMPARATOR_SUBMISSION_REPOSITORY,
                "commit": analysis_module.COMPARATOR_SUBMISSION_COMMIT,
                "path": analysis_module.COMPARATOR_SUBMISSION_PATH,
                "sha256": analysis_module.COMPARATOR_SUBMISSION_SHA256,
            },
            "agent_name": analysis_module.COMPARATOR_AGENT_NAME,
            "agent_version": analysis_module.COMPARATOR_AGENT_VERSION,
            "model": analysis_module.COMPARATOR_MODEL,
            "reasoning_effort": analysis_module.COMPARATOR_REASONING_EFFORT,
            "expected": {
                "rows": analysis_module.COMPARATOR_EXPECTED_ROWS,
                "tasks": analysis_module.COMPARATOR_EXPECTED_TASKS,
                "attempts_per_task": analysis_module.COMPARATOR_EXPECTED_ATTEMPTS,
                "reward_total": analysis_module.COMPARATOR_EXPECTED_REWARD_TOTAL,
                "token_spend_total": analysis_module.COMPARATOR_EXPECTED_TOKEN_TOTAL,
            },
        },
        "calibration": {
            "seed": analysis_module.CALIBRATION_SEED,
            "tasks": list(CALIBRATION_TASKS),
            "model_order": list(CALIBRATION_MODEL_ORDER),
            "call_models_by_config": {
                model: list(call_models)
                for model, call_models in (
                    analysis_module.CALIBRATION_CALL_MODELS.items()
                )
            },
            "engine_postures_by_config": {
                model: {
                    "version": analysis_module.CANONICAL_ENGINE_POSTURE_VERSION,
                    "posture": analysis_module.canonical_engine_posture(model)[0],
                    "sha256": analysis_module.canonical_engine_posture(model)[2],
                }
                for model in CALIBRATION_MODEL_ORDER
            },
            "job_name": analysis_module.CALIBRATION_JOB_NAME,
            "job_id": next(
                (
                    row.get("job_id")
                    for row in calibration_rows
                    if isinstance(row.get("job_id"), str)
                ),
                "cal-job-id",
            ),
            "attempts_per_model_task": (
                analysis_module.CALIBRATION_ATTEMPTS_PER_MODEL_TASK
            ),
            "n_concurrent_trials": (analysis_module.CALIBRATION_N_CONCURRENT_TRIALS),
            "minimum_passes": analysis_module.CALIBRATION_MINIMUM_PASSES,
            "projection_trials": analysis_module.CALIBRATION_PROJECTION_TRIALS,
            "projected_spend_limit_usd": (analysis_module.CALIBRATION_SPEND_LIMIT_USD),
            "selected_model": _CALIBRATION_WINNER,
            "trial_data_sha256": analysis_module._calibration_trial_data_sha256(
                calibration_rows
            ),
            "excluded_job_ids": list(
                dict.fromkeys(row.get("job_id") for row in calibration_ledger_rows)
            ),
            "excluded_ledger_sha256": analysis_module._calibration_ledger_sha256(
                calibration_ledger_rows
            ),
        },
        "confirmatory": {
            "job_name": "synthetic-job",
            "n_concurrent_trials": (analysis_module.CONFIRMATORY_N_CONCURRENT_TRIALS),
        },
    }


def _intent_payload(
    stage: str,
    *,
    usage_before_usd: float,
    task_set_sha256: str,
) -> dict:
    if stage == "readiness":
        models = [_READINESS_MODEL]
        job_name = analysis_module.READINESS_JOB_NAME
        dataset_name = analysis_module.READINESS_JOB_NAME
        dataset_ref = analysis_module.READINESS_TASK_REF
        task_count = 1
        requested_trials = 1
        attempts_per_task = 1
        concurrency = 1
        preregistration_commit = _SOURCE_COMMIT
        declared_at = "2025-12-31T00:00:10Z"
    elif stage == "calibration":
        models = list(CALIBRATION_MODEL_ORDER)
        job_name = analysis_module.CALIBRATION_JOB_NAME
        dataset_name = CANONICAL_DATASET_NAME
        dataset_ref = CANONICAL_DATASET_REF
        task_count = len(CALIBRATION_TASKS)
        requested_trials = analysis_module.CALIBRATION_EXPECTED_TRIALS
        attempts_per_task = analysis_module.CALIBRATION_ATTEMPTS_PER_MODEL_TASK
        concurrency = analysis_module.CALIBRATION_N_CONCURRENT_TRIALS
        preregistration_commit = _SOURCE_COMMIT
        declared_at = "2025-12-31T00:01:10Z"
    else:
        models = [_MODEL]
        job_name = "synthetic-job"
        dataset_name = CANONICAL_DATASET_NAME
        dataset_ref = CANONICAL_DATASET_REF
        task_count = 89
        requested_trials = 445
        attempts_per_task = 5
        concurrency = analysis_module.CONFIRMATORY_N_CONCURRENT_TRIALS
        preregistration_commit = _FREEZE_COMMIT
        declared_at = "2025-12-31T00:02:10Z"
    return {
        "intent_id": f"stella-tb21-{stage}-intent-v1",
        "stage": stage,
        "historical": False,
        "job_name": job_name,
        "models": models,
        "dataset": {
            "name": dataset_name,
            "ref": dataset_ref,
            "task_count": task_count,
            "task_set_sha256": task_set_sha256,
        },
        "requested_trials": requested_trials,
        "attempts_per_task": attempts_per_task,
        "n_concurrent_trials": concurrency,
        "retry_max_retries": 0,
        "per_trial_budget_usd": 0.17,
        "artifacts": {
            "binary_sha256": _BINARY_SHA256,
            "source_commit": _SOURCE_COMMIT,
            "agent_version": _AGENT_VERSION,
            "adapter_version": "0.6.0",
            "adapter_sha256": _ADAPTER_SHA256,
            "analysis_sha256": ANALYSIS_CONTENT_SHA256,
            "public_timing_sha256": analysis_module.PUBLIC_TIMING_CONTENT_SHA256,
            "harbor_version": analysis_module.CANONICAL_HARBOR_VERSION,
            "harbor_sha256": _HARBOR_SHA256,
            "engine_posture_sha256_by_model": {
                model: analysis_module.canonical_engine_posture(model)[2]
                for model in models
            },
        },
        "execution": {
            "base_url": CANONICAL_OPENROUTER_BASE_URL,
            "provider_route_policy": CANONICAL_PROVIDER_ROUTE_POLICY,
            "disable_reflection": True,
        },
        "provider_key": {
            "fingerprint_sha256": "f" * 64,
            "label": "stella-tb21-dedicated-key-v1",
            "limit_usd": analysis_module.DEDICATED_KEY_LIMIT_USD,
            "usage_before_usd": usage_before_usd,
            "snapshot_at": declared_at.replace("10Z", "09Z"),
        },
        "declared_at": declared_at,
        "preregistration_commit": preregistration_commit,
    }


def _intent_wrapper(sequence: int, intent: dict) -> dict:
    return {
        "sequence": sequence,
        "intent": intent,
        "intent_sha256": analysis_module._canonical_payload_sha256(intent),
    }


def _receipt_public_intent_attestation(
    job_name: str, intent_sha256: str, intent: dict | None = None
) -> dict:
    if job_name == analysis_module.READINESS_JOB_NAME:
        stage, subject_commit, sequence = "readiness", _SOURCE_COMMIT, 4
        created_at, comment_id, usage_before = "2025-12-31T00:00:20Z", 101, 0.0
        if intent is None:
            intent = _intent_payload(
                stage,
                usage_before_usd=usage_before,
                task_set_sha256=_task_set_digest([analysis_module.READINESS_TASK]),
            )
        prior_stage_outcome = None
    elif job_name == analysis_module.CALIBRATION_JOB_NAME:
        stage, subject_commit, sequence = "calibration", _SOURCE_COMMIT, 9
        created_at, comment_id, usage_before = "2025-12-31T00:01:20Z", 102, 0.01
        if intent is None:
            intent = _intent_payload(
                stage,
                usage_before_usd=usage_before,
                task_set_sha256=_task_set_digest(list(CALIBRATION_TASKS)),
            )
        readiness = _intent_payload(
            "readiness",
            usage_before_usd=0.0,
            task_set_sha256=_task_set_digest([analysis_module.READINESS_TASK]),
        )
        prior_stage_outcome = {
            "stage": "readiness",
            "intent_sha256": analysis_module._canonical_payload_sha256(readiness),
            "status": "excluded",
            "completed_at": "2026-01-01T00:00:05Z",
            "recorded_at": "2026-01-01T00:00:06Z",
        }
    else:
        stage, subject_commit, sequence = "confirmatory", _FREEZE_COMMIT, 14
        created_at, comment_id, usage_before = "2025-12-31T00:02:20Z", 103, 0.61
        if intent is None:
            intent = _intent_payload(
                stage,
                usage_before_usd=usage_before,
                task_set_sha256=_task_set_digest(_full_tasks()),
            )
        calibration = _intent_payload(
            "calibration",
            usage_before_usd=0.01,
            task_set_sha256=_task_set_digest(list(CALIBRATION_TASKS)),
        )
        prior_stage_outcome = {
            "stage": "calibration",
            "intent_sha256": analysis_module._canonical_payload_sha256(calibration),
            "status": "complete",
            "completed_at": "2026-01-01T00:01:30Z",
            "recorded_at": "2026-01-01T00:01:31Z",
        }
    ledger_commit = hashlib.sha1(  # noqa: S324 - deterministic fake Git SHA only.
        f"{subject_commit}:{sequence}".encode()
    ).hexdigest()
    body = {
        "schema_version": analysis_module.GITHUB_ATTESTATION_SCHEMA,
        "study_id": analysis_module.FIXED_STUDY_ID,
        "subject_type": "intent",
        "subject_id": intent_sha256,
        "kind": stage,
        "subject_commit": subject_commit,
        "ledger_commit": ledger_commit,
        "ledger_path": analysis_module.FIXED_RUN_LEDGER_PATH,
        "intent_sha256": intent_sha256,
    }
    issue_number = 17
    issue_url = (
        f"https://github.com/{analysis_module.FIXED_REPOSITORY}/issues/{issue_number}"
    )
    runtime_identity = {
        **intent["artifacts"],
        **intent["execution"],
        "provider_key_fingerprint_sha256": intent["provider_key"]["fingerprint_sha256"],
    }
    projected = intent["requested_trials"] * intent["per_trial_budget_usd"]
    remaining = intent["provider_key"]["limit_usd"] - usage_before
    return {
        "schema_version": analysis_module.PUBLIC_INTENT_ATTESTATION_SCHEMA,
        "verification_mode": analysis_module.PUBLIC_INTENT_VERIFICATION_MODE,
        "repository": analysis_module.FIXED_REPOSITORY,
        "repository_private": False,
        "issue_number": issue_number,
        "issue_url": issue_url,
        "issue_title": (
            "Stella Terminal-Bench 2.1 preregistration: "
            f"{analysis_module.FIXED_STUDY_ID}"
        ),
        "issue_author_login": "macanderson",
        "issue_author_association": "OWNER",
        "comment_id": comment_id,
        "comment_url": f"{issue_url}#issuecomment-{comment_id}",
        "comment_author_login": "macanderson",
        "comment_author_association": "OWNER",
        "server_created_at": created_at,
        "server_updated_at": created_at,
        "body_sha256": hashlib.sha256(
            json.dumps(body, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest(),
        "github_attestation_schema_version": analysis_module.GITHUB_ATTESTATION_SCHEMA,
        "study_id": analysis_module.FIXED_STUDY_ID,
        "subject_type": "intent",
        "subject_id": intent_sha256,
        "kind": stage,
        "subject_commit": subject_commit,
        "ledger_commit": ledger_commit,
        "ledger_path": analysis_module.FIXED_RUN_LEDGER_PATH,
        "intent_sha256": intent_sha256,
        "safety_margin_seconds": analysis_module.PUBLIC_INTENT_SAFETY_MARGIN_SECONDS,
        "safety_wait_completed_at_utc": created_at.replace("20Z", "22.000000Z"),
        "final_comment_get_completed_at_utc": created_at.replace("20Z", "22.100000Z"),
        "ledger_sha256": "6" * 64,
        "subject_commit_verified": True,
        "ledger_commit_verified": True,
        "source_commit_verified": True,
        "strict_ancestry_verified": True,
        "prior_stage_outcome": prior_stage_outcome,
        "runtime_identity": runtime_identity,
        "provider_key_live_snapshot": {
            "fingerprint_sha256": intent["provider_key"]["fingerprint_sha256"],
            "label": intent["provider_key"]["label"],
            "limit_usd": intent["provider_key"]["limit_usd"],
            "usage_usd": usage_before,
            "limit_remaining_usd": remaining,
            "nominal_planned_spend_usd": projected,
            "nominal_remaining_after_usd": remaining - projected,
            "total_credits_usd": 210.0,
            "total_usage_usd": 10.0,
            "available_credits_usd": 200.0,
            "fetched_at_utc": created_at.replace("20Z", "22.200000Z"),
        },
        "runtime_revalidated_after_final_get": True,
        "runtime_revalidated_at_utc": created_at.replace("20Z", "22.300000Z"),
    }


def _receipt_payload(job_name: str, models: list[str], intent_sha256: str) -> dict:
    return {
        "schema_version": analysis_module.SECURE_LAUNCH_RECEIPT_SCHEMA,
        "job_name": job_name,
        "models": models,
        "intent_sha256": intent_sha256,
        "public_intent_attestation": _receipt_public_intent_attestation(
            job_name, intent_sha256
        ),
        "launcher_controls": analysis_module.SECURE_LAUNCH_CONTROLS,
    }


def _host_timestamp(value: datetime) -> str:
    return (
        value.astimezone(UTC).isoformat(timespec="microseconds").replace("+00:00", "Z")
    )


def _host_snapshot(captured_at: datetime, jobs_dir: Path) -> dict:
    observed = {
        "os": {
            "system": "Linux",
            "kernel_release": "6.8.0-test",
            "distribution_id": "ubuntu",
            "distribution_version_id": "24.04",
            "distribution_pretty_name": "Ubuntu 24.04 LTS",
        },
        "architecture": "x86_64",
        "cpu": {"effective_vcpus": 8, "model": "Test Xeon"},
        # Linux MemTotal on a nominal 32-GiB class may exclude reserved memory.
        "memory": {"total_bytes": 31 * 1024**3},
        "disk": {
            "probe_path": str(jobs_dir.resolve()),
            "total_bytes": 500 * 1024**3,
            "used_bytes": 200 * 1024**3,
            "free_bytes": 300 * 1024**3,
        },
        "docker": {
            "client_version": "27.5.1",
            "client_api_version": "1.47",
            "server_version": "27.5.1",
            "server_api_version": "1.47",
            "server_os": "linux",
            "server_architecture": "x86_64",
            "reported_running_containers": 0,
        },
        "running_container_ids": [],
    }
    checks = {
        "native_linux_x86_64": True,
        "minimum_vcpus": True,
        "minimum_memory": True,
        "minimum_free_disk": True,
        "docker_native_linux_x86_64": True,
        "zero_running_containers": True,
        "all_passed": True,
    }
    return {
        "captured_at_utc": _host_timestamp(captured_at),
        "host_fingerprint_sha256": "4" * 64,
        "observed": observed,
        "checks": checks,
    }


def _host_binding(
    *,
    job_name: str,
    intent_sha256: str,
    stage: str,
    receipt_sha256: str,
    source_input: Path,
    job_started_at: str,
    public_commit: str = "5" * 40,
) -> dict:
    started = analysis_module._aware_timestamp(job_started_at)
    assert started is not None
    public = _host_snapshot(started - timedelta(seconds=30), source_input.parent)
    live = _host_snapshot(started - timedelta(seconds=10), source_input.parent)
    report = {
        "schema_version": analysis_module.HOST_REPORT_SCHEMA,
        "study_id": analysis_module.FIXED_STUDY_ID,
        "intent_sha256": intent_sha256,
        "stage": stage,
        "job_name": job_name,
        "captured_at_utc": public["captured_at_utc"],
        "host_fingerprint_sha256": public["host_fingerprint_sha256"],
        "requirements": analysis_module.HOST_REQUIREMENTS,
        "observed": public["observed"],
        "checks": public["checks"],
    }
    report_bytes = analysis_module._canonical_json_file_bytes(report)
    assert report_bytes is not None
    return {
        "schema_version": analysis_module.HOST_LAUNCH_BINDING_SCHEMA,
        "study_id": analysis_module.FIXED_STUDY_ID,
        "intent_sha256": intent_sha256,
        "stage": stage,
        "job_name": job_name,
        "public_report": {
            "repository": analysis_module.FIXED_REPOSITORY,
            "commit": public_commit,
            "path": (f"{analysis_module.HOST_REPORT_PATH_PREFIX}/{intent_sha256}.json"),
            "sha256": hashlib.sha256(report_bytes).hexdigest(),
            "fetched_at_utc": _host_timestamp(started - timedelta(seconds=20)),
        },
        "launch_receipt_sha256": receipt_sha256,
        "public_report_payload": report,
        "live_recheck": live,
    }


def _patch_host_attestation_metadata(
    rows: list[dict], *, job_name: str, intent_sha256: str, stage: str
) -> None:
    for row in rows:
        source = Path(row["source_input"])
        public_intent = json.loads(row["launch_receipt_public_intent_attestation_json"])
        binding = _host_binding(
            job_name=job_name,
            intent_sha256=intent_sha256,
            stage=stage,
            receipt_sha256=row["launch_receipt_sha256"],
            source_input=source,
            job_started_at=row.get("job_started_at", "2026-01-01T00:00:00Z"),
            public_commit=public_intent["ledger_commit"],
        )
        _set_host_attestation_payload(row, binding)
        row.update(
            {
                "host_attestation_present": True,
                "host_attestation_schema_version": (
                    analysis_module.HOST_LAUNCH_BINDING_SCHEMA
                ),
                "host_attestation_exact_top_level": True,
                "host_attestation_canonical_json": True,
                "host_attestation_regular_file": True,
                "host_attestation_mode_octal": "0600",
                "host_attestation_path": str(
                    (source / analysis_module.HOST_ATTESTATION_FILENAME).resolve()
                ),
            }
        )


def _set_host_attestation_payload(row: dict, binding: dict) -> None:
    encoded = json.dumps(
        binding, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    )
    sidecar_bytes = analysis_module._canonical_json_file_bytes(binding)
    assert sidecar_bytes is not None
    row["host_attestation_json"] = encoded
    row["host_attestation_sha256"] = hashlib.sha256(sidecar_bytes).hexdigest()


def _patch_receipt_metadata(
    rows: list[dict], *, job_name: str, models: list[str], intent_sha256: str
) -> None:
    controls = json.dumps(
        analysis_module.SECURE_LAUNCH_CONTROLS,
        sort_keys=True,
        separators=(",", ":"),
    )
    models_json = json.dumps(models, separators=(",", ":"))
    public_intent_attestation_json = json.dumps(
        _receipt_public_intent_attestation(job_name, intent_sha256),
        sort_keys=True,
        separators=(",", ":"),
    )
    for row in rows:
        source = Path(row["source_input"])
        row.update(
            {
                "launch_receipt_present": True,
                "launch_receipt_schema_version": (
                    analysis_module.SECURE_LAUNCH_RECEIPT_SCHEMA
                ),
                "launch_receipt_job_name": job_name,
                "launch_receipt_models_json": models_json,
                "launch_receipt_intent_sha256": intent_sha256,
                "launch_receipt_public_intent_attestation_json": (
                    public_intent_attestation_json
                ),
                "launch_receipt_public_intent_exact_fields": True,
                "launch_receipt_controls_json": controls,
                "launch_receipt_exact_top_level": True,
                "launch_receipt_regular_file": True,
                "launch_receipt_mode_octal": "0600",
                "launch_receipt_sha256": "9" * 64,
                "launch_receipt_path": str(
                    (source / analysis_module.SECURE_LAUNCH_RECEIPT_FILENAME).resolve()
                ),
            }
        )
    stage = _receipt_public_intent_attestation(job_name, intent_sha256)["kind"]
    _patch_host_attestation_metadata(
        rows,
        job_name=job_name,
        intent_sha256=intent_sha256,
        stage=stage,
    )


def _live_audit_publications(*row_groups: list[dict]) -> list[dict]:
    paid: dict[str, dict] = {}
    for row in (row for group in row_groups for row in group):
        encoded = row.get("launch_receipt_public_intent_attestation_json")
        if not isinstance(encoded, str):
            continue
        proof = json.loads(encoded)
        paid[proof["subject_id"]] = {
            "subject_type": "intent",
            "subject_id": proof["subject_id"],
            "kind": proof["kind"],
            "subject_commit": proof["subject_commit"],
            "ledger_commit": proof["ledger_commit"],
            "comment_id": proof["comment_id"],
            "html_url": proof["comment_url"],
            "server_created_at": proof["server_created_at"],
            "body_sha256": proof["body_sha256"],
            "payload_sha256": proof["intent_sha256"],
            "verified": True,
        }
    preregistrations = [
        {
            "subject_type": "preregistration",
            "subject_id": kind,
            "verified": True,
        }
        for kind in ("readiness", "calibration", "confirmatory_freeze")
    ]
    return [*preregistrations, *paid.values()]


def _live_audit_commits(*row_groups: list[dict]) -> list[dict]:
    commits: dict[str, dict] = {}
    for row in (row for group in row_groups for row in group):
        encoded = row.get("launch_receipt_public_intent_attestation_json")
        if not isinstance(encoded, str):
            continue
        proof = json.loads(encoded)
        commits[proof["ledger_commit"]] = {
            "commit_sha": proof["ledger_commit"],
            "files": {proof["ledger_path"]: proof["ledger_sha256"]},
            "verified": True,
        }
    return list(commits.values())


def _write_complete_study_job(
    tmp_path: Path,
    *,
    override_cpus: int | None = None,
    drop_metadata_field: str | None = None,
) -> Path:
    tasks = _full_tasks()
    attempted_tasks = tasks[:2]
    configured_attempts = 5
    written_attempts = 2
    job = tmp_path / "synthetic-job"
    job_config = _job_config(
        tasks,
        attempts=configured_attempts,
        n_concurrent_trials=analysis_module.CONFIRMATORY_N_CONCURRENT_TRIALS,
        jobs_dir=str(tmp_path.resolve()),
    )
    job_config["environment"]["override_cpus"] = override_cpus
    _write_json(job / "config.json", job_config)
    _write_json(
        job / "result.json",
        {
            "id": "job-id",
            "n_total_trials": 4,
            "started_at": "2026-01-01T00:02:00Z",
            "finished_at": "2026-01-01T00:02:12Z",
        },
    )
    confirm_intent = _intent_payload(
        "confirmatory",
        usage_before_usd=0.61,
        task_set_sha256=_task_set_digest(tasks),
    )
    receipt = job / analysis_module.SECURE_LAUNCH_RECEIPT_FILENAME
    _write_json(
        receipt,
        _receipt_payload(
            "synthetic-job",
            [_MODEL],
            analysis_module._canonical_payload_sha256(confirm_intent),
        ),
    )
    receipt.chmod(0o600)
    sidecar_payload = _host_binding(
        job_name="synthetic-job",
        intent_sha256=analysis_module._canonical_payload_sha256(confirm_intent),
        stage="confirmatory",
        receipt_sha256=analysis_module._sha256_file(receipt),
        source_input=job,
        job_started_at="2026-01-01T00:02:00Z",
        public_commit=_receipt_public_intent_attestation(
            "synthetic-job",
            analysis_module._canonical_payload_sha256(confirm_intent),
        )["ledger_commit"],
    )
    sidecar_bytes = analysis_module._canonical_json_file_bytes(sidecar_payload)
    assert sidecar_bytes is not None
    sidecar = job / analysis_module.HOST_ATTESTATION_FILENAME
    sidecar.write_bytes(sidecar_bytes)
    sidecar.chmod(0o600)

    for task in attempted_tasks:
        for attempt in range(1, written_attempts + 1):
            trial_name = f"{task}__{attempt}"
            trial = job / trial_name
            config = _trial_config(task, trial_name, trials_dir=str(job.resolve()))
            config["environment"]["override_cpus"] = override_cpus
            metadata = {
                "stella_model": _MODEL,
                "stella_agent_version": _AGENT_VERSION,
                "stella_adapter_version": "0.6.0",
                "stella_adapter_sha256": _ADAPTER_SHA256,
                "stella_binary_sha256": _BINARY_SHA256,
                "stella_binary_sha256_verified_in_container": True,
                "stella_source_commit": _SOURCE_COMMIT,
                "stella_source_commit_verified_in_binary": True,
                "stella_budget_usd": 0.17,
                "stella_disable_reflection": "1",
                "stella_base_url": CANONICAL_OPENROUTER_BASE_URL,
                "stella_provider_route_policy": CANONICAL_PROVIDER_ROUTE_POLICY,
                "stella_host_credential_source": (
                    analysis_module.CANONICAL_HOST_CREDENTIAL_SOURCE
                ),
                "stella_host_credential_name": (
                    analysis_module.CANONICAL_HOST_CREDENTIAL_NAME
                ),
                "stella_host_credential_bundle_count": 1,
                "stella_container_credential_absence_verified": True,
                "stella_engine_posture_version": (
                    analysis_module.CANONICAL_ENGINE_POSTURE_VERSION
                ),
                "stella_engine_posture": _POSTURE,
                "stella_engine_posture_json": _POSTURE_JSON,
                "stella_engine_posture_sha256": _POSTURE_SHA256,
                "stella_harbor_version": analysis_module.CANONICAL_HARBOR_VERSION,
                "stella_harbor_sha256": _HARBOR_SHA256,
                "stella_return_code": 0,
                "stella_status": "completed",
                "stella_accounting": {
                    "state": "complete",
                    "step_usage_records": 1,
                    "fields": {
                        "input_tokens": {"state": "complete", "total": 80},
                        "output_tokens": {"state": "complete", "total": 10},
                        "cached_input_tokens": {
                            "state": "complete",
                            "total": 40,
                        },
                        "cost_usd": {"state": "complete", "total": 0.01},
                    },
                    "envelope_total_cost_usd": 0.01,
                    "step_usage_total_cost_usd": 0.01,
                    "cost_consistency": "consistent",
                    "model_state": "complete",
                    "model_records": 1,
                    "models": list(analysis_module.REGISTERED_CALL_MODELS[_MODEL]),
                },
                "stella_stream": {
                    "stream_complete": True,
                    "terminal_event": "complete",
                    "cost_source": "complete_event",
                    "process_returned": True,
                },
            }
            if drop_metadata_field and task == attempted_tasks[0] and attempt == 1:
                metadata.pop(drop_metadata_field)
            _write_json(trial / "config.json", config)
            _write_json(
                trial / "result.json",
                {
                    "id": f"trial-{task}-{attempt}",
                    "task_name": f"terminal-bench/{task}",
                    "trial_name": trial_name,
                    "task_id": {"ref": _task_ref(task)},
                    "task_checksum": _task_checksum(task),
                    "config": config,
                    "agent_info": {
                        "name": "stella",
                        "version": _AGENT_VERSION,
                        "model_info": {
                            "provider": "openrouter",
                            "name": analysis_module.PRIMARY_CALL_MODELS[0],
                        },
                    },
                    "agent_result": {
                        "n_input_tokens": 80,
                        "n_cache_tokens": 40,
                        "n_output_tokens": 10,
                        "cost_usd": 0.01,
                        "metadata": metadata,
                    },
                    "verifier_result": {"rewards": {"reward": 1.0}},
                    "exception_info": None,
                    "started_at": f"2026-01-01T00:02:0{attempt}Z",
                    "finished_at": f"2026-01-01T00:02:1{attempt}Z",
                    "agent_execution": {
                        "started_at": f"2026-01-01T00:02:0{attempt}Z",
                        "finished_at": f"2026-01-01T00:02:1{attempt}Z",
                    },
                },
            )
            _write_json(trial / "agent" / "trajectory.json", _valid_atif())
    return job


def _materialize_confirmatory_rows(rows: list[dict]) -> list[dict]:
    """Fill the configured 89 x 5 slots without writing 445 trial directories."""
    template = next(row for row in rows if row.get("attempted") is True)
    job_metadata_fields = {
        key
        for key in template
        if key.startswith("job_") or key in {"source_input", "job_name", "job_id"}
    }
    materialized: list[dict] = []
    for row in rows:
        identity = {
            key: row.get(key)
            for key in (
                "source_input",
                "job_name",
                "job_id",
                "slot_id",
                "attempt_index",
                "task",
                "task_name",
                "model",
            )
        }
        identity.update({key: row.get(key) for key in job_metadata_fields})
        filled = dict(template)
        filled.update(identity)
        task = filled["task"]
        attempt = filled["attempt_index"]
        filled.update(
            {
                "requested": True,
                "instantiated": True,
                "attempted": True,
                "status": "completed",
                "task_ref": _task_ref(task),
                "task_checksum": _task_checksum(task),
                "trial_name": f"{task}__{attempt}",
                "trial_id": f"trial-{task}-{attempt}",
                "trial_dir": f"/claim/{task}/{attempt}",
                "reward": 1.0,
                "accuracy_value": 1.0,
            }
        )
        materialized.append(filled)
    return materialized


def _synthetic_comparator_rows() -> list[dict]:
    rows: list[dict] = []
    base_tokens, remainder = divmod(
        analysis_module.COMPARATOR_EXPECTED_TOKEN_TOTAL,
        analysis_module.COMPARATOR_EXPECTED_ROWS,
    )
    tasks = _full_tasks()
    for index in range(analysis_module.COMPARATOR_EXPECTED_ROWS):
        task_index = index // analysis_module.COMPARATOR_EXPECTED_ATTEMPTS
        task = tasks[task_index]
        token_spend = base_tokens + (1 if index < remainder else 0)
        rows.append(
            {
                "attempted": True,
                "task": task,
                "task_name": f"terminal-bench/{task}",
                "task_ref": _task_ref(task),
                "task_checksum": _task_checksum(task),
                "trial_name": f"trial-{index:03d}",
                "trial_id": f"trial-id-{index:03d}",
                "job_id": analysis_module.COMPARATOR_PUBLIC_JOB_ID,
                "reward": 1.0
                if index < analysis_module.COMPARATOR_EXPECTED_REWARD_TOTAL
                else 0.0,
                "accuracy_value": 1.0
                if index < analysis_module.COMPARATOR_EXPECTED_REWARD_TOTAL
                else 0.0,
                "prompt_tokens": token_spend,
                "completion_tokens": 0,
                "cache_tokens": 0,
                "token_spend": token_spend,
                "cost_usd": 0.0,
                "agent_wall_seconds": None,
                "exception_type": None,
                "comparator_source_sha256": (
                    analysis_module.COMPARATOR_MANIFEST_SHA256
                ),
                "comparator_token_consistent": True,
            }
        )
    digest = analysis_module._comparator_trial_data_sha256(rows)
    for row in rows:
        row["comparator_trial_data_sha256"] = digest
    return rows


def _synthetic_calibration_rows() -> list[dict]:
    rows: list[dict] = []
    pass_counts = {
        CALIBRATION_MODEL_ORDER[0]: 14,
        CALIBRATION_MODEL_ORDER[1]: 10,
        CALIBRATION_MODEL_ORDER[2]: 8,
    }
    for model_index, model in enumerate(CALIBRATION_MODEL_ORDER):
        posture, posture_json, posture_sha256 = (
            analysis_module.canonical_engine_posture(model)
        )
        for task_index, task in enumerate(CALIBRATION_TASKS):
            for attempt in range(
                1, analysis_module.CALIBRATION_ATTEMPTS_PER_MODEL_TASK + 1
            ):
                model_slot = task_index * 2 + attempt - 1
                reward = float(model_slot < pass_counts[model])
                row = {
                    "source_input": (f"/audit/{analysis_module.CALIBRATION_JOB_NAME}"),
                    "job_name": analysis_module.CALIBRATION_JOB_NAME,
                    "job_id": "cal-job-id",
                    "job_started_at": "2026-01-01T00:01:00Z",
                    "job_finished_at": "2026-01-01T00:01:30Z",
                    "requested": True,
                    "instantiated": True,
                    "attempted": True,
                    "attempt_index": attempt,
                    "status": "completed",
                    "trial_id": f"cal-{model_index}-{task_index}-{attempt}",
                    "trial_name": f"{task}__{model_index}__{attempt}",
                    "slot_id": f"cal:{model}:{task}:{attempt}",
                    "task": task,
                    "task_name": f"terminal-bench/{task}",
                    "task_ref": analysis_module.CALIBRATION_TASK_REFS[task],
                    "task_checksum": analysis_module.CALIBRATION_TASK_CHECKSUMS[task],
                    "model": model,
                    "reward": reward,
                    "accuracy_value": reward,
                    "prompt_tokens": 80 + model_index,
                    "completion_tokens": 10,
                    "cache_tokens": 40,
                    "token_spend": 90 + model_index,
                    "cost_usd": 0.01,
                    "agent_started_at": "2026-01-01T00:01:00Z",
                    "agent_finished_at": "2026-01-01T00:01:10Z",
                    "trial_started_at": "2026-01-01T00:01:00Z",
                    "trial_finished_at": "2026-01-01T00:01:10Z",
                    "agent_wall_seconds": 10.0 + model_index,
                    "exception_type": None,
                    "atif_valid": True,
                    "binary_sha256": _BINARY_SHA256,
                    "source_commit": _SOURCE_COMMIT,
                    "source_commit_verified_in_binary": True,
                    "agent_info_version": _AGENT_VERSION,
                    "stella_agent_version": _AGENT_VERSION,
                    "adapter_version": "0.6.0",
                    "adapter_sha256": _ADAPTER_SHA256,
                    "analysis_sha256": ANALYSIS_CONTENT_SHA256,
                    "budget_usd": 0.17,
                    "disable_reflection": True,
                    "base_url": CANONICAL_OPENROUTER_BASE_URL,
                    "provider_route_policy": CANONICAL_PROVIDER_ROUTE_POLICY,
                    "host_credential_source": (
                        analysis_module.CANONICAL_HOST_CREDENTIAL_SOURCE
                    ),
                    "host_credential_name": (
                        analysis_module.CANONICAL_HOST_CREDENTIAL_NAME
                    ),
                    "host_credential_bundle_count": 1,
                    "container_credential_absence_verified": True,
                    "engine_posture_version": (
                        analysis_module.CANONICAL_ENGINE_POSTURE_VERSION
                    ),
                    "engine_posture_json": posture_json,
                    "engine_posture_record_json": json.dumps(
                        posture,
                        sort_keys=True,
                        separators=(",", ":"),
                        ensure_ascii=False,
                    ),
                    "engine_posture_sha256": posture_sha256,
                    "atif_engine_posture_version": (
                        analysis_module.CANONICAL_ENGINE_POSTURE_VERSION
                    ),
                    "atif_engine_posture_json": posture_json,
                    "atif_engine_posture_record_json": posture_json,
                    "atif_engine_posture_sha256": posture_sha256,
                    "atif_host_credential_source": (
                        analysis_module.CANONICAL_HOST_CREDENTIAL_SOURCE
                    ),
                    "atif_host_credential_name": (
                        analysis_module.CANONICAL_HOST_CREDENTIAL_NAME
                    ),
                    "atif_host_credential_bundle_count": 1,
                    "atif_container_credential_absence_verified": True,
                    "harbor_version": analysis_module.CANONICAL_HARBOR_VERSION,
                    "harbor_sha256": _HARBOR_SHA256,
                    "trial_dataset_name": CANONICAL_DATASET_NAME,
                    "trial_harbor_missing_fields": "",
                    "trial_harbor_unknown_fields": "",
                    "trial_artifacts_json": "[]",
                    "trial_task_path": None,
                    "trial_task_git_url": None,
                    "trial_task_git_commit_id": None,
                    "trial_task_overwrite": False,
                    "trial_task_download_dir": None,
                    "trial_trials_dir": (
                        f"/audit/{analysis_module.CALIBRATION_JOB_NAME}"
                    ),
                    "job_dataset_count": 1,
                    "job_dataset_name": CANONICAL_DATASET_NAME,
                    "job_dataset_ref": CANONICAL_DATASET_REF,
                    "job_task_count": len(CALIBRATION_TASKS),
                    "job_n_attempts": 2,
                    "job_n_concurrent_trials": (
                        analysis_module.CALIBRATION_N_CONCURRENT_TRIALS
                    ),
                    "job_jobs_dir": "/audit",
                    "job_agent_count": len(CALIBRATION_MODEL_ORDER),
                    "job_agent_models_json": json.dumps(
                        list(CALIBRATION_MODEL_ORDER), separators=(",", ":")
                    ),
                    "job_agent_import_paths_json": json.dumps(
                        [analysis_module.CANONICAL_AGENT_IMPORT_PATH] * 3,
                        separators=(",", ":"),
                    ),
                    "job_harbor_missing_fields": "",
                    "job_harbor_unknown_fields": "",
                    "job_artifact_tree_sha256": "7" * 64,
                    "accounting_state": "complete",
                    "accounting_step_usage_records": 1,
                    "accounting_cost_consistency": "consistent",
                    "accounting_envelope_total_cost_usd": 0.01,
                    "accounting_step_usage_total_cost_usd": 0.01,
                    "accounting_input_tokens": 80 + model_index,
                    "accounting_output_tokens": 10,
                    "accounting_cached_input_tokens": 40,
                    "accounting_model_state": "complete",
                    "accounting_model_records": 1,
                    "accounting_models": analysis_module.CALIBRATION_CALL_MODELS[model][
                        0
                    ],
                    "stream_complete": True,
                    "stream_status": "completed",
                    "stream_terminal_event": "complete",
                    "stream_cost_source": "complete_event",
                    "stream_process_returned": True,
                }
                for name, value in CANONICAL_HARBOR_SETTINGS.items():
                    row[f"job_{name}"] = value
                    row[f"trial_{name}"] = value
                for name, value in CANONICAL_HARBOR_JOB_SETTINGS.items():
                    row[f"job_{name}"] = value
                for name, value in CANONICAL_HARBOR_DATASET_SETTINGS.items():
                    row[f"job_dataset_{name}"] = value
                rows.append(row)
    calibration_intent = _intent_payload(
        "calibration",
        usage_before_usd=0.01,
        task_set_sha256=_task_set_digest(list(CALIBRATION_TASKS)),
    )
    _patch_receipt_metadata(
        rows,
        job_name=analysis_module.CALIBRATION_JOB_NAME,
        models=list(CALIBRATION_MODEL_ORDER),
        intent_sha256=analysis_module._canonical_payload_sha256(calibration_intent),
    )
    return rows


def _synthetic_calibration_ledger_rows() -> list[dict]:
    historical = [
        {
            "source_input": f"/audit/{job_id}",
            "job_id": job_id,
            "slot_id": f"excluded:{job_id}",
            "requested": True,
            "instantiated": False,
            "attempted": False,
            "task": None,
            "trial_id": None,
            "model": None,
            "reward": None,
            "accuracy_value": None,
            "token_spend": None,
            "cost_usd": None,
            "exception_type": None,
        }
        for job_id in analysis_module.REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS
    ]
    readiness = {
        "source_input": f"/audit/{analysis_module.READINESS_JOB_NAME}",
        "job_name": analysis_module.READINESS_JOB_NAME,
        "job_id": "readiness-job-id",
        "job_started_at": "2026-01-01T00:00:00Z",
        "job_finished_at": "2026-01-01T00:00:05Z",
        "slot_id": "readiness:sentinel:1",
        "requested": True,
        "instantiated": True,
        "attempted": True,
        "attempt_index": 1,
        "status": "completed",
        "task": analysis_module.READINESS_TASK,
        "task_name": analysis_module.READINESS_TASK_NAME,
        "task_ref": None,
        "task_checksum": analysis_module.READINESS_TASK_SHA256,
        "trial_id": "readiness-trial-id",
        "trial_name": "synthetic-adapter-sentinel__1",
        "model": _READINESS_MODEL,
        "reward": 1.0,
        "accuracy_value": 1.0,
        "prompt_tokens": 80,
        "completion_tokens": 10,
        "cache_tokens": 40,
        "token_spend": 90,
        "cost_usd": 0.01,
        "agent_started_at": "2026-01-01T00:00:00Z",
        "agent_finished_at": "2026-01-01T00:00:05Z",
        "trial_started_at": "2026-01-01T00:00:00Z",
        "trial_finished_at": "2026-01-01T00:00:05Z",
        "exception_type": None,
        "stella_return_code": 0,
        "binary_sha256": _BINARY_SHA256,
        "source_commit": _SOURCE_COMMIT,
        "agent_info_version": _AGENT_VERSION,
        "adapter_version": "0.6.0",
        "adapter_sha256": _ADAPTER_SHA256,
        "analysis_sha256": ANALYSIS_CONTENT_SHA256,
        "harbor_version": analysis_module.CANONICAL_HARBOR_VERSION,
        "harbor_sha256": _HARBOR_SHA256,
        "engine_posture_sha256": _READINESS_POSTURE_SHA256,
        "engine_posture_version": (analysis_module.CANONICAL_ENGINE_POSTURE_VERSION),
        "engine_posture_json": _READINESS_POSTURE_JSON,
        "engine_posture_record_json": _READINESS_POSTURE_JSON,
        "atif_engine_posture_version": (
            analysis_module.CANONICAL_ENGINE_POSTURE_VERSION
        ),
        "atif_engine_posture_json": _READINESS_POSTURE_JSON,
        "atif_engine_posture_record_json": _READINESS_POSTURE_JSON,
        "atif_engine_posture_sha256": _READINESS_POSTURE_SHA256,
        "atif_valid": True,
        "budget_usd": 0.17,
        "disable_reflection": True,
        "base_url": CANONICAL_OPENROUTER_BASE_URL,
        "provider_route_policy": CANONICAL_PROVIDER_ROUTE_POLICY,
        "host_credential_source": analysis_module.CANONICAL_HOST_CREDENTIAL_SOURCE,
        "host_credential_name": analysis_module.CANONICAL_HOST_CREDENTIAL_NAME,
        "host_credential_bundle_count": 1,
        "container_credential_absence_verified": True,
        "atif_host_credential_source": (
            analysis_module.CANONICAL_HOST_CREDENTIAL_SOURCE
        ),
        "atif_host_credential_name": analysis_module.CANONICAL_HOST_CREDENTIAL_NAME,
        "atif_host_credential_bundle_count": 1,
        "atif_container_credential_absence_verified": True,
        "accounting_state": "complete",
        "accounting_step_usage_records": 1,
        "accounting_cost_consistency": "consistent",
        "accounting_envelope_total_cost_usd": 0.01,
        "accounting_step_usage_total_cost_usd": 0.01,
        "accounting_input_tokens": 80,
        "accounting_output_tokens": 10,
        "accounting_cached_input_tokens": 40,
        "accounting_model_state": "complete",
        "accounting_model_records": 1,
        "accounting_models": analysis_module.CALIBRATION_CALL_MODELS[_READINESS_MODEL][
            0
        ],
        "stream_complete": True,
        "stream_status": "completed",
        "stream_terminal_event": "complete",
        "stream_cost_source": "complete_event",
        "stream_process_returned": True,
        "job_dataset_count": 0,
        "job_dataset_name": None,
        "job_dataset_ref": None,
        "job_task_count": 1,
        "job_n_attempts": 1,
        "job_n_concurrent_trials": 1,
        "job_jobs_dir": "/audit",
        "job_agent_count": 1,
        "job_agent_models_json": json.dumps([_READINESS_MODEL], separators=(",", ":")),
        "job_agent_import_paths_json": json.dumps(
            [analysis_module.CANONICAL_AGENT_IMPORT_PATH], separators=(",", ":")
        ),
        "job_harbor_missing_fields": "",
        "job_harbor_unknown_fields": "",
        "job_artifact_tree_sha256": "8" * 64,
        "trial_dataset_name": None,
        "trial_harbor_missing_fields": "",
        "trial_harbor_unknown_fields": "",
        "trial_artifacts_json": "[]",
        "trial_task_path": (f"/repo/{analysis_module.READINESS_TASK_RELATIVE_PATH}"),
        "trial_task_git_url": None,
        "trial_task_git_commit_id": None,
        "trial_task_overwrite": False,
        "trial_task_download_dir": None,
        "trial_trials_dir": f"/audit/{analysis_module.READINESS_JOB_NAME}",
    }
    for name, value in CANONICAL_HARBOR_SETTINGS.items():
        readiness[f"job_{name}"] = value
        readiness[f"trial_{name}"] = value
    for name, value in CANONICAL_HARBOR_JOB_SETTINGS.items():
        readiness[f"job_{name}"] = value
    for name in CANONICAL_HARBOR_DATASET_SETTINGS:
        readiness[f"job_dataset_{name}"] = None
    readiness["job_tasks_json"] = json.dumps(
        [
            {
                "path": f"/repo/{analysis_module.READINESS_TASK_RELATIVE_PATH}",
                "git_url": None,
                "git_commit_id": None,
                "name": None,
                "ref": None,
                "overwrite": False,
                "download_dir": None,
                "source": None,
            }
        ],
        sort_keys=True,
        separators=(",", ":"),
    )
    readiness_intent = _intent_payload(
        "readiness",
        usage_before_usd=0.0,
        task_set_sha256=_task_set_digest([analysis_module.READINESS_TASK]),
    )
    _patch_receipt_metadata(
        [readiness],
        job_name=analysis_module.READINESS_JOB_NAME,
        models=[_READINESS_MODEL],
        intent_sha256=analysis_module._canonical_payload_sha256(readiness_intent),
    )
    return [*historical, readiness]


def test_calibration_hashes_ignore_evidence_copy_root() -> None:
    calibration = _synthetic_calibration_rows()
    ledger = _synthetic_calibration_ledger_rows()
    copied_calibration = [
        {**row, "source_input": row["source_input"].replace("/audit", "/public-copy")}
        for row in calibration
    ]
    copied_ledger = [
        {
            **row,
            "source_input": row["source_input"].replace("/audit", "/public-copy"),
        }
        for row in ledger
    ]

    assert analysis_module._calibration_trial_data_sha256(
        calibration
    ) == analysis_module._calibration_trial_data_sha256(copied_calibration)
    assert analysis_module._calibration_ledger_sha256(
        ledger
    ) == analysis_module._calibration_ledger_sha256(copied_ledger)


def test_naive_harbor_time_requires_attested_utc_receipt() -> None:
    attested = {
        "launch_receipt_controls_json": analysis_module._normalized_json_object(
            analysis_module.SECURE_LAUNCH_CONTROLS
        )
    }
    parsed = analysis_module._harbor_timestamp("2026-01-01T00:00:00", attested)

    assert parsed is not None
    assert parsed.isoformat() == "2026-01-01T00:00:00+00:00"
    assert analysis_module._harbor_timestamp("2026-01-01T00:00:00", {}) is None


def test_secure_receipt_v2_exactly_binds_public_intent_preflight() -> None:
    intent = _intent_payload(
        "confirmatory",
        usage_before_usd=0.61,
        task_set_sha256=_task_set_digest(_full_tasks()),
    )
    digest = analysis_module._canonical_payload_sha256(intent)
    row = {
        "source_input": "/audit/synthetic-job",
        "job_jobs_dir": "/audit",
    }
    _patch_receipt_metadata(
        [row], job_name="synthetic-job", models=[_MODEL], intent_sha256=digest
    )
    public_intent = _receipt_public_intent_attestation("synthetic-job", digest)

    assert (
        analysis_module._launch_receipt_reasons(
            row,
            expected_job_name="synthetic-job",
            expected_models=[_MODEL],
            expected_intent_sha256=digest,
            expected_kind="confirmatory",
            expected_subject_commit=_FREEZE_COMMIT,
            expected_ledger_commit=public_intent["ledger_commit"],
            expected_runtime_identity=public_intent["runtime_identity"],
            expected_provider_key=intent["provider_key"],
            expected_prior_stage_outcome=public_intent["prior_stage_outcome"],
            expected_projected_spend_usd=(
                intent["requested_trials"] * intent["per_trial_budget_usd"]
            ),
            label="Confirmatory Harbor job",
        )
        == []
    )

    public_intent["ledger_commit"] = _FREEZE_COMMIT
    public_intent["safety_wait_completed_at_utc"] = public_intent["server_created_at"]
    row["launch_receipt_public_intent_attestation_json"] = json.dumps(
        public_intent, sort_keys=True, separators=(",", ":")
    )
    reasons = analysis_module._launch_receipt_reasons(
        row,
        expected_job_name="synthetic-job",
        expected_models=[_MODEL],
        expected_intent_sha256=digest,
        expected_kind="confirmatory",
        expected_subject_commit=_FREEZE_COMMIT,
        expected_ledger_commit=_receipt_public_intent_attestation(
            "synthetic-job", digest
        )["ledger_commit"],
        expected_runtime_identity=public_intent["runtime_identity"],
        expected_provider_key=intent["provider_key"],
        expected_prior_stage_outcome=public_intent["prior_stage_outcome"],
        expected_projected_spend_usd=(
            intent["requested_trials"] * intent["per_trial_budget_usd"]
        ),
        label="Confirmatory Harbor job",
    )

    assert any("ledger_commit" in reason for reason in reasons)
    assert any("two-second safety wait" in reason for reason in reasons)


def test_host_attestation_metadata_ingests_only_exact_canonical_sidecar(
    tmp_path: Path,
) -> None:
    job = tmp_path / "synthetic-job"
    job.mkdir()
    binding = _host_binding(
        job_name=job.name,
        intent_sha256="a" * 64,
        stage="confirmatory",
        receipt_sha256="9" * 64,
        source_input=job,
        job_started_at="2026-01-01T00:02:00Z",
    )
    sidecar = job / analysis_module.HOST_ATTESTATION_FILENAME
    canonical = analysis_module._canonical_json_file_bytes(binding)
    assert canonical is not None
    sidecar.write_bytes(canonical)
    sidecar.chmod(0o600)
    warnings: list[str] = []

    metadata = analysis_module._host_attestation_metadata(job, warnings)

    assert warnings == []
    assert metadata["host_attestation_present"] is True
    assert metadata["host_attestation_schema_version"] == (
        analysis_module.HOST_LAUNCH_BINDING_SCHEMA
    )
    assert metadata["host_attestation_exact_top_level"] is True
    assert metadata["host_attestation_canonical_json"] is True
    assert metadata["host_attestation_regular_file"] is True
    assert metadata["host_attestation_mode_octal"] == "0600"
    assert metadata["host_attestation_sha256"] == hashlib.sha256(canonical).hexdigest()

    sidecar.write_text(json.dumps(binding, indent=2), encoding="utf-8")
    noncanonical = analysis_module._host_attestation_metadata(job, [])
    assert noncanonical["host_attestation_canonical_json"] is False


def test_exact_host_attestation_binds_public_live_and_receipt_evidence() -> None:
    digest = "a" * 64
    row = {
        "source_input": "/audit/synthetic-job",
        "job_jobs_dir": "/audit",
        "job_started_at": "2026-01-01T00:02:00Z",
    }
    _patch_receipt_metadata(
        [row], job_name="synthetic-job", models=[_MODEL], intent_sha256=digest
    )

    reasons = analysis_module._host_attestation_reasons(
        row,
        expected_job_name="synthetic-job",
        expected_intent_sha256=digest,
        expected_stage="confirmatory",
        label="Confirmatory Harbor job",
    )

    assert reasons == []


@pytest.mark.parametrize(
    ("mutation", "reason_fragment"),
    [
        ("missing", "host_attestation_present"),
        ("mode", "host_attestation_mode_octal"),
        ("receipt", "exact launch receipt"),
        ("public_commit", "public host-report identity"),
        ("report_sha", "public host-report SHA-256"),
        ("public_cpu", "host eligibility checks"),
        ("live_containers", "host eligibility checks"),
        ("live_host", "not the public host"),
        ("chronology", "not demonstrably prelaunch"),
        ("probe_path", "configured jobs_dir"),
        ("schema", "schema drift"),
        ("sidecar_sha", "host sidecar SHA-256"),
    ],
)
def test_host_attestation_rejects_missing_tampered_or_ineligible_evidence(
    mutation: str, reason_fragment: str
) -> None:
    digest = "a" * 64
    row = {
        "source_input": "/audit/synthetic-job",
        "job_jobs_dir": "/audit",
        "job_started_at": "2026-01-01T00:02:00Z",
    }
    _patch_receipt_metadata(
        [row], job_name="synthetic-job", models=[_MODEL], intent_sha256=digest
    )
    binding = json.loads(row["host_attestation_json"])
    if mutation == "missing":
        row["host_attestation_present"] = False
    elif mutation == "mode":
        row["host_attestation_mode_octal"] = "0644"
    elif mutation == "receipt":
        binding["launch_receipt_sha256"] = "0" * 64
        _set_host_attestation_payload(row, binding)
    elif mutation == "public_commit":
        binding["public_report"]["commit"] = "b" * 40
        _set_host_attestation_payload(row, binding)
    elif mutation == "report_sha":
        binding["public_report"]["sha256"] = "0" * 64
        _set_host_attestation_payload(row, binding)
    elif mutation == "public_cpu":
        report = binding["public_report_payload"]
        report["observed"]["cpu"]["effective_vcpus"] = 3
        report["checks"]["minimum_vcpus"] = False
        report["checks"]["all_passed"] = False
        report_bytes = analysis_module._canonical_json_file_bytes(report)
        assert report_bytes is not None
        binding["public_report"]["sha256"] = hashlib.sha256(report_bytes).hexdigest()
        _set_host_attestation_payload(row, binding)
    elif mutation == "live_containers":
        live = binding["live_recheck"]
        live["observed"]["running_container_ids"] = ["b" * 64]
        live["observed"]["docker"]["reported_running_containers"] = 1
        live["checks"]["zero_running_containers"] = False
        live["checks"]["all_passed"] = False
        _set_host_attestation_payload(row, binding)
    elif mutation == "live_host":
        binding["live_recheck"]["host_fingerprint_sha256"] = "b" * 64
        _set_host_attestation_payload(row, binding)
    elif mutation == "chronology":
        binding["live_recheck"]["captured_at_utc"] = "2026-01-01T00:02:01Z"
        _set_host_attestation_payload(row, binding)
    elif mutation == "probe_path":
        binding["public_report_payload"]["observed"]["disk"]["probe_path"] = "/other"
        binding["live_recheck"]["observed"]["disk"]["probe_path"] = "/other"
        report_bytes = analysis_module._canonical_json_file_bytes(
            binding["public_report_payload"]
        )
        assert report_bytes is not None
        binding["public_report"]["sha256"] = hashlib.sha256(report_bytes).hexdigest()
        _set_host_attestation_payload(row, binding)
    elif mutation == "schema":
        binding["unexpected"] = True
        _set_host_attestation_payload(row, binding)
    elif mutation == "sidecar_sha":
        row["host_attestation_sha256"] = "0" * 64
    else:  # pragma: no cover - exhaustive parametrization guard.
        raise AssertionError(mutation)

    reasons = analysis_module._host_attestation_reasons(
        row,
        expected_job_name="synthetic-job",
        expected_intent_sha256=digest,
        expected_stage="confirmatory",
        label="Confirmatory Harbor job",
    )

    assert any(reason_fragment in reason for reason in reasons), reasons


def test_secure_receipt_rejects_duplicate_json_keys(tmp_path: Path) -> None:
    job = tmp_path / "job"
    job.mkdir()
    receipt = job / analysis_module.SECURE_LAUNCH_RECEIPT_FILENAME
    receipt.write_text(
        '{"schema_version":"first","schema_version":"second"}\n',
        encoding="utf-8",
    )
    warnings: list[str] = []

    metadata = analysis_module._launch_receipt_metadata(job, warnings)

    assert metadata["launch_receipt_present"] is True
    assert metadata["launch_receipt_exact_top_level"] is False
    assert any("duplicate keys" in warning for warning in warnings)


def test_agent_interval_cannot_substitute_for_harbor_root_or_trial_time() -> None:
    evidence = analysis_module._job_evidence(
        [
            {
                "attempted": True,
                "instantiated": True,
                "agent_started_at": "2026-01-01T00:00:01Z",
                "agent_finished_at": "2026-01-01T00:00:02Z",
            }
        ]
    )

    assert evidence["started_at"] is None
    assert evidence["completed_at"] is None
    assert any(
        "authoritative Harbor root" in reason for reason in evidence["temporal_reasons"]
    )
    assert any(
        "authoritative Harbor trial" in reason
        for reason in evidence["temporal_reasons"]
    )


def _publication(
    sequence: int,
    *,
    subject_type: str,
    subject_id: str,
    commit: str,
    published_at: str,
) -> dict:
    ledger_commit = hashlib.sha1(  # noqa: S324 - deterministic fake Git SHA only.
        f"{commit}:{sequence}".encode()
    ).hexdigest()
    return {
        "sequence": sequence,
        "subject_type": subject_type,
        "subject_id": subject_id,
        "ledger_commit": ledger_commit,
        "public_url": f"https://github.com/acme/stella/commit/{ledger_commit}",
        "published_at": published_at,
    }


def _historical_intent(job_id: str, index: int) -> dict:
    return {
        "intent_id": f"historical-excluded-{index}-{job_id}",
        "stage": "historical_excluded",
        "historical": True,
        "job_name": None,
        "models": [],
        "dataset": {
            "name": None,
            "ref": None,
            "task_count": None,
            "task_set_sha256": None,
        },
        "requested_trials": None,
        "attempts_per_task": None,
        "n_concurrent_trials": None,
        "retry_max_retries": None,
        "per_trial_budget_usd": None,
        "artifacts": {
            "binary_sha256": None,
            "source_commit": None,
            "agent_version": None,
            "adapter_version": None,
            "adapter_sha256": None,
            "analysis_sha256": None,
            "public_timing_sha256": None,
            "harbor_version": None,
            "harbor_sha256": None,
            "engine_posture_sha256_by_model": None,
        },
        "execution": {
            "base_url": None,
            "provider_route_policy": None,
            "disable_reflection": None,
        },
        "provider_key": {
            "fingerprint_sha256": None,
            "label": None,
            "limit_usd": None,
            "usage_before_usd": None,
            "snapshot_at": None,
        },
        "declared_at": f"2026-01-02T00:00:{index:02d}Z",
        "preregistration_commit": None,
    }


def _run_ledger(
    manifest: dict,
    rows: list[dict],
    calibration_rows: list[dict],
    excluded_rows: list[dict],
) -> tuple[dict, Path, str]:
    manifest_sha256 = analysis_module._canonical_payload_sha256(manifest)
    readiness_rows = [
        row for row in excluded_rows if row.get("job_id") == "readiness-job-id"
    ]
    readiness = _intent_wrapper(
        3,
        _intent_payload(
            "readiness",
            usage_before_usd=0.0,
            task_set_sha256=_task_set_digest([analysis_module.READINESS_TASK]),
        ),
    )
    calibration = _intent_wrapper(
        8,
        _intent_payload(
            "calibration",
            usage_before_usd=0.01,
            task_set_sha256=_task_set_digest(list(CALIBRATION_TASKS)),
        ),
    )
    confirmatory = _intent_wrapper(
        13,
        _intent_payload(
            "confirmatory",
            usage_before_usd=0.61,
            task_set_sha256=_task_set_digest(_full_tasks()),
        ),
    )

    def outcome(
        sequence: int,
        wrapper: dict,
        job_id: str,
        job_rows: list[dict],
        *,
        status: str,
        before: float,
        after: float,
        delta: float,
        recorded_at: str,
    ) -> dict:
        evidence = analysis_module._job_evidence(job_rows)
        return {
            "sequence": sequence,
            "intent_sha256": wrapper["intent_sha256"],
            "job_id": job_id,
            "status": status,
            "started_at": evidence["started_at"],
            "completed_at": evidence["completed_at"],
            "artifact_tree_sha256": evidence["artifact_tree_sha256"],
            "provider_usage_before_usd": before,
            "provider_usage_after_usd": after,
            "provider_usage_delta_usd": delta,
            "telemetry_cost_sum_usd": evidence["cost_sum"],
            "reconciliation_status": "reconciled",
            "reconciliation_tolerance_usd": 0.000001,
            "recorded_at": recorded_at,
        }

    intent_records = [readiness, calibration, confirmatory]
    outcome_records = [
        outcome(
            5,
            readiness,
            "readiness-job-id",
            readiness_rows,
            status="excluded",
            before=0.0,
            after=0.01,
            delta=0.01,
            recorded_at="2026-01-01T00:00:06Z",
        ),
        outcome(
            10,
            calibration,
            "cal-job-id",
            calibration_rows,
            status="complete",
            before=0.01,
            after=0.61,
            delta=0.6,
            recorded_at="2026-01-01T00:01:31Z",
        ),
        outcome(
            15,
            confirmatory,
            "job-id",
            rows,
            status="complete",
            before=0.61,
            after=5.06,
            delta=4.45,
            recorded_at="2026-01-01T00:02:20Z",
        ),
    ]
    sequence = 16
    for index, job_id in enumerate(
        analysis_module.REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS, start=1
    ):
        intent = _historical_intent(job_id, index)
        wrapper = _intent_wrapper(sequence, intent)
        intent_records.append(wrapper)
        outcome_records.append(
            {
                "sequence": sequence + 1,
                "intent_sha256": wrapper["intent_sha256"],
                "job_id": job_id,
                "status": "historical_excluded",
                "started_at": None,
                "completed_at": None,
                "artifact_tree_sha256": None,
                "provider_usage_before_usd": None,
                "provider_usage_after_usd": None,
                "provider_usage_delta_usd": None,
                "telemetry_cost_sum_usd": None,
                "reconciliation_status": "unavailable",
                "reconciliation_tolerance_usd": None,
                "recorded_at": f"2026-01-02T00:01:{index:02d}Z",
            }
        )
        sequence += 2
    publications = [
        _publication(
            2,
            subject_type="preregistration",
            subject_id="readiness",
            commit=_SOURCE_COMMIT,
            published_at="2025-12-31T00:00:00Z",
        ),
        _publication(
            4,
            subject_type="intent",
            subject_id=readiness["intent_sha256"],
            commit=_SOURCE_COMMIT,
            published_at="2025-12-31T00:00:20Z",
        ),
        _publication(
            7,
            subject_type="preregistration",
            subject_id="calibration",
            commit=_SOURCE_COMMIT,
            published_at="2025-12-31T00:01:00Z",
        ),
        _publication(
            9,
            subject_type="intent",
            subject_id=calibration["intent_sha256"],
            commit=_SOURCE_COMMIT,
            published_at="2025-12-31T00:01:20Z",
        ),
        _publication(
            12,
            subject_type="preregistration",
            subject_id="confirmatory_freeze",
            commit=_FREEZE_COMMIT,
            published_at="2025-12-31T00:02:00Z",
        ),
        _publication(
            14,
            subject_type="intent",
            subject_id=confirmatory["intent_sha256"],
            commit=_FREEZE_COMMIT,
            published_at="2025-12-31T00:02:20Z",
        ),
    ]
    ledger = {
        "schema_version": analysis_module.RUN_LEDGER_SCHEMA,
        "study_id": manifest["preregistration"]["study_id"],
        "ledger_path": _RUN_LEDGER_RELATIVE,
        "historical_spend_disclosure": dict(
            analysis_module.HISTORICAL_SPEND_DISCLOSURE
        ),
        "preregistrations": [
            {
                "sequence": 1,
                "kind": "readiness",
                "commit": _SOURCE_COMMIT,
                "study_manifest_sha256": None,
                "declared_at": "2025-12-30T23:59:50Z",
            },
            {
                "sequence": 6,
                "kind": "calibration",
                "commit": _SOURCE_COMMIT,
                "study_manifest_sha256": None,
                "declared_at": "2025-12-31T00:00:30Z",
            },
            {
                "sequence": 11,
                "kind": "confirmatory_freeze",
                "commit": _FREEZE_COMMIT,
                "study_manifest_sha256": manifest_sha256,
                "declared_at": "2025-12-31T00:01:30Z",
            },
        ],
        "intents": intent_records,
        "publications": publications,
        "outcomes": sorted(outcome_records, key=lambda item: item["sequence"]),
    }
    return ledger, Path("/audit/repository") / _RUN_LEDGER_RELATIVE, manifest_sha256


def _run_ledger_kwargs(
    manifest: dict,
    rows: list[dict],
    calibration_rows: list[dict],
    excluded_rows: list[dict],
) -> dict:
    ledger, path, manifest_sha256 = _run_ledger(
        manifest, rows, calibration_rows, excluded_rows
    )
    return {
        "run_ledger": ledger,
        "run_ledger_path": path,
        "study_manifest_sha256": manifest_sha256,
    }


def _rehash_ledger_intent(
    ledger: dict,
    stage: str,
    *,
    receipt_rows: list[dict] | None = None,
) -> str:
    wrapper = next(
        item for item in ledger["intents"] if item["intent"]["stage"] == stage
    )
    old = wrapper["intent_sha256"]
    new = analysis_module._canonical_payload_sha256(wrapper["intent"])
    wrapper["intent_sha256"] = new
    publication = next(
        item
        for item in ledger["publications"]
        if item["subject_type"] == "intent" and item["subject_id"] == old
    )
    publication["subject_id"] = new
    next(item for item in ledger["outcomes"] if item["intent_sha256"] == old)[
        "intent_sha256"
    ] = new
    bound_rows = receipt_rows or []
    for row in bound_rows:
        row["launch_receipt_intent_sha256"] = new
        job_name = row.get("launch_receipt_job_name")
        if isinstance(job_name, str):
            public_intent = _receipt_public_intent_attestation(
                job_name, new, wrapper["intent"]
            )
            public_intent["subject_commit"] = wrapper["intent"].get(
                "preregistration_commit"
            )
            public_intent["ledger_commit"] = publication.get("ledger_commit")
            comment_body = {
                "schema_version": public_intent["github_attestation_schema_version"],
                "study_id": public_intent["study_id"],
                "subject_type": public_intent["subject_type"],
                "subject_id": new,
                "kind": public_intent["kind"],
                "subject_commit": public_intent["subject_commit"],
                "ledger_commit": public_intent["ledger_commit"],
                "ledger_path": public_intent["ledger_path"],
                "intent_sha256": new,
            }
            public_intent["body_sha256"] = hashlib.sha256(
                json.dumps(comment_body, sort_keys=True, separators=(",", ":")).encode()
            ).hexdigest()
            row["launch_receipt_public_intent_attestation_json"] = json.dumps(
                public_intent,
                sort_keys=True,
                separators=(",", ":"),
            )
    if bound_rows:
        job_name = bound_rows[0].get("launch_receipt_job_name")
        assert isinstance(job_name, str)
        _patch_host_attestation_metadata(
            bound_rows,
            job_name=job_name,
            intent_sha256=new,
            stage=stage,
        )
    return new


def _complete_study_inputs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> tuple[list[dict], list[dict], list[dict], list[dict], dict]:
    rows, _ = ingest_job(_write_complete_study_job(tmp_path))
    rows = _materialize_confirmatory_rows(rows)
    comparator_rows = _synthetic_comparator_rows()
    comparator_digest = analysis_module._comparator_trial_data_sha256(comparator_rows)
    monkeypatch.setattr(
        analysis_module,
        "COMPARATOR_TRIAL_DATA_SHA256",
        comparator_digest,
    )
    calibration_rows = _synthetic_calibration_rows()
    ledger_rows = _synthetic_calibration_ledger_rows()
    manifest = _study_manifest(
        calibration_rows=calibration_rows,
        calibration_ledger_rows=ledger_rows,
    )
    return rows, comparator_rows, calibration_rows, ledger_rows, manifest


def test_valid_study_manifest_freezes_complete_sut(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, warnings = ingest_job(_write_complete_study_job(tmp_path))
    rows = _materialize_confirmatory_rows(rows)
    comparator_rows = _synthetic_comparator_rows()
    comparator_digest = analysis_module._comparator_trial_data_sha256(comparator_rows)
    monkeypatch.setattr(
        analysis_module,
        "COMPARATOR_TRIAL_DATA_SHA256",
        comparator_digest,
    )
    calibration_rows = _synthetic_calibration_rows()
    calibration_ledger_rows = _synthetic_calibration_ledger_rows()

    manifest = _study_manifest(
        calibration_rows=calibration_rows,
        calibration_ledger_rows=calibration_ledger_rows,
    )
    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator_rows,
        calibration_rows=calibration_rows,
        calibration_ledger_rows=calibration_ledger_rows,
        **_run_ledger_kwargs(manifest, rows, calibration_rows, calibration_ledger_rows),
    )

    assert warnings == []
    assert "job_id" not in manifest["confirmatory"]
    assert validation["manifest_valid"] is True
    assert validation["homogeneous"] is True
    assert validation["matches_manifest"] is True
    assert validation["scientific_artifact_eligible"] is True
    assert validation["public_timing_verified"] is False
    assert validation["claim_eligible"] is False
    assert validation["reasons"] == [
        analysis_module.EXTERNAL_PUBLIC_TIMING_AUDIT_REASON
    ]
    historical_rows = [
        row
        for row in calibration_ledger_rows
        if row.get("job_id") in analysis_module.REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS
    ]
    assert len(historical_rows) == len(
        analysis_module.REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS
    )
    assert all(not row.get("host_attestation_present") for row in historical_rows)
    assert rows[0]["binary_sha256"] == _BINARY_SHA256
    assert rows[0]["adapter_version"] == "0.6.0"
    assert rows[0]["accounting_state"] == "complete"
    assert rows[0]["job_dataset_ref"] == CANONICAL_DATASET_REF
    assert rows[0]["engine_posture_version"] == (
        analysis_module.CANONICAL_ENGINE_POSTURE_VERSION
    )
    assert rows[0]["engine_posture_json"] == _POSTURE_JSON
    assert rows[0]["engine_posture_record_json"] == _POSTURE_JSON
    assert rows[0]["engine_posture_sha256"] == _POSTURE_SHA256
    assert rows[0]["host_credential_source"] == (
        analysis_module.CANONICAL_HOST_CREDENTIAL_SOURCE
    )
    assert rows[0]["host_credential_name"] == (
        analysis_module.CANONICAL_HOST_CREDENTIAL_NAME
    )
    assert rows[0]["container_credential_absence_verified"] is True
    assert (
        validation["calibration_validation"]["observed"]["derived_winner"]
        == _CALIBRATION_WINNER
    )
    assert validation["calibration_validation"]["observed"]["rows"] == 60
    assert validation["calibration_validation"]["ranking"][0]["passes"] == 14
    assert validation["confirmatory_validation"]["observed"]["requested"] == 445

    comparator_rows[0]["trial_id"] = comparator_rows[1]["trial_id"]
    tampered = validate_study(
        rows,
        manifest,
        comparator_rows=comparator_rows,
        calibration_rows=calibration_rows,
        calibration_ledger_rows=calibration_ledger_rows,
    )
    assert tampered["claim_eligible"] is False
    assert any("unique public trial IDs" in reason for reason in tampered["reasons"])
    assert any("trial-data SHA-256" in reason for reason in tampered["reasons"])


def test_missing_confirmatory_host_attestation_blocks_claim_eligibility(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    rows[0]["host_attestation_present"] = False

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **_run_ledger_kwargs(manifest, rows, calibration, excluded),
    )

    assert validation["scientific_artifact_eligible"] is False
    assert validation["claim_eligible"] is False
    assert any(
        "Confirmatory Harbor job" in reason and "host_attestation_present" in reason
        for reason in validation["reasons"]
    )


def test_tampered_calibration_host_receipt_binding_blocks_claim_eligibility(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    binding = json.loads(calibration[0]["host_attestation_json"])
    binding["launch_receipt_sha256"] = "0" * 64
    _set_host_attestation_payload(calibration[0], binding)

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **_run_ledger_kwargs(manifest, rows, calibration, excluded),
    )

    assert validation["scientific_artifact_eligible"] is False
    assert validation["claim_eligible"] is False
    assert any(
        "Calibration Harbor job" in reason and "exact launch receipt" in reason
        for reason in validation["reasons"]
    )


def test_host_ineligible_readiness_sidecar_blocks_claim_eligibility(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    readiness = next(
        row
        for row in excluded
        if row.get("job_name") == analysis_module.READINESS_JOB_NAME
    )
    binding = json.loads(readiness["host_attestation_json"])
    report = binding["public_report_payload"]
    report["observed"]["memory"]["total_bytes"] = 16 * 1024**3
    report["checks"]["minimum_memory"] = False
    report["checks"]["all_passed"] = False
    report_bytes = analysis_module._canonical_json_file_bytes(report)
    assert report_bytes is not None
    binding["public_report"]["sha256"] = hashlib.sha256(report_bytes).hexdigest()
    _set_host_attestation_payload(readiness, binding)

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **_run_ledger_kwargs(manifest, rows, calibration, excluded),
    )

    assert validation["scientific_artifact_eligible"] is False
    assert validation["claim_eligible"] is False
    assert any(
        "Readiness Harbor job public report host eligibility checks" in reason
        for reason in validation["reasons"]
    )


@pytest.mark.parametrize("mutation", ["extra", "missing"])
@pytest.mark.parametrize(
    ("label", "path", "missing_key"),
    [
        ("top-level", (), "schema_version"),
        ("sut", ("sut",), "budget_usd"),
        ("design", ("design",), "tasks"),
        ("harbor", ("harbor",), "sha256"),
        ("comparator", ("comparator",), "agent_name"),
        (
            "comparator.submission",
            ("comparator", "submission"),
            "path",
        ),
        ("comparator.expected", ("comparator", "expected"), "rows"),
        ("calibration", ("calibration",), "seed"),
        (
            "sut.engine_posture",
            ("sut", "engine_posture"),
            "auto_mode",
        ),
        (
            "sut.engine_posture.agents",
            ("sut", "engine_posture", "agents"),
            "triage",
        ),
        (
            "sut.engine_posture.agents.default",
            ("sut", "engine_posture", "agents", "default"),
            "effort",
        ),
        (
            "calibration.engine_postures_by_config",
            ("calibration", "engine_postures_by_config"),
            CALIBRATION_MODEL_ORDER[2],
        ),
        (
            f"calibration.engine_postures_by_config[{CALIBRATION_MODEL_ORDER[0]!r}]",
            (
                "calibration",
                "engine_postures_by_config",
                CALIBRATION_MODEL_ORDER[0],
            ),
            "version",
        ),
        (
            "calibration.engine_postures_by_config"
            f"[{CALIBRATION_MODEL_ORDER[0]!r}].posture",
            (
                "calibration",
                "engine_postures_by_config",
                CALIBRATION_MODEL_ORDER[0],
                "posture",
            ),
            "reasoning_auto",
        ),
        ("confirmatory", ("confirmatory",), "job_name"),
    ],
)
def test_manifest_v6_sections_reject_extra_and_missing_fields(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    mutation: str,
    label: str,
    path: tuple[str, ...],
    missing_key: str,
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    mutated = json.loads(json.dumps(manifest))
    section = mutated
    for key in path:
        section = section[key]
    if mutation == "extra":
        section["unexpected_v6_field"] = True
    else:
        section.pop(missing_key)

    validation = validate_study(
        rows,
        mutated,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **_run_ledger_kwargs(mutated, rows, calibration, excluded),
    )

    assert validation["manifest_valid"] is False
    exact_schema_reason = next(
        reason
        for reason in validation["reasons"]
        if reason.startswith(
            f"Study manifest {label} fields differ from the exact v6 schema"
        )
    )
    if mutation == "extra":
        assert "extra=['unexpected_v6_field']" in exact_schema_reason
    else:
        assert f"missing={[missing_key]!r}" in exact_schema_reason


def test_only_input_bound_live_audit_removes_public_timing_blocker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)
    ledger_sha = "d" * 64
    kwargs["run_ledger_sha256"] = ledger_sha
    manifest_sha = kwargs["study_manifest_sha256"]
    audit_report = {
        "schema_version": analysis_module.PUBLIC_TIMING_AUDIT_SCHEMA_VERSION,
        "repository": analysis_module.FIXED_REPOSITORY,
        "valid": True,
        "inputs": {
            "run_ledger_sha256": ledger_sha,
            "study_manifest_sha256": manifest_sha,
            "evidence_sha256": "e" * 64,
        },
        "commits": _live_audit_commits(rows, calibration, excluded),
        "publications": _live_audit_publications(rows, calibration, excluded),
        "finalization": {"verified": True},
        "errors": [],
    }
    audit = analysis_module.LivePublicTimingAudit(
        report=audit_report,
        run_ledger_sha256=ledger_sha,
        study_manifest_sha256=manifest_sha,
        evidence_sha256="e" * 64,
    )

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        public_timing_audit=audit,
        **kwargs,
    )
    saved_json_only = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        public_timing_audit=audit_report,  # type: ignore[arg-type]
        **kwargs,
    )

    assert validation["scientific_artifact_eligible"] is True
    assert validation["public_timing_verified"] is True
    assert validation["external_public_timing_audit_required"] is False
    assert validation["claim_eligible"] is True
    assert validation["reasons"] == []
    assert saved_json_only["public_timing_verified"] is False
    assert saved_json_only["claim_eligible"] is False
    assert any(
        "not generated by the live verifier" in reason
        for reason in saved_json_only["reasons"]
    )

    tampered_report = json.loads(json.dumps(audit_report))
    next(
        item
        for item in tampered_report["publications"]
        if item.get("subject_type") == "intent"
    )["body_sha256"] = "0" * 64
    tampered = analysis_module.LivePublicTimingAudit(
        report=tampered_report,
        run_ledger_sha256=ledger_sha,
        study_manifest_sha256=manifest_sha,
        evidence_sha256="e" * 64,
    )
    rejected = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        public_timing_audit=tampered,
        **kwargs,
    )
    assert rejected["public_timing_verified"] is False
    assert any(
        "pre-execution receipt proof" in reason for reason in rejected["reasons"]
    )


def test_run_ledger_binds_exact_intent_sha_to_secure_receipts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)
    rows[0]["launch_receipt_intent_sha256"] = "0" * 64

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **kwargs,
    )

    assert validation["claim_eligible"] is False
    assert any(
        "launch_receipt_intent_sha256" in reason
        and "creation-only secure launch receipt" in reason
        for reason in validation["reasons"]
    )


def test_public_evidence_copy_preserves_creation_time_absolute_paths(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    copied_source = Path("/public/evidence/synthetic-job")
    for row in rows:
        row["source_input"] = str(copied_source)
        row["launch_receipt_path"] = str(
            copied_source / analysis_module.SECURE_LAUNCH_RECEIPT_FILENAME
        )
        row["host_attestation_path"] = str(
            copied_source / analysis_module.HOST_ATTESTATION_FILENAME
        )
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **kwargs,
    )

    assert validation["scientific_artifact_eligible"] is True
    assert validation["claim_eligible"] is False


def test_run_ledger_rejects_mutated_immutable_intent_payload(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)
    confirm_intent = next(
        item
        for item in kwargs["run_ledger"]["intents"]
        if item["intent"]["stage"] == "confirmatory"
    )
    confirm_intent["intent"]["requested_trials"] = 444

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **kwargs,
    )

    assert validation["claim_eligible"] is False
    assert any(
        "canonical immutable payload" in reason for reason in validation["reasons"]
    )


def test_readiness_is_exact_deepseek_sentinel_with_complete_telemetry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    readiness_rows = [
        row for row in excluded if row.get("job_id") == "readiness-job-id"
    ]
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)
    readiness_wrapper = next(
        item
        for item in kwargs["run_ledger"]["intents"]
        if item["intent"]["stage"] == "readiness"
    )
    readiness_wrapper["intent"]["models"] = [CALIBRATION_MODEL_ORDER[1]]
    readiness_wrapper["intent"]["artifacts"]["engine_posture_sha256_by_model"] = {
        CALIBRATION_MODEL_ORDER[1]: analysis_module.canonical_engine_posture(
            CALIBRATION_MODEL_ORDER[1]
        )[2]
    }
    readiness_digest = _rehash_ledger_intent(
        kwargs["run_ledger"], "readiness", receipt_rows=readiness_rows
    )
    for row in calibration:
        proof = json.loads(row["launch_receipt_public_intent_attestation_json"])
        proof["prior_stage_outcome"]["intent_sha256"] = readiness_digest
        row["launch_receipt_public_intent_attestation_json"] = json.dumps(
            proof, sort_keys=True, separators=(",", ":")
        )
    readiness_rows[0]["atif_valid"] = False
    readiness_rows[0]["accounting_state"] = "incomplete"
    readiness_rows[0]["reward"] = 0.0
    readiness_rows[0]["accuracy_value"] = 0.0
    readiness_rows[0]["stream_terminal_event"] = "error"
    readiness_rows[0]["exception_type"] = "AgentError"
    readiness_rows[0]["stella_return_code"] = 1

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **kwargs,
    )

    assert validation["claim_eligible"] is False
    assert any(
        "exactly the frozen DeepSeek" in reason for reason in validation["reasons"]
    )
    assert any(
        "accounting state is not complete" in reason for reason in validation["reasons"]
    )
    assert any("atif_valid" in reason for reason in validation["reasons"])
    assert any(
        "verifier reward and accuracy exactly 1.0" in reason
        for reason in validation["reasons"]
    )


def test_readiness_source_can_precede_public_instrumentation_fix(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    readiness_commit = "a" * 40
    manifest["preregistration"]["readiness_commit"] = readiness_commit
    readiness_rows = [
        row for row in excluded if row.get("job_id") == "readiness-job-id"
    ]
    readiness_rows[0]["source_commit"] = readiness_commit
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)
    readiness_prereg = next(
        item
        for item in kwargs["run_ledger"]["preregistrations"]
        if item["kind"] == "readiness"
    )
    readiness_prereg["commit"] = readiness_commit
    readiness_publication = next(
        item
        for item in kwargs["run_ledger"]["publications"]
        if item["subject_type"] == "preregistration"
        and item["subject_id"] == "readiness"
    )
    readiness_publication["ledger_commit"] = "b" * 40
    readiness_publication["public_url"] = (
        f"https://github.com/acme/stella/commit/{'b' * 40}"
    )
    readiness_intent = next(
        item
        for item in kwargs["run_ledger"]["intents"]
        if item["intent"]["stage"] == "readiness"
    )
    readiness_intent["intent"]["artifacts"]["source_commit"] = readiness_commit
    readiness_intent["intent"]["preregistration_commit"] = readiness_commit
    readiness_digest = _rehash_ledger_intent(
        kwargs["run_ledger"], "readiness", receipt_rows=readiness_rows
    )
    for row in calibration:
        proof = json.loads(row["launch_receipt_public_intent_attestation_json"])
        proof["prior_stage_outcome"]["intent_sha256"] = readiness_digest
        row["launch_receipt_public_intent_attestation_json"] = json.dumps(
            proof, sort_keys=True, separators=(",", ":")
        )

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **kwargs,
    )

    assert readiness_commit != manifest["sut"]["source_commit"]
    assert validation["scientific_artifact_eligible"] is True
    assert validation["claim_eligible"] is False


def test_run_ledger_rejects_posthoc_provider_snapshot_and_unregistered_spend(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)
    readiness_rows = [
        row for row in excluded if row.get("job_id") == "readiness-job-id"
    ]
    readiness = next(
        item
        for item in kwargs["run_ledger"]["intents"]
        if item["intent"]["stage"] == "readiness"
    )
    readiness["intent"]["provider_key"]["snapshot_at"] = "2026-01-01T00:00:01Z"
    _rehash_ledger_intent(
        kwargs["run_ledger"], "readiness", receipt_rows=readiness_rows
    )
    calibration_wrapper = next(
        item
        for item in kwargs["run_ledger"]["intents"]
        if item["intent"]["stage"] == "calibration"
    )
    calibration_wrapper["intent"]["provider_key"]["usage_before_usd"] = 0.02
    calibration_outcome = next(
        item
        for item in kwargs["run_ledger"]["outcomes"]
        if item["intent_sha256"] == calibration_wrapper["intent_sha256"]
    )
    calibration_outcome["provider_usage_before_usd"] = 0.02
    calibration_outcome["provider_usage_after_usd"] = 0.62
    _rehash_ledger_intent(kwargs["run_ledger"], "calibration", receipt_rows=calibration)

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **kwargs,
    )

    assert validation["claim_eligible"] is False
    assert any("not demonstrably pre-run" in reason for reason in validation["reasons"])
    assert any("usage is discontinuous" in reason for reason in validation["reasons"])


@pytest.mark.parametrize("limit_usd", [179.0, 200.0])
def test_run_ledger_rejects_noncanonical_dedicated_key_limit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    limit_usd: float,
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)
    confirmatory = next(
        item
        for item in kwargs["run_ledger"]["intents"]
        if item["intent"]["stage"] == "confirmatory"
    )
    confirmatory["intent"]["provider_key"]["limit_usd"] = limit_usd
    _rehash_ledger_intent(
        kwargs["run_ledger"],
        "confirmatory",
        receipt_rows=rows,
    )

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **kwargs,
    )

    assert validation["scientific_artifact_eligible"] is False
    assert any(
        "Paid intents lack one valid dedicated-key identity/limit snapshot" in reason
        for reason in validation["reasons"]
    )


def test_run_ledger_requires_exact_historical_spend_disclosure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)
    kwargs["run_ledger"]["historical_spend_disclosure"]["known_lower_bound_usd"] = 0.0

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **kwargs,
    )

    assert validation["scientific_artifact_eligible"] is False
    assert any(
        "historical_spend_disclosure must exactly retain" in reason
        for reason in validation["reasons"]
    )


def test_exact_harbor_and_comparator_identity_drift_block_claim(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)
    rows[0]["job_retry_max_retries"] = 1
    rows[0]["task_checksum"] = "0" * 64

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **kwargs,
    )

    assert validation["claim_eligible"] is False
    assert any("retry_max_retries" in reason for reason in validation["reasons"])
    assert any(
        "validated pinned comparator mapping" in reason
        for reason in validation["reasons"]
    )


def test_public_confirmatory_intent_must_precede_execution(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    kwargs = _run_ledger_kwargs(manifest, rows, calibration, excluded)
    publication = next(
        item
        for item in kwargs["run_ledger"]["publications"]
        if item["subject_type"] == "intent"
        and item["subject_id"]
        == next(
            wrapper["intent_sha256"]
            for wrapper in kwargs["run_ledger"]["intents"]
            if wrapper["intent"]["stage"] == "confirmatory"
        )
    )
    publication["published_at"] = "2026-01-01T00:02:02Z"

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=excluded,
        **kwargs,
    )

    assert validation["claim_eligible"] is False
    assert any(
        "not public before execution" in reason for reason in validation["reasons"]
    )


def test_confirmatory_rejects_two_job_stitching(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, ledger, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    for row in rows[len(rows) // 2 :]:
        row["source_input"] = "/claim/resumed-job"
        row["job_id"] = "resumed-job-id"

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )

    assert validation["claim_eligible"] is False
    assert any(
        "exactly one supplied physical Harbor job directory" in reason
        for reason in validation["reasons"]
    )
    assert any(
        "exactly one Harbor job ID" in reason for reason in validation["reasons"]
    )


def test_confirmatory_rejects_duplicate_trial_ids(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, ledger, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    rows[1]["trial_id"] = rows[0]["trial_id"]

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )

    assert validation["claim_eligible"] is False
    assert any(
        "445 non-empty, unique Harbor trial IDs" in reason
        for reason in validation["reasons"]
    )


def test_confirmatory_rejects_missing_instantiated_slot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, ledger, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    rows[0]["instantiated"] = False

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )

    assert validation["claim_eligible"] is False
    assert any(
        "requested, instantiated, and attempted" in reason
        for reason in validation["reasons"]
    )


def test_calibration_accepts_interleaving_but_rejects_task_identity_drift(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, ledger, _ = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    calibration.reverse()
    manifest = _study_manifest(
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )
    valid = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
        **_run_ledger_kwargs(manifest, rows, calibration, ledger),
    )
    assert valid["scientific_artifact_eligible"] is True
    assert valid["claim_eligible"] is False

    calibration[0]["task_ref"] = "sha256:" + "0" * 64
    bad_ref = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )
    assert bad_ref["claim_eligible"] is False
    assert any("task ref differs" in reason for reason in bad_ref["reasons"])

    calibration[0]["task_ref"] = analysis_module.CALIBRATION_TASK_REFS[
        calibration[0]["task"]
    ]
    calibration[0]["task_checksum"] = "0" * 64
    drifted = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )
    assert drifted["claim_eligible"] is False
    assert any("task checksum differs" in reason for reason in drifted["reasons"])


def test_calibration_fractional_reward_is_in_range_but_not_a_pass(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, ledger, _ = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    passing = next(
        row
        for row in calibration
        if row["model"] == CALIBRATION_MODEL_ORDER[0] and row["reward"] == 1.0
    )
    passing["reward"] = 0.5
    passing["accuracy_value"] = 0.5
    manifest = _study_manifest(
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )

    first = validation["calibration_validation"]["ranking"][0]
    assert first["passes"] == 13
    assert first["advances"] is False
    assert validation["claim_eligible"] is False
    assert not any(
        "must have matching verifier reward and accuracy in [0, 1]" in reason
        for reason in validation["reasons"]
    )


def test_calibration_ties_use_usd_then_frozen_order_not_tokens_or_wall(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, ledger, _ = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    first, second = CALIBRATION_MODEL_ORDER[:2]
    for model in (first, second):
        model_rows = [row for row in calibration if row["model"] == model]
        for index, row in enumerate(model_rows):
            reward = float(index < analysis_module.CALIBRATION_MINIMUM_PASSES)
            row["reward"] = reward
            row["accuracy_value"] = reward
    for row in calibration:
        if row["model"] == first:
            row["cost_usd"] = 0.02
            row["accounting_envelope_total_cost_usd"] = 0.02
            row["accounting_step_usage_total_cost_usd"] = 0.02
    cheaper_manifest = _study_manifest(
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )
    cheaper_manifest["calibration"]["selected_model"] = second
    cheaper = validate_study(
        rows,
        cheaper_manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
        **_run_ledger_kwargs(cheaper_manifest, rows, calibration, ledger),
    )
    assert cheaper["calibration_validation"]["observed"]["derived_winner"] == second

    for row in calibration:
        if row["model"] == first:
            row["cost_usd"] = 0.01
            row["accounting_envelope_total_cost_usd"] = 0.01
            row["accounting_step_usage_total_cost_usd"] = 0.01
            row["prompt_tokens"] = 990
            row["token_spend"] = 1_000
            row["accounting_input_tokens"] = 990
            row["agent_wall_seconds"] = 100.0
    frozen_order_manifest = _study_manifest(
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )
    frozen_order_manifest["calibration"]["selected_model"] = first
    frozen_order = validate_study(
        rows,
        frozen_order_manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
        **_run_ledger_kwargs(frozen_order_manifest, rows, calibration, ledger),
    )
    assert frozen_order["calibration_validation"]["observed"]["derived_winner"] == first


def test_study_rejects_non_glm51_primary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rows, comparator, calibration, ledger, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    other = CALIBRATION_MODEL_ORDER[0]
    posture, _, posture_sha256 = analysis_module.canonical_engine_posture(other)
    manifest["sut"]["model"] = other
    manifest["sut"]["allowed_call_models"] = list(
        analysis_module.CALIBRATION_CALL_MODELS[other]
    )
    manifest["sut"]["engine_posture"] = posture
    manifest["sut"]["engine_posture_sha256"] = posture_sha256

    validation = validate_study(
        rows,
        manifest,
        comparator_rows=comparator,
        calibration_rows=calibration,
        calibration_ledger_rows=ledger,
    )

    assert validation["scientific_artifact_eligible"] is False
    assert any(
        "preregistered same-model primary" in reason for reason in validation["reasons"]
    )


def test_study_rejects_mixed_model_and_binary_hash(tmp_path: Path) -> None:
    rows, _ = ingest_job(_write_complete_study_job(tmp_path))
    rows[0]["model"] = "openrouter/other/model"
    rows[0]["binary_sha256"] = "c" * 64
    rows[0]["adapter_sha256"] = "c" * 64

    validation = validate_study(
        rows,
        _study_manifest(),
    )

    assert validation["claim_eligible"] is False
    assert validation["homogeneous"] is False
    assert any(
        "heterogeneous for config model" in reason for reason in validation["reasons"]
    )
    assert any(
        "heterogeneous for binary_sha256" in reason for reason in validation["reasons"]
    )
    assert any(
        "heterogeneous for adapter_sha256" in reason for reason in validation["reasons"]
    )


def test_study_rejects_partial_timeout_telemetry_that_has_numeric_totals(
    tmp_path: Path,
) -> None:
    rows, _ = ingest_job(_write_complete_study_job(tmp_path))
    partial = rows[0]
    assert partial["prompt_tokens"] == 80
    assert partial["cost_usd"] == pytest.approx(0.01)
    partial["accounting_state"] = "incomplete"
    partial["stream_complete"] = False
    partial["stream_status"] = "interrupted"
    partial["stream_terminal_event"] = None
    partial["stream_cost_source"] = "summed_step_usage"
    partial["accounting_cost_consistency"] = "derived_from_step_usage"

    validation = validate_study(rows, _study_manifest())

    assert validation["claim_eligible"] is False
    assert any(
        "accounting state is not complete" in reason for reason in validation["reasons"]
    )
    assert any(
        "stream is incomplete/interrupted" in reason for reason in validation["reasons"]
    )


def test_study_rejects_step_usage_model_drift_and_terminal_cost_mismatch(
    tmp_path: Path,
) -> None:
    rows, _ = ingest_job(_write_complete_study_job(tmp_path))
    rows[0]["accounting_models"] = "openrouter/other/model"
    rows[0]["accounting_cost_consistency"] = "mismatch"

    validation = validate_study(rows, _study_manifest())

    assert validation["claim_eligible"] is False
    assert any("StepUsage models" in reason for reason in validation["reasons"])
    assert any(
        "independent terminal cost" in reason for reason in validation["reasons"]
    )


def test_study_rejects_confirmatory_or_calibration_engine_posture_drift(
    tmp_path: Path,
) -> None:
    rows, _ = ingest_job(_write_complete_study_job(tmp_path))
    calibration_rows = _synthetic_calibration_rows()
    calibration_ledger_rows = _synthetic_calibration_ledger_rows()
    manifest = _study_manifest(
        calibration_rows=calibration_rows,
        calibration_ledger_rows=calibration_ledger_rows,
    )

    rows[0]["engine_posture_json"] = json.dumps(
        {"default_model": _MODEL, "reasoning_auto": "on"},
        sort_keys=True,
        separators=(",", ":"),
    )
    calibration_rows[0]["engine_posture_sha256"] = "f" * 64
    validation = validate_study(
        rows,
        manifest,
        calibration_rows=calibration_rows,
        calibration_ledger_rows=calibration_ledger_rows,
    )

    assert validation["claim_eligible"] is False
    assert any(
        "engine posture normalized JSON" in reason for reason in validation["reasons"]
    )
    assert any(
        "engine_posture_sha256 differs from the registered posture" in reason
        for reason in validation["reasons"]
    )


def test_study_rejects_resource_override(tmp_path: Path) -> None:
    rows, _ = ingest_job(_write_complete_study_job(tmp_path, override_cpus=8))

    validation = validate_study(
        rows,
        _study_manifest(),
    )

    assert validation["claim_eligible"] is False
    assert any("override_cpus" in reason for reason in validation["reasons"])


def test_study_rejects_missing_required_trial_metadata(tmp_path: Path) -> None:
    rows, _ = ingest_job(
        _write_complete_study_job(
            tmp_path,
            drop_metadata_field="stella_adapter_version",
        )
    )

    validation = validate_study(
        rows,
        _study_manifest(),
    )

    assert validation["claim_eligible"] is False
    assert any(
        "adapter version is missing" in reason for reason in validation["reasons"]
    )


def test_study_rejects_environment_credential_source(tmp_path: Path) -> None:
    rows, _ = ingest_job(
        _write_complete_study_job(
            tmp_path,
            drop_metadata_field="stella_host_credential_source",
        )
    )

    validation = validate_study(rows, _study_manifest())

    assert validation["claim_eligible"] is False
    assert any(
        "claim-eligible host credential source" in reason
        for reason in validation["reasons"]
    )


def test_study_rejects_missing_live_container_absence_proof(tmp_path: Path) -> None:
    rows, _ = ingest_job(_write_complete_study_job(tmp_path))
    rows[0]["container_credential_absence_verified"] = False

    validation = validate_study(rows, _study_manifest())

    assert validation["claim_eligible"] is False
    assert any(
        "absent from every live container Config" in reason
        for reason in validation["reasons"]
    )


def test_no_manifest_blocks_claim_but_keeps_descriptive_estimates() -> None:
    stella = _metric_rows(reward=1.0, tokens=80, wall=90.0, product="stella")
    comparator = _metric_rows(
        reward=0.5,
        tokens=100,
        wall=100.0,
        product="comparator",
    )

    result = task_cluster_bootstrap(
        stella,
        comparator,
        seed=123,
        draws=50,
        expected_tasks=2,
        expected_trials_per_task=2,
    )

    assert result["available"] is True
    assert result["wins"] == 2
    assert result["claim_eligible"] is False
    assert result["claim_established"] is False
    assert "manifest" in result["claim_eligibility_reasons"][0]


def test_build_report_blocks_claim_without_manifest(tmp_path: Path) -> None:
    job = _write_complete_study_job(tmp_path)
    comparator = tmp_path / "comparator.json"
    _write_json(
        comparator,
        {
            "entries": [
                {
                    "task_name": f"terminal-bench/{task}",
                    "reward": 0.5,
                    "input_tokens": 100,
                    "cached_input_tokens": 40,
                    "output_tokens": 10,
                    "agent_wall_seconds": 20,
                }
                for task in _full_tasks()[:2]
                for _ in range(2)
            ]
        },
    )

    report = build_report(
        [job],
        comparator_inputs=[comparator],
        draws=50,
        expected_tasks=2,
        expected_trials_per_task=2,
    )

    assert report["bootstrap"]["available"] is True
    assert report["bootstrap"]["wins"] == 2
    assert report["study_validation"]["manifest_supplied"] is False
    assert report["bootstrap"]["claim_established"] is False


def test_build_report_blocks_calibration_selected_non_primary_model(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    rows, comparator, calibration, excluded, manifest = _complete_study_inputs(
        tmp_path, monkeypatch
    )
    non_primary = _CALIBRATION_WINNER
    posture, _, posture_sha256 = analysis_module.canonical_engine_posture(non_primary)
    manifest["sut"]["model"] = non_primary
    manifest["sut"]["allowed_call_models"] = list(
        analysis_module.CALIBRATION_CALL_MODELS[non_primary]
    )
    manifest["sut"]["engine_posture"] = posture
    manifest["sut"]["engine_posture_sha256"] = posture_sha256
    manifest_path = tmp_path / "non-primary-study-manifest.json"
    _write_json(manifest_path, manifest)

    def fake_ingest_jobs(
        job_dirs: object,
        *,
        product: str = "stella",
    ) -> tuple[list[dict], list[str]]:
        del job_dirs
        if product == "stella":
            return rows, []
        if product == "calibration":
            return calibration, []
        if product == "calibration_excluded":
            return excluded, []
        raise AssertionError(f"unexpected product: {product}")

    monkeypatch.setattr(analysis_module, "ingest_jobs", fake_ingest_jobs)
    monkeypatch.setattr(
        analysis_module,
        "load_comparator_inputs",
        lambda paths: (comparator, []),
    )
    monkeypatch.setattr(
        analysis_module,
        "_bootstrap_metric",
        lambda *args, **kwargs: (0.2, 0.2),
    )

    report = build_report(
        [tmp_path / "confirmatory-job"],
        comparator_inputs=[tmp_path / "comparator.json"],
        calibration_job_dirs=[tmp_path / "calibration-job"],
        calibration_ledger_job_dirs=[tmp_path / "excluded-job"],
        study_manifest=manifest_path,
    )

    assert report["study_validation"]["scientific_artifact_eligible"] is False
    assert report["bootstrap"]["claim_scope"]["stella"]["model"] == non_primary
    assert report["bootstrap"]["claim_scope"]["same_model"] is False
    assert report["bootstrap"]["claim_established"] is False
    assert any(
        "preregistered same-model primary" in reason
        for reason in report["bootstrap"]["claim_eligibility_reasons"]
    )


def test_fixed_seed_bootstrap_establishes_two_eligible_wins() -> None:
    stella = _metric_rows(reward=1.0, tokens=80, wall=90.0, product="stella")
    comparator = _metric_rows(reward=0.5, tokens=100, wall=100.0, product="comparator")

    first = task_cluster_bootstrap(
        stella,
        comparator,
        seed=123,
        draws=500,
        expected_tasks=2,
        expected_trials_per_task=2,
        claim_eligibility_reasons=[],
    )
    second = task_cluster_bootstrap(
        stella,
        comparator,
        seed=123,
        draws=500,
        expected_tasks=2,
        expected_trials_per_task=2,
        claim_eligibility_reasons=[],
    )

    assert first == second
    assert first["available"] is True
    assert first["dimensions"]["accuracy"]["lower_confidence_bound"] == 1.0
    assert first["dimensions"]["tokens"]["lower_confidence_bound"] == pytest.approx(0.2)
    assert first["dimensions"]["wall_clock"]["eligible"] is False
    assert first["dimensions"]["wall_clock"]["win"] is False
    assert first["wins"] == 2
    assert first["claim_eligible"] is False
    assert first["claim_established"] is False
    assert any(
        "Custom settings are descriptive only" in reason
        for reason in first["claim_eligibility_reasons"]
    )


def test_cross_model_tokens_are_explicitly_ineligible() -> None:
    stella = _metric_rows(reward=1.0, tokens=50, wall=90.0, product="stella")
    comparator = _metric_rows(reward=0.5, tokens=100, wall=100.0, product="comparator")

    result = task_cluster_bootstrap(
        stella,
        comparator,
        seed=123,
        draws=500,
        expected_tasks=2,
        expected_trials_per_task=2,
        claim_eligibility_reasons=[],
        public_timing_verified=True,
        stella_model="openrouter/deepseek/deepseek-v4-pro",
        stella_route_policy="openrouter-auto",
    )

    assert result["claim_scope"]["comparator"] == {
        "agent": "claude-code",
        "agent_version": "2.1.123",
        "model": "glm-5.1",
        "reasoning_effort": "max",
        "public_job_id": analysis_module.COMPARATOR_PUBLIC_JOB_ID,
        "evidence_status": "historical public reviewed leaderboard job",
    }
    assert result["claim_scope"]["cross_model_tokenizer_confounding"] is True
    assert result["dimensions"]["tokens"]["available"] is True
    assert result["dimensions"]["tokens"]["eligible"] is False
    assert result["dimensions"]["tokens"]["win"] is False
    assert (
        "different model/tokenizer"
        in result["dimensions"]["tokens"]["eligibility_note"]
    )


def test_full_89_point_uses_calibration_tasks_but_lcb_uses_only_79() -> None:
    stella: list[dict] = []
    comparator: list[dict] = []
    for task in _full_tasks():
        for _ in range(5):
            comparator.append(
                {
                    "task": task,
                    "attempted": True,
                    "accuracy_value": 0.5,
                    "token_spend": 100,
                    "agent_wall_seconds": 100.0,
                }
            )
            stella.append(
                {
                    "task": task,
                    "attempted": True,
                    "accuracy_value": 1.0 if task in CALIBRATION_TASKS else 0.5,
                    "token_spend": 100,
                    "agent_wall_seconds": 100.0,
                }
            )

    result = task_cluster_bootstrap(
        stella,
        comparator,
        draws=100,
        expected_tasks=89,
        expected_trials_per_task=5,
        claim_eligibility_reasons=[],
    )

    accuracy = result["dimensions"]["accuracy"]
    assert result["observed_inference_tasks"] == 79
    assert accuracy["point_relative_improvement"] == pytest.approx(10 / 89)
    assert accuracy["point_meets_threshold"] is True
    assert accuracy["inferential_point_relative_improvement"] == 0.0
    assert accuracy["lower_confidence_bound"] == 0.0
    assert accuracy["win"] is False


def test_offline_two_win_result_fails_closed_without_external_timing_audit(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        analysis_module,
        "_bootstrap_metric",
        lambda *args, **kwargs: (0.2, 0.2),
    )
    stella: list[dict] = []
    comparator: list[dict] = []
    for task in _full_tasks():
        for _ in range(5):
            stella.append(
                {
                    "task": task,
                    "attempted": True,
                    "accuracy_value": 1.0,
                    "token_spend": 80,
                    "agent_wall_seconds": 80.0,
                }
            )
            comparator.append(
                {
                    "task": task,
                    "attempted": True,
                    "accuracy_value": 0.5,
                    "token_spend": 100,
                    "agent_wall_seconds": 100.0,
                }
            )

    result = task_cluster_bootstrap(
        stella,
        comparator,
        claim_eligibility_reasons=[],
        stella_model=_MODEL,
        stella_route_policy=CANONICAL_PROVIDER_ROUTE_POLICY,
    )

    assert result["wins"] == 2
    assert result["statistical_design_artifact_established"] is True
    assert result["public_timing_verified"] is False
    assert result["claim_eligible"] is False
    assert result["claim_established"] is False
    assert (
        analysis_module.EXTERNAL_PUBLIC_TIMING_AUDIT_REASON
        in (result["claim_eligibility_reasons"])
    )


def test_live_verified_timing_can_establish_two_win_claim(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        analysis_module,
        "_bootstrap_metric",
        lambda *args, **kwargs: (0.2, 0.2),
    )
    stella: list[dict] = []
    comparator: list[dict] = []
    for task in _full_tasks():
        for _ in range(5):
            stella.append(
                {
                    "task": task,
                    "attempted": True,
                    "accuracy_value": 1.0,
                    "token_spend": 80,
                    "agent_wall_seconds": 80.0,
                }
            )
            comparator.append(
                {
                    "task": task,
                    "attempted": True,
                    "accuracy_value": 0.5,
                    "token_spend": 100,
                    "agent_wall_seconds": 100.0,
                }
            )

    result = task_cluster_bootstrap(
        stella,
        comparator,
        claim_eligibility_reasons=[],
        public_timing_verified=True,
        stella_model=_MODEL,
        stella_route_policy=CANONICAL_PROVIDER_ROUTE_POLICY,
    )

    assert result["wins"] == 2
    assert result["public_timing_verified"] is True
    assert result["external_public_timing_audit_required"] is False
    assert result["claim_eligible"] is True
    assert result["claim_established"] is True
    assert result["claim_eligibility_reasons"] == []


def test_bootstrap_rejects_non_primary_model_even_with_glm_51_suffix(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        analysis_module,
        "_bootstrap_metric",
        lambda *args, **kwargs: (0.2, 0.2),
    )
    stella: list[dict] = []
    comparator: list[dict] = []
    for task in _full_tasks():
        for _ in range(5):
            stella.append(
                {
                    "task": task,
                    "attempted": True,
                    "accuracy_value": 1.0,
                    "token_spend": 80,
                    "agent_wall_seconds": 80.0,
                }
            )
            comparator.append(
                {
                    "task": task,
                    "attempted": True,
                    "accuracy_value": 0.5,
                    "token_spend": 100,
                    "agent_wall_seconds": 100.0,
                }
            )

    result = task_cluster_bootstrap(
        stella,
        comparator,
        claim_eligibility_reasons=[],
        public_timing_verified=True,
        stella_model="openrouter/other/glm-5.1",
        stella_route_policy=CANONICAL_PROVIDER_ROUTE_POLICY,
    )

    assert result["wins"] == 1
    assert result["claim_scope"]["same_model"] is False
    assert result["claim_eligible"] is False
    assert result["claim_established"] is False
    assert result["claim_eligibility_reasons"] == [
        "Claim mode requires the frozen primary Stella model exactly: "
        f"{_MODEL!r}; observed 'openrouter/other/glm-5.1'."
    ]


def test_exact_registered_point_thresholds() -> None:
    assert analysis_module.PRIMARY_MODEL == "openrouter/z-ai/glm-5.1"
    assert CALIBRATION_MODEL_ORDER == (
        "openrouter/deepseek/deepseek-v4-pro",
        "openrouter/z-ai/glm-5.2",
        "openrouter/x-ai/grok-4.5",
    )
    assert analysis_module.CALIBRATION_CALL_MODELS == {
        "openrouter/deepseek/deepseek-v4-pro": ("deepseek/deepseek-v4-pro",),
        "openrouter/z-ai/glm-5.2": ("z-ai/glm-5.2",),
        "openrouter/x-ai/grok-4.5": ("x-ai/grok-4.5",),
    }
    assert analysis_module.CALIBRATION_MINIMUM_PASSES == 14
    assert analysis_module.DEDICATED_KEY_LIMIT_USD == 180.0
    assert pytest.approx((261 / 445) * 1.1) == (
        analysis_module.ACCURACY_POINT_THRESHOLD
    )
    assert pytest.approx(0.6451685393258427) == (
        analysis_module.ACCURACY_POINT_THRESHOLD
    )
    assert analysis_module.ACCURACY_MIN_BINARY_PASSES == 288
    assert analysis_module.TOKEN_POINT_THRESHOLD_MAX == 358_905_384

    accuracy_comparator = {"task": [1.0] * 261 + [0.0] * (445 - 261)}
    accuracy_288 = {"task": [1.0] * 288 + [0.0] * (445 - 288)}
    accuracy_287 = {"task": [1.0] * 287 + [0.0] * (445 - 287)}
    point_288, _ = analysis_module._bootstrap_metric(
        accuracy_288,
        accuracy_comparator,
        lower_is_better=False,
        seed=20260721,
        draws=5,
    )
    point_287, _ = analysis_module._bootstrap_metric(
        accuracy_287,
        accuracy_comparator,
        lower_is_better=False,
        seed=20260721,
        draws=5,
    )
    assert point_288 > 0.10
    assert point_287 < 0.10

    token_comparator = {"task": [398_783_761.0]}
    token_at_limit = {"task": [358_905_384.0]}
    token_over_limit = {"task": [358_905_385.0]}
    point_at_limit, _ = analysis_module._bootstrap_metric(
        token_at_limit,
        token_comparator,
        lower_is_better=True,
        seed=20260721,
        draws=5,
    )
    point_over_limit, _ = analysis_module._bootstrap_metric(
        token_over_limit,
        token_comparator,
        lower_is_better=True,
        seed=20260721,
        draws=5,
    )
    assert point_at_limit > 0.10
    assert point_over_limit < 0.10


def test_point_below_threshold_cannot_win_even_with_high_lcb(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        analysis_module,
        "_bootstrap_metric",
        lambda *args, **kwargs: (0.09, 0.20),
    )
    monkeypatch.setattr(
        analysis_module,
        "_metric_point",
        lambda *args, **kwargs: 0.09,
    )
    stella = _metric_rows(reward=1.0, tokens=80, wall=90.0, product="stella")
    comparator = _metric_rows(
        reward=0.5,
        tokens=100,
        wall=100.0,
        product="comparator",
    )

    result = task_cluster_bootstrap(
        stella,
        comparator,
        seed=123,
        draws=10,
        expected_tasks=2,
        expected_trials_per_task=2,
        claim_eligibility_reasons=[],
    )

    for dimension in result["dimensions"].values():
        assert dimension["point_meets_threshold"] is False
        assert dimension["lower_bound_exceeds_threshold"] is True
        assert dimension["win"] is False


def test_bootstrap_refuses_aggregate_or_incomplete_design() -> None:
    stella = _metric_rows(reward=1.0, tokens=80, wall=90.0, product="stella")
    no_comparator = task_cluster_bootstrap(
        stella,
        [],
        draws=10,
        expected_tasks=2,
        expected_trials_per_task=2,
    )
    assert no_comparator["available"] is False
    assert "per-task" in no_comparator["unavailable_reasons"][0]

    incomplete = _metric_rows(reward=0.5, tokens=100, wall=100.0, product="comparator")[
        :-1
    ]
    result = task_cluster_bootstrap(
        stella,
        incomplete,
        draws=10,
        expected_tasks=2,
        expected_trials_per_task=2,
    )
    assert result["available"] is False
    assert any("exactly 2" in reason for reason in result["unavailable_reasons"])


def test_build_report_writes_comparator_unavailable_explicitly(tmp_path: Path) -> None:
    job = tmp_path / "empty-job"
    _write_json(job / "config.json", _job_config(["requested-only"]))

    report = build_report([job], draws=10)

    assert report["summary"]["requested_slots"] == 1
    assert report["summary"]["instantiated_trials"] == 0
    assert report["bootstrap"]["available"] is False
    assert (
        "aggregate leaderboard totals" in report["bootstrap"]["unavailable_reasons"][0]
    )

    output = tmp_path / "output"
    write_outputs(report, output)
    assert (output / "trials.csv").is_file()
    assert json.loads((output / "report.json").read_text())["schema_version"] == (
        "stella-tb21-analysis-v1"
    )
    assert "Comparator per-task" in (output / "report.md").read_text()


def test_comparator_csv_numeric_fields_are_parsed(tmp_path: Path) -> None:
    comparator = tmp_path / "comparator.csv"
    with comparator.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(
            handle,
            fieldnames=[
                "task",
                "reward",
                "prompt_tokens",
                "completion_tokens",
                "cache_tokens",
                "cost_usd",
                "agent_wall_seconds",
            ],
        )
        writer.writeheader()
        writer.writerow(
            {
                "task": "terminal-bench/example",
                "reward": "1",
                "prompt_tokens": "100",
                "completion_tokens": "20",
                "cache_tokens": "80",
                "cost_usd": "0.125",
                "agent_wall_seconds": "4.5",
            }
        )

    rows, warnings = load_comparator_inputs([comparator])

    assert warnings == []
    assert rows[0]["task"] == "example"
    assert rows[0]["reward"] == 1
    assert rows[0]["token_spend"] == 120
    assert rows[0]["cache_tokens"] == 80
    assert rows[0]["cost_usd"] == pytest.approx(0.125)
    assert rows[0]["agent_wall_seconds"] == pytest.approx(4.5)


def test_public_comparator_manifest_entries_are_parsed(tmp_path: Path) -> None:
    comparator = tmp_path / "manifest.json"
    _write_json(
        comparator,
        {
            "entries": [
                {
                    "task_name": "terminal-bench/example",
                    "reward": 0.0,
                    "input_tokens": 3_309_949,
                    "cached_input_tokens": 3_243_968,
                    "output_tokens": 12_992,
                    "cost_usd": 2.321449,
                    "error_type": None,
                },
                {
                    "task_name": "terminal-bench/timeout-but-correct",
                    "reward": 1.0,
                    "input_tokens": 100,
                    "cached_input_tokens": 80,
                    "output_tokens": 20,
                    "cost_usd": None,
                    "error_type": "AgentTimeoutError",
                },
            ]
        },
    )

    rows, warnings = load_comparator_inputs([comparator])

    assert warnings == []
    assert rows[0]["task"] == "example"
    assert rows[0]["prompt_tokens"] == 3_309_949
    assert rows[0]["cache_tokens"] == 3_243_968
    assert rows[0]["token_spend"] == 3_322_941
    assert rows[1]["exception_type"] == "AgentTimeoutError"
    assert rows[1]["accuracy_value"] == 1.0
