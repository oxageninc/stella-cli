"""Credential-bundle and secure Harbor launcher tests (no Docker/network)."""

from __future__ import annotations

import hashlib
import json
import os
import stat
from concurrent.futures import ThreadPoolExecutor
from copy import deepcopy
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

pytest.importorskip("harbor", reason="Harbor is required to import the package")

import stella_harbor  # noqa: E402
import stella_harbor.host_attestation as host_module  # noqa: E402
import stella_harbor.secure_launcher as launcher_module  # noqa: E402
from stella_harbor.credential_bundle import (  # noqa: E402
    HOST_CREDENTIAL_BUNDLE_FD_ENV,
    create_anonymous_credential_bundle,
    read_anonymous_credential_bundle,
    sanitized_harbor_environment,
)
from stella_harbor.secure_launcher import (  # noqa: E402
    LAUNCH_RECEIPT_CONTROLS,
    LAUNCH_RECEIPT_FILENAME,
    LAUNCH_RECEIPT_SCHEMA,
    PUBLIC_INTENT_ATTESTATION_FIELDS,
    _AnonymousGitHubPublicIntentReader,
    _binary_version_source_commits,
    _canonical_payload_sha256,
    _canonical_readiness_path,
    _claim_job_destination,
    _claim_options,
    _harbor_command,
    _intent_sha256,
    _isolated_harbor_command,
    _literal_model_arguments,
    _OpenRouterProviderKeyReader,
    _reserve_fresh_job,
    _resolve_harbor_executable,
    _validate_claim_command,
    _validate_claim_environment,
    _validate_public_intent_ledger,
    _validate_stage_shape,
    _validated_runtime_identity,
    _verify_public_intent,
    exec_harbor_securely,
    mark_only_bundle_fd_inheritable,
)

_TEST_INTENT_SHA256 = "a" * 64
_TEST_SOURCE_COMMIT = "d" * 40
_TEST_COMMENT_URL = "https://github.com/macanderson/stella/issues/123#issuecomment-456"
_TEST_MANAGEMENT_CREDENTIAL = "test-management-secret"
_CANONICAL_DATASET = (
    "terminal-bench/terminal-bench-2-1@"
    "sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a"
)
_PRIMARY_MODEL = "openrouter/z-ai/glm-5.1"
_CANDIDATE_MODELS = [
    "openrouter/deepseek/deepseek-v4-pro",
    "openrouter/z-ai/glm-5.2",
    "openrouter/x-ai/grok-4.5",
]
_CALIBRATION_FILTERS = [
    "terminal-bench/fix-git",
    "terminal-bench/filter-js-from-html",
    "terminal-bench/kv-store-grpc",
    "terminal-bench/large-scale-text-editing",
    "terminal-bench/regex-log",
    "terminal-bench/schemelike-metacircular-eval",
    "terminal-bench/sqlite-with-gcov",
    "terminal-bench/bn-fit-modify",
    "terminal-bench/make-mips-interpreter",
    "terminal-bench/train-fasttext",
]
_REAL_PRIOR_STAGE_REPLAY = launcher_module._replay_prior_stage_evidence


def _host_snapshot(captured_at: datetime, jobs_dir: Path) -> dict[str, object]:
    return {
        "captured_at_utc": captured_at.astimezone(timezone.utc)
        .isoformat(timespec="microseconds")
        .replace("+00:00", "Z"),
        "host_fingerprint_sha256": "4" * 64,
        "observed": {
            "os": {
                "system": "Linux",
                "kernel_release": "6.8.0-test",
                "distribution_id": "ubuntu",
                "distribution_version_id": "24.04",
                "distribution_pretty_name": "Ubuntu 24.04 LTS",
            },
            "architecture": "x86_64",
            "cpu": {"effective_vcpus": 8, "model": "Test Xeon"},
            "memory": {"total_bytes": 64 * 1024**3},
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
        },
        "checks": {
            "native_linux_x86_64": True,
            "minimum_vcpus": True,
            "minimum_memory": True,
            "minimum_free_disk": True,
            "docker_native_linux_x86_64": True,
            "zero_running_containers": True,
            "all_passed": True,
        },
    }


def _fake_host_probe(*, jobs_dir: Path, docker_executable: Path) -> dict[str, object]:
    assert docker_executable.is_absolute()
    return _host_snapshot(datetime.now(timezone.utc), jobs_dir)


@pytest.fixture(autouse=True)
def _isolate_global_launch_reservations(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    reservation_root = tmp_path / "launcher-global-reservations"
    monkeypatch.setattr(
        "stella_harbor.secure_launcher._launcher_reservation_root",
        lambda: reservation_root,
    )
    monkeypatch.setattr(
        "stella_harbor.secure_launcher._replay_prior_stage_evidence",
        lambda *_args, **_kwargs: None,
    )
    monkeypatch.setattr(
        "stella_harbor.secure_launcher.probe_host",
        _fake_host_probe,
    )


def _write_claim_elf(path: Path, source_commit: str = _TEST_SOURCE_COMMIT) -> Path:
    header = bytearray(64)
    header[:7] = b"\x7fELF\x02\x01\x01"
    header[18:20] = (62).to_bytes(2, "little")
    path.write_bytes(
        bytes(header) + f"stella 0.4.47-dev.{source_commit}\0".encode("ascii")
    )
    path.chmod(0o755)
    return path


def _write_runtime_assembled_claim_elf(
    path: Path, source_commit: str = _TEST_SOURCE_COMMIT
) -> Path:
    """Model the old Rust binary, whose version pieces were separate literals."""
    header = bytearray(64)
    header[:7] = b"\x7fELF\x02\x01\x01"
    header[18:20] = (62).to_bytes(2, "little")
    path.write_bytes(
        bytes(header)
        + b"0.4.49-dev.\0"
        + source_commit.encode("ascii")
        + b"\0stella v\0"
    )
    path.chmod(0o755)
    return path


def _claim_environment(binary: Path, **overrides: str) -> dict[str, str]:
    environment = {
        "OPENROUTER_API_KEY": "test-secret",
        "OPENROUTER_MANAGEMENT_API_KEY": _TEST_MANAGEMENT_CREDENTIAL,
        "STELLA_BINARY": str(binary),
        "STELLA_SOURCE_COMMIT": _TEST_SOURCE_COMMIT,
        "STELLA_BUDGET": "0.17",
        "STELLA_DISABLE_REFLECTION": "1",
    }
    environment.update(overrides)
    return environment


def _readiness_command(jobs_dir: Path) -> list[str]:
    return [
        "harbor",
        "run",
        "--env",
        "docker",
        "--path",
        str(_canonical_readiness_path()),
        "--agent-import-path",
        "stella_harbor:StellaAgent",
        "--model",
        _CANDIDATE_MODELS[0],
        "--job-name",
        "stella-readiness-synthetic-v1",
        "--jobs-dir",
        str(jobs_dir),
        "--intent-sha256",
        _TEST_INTENT_SHA256,
        "--intent-comment-url",
        _TEST_COMMENT_URL,
        "--n-attempts",
        "1",
        "--n-concurrent",
        "1",
        "--max-retries",
        "0",
    ]


def _calibration_command(jobs_dir: Path) -> list[str]:
    command = [
        "harbor",
        "run",
        "--env",
        "docker",
        "--dataset",
        _CANONICAL_DATASET,
    ]
    for task in _CALIBRATION_FILTERS:
        command.extend(("--include-task-name", task))
    command.extend(("--agent-import-path", "stella_harbor:StellaAgent"))
    for model in _CANDIDATE_MODELS:
        command.extend(("--model", model))
    command.extend(
        (
            "--job-name",
            "stella-tb21-calibration-20260721",
            "--jobs-dir",
            str(jobs_dir),
            "--intent-sha256",
            _TEST_INTENT_SHA256,
            "--intent-comment-url",
            _TEST_COMMENT_URL,
            "--n-attempts",
            "2",
            "--n-concurrent",
            "3",
            "--max-retries",
            "0",
        )
    )
    return command


def _confirmatory_command(jobs_dir: Path, job_name: str) -> list[str]:
    return [
        "harbor",
        "run",
        "--env",
        "docker",
        "--dataset",
        _CANONICAL_DATASET,
        "--agent-import-path",
        "stella_harbor:StellaAgent",
        "--model",
        _PRIMARY_MODEL,
        "--job-name",
        job_name,
        "--jobs-dir",
        str(jobs_dir),
        "--intent-sha256",
        _TEST_INTENT_SHA256,
        "--intent-comment-url",
        _TEST_COMMENT_URL,
        "--n-attempts",
        "5",
        "--n-concurrent",
        "1",
        "--max-retries",
        "0",
    ]


class _FakeProviderKeyReader:
    def __init__(self, *, usage: float = 0.0, credits: float = 200.0) -> None:
        self.key = {
            "data": {
                "byok_usage": 0.0,
                "byok_usage_daily": 0.0,
                "byok_usage_monthly": 0.0,
                "byok_usage_weekly": 0.0,
                "creator_user_id": "user_test",
                "include_byok_in_limit": True,
                "is_free_tier": False,
                "is_management_key": False,
                "label": "sk-or-v1-tes...cret",
                "limit": 180.0,
                "limit_remaining": 180.0 - usage,
                "limit_reset": None,
                "usage": usage,
                "usage_daily": usage,
                "usage_monthly": usage,
                "usage_weekly": usage,
                "is_provisioning_key": False,
                "rate_limit": {
                    "interval": "1h",
                    "note": "This field is deprecated and safe to ignore.",
                    "requests": 1000,
                },
                "expires_at": None,
            }
        }
        self.key_record = {
            "data": {
                "byok_usage": 0.0,
                "byok_usage_daily": 0.0,
                "byok_usage_monthly": 0.0,
                "byok_usage_weekly": 0.0,
                "created_at": "2026-07-21T00:00:00Z",
                "creator_user_id": "user_test",
                "disabled": False,
                "hash": "",
                "include_byok_in_limit": True,
                "label": "sk-or-v1-tes...cret",
                "limit": 180.0,
                "limit_remaining": 180.0 - usage,
                "limit_reset": None,
                "name": "stella-tb21-dedicated-key-v1",
                "updated_at": "2026-07-21T00:00:00Z",
                "usage": usage,
                "usage_daily": usage,
                "usage_monthly": usage,
                "usage_weekly": usage,
                "workspace_id": "0df9e665-d932-5740-b2c7-b52af166bc11",
                "expires_at": None,
            }
        }
        self.credits = {"data": {"total_credits": credits, "total_usage": usage}}

    def get_key(self, credential: str) -> dict[str, object]:
        assert credential
        return deepcopy(self.key)

    def get_key_record(self, credential: str, fingerprint: str) -> dict[str, object]:
        assert credential
        response = deepcopy(self.key_record)
        response["data"]["hash"] = fingerprint
        return response

    def get_credits(self, credential: str) -> dict[str, object]:
        assert credential
        return deepcopy(self.credits)


class _BoundProviderKeyReader(_FakeProviderKeyReader):
    def __init__(self, benchmark_credential: str, management_credential: str) -> None:
        super().__init__()
        self.benchmark_credential = benchmark_credential
        self.management_credential = management_credential
        self.fingerprint = hashlib.sha256(benchmark_credential.encode()).hexdigest()
        self.key_record["data"]["hash"] = self.fingerprint
        self.calls: list[tuple[str, ...]] = []

    def get_key(self, credential: str) -> dict[str, object]:
        assert credential == self.benchmark_credential
        self.calls.append(("key", credential))
        return deepcopy(self.key)

    def get_key_record(self, credential: str, fingerprint: str) -> dict[str, object]:
        assert credential == self.management_credential
        assert fingerprint == self.fingerprint
        self.calls.append(("key_record", credential, fingerprint))
        return deepcopy(self.key_record)

    def get_credits(self, credential: str) -> dict[str, object]:
        assert credential == self.management_credential
        self.calls.append(("credits", credential))
        return deepcopy(self.credits)


class _FakePublicIntentReader:
    def __init__(self, command: list[str], runtime_identity: dict[str, object]) -> None:
        options = _claim_options(command)
        job_name = options["--job-name"][0]
        if job_name == "stella-readiness-synthetic-v1":
            stage = "readiness"
        elif job_name == "stella-tb21-calibration-20260721":
            stage = "calibration"
        else:
            stage = "confirmatory"
        self.job_name = job_name
        self.stage = stage
        self.jobs_dir = Path(options["--jobs-dir"][0])
        source_commit = str(runtime_identity["source_commit"])
        self.subject_commit = source_commit if stage != "confirmatory" else "b" * 40
        self.ledger_commit = "c" * 40
        self.publication_commit = "f" * 40
        self.repository_root = Path(__file__).resolve().parents[3]

        def make_intent(intent_stage: str, preregistration_commit: str) -> dict:
            if intent_stage == "readiness":
                models = [_CANDIDATE_MODELS[0]]
                prior_job_name = "stella-readiness-synthetic-v1"
                dataset = {
                    "name": prior_job_name,
                    "ref": (
                        "sha256:05a040c7df0fd77f66f533ba023cb5f16e2dd0f89957440b099374210e475ad6"
                    ),
                    "task_count": 1,
                    "task_set_sha256": (
                        "2020954593c84785eec3b16817beefa84480aa05e0ba38ad88f31d87347e39eb"
                    ),
                }
                requested, attempts, concurrency = 1, 1, 1
                declared_at = "2025-12-31T00:00:10Z"
            elif intent_stage == "calibration":
                models = list(_CANDIDATE_MODELS)
                prior_job_name = "stella-tb21-calibration-20260721"
                dataset = {
                    "name": "terminal-bench/terminal-bench-2-1",
                    "ref": _CANONICAL_DATASET.partition("@")[2],
                    "task_count": 10,
                    "task_set_sha256": (
                        "61a065631b2afe551ade7504bab7f15b222b099c8fec1fbbfdc3f99ef5baeb46"
                    ),
                }
                requested, attempts, concurrency = 60, 2, 3
                declared_at = "2025-12-31T00:01:10Z"
            else:
                models = list(options["--model"])
                prior_job_name = job_name
                dataset = {
                    "name": "terminal-bench/terminal-bench-2-1",
                    "ref": _CANONICAL_DATASET.partition("@")[2],
                    "task_count": 89,
                    "task_set_sha256": launcher_module._CONFIRMATORY_TASK_SET_SHA256,
                }
                requested, attempts, concurrency = 445, 5, 1
                declared_at = "2025-12-31T00:02:10Z"
            artifacts = {
                key: runtime_identity[key]
                for key in (
                    "binary_sha256",
                    "source_commit",
                    "agent_version",
                    "adapter_version",
                    "adapter_sha256",
                    "analysis_sha256",
                    "public_timing_sha256",
                    "harbor_version",
                    "harbor_sha256",
                )
            }
            all_postures = runtime_identity["engine_posture_sha256_by_model"]
            assert isinstance(all_postures, dict)
            artifacts["engine_posture_sha256_by_model"] = {
                model: all_postures.get(model)
                or stella_harbor._benchmark_engine_posture(model)[2]
                for model in models
            }
            return {
                "intent_id": f"stella-tb21-{intent_stage}-intent-v1",
                "stage": intent_stage,
                "historical": False,
                "job_name": prior_job_name,
                "models": models,
                "dataset": dataset,
                "requested_trials": requested,
                "attempts_per_task": attempts,
                "n_concurrent_trials": concurrency,
                "retry_max_retries": 0,
                "per_trial_budget_usd": 0.17,
                "artifacts": artifacts,
                "execution": {
                    "base_url": "https://openrouter.ai/api/v1",
                    "provider_route_policy": "openrouter-auto",
                    "disable_reflection": True,
                },
                "provider_key": {
                    "fingerprint_sha256": runtime_identity[
                        "provider_key_fingerprint_sha256"
                    ],
                    "label": "stella-tb21-dedicated-key-v1",
                    "limit_usd": 180.0,
                    "usage_before_usd": 0.0,
                    "snapshot_at": declared_at.replace("10Z", "09Z"),
                },
                "declared_at": declared_at,
                "preregistration_commit": preregistration_commit,
            }

        readiness = make_intent("readiness", source_commit)
        calibration = make_intent("calibration", source_commit)
        current_by_stage = {
            "readiness": readiness,
            "calibration": calibration,
            "confirmatory": make_intent("confirmatory", self.subject_commit),
        }
        self.intent = current_by_stage[stage]
        readiness_digest = _canonical_payload_sha256(readiness)
        calibration_digest = _canonical_payload_sha256(calibration)
        self.intent_sha256 = _canonical_payload_sha256(self.intent)
        command[command.index("--intent-sha256") + 1] = self.intent_sha256
        host_report = host_module.build_public_host_report(
            intent_sha256=self.intent_sha256,
            stage=stage,
            job_name=job_name,
            snapshot=_host_snapshot(
                datetime.now(timezone.utc) - timedelta(seconds=1),
                self.jobs_dir,
            ),
        )
        self.host_report = host_module.canonical_json_bytes(host_report)
        self.host_report_fetches: list[tuple[str, str]] = []
        self.repository = {
            "full_name": "macanderson/stella",
            "url": "https://api.github.com/repos/macanderson/stella",
            "html_url": "https://github.com/macanderson/stella",
            "private": False,
            "default_branch": "main",
        }
        self.issue = {
            "number": 123,
            "url": "https://api.github.com/repos/macanderson/stella/issues/123",
            "html_url": "https://github.com/macanderson/stella/issues/123",
            "repository_url": "https://api.github.com/repos/macanderson/stella",
            "title": (
                "Stella Terminal-Bench 2.1 preregistration: "
                "stella-tb21-scientific-study-v1"
            ),
            "user": {"login": "macanderson"},
            "author_association": "OWNER",
        }
        body = {
            "schema_version": "stella-tb21-github-attestation-v2",
            "study_id": "stella-tb21-scientific-study-v1",
            "subject_type": "intent",
            "subject_id": self.intent_sha256,
            "kind": stage,
            "subject_commit": self.subject_commit,
            "ledger_commit": self.ledger_commit,
            "ledger_path": "bench/evidence/stella-tb21-run-ledger.json",
            "intent_sha256": self.intent_sha256,
        }
        self.comment = {
            "id": 456,
            "url": (
                "https://api.github.com/repos/macanderson/stella/issues/comments/456"
            ),
            "html_url": _TEST_COMMENT_URL,
            "issue_url": ("https://api.github.com/repos/macanderson/stella/issues/123"),
            "user": {"login": "macanderson"},
            "author_association": "OWNER",
            "body": json.dumps(body, sort_keys=True, separators=(",", ":")),
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }
        preregistrations: list[dict[str, object]] = [
            {
                "sequence": 1,
                "kind": "readiness",
                "commit": source_commit,
                "study_manifest_sha256": None,
                "declared_at": "2025-12-30T23:59:50Z",
            }
        ]
        intents = [
            {"sequence": 3, "intent": readiness, "intent_sha256": readiness_digest}
        ]
        outcomes: list[dict[str, object]] = []
        publications: list[dict[str, object]] = [
            {
                "sequence": 2,
                "subject_type": "preregistration",
                "subject_id": "readiness",
                "ledger_commit": "a" * 40,
                "public_url": (
                    f"https://github.com/macanderson/stella/commit/{'a' * 40}"
                ),
                "published_at": "2025-12-31T00:00:00Z",
            }
        ]

        def publication(
            sequence: int,
            subject_type: str,
            subject_id: str,
            ledger_sha: str,
            published_at: str,
        ) -> dict[str, object]:
            return {
                "sequence": sequence,
                "subject_type": subject_type,
                "subject_id": subject_id,
                "ledger_commit": ledger_sha,
                "public_url": (
                    f"https://github.com/macanderson/stella/commit/{ledger_sha}"
                ),
                "published_at": published_at,
            }

        def outcome(sequence: int, digest: str, status: str, minute: int) -> dict:
            return {
                "sequence": sequence,
                "intent_sha256": digest,
                "job_id": f"00000000-0000-0000-0000-{sequence:012d}",
                "status": status,
                "started_at": f"2025-12-31T00:0{minute}:20Z",
                "completed_at": f"2025-12-31T00:0{minute}:30Z",
                "artifact_tree_sha256": "e" * 64,
                "provider_usage_before_usd": 0.0,
                "provider_usage_after_usd": 0.0,
                "provider_usage_delta_usd": 0.0,
                "telemetry_cost_sum_usd": 0.0,
                "reconciliation_status": "reconciled",
                "reconciliation_tolerance_usd": 0.000001,
                "recorded_at": f"2025-12-31T00:0{minute}:40Z",
            }

        readiness_publication_time = (
            "2026-01-01T00:00:00Z" if stage == "readiness" else "2025-12-31T00:00:12Z"
        )
        publications.append(
            publication(
                4,
                "intent",
                readiness_digest,
                self.ledger_commit if stage == "readiness" else "1" * 40,
                readiness_publication_time,
            )
        )
        if stage in {"calibration", "confirmatory"}:
            outcomes.append(outcome(5, readiness_digest, "excluded", 0))
            preregistrations.append(
                {
                    "sequence": 6,
                    "kind": "calibration",
                    "commit": source_commit,
                    "study_manifest_sha256": None,
                    "declared_at": "2025-12-31T00:00:50Z",
                }
            )
            publications.append(
                publication(
                    7,
                    "preregistration",
                    "calibration",
                    "e" * 40,
                    "2025-12-31T00:01:00Z",
                )
            )
            intents.append(
                {
                    "sequence": 8,
                    "intent": calibration,
                    "intent_sha256": calibration_digest,
                }
            )
            publications.append(
                publication(
                    9,
                    "intent",
                    calibration_digest,
                    self.ledger_commit if stage == "calibration" else "2" * 40,
                    (
                        "2026-01-01T00:00:00Z"
                        if stage == "calibration"
                        else "2025-12-31T00:01:12Z"
                    ),
                )
            )
        if stage == "confirmatory":
            outcomes.append(outcome(10, calibration_digest, "complete", 1))
            intents.append(
                {
                    "sequence": 13,
                    "intent": self.intent,
                    "intent_sha256": self.intent_sha256,
                }
            )
            publications.append(
                publication(
                    12,
                    "preregistration",
                    "confirmatory_freeze",
                    "8" * 40,
                    "2025-12-31T00:02:00Z",
                )
            )
            for index, job_id in enumerate(
                launcher_module._REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS, start=1
            ):
                historical_intent = {
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
                historical_digest = _canonical_payload_sha256(historical_intent)
                sequence = 14 + index * 2
                intents.append(
                    {
                        "sequence": sequence,
                        "intent": historical_intent,
                        "intent_sha256": historical_digest,
                    }
                )
                outcomes.append(
                    {
                        "sequence": sequence + 1,
                        "intent_sha256": historical_digest,
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
            publications.append(
                publication(
                    14,
                    "intent",
                    self.intent_sha256,
                    self.ledger_commit,
                    "2026-01-01T00:00:00Z",
                )
            )

        primary_posture = stella_harbor._benchmark_engine_posture(_PRIMARY_MODEL)
        calibration_outcome = next(
            (item for item in outcomes if item["intent_sha256"] == calibration_digest),
            None,
        )
        readiness_outcome = next(
            (item for item in outcomes if item["intent_sha256"] == readiness_digest),
            None,
        )
        calibration_job_id = (
            calibration_outcome["job_id"]
            if calibration_outcome is not None
            else "00000000-0000-0000-0000-000000000010"
        )
        manifest = {
            "schema_version": "stella-tb21-study-manifest-v6",
            "preregistration": {
                "study_id": "stella-tb21-scientific-study-v1",
                "run_ledger_path": "bench/evidence/stella-tb21-run-ledger.json",
                "readiness_commit": source_commit,
                "calibration_commit": source_commit,
            },
            "sut": {
                "model": _PRIMARY_MODEL,
                "allowed_call_models": ["z-ai/glm-5.1"],
                "binary_sha256": runtime_identity["binary_sha256"],
                "source_commit": runtime_identity["source_commit"],
                "source_commit_embedded": True,
                "agent_version": runtime_identity["agent_version"],
                "adapter_version": runtime_identity["adapter_version"],
                "adapter_sha256": runtime_identity["adapter_sha256"],
                "budget_usd": 0.17,
                "disable_reflection": True,
                "base_url": runtime_identity["base_url"],
                "provider_route_policy": runtime_identity["provider_route_policy"],
                "host_credential_source": "anonymous-seekable-fd-v1",
                "host_credential_name": "OPENROUTER_API_KEY",
                "host_credential_bundle_count": 1,
                "engine_posture_version": "stella-tb21-engine-posture-v1",
                "engine_posture": primary_posture[0],
                "engine_posture_sha256": primary_posture[2],
            },
            "analysis": {
                "sha256": runtime_identity["analysis_sha256"],
                "public_timing_sha256": runtime_identity["public_timing_sha256"],
            },
            "dataset": {
                "name": "terminal-bench/terminal-bench-2-1",
                "ref": _CANONICAL_DATASET.partition("@")[2],
                "task_set_sha256": launcher_module._CONFIRMATORY_TASK_SET_SHA256,
                **launcher_module._CANONICAL_HARBOR_DATASET_SETTINGS,
            },
            "design": {"tasks": 89, "attempts_per_task": 5},
            "harbor": {
                "version": runtime_identity["harbor_version"],
                "sha256": runtime_identity["harbor_sha256"],
                **launcher_module._CANONICAL_HARBOR_SETTINGS,
                **launcher_module._CANONICAL_HARBOR_JOB_SETTINGS,
            },
            "comparator": deepcopy(launcher_module._CANONICAL_COMPARATOR),
            "calibration": {
                "seed": 20260721,
                "tasks": [
                    task.removeprefix("terminal-bench/")
                    for task in _CALIBRATION_FILTERS
                ],
                "model_order": list(_CANDIDATE_MODELS),
                "call_models_by_config": {
                    model: [model.removeprefix("openrouter/")]
                    for model in _CANDIDATE_MODELS
                },
                "engine_postures_by_config": {
                    model: {
                        "version": "stella-tb21-engine-posture-v1",
                        "posture": stella_harbor._benchmark_engine_posture(model)[0],
                        "sha256": stella_harbor._benchmark_engine_posture(model)[2],
                    }
                    for model in _CANDIDATE_MODELS
                },
                "job_name": "stella-tb21-calibration-20260721",
                "job_id": calibration_job_id,
                "attempts_per_model_task": 2,
                "n_concurrent_trials": 3,
                "minimum_passes": 14,
                "projection_trials": 445,
                "projected_spend_limit_usd": 75.0,
                "selected_model": _CANDIDATE_MODELS[0],
                "trial_data_sha256": "7" * 64,
                "excluded_job_ids": list(
                    launcher_module._REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS
                )
                + [
                    readiness_outcome["job_id"]
                    if readiness_outcome is not None
                    else "00000000-0000-0000-0000-000000000005"
                ],
                "excluded_ledger_sha256": "6" * 64,
            },
            "confirmatory": {"job_name": job_name, "n_concurrent_trials": 1},
        }
        self.manifest = json.dumps(
            manifest, sort_keys=True, separators=(",", ":")
        ).encode()
        if stage == "confirmatory":
            preregistrations.append(
                {
                    "sequence": 11,
                    "kind": "confirmatory_freeze",
                    "commit": self.subject_commit,
                    "study_manifest_sha256": hashlib.sha256(self.manifest).hexdigest(),
                    "declared_at": "2025-12-31T00:01:50Z",
                }
            )

        ledger = {
            "schema_version": "stella-tb21-run-ledger-v2",
            "study_id": "stella-tb21-scientific-study-v1",
            "ledger_path": "bench/evidence/stella-tb21-run-ledger.json",
            "historical_spend_disclosure": {
                "known_lower_bound_usd": 0.2429614978,
                "unknown_cancellation_spend": True,
                "new_authorized_budget_usd": 200.0,
            },
            "preregistrations": sorted(
                preregistrations, key=lambda item: item["sequence"]
            ),
            "intents": sorted(intents, key=lambda item: item["sequence"]),
            "publications": sorted(publications, key=lambda item: item["sequence"]),
            "outcomes": sorted(outcomes, key=lambda item: item["sequence"]),
        }
        self.preregistration_ledger_snapshots: dict[str, bytes] = {}
        publication_by_kind = {
            item["subject_id"]: item
            for item in ledger["publications"]
            if item["subject_type"] == "preregistration"
        }
        for preregistration in ledger["preregistrations"]:
            kind = preregistration["kind"]
            publication_record = publication_by_kind[kind]
            preregistration_snapshot = deepcopy(ledger)
            for array_name in (
                "preregistrations",
                "intents",
                "publications",
                "outcomes",
            ):
                preregistration_snapshot[array_name] = [
                    item
                    for item in preregistration_snapshot[array_name]
                    if item["sequence"] <= preregistration["sequence"]
                ]
            self.preregistration_ledger_snapshots[
                publication_record["ledger_commit"]
            ] = json.dumps(
                preregistration_snapshot,
                sort_keys=True,
                separators=(",", ":"),
            ).encode()
        snapshot = deepcopy(ledger)
        snapshot["publications"] = snapshot["publications"][:-1]
        self.snapshot_ledger = json.dumps(
            snapshot,
            sort_keys=True,
            separators=(",", ":"),
        ).encode()
        self.ledger = json.dumps(ledger, sort_keys=True, separators=(",", ":")).encode()
        self.changed_comment: dict[str, object] | None = None
        self.changed_branch: dict[str, object] | None = None
        self.failure: str | None = None
        self.comment_reads = 0
        self.branch_reads = 0

    def get_repository(self) -> dict[str, object]:
        if self.failure == "repository":
            raise RuntimeError("anonymous repository GET failed")
        return deepcopy(self.repository)

    def get_branch(self, branch: str) -> dict[str, object]:
        assert branch == "main"
        self.branch_reads += 1
        if self.branch_reads > 1 and self.changed_branch is not None:
            return deepcopy(self.changed_branch)
        return {"name": "main", "commit": {"sha": self.publication_commit}}

    def get_issue(self, issue_number: int) -> dict[str, object]:
        assert issue_number == 123
        if self.failure == "issue":
            raise RuntimeError("anonymous issue GET failed")
        return deepcopy(self.issue)

    def get_comment(self, comment_id: int) -> dict[str, object]:
        assert comment_id == 456
        self.comment_reads += 1
        if self.failure == "comment":
            raise RuntimeError("anonymous comment GET failed")
        if self.comment_reads > 1 and self.changed_comment is not None:
            return deepcopy(self.changed_comment)
        return deepcopy(self.comment)

    def get_commit(self, commit_sha: str) -> dict[str, object]:
        assert isinstance(commit_sha, str) and len(commit_sha) == 40
        return {
            "sha": commit_sha,
            "url": f"https://api.github.com/repos/macanderson/stella/commits/{commit_sha}",
            "html_url": f"https://github.com/macanderson/stella/commit/{commit_sha}",
            "commit": {"tree": {"sha": commit_sha}},
        }

    def compare_commits(self, base_sha: str, head_sha: str) -> dict[str, object]:
        assert base_sha != head_sha
        return {
            "status": "ahead",
            "ahead_by": 1,
            "base_commit": {"sha": base_sha},
            "merge_base_commit": {"sha": base_sha},
            "commits": [{"sha": head_sha}],
        }

    def get_tree(self, commit_sha: str) -> dict[str, object]:
        assert isinstance(commit_sha, str) and len(commit_sha) == 40
        paths = (
            *launcher_module._FIXED_ADAPTER_SOURCE_PATHS,
            launcher_module._FIXED_ANALYZER_PATH,
            launcher_module._FIXED_PUBLIC_TIMING_PATH,
            launcher_module._FIXED_PROTOCOL_PATH,
            *launcher_module._FIXED_READINESS_SOURCE_PATHS,
        )
        return {
            "truncated": False,
            "tree": [
                {"path": path, "type": "blob", "mode": "100644"} for path in paths
            ],
        }

    def get_content(self, path: str, commit_sha: str) -> bytes:
        if path == host_module.public_report_path(self.intent_sha256):
            self.host_report_fetches.append((path, commit_sha))
            if self.failure == "host-content":
                raise RuntimeError("anonymous host-report GET failed")
            assert commit_sha == self.ledger_commit
            return self.host_report
        if path == "bench/evidence/stella-tb21-study-manifest.json":
            assert commit_sha == self.subject_commit
            return self.manifest
        if path == "bench/evidence/stella-tb21-run-ledger.json":
            if self.failure == "content":
                raise RuntimeError("anonymous ledger GET failed")
            if commit_sha == self.ledger_commit:
                return self.snapshot_ledger
            if commit_sha == self.publication_commit:
                return self.ledger
            assert commit_sha in self.preregistration_ledger_snapshots
            return self.preregistration_ledger_snapshots[commit_sha]
        assert commit_sha in {
            self.subject_commit,
            self.intent["artifacts"]["source_commit"],
        }
        return (self.repository_root / path).read_bytes()


def _public_reader(
    command: list[str], binary: Path, credential: str = "test-secret"
) -> _FakePublicIntentReader:
    return _FakePublicIntentReader(
        command, _runtime_identity(command, binary, credential)
    )


def _runtime_identity(
    command: list[str], binary: Path, credential: str = "test-secret"
) -> dict[str, object]:
    runtime_identity, _ = _validated_runtime_identity(
        command,
        _claim_environment(binary, OPENROUTER_API_KEY=credential),
        {"OPENROUTER_API_KEY": credential},
    )
    return runtime_identity


def _verify_fixture(
    command: list[str],
    binary: Path,
    *,
    reader: _FakePublicIntentReader | None = None,
    provider: _FakeProviderKeyReader | None = None,
    clock: object | None = None,
    runtime_revalidator: object | None = None,
) -> dict[str, object]:
    public_reader = reader or _public_reader(command, binary)
    runtime_identity = _runtime_identity(command, binary)
    return _verify_public_intent(
        command,
        runtime_identity=runtime_identity,
        runtime_revalidator=(
            runtime_revalidator  # type: ignore[arg-type]
            if runtime_revalidator is not None
            else lambda: runtime_identity
        ),
        provider_credential="test-secret",
        management_credential=_TEST_MANAGEMENT_CREDENTIAL,
        reader=public_reader,
        provider_key_reader=provider or _FakeProviderKeyReader(),
        clock=clock,  # type: ignore[arg-type]
    )


def _host_preflight_fixture(
    command: list[str],
    reader: _FakePublicIntentReader,
    public_intent_attestation: dict[str, object],
) -> launcher_module._VerifiedHostPreflight:
    return launcher_module._verify_public_host_preflight(
        command,
        public_intent_attestation=public_intent_attestation,
        forbidden_credentials=("test-secret", _TEST_MANAGEMENT_CREDENTIAL),
        docker_executable=Path("/usr/bin/docker"),
        reader=reader,
        host_probe=_fake_host_probe,
    )


def test_launch_receipt_controls_are_exact_nonsecret_attestation() -> None:
    assert LAUNCH_RECEIPT_CONTROLS == {
        "command": "harbor-run-only",
        "agent_import_path": "stella_harbor:StellaAgent",
        "environment": "docker",
        "credential_source": "anonymous-seekable-fd-v1",
        "fresh_job_directory": "atomic-create",
        "resume": "forbidden",
        "in_run_publication": "forbidden",
        "filesystem_settings": "disabled",
        "filesystem_credentials": "disabled",
        "project_env_files": "disabled",
        "subprocess_credential_scrub": "enabled",
        "harbor_clock_timezone": "UTC",
    }


def test_bundle_is_unlinked_owner_only_and_pread_is_offset_independent() -> None:
    handle = create_anonymous_credential_bundle(
        {"OPENROUTER_API_KEY": "test-openrouter-secret"}
    )
    try:
        fd = handle.fileno()
        info = os.fstat(fd)
        assert stat.S_ISREG(info.st_mode)
        assert info.st_nlink == 0
        assert stat.S_IMODE(info.st_mode) == 0o600
        assert os.get_inheritable(fd) is True
        assert handle.tell() == 0

        with ThreadPoolExecutor(max_workers=4) as executor:
            reads = list(
                executor.map(
                    read_anonymous_credential_bundle,
                    [str(fd)] * 16,
                )
            )
        assert reads == [{"OPENROUTER_API_KEY": "test-openrouter-secret"}] * 16
        assert handle.tell() == 0
    finally:
        handle.close()


def test_sanitizer_removes_named_and_arbitrary_alias_copies() -> None:
    secret = "test-openrouter-secret"
    sanitized = sanitized_harbor_environment(
        {
            "OPENROUTER_API_KEY": secret,
            "HARBOR_ALIAS": f"prefix:{secret}:suffix",
            "BENIGN": "kept",
        },
        91,
    )

    assert sanitized == {HOST_CREDENTIAL_BUNDLE_FD_ENV: "91"}


def test_sanitizer_removes_management_key_aliases_from_allowlisted_values() -> None:
    benchmark_secret = "test-openrouter-benchmark-secret"
    management_secret = "test-openrouter-management-secret"
    sanitized = sanitized_harbor_environment(
        {
            "OPENROUTER_API_KEY": benchmark_secret,
            "OPENROUTER_MANAGEMENT_API_KEY": management_secret,
            "HOME": f"/copied/{management_secret}",
            "XDG_CACHE_HOME": f"/copied/{benchmark_secret}",
        },
        91,
    )

    assert sanitized == {HOST_CREDENTIAL_BUNDLE_FD_ENV: "91"}


@pytest.mark.parametrize(
    "management_secret",
    [None, "", "test-openrouter-benchmark-secret"],
)
def test_secure_launch_requires_distinct_host_only_management_key(
    tmp_path: Path, management_secret: str | None
) -> None:
    benchmark_secret = "test-openrouter-benchmark-secret"
    binary = _write_claim_elf(tmp_path / "stella")
    environment = _claim_environment(
        binary,
        OPENROUTER_API_KEY=benchmark_secret,
    )
    environment.pop("OPENROUTER_MANAGEMENT_API_KEY")
    if management_secret is not None:
        environment["OPENROUTER_MANAGEMENT_API_KEY"] = management_secret

    with pytest.raises(RuntimeError, match="OPENROUTER_MANAGEMENT_API_KEY"):
        exec_harbor_securely(
            _readiness_command(tmp_path),
            environ=environment,
            public_intent_reader=object(),  # type: ignore[arg-type]
            provider_key_reader=_FakeProviderKeyReader(),
        )


def test_sanitizer_allowlists_hostile_interpreter_loader_and_shell_environment() -> (
    None
):
    sanitized = sanitized_harbor_environment(
        {
            "OPENROUTER_API_KEY": "test-secret",
            "HOME": "/safe-home",
            "LC_ALL": "C",
            "PATH": "/attacker/bin",
            "PYTHONPATH": "/attacker/python",
            "PYTHONSTARTUP": "/attacker/startup.py",
            "DYLD_INSERT_LIBRARIES": "/attacker/inject.dylib",
            "LD_PRELOAD": "/attacker/inject.so",
            "BASH_ENV": "/attacker/bashrc",
            "ENV": "/attacker/shrc",
            "COMPOSE_FILE": "/attacker/compose.yml",
            "NODE_OPTIONS": "--require=/attacker/hook.js",
        },
        17,
    )

    assert sanitized == {
        "HOME": "/safe-home",
        "LC_ALL": "C",
        HOST_CREDENTIAL_BUNDLE_FD_ENV: "17",
    }


def test_literal_model_roster_accepts_repeated_openrouter_forms() -> None:
    models = _literal_model_arguments(
        [
            "harbor",
            "run",
            "--model",
            "openrouter/deepseek/deepseek-v4-pro",
            "--model=openrouter/z-ai/glm-5.2",
            "-m",
            "openrouter/x-ai/grok-4.5",
        ]
    )
    assert models == [
        "openrouter/deepseek/deepseek-v4-pro",
        "openrouter/z-ai/glm-5.2",
        "openrouter/x-ai/grok-4.5",
    ]


@pytest.mark.parametrize(
    "command",
    [
        ["harbor", "run"],
        ["harbor", "run", "--model", "openrouter/"],
        [
            "harbor",
            "run",
            "--model",
            "openrouter/deepseek/deepseek-v4-pro",
            "--model",
            "anthropic/claude-fable-5",
        ],
    ],
)
def test_literal_model_roster_rejects_missing_malformed_or_mixed(
    command: list[str],
) -> None:
    with pytest.raises(RuntimeError):
        _literal_model_arguments(command)


def test_exec_bundles_only_roster_key_and_scrubs_unrelated_keys(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    openrouter_secret = "test-openrouter-secret"
    management_secret = _TEST_MANAGEMENT_CREDENTIAL
    unrelated_secret = "test-anthropic-secret"
    observed: dict[str, object] = {}
    forbidden_sidecar_values: list[tuple[str, ...]] = []
    binary = _write_claim_elf(tmp_path / "stella")

    def recording_sidecar_writer(
        job_dir: Path,
        binding: dict[str, object],
        *,
        forbidden_values: tuple[str, ...] = (),
    ) -> Path:
        forbidden_sidecar_values.append(forbidden_values)
        return host_module.write_launch_binding_sidecar(
            job_dir,
            binding,
            forbidden_values=forbidden_values,
        )

    monkeypatch.setattr(
        launcher_module,
        "write_launch_binding_sidecar",
        recording_sidecar_writer,
    )

    def fake_execvpe(
        executable: str, command: list[str], child_env: dict[str, str]
    ) -> None:
        assert Path(executable).is_absolute()
        assert Path(executable).name.startswith("python")
        assert command[0] == executable
        assert command[1:7] == [
            "-I",
            "-S",
            "-B",
            "-X",
            "pycache_prefix="
            f"{tmp_path / 'stella-tb21-calibration-20260721' / '.stella-python-cache'}",
            "-c",
        ]
        assert "from harbor.cli.main import app" in command[7]
        observed["command"] = command
        observed["environment"] = child_env
        bundle = read_anonymous_credential_bundle(
            child_env[HOST_CREDENTIAL_BUNDLE_FD_ENV]
        )
        observed["bundle"] = bundle

    command = _calibration_command(tmp_path)
    public_reader = _public_reader(command, binary, openrouter_secret)
    with pytest.raises(RuntimeError, match="unexpectedly returned"):
        exec_harbor_securely(
            command,
            environ={
                **_claim_environment(binary),
                "OPENROUTER_API_KEY": openrouter_secret,
                "OPENROUTER_MANAGEMENT_API_KEY": management_secret,
                "ANTHROPIC_API_KEY": unrelated_secret,
                "ARBITRARY_ALIAS": f"copied:{openrouter_secret}",
                "HOME": f"/copied/{management_secret}",
                "BENIGN": "kept",
                "PATH": "/attacker/bin",
                "PYTHONPATH": "/attacker/python",
                "DYLD_INSERT_LIBRARIES": "/attacker/inject.dylib",
            },
            execvpe=fake_execvpe,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )

    assert observed["bundle"] == {"OPENROUTER_API_KEY": openrouter_secret}
    child_env = observed["environment"]
    assert isinstance(child_env, dict)
    assert "BENIGN" not in child_env
    assert "PYTHONPATH" not in child_env
    assert "DYLD_INSERT_LIBRARIES" not in child_env
    assert child_env["PATH"] != "/attacker/bin"
    assert "/usr/bin" in child_env["PATH"].split(os.pathsep)
    assert child_env["TZ"] == "UTC"
    assert all(
        openrouter_secret not in value
        and management_secret not in value
        and unrelated_secret not in value
        for value in child_env.values()
    )
    assert "ARBITRARY_ALIAS" not in child_env
    forwarded_command = observed["command"]
    assert isinstance(forwarded_command, list)
    assert "--intent-sha256" not in forwarded_command
    assert "--intent-comment-url" not in forwarded_command
    assert public_reader.intent_sha256 not in forwarded_command
    assert _TEST_COMMENT_URL not in forwarded_command
    job_dir = tmp_path / "stella-tb21-calibration-20260721"
    receipt_path = job_dir / LAUNCH_RECEIPT_FILENAME
    receipt_raw = receipt_path.read_bytes()
    receipt = json.loads(receipt_raw)
    assert receipt == {
        "schema_version": LAUNCH_RECEIPT_SCHEMA,
        "job_name": "stella-tb21-calibration-20260721",
        "intent_sha256": public_reader.intent_sha256,
        "models": _CANDIDATE_MODELS,
        "public_intent_attestation": receipt["public_intent_attestation"],
        "launcher_controls": LAUNCH_RECEIPT_CONTROLS,
    }
    public_attestation = receipt["public_intent_attestation"]
    assert set(public_attestation) == PUBLIC_INTENT_ATTESTATION_FIELDS
    sidecar_path = job_dir / host_module.HOST_ATTESTATION_FILENAME
    sidecar_raw = sidecar_path.read_bytes()
    sidecar = json.loads(sidecar_raw)
    assert stat.S_IMODE(sidecar_path.stat().st_mode) == 0o600
    assert forbidden_sidecar_values == [(openrouter_secret, management_secret)]
    assert openrouter_secret.encode() not in sidecar_raw
    assert management_secret.encode() not in sidecar_raw
    assert unrelated_secret.encode() not in sidecar_raw
    assert openrouter_secret.encode() not in receipt_raw
    assert management_secret.encode() not in receipt_raw
    assert (
        sidecar["launch_receipt_sha256"]
        == hashlib.sha256(receipt_path.read_bytes()).hexdigest()
    )
    assert sidecar["public_report"] == {
        "repository": "macanderson/stella",
        "commit": public_reader.ledger_commit,
        "path": host_module.public_report_path(public_reader.intent_sha256),
        "sha256": hashlib.sha256(public_reader.host_report).hexdigest(),
        "fetched_at_utc": sidecar["public_report"]["fetched_at_utc"],
    }
    assert sidecar["public_report_payload"] == json.loads(public_reader.host_report)
    assert sidecar["live_recheck"]["checks"]["all_passed"] is True
    assert public_reader.host_report_fetches == [
        (
            host_module.public_report_path(public_reader.intent_sha256),
            public_reader.ledger_commit,
        )
    ]
    assert public_attestation["intent_sha256"] == public_reader.intent_sha256
    assert public_attestation["comment_url"] == _TEST_COMMENT_URL
    assert public_attestation["verification_mode"] == "anonymous-get-v1"
    pycache_prefix = (
        tmp_path / "stella-tb21-calibration-20260721" / ".stella-python-cache"
    )
    assert pycache_prefix.is_dir()
    assert stat.S_IMODE(pycache_prefix.stat().st_mode) == 0o700
    assert list(pycache_prefix.iterdir()) == []


@pytest.mark.parametrize(
    "extra",
    [
        ["--env-file", "secrets.env"],
        ["--config=job.json"],
        ["--agent-env", "SAFE=value"],
        ["--verifier-env=SAFE=value"],
        ["--environment-kwarg", "foo=bar"],
        ["--agent-kwarg", "foo=bar"],
        ["--environment-import-path", "hostile:Environment"],
        ["--mounts-json", "[]"],
        ["--upload"],
        ["--export-push"],
        ["--public"],
        ["--share-org", "example"],
        ["--agent", "oracle"],
        ["-o", "/absolute/unregistered-jobs"],
        ["-c/private/tmp/job.json"],
        ["-aoracle"],
        ["-eapple-container"],
        ["-mopenrouter/model"],
    ],
)
def test_claim_command_rejects_reintroduction_and_custom_execution_options(
    extra: list[str],
) -> None:
    command = [
        "harbor",
        "run",
        "--agent-import-path",
        "stella_harbor:StellaAgent",
        "--model",
        "openrouter/model",
        *extra,
    ]
    with pytest.raises(
        RuntimeError,
        match="noncanonical Harbor claim option|attached short-option values",
    ):
        _validate_claim_command(command, {"OPENROUTER_API_KEY": "test-secret"})


def test_claim_command_requires_stella_import_and_docker() -> None:
    base = ["harbor", "run", "--model", "openrouter/model"]
    with pytest.raises(RuntimeError, match="canonical Stella agent import"):
        _validate_claim_command(base, {"OPENROUTER_API_KEY": "test-secret"})

    with pytest.raises(RuntimeError, match="Docker environment"):
        _validate_claim_command(
            [
                *base,
                "--agent-import-path=stella_harbor:StellaAgent",
                "--env",
                "apple-container",
            ],
            {"OPENROUTER_API_KEY": "test-secret"},
        )

    with pytest.raises(RuntimeError, match="Docker environment"):
        _validate_claim_command(
            [*base, "--agent-import-path=stella_harbor:StellaAgent"],
            {"OPENROUTER_API_KEY": "test-secret"},
        )

    with pytest.raises(RuntimeError, match="Docker environment"):
        _validate_claim_command(
            [
                *base,
                "--agent-import-path=stella_harbor:StellaAgent",
                "--env=docker",
                "--env=docker",
            ],
            {"OPENROUTER_API_KEY": "test-secret"},
        )


def test_claim_command_requires_one_safe_explicit_job_destination(
    tmp_path: Path,
) -> None:
    base = [
        "harbor",
        "run",
        "--agent-import-path=stella_harbor:StellaAgent",
        "--env=docker",
        "--model=openrouter/model",
        "--intent-sha256",
        _TEST_INTENT_SHA256,
    ]
    with pytest.raises(RuntimeError, match="exactly one literal --job-name"):
        _validate_claim_command(base, {"OPENROUTER_API_KEY": "test-secret"})

    valid = [
        *base,
        "--job-name",
        "confirmatory-001",
        "--jobs-dir",
        str(tmp_path),
    ]
    _validate_claim_command(valid, {"OPENROUTER_API_KEY": "test-secret"})
    assert _claim_job_destination(valid) == (
        "confirmatory-001",
        tmp_path.resolve() / "confirmatory-001",
    )

    with pytest.raises(RuntimeError, match="single safe job-name component"):
        _validate_claim_command(
            [
                *base,
                "--job-name",
                "../resume-existing",
                "--jobs-dir",
                str(tmp_path),
            ],
            {"OPENROUTER_API_KEY": "test-secret"},
        )
    with pytest.raises(RuntimeError, match="exactly one literal --job-name"):
        _validate_claim_command(
            [*valid, "--job-name=duplicate"],
            {"OPENROUTER_API_KEY": "test-secret"},
        )
    with pytest.raises(RuntimeError, match="absolute explicit jobs-dir"):
        _validate_claim_command(
            [
                *base,
                "--job-name",
                "relative-jobs-root",
                "--jobs-dir",
                "jobs",
            ],
            {"OPENROUTER_API_KEY": "test-secret"},
        )


def test_claim_command_rejects_unversioned_or_duplicate_dataset(tmp_path: Path) -> None:
    base = [
        "harbor",
        "run",
        "--agent-import-path=stella_harbor:StellaAgent",
        "--env=docker",
        "--model=openrouter/model",
        "--job-name=dataset-bound",
        f"--jobs-dir={tmp_path}",
        f"--intent-sha256={_TEST_INTENT_SHA256}",
    ]
    canonical = (
        "terminal-bench/terminal-bench-2-1@"
        "sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a"
    )

    _validate_claim_command(
        [*base, "--dataset", canonical],
        {"OPENROUTER_API_KEY": "test-secret"},
    )
    _validate_claim_command(
        [*base, "-d", canonical],
        {"OPENROUTER_API_KEY": "test-secret"},
    )
    for invalid in (
        [*base, "--dataset", "terminal-bench/terminal-bench-2-1"],
        [*base, "--dataset", canonical, "--dataset", canonical],
        [*base, "--dataset", canonical, "-d", canonical],
    ):
        with pytest.raises(RuntimeError, match="version-pinned Terminal-Bench"):
            _validate_claim_command(
                invalid,
                {"OPENROUTER_API_KEY": "test-secret"},
            )


def test_tb21_include_filters_require_registry_prefix_and_path_readiness_survives(
    tmp_path: Path,
) -> None:
    base = [
        "harbor",
        "run",
        "--agent-import-path=stella_harbor:StellaAgent",
        "--env=docker",
        "--model=openrouter/model",
        "--job-name=include-bound",
        f"--jobs-dir={tmp_path}",
        f"--intent-sha256={_TEST_INTENT_SHA256}",
    ]
    canonical = (
        "terminal-bench/terminal-bench-2-1@"
        "sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a"
    )

    _validate_claim_command(
        [
            *base,
            "--dataset",
            canonical,
            "--include-task-name=terminal-bench/fix-git",
            "-i",
            "terminal-bench/regex-log",
        ],
        {"OPENROUTER_API_KEY": "test-secret"},
    )
    with pytest.raises(RuntimeError, match="`terminal-bench/` task-name prefix"):
        _validate_claim_command(
            [*base, "--dataset", canonical, "--include-task-name", "fix-git"],
            {"OPENROUTER_API_KEY": "test-secret"},
        )

    # The paid readiness sentinel is a path-only task, not a registry dataset,
    # and deliberately has no include filter.
    _validate_claim_command(
        [*base, "--path", str(tmp_path / "synthetic-adapter-sentinel")],
        {"OPENROUTER_API_KEY": "test-secret"},
    )


@pytest.mark.parametrize(
    "unsafe_option",
    [
        "--timeout-multiplier",
        "--agent-timeout-multiplier",
        "--override-cpus",
        "--retry-include",
        "--retry-exclude",
        "--yes",
        "--artifact",
        "--exclude-task-name",
        "--n-tasks",
        "--task-git-url",
        "--registry-url",
        "--force-build",
        "--disable-verification",
        "--future-harbor-option",
    ],
)
def test_claim_option_allowlist_rejects_every_unregistered_harbor_control(
    tmp_path: Path, unsafe_option: str
) -> None:
    with pytest.raises(RuntimeError, match="noncanonical Harbor claim option"):
        _validate_claim_command(
            [*_readiness_command(tmp_path), unsafe_option, "untrusted"],
            {"OPENROUTER_API_KEY": "test-secret"},
        )


def test_exact_readiness_calibration_and_confirmatory_shapes_are_accepted(
    tmp_path: Path,
) -> None:
    _validate_stage_shape(_readiness_command(tmp_path))
    _validate_stage_shape(_calibration_command(tmp_path))
    _validate_stage_shape(_confirmatory_command(tmp_path, "confirmatory-primary"))
    for model in _CANDIDATE_MODELS:
        command = _confirmatory_command(tmp_path, f"confirmatory-{model.count('/')}")
        command[command.index("--model") + 1] = model
        with pytest.raises(RuntimeError, match="fixed GLM-5.1 primary"):
            _validate_stage_shape(command)


def test_stage_shapes_reject_readiness_path_model_and_control_drift(
    tmp_path: Path,
) -> None:
    wrong_path = _readiness_command(tmp_path)
    wrong_path[wrong_path.index("--path") + 1] = str(tmp_path)
    with pytest.raises(RuntimeError, match="canonical readiness path"):
        _validate_stage_shape(wrong_path)

    wrong_model = _readiness_command(tmp_path)
    wrong_model[wrong_model.index("--model") + 1] = _CANDIDATE_MODELS[1]
    with pytest.raises(RuntimeError, match="readiness model roster"):
        _validate_stage_shape(wrong_model)

    wrong_attempts = _readiness_command(tmp_path)
    wrong_attempts[wrong_attempts.index("--n-attempts") + 1] = "2"
    with pytest.raises(RuntimeError, match="readiness attempt count 1"):
        _validate_stage_shape(wrong_attempts)


def test_stage_shapes_reject_calibration_filter_model_and_control_drift(
    tmp_path: Path,
) -> None:
    wrong_filters = _calibration_command(tmp_path)
    filter_indices = [
        index
        for index, value in enumerate(wrong_filters)
        if value == "--include-task-name"
    ]
    first_value = filter_indices[0] + 1
    second_value = filter_indices[1] + 1
    wrong_filters[first_value], wrong_filters[second_value] = (
        wrong_filters[second_value],
        wrong_filters[first_value],
    )
    with pytest.raises(RuntimeError, match="ordered calibration task filters"):
        _validate_stage_shape(wrong_filters)

    wrong_models = _calibration_command(tmp_path)
    model_indices = [
        index for index, value in enumerate(wrong_models) if value == "--model"
    ]
    first_model = model_indices[0] + 1
    second_model = model_indices[1] + 1
    wrong_models[first_model], wrong_models[second_model] = (
        wrong_models[second_model],
        wrong_models[first_model],
    )
    with pytest.raises(RuntimeError, match="ordered calibration model roster"):
        _validate_stage_shape(wrong_models)

    wrong_concurrency = _calibration_command(tmp_path)
    wrong_concurrency[wrong_concurrency.index("--n-concurrent") + 1] = "1"
    with pytest.raises(RuntimeError, match="calibration concurrency 3"):
        _validate_stage_shape(wrong_concurrency)


def test_stage_shapes_reject_confirmatory_scope_model_and_retry_drift(
    tmp_path: Path,
) -> None:
    included_subset = _confirmatory_command(tmp_path, "confirmatory-included")
    included_subset.extend(("--include-task-name", _CALIBRATION_FILTERS[0]))
    with pytest.raises(RuntimeError, match="confirmatory Harbor option shape"):
        _validate_stage_shape(included_subset)

    wrong_model = _confirmatory_command(tmp_path, "confirmatory-model")
    wrong_model[wrong_model.index("--model") + 1] = "openrouter/other/model"
    with pytest.raises(RuntimeError, match="fixed GLM-5.1 primary"):
        _validate_stage_shape(wrong_model)

    retries = _confirmatory_command(tmp_path, "confirmatory-retries")
    retries[retries.index("--max-retries") + 1] = "1"
    with pytest.raises(RuntimeError, match="zero confirmatory Harbor retries"):
        _validate_stage_shape(retries)


def test_claim_environment_accepts_only_frozen_controls_and_stamped_elf(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")

    assert _binary_version_source_commits(binary) == {_TEST_SOURCE_COMMIT}
    assert _validate_claim_environment(_claim_environment(binary)) == (
        binary,
        _TEST_SOURCE_COMMIT,
    )


def test_claim_environment_rejects_runtime_assembled_version_literals(
    tmp_path: Path,
) -> None:
    binary = _write_runtime_assembled_claim_elf(tmp_path / "stella")

    assert _binary_version_source_commits(binary) == set()
    assert launcher_module._binary_version_texts(binary) == set()
    with pytest.raises(RuntimeError, match="compile-time version bytes"):
        _validate_claim_environment(_claim_environment(binary))


@pytest.mark.parametrize(
    ("name", "value", "message"),
    [
        ("STELLA_BUDGET", "0.170", "exact STELLA_BUDGET=0.17"),
        (
            "STELLA_DISABLE_REFLECTION",
            "true",
            "exact STELLA_DISABLE_REFLECTION=1",
        ),
        ("STELLA_SOURCE_COMMIT", "D" * 40, "lowercase 40-hex"),
        ("STELLA_SOURCE_COMMIT", "d" * 39, "lowercase 40-hex"),
    ],
)
def test_claim_environment_rejects_control_drift_before_launch(
    tmp_path: Path, name: str, value: str, message: str
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    with pytest.raises(RuntimeError, match=message):
        _validate_claim_environment(_claim_environment(binary, **{name: value}))


def test_claim_environment_requires_canonical_regular_executable(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    relative = _claim_environment(binary)
    relative["STELLA_BINARY"] = "stella"
    with pytest.raises(RuntimeError, match="absolute canonical path"):
        _validate_claim_environment(relative)

    alias = tmp_path / "stella-alias"
    alias.symlink_to(binary)
    with pytest.raises(RuntimeError, match="absolute canonical path"):
        _validate_claim_environment(_claim_environment(alias))

    binary.chmod(0o644)
    with pytest.raises(RuntimeError, match="regular executable"):
        _validate_claim_environment(_claim_environment(binary))


@pytest.mark.parametrize(
    ("offset", "replacement"),
    [
        (0, b"BAD!"),
        (4, b"\x01"),
        (5, b"\x02"),
        (6, b"\x00"),
        (18, (183).to_bytes(2, "little")),
    ],
)
def test_claim_environment_rejects_non_x86_64_elf(
    tmp_path: Path, offset: int, replacement: bytes
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    payload = bytearray(binary.read_bytes())
    payload[offset : offset + len(replacement)] = replacement
    binary.write_bytes(payload)
    binary.chmod(0o755)

    with pytest.raises(RuntimeError, match="ELF64 little-endian x86_64"):
        _validate_claim_environment(_claim_environment(binary))


def test_claim_environment_rejects_missing_mismatched_or_ambiguous_version_stamp(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella", "e" * 40)
    with pytest.raises(RuntimeError, match="compile-time version bytes"):
        _validate_claim_environment(_claim_environment(binary))

    with binary.open("ab") as handle:
        handle.write(f"other-dev.{_TEST_SOURCE_COMMIT}\0".encode("ascii"))
    with pytest.raises(RuntimeError, match="compile-time version bytes"):
        _validate_claim_environment(
            _claim_environment(binary, STELLA_SOURCE_COMMIT="e" * 40)
        )


def test_harbor_resolution_ignores_path_and_attests_current_adapter(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("PATH", "/untrusted/path")

    interpreter, adapter_sha256 = _resolve_harbor_executable()

    assert interpreter == Path(os.sys.executable).resolve()
    assert interpreter.is_absolute()
    assert adapter_sha256 == stella_harbor._adapter_content_sha256()


def test_isolated_harbor_command_never_executes_a_hostile_console_script(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    hostile_harbor = tmp_path / "harbor"
    hostile_harbor.write_text("#!/bin/sh\nexit 99\n")
    hostile_harbor.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path))
    interpreter, _adapter_sha256 = _resolve_harbor_executable()
    pycache_prefix = tmp_path / "fresh-pycache"
    pycache_prefix.mkdir(mode=0o700)

    command = _isolated_harbor_command(
        interpreter, ["harbor", "--help"], pycache_prefix
    )

    assert command[:7] == [
        str(interpreter),
        "-I",
        "-S",
        "-B",
        "-X",
        f"pycache_prefix={pycache_prefix}",
        "-c",
    ]
    assert "from harbor.cli.main import app" in command[7]
    assert str(hostile_harbor) not in command
    assert command[-1] == "--help"


def test_isolated_harbor_command_rejects_a_prepopulated_pycache_prefix(
    tmp_path: Path,
) -> None:
    interpreter, _adapter_sha256 = _resolve_harbor_executable()
    pycache_prefix = tmp_path / "not-fresh"
    pycache_prefix.mkdir(mode=0o700)
    (pycache_prefix / "hostile.pyc").write_bytes(b"not bytecode")

    with pytest.raises(RuntimeError, match="must be fresh"):
        _isolated_harbor_command(interpreter, ["harbor", "--help"], pycache_prefix)


def test_harbor_resolution_rejects_a_missing_interpreter(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    interpreter = tmp_path / "missing-python"
    monkeypatch.setattr(
        "stella_harbor.secure_launcher.sys.executable", str(interpreter)
    )

    with pytest.raises(RuntimeError, match="cannot resolve its Python interpreter"):
        _resolve_harbor_executable()


def test_failed_preflight_never_reserves_job_directory(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "preflight-failed")

    invalid_environment = _claim_environment(binary, STELLA_BUDGET="0.18")
    with pytest.raises(RuntimeError, match="STELLA_BUDGET"):
        exec_harbor_securely(command, environ=invalid_environment)
    assert not (tmp_path / "preflight-failed").exists()

    def failed_resolution() -> tuple[Path, str]:
        raise RuntimeError("Harbor preflight failed")

    monkeypatch.setattr(
        "stella_harbor.secure_launcher._resolve_harbor_executable",
        failed_resolution,
    )
    with pytest.raises(RuntimeError, match="Harbor preflight failed"):
        exec_harbor_securely(command, environ=_claim_environment(binary))
    assert not (tmp_path / "preflight-failed").exists()


def test_stage_shape_failure_never_reserves_job_directory(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "invalid-confirmatory-shape")
    command[command.index("--n-attempts") + 1] = "4"

    with pytest.raises(RuntimeError, match="confirmatory attempt count 5"):
        exec_harbor_securely(command, environ=_claim_environment(binary))
    assert not (tmp_path / "invalid-confirmatory-shape").exists()


@pytest.mark.parametrize(
    "failure_case",
    [
        "changed",
        "edited",
        "wrong-author",
        "wrong-returned-url",
        "private",
        "auth-only",
        "network",
        "wrong-schema",
        "wrong-digest",
    ],
)
def test_public_intent_failure_never_reserves_or_execs(
    tmp_path: Path, failure_case: str
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    job_name = f"public-preflight-{failure_case}"
    command = _confirmatory_command(tmp_path, job_name)
    public_reader = _public_reader(command, binary)
    if failure_case == "changed":
        public_reader.changed_comment = deepcopy(public_reader.comment)
        public_reader.changed_comment["body"] = (
            str(public_reader.changed_comment["body"]) + " "
        )
    elif failure_case == "edited":
        public_reader.comment["updated_at"] = "2026-01-01T00:00:01Z"
    elif failure_case == "wrong-author":
        public_reader.comment["user"] = {"login": "someone-else"}
    elif failure_case == "wrong-returned-url":
        public_reader.comment["html_url"] = (
            "https://github.com/macanderson/stella/issues/123#issuecomment-999"
        )
    elif failure_case == "private":
        public_reader.repository["private"] = True
    elif failure_case == "auth-only":
        public_reader.failure = "repository"
    elif failure_case == "network":
        public_reader.failure = "content"
    else:
        body = json.loads(str(public_reader.comment["body"]))
        if failure_case == "wrong-schema":
            body["schema_version"] = "stella-tb21-github-attestation-v1"
        else:
            body["subject_id"] = "f" * 64
        public_reader.comment["body"] = json.dumps(
            body, sort_keys=True, separators=(",", ":")
        )

    def forbidden_exec(*_args: object) -> None:
        raise AssertionError("Harbor exec must not run after failed public preflight")

    with pytest.raises(RuntimeError):
        exec_harbor_securely(
            command,
            environ=_claim_environment(binary),
            execvpe=forbidden_exec,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )
    assert not (tmp_path / job_name).exists()


@pytest.mark.parametrize(
    "failure_case",
    ["missing", "tampered", "stale", "credential"],
)
def test_public_host_report_failure_never_reserves_or_execs(
    tmp_path: Path, failure_case: str
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    public_reader = _public_reader(command, binary)
    if failure_case == "missing":
        public_reader.failure = "host-content"
    elif failure_case == "credential":
        public_reader.host_report = b'{"leaked":"test-secret"}\n'
    else:
        report = json.loads(public_reader.host_report)
        if failure_case == "tampered":
            report["observed"]["cpu"]["effective_vcpus"] = 3
        else:
            report["captured_at_utc"] = (
                (datetime.now(timezone.utc) - timedelta(minutes=16))
                .isoformat(timespec="microseconds")
                .replace("+00:00", "Z")
            )
        public_reader.host_report = host_module.canonical_json_bytes(report)

    def forbidden_exec(*_args: object) -> None:
        raise AssertionError("Harbor exec must not run after failed host preflight")

    with pytest.raises(RuntimeError):
        exec_harbor_securely(
            command,
            environ=_claim_environment(binary),
            execvpe=forbidden_exec,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )
    assert not (tmp_path / "stella-readiness-synthetic-v1").exists()
    assert not launcher_module._launcher_reservation_root().exists()


@pytest.mark.parametrize("failure_case", ["different_boot", "running_container"])
def test_live_host_failure_never_reserves_or_execs(
    tmp_path: Path, failure_case: str
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    public_reader = _public_reader(command, binary)

    def ineligible_probe(
        *, jobs_dir: Path, docker_executable: Path
    ) -> dict[str, object]:
        snapshot = _fake_host_probe(
            jobs_dir=jobs_dir,
            docker_executable=docker_executable,
        )
        if failure_case == "different_boot":
            snapshot["host_fingerprint_sha256"] = "b" * 64
        else:
            observed = snapshot["observed"]
            checks = snapshot["checks"]
            assert isinstance(observed, dict) and isinstance(checks, dict)
            observed["running_container_ids"] = ["c" * 64]
            docker = observed["docker"]
            assert isinstance(docker, dict)
            docker["reported_running_containers"] = 1
            checks["zero_running_containers"] = False
            checks["all_passed"] = False
        return snapshot

    with pytest.raises(RuntimeError, match="native host preflight failed"):
        exec_harbor_securely(
            command,
            environ=_claim_environment(binary),
            execvpe=lambda *_args: None,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
            host_probe=ineligible_probe,
        )
    assert not (tmp_path / "stella-readiness-synthetic-v1").exists()
    assert not launcher_module._launcher_reservation_root().exists()


def test_live_host_probe_precedes_global_paid_intent_reservation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    public_reader = _public_reader(command, binary)
    events: list[str] = []
    real_reserve = launcher_module._reserve_global_intent

    def ordered_probe(*, jobs_dir: Path, docker_executable: Path) -> dict[str, object]:
        events.append("probe")
        return _fake_host_probe(
            jobs_dir=jobs_dir,
            docker_executable=docker_executable,
        )

    def ordered_reservation(intent_sha256: str) -> Path:
        events.append("reserve")
        return real_reserve(intent_sha256)

    monkeypatch.setattr(
        launcher_module,
        "_reserve_global_intent",
        ordered_reservation,
    )
    with pytest.raises(RuntimeError, match="unexpectedly returned"):
        exec_harbor_securely(
            command,
            environ=_claim_environment(binary),
            execvpe=lambda *_args: None,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
            host_probe=ordered_probe,
        )

    assert events == ["probe", "reserve"]


def test_readiness_non_owner_only_jobs_root_never_reserves_or_execs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    public_reader = _public_reader(command, binary)
    tmp_path.chmod(0o755)
    monkeypatch.setattr(
        launcher_module,
        "_replay_prior_stage_evidence",
        _REAL_PRIOR_STAGE_REPLAY,
    )

    def forbidden_exec(*_args: object) -> None:
        raise AssertionError("Harbor exec must not run with a non-owner-only root")

    with pytest.raises(RuntimeError, match="jobs-dir must be canonical and owner-only"):
        exec_harbor_securely(
            command,
            environ=_claim_environment(binary),
            execvpe=forbidden_exec,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )
    assert not (tmp_path / "stella-readiness-synthetic-v1").exists()


def test_missing_prior_readiness_job_never_reserves_or_execs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _calibration_command(tmp_path)
    public_reader = _public_reader(command, binary)
    monkeypatch.setattr(
        launcher_module,
        "_replay_prior_stage_evidence",
        _REAL_PRIOR_STAGE_REPLAY,
    )

    def forbidden_exec(*_args: object) -> None:
        raise AssertionError("Harbor exec must not run without prior evidence")

    with pytest.raises(RuntimeError, match="prior Harbor job is missing"):
        exec_harbor_securely(
            command,
            environ=_claim_environment(binary),
            execvpe=forbidden_exec,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )
    assert not (tmp_path / "stella-tb21-calibration-20260721").exists()


def test_forged_prior_readiness_artifacts_never_reserve_or_exec(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _calibration_command(tmp_path)
    public_reader = _public_reader(command, binary)
    readiness_dir = tmp_path / "stella-readiness-synthetic-v1"
    readiness_dir.mkdir()
    readiness_outcome = next(
        outcome
        for outcome in json.loads(public_reader.ledger)["outcomes"]
        if outcome["status"] == "excluded"
    )
    (readiness_dir / "config.json").write_text(
        json.dumps(
            {
                "job_name": "stella-readiness-synthetic-v1",
                "jobs_dir": str(tmp_path),
                "n_attempts": 1,
                "n_concurrent_trials": 1,
                "tasks": [{"path": str(_canonical_readiness_path())}],
                "agents": [
                    {
                        "model_name": _CANDIDATE_MODELS[0],
                        "import_path": "stella_harbor:StellaAgent",
                        "name": None,
                    }
                ],
            }
        )
    )
    (readiness_dir / "result.json").write_text(
        json.dumps(
            {
                "id": readiness_outcome["job_id"],
                "started_at": readiness_outcome["started_at"],
                "finished_at": readiness_outcome["completed_at"],
                "n_total_trials": 1,
            }
        )
    )
    monkeypatch.setattr(
        launcher_module,
        "_replay_prior_stage_evidence",
        _REAL_PRIOR_STAGE_REPLAY,
    )

    def forbidden_exec(*_args: object) -> None:
        raise AssertionError("Harbor exec must not run with forged prior artifacts")

    with pytest.raises(RuntimeError, match="prior readiness replay failed"):
        exec_harbor_securely(
            command,
            environ=_claim_environment(binary),
            execvpe=forbidden_exec,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )
    assert not (tmp_path / "stella-tb21-calibration-20260721").exists()


def test_arbitrary_confirmatory_task_set_never_reserves_or_execs(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    job_name = "arbitrary-task-set"
    command = _confirmatory_command(tmp_path, job_name)
    public_reader = _public_reader(command, binary)
    arbitrary_digest = "9" * 64

    def rewrite_ledger(raw: bytes) -> tuple[bytes, str]:
        ledger = json.loads(raw)
        wrapper = next(
            item
            for item in ledger["intents"]
            if item["intent"]["stage"] == "confirmatory"
        )
        old_digest = wrapper["intent_sha256"]
        wrapper["intent"]["dataset"]["task_set_sha256"] = arbitrary_digest
        new_digest = _canonical_payload_sha256(wrapper["intent"])
        wrapper["intent_sha256"] = new_digest
        for publication in ledger["publications"]:
            if (
                publication["subject_type"] == "intent"
                and publication["subject_id"] == old_digest
            ):
                publication["subject_id"] = new_digest
        return (
            json.dumps(ledger, sort_keys=True, separators=(",", ":")).encode(),
            new_digest,
        )

    public_reader.snapshot_ledger, rewritten_digest = rewrite_ledger(
        public_reader.snapshot_ledger
    )
    public_reader.ledger, final_digest = rewrite_ledger(public_reader.ledger)
    assert final_digest == rewritten_digest
    command[command.index("--intent-sha256") + 1] = rewritten_digest
    body = json.loads(str(public_reader.comment["body"]))
    body["subject_id"] = rewritten_digest
    body["intent_sha256"] = rewritten_digest
    public_reader.comment["body"] = json.dumps(
        body, sort_keys=True, separators=(",", ":")
    )

    def forbidden_exec(*_args: object) -> None:
        raise AssertionError("Harbor exec must not run an arbitrary task set")

    with pytest.raises(RuntimeError, match="dataset identity is not frozen exactly"):
        exec_harbor_securely(
            command,
            environ=_claim_environment(binary),
            execvpe=forbidden_exec,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )
    assert not (tmp_path / job_name).exists()


@pytest.mark.parametrize(
    "comment_url",
    [
        "https://api.github.com/repos/macanderson/stella/issues/comments/456",
        "https://github.com/other/stella/issues/123#issuecomment-456",
        "https://github.com/macanderson/stella/issues/123#issuecomment-456?x=1",
    ],
)
def test_intent_comment_url_requires_exact_fixed_repository_html_form(
    tmp_path: Path, comment_url: str
) -> None:
    command = _confirmatory_command(tmp_path, "invalid-comment-url")
    command[command.index("--intent-comment-url") + 1] = comment_url
    with pytest.raises(RuntimeError, match="fixed-repository"):
        _validate_stage_shape(command)


def test_public_intent_waits_two_seconds_then_records_final_get(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "public-preflight-wait")
    public_reader = _public_reader(command, binary)
    runtime_identity = _runtime_identity(command, binary)
    current = datetime(2026, 1, 1, 0, 0, 1, tzinfo=timezone.utc)
    sleeps: list[float] = []

    def clock() -> datetime:
        return current

    def sleeper(seconds: float) -> None:
        nonlocal current
        sleeps.append(seconds)
        current = current.replace(second=2)

    attestation = _verify_public_intent(
        command,
        runtime_identity=runtime_identity,
        runtime_revalidator=lambda: runtime_identity,
        provider_credential="test-secret",
        management_credential=_TEST_MANAGEMENT_CREDENTIAL,
        reader=public_reader,
        provider_key_reader=_FakeProviderKeyReader(),
        clock=clock,
        sleeper=sleeper,
    )

    assert sleeps == [1.0]
    assert public_reader.comment_reads == 2
    assert attestation["safety_margin_seconds"] == 2
    assert attestation["safety_wait_completed_at_utc"].endswith("00:00:02.000000Z")
    assert attestation["final_comment_get_completed_at_utc"].endswith(
        "00:00:02.000000Z"
    )
    provider_snapshot = attestation["provider_key_live_snapshot"]
    assert set(provider_snapshot) == {
        "fingerprint_sha256",
        "label",
        "limit_usd",
        "usage_usd",
        "limit_remaining_usd",
        "nominal_planned_spend_usd",
        "nominal_remaining_after_usd",
        "total_credits_usd",
        "total_usage_usd",
        "available_credits_usd",
        "fetched_at_utc",
    }
    assert provider_snapshot["limit_usd"] == 180.0


def test_public_intent_rejects_existing_but_mismatched_runtime_source(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "source-mismatch")
    reader = _public_reader(command, binary)
    runtime_identity = _runtime_identity(command, binary)
    ledger = json.loads(reader.ledger)
    wrapper = next(
        item for item in ledger["intents"] if item["intent"]["stage"] == "confirmatory"
    )
    wrapper["intent"]["artifacts"]["source_commit"] = reader.subject_commit
    wrapper["intent_sha256"] = _canonical_payload_sha256(wrapper["intent"])
    command[command.index("--intent-sha256") + 1] = wrapper["intent_sha256"]

    with pytest.raises(RuntimeError, match="artifacts differ"):
        _validate_public_intent_ledger(
            json.dumps(ledger, sort_keys=True, separators=(",", ":")).encode(),
            reader.ledger,
            command=command,
            expected_digest=wrapper["intent_sha256"],
            subject_commit=reader.subject_commit,
            runtime_identity=runtime_identity,
            current_comment_created_at="2026-01-01T00:00:00Z",
            current_ledger_commit=reader.ledger_commit,
        )


def test_public_intent_rejects_unproven_commit_ancestry(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "bad-ancestry")
    reader = _public_reader(command, binary)

    def behind(base_sha: str, head_sha: str) -> dict[str, object]:
        return {
            "status": "behind",
            "ahead_by": 0,
            "base_commit": {"sha": base_sha},
            "merge_base_commit": {"sha": base_sha},
            "commits": [{"sha": head_sha}],
        }

    reader.compare_commits = behind  # type: ignore[method-assign]
    with pytest.raises(RuntimeError, match="strict .* ancestry"):
        _verify_fixture(command, binary, reader=reader)


def test_public_intent_requires_prior_stage_completed_outcome(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "missing-prior-outcome")
    reader = _public_reader(command, binary)
    ledger = json.loads(reader.ledger)
    ledger["outcomes"] = ledger["outcomes"][:1]
    reader.ledger = json.dumps(ledger, sort_keys=True, separators=(",", ":")).encode()
    snapshot = json.loads(reader.snapshot_ledger)
    snapshot["outcomes"] = snapshot["outcomes"][:1]
    reader.snapshot_ledger = json.dumps(
        snapshot, sort_keys=True, separators=(",", ":")
    ).encode()
    with pytest.raises(RuntimeError, match="paid outcomes"):
        _verify_fixture(command, binary, reader=reader)


def test_readiness_rejects_an_already_recorded_current_outcome(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    reader = _public_reader(command, binary)
    current_outcome = {
        "sequence": 5,
        "intent_sha256": reader.intent_sha256,
        "job_id": "00000000-0000-0000-0000-000000000005",
        "status": "excluded",
        "started_at": "2026-01-01T00:00:03Z",
        "completed_at": "2026-01-01T00:00:04Z",
        "artifact_tree_sha256": "e" * 64,
        "provider_usage_before_usd": 0.0,
        "provider_usage_after_usd": 0.0,
        "provider_usage_delta_usd": 0.0,
        "telemetry_cost_sum_usd": 0.0,
        "reconciliation_status": "reconciled",
        "reconciliation_tolerance_usd": 0.000001,
        "recorded_at": "2026-01-01T00:00:05Z",
    }
    for attribute in ("snapshot_ledger", "ledger"):
        payload = json.loads(getattr(reader, attribute))
        payload["outcomes"].append(current_outcome)
        setattr(
            reader,
            attribute,
            json.dumps(payload, sort_keys=True, separators=(",", ":")).encode(),
        )

    with pytest.raises(RuntimeError, match="already has a post-launch outcome"):
        _verify_fixture(command, binary, reader=reader)


def test_paid_stage_usage_must_be_continuous_across_the_same_key(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "usage-discontinuity")
    reader = _public_reader(command, binary)
    for attribute in ("snapshot_ledger", "ledger"):
        payload = json.loads(getattr(reader, attribute))
        readiness_outcome = payload["outcomes"][0]
        readiness_outcome["provider_usage_after_usd"] = 1.0
        readiness_outcome["provider_usage_delta_usd"] = 1.0
        readiness_outcome["telemetry_cost_sum_usd"] = 1.0
        setattr(
            reader,
            attribute,
            json.dumps(payload, sort_keys=True, separators=(",", ":")).encode(),
        )

    with pytest.raises(RuntimeError, match="provider usage is not continuous"):
        _verify_fixture(command, binary, reader=reader)


def test_public_intent_requires_exact_current_publication_record(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _calibration_command(tmp_path)
    reader = _public_reader(command, binary)
    ledger = json.loads(reader.ledger)
    ledger["publications"][-1]["published_at"] = "2026-01-01T00:00:01Z"
    reader.ledger = json.dumps(ledger, sort_keys=True, separators=(",", ":")).encode()

    with pytest.raises(RuntimeError, match="does not match its GitHub comment"):
        _verify_fixture(command, binary, reader=reader)


def test_public_intent_rejects_branch_change_after_publication_wait(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    reader = _public_reader(command, binary)
    reader.changed_branch = {"name": "main", "commit": {"sha": "7" * 40}}

    with pytest.raises(RuntimeError, match="publication commit changed"):
        _verify_fixture(command, binary, reader=reader)


def test_public_evidence_rejects_raw_provider_credential_bytes(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    reader = _public_reader(command, binary)
    reader.snapshot_ledger += b"test-secret"

    with pytest.raises(RuntimeError, match="contains the live provider credential"):
        _verify_fixture(command, binary, reader=reader)


def test_public_evidence_rejects_raw_management_credential_bytes(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    reader = _public_reader(command, binary)
    reader.snapshot_ledger += _TEST_MANAGEMENT_CREDENTIAL.encode()

    with pytest.raises(RuntimeError, match="contains the live provider credential"):
        _verify_fixture(command, binary, reader=reader)


def test_public_adapter_tree_rejects_extra_python_source(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    reader = _public_reader(command, binary)
    original_get_tree = reader.get_tree

    def extra_tree(commit_sha: str) -> dict[str, object]:
        tree = original_get_tree(commit_sha)
        entries = tree["tree"]
        assert isinstance(entries, list)
        entries.append(
            {
                "path": "bench/harbor_adapter/stella_harbor/injected.py",
                "type": "blob",
                "mode": "100644",
            }
        )
        return tree

    reader.get_tree = extra_tree  # type: ignore[method-assign]
    with pytest.raises(RuntimeError, match="adapter Python tree differs"):
        _verify_fixture(command, binary, reader=reader)


def test_public_runtime_rejects_source_bytes_different_from_local(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    reader = _public_reader(command, binary)
    original_get_content = reader.get_content

    def changed_content(path: str, commit_sha: str) -> bytes:
        raw = original_get_content(path, commit_sha)
        if path.endswith("stella_harbor/secure_launcher.py"):
            return raw + b"\n# injected\n"
        return raw

    reader.get_content = changed_content  # type: ignore[method-assign]
    with pytest.raises(RuntimeError, match="differs from local runtime bytes"):
        _verify_fixture(command, binary, reader=reader)


def test_public_intent_requires_account_credit_for_nominal_job(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "insufficient-credit")
    reader = _public_reader(command, binary)
    with pytest.raises(RuntimeError, match="account credit"):
        _verify_fixture(
            command,
            binary,
            reader=reader,
            provider=_FakeProviderKeyReader(credits=50.0),
        )


def test_public_intent_accepts_complete_current_key_shape_and_ignores_masked_label(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    reader = _public_reader(command, binary)
    provider = _FakeProviderKeyReader()

    attestation = _verify_fixture(command, binary, reader=reader, provider=provider)

    assert (
        attestation["provider_key_live_snapshot"]["label"]
        == "stella-tb21-dedicated-key-v1"
    )


def test_secure_launch_separates_runtime_and_management_key_reads(
    tmp_path: Path,
) -> None:
    benchmark_secret = "test-openrouter-benchmark-secret"
    management_secret = "test-openrouter-management-secret"
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    reader = _public_reader(command, binary, benchmark_secret)
    provider = _BoundProviderKeyReader(benchmark_secret, management_secret)
    observed: dict[str, object] = {}

    def fake_execvpe(
        _executable: str, _command: list[str], child_env: dict[str, str]
    ) -> None:
        observed["environment"] = child_env
        observed["bundle"] = read_anonymous_credential_bundle(
            child_env[HOST_CREDENTIAL_BUNDLE_FD_ENV]
        )

    with pytest.raises(RuntimeError, match="unexpectedly returned"):
        exec_harbor_securely(
            command,
            environ=_claim_environment(
                binary,
                OPENROUTER_API_KEY=benchmark_secret,
                OPENROUTER_MANAGEMENT_API_KEY=management_secret,
            ),
            execvpe=fake_execvpe,
            public_intent_reader=reader,
            provider_key_reader=provider,
        )

    assert provider.calls == [
        ("key", benchmark_secret),
        ("key_record", management_secret, provider.fingerprint),
        ("credits", management_secret),
    ]
    assert observed["bundle"] == {"OPENROUTER_API_KEY": benchmark_secret}
    child_env = observed["environment"]
    assert isinstance(child_env, dict)
    assert all(
        benchmark_secret not in value and management_secret not in value
        for value in child_env.values()
    )


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("hash", "0" * 64),
        ("name", "wrong-key-name"),
        ("disabled", True),
        ("include_byok_in_limit", False),
        ("limit_reset", "daily"),
        ("limit", 179.0),
        ("usage", 1.0),
        ("limit_remaining", 179.0),
    ],
)
def test_secure_launch_rejects_unbound_management_key_record(
    tmp_path: Path, field: str, value: object
) -> None:
    benchmark_secret = "test-openrouter-benchmark-secret"
    management_secret = "test-openrouter-management-secret"
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    provider = _BoundProviderKeyReader(benchmark_secret, management_secret)
    provider.key_record["data"][field] = value

    with pytest.raises(RuntimeError, match="management key record"):
        exec_harbor_securely(
            command,
            environ=_claim_environment(
                binary,
                OPENROUTER_API_KEY=benchmark_secret,
                OPENROUTER_MANAGEMENT_API_KEY=management_secret,
            ),
            execvpe=lambda *_args: None,
            public_intent_reader=_public_reader(command, binary, benchmark_secret),
            provider_key_reader=provider,
        )


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("is_management_key", True),
        ("is_provisioning_key", True),
        ("limit_reset", "monthly"),
    ],
)
def test_secure_launch_rejects_nonruntime_current_key_posture(
    tmp_path: Path, field: str, value: object
) -> None:
    benchmark_secret = "test-openrouter-benchmark-secret"
    management_secret = "test-openrouter-management-secret"
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    provider = _BoundProviderKeyReader(benchmark_secret, management_secret)
    provider.key["data"][field] = value

    with pytest.raises(RuntimeError, match="normal dedicated hard-limit key"):
        exec_harbor_securely(
            command,
            environ=_claim_environment(
                binary,
                OPENROUTER_API_KEY=benchmark_secret,
                OPENROUTER_MANAGEMENT_API_KEY=management_secret,
            ),
            execvpe=lambda *_args: None,
            public_intent_reader=_public_reader(command, binary, benchmark_secret),
            provider_key_reader=provider,
        )


def test_public_intent_rejects_credits_response_shape_extensions(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    reader = _public_reader(command, binary)
    provider = _FakeProviderKeyReader()
    provider.credits["data"]["unexpected"] = 0.0

    with pytest.raises(RuntimeError, match="exact v2 schema"):
        _verify_fixture(command, binary, reader=reader, provider=provider)


def test_public_intent_rehashes_runtime_after_final_github_get(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "runtime-changed")
    reader = _public_reader(command, binary)
    runtime_identity = _runtime_identity(command, binary)
    changed = deepcopy(runtime_identity)
    changed["binary_sha256"] = "0" * 64
    with pytest.raises(RuntimeError, match="runtime changed"):
        _verify_public_intent(
            command,
            runtime_identity=runtime_identity,
            runtime_revalidator=lambda: changed,
            provider_credential="test-secret",
            management_credential=_TEST_MANAGEMENT_CREDENTIAL,
            reader=reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )


def test_public_intent_rejects_confirmatory_manifest_task_digest_drift(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "manifest-task-drift")
    reader = _public_reader(command, binary)
    manifest = json.loads(reader.manifest)
    manifest["dataset"]["task_set_sha256"] = "0" * 64
    reader.manifest = json.dumps(
        manifest, sort_keys=True, separators=(",", ":")
    ).encode()
    ledger = json.loads(reader.ledger)
    ledger["preregistrations"][-1]["study_manifest_sha256"] = hashlib.sha256(
        reader.manifest
    ).hexdigest()
    reader.ledger = json.dumps(ledger, sort_keys=True, separators=(",", ":")).encode()
    snapshot = json.loads(reader.snapshot_ledger)
    snapshot["preregistrations"][-1]["study_manifest_sha256"] = hashlib.sha256(
        reader.manifest
    ).hexdigest()
    reader.snapshot_ledger = json.dumps(
        snapshot, sort_keys=True, separators=(",", ":")
    ).encode()
    with pytest.raises(RuntimeError, match="manifest differs"):
        _verify_fixture(command, binary, reader=reader)


def test_public_intent_rejects_manifest_schema_extensions(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "manifest-schema-extension")
    reader = _public_reader(command, binary)
    manifest = json.loads(reader.manifest)
    manifest["unregistered_extension"] = True
    reader.manifest = json.dumps(
        manifest, sort_keys=True, separators=(",", ":")
    ).encode()
    manifest_sha256 = hashlib.sha256(reader.manifest).hexdigest()
    for attribute in ("snapshot_ledger", "ledger"):
        payload = json.loads(getattr(reader, attribute))
        payload["preregistrations"][-1]["study_manifest_sha256"] = manifest_sha256
        setattr(
            reader,
            attribute,
            json.dumps(payload, sort_keys=True, separators=(",", ":")).encode(),
        )

    with pytest.raises(RuntimeError, match="exact v2 schema"):
        _verify_fixture(command, binary, reader=reader)


def test_public_intent_rejects_clock_rollback_after_final_get(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _confirmatory_command(tmp_path, "clock-rollback")
    reader = _public_reader(command, binary)
    moments = iter(
        [
            datetime(2026, 1, 1, 0, 0, 2, tzinfo=timezone.utc),
            datetime(2026, 1, 1, 0, 0, 2, tzinfo=timezone.utc),
            datetime(2026, 1, 1, 0, 0, 1, tzinfo=timezone.utc),
        ]
    )
    with pytest.raises(RuntimeError, match="clock rolled back"):
        _verify_fixture(command, binary, reader=reader, clock=lambda: next(moments))


def test_default_github_reader_uses_no_auth_or_ambient_proxy(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    observed: dict[str, object] = {}

    class _Response:
        status = 200

        def __enter__(self):
            return self

        def __exit__(self, *_args: object) -> None:
            return None

        def getcode(self) -> int:
            return 200

        def geturl(self) -> str:
            return "https://api.github.com/repos/macanderson/stella"

        def read(self, _limit: int) -> bytes:
            return json.dumps(
                {
                    "full_name": "macanderson/stella",
                    "url": "https://api.github.com/repos/macanderson/stella",
                    "html_url": "https://github.com/macanderson/stella",
                    "private": False,
                }
            ).encode()

    class _Opener:
        def open(self, request: object, *, timeout: int):
            observed["request"] = request
            observed["timeout"] = timeout
            return _Response()

    def build_opener(*handlers: object) -> _Opener:
        observed["handlers"] = handlers
        return _Opener()

    monkeypatch.setenv("HTTPS_PROXY", "http://ambient-proxy.invalid")
    monkeypatch.setenv("GITHUB_TOKEN", "ambient-auth-must-not-be-used")
    monkeypatch.setattr(
        "stella_harbor.secure_launcher._fixed_system_tls_context", lambda: object()
    )
    monkeypatch.setattr(
        "stella_harbor.secure_launcher.urllib.request.build_opener", build_opener
    )

    reader = _AnonymousGitHubPublicIntentReader()
    assert reader.get_repository()["private"] is False

    request = observed["request"]
    assert not request.has_header("Authorization")
    assert "ambient-auth-must-not-be-used" not in repr(request.header_items())
    handlers = observed["handlers"]
    proxy_handlers = [handler for handler in handlers if hasattr(handler, "proxies")]
    assert len(proxy_handlers) == 1
    assert proxy_handlers[0].proxies == {}
    assert any(type(handler).__name__ == "_NoRedirectHandler" for handler in handlers)


def test_provider_readers_disable_redirects_and_ambient_proxy(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    observed: dict[str, object] = {"requests": []}

    class _Response:
        status = 200

        def __init__(self, url: str) -> None:
            self.url = url

        def __enter__(self):
            return self

        def __exit__(self, *_args: object) -> None:
            return None

        def getcode(self) -> int:
            return 200

        def geturl(self) -> str:
            return self.url

        def read(self, _limit: int) -> bytes:
            return b'{"data":{}}'

    class _Opener:
        def open(self, request: object, *, timeout: int):
            requests = observed["requests"]
            assert isinstance(requests, list)
            requests.append(request)
            observed["timeout"] = timeout
            return _Response(request.full_url)

    def build_opener(*handlers: object) -> _Opener:
        observed["handlers"] = handlers
        return _Opener()

    monkeypatch.setenv("HTTPS_PROXY", "http://ambient-proxy.invalid")
    monkeypatch.setattr(
        "stella_harbor.secure_launcher._fixed_system_tls_context", lambda: object()
    )
    monkeypatch.setattr(
        "stella_harbor.secure_launcher.urllib.request.build_opener", build_opener
    )
    reader = _OpenRouterProviderKeyReader()
    fingerprint = "a" * 64
    assert reader.get_key("test-benchmark-secret") == {"data": {}}
    assert reader.get_key_record("test-management-secret", fingerprint) == {"data": {}}
    assert reader.get_credits("test-management-secret") == {"data": {}}
    requests = observed["requests"]
    assert isinstance(requests, list)
    assert [request.full_url for request in requests] == [
        "https://openrouter.ai/api/v1/key",
        f"https://openrouter.ai/api/v1/keys/{fingerprint}",
        "https://openrouter.ai/api/v1/credits",
    ]
    assert [request.get_header("Authorization") for request in requests] == [
        "Bearer test-benchmark-secret",
        "Bearer test-management-secret",
        "Bearer test-management-secret",
    ]
    handlers = observed["handlers"]
    assert any(type(handler).__name__ == "_NoRedirectHandler" for handler in handlers)
    proxy_handlers = [handler for handler in handlers if hasattr(handler, "proxies")]
    assert len(proxy_handlers) == 1
    assert proxy_handlers[0].proxies == {}


def test_claim_command_requires_one_lowercase_paid_intent_sha(tmp_path: Path) -> None:
    base = [
        "harbor",
        "run",
        "--agent-import-path=stella_harbor:StellaAgent",
        "--env=docker",
        "--model=openrouter/model",
        "--job-name=intent-bound",
        f"--jobs-dir={tmp_path}",
    ]
    with pytest.raises(RuntimeError, match="lowercase 64-hex --intent-sha256"):
        _validate_claim_command(base, {"OPENROUTER_API_KEY": "test-secret"})
    with pytest.raises(RuntimeError, match="lowercase 64-hex --intent-sha256"):
        _validate_claim_command(
            [*base, "--intent-sha256", "A" * 64],
            {"OPENROUTER_API_KEY": "test-secret"},
        )

    command = [*base, f"--intent-sha256={_TEST_INTENT_SHA256}"]
    _validate_claim_command(command, {"OPENROUTER_API_KEY": "test-secret"})
    assert _intent_sha256(command) == _TEST_INTENT_SHA256
    assert _harbor_command(command) == base


def test_second_secure_launch_of_same_job_is_refused(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    public_reader = _public_reader(command, binary)
    environment = _claim_environment(binary)

    def returned_exec(*_args: object) -> None:
        return None

    with pytest.raises(RuntimeError, match="unexpectedly returned"):
        exec_harbor_securely(
            command,
            environ=environment,
            execvpe=returned_exec,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )
    with pytest.raises(FileExistsError, match="already-reserved paid intent"):
        exec_harbor_securely(
            command,
            environ=environment,
            execvpe=returned_exec,
            public_intent_reader=public_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )

    receipt_path = tmp_path / "stella-readiness-synthetic-v1" / LAUNCH_RECEIPT_FILENAME
    assert receipt_path.is_file()
    assert stat.S_IMODE(receipt_path.stat().st_mode) == 0o600


def test_same_intent_cannot_replay_under_a_different_jobs_root(tmp_path: Path) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    first_root = tmp_path / "jobs-a"
    second_root = tmp_path / "jobs-b"
    first = _readiness_command(first_root)
    first_reader = _public_reader(first, binary)
    second = deepcopy(first)
    second[second.index("--jobs-dir") + 1] = str(second_root)
    second_reader = _public_reader(second, binary)

    with pytest.raises(RuntimeError, match="unexpectedly returned"):
        exec_harbor_securely(
            first,
            environ=_claim_environment(binary),
            execvpe=lambda *_args: None,
            public_intent_reader=first_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )
    with pytest.raises(FileExistsError, match="changing --jobs-dir cannot replay"):
        exec_harbor_securely(
            second,
            environ=_claim_environment(binary),
            execvpe=lambda *_args: None,
            public_intent_reader=second_reader,
            provider_key_reader=_FakeProviderKeyReader(),
        )
    assert not (second_root / "stella-readiness-synthetic-v1").exists()


def test_concurrent_paid_intent_reservations_allow_exactly_one(
    tmp_path: Path,
) -> None:
    binary = _write_claim_elf(tmp_path / "stella")
    first = _readiness_command(tmp_path / "parallel-a")
    reader = _public_reader(first, binary)
    runtime_identity = _runtime_identity(first, binary)
    attestation = _verify_public_intent(
        first,
        runtime_identity=runtime_identity,
        runtime_revalidator=lambda: runtime_identity,
        provider_credential="test-secret",
        management_credential=_TEST_MANAGEMENT_CREDENTIAL,
        reader=reader,
        provider_key_reader=_FakeProviderKeyReader(),
    )
    host_preflight = _host_preflight_fixture(first, reader, attestation)

    def reserve(command: list[str]) -> Path | BaseException:
        try:
            return _reserve_fresh_job(
                command,
                [_CANDIDATE_MODELS[0]],
                attestation,
                host_preflight,
                ("test-secret", _TEST_MANAGEMENT_CREDENTIAL),
            )
        except BaseException as exc:  # test captures the losing atomic reservation
            return exc

    with ThreadPoolExecutor(max_workers=2) as executor:
        results = list(executor.map(reserve, (first, deepcopy(first))))

    assert sum(isinstance(result, Path) for result in results) == 1
    errors = [result for result in results if isinstance(result, BaseException)]
    assert len(errors) == 1
    assert isinstance(errors[0], FileExistsError)
    assert "already-reserved paid intent" in str(errors[0])


def test_harbor_accepts_precreated_job_dir_with_attestations(tmp_path: Path) -> None:
    from harbor.job import Job
    from harbor.models.job.config import JobConfig

    binary = _write_claim_elf(tmp_path / "stella")
    command = _readiness_command(tmp_path)
    public_reader = _public_reader(command, binary)
    runtime_identity = _runtime_identity(command, binary)
    attestation = _verify_public_intent(
        command,
        runtime_identity=runtime_identity,
        runtime_revalidator=lambda: runtime_identity,
        provider_credential="test-secret",
        management_credential=_TEST_MANAGEMENT_CREDENTIAL,
        reader=public_reader,
        provider_key_reader=_FakeProviderKeyReader(),
    )
    host_preflight = _host_preflight_fixture(command, public_reader, attestation)
    _reserve_fresh_job(
        command,
        [_CANDIDATE_MODELS[0]],
        attestation,
        host_preflight,
        ("test-secret", _TEST_MANAGEMENT_CREDENTIAL),
    )

    job = Job(
        JobConfig(job_name="stella-readiness-synthetic-v1", jobs_dir=tmp_path),
        _task_configs=[],
        _metrics={},
    )
    try:
        assert job.is_resuming is False
        assert job.job_dir == tmp_path / "stella-readiness-synthetic-v1"
        assert (job.job_dir / LAUNCH_RECEIPT_FILENAME).is_file()
        assert (job.job_dir / host_module.HOST_ATTESTATION_FILENAME).is_file()
    finally:
        job._close_logger_handlers()


def test_claim_command_rejects_key_material_or_assignments_in_argv() -> None:
    base = [
        "harbor",
        "run",
        "--agent-import-path=stella_harbor:StellaAgent",
        "--model=openrouter/model",
    ]
    with pytest.raises(RuntimeError, match="credential material"):
        _validate_claim_command(
            [*base, "--job-name=prefix-test-secret-suffix"],
            {"OPENROUTER_API_KEY": "test-secret"},
        )
    with pytest.raises(RuntimeError, match="credential assignments"):
        _validate_claim_command(
            [*base, "OPENROUTER_API_KEY=not-the-selected-value"],
            {"OPENROUTER_API_KEY": "test-secret"},
        )


def test_claim_command_rejects_management_key_alias_in_argv(tmp_path: Path) -> None:
    management_secret = "test-openrouter-management-secret"
    command = _readiness_command(tmp_path)
    comment_index = command.index("--intent-comment-url") + 1
    command[comment_index] = f"{command[comment_index]}?alias={management_secret}"

    with pytest.raises(RuntimeError, match="credential material"):
        _validate_claim_command(
            command,
            {
                "OPENROUTER_API_KEY": "test-openrouter-benchmark-secret",
                "OPENROUTER_MANAGEMENT_API_KEY": management_secret,
            },
        )


def test_only_bundle_descriptor_remains_inheritable(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    bundle = create_anonymous_credential_bundle(
        {"OPENROUTER_API_KEY": "test-openrouter-secret"}
    )
    read_fd, write_fd = os.pipe()
    try:
        os.set_inheritable(read_fd, True)
        os.set_inheritable(write_fd, True)
        monkeypatch.setattr(
            "stella_harbor.secure_launcher._open_descriptor_numbers",
            lambda: iter((0, 1, 2, bundle.fileno(), read_fd, write_fd)),
        )

        mark_only_bundle_fd_inheritable(bundle.fileno())

        assert os.get_inheritable(bundle.fileno()) is True
        assert os.get_inheritable(read_fd) is False
        assert os.get_inheritable(write_fd) is False
    finally:
        os.close(read_fd)
        os.close(write_fd)
        bundle.close()
