"""Audit Harbor jobs and run Stella's preregistered Terminal-Bench analysis."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import random
import re
import stat
import statistics
from collections import Counter, defaultdict
from collections.abc import Iterable, Sequence
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import github_public_timing
from github_public_timing import (
    AUDIT_SCHEMA_VERSION as PUBLIC_TIMING_AUDIT_SCHEMA_VERSION,
)
from github_public_timing import FIXED_REPOSITORY, LivePublicTimingAudit
from github_public_timing import verify_public_timing as verify_github_public_timing

try:
    from harbor.models.trajectories import Trajectory
except ImportError:  # pragma: no cover - the pinned runtime dependency supplies it.
    Trajectory = None  # type: ignore[assignment,misc]


ANALYSIS_VERSION = "stella-tb21-analysis-v1"
STUDY_MANIFEST_VERSION = "stella-tb21-study-manifest-v6"
ANALYSIS_CONTENT_SHA256 = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
PUBLIC_TIMING_CONTENT_SHA256 = hashlib.sha256(
    Path(github_public_timing.__file__).read_bytes()
).hexdigest()
DEFAULT_BOOTSTRAP_SEED = 20260721
DEFAULT_BOOTSTRAP_DRAWS = 50_000
DEFAULT_EXPECTED_TASKS = 89
DEFAULT_TRIALS_PER_TASK = 5
DEFAULT_INFERENCE_TASKS = 79
CANONICAL_DATASET_NAME = "terminal-bench/terminal-bench-2-1"
CANONICAL_DATASET_REF = (
    "sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a"
)
CANONICAL_AGENT_IMPORT_PATH = "stella_harbor:StellaAgent"
CANONICAL_OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1"
CANONICAL_PROVIDER_ROUTE_POLICY = "openrouter-auto"
CANONICAL_HOST_CREDENTIAL_SOURCE = "anonymous-seekable-fd-v1"
CANONICAL_HOST_CREDENTIAL_NAME = "OPENROUTER_API_KEY"
CANONICAL_HARBOR_VERSION = "0.6.1"
CANONICAL_ENGINE_POSTURE_VERSION = "stella-tb21-engine-posture-v1"


def canonical_engine_posture(model: str) -> tuple[dict[str, Any], str, str]:
    """Return the registered launcher-owned engine config and byte hash."""
    selected_model = model.strip()
    if not selected_model or "/" not in selected_model:
        raise ValueError("model must be a non-empty provider/model spec")
    posture: dict[str, Any] = {
        "default_model": selected_model,
        "allowed_models": [selected_model],
        "auto_mode": "off",
        "effort_auto": "off",
        "reasoning_auto": "off",
        "agents": {
            "default": {"effort": "high", "reasoning": "on"},
            "worker": {"effort": "high", "reasoning": "on"},
            "judge": {"effort": "high", "reasoning": "on"},
            "triage": {"effort": "low", "reasoning": "off"},
        },
    }
    normalized = json.dumps(
        posture,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    )
    return (
        posture,
        normalized,
        hashlib.sha256(normalized.encode("utf-8")).hexdigest(),
    )


CALIBRATION_SEED = 20260721
CALIBRATION_TASKS = (
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
PRIMARY_MODEL = "openrouter/z-ai/glm-5.1"
PRIMARY_CALL_MODELS = ("z-ai/glm-5.1",)
CALIBRATION_MODEL_ORDER = (
    "openrouter/deepseek/deepseek-v4-pro",
    "openrouter/z-ai/glm-5.2",
    "openrouter/x-ai/grok-4.5",
)
CALIBRATION_CALL_MODELS: dict[str, tuple[str, ...]] = {
    model: (model.removeprefix("openrouter/"),) for model in CALIBRATION_MODEL_ORDER
}
REGISTERED_CALL_MODELS: dict[str, tuple[str, ...]] = {
    PRIMARY_MODEL: PRIMARY_CALL_MODELS,
    **CALIBRATION_CALL_MODELS,
}
CALIBRATION_JOB_NAME = "stella-tb21-calibration-20260721"
CALIBRATION_N_CONCURRENT_TRIALS = 3
CALIBRATION_ATTEMPTS_PER_MODEL_TASK = 2
CALIBRATION_EXPECTED_TRIALS = (
    len(CALIBRATION_TASKS)
    * len(CALIBRATION_MODEL_ORDER)
    * CALIBRATION_ATTEMPTS_PER_MODEL_TASK
)
CALIBRATION_MINIMUM_PASSES = 14
CALIBRATION_PROJECTION_TRIALS = 445
CALIBRATION_SPEND_LIMIT_USD = 75.0
CONFIRMATORY_N_CONCURRENT_TRIALS = 1
SECURE_LAUNCH_RECEIPT_FILENAME = "stella-secure-launch-receipt.json"
SECURE_LAUNCH_RECEIPT_SCHEMA = "stella-harbor-secure-launch-receipt-v2"
HOST_ATTESTATION_FILENAME = "stella-host-attestation.json"
HOST_REPORT_SCHEMA = "stella-tb21-host-report-v1"
HOST_LAUNCH_BINDING_SCHEMA = "stella-tb21-host-launch-binding-v1"
HOST_REPORT_PATH_PREFIX = "bench/evidence/host-attestations"
HOST_MIN_VCPUS = 4
HOST_MIN_MEMORY_BYTES = 31 * 1024**3
HOST_MIN_FREE_DISK_BYTES = 150 * 1024**3
HOST_MAX_REPORT_AGE_SECONDS = 15 * 60
PUBLIC_INTENT_ATTESTATION_SCHEMA = "stella-harbor-public-intent-preflight-v2"
PUBLIC_INTENT_VERIFICATION_MODE = "anonymous-get-v1"
GITHUB_ATTESTATION_SCHEMA = "stella-tb21-github-attestation-v2"
FIXED_STUDY_ID = "stella-tb21-scientific-study-v1"
FIXED_RUN_LEDGER_PATH = "bench/evidence/stella-tb21-run-ledger.json"
PUBLIC_INTENT_SAFETY_MARGIN_SECONDS = 2
PUBLIC_INTENT_ATTESTATION_FIELDS = frozenset(
    {
        "schema_version",
        "verification_mode",
        "repository",
        "repository_private",
        "issue_number",
        "issue_url",
        "issue_title",
        "issue_author_login",
        "issue_author_association",
        "comment_id",
        "comment_url",
        "comment_author_login",
        "comment_author_association",
        "server_created_at",
        "server_updated_at",
        "body_sha256",
        "github_attestation_schema_version",
        "study_id",
        "subject_type",
        "subject_id",
        "kind",
        "subject_commit",
        "ledger_commit",
        "ledger_path",
        "intent_sha256",
        "safety_margin_seconds",
        "safety_wait_completed_at_utc",
        "final_comment_get_completed_at_utc",
        "ledger_sha256",
        "subject_commit_verified",
        "ledger_commit_verified",
        "source_commit_verified",
        "strict_ancestry_verified",
        "prior_stage_outcome",
        "runtime_identity",
        "provider_key_live_snapshot",
        "runtime_revalidated_after_final_get",
        "runtime_revalidated_at_utc",
    }
)
PUBLIC_INTENT_RUNTIME_IDENTITY_FIELDS = frozenset(
    {
        "binary_sha256",
        "source_commit",
        "agent_version",
        "adapter_version",
        "adapter_sha256",
        "analysis_sha256",
        "public_timing_sha256",
        "harbor_version",
        "harbor_sha256",
        "engine_posture_sha256_by_model",
        "base_url",
        "provider_route_policy",
        "disable_reflection",
        "provider_key_fingerprint_sha256",
    }
)
PUBLIC_INTENT_PRIOR_OUTCOME_FIELDS = frozenset(
    {"stage", "intent_sha256", "status", "completed_at", "recorded_at"}
)
PUBLIC_INTENT_PROVIDER_SNAPSHOT_FIELDS = frozenset(
    {
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
)
PUBLIC_INTENT_COMMENT_URL_RE = re.compile(
    r"https://github\.com/macanderson/stella/issues/(?P<issue>[1-9][0-9]*)"
    r"#issuecomment-(?P<comment>[1-9][0-9]*)"
)
HOST_REPORT_FIELDS = frozenset(
    {
        "schema_version",
        "study_id",
        "intent_sha256",
        "stage",
        "job_name",
        "captured_at_utc",
        "host_fingerprint_sha256",
        "requirements",
        "observed",
        "checks",
    }
)
HOST_OBSERVED_FIELDS = frozenset(
    {
        "os",
        "architecture",
        "cpu",
        "memory",
        "disk",
        "docker",
        "running_container_ids",
    }
)
HOST_OS_FIELDS = frozenset(
    {
        "system",
        "kernel_release",
        "distribution_id",
        "distribution_version_id",
        "distribution_pretty_name",
    }
)
HOST_CPU_FIELDS = frozenset({"effective_vcpus", "model"})
HOST_MEMORY_FIELDS = frozenset({"total_bytes"})
HOST_DISK_FIELDS = frozenset({"probe_path", "total_bytes", "used_bytes", "free_bytes"})
HOST_DOCKER_FIELDS = frozenset(
    {
        "client_version",
        "client_api_version",
        "server_version",
        "server_api_version",
        "server_os",
        "server_architecture",
        "reported_running_containers",
    }
)
HOST_CHECK_FIELDS = frozenset(
    {
        "native_linux_x86_64",
        "minimum_vcpus",
        "minimum_memory",
        "minimum_free_disk",
        "docker_native_linux_x86_64",
        "zero_running_containers",
        "all_passed",
    }
)
HOST_SNAPSHOT_FIELDS = frozenset(
    {"captured_at_utc", "host_fingerprint_sha256", "observed", "checks"}
)
HOST_BINDING_FIELDS = frozenset(
    {
        "schema_version",
        "study_id",
        "intent_sha256",
        "stage",
        "job_name",
        "public_report",
        "launch_receipt_sha256",
        "public_report_payload",
        "live_recheck",
    }
)
HOST_PUBLIC_REFERENCE_FIELDS = frozenset(
    {"repository", "commit", "path", "sha256", "fetched_at_utc"}
)
HOST_REQUIREMENTS = {
    "system": "Linux",
    "architecture": "x86_64",
    "min_vcpus": HOST_MIN_VCPUS,
    "min_memory_bytes": HOST_MIN_MEMORY_BYTES,
    "min_free_disk_bytes": HOST_MIN_FREE_DISK_BYTES,
    "max_running_containers_before_launch": 0,
}
RUN_LEDGER_SCHEMA = "stella-tb21-run-ledger-v2"
READINESS_JOB_NAME = "stella-readiness-synthetic-v1"
READINESS_TASK_NAME = "stella/synthetic-adapter-sentinel"
READINESS_TASK = "synthetic-adapter-sentinel"
READINESS_TASK_SHA256 = (
    "05a040c7df0fd77f66f533ba023cb5f16e2dd0f89957440b099374210e475ad6"
)
READINESS_TASK_REF = f"sha256:{READINESS_TASK_SHA256}"
READINESS_TASK_RELATIVE_PATH = "bench/readiness/synthetic-adapter-sentinel"
RUN_SPEND_LIMIT_USD = 200.0
DEDICATED_KEY_LIMIT_USD = 180.0
MAX_RECONCILIATION_TOLERANCE_USD = 0.01
EXTERNAL_PUBLIC_TIMING_AUDIT_REASON = (
    "external_public_timing_audit_required: offline analysis cannot verify GitHub "
    "server-stamped publication time, unedited attestation bodies, or commit ancestry"
)
SECURE_LAUNCH_CONTROLS = {
    "command": "harbor-run-only",
    "agent_import_path": CANONICAL_AGENT_IMPORT_PATH,
    "environment": "docker",
    "credential_source": CANONICAL_HOST_CREDENTIAL_SOURCE,
    "fresh_job_directory": "atomic-create",
    "resume": "forbidden",
    "in_run_publication": "forbidden",
    "filesystem_settings": "disabled",
    "filesystem_credentials": "disabled",
    "project_env_files": "disabled",
    "subprocess_credential_scrub": "enabled",
    "harbor_clock_timezone": "UTC",
}
CALIBRATION_TASK_CHECKSUMS = {
    "fix-git": "d3220d70bc668ec6f4034fab51e62873dff724a61f824d764fd201d6f5e7a88a",
    "filter-js-from-html": (
        "53d156752f8706d9e88c598e0e562ddacf52ab478c7655352e939b8f44a5d13b"
    ),
    "kv-store-grpc": (
        "901a4dd5c3078d5155043875a0d07f3583f462c73f919b52855c025e680b1edd"
    ),
    "large-scale-text-editing": (
        "e2851ab29f9dc799ae4ba2ad8f7495ccd1625476a3954dde8cec09771e41208a"
    ),
    "regex-log": ("31dc6115c061b96539a5287090ce41a7a89d3201c291b9b843bd70e416f35c39"),
    "schemelike-metacircular-eval": (
        "e4525988ae20585b6b90a2f1af2fcc9d18d6363d3cfcdd7be5305199428883d2"
    ),
    "sqlite-with-gcov": (
        "243352f9b742b39a4a21fe51a1d88c04cc45ede977e83cc968c9becfaf3f1de2"
    ),
    "bn-fit-modify": (
        "de87a0c253c75b7dfbc68a2fcf120d5bd77b1df632c7469e3d0a3b74321d26c2"
    ),
    "make-mips-interpreter": (
        "929f50bc438e551fb6c09fdc9f91ab528e266bc57a79e52c90d398738a553a72"
    ),
    "train-fasttext": (
        "f521c0d2f312c54d2ba22574c94cfe2e646969cc744974ab9faf68aead2bc1b2"
    ),
}
CALIBRATION_TASK_REFS = {
    "fix-git": (
        "sha256:16948b980df9d96de616a205f5acca1c5d395de83ff4f8ffabcafacb93226f2e"
    ),
    "filter-js-from-html": (
        "sha256:2d1496b6fc62adeccdba7a56f4bc24e5ef265840434d2011234ed20b6c240759"
    ),
    "kv-store-grpc": (
        "sha256:973c5d4c111fb61a344457936f1c36400acd2d9e44389e7b319586fe23a7a307"
    ),
    "large-scale-text-editing": (
        "sha256:1f1cddc3df15e452fe2d3c6928f6b1e5b5330a7ae67cab373a0d089ea7d334a2"
    ),
    "regex-log": (
        "sha256:802c16cfd132e6c457529cb864be5a757c1b23b6cadc57f2d01983cb0110292a"
    ),
    "schemelike-metacircular-eval": (
        "sha256:58130c2166c3115276dc8592f358e326ff2d81ea852e3d88636c82fd1dff57e6"
    ),
    "sqlite-with-gcov": (
        "sha256:9f9bd57fbf9f4831e9031755e83aea6b9d60d2b2d54e8a12d48cff4dca3c231d"
    ),
    "bn-fit-modify": (
        "sha256:b5f9644970c17ad9ddb46b7266f7bcd87c761d77d7e6f55d7cfe7284d5ff66e9"
    ),
    "make-mips-interpreter": (
        "sha256:41a55da0abec5d7b32a0c2321f8b18e84000ca8074ae62c6874d6ed4a3a1cd3c"
    ),
    "train-fasttext": (
        "sha256:460fc0818971ec83545a76805267b65459128fad52e68c26a199a0d74022badb"
    ),
}
REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS = (
    "9b704487-9d21-46a7-8103-e5396cb7d4ea",
    "0c44d9ee-4389-4c7a-8445-ea4be2404115",
    "c5686c41-1d2d-41cf-a275-177c2e6878b3",
    "37ee4276-8595-4ff9-8507-be21adb891cc",
    "7e59ed1e-2abe-40b9-bf7e-6b24c7f9a350",
)

COMPARATOR_PUBLIC_JOB_ID = "fd8707bb-51e8-56fa-8e46-769a82a531ae"
COMPARATOR_SUBMISSION_REPOSITORY = (
    "https://github.com/harbor-framework/terminal-bench-2-1"
)
COMPARATOR_SUBMISSION_COMMIT = "327a5a0b2ee4675871dc57e1d53fff2d2cf974e1"
COMPARATOR_SUBMISSION_PATH = (
    "leaderboard/submissions/2026-05-01-glm-5-1-max-claude-code.json"
)
COMPARATOR_SUBMISSION_SHA256 = (
    "36d20c181be246dc55965bf4320a3005f292c737f31511bbde19ba1808a2bd2c"
)
COMPARATOR_MANIFEST_SHA256 = (
    "7963a7af2b306fd4b6e82963fdadf9374e701ec16f47b194300e2843c8002a76"
)
# This is filled from the canonical normalized trial rows below. Keeping it a
# constant means a comparator change requires an explicit analyzer revision,
# not merely a rewritten study manifest.
COMPARATOR_TRIAL_DATA_SHA256 = (
    "f7b916c7d3028c62003bb12eeb1fff3df0bb41a82ce21ba6e59b3a1b50139a99"
)
COMPARATOR_EXPECTED_ROWS = 445
COMPARATOR_EXPECTED_TASKS = 89
COMPARATOR_EXPECTED_ATTEMPTS = 5
COMPARATOR_EXPECTED_REWARD_TOTAL = 261.0
COMPARATOR_EXPECTED_TOKEN_TOTAL = 398_783_761
COMPARATOR_AGENT_NAME = "claude-code"
COMPARATOR_AGENT_VERSION = "2.1.123"
COMPARATOR_MODEL = "glm-5.1"
COMPARATOR_REASONING_EFFORT = "max"
FAMILY_ALPHA = 0.05
REGISTERED_DIMENSIONS = 3
LOWER_BOUND_ALPHA = FAMILY_ALPHA / REGISTERED_DIMENSIONS
WIN_THRESHOLD = 0.10
ACCURACY_POINT_THRESHOLD = (
    COMPARATOR_EXPECTED_REWARD_TOTAL / COMPARATOR_EXPECTED_ROWS
) * (1.0 + WIN_THRESHOLD)
ACCURACY_MIN_BINARY_PASSES = math.ceil(
    COMPARATOR_EXPECTED_REWARD_TOTAL * (1.0 + WIN_THRESHOLD)
)
TOKEN_POINT_THRESHOLD_MAX = math.floor(
    COMPARATOR_EXPECTED_TOKEN_TOTAL * (1.0 - WIN_THRESHOLD)
)

# Terminal-Bench 2.1 submissions must use the task's published defaults. Keep
# this list explicit so a new Harbor field cannot silently become eligible.
CANONICAL_HARBOR_SETTINGS: dict[str, Any] = {
    "timeout_multiplier": 1.0,
    "agent_timeout_multiplier": None,
    "verifier_timeout_multiplier": None,
    "agent_setup_timeout_multiplier": None,
    "environment_build_timeout_multiplier": None,
    "environment_type": "docker",
    "environment_import_path": None,
    "environment_force_build": False,
    "environment_delete": True,
    "environment_suppress_override_warnings": False,
    "environment_mounts_json": None,
    "environment_env_json": "{}",
    "environment_kwargs_json": "{}",
    "environment_override_cpus": None,
    "environment_override_memory_mb": None,
    "environment_override_storage_mb": None,
    "environment_override_gpus": None,
    "agent_override_timeout_sec": None,
    "agent_override_setup_timeout_sec": None,
    "agent_max_timeout_sec": None,
    "agent_name": None,
    "agent_env_json": "{}",
    "agent_kwargs_json": "{}",
    "verifier_override_timeout_sec": None,
    "verifier_max_timeout_sec": None,
    "verifier_disable": False,
    "verifier_env_json": "{}",
}

CANONICAL_HARBOR_JOB_SETTINGS: dict[str, Any] = {
    "retry_max_retries": 0,
    "retry_include_exceptions_json": "null",
    "retry_exclude_exceptions_json": (
        '["VerifierOutputParseError","RewardFileEmptyError",'
        '"RewardFileNotFoundError","AgentTimeoutError","VerifierTimeoutError"]'
    ),
    "retry_wait_multiplier": 1.0,
    "retry_min_wait_sec": 1.0,
    "retry_max_wait_sec": 60.0,
    "artifacts_json": "[]",
    "metrics_json": "[]",
    "quiet": False,
    "debug": False,
    "tasks_json": "[]",
}
CANONICAL_RETRY_EXCLUDE_EXCEPTIONS = frozenset(
    {
        "VerifierOutputParseError",
        "RewardFileEmptyError",
        "RewardFileNotFoundError",
        "AgentTimeoutError",
        "VerifierTimeoutError",
    }
)

CANONICAL_HARBOR_DATASET_SETTINGS: dict[str, Any] = {
    "path": None,
    "version": None,
    "registry_url": None,
    "registry_path": None,
    "overwrite": False,
    "download_dir": None,
    "exclude_task_names_json": "null",
    "n_tasks": None,
}

STUDY_MANIFEST_TOP_LEVEL_FIELDS = frozenset(
    {
        "schema_version",
        "preregistration",
        "sut",
        "analysis",
        "dataset",
        "design",
        "harbor",
        "comparator",
        "calibration",
        "confirmatory",
    }
)
STUDY_MANIFEST_SUT_FIELDS = frozenset(
    {
        "model",
        "allowed_call_models",
        "binary_sha256",
        "source_commit",
        "source_commit_embedded",
        "agent_version",
        "adapter_version",
        "adapter_sha256",
        "budget_usd",
        "disable_reflection",
        "base_url",
        "provider_route_policy",
        "host_credential_source",
        "host_credential_name",
        "host_credential_bundle_count",
        "engine_posture_version",
        "engine_posture",
        "engine_posture_sha256",
    }
)
STUDY_MANIFEST_DESIGN_FIELDS = frozenset({"tasks", "attempts_per_task"})
STUDY_MANIFEST_HARBOR_FIELDS = frozenset(
    {
        "version",
        "sha256",
        *CANONICAL_HARBOR_SETTINGS,
        *CANONICAL_HARBOR_JOB_SETTINGS,
    }
)
STUDY_MANIFEST_COMPARATOR_FIELDS = frozenset(
    {
        "public_job_id",
        "manifest_sha256",
        "trial_data_sha256",
        "submission",
        "agent_name",
        "agent_version",
        "model",
        "reasoning_effort",
        "expected",
    }
)
STUDY_MANIFEST_COMPARATOR_SUBMISSION_FIELDS = frozenset(
    {"repository", "commit", "path", "sha256"}
)
STUDY_MANIFEST_COMPARATOR_EXPECTED_FIELDS = frozenset(
    {"rows", "tasks", "attempts_per_task", "reward_total", "token_spend_total"}
)
STUDY_MANIFEST_CALIBRATION_FIELDS = frozenset(
    {
        "seed",
        "tasks",
        "model_order",
        "call_models_by_config",
        "engine_postures_by_config",
        "job_name",
        "job_id",
        "attempts_per_model_task",
        "n_concurrent_trials",
        "minimum_passes",
        "projection_trials",
        "projected_spend_limit_usd",
        "selected_model",
        "trial_data_sha256",
        "excluded_job_ids",
        "excluded_ledger_sha256",
    }
)
STUDY_MANIFEST_ENGINE_POSTURE_RECORD_FIELDS = frozenset(
    {"version", "posture", "sha256"}
)
STUDY_MANIFEST_ENGINE_POSTURE_FIELDS = frozenset(
    {
        "default_model",
        "allowed_models",
        "auto_mode",
        "effort_auto",
        "reasoning_auto",
        "agents",
    }
)
STUDY_MANIFEST_ENGINE_POSTURE_AGENT_ROLES = frozenset(
    {"default", "worker", "judge", "triage"}
)
STUDY_MANIFEST_ENGINE_POSTURE_AGENT_FIELDS = frozenset({"effort", "reasoning"})
STUDY_MANIFEST_CONFIRMATORY_FIELDS = frozenset({"job_name", "n_concurrent_trials"})

JOB_CONFIG_ALLOWED_FIELDS = frozenset(
    {
        "agent_setup_timeout_multiplier",
        "agent_timeout_multiplier",
        "agents",
        "artifacts",
        "datasets",
        "debug",
        "environment",
        "environment_build_timeout_multiplier",
        "job_name",
        "jobs_dir",
        "metrics",
        "n_attempts",
        "n_concurrent_trials",
        "quiet",
        "retry",
        "tasks",
        "timeout_multiplier",
        "verifier",
        "verifier_timeout_multiplier",
    }
)
TRIAL_CONFIG_ALLOWED_FIELDS = frozenset(
    {
        "agent",
        "agent_setup_timeout_multiplier",
        "agent_timeout_multiplier",
        "artifacts",
        "environment",
        "environment_build_timeout_multiplier",
        "job_id",
        "task",
        "timeout_multiplier",
        "trial_name",
        "trials_dir",
        "verifier",
        "verifier_timeout_multiplier",
    }
)
ENVIRONMENT_ALLOWED_FIELDS = frozenset(
    {
        "type",
        "import_path",
        "force_build",
        "delete",
        "override_cpus",
        "override_memory_mb",
        "override_storage_mb",
        "override_gpus",
        "suppress_override_warnings",
        "mounts_json",
        "env",
        "kwargs",
    }
)
AGENT_ALLOWED_FIELDS = frozenset(
    {
        "name",
        "import_path",
        "model_name",
        "override_timeout_sec",
        "override_setup_timeout_sec",
        "max_timeout_sec",
        "kwargs",
        "env",
    }
)
VERIFIER_ALLOWED_FIELDS = frozenset(
    {"override_timeout_sec", "max_timeout_sec", "env", "disable"}
)
DATASET_ALLOWED_FIELDS = frozenset(
    {
        "path",
        "name",
        "version",
        "ref",
        "registry_url",
        "registry_path",
        "overwrite",
        "download_dir",
        "task_names",
        "exclude_task_names",
        "n_tasks",
    }
)
TASK_ALLOWED_FIELDS = frozenset(
    {
        "path",
        "git_url",
        "git_commit_id",
        "name",
        "ref",
        "overwrite",
        "download_dir",
        "source",
    }
)
RETRY_ALLOWED_FIELDS = frozenset(
    {
        "max_retries",
        "include_exceptions",
        "exclude_exceptions",
        "wait_multiplier",
        "min_wait_sec",
        "max_wait_sec",
    }
)

RUN_LEDGER_TOP_LEVEL_FIELDS = frozenset(
    {
        "schema_version",
        "study_id",
        "ledger_path",
        "historical_spend_disclosure",
        "preregistrations",
        "intents",
        "publications",
        "outcomes",
    }
)
HISTORICAL_SPEND_DISCLOSURE_FIELDS = frozenset(
    {
        "known_lower_bound_usd",
        "unknown_cancellation_spend",
        "new_authorized_budget_usd",
    }
)
HISTORICAL_SPEND_DISCLOSURE = {
    "known_lower_bound_usd": 0.2429614978,
    "unknown_cancellation_spend": True,
    "new_authorized_budget_usd": 200.0,
}
PREREGISTRATION_FIELDS = frozenset(
    {"sequence", "kind", "commit", "study_manifest_sha256", "declared_at"}
)
INTENT_WRAPPER_FIELDS = frozenset({"sequence", "intent", "intent_sha256"})
INTENT_FIELDS = frozenset(
    {
        "intent_id",
        "stage",
        "historical",
        "job_name",
        "models",
        "dataset",
        "requested_trials",
        "attempts_per_task",
        "n_concurrent_trials",
        "retry_max_retries",
        "per_trial_budget_usd",
        "artifacts",
        "execution",
        "provider_key",
        "declared_at",
        "preregistration_commit",
    }
)
INTENT_DATASET_FIELDS = frozenset({"name", "ref", "task_count", "task_set_sha256"})
INTENT_ARTIFACT_FIELDS = frozenset(
    {
        "binary_sha256",
        "source_commit",
        "agent_version",
        "adapter_version",
        "adapter_sha256",
        "analysis_sha256",
        "public_timing_sha256",
        "harbor_version",
        "harbor_sha256",
        "engine_posture_sha256_by_model",
    }
)
INTENT_EXECUTION_FIELDS = frozenset(
    {"base_url", "provider_route_policy", "disable_reflection"}
)
INTENT_PROVIDER_KEY_FIELDS = frozenset(
    {
        "fingerprint_sha256",
        "label",
        "limit_usd",
        "usage_before_usd",
        "snapshot_at",
    }
)
PUBLICATION_FIELDS = frozenset(
    {
        "sequence",
        "subject_type",
        "subject_id",
        "ledger_commit",
        "public_url",
        "published_at",
    }
)
OUTCOME_FIELDS = frozenset(
    {
        "sequence",
        "intent_sha256",
        "job_id",
        "status",
        "started_at",
        "completed_at",
        "artifact_tree_sha256",
        "provider_usage_before_usd",
        "provider_usage_after_usd",
        "provider_usage_delta_usd",
        "telemetry_cost_sum_usd",
        "reconciliation_status",
        "reconciliation_tolerance_usd",
        "recorded_at",
    }
)

CSV_FIELDS = [
    "product",
    "source_input",
    "job_name",
    "job_id",
    "job_started_at",
    "job_finished_at",
    "slot_id",
    "requested",
    "instantiated",
    "attempted",
    "attempt_index",
    "status",
    "task",
    "task_name",
    "task_ref",
    "task_checksum",
    "model",
    "stella_model",
    "agent_info_name",
    "agent_info_version",
    "agent_info_model",
    "stella_agent_version",
    "adapter_version",
    "adapter_sha256",
    "analysis_sha256",
    "binary_sha256",
    "binary_sha256_verified_in_container",
    "source_commit",
    "source_commit_verified_in_binary",
    "budget_usd",
    "disable_reflection",
    "base_url",
    "provider_route_policy",
    "host_credential_source",
    "host_credential_name",
    "host_credential_bundle_count",
    "container_credential_absence_verified",
    "engine_posture_version",
    "engine_posture_json",
    "engine_posture_record_json",
    "engine_posture_sha256",
    "harbor_version",
    "harbor_sha256",
    "trial_dataset_name",
    "job_dataset_name",
    "job_dataset_ref",
    "job_dataset_count",
    "job_agent_count",
    "job_agent_model",
    "job_agent_import_path",
    "job_agent_models_json",
    "job_agent_import_paths_json",
    "job_n_attempts",
    "job_n_concurrent_trials",
    "job_jobs_dir",
    "job_task_count",
    "job_harbor_missing_fields",
    "job_harbor_unknown_fields",
    "job_artifact_tree_sha256",
    "launch_receipt_present",
    "launch_receipt_schema_version",
    "launch_receipt_job_name",
    "launch_receipt_models_json",
    "launch_receipt_intent_sha256",
    "launch_receipt_public_intent_attestation_json",
    "launch_receipt_public_intent_exact_fields",
    "launch_receipt_controls_json",
    "launch_receipt_exact_top_level",
    "launch_receipt_regular_file",
    "launch_receipt_mode_octal",
    "launch_receipt_sha256",
    "launch_receipt_path",
    "host_attestation_present",
    "host_attestation_schema_version",
    "host_attestation_json",
    "host_attestation_exact_top_level",
    "host_attestation_canonical_json",
    "host_attestation_regular_file",
    "host_attestation_mode_octal",
    "host_attestation_sha256",
    "host_attestation_path",
    "trial_harbor_missing_fields",
    "trial_harbor_unknown_fields",
    "trial_artifacts_json",
    "trial_task_path",
    "trial_task_git_url",
    "trial_task_git_commit_id",
    "trial_task_overwrite",
    "trial_task_download_dir",
    "trial_trials_dir",
    "trial_name",
    "trial_id",
    "trial_dir",
    "reward",
    "accuracy_value",
    "prompt_tokens",
    "completion_tokens",
    "cache_tokens",
    "token_spend",
    "cost_usd",
    "cost_source",
    "agent_started_at",
    "agent_finished_at",
    "trial_started_at",
    "trial_finished_at",
    "agent_wall_seconds",
    "exception_type",
    "exception_message",
    "stella_return_code",
    "accounting_state",
    "accounting_step_usage_records",
    "accounting_cost_consistency",
    "accounting_envelope_total_cost_usd",
    "accounting_step_usage_total_cost_usd",
    "accounting_input_tokens",
    "accounting_output_tokens",
    "accounting_cached_input_tokens",
    "accounting_model_state",
    "accounting_model_records",
    "accounting_models",
    "stream_complete",
    "stream_status",
    "stream_terminal_event",
    "stream_cost_source",
    "stream_process_returned",
    "atif_present",
    "atif_valid",
    "atif_schema_version",
    "atif_validation_method",
    "atif_validation_error",
    "atif_engine_posture_version",
    "atif_engine_posture_json",
    "atif_engine_posture_record_json",
    "atif_engine_posture_sha256",
    "atif_host_credential_source",
    "atif_host_credential_name",
    "atif_host_credential_bundle_count",
    "atif_container_credential_absence_verified",
    "result_path",
]

for _scope in ("job", "trial"):
    CSV_FIELDS.extend(f"{_scope}_{setting}" for setting in CANONICAL_HARBOR_SETTINGS)
CSV_FIELDS.extend(f"job_{setting}" for setting in CANONICAL_HARBOR_JOB_SETTINGS)
CSV_FIELDS.extend(
    f"job_dataset_{setting}" for setting in CANONICAL_HARBOR_DATASET_SETTINGS
)


def _read_json(path: Path) -> Any:
    with path.open(encoding="utf-8") as handle:
        return json.load(handle)


def _read_strict_json(path: Path) -> Any:
    def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for key, item in pairs:
            if key in value:
                raise ValueError("JSON object contains duplicate keys")
            value[key] = item
        return value

    return json.loads(
        path.read_text(encoding="utf-8"),
        object_pairs_hook=reject_duplicate_keys,
    )


def _normalized_json_object(value: Any) -> str | None:
    """Canonicalize one JSON object, returning None for any other shape."""
    if not isinstance(value, dict):
        return None
    return json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    )


def _canonical_json_file_bytes(value: Any) -> bytes | None:
    """Return the launcher's exact stable JSON-file representation."""
    if not isinstance(value, dict):
        return None
    try:
        return (
            json.dumps(
                value,
                sort_keys=True,
                separators=(",", ":"),
                ensure_ascii=False,
                allow_nan=False,
            )
            + "\n"
        ).encode("utf-8")
    except (TypeError, ValueError):
        return None


def _normalized_json_value(value: Any) -> str | None:
    try:
        return json.dumps(
            value,
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
        )
    except (TypeError, ValueError):
        return None


def _unknown_fields(value: Any, allowed: frozenset[str]) -> list[str]:
    if not isinstance(value, dict):
        return ["<not-an-object>"]
    return sorted(set(value) - allowed)


def _number(value: Any) -> float | int | None:
    if isinstance(value, str):
        stripped = value.strip()
        if not stripped:
            return None
        try:
            parsed = float(stripped)
        except ValueError:
            return None
        if not math.isfinite(parsed):
            return None
        return int(parsed) if parsed.is_integer() else parsed
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    if not math.isfinite(number):
        return None
    if isinstance(value, int):
        return value
    return number


def _nonnegative_int(value: Any) -> int | None:
    number = _number(value)
    if number is None or number < 0 or not float(number).is_integer():
        return None
    return int(number)


def _nonnegative_float(value: Any) -> float | None:
    number = _number(value)
    if number is None or number < 0:
        return None
    return float(number)


def _boolish(value: Any) -> bool | None:
    if isinstance(value, bool):
        return value
    if isinstance(value, int) and value in (0, 1):
        return bool(value)
    if isinstance(value, str):
        normalized = value.strip().lower()
        if normalized in {"1", "true", "yes", "on"}:
            return True
        if normalized in {"0", "false", "no", "off"}:
            return False
    return None


def _nonempty_string(value: Any) -> str | None:
    return value if isinstance(value, str) and value.strip() else None


def _json_string_array(value: Any) -> list[str] | None:
    if not isinstance(value, str):
        return None
    try:
        parsed = json.loads(value)
    except json.JSONDecodeError:
        return None
    if not isinstance(parsed, list) or not all(
        isinstance(item, str) and item for item in parsed
    ):
        return None
    return parsed


def _normalized_sha256(value: Any) -> str | None:
    if not isinstance(value, str):
        return None
    normalized = value.strip().lower().removeprefix("sha256:")
    return normalized if re.fullmatch(r"[0-9a-f]{64}", normalized) else None


def _agent_model(agent_info: dict[str, Any]) -> str | None:
    model_info = agent_info.get("model_info")
    if not isinstance(model_info, dict):
        return None
    name = model_info.get("name")
    provider = model_info.get("provider")
    if not isinstance(name, str) or not name.strip():
        return None
    name = name.strip()
    if not isinstance(provider, str) or not provider.strip():
        return name
    provider = provider.strip()
    return name if name.startswith(f"{provider}/") else f"{provider}/{name}"


def _harbor_settings(
    config: dict[str, Any], agent: dict[str, Any]
) -> tuple[dict[str, Any], list[str]]:
    """Extract every timeout/resource control and record absent fields."""
    environment = config.get("environment")
    environment = environment if isinstance(environment, dict) else {}
    verifier = config.get("verifier")
    verifier = verifier if isinstance(verifier, dict) else {}
    sources: dict[str, tuple[dict[str, Any], str]] = {
        "timeout_multiplier": (config, "timeout_multiplier"),
        "agent_timeout_multiplier": (config, "agent_timeout_multiplier"),
        "verifier_timeout_multiplier": (config, "verifier_timeout_multiplier"),
        "agent_setup_timeout_multiplier": (
            config,
            "agent_setup_timeout_multiplier",
        ),
        "environment_build_timeout_multiplier": (
            config,
            "environment_build_timeout_multiplier",
        ),
        "environment_type": (environment, "type"),
        "environment_import_path": (environment, "import_path"),
        "environment_force_build": (environment, "force_build"),
        "environment_delete": (environment, "delete"),
        "environment_suppress_override_warnings": (
            environment,
            "suppress_override_warnings",
        ),
        "environment_mounts_json": (environment, "mounts_json"),
        "environment_override_cpus": (environment, "override_cpus"),
        "environment_override_memory_mb": (environment, "override_memory_mb"),
        "environment_override_storage_mb": (environment, "override_storage_mb"),
        "environment_override_gpus": (environment, "override_gpus"),
        "agent_override_timeout_sec": (agent, "override_timeout_sec"),
        "agent_override_setup_timeout_sec": (agent, "override_setup_timeout_sec"),
        "agent_max_timeout_sec": (agent, "max_timeout_sec"),
        "agent_name": (agent, "name"),
        "verifier_override_timeout_sec": (verifier, "override_timeout_sec"),
        "verifier_max_timeout_sec": (verifier, "max_timeout_sec"),
        "verifier_disable": (verifier, "disable"),
    }
    values: dict[str, Any] = {}
    missing: list[str] = []
    for name, (parent, key) in sources.items():
        if key not in parent:
            missing.append(name)
            values[name] = None
        else:
            values[name] = parent[key]
    for name, parent, key in (
        ("environment_env_json", environment, "env"),
        ("environment_kwargs_json", environment, "kwargs"),
        ("agent_env_json", agent, "env"),
        ("agent_kwargs_json", agent, "kwargs"),
        ("verifier_env_json", verifier, "env"),
    ):
        if key not in parent:
            missing.append(name)
            values[name] = None
        else:
            values[name] = _normalized_json_value(parent[key])
    return values, missing


def _job_only_settings(config: dict[str, Any]) -> tuple[dict[str, Any], list[str]]:
    retry = config.get("retry")
    retry = retry if isinstance(retry, dict) else {}
    sources: dict[str, tuple[dict[str, Any], str]] = {
        "retry_max_retries": (retry, "max_retries"),
        "retry_wait_multiplier": (retry, "wait_multiplier"),
        "retry_min_wait_sec": (retry, "min_wait_sec"),
        "retry_max_wait_sec": (retry, "max_wait_sec"),
        "quiet": (config, "quiet"),
        "debug": (config, "debug"),
    }
    values: dict[str, Any] = {}
    missing: list[str] = []
    for name, (parent, key) in sources.items():
        if key not in parent:
            missing.append(name)
            values[name] = None
        else:
            values[name] = parent[key]
    for name, parent, key in (
        ("retry_include_exceptions_json", retry, "include_exceptions"),
        ("retry_exclude_exceptions_json", retry, "exclude_exceptions"),
        ("artifacts_json", config, "artifacts"),
        ("metrics_json", config, "metrics"),
        ("tasks_json", config, "tasks"),
    ):
        if key not in parent:
            missing.append(name)
            values[name] = None
        else:
            values[name] = _normalized_json_value(parent[key])
    return values, missing


def _job_study_metadata(config: dict[str, Any]) -> dict[str, Any]:
    datasets = [item for item in config.get("datasets") or [] if isinstance(item, dict)]
    agents = [item for item in config.get("agents") or [] if isinstance(item, dict)]
    dataset = datasets[0] if len(datasets) == 1 else {}
    agent = agents[0] if len(agents) == 1 else {}
    settings, missing = _harbor_settings(config, agent)
    agent_setting_names = (
        "agent_override_timeout_sec",
        "agent_override_setup_timeout_sec",
        "agent_max_timeout_sec",
        "agent_name",
        "agent_env_json",
        "agent_kwargs_json",
    )
    if len(agents) > 1:
        # A multi-agent calibration job still has one job-level value for each
        # control only when every declared agent explicitly supplies the field
        # and all values agree. Do not mistake the absence of a single-agent
        # projection for a missing Harbor setting.
        missing = [name for name in missing if name not in agent_setting_names]
        per_agent_settings = [_harbor_settings(config, item) for item in agents]
        for name in agent_setting_names:
            values = [item_settings[name] for item_settings, _ in per_agent_settings]
            if any(name in item_missing for _, item_missing in per_agent_settings):
                missing.append(name)
            settings[name] = (
                values[0]
                if all(_matches_setting(value, values[0]) for value in values)
                else {"heterogeneous_per_agent": values}
            )
    job_only_settings, job_only_missing = _job_only_settings(config)
    unknown_fields: list[str] = [
        f"job.{name}" for name in _unknown_fields(config, JOB_CONFIG_ALLOWED_FIELDS)
    ]
    environment = config.get("environment")
    verifier = config.get("verifier")
    retry = config.get("retry")
    unknown_fields.extend(
        f"environment.{name}"
        for name in _unknown_fields(environment, ENVIRONMENT_ALLOWED_FIELDS)
    )
    unknown_fields.extend(
        f"verifier.{name}"
        for name in _unknown_fields(verifier, VERIFIER_ALLOWED_FIELDS)
    )
    unknown_fields.extend(
        f"retry.{name}" for name in _unknown_fields(retry, RETRY_ALLOWED_FIELDS)
    )
    for index, item in enumerate(agents):
        unknown_fields.extend(
            f"agents[{index}].{name}"
            for name in _unknown_fields(item, AGENT_ALLOWED_FIELDS)
        )
    for index, item in enumerate(datasets):
        unknown_fields.extend(
            f"datasets[{index}].{name}"
            for name in _unknown_fields(item, DATASET_ALLOWED_FIELDS)
        )
    metadata = {
        "job_dataset_name": dataset.get("name"),
        "job_dataset_ref": dataset.get("ref"),
        "job_dataset_count": len(datasets),
        "job_agent_count": len(agents),
        "job_agent_model": agent.get("model_name"),
        "job_agent_import_path": agent.get("import_path"),
        "job_agent_models_json": json.dumps(
            [item.get("model_name") for item in agents],
            separators=(",", ":"),
            ensure_ascii=False,
        ),
        "job_agent_import_paths_json": json.dumps(
            [item.get("import_path") for item in agents],
            separators=(",", ":"),
            ensure_ascii=False,
        ),
        "job_n_attempts": _nonnegative_int(config.get("n_attempts")),
        "job_n_concurrent_trials": _nonnegative_int(config.get("n_concurrent_trials")),
        "job_jobs_dir": config.get("jobs_dir"),
        "job_task_count": len(_job_tasks(config)),
        "job_harbor_missing_fields": "|".join(missing + job_only_missing),
        "job_harbor_unknown_fields": "|".join(sorted(unknown_fields)),
        **{
            f"job_dataset_{name}": (
                _normalized_json_value(dataset.get(name.removesuffix("_json")))
                if name.endswith("_json") and name.removesuffix("_json") in dataset
                else dataset.get(name)
            )
            for name in CANONICAL_HARBOR_DATASET_SETTINGS
        },
    }
    metadata.update({f"job_{name}": value for name, value in settings.items()})
    metadata.update({f"job_{name}": value for name, value in job_only_settings.items()})
    return metadata


def _trial_study_metadata(config: dict[str, Any]) -> dict[str, Any]:
    task = config.get("task")
    task = task if isinstance(task, dict) else {}
    agent = config.get("agent")
    agent = agent if isinstance(agent, dict) else {}
    settings, missing = _harbor_settings(config, agent)
    environment = config.get("environment")
    verifier = config.get("verifier")
    unknown_fields = [
        f"trial.{name}" for name in _unknown_fields(config, TRIAL_CONFIG_ALLOWED_FIELDS)
    ]
    unknown_fields.extend(
        f"environment.{name}"
        for name in _unknown_fields(environment, ENVIRONMENT_ALLOWED_FIELDS)
    )
    unknown_fields.extend(
        f"agent.{name}" for name in _unknown_fields(agent, AGENT_ALLOWED_FIELDS)
    )
    unknown_fields.extend(
        f"verifier.{name}"
        for name in _unknown_fields(verifier, VERIFIER_ALLOWED_FIELDS)
    )
    unknown_fields.extend(
        f"task.{name}" for name in _unknown_fields(task, TASK_ALLOWED_FIELDS)
    )
    metadata = {
        "trial_dataset_name": task.get("source"),
        "trial_harbor_missing_fields": "|".join(missing),
        "trial_harbor_unknown_fields": "|".join(sorted(unknown_fields)),
        "trial_artifacts_json": (
            _normalized_json_value(config.get("artifacts"))
            if "artifacts" in config
            else None
        ),
        "trial_task_path": task.get("path"),
        "trial_task_git_url": task.get("git_url"),
        "trial_task_git_commit_id": task.get("git_commit_id"),
        "trial_task_overwrite": task.get("overwrite"),
        "trial_task_download_dir": task.get("download_dir"),
        "trial_trials_dir": config.get("trials_dir"),
    }
    metadata.update({f"trial_{name}": value for name, value in settings.items()})
    return metadata


def _normalize_task(value: Any) -> str | None:
    if not isinstance(value, str) or not value.strip():
        return None
    task = value.strip()
    if "/" in task:
        task = task.rsplit("/", 1)[-1]
    return task


def _full_task_name(value: Any) -> str | None:
    if not isinstance(value, str) or not value.strip():
        return None
    return value.strip()


def _parse_timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _elapsed_seconds(start: Any, finish: Any) -> float | None:
    started_at = _parse_timestamp(start)
    finished_at = _parse_timestamp(finish)
    if started_at is None or finished_at is None:
        return None
    elapsed = (finished_at - started_at).total_seconds()
    return elapsed if elapsed >= 0 else None


def _recover_cost_from_exception(message: Any) -> float | None:
    """Recover only Stella's top-level envelope cost from captured stdout."""
    if not isinstance(message, str):
        return None
    match = re.search(r'"cost_usd"\s*:\s*([0-9]+(?:\.[0-9]+)?)', message)
    if match is None:
        return None
    return _nonnegative_float(float(match.group(1)))


def _validate_atif(path: Path | None) -> dict[str, Any]:
    base = {
        "atif_present": False,
        "atif_valid": False,
        "atif_schema_version": None,
        "atif_validation_method": "harbor-0.6.1-Trajectory",
        "atif_validation_error": "trajectory.json is missing",
        "atif_engine_posture_version": None,
        "atif_engine_posture_json": None,
        "atif_engine_posture_record_json": None,
        "atif_engine_posture_sha256": None,
        "atif_host_credential_source": None,
        "atif_host_credential_name": None,
        "atif_host_credential_bundle_count": None,
        "atif_container_credential_absence_verified": None,
    }
    if path is None or not path.is_file():
        return base

    base["atif_present"] = True
    try:
        raw_text = path.read_text(encoding="utf-8")
        payload = json.loads(raw_text)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        base["atif_validation_error"] = f"{type(error).__name__}: {error}"
        return base

    if isinstance(payload, dict):
        base["atif_schema_version"] = payload.get("schema_version")
        agent = payload.get("agent")
        agent = agent if isinstance(agent, dict) else {}
        extra = agent.get("extra")
        extra = extra if isinstance(extra, dict) else {}
        base["atif_engine_posture_version"] = extra.get("engine_posture_version")
        base["atif_engine_posture_json"] = extra.get("engine_posture_json")
        base["atif_engine_posture_record_json"] = _normalized_json_object(
            extra.get("engine_posture")
        )
        base["atif_engine_posture_sha256"] = extra.get("engine_posture_sha256")
        base["atif_host_credential_source"] = extra.get("host_credential_source")
        base["atif_host_credential_name"] = extra.get("host_credential_name")
        base["atif_host_credential_bundle_count"] = _nonnegative_int(
            extra.get("host_credential_bundle_count")
        )
        base["atif_container_credential_absence_verified"] = _boolish(
            extra.get("container_credential_absence_verified")
        )

    if Trajectory is None:
        base["atif_valid"] = None
        base["atif_validation_method"] = "unavailable"
        base["atif_validation_error"] = (
            "Harbor is not installed; official ATIF validation is unavailable"
        )
        return base

    try:
        Trajectory.model_validate(payload)
    except Exception as error:  # Pydantic's error hierarchy is version-dependent.
        base["atif_validation_error"] = f"{type(error).__name__}: {error}"
        return base

    if base["atif_schema_version"] != "ATIF-v1.7":
        base["atif_validation_error"] = (
            f"expected ATIF-v1.7, got {base['atif_schema_version']!r}"
        )
        return base

    base["atif_valid"] = True
    base["atif_validation_error"] = None
    return base


def _job_tasks(config: dict[str, Any]) -> list[str]:
    tasks: list[str] = []
    for dataset in config.get("datasets") or []:
        if not isinstance(dataset, dict):
            continue
        for name in dataset.get("task_names") or []:
            if isinstance(name, str):
                tasks.append(name)
    for item in config.get("tasks") or []:
        if isinstance(item, str):
            tasks.append(item)
        elif isinstance(item, dict):
            value = item.get("name")
            if value is None and isinstance(item.get("task"), dict):
                value = item["task"].get("name")
            if value is None:
                value = item.get("path")
            if isinstance(value, str):
                tasks.append(value)
    return list(dict.fromkeys(tasks))


def _job_agents(config: dict[str, Any]) -> list[dict[str, Any]]:
    agents = []
    for index, agent in enumerate(config.get("agents") or []):
        if not isinstance(agent, dict):
            continue
        agents.append(
            {
                "agent_index": index,
                "model": agent.get("model_name"),
                "import_path": agent.get("import_path"),
                "name": agent.get("name"),
            }
        )
    return agents


def _launch_receipt_metadata(job_dir: Path, warnings: list[str]) -> dict[str, Any]:
    path = job_dir / SECURE_LAUNCH_RECEIPT_FILENAME
    base = {
        "launch_receipt_present": False,
        "launch_receipt_schema_version": None,
        "launch_receipt_job_name": None,
        "launch_receipt_models_json": None,
        "launch_receipt_intent_sha256": None,
        "launch_receipt_public_intent_attestation_json": None,
        "launch_receipt_public_intent_exact_fields": False,
        "launch_receipt_controls_json": None,
        "launch_receipt_exact_top_level": False,
        "launch_receipt_regular_file": False,
        "launch_receipt_mode_octal": None,
        "launch_receipt_sha256": None,
        "launch_receipt_path": str(path.resolve()),
    }
    if not path.exists():
        return base
    base["launch_receipt_present"] = True
    if path.is_symlink() or not path.is_file():
        warnings.append(f"{path}: secure launch receipt is not a regular file")
        return base
    base["launch_receipt_regular_file"] = True
    try:
        mode = stat.S_IMODE(path.stat().st_mode)
        payload = _read_strict_json(path)
        base["launch_receipt_sha256"] = _sha256_file(path)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        warnings.append(f"{path}: {type(error).__name__}: {error}")
        return base
    base["launch_receipt_mode_octal"] = f"{mode:04o}"
    if not isinstance(payload, dict):
        warnings.append(f"{path}: secure launch receipt is not a JSON object")
        return base
    base["launch_receipt_exact_top_level"] = set(payload) == {
        "schema_version",
        "job_name",
        "models",
        "intent_sha256",
        "public_intent_attestation",
        "launcher_controls",
    }
    base["launch_receipt_schema_version"] = payload.get("schema_version")
    base["launch_receipt_job_name"] = payload.get("job_name")
    base["launch_receipt_models_json"] = _normalized_json_value(payload.get("models"))
    base["launch_receipt_intent_sha256"] = payload.get("intent_sha256")
    public_intent_attestation = payload.get("public_intent_attestation")
    base["launch_receipt_public_intent_attestation_json"] = _normalized_json_object(
        public_intent_attestation
    )
    base["launch_receipt_public_intent_exact_fields"] = bool(
        isinstance(public_intent_attestation, dict)
        and set(public_intent_attestation) == PUBLIC_INTENT_ATTESTATION_FIELDS
    )
    base["launch_receipt_controls_json"] = _normalized_json_object(
        payload.get("launcher_controls")
    )
    return base


def _host_attestation_metadata(job_dir: Path, warnings: list[str]) -> dict[str, Any]:
    """Ingest the immutable host sidecar without trusting any nested assertion."""
    path = job_dir / HOST_ATTESTATION_FILENAME
    base = {
        "host_attestation_present": False,
        "host_attestation_schema_version": None,
        "host_attestation_json": None,
        "host_attestation_exact_top_level": False,
        "host_attestation_canonical_json": False,
        "host_attestation_regular_file": False,
        "host_attestation_mode_octal": None,
        "host_attestation_sha256": None,
        "host_attestation_path": str(path.resolve()),
    }
    if not path.exists():
        return base
    base["host_attestation_present"] = True
    if path.is_symlink() or not path.is_file():
        warnings.append(f"{path}: host attestation is not a regular file")
        return base
    base["host_attestation_regular_file"] = True
    try:
        mode = stat.S_IMODE(path.stat().st_mode)
        raw = path.read_bytes()
        payload = _read_strict_json(path)
        base["host_attestation_sha256"] = hashlib.sha256(raw).hexdigest()
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        warnings.append(f"{path}: {type(error).__name__}: {error}")
        return base
    base["host_attestation_mode_octal"] = f"{mode:04o}"
    if not isinstance(payload, dict):
        warnings.append(f"{path}: host attestation is not a JSON object")
        return base
    canonical = _canonical_json_file_bytes(payload)
    base["host_attestation_schema_version"] = payload.get("schema_version")
    base["host_attestation_json"] = _normalized_json_object(payload)
    base["host_attestation_exact_top_level"] = set(payload) == HOST_BINDING_FIELDS
    base["host_attestation_canonical_json"] = canonical == raw
    return base


def _artifact_tree_sha256(job_dir: Path, warnings: list[str]) -> str | None:
    entries: list[dict[str, Any]] = []
    for path in sorted(job_dir.rglob("*"), key=lambda item: item.as_posix()):
        relative = path.relative_to(job_dir).as_posix()
        if path.is_symlink():
            warnings.append(f"{path}: symlink prevents artifact-tree hashing")
            return None
        if path.is_dir():
            continue
        if not path.is_file():
            warnings.append(f"{path}: special file prevents artifact-tree hashing")
            return None
        try:
            entries.append(
                {
                    "path": relative,
                    "bytes": path.stat().st_size,
                    "sha256": _sha256_file(path),
                }
            )
        except OSError as error:
            warnings.append(f"{path}: {type(error).__name__}: {error}")
            return None
    encoded = json.dumps(
        {"schema": "stella-harbor-artifact-tree-v1", "files": entries},
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _load_trial_shell(trial_dir: Path, warnings: list[str]) -> dict[str, Any]:
    config_path = trial_dir / "config.json"
    result_path = trial_dir / "result.json"
    config: dict[str, Any] = {}
    result: dict[str, Any] | None = None
    try:
        loaded = _read_json(config_path)
        if isinstance(loaded, dict):
            config = loaded
    except (OSError, json.JSONDecodeError) as error:
        warnings.append(f"{config_path}: {type(error).__name__}: {error}")
    if result_path.is_file():
        try:
            loaded = _read_json(result_path)
            if isinstance(loaded, dict):
                result = loaded
        except (OSError, json.JSONDecodeError) as error:
            warnings.append(f"{result_path}: {type(error).__name__}: {error}")

    result_config = result.get("config") if result is not None else None
    effective_config = result_config if isinstance(result_config, dict) else config
    task_config = effective_config.get("task") or {}
    agent_config = effective_config.get("agent") or {}
    task_name = (
        result.get("task_name") if result is not None else task_config.get("name")
    )
    model = agent_config.get("model_name")
    started_at = result.get("started_at") if result is not None else None
    return {
        "trial_dir": trial_dir,
        "config": config,
        "result": result,
        "result_path": result_path,
        "task_name": task_name,
        "task": _normalize_task(task_name),
        "model": model,
        "sort_key": (started_at or "", trial_dir.name),
    }


def _trial_row(
    *,
    job_dir: Path,
    job_name: str,
    job_id: str | None,
    product: str,
    attempt_index: int,
    requested: bool,
    expected_task_name: str | None,
    expected_model: str | None,
    shell: dict[str, Any] | None,
    slot_id: str,
    job_metadata: dict[str, Any],
) -> dict[str, Any]:
    if shell is None:
        return {
            "product": product,
            "source_input": str(job_dir.resolve()),
            "job_name": job_name,
            "job_id": job_id,
            "slot_id": slot_id,
            "requested": requested,
            "instantiated": False,
            "attempted": False,
            "attempt_index": attempt_index,
            "status": "not_instantiated",
            "task": _normalize_task(expected_task_name),
            "task_name": expected_task_name,
            "task_ref": None,
            "task_checksum": None,
            "model": expected_model,
            "stella_model": None,
            "agent_info_name": None,
            "agent_info_version": None,
            "agent_info_model": None,
            "stella_agent_version": None,
            "adapter_version": None,
            "adapter_sha256": None,
            "analysis_sha256": None,
            "binary_sha256": None,
            "binary_sha256_verified_in_container": None,
            "source_commit": None,
            "source_commit_verified_in_binary": None,
            "budget_usd": None,
            "disable_reflection": None,
            "base_url": None,
            "provider_route_policy": None,
            "host_credential_source": None,
            "host_credential_name": None,
            "host_credential_bundle_count": None,
            "container_credential_absence_verified": None,
            "engine_posture_version": None,
            "engine_posture_json": None,
            "engine_posture_record_json": None,
            "engine_posture_sha256": None,
            "harbor_version": None,
            "harbor_sha256": None,
            **job_metadata,
            **_trial_study_metadata({}),
            "trial_name": None,
            "trial_id": None,
            "trial_dir": None,
            "reward": None,
            "accuracy_value": None,
            "prompt_tokens": None,
            "completion_tokens": None,
            "cache_tokens": None,
            "token_spend": None,
            "cost_usd": None,
            "cost_source": None,
            "agent_started_at": None,
            "agent_finished_at": None,
            "agent_wall_seconds": None,
            "exception_type": None,
            "exception_message": None,
            "stella_return_code": None,
            "accounting_state": None,
            "accounting_step_usage_records": None,
            "accounting_cost_consistency": None,
            "accounting_envelope_total_cost_usd": None,
            "accounting_step_usage_total_cost_usd": None,
            "accounting_input_tokens": None,
            "accounting_output_tokens": None,
            "accounting_cached_input_tokens": None,
            "accounting_model_state": None,
            "accounting_model_records": None,
            "accounting_models": None,
            "stream_complete": None,
            "stream_status": None,
            "stream_terminal_event": None,
            "stream_cost_source": None,
            "stream_process_returned": None,
            **_validate_atif(None),
            "result_path": None,
        }

    trial_dir = shell["trial_dir"]
    config = shell["config"]
    result = shell["result"]
    result_config = result.get("config") if result is not None else None
    effective_config = result_config if isinstance(result_config, dict) else config
    task_config = effective_config.get("task") or {}
    agent_config = effective_config.get("agent") or {}
    task_id = result.get("task_id") if result is not None else None
    task_id = task_id if isinstance(task_id, dict) else {}
    agent_result = result.get("agent_result") if result is not None else None
    agent_result = agent_result if isinstance(agent_result, dict) else {}
    verifier_result = result.get("verifier_result") if result is not None else None
    verifier_result = verifier_result if isinstance(verifier_result, dict) else {}
    rewards = verifier_result.get("rewards")
    rewards = rewards if isinstance(rewards, dict) else {}
    exception_info = result.get("exception_info") if result is not None else None
    exception_info = exception_info if isinstance(exception_info, dict) else {}
    execution = result.get("agent_execution") if result is not None else None
    execution = execution if isinstance(execution, dict) else {}
    agent_info = result.get("agent_info") if result is not None else None
    agent_info = agent_info if isinstance(agent_info, dict) else {}
    metadata = agent_result.get("metadata")
    metadata = metadata if isinstance(metadata, dict) else {}
    accounting = metadata.get("stella_accounting")
    accounting = accounting if isinstance(accounting, dict) else {}
    accounting_fields = accounting.get("fields")
    accounting_fields = accounting_fields if isinstance(accounting_fields, dict) else {}

    def accounting_total(name: str) -> int | None:
        field = accounting_fields.get(name)
        field = field if isinstance(field, dict) else {}
        return _nonnegative_int(field.get("total"))

    stream = metadata.get("stella_stream")
    stream = stream if isinstance(stream, dict) else {}
    accounting_models = accounting.get("models")
    if isinstance(accounting_models, list) and all(
        isinstance(value, str) for value in accounting_models
    ):
        normalized_accounting_models: str | None = "|".join(accounting_models)
    else:
        normalized_accounting_models = None

    reward = _number(rewards.get("reward"))
    exception_type = exception_info.get("exception_type")
    exception_message = exception_info.get("exception_message")
    # Terminal-Bench's canonical score is the verifier reward. An agent may
    # time out after leaving a correct solution behind; if the verifier still
    # produced a reward, retain it exactly as the official leaderboard does.
    # Only an attempted, finished/error trial with no reward receives zero.
    accuracy_value = reward
    if accuracy_value is None and (
        exception_type or (result is not None and result.get("finished_at"))
    ):
        accuracy_value = 0.0

    prompt_tokens = _nonnegative_int(agent_result.get("n_input_tokens"))
    completion_tokens = _nonnegative_int(agent_result.get("n_output_tokens"))
    cache_tokens = _nonnegative_int(agent_result.get("n_cache_tokens"))
    token_spend = None
    if prompt_tokens is not None and completion_tokens is not None:
        token_spend = prompt_tokens + completion_tokens

    cost = _nonnegative_float(agent_result.get("cost_usd"))
    cost_source = "harbor_agent_result" if cost is not None else None
    if cost is None:
        cost = _recover_cost_from_exception(exception_message)
        if cost is not None:
            cost_source = "stella_envelope_in_exception"

    if exception_type:
        status = "error"
    elif result is None or not result.get("finished_at"):
        status = "in_progress"
    else:
        status = "completed"

    task_name = (
        result.get("task_name") if result is not None else task_config.get("name")
    ) or expected_task_name
    trial_name = (
        result.get("trial_name") if result is not None else config.get("trial_name")
    ) or trial_dir.name
    atif = _validate_atif(trial_dir / "agent" / "trajectory.json")
    effective_job_id = effective_config.get("job_id") or job_id

    return {
        "product": product,
        "source_input": str(job_dir.resolve()),
        "job_name": job_name,
        "job_id": effective_job_id,
        "slot_id": slot_id,
        "requested": requested,
        "instantiated": True,
        "attempted": True,
        "attempt_index": attempt_index,
        "status": status,
        "task": _normalize_task(task_name),
        "task_name": task_name,
        "task_ref": task_id.get("ref") or task_config.get("ref"),
        "task_checksum": result.get("task_checksum") if result is not None else None,
        "model": agent_config.get("model_name") or expected_model,
        "stella_model": metadata.get("stella_model"),
        "agent_info_name": agent_info.get("name"),
        "agent_info_version": agent_info.get("version"),
        "agent_info_model": _agent_model(agent_info),
        "stella_agent_version": metadata.get("stella_agent_version"),
        "adapter_version": metadata.get("stella_adapter_version"),
        "adapter_sha256": metadata.get("stella_adapter_sha256"),
        "analysis_sha256": None,
        "binary_sha256": metadata.get("stella_binary_sha256"),
        "binary_sha256_verified_in_container": _boolish(
            metadata.get("stella_binary_sha256_verified_in_container")
        ),
        "source_commit": metadata.get("stella_source_commit"),
        "source_commit_verified_in_binary": _boolish(
            metadata.get("stella_source_commit_verified_in_binary")
        ),
        "budget_usd": _nonnegative_float(metadata.get("stella_budget_usd")),
        "disable_reflection": _boolish(metadata.get("stella_disable_reflection")),
        "base_url": metadata.get("stella_base_url"),
        "provider_route_policy": metadata.get("stella_provider_route_policy"),
        "host_credential_source": metadata.get("stella_host_credential_source"),
        "host_credential_name": metadata.get("stella_host_credential_name"),
        "host_credential_bundle_count": _nonnegative_int(
            metadata.get("stella_host_credential_bundle_count")
        ),
        "container_credential_absence_verified": _boolish(
            metadata.get("stella_container_credential_absence_verified")
        ),
        "engine_posture_version": metadata.get("stella_engine_posture_version"),
        "engine_posture_json": metadata.get("stella_engine_posture_json"),
        "engine_posture_record_json": _normalized_json_object(
            metadata.get("stella_engine_posture")
        ),
        "engine_posture_sha256": metadata.get("stella_engine_posture_sha256"),
        "harbor_version": metadata.get("stella_harbor_version"),
        "harbor_sha256": metadata.get("stella_harbor_sha256"),
        **job_metadata,
        **_trial_study_metadata(effective_config),
        "trial_name": trial_name,
        "trial_id": result.get("id") if result is not None else None,
        "trial_dir": str(trial_dir.resolve()),
        "reward": reward,
        "accuracy_value": accuracy_value,
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "cache_tokens": cache_tokens,
        "token_spend": token_spend,
        "cost_usd": cost,
        "cost_source": cost_source,
        "agent_started_at": execution.get("started_at"),
        "agent_finished_at": execution.get("finished_at"),
        "trial_started_at": result.get("started_at") if result is not None else None,
        "trial_finished_at": result.get("finished_at") if result is not None else None,
        "agent_wall_seconds": _elapsed_seconds(
            execution.get("started_at"), execution.get("finished_at")
        ),
        "exception_type": exception_type,
        "exception_message": exception_message,
        "stella_return_code": _nonnegative_int(metadata.get("stella_return_code")),
        "accounting_state": accounting.get("state"),
        "accounting_step_usage_records": _nonnegative_int(
            accounting.get("step_usage_records")
        ),
        "accounting_cost_consistency": accounting.get("cost_consistency"),
        "accounting_envelope_total_cost_usd": _nonnegative_float(
            accounting.get("envelope_total_cost_usd")
        ),
        "accounting_step_usage_total_cost_usd": _nonnegative_float(
            accounting.get("step_usage_total_cost_usd")
        ),
        "accounting_input_tokens": accounting_total("input_tokens"),
        "accounting_output_tokens": accounting_total("output_tokens"),
        "accounting_cached_input_tokens": accounting_total("cached_input_tokens"),
        "accounting_model_state": accounting.get("model_state"),
        "accounting_model_records": _nonnegative_int(accounting.get("model_records")),
        "accounting_models": normalized_accounting_models,
        "stream_complete": _boolish(stream.get("stream_complete")),
        "stream_status": metadata.get("stella_status"),
        "stream_terminal_event": stream.get("terminal_event"),
        "stream_cost_source": stream.get("cost_source"),
        "stream_process_returned": _boolish(stream.get("process_returned")),
        **atif,
        "result_path": str(shell["result_path"].resolve())
        if shell["result_path"].is_file()
        else None,
    }


def ingest_job(
    job_dir: Path, *, product: str = "stella"
) -> tuple[list[dict], list[str]]:
    """Enumerate expected slots and all actual trial directories for one job."""
    job_dir = job_dir.resolve()
    warnings: list[str] = []
    if not job_dir.is_dir():
        raise ValueError(f"Harbor job directory does not exist: {job_dir}")

    try:
        job_config = _read_json(job_dir / "config.json")
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read {job_dir / 'config.json'}: {error}") from error
    if not isinstance(job_config, dict):
        raise ValueError(f"job config is not an object: {job_dir / 'config.json'}")

    job_result: dict[str, Any] = {}
    if (job_dir / "result.json").is_file():
        try:
            loaded = _read_json(job_dir / "result.json")
            if isinstance(loaded, dict):
                job_result = loaded
        except (OSError, json.JSONDecodeError) as error:
            warnings.append(
                f"{job_dir / 'result.json'}: {type(error).__name__}: {error}"
            )

    shells = []
    for child in sorted(job_dir.iterdir()):
        if child.is_dir() and (child / "config.json").is_file():
            shells.append(_load_trial_shell(child, warnings))
    shells.sort(key=lambda shell: shell["sort_key"])

    tasks = _job_tasks(job_config)
    agents = _job_agents(job_config)
    if not tasks:
        tasks = list(
            dict.fromkeys(
                shell["task_name"] for shell in shells if shell["task_name"] is not None
            )
        )
        warnings.append(
            f"{job_dir}: requested task manifest is absent; task names were inferred "
            "from instantiated trials"
        )
    if not agents:
        models = list(
            dict.fromkeys(
                shell["model"] for shell in shells if shell["model"] is not None
            )
        )
        agents = [
            {"agent_index": index, "model": model, "import_path": None, "name": None}
            for index, model in enumerate(models or [None])
        ]

    n_attempts = _nonnegative_int(job_config.get("n_attempts")) or 1
    job_name = str(job_config.get("job_name") or job_dir.name)
    job_id = job_result.get("id") if isinstance(job_result.get("id"), str) else None
    job_metadata = _job_study_metadata(job_config)
    job_metadata["job_started_at"] = job_result.get("started_at")
    job_metadata["job_finished_at"] = job_result.get("finished_at")
    job_metadata.update(_launch_receipt_metadata(job_dir, warnings))
    job_metadata.update(_host_attestation_metadata(job_dir, warnings))
    job_metadata["job_artifact_tree_sha256"] = _artifact_tree_sha256(job_dir, warnings)
    unused = list(shells)
    rows: list[dict[str, Any]] = []

    for task_name in tasks:
        for agent in agents:
            for attempt_index in range(1, n_attempts + 1):
                task = _normalize_task(task_name)
                model = agent["model"]
                matching_index = next(
                    (
                        index
                        for index, shell in enumerate(unused)
                        if shell["task"] == task
                        and (model is None or shell["model"] == model)
                    ),
                    None,
                )
                shell = (
                    unused.pop(matching_index) if matching_index is not None else None
                )
                slot_id = (
                    f"{job_name}:{task or 'unknown'}:{agent['agent_index']}:"
                    f"{attempt_index}"
                )
                rows.append(
                    _trial_row(
                        job_dir=job_dir,
                        job_name=job_name,
                        job_id=job_id,
                        product=product,
                        attempt_index=attempt_index,
                        requested=True,
                        expected_task_name=task_name,
                        expected_model=model,
                        shell=shell,
                        slot_id=slot_id,
                        job_metadata=job_metadata,
                    )
                )

    for ordinal, shell in enumerate(unused, start=1):
        task = shell["task"] or "unknown"
        same_task_model = [
            row
            for row in rows
            if row["task"] == shell["task"] and row["model"] == shell["model"]
        ]
        attempt_index = len(same_task_model) + ordinal
        rows.append(
            _trial_row(
                job_dir=job_dir,
                job_name=job_name,
                job_id=job_id,
                product=product,
                attempt_index=attempt_index,
                requested=False,
                expected_task_name=shell["task_name"],
                expected_model=shell["model"],
                shell=shell,
                slot_id=f"{job_name}:unexpected:{task}:{ordinal}",
                job_metadata=job_metadata,
            )
        )
        warnings.append(
            f"{shell['trial_dir']}: instantiated trial was not in job manifest"
        )

    requested_total = _nonnegative_int(job_result.get("n_total_trials"))
    current_requested = sum(row["requested"] for row in rows)
    if requested_total is not None and requested_total > current_requested:
        for ordinal in range(1, requested_total - current_requested + 1):
            rows.append(
                _trial_row(
                    job_dir=job_dir,
                    job_name=job_name,
                    job_id=job_id,
                    product=product,
                    attempt_index=ordinal,
                    requested=True,
                    expected_task_name=None,
                    expected_model=None,
                    shell=None,
                    slot_id=f"{job_name}:unknown-requested-slot:{ordinal}",
                    job_metadata=job_metadata,
                )
            )
        warnings.append(
            f"{job_dir}: {requested_total - current_requested} requested slots could "
            "not be resolved to task names"
        )

    return rows, warnings


def ingest_jobs(
    job_dirs: Iterable[Path], *, product: str = "stella"
) -> tuple[list[dict], list[str]]:
    rows: list[dict] = []
    warnings: list[str] = []
    for job_dir in job_dirs:
        job_rows, job_warnings = ingest_job(job_dir, product=product)
        rows.extend(job_rows)
        warnings.extend(job_warnings)
    return rows, warnings


_MISSING = object()


def load_study_manifest(path: Path) -> dict[str, Any]:
    """Load a claim-gating study manifest without treating it as comparator data."""
    payload = _read_json(path)
    if not isinstance(payload, dict):
        raise ValueError(f"study manifest is not a JSON object: {path}")
    return payload


def load_run_ledger(path: Path) -> dict[str, Any]:
    """Load the append-only public run ledger used for paid-run gating."""
    payload = _read_json(path)
    if not isinstance(payload, dict):
        raise ValueError(f"run ledger is not a JSON object: {path}")
    return payload


def _required_manifest_value(
    section: Any,
    key: str,
    label: str,
    reasons: list[str],
) -> Any:
    if not isinstance(section, dict) or key not in section:
        reasons.append(f"Study manifest is missing required field {label}.")
        return _MISSING
    return section[key]


def _require_exact_manifest_fields(
    section: Any,
    expected_fields: frozenset[str],
    label: str,
    reasons: list[str],
) -> None:
    """Reject both extension fields and omissions from the frozen v6 contract."""
    if not isinstance(section, dict):
        reasons.append(
            f"Study manifest {label} must be an object with the exact v6 fields."
        )
        return
    observed_fields = set(section)
    if observed_fields == expected_fields:
        return
    missing = sorted(expected_fields - observed_fields)
    extra = sorted(observed_fields - expected_fields)
    reasons.append(
        f"Study manifest {label} fields differ from the exact v6 schema; "
        f"missing={missing!r}; extra={extra!r}."
    )


def _validate_engine_posture_manifest_schema(
    posture: Any,
    label: str,
    reasons: list[str],
) -> None:
    _require_exact_manifest_fields(
        posture,
        STUDY_MANIFEST_ENGINE_POSTURE_FIELDS,
        label,
        reasons,
    )
    if not isinstance(posture, dict):
        return
    agents = posture.get("agents")
    _require_exact_manifest_fields(
        agents,
        STUDY_MANIFEST_ENGINE_POSTURE_AGENT_ROLES,
        f"{label}.agents",
        reasons,
    )
    if not isinstance(agents, dict):
        return
    for role in sorted(STUDY_MANIFEST_ENGINE_POSTURE_AGENT_ROLES):
        if role not in agents:
            continue
        _require_exact_manifest_fields(
            agents[role],
            STUDY_MANIFEST_ENGINE_POSTURE_AGENT_FIELDS,
            f"{label}.agents.{role}",
            reasons,
        )


def _matches_setting(actual: Any, expected: Any) -> bool:
    if expected is None:
        return actual is None
    if isinstance(expected, bool):
        return isinstance(actual, bool) and actual is expected
    if isinstance(expected, (int, float)):
        number = _number(actual)
        return number is not None and float(number) == float(expected)
    return actual == expected


def _matches_harbor_setting(name: str, actual: Any, expected: Any) -> bool:
    if name != "retry_exclude_exceptions_json":
        return _matches_setting(actual, expected)
    if not isinstance(actual, str):
        return False
    try:
        parsed = json.loads(actual)
    except json.JSONDecodeError:
        return False
    return bool(
        isinstance(parsed, list)
        and all(isinstance(item, str) for item in parsed)
        and len(parsed) == len(CANONICAL_RETRY_EXCLUDE_EXCEPTIONS)
        and set(parsed) == CANONICAL_RETRY_EXCLUDE_EXCEPTIONS
    )


def _display_values(values: set[Any]) -> list[Any]:
    return sorted(values, key=lambda value: (type(value).__name__, repr(value)))


def _public_intent_attestation_reasons(
    row: dict[str, Any],
    *,
    expected_intent_sha256: str | None,
    expected_kind: str | None,
    expected_subject_commit: str | None,
    expected_ledger_commit: str | None,
    expected_runtime_identity: dict[str, Any] | None,
    expected_provider_key: dict[str, Any] | None,
    expected_prior_stage_outcome: dict[str, Any] | None,
    expected_projected_spend_usd: float | None,
    label: str,
) -> list[str]:
    reasons: list[str] = []
    encoded = row.get("launch_receipt_public_intent_attestation_json")
    try:
        attestation = json.loads(encoded) if isinstance(encoded, str) else None
    except json.JSONDecodeError:
        attestation = None
    if not isinstance(attestation, dict):
        return [f"{label} secure launch receipt has no JSON public-intent proof."]
    if (
        set(attestation) != PUBLIC_INTENT_ATTESTATION_FIELDS
        or row.get("launch_receipt_public_intent_exact_fields") is not True
    ):
        reasons.append(
            f"{label} public-intent proof differs from the exact preflight schema."
        )

    issue_number = attestation.get("issue_number")
    comment_id = attestation.get("comment_id")
    valid_issue_number = (
        isinstance(issue_number, int)
        and not isinstance(issue_number, bool)
        and issue_number > 0
    )
    valid_comment_id = (
        isinstance(comment_id, int)
        and not isinstance(comment_id, bool)
        and comment_id > 0
    )
    issue_url = (
        f"https://github.com/{FIXED_REPOSITORY}/issues/{issue_number}"
        if valid_issue_number
        else None
    )
    comment_url = attestation.get("comment_url")
    comment_match = (
        PUBLIC_INTENT_COMMENT_URL_RE.fullmatch(comment_url)
        if isinstance(comment_url, str)
        else None
    )
    if (
        not valid_issue_number
        or not valid_comment_id
        or attestation.get("issue_url") != issue_url
        or comment_match is None
        or int(comment_match.group("issue")) != issue_number
        or int(comment_match.group("comment")) != comment_id
    ):
        reasons.append(
            f"{label} public-intent proof does not identify one fixed-repository "
            "issue comment."
        )

    exact_values = {
        "schema_version": PUBLIC_INTENT_ATTESTATION_SCHEMA,
        "verification_mode": PUBLIC_INTENT_VERIFICATION_MODE,
        "repository": FIXED_REPOSITORY,
        "repository_private": False,
        "issue_title": (f"Stella Terminal-Bench 2.1 preregistration: {FIXED_STUDY_ID}"),
        "issue_author_login": "macanderson",
        "issue_author_association": "OWNER",
        "comment_author_login": "macanderson",
        "comment_author_association": "OWNER",
        "github_attestation_schema_version": GITHUB_ATTESTATION_SCHEMA,
        "study_id": FIXED_STUDY_ID,
        "subject_type": "intent",
        "subject_id": expected_intent_sha256,
        "kind": expected_kind,
        "subject_commit": expected_subject_commit,
        "ledger_commit": expected_ledger_commit,
        "ledger_path": FIXED_RUN_LEDGER_PATH,
        "intent_sha256": expected_intent_sha256,
        "safety_margin_seconds": PUBLIC_INTENT_SAFETY_MARGIN_SECONDS,
    }
    for field, expected in exact_values.items():
        if attestation.get(field) != expected:
            reasons.append(
                f"{label} public-intent proof field {field} does not match the "
                f"registered public intent: {attestation.get(field)!r} != "
                f"{expected!r}."
            )

    subject_commit = attestation.get("subject_commit")
    ledger_commit = attestation.get("ledger_commit")
    if (
        not isinstance(subject_commit, str)
        or re.fullmatch(r"[0-9a-f]{40}", subject_commit) is None
        or not isinstance(ledger_commit, str)
        or re.fullmatch(r"[0-9a-f]{40}", ledger_commit) is None
        or subject_commit == ledger_commit
    ):
        reasons.append(
            f"{label} public-intent proof lacks distinct full subject and ledger "
            "commits."
        )
    body_sha256 = attestation.get("body_sha256")
    if (
        not isinstance(body_sha256, str)
        or _normalized_sha256(body_sha256) != body_sha256
    ):
        reasons.append(
            f"{label} public-intent proof has no exact lowercase body SHA-256."
        )

    created_text = attestation.get("server_created_at")
    updated_text = attestation.get("server_updated_at")
    waited_text = attestation.get("safety_wait_completed_at_utc")
    final_get_text = attestation.get("final_comment_get_completed_at_utc")
    created = (
        _aware_timestamp(created_text)
        if isinstance(created_text, str) and created_text.endswith("Z")
        else None
    )
    waited = (
        _aware_timestamp(waited_text)
        if isinstance(waited_text, str) and waited_text.endswith("Z")
        else None
    )
    final_get = (
        _aware_timestamp(final_get_text)
        if isinstance(final_get_text, str) and final_get_text.endswith("Z")
        else None
    )
    if created is None or updated_text != created_text:
        reasons.append(
            f"{label} public-intent proof does not preserve one unedited GitHub "
            "server timestamp."
        )
    safety_target = (
        created + timedelta(seconds=PUBLIC_INTENT_SAFETY_MARGIN_SECONDS)
        if created is not None
        else None
    )
    if (
        safety_target is None
        or waited is None
        or final_get is None
        or waited < safety_target
        or final_get < waited
    ):
        reasons.append(
            f"{label} public-intent proof does not attest the two-second safety "
            "wait followed by a final anonymous GitHub GET."
        )

    ledger_sha256 = attestation.get("ledger_sha256")
    if (
        not isinstance(ledger_sha256, str)
        or _normalized_sha256(ledger_sha256) != ledger_sha256
    ):
        reasons.append(
            f"{label} public-intent proof has no exact ledger-snapshot SHA-256."
        )
    for field in (
        "subject_commit_verified",
        "ledger_commit_verified",
        "source_commit_verified",
        "strict_ancestry_verified",
        "runtime_revalidated_after_final_get",
    ):
        if attestation.get(field) is not True:
            reasons.append(f"{label} public-intent proof does not attest {field}.")

    runtime_identity = attestation.get("runtime_identity")
    if (
        not isinstance(runtime_identity, dict)
        or set(runtime_identity) != PUBLIC_INTENT_RUNTIME_IDENTITY_FIELDS
        or runtime_identity != expected_runtime_identity
    ):
        reasons.append(
            f"{label} public-intent proof runtime identity does not equal the "
            "immutable intent and observed runtime."
        )
    if attestation.get("prior_stage_outcome") != expected_prior_stage_outcome:
        reasons.append(
            f"{label} public-intent proof does not bind the exact completed prior "
            "stage outcome."
        )
    prior_stage_outcome = attestation.get("prior_stage_outcome")
    if prior_stage_outcome is not None and (
        not isinstance(prior_stage_outcome, dict)
        or set(prior_stage_outcome) != PUBLIC_INTENT_PRIOR_OUTCOME_FIELDS
    ):
        reasons.append(
            f"{label} public-intent proof prior-stage outcome schema drifts."
        )

    provider_snapshot = attestation.get("provider_key_live_snapshot")
    provider_fetched = None
    if (
        not isinstance(provider_snapshot, dict)
        or set(provider_snapshot) != PUBLIC_INTENT_PROVIDER_SNAPSHOT_FIELDS
    ):
        reasons.append(
            f"{label} public-intent proof live provider-key snapshot schema drifts."
        )
    else:
        expected_provider_key = expected_provider_key or {}
        limit = _nonnegative_float(provider_snapshot.get("limit_usd"))
        usage = _nonnegative_float(provider_snapshot.get("usage_usd"))
        remaining = _nonnegative_float(provider_snapshot.get("limit_remaining_usd"))
        projected = _nonnegative_float(
            provider_snapshot.get("nominal_planned_spend_usd")
        )
        projected_remaining = _number(
            provider_snapshot.get("nominal_remaining_after_usd")
        )
        total_credits = _nonnegative_float(provider_snapshot.get("total_credits_usd"))
        total_usage = _nonnegative_float(provider_snapshot.get("total_usage_usd"))
        available_credits = _nonnegative_float(
            provider_snapshot.get("available_credits_usd")
        )
        if (
            provider_snapshot.get("fingerprint_sha256")
            != expected_provider_key.get("fingerprint_sha256")
            or provider_snapshot.get("label") != expected_provider_key.get("label")
            or limit != _nonnegative_float(expected_provider_key.get("limit_usd"))
            or limit != DEDICATED_KEY_LIMIT_USD
            or usage
            != _nonnegative_float(expected_provider_key.get("usage_before_usd"))
            or usage is None
            or remaining is None
            or limit is None
            or not math.isclose(remaining, limit - (usage or 0.0), abs_tol=1e-6)
            or projected != expected_projected_spend_usd
            or projected_remaining is None
            or not math.isclose(
                float(projected_remaining),
                remaining - (projected or 0.0),
                abs_tol=1e-6,
            )
            or total_credits is None
            or total_usage is None
            or available_credits is None
            or not math.isclose(
                available_credits, total_credits - total_usage, abs_tol=1e-6
            )
            or available_credits + 1e-9 < (projected or 0.0)
        ):
            reasons.append(
                f"{label} public-intent proof live provider-key snapshot does not "
                "equal the immutable intent and no-reset budget."
            )
        fetched_text = provider_snapshot.get("fetched_at_utc")
        provider_fetched = (
            _aware_timestamp(fetched_text)
            if isinstance(fetched_text, str) and fetched_text.endswith("Z")
            else None
        )
    runtime_revalidated_text = attestation.get("runtime_revalidated_at_utc")
    runtime_revalidated = (
        _aware_timestamp(runtime_revalidated_text)
        if isinstance(runtime_revalidated_text, str)
        and runtime_revalidated_text.endswith("Z")
        else None
    )
    if (
        final_get is None
        or provider_fetched is None
        or runtime_revalidated is None
        or provider_fetched < final_get
        or runtime_revalidated < provider_fetched
    ):
        reasons.append(
            f"{label} public-intent proof does not order final GitHub GET, live "
            "provider check, and runtime revalidation before launch."
        )
    return reasons


def _launch_receipt_reasons(
    row: dict[str, Any],
    *,
    expected_job_name: str,
    expected_models: Sequence[str],
    expected_intent_sha256: str | None,
    expected_kind: str | None,
    expected_subject_commit: str | None,
    expected_ledger_commit: str | None,
    expected_runtime_identity: dict[str, Any] | None,
    expected_provider_key: dict[str, Any] | None,
    expected_prior_stage_outcome: dict[str, Any] | None,
    expected_projected_spend_usd: float | None,
    label: str,
) -> list[str]:
    reasons: list[str] = []
    source_text = row.get("source_input")
    source = Path(source_text) if isinstance(source_text, str) else None
    expected_path = (
        str((source / SECURE_LAUNCH_RECEIPT_FILENAME).resolve())
        if source is not None
        else None
    )
    expected_controls_json = _normalized_json_object(SECURE_LAUNCH_CONTROLS)
    expected_models_json = _normalized_json_value(list(expected_models))
    expectations = {
        "launch_receipt_present": True,
        "launch_receipt_schema_version": SECURE_LAUNCH_RECEIPT_SCHEMA,
        "launch_receipt_job_name": expected_job_name,
        "launch_receipt_models_json": expected_models_json,
        "launch_receipt_intent_sha256": expected_intent_sha256,
        "launch_receipt_controls_json": expected_controls_json,
        "launch_receipt_exact_top_level": True,
        "launch_receipt_regular_file": True,
        "launch_receipt_mode_octal": "0600",
        "launch_receipt_path": expected_path,
    }
    for field, expected in expectations.items():
        if row.get(field) != expected:
            reasons.append(
                f"{label} {field} does not match the creation-only secure launch "
                f"receipt contract: {row.get(field)!r} != {expected!r}."
            )
    if _normalized_sha256(row.get("launch_receipt_sha256")) is None:
        reasons.append(f"{label} secure launch receipt has no valid SHA-256.")
    reasons.extend(
        _public_intent_attestation_reasons(
            row,
            expected_intent_sha256=expected_intent_sha256,
            expected_kind=expected_kind,
            expected_subject_commit=expected_subject_commit,
            expected_ledger_commit=expected_ledger_commit,
            expected_runtime_identity=expected_runtime_identity,
            expected_provider_key=expected_provider_key,
            expected_prior_stage_outcome=expected_prior_stage_outcome,
            expected_projected_spend_usd=expected_projected_spend_usd,
            label=label,
        )
    )
    if source is None or source.name != expected_job_name:
        reasons.append(
            f"{label} job directory basename does not equal the literal job name."
        )
    jobs_dir = row.get("job_jobs_dir")
    if (
        not isinstance(jobs_dir, str)
        or not jobs_dir
        or not Path(jobs_dir).is_absolute()
    ):
        reasons.append(
            f"{label} config jobs_dir is not the absolute creation-time Harbor "
            "jobs directory."
        )
    return reasons


def _host_timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value.endswith("Z"):
        return None
    parsed = _aware_timestamp(value)
    return parsed.astimezone(UTC) if parsed is not None else None


def _host_snapshot_validation(
    snapshot: Any, *, label: str
) -> tuple[list[str], dict[str, Any] | None]:
    """Validate one exact public/live host snapshot and recompute every check."""
    reasons: list[str] = []
    if not isinstance(snapshot, dict) or set(snapshot) != HOST_SNAPSHOT_FIELDS:
        return [f"{label} does not have the exact host-snapshot fields."], None
    captured = _host_timestamp(snapshot.get("captured_at_utc"))
    fingerprint = snapshot.get("host_fingerprint_sha256")
    if captured is None:
        reasons.append(f"{label} captured_at_utc is not an aware UTC timestamp.")
    if (
        not isinstance(fingerprint, str)
        or _normalized_sha256(fingerprint) != fingerprint
    ):
        reasons.append(f"{label} host fingerprint is not a lowercase SHA-256.")

    observed = snapshot.get("observed")
    if not isinstance(observed, dict) or set(observed) != HOST_OBSERVED_FIELDS:
        reasons.append(f"{label} observed host record has schema drift.")
        return reasons, None
    os_record = observed.get("os")
    cpu = observed.get("cpu")
    memory = observed.get("memory")
    disk = observed.get("disk")
    docker = observed.get("docker")
    nested = (
        (os_record, HOST_OS_FIELDS, "OS"),
        (cpu, HOST_CPU_FIELDS, "CPU"),
        (memory, HOST_MEMORY_FIELDS, "memory"),
        (disk, HOST_DISK_FIELDS, "disk"),
        (docker, HOST_DOCKER_FIELDS, "Docker"),
    )
    for record, fields, name in nested:
        if not isinstance(record, dict) or set(record) != fields:
            reasons.append(f"{label} {name} record has schema drift.")
    if reasons:
        return reasons, None
    assert isinstance(os_record, dict)
    assert isinstance(cpu, dict)
    assert isinstance(memory, dict)
    assert isinstance(disk, dict)
    assert isinstance(docker, dict)

    text_fields = (
        *(os_record.get(field) for field in HOST_OS_FIELDS),
        observed.get("architecture"),
        cpu.get("model"),
        disk.get("probe_path"),
        *(
            docker.get(field)
            for field in HOST_DOCKER_FIELDS
            if field != "reported_running_containers"
        ),
    )
    if any(
        not isinstance(value, str)
        or not value
        or value.strip() != value
        or any(ord(character) < 32 for character in value)
        for value in text_fields
    ):
        reasons.append(f"{label} contains a noncanonical host identity field.")
    vcpus = cpu.get("effective_vcpus")
    memory_total = memory.get("total_bytes")
    disk_total = disk.get("total_bytes")
    disk_used = disk.get("used_bytes")
    disk_free = disk.get("free_bytes")
    running_reported = docker.get("reported_running_containers")
    integer_fields = (
        vcpus,
        memory_total,
        disk_total,
        disk_used,
        disk_free,
        running_reported,
    )
    if any(
        not isinstance(value, int) or isinstance(value, bool) or value < 0
        for value in integer_fields
    ):
        reasons.append(f"{label} contains a non-integer or negative host quantity.")
        return reasons, None
    assert isinstance(vcpus, int)
    assert isinstance(memory_total, int)
    assert isinstance(disk_total, int)
    assert isinstance(disk_used, int)
    assert isinstance(disk_free, int)
    assert isinstance(running_reported, int)
    if vcpus == 0 or memory_total == 0 or disk_total == 0:
        reasons.append(f"{label} has a zero CPU, memory, or disk capacity.")
    if disk_used + disk_free > disk_total:
        reasons.append(f"{label} disk byte accounting is inconsistent.")
    probe_path = disk.get("probe_path")
    if not isinstance(probe_path, str) or not Path(probe_path).is_absolute():
        reasons.append(f"{label} disk probe path is not absolute.")
    containers = observed.get("running_container_ids")
    if (
        not isinstance(containers, list)
        or any(
            not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None
            for value in containers
        )
        or len(set(containers)) != len(containers)
        or running_reported != len(containers)
    ):
        reasons.append(f"{label} running-container evidence is inconsistent.")
        containers = [] if not isinstance(containers, list) else containers

    expected_checks = {
        "native_linux_x86_64": (
            os_record.get("system") == "Linux"
            and observed.get("architecture") == "x86_64"
        ),
        "minimum_vcpus": vcpus >= HOST_MIN_VCPUS,
        "minimum_memory": memory_total >= HOST_MIN_MEMORY_BYTES,
        "minimum_free_disk": disk_free >= HOST_MIN_FREE_DISK_BYTES,
        "docker_native_linux_x86_64": (
            docker.get("server_os") == "linux"
            and docker.get("server_architecture") == "x86_64"
        ),
        "zero_running_containers": not containers and running_reported == 0,
    }
    expected_checks["all_passed"] = all(expected_checks.values())
    checks = snapshot.get("checks")
    if (
        not isinstance(checks, dict)
        or set(checks) != HOST_CHECK_FIELDS
        or any(not isinstance(value, bool) for value in checks.values())
        or checks != expected_checks
        or checks.get("all_passed") is not True
    ):
        failed = [
            name
            for name, passed in expected_checks.items()
            if name != "all_passed" and not passed
        ]
        suffix = f": {', '.join(failed)}" if failed else ""
        reasons.append(f"{label} host eligibility checks do not all pass{suffix}.")
    return reasons, {
        "captured": captured,
        "fingerprint": fingerprint,
        "observed": observed,
    }


def _host_attestation_reasons(
    row: dict[str, Any],
    *,
    expected_job_name: str,
    expected_intent_sha256: str | None,
    expected_stage: str,
    label: str,
) -> list[str]:
    """Require and independently replay the launcher's public/live host binding."""
    reasons: list[str] = []
    source_text = row.get("source_input")
    source = Path(source_text) if isinstance(source_text, str) else None
    expected_path = (
        str((source / HOST_ATTESTATION_FILENAME).resolve())
        if source is not None
        else None
    )
    expectations = {
        "host_attestation_present": True,
        "host_attestation_schema_version": HOST_LAUNCH_BINDING_SCHEMA,
        "host_attestation_exact_top_level": True,
        "host_attestation_canonical_json": True,
        "host_attestation_regular_file": True,
        "host_attestation_mode_octal": "0600",
        "host_attestation_path": expected_path,
    }
    for field, expected in expectations.items():
        if row.get(field) != expected:
            reasons.append(
                f"{label} {field} does not match the immutable host-sidecar "
                f"contract: {row.get(field)!r} != {expected!r}."
            )
    if _normalized_sha256(row.get("host_attestation_sha256")) is None:
        reasons.append(f"{label} host sidecar has no valid SHA-256.")
    encoded = row.get("host_attestation_json")
    try:
        payload = json.loads(encoded) if isinstance(encoded, str) else None
    except json.JSONDecodeError:
        payload = None
    if not isinstance(payload, dict) or set(payload) != HOST_BINDING_FIELDS:
        reasons.append(f"{label} host sidecar JSON has schema drift.")
        return reasons
    canonical_sidecar = _canonical_json_file_bytes(payload)
    if canonical_sidecar is None or hashlib.sha256(
        canonical_sidecar
    ).hexdigest() != row.get("host_attestation_sha256"):
        reasons.append(f"{label} host sidecar SHA-256 does not bind its payload.")
    exact_binding = {
        "schema_version": HOST_LAUNCH_BINDING_SCHEMA,
        "study_id": FIXED_STUDY_ID,
        "intent_sha256": expected_intent_sha256,
        "stage": expected_stage,
        "job_name": expected_job_name,
    }
    for field, expected in exact_binding.items():
        if payload.get(field) != expected:
            reasons.append(
                f"{label} host sidecar field {field} does not match the paid intent."
            )
    receipt_sha = payload.get("launch_receipt_sha256")
    if (
        not isinstance(receipt_sha, str)
        or _normalized_sha256(receipt_sha) != receipt_sha
        or receipt_sha != row.get("launch_receipt_sha256")
    ):
        reasons.append(f"{label} host sidecar does not bind the exact launch receipt.")

    reference = payload.get("public_report")
    if (
        not isinstance(reference, dict)
        or set(reference) != HOST_PUBLIC_REFERENCE_FIELDS
    ):
        reasons.append(f"{label} public host-report reference has schema drift.")
        return reasons
    expected_public_path = (
        f"{HOST_REPORT_PATH_PREFIX}/{expected_intent_sha256}.json"
        if isinstance(expected_intent_sha256, str)
        else None
    )
    public_intent_encoded = row.get("launch_receipt_public_intent_attestation_json")
    try:
        public_intent = (
            json.loads(public_intent_encoded)
            if isinstance(public_intent_encoded, str)
            else None
        )
    except json.JSONDecodeError:
        public_intent = None
    expected_public_commit = (
        public_intent.get("ledger_commit") if isinstance(public_intent, dict) else None
    )
    if (
        reference.get("repository") != FIXED_REPOSITORY
        or not isinstance(reference.get("commit"), str)
        or re.fullmatch(r"[0-9a-f]{40}", reference["commit"]) is None
        or reference.get("commit") != expected_public_commit
        or reference.get("path") != expected_public_path
        or _normalized_sha256(reference.get("sha256")) != reference.get("sha256")
    ):
        reasons.append(f"{label} public host-report identity is not canonical.")
    fetched = _host_timestamp(reference.get("fetched_at_utc"))
    if fetched is None:
        reasons.append(f"{label} public host-report fetch time is invalid.")

    report = payload.get("public_report_payload")
    if not isinstance(report, dict) or set(report) != HOST_REPORT_FIELDS:
        reasons.append(f"{label} public host-report payload has schema drift.")
        return reasons
    report_exact = {
        "schema_version": HOST_REPORT_SCHEMA,
        "study_id": FIXED_STUDY_ID,
        "intent_sha256": expected_intent_sha256,
        "stage": expected_stage,
        "job_name": expected_job_name,
        "requirements": HOST_REQUIREMENTS,
    }
    for field, expected in report_exact.items():
        if report.get(field) != expected:
            reasons.append(
                f"{label} public host-report field {field} is not frozen exactly."
            )
    canonical_report = _canonical_json_file_bytes(report)
    if canonical_report is None or hashlib.sha256(
        canonical_report
    ).hexdigest() != reference.get("sha256"):
        reasons.append(f"{label} public host-report SHA-256 does not bind its payload.")

    public_snapshot = {
        "captured_at_utc": report.get("captured_at_utc"),
        "host_fingerprint_sha256": report.get("host_fingerprint_sha256"),
        "observed": report.get("observed"),
        "checks": report.get("checks"),
    }
    public_reasons, public = _host_snapshot_validation(
        public_snapshot, label=f"{label} public report"
    )
    live_reasons, live = _host_snapshot_validation(
        payload.get("live_recheck"), label=f"{label} live recheck"
    )
    reasons.extend(public_reasons)
    reasons.extend(live_reasons)
    if public is None or live is None:
        return reasons
    public_observed = public["observed"]
    live_observed = live["observed"]
    jobs_dir = row.get("job_jobs_dir")
    if (
        not isinstance(jobs_dir, str)
        or not Path(jobs_dir).is_absolute()
        or public_observed["disk"]["probe_path"] != jobs_dir
        or live_observed["disk"]["probe_path"] != jobs_dir
    ):
        reasons.append(
            f"{label} host disk evidence does not probe the configured jobs_dir."
        )
    same_static_host = bool(
        public["fingerprint"] == live["fingerprint"]
        and public_observed["os"] == live_observed["os"]
        and public_observed["architecture"] == live_observed["architecture"]
        and public_observed["cpu"] == live_observed["cpu"]
        and public_observed["memory"] == live_observed["memory"]
        and public_observed["disk"]["probe_path"] == live_observed["disk"]["probe_path"]
        and public_observed["disk"]["total_bytes"]
        == live_observed["disk"]["total_bytes"]
        and public_observed["docker"] == live_observed["docker"]
    )
    if not same_static_host:
        reasons.append(f"{label} live recheck is not the public host and boot.")
    public_captured = public["captured"]
    live_captured = live["captured"]
    job_started = _harbor_timestamp(row.get("job_started_at"), row)
    if (
        public_captured is None
        or fetched is None
        or live_captured is None
        or not (public_captured <= fetched <= live_captured)
        or (live_captured - public_captured).total_seconds()
        > HOST_MAX_REPORT_AGE_SECONDS
        or job_started is None
        or live_captured > job_started
    ):
        reasons.append(
            f"{label} host report/fetch/live recheck is not demonstrably prelaunch."
        )
    return reasons


def _exact_harbor_job_reasons(
    row: dict[str, Any], *, expected_n_concurrent: int, label: str
) -> list[str]:
    reasons: list[str] = []
    if row.get("job_n_concurrent_trials") != expected_n_concurrent:
        reasons.append(
            f"{label} n_concurrent_trials must be {expected_n_concurrent}; "
            f"observed {row.get('job_n_concurrent_trials')!r}."
        )
    if row.get("job_harbor_unknown_fields"):
        reasons.append(
            f"{label} contains fields outside the normalized Harbor allowlist: "
            f"{row.get('job_harbor_unknown_fields')}."
        )
    for name, canonical in CANONICAL_HARBOR_JOB_SETTINGS.items():
        actual = row.get(f"job_{name}")
        if not _matches_harbor_setting(name, actual, canonical):
            reasons.append(
                f"{label} noncanonically sets {name}: expected {canonical!r}, "
                f"observed {actual!r}."
            )
    for name, canonical in CANONICAL_HARBOR_DATASET_SETTINGS.items():
        actual = row.get(f"job_dataset_{name}")
        if not _matches_harbor_setting(name, actual, canonical):
            reasons.append(
                f"{label} dataset noncanonically sets {name}: expected "
                f"{canonical!r}, observed {actual!r}."
            )
    return reasons


def _exact_harbor_trial_reasons(row: dict[str, Any], *, label: str) -> list[str]:
    reasons: list[str] = []
    if row.get("trial_harbor_unknown_fields"):
        reasons.append(
            f"{label} contains fields outside the normalized Harbor allowlist: "
            f"{row.get('trial_harbor_unknown_fields')}."
        )
    trial_expectations = {
        "trial_artifacts_json": "[]",
        "trial_task_path": None,
        "trial_task_git_url": None,
        "trial_task_git_commit_id": None,
        "trial_task_overwrite": False,
        "trial_task_download_dir": None,
    }
    for field, expected in trial_expectations.items():
        if not _matches_setting(row.get(field), expected):
            reasons.append(
                f"{label} {field} is not canonical: expected {expected!r}, "
                f"observed {row.get(field)!r}."
            )
    trials_dir = row.get("trial_trials_dir")
    if (
        not isinstance(trials_dir, str)
        or not Path(trials_dir).is_absolute()
        or Path(trials_dir).name != row.get("job_name")
    ):
        reasons.append(
            f"{label} trial_trials_dir is not the absolute creation-time job path."
        )
    return reasons


def _readiness_harbor_reasons(row: dict[str, Any]) -> list[str]:
    """Validate Harbor's exact path-only local-task serialization for the sentinel."""
    reasons: list[str] = []
    label = "Readiness Harbor job"
    if row.get("job_harbor_missing_fields"):
        reasons.append(
            f"{label} omits canonical setting fields: "
            f"{row.get('job_harbor_missing_fields')}."
        )
    if row.get("trial_harbor_missing_fields"):
        reasons.append(
            "Readiness trial omits canonical Harbor setting fields: "
            f"{row.get('trial_harbor_missing_fields')}."
        )
    if row.get("job_harbor_unknown_fields") or row.get("trial_harbor_unknown_fields"):
        reasons.append("Readiness Harbor config contains fields outside the allowlist.")
    for scope in ("job", "trial"):
        for name, canonical in CANONICAL_HARBOR_SETTINGS.items():
            actual = row.get(f"{scope}_{name}")
            if not _matches_setting(actual, canonical):
                reasons.append(
                    f"Readiness {scope} noncanonically sets {name}: expected "
                    f"{canonical!r}, observed {actual!r}."
                )
    if row.get("job_n_concurrent_trials") != 1 or row.get("job_n_attempts") != 1:
        reasons.append(
            "Readiness Harbor job must be exactly one attempt at concurrency 1."
        )
    if row.get("job_dataset_count") != 0 or row.get("job_task_count") != 1:
        reasons.append(
            "Readiness must serialize as one path-only task and zero registry datasets."
        )
    if row.get("task_ref") is not None:
        reasons.append(
            "Readiness LocalTaskId must preserve Harbor's null task ref; identity "
            "comes from the tracked path and observed task checksum."
        )
    for name, canonical in CANONICAL_HARBOR_JOB_SETTINGS.items():
        actual = row.get(f"job_{name}")
        if name == "tasks_json":
            continue
        if not _matches_harbor_setting(name, actual, canonical):
            reasons.append(
                f"Readiness Harbor job noncanonically sets {name}: expected "
                f"{canonical!r}, observed {actual!r}."
            )
    task_path = row.get("trial_task_path")
    if not isinstance(task_path, str) or not Path(task_path).is_absolute():
        reasons.append(
            "Readiness trial task path must be absolute after Harbor resolution."
        )
        resolved_task_path: str | None = None
    else:
        resolved_task_path = Path(task_path).resolve().as_posix()
        if not resolved_task_path.endswith("/" + READINESS_TASK_RELATIVE_PATH):
            reasons.append(
                "Readiness trial task path is not the tracked local sentinel path."
            )
    tasks_json = row.get("job_tasks_json")
    try:
        job_tasks = json.loads(tasks_json) if isinstance(tasks_json, str) else None
    except json.JSONDecodeError:
        job_tasks = None
    expected_task = {
        "path": resolved_task_path,
        "git_url": None,
        "git_commit_id": None,
        "name": None,
        "ref": None,
        "overwrite": False,
        "download_dir": None,
        "source": None,
    }
    if job_tasks != [expected_task]:
        reasons.append(
            "Readiness job tasks_json is not Harbor's exact one path-only TaskConfig."
        )
    trial_expectations = {
        "trial_artifacts_json": "[]",
        "trial_task_git_url": None,
        "trial_task_git_commit_id": None,
        "trial_task_overwrite": False,
        "trial_task_download_dir": None,
    }
    for field, expected in trial_expectations.items():
        if not _matches_setting(row.get(field), expected):
            reasons.append(
                f"Readiness trial {field} is not canonical: expected {expected!r}, "
                f"observed {row.get(field)!r}."
            )
    trials_dir = row.get("trial_trials_dir")
    if (
        not isinstance(trials_dir, str)
        or not Path(trials_dir).is_absolute()
        or Path(trials_dir).name != row.get("job_name")
    ):
        reasons.append(
            "Readiness trial_trials_dir is not the absolute creation-time job path."
        )
    return reasons


def _trial_telemetry_reasons(
    row: dict[str, Any], *, allowed_call_models: Sequence[str], label: str
) -> list[str]:
    """Return claim-blocking reasons for one Stella trial's raw call evidence."""
    reasons: list[str] = []
    if row.get("host_credential_source") != CANONICAL_HOST_CREDENTIAL_SOURCE:
        reasons.append(
            f"{label} did not use the claim-eligible host credential source: "
            f"{row.get('host_credential_source')!r}."
        )
    if row.get("host_credential_name") != CANONICAL_HOST_CREDENTIAL_NAME:
        reasons.append(
            f"{label} did not bundle exactly the registered provider credential."
        )
    if _nonnegative_int(row.get("host_credential_bundle_count")) != 1:
        reasons.append(f"{label} host credential bundle count is not exactly one.")
    if row.get("container_credential_absence_verified") is not True:
        reasons.append(
            f"{label} did not verify the active key was absent from every live "
            "container Config immediately before handoff."
        )
    if row.get("atif_host_credential_source") != CANONICAL_HOST_CREDENTIAL_SOURCE:
        reasons.append(f"{label} ATIF omits the claim-eligible credential source.")
    if row.get("atif_host_credential_name") != CANONICAL_HOST_CREDENTIAL_NAME:
        reasons.append(f"{label} ATIF omits the registered credential name.")
    if _nonnegative_int(row.get("atif_host_credential_bundle_count")) != 1:
        reasons.append(f"{label} ATIF credential bundle count is not exactly one.")
    if row.get("atif_container_credential_absence_verified") is not True:
        reasons.append(
            f"{label} ATIF does not attest live container credential absence."
        )
    state = row.get("accounting_state")
    if state != "complete":
        reasons.append(f"{label} accounting state is not complete: {state!r}.")

    stream_complete = row.get("stream_complete")
    stream_status = row.get("stream_status")
    terminal_event = row.get("stream_terminal_event")
    if stream_complete is not True or stream_status == "interrupted":
        reasons.append(
            f"{label} stream is incomplete/interrupted: complete="
            f"{stream_complete!r}, status={stream_status!r}."
        )
    if terminal_event not in {"complete", "error"}:
        reasons.append(
            f"{label} has no recognized terminal stream event: {terminal_event!r}."
        )

    cost_source = row.get("stream_cost_source")
    cost_consistency = row.get("accounting_cost_consistency")
    if cost_source == "complete_event":
        if cost_consistency != "consistent":
            reasons.append(
                f"{label} has an independent terminal cost total but cost "
                f"consistency is {cost_consistency!r}."
            )
    elif cost_source == "summed_step_usage":
        if cost_consistency != "derived_from_step_usage":
            reasons.append(
                f"{label} has a derived cost total with unexpected consistency "
                f"state {cost_consistency!r}."
            )
    else:
        reasons.append(f"{label} has unknown cost provenance: {cost_source!r}.")

    model_state = row.get("accounting_model_state")
    usage_records = _nonnegative_int(row.get("accounting_step_usage_records"))
    model_records = _nonnegative_int(row.get("accounting_model_records"))
    models = row.get("accounting_models")
    observed_models = models.split("|") if isinstance(models, str) and models else []
    if model_state != "complete" or usage_records in (None, 0):
        reasons.append(
            f"{label} StepUsage model telemetry is not complete: state="
            f"{model_state!r}, usage_records={usage_records!r}."
        )
    if model_records != usage_records:
        reasons.append(
            f"{label} reports models for {model_records!r}/{usage_records!r} "
            "StepUsage records."
        )
    unexpected_models = sorted(set(observed_models) - set(allowed_call_models))
    if not observed_models or unexpected_models:
        reasons.append(
            f"{label} StepUsage models are outside the frozen allowed call-model "
            f"roster {list(allowed_call_models)!r}: observed {observed_models!r}."
        )

    for observed_field, accounting_field in (
        ("prompt_tokens", "accounting_input_tokens"),
        ("completion_tokens", "accounting_output_tokens"),
        ("cache_tokens", "accounting_cached_input_tokens"),
    ):
        if row.get(observed_field) != row.get(accounting_field):
            reasons.append(
                f"{label} Harbor {observed_field} does not match raw StepUsage "
                f"accounting: {row.get(observed_field)!r} != "
                f"{row.get(accounting_field)!r}."
            )
    envelope_cost = _nonnegative_float(row.get("accounting_envelope_total_cost_usd"))
    harbor_cost = _nonnegative_float(row.get("cost_usd"))
    if (
        envelope_cost is None
        or harbor_cost is None
        or not math.isclose(
            envelope_cost,
            harbor_cost,
            rel_tol=1e-6,
            abs_tol=1e-9,
        )
    ):
        reasons.append(
            f"{label} Harbor cost does not match the envelope total: "
            f"{harbor_cost!r} != {envelope_cost!r}."
        )
    return reasons


def _comparator_task_identity_map(
    rows: Sequence[dict[str, Any]],
) -> tuple[dict[str, dict[str, str]], list[str]]:
    refs: dict[str, set[str]] = defaultdict(set)
    checksums: dict[str, set[str]] = defaultdict(set)
    tasks = {
        row.get("task")
        for row in rows
        if row.get("attempted") is True and isinstance(row.get("task"), str)
    }
    for row in rows:
        task = row.get("task")
        if row.get("attempted") is not True or not isinstance(task, str):
            continue
        ref = row.get("task_ref")
        checksum = _normalized_sha256(row.get("task_checksum"))
        if isinstance(ref, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", ref):
            refs[task].add(ref)
        else:
            refs[task].add("<missing-or-invalid>")
        checksums[task].add(checksum or "<missing-or-invalid>")
    identity_map: dict[str, dict[str, str]] = {}
    reasons: list[str] = []
    for task in sorted(tasks):
        task_refs = refs[task]
        task_checksums = checksums[task]
        if (
            len(task_refs) != 1
            or "<missing-or-invalid>" in task_refs
            or len(task_checksums) != 1
            or "<missing-or-invalid>" in task_checksums
        ):
            reasons.append(
                f"Comparator task {task!r} does not have one consistent valid "
                "task ref and checksum across all five public trials."
            )
            continue
        identity_map[task] = {
            "task_ref": next(iter(task_refs)),
            "task_checksum": next(iter(task_checksums)),
        }
    return identity_map, reasons


def _task_set_sha256(identity_map: dict[str, dict[str, str]]) -> str:
    tasks = [
        {
            "task": task,
            "task_ref": identity_map[task]["task_ref"],
            "task_checksum": identity_map[task]["task_checksum"],
        }
        for task in sorted(identity_map)
    ]
    encoded = json.dumps(
        {"schema": "stella-tb21-task-set-v1", "tasks": tasks},
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _canonical_payload_sha256(value: Any) -> str:
    encoded = json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _aware_timestamp(value: Any) -> datetime | None:
    parsed = _parse_timestamp(value)
    if parsed is None or parsed.tzinfo is None:
        return None
    return parsed


def _harbor_timestamp(value: Any, row: dict[str, Any]) -> datetime | None:
    """Parse Harbor timestamps, accepting naive values only under an attested UTC TZ."""
    if not isinstance(value, str) or not value:
        return None
    normalized = value[:-1] + "+00:00" if value.endswith("Z") else value
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    if parsed.tzinfo is not None:
        return parsed
    expected_controls = _normalized_json_object(SECURE_LAUNCH_CONTROLS)
    if (
        SECURE_LAUNCH_CONTROLS.get("harbor_clock_timezone") == "UTC"
        and row.get("launch_receipt_controls_json") == expected_controls
    ):
        return parsed.replace(tzinfo=UTC)
    return None


def _valid_commit_publication(record: dict[str, Any]) -> bool:
    commit = record.get("ledger_commit")
    url = record.get("public_url")
    return bool(
        isinstance(commit, str)
        and re.fullmatch(r"[0-9a-f]{40}", commit)
        and isinstance(url, str)
        and re.fullmatch(
            rf"https://github\.com/[^/]+/[^/]+/commit/{re.escape(commit)}",
            url,
        )
    )


def _task_identity_map_from_rows(
    rows: Sequence[dict[str, Any]], *, label: str
) -> tuple[dict[str, dict[str, str]], list[str]]:
    refs: dict[str, set[str]] = defaultdict(set)
    checksums: dict[str, set[str]] = defaultdict(set)
    for row in rows:
        if row.get("attempted") is not True or not isinstance(row.get("task"), str):
            continue
        task = row["task"]
        ref = row.get("task_ref")
        checksum = _normalized_sha256(row.get("task_checksum"))
        refs[task].add(
            ref
            if isinstance(ref, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", ref)
            else "<missing-or-invalid>"
        )
        checksums[task].add(checksum or "<missing-or-invalid>")
    identity_map: dict[str, dict[str, str]] = {}
    reasons: list[str] = []
    for task in sorted(refs):
        if (
            len(refs[task]) != 1
            or "<missing-or-invalid>" in refs[task]
            or len(checksums[task]) != 1
            or "<missing-or-invalid>" in checksums[task]
        ):
            reasons.append(
                f"{label} task {task!r} lacks one consistent valid ref/checksum."
            )
            continue
        identity_map[task] = {
            "task_ref": next(iter(refs[task])),
            "task_checksum": next(iter(checksums[task])),
        }
    return identity_map, reasons


def _job_evidence(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    attempted = [row for row in rows if row.get("attempted") is True]
    temporal_reasons: list[str] = []
    job_start_values = {row.get("job_started_at") for row in rows}
    job_finish_values = {row.get("job_finished_at") for row in rows}
    first_row = rows[0] if rows else {}
    job_started_raw = (
        next(iter(job_start_values)) if len(job_start_values) == 1 else None
    )
    job_finished_raw = (
        next(iter(job_finish_values)) if len(job_finish_values) == 1 else None
    )
    job_started = _harbor_timestamp(job_started_raw, first_row)
    job_finished = _harbor_timestamp(job_finished_raw, first_row)
    if job_started is None or job_finished is None or job_finished < job_started:
        temporal_reasons.append(
            "Paid job lacks one valid authoritative Harbor root start/finish interval."
        )
    for row in rows:
        if row.get("instantiated") is not True:
            continue
        trial_started = _harbor_timestamp(row.get("trial_started_at"), row)
        trial_finished = _harbor_timestamp(row.get("trial_finished_at"), row)
        if (
            trial_started is None
            or trial_finished is None
            or trial_finished < trial_started
        ):
            temporal_reasons.append(
                f"Instantiated slot {row.get('slot_id')!r} lacks authoritative "
                "Harbor trial boundaries."
            )
            continue
        if (
            job_started is not None
            and job_finished is not None
            and (trial_started < job_started or trial_finished > job_finished)
        ):
            temporal_reasons.append(
                f"Instantiated slot {row.get('slot_id')!r} lies outside its Harbor "
                "root job interval."
            )
    costs = [_nonnegative_float(row.get("cost_usd")) for row in attempted]
    artifacts = {
        value
        for row in rows
        if (value := _normalized_sha256(row.get("job_artifact_tree_sha256")))
        is not None
    }
    identities, identity_reasons = _task_identity_map_from_rows(rows, label="Paid job")
    return {
        "attempted": attempted,
        "started_at": job_started_raw if job_started is not None else None,
        "completed_at": job_finished_raw if job_finished is not None else None,
        "cost_sum": (
            sum(float(value) for value in costs if value is not None)
            if attempted and all(value is not None for value in costs)
            else None
        ),
        "artifact_tree_sha256": next(iter(artifacts)) if len(artifacts) == 1 else None,
        "artifact_hash_count": len(artifacts),
        "task_identity_map": identities,
        "task_identity_reasons": identity_reasons,
        "temporal_reasons": temporal_reasons,
    }


def _validate_comparator(
    manifest: dict[str, Any],
    rows: Sequence[dict[str, Any]] | None,
    structural_reasons: list[str],
    reasons: list[str],
) -> dict[str, Any]:
    section = manifest.get("comparator")
    submission = section.get("submission") if isinstance(section, dict) else None
    expected = section.get("expected") if isinstance(section, dict) else None
    _require_exact_manifest_fields(
        section,
        STUDY_MANIFEST_COMPARATOR_FIELDS,
        "comparator",
        structural_reasons,
    )
    _require_exact_manifest_fields(
        submission,
        STUDY_MANIFEST_COMPARATOR_SUBMISSION_FIELDS,
        "comparator.submission",
        structural_reasons,
    )
    _require_exact_manifest_fields(
        expected,
        STUDY_MANIFEST_COMPARATOR_EXPECTED_FIELDS,
        "comparator.expected",
        structural_reasons,
    )
    fields = {
        "public_job_id": _required_manifest_value(
            section,
            "public_job_id",
            "comparator.public_job_id",
            structural_reasons,
        ),
        "manifest_sha256": _required_manifest_value(
            section,
            "manifest_sha256",
            "comparator.manifest_sha256",
            structural_reasons,
        ),
        "trial_data_sha256": _required_manifest_value(
            section,
            "trial_data_sha256",
            "comparator.trial_data_sha256",
            structural_reasons,
        ),
        "submission_repository": _required_manifest_value(
            submission,
            "repository",
            "comparator.submission.repository",
            structural_reasons,
        ),
        "submission_commit": _required_manifest_value(
            submission,
            "commit",
            "comparator.submission.commit",
            structural_reasons,
        ),
        "submission_path": _required_manifest_value(
            submission,
            "path",
            "comparator.submission.path",
            structural_reasons,
        ),
        "submission_sha256": _required_manifest_value(
            submission,
            "sha256",
            "comparator.submission.sha256",
            structural_reasons,
        ),
        "agent_name": _required_manifest_value(
            section,
            "agent_name",
            "comparator.agent_name",
            structural_reasons,
        ),
        "agent_version": _required_manifest_value(
            section,
            "agent_version",
            "comparator.agent_version",
            structural_reasons,
        ),
        "model": _required_manifest_value(
            section, "model", "comparator.model", structural_reasons
        ),
        "reasoning_effort": _required_manifest_value(
            section,
            "reasoning_effort",
            "comparator.reasoning_effort",
            structural_reasons,
        ),
        "rows": _required_manifest_value(
            expected, "rows", "comparator.expected.rows", structural_reasons
        ),
        "tasks": _required_manifest_value(
            expected, "tasks", "comparator.expected.tasks", structural_reasons
        ),
        "attempts_per_task": _required_manifest_value(
            expected,
            "attempts_per_task",
            "comparator.expected.attempts_per_task",
            structural_reasons,
        ),
        "reward_total": _required_manifest_value(
            expected,
            "reward_total",
            "comparator.expected.reward_total",
            structural_reasons,
        ),
        "token_spend_total": _required_manifest_value(
            expected,
            "token_spend_total",
            "comparator.expected.token_spend_total",
            structural_reasons,
        ),
    }
    canonical = {
        "public_job_id": COMPARATOR_PUBLIC_JOB_ID,
        "manifest_sha256": COMPARATOR_MANIFEST_SHA256,
        "trial_data_sha256": COMPARATOR_TRIAL_DATA_SHA256,
        "submission_repository": COMPARATOR_SUBMISSION_REPOSITORY,
        "submission_commit": COMPARATOR_SUBMISSION_COMMIT,
        "submission_path": COMPARATOR_SUBMISSION_PATH,
        "submission_sha256": COMPARATOR_SUBMISSION_SHA256,
        "agent_name": COMPARATOR_AGENT_NAME,
        "agent_version": COMPARATOR_AGENT_VERSION,
        "model": COMPARATOR_MODEL,
        "reasoning_effort": COMPARATOR_REASONING_EFFORT,
        "rows": COMPARATOR_EXPECTED_ROWS,
        "tasks": COMPARATOR_EXPECTED_TASKS,
        "attempts_per_task": COMPARATOR_EXPECTED_ATTEMPTS,
        "reward_total": COMPARATOR_EXPECTED_REWARD_TOTAL,
        "token_spend_total": COMPARATOR_EXPECTED_TOKEN_TOTAL,
    }
    for key, canonical_value in canonical.items():
        value = fields[key]
        if value is not _MISSING and value != canonical_value:
            structural_reasons.append(
                f"Study manifest comparator {key} is not the frozen canonical "
                f"value: expected {canonical_value!r}, observed {value!r}."
            )

    if rows is None:
        reasons.append(
            "Comparator trial data was not supplied to study validation; the "
            "frozen comparator cannot be verified."
        )
        return {"expected": canonical, "observed": {}}

    attempted = [row for row in rows if row.get("attempted")]
    counts = Counter(row.get("task") for row in attempted)
    accuracy = [_number(row.get("accuracy_value")) for row in attempted]
    tokens = [_nonnegative_int(row.get("token_spend")) for row in attempted]
    prompt_tokens = [_nonnegative_int(row.get("prompt_tokens")) for row in attempted]
    completion_tokens = [
        _nonnegative_int(row.get("completion_tokens")) for row in attempted
    ]
    trial_ids = [row.get("trial_id") for row in attempted]
    source_hashes = {
        row.get("comparator_source_sha256")
        for row in attempted
        if row.get("comparator_source_sha256") is not None
    }
    data_hashes = {
        row.get("comparator_trial_data_sha256")
        for row in attempted
        if row.get("comparator_trial_data_sha256") is not None
    }
    recomputed_data_sha256 = _comparator_trial_data_sha256(attempted)
    job_ids = {row.get("job_id") for row in attempted if row.get("job_id") is not None}
    task_identity_map, task_identity_reasons = _comparator_task_identity_map(attempted)
    reasons.extend(task_identity_reasons)
    observed = {
        "rows": len(attempted),
        "tasks": len(counts),
        "task_count_distribution": dict(sorted(Counter(counts.values()).items())),
        "reward_total": sum(value for value in accuracy if value is not None),
        "reward_coverage": (
            f"{sum(value is not None for value in accuracy)}/{len(attempted)}"
        ),
        "token_spend_total": sum(value for value in tokens if value is not None),
        "token_coverage": (
            f"{sum(value is not None for value in tokens)}/{len(attempted)}"
        ),
        "unique_trial_ids": len(
            {value for value in trial_ids if isinstance(value, str) and value}
        ),
        "source_sha256": _display_values(source_hashes),
        "trial_data_sha256": _display_values(data_hashes),
        "recomputed_trial_data_sha256": recomputed_data_sha256,
        "job_ids": _display_values(job_ids),
        "task_identity_map": task_identity_map,
    }
    if len(attempted) != COMPARATOR_EXPECTED_ROWS:
        reasons.append(
            f"Comparator has {len(attempted)} attempted rows; expected "
            f"{COMPARATOR_EXPECTED_ROWS}."
        )
    if len(counts) != COMPARATOR_EXPECTED_TASKS or any(
        count != COMPARATOR_EXPECTED_ATTEMPTS for count in counts.values()
    ):
        reasons.append("Comparator does not contain exactly 89 tasks x 5 attempts.")
    if any(value is None for value in accuracy) or not math.isclose(
        sum(value for value in accuracy if value is not None),
        COMPARATOR_EXPECTED_REWARD_TOTAL,
        rel_tol=0,
        abs_tol=1e-12,
    ):
        reasons.append(
            "Comparator reward coverage/total does not match the frozen total "
            f"{COMPARATOR_EXPECTED_REWARD_TOTAL}."
        )
    if (
        any(value is None for value in tokens)
        or sum(value for value in tokens if value is not None)
        != COMPARATOR_EXPECTED_TOKEN_TOTAL
    ):
        reasons.append(
            "Comparator token coverage/total does not match the frozen total "
            f"{COMPARATOR_EXPECTED_TOKEN_TOTAL}."
        )
    if (
        any(value is None for value in prompt_tokens)
        or any(value is None for value in completion_tokens)
        or any(row.get("comparator_token_consistent") is not True for row in attempted)
        or any(
            total != prompt + completion
            for total, prompt, completion in zip(
                tokens,
                prompt_tokens,
                completion_tokens,
                strict=True,
            )
            if total is not None and prompt is not None and completion is not None
        )
    ):
        reasons.append(
            "Comparator token_spend is not exactly prompt/input plus "
            "completion/output tokens for every trial."
        )
    valid_trial_ids = [value for value in trial_ids if isinstance(value, str) and value]
    if len(valid_trial_ids) != len(attempted) or len(set(valid_trial_ids)) != len(
        attempted
    ):
        reasons.append(
            "Comparator must contain 445 non-empty, unique public trial IDs."
        )
    if source_hashes != {COMPARATOR_MANIFEST_SHA256}:
        reasons.append(
            "Comparator source file SHA-256 does not match the frozen public "
            f"manifest: observed {_display_values(source_hashes)!r}."
        )
    if (
        data_hashes != {COMPARATOR_TRIAL_DATA_SHA256}
        or recomputed_data_sha256 != COMPARATOR_TRIAL_DATA_SHA256
    ):
        reasons.append(
            "Comparator normalized trial-data SHA-256 does not match the frozen "
            f"content: observed {_display_values(data_hashes)!r}."
        )
    if job_ids != {COMPARATOR_PUBLIC_JOB_ID}:
        reasons.append(
            "Comparator rows do not all identify the frozen public Harbor job: "
            f"observed {_display_values(job_ids)!r}."
        )
    return {
        "expected": canonical,
        "observed": observed,
        "task_identity_map": task_identity_map,
    }


def _validate_run_ledger(
    manifest: dict[str, Any],
    payload: dict[str, Any] | None,
    *,
    run_ledger_path: Path | None,
    study_manifest_sha256: str | None,
    confirmatory_rows: Sequence[dict[str, Any]],
    calibration_rows: Sequence[dict[str, Any]] | None,
    excluded_rows: Sequence[dict[str, Any]] | None,
    comparator_rows: Sequence[dict[str, Any]] | None,
    structural_reasons: list[str],
    reasons: list[str],
) -> dict[str, Any]:
    """Validate immutable paid-run declarations, public attestations, and spend."""
    initial_reason_count = len(reasons)
    initial_structural_reason_count = len(structural_reasons)
    section = manifest.get("preregistration")
    study_id = _required_manifest_value(
        section, "study_id", "preregistration.study_id", structural_reasons
    )
    ledger_path_expected = _required_manifest_value(
        section,
        "run_ledger_path",
        "preregistration.run_ledger_path",
        structural_reasons,
    )
    readiness_commit = _required_manifest_value(
        section,
        "readiness_commit",
        "preregistration.readiness_commit",
        structural_reasons,
    )
    calibration_commit = _required_manifest_value(
        section,
        "calibration_commit",
        "preregistration.calibration_commit",
        structural_reasons,
    )
    if isinstance(section, dict) and set(section) != {
        "study_id",
        "run_ledger_path",
        "readiness_commit",
        "calibration_commit",
    }:
        structural_reasons.append(
            "Study manifest preregistration must contain exactly study_id, "
            "run_ledger_path, readiness_commit, and calibration_commit."
        )
    source_commit = (manifest.get("sut") or {}).get("source_commit")
    if not isinstance(study_id, str) or not study_id.strip():
        structural_reasons.append("preregistration.study_id must be non-empty.")
    if (
        not isinstance(ledger_path_expected, str)
        or not ledger_path_expected.strip()
        or Path(ledger_path_expected).is_absolute()
        or ".." in Path(ledger_path_expected).parts
    ):
        structural_reasons.append(
            "preregistration.run_ledger_path must be a non-empty repository-relative "
            "path without '..'."
        )
    if (
        not isinstance(readiness_commit, str)
        or re.fullmatch(r"[0-9a-f]{40}", readiness_commit) is None
    ):
        structural_reasons.append(
            "preregistration.readiness_commit must be a full lowercase Git commit."
        )
    if (
        not isinstance(calibration_commit, str)
        or re.fullmatch(r"[0-9a-f]{40}", calibration_commit) is None
        or calibration_commit != source_commit
    ):
        structural_reasons.append(
            "preregistration.calibration_commit must exactly equal the frozen "
            "sut.source_commit (the inclusive ancestry case)."
        )

    empty_result = {
        "supplied": payload is not None,
        "valid": False,
        "intent_sha256_by_job_id": {},
        "public_intent_expectation_by_job_id": {},
        "readiness_job_id": None,
        "public_timing_verification": (
            "declared-and-linkable; GitHub publication existence and commit ancestry "
            "require external verification"
        ),
        "public_timing_verified": False,
        "external_public_timing_audit_required": True,
    }
    if payload is None:
        reasons.append(
            "The append-only public run ledger was not supplied; paid-run intent, "
            "receipt binding, public timing, and provider reconciliation are unaudited."
        )
        return empty_result
    if set(payload) != RUN_LEDGER_TOP_LEVEL_FIELDS:
        reasons.append(
            "Run ledger top-level fields differ from the exact append-only v1 schema."
        )
    if payload.get("schema_version") != RUN_LEDGER_SCHEMA:
        reasons.append(f"Run ledger schema_version must be {RUN_LEDGER_SCHEMA!r}.")
    if payload.get("study_id") != study_id:
        reasons.append("Run ledger study_id does not match the study manifest.")
    if payload.get("ledger_path") != ledger_path_expected:
        reasons.append("Run ledger ledger_path does not match the study manifest.")
    historical_spend_disclosure = payload.get("historical_spend_disclosure")
    if (
        not isinstance(historical_spend_disclosure, dict)
        or set(historical_spend_disclosure) != HISTORICAL_SPEND_DISCLOSURE_FIELDS
        or historical_spend_disclosure != HISTORICAL_SPEND_DISCLOSURE
    ):
        reasons.append(
            "Run ledger historical_spend_disclosure must exactly retain the "
            "known old-shared-key lower bound, unknown cancellation spend, and "
            "$200 new authorized budget."
        )
    if (
        run_ledger_path is None
        or not isinstance(ledger_path_expected, str)
        or not run_ledger_path.resolve()
        .as_posix()
        .endswith("/" + ledger_path_expected.lstrip("/"))
    ):
        reasons.append(
            "The supplied run-ledger file path does not end with the frozen "
            "repository-relative ledger path."
        )
    if _normalized_sha256(study_manifest_sha256) is None:
        reasons.append(
            "The exact study-manifest file SHA-256 was not supplied to run-ledger "
            "validation."
        )

    preregistrations = payload.get("preregistrations")
    intents = payload.get("intents")
    publications = payload.get("publications")
    outcomes = payload.get("outcomes")
    arrays = {
        "preregistrations": preregistrations,
        "intents": intents,
        "publications": publications,
        "outcomes": outcomes,
    }
    for label, value in arrays.items():
        if not isinstance(value, list) or not all(
            isinstance(item, dict) for item in value
        ):
            reasons.append(f"Run ledger {label} must be an array of objects.")
            arrays[label] = []
    preregistrations = arrays["preregistrations"]
    intents = arrays["intents"]
    publications = arrays["publications"]
    outcomes = arrays["outcomes"]

    all_records = [*preregistrations, *intents, *publications, *outcomes]
    sequences = [record.get("sequence") for record in all_records]
    if any(
        not isinstance(value, int) or isinstance(value, bool) or value <= 0
        for value in sequences
    ) or len(set(sequences)) != len(sequences):
        reasons.append(
            "Every append-only run-ledger record must have one unique positive "
            "integer sequence."
        )
    for label, records in arrays.items():
        local = [record.get("sequence") for record in records]
        if all(isinstance(value, int) for value in local) and local != sorted(local):
            reasons.append(f"Run ledger {label} are not in append sequence order.")

    prereg_by_kind: dict[str, dict[str, Any]] = {}
    for record in preregistrations:
        if set(record) != PREREGISTRATION_FIELDS:
            reasons.append(
                "A preregistration record differs from the exact immutable schema."
            )
        kind = record.get("kind")
        if kind in prereg_by_kind or kind not in {
            "readiness",
            "calibration",
            "confirmatory_freeze",
        }:
            reasons.append("Run ledger preregistration kinds must occur exactly once.")
        elif isinstance(kind, str):
            prereg_by_kind[kind] = record
        if _aware_timestamp(record.get("declared_at")) is None:
            reasons.append(
                f"Preregistration {kind!r} has no timezone-aware declared_at."
            )
    if set(prereg_by_kind) != {
        "readiness",
        "calibration",
        "confirmatory_freeze",
    }:
        reasons.append(
            "Run ledger must contain exactly readiness, calibration, and "
            "confirmatory_freeze preregistration records."
        )
    readiness_prereg = prereg_by_kind.get("readiness", {})
    calibration_prereg = prereg_by_kind.get("calibration", {})
    freeze_prereg = prereg_by_kind.get("confirmatory_freeze", {})
    if (
        readiness_prereg.get("commit") != readiness_commit
        or readiness_prereg.get("study_manifest_sha256") is not None
    ):
        reasons.append(
            "Readiness preregistration must use readiness_commit and an explicit "
            "null study_manifest_sha256."
        )
    if (
        calibration_prereg.get("commit") != calibration_commit
        or calibration_prereg.get("study_manifest_sha256") is not None
    ):
        reasons.append(
            "Calibration preregistration must use calibration_commit and an explicit "
            "null study_manifest_sha256."
        )
    freeze_commit = freeze_prereg.get("commit")
    if (
        not isinstance(freeze_commit, str)
        or re.fullmatch(r"[0-9a-f]{40}", freeze_commit) is None
        or freeze_commit == calibration_commit
    ):
        reasons.append(
            "Confirmatory freeze must use a valid commit distinct from calibration."
        )
    if freeze_prereg.get("study_manifest_sha256") != study_manifest_sha256:
        reasons.append(
            "Confirmatory freeze does not bind the exact supplied study-manifest "
            "file SHA-256."
        )

    publications_by_subject: dict[tuple[Any, Any], dict[str, Any]] = {}
    for record in publications:
        if set(record) != PUBLICATION_FIELDS:
            reasons.append("A publication record differs from the exact schema.")
        key = (record.get("subject_type"), record.get("subject_id"))
        if key in publications_by_subject:
            reasons.append(f"Duplicate run-ledger publication subject {key!r}.")
        publications_by_subject[key] = record
        if not _valid_commit_publication(record):
            reasons.append(
                f"Publication {key!r} is not an exact public GitHub commit URL."
            )
        if _aware_timestamp(record.get("published_at")) is None:
            reasons.append(f"Publication {key!r} has no timezone-aware published_at.")
    expected_prereg_publications = {
        ("preregistration", "readiness"),
        ("preregistration", "calibration"),
        ("preregistration", "confirmatory_freeze"),
    }
    if not expected_prereg_publications.issubset(publications_by_subject):
        reasons.append(
            "All three preregistration records require separate public publication "
            "attestations."
        )
    for kind, prereg in prereg_by_kind.items():
        publication = publications_by_subject.get(("preregistration", kind))
        if publication is None:
            continue
        if publication.get("ledger_commit") == prereg.get("commit"):
            reasons.append(
                f"{kind} publication ledger snapshot must be later than and "
                "distinct from its subject commit."
            )
        declared = _aware_timestamp(prereg.get("declared_at"))
        published = _aware_timestamp(publication.get("published_at"))
        if declared is not None and published is not None and published < declared:
            reasons.append(f"{kind} publication predates its immutable declaration.")
        if (
            isinstance(publication.get("sequence"), int)
            and isinstance(prereg.get("sequence"), int)
            and publication["sequence"] <= prereg["sequence"]
        ):
            reasons.append(f"{kind} publication sequence does not follow declaration.")

    intent_by_sha: dict[str, dict[str, Any]] = {}
    wrapper_by_sha: dict[str, dict[str, Any]] = {}
    intent_ids: set[str] = set()
    for wrapper in intents:
        if set(wrapper) != INTENT_WRAPPER_FIELDS:
            reasons.append("An intent wrapper differs from the exact immutable schema.")
        intent = wrapper.get("intent")
        if not isinstance(intent, dict) or set(intent) != INTENT_FIELDS:
            reasons.append("An intent payload differs from the exact immutable schema.")
            continue
        for key, fields in (
            ("dataset", INTENT_DATASET_FIELDS),
            ("artifacts", INTENT_ARTIFACT_FIELDS),
            ("execution", INTENT_EXECUTION_FIELDS),
            ("provider_key", INTENT_PROVIDER_KEY_FIELDS),
        ):
            if not isinstance(intent.get(key), dict) or set(intent[key]) != fields:
                reasons.append(
                    f"Intent {intent.get('intent_id')!r} {key} schema drifts."
                )
        digest = wrapper.get("intent_sha256")
        recomputed = _canonical_payload_sha256(intent)
        if digest != recomputed or _normalized_sha256(digest) is None:
            reasons.append(
                f"Intent {intent.get('intent_id')!r} SHA-256 does not match its exact "
                "canonical immutable payload."
            )
            continue
        if digest in intent_by_sha:
            reasons.append(f"Duplicate intent SHA-256 {digest}.")
        intent_by_sha[digest] = intent
        wrapper_by_sha[digest] = wrapper
        intent_id = intent.get("intent_id")
        if not isinstance(intent_id, str) or not intent_id or intent_id in intent_ids:
            reasons.append("Intent IDs must be unique non-empty strings.")
        else:
            intent_ids.add(intent_id)
        if _aware_timestamp(intent.get("declared_at")) is None:
            reasons.append(f"Intent {intent_id!r} has no timezone-aware declared_at.")

    outcome_by_sha: dict[str, dict[str, Any]] = {}
    job_ids: set[str] = set()
    for outcome in outcomes:
        if set(outcome) != OUTCOME_FIELDS:
            reasons.append("An outcome record differs from the exact schema.")
        digest = outcome.get("intent_sha256")
        if digest not in intent_by_sha or digest in outcome_by_sha:
            reasons.append("Every outcome must reference one unique declared intent.")
        elif isinstance(digest, str):
            outcome_by_sha[digest] = outcome
        job_id = outcome.get("job_id")
        if not isinstance(job_id, str) or not job_id or job_id in job_ids:
            reasons.append("Outcome job IDs must be unique non-empty strings.")
        else:
            job_ids.add(job_id)
    if set(outcome_by_sha) != set(intent_by_sha):
        reasons.append("Every intent must have exactly one immutable outcome record.")

    stages: dict[str, list[str]] = defaultdict(list)
    for digest, intent in intent_by_sha.items():
        stage = intent.get("stage")
        if isinstance(stage, str):
            stages[stage].append(digest)
    historical_digests = stages.get("historical_excluded", [])
    for stage in ("readiness", "calibration", "confirmatory"):
        if len(stages.get(stage, [])) != 1:
            reasons.append(f"Run ledger must contain exactly one {stage} intent.")
    if len(historical_digests) != len(REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS):
        reasons.append(
            "Run ledger must contain exactly one historical intent per known "
            "pre-freeze excluded job."
        )
    unknown_stages = set(stages) - {
        "historical_excluded",
        "readiness",
        "calibration",
        "confirmatory",
    }
    if unknown_stages:
        reasons.append(
            f"Run ledger contains unregistered intent stages: {unknown_stages!r}."
        )

    observed_rows = [
        *confirmatory_rows,
        *(calibration_rows or []),
        *(excluded_rows or []),
    ]
    rows_by_job_id: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in observed_rows:
        if isinstance(row.get("job_id"), str) and row.get("job_id"):
            rows_by_job_id[row["job_id"]].append(row)
    if set(rows_by_job_id) != job_ids:
        reasons.append(
            "Observed Harbor job IDs do not exactly equal run-ledger outcomes; "
            "unregistered, missing, or stitched spend is present."
        )

    comparator_identity, _ = _comparator_task_identity_map(comparator_rows or [])
    expected_calibration_identity = {
        task: {
            "task_ref": CALIBRATION_TASK_REFS[task],
            "task_checksum": CALIBRATION_TASK_CHECKSUMS[task],
        }
        for task in CALIBRATION_TASKS
    }
    sut = manifest.get("sut") if isinstance(manifest.get("sut"), dict) else {}
    harbor = manifest.get("harbor") if isinstance(manifest.get("harbor"), dict) else {}
    confirmatory = (
        manifest.get("confirmatory")
        if isinstance(manifest.get("confirmatory"), dict)
        else {}
    )
    calibration = (
        manifest.get("calibration")
        if isinstance(manifest.get("calibration"), dict)
        else {}
    )
    expected_artifacts = {
        "binary_sha256": sut.get("binary_sha256"),
        "source_commit": sut.get("source_commit"),
        "agent_version": sut.get("agent_version"),
        "adapter_version": sut.get("adapter_version"),
        "adapter_sha256": sut.get("adapter_sha256"),
        "analysis_sha256": (manifest.get("analysis") or {}).get("sha256"),
        "public_timing_sha256": (manifest.get("analysis") or {}).get(
            "public_timing_sha256"
        ),
        "harbor_version": harbor.get("version"),
        "harbor_sha256": harbor.get("sha256"),
    }
    expected_execution = {
        "base_url": sut.get("base_url"),
        "provider_route_policy": sut.get("provider_route_policy"),
        "disable_reflection": sut.get("disable_reflection"),
    }

    intent_sha_by_job_id: dict[str, str] = {}
    public_intent_expectation_by_job_id: dict[str, dict[str, Any]] = {}
    readiness_job_id: str | None = None
    paid_records: list[tuple[int, str, dict[str, Any], dict[str, Any]]] = []
    historical_job_ids: set[str] = set()
    for digest, intent in intent_by_sha.items():
        outcome = outcome_by_sha.get(digest)
        if outcome is None:
            continue
        stage = intent.get("stage")
        historical = stage == "historical_excluded"
        if intent.get("historical") is not historical:
            reasons.append(f"Intent {digest} historical flag disagrees with stage.")
        job_id = outcome.get("job_id")
        job_rows = rows_by_job_id.get(job_id, []) if isinstance(job_id, str) else []
        if historical:
            historical_job_ids.add(job_id)
            nullable_fields = {
                "job_name",
                "requested_trials",
                "attempts_per_task",
                "n_concurrent_trials",
                "retry_max_retries",
                "per_trial_budget_usd",
                "preregistration_commit",
            }
            if any(intent.get(name) is not None for name in nullable_fields):
                reasons.append(
                    "Historical intents must preserve explicit null run fields."
                )
            if intent.get("models") != []:
                reasons.append(
                    "Historical intents must preserve models as an empty array."
                )
            for key in ("dataset", "artifacts", "execution", "provider_key"):
                value = intent.get(key)
                if isinstance(value, dict) and any(
                    item is not None for item in value.values()
                ):
                    reasons.append(f"Historical intent {key} fields must all be null.")
            if outcome.get("status") != "historical_excluded":
                reasons.append("Historical outcome status must be historical_excluded.")
            historical_nulls = OUTCOME_FIELDS - {
                "sequence",
                "intent_sha256",
                "job_id",
                "status",
                "reconciliation_status",
                "recorded_at",
            }
            if any(outcome.get(name) is not None for name in historical_nulls):
                reasons.append(
                    "Historical outcomes must preserve unavailable fields as null."
                )
            if outcome.get("reconciliation_status") != "unavailable":
                reasons.append(
                    "Historical outcome reconciliation_status must be unavailable."
                )
            if _aware_timestamp(outcome.get("recorded_at")) is None:
                reasons.append("Historical outcome recorded_at must be timezone-aware.")
            continue

        if isinstance(job_id, str):
            intent_sha_by_job_id[job_id] = digest
        evidence = _job_evidence(job_rows)
        evidence_row = job_rows[0] if job_rows else {}
        reasons.extend(evidence["temporal_reasons"])
        if stage != "readiness":
            reasons.extend(evidence["task_identity_reasons"])
        if not job_rows or not evidence["attempted"]:
            reasons.append(f"Paid {stage} outcome has no attempted Harbor evidence.")
        expected_name: Any
        expected_models: list[str]
        expected_attempts: int
        expected_concurrency: int
        expected_identity: dict[str, dict[str, str]] | None
        expected_dataset_name: str
        expected_dataset_ref: str | None
        expected_status: str
        expected_prereg_commit: Any
        if stage == "readiness":
            expected_name = READINESS_JOB_NAME
            readiness_models = (
                intent.get("models") if isinstance(intent.get("models"), list) else []
            )
            expected_models = [CALIBRATION_MODEL_ORDER[0]]
            if expected_models != readiness_models:
                reasons.append(
                    "Readiness intent must use exactly the frozen DeepSeek calibration "
                    "configuration."
                )
            expected_attempts = 1
            expected_concurrency = 1
            expected_identity = {
                READINESS_TASK: {
                    "task_ref": READINESS_TASK_REF,
                    "task_checksum": READINESS_TASK_SHA256,
                }
            }
            expected_dataset_name = READINESS_JOB_NAME
            expected_dataset_ref = READINESS_TASK_REF
            expected_status = "excluded"
            expected_prereg_commit = readiness_commit
            readiness_job_id = job_id if isinstance(job_id, str) else None
        elif stage == "calibration":
            expected_name = CALIBRATION_JOB_NAME
            expected_models = list(CALIBRATION_MODEL_ORDER)
            expected_attempts = CALIBRATION_ATTEMPTS_PER_MODEL_TASK
            expected_concurrency = CALIBRATION_N_CONCURRENT_TRIALS
            expected_identity = expected_calibration_identity
            expected_dataset_name = CANONICAL_DATASET_NAME
            expected_dataset_ref = CANONICAL_DATASET_REF
            expected_status = "complete"
            expected_prereg_commit = calibration_commit
            if job_id != calibration.get("job_id"):
                reasons.append(
                    "Calibration intent outcome job ID differs from manifest."
                )
        else:
            expected_name = confirmatory.get("job_name")
            expected_models = (
                [sut.get("model")] if isinstance(sut.get("model"), str) else []
            )
            expected_attempts = DEFAULT_TRIALS_PER_TASK
            expected_concurrency = CONFIRMATORY_N_CONCURRENT_TRIALS
            expected_identity = comparator_identity
            expected_dataset_name = CANONICAL_DATASET_NAME
            expected_dataset_ref = CANONICAL_DATASET_REF
            expected_status = "complete"
            expected_prereg_commit = freeze_commit
        if intent.get("job_name") != expected_name:
            reasons.append(f"Paid {stage} intent job_name is not frozen exactly.")
        observed_names = {row.get("job_name") for row in job_rows}
        if observed_names != {expected_name}:
            reasons.append(
                f"Paid {stage} Harbor rows do not share the intent job_name."
            )
        if intent.get("models") != expected_models or not expected_models:
            reasons.append(f"Paid {stage} intent models are not the registered roster.")
        if stage == "readiness":
            if (
                len(job_rows) != 1
                or job_rows[0].get("task") != READINESS_TASK
                or job_rows[0].get("task_name") != READINESS_TASK_NAME
                or job_rows[0].get("task_ref") is not None
                or _normalized_sha256(job_rows[0].get("task_checksum"))
                != READINESS_TASK_SHA256
            ):
                reasons.append(
                    "Readiness must be exactly the one frozen local "
                    "stella/synthetic-adapter-sentinel path/null-ref/checksum."
                )
            observed_models = {
                row.get("model")
                for row in job_rows
                if isinstance(row.get("model"), str)
            }
            if observed_models != set(expected_models):
                reasons.append("Readiness intent models differ from Harbor evidence.")
        requested_trials = len(job_rows)
        if intent.get("requested_trials") != requested_trials:
            reasons.append(
                f"Paid {stage} intent requested_trials differs from evidence."
            )
        if intent.get("attempts_per_task") != expected_attempts:
            reasons.append(f"Paid {stage} intent attempts_per_task is not canonical.")
        if intent.get("n_concurrent_trials") != expected_concurrency:
            reasons.append(f"Paid {stage} intent concurrency is not canonical.")
        if intent.get("retry_max_retries") != 0:
            reasons.append(f"Paid {stage} intent must freeze retry_max_retries=0.")
        if not math.isclose(
            _nonnegative_float(intent.get("per_trial_budget_usd")) or -1.0,
            _nonnegative_float(sut.get("budget_usd")) or -2.0,
            rel_tol=0,
            abs_tol=1e-12,
        ):
            reasons.append(f"Paid {stage} intent budget differs from sut.budget_usd.")
        if intent.get("preregistration_commit") != expected_prereg_commit:
            reasons.append(
                f"Paid {stage} intent binds the wrong preregistration commit."
            )
        dataset = (
            intent.get("dataset") if isinstance(intent.get("dataset"), dict) else {}
        )
        expected_task_digest = (
            _task_set_sha256(expected_identity) if expected_identity else None
        )
        if dataset.get("name") != expected_dataset_name:
            reasons.append(f"Paid {stage} intent dataset name is not canonical.")
        if stage == "readiness":
            if dataset.get("ref") != expected_dataset_ref:
                reasons.append("Readiness dataset ref is not the frozen sentinel ref.")
        elif dataset.get("ref") != expected_dataset_ref:
            reasons.append(f"Paid {stage} intent dataset ref is not canonical.")
        if dataset.get("task_count") != len(expected_identity or {}):
            reasons.append(f"Paid {stage} intent task_count differs from evidence.")
        if dataset.get("task_set_sha256") != expected_task_digest:
            reasons.append(
                f"Paid {stage} intent task-set digest differs from evidence."
            )
        artifacts = (
            intent.get("artifacts") if isinstance(intent.get("artifacts"), dict) else {}
        )
        stage_artifacts = expected_artifacts
        if stage == "readiness":
            stage_artifacts = {}
            for key, row_field in (
                ("binary_sha256", "binary_sha256"),
                ("source_commit", "source_commit"),
                ("agent_version", "agent_info_version"),
                ("adapter_version", "adapter_version"),
                ("adapter_sha256", "adapter_sha256"),
                ("harbor_version", "harbor_version"),
                ("harbor_sha256", "harbor_sha256"),
            ):
                values = {row.get(row_field) for row in job_rows}
                stage_artifacts[key] = next(iter(values)) if len(values) == 1 else None
            # Analysis happens after immutable artifacts are collected, so Harbor
            # rows cannot circularly attest the analyzer. Bind the intent to the
            # executing analyzer digest already frozen in the public manifest.
            stage_artifacts["analysis_sha256"] = expected_artifacts["analysis_sha256"]
            stage_artifacts["public_timing_sha256"] = expected_artifacts[
                "public_timing_sha256"
            ]
            readiness_hash_fields = (
                "binary_sha256",
                "adapter_sha256",
                "analysis_sha256",
                "public_timing_sha256",
                "harbor_sha256",
            )
            if any(
                _normalized_sha256(stage_artifacts.get(key)) is None
                for key in readiness_hash_fields
            ) or not (
                isinstance(stage_artifacts.get("source_commit"), str)
                and re.fullmatch(r"[0-9a-f]{40}", stage_artifacts["source_commit"])
            ):
                reasons.append(
                    "Readiness artifact evidence does not contain well-formed source "
                    "and binary/adapter/analysis/Harbor identities."
                )
            if stage_artifacts.get("source_commit") != readiness_commit:
                reasons.append(
                    "Readiness artifact source commit differs from readiness_commit."
                )
            if any(
                not isinstance(stage_artifacts.get(key), str)
                or not stage_artifacts[key]
                for key in ("agent_version", "adapter_version", "harbor_version")
            ):
                reasons.append("Readiness artifact version evidence is incomplete.")
        for key, expected in stage_artifacts.items():
            if artifacts.get(key) != expected:
                reasons.append(f"Paid {stage} intent artifact {key} is not frozen.")
        if stage == "readiness":
            expected_postures: dict[str, str] = {}
            for model in expected_models:
                values = {
                    _normalized_sha256(row.get("engine_posture_sha256"))
                    for row in job_rows
                    if row.get("model") == model
                }
                if len(values) == 1 and None not in values:
                    posture_hash = next(iter(values))
                    if posture_hash is not None:
                        expected_postures[model] = posture_hash
        else:
            expected_postures = {
                model: canonical_engine_posture(model)[2] for model in expected_models
            }
        if artifacts.get("engine_posture_sha256_by_model") != expected_postures:
            reasons.append(f"Paid {stage} intent engine posture hashes drift.")
        if intent.get("execution") != expected_execution:
            reasons.append(f"Paid {stage} intent execution route/reflection drifts.")

        publication = publications_by_subject.get(("intent", digest))
        if publication is None:
            reasons.append(f"Paid {stage} intent lacks a separate public publication.")
        else:
            if isinstance(job_id, str):
                artifacts = (
                    intent.get("artifacts")
                    if isinstance(intent.get("artifacts"), dict)
                    else {}
                )
                execution = (
                    intent.get("execution")
                    if isinstance(intent.get("execution"), dict)
                    else {}
                )
                provider = (
                    intent.get("provider_key")
                    if isinstance(intent.get("provider_key"), dict)
                    else {}
                )
                prior_stage = {
                    "readiness": None,
                    "calibration": "readiness",
                    "confirmatory": "calibration",
                }.get(stage)
                prior_stage_outcome = None
                prior_digests = stages.get(prior_stage, []) if prior_stage else []
                if len(prior_digests) == 1:
                    prior_digest = prior_digests[0]
                    prior_outcome = outcome_by_sha.get(prior_digest)
                    if isinstance(prior_outcome, dict):
                        prior_stage_outcome = {
                            "stage": prior_stage,
                            "intent_sha256": prior_digest,
                            "status": prior_outcome.get("status"),
                            "completed_at": prior_outcome.get("completed_at"),
                            "recorded_at": prior_outcome.get("recorded_at"),
                        }
                public_intent_expectation_by_job_id[job_id] = {
                    "intent_sha256": digest,
                    "kind": stage,
                    "subject_commit": intent.get("preregistration_commit"),
                    "ledger_commit": publication.get("ledger_commit"),
                    "runtime_identity": {
                        **{
                            field: artifacts.get(field)
                            for field in INTENT_ARTIFACT_FIELDS
                        },
                        **{
                            field: execution.get(field)
                            for field in INTENT_EXECUTION_FIELDS
                        },
                        "provider_key_fingerprint_sha256": provider.get(
                            "fingerprint_sha256"
                        ),
                    },
                    "provider_key": dict(provider),
                    "prior_stage_outcome": prior_stage_outcome,
                    "projected_spend_usd": (
                        _nonnegative_float(intent.get("requested_trials"))
                        * _nonnegative_float(intent.get("per_trial_budget_usd"))
                        if _nonnegative_float(intent.get("requested_trials"))
                        is not None
                        and _nonnegative_float(intent.get("per_trial_budget_usd"))
                        is not None
                        else None
                    ),
                }
            if publication.get("ledger_commit") == intent.get("preregistration_commit"):
                reasons.append(
                    f"Paid {stage} publication ledger snapshot must be distinct "
                    "from its subject commit."
                )
            intent_declared = _aware_timestamp(intent.get("declared_at"))
            published = _aware_timestamp(publication.get("published_at"))
            started = _harbor_timestamp(evidence["started_at"], evidence_row)
            if (
                intent_declared is not None
                and published is not None
                and published < intent_declared
            ):
                reasons.append(f"Paid {stage} intent publication predates declaration.")
            if (
                published is not None
                and started is not None
                and published + timedelta(seconds=2) > started
            ):
                reasons.append(f"Paid {stage} intent was not public before execution.")
            wrapper = wrapper_by_sha[digest]
            if (
                isinstance(publication.get("sequence"), int)
                and isinstance(wrapper.get("sequence"), int)
                and publication["sequence"] <= wrapper["sequence"]
            ):
                reasons.append(f"Paid {stage} publication does not follow intent.")

        prereg_publication = publications_by_subject.get(
            (
                "preregistration",
                ("confirmatory_freeze" if stage == "confirmatory" else stage),
            )
        )
        prereg_published = (
            _aware_timestamp(prereg_publication.get("published_at"))
            if prereg_publication
            else None
        )
        intent_declared = _aware_timestamp(intent.get("declared_at"))
        if (
            prereg_published is not None
            and intent_declared is not None
            and prereg_published >= intent_declared
        ):
            reasons.append(f"Paid {stage} intent predates its public preregistration.")
        if (
            prereg_publication is not None
            and isinstance(prereg_publication.get("sequence"), int)
            and isinstance(wrapper_by_sha[digest].get("sequence"), int)
            and prereg_publication["sequence"] >= wrapper_by_sha[digest]["sequence"]
        ):
            reasons.append(
                f"Paid {stage} intent sequence does not follow prereg publication."
            )
        started = _harbor_timestamp(evidence["started_at"], evidence_row)
        if (
            prereg_published is not None
            and started is not None
            and prereg_published + timedelta(seconds=2) > started
        ):
            reasons.append(
                f"Paid {stage} execution began before preregistration was public."
            )

        if outcome.get("status") != expected_status:
            reasons.append(f"Paid {stage} outcome status must be {expected_status!r}.")
        if _aware_timestamp(outcome.get("started_at")) != _harbor_timestamp(
            evidence["started_at"], evidence_row
        ) or _aware_timestamp(outcome.get("completed_at")) != _harbor_timestamp(
            evidence["completed_at"], evidence_row
        ):
            reasons.append(
                f"Paid {stage} outcome timestamps differ from Harbor evidence."
            )
        if (
            evidence["artifact_hash_count"] != 1
            or outcome.get("artifact_tree_sha256") != evidence["artifact_tree_sha256"]
        ):
            reasons.append(f"Paid {stage} outcome artifact-tree digest differs.")
        telemetry_cost = evidence["cost_sum"]
        if telemetry_cost is None or not math.isclose(
            _nonnegative_float(outcome.get("telemetry_cost_sum_usd")) or -1.0,
            telemetry_cost,
            rel_tol=0,
            abs_tol=1e-9,
        ):
            reasons.append(
                f"Paid {stage} outcome telemetry cost is incomplete or drifts."
            )
        before = _nonnegative_float(outcome.get("provider_usage_before_usd"))
        after = _nonnegative_float(outcome.get("provider_usage_after_usd"))
        delta = _nonnegative_float(outcome.get("provider_usage_delta_usd"))
        tolerance = _nonnegative_float(outcome.get("reconciliation_tolerance_usd"))
        provider = (
            intent.get("provider_key")
            if isinstance(intent.get("provider_key"), dict)
            else {}
        )
        if before is None or after is None or delta is None or after < before:
            reasons.append(f"Paid {stage} provider usage snapshots are invalid.")
        elif not math.isclose(after - before, delta, rel_tol=0, abs_tol=1e-9):
            reasons.append(f"Paid {stage} provider delta is not final minus initial.")
        if provider.get("usage_before_usd") != before:
            reasons.append(
                f"Paid {stage} intent pre-run dedicated-key cumulative-usage "
                "snapshot differs."
            )
        snapshot_at = _aware_timestamp(provider.get("snapshot_at"))
        intent_declared = _aware_timestamp(intent.get("declared_at"))
        intent_published = (
            _aware_timestamp(publication.get("published_at")) if publication else None
        )
        job_started = _harbor_timestamp(evidence["started_at"], evidence_row)
        if (
            snapshot_at is None
            or intent_declared is None
            or intent_published is None
            or job_started is None
            or snapshot_at > intent_declared
            or snapshot_at > intent_published
            or snapshot_at >= job_started
        ):
            reasons.append(
                f"Paid {stage} dedicated-key usage snapshot is not demonstrably "
                "pre-run."
            )
        if (
            tolerance is None
            or tolerance > MAX_RECONCILIATION_TOLERANCE_USD
            or delta is None
            or telemetry_cost is None
            or abs(delta - telemetry_cost) > tolerance
            or outcome.get("reconciliation_status") != "reconciled"
        ):
            reasons.append(f"Paid {stage} provider/telemetry spend is not reconciled.")
        completed = _aware_timestamp(outcome.get("completed_at"))
        recorded = _aware_timestamp(outcome.get("recorded_at"))
        if completed is None or recorded is None or recorded < completed:
            reasons.append(f"Paid {stage} outcome recorded_at is invalid.")
        if (
            isinstance(outcome.get("sequence"), int)
            and publication is not None
            and isinstance(publication.get("sequence"), int)
            and outcome["sequence"] <= publication["sequence"]
        ):
            reasons.append(
                f"Paid {stage} outcome sequence does not follow publication."
            )
        if isinstance(wrapper_by_sha[digest].get("sequence"), int):
            paid_records.append(
                (wrapper_by_sha[digest]["sequence"], digest, intent, outcome)
            )

        if stage == "readiness" and job_rows:
            row = job_rows[0]
            public_intent_expectation = public_intent_expectation_by_job_id.get(
                row.get("job_id"), {}
            )
            reasons.extend(_readiness_harbor_reasons(row))
            reasons.extend(
                _trial_telemetry_reasons(
                    row,
                    allowed_call_models=CALIBRATION_CALL_MODELS[
                        CALIBRATION_MODEL_ORDER[0]
                    ],
                    label="Readiness trial",
                )
            )
            _, posture_json, posture_sha256 = canonical_engine_posture(
                CALIBRATION_MODEL_ORDER[0]
            )
            readiness_posture_expectations = {
                "engine_posture_version": CANONICAL_ENGINE_POSTURE_VERSION,
                "engine_posture_json": posture_json,
                "engine_posture_record_json": posture_json,
                "engine_posture_sha256": posture_sha256,
                "atif_engine_posture_version": CANONICAL_ENGINE_POSTURE_VERSION,
                "atif_engine_posture_json": posture_json,
                "atif_engine_posture_record_json": posture_json,
                "atif_engine_posture_sha256": posture_sha256,
                "atif_valid": True,
                "container_credential_absence_verified": True,
                "atif_container_credential_absence_verified": True,
            }
            for field, expected in readiness_posture_expectations.items():
                if row.get(field) != expected:
                    reasons.append(
                        f"Readiness trial {field} differs from the exact telemetry "
                        "and posture contract."
                    )
            reward = _number(row.get("reward"))
            accuracy = _number(row.get("accuracy_value"))
            if (
                row.get("status") != "completed"
                or row.get("exception_type") is not None
                or row.get("stream_terminal_event") != "complete"
                or _nonnegative_int(row.get("stella_return_code")) != 0
                or reward != 1.0
                or accuracy != 1.0
            ):
                reasons.append(
                    "Readiness sentinel must complete without an agent exception, "
                    "emit terminal complete/return code 0, and receive external "
                    "verifier reward and accuracy exactly 1.0 before calibration."
                )
            reasons.extend(
                _launch_receipt_reasons(
                    row,
                    expected_job_name=READINESS_JOB_NAME,
                    expected_models=expected_models,
                    expected_intent_sha256=digest,
                    expected_kind=stage,
                    expected_subject_commit=intent.get("preregistration_commit"),
                    expected_ledger_commit=(
                        publication.get("ledger_commit") if publication else None
                    ),
                    expected_runtime_identity=public_intent_expectation.get(
                        "runtime_identity"
                    ),
                    expected_provider_key=public_intent_expectation.get("provider_key"),
                    expected_prior_stage_outcome=public_intent_expectation.get(
                        "prior_stage_outcome"
                    ),
                    expected_projected_spend_usd=public_intent_expectation.get(
                        "projected_spend_usd"
                    ),
                    label="Readiness Harbor job",
                )
            )
            reasons.extend(
                _host_attestation_reasons(
                    row,
                    expected_job_name=READINESS_JOB_NAME,
                    expected_intent_sha256=digest,
                    expected_stage="readiness",
                    label="Readiness Harbor job",
                )
            )

    if historical_job_ids != set(REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS):
        reasons.append(
            "Historical run-ledger outcomes do not exactly equal the five known "
            "pre-freeze excluded job IDs."
        )
    excluded_job_ids = set(calibration.get("excluded_job_ids") or [])
    expected_excluded = set(REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS)
    if readiness_job_id is not None:
        expected_excluded.add(readiness_job_id)
    if excluded_job_ids != expected_excluded:
        reasons.append(
            "calibration.excluded_job_ids must exactly contain the five historical "
            "jobs plus the readiness job outcome ID."
        )

    paid_records.sort()
    if [intent.get("stage") for _, _, intent, _ in paid_records] != [
        "readiness",
        "calibration",
        "confirmatory",
    ]:
        reasons.append(
            "Paid intent sequence must be readiness, calibration, confirmatory."
        )
    key_identities: set[tuple[Any, Any, Any]] = set()
    total_delta = 0.0
    first_before: float | None = None
    previous_after: float | None = None
    previous_completed: datetime | None = None
    previous_recorded: datetime | None = None
    last_after: float | None = None
    for _, _, intent, outcome in paid_records:
        provider = (
            intent.get("provider_key")
            if isinstance(intent.get("provider_key"), dict)
            else {}
        )
        identity = (
            provider.get("fingerprint_sha256"),
            provider.get("label"),
            _nonnegative_float(provider.get("limit_usd")),
        )
        key_identities.add(identity)
        fingerprint, label, limit = identity
        if (
            _normalized_sha256(fingerprint) is None
            or not isinstance(label, str)
            or not label
            or limit is None
            or limit != DEDICATED_KEY_LIMIT_USD
            or _aware_timestamp(provider.get("snapshot_at")) is None
        ):
            reasons.append(
                "Paid intents lack one valid dedicated-key identity/limit snapshot."
            )
        before = _nonnegative_float(outcome.get("provider_usage_before_usd"))
        after = _nonnegative_float(outcome.get("provider_usage_after_usd"))
        delta = _nonnegative_float(outcome.get("provider_usage_delta_usd"))
        if first_before is None:
            first_before = before
        if (
            previous_after is not None
            and before is not None
            and not math.isclose(before, previous_after, rel_tol=0, abs_tol=1e-9)
        ):
            reasons.append(
                "Dedicated-key usage is discontinuous between registered paid jobs; "
                "unregistered spend is possible."
            )
        started = _aware_timestamp(outcome.get("started_at"))
        completed = _aware_timestamp(outcome.get("completed_at"))
        prior_stage_final = (
            max(previous_completed, previous_recorded)
            if previous_completed is not None and previous_recorded is not None
            else previous_completed or previous_recorded
        )
        if prior_stage_final is not None and (
            started is None or started <= prior_stage_final
        ):
            reasons.append(
                "Each prior paid stage must be fully graded and its outcome recorded "
                "before the next stage begins."
            )
        if delta is not None:
            total_delta += delta
        previous_after = after
        last_after = after
        previous_completed = completed
        previous_recorded = _aware_timestamp(outcome.get("recorded_at"))
    if len(key_identities) != 1:
        reasons.append(
            "All paid intents must bind one unchanged dedicated provider key."
        )
    if (
        first_before is None
        or last_after is None
        or not math.isclose(
            last_after - first_before, total_delta, rel_tol=0, abs_tol=1e-9
        )
    ):
        reasons.append(
            "Dedicated-key final-minus-initial usage does not equal all registered "
            "per-job deltas."
        )
    elif key_identities:
        limit = next(iter(key_identities))[2]
        if isinstance(limit, (int, float)) and total_delta > float(limit) + 1e-9:
            reasons.append("Registered paid-run spend exceeds the dedicated-key limit.")

    expected_publication_subjects = expected_prereg_publications | {
        ("intent", digest)
        for stage in ("readiness", "calibration", "confirmatory")
        for digest in stages.get(stage, [])
    }
    if set(publications_by_subject) != expected_publication_subjects:
        reasons.append(
            "Publication records do not exactly equal three preregistration and three "
            "paid-intent attestations."
        )
    result = {
        **empty_result,
        "valid": (
            len(reasons) == initial_reason_count
            and len(structural_reasons) == initial_structural_reason_count
        ),
        "intent_sha256_by_job_id": intent_sha_by_job_id,
        "public_intent_expectation_by_job_id": (public_intent_expectation_by_job_id),
        "readiness_job_id": readiness_job_id,
        "observed": {
            "preregistrations": len(preregistrations),
            "intents": len(intents),
            "publications": len(publications),
            "outcomes": len(outcomes),
            "historical_job_ids": sorted(historical_job_ids),
            "paid_job_ids": sorted(intent_sha_by_job_id),
            "historical_spend_disclosure": historical_spend_disclosure,
            "dedicated_key_delta_usd": total_delta,
        },
    }
    return result


def _calibration_trial_data_sha256(rows: Sequence[dict[str, Any]]) -> str:
    fields = (
        "job_name",
        "job_id",
        "slot_id",
        "attempt_index",
        "requested",
        "instantiated",
        "attempted",
        "task",
        "task_ref",
        "task_checksum",
        "trial_id",
        "model",
        "reward",
        "accuracy_value",
        "prompt_tokens",
        "completion_tokens",
        "cache_tokens",
        "token_spend",
        "cost_usd",
        "agent_wall_seconds",
        "exception_type",
        "binary_sha256",
        "source_commit",
        "adapter_sha256",
        "engine_posture_version",
        "engine_posture_json",
        "engine_posture_sha256",
        "atif_engine_posture_version",
        "atif_engine_posture_json",
        "atif_engine_posture_sha256",
    )
    trials = [{field: row.get(field) for field in fields} for row in rows]
    encoded = json.dumps(
        {
            "schema": "stella-tb21-calibration-normalized-v4",
            "trials": trials,
        },
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _calibration_ledger_sha256(rows: Sequence[dict[str, Any]]) -> str:
    fields = (
        "job_id",
        "slot_id",
        "requested",
        "instantiated",
        "attempted",
        "task",
        "trial_id",
        "model",
        "reward",
        "accuracy_value",
        "token_spend",
        "cost_usd",
        "exception_type",
    )
    entries = [{field: row.get(field) for field in fields} for row in rows]
    entries.sort(
        key=lambda row: json.dumps(
            row,
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
        )
    )
    encoded = json.dumps(
        {
            "schema": "stella-tb21-excluded-calibration-ledger-v2",
            "rows": entries,
        },
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _validate_calibration(
    manifest: dict[str, Any],
    rows: Sequence[dict[str, Any]] | None,
    ledger_rows: Sequence[dict[str, Any]] | None,
    *,
    input_job_count: int | None,
    binary_sha256: Any,
    source_commit: Any,
    agent_version: Any,
    adapter_version: Any,
    adapter_sha256: Any,
    budget_usd: Any,
    disable_reflection: Any,
    base_url: Any,
    provider_route_policy: Any,
    harbor_version: Any,
    harbor_sha256: Any,
    intent_sha256_by_job_id: dict[str, str],
    public_intent_expectation_by_job_id: dict[str, dict[str, Any]],
    structural_reasons: list[str],
    reasons: list[str],
) -> dict[str, Any]:
    section = manifest.get("calibration")
    _require_exact_manifest_fields(
        section,
        STUDY_MANIFEST_CALIBRATION_FIELDS,
        "calibration",
        structural_reasons,
    )
    expected_fields = {
        "seed": _required_manifest_value(
            section, "seed", "calibration.seed", structural_reasons
        ),
        "tasks": _required_manifest_value(
            section, "tasks", "calibration.tasks", structural_reasons
        ),
        "model_order": _required_manifest_value(
            section,
            "model_order",
            "calibration.model_order",
            structural_reasons,
        ),
        "call_models_by_config": _required_manifest_value(
            section,
            "call_models_by_config",
            "calibration.call_models_by_config",
            structural_reasons,
        ),
        "engine_postures_by_config": _required_manifest_value(
            section,
            "engine_postures_by_config",
            "calibration.engine_postures_by_config",
            structural_reasons,
        ),
        "attempts_per_model_task": _required_manifest_value(
            section,
            "attempts_per_model_task",
            "calibration.attempts_per_model_task",
            structural_reasons,
        ),
        "n_concurrent_trials": _required_manifest_value(
            section,
            "n_concurrent_trials",
            "calibration.n_concurrent_trials",
            structural_reasons,
        ),
        "minimum_passes": _required_manifest_value(
            section,
            "minimum_passes",
            "calibration.minimum_passes",
            structural_reasons,
        ),
        "projection_trials": _required_manifest_value(
            section,
            "projection_trials",
            "calibration.projection_trials",
            structural_reasons,
        ),
        "projected_spend_limit_usd": _required_manifest_value(
            section,
            "projected_spend_limit_usd",
            "calibration.projected_spend_limit_usd",
            structural_reasons,
        ),
        "selected_model": _required_manifest_value(
            section,
            "selected_model",
            "calibration.selected_model",
            structural_reasons,
        ),
        "job_name": _required_manifest_value(
            section, "job_name", "calibration.job_name", structural_reasons
        ),
        "job_id": _required_manifest_value(
            section, "job_id", "calibration.job_id", structural_reasons
        ),
        "trial_data_sha256": _required_manifest_value(
            section,
            "trial_data_sha256",
            "calibration.trial_data_sha256",
            structural_reasons,
        ),
        "excluded_job_ids": _required_manifest_value(
            section,
            "excluded_job_ids",
            "calibration.excluded_job_ids",
            structural_reasons,
        ),
        "excluded_ledger_sha256": _required_manifest_value(
            section,
            "excluded_ledger_sha256",
            "calibration.excluded_ledger_sha256",
            structural_reasons,
        ),
    }
    engine_postures = expected_fields["engine_postures_by_config"]
    if engine_postures is not _MISSING:
        _require_exact_manifest_fields(
            engine_postures,
            frozenset(CALIBRATION_MODEL_ORDER),
            "calibration.engine_postures_by_config",
            structural_reasons,
        )
        if isinstance(engine_postures, dict):
            for model in CALIBRATION_MODEL_ORDER:
                if model not in engine_postures:
                    continue
                record = engine_postures[model]
                record_label = f"calibration.engine_postures_by_config[{model!r}]"
                _require_exact_manifest_fields(
                    record,
                    STUDY_MANIFEST_ENGINE_POSTURE_RECORD_FIELDS,
                    record_label,
                    structural_reasons,
                )
                if isinstance(record, dict) and "posture" in record:
                    _validate_engine_posture_manifest_schema(
                        record["posture"],
                        f"{record_label}.posture",
                        structural_reasons,
                    )
    canonical = {
        "seed": CALIBRATION_SEED,
        "tasks": list(CALIBRATION_TASKS),
        "model_order": list(CALIBRATION_MODEL_ORDER),
        "call_models_by_config": {
            model: list(call_models)
            for model, call_models in CALIBRATION_CALL_MODELS.items()
        },
        "engine_postures_by_config": {
            model: {
                "version": CANONICAL_ENGINE_POSTURE_VERSION,
                "posture": canonical_engine_posture(model)[0],
                "sha256": canonical_engine_posture(model)[2],
            }
            for model in CALIBRATION_MODEL_ORDER
        },
        "job_name": CALIBRATION_JOB_NAME,
        "attempts_per_model_task": CALIBRATION_ATTEMPTS_PER_MODEL_TASK,
        "n_concurrent_trials": CALIBRATION_N_CONCURRENT_TRIALS,
        "minimum_passes": CALIBRATION_MINIMUM_PASSES,
        "projection_trials": CALIBRATION_PROJECTION_TRIALS,
        "projected_spend_limit_usd": CALIBRATION_SPEND_LIMIT_USD,
    }
    for key, expected in canonical.items():
        value = expected_fields[key]
        if value is not _MISSING and value != expected:
            structural_reasons.append(
                f"Study manifest calibration.{key} is not preregistered: "
                f"expected {expected!r}, observed {value!r}."
            )
    job_id_manifest = expected_fields["job_id"]
    if job_id_manifest is not _MISSING and (
        not isinstance(job_id_manifest, str) or not job_id_manifest
    ):
        structural_reasons.append(
            "Study manifest calibration.job_id must be a non-empty string."
        )
    calibration_digest = expected_fields["trial_data_sha256"]
    if calibration_digest is not _MISSING and (
        not isinstance(calibration_digest, str)
        or re.fullmatch(r"[0-9a-fA-F]{64}", calibration_digest) is None
    ):
        structural_reasons.append(
            "Study manifest calibration.trial_data_sha256 must be a 64-character "
            "hex digest."
        )
    excluded_job_ids = expected_fields["excluded_job_ids"]
    if excluded_job_ids is not _MISSING and (
        not isinstance(excluded_job_ids, list)
        or not all(isinstance(value, str) and value for value in excluded_job_ids)
        or not set(REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS).issubset(excluded_job_ids)
    ):
        structural_reasons.append(
            "calibration.excluded_job_ids must include every known pre-freeze "
            "instrumentation/aborted job ID."
        )
    excluded_ledger_digest = expected_fields["excluded_ledger_sha256"]
    if excluded_ledger_digest is not _MISSING and (
        not isinstance(excluded_ledger_digest, str)
        or re.fullmatch(r"[0-9a-fA-F]{64}", excluded_ledger_digest) is None
    ):
        structural_reasons.append(
            "calibration.excluded_ledger_sha256 must be a 64-character hex digest."
        )

    if rows is None:
        reasons.append(
            "Calibration job data was not supplied; the preregistered model "
            "selection cannot be reproduced."
        )
        return {"expected": canonical, "observed": {}, "ranking": []}

    if ledger_rows is None:
        reasons.append(
            "Excluded calibration ledger jobs were not supplied; known aborted "
            "and instrumentation-failure runs cannot be audited."
        )
        ledger_rows = []
    observed_excluded_ids = list(
        dict.fromkeys(
            row.get("job_id")
            for row in ledger_rows
            if isinstance(row.get("job_id"), str)
        )
    )
    if isinstance(excluded_job_ids, list) and set(observed_excluded_ids) != set(
        excluded_job_ids
    ):
        reasons.append(
            "Excluded calibration ledger job IDs do not match the frozen manifest: "
            f"observed {observed_excluded_ids!r}."
        )
    actual_ledger_digest = _calibration_ledger_sha256(ledger_rows)
    if (
        isinstance(excluded_ledger_digest, str)
        and actual_ledger_digest != excluded_ledger_digest
    ):
        reasons.append(
            "Excluded calibration ledger SHA-256 does not match the frozen "
            f"manifest: observed {actual_ledger_digest}."
        )

    attempted = [row for row in rows if row.get("attempted")]
    expected_slots = Counter(
        (model, task, attempt)
        for model in CALIBRATION_MODEL_ORDER
        for task in CALIBRATION_TASKS
        for attempt in range(1, CALIBRATION_ATTEMPTS_PER_MODEL_TASK + 1)
    )
    observed_slots = Counter(
        (row.get("model"), row.get("task"), row.get("attempt_index"))
        for row in attempted
    )
    requested_count = sum(row.get("requested") is True for row in rows)
    instantiated_count = sum(row.get("instantiated") is True for row in rows)
    if (
        len(rows) != CALIBRATION_EXPECTED_TRIALS
        or requested_count != CALIBRATION_EXPECTED_TRIALS
        or instantiated_count != CALIBRATION_EXPECTED_TRIALS
        or len(attempted) != CALIBRATION_EXPECTED_TRIALS
        or observed_slots != expected_slots
    ):
        reasons.append(
            "Calibration must contain exactly one complete 60-slot job: three "
            "registered models x 10 frozen tasks x two attempts, all requested, "
            "instantiated, and attempted once, with no extras."
        )
    trial_ids = [row.get("trial_id") for row in attempted]
    valid_trial_ids = [value for value in trial_ids if isinstance(value, str) and value]
    if (
        len(valid_trial_ids) != CALIBRATION_EXPECTED_TRIALS
        or len(set(valid_trial_ids)) != CALIBRATION_EXPECTED_TRIALS
    ):
        reasons.append(
            "Calibration must contain 60 non-empty, unique Harbor trial IDs."
        )
    slot_ids = [row.get("slot_id") for row in rows]
    if (
        any(not isinstance(value, str) or not value for value in slot_ids)
        or len(set(slot_ids)) != CALIBRATION_EXPECTED_TRIALS
    ):
        reasons.append("Calibration requested slot IDs must be non-empty and unique.")

    sources = {
        row.get("source_input")
        for row in rows
        if isinstance(row.get("source_input"), str) and row.get("source_input")
    }
    physical_input_count = len(sources) if input_job_count is None else input_job_count
    actual_job_names = {
        row.get("job_name")
        for row in rows
        if isinstance(row.get("job_name"), str) and row.get("job_name")
    }
    actual_job_ids = {
        row.get("job_id")
        for row in rows
        if isinstance(row.get("job_id"), str) and row.get("job_id")
    }
    if physical_input_count != 1 or len(sources) != 1:
        reasons.append(
            "Calibration must come from exactly one supplied physical Harbor job "
            f"directory; received {physical_input_count}, observed row sources "
            f"{sorted(sources)!r}."
        )
    if actual_job_names != {CALIBRATION_JOB_NAME}:
        reasons.append(
            "Calibration Harbor job name is not the frozen preregistered name: "
            f"observed {sorted(actual_job_names)!r}."
        )
    expected_job_ids = {job_id_manifest} if isinstance(job_id_manifest, str) else set()
    if actual_job_ids != expected_job_ids:
        reasons.append(
            "Calibration Harbor job ID does not match calibration.job_id: "
            f"observed {sorted(actual_job_ids)!r}."
        )
    overlap = actual_job_ids.intersection(observed_excluded_ids)
    if overlap:
        reasons.append(
            "Selection calibration and excluded ledger reuse Harbor job IDs: "
            f"{sorted(overlap)!r}."
        )
    actual_digest = _calibration_trial_data_sha256(attempted)
    if isinstance(calibration_digest, str) and actual_digest != calibration_digest:
        reasons.append(
            "Calibration normalized trial-data SHA-256 does not match the frozen "
            f"manifest: observed {actual_digest}."
        )

    calibration_job_rows = [
        next(row for row in rows if row.get("source_input") == source)
        for source in sources
    ]
    for row in calibration_job_rows:
        source = row.get("source_input")
        job_expectations = {
            "job_dataset_count": 1,
            "job_dataset_name": CANONICAL_DATASET_NAME,
            "job_dataset_ref": CANONICAL_DATASET_REF,
            "job_task_count": len(CALIBRATION_TASKS),
            "job_n_attempts": CALIBRATION_ATTEMPTS_PER_MODEL_TASK,
            "job_agent_count": len(CALIBRATION_MODEL_ORDER),
            "job_n_concurrent_trials": CALIBRATION_N_CONCURRENT_TRIALS,
        }
        for field, expected in job_expectations.items():
            if row.get(field) != expected:
                reasons.append(
                    f"Calibration Harbor job {source} {field} is not canonical: "
                    f"{row.get(field)!r} != {expected!r}."
                )
        if _json_string_array(row.get("job_agent_models_json")) != list(
            CALIBRATION_MODEL_ORDER
        ):
            reasons.append(
                f"Calibration Harbor job {source} agent models do not match the "
                "registered three-model roster."
            )
        if _json_string_array(row.get("job_agent_import_paths_json")) != [
            CANONICAL_AGENT_IMPORT_PATH
        ] * len(CALIBRATION_MODEL_ORDER):
            reasons.append(
                f"Calibration Harbor job {source} does not use "
                f"{CANONICAL_AGENT_IMPORT_PATH!r} for all three agents."
            )
        if row.get("job_harbor_missing_fields"):
            reasons.append(
                f"Calibration Harbor job {source} omits canonical setting fields: "
                f"{row.get('job_harbor_missing_fields')}."
            )
        for name, canonical_value in CANONICAL_HARBOR_SETTINGS.items():
            actual = row.get(f"job_{name}")
            if not _matches_setting(actual, canonical_value):
                reasons.append(
                    f"Calibration Harbor job {source} noncanonically sets {name}: "
                    f"expected {canonical_value!r}, observed {actual!r}."
                )
        reasons.extend(
            _exact_harbor_job_reasons(
                row,
                expected_n_concurrent=CALIBRATION_N_CONCURRENT_TRIALS,
                label=f"Calibration Harbor job {source}",
            )
        )
        public_intent_expectation = public_intent_expectation_by_job_id.get(
            row.get("job_id"), {}
        )
        reasons.extend(
            _launch_receipt_reasons(
                row,
                expected_job_name=CALIBRATION_JOB_NAME,
                expected_models=CALIBRATION_MODEL_ORDER,
                expected_intent_sha256=intent_sha256_by_job_id.get(row.get("job_id")),
                expected_kind=public_intent_expectation.get("kind"),
                expected_subject_commit=public_intent_expectation.get("subject_commit"),
                expected_ledger_commit=public_intent_expectation.get("ledger_commit"),
                expected_runtime_identity=public_intent_expectation.get(
                    "runtime_identity"
                ),
                expected_provider_key=public_intent_expectation.get("provider_key"),
                expected_prior_stage_outcome=public_intent_expectation.get(
                    "prior_stage_outcome"
                ),
                expected_projected_spend_usd=public_intent_expectation.get(
                    "projected_spend_usd"
                ),
                label=f"Calibration Harbor job {source}",
            )
        )
        reasons.extend(
            _host_attestation_reasons(
                row,
                expected_job_name=CALIBRATION_JOB_NAME,
                expected_intent_sha256=intent_sha256_by_job_id.get(row.get("job_id")),
                expected_stage="calibration",
                label=f"Calibration Harbor job {source}",
            )
        )

    identity_expectations = {
        "binary_sha256": binary_sha256,
        "source_commit": source_commit,
        "agent_info_version": agent_version,
        "stella_agent_version": agent_version,
        "adapter_version": adapter_version,
        "adapter_sha256": adapter_sha256,
        "budget_usd": _nonnegative_float(budget_usd),
        "disable_reflection": disable_reflection,
        "base_url": base_url,
        "provider_route_policy": provider_route_policy,
        "harbor_version": harbor_version,
        "harbor_sha256": harbor_sha256,
    }
    for row in attempted:
        label = f"Calibration trial {row.get('trial_name') or row.get('slot_id')}"
        model = row.get("model")
        task = row.get("task")
        expected_task_ref = CALIBRATION_TASK_REFS.get(task)
        if row.get("task_ref") != expected_task_ref:
            reasons.append(
                f"{label} task ref differs from the frozen canonical Harbor "
                f"package ref: {row.get('task_ref')!r} != {expected_task_ref!r}."
            )
        expected_checksum = CALIBRATION_TASK_CHECKSUMS.get(task)
        if _normalized_sha256(row.get("task_checksum")) != expected_checksum:
            reasons.append(
                f"{label} task checksum differs from the frozen task-directory "
                f"SHA-256: {row.get('task_checksum')!r} != {expected_checksum!r}."
            )
        if row.get("trial_dataset_name") != CANONICAL_DATASET_NAME:
            reasons.append(f"{label} dataset source is not {CANONICAL_DATASET_NAME!r}.")
        if row.get("trial_harbor_missing_fields"):
            reasons.append(
                f"{label} omits canonical Harbor setting fields: "
                f"{row.get('trial_harbor_missing_fields')}."
            )
        for name, canonical_value in CANONICAL_HARBOR_SETTINGS.items():
            actual = row.get(f"trial_{name}")
            if not _matches_setting(actual, canonical_value):
                reasons.append(
                    f"{label} noncanonically sets {name}: expected "
                    f"{canonical_value!r}, observed {actual!r}."
                )
        reasons.extend(_exact_harbor_trial_reasons(row, label=label))
        reward = _number(row.get("reward"))
        accuracy = _number(row.get("accuracy_value"))
        if (
            reward is None
            or accuracy is None
            or not 0 <= float(reward) <= 1
            or not 0 <= float(accuracy) <= 1
            or float(reward) != float(accuracy)
        ):
            reasons.append(
                f"{label} must have matching verifier reward and accuracy in "
                f"[0, 1]; observed {reward!r} and {accuracy!r}."
            )
        call_models = (
            CALIBRATION_CALL_MODELS.get(model) if isinstance(model, str) else None
        )
        if call_models is not None:
            reasons.extend(
                _trial_telemetry_reasons(
                    row,
                    allowed_call_models=call_models,
                    label=label,
                )
            )
        else:
            reasons.append(f"{label} uses an unregistered configuration model.")
        if isinstance(model, str) and model in CALIBRATION_MODEL_ORDER:
            _, posture_json, posture_sha256 = canonical_engine_posture(model)
            posture_expectations = {
                "engine_posture_version": CANONICAL_ENGINE_POSTURE_VERSION,
                "engine_posture_json": posture_json,
                "engine_posture_record_json": posture_json,
                "engine_posture_sha256": posture_sha256,
                "atif_engine_posture_version": CANONICAL_ENGINE_POSTURE_VERSION,
                "atif_engine_posture_json": posture_json,
                "atif_engine_posture_record_json": posture_json,
                "atif_engine_posture_sha256": posture_sha256,
            }
            for field, expected in posture_expectations.items():
                actual = row.get(field)
                if field in {
                    "engine_posture_sha256",
                    "atif_engine_posture_sha256",
                }:
                    actual = actual.lower() if isinstance(actual, str) else actual
                if actual != expected:
                    reasons.append(
                        f"{label} {field} differs from the registered posture: "
                        f"{actual!r} != {expected!r}."
                    )
        for field, expected in identity_expectations.items():
            actual = row.get(field)
            if field in {
                "binary_sha256",
                "source_commit",
                "adapter_sha256",
            }:
                actual = actual.lower() if isinstance(actual, str) else actual
                expected = expected.lower() if isinstance(expected, str) else expected
            if expected is not _MISSING and actual != expected:
                reasons.append(
                    f"{label} {field} differs from the frozen SUT: "
                    f"{actual!r} != {expected!r}."
                )
        if row.get("source_commit_verified_in_binary") is not True:
            reasons.append(f"{label} source commit is not verified in the binary.")
        if reward is not None and reward > 0 and row.get("atif_valid") is not True:
            reasons.append(f"{label} passed without a valid ATIF-v1.7 trajectory.")

    ranking: list[dict[str, Any]] = []
    for model in CALIBRATION_MODEL_ORDER:
        model_rows = [row for row in attempted if row.get("model") == model]
        accuracy = [_number(row.get("accuracy_value")) for row in model_rows]
        tokens = [_nonnegative_int(row.get("token_spend")) for row in model_rows]
        walls = [
            _nonnegative_float(row.get("agent_wall_seconds")) for row in model_rows
        ]
        costs = [_nonnegative_float(row.get("cost_usd")) for row in model_rows]
        coverage_complete = (
            len(model_rows)
            == len(CALIBRATION_TASKS) * CALIBRATION_ATTEMPTS_PER_MODEL_TASK
            and all(value is not None for value in accuracy)
            and all(value is not None for value in costs)
        )
        passes = sum(
            1 for value in accuracy if value is not None and float(value) == 1.0
        )
        total_tokens = (
            sum(value for value in tokens if value is not None)
            if all(value is not None for value in tokens)
            else None
        )
        median_wall = (
            statistics.median(value for value in walls if value is not None)
            if walls and all(value is not None for value in walls)
            else None
        )
        calibration_spend = (
            sum(value for value in costs if value is not None)
            if costs and all(value is not None for value in costs)
            else None
        )
        projected_spend = (
            statistics.mean(value for value in costs if value is not None)
            * CALIBRATION_PROJECTION_TRIALS
            if costs and all(value is not None for value in costs)
            else None
        )
        telemetry_complete = all(
            not _trial_telemetry_reasons(
                row,
                allowed_call_models=CALIBRATION_CALL_MODELS[model],
                label="calibration",
            )
            for row in model_rows
        )
        successes_valid = all(
            _number(row.get("reward")) in (None, 0) or row.get("atif_valid") is True
            for row in model_rows
        )
        advances = (
            coverage_complete
            and telemetry_complete
            and successes_valid
            and passes >= CALIBRATION_MINIMUM_PASSES
            and projected_spend is not None
            and projected_spend <= CALIBRATION_SPEND_LIMIT_USD
        )
        ranking.append(
            {
                "model": model,
                "passes": passes,
                "total_tokens": total_tokens,
                "median_agent_wall_seconds": median_wall,
                "calibration_spend_usd": calibration_spend,
                "projected_445_trial_spend_usd": projected_spend,
                "telemetry_complete": telemetry_complete,
                "success_trajectories_valid": successes_valid,
                "advances": advances,
            }
        )

    eligible = [entry for entry in ranking if entry["advances"]]
    frozen_order = {model: index for index, model in enumerate(CALIBRATION_MODEL_ORDER)}
    eligible.sort(
        key=lambda entry: (
            -entry["passes"],
            entry["projected_445_trial_spend_usd"],
            frozen_order[entry["model"]],
        )
    )
    derived_winner: str | None = None
    if not eligible:
        reasons.append("No calibration configuration satisfies every advance gate.")
    else:
        derived_winner = eligible[0]["model"]
    registered_winner = expected_fields["selected_model"]
    if registered_winner is not _MISSING and derived_winner != registered_winner:
        reasons.append(
            "calibration.selected_model is not the calibration-derived winner: "
            f"derived {derived_winner!r}, manifest {registered_winner!r}."
        )

    return {
        "expected": canonical,
        "observed": {
            "rows": len(rows),
            "requested": requested_count,
            "instantiated": instantiated_count,
            "attempted": len(attempted),
            "job_names": sorted(actual_job_names),
            "job_ids": sorted(actual_job_ids),
            "physical_job_directories": sorted(sources),
            "input_job_count": physical_input_count,
            "trial_data_sha256": actual_digest,
            "excluded_job_ids": observed_excluded_ids,
            "excluded_ledger_sha256": actual_ledger_digest,
            "derived_winner": derived_winner,
        },
        "ranking": ranking,
    }


def _validate_confirmatory(
    manifest: dict[str, Any],
    rows: Sequence[dict[str, Any]],
    comparator_rows: Sequence[dict[str, Any]] | None,
    *,
    input_job_count: int | None,
    selected_model: Any,
    intent_sha256_by_job_id: dict[str, str],
    public_intent_expectation_by_job_id: dict[str, dict[str, Any]],
    structural_reasons: list[str],
    reasons: list[str],
) -> dict[str, Any]:
    """Require one immutable physical Harbor job with exact 89 x 5 coverage."""
    section = manifest.get("confirmatory")
    _require_exact_manifest_fields(
        section,
        STUDY_MANIFEST_CONFIRMATORY_FIELDS,
        "confirmatory",
        structural_reasons,
    )
    job_name = _required_manifest_value(
        section,
        "job_name",
        "confirmatory.job_name",
        structural_reasons,
    )
    n_concurrent_trials = _required_manifest_value(
        section,
        "n_concurrent_trials",
        "confirmatory.n_concurrent_trials",
        structural_reasons,
    )
    for label, value in (("confirmatory.job_name", job_name),):
        if value is not _MISSING and (not isinstance(value, str) or not value.strip()):
            structural_reasons.append(
                f"Study manifest field {label} must be a non-empty string."
            )
    if n_concurrent_trials is not _MISSING and (
        _nonnegative_int(n_concurrent_trials) != CONFIRMATORY_N_CONCURRENT_TRIALS
    ):
        structural_reasons.append(
            "Study manifest confirmatory.n_concurrent_trials must be exactly 1."
        )

    expected_total = DEFAULT_EXPECTED_TASKS * DEFAULT_TRIALS_PER_TASK
    attempted = [row for row in rows if row.get("attempted") is True]
    requested_count = sum(row.get("requested") is True for row in rows)
    instantiated_count = sum(row.get("instantiated") is True for row in rows)
    exact_counts = (
        len(rows) == expected_total
        and requested_count == expected_total
        and instantiated_count == expected_total
        and len(attempted) == expected_total
    )
    if not exact_counts:
        reasons.append(
            "Confirmatory input must be exactly 445 rows with all 445 slots "
            "requested, instantiated, and attempted; partial jobs, extras, and "
            "replacement trials are ineligible."
        )

    sources = {
        row.get("source_input")
        for row in rows
        if isinstance(row.get("source_input"), str) and row.get("source_input")
    }
    physical_input_count = len(sources) if input_job_count is None else input_job_count
    observed_job_names = {
        row.get("job_name")
        for row in rows
        if isinstance(row.get("job_name"), str) and row.get("job_name")
    }
    observed_job_ids = {
        row.get("job_id")
        for row in rows
        if isinstance(row.get("job_id"), str) and row.get("job_id")
    }
    if physical_input_count != 1 or len(sources) != 1:
        reasons.append(
            "Confirmatory rows must come from exactly one supplied physical Harbor "
            f"job directory; received {physical_input_count}, observed row sources "
            f"{sorted(sources)!r}. Multiple directories, resumes, and stitched jobs "
            "are ineligible."
        )
    if len(observed_job_ids) != 1:
        reasons.append(
            "Confirmatory rows must contain exactly one Harbor job ID; observed "
            f"{sorted(observed_job_ids)!r}."
        )
    if isinstance(job_name, str) and observed_job_names != {job_name}:
        reasons.append(
            "Confirmatory Harbor job name does not match the frozen manifest: "
            f"observed {sorted(observed_job_names)!r}."
        )
    for source in sources:
        row = next(item for item in rows if item.get("source_input") == source)
        reasons.extend(
            _exact_harbor_job_reasons(
                row,
                expected_n_concurrent=CONFIRMATORY_N_CONCURRENT_TRIALS,
                label=f"Confirmatory Harbor job {source}",
            )
        )
        expected_models = [selected_model] if isinstance(selected_model, str) else []
        public_intent_expectation = public_intent_expectation_by_job_id.get(
            row.get("job_id"), {}
        )
        reasons.extend(
            _launch_receipt_reasons(
                row,
                expected_job_name=job_name if isinstance(job_name, str) else "",
                expected_models=expected_models,
                expected_intent_sha256=intent_sha256_by_job_id.get(row.get("job_id")),
                expected_kind=public_intent_expectation.get("kind"),
                expected_subject_commit=public_intent_expectation.get("subject_commit"),
                expected_ledger_commit=public_intent_expectation.get("ledger_commit"),
                expected_runtime_identity=public_intent_expectation.get(
                    "runtime_identity"
                ),
                expected_provider_key=public_intent_expectation.get("provider_key"),
                expected_prior_stage_outcome=public_intent_expectation.get(
                    "prior_stage_outcome"
                ),
                expected_projected_spend_usd=public_intent_expectation.get(
                    "projected_spend_usd"
                ),
                label=f"Confirmatory Harbor job {source}",
            )
        )
        reasons.extend(
            _host_attestation_reasons(
                row,
                expected_job_name=job_name if isinstance(job_name, str) else "",
                expected_intent_sha256=intent_sha256_by_job_id.get(row.get("job_id")),
                expected_stage="confirmatory",
                label=f"Confirmatory Harbor job {source}",
            )
        )

    trial_ids = [row.get("trial_id") for row in attempted]
    valid_trial_ids = [value for value in trial_ids if isinstance(value, str) and value]
    if (
        len(valid_trial_ids) != expected_total
        or len(set(valid_trial_ids)) != expected_total
    ):
        reasons.append(
            "Confirmatory input must contain 445 non-empty, unique Harbor trial "
            "IDs; duplicate or missing IDs are ineligible."
        )
    slot_ids = [row.get("slot_id") for row in rows]
    if (
        any(not isinstance(value, str) or not value for value in slot_ids)
        or len(set(slot_ids)) != expected_total
    ):
        reasons.append(
            "Confirmatory requested slot IDs must be 445 non-empty unique values."
        )

    comparator_attempted = [
        row
        for row in (comparator_rows or [])
        if row.get("attempted") is True and isinstance(row.get("task"), str)
    ]
    comparator_task_counts = Counter(row.get("task") for row in comparator_attempted)
    comparator_identity_map, comparator_identity_reasons = (
        _comparator_task_identity_map(comparator_attempted)
    )
    reasons.extend(comparator_identity_reasons)
    if len(comparator_task_counts) != DEFAULT_EXPECTED_TASKS or any(
        count != DEFAULT_TRIALS_PER_TASK for count in comparator_task_counts.values()
    ):
        reasons.append(
            "The frozen comparator does not expose the canonical 89-task set "
            "needed to validate confirmatory slots."
        )
        expected_slots: Counter[tuple[Any, Any]] = Counter()
    else:
        expected_slots = Counter(
            (task, attempt)
            for task in comparator_task_counts
            for attempt in range(1, DEFAULT_TRIALS_PER_TASK + 1)
        )
    observed_slots = Counter(
        (row.get("task"), row.get("attempt_index")) for row in attempted
    )
    if not expected_slots or observed_slots != expected_slots:
        reasons.append(
            "Confirmatory rows do not cover each frozen task-attempt slot exactly "
            "once (89 tasks x attempts 1..5); reruns, omissions, and substitutions "
            "are ineligible."
        )

    for row in attempted:
        task = row.get("task")
        expected_identity = comparator_identity_map.get(task)
        if expected_identity is None:
            continue
        if row.get("task_ref") != expected_identity["task_ref"]:
            reasons.append(
                f"Confirmatory trial {row.get('trial_id')!r} task ref differs "
                "from the validated pinned comparator mapping."
            )
        if (
            _normalized_sha256(row.get("task_checksum"))
            != expected_identity["task_checksum"]
        ):
            reasons.append(
                f"Confirmatory trial {row.get('trial_id')!r} task checksum differs "
                "from the validated pinned comparator mapping."
            )

    for row in attempted:
        reasons.extend(
            _exact_harbor_trial_reasons(
                row,
                label=f"Confirmatory trial {row.get('trial_id')!r}",
            )
        )
        reward = _number(row.get("reward"))
        accuracy = _number(row.get("accuracy_value"))
        if reward is not None and not 0 <= float(reward) <= 1:
            reasons.append(
                f"Confirmatory trial {row.get('trial_id')!r} has reward outside "
                f"[0, 1]: {reward!r}."
            )
        if accuracy is None or not 0 <= float(accuracy) <= 1:
            reasons.append(
                f"Confirmatory trial {row.get('trial_id')!r} has accuracy outside "
                f"[0, 1]: {accuracy!r}."
            )

    return {
        "expected": {
            "job_name": None if job_name is _MISSING else job_name,
            "job_id_source": "append-only run-ledger outcome plus Harbor rows",
            "physical_jobs": 1,
            "requested": expected_total,
            "instantiated": expected_total,
            "attempted": expected_total,
            "unique_trial_ids": expected_total,
            "tasks": DEFAULT_EXPECTED_TASKS,
            "attempts_per_task": DEFAULT_TRIALS_PER_TASK,
            "n_concurrent_trials": CONFIRMATORY_N_CONCURRENT_TRIALS,
        },
        "observed": {
            "physical_job_directories": sorted(sources),
            "input_job_count": physical_input_count,
            "job_names": sorted(observed_job_names),
            "job_ids": sorted(observed_job_ids),
            "rows": len(rows),
            "requested": requested_count,
            "instantiated": instantiated_count,
            "attempted": len(attempted),
            "unique_trial_ids": len(set(valid_trial_ids)),
            "unique_task_attempt_slots": len(observed_slots),
            "task_set_sha256": (
                _task_set_sha256(comparator_identity_map)
                if len(comparator_identity_map) == DEFAULT_EXPECTED_TASKS
                else None
            ),
        },
    }


def _live_receipt_publication_reasons(
    audit_publications: Sequence[Any],
    audit_commits: Sequence[Any],
    rows: Sequence[dict[str, Any]],
) -> list[str]:
    """Bind live comment witnesses to the pre-execution receipt witnesses."""
    reasons: list[str] = []
    receipt_by_subject: dict[str, dict[str, Any]] = {}
    for row in rows:
        if row.get("launch_receipt_present") is not True:
            continue
        encoded = row.get("launch_receipt_public_intent_attestation_json")
        try:
            attestation = json.loads(encoded) if isinstance(encoded, str) else None
        except json.JSONDecodeError:
            attestation = None
        if not isinstance(attestation, dict):
            reasons.append(
                "A paid secure launch receipt lacks its public-intent attestation."
            )
            continue
        subject_id = attestation.get("subject_id")
        if not isinstance(subject_id, str):
            reasons.append(
                "A paid secure launch receipt has no public intent subject digest."
            )
            continue
        existing = receipt_by_subject.get(subject_id)
        if existing is not None and existing != attestation:
            reasons.append(
                f"Paid intent {subject_id} has inconsistent launch receipt proofs."
            )
        receipt_by_subject[subject_id] = attestation

    live_by_subject = {
        item.get("subject_id"): item
        for item in audit_publications
        if isinstance(item, dict)
        and item.get("subject_type") == "intent"
        and isinstance(item.get("subject_id"), str)
    }
    live_commit_by_sha = {
        item.get("commit_sha"): item
        for item in audit_commits
        if isinstance(item, dict) and isinstance(item.get("commit_sha"), str)
    }
    if len(receipt_by_subject) != 3 or set(live_by_subject) != set(receipt_by_subject):
        reasons.append(
            "Live GitHub intent publications do not exactly cover the three paid "
            "launch-receipt preflight proofs."
        )
    field_pairs = {
        "subject_type": "subject_type",
        "subject_id": "subject_id",
        "kind": "kind",
        "subject_commit": "subject_commit",
        "ledger_commit": "ledger_commit",
        "comment_id": "comment_id",
        "html_url": "comment_url",
        "server_created_at": "server_created_at",
        "body_sha256": "body_sha256",
        "payload_sha256": "intent_sha256",
    }
    for subject_id, receipt in receipt_by_subject.items():
        live = live_by_subject.get(subject_id)
        if live is None:
            continue
        mismatches = [
            live_field
            for live_field, receipt_field in field_pairs.items()
            if live.get(live_field) != receipt.get(receipt_field)
        ]
        if live.get("verified") is not True or mismatches:
            reasons.append(
                f"Live GitHub witness for paid intent {subject_id} does not match "
                "its pre-execution receipt proof"
                + (f" at {', '.join(mismatches)}." if mismatches else ".")
            )
        ledger_commit = receipt.get("ledger_commit")
        live_commit = live_commit_by_sha.get(ledger_commit)
        live_files = live_commit.get("files") if isinstance(live_commit, dict) else None
        if not isinstance(live_files, dict) or live_files.get(
            receipt.get("ledger_path")
        ) != receipt.get("ledger_sha256"):
            reasons.append(
                f"Live GitHub ledger bytes for paid intent {subject_id} do not "
                "match its pre-execution receipt proof."
            )
    return reasons


def validate_study(
    rows: Sequence[dict[str, Any]],
    manifest: dict[str, Any] | None,
    *,
    comparator_rows: Sequence[dict[str, Any]] | None = None,
    calibration_rows: Sequence[dict[str, Any]] | None = None,
    calibration_ledger_rows: Sequence[dict[str, Any]] | None = None,
    run_ledger: dict[str, Any] | None = None,
    run_ledger_path: Path | None = None,
    run_ledger_sha256: str | None = None,
    study_manifest_sha256: str | None = None,
    public_timing_audit: LivePublicTimingAudit | None = None,
    confirmatory_input_job_count: int | None = None,
    calibration_input_job_count: int | None = None,
    expected_tasks: int = DEFAULT_EXPECTED_TASKS,
    expected_trials_per_task: int = DEFAULT_TRIALS_PER_TASK,
) -> dict[str, Any]:
    """Validate that all Stella trials are one frozen, canonical study SUT."""
    if manifest is None:
        reason = (
            "A JSON study manifest was not supplied; statistical estimates are "
            "descriptive and cannot establish a comparative claim."
        )
        return {
            "manifest_supplied": False,
            "manifest_valid": False,
            "homogeneous": False,
            "matches_manifest": False,
            "scientific_artifact_eligible": False,
            "public_timing_verified": False,
            "external_public_timing_audit_required": True,
            "claim_eligible": False,
            "reasons": [reason],
            "expected": None,
            "observed": {},
        }

    reasons: list[str] = []
    structural_reasons: list[str] = []
    _require_exact_manifest_fields(
        manifest,
        STUDY_MANIFEST_TOP_LEVEL_FIELDS,
        "top-level",
        structural_reasons,
    )
    if manifest.get("schema_version") != STUDY_MANIFEST_VERSION:
        structural_reasons.append(
            "Study manifest schema_version must be "
            f"{STUDY_MANIFEST_VERSION!r}; observed "
            f"{manifest.get('schema_version')!r}."
        )

    sut = manifest.get("sut")
    analysis = manifest.get("analysis")
    dataset = manifest.get("dataset")
    design = manifest.get("design")
    harbor = manifest.get("harbor")
    _require_exact_manifest_fields(
        sut,
        STUDY_MANIFEST_SUT_FIELDS,
        "sut",
        structural_reasons,
    )
    _require_exact_manifest_fields(
        design,
        STUDY_MANIFEST_DESIGN_FIELDS,
        "design",
        structural_reasons,
    )
    _require_exact_manifest_fields(
        harbor,
        STUDY_MANIFEST_HARBOR_FIELDS,
        "harbor",
        structural_reasons,
    )
    model = _required_manifest_value(sut, "model", "sut.model", structural_reasons)
    allowed_call_models = _required_manifest_value(
        sut,
        "allowed_call_models",
        "sut.allowed_call_models",
        structural_reasons,
    )
    binary_sha256 = _required_manifest_value(
        sut, "binary_sha256", "sut.binary_sha256", structural_reasons
    )
    source_commit = _required_manifest_value(
        sut, "source_commit", "sut.source_commit", structural_reasons
    )
    agent_version = _required_manifest_value(
        sut, "agent_version", "sut.agent_version", structural_reasons
    )
    adapter_version = _required_manifest_value(
        sut, "adapter_version", "sut.adapter_version", structural_reasons
    )
    adapter_sha256 = _required_manifest_value(
        sut, "adapter_sha256", "sut.adapter_sha256", structural_reasons
    )
    source_commit_embedded = _required_manifest_value(
        sut,
        "source_commit_embedded",
        "sut.source_commit_embedded",
        structural_reasons,
    )
    base_url = _required_manifest_value(
        sut, "base_url", "sut.base_url", structural_reasons
    )
    provider_route_policy = _required_manifest_value(
        sut,
        "provider_route_policy",
        "sut.provider_route_policy",
        structural_reasons,
    )
    host_credential_bundle_count = _required_manifest_value(
        sut,
        "host_credential_bundle_count",
        "sut.host_credential_bundle_count",
        structural_reasons,
    )
    host_credential_source = _required_manifest_value(
        sut,
        "host_credential_source",
        "sut.host_credential_source",
        structural_reasons,
    )
    host_credential_name = _required_manifest_value(
        sut,
        "host_credential_name",
        "sut.host_credential_name",
        structural_reasons,
    )
    engine_posture_version = _required_manifest_value(
        sut,
        "engine_posture_version",
        "sut.engine_posture_version",
        structural_reasons,
    )
    engine_posture = _required_manifest_value(
        sut,
        "engine_posture",
        "sut.engine_posture",
        structural_reasons,
    )
    if engine_posture is not _MISSING:
        _validate_engine_posture_manifest_schema(
            engine_posture,
            "sut.engine_posture",
            structural_reasons,
        )
    engine_posture_sha256 = _required_manifest_value(
        sut,
        "engine_posture_sha256",
        "sut.engine_posture_sha256",
        structural_reasons,
    )
    budget_usd = _required_manifest_value(
        sut, "budget_usd", "sut.budget_usd", structural_reasons
    )
    disable_reflection = _required_manifest_value(
        sut,
        "disable_reflection",
        "sut.disable_reflection",
        structural_reasons,
    )
    analysis_sha256 = _required_manifest_value(
        analysis, "sha256", "analysis.sha256", structural_reasons
    )
    public_timing_sha256 = _required_manifest_value(
        analysis,
        "public_timing_sha256",
        "analysis.public_timing_sha256",
        structural_reasons,
    )
    if not isinstance(analysis, dict) or set(analysis) != {
        "sha256",
        "public_timing_sha256",
    }:
        structural_reasons.append(
            "Study manifest analysis must contain exactly sha256 and "
            "public_timing_sha256."
        )
    dataset_name = _required_manifest_value(
        dataset, "name", "dataset.name", structural_reasons
    )
    dataset_ref = _required_manifest_value(
        dataset, "ref", "dataset.ref", structural_reasons
    )
    dataset_task_set_sha256 = _required_manifest_value(
        dataset,
        "task_set_sha256",
        "dataset.task_set_sha256",
        structural_reasons,
    )
    if not isinstance(dataset, dict) or set(dataset) != {
        "name",
        "ref",
        "task_set_sha256",
        *CANONICAL_HARBOR_DATASET_SETTINGS,
    }:
        structural_reasons.append(
            "Study manifest dataset fields differ from the exact frozen schema."
        )
    task_count = _required_manifest_value(
        design, "tasks", "design.tasks", structural_reasons
    )
    attempts = _required_manifest_value(
        design,
        "attempts_per_task",
        "design.attempts_per_task",
        structural_reasons,
    )
    harbor_version = _required_manifest_value(
        harbor, "version", "harbor.version", structural_reasons
    )
    harbor_sha256 = _required_manifest_value(
        harbor, "sha256", "harbor.sha256", structural_reasons
    )

    string_fields = {
        "sut.model": model,
        "sut.agent_version": agent_version,
        "sut.adapter_version": adapter_version,
        "sut.base_url": base_url,
        "sut.provider_route_policy": provider_route_policy,
        "sut.host_credential_source": host_credential_source,
        "sut.host_credential_name": host_credential_name,
        "sut.engine_posture_version": engine_posture_version,
        "dataset.name": dataset_name,
        "harbor.version": harbor_version,
    }
    for label, value in string_fields.items():
        if value is not _MISSING and (not isinstance(value, str) or not value.strip()):
            structural_reasons.append(f"Study manifest field {label} must be a string.")
    if model is not _MISSING and model != PRIMARY_MODEL:
        structural_reasons.append(
            "Study manifest sut.model must be the preregistered same-model primary "
            f"{PRIMARY_MODEL!r}; observed {model!r}."
        )
    if (
        not isinstance(allowed_call_models, list)
        or not allowed_call_models
        or not all(isinstance(value, str) and value for value in allowed_call_models)
        or len(set(allowed_call_models)) != len(allowed_call_models)
    ):
        structural_reasons.append(
            "Study manifest sut.allowed_call_models must be a non-empty array of "
            "unique model strings."
        )
    elif isinstance(model, str):
        canonical_call_models = REGISTERED_CALL_MODELS.get(model)
        if canonical_call_models is None or allowed_call_models != list(
            canonical_call_models
        ):
            structural_reasons.append(
                "Study manifest sut.allowed_call_models does not match the explicit "
                f"registered route for {model!r}: expected "
                f"{list(canonical_call_models or ())!r}, observed "
                f"{allowed_call_models!r}."
            )
    for label, digest in (
        ("sut.binary_sha256", binary_sha256),
        ("sut.adapter_sha256", adapter_sha256),
        ("sut.engine_posture_sha256", engine_posture_sha256),
        ("analysis.sha256", analysis_sha256),
        ("analysis.public_timing_sha256", public_timing_sha256),
        ("dataset.task_set_sha256", dataset_task_set_sha256),
        ("harbor.sha256", harbor_sha256),
    ):
        if digest is not _MISSING and (
            not isinstance(digest, str)
            or re.fullmatch(r"[0-9a-fA-F]{64}", digest) is None
        ):
            structural_reasons.append(
                f"Study manifest field {label} must be a 64-character hex digest."
            )
    if source_commit is not _MISSING and (
        not isinstance(source_commit, str)
        or re.fullmatch(r"[0-9a-fA-F]{40}", source_commit) is None
    ):
        structural_reasons.append(
            "Study manifest field sut.source_commit must be a full 40-character "
            "Git commit."
        )
    normalized_budget = _nonnegative_float(budget_usd)
    if budget_usd is not _MISSING and (
        normalized_budget is None or normalized_budget <= 0
    ):
        structural_reasons.append(
            "Study manifest field sut.budget_usd must be a positive number."
        )
    if disable_reflection is not _MISSING and not isinstance(disable_reflection, bool):
        structural_reasons.append(
            "Study manifest field sut.disable_reflection must be a boolean."
        )
    if source_commit_embedded is not True:
        structural_reasons.append(
            "Study manifest field sut.source_commit_embedded must be true."
        )
    if base_url is not _MISSING and base_url != CANONICAL_OPENROUTER_BASE_URL:
        structural_reasons.append(
            "Study manifest sut.base_url must freeze the canonical OpenRouter "
            f"endpoint {CANONICAL_OPENROUTER_BASE_URL!r}; observed {base_url!r}."
        )
    if (
        provider_route_policy is not _MISSING
        and provider_route_policy != CANONICAL_PROVIDER_ROUTE_POLICY
    ):
        structural_reasons.append(
            "Study manifest sut.provider_route_policy must transparently record "
            f"{CANONICAL_PROVIDER_ROUTE_POLICY!r}; observed "
            f"{provider_route_policy!r}."
        )
    if (
        host_credential_source is not _MISSING
        and host_credential_source != CANONICAL_HOST_CREDENTIAL_SOURCE
    ):
        structural_reasons.append(
            "Study manifest sut.host_credential_source must be the secure "
            f"launcher handoff {CANONICAL_HOST_CREDENTIAL_SOURCE!r}; observed "
            f"{host_credential_source!r}."
        )
    if (
        host_credential_name is not _MISSING
        and host_credential_name != CANONICAL_HOST_CREDENTIAL_NAME
    ):
        structural_reasons.append(
            "Study manifest sut.host_credential_name must be exactly "
            f"{CANONICAL_HOST_CREDENTIAL_NAME!r}; observed "
            f"{host_credential_name!r}."
        )
    if (
        host_credential_bundle_count is not _MISSING
        and _nonnegative_int(host_credential_bundle_count) != 1
    ):
        structural_reasons.append(
            "Study manifest sut.host_credential_bundle_count must be exactly 1."
        )
    if (
        engine_posture_version is not _MISSING
        and engine_posture_version != CANONICAL_ENGINE_POSTURE_VERSION
    ):
        structural_reasons.append(
            "Study manifest sut.engine_posture_version must be "
            f"{CANONICAL_ENGINE_POSTURE_VERSION!r}; observed "
            f"{engine_posture_version!r}."
        )
    manifest_posture_json = _normalized_json_object(engine_posture)
    expected_posture_json: str | Any = _MISSING
    expected_posture_sha256: str | Any = _MISSING
    if isinstance(model, str) and model.strip():
        try:
            expected_posture, expected_posture_json, expected_posture_sha256 = (
                canonical_engine_posture(model)
            )
        except ValueError:
            structural_reasons.append(
                "Study manifest sut.model cannot define a canonical engine posture."
            )
        else:
            if engine_posture is not _MISSING and engine_posture != expected_posture:
                structural_reasons.append(
                    "Study manifest sut.engine_posture is not the registered exact "
                    f"posture for {model!r}."
                )
            if (
                isinstance(engine_posture_sha256, str)
                and engine_posture_sha256.lower() != expected_posture_sha256
            ):
                structural_reasons.append(
                    "Study manifest sut.engine_posture_sha256 does not match the "
                    "canonical normalized posture: expected "
                    f"{expected_posture_sha256}, "
                    f"observed {engine_posture_sha256}."
                )
    if engine_posture is not _MISSING and manifest_posture_json is None:
        structural_reasons.append(
            "Study manifest sut.engine_posture must be a JSON object."
        )
    if (
        isinstance(analysis_sha256, str)
        and analysis_sha256.lower() != ANALYSIS_CONTENT_SHA256.lower()
    ):
        structural_reasons.append(
            "Study manifest analysis.sha256 does not match the executing analyzer: "
            f"expected {ANALYSIS_CONTENT_SHA256}, observed {analysis_sha256}."
        )
    if (
        isinstance(public_timing_sha256, str)
        and public_timing_sha256.lower() != PUBLIC_TIMING_CONTENT_SHA256.lower()
    ):
        structural_reasons.append(
            "Study manifest analysis.public_timing_sha256 does not match the "
            "executing public verifier: expected "
            f"{PUBLIC_TIMING_CONTENT_SHA256}, observed {public_timing_sha256}."
        )
    if harbor_version is not _MISSING and harbor_version != CANONICAL_HARBOR_VERSION:
        structural_reasons.append(
            "Study manifest harbor.version must be the frozen Harbor release "
            f"{CANONICAL_HARBOR_VERSION!r}; observed {harbor_version!r}."
        )
    if dataset_ref is not _MISSING and (
        not isinstance(dataset_ref, str)
        or re.fullmatch(r"sha256:[0-9a-fA-F]{64}", dataset_ref) is None
    ):
        structural_reasons.append(
            "Study manifest field dataset.ref must be a sha256: digest."
        )
    if dataset_name is not _MISSING and dataset_name != CANONICAL_DATASET_NAME:
        structural_reasons.append(
            "Study manifest dataset.name is not the canonical Terminal-Bench 2.1 "
            f"dataset: expected {CANONICAL_DATASET_NAME!r}, observed "
            f"{dataset_name!r}."
        )
    if dataset_ref is not _MISSING and dataset_ref != CANONICAL_DATASET_REF:
        structural_reasons.append(
            "Study manifest dataset.ref is not the frozen canonical dataset ref: "
            f"expected {CANONICAL_DATASET_REF!r}, observed {dataset_ref!r}."
        )

    normalized_tasks = _nonnegative_int(task_count)
    normalized_attempts = _nonnegative_int(attempts)
    if normalized_tasks != DEFAULT_EXPECTED_TASKS:
        structural_reasons.append(
            f"A claim manifest must freeze all {DEFAULT_EXPECTED_TASKS} canonical "
            f"tasks; observed {task_count!r}."
        )
    if normalized_attempts != DEFAULT_TRIALS_PER_TASK:
        structural_reasons.append(
            "A claim manifest must freeze exactly "
            f"{DEFAULT_TRIALS_PER_TASK} attempts per task; observed {attempts!r}."
        )
    if normalized_tasks != expected_tasks:
        structural_reasons.append(
            f"Study manifest design.tasks must equal the analysis design "
            f"({expected_tasks}); observed {task_count!r}."
        )
    if normalized_attempts != expected_trials_per_task:
        structural_reasons.append(
            "Study manifest design.attempts_per_task must equal the analysis design "
            f"({expected_trials_per_task}); observed {attempts!r}."
        )

    manifest_settings: dict[str, Any] = {}
    for name, canonical in CANONICAL_HARBOR_SETTINGS.items():
        value = _required_manifest_value(
            harbor,
            name,
            f"harbor.{name}",
            structural_reasons,
        )
        manifest_settings[name] = None if value is _MISSING else value
        if value is not _MISSING and not _matches_harbor_setting(
            name, value, canonical
        ):
            structural_reasons.append(
                f"Study manifest harbor.{name} is not canonical: expected "
                f"{canonical!r}, observed {value!r}."
            )
    manifest_job_settings: dict[str, Any] = {}
    for name, canonical in CANONICAL_HARBOR_JOB_SETTINGS.items():
        value = _required_manifest_value(
            harbor,
            name,
            f"harbor.{name}",
            structural_reasons,
        )
        manifest_job_settings[name] = None if value is _MISSING else value
        if value is not _MISSING and not _matches_harbor_setting(
            name, value, canonical
        ):
            structural_reasons.append(
                f"Study manifest harbor.{name} is not canonical: expected "
                f"{canonical!r}, observed {value!r}."
            )
    manifest_dataset_settings: dict[str, Any] = {}
    for name, canonical in CANONICAL_HARBOR_DATASET_SETTINGS.items():
        value = _required_manifest_value(
            dataset,
            name,
            f"dataset.{name}",
            structural_reasons,
        )
        manifest_dataset_settings[name] = None if value is _MISSING else value
        if value is not _MISSING and not _matches_setting(value, canonical):
            structural_reasons.append(
                f"Study manifest dataset.{name} is not canonical: expected "
                f"{canonical!r}, observed {value!r}."
            )

    comparator_validation = _validate_comparator(
        manifest,
        comparator_rows,
        structural_reasons,
        reasons,
    )
    comparator_identity_map = comparator_validation.get("task_identity_map")
    expected_task_set_sha256 = (
        _task_set_sha256(comparator_identity_map)
        if isinstance(comparator_identity_map, dict)
        and len(comparator_identity_map) == DEFAULT_EXPECTED_TASKS
        else None
    )
    if dataset_task_set_sha256 != expected_task_set_sha256:
        structural_reasons.append(
            "Study manifest dataset.task_set_sha256 does not equal the frozen "
            "comparator task refs/checksums."
        )
    run_ledger_validation = _validate_run_ledger(
        manifest,
        run_ledger,
        run_ledger_path=run_ledger_path,
        study_manifest_sha256=study_manifest_sha256,
        confirmatory_rows=rows,
        calibration_rows=calibration_rows,
        excluded_rows=calibration_ledger_rows,
        comparator_rows=comparator_rows,
        structural_reasons=structural_reasons,
        reasons=reasons,
    )
    intent_sha256_by_job_id = run_ledger_validation["intent_sha256_by_job_id"]
    public_intent_expectation_by_job_id = run_ledger_validation[
        "public_intent_expectation_by_job_id"
    ]
    calibration_validation = _validate_calibration(
        manifest,
        calibration_rows,
        calibration_ledger_rows,
        input_job_count=calibration_input_job_count,
        binary_sha256=binary_sha256,
        source_commit=source_commit,
        agent_version=agent_version,
        adapter_version=adapter_version,
        adapter_sha256=adapter_sha256,
        budget_usd=budget_usd,
        disable_reflection=disable_reflection,
        base_url=base_url,
        provider_route_policy=provider_route_policy,
        harbor_version=harbor_version,
        harbor_sha256=harbor_sha256,
        intent_sha256_by_job_id=intent_sha256_by_job_id,
        public_intent_expectation_by_job_id=(public_intent_expectation_by_job_id),
        structural_reasons=structural_reasons,
        reasons=reasons,
    )
    confirmatory_validation = _validate_confirmatory(
        manifest,
        rows,
        comparator_rows,
        input_job_count=confirmatory_input_job_count,
        selected_model=model,
        intent_sha256_by_job_id=intent_sha256_by_job_id,
        public_intent_expectation_by_job_id=(public_intent_expectation_by_job_id),
        structural_reasons=structural_reasons,
        reasons=reasons,
    )
    reasons.extend(structural_reasons)
    attempted = [row for row in rows if row.get("attempted")]
    if not attempted:
        reasons.append("The study contains no attempted Stella trials.")

    observed: dict[str, Any] = {}
    homogeneous = bool(attempted)

    def check_trial_field(
        field: str,
        label: str,
        expected: Any,
        *,
        normalize: Any = lambda value: value,
    ) -> None:
        nonlocal homogeneous
        present: list[Any] = []
        missing = 0
        for row in attempted:
            value = normalize(row.get(field))
            if value is None:
                missing += 1
            else:
                present.append(value)
        unique = set(present)
        observed[label] = {
            "values": _display_values(unique),
            "missing_trials": missing,
        }
        if missing:
            homogeneous = False
            reasons.append(
                f"Required Stella field {label} is missing for "
                f"{missing}/{len(attempted)} "
                "attempted trials."
            )
        if len(unique) > 1:
            homogeneous = False
            reasons.append(
                f"Attempted Stella trials are heterogeneous for {label}: "
                f"{_display_values(unique)!r}."
            )
        if expected is not _MISSING:
            mismatches = sum(value != expected for value in present)
            if mismatches:
                reasons.append(
                    f"Stella field {label} does not match the study manifest for "
                    f"{mismatches}/{len(attempted)} attempted trials; expected "
                    f"{expected!r}."
                )

    normalized_hash = (
        binary_sha256.lower() if isinstance(binary_sha256, str) else _MISSING
    )
    normalized_adapter_hash = (
        adapter_sha256.lower() if isinstance(adapter_sha256, str) else _MISSING
    )
    normalized_commit = (
        source_commit.lower() if isinstance(source_commit, str) else _MISSING
    )
    check_trial_field("model", "config model", model, normalize=_nonempty_string)
    check_trial_field(
        "stella_model", "reported Stella model", model, normalize=_nonempty_string
    )
    check_trial_field(
        "agent_info_model", "agent_info model", model, normalize=_nonempty_string
    )
    check_trial_field(
        "agent_info_name", "agent_info name", "stella", normalize=_nonempty_string
    )
    check_trial_field(
        "agent_info_version",
        "agent_info version",
        agent_version,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "stella_agent_version",
        "Stella agent version",
        agent_version,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "adapter_version",
        "adapter version",
        adapter_version,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "adapter_sha256",
        "adapter_sha256",
        normalized_adapter_hash,
        normalize=lambda value: value.lower() if isinstance(value, str) else None,
    )
    check_trial_field(
        "binary_sha256",
        "binary_sha256",
        normalized_hash,
        normalize=lambda value: value.lower() if isinstance(value, str) else None,
    )
    check_trial_field(
        "binary_sha256_verified_in_container",
        "binary_sha256 verified in container",
        True,
        normalize=_boolish,
    )
    check_trial_field(
        "source_commit",
        "source_commit",
        normalized_commit,
        normalize=lambda value: value.lower() if isinstance(value, str) else None,
    )
    check_trial_field(
        "source_commit_verified_in_binary",
        "source_commit verified in binary",
        True,
        normalize=_boolish,
    )
    check_trial_field(
        "budget_usd",
        "per-trial budget_usd",
        _MISSING if budget_usd is _MISSING else normalized_budget,
        normalize=_nonnegative_float,
    )
    check_trial_field(
        "disable_reflection",
        "disable_reflection",
        disable_reflection,
        normalize=_boolish,
    )
    check_trial_field(
        "base_url",
        "base_url",
        base_url,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "provider_route_policy",
        "provider route policy",
        provider_route_policy,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "host_credential_source",
        "host credential source",
        host_credential_source,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "host_credential_name",
        "host credential name",
        host_credential_name,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "host_credential_bundle_count",
        "host credential bundle count",
        (
            _MISSING
            if host_credential_bundle_count is _MISSING
            else _nonnegative_int(host_credential_bundle_count)
        ),
        normalize=_nonnegative_int,
    )
    check_trial_field(
        "container_credential_absence_verified",
        "live container credential absence verified",
        True,
        normalize=_boolish,
    )
    check_trial_field(
        "atif_host_credential_source",
        "ATIF host credential source",
        host_credential_source,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "atif_host_credential_name",
        "ATIF host credential name",
        host_credential_name,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "atif_host_credential_bundle_count",
        "ATIF host credential bundle count",
        (
            _MISSING
            if host_credential_bundle_count is _MISSING
            else _nonnegative_int(host_credential_bundle_count)
        ),
        normalize=_nonnegative_int,
    )
    check_trial_field(
        "atif_container_credential_absence_verified",
        "ATIF live container credential absence verified",
        True,
        normalize=_boolish,
    )
    check_trial_field(
        "engine_posture_version",
        "engine posture version",
        engine_posture_version,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "engine_posture_json",
        "engine posture normalized JSON",
        expected_posture_json,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "engine_posture_record_json",
        "engine posture metadata object",
        expected_posture_json,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "engine_posture_sha256",
        "engine posture SHA-256",
        expected_posture_sha256,
        normalize=lambda value: value.lower() if isinstance(value, str) else None,
    )
    check_trial_field(
        "atif_engine_posture_version",
        "ATIF engine posture version",
        engine_posture_version,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "atif_engine_posture_json",
        "ATIF engine posture normalized JSON",
        expected_posture_json,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "atif_engine_posture_record_json",
        "ATIF engine posture metadata object",
        expected_posture_json,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "atif_engine_posture_sha256",
        "ATIF engine posture SHA-256",
        expected_posture_sha256,
        normalize=lambda value: value.lower() if isinstance(value, str) else None,
    )
    check_trial_field(
        "harbor_version",
        "Harbor version",
        harbor_version,
        normalize=_nonempty_string,
    )
    check_trial_field(
        "harbor_sha256",
        "Harbor content SHA-256",
        harbor_sha256.lower() if isinstance(harbor_sha256, str) else _MISSING,
        normalize=lambda value: value.lower() if isinstance(value, str) else None,
    )
    check_trial_field(
        "trial_dataset_name",
        "trial dataset name",
        dataset_name,
        normalize=_nonempty_string,
    )

    job_rows: list[dict[str, Any]] = []
    for source in dict.fromkeys(row.get("source_input") for row in attempted):
        row = next(item for item in attempted if item.get("source_input") == source)
        job_rows.append(row)
    for row in job_rows:
        source = row.get("source_input")
        if row.get("job_dataset_count") != 1:
            reasons.append(
                f"Harbor job {source} must declare exactly one dataset; observed "
                f"{row.get('job_dataset_count')!r}."
            )
        if row.get("job_agent_count") != 1:
            reasons.append(
                f"Harbor job {source} must declare exactly one agent; observed "
                f"{row.get('job_agent_count')!r}."
            )
        if model is not _MISSING and row.get("job_agent_model") != model:
            reasons.append(
                f"Harbor job {source} agent model does not match the manifest: "
                f"{row.get('job_agent_model')!r}."
            )
        if row.get("job_agent_import_path") != CANONICAL_AGENT_IMPORT_PATH:
            reasons.append(
                f"Harbor job {source} does not use the canonical Stella adapter "
                f"import {CANONICAL_AGENT_IMPORT_PATH!r}: observed "
                f"{row.get('job_agent_import_path')!r}."
            )
        if dataset_name is not _MISSING and row.get("job_dataset_name") != dataset_name:
            reasons.append(
                f"Harbor job {source} dataset name does not match the manifest: "
                f"{row.get('job_dataset_name')!r}."
            )
        if dataset_ref is not _MISSING and row.get("job_dataset_ref") != dataset_ref:
            reasons.append(
                f"Harbor job {source} dataset ref does not match the manifest: "
                f"{row.get('job_dataset_ref')!r}."
            )
        if (
            normalized_attempts is not None
            and row.get("job_n_attempts") != normalized_attempts
        ):
            reasons.append(
                f"Harbor job {source} n_attempts does not match the manifest: "
                f"{row.get('job_n_attempts')!r}."
            )
        if (
            normalized_tasks is not None
            and row.get("job_task_count") != normalized_tasks
        ):
            reasons.append(
                f"Harbor job {source} task count does not match the manifest: "
                f"{row.get('job_task_count')!r}."
            )
        missing = row.get("job_harbor_missing_fields")
        if missing:
            reasons.append(
                f"Harbor job {source} omits canonical setting fields: {missing}."
            )
        for name, canonical in CANONICAL_HARBOR_SETTINGS.items():
            actual = row.get(f"job_{name}")
            if not _matches_setting(actual, canonical):
                reasons.append(
                    f"Harbor job {source} overrides/noncanonically sets {name}: "
                    f"expected {canonical!r}, observed {actual!r}."
                )

    for row in attempted:
        trial = row.get("trial_name") or row.get("slot_id")
        if isinstance(model, str) and isinstance(allowed_call_models, list):
            reasons.extend(
                _trial_telemetry_reasons(
                    row,
                    allowed_call_models=allowed_call_models,
                    label=f"Stella trial {trial}",
                )
            )
        missing = row.get("trial_harbor_missing_fields")
        if missing:
            reasons.append(
                f"Stella trial {trial} omits canonical setting fields: {missing}."
            )
        for name, canonical in CANONICAL_HARBOR_SETTINGS.items():
            actual = row.get(f"trial_{name}")
            if not _matches_setting(actual, canonical):
                reasons.append(
                    f"Stella trial {trial} overrides/noncanonically sets {name}: "
                    f"expected {canonical!r}, observed {actual!r}."
                )
        missing_tokens = [
            field
            for field in ("prompt_tokens", "completion_tokens", "cache_tokens")
            if row.get(field) is None
        ]
        if row.get("token_spend") is None:
            missing_tokens.append("token_spend")
        if missing_tokens:
            reasons.append(
                f"Stella trial {trial} has incomplete token telemetry: "
                + ", ".join(missing_tokens)
                + "."
            )
        reward = _number(row.get("reward"))
        if reward is not None and reward > 0 and row.get("atif_valid") is not True:
            reasons.append(
                f"Reward-positive Stella trial {trial} lacks a valid ATIF-v1.7 "
                "trajectory."
            )

    # Avoid multiplying the same reason across repeated rows while retaining
    # deterministic first-observed order for review artifacts.
    local_reasons = list(dict.fromkeys(reasons))
    scientific_artifact_eligible = not local_reasons
    timing_audit_reasons: list[str] = []
    public_timing_verified = False
    if public_timing_audit is not None:
        if not isinstance(public_timing_audit, LivePublicTimingAudit):
            timing_audit_reasons.append(
                "Public-timing evidence was not generated by the live verifier."
            )
            audit_report: dict[str, Any] = {}
            audit_inputs: dict[str, Any] | None = None
            audit_publications: list[Any] | None = None
            audit_commits: list[Any] | None = None
            audit_finalization: Any = None
        else:
            audit_report = public_timing_audit.report
            audit_inputs_value = audit_report.get("inputs")
            audit_inputs = (
                audit_inputs_value if isinstance(audit_inputs_value, dict) else None
            )
            audit_publications_value = audit_report.get("publications")
            audit_publications = (
                audit_publications_value
                if isinstance(audit_publications_value, list)
                else None
            )
            audit_commits_value = audit_report.get("commits")
            audit_commits = (
                audit_commits_value if isinstance(audit_commits_value, list) else None
            )
            audit_finalization = audit_report.get("finalization")
        if audit_inputs is None:
            timing_audit_reasons.append(
                "Live public-timing audit lacks its exact input-byte bindings."
            )
        receipt_live_reasons = (
            _live_receipt_publication_reasons(
                audit_publications,
                audit_commits or [],
                [
                    *rows,
                    *(calibration_rows or []),
                    *(calibration_ledger_rows or []),
                ],
            )
            if isinstance(audit_publications, list)
            else [
                "Live public-timing audit cannot bind paid launch receipts without "
                "publication witnesses."
            ]
        )
        timing_audit_reasons.extend(receipt_live_reasons)
        if isinstance(public_timing_audit, LivePublicTimingAudit) and (
            audit_report.get("schema_version") != PUBLIC_TIMING_AUDIT_SCHEMA_VERSION
            or audit_report.get("repository") != FIXED_REPOSITORY
            or audit_report.get("valid") is not True
            or audit_report.get("errors") != []
            or public_timing_audit.run_ledger_sha256 != run_ledger_sha256
            or public_timing_audit.study_manifest_sha256 != study_manifest_sha256
            or not isinstance(audit_inputs, dict)
            or audit_inputs.get("run_ledger_sha256") != run_ledger_sha256
            or audit_inputs.get("study_manifest_sha256") != study_manifest_sha256
            or not isinstance(audit_publications, list)
            or len(audit_publications) != 6
            or not all(
                isinstance(item, dict) and item.get("verified") is True
                for item in audit_publications or []
            )
            or not isinstance(audit_commits, list)
            or not audit_commits
            or not all(
                isinstance(item, dict) and item.get("verified") is True
                for item in audit_commits or []
            )
            or not isinstance(audit_finalization, dict)
            or audit_finalization.get("verified") is not True
            or bool(receipt_live_reasons)
        ):
            timing_audit_reasons.append(
                "Live GitHub public-timing audit is invalid or does not bind the "
                "exact supplied ledger and manifest bytes."
            )
            if isinstance(audit_report.get("errors"), list):
                timing_audit_reasons.extend(
                    f"Live GitHub audit: {reason}"
                    for reason in audit_report["errors"]
                    if isinstance(reason, str)
                )
        elif isinstance(public_timing_audit, LivePublicTimingAudit):
            public_timing_verified = True
    if public_timing_verified:
        reasons = local_reasons
    else:
        reasons = [
            *local_reasons,
            *timing_audit_reasons,
            EXTERNAL_PUBLIC_TIMING_AUDIT_REASON,
        ]
    expected = {
        "model": None if model is _MISSING else model,
        "allowed_call_models": (
            None if allowed_call_models is _MISSING else allowed_call_models
        ),
        "provider_route_policy": (
            None if provider_route_policy is _MISSING else provider_route_policy
        ),
        "host_credential_bundle_count": (
            None
            if host_credential_bundle_count is _MISSING
            else host_credential_bundle_count
        ),
        "host_credential_source": (
            None if host_credential_source is _MISSING else host_credential_source
        ),
        "host_credential_name": (
            None if host_credential_name is _MISSING else host_credential_name
        ),
        "engine_posture_version": (
            None if engine_posture_version is _MISSING else engine_posture_version
        ),
        "engine_posture": None if engine_posture is _MISSING else engine_posture,
        "engine_posture_sha256": (
            None if engine_posture_sha256 is _MISSING else engine_posture_sha256
        ),
        "binary_sha256": None if binary_sha256 is _MISSING else binary_sha256,
        "source_commit": None if source_commit is _MISSING else source_commit,
        "agent_version": None if agent_version is _MISSING else agent_version,
        "adapter_version": None if adapter_version is _MISSING else adapter_version,
        "adapter_sha256": (None if adapter_sha256 is _MISSING else adapter_sha256),
        "analysis_sha256": (None if analysis_sha256 is _MISSING else analysis_sha256),
        "public_timing_sha256": (
            None if public_timing_sha256 is _MISSING else public_timing_sha256
        ),
        "budget_usd": None if budget_usd is _MISSING else budget_usd,
        "disable_reflection": (
            None if disable_reflection is _MISSING else disable_reflection
        ),
        "dataset_name": None if dataset_name is _MISSING else dataset_name,
        "dataset_ref": None if dataset_ref is _MISSING else dataset_ref,
        "dataset_task_set_sha256": (
            None if dataset_task_set_sha256 is _MISSING else dataset_task_set_sha256
        ),
        "tasks": None if task_count is _MISSING else task_count,
        "attempts_per_task": None if attempts is _MISSING else attempts,
        "harbor": {**manifest_settings, **manifest_job_settings},
        "dataset_settings": manifest_dataset_settings,
        "comparator": comparator_validation["expected"],
        "calibration": calibration_validation["expected"],
        "confirmatory": confirmatory_validation["expected"],
    }
    return {
        "manifest_supplied": True,
        "manifest_valid": not structural_reasons,
        "homogeneous": homogeneous,
        "matches_manifest": scientific_artifact_eligible,
        "scientific_artifact_eligible": scientific_artifact_eligible,
        "public_timing_verified": public_timing_verified,
        "external_public_timing_audit_required": not public_timing_verified,
        "claim_eligible": scientific_artifact_eligible and public_timing_verified,
        "reasons": reasons,
        "local_reasons": local_reasons,
        "expected": expected,
        "observed": observed,
        "comparator_validation": comparator_validation,
        "calibration_validation": calibration_validation,
        "confirmatory_validation": confirmatory_validation,
        "run_ledger_validation": run_ledger_validation,
    }


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _job_id_from_comparator_row(raw: dict[str, Any]) -> str | None:
    direct = raw.get("job_id")
    if isinstance(direct, str) and direct:
        return direct
    for key in ("result_url", "trajectory_url"):
        value = raw.get(key)
        if not isinstance(value, str):
            continue
        match = re.search(r"(?:[?&]jobId=)([0-9a-fA-F-]{36})(?:&|$)", value)
        if match is not None:
            return match.group(1)
    return None


def _comparator_trial_data_sha256(rows: Sequence[dict[str, Any]]) -> str:
    """Hash normalized trial data that can affect a registered comparison."""
    fields = (
        "task",
        "task_name",
        "task_ref",
        "task_checksum",
        "trial_name",
        "trial_id",
        "job_id",
        "reward",
        "accuracy_value",
        "prompt_tokens",
        "completion_tokens",
        "cache_tokens",
        "token_spend",
        "cost_usd",
        "agent_wall_seconds",
        "exception_type",
    )
    trials = [{field: row.get(field) for field in fields} for row in rows]
    trials.sort(
        key=lambda row: json.dumps(
            row,
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
        )
    )
    payload = {
        "schema": "stella-tb21-comparator-normalized-v1",
        "trials": trials,
    }
    encoded = json.dumps(
        payload,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _coerce_file_row(
    raw: dict[str, Any], source: Path, *, source_sha256: str
) -> dict[str, Any]:
    def first(*names: str) -> Any:
        return next(
            (raw[name] for name in names if raw.get(name) not in (None, "")), None
        )

    task_name = first("task_name", "task")
    exception_type = first("exception_type", "error_type")
    status = first("status") or ("error" if exception_type else "completed")
    prompt_tokens = _nonnegative_int(
        first("prompt_tokens", "input_tokens", "n_input_tokens")
    )
    completion_tokens = _nonnegative_int(
        first("completion_tokens", "output_tokens", "n_output_tokens")
    )
    cache_tokens = _nonnegative_int(
        first("cache_tokens", "cached_input_tokens", "n_cache_tokens")
    )
    token_spend = _nonnegative_int(first("token_spend", "total_tokens"))
    computed_token_spend = None
    if prompt_tokens is not None and completion_tokens is not None:
        computed_token_spend = prompt_tokens + completion_tokens
    token_consistent = token_spend is None or token_spend == computed_token_spend
    if token_spend is None and computed_token_spend is not None:
        token_spend = computed_token_spend
    reward = _number(first("reward", "accuracy_value"))
    # Supplied comparator rows represent attempted, archived trials. Preserve
    # any canonical verifier reward even when the agent also has an exception;
    # a missing reward counts as zero under the official submission rule.
    accuracy_value = reward if reward is not None else 0.0
    attempted_raw = first("attempted")
    attempted = str(attempted_raw).lower() not in {"false", "0", "no"}
    return {
        "product": "comparator",
        "source_input": str(source.resolve()),
        "job_name": first("job_name"),
        "job_id": _job_id_from_comparator_row(raw),
        "slot_id": first("slot_id"),
        "requested": True,
        "instantiated": attempted,
        "attempted": attempted,
        "attempt_index": _nonnegative_int(first("attempt_index")),
        "status": status,
        "task": _normalize_task(task_name),
        "task_name": _full_task_name(task_name),
        "task_ref": first("task_ref"),
        "task_checksum": first("task_checksum"),
        "model": first("model", "model_name"),
        "trial_name": first("trial_name"),
        "trial_id": first("trial_id", "submitted_trial_id", "result_id", "id"),
        "trial_dir": first("trial_dir"),
        "reward": reward,
        "accuracy_value": accuracy_value,
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "cache_tokens": cache_tokens,
        "token_spend": token_spend,
        "cost_usd": _nonnegative_float(first("cost_usd")),
        "cost_source": first("cost_source") or "supplied_comparator_data",
        "agent_started_at": first("agent_started_at"),
        "agent_finished_at": first("agent_finished_at"),
        "agent_wall_seconds": _nonnegative_float(
            first("agent_wall_seconds", "wall_seconds")
        ),
        "exception_type": exception_type,
        "exception_message": first("exception_message", "error_message"),
        "stella_return_code": None,
        "atif_present": first("atif_present"),
        "atif_valid": first("atif_valid"),
        "atif_schema_version": first("atif_schema_version"),
        "atif_validation_method": first("atif_validation_method"),
        "atif_validation_error": first("atif_validation_error"),
        "result_path": first("result_path"),
        "comparator_source_sha256": source_sha256,
        "comparator_token_consistent": token_consistent,
    }


def _enrich_comparator_manifest_rows(
    rows: list[dict[str, Any]], source: Path, warnings: list[str]
) -> list[dict[str, Any]]:
    enriched: list[dict[str, Any]] = []
    for raw in rows:
        item = dict(raw)
        trial_id = next(
            (
                item.get(name)
                for name in ("submitted_trial_id", "trial_id", "result_id", "id")
                if isinstance(item.get(name), str) and item.get(name)
            ),
            None,
        )
        result_path = (
            source.parent / "trials" / trial_id / "result.json"
            if isinstance(trial_id, str)
            else None
        )
        if result_path is None or not result_path.is_file():
            enriched.append(item)
            continue
        try:
            expected_sha = item.get("result_sha256")
            observed_sha = _sha256_file(result_path)
            if isinstance(expected_sha, str) and observed_sha != expected_sha.lower():
                raise ValueError(
                    f"{result_path}: SHA-256 differs from public comparator manifest"
                )
            result = _read_json(result_path)
        except (OSError, json.JSONDecodeError, ValueError) as error:
            warnings.append(f"{type(error).__name__}: {error}")
            enriched.append(item)
            continue
        if isinstance(result, dict):
            task_id = result.get("task_id")
            task_id = task_id if isinstance(task_id, dict) else {}
            config = result.get("config")
            config = config if isinstance(config, dict) else {}
            task = config.get("task")
            task = task if isinstance(task, dict) else {}
            item["task_ref"] = task_id.get("ref") or task.get("ref")
            item["task_checksum"] = result.get("task_checksum")
            item["result_path"] = str(result_path.resolve())
        enriched.append(item)
    return enriched


def load_comparator_inputs(paths: Sequence[Path]) -> tuple[list[dict], list[str]]:
    job_dirs = [path for path in paths if path.is_dir()]
    rows, warnings = ingest_jobs(job_dirs, product="comparator")
    for path in paths:
        if path.is_dir():
            continue
        if not path.is_file():
            raise ValueError(f"comparator input does not exist: {path}")
        source_sha256 = _sha256_file(path)
        if path.suffix.lower() == ".csv":
            with path.open(newline="", encoding="utf-8") as handle:
                raw_rows = list(csv.DictReader(handle))
        elif path.suffix.lower() == ".json":
            payload = _read_json(path)
            if isinstance(payload, list):
                raw_rows = payload
            elif isinstance(payload, dict):
                raw_rows = (
                    payload.get("trials")
                    or payload.get("rows")
                    or payload.get("entries")
                    or []
                )
                if payload.get("entries") is raw_rows:
                    raw_rows = _enrich_comparator_manifest_rows(
                        raw_rows, path, warnings
                    )
            else:
                raw_rows = []
        else:
            raise ValueError(
                f"comparator input must be a job directory, CSV, or JSON: {path}"
            )
        if not isinstance(raw_rows, list) or not all(
            isinstance(row, dict) for row in raw_rows
        ):
            raise ValueError(f"comparator rows are not an array of objects: {path}")
        rows.extend(
            _coerce_file_row(row, path, source_sha256=source_sha256) for row in raw_rows
        )
    if rows:
        trial_data_sha256 = _comparator_trial_data_sha256(rows)
        for row in rows:
            row["comparator_trial_data_sha256"] = trial_data_sha256
    return rows, warnings


def summarize(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    attempted = [row for row in rows if row.get("attempted")]
    known_accuracy = [
        row["accuracy_value"] for row in attempted if row["accuracy_value"] is not None
    ]
    known_tokens = [
        row["token_spend"] for row in attempted if row["token_spend"] is not None
    ]
    known_costs = [row["cost_usd"] for row in attempted if row["cost_usd"] is not None]
    known_walls = [
        row["agent_wall_seconds"]
        for row in attempted
        if row["agent_wall_seconds"] is not None
    ]
    successes = [row for row in attempted if row.get("accuracy_value") == 1.0]
    status_counts = Counter(str(row.get("status")) for row in rows)
    exception_counts = Counter(
        str(row["exception_type"]) for row in attempted if row.get("exception_type")
    )
    valid_attempted = sum(row.get("atif_valid") is True for row in attempted)
    valid_successes = sum(row.get("atif_valid") is True for row in successes)
    return {
        "rows": len(rows),
        "requested_slots": sum(bool(row.get("requested")) for row in rows),
        "instantiated_trials": sum(bool(row.get("instantiated")) for row in rows),
        "attempted_trials": len(attempted),
        "status_counts": dict(sorted(status_counts.items())),
        "exception_counts": dict(sorted(exception_counts.items())),
        "accuracy": sum(known_accuracy) / len(attempted)
        if attempted and len(known_accuracy) == len(attempted)
        else None,
        "accuracy_coverage": f"{len(known_accuracy)}/{len(attempted)}",
        "prompt_tokens_observed": sum(
            row["prompt_tokens"]
            for row in attempted
            if row["prompt_tokens"] is not None
        ),
        "completion_tokens_observed": sum(
            row["completion_tokens"]
            for row in attempted
            if row["completion_tokens"] is not None
        ),
        "cache_tokens_observed_subset": sum(
            row["cache_tokens"] for row in attempted if row["cache_tokens"] is not None
        ),
        "token_spend": sum(known_tokens)
        if attempted and len(known_tokens) == len(attempted)
        else None,
        "token_spend_observed": sum(known_tokens),
        "token_coverage": f"{len(known_tokens)}/{len(attempted)}",
        "cost_usd": sum(known_costs)
        if attempted and len(known_costs) == len(attempted)
        else None,
        "cost_usd_observed": sum(known_costs),
        "cost_coverage": f"{len(known_costs)}/{len(attempted)}",
        "median_agent_wall_seconds": statistics.median(known_walls)
        if attempted and len(known_walls) == len(attempted)
        else None,
        "agent_wall_coverage": f"{len(known_walls)}/{len(attempted)}",
        "atif_valid_attempted": f"{valid_attempted}/{len(attempted)}",
        "atif_valid_successes": f"{valid_successes}/{len(successes)}",
    }


def _bootstrap_metric(
    stella_by_task: dict[str, list[float]],
    comparator_by_task: dict[str, list[float]],
    *,
    lower_is_better: bool,
    seed: int,
    draws: int,
) -> tuple[float, float] | None:
    tasks = sorted(stella_by_task)
    stella_stats = [
        (sum(stella_by_task[task]), len(stella_by_task[task])) for task in tasks
    ]
    comparator_stats = [
        (sum(comparator_by_task[task]), len(comparator_by_task[task])) for task in tasks
    ]

    def effect(indices: Iterable[int]) -> float | None:
        stella_sum = stella_count = comparator_sum = comparator_count = 0.0
        for index in indices:
            task_stella_sum, task_stella_count = stella_stats[index]
            task_comparator_sum, task_comparator_count = comparator_stats[index]
            stella_sum += task_stella_sum
            stella_count += task_stella_count
            comparator_sum += task_comparator_sum
            comparator_count += task_comparator_count
        if stella_count <= 0 or comparator_count <= 0:
            return None
        stella_mean = stella_sum / stella_count
        comparator_mean = comparator_sum / comparator_count
        if comparator_mean <= 0:
            return None
        if lower_is_better:
            return 1.0 - (stella_mean / comparator_mean)
        return (stella_mean / comparator_mean) - 1.0

    point = effect(range(len(tasks)))
    if point is None:
        return None
    rng = random.Random(seed)
    values: list[float] = []
    for _ in range(draws):
        sampled = (rng.randrange(len(tasks)) for _ in tasks)
        value = effect(sampled)
        if value is None:
            return None
        values.append(value)
    values.sort()
    lower_index = max(0, math.ceil(LOWER_BOUND_ALPHA * draws) - 1)
    return point, values[lower_index]


def _metric_point(
    stella_by_task: dict[str, list[float]],
    comparator_by_task: dict[str, list[float]],
    *,
    lower_is_better: bool,
) -> float | None:
    stella_values = [value for values in stella_by_task.values() for value in values]
    comparator_values = [
        value for values in comparator_by_task.values() for value in values
    ]
    if not stella_values or not comparator_values:
        return None
    comparator_mean = statistics.mean(comparator_values)
    if comparator_mean <= 0:
        return None
    stella_mean = statistics.mean(stella_values)
    if lower_is_better:
        return 1.0 - (stella_mean / comparator_mean)
    return (stella_mean / comparator_mean) - 1.0


def task_cluster_bootstrap(
    stella_rows: Sequence[dict[str, Any]],
    comparator_rows: Sequence[dict[str, Any]],
    *,
    seed: int = DEFAULT_BOOTSTRAP_SEED,
    draws: int = DEFAULT_BOOTSTRAP_DRAWS,
    expected_tasks: int = DEFAULT_EXPECTED_TASKS,
    expected_trials_per_task: int = DEFAULT_TRIALS_PER_TASK,
    wall_eligible: bool = False,
    claim_eligibility_reasons: Sequence[str] | None = None,
    public_timing_verified: bool = False,
    stella_model: str | None = None,
    stella_route_policy: str | None = None,
) -> dict[str, Any]:
    if claim_eligibility_reasons is None:
        eligibility_reasons = [
            "A validated JSON study manifest was not supplied to the bootstrap; "
            "estimates are descriptive only."
        ]
    else:
        eligibility_reasons = list(claim_eligibility_reasons)
    registered_design = {
        "seed": DEFAULT_BOOTSTRAP_SEED,
        "draws": DEFAULT_BOOTSTRAP_DRAWS,
        "expected_tasks": DEFAULT_EXPECTED_TASKS,
        "expected_trials_per_task": DEFAULT_TRIALS_PER_TASK,
        "wall_eligible": False,
        "inference_tasks": DEFAULT_INFERENCE_TASKS,
    }
    observed_design = {
        "seed": seed,
        "draws": draws,
        "expected_tasks": expected_tasks,
        "expected_trials_per_task": expected_trials_per_task,
        "wall_eligible": wall_eligible,
        "inference_tasks": (
            expected_tasks - len(CALIBRATION_TASKS)
            if expected_tasks == DEFAULT_EXPECTED_TASKS
            else expected_tasks
        ),
    }
    if observed_design != registered_design:
        eligibility_reasons.append(
            "Claim mode requires the frozen bootstrap design exactly: seed "
            f"{DEFAULT_BOOTSTRAP_SEED}, {DEFAULT_BOOTSTRAP_DRAWS} draws, "
            f"{DEFAULT_EXPECTED_TASKS} tasks x {DEFAULT_TRIALS_PER_TASK} attempts, "
            f"{DEFAULT_INFERENCE_TASKS} non-calibration inference clusters, and "
            "wall_eligible=false. Custom settings are descriptive only."
        )
    if stella_model != PRIMARY_MODEL:
        eligibility_reasons.append(
            "Claim mode requires the frozen primary Stella model exactly: "
            f"{PRIMARY_MODEL!r}; observed {stella_model!r}."
        )
    local_eligibility_reasons = [
        reason
        for reason in eligibility_reasons
        if reason != EXTERNAL_PUBLIC_TIMING_AUDIT_REASON
    ]
    eligibility_reasons = list(local_eligibility_reasons)
    if not public_timing_verified:
        eligibility_reasons.append(EXTERNAL_PUBLIC_TIMING_AUDIT_REASON)
    same_model = None if stella_model is None else stella_model == PRIMARY_MODEL
    base = {
        "available": False,
        "seed": seed,
        "draws": draws,
        "expected_tasks": expected_tasks,
        "expected_inference_tasks": (
            DEFAULT_INFERENCE_TASKS
            if expected_tasks == DEFAULT_EXPECTED_TASKS
            else expected_tasks
        ),
        "inference_excluded_tasks": (
            list(CALIBRATION_TASKS) if expected_tasks == DEFAULT_EXPECTED_TASKS else []
        ),
        "expected_trials_per_product_per_task": expected_trials_per_task,
        "one_sided_confidence": 1.0 - LOWER_BOUND_ALPHA,
        "alpha": LOWER_BOUND_ALPHA,
        "lower_quantile_order_statistic": "ceil(alpha * draws) - 1",
        "win_threshold_relative": WIN_THRESHOLD,
        "registered_point_thresholds": {
            "accuracy_score": ACCURACY_POINT_THRESHOLD,
            "accuracy_min_binary_passes": ACCURACY_MIN_BINARY_PASSES,
            "tokens_max_integer": TOKEN_POINT_THRESHOLD_MAX,
        },
        "dimensions": {},
        "wins": 0,
        "statistical_design_artifact_established": False,
        "scientific_claim_eligible": not local_eligibility_reasons,
        "public_timing_verified": public_timing_verified,
        "external_public_timing_audit_required": not public_timing_verified,
        "claim_established": False,
        "claim_eligible": not eligibility_reasons,
        "claim_eligibility_reasons": list(dict.fromkeys(eligibility_reasons)),
        "unavailable_reasons": [],
        "claim_scope": {
            "statement": (
                "Stella CLI versus Claude Code 2.1.123 using GLM-5.1 max on the "
                "frozen Terminal-Bench 2.1 public job comparator"
            ),
            "comparator": {
                "agent": COMPARATOR_AGENT_NAME,
                "agent_version": COMPARATOR_AGENT_VERSION,
                "model": COMPARATOR_MODEL,
                "reasoning_effort": COMPARATOR_REASONING_EFFORT,
                "public_job_id": COMPARATOR_PUBLIC_JOB_ID,
                "evidence_status": "historical public reviewed leaderboard job",
            },
            "stella": {
                "model": stella_model,
                "route_policy": stella_route_policy,
                "evidence_status": "new nonhistorical confirmatory Harbor job",
            },
            "same_model": same_model,
            "cross_model_tokenizer_confounding": same_model is False,
        },
    }
    if not comparator_rows:
        base["unavailable_reasons"] = [
            "Comparator per-task trial data was not supplied; aggregate leaderboard "
            "totals cannot support a task-cluster bootstrap."
        ]
        return base

    stella_attempted = [row for row in stella_rows if row.get("attempted")]
    comparator_attempted = [row for row in comparator_rows if row.get("attempted")]
    stella_counts = Counter(row.get("task") for row in stella_attempted)
    comparator_counts = Counter(row.get("task") for row in comparator_attempted)
    reasons: list[str] = []
    if None in stella_counts or None in comparator_counts:
        reasons.append("At least one attempted trial has no task identity.")
    stella_tasks = {task for task in stella_counts if task is not None}
    comparator_tasks = {task for task in comparator_counts if task is not None}
    if stella_tasks != comparator_tasks:
        missing_comparator = sorted(stella_tasks - comparator_tasks)
        missing_stella = sorted(comparator_tasks - stella_tasks)
        if missing_comparator:
            reasons.append(
                f"Comparator is missing tasks: {', '.join(missing_comparator)}"
            )
        if missing_stella:
            reasons.append(f"Stella is missing tasks: {', '.join(missing_stella)}")
    if len(stella_tasks) != expected_tasks or len(comparator_tasks) != expected_tasks:
        reasons.append(
            f"Expected {expected_tasks} shared tasks, observed {len(stella_tasks)} "
            f"Stella and {len(comparator_tasks)} comparator tasks."
        )
    bad_stella_counts = sorted(
        task
        for task, count in stella_counts.items()
        if task is not None and count != expected_trials_per_task
    )
    bad_comparator_counts = sorted(
        task
        for task, count in comparator_counts.items()
        if task is not None and count != expected_trials_per_task
    )
    if bad_stella_counts:
        reasons.append(
            "Stella does not have exactly "
            f"{expected_trials_per_task} attempted trials for: "
            + ", ".join(bad_stella_counts)
        )
    if bad_comparator_counts:
        reasons.append(
            "Comparator does not have exactly "
            f"{expected_trials_per_task} attempted trials for: "
            + ", ".join(bad_comparator_counts)
        )
    inference_exclusions = (
        set(CALIBRATION_TASKS) if expected_tasks == DEFAULT_EXPECTED_TASKS else set()
    )
    if inference_exclusions and not inference_exclusions.issubset(stella_tasks):
        reasons.append(
            "The registered 10 calibration tasks are not all present in the full "
            "89-task confirmatory/comparator task set."
        )
    inference_tasks = stella_tasks - inference_exclusions
    expected_inference_tasks = (
        DEFAULT_INFERENCE_TASKS
        if expected_tasks == DEFAULT_EXPECTED_TASKS
        else expected_tasks
    )
    if len(inference_tasks) != expected_inference_tasks:
        reasons.append(
            f"Expected {expected_inference_tasks} inferential task clusters after "
            f"calibration-task exclusion; observed {len(inference_tasks)}."
        )
    if reasons:
        base["unavailable_reasons"] = reasons
        return base

    base["available"] = True
    base["observed_inference_tasks"] = len(inference_tasks)
    dimension_specs = {
        "accuracy": ("accuracy_value", False, True),
        "tokens": ("token_spend", True, same_model is not False),
        "wall_clock": ("agent_wall_seconds", True, wall_eligible),
    }
    for name, (field, lower_is_better, eligible) in dimension_specs.items():
        missing_stella = [row for row in stella_attempted if row.get(field) is None]
        missing_comparator = [
            row for row in comparator_attempted if row.get(field) is None
        ]
        dimension = {
            "available": False,
            "eligible": eligible,
            "point_relative_improvement": None,
            "inferential_point_relative_improvement": None,
            "lower_confidence_bound": None,
            "point_meets_threshold": False,
            "lower_bound_exceeds_threshold": False,
            "win": False,
            "unavailable_reason": None,
        }
        if missing_stella or missing_comparator:
            dimension["unavailable_reason"] = (
                f"missing {field} for {len(missing_stella)} Stella and "
                f"{len(missing_comparator)} comparator trials"
            )
            base["dimensions"][name] = dimension
            continue

        stella_by_task: dict[str, list[float]] = defaultdict(list)
        comparator_by_task: dict[str, list[float]] = defaultdict(list)
        for row in stella_attempted:
            stella_by_task[row["task"]].append(float(row[field]))
        for row in comparator_attempted:
            comparator_by_task[row["task"]].append(float(row[field]))
        full_point = _metric_point(
            stella_by_task,
            comparator_by_task,
            lower_is_better=lower_is_better,
        )
        inference_stella_by_task = {
            task: values
            for task, values in stella_by_task.items()
            if task in inference_tasks
        }
        inference_comparator_by_task = {
            task: values
            for task, values in comparator_by_task.items()
            if task in inference_tasks
        }
        estimate = _bootstrap_metric(
            inference_stella_by_task,
            inference_comparator_by_task,
            lower_is_better=lower_is_better,
            seed=seed,
            draws=draws,
        )
        if full_point is None or estimate is None:
            dimension["unavailable_reason"] = (
                "the comparator denominator was zero in the point estimate or a draw"
            )
            base["dimensions"][name] = dimension
            continue
        inferential_point, lower = estimate
        point_meets_threshold = full_point >= WIN_THRESHOLD
        lower_bound_exceeds_threshold = lower > WIN_THRESHOLD
        dimension.update(
            {
                "available": True,
                "point_relative_improvement": full_point,
                "inferential_point_relative_improvement": inferential_point,
                "lower_confidence_bound": lower,
                "point_meets_threshold": point_meets_threshold,
                "lower_bound_exceeds_threshold": lower_bound_exceeds_threshold,
                "win": bool(
                    eligible and point_meets_threshold and lower_bound_exceeds_threshold
                ),
            }
        )
        if name == "wall_clock" and not eligible:
            dimension["eligibility_note"] = (
                "Descriptive only: --wall-eligible was not supplied."
            )
        if name == "tokens" and same_model is False:
            dimension["eligibility_note"] = (
                "Ineligible: Stella and Claude Code used different model/tokenizer "
                "families. Token counts are not a scientific spend comparison."
            )
        base["dimensions"][name] = dimension

    base["wins"] = sum(
        bool(dimension["win"]) for dimension in base["dimensions"].values()
    )
    base["statistical_design_artifact_established"] = bool(
        base["wins"] >= 2 and base["scientific_claim_eligible"]
    )
    base["claim_established"] = bool(
        base["statistical_design_artifact_established"]
        and base["claim_eligible"]
        and public_timing_verified
    )
    return base


def _format_number(value: Any, *, percent: bool = False) -> str:
    if value is None:
        return "unavailable"
    if percent:
        return f"{float(value):.2%}"
    if isinstance(value, float):
        return f"{value:,.4f}"
    return f"{value:,}" if isinstance(value, int) else str(value)


def render_markdown(report: dict[str, Any]) -> str:
    summary = report["summary"]
    study = report["study_validation"]
    bootstrap = report["bootstrap"]
    accuracy = _format_number(summary["accuracy"], percent=True)
    token_spend = _format_number(summary["token_spend"])
    observed_token_spend = _format_number(summary["token_spend_observed"])
    cost = _format_number(summary["cost_usd"])
    observed_cost = _format_number(summary["cost_usd_observed"])
    wall = _format_number(summary["median_agent_wall_seconds"])
    lines = [
        "# Terminal-Bench 2.1 audit report",
        "",
        f"Generated by `{ANALYSIS_VERSION}` from {len(report['inputs']['jobs'])} "
        "Stella Harbor job director"
        + ("y." if len(report["inputs"]["jobs"]) == 1 else "ies."),
        "",
        "## Trial accounting",
        "",
        "| Requested | Instantiated | Attempted | Completed | Errors | "
        "In progress | Not instantiated |",
        "|---:|---:|---:|---:|---:|---:|---:|",
        "| {requested_slots} | {instantiated_trials} | {attempted_trials} | "
        "{completed} | {error} | {in_progress} | {not_instantiated} |".format(
            **summary,
            completed=summary["status_counts"].get("completed", 0),
            error=summary["status_counts"].get("error", 0),
            in_progress=summary["status_counts"].get("in_progress", 0),
            not_instantiated=summary["status_counts"].get("not_instantiated", 0),
        ),
        "",
        "## Observed Stella metrics",
        "",
        "| Metric | Result | Coverage |",
        "|---|---:|---:|",
        f"| External verifier accuracy | {accuracy} | {summary['accuracy_coverage']} |",
        f"| Token spend | {token_spend} | {summary['token_coverage']} |",
        f"| Observed token spend (lower bound) | {observed_token_spend} | "
        f"{summary['token_coverage']} |",
        f"| USD cost | {cost} | {summary['cost_coverage']} |",
        f"| Recovered USD cost (lower bound) | {observed_cost} | "
        f"{summary['cost_coverage']} |",
        f"| Median external agent wall time (s) | {wall} | "
        f"{summary['agent_wall_coverage']} |",
        "| Valid ATIF among attempted trials | "
        f"{summary['atif_valid_attempted']} | -- |",
        "| Valid ATIF among verifier passes | "
        f"{summary['atif_valid_successes']} | -- |",
        "",
        "Token spend is prompt/input plus completion/output tokens. Cache tokens are "
        "a subset of prompt tokens and are not added again.",
        "",
        "## Frozen-SUT claim gate",
        "",
        "Study manifest supplied: "
        f"**{'yes' if study['manifest_supplied'] else 'no'}**. Scientific/artifact "
        "gate: "
        f"**{'yes' if study.get('scientific_artifact_eligible') else 'no'}**. "
        "External public timing verified: "
        f"**{'yes' if study.get('public_timing_verified') else 'no'}**. Final claim "
        f"eligible: **{'yes' if study['claim_eligible'] else 'no'}**.",
    ]
    if study["reasons"]:
        lines.extend(["", "Claim-blocking reasons:", ""])
        lines.extend(f"- {reason}" for reason in study["reasons"])
    lines.extend(
        [
            "",
            "## Comparative claim scope",
            "",
            bootstrap.get("claim_scope", {}).get("statement", "Unspecified."),
            "",
            "Stella model/route: `{} / {}`. Comparator: `{} {} / {} {}`; "
            "public job `{}`. Cross-model/tokenizer confounding: **{}**.".format(
                bootstrap.get("claim_scope", {}).get("stella", {}).get("model"),
                bootstrap.get("claim_scope", {}).get("stella", {}).get("route_policy"),
                COMPARATOR_AGENT_NAME,
                COMPARATOR_AGENT_VERSION,
                COMPARATOR_MODEL,
                COMPARATOR_REASONING_EFFORT,
                COMPARATOR_PUBLIC_JOB_ID,
                "yes"
                if bootstrap.get("claim_scope", {}).get(
                    "cross_model_tokenizer_confounding"
                )
                else "no",
            ),
            "",
            "## Preregistered task-cluster bootstrap",
            "",
        ]
    )
    if not bootstrap["available"]:
        lines.append("Unavailable:")
        lines.append("")
        lines.extend(f"- {reason}" for reason in bootstrap["unavailable_reasons"])
    else:
        lines.extend(
            [
                f"Fixed seed `{bootstrap['seed']}`, {bootstrap['draws']:,} draws, "
                f"one-sided {bootstrap['one_sided_confidence']:.2%} lower bounds.",
                f"Point gates use all {bootstrap['expected_tasks']} confirmatory "
                f"tasks; inferential estimates and LCBs use the "
                f"{bootstrap['observed_inference_tasks']} non-calibration tasks.",
                "",
                "| Dimension | Full point | 79-task inferential point | Lower bound | "
                "Eligible | "
                "Point >=10% and LCB >10% |",
                "|---|---:|---:|---:|:---:|:---:|",
            ]
        )
        for name in ("accuracy", "tokens", "wall_clock"):
            dimension = bootstrap["dimensions"].get(name, {})
            point = _format_number(
                dimension.get("point_relative_improvement"), percent=True
            )
            lower = _format_number(
                dimension.get("lower_confidence_bound"), percent=True
            )
            inferential = _format_number(
                dimension.get("inferential_point_relative_improvement"),
                percent=True,
            )
            lines.append(
                f"| {name.replace('_', ' ').title()} | "
                f"{point} | {inferential} | {lower} | "
                f"{'yes' if dimension.get('eligible') else 'no'} | "
                f"{'yes' if dimension.get('win') else 'no'} |"
            )
        established = "yes" if bootstrap["claim_established"] else "no"
        local_established = (
            "yes" if bootstrap.get("statistical_design_artifact_established") else "no"
        )
        lines.extend(
            [
                "",
                f"Registered wins: **{bootstrap['wins']} of 3**. Local statistical/"
                f"design/artifact gate established: **{local_established}**. Final "
                f"externally audited comparative claim established: **{established}**.",
            ]
        )
        if bootstrap["claim_eligibility_reasons"]:
            lines.extend(["", "Bootstrap claim-blocking reasons:", ""])
            lines.extend(
                f"- {reason}" for reason in bootstrap["claim_eligibility_reasons"]
            )
    if report["warnings"]:
        lines.extend(["", "## Audit warnings", ""])
        lines.extend(f"- {warning}" for warning in report["warnings"])
    lines.append("")
    return "\n".join(lines)


def write_outputs(report: dict[str, Any], output_dir: Path) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)
    with (output_dir / "trials.csv").open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=CSV_FIELDS, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(report["trials"])
    (output_dir / "report.json").write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    (output_dir / "report.md").write_text(render_markdown(report), encoding="utf-8")


def build_report(
    job_dirs: Sequence[Path],
    *,
    comparator_inputs: Sequence[Path] = (),
    calibration_job_dirs: Sequence[Path] = (),
    calibration_ledger_job_dirs: Sequence[Path] = (),
    study_manifest: Path | None = None,
    run_ledger: Path | None = None,
    public_timing_evidence: Path | None = None,
    seed: int = DEFAULT_BOOTSTRAP_SEED,
    draws: int = DEFAULT_BOOTSTRAP_DRAWS,
    expected_tasks: int = DEFAULT_EXPECTED_TASKS,
    expected_trials_per_task: int = DEFAULT_TRIALS_PER_TASK,
    wall_eligible: bool = False,
) -> dict[str, Any]:
    stella_rows, warnings = ingest_jobs(job_dirs)
    comparator_rows, comparator_warnings = load_comparator_inputs(comparator_inputs)
    warnings.extend(comparator_warnings)
    calibration_rows, calibration_warnings = ingest_jobs(
        calibration_job_dirs,
        product="calibration",
    )
    warnings.extend(calibration_warnings)
    calibration_ledger_rows, ledger_warnings = ingest_jobs(
        calibration_ledger_job_dirs,
        product="calibration_excluded",
    )
    warnings.extend(ledger_warnings)
    manifest_payload = (
        load_study_manifest(study_manifest) if study_manifest is not None else None
    )
    run_ledger_payload = load_run_ledger(run_ledger) if run_ledger is not None else None
    public_timing_audit: LivePublicTimingAudit | None = None
    if public_timing_evidence is not None:
        if study_manifest is None or run_ledger is None:
            raise ValueError(
                "live GitHub public-timing verification requires both "
                "--study-manifest and --run-ledger"
            )
        public_timing_audit = verify_github_public_timing(
            run_ledger,
            study_manifest,
            public_timing_evidence,
        )
    study = validate_study(
        stella_rows,
        manifest_payload,
        comparator_rows=comparator_rows,
        calibration_rows=(calibration_rows if calibration_job_dirs else None),
        calibration_ledger_rows=(
            calibration_ledger_rows if calibration_ledger_job_dirs else None
        ),
        run_ledger=run_ledger_payload,
        run_ledger_path=run_ledger,
        run_ledger_sha256=(
            _sha256_file(run_ledger) if run_ledger is not None else None
        ),
        study_manifest_sha256=(
            _sha256_file(study_manifest) if study_manifest is not None else None
        ),
        public_timing_audit=public_timing_audit,
        confirmatory_input_job_count=len(job_dirs),
        calibration_input_job_count=len(calibration_job_dirs),
        expected_tasks=expected_tasks,
        expected_trials_per_task=expected_trials_per_task,
    )
    bootstrap = task_cluster_bootstrap(
        stella_rows,
        comparator_rows,
        seed=seed,
        draws=draws,
        expected_tasks=expected_tasks,
        expected_trials_per_task=expected_trials_per_task,
        wall_eligible=wall_eligible,
        claim_eligibility_reasons=study.get("local_reasons", study["reasons"]),
        public_timing_verified=study["public_timing_verified"],
        stella_model=(
            (manifest_payload.get("sut") or {}).get("model")
            if isinstance(manifest_payload, dict)
            else None
        ),
        stella_route_policy=(
            (manifest_payload.get("sut") or {}).get("provider_route_policy")
            if isinstance(manifest_payload, dict)
            else None
        ),
    )
    return {
        "schema_version": ANALYSIS_VERSION,
        "analysis_sha256": ANALYSIS_CONTENT_SHA256,
        "public_timing_sha256": PUBLIC_TIMING_CONTENT_SHA256,
        "generated_at": datetime.now(UTC).isoformat(),
        "inputs": {
            "jobs": [str(path.resolve()) for path in job_dirs],
            "comparators": [str(path.resolve()) for path in comparator_inputs],
            "calibration_jobs": [str(path.resolve()) for path in calibration_job_dirs],
            "calibration_ledger_jobs": [
                str(path.resolve()) for path in calibration_ledger_job_dirs
            ],
            "study_manifest": (
                str(study_manifest.resolve()) if study_manifest is not None else None
            ),
            "run_ledger": str(run_ledger.resolve()) if run_ledger is not None else None,
            "public_timing_evidence": (
                str(public_timing_evidence.resolve())
                if public_timing_evidence is not None
                else None
            ),
        },
        "metric_definitions": {
            "accuracy": (
                "canonical external verifier reward even when the agent also errors; "
                "an attempted trial with no verifier reward counts as zero"
            ),
            "token_spend": (
                "Harbor prompt/input tokens plus completion/output tokens; cache is a "
                "reported subset of prompt and is not added again"
            ),
            "wall_clock": "external Harbor agent_execution interval",
        },
        "summary": summarize(stella_rows),
        "study_validation": study,
        "public_timing_audit": (
            public_timing_audit.report if public_timing_audit is not None else None
        ),
        "bootstrap": bootstrap,
        "warnings": warnings,
        "trials": [
            *stella_rows,
            *comparator_rows,
            *calibration_rows,
            *calibration_ledger_rows,
        ],
    }


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("jobs", type=Path, nargs="+", help="Harbor job directories")
    parser.add_argument(
        "--comparator",
        type=Path,
        action="append",
        default=[],
        help="Comparator Harbor job directory, CSV, or JSON; repeatable",
    )
    parser.add_argument(
        "--calibration-job",
        type=Path,
        action="append",
        default=[],
        help=(
            "The single preregistered calibration Harbor job directory. Exactly "
            "one is required for claim eligibility"
        ),
    )
    parser.add_argument(
        "--calibration-ledger-job",
        type=Path,
        action="append",
        default=[],
        help=(
            "Excluded/aborted calibration Harbor job directory; repeat for every "
            "entry in the frozen audit ledger. Required for claim eligibility"
        ),
    )
    parser.add_argument(
        "--study-manifest",
        type=Path,
        help=(
            "Frozen-SUT JSON manifest required for claim eligibility; without it "
            "bootstrap estimates are descriptive only"
        ),
    )
    parser.add_argument(
        "--run-ledger",
        type=Path,
        help=(
            "Append-only public preregistration, paid-run intent/publication, and "
            "provider reconciliation ledger required for claim eligibility"
        ),
    )
    parser.add_argument(
        "--github-public-timing-evidence",
        type=Path,
        help=(
            "Issue-comment evidence map. When supplied, the analyzer performs "
            "fresh read-only GitHub API verification in this process; saved audit "
            "booleans are never accepted as claim evidence"
        ),
    )
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--bootstrap-seed", type=int, default=DEFAULT_BOOTSTRAP_SEED)
    parser.add_argument("--bootstrap-draws", type=int, default=DEFAULT_BOOTSTRAP_DRAWS)
    parser.add_argument("--expected-tasks", type=int, default=DEFAULT_EXPECTED_TASKS)
    parser.add_argument(
        "--expected-trials-per-task", type=int, default=DEFAULT_TRIALS_PER_TASK
    )
    parser.add_argument(
        "--wall-eligible",
        action="store_true",
        help="Allow wall time to count only for a preregistered matched comparison",
    )
    args = parser.parse_args(argv)
    for name in ("bootstrap_draws", "expected_tasks", "expected_trials_per_task"):
        if getattr(args, name) <= 0:
            parser.error(f"--{name.replace('_', '-')} must be positive")
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        report = build_report(
            args.jobs,
            comparator_inputs=args.comparator,
            calibration_job_dirs=args.calibration_job,
            calibration_ledger_job_dirs=args.calibration_ledger_job,
            study_manifest=args.study_manifest,
            run_ledger=args.run_ledger,
            public_timing_evidence=args.github_public_timing_evidence,
            seed=args.bootstrap_seed,
            draws=args.bootstrap_draws,
            expected_tasks=args.expected_tasks,
            expected_trials_per_task=args.expected_trials_per_task,
            wall_eligible=args.wall_eligible,
        )
    except (OSError, ValueError, json.JSONDecodeError) as error:
        raise SystemExit(f"analysis failed: {error}") from error
    write_outputs(report, args.output_dir)
    print(args.output_dir.resolve())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
