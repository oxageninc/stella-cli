"""Exec Harbor with provider credentials only in an anonymous host FD."""

from __future__ import annotations

import hashlib
import json
import math
import os
import pwd
import re
import ssl
import stat
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from collections.abc import Callable, Iterable, Mapping, Sequence
from contextlib import suppress
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from types import ModuleType
from typing import Any, Protocol

from .credential_bundle import (
    create_anonymous_credential_bundle,
    credential_values_from_environment,
    is_credential_env_name,
    provider_credential_for_model,
    sanitized_harbor_environment,
)
from .host_attestation import (
    HOST_ATTESTATION_FILENAME,
    HostAttestationError,
    build_launch_binding,
    probe_host,
    public_report_path,
    write_launch_binding_sidecar,
)

_CANONICAL_AGENT_IMPORT_PATH = "stella_harbor:StellaAgent"
_CANONICAL_DATASET_ARGUMENT = (
    "terminal-bench/terminal-bench-2-1@"
    "sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a"
)
_CANONICAL_BUDGET = "0.17"
_CANONICAL_DISABLE_REFLECTION = "1"
_CANONICAL_ADAPTER_VERSION = "0.6.0"
_CANONICAL_HARBOR_VERSION = "0.6.1"
_CANONICAL_OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1"
_CANONICAL_PROVIDER_ROUTE_POLICY = "openrouter-auto"
_DEDICATED_KEY_HARD_LIMIT_USD = 180.0
_OPENROUTER_KEY_URL = "https://openrouter.ai/api/v1/key"
_OPENROUTER_KEYS_URL = "https://openrouter.ai/api/v1/keys"
_OPENROUTER_CREDITS_URL = "https://openrouter.ai/api/v1/credits"
_FIXED_REPOSITORY = "macanderson/stella"
_FIXED_WEB_ROOT = f"https://github.com/{_FIXED_REPOSITORY}"
_FIXED_API_ROOT = f"https://api.github.com/repos/{_FIXED_REPOSITORY}"
_FIXED_STUDY_ID = "stella-tb21-scientific-study-v1"
_FIXED_LEDGER_PATH = "bench/evidence/stella-tb21-run-ledger.json"
_FIXED_MANIFEST_PATH = "bench/evidence/stella-tb21-study-manifest.json"
_FIXED_ANALYZER_PATH = "bench/terminal_bench_analysis/tb21_analysis.py"
_FIXED_PUBLIC_TIMING_PATH = "bench/terminal_bench_analysis/github_public_timing.py"
_FIXED_PROTOCOL_PATH = "bench/terminal-bench-2.1-protocol.md"
_FIXED_ADAPTER_SOURCE_PATHS = (
    "bench/harbor_adapter/stella_harbor/__init__.py",
    "bench/harbor_adapter/stella_harbor/atif.py",
    "bench/harbor_adapter/stella_harbor/credential_bundle.py",
    "bench/harbor_adapter/stella_harbor/host_attestation.py",
    "bench/harbor_adapter/stella_harbor/secure_launcher.py",
)
_FIXED_READINESS_SOURCE_PATHS = (
    "bench/readiness/synthetic-adapter-sentinel/environment/Dockerfile",
    "bench/readiness/synthetic-adapter-sentinel/environment/app/.env",
    "bench/readiness/synthetic-adapter-sentinel/environment/app/.stella/settings.json",
    "bench/readiness/synthetic-adapter-sentinel/environment/app/checks.py",
    "bench/readiness/synthetic-adapter-sentinel/environment/app/slugger.py",
    "bench/readiness/synthetic-adapter-sentinel/instruction.md",
    "bench/readiness/synthetic-adapter-sentinel/task.toml",
    "bench/readiness/synthetic-adapter-sentinel/tests/test.sh",
)
_RUN_LEDGER_SCHEMA = "stella-tb21-run-ledger-v2"
_GITHUB_ATTESTATION_SCHEMA = "stella-tb21-github-attestation-v2"
_PUBLIC_INTENT_ATTESTATION_SCHEMA = "stella-harbor-public-intent-preflight-v2"
_PUBLICATION_SAFETY_MARGIN_SECONDS = 2
_MAX_GITHUB_RESPONSE_BYTES = 8 * 1024 * 1024
_MAX_CLOCK_WAIT_SECONDS = 30.0
_TRUSTED_DOCKER_CANDIDATES = (
    Path("/opt/homebrew/bin/docker"),
    Path("/usr/local/bin/docker"),
    Path("/usr/bin/docker"),
)
_READINESS_JOB_NAME = "stella-readiness-synthetic-v1"
_CALIBRATION_JOB_NAME = "stella-tb21-calibration-20260721"
_PRIMARY_MODEL = "openrouter/z-ai/glm-5.1"
_CANDIDATE_MODELS = (
    "openrouter/deepseek/deepseek-v4-pro",
    "openrouter/z-ai/glm-5.2",
    "openrouter/x-ai/grok-4.5",
)
# A secondary full run cannot be selected safely until its complete 60-slot
# calibration outcome is mechanically replayed by the launcher.  Keep the paid
# gate primary-only rather than trusting a manifest's selected_model assertion.
_CONFIRMATORY_MODELS = (_PRIMARY_MODEL,)
_CALIBRATION_TASK_FILTERS = (
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
)
_CALIBRATION_TASKS = tuple(
    task.removeprefix("terminal-bench/") for task in _CALIBRATION_TASK_FILTERS
)
_REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS = (
    "9b704487-9d21-46a7-8103-e5396cb7d4ea",
    "0c44d9ee-4389-4c7a-8445-ea4be2404115",
    "c5686c41-1d2d-41cf-a275-177c2e6878b3",
    "37ee4276-8595-4ff9-8507-be21adb891cc",
    "7e59ed1e-2abe-40b9-bf7e-6b24c7f9a350",
)
_CANONICAL_HARBOR_SETTINGS: dict[str, Any] = {
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
_CANONICAL_HARBOR_JOB_SETTINGS: dict[str, Any] = {
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
_CANONICAL_HARBOR_DATASET_SETTINGS: dict[str, Any] = {
    "path": None,
    "version": None,
    "registry_url": None,
    "registry_path": None,
    "overwrite": False,
    "download_dir": None,
    "exclude_task_names_json": "null",
    "n_tasks": None,
}
_CANONICAL_COMPARATOR = {
    "public_job_id": "fd8707bb-51e8-56fa-8e46-769a82a531ae",
    "manifest_sha256": (
        "7963a7af2b306fd4b6e82963fdadf9374e701ec16f47b194300e2843c8002a76"
    ),
    "trial_data_sha256": (
        "f7b916c7d3028c62003bb12eeb1fff3df0bb41a82ce21ba6e59b3a1b50139a99"
    ),
    "submission": {
        "repository": "https://github.com/harbor-framework/terminal-bench-2-1",
        "commit": "327a5a0b2ee4675871dc57e1d53fff2d2cf974e1",
        "path": "leaderboard/submissions/2026-05-01-glm-5-1-max-claude-code.json",
        "sha256": "36d20c181be246dc55965bf4320a3005f292c737f31511bbde19ba1808a2bd2c",
    },
    "agent_name": "claude-code",
    "agent_version": "2.1.123",
    "model": "glm-5.1",
    "reasoning_effort": "max",
    "expected": {
        "rows": 445,
        "tasks": 89,
        "attempts_per_task": 5,
        "reward_total": 261.0,
        "token_spend_total": 398_783_761,
    },
}
_READINESS_TASK_REF = (
    "sha256:05a040c7df0fd77f66f533ba023cb5f16e2dd0f89957440b099374210e475ad6"
)
_READINESS_TASK_SET_SHA256 = (
    "2020954593c84785eec3b16817beefa84480aa05e0ba38ad88f31d87347e39eb"
)
_CALIBRATION_TASK_SET_SHA256 = (
    "61a065631b2afe551ade7504bab7f15b222b099c8fec1fbbfdc3f99ef5baeb46"
)
_CONFIRMATORY_TASK_SET_SHA256 = (
    "7e495afe0a86eaf572be1c2da2b9929c24e502adc888e550385d915cc0125ece"
)
_RUN_LEDGER_FIELDS = frozenset(
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
_HISTORICAL_SPEND_FIELDS = frozenset(
    {"known_lower_bound_usd", "unknown_cancellation_spend", "new_authorized_budget_usd"}
)
_PREREGISTRATION_FIELDS = frozenset(
    {"sequence", "kind", "commit", "study_manifest_sha256", "declared_at"}
)
_INTENT_WRAPPER_FIELDS = frozenset({"sequence", "intent", "intent_sha256"})
_INTENT_FIELDS = frozenset(
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
_INTENT_DATASET_FIELDS = frozenset({"name", "ref", "task_count", "task_set_sha256"})
_INTENT_ARTIFACT_FIELDS = frozenset(
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
_INTENT_EXECUTION_FIELDS = frozenset(
    {"base_url", "provider_route_policy", "disable_reflection"}
)
_INTENT_PROVIDER_FIELDS = frozenset(
    {"fingerprint_sha256", "label", "limit_usd", "usage_before_usd", "snapshot_at"}
)
_PUBLICATION_FIELDS = frozenset(
    {
        "sequence",
        "subject_type",
        "subject_id",
        "ledger_commit",
        "public_url",
        "published_at",
    }
)
_OUTCOME_FIELDS = frozenset(
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
_MANIFEST_FIELDS = frozenset(
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
_MANIFEST_PREREGISTRATION_FIELDS = frozenset(
    {"study_id", "run_ledger_path", "readiness_commit", "calibration_commit"}
)
_MANIFEST_SUT_FIELDS = frozenset(
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
_MANIFEST_ANALYSIS_FIELDS = frozenset({"sha256", "public_timing_sha256"})
_MANIFEST_DATASET_FIELDS = frozenset(
    {"name", "ref", "task_set_sha256", *_CANONICAL_HARBOR_DATASET_SETTINGS}
)
_MANIFEST_DESIGN_FIELDS = frozenset({"tasks", "attempts_per_task"})
_MANIFEST_HARBOR_FIELDS = frozenset(
    {
        "version",
        "sha256",
        *_CANONICAL_HARBOR_SETTINGS,
        *_CANONICAL_HARBOR_JOB_SETTINGS,
    }
)
_MANIFEST_COMPARATOR_FIELDS = frozenset(_CANONICAL_COMPARATOR)
_MANIFEST_CALIBRATION_FIELDS = frozenset(
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
_MANIFEST_CONFIRMATORY_FIELDS = frozenset({"job_name", "n_concurrent_trials"})
_ENGINE_POSTURE_RECORD_FIELDS = frozenset({"version", "posture", "sha256"})
_ENGINE_POSTURE_FIELDS = frozenset(
    {
        "default_model",
        "allowed_models",
        "auto_mode",
        "effort_auto",
        "reasoning_auto",
        "agents",
    }
)
_ENGINE_POSTURE_AGENT_ROLES = frozenset({"default", "worker", "judge", "triage"})
_ENGINE_POSTURE_AGENT_FIELDS = frozenset({"effort", "reasoning"})
RUNTIME_IDENTITY_FIELDS = frozenset(
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
PRIOR_STAGE_OUTCOME_FIELDS = frozenset(
    {"stage", "intent_sha256", "status", "completed_at", "recorded_at"}
)
PROVIDER_KEY_LIVE_SNAPSHOT_FIELDS = frozenset(
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
_COMMON_STAGE_OPTIONS = frozenset(
    {
        "--agent-import-path",
        "--env",
        "--intent-sha256",
        "--intent-comment-url",
        "--job-name",
        "--jobs-dir",
        "--max-retries",
        "--model",
        "--n-attempts",
        "--n-concurrent",
    }
)
_ALLOWED_CLAIM_OPTIONS = _COMMON_STAGE_OPTIONS | frozenset(
    {"--dataset", "--include-task-name", "--path"}
)
_ALLOWED_CLAIM_OPTION_ALIASES = {
    "-d": "--dataset",
    "-e": "--env",
    "-i": "--include-task-name",
    "-m": "--model",
}
_ELF64_LITTLE_ENDIAN_X86_64_HEADER_BYTES = 20
_SOURCE_COMMIT_RE = re.compile(r"[0-9a-f]{40}")
_VERSION_COMMIT_BYTES_RE = re.compile(rb"-dev\.([0-9A-Fa-f]{40})(?=[^0-9A-Fa-f])")
_VERSION_TEXT_BYTES_RE = re.compile(
    rb"(?<![0-9A-Za-z.-])([0-9]+\.[0-9]+\.[0-9]+-dev\.[0-9a-f]{40})"
    rb"(?=[^0-9A-Za-z.-]|$)"
)
_COMMENT_URL_RE = re.compile(
    r"https://github\.com/macanderson/stella/issues/(?P<issue>[1-9][0-9]*)"
    r"#issuecomment-(?P<comment>[1-9][0-9]*)"
)
_DEDICATED_KEY_LABEL = "stella-tb21-dedicated-key-v1"
_ISOLATED_HARBOR_SHIM = """\
import sys
adapter_root, site_root = sys.argv[1:3]
del sys.argv[1:3]
sys.path[:0] = [adapter_root, site_root]
sys.argv[0] = "harbor"
from harbor.cli.main import app
raise SystemExit(app())
"""
_PYTHON_CACHE_PREFIX_DIRNAME = ".stella-python-cache"
LAUNCH_RECEIPT_FILENAME = "stella-secure-launch-receipt.json"
LAUNCH_RECEIPT_SCHEMA = "stella-harbor-secure-launch-receipt-v2"
LAUNCH_RECEIPT_CONTROLS: dict[str, str] = {
    "command": "harbor-run-only",
    "agent_import_path": _CANONICAL_AGENT_IMPORT_PATH,
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
_BANNED_CLAIM_OPTIONS = frozenset(
    {
        "-a",
        "--agent",
        "-c",
        "--config",
        "-o",
        "--env-file",
        "--ae",
        "--agent-env",
        "--ve",
        "--verifier-env",
        "--ak",
        "--agent-kwarg",
        "--ek",
        "--environment-kwarg",
        "--environment-import-path",
        "--mounts-json",
        "--upload",
        "--export-push",
        "--public",
        "--private",
        "--share-org",
        "--share-user",
    }
)


def _option_values(command: Sequence[str], names: frozenset[str]) -> list[str]:
    """Collect separate or ``--name=value`` option values, failing if absent."""
    values: list[str] = []
    index = 2
    while index < len(command):
        argument = command[index]
        if argument in names:
            index += 1
            if index >= len(command):
                raise RuntimeError("secure launcher option is missing its value")
            values.append(command[index])
        else:
            for name in names:
                if name.startswith("--") and argument.startswith(f"{name}="):
                    values.append(argument.partition("=")[2])
                    break
        index += 1
    return values


def _claim_options(command: Sequence[str]) -> dict[str, list[str]]:
    """Parse only the finite, value-taking option surface used by the study."""
    parsed: dict[str, list[str]] = {}
    index = 2
    while index < len(command):
        argument = command[index]
        supplied_name, separator, attached_value = argument.partition("=")
        name = _ALLOWED_CLAIM_OPTION_ALIASES.get(supplied_name, supplied_name)
        if name not in _ALLOWED_CLAIM_OPTIONS:
            raise RuntimeError(
                "secure launcher rejects a noncanonical Harbor claim option"
            )
        if separator:
            value = attached_value
            index += 1
        else:
            index += 1
            if index >= len(command) or command[index].startswith("-"):
                raise RuntimeError("secure launcher option is missing its value")
            value = command[index]
            index += 1
        if not value:
            raise RuntimeError("secure launcher option is missing its value")
        parsed.setdefault(name, []).append(value)
    return parsed


class _PublicIntentReader(Protocol):
    """Anonymous GitHub reads required by the paid-launch preflight."""

    def get_repository(self) -> dict[str, Any]: ...

    def get_issue(self, issue_number: int) -> dict[str, Any]: ...

    def get_comment(self, comment_id: int) -> dict[str, Any]: ...

    def get_branch(self, branch: str) -> dict[str, Any]: ...

    def get_commit(self, commit_sha: str) -> dict[str, Any]: ...

    def get_content(self, path: str, commit_sha: str) -> bytes: ...

    def compare_commits(self, base_sha: str, head_sha: str) -> dict[str, Any]: ...

    def get_tree(self, commit_sha: str) -> dict[str, Any]: ...


class _ProviderKeyReader(Protocol):
    """Credentialed provider account read required immediately before spend."""

    def get_key(self, credential: str) -> dict[str, Any]: ...

    def get_key_record(self, credential: str, fingerprint: str) -> dict[str, Any]: ...

    def get_credits(self, credential: str) -> dict[str, Any]: ...


class _HostProbe(Protocol):
    """Injectable native-host probe used immediately before paid reservation."""

    def __call__(
        self, *, jobs_dir: Path, docker_executable: Path
    ) -> dict[str, Any]: ...


@dataclass(frozen=True)
class _VerifiedHostPreflight:
    """Exact public bytes and same-boot live probe captured before reservation."""

    public_report_raw: bytes
    public_commit: str
    public_fetched_at_utc: str
    live_recheck: Mapping[str, Any]
    jobs_dir: Path


class _PublicIntentReadError(RuntimeError):
    """Fail-closed wrapper for anonymous GitHub read failures."""


class _ProviderKeyReadError(RuntimeError):
    """Fail-closed wrapper that never includes provider response content."""


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Refuse redirects so authorization can never cross to another target."""

    def redirect_request(self, *_args: Any, **_kwargs: Any) -> None:
        return None


def _fixed_system_tls_context() -> ssl.SSLContext:
    """Build a public-root TLS context without ambient CA path overrides."""
    paths = ssl.get_default_verify_paths()
    cafile = (
        paths.openssl_cafile
        if paths.openssl_cafile and Path(paths.openssl_cafile).is_file()
        else None
    )
    capath = (
        paths.openssl_capath
        if paths.openssl_capath and Path(paths.openssl_capath).is_dir()
        else None
    )
    if cafile is None and capath is None:
        raise _PublicIntentReadError(
            "cannot locate the interpreter's compiled public CA roots"
        )
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.check_hostname = True
    context.verify_mode = ssl.CERT_REQUIRED
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.load_verify_locations(cafile=cafile, capath=capath)
    return context


def _object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("JSON object contains duplicate keys")
        value[key] = item
    return value


def _json_object(raw: bytes, *, label: str) -> dict[str, Any]:
    try:
        value = json.loads(
            raw.decode("utf-8"), object_pairs_hook=_object_without_duplicate_keys
        )
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as exc:
        raise RuntimeError(f"{label} is not strict UTF-8 JSON") from exc
    if not isinstance(value, dict):
        raise RuntimeError(f"{label} is not a JSON object")
    return value


class _AnonymousGitHubPublicIntentReader:
    """GET-only GitHub client with no auth, ambient proxies, or CA overrides."""

    def __init__(self) -> None:
        context = _fixed_system_tls_context()
        self._opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}),
            _NoRedirectHandler(),
            urllib.request.HTTPSHandler(context=context),
        )
        self._commit_cache: dict[str, dict[str, Any]] = {}

    def _read(self, url: str, *, accept: str) -> bytes:
        request = urllib.request.Request(
            url,
            headers={
                "Accept": accept,
                "User-Agent": "stella-harbor-public-intent-preflight/1",
                "X-GitHub-Api-Version": "2022-11-28",
            },
            method="GET",
        )
        if request.has_header("Authorization"):
            raise _PublicIntentReadError("anonymous GitHub request gained auth")
        try:
            with self._opener.open(request, timeout=30) as response:  # noqa: S310
                status = getattr(response, "status", response.getcode())
                final_url = response.geturl()
                raw = response.read(_MAX_GITHUB_RESPONSE_BYTES + 1)
        except (OSError, urllib.error.HTTPError, urllib.error.URLError) as exc:
            raise _PublicIntentReadError(f"anonymous GET failed for {url}") from exc
        if status != 200 or final_url != url:
            raise _PublicIntentReadError(
                "anonymous GitHub GET did not return the exact requested resource"
            )
        if len(raw) > _MAX_GITHUB_RESPONSE_BYTES:
            raise _PublicIntentReadError("anonymous GitHub response exceeded 8 MiB")
        return raw

    def _get_json(self, url: str) -> dict[str, Any]:
        return _json_object(
            self._read(url, accept="application/vnd.github+json"),
            label=f"GitHub response for {url}",
        )

    def get_repository(self) -> dict[str, Any]:
        return self._get_json(_FIXED_API_ROOT)

    def get_issue(self, issue_number: int) -> dict[str, Any]:
        return self._get_json(f"{_FIXED_API_ROOT}/issues/{issue_number}")

    def get_comment(self, comment_id: int) -> dict[str, Any]:
        return self._get_json(f"{_FIXED_API_ROOT}/issues/comments/{comment_id}")

    def get_branch(self, branch: str) -> dict[str, Any]:
        encoded_branch = urllib.parse.quote(branch, safe="")
        return self._get_json(f"{_FIXED_API_ROOT}/branches/{encoded_branch}")

    def get_commit(self, commit_sha: str) -> dict[str, Any]:
        cached = self._commit_cache.get(commit_sha)
        if cached is None:
            cached = self._get_json(f"{_FIXED_API_ROOT}/commits/{commit_sha}")
            self._commit_cache[commit_sha] = cached
        return dict(cached)

    def get_content(self, path: str, commit_sha: str) -> bytes:
        encoded_path = urllib.parse.quote(path, safe="/")
        encoded_ref = urllib.parse.quote(commit_sha, safe="")
        return self._read(
            f"{_FIXED_API_ROOT}/contents/{encoded_path}?ref={encoded_ref}",
            accept="application/vnd.github.raw+json",
        )

    def compare_commits(self, base_sha: str, head_sha: str) -> dict[str, Any]:
        encoded_base = urllib.parse.quote(base_sha, safe="")
        encoded_head = urllib.parse.quote(head_sha, safe="")
        return self._get_json(
            f"{_FIXED_API_ROOT}/compare/{encoded_base}...{encoded_head}"
        )

    def get_tree(self, commit_sha: str) -> dict[str, Any]:
        encoded_sha = urllib.parse.quote(commit_sha, safe="")
        return self._get_json(f"{_FIXED_API_ROOT}/git/trees/{encoded_sha}?recursive=1")


class _OpenRouterProviderKeyReader:
    """Read the current OpenRouter key controls without ambient proxy state."""

    def __init__(self) -> None:
        context = _fixed_system_tls_context()
        self._opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}),
            _NoRedirectHandler(),
            urllib.request.HTTPSHandler(context=context),
        )

    def _get(self, url: str, credential: str, *, label: str) -> dict[str, Any]:
        if not credential:
            raise _ProviderKeyReadError("provider key is empty")
        request = urllib.request.Request(
            url,
            headers={
                "Accept": "application/json",
                "Authorization": f"Bearer {credential}",
                "User-Agent": "stella-harbor-provider-budget-preflight/1",
            },
            method="GET",
        )
        try:
            with self._opener.open(request, timeout=30) as response:  # noqa: S310
                status = getattr(response, "status", response.getcode())
                final_url = response.geturl()
                raw = response.read(_MAX_GITHUB_RESPONSE_BYTES + 1)
        except (OSError, urllib.error.HTTPError, urllib.error.URLError) as exc:
            raise _ProviderKeyReadError("OpenRouter key-control GET failed") from exc
        if status != 200 or final_url != url:
            raise _ProviderKeyReadError(
                "OpenRouter key-control GET returned an unexpected response"
            )
        if len(raw) > _MAX_GITHUB_RESPONSE_BYTES:
            raise _ProviderKeyReadError(
                "OpenRouter key-control response exceeded 8 MiB"
            )
        try:
            return _json_object(raw, label=label)
        except RuntimeError as exc:
            raise _ProviderKeyReadError(
                "OpenRouter key-control response was invalid"
            ) from exc

    def get_key(self, credential: str) -> dict[str, Any]:
        return self._get(
            _OPENROUTER_KEY_URL,
            credential,
            label="OpenRouter key-control response",
        )

    def get_key_record(self, credential: str, fingerprint: str) -> dict[str, Any]:
        if re.fullmatch(r"[0-9a-f]{64}", fingerprint) is None:
            raise _ProviderKeyReadError("provider key fingerprint is invalid")
        return self._get(
            f"{_OPENROUTER_KEYS_URL}/{fingerprint}",
            credential,
            label="OpenRouter management key-record response",
        )

    def get_credits(self, credential: str) -> dict[str, Any]:
        return self._get(
            _OPENROUTER_CREDITS_URL,
            credential,
            label="OpenRouter credits response",
        )


def _canonical_payload_sha256(value: Any) -> str:
    raw = json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return hashlib.sha256(raw).hexdigest()


def _parse_github_timestamp(value: Any) -> datetime:
    if not isinstance(value, str) or not value.endswith("Z"):
        raise RuntimeError("GitHub comment timestamp is not canonical UTC")
    try:
        parsed = datetime.fromisoformat(value[:-1] + "+00:00")
    except ValueError as exc:
        raise RuntimeError("GitHub comment timestamp is invalid") from exc
    if parsed.tzinfo is None:
        raise RuntimeError("GitHub comment timestamp lacks a timezone")
    return parsed.astimezone(timezone.utc)


def _utc_text(value: datetime) -> str:
    if value.tzinfo is None:
        raise RuntimeError("public-intent preflight clock lacks a timezone")
    return (
        value.astimezone(timezone.utc)
        .isoformat(timespec="microseconds")
        .replace("+00:00", "Z")
    )


def _intent_comment_url(command: Sequence[str]) -> tuple[str, int, int]:
    values = _claim_options(command).get("--intent-comment-url", [])
    match = _COMMENT_URL_RE.fullmatch(values[0]) if len(values) == 1 else None
    if match is None:
        raise RuntimeError(
            "secure launcher requires exactly one fixed-repository --intent-comment-url"
        )
    return values[0], int(match.group("issue")), int(match.group("comment"))


def _paid_stage(command: Sequence[str]) -> str:
    job_name, _ = _claim_job_destination(command)
    if job_name == _READINESS_JOB_NAME:
        return "readiness"
    if job_name == _CALIBRATION_JOB_NAME:
        return "calibration"
    return "confirmatory"


def _aware_timestamp(value: Any, *, label: str) -> datetime:
    if not isinstance(value, str) or not value:
        raise RuntimeError(f"{label} is not a timezone-aware ISO-8601 timestamp")
    normalized = value[:-1] + "+00:00" if value.endswith("Z") else value
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError as exc:
        raise RuntimeError(
            f"{label} is not a timezone-aware ISO-8601 timestamp"
        ) from exc
    if parsed.tzinfo is None:
        raise RuntimeError(f"{label} is not a timezone-aware ISO-8601 timestamp")
    return parsed.astimezone(timezone.utc)


def _finite_nonnegative_number(value: Any, *, label: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise RuntimeError(f"{label} must be a finite nonnegative number")
    result = float(value)
    if not math.isfinite(result) or result < 0:
        raise RuntimeError(f"{label} must be a finite nonnegative number")
    return result


def _require_exact_object(
    value: Any, fields: frozenset[str], *, label: str
) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        raise RuntimeError(f"{label} differs from the exact v2 schema")
    return value


def _positive_sequence(value: Any, *, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise RuntimeError(f"{label} sequence must be a positive integer")
    return value


def _validate_run_ledger_schema(ledger: dict[str, Any]) -> None:
    _require_exact_object(ledger, _RUN_LEDGER_FIELDS, label="public run ledger")
    if (
        ledger.get("schema_version") != _RUN_LEDGER_SCHEMA
        or ledger.get("study_id") != _FIXED_STUDY_ID
        or ledger.get("ledger_path") != _FIXED_LEDGER_PATH
    ):
        raise RuntimeError("public run ledger identity is not frozen exactly")
    disclosure = _require_exact_object(
        ledger.get("historical_spend_disclosure"),
        _HISTORICAL_SPEND_FIELDS,
        label="historical spend disclosure",
    )
    if disclosure != {
        "known_lower_bound_usd": 0.2429614978,
        "unknown_cancellation_spend": True,
        "new_authorized_budget_usd": 200.0,
    }:
        raise RuntimeError("historical spend disclosure is not frozen exactly")

    schema_by_array = {
        "preregistrations": _PREREGISTRATION_FIELDS,
        "intents": _INTENT_WRAPPER_FIELDS,
        "publications": _PUBLICATION_FIELDS,
        "outcomes": _OUTCOME_FIELDS,
    }
    all_sequences: list[int] = []
    for array_name, fields in schema_by_array.items():
        records = ledger.get(array_name)
        if not isinstance(records, list):
            raise RuntimeError(f"public run ledger {array_name} is not an array")
        array_sequences: list[int] = []
        for record in records:
            exact = _require_exact_object(
                record, fields, label=f"run-ledger {array_name} record"
            )
            sequence = _positive_sequence(
                exact.get("sequence"), label=f"run-ledger {array_name}"
            )
            array_sequences.append(sequence)
            all_sequences.append(sequence)
        if array_sequences != sorted(array_sequences):
            raise RuntimeError(f"run-ledger {array_name} sequences are not ordered")
    if len(all_sequences) != len(set(all_sequences)):
        raise RuntimeError("run-ledger sequences are not globally unique")

    for preregistration in ledger["preregistrations"]:
        _aware_timestamp(
            preregistration.get("declared_at"), label="preregistration declared_at"
        )
        commit = preregistration.get("commit")
        if not isinstance(commit, str) or _SOURCE_COMMIT_RE.fullmatch(commit) is None:
            raise RuntimeError("preregistration commit is not one full lowercase SHA")

    for wrapper in ledger["intents"]:
        intent = _require_exact_object(
            wrapper.get("intent"), _INTENT_FIELDS, label="run-ledger intent"
        )
        _require_exact_object(
            intent.get("dataset"), _INTENT_DATASET_FIELDS, label="intent dataset"
        )
        _require_exact_object(
            intent.get("artifacts"),
            _INTENT_ARTIFACT_FIELDS,
            label="intent artifacts",
        )
        _require_exact_object(
            intent.get("execution"),
            _INTENT_EXECUTION_FIELDS,
            label="intent execution",
        )
        _require_exact_object(
            intent.get("provider_key"),
            _INTENT_PROVIDER_FIELDS,
            label="intent provider key",
        )
        digest = wrapper.get("intent_sha256")
        if (
            not isinstance(digest, str)
            or re.fullmatch(r"[0-9a-f]{64}", digest) is None
            or _canonical_payload_sha256(intent) != digest
        ):
            raise RuntimeError("run-ledger intent digest does not bind exact bytes")

    for publication in ledger["publications"]:
        _aware_timestamp(publication.get("published_at"), label="published_at")
    for outcome in ledger["outcomes"]:
        _aware_timestamp(outcome.get("recorded_at"), label="outcome recorded_at")


def _expected_stage_dataset(stage: str) -> dict[str, Any]:
    if stage == "readiness":
        return {
            "name": _READINESS_JOB_NAME,
            "ref": _READINESS_TASK_REF,
            "task_count": 1,
            "task_set_sha256": _READINESS_TASK_SET_SHA256,
        }
    if stage == "calibration":
        return {
            "name": "terminal-bench/terminal-bench-2-1",
            "ref": _CANONICAL_DATASET_ARGUMENT.partition("@")[2],
            "task_count": len(_CALIBRATION_TASK_FILTERS),
            "task_set_sha256": _CALIBRATION_TASK_SET_SHA256,
        }
    return {
        "name": "terminal-bench/terminal-bench-2-1",
        "ref": _CANONICAL_DATASET_ARGUMENT.partition("@")[2],
        "task_count": 89,
        "task_set_sha256": _CONFIRMATORY_TASK_SET_SHA256,
    }


def _validate_current_intent(
    intent: dict[str, Any],
    *,
    command: Sequence[str],
    subject_commit: str,
    runtime_identity: Mapping[str, Any],
) -> None:
    options = _claim_options(command)
    stage = _paid_stage(command)
    expected_requested = {"readiness": 1, "calibration": 60, "confirmatory": 445}[stage]
    expected_dataset = _expected_stage_dataset(stage)
    dataset = intent["dataset"]
    dataset_matches = all(
        dataset.get(key) == value for key, value in expected_dataset.items()
    )
    if not dataset_matches:
        raise RuntimeError("public paid intent dataset identity is not frozen exactly")
    if (
        intent.get("historical") is not False
        or intent.get("stage") != stage
        or not isinstance(intent.get("intent_id"), str)
        or not intent["intent_id"]
        or intent.get("job_name") != options["--job-name"][0]
        or intent.get("models") != options["--model"]
        or intent.get("requested_trials") != expected_requested
        or intent.get("attempts_per_task") != int(options["--n-attempts"][0])
        or intent.get("n_concurrent_trials") != int(options["--n-concurrent"][0])
        or intent.get("retry_max_retries") != int(options["--max-retries"][0])
        or intent.get("per_trial_budget_usd") != float(_CANONICAL_BUDGET)
        or intent.get("preregistration_commit") != subject_commit
    ):
        raise RuntimeError(
            "public paid intent does not exactly bind stage/job/models/trials/cost"
        )
    declared_at = _aware_timestamp(
        intent.get("declared_at"), label="intent declared_at"
    )

    artifacts = intent["artifacts"]
    expected_artifacts = {
        key: runtime_identity[key]
        for key in _INTENT_ARTIFACT_FIELDS
        if key != "engine_posture_sha256_by_model"
    }
    expected_artifacts["engine_posture_sha256_by_model"] = runtime_identity[
        "engine_posture_sha256_by_model"
    ]
    if artifacts != expected_artifacts:
        raise RuntimeError(
            "public paid intent artifacts differ from the validated runtime"
        )
    if intent["execution"] != {
        "base_url": runtime_identity["base_url"],
        "provider_route_policy": runtime_identity["provider_route_policy"],
        "disable_reflection": runtime_identity["disable_reflection"],
    }:
        raise RuntimeError(
            "public paid intent execution posture differs from the validated runtime"
        )
    provider = intent["provider_key"]
    if (
        provider.get("fingerprint_sha256")
        != runtime_identity["provider_key_fingerprint_sha256"]
        or provider.get("label") != _DEDICATED_KEY_LABEL
        or provider.get("limit_usd") != _DEDICATED_KEY_HARD_LIMIT_USD
    ):
        raise RuntimeError("public paid intent does not bind the actual dedicated key")
    _finite_nonnegative_number(
        provider.get("usage_before_usd"), label="intent provider usage_before_usd"
    )
    snapshot_at = _aware_timestamp(
        provider.get("snapshot_at"), label="intent provider snapshot_at"
    )
    if snapshot_at > declared_at:
        raise RuntimeError(
            "provider usage snapshot was recorded after intent declaration"
        )
    if (
        stage in {"readiness", "calibration"}
        and subject_commit != runtime_identity["source_commit"]
    ):
        raise RuntimeError(
            "readiness/calibration subject commit differs from runtime source commit"
        )


def _prior_stage_outcome(
    ledger: dict[str, Any],
    *,
    stage: str,
    current_digest: str,
    current_intent: dict[str, Any],
    publications_by_subject: Mapping[tuple[str, str], Mapping[str, Any]],
) -> dict[str, Any] | None:
    current_wrappers = [
        wrapper
        for wrapper in ledger["intents"]
        if wrapper["intent_sha256"] == current_digest
    ]
    if len(current_wrappers) != 1:
        raise RuntimeError("public ledger does not uniquely identify current intent")
    if any(
        outcome.get("intent_sha256") == current_digest for outcome in ledger["outcomes"]
    ):
        raise RuntimeError("current paid intent already has a post-launch outcome")

    expected_stages = {
        "readiness": ["readiness"],
        "calibration": ["readiness", "calibration"],
        "confirmatory": ["readiness", "calibration", "confirmatory"],
    }[stage]
    paid_wrappers = [
        wrapper
        for wrapper in ledger["intents"]
        if wrapper["intent"].get("historical") is False
        and wrapper["intent"].get("stage")
        in {"readiness", "calibration", "confirmatory"}
    ]
    if [wrapper["intent"].get("stage") for wrapper in paid_wrappers] != expected_stages:
        raise RuntimeError("public ledger paid-stage chain is not the exact prefix")
    if paid_wrappers[-1] != current_wrappers[0]:
        raise RuntimeError("public ledger current intent is not the paid-stage tip")

    current_sequence = current_wrappers[0]["sequence"]
    current_declared = _aware_timestamp(
        current_intent.get("declared_at"), label="current intent declared_at"
    )

    current_provider = current_intent["provider_key"]
    expected_key_identity = {
        "fingerprint_sha256": current_provider["fingerprint_sha256"],
        "label": _DEDICATED_KEY_LABEL,
        "limit_usd": _DEDICATED_KEY_HARD_LIMIT_USD,
    }
    prior_summary: dict[str, Any] | None = None
    previous_usage_after: float | None = None
    paid_digests = {wrapper["intent_sha256"] for wrapper in paid_wrappers}
    paid_outcomes = [
        outcome
        for outcome in ledger["outcomes"]
        if outcome.get("intent_sha256") in paid_digests
    ]
    if len(paid_outcomes) != len(paid_wrappers) - 1:
        raise RuntimeError(
            "public ledger paid outcomes do not exactly cover prior stages"
        )

    for index, wrapper in enumerate(paid_wrappers):
        intent = wrapper["intent"]
        intent_stage = intent["stage"]
        provider = intent["provider_key"]
        observed_key_identity = {
            "fingerprint_sha256": provider.get("fingerprint_sha256"),
            "label": provider.get("label"),
            "limit_usd": provider.get("limit_usd"),
        }
        if observed_key_identity != expected_key_identity:
            raise RuntimeError("all paid stages must bind one exact dedicated key")
        usage_before = _finite_nonnegative_number(
            provider.get("usage_before_usd"),
            label=f"{intent_stage} intent provider usage_before_usd",
        )
        if previous_usage_after is not None and not math.isclose(
            usage_before, previous_usage_after, rel_tol=0, abs_tol=1e-9
        ):
            raise RuntimeError("paid-stage provider usage is not continuous")
        if index == len(paid_wrappers) - 1:
            break

        matches = [
            outcome
            for outcome in paid_outcomes
            if outcome.get("intent_sha256") == wrapper["intent_sha256"]
        ]
        if len(matches) != 1:
            raise RuntimeError(
                f"public ledger lacks exactly one prior {intent_stage} outcome"
            )
        outcome = matches[0]
        try:
            parsed_job_id = uuid.UUID(str(outcome.get("job_id")))
        except (ValueError, AttributeError) as exc:
            raise RuntimeError(f"prior {intent_stage} job_id is not one UUID") from exc
        if parsed_job_id.int == 0 or str(parsed_job_id) != outcome.get("job_id"):
            raise RuntimeError(f"prior {intent_stage} job_id is not canonical")
        artifact_digest = outcome.get("artifact_tree_sha256")
        if (
            not isinstance(artifact_digest, str)
            or re.fullmatch(r"[0-9a-f]{64}", artifact_digest) is None
        ):
            raise RuntimeError(f"prior {intent_stage} artifact digest is invalid")

        before = _finite_nonnegative_number(
            outcome.get("provider_usage_before_usd"),
            label=f"prior {intent_stage} provider usage before",
        )
        after = _finite_nonnegative_number(
            outcome.get("provider_usage_after_usd"),
            label=f"prior {intent_stage} provider usage after",
        )
        delta = _finite_nonnegative_number(
            outcome.get("provider_usage_delta_usd"),
            label=f"prior {intent_stage} provider usage delta",
        )
        telemetry = _finite_nonnegative_number(
            outcome.get("telemetry_cost_sum_usd"),
            label=f"prior {intent_stage} telemetry sum",
        )
        tolerance = _finite_nonnegative_number(
            outcome.get("reconciliation_tolerance_usd"),
            label=f"prior {intent_stage} reconciliation tolerance",
        )
        if (
            not math.isclose(before, usage_before, rel_tol=0, abs_tol=1e-9)
            or after < before
            or not math.isclose(delta, after - before, rel_tol=0, abs_tol=1e-9)
            or tolerance > 0.01
            or abs(delta - telemetry) > tolerance + 1e-12
            or outcome.get("reconciliation_status") != "reconciled"
        ):
            raise RuntimeError(f"prior {intent_stage} spend is not reconciled")

        completed_at = _aware_timestamp(
            outcome.get("completed_at"), label=f"prior {intent_stage} completed_at"
        )
        started_at = _aware_timestamp(
            outcome.get("started_at"), label=f"prior {intent_stage} started_at"
        )
        recorded_at = _aware_timestamp(
            outcome.get("recorded_at"), label=f"prior {intent_stage} recorded_at"
        )
        intent_publication = publications_by_subject.get(
            ("intent", wrapper["intent_sha256"])
        )
        publication_sequence = (
            intent_publication.get("sequence")
            if isinstance(intent_publication, Mapping)
            else None
        )
        next_sequence = paid_wrappers[index + 1]["sequence"]
        expected_status = "excluded" if intent_stage == "readiness" else "complete"
        if (
            outcome.get("status") != expected_status
            or not isinstance(publication_sequence, int)
            or publication_sequence >= outcome["sequence"]
            or outcome["sequence"] >= next_sequence
            or started_at > completed_at
            or completed_at > recorded_at
            or recorded_at > current_declared
            or outcome["sequence"] >= current_sequence
        ):
            raise RuntimeError(
                f"prior {intent_stage} outcome was not completed and recorded"
            )
        previous_usage_after = after
        prior_summary = {
            "stage": intent_stage,
            "intent_sha256": wrapper["intent_sha256"],
            "status": outcome["status"],
            "completed_at": outcome["completed_at"],
            "recorded_at": outcome["recorded_at"],
        }

    if stage == "readiness":
        return None
    if prior_summary is None:
        raise RuntimeError("public ledger lacks a reconciled prior-stage outcome")
    return prior_summary


def _ledger_is_exact_prefix(
    snapshot: Mapping[str, Any], final: Mapping[str, Any]
) -> bool:
    """Return whether a publication ledger only appends to the snapshot arrays."""
    scalar_fields = _RUN_LEDGER_FIELDS - {
        "preregistrations",
        "intents",
        "publications",
        "outcomes",
    }
    if any(snapshot.get(name) != final.get(name) for name in scalar_fields):
        return False
    for name in ("preregistrations", "intents", "publications", "outcomes"):
        before = snapshot.get(name)
        after = final.get(name)
        if not isinstance(before, list) or not isinstance(after, list):
            return False
        if after[: len(before)] != before:
            return False
    return True


def _validate_stage_publications(
    *,
    snapshot: Mapping[str, Any],
    final: Mapping[str, Any],
    stage: str,
    current_digest: str,
    current_comment_created_at: str,
    current_ledger_commit: str,
) -> tuple[
    dict[tuple[str, str], dict[str, Any]],
    dict[str, Any],
]:
    if not _ledger_is_exact_prefix(snapshot, final):
        raise RuntimeError("publication ledger is not an exact append-only snapshot")
    if (
        len(final["preregistrations"]) != len(snapshot["preregistrations"])
        or len(final["intents"]) != len(snapshot["intents"])
        or len(final["outcomes"]) != len(snapshot["outcomes"])
        or len(final["publications"]) != len(snapshot["publications"]) + 1
    ):
        raise RuntimeError(
            "publication ledger must append only the current publication"
        )

    expected_stages = {
        "readiness": ["readiness"],
        "calibration": ["readiness", "calibration"],
        "confirmatory": ["readiness", "calibration", "confirmatory_freeze"],
    }[stage]
    paid_wrappers = [
        wrapper
        for wrapper in final["intents"]
        if wrapper["intent"].get("historical") is False
        and wrapper["intent"].get("stage")
        in {"readiness", "calibration", "confirmatory"}
    ]
    preregistrations = [
        item
        for item in final["preregistrations"]
        if item.get("kind") in expected_stages
    ]
    if [item.get("kind") for item in preregistrations] != expected_stages:
        raise RuntimeError(
            "public ledger preregistration chain is not the exact prefix"
        )

    by_subject: dict[tuple[str, str], dict[str, Any]] = {}
    for publication in final["publications"]:
        key = (publication.get("subject_type"), publication.get("subject_id"))
        if not all(isinstance(value, str) for value in key) or key in by_subject:
            raise RuntimeError("public ledger publication subjects are not unique")
        by_subject[key] = publication

    expected_subjects: set[tuple[str, str]] = {
        *(("preregistration", kind) for kind in expected_stages),
        *(("intent", wrapper["intent_sha256"]) for wrapper in paid_wrappers),
    }
    if set(by_subject) != expected_subjects:
        raise RuntimeError(
            "public ledger publications do not exactly cover paid stages"
        )

    prereg_by_kind = {item["kind"]: item for item in preregistrations}
    intent_by_stage = {wrapper["intent"]["stage"]: wrapper for wrapper in paid_wrappers}
    for kind in expected_stages:
        intent_stage = "confirmatory" if kind == "confirmatory_freeze" else kind
        prereg = prereg_by_kind[kind]
        wrapper = intent_by_stage[intent_stage]
        prereg_publication = by_subject[("preregistration", kind)]
        intent_publication = by_subject[("intent", wrapper["intent_sha256"])]
        for publication, subject_commit in (
            (prereg_publication, prereg["commit"]),
            (intent_publication, wrapper["intent"]["preregistration_commit"]),
        ):
            ledger_commit = publication.get("ledger_commit")
            if (
                not isinstance(ledger_commit, str)
                or _SOURCE_COMMIT_RE.fullmatch(ledger_commit) is None
                or ledger_commit == subject_commit
                or publication.get("public_url")
                != f"{_FIXED_WEB_ROOT}/commit/{ledger_commit}"
            ):
                raise RuntimeError("public ledger publication commit/URL is invalid")
        if not (
            prereg["sequence"]
            < prereg_publication["sequence"]
            < wrapper["sequence"]
            < intent_publication["sequence"]
        ):
            raise RuntimeError(
                "public ledger publication sequence is not chronological"
            )
        prereg_published = _aware_timestamp(
            prereg_publication.get("published_at"),
            label=f"{kind} preregistration published_at",
        )
        intent_declared = _aware_timestamp(
            wrapper["intent"].get("declared_at"),
            label=f"{intent_stage} intent declared_at",
        )
        if (
            prereg_published + timedelta(seconds=_PUBLICATION_SAFETY_MARGIN_SECONDS)
            > intent_declared
        ):
            raise RuntimeError("preregistration publication lacks the safety margin")

    current_publication = by_subject[("intent", current_digest)]
    if (
        current_publication.get("ledger_commit") != current_ledger_commit
        or current_publication.get("published_at") != current_comment_created_at
        or current_publication in snapshot["publications"]
    ):
        raise RuntimeError(
            "current intent publication does not match its GitHub comment"
        )
    current_kind = expected_stages[-1]
    return by_subject, by_subject[("preregistration", current_kind)]


def _validate_public_intent_ledger(
    snapshot_raw: bytes,
    publication_raw: bytes,
    *,
    command: Sequence[str],
    expected_digest: str,
    subject_commit: str,
    runtime_identity: Mapping[str, Any],
    current_comment_created_at: str,
    current_ledger_commit: str,
) -> tuple[
    dict[str, Any],
    dict[str, Any] | None,
    dict[str, Any],
    dict[str, Any],
]:
    snapshot = _json_object(snapshot_raw, label="public paid-intent ledger snapshot")
    ledger = _json_object(publication_raw, label="public publication ledger")
    _validate_run_ledger_schema(snapshot)
    _validate_run_ledger_schema(ledger)
    intents = snapshot["intents"]
    matches = [
        item
        for item in intents
        if isinstance(item, dict) and item.get("intent_sha256") == expected_digest
    ]
    if len(matches) != 1:
        raise RuntimeError(
            "public paid-intent ledger must contain the intent digest exactly once"
        )
    intent = matches[0]["intent"]
    _validate_current_intent(
        intent,
        command=command,
        subject_commit=subject_commit,
        runtime_identity=runtime_identity,
    )
    prereg_kind = {
        "readiness": "readiness",
        "calibration": "calibration",
        "confirmatory": "confirmatory_freeze",
    }[_paid_stage(command)]
    preregistrations = [
        item
        for item in ledger["preregistrations"]
        if item.get("kind") == prereg_kind and item.get("commit") == subject_commit
    ]
    if len(preregistrations) != 1:
        raise RuntimeError("public ledger does not contain the exact subject freeze")
    publications, current_preregistration_publication = _validate_stage_publications(
        snapshot=snapshot,
        final=ledger,
        stage=_paid_stage(command),
        current_digest=expected_digest,
        current_comment_created_at=current_comment_created_at,
        current_ledger_commit=current_ledger_commit,
    )
    prior = _prior_stage_outcome(
        ledger,
        stage=_paid_stage(command),
        current_digest=expected_digest,
        current_intent=intent,
        publications_by_subject=publications,
    )
    return intent, prior, ledger, current_preregistration_publication


def _validate_confirmatory_manifest(
    raw: bytes,
    *,
    ledger: Mapping[str, Any],
    subject_commit: str,
    intent: Mapping[str, Any],
    runtime_identity: Mapping[str, Any],
    prior_stage_outcome: Mapping[str, Any] | None,
) -> dict[str, Any]:
    preregistrations = [
        item
        for item in ledger["preregistrations"]
        if item.get("kind") == "confirmatory_freeze"
        and item.get("commit") == subject_commit
    ]
    if len(preregistrations) != 1:
        raise RuntimeError("confirmatory freeze record is not unique")
    manifest_sha256 = hashlib.sha256(raw).hexdigest()
    if preregistrations[0].get("study_manifest_sha256") != manifest_sha256:
        raise RuntimeError("confirmatory freeze does not bind exact manifest bytes")
    manifest = _json_object(raw, label="confirmatory study manifest")
    _require_exact_object(manifest, _MANIFEST_FIELDS, label="confirmatory manifest")
    preregistration = _require_exact_object(
        manifest.get("preregistration"),
        _MANIFEST_PREREGISTRATION_FIELDS,
        label="confirmatory manifest preregistration",
    )
    sut = _require_exact_object(
        manifest.get("sut"), _MANIFEST_SUT_FIELDS, label="confirmatory manifest sut"
    )
    analysis = _require_exact_object(
        manifest.get("analysis"),
        _MANIFEST_ANALYSIS_FIELDS,
        label="confirmatory manifest analysis",
    )
    dataset = _require_exact_object(
        manifest.get("dataset"),
        _MANIFEST_DATASET_FIELDS,
        label="confirmatory manifest dataset",
    )
    design = _require_exact_object(
        manifest.get("design"),
        _MANIFEST_DESIGN_FIELDS,
        label="confirmatory manifest design",
    )
    harbor = _require_exact_object(
        manifest.get("harbor"),
        _MANIFEST_HARBOR_FIELDS,
        label="confirmatory manifest Harbor",
    )
    comparator = _require_exact_object(
        manifest.get("comparator"),
        _MANIFEST_COMPARATOR_FIELDS,
        label="confirmatory manifest comparator",
    )
    calibration = _require_exact_object(
        manifest.get("calibration"),
        _MANIFEST_CALIBRATION_FIELDS,
        label="confirmatory manifest calibration",
    )
    confirmatory = _require_exact_object(
        manifest.get("confirmatory"),
        _MANIFEST_CONFIRMATORY_FIELDS,
        label="confirmatory manifest run",
    )
    model = intent["models"][0]
    prior_stage = (
        prior_stage_outcome.get("stage")
        if isinstance(prior_stage_outcome, Mapping)
        else None
    )
    if prior_stage != "calibration" or model != _PRIMARY_MODEL:
        raise RuntimeError("secure launcher permits only the fixed GLM-5.1 primary")

    prereg_by_kind = {
        item["kind"]: item
        for item in ledger["preregistrations"]
        if item.get("kind") in {"readiness", "calibration"}
    }
    if set(prereg_by_kind) != {"readiness", "calibration"}:
        raise RuntimeError("manifest lacks exact readiness/calibration freezes")
    expected_preregistration = {
        "study_id": _FIXED_STUDY_ID,
        "run_ledger_path": _FIXED_LEDGER_PATH,
        "readiness_commit": prereg_by_kind["readiness"]["commit"],
        "calibration_commit": prereg_by_kind["calibration"]["commit"],
    }

    package = _current_adapter_module()
    posture_function = getattr(package, "_benchmark_engine_posture", None)
    if not callable(posture_function):
        raise RuntimeError("adapter engine posture constructor is unavailable")
    primary_posture = posture_function(model)
    expected_sut = {
        "model": model,
        "allowed_call_models": [model.removeprefix("openrouter/")],
        "binary_sha256": runtime_identity["binary_sha256"],
        "source_commit": runtime_identity["source_commit"],
        "source_commit_embedded": True,
        "agent_version": runtime_identity["agent_version"],
        "adapter_version": runtime_identity["adapter_version"],
        "adapter_sha256": runtime_identity["adapter_sha256"],
        "budget_usd": float(_CANONICAL_BUDGET),
        "disable_reflection": True,
        "base_url": runtime_identity["base_url"],
        "provider_route_policy": runtime_identity["provider_route_policy"],
        "host_credential_source": "anonymous-seekable-fd-v1",
        "host_credential_name": "OPENROUTER_API_KEY",
        "host_credential_bundle_count": 1,
        "engine_posture_version": "stella-tb21-engine-posture-v1",
        "engine_posture": primary_posture[0],
        "engine_posture_sha256": primary_posture[2],
    }
    expected_analysis = {
        "sha256": runtime_identity["analysis_sha256"],
        "public_timing_sha256": runtime_identity["public_timing_sha256"],
    }
    expected_dataset = {
        "name": intent["dataset"]["name"],
        "ref": intent["dataset"]["ref"],
        "task_set_sha256": intent["dataset"]["task_set_sha256"],
        **_CANONICAL_HARBOR_DATASET_SETTINGS,
    }
    expected_harbor = {
        "version": runtime_identity["harbor_version"],
        "sha256": runtime_identity["harbor_sha256"],
        **_CANONICAL_HARBOR_SETTINGS,
        **_CANONICAL_HARBOR_JOB_SETTINGS,
    }
    if (
        manifest.get("schema_version") != "stella-tb21-study-manifest-v6"
        or preregistration != expected_preregistration
        or sut != expected_sut
        or analysis != expected_analysis
        or dataset != expected_dataset
        or design != {"tasks": 89, "attempts_per_task": 5}
        or harbor != expected_harbor
        or comparator != _CANONICAL_COMPARATOR
        or confirmatory != {"job_name": intent["job_name"], "n_concurrent_trials": 1}
    ):
        raise RuntimeError("confirmatory manifest differs from exact analyzer contract")

    expected_engine_postures: dict[str, Any] = {}
    for candidate in _CANDIDATE_MODELS:
        posture = posture_function(candidate)
        expected_engine_postures[candidate] = {
            "version": "stella-tb21-engine-posture-v1",
            "posture": posture[0],
            "sha256": posture[2],
        }
    expected_calibration_fixed = {
        "seed": 20260721,
        "tasks": list(_CALIBRATION_TASKS),
        "model_order": list(_CANDIDATE_MODELS),
        "call_models_by_config": {
            candidate: [candidate.removeprefix("openrouter/")]
            for candidate in _CANDIDATE_MODELS
        },
        "engine_postures_by_config": expected_engine_postures,
        "job_name": _CALIBRATION_JOB_NAME,
        "attempts_per_model_task": 2,
        "n_concurrent_trials": 3,
        "minimum_passes": 14,
        "projection_trials": 445,
        "projected_spend_limit_usd": 75.0,
    }
    if any(
        calibration.get(key) != value
        for key, value in expected_calibration_fixed.items()
    ):
        raise RuntimeError(
            "manifest calibration settings differ from analyzer contract"
        )
    for candidate in expected_engine_postures:
        _require_exact_object(
            calibration["engine_postures_by_config"].get(candidate),
            _ENGINE_POSTURE_RECORD_FIELDS,
            label=f"manifest calibration posture {candidate}",
        )

    calibration_wrappers = [
        wrapper
        for wrapper in ledger["intents"]
        if wrapper["intent"].get("historical") is False
        and wrapper["intent"].get("stage") == "calibration"
    ]
    if len(calibration_wrappers) != 1:
        raise RuntimeError("manifest cannot bind one calibration outcome")
    calibration_outcomes = [
        outcome
        for outcome in ledger["outcomes"]
        if outcome.get("intent_sha256") == calibration_wrappers[0]["intent_sha256"]
    ]
    readiness_wrappers = [
        wrapper
        for wrapper in ledger["intents"]
        if wrapper["intent"].get("historical") is False
        and wrapper["intent"].get("stage") == "readiness"
    ]
    readiness_outcomes = [
        outcome
        for outcome in ledger["outcomes"]
        if len(readiness_wrappers) == 1
        and outcome.get("intent_sha256") == readiness_wrappers[0]["intent_sha256"]
    ]
    selected_model = calibration.get("selected_model")
    excluded_job_ids = calibration.get("excluded_job_ids")
    expected_excluded_job_ids = set(_REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS)
    if len(readiness_outcomes) == 1:
        expected_excluded_job_ids.add(readiness_outcomes[0].get("job_id"))
    if (
        len(calibration_outcomes) != 1
        or len(readiness_wrappers) != 1
        or len(readiness_outcomes) != 1
        or calibration.get("job_id") != calibration_outcomes[0].get("job_id")
        or selected_model not in _CANDIDATE_MODELS
        or not isinstance(excluded_job_ids, list)
        or not all(isinstance(job_id, str) for job_id in excluded_job_ids)
        or len(excluded_job_ids) != len(set(excluded_job_ids))
        or set(excluded_job_ids) != expected_excluded_job_ids
        or re.fullmatch(r"[0-9a-f]{64}", str(calibration.get("trial_data_sha256")))
        is None
        or re.fullmatch(r"[0-9a-f]{64}", str(calibration.get("excluded_ledger_sha256")))
        is None
    ):
        raise RuntimeError("manifest calibration evidence fields are invalid")
    return manifest


def _load_frozen_replay_analyzer(runtime_identity: Mapping[str, Any]) -> ModuleType:
    """Execute the exact source-bound analyzer bytes without consulting ``.pyc``."""
    repository_root = Path(__file__).resolve().parents[3]
    analyzer_path = repository_root / _FIXED_ANALYZER_PATH
    timing_path = repository_root / _FIXED_PUBLIC_TIMING_PATH
    try:
        analyzer_raw = analyzer_path.read_bytes()
        timing_raw = timing_path.read_bytes()
    except OSError as exc:
        raise RuntimeError(
            "prior-stage replay cannot read the frozen analyzer"
        ) from exc
    analyzer_sha256 = hashlib.sha256(analyzer_raw).hexdigest()
    timing_sha256 = hashlib.sha256(timing_raw).hexdigest()
    if analyzer_sha256 != runtime_identity.get(
        "analysis_sha256"
    ) or timing_sha256 != runtime_identity.get("public_timing_sha256"):
        raise RuntimeError("prior-stage replay analyzer bytes drifted")

    timing_module = ModuleType("github_public_timing")
    timing_module.__file__ = str(timing_path)
    timing_module.__package__ = ""
    analyzer_module = ModuleType("_stella_tb21_paid_stage_replay")
    analyzer_module.__file__ = str(analyzer_path)
    analyzer_module.__package__ = ""
    previous_timing = sys.modules.get("github_public_timing")
    previous_analyzer = sys.modules.get(analyzer_module.__name__)
    try:
        sys.modules["github_public_timing"] = timing_module
        exec(  # noqa: S102 - exact verified source bytes are the replay authority
            compile(timing_raw, str(timing_path), "exec"), timing_module.__dict__
        )
        sys.modules[analyzer_module.__name__] = analyzer_module
        exec(  # noqa: S102 - exact verified source bytes are the replay authority
            compile(analyzer_raw, str(analyzer_path), "exec"),
            analyzer_module.__dict__,
        )
    except Exception as exc:
        raise RuntimeError("prior-stage replay analyzer could not be loaded") from exc
    finally:
        if previous_timing is None:
            sys.modules.pop("github_public_timing", None)
        else:
            sys.modules["github_public_timing"] = previous_timing
        if previous_analyzer is None:
            sys.modules.pop(analyzer_module.__name__, None)
        else:
            sys.modules[analyzer_module.__name__] = previous_analyzer

    required_callables = (
        "ingest_job",
        "_job_evidence",
        "_readiness_harbor_reasons",
        "_trial_telemetry_reasons",
        "_launch_receipt_reasons",
        "_validate_calibration",
        "_aware_timestamp",
        "_harbor_timestamp",
        "_number",
        "_nonnegative_int",
    )
    if (
        analyzer_module.__dict__.get("ANALYSIS_CONTENT_SHA256") != analyzer_sha256
        or analyzer_module.__dict__.get("PUBLIC_TIMING_CONTENT_SHA256") != timing_sha256
        or analyzer_module.__dict__.get("READINESS_JOB_NAME") != _READINESS_JOB_NAME
        or analyzer_module.__dict__.get("CALIBRATION_JOB_NAME") != _CALIBRATION_JOB_NAME
        or tuple(analyzer_module.__dict__.get("CALIBRATION_MODEL_ORDER", ()))
        != _CANDIDATE_MODELS
        or tuple(analyzer_module.__dict__.get("CALIBRATION_TASKS", ()))
        != _CALIBRATION_TASKS
        or tuple(
            analyzer_module.__dict__.get("REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS", ())
        )
        != _REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS
        or any(
            not callable(analyzer_module.__dict__.get(name))
            for name in required_callables
        )
    ):
        raise RuntimeError("prior-stage replay analyzer contract drifted")
    if (
        hashlib.sha256(analyzer_path.read_bytes()).hexdigest() != analyzer_sha256
        or hashlib.sha256(timing_path.read_bytes()).hexdigest() != timing_sha256
    ):
        raise RuntimeError("prior-stage replay analyzer changed while loading")
    return analyzer_module


def _fixed_prior_jobs_root(command: Sequence[str]) -> Path:
    """Resolve the one canonical owner-only jobs root shared by every paid stage."""
    values = _claim_options(command).get("--jobs-dir", [])
    if len(values) != 1:
        raise RuntimeError("prior-stage replay requires one fixed --jobs-dir")
    supplied = Path(values[0])
    try:
        resolved = supplied.resolve(strict=True)
        info = supplied.lstat()
    except OSError as exc:
        raise RuntimeError("prior-stage replay jobs-dir does not exist") from exc
    if (
        supplied != resolved
        or not stat.S_ISDIR(info.st_mode)
        or info.st_uid != os.getuid()
        or stat.S_IMODE(info.st_mode) & 0o077
    ):
        raise RuntimeError(
            "prior-stage replay jobs-dir must be canonical and owner-only"
        )
    return resolved


def _canonical_prior_job_dir(jobs_root: Path, job_name: str) -> Path:
    candidate = jobs_root / job_name
    try:
        resolved = candidate.resolve(strict=True)
        info = candidate.lstat()
    except OSError as exc:
        raise RuntimeError(f"prior Harbor job is missing: {job_name}") from exc
    if (
        resolved != candidate
        or resolved.parent != jobs_root
        or not stat.S_ISDIR(info.st_mode)
    ):
        raise RuntimeError(f"prior Harbor job is not canonical: {job_name}")
    return resolved


def _historical_outcomes_by_job_id(
    ledger: Mapping[str, Any],
) -> dict[str, Mapping[str, Any]]:
    wrappers = [
        wrapper
        for wrapper in ledger["intents"]
        if wrapper["intent"].get("stage") == "historical_excluded"
    ]
    if len(wrappers) != len(_REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS):
        raise RuntimeError("public ledger lacks the exact historical excluded intents")
    outcomes_by_digest: dict[str, list[Mapping[str, Any]]] = {}
    for outcome in ledger["outcomes"]:
        digest = outcome.get("intent_sha256")
        if isinstance(digest, str):
            outcomes_by_digest.setdefault(digest, []).append(outcome)

    by_job_id: dict[str, Mapping[str, Any]] = {}
    nullable_intent_fields = (
        "job_name",
        "requested_trials",
        "attempts_per_task",
        "n_concurrent_trials",
        "retry_max_retries",
        "per_trial_budget_usd",
        "preregistration_commit",
    )
    nullable_outcome_fields = _OUTCOME_FIELDS - {
        "sequence",
        "intent_sha256",
        "job_id",
        "status",
        "reconciliation_status",
        "recorded_at",
    }
    for wrapper in wrappers:
        intent = wrapper["intent"]
        digest = wrapper["intent_sha256"]
        matches = outcomes_by_digest.get(digest, [])
        if (
            intent.get("historical") is not True
            or intent.get("models") != []
            or any(intent.get(field) is not None for field in nullable_intent_fields)
            or any(
                not isinstance(intent.get(field), dict)
                or any(value is not None for value in intent[field].values())
                for field in ("dataset", "artifacts", "execution", "provider_key")
            )
            or len(matches) != 1
        ):
            raise RuntimeError("historical excluded ledger record is not exact")
        outcome = matches[0]
        job_id = outcome.get("job_id")
        if (
            not isinstance(job_id, str)
            or job_id in by_job_id
            or outcome.get("status") != "historical_excluded"
            or outcome.get("reconciliation_status") != "unavailable"
            or any(outcome.get(field) is not None for field in nullable_outcome_fields)
        ):
            raise RuntimeError("historical excluded outcome is not exact")
        by_job_id[job_id] = outcome
    if set(by_job_id) != set(_REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS):
        raise RuntimeError("historical excluded outcomes are not the frozen five jobs")
    return by_job_id


def _find_historical_job_dirs(
    jobs_root: Path, expected_job_ids: set[str]
) -> dict[str, Path]:
    matches: dict[str, Path] = {}
    try:
        children = sorted(jobs_root.iterdir(), key=lambda path: path.name)
    except OSError as exc:
        raise RuntimeError("cannot enumerate prior Harbor jobs") from exc
    for candidate in children:
        if candidate.is_symlink() or not candidate.is_dir():
            continue
        result_path = candidate / "result.json"
        if not result_path.is_file() or result_path.is_symlink():
            continue
        try:
            payload = _json_object(
                result_path.read_bytes(), label=f"historical result {result_path}"
            )
        except (OSError, RuntimeError) as exc:
            raise RuntimeError("cannot read a prior Harbor root result") from exc
        job_id = payload.get("id")
        if job_id not in expected_job_ids:
            continue
        resolved = _canonical_prior_job_dir(jobs_root, candidate.name)
        if job_id in matches:
            raise RuntimeError("historical Harbor job ID appears more than once")
        matches[job_id] = resolved
    if set(matches) != expected_job_ids:
        missing = sorted(expected_job_ids - set(matches))
        raise RuntimeError(f"historical Harbor jobs are missing: {missing!r}")
    return matches


def _ingest_prior_job(
    analyzer: ModuleType,
    job_dir: Path,
    *,
    expected_job_id: str,
    reject_warnings: bool,
) -> list[dict[str, Any]]:
    try:
        rows, warnings = analyzer.ingest_job(job_dir)
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        raise RuntimeError(f"cannot ingest prior Harbor job {job_dir.name}") from exc
    if reject_warnings and warnings:
        raise RuntimeError(
            f"prior Harbor job {job_dir.name} has ingestion warnings: {warnings!r}"
        )
    if (
        not rows
        or {row.get("source_input") for row in rows} != {str(job_dir)}
        or {row.get("job_id") for row in rows} != {expected_job_id}
        or {row.get("job_jobs_dir") for row in rows} != {str(job_dir.parent)}
    ):
        raise RuntimeError(f"prior Harbor job {job_dir.name} identity differs")
    return rows


def _paid_stage_record(
    ledger: Mapping[str, Any], stage: str
) -> tuple[Mapping[str, Any], str, Mapping[str, Any], Mapping[str, Any]]:
    wrappers = [
        wrapper
        for wrapper in ledger["intents"]
        if wrapper["intent"].get("historical") is False
        and wrapper["intent"].get("stage") == stage
    ]
    if len(wrappers) != 1:
        raise RuntimeError(f"public ledger lacks one prior {stage} intent")
    wrapper = wrappers[0]
    digest = wrapper["intent_sha256"]
    outcomes = [
        outcome
        for outcome in ledger["outcomes"]
        if outcome.get("intent_sha256") == digest
    ]
    publications = [
        publication
        for publication in ledger["publications"]
        if publication.get("subject_type") == "intent"
        and publication.get("subject_id") == digest
    ]
    if len(outcomes) != 1 or len(publications) != 1:
        raise RuntimeError(f"public ledger lacks exact prior {stage} evidence")
    return wrapper["intent"], digest, outcomes[0], publications[0]


def _runtime_identity_from_intent(intent: Mapping[str, Any]) -> dict[str, Any]:
    artifacts = intent["artifacts"]
    execution = intent["execution"]
    provider = intent["provider_key"]
    return {
        **{field: artifacts[field] for field in _INTENT_ARTIFACT_FIELDS},
        **{field: execution[field] for field in _INTENT_EXECUTION_FIELDS},
        "provider_key_fingerprint_sha256": provider["fingerprint_sha256"],
    }


def _prior_outcome_summary(
    stage: str, digest: str, outcome: Mapping[str, Any]
) -> dict[str, Any]:
    return {
        "stage": stage,
        "intent_sha256": digest,
        "status": outcome.get("status"),
        "completed_at": outcome.get("completed_at"),
        "recorded_at": outcome.get("recorded_at"),
    }


def _prior_public_intent_expectation(
    *,
    stage: str,
    intent: Mapping[str, Any],
    digest: str,
    publication: Mapping[str, Any],
    prior_stage_outcome: Mapping[str, Any] | None,
) -> dict[str, Any]:
    requested = _finite_nonnegative_number(
        intent.get("requested_trials"), label=f"prior {stage} requested trials"
    )
    budget = _finite_nonnegative_number(
        intent.get("per_trial_budget_usd"), label=f"prior {stage} trial budget"
    )
    return {
        "kind": stage,
        "subject_commit": intent.get("preregistration_commit"),
        "ledger_commit": publication.get("ledger_commit"),
        "runtime_identity": _runtime_identity_from_intent(intent),
        "provider_key": dict(intent["provider_key"]),
        "prior_stage_outcome": prior_stage_outcome,
        "projected_spend_usd": requested * budget,
        "intent_sha256": digest,
    }


def _prior_outcome_evidence_reasons(
    analyzer: ModuleType,
    *,
    stage: str,
    rows: Sequence[dict[str, Any]],
    intent: Mapping[str, Any],
    outcome: Mapping[str, Any],
) -> list[str]:
    reasons: list[str] = []
    evidence = analyzer._job_evidence(rows)
    reasons.extend(evidence["temporal_reasons"])
    if stage == "calibration":
        reasons.extend(evidence["task_identity_reasons"])
    row = rows[0]
    expected_status = "excluded" if stage == "readiness" else "complete"
    if not evidence["attempted"]:
        reasons.append(f"prior {stage} job has no attempted trials")
    if {item.get("job_name") for item in rows} != {intent.get("job_name")}:
        reasons.append(f"prior {stage} job name differs from its public intent")
    if outcome.get("status") != expected_status:
        reasons.append(f"prior {stage} outcome status is not {expected_status}")
    if analyzer._aware_timestamp(
        outcome.get("started_at")
    ) != analyzer._harbor_timestamp(
        evidence["started_at"], row
    ) or analyzer._aware_timestamp(
        outcome.get("completed_at")
    ) != analyzer._harbor_timestamp(evidence["completed_at"], row):
        reasons.append(f"prior {stage} outcome timestamps differ from Harbor")
    if (
        evidence["artifact_hash_count"] != 1
        or outcome.get("artifact_tree_sha256") != evidence["artifact_tree_sha256"]
    ):
        reasons.append(f"prior {stage} artifact-tree digest differs from Harbor")
    telemetry = evidence["cost_sum"]
    recorded_telemetry = outcome.get("telemetry_cost_sum_usd")
    if (
        telemetry is None
        or isinstance(recorded_telemetry, bool)
        or not isinstance(recorded_telemetry, (int, float))
        or not math.isclose(
            float(recorded_telemetry), float(telemetry), rel_tol=0, abs_tol=1e-9
        )
    ):
        reasons.append(f"prior {stage} telemetry cost differs from Harbor")
    return reasons


def _readiness_replay_reasons(
    analyzer: ModuleType,
    *,
    rows: Sequence[dict[str, Any]],
    intent: Mapping[str, Any],
    digest: str,
    outcome: Mapping[str, Any],
    publication: Mapping[str, Any],
) -> list[str]:
    reasons = _prior_outcome_evidence_reasons(
        analyzer, stage="readiness", rows=rows, intent=intent, outcome=outcome
    )
    expected_model = _CANDIDATE_MODELS[0]
    if (
        intent.get("job_name") != _READINESS_JOB_NAME
        or intent.get("models") != [expected_model]
        or intent.get("dataset") != _expected_stage_dataset("readiness")
        or intent.get("requested_trials") != 1
        or intent.get("attempts_per_task") != 1
        or intent.get("n_concurrent_trials") != 1
        or intent.get("retry_max_retries") != 0
    ):
        reasons.append("prior readiness intent does not match the frozen stage")
    if len(rows) != 1:
        return [*reasons, "prior readiness job is not exactly one trial"]
    row = rows[0]
    reasons.extend(analyzer._readiness_harbor_reasons(row))
    reasons.extend(
        analyzer._trial_telemetry_reasons(
            row,
            allowed_call_models=[expected_model.removeprefix("openrouter/")],
            label="Prior readiness trial",
        )
    )
    posture_function = _current_adapter_module()._benchmark_engine_posture  # type: ignore[attr-defined]
    posture, posture_json, posture_sha256 = posture_function(expected_model)
    del posture
    exact = {
        "requested": True,
        "instantiated": True,
        "attempted": True,
        "task": "synthetic-adapter-sentinel",
        "task_name": "stella/synthetic-adapter-sentinel",
        "task_ref": None,
        "task_checksum": _READINESS_TASK_REF.removeprefix("sha256:"),
        "model": expected_model,
        "engine_posture_version": "stella-tb21-engine-posture-v1",
        "engine_posture_json": posture_json,
        "engine_posture_record_json": posture_json,
        "engine_posture_sha256": posture_sha256,
        "atif_engine_posture_version": "stella-tb21-engine-posture-v1",
        "atif_engine_posture_json": posture_json,
        "atif_engine_posture_record_json": posture_json,
        "atif_engine_posture_sha256": posture_sha256,
        "atif_valid": True,
        "container_credential_absence_verified": True,
        "atif_container_credential_absence_verified": True,
        "status": "completed",
        "exception_type": None,
        "stream_terminal_event": "complete",
        "stella_return_code": 0,
    }
    for field, expected in exact.items():
        if row.get(field) != expected:
            reasons.append(f"prior readiness {field} differs from the frozen evidence")
    if (
        analyzer._number(row.get("reward")) != 1.0
        or analyzer._number(row.get("accuracy_value")) != 1.0
    ):
        reasons.append("prior readiness reward and accuracy are not exactly one")
    artifacts = intent["artifacts"]
    execution = intent["execution"]
    row_identity = {
        "binary_sha256": artifacts.get("binary_sha256"),
        "source_commit": artifacts.get("source_commit"),
        "agent_info_version": artifacts.get("agent_version"),
        "adapter_version": artifacts.get("adapter_version"),
        "adapter_sha256": artifacts.get("adapter_sha256"),
        "harbor_version": artifacts.get("harbor_version"),
        "harbor_sha256": artifacts.get("harbor_sha256"),
        "budget_usd": intent.get("per_trial_budget_usd"),
        "disable_reflection": execution.get("disable_reflection"),
        "base_url": execution.get("base_url"),
        "provider_route_policy": execution.get("provider_route_policy"),
        "source_commit_verified_in_binary": True,
    }
    for field, expected in row_identity.items():
        if row.get(field) != expected:
            reasons.append(f"prior readiness {field} differs from its public intent")
    expectation = _prior_public_intent_expectation(
        stage="readiness",
        intent=intent,
        digest=digest,
        publication=publication,
        prior_stage_outcome=None,
    )
    reasons.extend(
        analyzer._launch_receipt_reasons(
            row,
            expected_job_name=_READINESS_JOB_NAME,
            expected_models=[expected_model],
            expected_intent_sha256=digest,
            expected_kind="readiness",
            expected_subject_commit=intent.get("preregistration_commit"),
            expected_ledger_commit=publication.get("ledger_commit"),
            expected_runtime_identity=expectation["runtime_identity"],
            expected_provider_key=expectation["provider_key"],
            expected_prior_stage_outcome=None,
            expected_projected_spend_usd=expectation["projected_spend_usd"],
            label="Prior readiness Harbor job",
        )
    )
    return reasons


def _calibration_replay_reasons(
    analyzer: ModuleType,
    *,
    manifest: Mapping[str, Any],
    rows: Sequence[dict[str, Any]],
    excluded_rows: Sequence[dict[str, Any]],
    intent: Mapping[str, Any],
    digest: str,
    outcome: Mapping[str, Any],
    publication: Mapping[str, Any],
    readiness_digest: str,
    readiness_outcome: Mapping[str, Any],
) -> list[str]:
    reasons = _prior_outcome_evidence_reasons(
        analyzer, stage="calibration", rows=rows, intent=intent, outcome=outcome
    )
    sut = manifest["sut"]
    analysis = manifest["analysis"]
    harbor = manifest["harbor"]
    expected_artifacts = {
        "binary_sha256": sut["binary_sha256"],
        "source_commit": sut["source_commit"],
        "agent_version": sut["agent_version"],
        "adapter_version": sut["adapter_version"],
        "adapter_sha256": sut["adapter_sha256"],
        "analysis_sha256": analysis["sha256"],
        "public_timing_sha256": analysis["public_timing_sha256"],
        "harbor_version": harbor["version"],
        "harbor_sha256": harbor["sha256"],
        "engine_posture_sha256_by_model": {
            model: _current_adapter_module()._benchmark_engine_posture(model)[2]  # type: ignore[attr-defined]
            for model in _CANDIDATE_MODELS
        },
    }
    expected_execution = {
        "base_url": sut["base_url"],
        "provider_route_policy": sut["provider_route_policy"],
        "disable_reflection": sut["disable_reflection"],
    }
    if (
        intent.get("job_name") != _CALIBRATION_JOB_NAME
        or intent.get("models") != list(_CANDIDATE_MODELS)
        or intent.get("dataset") != _expected_stage_dataset("calibration")
        or intent.get("requested_trials") != 60
        or intent.get("attempts_per_task") != 2
        or intent.get("n_concurrent_trials") != 3
        or intent.get("retry_max_retries") != 0
        or intent.get("per_trial_budget_usd") != sut["budget_usd"]
        or intent.get("artifacts") != expected_artifacts
        or intent.get("execution") != expected_execution
        or intent.get("preregistration_commit")
        != manifest["preregistration"]["calibration_commit"]
    ):
        reasons.append("prior calibration intent does not match the frozen manifest")
    expectation = _prior_public_intent_expectation(
        stage="calibration",
        intent=intent,
        digest=digest,
        publication=publication,
        prior_stage_outcome=_prior_outcome_summary(
            "readiness", readiness_digest, readiness_outcome
        ),
    )
    structural_reasons: list[str] = []
    analyzer._validate_calibration(
        dict(manifest),
        rows,
        excluded_rows,
        input_job_count=1,
        binary_sha256=sut["binary_sha256"],
        source_commit=sut["source_commit"],
        agent_version=sut["agent_version"],
        adapter_version=sut["adapter_version"],
        adapter_sha256=sut["adapter_sha256"],
        budget_usd=sut["budget_usd"],
        disable_reflection=sut["disable_reflection"],
        base_url=sut["base_url"],
        provider_route_policy=sut["provider_route_policy"],
        harbor_version=harbor["version"],
        harbor_sha256=harbor["sha256"],
        intent_sha256_by_job_id={str(outcome["job_id"]): digest},
        public_intent_expectation_by_job_id={str(outcome["job_id"]): expectation},
        structural_reasons=structural_reasons,
        reasons=reasons,
    )
    return [*structural_reasons, *reasons]


def _replay_prior_stage_evidence(
    command: Sequence[str],
    *,
    ledger: Mapping[str, Any],
    manifest: Mapping[str, Any] | None,
    runtime_identity: Mapping[str, Any],
) -> None:
    """Recompute prior Harbor evidence before the next paid-stage reservation."""
    stage = _paid_stage(command)
    jobs_root = _fixed_prior_jobs_root(command)
    if stage == "readiness":
        return
    analyzer = _load_frozen_replay_analyzer(runtime_identity)
    readiness_intent, readiness_digest, readiness_outcome, readiness_publication = (
        _paid_stage_record(ledger, "readiness")
    )
    readiness_job_id = str(readiness_outcome.get("job_id"))
    readiness_dir = _canonical_prior_job_dir(jobs_root, _READINESS_JOB_NAME)
    readiness_rows = _ingest_prior_job(
        analyzer,
        readiness_dir,
        expected_job_id=readiness_job_id,
        reject_warnings=True,
    )
    reasons = _readiness_replay_reasons(
        analyzer,
        rows=readiness_rows,
        intent=readiness_intent,
        digest=readiness_digest,
        outcome=readiness_outcome,
        publication=readiness_publication,
    )
    if reasons:
        raise RuntimeError(f"prior readiness replay failed: {reasons!r}")
    if stage == "calibration":
        return
    if manifest is None:
        raise RuntimeError("confirmatory prior-stage replay lacks the frozen manifest")

    historical = _historical_outcomes_by_job_id(ledger)
    historical_dirs = _find_historical_job_dirs(jobs_root, set(historical))
    historical_rows: list[dict[str, Any]] = []
    for job_id in _REQUIRED_EXCLUDED_CALIBRATION_JOB_IDS:
        historical_rows.extend(
            _ingest_prior_job(
                analyzer,
                historical_dirs[job_id],
                expected_job_id=job_id,
                reject_warnings=False,
            )
        )

    calibration_intent, calibration_digest, calibration_outcome, publication = (
        _paid_stage_record(ledger, "calibration")
    )
    calibration_job_id = str(calibration_outcome.get("job_id"))
    calibration_dir = _canonical_prior_job_dir(jobs_root, _CALIBRATION_JOB_NAME)
    calibration_rows = _ingest_prior_job(
        analyzer,
        calibration_dir,
        expected_job_id=calibration_job_id,
        reject_warnings=True,
    )
    reasons = _calibration_replay_reasons(
        analyzer,
        manifest=manifest,
        rows=calibration_rows,
        excluded_rows=[*readiness_rows, *historical_rows],
        intent=calibration_intent,
        digest=calibration_digest,
        outcome=calibration_outcome,
        publication=publication,
        readiness_digest=readiness_digest,
        readiness_outcome=readiness_outcome,
    )
    if reasons:
        raise RuntimeError(f"prior calibration replay failed: {reasons!r}")


def _validate_public_comment_record(
    comment: Mapping[str, Any],
    *,
    comment_url: str,
    issue_number: int,
    comment_id: int,
) -> str:
    user = comment.get("user")
    body = comment.get("body")
    if (
        comment.get("id") != comment_id
        or comment.get("url") != f"{_FIXED_API_ROOT}/issues/comments/{comment_id}"
        or comment.get("html_url") != comment_url
        or comment.get("issue_url") != f"{_FIXED_API_ROOT}/issues/{issue_number}"
        or not isinstance(user, dict)
        or user.get("login") != "macanderson"
        or comment.get("author_association") != "OWNER"
        or not isinstance(body, str)
    ):
        raise RuntimeError(
            "public intent comment is not the exact owner-authored issue comment"
        )
    if comment.get("created_at") != comment.get("updated_at"):
        raise RuntimeError("public intent comment was edited after creation")
    _parse_github_timestamp(comment.get("created_at"))
    return body


def _verify_commit_identity_and_ancestry(
    reader: _PublicIntentReader,
    *,
    subject_commit: str,
    ledger_commit: str,
    source_commit: str,
) -> None:
    for commit_sha in dict.fromkeys((subject_commit, ledger_commit, source_commit)):
        try:
            record = reader.get_commit(commit_sha)
        except Exception as exc:
            raise RuntimeError(
                "a bound public commit is not anonymously readable"
            ) from exc
        if (
            record.get("sha") != commit_sha
            or record.get("url") != f"{_FIXED_API_ROOT}/commits/{commit_sha}"
            or record.get("html_url") != f"{_FIXED_WEB_ROOT}/commit/{commit_sha}"
        ):
            raise RuntimeError("a bound public commit has the wrong API identity")

    def require_descendant(base: str, head: str, *, label: str) -> None:
        try:
            comparison = reader.compare_commits(base, head)
        except Exception as exc:
            raise RuntimeError(f"public {label} ancestry GET failed") from exc
        commits = comparison.get("commits")
        last_sha = (
            commits[-1].get("sha")
            if isinstance(commits, list) and commits and isinstance(commits[-1], dict)
            else None
        )
        if not (
            comparison.get("status") == "ahead"
            and isinstance(comparison.get("ahead_by"), int)
            and not isinstance(comparison.get("ahead_by"), bool)
            and comparison["ahead_by"] > 0
            and isinstance(comparison.get("base_commit"), dict)
            and comparison["base_commit"].get("sha") == base
            and isinstance(comparison.get("merge_base_commit"), dict)
            and comparison["merge_base_commit"].get("sha") == base
            and last_sha == head
        ):
            raise RuntimeError(f"GitHub compare did not prove strict {label} ancestry")

    if source_commit != subject_commit:
        require_descendant(source_commit, subject_commit, label="source-to-subject")
    require_descendant(subject_commit, ledger_commit, label="subject-to-ledger")


def _reject_public_credential(raw: bytes, credential: str, *, label: str) -> None:
    """Reject an exact raw credential appearing in any public evidence bytes."""
    encoded = credential.encode("utf-8")
    if encoded and encoded in raw:
        raise RuntimeError(f"{label} contains the live provider credential")


def _reject_public_credentials(
    raw: bytes, credentials: Sequence[str], *, label: str
) -> None:
    for credential in credentials:
        _reject_public_credential(raw, credential, label=label)


def _canonical_source_tree_digest(files: Mapping[str, bytes], *, domain: str) -> str:
    digest = hashlib.sha256()
    digest.update(domain.encode("utf-8") + b"\0")
    if not files:
        raise RuntimeError("public source tree contains no Python sources")
    for relative, content in sorted(files.items()):
        relative_bytes = relative.encode("utf-8")
        digest.update(len(relative_bytes).to_bytes(8, "big"))
        digest.update(relative_bytes)
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


def _verify_public_runtime_sources(
    reader: _PublicIntentReader,
    *,
    source_commit: str,
    subject_commit: str,
    runtime_identity: Mapping[str, Any],
    forbidden_credentials: Sequence[str],
) -> None:
    """Recompute the public adapter tree and compare every frozen source byte."""
    repository_root = Path(__file__).resolve().parents[3]
    adapter_prefix = "bench/harbor_adapter/stella_harbor/"
    expected_adapter_paths = set(_FIXED_ADAPTER_SOURCE_PATHS)
    expected_readiness_paths = set(_FIXED_READINESS_SOURCE_PATHS)
    fixed_paths = (
        _FIXED_ANALYZER_PATH,
        _FIXED_PUBLIC_TIMING_PATH,
        _FIXED_PROTOCOL_PATH,
        *_FIXED_READINESS_SOURCE_PATHS,
    )
    local_bytes: dict[str, bytes] = {}
    for relative in (*_FIXED_ADAPTER_SOURCE_PATHS, *fixed_paths):
        path = repository_root / relative
        try:
            resolved = path.resolve(strict=True)
            info = path.lstat()
        except OSError as exc:
            raise RuntimeError(
                f"cannot resolve frozen local source {relative}"
            ) from exc
        if resolved != path or not stat.S_ISREG(info.st_mode):
            raise RuntimeError(f"frozen local source {relative} is not canonical")
        local_bytes[relative] = path.read_bytes()

    for commit_sha in dict.fromkeys((source_commit, subject_commit)):
        try:
            commit_record = reader.get_commit(commit_sha)
        except Exception as exc:
            raise RuntimeError("public source commit is unreadable") from exc
        commit_payload = commit_record.get("commit")
        tree_payload = (
            commit_payload.get("tree") if isinstance(commit_payload, Mapping) else None
        )
        tree_sha = (
            tree_payload.get("sha") if isinstance(tree_payload, Mapping) else None
        )
        if (
            commit_record.get("sha") != commit_sha
            or not isinstance(tree_sha, str)
            or _SOURCE_COMMIT_RE.fullmatch(tree_sha) is None
        ):
            raise RuntimeError("public source commit does not bind one Git tree")
        try:
            tree = reader.get_tree(tree_sha)
        except Exception as exc:
            raise RuntimeError("public recursive source tree is unreadable") from exc
        entries = tree.get("tree")
        if tree.get("truncated") is not False or not isinstance(entries, list):
            raise RuntimeError("public recursive source tree is truncated or invalid")
        paths: dict[str, Mapping[str, Any]] = {}
        for entry in entries:
            if not isinstance(entry, Mapping):
                raise RuntimeError(
                    "public recursive source tree contains an invalid entry"
                )
            path = entry.get("path")
            if isinstance(path, str):
                if path in paths:
                    raise RuntimeError("public recursive source tree repeats a path")
                paths[path] = entry

        public_adapter_paths = {
            path
            for path in paths
            if path.startswith(adapter_prefix) and path.endswith(".py")
        }
        public_readiness_paths = {
            path
            for path in paths
            if path.startswith("bench/readiness/synthetic-adapter-sentinel/")
            and paths[path].get("type") == "blob"
        }
        if public_adapter_paths != expected_adapter_paths:
            raise RuntimeError(
                "public adapter Python tree differs from the frozen tree"
            )
        if public_readiness_paths != expected_readiness_paths:
            raise RuntimeError(
                "public readiness fixture tree differs from the frozen tree"
            )

        adapter_files: dict[str, bytes] = {}
        for relative in (*_FIXED_ADAPTER_SOURCE_PATHS, *fixed_paths):
            entry = paths.get(relative)
            if (
                not isinstance(entry, Mapping)
                or entry.get("type") != "blob"
                or entry.get("mode") not in {"100644", "100755"}
            ):
                raise RuntimeError(
                    f"public frozen source {relative} is not a regular blob"
                )
            try:
                raw = reader.get_content(relative, commit_sha)
            except Exception as exc:
                raise RuntimeError(
                    f"public frozen source {relative} is unreadable"
                ) from exc
            _reject_public_credentials(
                raw,
                forbidden_credentials,
                label=f"public frozen source {relative}",
            )
            if raw != local_bytes[relative]:
                raise RuntimeError(
                    f"public frozen source {relative} differs from local runtime bytes"
                )
            if relative in expected_adapter_paths:
                adapter_files[relative.removeprefix(adapter_prefix)] = raw

        public_adapter_sha256 = _canonical_source_tree_digest(
            adapter_files, domain="stella-harbor-adapter-source-v1"
        )
        if public_adapter_sha256 != runtime_identity["adapter_sha256"]:
            raise RuntimeError("public adapter tree hash differs from runtime identity")

        if (
            hashlib.sha256(local_bytes[_FIXED_ANALYZER_PATH]).hexdigest()
            != runtime_identity["analysis_sha256"]
            or hashlib.sha256(local_bytes[_FIXED_PUBLIC_TIMING_PATH]).hexdigest()
            != runtime_identity["public_timing_sha256"]
        ):
            raise RuntimeError("public analysis bytes differ from runtime identity")


def _validate_live_provider_key(
    response: Mapping[str, Any],
    key_record_response: Mapping[str, Any],
    credits_response: Mapping[str, Any],
    *,
    intent: Mapping[str, Any],
    runtime_identity: Mapping[str, Any],
    fetched_at: datetime,
) -> dict[str, Any]:
    outer = _require_exact_object(
        response, frozenset({"data"}), label="OpenRouter key-control response"
    )
    data = outer.get("data")
    required_key_fields = frozenset(
        {
            "is_management_key",
            "is_provisioning_key",
            "limit",
            "limit_remaining",
            "limit_reset",
            "usage",
        }
    )
    if (
        not isinstance(data, dict)
        or not required_key_fields.issubset(data)
        or data.get("is_management_key") is not False
        or data.get("is_provisioning_key") is not False
        or data.get("limit_reset") is not None
    ):
        raise RuntimeError(
            "OpenRouter benchmark credential must be a normal dedicated hard-limit key"
        )
    provider = intent["provider_key"]
    live_limit = _finite_nonnegative_number(data.get("limit"), label="live key limit")
    live_usage = _finite_nonnegative_number(data.get("usage"), label="live key usage")
    live_remaining = _finite_nonnegative_number(
        data.get("limit_remaining"), label="live key limit_remaining"
    )
    intended_limit = _finite_nonnegative_number(
        provider.get("limit_usd"), label="intent key limit"
    )
    intended_usage = _finite_nonnegative_number(
        provider.get("usage_before_usd"), label="intent key usage"
    )
    record_outer = _require_exact_object(
        key_record_response,
        frozenset({"data"}),
        label="OpenRouter management key-record response",
    )
    record = record_outer.get("data")
    required_record_fields = frozenset(
        {
            "disabled",
            "hash",
            "include_byok_in_limit",
            "limit",
            "limit_remaining",
            "limit_reset",
            "name",
            "usage",
        }
    )
    if not isinstance(record, dict) or not required_record_fields.issubset(record):
        raise RuntimeError("OpenRouter management key record lacks required fields")
    record_limit = _finite_nonnegative_number(
        record.get("limit"), label="management key record limit"
    )
    record_usage = _finite_nonnegative_number(
        record.get("usage"), label="management key record usage"
    )
    record_remaining = _finite_nonnegative_number(
        record.get("limit_remaining"),
        label="management key record limit_remaining",
    )
    label = record.get("name")
    if (
        record.get("hash") != runtime_identity["provider_key_fingerprint_sha256"]
        or label != _DEDICATED_KEY_LABEL
        or label != provider.get("label")
        or record.get("disabled") is not False
        or record.get("include_byok_in_limit") is not True
        or record.get("limit_reset") is not None
        or live_limit != intended_limit
        or live_limit != _DEDICATED_KEY_HARD_LIMIT_USD
        or record_limit != live_limit
        or not math.isclose(live_usage, intended_usage, rel_tol=0, abs_tol=1e-9)
        or not math.isclose(record_usage, live_usage, rel_tol=0, abs_tol=1e-9)
        or not math.isclose(record_remaining, live_remaining, rel_tol=0, abs_tol=1e-6)
        or not math.isclose(
            live_remaining, live_limit - live_usage, rel_tol=0, abs_tol=1e-6
        )
    ):
        raise RuntimeError("live OpenRouter management key record differs from intent")
    projected = float(intent["requested_trials"]) * float(
        intent["per_trial_budget_usd"]
    )
    projected_remaining = live_remaining - projected
    if projected_remaining < -1e-9:
        raise RuntimeError("live OpenRouter hard limit cannot cover the registered job")
    credits_outer = _require_exact_object(
        credits_response,
        frozenset({"data"}),
        label="OpenRouter credits response",
    )
    credits_data = _require_exact_object(
        credits_outer.get("data"),
        frozenset({"total_credits", "total_usage"}),
        label="OpenRouter credits data",
    )
    total_credits = _finite_nonnegative_number(
        credits_data.get("total_credits"), label="OpenRouter total credits"
    )
    total_usage = _finite_nonnegative_number(
        credits_data.get("total_usage"), label="OpenRouter total usage"
    )
    available = total_credits - total_usage
    if available < -1e-6 or available + 1e-9 < projected:
        raise RuntimeError("OpenRouter account credit cannot cover the registered job")
    return {
        "fingerprint_sha256": runtime_identity["provider_key_fingerprint_sha256"],
        "label": label,
        "limit_usd": live_limit,
        "usage_usd": live_usage,
        "limit_remaining_usd": live_remaining,
        "nominal_planned_spend_usd": projected,
        "nominal_remaining_after_usd": projected_remaining,
        "total_credits_usd": total_credits,
        "total_usage_usd": total_usage,
        "available_credits_usd": available,
        "fetched_at_utc": _utc_text(fetched_at),
    }


def _verify_public_intent(
    command: Sequence[str],
    *,
    runtime_identity: Mapping[str, Any],
    runtime_revalidator: Callable[[], Mapping[str, Any]],
    provider_credential: str,
    management_credential: str,
    reader: _PublicIntentReader | None = None,
    provider_key_reader: _ProviderKeyReader | None = None,
    clock: Callable[[], datetime] | None = None,
    sleeper: Callable[[float], None] = time.sleep,
) -> dict[str, Any]:
    """Anonymously verify the public paid intent immediately before launch."""
    source = reader or _AnonymousGitHubPublicIntentReader()
    provider_source = provider_key_reader or _OpenRouterProviderKeyReader()
    now = clock or (lambda: datetime.now(timezone.utc))
    forbidden_credentials = (provider_credential, management_credential)
    if any(not credential for credential in forbidden_credentials) or len(
        set(forbidden_credentials)
    ) != len(forbidden_credentials):
        raise RuntimeError(
            "provider control credentials must be non-empty and distinct"
        )
    if set(runtime_identity) != RUNTIME_IDENTITY_FIELDS:
        raise RuntimeError("secure launcher runtime identity fields drifted")
    comment_url, issue_number, comment_id = _intent_comment_url(command)
    expected_digest = _intent_sha256(command)
    stage = _paid_stage(command)
    try:
        repository = source.get_repository()
        initial_comment = source.get_comment(comment_id)
        publication_branch = source.get_branch("main")
    except Exception as exc:
        raise RuntimeError("anonymous public-intent GitHub GET failed") from exc
    if (
        repository.get("full_name") != _FIXED_REPOSITORY
        or repository.get("url") != _FIXED_API_ROOT
        or repository.get("html_url") != _FIXED_WEB_ROOT
        or repository.get("private") is not False
        or repository.get("default_branch") != "main"
    ):
        raise RuntimeError(
            "fixed Stella repository is not public and anonymously readable"
        )
    initial_body = _validate_public_comment_record(
        initial_comment,
        comment_url=comment_url,
        issue_number=issue_number,
        comment_id=comment_id,
    )
    _reject_public_credentials(
        initial_body.encode("utf-8"),
        forbidden_credentials,
        label="public intent comment",
    )
    body = _json_object(initial_body.encode("utf-8"), label="public intent comment")
    body_fields = {
        "schema_version",
        "study_id",
        "subject_type",
        "subject_id",
        "kind",
        "subject_commit",
        "ledger_commit",
        "ledger_path",
        "intent_sha256",
    }
    subject_commit = body.get("subject_commit")
    ledger_commit = body.get("ledger_commit")
    expected_body = {
        "schema_version": _GITHUB_ATTESTATION_SCHEMA,
        "study_id": _FIXED_STUDY_ID,
        "subject_type": "intent",
        "subject_id": expected_digest,
        "kind": stage,
        "subject_commit": subject_commit,
        "ledger_commit": ledger_commit,
        "ledger_path": _FIXED_LEDGER_PATH,
        "intent_sha256": expected_digest,
    }
    if (
        set(body) != body_fields
        or body != expected_body
        or not isinstance(subject_commit, str)
        or _SOURCE_COMMIT_RE.fullmatch(subject_commit) is None
        or not isinstance(ledger_commit, str)
        or _SOURCE_COMMIT_RE.fullmatch(ledger_commit) is None
        or subject_commit == ledger_commit
    ):
        raise RuntimeError(
            "public intent comment body does not exactly bind the paid intent"
        )

    branch_commit = publication_branch.get("commit")
    publication_commit = (
        branch_commit.get("sha") if isinstance(branch_commit, Mapping) else None
    )
    if (
        publication_branch.get("name") != "main"
        or not isinstance(publication_commit, str)
        or _SOURCE_COMMIT_RE.fullmatch(publication_commit) is None
        or publication_commit in {subject_commit, ledger_commit}
    ):
        raise RuntimeError(
            "public main branch does not expose a later publication commit"
        )

    try:
        issue = source.get_issue(issue_number)
        ledger_raw = source.get_content(_FIXED_LEDGER_PATH, ledger_commit)
        publication_ledger_raw = source.get_content(
            _FIXED_LEDGER_PATH, publication_commit
        )
    except Exception as exc:
        raise RuntimeError("anonymous public-intent GitHub GET failed") from exc
    _reject_public_credentials(
        ledger_raw, forbidden_credentials, label="public intent ledger snapshot"
    )
    _reject_public_credentials(
        publication_ledger_raw,
        forbidden_credentials,
        label="public publication ledger",
    )
    issue_user = issue.get("user")
    issue_url = f"{_FIXED_WEB_ROOT}/issues/{issue_number}"
    issue_title = f"Stella Terminal-Bench 2.1 preregistration: {_FIXED_STUDY_ID}"
    if (
        issue.get("number") != issue_number
        or issue.get("url") != f"{_FIXED_API_ROOT}/issues/{issue_number}"
        or issue.get("html_url") != issue_url
        or issue.get("repository_url") != _FIXED_API_ROOT
        or issue.get("title") != issue_title
        or "pull_request" in issue
        or not isinstance(issue_user, dict)
        or issue_user.get("login") != "macanderson"
        or issue.get("author_association") != "OWNER"
    ):
        raise RuntimeError("public intent URL is not the dedicated owner issue")
    created_at = initial_comment.get("created_at")
    if not isinstance(created_at, str):
        raise RuntimeError("public intent comment lacks a server timestamp")
    created_time = _parse_github_timestamp(created_at)
    intent, prior_stage_outcome, ledger, preregistration_publication = (
        _validate_public_intent_ledger(
            ledger_raw,
            publication_ledger_raw,
            command=command,
            expected_digest=expected_digest,
            subject_commit=subject_commit,
            runtime_identity=runtime_identity,
            current_comment_created_at=created_at,
            current_ledger_commit=ledger_commit,
        )
    )
    manifest: dict[str, Any] | None = None
    if stage == "confirmatory":
        try:
            manifest_raw = source.get_content(_FIXED_MANIFEST_PATH, subject_commit)
        except Exception as exc:
            raise RuntimeError("confirmatory freeze manifest is not public") from exc
        _reject_public_credentials(
            manifest_raw,
            forbidden_credentials,
            label="public confirmatory manifest",
        )
        manifest = _validate_confirmatory_manifest(
            manifest_raw,
            ledger=ledger,
            subject_commit=subject_commit,
            intent=intent,
            runtime_identity=runtime_identity,
            prior_stage_outcome=prior_stage_outcome,
        )
    if (
        _aware_timestamp(intent.get("declared_at"), label="intent declared_at")
        > created_time
        or _aware_timestamp(
            intent["provider_key"].get("snapshot_at"),
            label="provider snapshot_at",
        )
        > created_time
    ):
        raise RuntimeError(
            "public intent chronology precedes neither declaration nor snapshot"
        )
    _verify_commit_identity_and_ancestry(
        source,
        subject_commit=subject_commit,
        ledger_commit=ledger_commit,
        source_commit=str(runtime_identity["source_commit"]),
    )
    _verify_commit_identity_and_ancestry(
        source,
        subject_commit=ledger_commit,
        ledger_commit=publication_commit,
        source_commit=ledger_commit,
    )
    preregistration_ledger_commit = preregistration_publication.get("ledger_commit")
    if not isinstance(preregistration_ledger_commit, str):
        raise RuntimeError("current preregistration publication lacks a ledger commit")
    _verify_commit_identity_and_ancestry(
        source,
        subject_commit=subject_commit,
        ledger_commit=preregistration_ledger_commit,
        source_commit=subject_commit,
    )
    try:
        preregistration_ledger_raw = source.get_content(
            _FIXED_LEDGER_PATH, preregistration_ledger_commit
        )
    except Exception as exc:
        raise RuntimeError(
            "current preregistration ledger snapshot is unreadable"
        ) from exc
    _reject_public_credentials(
        preregistration_ledger_raw,
        forbidden_credentials,
        label="public preregistration ledger snapshot",
    )
    preregistration_ledger = _json_object(
        preregistration_ledger_raw,
        label="public preregistration ledger snapshot",
    )
    _validate_run_ledger_schema(preregistration_ledger)
    current_preregistration_kind = {
        "readiness": "readiness",
        "calibration": "calibration",
        "confirmatory": "confirmatory_freeze",
    }[stage]
    bound_preregistrations = [
        item
        for item in preregistration_ledger["preregistrations"]
        if item.get("kind") == current_preregistration_kind
        and item.get("commit") == subject_commit
    ]
    if (
        len(bound_preregistrations) != 1
        or not _ledger_is_exact_prefix(preregistration_ledger, ledger)
        or preregistration_publication in preregistration_ledger["publications"]
    ):
        raise RuntimeError(
            "current preregistration publication does not bind an exact ledger snapshot"
        )
    _verify_public_runtime_sources(
        source,
        source_commit=str(runtime_identity["source_commit"]),
        subject_commit=subject_commit,
        runtime_identity=runtime_identity,
        forbidden_credentials=forbidden_credentials,
    )

    safety_target = created_time + timedelta(seconds=_PUBLICATION_SAFETY_MARGIN_SECONDS)
    before_wait = now()
    if before_wait.tzinfo is None:
        raise RuntimeError("public-intent preflight clock lacks a timezone")
    wait_seconds = max(
        0.0,
        (safety_target - before_wait.astimezone(timezone.utc)).total_seconds(),
    )
    if wait_seconds > _MAX_CLOCK_WAIT_SECONDS:
        raise RuntimeError("local clock is too far behind GitHub's server timestamp")
    if wait_seconds:
        sleeper(wait_seconds)
    wait_completed = now()
    if (
        wait_completed.tzinfo is None
        or wait_completed.astimezone(timezone.utc) < safety_target
    ):
        raise RuntimeError("public intent safety wait did not reach two seconds")

    try:
        final_comment = source.get_comment(comment_id)
        final_publication_branch = source.get_branch("main")
    except Exception as exc:
        raise RuntimeError("final anonymous public-intent GitHub GET failed") from exc
    final_get_completed = now()
    if final_get_completed.tzinfo is None or final_get_completed.astimezone(
        timezone.utc
    ) < wait_completed.astimezone(timezone.utc):
        raise RuntimeError("public-intent clock rolled back after the safety wait")
    final_body = _validate_public_comment_record(
        final_comment,
        comment_url=comment_url,
        issue_number=issue_number,
        comment_id=comment_id,
    )
    if final_body != initial_body or any(
        final_comment.get(field) != initial_comment.get(field)
        for field in (
            "id",
            "url",
            "html_url",
            "issue_url",
            "created_at",
            "updated_at",
            "author_association",
        )
    ):
        raise RuntimeError("public intent comment changed during launch preflight")
    final_branch_commit = final_publication_branch.get("commit")
    final_publication_commit = (
        final_branch_commit.get("sha")
        if isinstance(final_branch_commit, Mapping)
        else None
    )
    if (
        final_publication_branch.get("name") != "main"
        or final_publication_commit != publication_commit
    ):
        raise RuntimeError("public publication commit changed during launch preflight")

    try:
        provider_response = provider_source.get_key(provider_credential)
        fingerprint = str(runtime_identity["provider_key_fingerprint_sha256"])
        key_record_response = provider_source.get_key_record(
            management_credential, fingerprint
        )
        credits_response = provider_source.get_credits(management_credential)
    except Exception as exc:
        raise RuntimeError("live provider key-control preflight failed") from exc
    provider_fetched_at = now()
    if provider_fetched_at.tzinfo is None or provider_fetched_at.astimezone(
        timezone.utc
    ) < final_get_completed.astimezone(timezone.utc):
        raise RuntimeError("provider clock rolled back after final GitHub GET")
    provider_snapshot = _validate_live_provider_key(
        provider_response,
        key_record_response,
        credits_response,
        intent=intent,
        runtime_identity=runtime_identity,
        fetched_at=provider_fetched_at,
    )
    revalidated_identity = runtime_revalidator()
    runtime_revalidated_at = now()
    if (
        runtime_revalidated_at.tzinfo is None
        or runtime_revalidated_at.astimezone(timezone.utc)
        < provider_fetched_at.astimezone(timezone.utc)
        or dict(revalidated_identity) != dict(runtime_identity)
    ):
        raise RuntimeError("validated runtime changed after the final GitHub GET")
    _replay_prior_stage_evidence(
        command,
        ledger=ledger,
        manifest=manifest,
        runtime_identity=runtime_identity,
    )

    attestation = {
        "schema_version": _PUBLIC_INTENT_ATTESTATION_SCHEMA,
        "verification_mode": "anonymous-get-v1",
        "repository": _FIXED_REPOSITORY,
        "repository_private": False,
        "issue_number": issue_number,
        "issue_url": issue_url,
        "issue_title": issue_title,
        "issue_author_login": "macanderson",
        "issue_author_association": "OWNER",
        "comment_id": comment_id,
        "comment_url": comment_url,
        "comment_author_login": "macanderson",
        "comment_author_association": "OWNER",
        "server_created_at": created_at,
        "server_updated_at": final_comment.get("updated_at"),
        "body_sha256": hashlib.sha256(final_body.encode("utf-8")).hexdigest(),
        "github_attestation_schema_version": _GITHUB_ATTESTATION_SCHEMA,
        "study_id": _FIXED_STUDY_ID,
        "subject_type": "intent",
        "subject_id": expected_digest,
        "kind": stage,
        "subject_commit": subject_commit,
        "ledger_commit": ledger_commit,
        "ledger_path": _FIXED_LEDGER_PATH,
        "intent_sha256": expected_digest,
        "safety_margin_seconds": _PUBLICATION_SAFETY_MARGIN_SECONDS,
        "safety_wait_completed_at_utc": _utc_text(wait_completed),
        "final_comment_get_completed_at_utc": _utc_text(final_get_completed),
        "ledger_sha256": hashlib.sha256(ledger_raw).hexdigest(),
        "subject_commit_verified": True,
        "ledger_commit_verified": True,
        "source_commit_verified": True,
        "strict_ancestry_verified": True,
        "prior_stage_outcome": prior_stage_outcome,
        "runtime_identity": dict(runtime_identity),
        "provider_key_live_snapshot": provider_snapshot,
        "runtime_revalidated_after_final_get": True,
        "runtime_revalidated_at_utc": _utc_text(runtime_revalidated_at),
    }
    if set(attestation) != PUBLIC_INTENT_ATTESTATION_FIELDS:
        raise RuntimeError("public intent receipt attestation fields drifted")
    return attestation


def _verify_public_host_preflight(
    command: Sequence[str],
    *,
    public_intent_attestation: Mapping[str, Any],
    forbidden_credentials: Sequence[str],
    docker_executable: Path,
    reader: _PublicIntentReader,
    host_probe: _HostProbe = probe_host,
    clock: Callable[[], datetime] | None = None,
) -> _VerifiedHostPreflight:
    """Fetch immutable host evidence and prove a fresh same-boot live host."""
    if set(public_intent_attestation) != PUBLIC_INTENT_ATTESTATION_FIELDS:
        raise RuntimeError("host preflight requires the exact public intent proof")
    intent_sha256 = _intent_sha256(command)
    stage = _paid_stage(command)
    job_name, job_dir = _claim_job_destination(command)
    if (
        public_intent_attestation.get("intent_sha256") != intent_sha256
        or public_intent_attestation.get("kind") != stage
    ):
        raise RuntimeError("host preflight public intent identity drifted")
    public_commit = public_intent_attestation.get("ledger_commit")
    if (
        not isinstance(public_commit, str)
        or _SOURCE_COMMIT_RE.fullmatch(public_commit) is None
    ):
        raise RuntimeError("host preflight public commit is invalid")
    report_path = public_report_path(intent_sha256)
    try:
        public_report_raw = reader.get_content(report_path, public_commit)
    except Exception as exc:
        raise RuntimeError("anonymous public host-report GET failed") from exc
    _reject_public_credentials(
        public_report_raw,
        forbidden_credentials,
        label="public host report",
    )
    now = clock or (lambda: datetime.now(timezone.utc))
    fetched_at = now()
    if fetched_at.tzinfo is None:
        raise RuntimeError("host preflight clock lacks a timezone")
    fetched_at = fetched_at.astimezone(timezone.utc)
    prior_runtime_check = _aware_timestamp(
        public_intent_attestation.get("runtime_revalidated_at_utc"),
        label="public intent runtime_revalidated_at_utc",
    )
    if fetched_at < prior_runtime_check:
        raise RuntimeError("host preflight clock rolled back after public intent")
    fetched_text = _utc_text(fetched_at)
    try:
        live_recheck = host_probe(
            jobs_dir=job_dir.parent,
            docker_executable=docker_executable,
        )
        validated = build_launch_binding(
            public_report_raw=public_report_raw,
            public_commit=public_commit,
            public_fetched_at_utc=fetched_text,
            launch_receipt_sha256="0" * 64,
            live_recheck=live_recheck,
            expected_intent_sha256=intent_sha256,
            expected_stage=stage,
            expected_job_name=job_name,
        )
    except HostAttestationError as exc:
        raise RuntimeError(f"native host preflight failed: {exc}") from exc
    public_probe_path = validated["public_report_payload"]["observed"]["disk"][
        "probe_path"
    ]
    live_probe_path = validated["live_recheck"]["observed"]["disk"]["probe_path"]
    if public_probe_path != str(job_dir.parent) or live_probe_path != str(
        job_dir.parent
    ):
        raise RuntimeError("native host report does not probe the exact jobs-dir")
    return _VerifiedHostPreflight(
        public_report_raw=public_report_raw,
        public_commit=public_commit,
        public_fetched_at_utc=fetched_text,
        live_recheck=validated["live_recheck"],
        jobs_dir=job_dir.parent,
    )


def _validate_claim_command(command: Sequence[str], environ: Mapping[str, str]) -> None:
    """Reject Harbor argv channels that can reintroduce secrets or drift."""
    if len(command) < 2 or list(command[:2]) != ["harbor", "run"]:
        raise RuntimeError("secure launcher only accepts `harbor run`")

    for argument in command[2:]:
        if (
            argument.startswith("-")
            and not argument.startswith("--")
            and len(argument) > 2
        ):
            raise RuntimeError("secure launcher rejects attached short-option values")
        option_name = argument.partition("=")[0]
        if option_name in _BANNED_CLAIM_OPTIONS:
            raise RuntimeError(
                "secure launcher rejects a noncanonical Harbor claim option"
            )
        assignment_name, separator, _ = argument.partition("=")
        if separator and is_credential_env_name(assignment_name.lstrip("-")):
            raise RuntimeError("secure launcher rejects credential assignments in argv")

    credential_values = credential_values_from_environment(environ)
    if any(
        secret in argument for secret in credential_values for argument in command[2:]
    ):
        raise RuntimeError("secure launcher detected credential material in argv")

    options = _claim_options(command)

    agent_imports = options.get("--agent-import-path", [])
    if agent_imports != [_CANONICAL_AGENT_IMPORT_PATH]:
        raise RuntimeError(
            "secure launcher requires exactly the canonical Stella agent import"
        )

    environment_types = options.get("--env", [])
    if environment_types != ["docker"]:
        raise RuntimeError("secure launcher requires Harbor's Docker environment")

    datasets = options.get("--dataset", [])
    if datasets and datasets != [_CANONICAL_DATASET_ARGUMENT]:
        raise RuntimeError(
            "secure launcher requires the exact version-pinned Terminal-Bench "
            "dataset whenever --dataset is present"
        )

    include_filters = options.get("--include-task-name", [])
    if any(
        not value.startswith("terminal-bench/") or value == "terminal-bench/"
        for value in include_filters
    ):
        raise RuntimeError(
            "secure launcher requires Terminal-Bench include filters to use "
            "the `terminal-bench/` task-name prefix"
        )

    # Defaults are not enough for claim evidence: an explicit unique job path
    # is the launcher-owned anti-resume boundary.
    _claim_job_destination(command)
    _intent_sha256(command)


def _canonical_readiness_path() -> Path:
    """Return the tracked readiness fixture beside this source checkout."""
    fixture = (
        Path(__file__).resolve().parents[2] / "readiness" / "synthetic-adapter-sentinel"
    )
    try:
        resolved = fixture.resolve(strict=True)
    except OSError as exc:
        raise RuntimeError(
            "secure launcher cannot resolve the canonical readiness fixture"
        ) from exc
    if not resolved.is_dir():
        raise RuntimeError(
            "secure launcher requires the canonical readiness fixture directory"
        )
    return resolved


def _require_exact_stage_options(
    options: Mapping[str, Sequence[str]], expected: frozenset[str], stage: str
) -> None:
    if set(options) != expected:
        raise RuntimeError(
            f"secure launcher requires the exact {stage} Harbor option shape"
        )


def _validate_stage_shape(command: Sequence[str]) -> None:
    """Require one of the three preregistered paid-study command shapes."""
    options = _claim_options(command)
    _intent_comment_url(command)
    job_names = options.get("--job-name", [])
    if len(job_names) != 1:
        raise RuntimeError("secure launcher requires exactly one stage job name")
    job_name = job_names[0]

    if job_name == _READINESS_JOB_NAME:
        stage = "readiness"
        _require_exact_stage_options(
            options, _COMMON_STAGE_OPTIONS | frozenset({"--path"}), stage
        )
        path_values = options.get("--path", [])
        if len(path_values) != 1:
            raise RuntimeError(
                "secure launcher requires the exact canonical readiness path"
            )
        supplied_path = Path(path_values[0])
        try:
            resolved_path = supplied_path.resolve(strict=True)
        except OSError as exc:
            raise RuntimeError(
                "secure launcher requires the exact canonical readiness path"
            ) from exc
        if (
            not supplied_path.is_absolute()
            or supplied_path != resolved_path
            or resolved_path != _canonical_readiness_path()
        ):
            raise RuntimeError(
                "secure launcher requires the exact canonical readiness path"
            )
        expected_models = [_CANDIDATE_MODELS[0]]
        expected_attempts = "1"
        expected_concurrency = "1"
    elif job_name == _CALIBRATION_JOB_NAME:
        stage = "calibration"
        _require_exact_stage_options(
            options,
            _COMMON_STAGE_OPTIONS | frozenset({"--dataset", "--include-task-name"}),
            stage,
        )
        if options.get("--dataset") != [_CANONICAL_DATASET_ARGUMENT]:
            raise RuntimeError("secure launcher requires the exact calibration dataset")
        if options.get("--include-task-name") != list(_CALIBRATION_TASK_FILTERS):
            raise RuntimeError(
                "secure launcher requires the exact ordered calibration task filters"
            )
        expected_models = list(_CANDIDATE_MODELS)
        expected_attempts = "2"
        expected_concurrency = "3"
    else:
        stage = "confirmatory"
        _require_exact_stage_options(
            options, _COMMON_STAGE_OPTIONS | frozenset({"--dataset"}), stage
        )
        if options.get("--dataset") != [_CANONICAL_DATASET_ARGUMENT]:
            raise RuntimeError(
                "secure launcher requires the exact confirmatory dataset"
            )
        models = options.get("--model", [])
        if len(models) != 1 or models[0] not in _CONFIRMATORY_MODELS:
            raise RuntimeError("secure launcher requires the fixed GLM-5.1 primary")
        expected_models = models
        expected_attempts = "5"
        expected_concurrency = "1"

    if options.get("--model") != expected_models:
        raise RuntimeError(
            f"secure launcher requires the exact ordered {stage} model roster"
        )
    if options.get("--n-attempts") != [expected_attempts]:
        raise RuntimeError(
            f"secure launcher requires exact {stage} attempt count {expected_attempts}"
        )
    if options.get("--n-concurrent") != [expected_concurrency]:
        raise RuntimeError(
            f"secure launcher requires exact {stage} concurrency {expected_concurrency}"
        )
    if options.get("--max-retries") != ["0"]:
        raise RuntimeError(f"secure launcher requires zero {stage} Harbor retries")


def _binary_version_source_commits(path: Path) -> set[str]:
    """Return full commits embedded in compile-time ``-dev.<sha>`` bytes."""
    matches: set[str] = set()
    tail = b""
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            window = tail + chunk
            matches.update(
                match.decode("ascii")
                for match in _VERSION_COMMIT_BYTES_RE.findall(window)
            )
            tail = window[-64:]

    # The normal Rust version string is NUL-terminated and is handled above,
    # but accept an otherwise-valid marker ending exactly at EOF as well.
    eof_match = re.search(rb"-dev\.([0-9A-Fa-f]{40})$", tail)
    if eof_match is not None:
        matches.add(eof_match.group(1).decode("ascii"))
    return matches


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _resolve_docker_runtime() -> tuple[Path, str]:
    """Resolve Docker from fixed host locations, never the caller's ``PATH``."""
    for candidate in _TRUSTED_DOCKER_CANDIDATES:
        try:
            resolved = candidate.resolve(strict=True)
            info = resolved.stat()
        except OSError:
            continue
        if stat.S_ISREG(info.st_mode) and os.access(resolved, os.X_OK):
            return resolved, _sha256_file(resolved)
    raise RuntimeError("secure launcher cannot resolve a trusted Docker executable")


def _binary_version_texts(path: Path) -> set[str]:
    matches: set[str] = set()
    tail = b""
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            window = tail + chunk
            matches.update(
                value.decode("ascii")
                for value in _VERSION_TEXT_BYTES_RE.findall(window)
            )
            tail = window[-128:]
    return matches


def _validate_claim_environment(environ: Mapping[str, str]) -> tuple[Path, str]:
    """Validate immutable claim controls and the exact host Stella artifact."""
    if environ.get("STELLA_BUDGET") != _CANONICAL_BUDGET:
        raise RuntimeError(
            f"secure launcher requires exact STELLA_BUDGET={_CANONICAL_BUDGET}"
        )
    if environ.get("STELLA_DISABLE_REFLECTION") != _CANONICAL_DISABLE_REFLECTION:
        raise RuntimeError(
            "secure launcher requires exact STELLA_DISABLE_REFLECTION="
            f"{_CANONICAL_DISABLE_REFLECTION}"
        )

    source_commit = environ.get("STELLA_SOURCE_COMMIT", "")
    if _SOURCE_COMMIT_RE.fullmatch(source_commit) is None:
        raise RuntimeError(
            "secure launcher requires exact lowercase 40-hex STELLA_SOURCE_COMMIT"
        )

    binary_text = environ.get("STELLA_BINARY", "")
    binary = Path(binary_text)
    if not binary_text or not binary.is_absolute():
        raise RuntimeError(
            "secure launcher requires STELLA_BINARY to be an absolute canonical path"
        )
    try:
        resolved_binary = binary.resolve(strict=True)
        binary_info = binary.lstat()
    except OSError as exc:
        raise RuntimeError(
            "secure launcher requires STELLA_BINARY to name an existing executable"
        ) from exc
    if binary != resolved_binary:
        raise RuntimeError(
            "secure launcher requires STELLA_BINARY to be an absolute canonical path"
        )
    if not stat.S_ISREG(binary_info.st_mode) or not os.access(binary, os.X_OK):
        raise RuntimeError(
            "secure launcher requires STELLA_BINARY to be a regular executable"
        )

    try:
        with binary.open("rb") as handle:
            header = handle.read(_ELF64_LITTLE_ENDIAN_X86_64_HEADER_BYTES)
    except OSError as exc:
        raise RuntimeError("secure launcher cannot read STELLA_BINARY") from exc
    if (
        len(header) < _ELF64_LITTLE_ENDIAN_X86_64_HEADER_BYTES
        or header[:4] != b"\x7fELF"
        or header[4] != 2
        or header[5] != 1
        or header[6] != 1
        or int.from_bytes(header[18:20], "little") != 62
    ):
        raise RuntimeError(
            "secure launcher requires STELLA_BINARY to be an ELF64 "
            "little-endian x86_64 executable"
        )

    embedded_commits = _binary_version_source_commits(binary)
    if embedded_commits != {source_commit}:
        raise RuntimeError(
            "secure launcher requires the exact lowercase STELLA_SOURCE_COMMIT "
            "in the binary's compile-time version bytes"
        )
    return binary, source_commit


def _current_adapter_module() -> ModuleType:
    """Return the already-imported package that owns this launcher."""
    package = sys.modules.get(__package__)
    if not isinstance(package, ModuleType):
        raise RuntimeError("secure launcher cannot resolve its current adapter import")
    expected_init = Path(__file__).resolve().with_name("__init__.py")
    package_file = getattr(package, "__file__", None)
    if package_file is None or Path(package_file).resolve() != expected_init:
        raise RuntimeError("secure launcher resolved a different adapter import")
    agent = getattr(package, "StellaAgent", None)
    if getattr(agent, "__module__", None) != __package__:
        raise RuntimeError("secure launcher resolved a different StellaAgent import")
    return package


def _resolve_harbor_executable() -> tuple[Path, str]:
    """Resolve the current interpreter and attest the imported adapter.

    The generated ``.venv/bin/harbor`` script is intentionally not inspected
    or executed: it is mutable code outside the adapter tree digest.  The
    caller executes this already-running interpreter in isolated, no-site mode
    through :data:`_ISOLATED_HARBOR_SHIM` instead.
    """
    interpreter = Path(sys.executable)
    if not interpreter.is_absolute():
        raise RuntimeError("secure launcher requires an absolute Python interpreter")
    try:
        interpreter_real = interpreter.resolve(strict=True)
        interpreter_info = interpreter_real.stat()
    except OSError as exc:
        raise RuntimeError(
            "secure launcher cannot resolve its Python interpreter"
        ) from exc
    if not stat.S_ISREG(interpreter_info.st_mode) or not os.access(
        interpreter_real, os.X_OK
    ):
        raise RuntimeError("secure launcher requires an executable Python runtime")

    package = _current_adapter_module()
    digest_function = getattr(package, "_adapter_content_sha256", None)
    if not callable(digest_function):
        raise RuntimeError("secure launcher cannot hash its current adapter import")
    adapter_sha256 = digest_function()
    if (
        not isinstance(adapter_sha256, str)
        or re.fullmatch(r"[0-9a-f]{64}", adapter_sha256) is None
    ):
        raise RuntimeError("secure launcher produced an invalid adapter source hash")
    return interpreter_real, adapter_sha256


def _isolated_harbor_command(
    interpreter: Path, harbor_command: Sequence[str], pycache_prefix: Path
) -> list[str]:
    """Build the fixed source-only ``python -I -S -B -X ... -c`` invocation."""
    if not harbor_command or harbor_command[0] != "harbor":
        raise RuntimeError("isolated Harbor shim requires a Harbor command")
    package = _current_adapter_module()
    package_file = Path(str(package.__file__)).resolve(strict=True)
    adapter_root = package_file.parent.parent
    harbor_package = sys.modules.get("harbor")
    harbor_file_text = getattr(harbor_package, "__file__", None)
    if not isinstance(harbor_package, ModuleType) or not isinstance(
        harbor_file_text, str
    ):
        raise RuntimeError("secure launcher cannot resolve imported Harbor package")
    harbor_file = Path(harbor_file_text).resolve(strict=True)
    site_root = harbor_file.parent.parent
    for path, label in (
        (adapter_root, "adapter import root"),
        (site_root, "Harbor site-packages root"),
    ):
        try:
            info = path.lstat()
        except OSError as exc:
            raise RuntimeError(f"secure launcher cannot resolve {label}") from exc
        if not path.is_absolute() or not stat.S_ISDIR(info.st_mode):
            raise RuntimeError(f"secure launcher requires canonical {label}")
    if harbor_file.parent.parent != site_root:
        raise RuntimeError("secure launcher resolved Harbor outside its site root")
    try:
        resolved_cache = pycache_prefix.resolve(strict=True)
        cache_info = pycache_prefix.lstat()
        cache_entries = list(pycache_prefix.iterdir())
    except OSError as exc:
        raise RuntimeError("isolated Harbor pycache prefix is unreadable") from exc
    if (
        not pycache_prefix.is_absolute()
        or resolved_cache != pycache_prefix
        or not stat.S_ISDIR(cache_info.st_mode)
        or cache_info.st_uid != os.getuid()
        or stat.S_IMODE(cache_info.st_mode) != 0o700
        or cache_entries
    ):
        raise RuntimeError(
            "isolated Harbor pycache prefix must be fresh, canonical, and owner-only"
        )
    return [
        str(interpreter),
        "-I",
        "-S",
        "-B",
        "-X",
        f"pycache_prefix={pycache_prefix}",
        "-c",
        _ISOLATED_HARBOR_SHIM,
        str(adapter_root),
        str(site_root),
        *harbor_command[1:],
    ]


def _validated_runtime_identity(
    command: Sequence[str],
    environ: Mapping[str, str],
    credentials: Mapping[str, str],
) -> tuple[dict[str, Any], Path]:
    """Recompute every runtime field bound by the immutable public intent."""
    binary, source_commit = _validate_claim_environment(environ)
    python_executable, adapter_sha256 = _resolve_harbor_executable()
    if len(credentials) != 1:
        raise RuntimeError("runtime identity requires exactly one provider credential")
    credential = next(iter(credentials.values()))
    binary_sha256 = _sha256_file(binary)
    version_texts = _binary_version_texts(binary)
    expected_version = next(iter(version_texts)) if len(version_texts) == 1 else None
    if expected_version is None or not expected_version.endswith(source_commit):
        raise RuntimeError("Stella binary has no unique compile-time version identity")

    package = _current_adapter_module()
    adapter_version = getattr(package, "_ADAPTER_VERSION", None)
    harbor_version_function = getattr(package, "_harbor_version", None)
    harbor_digest_function = getattr(package, "_harbor_content_sha256", None)
    posture_function = getattr(package, "_benchmark_engine_posture", None)
    if (
        adapter_version != _CANONICAL_ADAPTER_VERSION
        or not callable(harbor_version_function)
        or not callable(harbor_digest_function)
        or not callable(posture_function)
    ):
        raise RuntimeError("adapter runtime identity surface drifted")
    harbor_version = harbor_version_function()
    harbor_sha256 = harbor_digest_function()
    if harbor_version != _CANONICAL_HARBOR_VERSION:
        raise RuntimeError("secure launcher requires exact Harbor 0.6.1")
    if re.fullmatch(r"[0-9a-f]{64}", harbor_sha256 or "") is None:
        raise RuntimeError("secure launcher produced an invalid Harbor source hash")

    repository_root = Path(__file__).resolve().parents[3]
    source_digests: dict[str, str] = {}
    for name, relative in (
        ("analysis_sha256", _FIXED_ANALYZER_PATH),
        ("public_timing_sha256", _FIXED_PUBLIC_TIMING_PATH),
    ):
        path = repository_root / relative
        try:
            resolved = path.resolve(strict=True)
            info = path.lstat()
        except OSError as exc:
            raise RuntimeError(
                f"cannot resolve frozen runtime source {relative}"
            ) from exc
        if resolved != path or not stat.S_ISREG(info.st_mode):
            raise RuntimeError(f"frozen runtime source {relative} is not canonical")
        source_digests[name] = _sha256_file(path)

    models = _claim_options(command).get("--model", [])
    postures: dict[str, str] = {}
    for model in models:
        posture = posture_function(model)
        if (
            not isinstance(posture, tuple)
            or len(posture) != 3
            or re.fullmatch(r"[0-9a-f]{64}", posture[2] or "") is None
        ):
            raise RuntimeError("adapter produced an invalid engine posture identity")
        postures[model] = posture[2]
    identity = {
        "binary_sha256": binary_sha256,
        "source_commit": source_commit,
        "agent_version": (f"stella {expected_version} [binary-sha256:{binary_sha256}]"),
        "adapter_version": adapter_version,
        "adapter_sha256": adapter_sha256,
        **source_digests,
        "harbor_version": harbor_version,
        "harbor_sha256": harbor_sha256,
        "engine_posture_sha256_by_model": postures,
        "base_url": _CANONICAL_OPENROUTER_BASE_URL,
        "provider_route_policy": _CANONICAL_PROVIDER_ROUTE_POLICY,
        "disable_reflection": True,
        "provider_key_fingerprint_sha256": hashlib.sha256(
            credential.encode("utf-8")
        ).hexdigest(),
    }
    if set(identity) != RUNTIME_IDENTITY_FIELDS:
        raise RuntimeError("validated runtime identity fields drifted")
    return identity, python_executable


def _literal_model_arguments(command: Sequence[str]) -> list[str]:
    """Return explicit Harbor models sharing one provider, or fail closed."""
    values: list[str] = []
    index = 1
    while index < len(command):
        argument = command[index]
        if argument in {"-m", "--model"}:
            index += 1
            if index >= len(command):
                raise RuntimeError("secure launcher requires a value after --model")
            values.append(command[index])
        elif argument.startswith("--model="):
            values.append(argument.partition("=")[2])
        index += 1
    if not values or any(not value for value in values):
        raise RuntimeError(
            "secure launcher requires at least one literal Harbor --model argument"
        )
    parsed = [value.partition("/") for value in values]
    if any(
        not provider.strip() or not separator or not model_id.strip()
        for provider, separator, model_id in parsed
    ):
        raise RuntimeError("every Harbor --model must be a literal provider/model")
    providers = {provider.strip().lower() for provider, _, _ in parsed}
    if len(providers) != 1:
        raise RuntimeError(
            "every Harbor --model route must use one unique provider roster"
        )
    return values


def _claim_job_destination(command: Sequence[str]) -> tuple[str, Path]:
    """Resolve one explicit Harbor job directory without creating it."""
    job_names = _option_values(command, frozenset({"--job-name"}))
    jobs_dirs = _option_values(command, frozenset({"--jobs-dir"}))
    if len(job_names) != 1 or len(jobs_dirs) != 1:
        raise RuntimeError(
            "secure launcher requires exactly one literal --job-name and --jobs-dir"
        )

    job_name = job_names[0]
    jobs_dir_text = jobs_dirs[0]
    if (
        not job_name
        or job_name.startswith("-")
        or Path(job_name).name != job_name
        or job_name in {".", ".."}
        or "/" in job_name
        or "\\" in job_name
    ):
        raise RuntimeError("secure launcher requires a single safe job-name component")
    if (
        not jobs_dir_text
        or jobs_dir_text.startswith("-")
        or not Path(jobs_dir_text).is_absolute()
    ):
        raise RuntimeError(
            "secure launcher requires a nonempty absolute explicit jobs-dir"
        )

    jobs_root = Path(jobs_dir_text).resolve(strict=False)
    job_dir = (jobs_root / job_name).resolve(strict=False)
    if job_dir.parent != jobs_root:
        raise RuntimeError("secure launcher job path escapes the explicit jobs-dir")
    return job_name, job_dir


def _intent_sha256(command: Sequence[str]) -> str:
    """Return the one preregistered paid-intent digest, or fail closed."""
    values = _option_values(command, frozenset({"--intent-sha256"}))
    if len(values) != 1 or re.fullmatch(r"[0-9a-f]{64}", values[0]) is None:
        raise RuntimeError(
            "secure launcher requires exactly one lowercase 64-hex --intent-sha256"
        )
    return values[0]


def _harbor_command(command: Sequence[str]) -> list[str]:
    """Remove launcher-only attestation options before Harbor parses argv."""
    forwarded: list[str] = []
    index = 0
    while index < len(command):
        argument = command[index]
        if argument == "--intent-sha256":
            index += 2
            continue
        if argument.startswith("--intent-sha256="):
            index += 1
            continue
        if argument == "--intent-comment-url":
            index += 2
            continue
        if argument.startswith("--intent-comment-url="):
            index += 1
            continue
        forwarded.append(argument)
        index += 1
    return forwarded


def _launcher_reservation_root() -> Path:
    """Return the host-global, launcher-owned paid-intent reservation root."""
    try:
        home = Path(pwd.getpwuid(os.getuid()).pw_dir).resolve(strict=True)
    except (KeyError, OSError) as exc:
        raise RuntimeError(
            "secure launcher cannot resolve its host state root"
        ) from exc
    return home / ".local" / "state" / "stella" / "tb21-paid-intents-v1"


def _reserve_global_intent(intent_sha256: str) -> Path:
    """Atomically reserve one intent independently of Harbor's jobs directory."""
    if re.fullmatch(r"[0-9a-f]{64}", intent_sha256) is None:
        raise RuntimeError("global launch reservation requires an intent digest")
    root = _launcher_reservation_root()
    root.mkdir(parents=True, mode=0o700, exist_ok=True)
    try:
        resolved_root = root.resolve(strict=True)
        root_info = root.lstat()
    except OSError as exc:
        raise RuntimeError("secure launcher cannot validate reservation root") from exc
    if (
        resolved_root != root
        or not stat.S_ISDIR(root_info.st_mode)
        or root_info.st_uid != os.getuid()
        or stat.S_IMODE(root_info.st_mode) & 0o077
    ):
        raise RuntimeError(
            "secure launcher reservation root must be canonical and owner-only"
        )
    reservation = root / intent_sha256
    try:
        reservation.mkdir(mode=0o700, exist_ok=False)
    except FileExistsError as exc:
        raise FileExistsError(
            "secure launcher refuses an already-reserved paid intent; "
            "changing --jobs-dir cannot replay spend"
        ) from exc
    directory_flags = os.O_RDONLY
    if hasattr(os, "O_DIRECTORY"):
        directory_flags |= os.O_DIRECTORY
    root_fd = os.open(root, directory_flags)
    try:
        os.fsync(root_fd)
    finally:
        os.close(root_fd)
    return reservation


def _reserve_fresh_job(
    command: Sequence[str],
    models: Sequence[str],
    public_intent_attestation: Mapping[str, Any],
    host_preflight: _VerifiedHostPreflight,
    forbidden_credentials: Sequence[str],
) -> Path:
    """Reserve one job and bind its receipt to verified native-host evidence."""
    if (
        set(public_intent_attestation) != PUBLIC_INTENT_ATTESTATION_FIELDS
        or public_intent_attestation.get("intent_sha256") != _intent_sha256(command)
        or public_intent_attestation.get("comment_url")
        != _intent_comment_url(command)[0]
        or any(not credential for credential in forbidden_credentials)
    ):
        raise RuntimeError("secure launcher received an invalid public intent proof")
    intent_sha256 = _intent_sha256(command)
    job_name, job_dir = _claim_job_destination(command)
    if (
        host_preflight.public_commit != public_intent_attestation.get("ledger_commit")
        or host_preflight.jobs_dir != job_dir.parent
    ):
        raise RuntimeError("host preflight is not from the intent publication commit")
    try:
        build_launch_binding(
            public_report_raw=host_preflight.public_report_raw,
            public_commit=host_preflight.public_commit,
            public_fetched_at_utc=host_preflight.public_fetched_at_utc,
            launch_receipt_sha256="0" * 64,
            live_recheck=host_preflight.live_recheck,
            expected_intent_sha256=intent_sha256,
            expected_stage=_paid_stage(command),
            expected_job_name=job_name,
        )
    except HostAttestationError as exc:
        raise RuntimeError("secure launcher received invalid host preflight") from exc
    reservation = _reserve_global_intent(intent_sha256)
    try:
        job_dir.parent.mkdir(parents=True, exist_ok=True)
        try:
            job_dir.mkdir(mode=0o700, exist_ok=False)
        except FileExistsError as exc:
            raise FileExistsError(
                "secure launcher refuses an existing Harbor job directory; "
                "claim runs cannot resume"
            ) from exc
    except BaseException:
        reservation.rmdir()
        raise

    receipt_path = job_dir / LAUNCH_RECEIPT_FILENAME
    host_sidecar_path = job_dir / HOST_ATTESTATION_FILENAME
    pycache_prefix = job_dir / _PYTHON_CACHE_PREFIX_DIRNAME
    receipt = {
        "schema_version": LAUNCH_RECEIPT_SCHEMA,
        "job_name": job_name,
        "models": list(models),
        "intent_sha256": intent_sha256,
        "public_intent_attestation": dict(public_intent_attestation),
        "launcher_controls": dict(LAUNCH_RECEIPT_CONTROLS),
    }
    payload = (
        json.dumps(
            receipt,
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
        )
        + "\n"
    ).encode("utf-8")
    _reject_public_credentials(
        payload, forbidden_credentials, label="secure launch receipt"
    )
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    fd: int | None = None
    try:
        fd = os.open(receipt_path, flags, 0o600)
        with os.fdopen(fd, "wb") as handle:
            fd = None
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        binding = build_launch_binding(
            public_report_raw=host_preflight.public_report_raw,
            public_commit=host_preflight.public_commit,
            public_fetched_at_utc=host_preflight.public_fetched_at_utc,
            launch_receipt_sha256=_sha256_file(receipt_path),
            live_recheck=host_preflight.live_recheck,
            expected_intent_sha256=intent_sha256,
            expected_stage=_paid_stage(command),
            expected_job_name=job_name,
        )
        write_launch_binding_sidecar(
            job_dir,
            binding,
            forbidden_values=tuple(forbidden_credentials),
        )
        pycache_prefix.mkdir(mode=0o700, exist_ok=False)
        directory_flags = os.O_RDONLY
        if hasattr(os, "O_DIRECTORY"):
            directory_flags |= os.O_DIRECTORY
        directory_fd = os.open(job_dir, directory_flags)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except BaseException:
        if fd is not None:
            os.close(fd)
        # This path can only remove the brand-new directory and receipt this
        # invocation created; never touch a pre-existing Harbor job.
        with suppress(OSError):
            pycache_prefix.rmdir()
        with suppress(OSError):
            host_sidecar_path.unlink(missing_ok=True)
        with suppress(OSError):
            receipt_path.unlink(missing_ok=True)
        with suppress(OSError):
            job_dir.rmdir()
        with suppress(OSError):
            reservation.rmdir()
        raise
    return job_dir


def _open_descriptor_numbers() -> Iterable[int]:
    """Enumerate open descriptors without scanning an unbounded RLIMIT."""
    for directory in (Path("/proc/self/fd"), Path("/dev/fd")):
        try:
            names = list(directory.iterdir())
        except OSError:
            continue
        for path in names:
            try:
                yield int(path.name)
            except ValueError:
                continue
        return
    # This launcher is supported on the Unix hosts Harbor supports. Python
    # descriptors are non-inheritable by default; fail rather than assume that
    # property if the host exposes neither standard descriptor directory.
    raise RuntimeError("cannot enumerate host descriptors for secure Harbor exec")


def mark_only_bundle_fd_inheritable(bundle_fd: int) -> None:
    """Ensure no non-stdio descriptor except the credential bundle crosses exec."""
    for fd in _open_descriptor_numbers():
        if fd <= 2 or fd == bundle_fd:
            continue
        try:
            os.set_inheritable(fd, False)
        except OSError:
            # The directory enumeration itself briefly owns a descriptor that
            # may close before this loop reaches it.
            continue
    os.set_inheritable(bundle_fd, True)


def exec_harbor_securely(
    command: Sequence[str],
    *,
    environ: Mapping[str, str] | None = None,
    execvpe: object = os.execvpe,
    public_intent_reader: _PublicIntentReader | None = None,
    provider_key_reader: _ProviderKeyReader | None = None,
    host_probe: _HostProbe | None = None,
    clock: Callable[[], datetime] | None = None,
    sleeper: Callable[[float], None] = time.sleep,
) -> None:
    """Create the bundle, scrub the environment, and replace this process."""
    if not command or command[0] != "harbor":
        raise RuntimeError("secure launcher requires a `harbor ...` command")
    source_env = dict(os.environ if environ is None else environ)
    _validate_claim_command(command, source_env)
    models = _literal_model_arguments(command)
    credentials = provider_credential_for_model(source_env, models[0])
    provider_credential = next(iter(credentials.values()))
    management_credential = source_env.get("OPENROUTER_MANAGEMENT_API_KEY")
    if (
        not isinstance(management_credential, str)
        or not management_credential
        or management_credential == provider_credential
    ):
        raise RuntimeError(
            "secure launcher requires a distinct non-empty "
            "OPENROUTER_MANAGEMENT_API_KEY"
        )
    _validate_stage_shape(command)
    runtime_identity, python_executable = _validated_runtime_identity(
        command, source_env, credentials
    )
    python_sha256 = _sha256_file(python_executable)
    docker_executable, docker_sha256 = _resolve_docker_runtime()

    def revalidate_runtime() -> Mapping[str, Any]:
        revalidated, revalidated_python = _validated_runtime_identity(
            command, source_env, credentials
        )
        if revalidated_python != python_executable:
            raise RuntimeError("Python executable changed during public preflight")
        if _sha256_file(revalidated_python) != python_sha256:
            raise RuntimeError("Python runtime bytes changed during public preflight")
        if _resolve_docker_runtime() != (docker_executable, docker_sha256):
            raise RuntimeError("Docker executable changed during public preflight")
        return revalidated

    public_source = public_intent_reader or _AnonymousGitHubPublicIntentReader()
    public_intent_attestation = _verify_public_intent(
        command,
        runtime_identity=runtime_identity,
        runtime_revalidator=revalidate_runtime,
        provider_credential=provider_credential,
        management_credential=management_credential,
        reader=public_source,
        provider_key_reader=provider_key_reader,
        clock=clock,
        sleeper=sleeper,
    )
    handle = create_anonymous_credential_bundle(credentials)
    try:
        fd = handle.fileno()
        mark_only_bundle_fd_inheritable(fd)
        child_env = sanitized_harbor_environment(source_env, fd)
        # Harbor 0.6.1 serializes `datetime.now()` without an offset in the
        # root job result. Pin the execed process to UTC so those authoritative
        # whole-job boundaries have one reproducible interpretation.
        child_env["TZ"] = "UTC"
        child_env["PATH"] = os.pathsep.join(
            dict.fromkeys(
                (
                    str(docker_executable.parent),
                    "/usr/bin",
                    "/bin",
                    "/usr/sbin",
                    "/sbin",
                )
            )
        )
        # Typed as object to keep the test seam simple without a Protocol just
        # for os.execvpe's non-returning callable.
        # Recompute every source/runtime digest once more immediately before
        # reserving spend, then execute Harbor through the fixed in-tree shim.
        if dict(revalidate_runtime()) != dict(runtime_identity):
            raise RuntimeError("validated runtime changed before secure exec")
        host_preflight = _verify_public_host_preflight(
            command,
            public_intent_attestation=public_intent_attestation,
            forbidden_credentials=(provider_credential, management_credential),
            docker_executable=docker_executable,
            reader=public_source,
            host_probe=host_probe or probe_host,
            clock=clock,
        )
        job_dir = _reserve_fresh_job(
            command,
            models,
            public_intent_attestation,
            host_preflight,
            (provider_credential, management_credential),
        )
        forwarded_command = _isolated_harbor_command(
            python_executable,
            _harbor_command(command),
            job_dir / _PYTHON_CACHE_PREFIX_DIRNAME,
        )
        execvpe(  # type: ignore[operator]
            str(python_executable), forwarded_command, child_env
        )
        raise RuntimeError("secure Harbor exec unexpectedly returned")
    finally:
        handle.close()


def main(argv: Sequence[str] | None = None) -> int:
    """Console entry point: ``stella-harbor-secure harbor run ...``."""
    command = list(sys.argv[1:] if argv is None else argv)
    if command[:1] == ["--"]:
        command = command[1:]
    try:
        exec_harbor_securely(command)
    except (OSError, RuntimeError) as exc:
        print(f"stella-harbor-secure: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":  # pragma: no cover - exercised via console entry point
    raise SystemExit(main())
