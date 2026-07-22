"""Fail-closed native-host evidence for the Terminal-Bench study.

The paid launcher needs two related artifacts without widening its exact launch
receipt: a public report committed before launch and a job-local binding made
after the launcher anonymously reads that report and probes the host again.
This module owns their exact schemas, host probes, validation, and the atomic
sidecar writer.  It deliberately has no GitHub or launcher dependencies.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import platform
import re
import shlex
import shutil
import stat
import subprocess
import uuid
from collections.abc import Callable, Mapping, Sequence
from contextlib import suppress
from copy import deepcopy
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

HOST_REPORT_SCHEMA = "stella-tb21-host-report-v1"
HOST_LAUNCH_BINDING_SCHEMA = "stella-tb21-host-launch-binding-v1"
HOST_FINGERPRINT_DOMAIN = "stella-tb21-host-fingerprint-v1"
HOST_ATTESTATION_FILENAME = "stella-host-attestation.json"
FIXED_STUDY_ID = "stella-tb21-scientific-study-v1"
FIXED_REPOSITORY = "macanderson/stella"
PUBLIC_REPORT_PATH_PREFIX = "bench/evidence/host-attestations"
MIN_VCPUS = 4
# A provider's nominal 32-GiB class exposes slightly less through Linux
# MemTotal after firmware/kernel reservations.  The scientific floor is the
# observed quantity, not the marketing label.
MIN_MEMORY_BYTES = 31 * 1024**3
MIN_FREE_DISK_BYTES = 150 * 1024**3
MAX_RUNNING_CONTAINERS_BEFORE_LAUNCH = 0
MAX_PUBLIC_REPORT_AGE_SECONDS = 15 * 60
MAX_JSON_BYTES = 256 * 1024

_STAGES = frozenset({"readiness", "calibration", "confirmatory"})
_SHA256_RE = re.compile(r"[0-9a-f]{64}")
_COMMIT_RE = re.compile(r"[0-9a-f]{40}")
_CONTAINER_ID_RE = re.compile(r"[0-9a-f]{64}")
_MACHINE_ID_RE = re.compile(r"[0-9a-fA-F]{32}")
_BOOT_ID_RE = re.compile(
    r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
    r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
)

_REQUIREMENTS = {
    "system": "Linux",
    "architecture": "x86_64",
    "min_vcpus": MIN_VCPUS,
    "min_memory_bytes": MIN_MEMORY_BYTES,
    "min_free_disk_bytes": MIN_FREE_DISK_BYTES,
    "max_running_containers_before_launch": (MAX_RUNNING_CONTAINERS_BEFORE_LAUNCH),
}
_REPORT_FIELDS = frozenset(
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
_OBSERVED_FIELDS = frozenset(
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
_OS_FIELDS = frozenset(
    {
        "system",
        "kernel_release",
        "distribution_id",
        "distribution_version_id",
        "distribution_pretty_name",
    }
)
_CPU_FIELDS = frozenset({"effective_vcpus", "model"})
_MEMORY_FIELDS = frozenset({"total_bytes"})
_DISK_FIELDS = frozenset({"probe_path", "total_bytes", "used_bytes", "free_bytes"})
_DOCKER_FIELDS = frozenset(
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
_CHECK_FIELDS = frozenset(
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
_SNAPSHOT_FIELDS = frozenset(
    {"captured_at_utc", "host_fingerprint_sha256", "observed", "checks"}
)
_BINDING_FIELDS = frozenset(
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
_PUBLIC_REFERENCE_FIELDS = frozenset(
    {"repository", "commit", "path", "sha256", "fetched_at_utc"}
)


class HostAttestationError(RuntimeError):
    """The host or an attestation failed the frozen study contract."""


CommandRunner = Callable[[Sequence[str]], bytes]


@dataclass(frozen=True)
class HostProbeSeams:
    """Injectable low-level host operations used by :func:`probe_host`."""

    system: Callable[[], str]
    machine: Callable[[], str]
    kernel_release: Callable[[], str]
    effective_vcpus: Callable[[], int]
    read_text: Callable[[Path], str]
    disk_usage: Callable[[Path], tuple[int, int, int]]
    run_command: CommandRunner
    now: Callable[[], datetime]


def _default_effective_vcpus() -> int:
    affinity = getattr(os, "sched_getaffinity", None)
    if callable(affinity):
        return len(affinity(0))
    count = os.cpu_count()
    if count is None:
        raise HostAttestationError("host CPU count is unavailable")
    return count


def _default_run_command(command: Sequence[str]) -> bytes:
    try:
        completed = subprocess.run(  # noqa: S603 - argv only; no shell is used.
            list(command),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            check=False,
            timeout=30,
            env={"LC_ALL": "C", "PATH": "/usr/bin:/bin"},
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise HostAttestationError("host probe command failed to execute") from exc
    if completed.returncode != 0:
        raise HostAttestationError(
            f"host probe command exited with status {completed.returncode}"
        )
    if len(completed.stdout) > MAX_JSON_BYTES:
        raise HostAttestationError("host probe command output is too large")
    return completed.stdout


def default_probe_seams() -> HostProbeSeams:
    """Return production seams using only the local kernel and fixed argv calls."""
    return HostProbeSeams(
        system=platform.system,
        machine=platform.machine,
        kernel_release=platform.release,
        effective_vcpus=_default_effective_vcpus,
        read_text=lambda path: path.read_text(encoding="utf-8"),
        disk_usage=lambda path: tuple(shutil.disk_usage(path)),
        run_command=_default_run_command,
        now=lambda: datetime.now(UTC),
    )


def _exact_object(value: Any, fields: frozenset[str], *, label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        raise HostAttestationError(f"{label} does not have the exact frozen fields")
    return value


def _nonempty_text(value: Any, *, label: str) -> str:
    if not isinstance(value, str) or not value or value.strip() != value:
        raise HostAttestationError(f"{label} must be one nonempty canonical string")
    if any(ord(character) < 32 for character in value):
        raise HostAttestationError(f"{label} contains a control character")
    return value


def _positive_int(value: Any, *, label: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
        raise HostAttestationError(f"{label} must be a positive integer")
    return value


def _nonnegative_int(value: Any, *, label: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise HostAttestationError(f"{label} must be a nonnegative integer")
    return value


def _timestamp(value: Any, *, label: str) -> datetime:
    if not isinstance(value, str) or not value.endswith("Z"):
        raise HostAttestationError(f"{label} must be an RFC3339 UTC timestamp")
    try:
        parsed = datetime.fromisoformat(value[:-1] + "+00:00")
    except ValueError as exc:
        raise HostAttestationError(f"{label} must be an RFC3339 UTC timestamp") from exc
    if parsed.tzinfo is None:
        raise HostAttestationError(f"{label} lacks a timezone")
    return parsed.astimezone(UTC)


def _utc_text(value: datetime) -> str:
    if value.tzinfo is None:
        raise HostAttestationError("host probe clock lacks a timezone")
    return (
        value.astimezone(UTC).isoformat(timespec="microseconds").replace("+00:00", "Z")
    )


def _strict_json_object(raw: bytes, *, label: str) -> dict[str, Any]:
    if len(raw) > MAX_JSON_BYTES:
        raise HostAttestationError(f"{label} exceeds the JSON size limit")

    def no_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise HostAttestationError(f"{label} repeats JSON field {key!r}")
            result[key] = value
        return result

    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=no_duplicates)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise HostAttestationError(f"{label} is not strict UTF-8 JSON") from exc
    if not isinstance(value, dict):
        raise HostAttestationError(f"{label} is not a JSON object")
    return value


def canonical_json_bytes(value: Mapping[str, Any]) -> bytes:
    """Encode an attestation in its sole accepted stable file representation."""
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


def public_report_path(intent_sha256: str) -> str:
    """Return the deterministic repository path for one paid intent report."""
    if _SHA256_RE.fullmatch(intent_sha256) is None:
        raise HostAttestationError("host report path requires a lowercase SHA-256")
    return f"{PUBLIC_REPORT_PATH_PREFIX}/{intent_sha256}.json"


def host_fingerprint_sha256(
    *, machine_id: str, boot_id: str, docker_daemon_id: str
) -> str:
    """Hash private host identifiers into one domain-separated boot identity."""
    machine = machine_id.strip()
    boot = boot_id.strip()
    daemon = docker_daemon_id.strip()
    if _MACHINE_ID_RE.fullmatch(machine) is None:
        raise HostAttestationError("Linux machine-id is missing or malformed")
    if _BOOT_ID_RE.fullmatch(boot) is None:
        raise HostAttestationError("Linux boot-id is missing or malformed")
    if not daemon or any(ord(character) < 32 for character in daemon):
        raise HostAttestationError("Docker daemon ID is missing or malformed")
    payload = json.dumps(
        {
            "schema_version": HOST_FINGERPRINT_DOMAIN,
            "machine_id": machine.lower(),
            "boot_id": boot.lower(),
            "docker_daemon_id": daemon,
        },
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def _parse_os_release(raw: str) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in raw.splitlines():
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, encoded = line.split("=", 1)
        if key in values:
            raise HostAttestationError("/etc/os-release repeats a field")
        try:
            decoded = shlex.split(encoded, posix=True)
        except ValueError as exc:
            raise HostAttestationError("/etc/os-release has invalid quoting") from exc
        if len(decoded) != 1:
            raise HostAttestationError("/etc/os-release has a malformed value")
        values[key] = decoded[0]
    required = ("ID", "VERSION_ID", "PRETTY_NAME")
    if any(not values.get(key) for key in required):
        raise HostAttestationError("/etc/os-release lacks required identity fields")
    return {
        "distribution_id": values["ID"],
        "distribution_version_id": values["VERSION_ID"],
        "distribution_pretty_name": values["PRETTY_NAME"],
    }


def _parse_cpu_model(raw: str) -> str:
    candidates = ("model name", "hardware", "processor")
    parsed: dict[str, str] = {}
    for line in raw.splitlines():
        key, separator, value = line.partition(":")
        if separator and key.strip().lower() in candidates and value.strip():
            parsed.setdefault(key.strip().lower(), value.strip())
    for candidate in candidates:
        if candidate in parsed:
            return _nonempty_text(parsed[candidate], label="CPU model")
    raise HostAttestationError("/proc/cpuinfo lacks a CPU model")


def _parse_memory_total(raw: str) -> int:
    matches = re.findall(r"^MemTotal:\s+([0-9]+)\s+kB\s*$", raw, flags=re.MULTILINE)
    if len(matches) != 1:
        raise HostAttestationError("/proc/meminfo lacks one canonical MemTotal")
    return int(matches[0]) * 1024


def _docker_json(raw: bytes, *, label: str) -> dict[str, Any]:
    value = _strict_json_object(raw, label=label)
    return value


def _nested_text(parent: Mapping[str, Any], key: str, *, label: str) -> str:
    return _nonempty_text(parent.get(key), label=f"{label}.{key}")


def _normalized_x86(value: str, *, label: str) -> str:
    if value not in {"x86_64", "amd64"}:
        raise HostAttestationError(f"{label} is not native x86_64")
    return "x86_64"


def _probe_checks(observed: Mapping[str, Any]) -> dict[str, bool]:
    os_record = observed["os"]
    cpu = observed["cpu"]
    memory = observed["memory"]
    disk = observed["disk"]
    docker = observed["docker"]
    container_ids = observed["running_container_ids"]
    checks = {
        "native_linux_x86_64": (
            os_record["system"] == "Linux" and observed["architecture"] == "x86_64"
        ),
        "minimum_vcpus": cpu["effective_vcpus"] >= MIN_VCPUS,
        "minimum_memory": memory["total_bytes"] >= MIN_MEMORY_BYTES,
        "minimum_free_disk": disk["free_bytes"] >= MIN_FREE_DISK_BYTES,
        "docker_native_linux_x86_64": (
            docker["server_os"] == "linux" and docker["server_architecture"] == "x86_64"
        ),
        "zero_running_containers": (
            not container_ids
            and docker["reported_running_containers"]
            == MAX_RUNNING_CONTAINERS_BEFORE_LAUNCH
        ),
    }
    checks["all_passed"] = all(checks.values())
    return checks


def probe_host(
    *,
    jobs_dir: Path,
    docker_executable: Path,
    seams: HostProbeSeams | None = None,
) -> dict[str, Any]:
    """Measure one native Linux runner and fail if any frozen minimum is absent."""
    operations = seams or default_probe_seams()
    try:
        resolved_jobs_dir = jobs_dir.resolve(strict=True)
        jobs_info = resolved_jobs_dir.stat()
        resolved_docker = docker_executable.resolve(strict=True)
        docker_info = resolved_docker.stat()
    except OSError as exc:
        raise HostAttestationError("host probe paths are not resolvable") from exc
    if not jobs_dir.is_absolute() or not stat.S_ISDIR(jobs_info.st_mode):
        raise HostAttestationError("jobs_dir must be an existing absolute directory")
    if not docker_executable.is_absolute() or not stat.S_ISREG(docker_info.st_mode):
        raise HostAttestationError("Docker must be one absolute regular executable")
    if not os.access(resolved_docker, os.X_OK):
        raise HostAttestationError("Docker executable is not executable")

    system = _nonempty_text(operations.system(), label="host system")
    architecture = _normalized_x86(
        _nonempty_text(operations.machine(), label="host architecture"),
        label="host architecture",
    )
    kernel_release = _nonempty_text(operations.kernel_release(), label="kernel release")
    vcpus = _positive_int(operations.effective_vcpus(), label="effective vCPU count")
    os_release = _parse_os_release(operations.read_text(Path("/etc/os-release")))
    cpu_model = _parse_cpu_model(operations.read_text(Path("/proc/cpuinfo")))
    memory_total = _parse_memory_total(operations.read_text(Path("/proc/meminfo")))
    machine_id = operations.read_text(Path("/etc/machine-id"))
    boot_id = operations.read_text(Path("/proc/sys/kernel/random/boot_id"))
    total, used, free = operations.disk_usage(resolved_jobs_dir)
    disk_total = _positive_int(total, label="disk total bytes")
    disk_used = _nonnegative_int(used, label="disk used bytes")
    disk_free = _nonnegative_int(free, label="disk free bytes")
    if disk_used + disk_free > disk_total:
        raise HostAttestationError("disk byte accounting is inconsistent")

    executable = str(resolved_docker)
    version = _docker_json(
        operations.run_command([executable, "version", "--format", "{{json .}}"]),
        label="Docker version response",
    )
    client = version.get("Client")
    server = version.get("Server")
    if not isinstance(client, dict) or not isinstance(server, dict):
        raise HostAttestationError("Docker version response lacks client/server")
    info = _docker_json(
        operations.run_command([executable, "info", "--format", "{{json .}}"]),
        label="Docker info response",
    )
    raw_container_ids = operations.run_command(
        [executable, "ps", "--no-trunc", "--format", "{{.ID}}"]
    )
    try:
        container_text = raw_container_ids.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise HostAttestationError("Docker container list is not UTF-8") from exc
    container_ids = [
        line.strip() for line in container_text.splitlines() if line.strip()
    ]
    if len(set(container_ids)) != len(container_ids) or any(
        _CONTAINER_ID_RE.fullmatch(value) is None for value in container_ids
    ):
        raise HostAttestationError("Docker container list has invalid identities")
    reported_running = _nonnegative_int(
        info.get("ContainersRunning"), label="Docker ContainersRunning"
    )
    if reported_running != len(container_ids):
        raise HostAttestationError("Docker running-container probes disagree")
    info_os = _nested_text(info, "OSType", label="Docker info")
    info_architecture = _normalized_x86(
        _nested_text(info, "Architecture", label="Docker info"),
        label="Docker server architecture",
    )
    server_os = _nested_text(server, "Os", label="Docker server")
    server_architecture = _normalized_x86(
        _nested_text(server, "Arch", label="Docker server"),
        label="Docker server architecture",
    )
    if info_os != server_os or info_architecture != server_architecture:
        raise HostAttestationError("Docker version and info identities disagree")
    daemon_id = _nested_text(info, "ID", label="Docker info")

    observed = {
        "os": {
            "system": system,
            "kernel_release": kernel_release,
            **os_release,
        },
        "architecture": architecture,
        "cpu": {"effective_vcpus": vcpus, "model": cpu_model},
        "memory": {"total_bytes": memory_total},
        "disk": {
            "probe_path": str(resolved_jobs_dir),
            "total_bytes": disk_total,
            "used_bytes": disk_used,
            "free_bytes": disk_free,
        },
        "docker": {
            "client_version": _nested_text(client, "Version", label="Docker client"),
            "client_api_version": _nested_text(
                client, "ApiVersion", label="Docker client"
            ),
            "server_version": _nested_text(server, "Version", label="Docker server"),
            "server_api_version": _nested_text(
                server, "ApiVersion", label="Docker server"
            ),
            "server_os": server_os,
            "server_architecture": server_architecture,
            "reported_running_containers": reported_running,
        },
        "running_container_ids": container_ids,
    }
    checks = _probe_checks(observed)
    failed = [
        name for name, passed in checks.items() if name != "all_passed" and not passed
    ]
    if failed:
        raise HostAttestationError("host requirements failed: " + ", ".join(failed))
    return {
        "captured_at_utc": _utc_text(operations.now()),
        "host_fingerprint_sha256": host_fingerprint_sha256(
            machine_id=machine_id,
            boot_id=boot_id,
            docker_daemon_id=daemon_id,
        ),
        "observed": observed,
        "checks": checks,
    }


def build_public_host_report(
    *,
    intent_sha256: str,
    stage: str,
    job_name: str,
    snapshot: Mapping[str, Any],
) -> dict[str, Any]:
    """Build and validate the exact public prelaunch report."""
    validated_snapshot = _validate_snapshot(snapshot)
    report = {
        "schema_version": HOST_REPORT_SCHEMA,
        "study_id": FIXED_STUDY_ID,
        "intent_sha256": intent_sha256,
        "stage": stage,
        "job_name": job_name,
        "captured_at_utc": validated_snapshot["captured_at_utc"],
        "host_fingerprint_sha256": validated_snapshot["host_fingerprint_sha256"],
        "requirements": deepcopy(_REQUIREMENTS),
        "observed": deepcopy(validated_snapshot["observed"]),
        "checks": deepcopy(validated_snapshot["checks"]),
    }
    return validate_public_host_report(
        report,
        expected_intent_sha256=intent_sha256,
        expected_stage=stage,
        expected_job_name=job_name,
    )


def collect_public_host_report(
    *,
    intent_sha256: str,
    stage: str,
    job_name: str,
    jobs_dir: Path,
    docker_executable: Path,
    seams: HostProbeSeams | None = None,
) -> dict[str, Any]:
    """Probe the runner and build its exact public report."""
    snapshot = probe_host(
        jobs_dir=jobs_dir,
        docker_executable=docker_executable,
        seams=seams,
    )
    return build_public_host_report(
        intent_sha256=intent_sha256,
        stage=stage,
        job_name=job_name,
        snapshot=snapshot,
    )


def _validate_observed(observed: Any) -> dict[str, Any]:
    record = _exact_object(observed, _OBSERVED_FIELDS, label="host observed record")
    os_record = _exact_object(record["os"], _OS_FIELDS, label="host OS record")
    cpu = _exact_object(record["cpu"], _CPU_FIELDS, label="host CPU record")
    memory = _exact_object(record["memory"], _MEMORY_FIELDS, label="host memory record")
    disk = _exact_object(record["disk"], _DISK_FIELDS, label="host disk record")
    docker = _exact_object(record["docker"], _DOCKER_FIELDS, label="host Docker record")
    for field in (
        "system",
        "kernel_release",
        "distribution_id",
        "distribution_version_id",
        "distribution_pretty_name",
    ):
        _nonempty_text(os_record[field], label=f"host OS {field}")
    if os_record["system"] != "Linux":
        raise HostAttestationError("host system is not Linux")
    if record["architecture"] != "x86_64":
        raise HostAttestationError("host architecture is not x86_64")
    _positive_int(cpu["effective_vcpus"], label="effective vCPU count")
    _nonempty_text(cpu["model"], label="CPU model")
    _positive_int(memory["total_bytes"], label="memory total bytes")
    probe_path = _nonempty_text(disk["probe_path"], label="disk probe path")
    if not Path(probe_path).is_absolute():
        raise HostAttestationError("disk probe path is not absolute")
    total = _positive_int(disk["total_bytes"], label="disk total bytes")
    used = _nonnegative_int(disk["used_bytes"], label="disk used bytes")
    free = _nonnegative_int(disk["free_bytes"], label="disk free bytes")
    if used + free > total:
        raise HostAttestationError("disk byte accounting is inconsistent")
    for field in (
        "client_version",
        "client_api_version",
        "server_version",
        "server_api_version",
    ):
        _nonempty_text(docker[field], label=f"Docker {field}")
    if docker["server_os"] != "linux":
        raise HostAttestationError("Docker server OS is not linux")
    if docker["server_architecture"] != "x86_64":
        raise HostAttestationError("Docker server architecture is not x86_64")
    reported = _nonnegative_int(
        docker["reported_running_containers"],
        label="Docker reported running containers",
    )
    containers = record["running_container_ids"]
    if not isinstance(containers, list) or any(
        not isinstance(value, str) or _CONTAINER_ID_RE.fullmatch(value) is None
        for value in containers
    ):
        raise HostAttestationError("running_container_ids is not a canonical array")
    if len(set(containers)) != len(containers) or reported != len(containers):
        raise HostAttestationError("running-container evidence is inconsistent")
    return record


def _validate_snapshot(snapshot: Mapping[str, Any]) -> dict[str, Any]:
    value = _exact_object(dict(snapshot), _SNAPSHOT_FIELDS, label="live host snapshot")
    _timestamp(value["captured_at_utc"], label="host snapshot captured_at_utc")
    fingerprint = value["host_fingerprint_sha256"]
    if not isinstance(fingerprint, str) or _SHA256_RE.fullmatch(fingerprint) is None:
        raise HostAttestationError("host snapshot fingerprint is not a SHA-256")
    observed = _validate_observed(value["observed"])
    checks = _exact_object(value["checks"], _CHECK_FIELDS, label="host snapshot checks")
    if any(not isinstance(checks[field], bool) for field in _CHECK_FIELDS):
        raise HostAttestationError("host snapshot checks must all be booleans")
    if checks != _probe_checks(observed) or checks["all_passed"] is not True:
        raise HostAttestationError("host snapshot checks do not all pass")
    return deepcopy(value)


def validate_public_host_report(
    report: Mapping[str, Any],
    *,
    expected_intent_sha256: str | None = None,
    expected_stage: str | None = None,
    expected_job_name: str | None = None,
    now: datetime | None = None,
    max_age_seconds: int = MAX_PUBLIC_REPORT_AGE_SECONDS,
) -> dict[str, Any]:
    """Validate exact report structure, identity, thresholds, and freshness."""
    value = _exact_object(dict(report), _REPORT_FIELDS, label="public host report")
    if value["schema_version"] != HOST_REPORT_SCHEMA:
        raise HostAttestationError("public host report schema_version drifted")
    if value["study_id"] != FIXED_STUDY_ID:
        raise HostAttestationError("public host report study_id drifted")
    intent = value["intent_sha256"]
    if not isinstance(intent, str) or _SHA256_RE.fullmatch(intent) is None:
        raise HostAttestationError("public host report intent is not a SHA-256")
    if expected_intent_sha256 is not None and intent != expected_intent_sha256:
        raise HostAttestationError("public host report intent does not match")
    stage = value["stage"]
    if stage not in _STAGES:
        raise HostAttestationError("public host report stage is not registered")
    if expected_stage is not None and stage != expected_stage:
        raise HostAttestationError("public host report stage does not match")
    job_name = _nonempty_text(value["job_name"], label="host report job_name")
    if Path(job_name).name != job_name or job_name in {".", ".."}:
        raise HostAttestationError("public host report job_name is unsafe")
    if expected_job_name is not None and job_name != expected_job_name:
        raise HostAttestationError("public host report job_name does not match")
    captured = _timestamp(value["captured_at_utc"], label="host captured_at_utc")
    fingerprint = value["host_fingerprint_sha256"]
    if not isinstance(fingerprint, str) or _SHA256_RE.fullmatch(fingerprint) is None:
        raise HostAttestationError("public host fingerprint is not a SHA-256")
    if value["requirements"] != _REQUIREMENTS:
        raise HostAttestationError("public host requirements are not frozen exactly")
    observed = _validate_observed(value["observed"])
    checks = _exact_object(value["checks"], _CHECK_FIELDS, label="host checks")
    if any(not isinstance(checks[field], bool) for field in _CHECK_FIELDS):
        raise HostAttestationError("host checks must all be booleans")
    expected_checks = _probe_checks(observed)
    if checks != expected_checks or checks["all_passed"] is not True:
        failed = [
            name
            for name, passed in expected_checks.items()
            if name != "all_passed" and not passed
        ]
        suffix = ": " + ", ".join(failed) if failed else ""
        raise HostAttestationError("public host checks do not all pass" + suffix)
    if now is not None:
        current = now.astimezone(UTC) if now.tzinfo is not None else None
        if current is None:
            raise HostAttestationError("host validation clock lacks a timezone")
        age = (current - captured).total_seconds()
        if not math.isfinite(age) or age < 0 or age > max_age_seconds:
            raise HostAttestationError("public host report is future-dated or stale")
    return deepcopy(value)


def parse_public_host_report_bytes(
    raw: bytes,
    **expectations: Any,
) -> dict[str, Any]:
    """Parse strict public bytes and apply :func:`validate_public_host_report`."""
    report = _strict_json_object(raw, label="public host report")
    validated = validate_public_host_report(report, **expectations)
    if raw != canonical_json_bytes(validated):
        raise HostAttestationError(
            "public host report bytes are not canonical exact JSON"
        )
    return validated


def _same_host_static_identity(
    public_report: Mapping[str, Any], live_recheck: Mapping[str, Any]
) -> bool:
    public_observed = public_report["observed"]
    live_observed = live_recheck["observed"]
    return bool(
        public_report["host_fingerprint_sha256"]
        == live_recheck["host_fingerprint_sha256"]
        and public_observed["os"] == live_observed["os"]
        and public_observed["architecture"] == live_observed["architecture"]
        and public_observed["cpu"] == live_observed["cpu"]
        and public_observed["memory"] == live_observed["memory"]
        and public_observed["disk"]["probe_path"] == live_observed["disk"]["probe_path"]
        and public_observed["disk"]["total_bytes"]
        == live_observed["disk"]["total_bytes"]
        and public_observed["docker"] == live_observed["docker"]
    )


def build_launch_binding(
    *,
    public_report_raw: bytes,
    public_commit: str,
    public_fetched_at_utc: str,
    launch_receipt_sha256: str,
    live_recheck: Mapping[str, Any],
    expected_intent_sha256: str,
    expected_stage: str,
    expected_job_name: str,
) -> dict[str, Any]:
    """Bind public-before-launch bytes to a fresh same-host recheck and receipt."""
    fetched = _timestamp(public_fetched_at_utc, label="public report fetched_at_utc")
    report = parse_public_host_report_bytes(
        public_report_raw,
        expected_intent_sha256=expected_intent_sha256,
        expected_stage=expected_stage,
        expected_job_name=expected_job_name,
        now=fetched,
    )
    binding = {
        "schema_version": HOST_LAUNCH_BINDING_SCHEMA,
        "study_id": FIXED_STUDY_ID,
        "intent_sha256": expected_intent_sha256,
        "stage": expected_stage,
        "job_name": expected_job_name,
        "public_report": {
            "repository": FIXED_REPOSITORY,
            "commit": public_commit,
            "path": public_report_path(expected_intent_sha256),
            "sha256": hashlib.sha256(public_report_raw).hexdigest(),
            "fetched_at_utc": public_fetched_at_utc,
        },
        "launch_receipt_sha256": launch_receipt_sha256,
        "public_report_payload": report,
        "live_recheck": deepcopy(live_recheck),
    }
    return validate_launch_binding(binding)


def validate_launch_binding(binding: Mapping[str, Any]) -> dict[str, Any]:
    """Validate the exact immutable job-local launch-binding sidecar."""
    value = _exact_object(dict(binding), _BINDING_FIELDS, label="host launch binding")
    if value["schema_version"] != HOST_LAUNCH_BINDING_SCHEMA:
        raise HostAttestationError("host launch binding schema_version drifted")
    if value["study_id"] != FIXED_STUDY_ID:
        raise HostAttestationError("host launch binding study_id drifted")
    intent = value["intent_sha256"]
    if not isinstance(intent, str) or _SHA256_RE.fullmatch(intent) is None:
        raise HostAttestationError("host launch binding intent is invalid")
    stage = value["stage"]
    if stage not in _STAGES:
        raise HostAttestationError("host launch binding stage is invalid")
    job_name = _nonempty_text(value["job_name"], label="binding job_name")
    if Path(job_name).name != job_name or job_name in {".", ".."}:
        raise HostAttestationError("host launch binding job_name is unsafe")
    reference = _exact_object(
        value["public_report"],
        _PUBLIC_REFERENCE_FIELDS,
        label="public host report reference",
    )
    if reference["repository"] != FIXED_REPOSITORY:
        raise HostAttestationError("public host report repository drifted")
    commit = reference["commit"]
    if not isinstance(commit, str) or _COMMIT_RE.fullmatch(commit) is None:
        raise HostAttestationError("public host report commit is invalid")
    if reference["path"] != public_report_path(intent):
        raise HostAttestationError("public host report path is not deterministic")
    report_sha = reference["sha256"]
    receipt_sha = value["launch_receipt_sha256"]
    if not isinstance(report_sha, str) or _SHA256_RE.fullmatch(report_sha) is None:
        raise HostAttestationError("public host report SHA-256 is invalid")
    if not isinstance(receipt_sha, str) or _SHA256_RE.fullmatch(receipt_sha) is None:
        raise HostAttestationError("launch receipt SHA-256 is invalid")
    fetched = _timestamp(reference["fetched_at_utc"], label="public fetched_at_utc")
    report = validate_public_host_report(
        value["public_report_payload"],
        expected_intent_sha256=intent,
        expected_stage=stage,
        expected_job_name=job_name,
        now=fetched,
    )
    if hashlib.sha256(canonical_json_bytes(report)).hexdigest() != report_sha:
        raise HostAttestationError(
            "public host report SHA-256 does not bind its exact payload"
        )
    live_snapshot = _validate_snapshot(value["live_recheck"])
    live = validate_public_host_report(
        {
            **deepcopy(report),
            "captured_at_utc": live_snapshot["captured_at_utc"],
            "host_fingerprint_sha256": live_snapshot["host_fingerprint_sha256"],
            "observed": live_snapshot["observed"],
            "checks": live_snapshot["checks"],
        },
        expected_intent_sha256=intent,
        expected_stage=stage,
        expected_job_name=job_name,
    )
    live_captured = _timestamp(live["captured_at_utc"], label="live captured_at_utc")
    report_captured = _timestamp(
        report["captured_at_utc"], label="public captured_at_utc"
    )
    if not (report_captured <= fetched <= live_captured):
        raise HostAttestationError(
            "host report, public fetch, and live recheck chronology is invalid"
        )
    if (
        live_captured - report_captured
    ).total_seconds() > MAX_PUBLIC_REPORT_AGE_SECONDS:
        raise HostAttestationError("live host recheck is too far after public report")
    if not _same_host_static_identity(report, live):
        raise HostAttestationError("live host recheck is not the public host/boot")
    return deepcopy(value)


def write_launch_binding_sidecar(
    job_dir: Path,
    binding: Mapping[str, Any],
    *,
    forbidden_values: Sequence[str] = (),
) -> Path:
    """Atomically create the mode-0600 job sidecar without replacing anything."""
    validated = validate_launch_binding(binding)
    payload = canonical_json_bytes(validated)
    for forbidden in forbidden_values:
        if forbidden and forbidden.encode("utf-8") in payload:
            raise HostAttestationError("host sidecar contains a forbidden value")
    try:
        resolved_job_dir = job_dir.resolve(strict=True)
        directory_info = resolved_job_dir.stat()
    except OSError as exc:
        raise HostAttestationError("host sidecar job directory is unavailable") from exc
    if (
        not job_dir.is_absolute()
        or resolved_job_dir != job_dir
        or not stat.S_ISDIR(directory_info.st_mode)
        or directory_info.st_uid != os.getuid()
    ):
        raise HostAttestationError(
            "host sidecar job directory must be canonical and owner-controlled"
        )
    destination = resolved_job_dir / HOST_ATTESTATION_FILENAME
    temporary = resolved_job_dir / (
        f".{HOST_ATTESTATION_FILENAME}.{uuid.uuid4().hex}.tmp"
    )
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    fd: int | None = None
    linked = False
    try:
        fd = os.open(temporary, flags, 0o600)
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as handle:
            fd = None
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.link(temporary, destination, follow_symlinks=False)
        linked = True
        temporary.unlink()
        directory_flags = os.O_RDONLY
        if hasattr(os, "O_DIRECTORY"):
            directory_flags |= os.O_DIRECTORY
        directory_fd = os.open(resolved_job_dir, directory_flags)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except FileExistsError as exc:
        raise FileExistsError("host attestation sidecar already exists") from exc
    except OSError as exc:
        raise HostAttestationError("cannot atomically create host sidecar") from exc
    finally:
        if fd is not None:
            os.close(fd)
        if not linked or temporary.exists():
            with suppress(OSError):
                temporary.unlink(missing_ok=True)
    return destination


__all__ = [
    "FIXED_REPOSITORY",
    "FIXED_STUDY_ID",
    "HOST_ATTESTATION_FILENAME",
    "HOST_LAUNCH_BINDING_SCHEMA",
    "HOST_REPORT_SCHEMA",
    "HostAttestationError",
    "HostProbeSeams",
    "build_launch_binding",
    "build_public_host_report",
    "canonical_json_bytes",
    "collect_public_host_report",
    "default_probe_seams",
    "host_fingerprint_sha256",
    "parse_public_host_report_bytes",
    "probe_host",
    "public_report_path",
    "validate_launch_binding",
    "validate_public_host_report",
    "write_launch_binding_sidecar",
]
