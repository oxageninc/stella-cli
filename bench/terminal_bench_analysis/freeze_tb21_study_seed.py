#!/usr/bin/env python3
"""Freeze the TB 2.1 task identities from pinned, local evidence only."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from collections import Counter
from pathlib import Path

import harbor
from harbor.models.task.task import Task

LEADERBOARD_JOB_ID = "fd8707bb-51e8-56fa-8e46-769a82a531ae"
TASK_SET_HASH_DOMAIN = "stella-tb21-task-set-v1"
EXPECTED_TRIAL_COUNT = 445
PINNED_TASK_SET_SHA256 = (
    "7e495afe0a86eaf572be1c2da2b9929c24e502adc888e550385d915cc0125ece"
)
CONTROL_SHA256 = {
    "manifest.json": "7963a7af2b306fd4b6e82963fdadf9374e701ec16f47b194300e2843c8002a76",
    "submission.json": (
        "36d20c181be246dc55965bf4320a3005f292c737f31511bbde19ba1808a2bd2c"
    ),
    "audit.json": "2c214bbeff6963a8a4e54f7bf0c2f8e76c8dac31053a3d40d036c2e284f12687",
}
_MANIFEST_FIELDS = {
    "leaderboard_job_id",
    "submission_url",
    "entries",
    "failures",
}
_MANIFEST_ENTRY_FIELDS = {
    "submitted_trial_id",
    "result_id",
    "trial_name",
    "task_name",
    "reward",
    "error_type",
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "cost_usd",
    "trajectory_schema",
    "trajectory_steps",
    "trajectory_metrics",
    "result_url",
    "trajectory_url",
    "result_sha256",
    "trajectory_sha256",
    "result_bytes",
    "trajectory_bytes",
}
_RESULT_FIELDS = {
    "agent_execution",
    "agent_info",
    "agent_result",
    "agent_setup",
    "config",
    "environment_setup",
    "exception_info",
    "finished_at",
    "id",
    "source",
    "started_at",
    "step_results",
    "task_checksum",
    "task_id",
    "task_name",
    "trial_name",
    "trial_uri",
    "verifier",
    "verifier_result",
}
_TASK_ID_FIELDS = {"org", "name", "ref"}
_HEX_SHA256_RE = re.compile(r"[0-9a-f]{64}")
_TASK_REF_RE = re.compile(r"sha256:[0-9a-f]{64}")

TaskIdentity = tuple[str, str, str]


def _sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _task_set_sha256(identities: tuple[TaskIdentity, ...]) -> str:
    tasks = [
        {"task": name, "task_ref": task_ref, "task_checksum": checksum}
        for name, task_ref, checksum in sorted(identities)
    ]
    encoded = json.dumps(
        {"schema": TASK_SET_HASH_DOMAIN, "tasks": tasks},
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return _sha256(encoded)


def _reject_duplicate_pairs(pairs: list[tuple[str, object]]) -> dict[str, object]:
    value: dict[str, object] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON key {key!r}")
        value[key] = item
    return value


def _load_json_object(path: Path) -> dict[str, object]:
    try:
        value = json.loads(path.read_bytes(), object_pairs_hook=_reject_duplicate_pairs)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, ValueError) as exc:
        raise RuntimeError(f"cannot load strict JSON object {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise RuntimeError(f"{path} must contain a JSON object")
    return value


def _require_fields(value: dict[str, object], expected: set[str], label: str) -> None:
    actual = set(value)
    if actual != expected:
        raise RuntimeError(
            f"{label} fields differ: "
            f"missing={sorted(expected - actual)!r}, "
            f"extra={sorted(actual - expected)!r}"
        )


def _load_control_files(comparator_dir: Path) -> dict[str, dict[str, object]]:
    controls: dict[str, dict[str, object]] = {}
    for filename, expected_digest in CONTROL_SHA256.items():
        path = comparator_dir / filename
        try:
            raw = path.read_bytes()
        except OSError as exc:
            raise RuntimeError(
                f"cannot read pinned control file {path}: {exc}"
            ) from exc
        actual_digest = _sha256(raw)
        if actual_digest != expected_digest:
            raise RuntimeError(
                f"pinned control digest differs for {filename}: "
                f"{actual_digest} != {expected_digest}"
            )
        controls[filename] = _load_json_object(path)
    return controls


def _verify_public_job_identity(controls: dict[str, dict[str, object]]) -> None:
    manifest = controls["manifest.json"]
    submission = controls["submission.json"]
    audit = controls["audit.json"]
    if manifest.get("leaderboard_job_id") != LEADERBOARD_JOB_ID:
        raise RuntimeError("manifest does not identify the pinned leaderboard job")
    if submission.get("source_jobs") != [LEADERBOARD_JOB_ID]:
        raise RuntimeError("submission does not identify the pinned leaderboard job")
    identity = audit.get("identity")
    if (
        not isinstance(identity, dict)
        or identity.get("leaderboard_job_id") != LEADERBOARD_JOB_ID
    ):
        raise RuntimeError("audit does not identify the pinned leaderboard job")


def _manifest_trials(
    comparator_dir: Path,
    controls: dict[str, dict[str, object]],
) -> list[TaskIdentity]:
    manifest = controls["manifest.json"]
    submission = controls["submission.json"]
    _require_fields(manifest, _MANIFEST_FIELDS, "manifest")
    entries = manifest["entries"]
    failures = manifest["failures"]
    submitted_ids = submission.get("trials")
    if not isinstance(entries, list) or not isinstance(submitted_ids, list):
        raise RuntimeError("manifest entries and submission trials must be arrays")
    if failures != []:
        raise RuntimeError("manifest contains failed downloads")
    if (
        len(entries) != EXPECTED_TRIAL_COUNT
        or len(submitted_ids) != EXPECTED_TRIAL_COUNT
    ):
        raise RuntimeError(
            f"expected exactly {EXPECTED_TRIAL_COUNT} manifest entries and "
            "submitted trials"
        )
    if any(not isinstance(trial_id, str) for trial_id in submitted_ids):
        raise RuntimeError("submission trial IDs must all be strings")
    if len(set(submitted_ids)) != EXPECTED_TRIAL_COUNT:
        raise RuntimeError("submission trial IDs are not unique")

    entry_ids: list[str] = []
    identities: list[TaskIdentity] = []
    trials_dir = comparator_dir / "trials"
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            raise RuntimeError(f"manifest entry {index} must be an object")
        _require_fields(entry, _MANIFEST_ENTRY_FIELDS, f"manifest entry {index}")
        trial_id = entry["submitted_trial_id"]
        if not isinstance(trial_id, str):
            raise RuntimeError(f"manifest entry {index} has a non-string trial ID")
        entry_ids.append(trial_id)
        result_path = trials_dir / trial_id / "result.json"
        try:
            result_raw = result_path.read_bytes()
        except OSError as exc:
            raise RuntimeError(
                f"cannot read comparator result {result_path}: {exc}"
            ) from exc
        result_digest = entry["result_sha256"]
        if not isinstance(result_digest, str) or not _HEX_SHA256_RE.fullmatch(
            result_digest
        ):
            raise RuntimeError(f"manifest entry {index} has an invalid result digest")
        if _sha256(result_raw) != result_digest:
            raise RuntimeError(f"result digest differs for submitted trial {trial_id}")
        if entry["result_bytes"] != len(result_raw):
            raise RuntimeError(
                f"result byte size differs for submitted trial {trial_id}"
            )

        result = _load_json_object(result_path)
        _require_fields(result, _RESULT_FIELDS, f"result {trial_id}")
        task_id = result["task_id"]
        if not isinstance(task_id, dict):
            raise RuntimeError(f"result {trial_id} task_id must be an object")
        _require_fields(task_id, _TASK_ID_FIELDS, f"result {trial_id} task_id")
        task_name = task_id["name"]
        task_ref = task_id["ref"]
        task_checksum = result["task_checksum"]
        if not isinstance(task_name, str) or not task_name:
            raise RuntimeError(f"result {trial_id} has an invalid task name")
        if task_id["org"] != "terminal-bench":
            raise RuntimeError(f"result {trial_id} has an unexpected task org")
        if not isinstance(task_ref, str) or not _TASK_REF_RE.fullmatch(task_ref):
            raise RuntimeError(f"result {trial_id} has an invalid task ref")
        if not isinstance(task_checksum, str) or not _HEX_SHA256_RE.fullmatch(
            task_checksum
        ):
            raise RuntimeError(f"result {trial_id} has an invalid task checksum")
        expected_qualified_name = f"terminal-bench/{task_name}"
        if result["task_name"] != expected_qualified_name:
            raise RuntimeError(f"result {trial_id} has an inconsistent task name")
        if entry["task_name"] != expected_qualified_name:
            raise RuntimeError(
                f"manifest entry {trial_id} has an inconsistent task name"
            )
        if result["id"] != entry["result_id"]:
            raise RuntimeError(
                f"manifest entry {trial_id} has an inconsistent result ID"
            )
        if result["trial_name"] != entry["trial_name"]:
            raise RuntimeError(
                f"manifest entry {trial_id} has an inconsistent trial name"
            )
        identities.append((task_name, task_ref, task_checksum))

    if len(set(entry_ids)) != EXPECTED_TRIAL_COUNT or set(entry_ids) != set(
        submitted_ids
    ):
        raise RuntimeError("manifest trials differ from the submitted trial allowlist")
    actual_trial_entries = {path.name for path in trials_dir.iterdir()}
    if actual_trial_entries != set(submitted_ids) or not all(
        (trials_dir / trial_id).is_dir() for trial_id in submitted_ids
    ):
        raise RuntimeError("comparator trials directory has extra or missing trials")
    return identities


def _stable_task_identities(trials: list[TaskIdentity]) -> tuple[TaskIdentity, ...]:
    by_name: dict[str, set[TaskIdentity]] = {}
    for identity in trials:
        by_name.setdefault(identity[0], set()).add(identity)
    counts = Counter(identity[0] for identity in trials)
    if len(by_name) != 89 or any(count != 5 for count in counts.values()):
        raise RuntimeError("expected exactly 89 tasks with five trials each")
    unstable = sorted(name for name, values in by_name.items() if len(values) != 1)
    if unstable:
        raise RuntimeError(f"tasks have unstable identity fields: {unstable!r}")
    return tuple(sorted(next(iter(values)) for values in by_name.values()))


def _verify_local_dataset(
    dataset_dir: Path, identities: tuple[TaskIdentity, ...]
) -> None:
    if harbor.__version__ != "0.6.1":
        raise RuntimeError(f"Harbor 0.6.1 is required, found {harbor.__version__!r}")
    expected_names = {identity[0] for identity in identities}
    actual_names = {path.name for path in dataset_dir.iterdir()}
    if actual_names != expected_names or not all(
        (dataset_dir / name).is_dir() for name in expected_names
    ):
        raise RuntimeError("local dataset has extra or missing task directories")
    locally_verified: list[TaskIdentity] = []
    for task_name, task_ref, expected_checksum in identities:
        actual_checksum = Task(dataset_dir / task_name).checksum
        if actual_checksum != expected_checksum:
            raise RuntimeError(
                f"Harbor task checksum differs for {task_name}: "
                f"{actual_checksum} != {expected_checksum}"
            )
        locally_verified.append((task_name, task_ref, actual_checksum))
    if _task_set_sha256(tuple(locally_verified)) != PINNED_TASK_SET_SHA256:
        raise RuntimeError(
            "locally checksum-verified tasks differ from the pinned task identity "
            "binding"
        )


def _source_bytes(identities: tuple[TaskIdentity, ...]) -> bytes:
    records = "".join(
        "    (\n"
        f"        {json.dumps(name)},\n"
        f"        {json.dumps(task_ref)},\n"
        f"        {json.dumps(checksum)},\n"
        "    ),\n"
        for name, task_ref, checksum in identities
    )
    source = f'''\
"""Frozen Terminal-Bench 2.1 task identities for the hybrid study."""

from __future__ import annotations

import hashlib
import json
from collections.abc import Sequence

TaskIdentity = tuple[str, str, str]

# Internal hash-domain label; not an artifact schema version.
TASK_SET_HASH_DOMAIN = {json.dumps(TASK_SET_HASH_DOMAIN)}

TASK_IDENTITIES: tuple[TaskIdentity, ...] = (
{records})


def task_set_sha256(identities: Sequence[TaskIdentity]) -> str:
    """Hash a task identity set using the frozen study representation."""
    tasks = [
        {{"task": name, "task_ref": task_ref, "task_checksum": checksum}}
        for name, task_ref, checksum in sorted(identities)
    ]
    encoded = json.dumps(
        {{"schema": TASK_SET_HASH_DOMAIN, "tasks": tasks}},
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()
'''
    return source.encode("utf-8")


def _create_or_verify_identical(output: Path, raw: bytes) -> str:
    try:
        with output.open("xb") as handle:
            handle.write(raw)
    except FileExistsError as exc:
        if output.read_bytes() != raw:
            raise RuntimeError(
                f"refusing to overwrite non-identical output {output}"
            ) from exc
        return "identical"
    return "created"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--comparator-dir", required=True, type=Path)
    parser.add_argument("--dataset-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    comparator_dir = args.comparator_dir.resolve(strict=True)
    dataset_dir = args.dataset_dir.resolve(strict=True)
    controls = _load_control_files(comparator_dir)
    _verify_public_job_identity(controls)
    trials = _manifest_trials(comparator_dir, controls)
    identities = _stable_task_identities(trials)
    _verify_local_dataset(dataset_dir, identities)
    disposition = _create_or_verify_identical(args.output, _source_bytes(identities))
    print(f"verified {len(trials)} trials and {len(identities)} tasks; {disposition}")


if __name__ == "__main__":
    main()
