"""Host-attestation schema, probe, binding, and persistence witnesses."""

from __future__ import annotations

import hashlib
import json
import stat
from copy import deepcopy
from dataclasses import replace
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

import stella_harbor.host_attestation as host

_INTENT = "a" * 64
_COMMIT = "b" * 40
_RECEIPT_SHA256 = "c" * 64
_JOB_NAME = "stella-tb21-calibration-20260721"
_MACHINE_ID = "0123456789abcdef0123456789abcdef"
_BOOT_ID = "12345678-1234-4234-8234-123456789abc"
_DAEMON_ID = "test-docker-daemon-id"
_CONTAINER_ID = "d" * 64
_NOW = datetime(2026, 7, 21, 12, 0, 0, tzinfo=UTC)


def _docker_version(*, server_os: str = "linux", server_arch: str = "amd64") -> bytes:
    return json.dumps(
        {
            "Client": {
                "Version": "28.3.2",
                "ApiVersion": "1.51",
                "Os": "linux",
                "Arch": "amd64",
            },
            "Server": {
                "Version": "28.3.2",
                "ApiVersion": "1.51",
                "Os": server_os,
                "Arch": server_arch,
            },
        }
    ).encode()


def _docker_info(
    *,
    server_os: str = "linux",
    server_arch: str = "x86_64",
    running: int = 0,
) -> bytes:
    return json.dumps(
        {
            "ID": _DAEMON_ID,
            "OSType": server_os,
            "Architecture": server_arch,
            "ContainersRunning": running,
            "IgnoredExtension": "allowed in raw Docker responses",
        }
    ).encode()


def _probe_paths(tmp_path: Path) -> tuple[Path, Path]:
    jobs_dir = tmp_path / "jobs"
    jobs_dir.mkdir()
    docker = tmp_path / "docker"
    docker.write_bytes(b"fake-docker")
    docker.chmod(0o755)
    return jobs_dir, docker


def _seams(
    *,
    now: datetime = _NOW,
    system: str = "Linux",
    machine: str = "x86_64",
    vcpus: int = 8,
    memory_bytes: int = 64 * 1024**3,
    free_disk_bytes: int = 300 * 1024**3,
    docker_os: str = "linux",
    docker_version_arch: str = "amd64",
    docker_info_arch: str = "x86_64",
    container_ids: tuple[str, ...] = (),
    reported_running: int | None = None,
) -> host.HostProbeSeams:
    total_disk = 500 * 1024**3
    reads = {
        "/etc/os-release": (
            'ID="ubuntu"\nVERSION_ID="24.04"\nPRETTY_NAME="Ubuntu 24.04 LTS"\n'
        ),
        "/proc/cpuinfo": "processor: 0\nmodel name: Test Xeon 9000\n",
        "/proc/meminfo": f"MemTotal: {memory_bytes // 1024} kB\n",
        "/etc/machine-id": _MACHINE_ID + "\n",
        "/proc/sys/kernel/random/boot_id": _BOOT_ID + "\n",
    }

    def run(command: list[str] | tuple[str, ...]) -> bytes:
        if command[1] == "version":
            return _docker_version(server_os=docker_os, server_arch=docker_version_arch)
        if command[1] == "info":
            return _docker_info(
                server_os=docker_os,
                server_arch=docker_info_arch,
                running=(
                    len(container_ids) if reported_running is None else reported_running
                ),
            )
        if command[1] == "ps":
            suffix = "\n" if container_ids else ""
            return ("\n".join(container_ids) + suffix).encode()
        raise AssertionError(f"unexpected command: {command!r}")

    return host.HostProbeSeams(
        system=lambda: system,
        machine=lambda: machine,
        kernel_release=lambda: "6.8.0-1018-azure",
        effective_vcpus=lambda: vcpus,
        read_text=lambda path: reads[str(path)],
        disk_usage=lambda _path: (
            total_disk,
            total_disk - free_disk_bytes,
            free_disk_bytes,
        ),
        run_command=run,
        now=lambda: now,
    )


def _report(
    tmp_path: Path,
    *,
    now: datetime = _NOW,
    seams: host.HostProbeSeams | None = None,
) -> tuple[dict[str, object], Path, Path]:
    jobs_dir, docker = _probe_paths(tmp_path)
    report = host.collect_public_host_report(
        intent_sha256=_INTENT,
        stage="calibration",
        job_name=_JOB_NAME,
        jobs_dir=jobs_dir,
        docker_executable=docker,
        seams=seams or _seams(now=now),
    )
    return report, jobs_dir, docker


def _binding(tmp_path: Path) -> tuple[dict[str, object], Path, dict[str, object]]:
    report, jobs_dir, docker = _report(tmp_path)
    public_raw = host.canonical_json_bytes(report)
    live = host.probe_host(
        jobs_dir=jobs_dir,
        docker_executable=docker,
        seams=_seams(now=_NOW + timedelta(seconds=60)),
    )
    binding = host.build_launch_binding(
        public_report_raw=public_raw,
        public_commit=_COMMIT,
        public_fetched_at_utc=(_NOW + timedelta(seconds=30))
        .isoformat(timespec="microseconds")
        .replace("+00:00", "Z"),
        launch_receipt_sha256=_RECEIPT_SHA256,
        live_recheck=live,
        expected_intent_sha256=_INTENT,
        expected_stage="calibration",
        expected_job_name=_JOB_NAME,
    )
    return binding, jobs_dir, report


def test_probe_builds_exact_passing_report_and_hides_private_host_ids(
    tmp_path: Path,
) -> None:
    report, _, _ = _report(tmp_path)

    assert set(report) == {
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
    assert report["checks"]["all_passed"] is True  # type: ignore[index]
    assert report["requirements"] == {
        "system": "Linux",
        "architecture": "x86_64",
        "min_vcpus": 4,
        "min_memory_bytes": 31 * 1024**3,
        "min_free_disk_bytes": 150 * 1024**3,
        "max_running_containers_before_launch": 0,
    }
    raw = host.canonical_json_bytes(report)
    assert (
        raw
        == (
            json.dumps(
                report,
                sort_keys=True,
                separators=(",", ":"),
                ensure_ascii=False,
            )
            + "\n"
        ).encode()
    )
    assert _MACHINE_ID.encode() not in raw
    assert _BOOT_ID.encode() not in raw
    assert _DAEMON_ID.encode() not in raw


def test_observed_31_gib_memtotal_accepts_nominal_32_gib_runner(
    tmp_path: Path,
) -> None:
    report, _, _ = _report(
        tmp_path,
        seams=_seams(memory_bytes=31 * 1024**3),
    )

    assert report["observed"]["memory"]["total_bytes"] == 31 * 1024**3  # type: ignore[index]
    assert report["checks"]["minimum_memory"] is True  # type: ignore[index]
    assert report["checks"]["all_passed"] is True  # type: ignore[index]


@pytest.mark.parametrize(
    ("seams", "match"),
    [
        (_seams(system="Darwin"), "native_linux_x86_64"),
        (_seams(machine="arm64"), "not native x86_64"),
        (_seams(vcpus=3), "minimum_vcpus"),
        (
            _seams(memory_bytes=host.MIN_MEMORY_BYTES - 1024),
            "minimum_memory",
        ),
        (
            _seams(free_disk_bytes=host.MIN_FREE_DISK_BYTES - 1),
            "minimum_free_disk",
        ),
        (_seams(docker_os="windows"), "docker_native_linux_x86_64"),
        (
            _seams(docker_version_arch="arm64", docker_info_arch="arm64"),
            "not native x86_64",
        ),
        (
            _seams(container_ids=(_CONTAINER_ID,)),
            "zero_running_containers",
        ),
    ],
)
def test_probe_fails_each_frozen_host_requirement(
    tmp_path: Path, seams: host.HostProbeSeams, match: str
) -> None:
    jobs_dir, docker = _probe_paths(tmp_path)
    with pytest.raises(host.HostAttestationError, match=match):
        host.collect_public_host_report(
            intent_sha256=_INTENT,
            stage="calibration",
            job_name=_JOB_NAME,
            jobs_dir=jobs_dir,
            docker_executable=docker,
            seams=seams,
        )


def test_probe_rejects_disagreement_between_docker_container_probes(
    tmp_path: Path,
) -> None:
    jobs_dir, docker = _probe_paths(tmp_path)
    with pytest.raises(host.HostAttestationError, match="probes disagree"):
        host.probe_host(
            jobs_dir=jobs_dir,
            docker_executable=docker,
            seams=_seams(reported_running=1),
        )


@pytest.mark.parametrize("mutation", ["extra", "missing"])
def test_public_report_rejects_schema_extension_or_omission(
    tmp_path: Path, mutation: str
) -> None:
    report, _, _ = _report(tmp_path)
    changed = deepcopy(report)
    if mutation == "extra":
        changed["extension"] = True
    else:
        del changed["checks"]

    with pytest.raises(host.HostAttestationError, match="exact frozen fields"):
        host.validate_public_host_report(changed)


@pytest.mark.parametrize(
    ("expectations", "match"),
    [
        ({"expected_intent_sha256": "f" * 64}, "intent does not match"),
        ({"expected_stage": "confirmatory"}, "stage does not match"),
        ({"expected_job_name": "wrong-job"}, "job_name does not match"),
    ],
)
def test_public_report_rejects_wrong_bound_identity(
    tmp_path: Path, expectations: dict[str, str], match: str
) -> None:
    report, _, _ = _report(tmp_path)
    with pytest.raises(host.HostAttestationError, match=match):
        host.validate_public_host_report(report, **expectations)


@pytest.mark.parametrize("offset_seconds", [-1, host.MAX_PUBLIC_REPORT_AGE_SECONDS + 1])
def test_public_report_rejects_future_or_stale_timestamp(
    tmp_path: Path, offset_seconds: int
) -> None:
    report, _, _ = _report(tmp_path)
    with pytest.raises(host.HostAttestationError, match="future-dated or stale"):
        host.validate_public_host_report(
            report,
            now=_NOW + timedelta(seconds=offset_seconds),
        )


def test_public_report_bytes_must_be_canonical_and_duplicate_free(
    tmp_path: Path,
) -> None:
    report, _, _ = _report(tmp_path)
    canonical = host.canonical_json_bytes(report)
    noncanonical = json.dumps(report, indent=2).encode()
    duplicate = canonical[:-2] + b',"stage":"calibration"}\n'

    host.parse_public_host_report_bytes(canonical)
    with pytest.raises(host.HostAttestationError):
        host.parse_public_host_report_bytes(noncanonical)
    with pytest.raises(host.HostAttestationError, match="repeats JSON field"):
        host.parse_public_host_report_bytes(duplicate)


def test_fingerprint_is_deterministic_domain_separated_and_private() -> None:
    first = host.host_fingerprint_sha256(
        machine_id=_MACHINE_ID,
        boot_id=_BOOT_ID,
        docker_daemon_id=_DAEMON_ID,
    )
    second = host.host_fingerprint_sha256(
        machine_id=_MACHINE_ID.upper(),
        boot_id=_BOOT_ID.upper(),
        docker_daemon_id=_DAEMON_ID,
    )
    changed = host.host_fingerprint_sha256(
        machine_id=_MACHINE_ID,
        boot_id=_BOOT_ID,
        docker_daemon_id=_DAEMON_ID + "-replacement",
    )

    assert first == second
    assert first != changed
    assert len(first) == 64
    assert _MACHINE_ID not in first


def test_launch_binding_binds_public_bytes_receipt_and_live_same_host(
    tmp_path: Path,
) -> None:
    binding, _, report = _binding(tmp_path)

    assert host.validate_launch_binding(binding) == binding
    assert binding["launch_receipt_sha256"] == _RECEIPT_SHA256
    public_reference = binding["public_report"]
    assert public_reference["path"] == host.public_report_path(_INTENT)  # type: ignore[index]
    assert (
        public_reference["sha256"]
        == hashlib.sha256(  # type: ignore[index]
            host.canonical_json_bytes(report)
        ).hexdigest()
    )


def test_launch_binding_rejects_public_byte_drift(tmp_path: Path) -> None:
    report, jobs_dir, docker = _report(tmp_path)
    live = host.probe_host(
        jobs_dir=jobs_dir,
        docker_executable=docker,
        seams=_seams(now=_NOW + timedelta(seconds=60)),
    )
    raw = host.canonical_json_bytes(report) + b" "

    with pytest.raises(host.HostAttestationError):
        host.build_launch_binding(
            public_report_raw=raw,
            public_commit=_COMMIT,
            public_fetched_at_utc="2026-07-21T12:00:30.000000Z",
            launch_receipt_sha256=_RECEIPT_SHA256,
            live_recheck=live,
            expected_intent_sha256=_INTENT,
            expected_stage="calibration",
            expected_job_name=_JOB_NAME,
        )


def test_launch_binding_rejects_fingerprint_or_static_host_drift(
    tmp_path: Path,
) -> None:
    binding, _, _ = _binding(tmp_path)
    changed_fingerprint = deepcopy(binding)
    changed_fingerprint["live_recheck"]["host_fingerprint_sha256"] = "e" * 64  # type: ignore[index]
    with pytest.raises(host.HostAttestationError, match="not the public host/boot"):
        host.validate_launch_binding(changed_fingerprint)

    changed_cpu = deepcopy(binding)
    changed_cpu["live_recheck"]["observed"]["cpu"]["model"] = "Other CPU"  # type: ignore[index]
    with pytest.raises(host.HostAttestationError, match="not the public host/boot"):
        host.validate_launch_binding(changed_cpu)


def test_launch_binding_rejects_live_snapshot_schema_extension(
    tmp_path: Path,
) -> None:
    binding, _, _ = _binding(tmp_path)
    binding["live_recheck"]["extension"] = True  # type: ignore[index]

    with pytest.raises(host.HostAttestationError, match="exact frozen fields"):
        host.validate_launch_binding(binding)


def test_launch_binding_rejects_invalid_chronology(tmp_path: Path) -> None:
    binding, _, _ = _binding(tmp_path)
    changed = deepcopy(binding)
    changed["live_recheck"]["captured_at_utc"] = "2026-07-21T12:00:20.000000Z"  # type: ignore[index]

    with pytest.raises(host.HostAttestationError, match="chronology"):
        host.validate_launch_binding(changed)


def test_sidecar_is_atomic_owner_only_exact_and_refuses_overwrite(
    tmp_path: Path,
) -> None:
    binding, _, _ = _binding(tmp_path)
    job_dir = tmp_path / "paid-job"
    job_dir.mkdir(mode=0o700)

    path = host.write_launch_binding_sidecar(
        job_dir.resolve(), binding, forbidden_values=("provider-super-secret",)
    )
    assert path.name == host.HOST_ATTESTATION_FILENAME
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    assert path.read_bytes() == host.canonical_json_bytes(binding)
    assert list(job_dir.glob(f".{host.HOST_ATTESTATION_FILENAME}.*.tmp")) == []

    original = path.read_bytes()
    with pytest.raises(FileExistsError, match="already exists"):
        host.write_launch_binding_sidecar(job_dir.resolve(), binding)
    assert path.read_bytes() == original


def test_sidecar_refuses_forbidden_secret_without_creating_a_file(
    tmp_path: Path,
) -> None:
    binding, _, _ = _binding(tmp_path)
    secret = "provider-super-secret"
    binding["live_recheck"]["observed"]["cpu"]["model"] = secret  # type: ignore[index]
    binding["public_report_payload"]["observed"]["cpu"]["model"] = secret  # type: ignore[index]
    binding["public_report"]["sha256"] = hashlib.sha256(  # type: ignore[index]
        host.canonical_json_bytes(binding["public_report_payload"])  # type: ignore[arg-type]
    ).hexdigest()
    job_dir = tmp_path / "paid-job"
    job_dir.mkdir(mode=0o700)

    with pytest.raises(host.HostAttestationError, match="forbidden value"):
        host.write_launch_binding_sidecar(
            job_dir.resolve(), binding, forbidden_values=(secret,)
        )
    assert not (job_dir / host.HOST_ATTESTATION_FILENAME).exists()


def test_probe_seams_are_replaceable_without_touching_real_host(
    tmp_path: Path,
) -> None:
    jobs_dir, docker = _probe_paths(tmp_path)
    seams = replace(_seams(), now=lambda: _NOW + timedelta(seconds=1))

    snapshot = host.probe_host(jobs_dir=jobs_dir, docker_executable=docker, seams=seams)
    assert snapshot["captured_at_utc"] == "2026-07-21T12:00:01.000000Z"
