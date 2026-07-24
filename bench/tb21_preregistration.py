#!/usr/bin/env python3
"""Generate the Terminal-Bench 2.1 v2 preregistration package.

There is deliberately no generator in the frozen tree, and the six GitHub comment
bodies + the append-only run ledger must be *byte-exact* in a single public push
(a corrected re-push muddies the timestamp the timing verifier binds). Hand-
authoring 15-field intent objects + JCS SHA-256 digests is a foot-gun. This tool
removes it: it reuses the launcher's OWN functions —

  stella_harbor.secure_launcher._canonical_payload_sha256   (the exact intent digest)
  stella_harbor.secure_launcher._validate_current_intent    (the exact launch contract)
  stella_harbor.secure_launcher._expected_stage_dataset     (frozen dataset identity)
  stella_harbor.__init__._benchmark_engine_posture          (the 0.5.1 posture hashes)
  ... plus the frozen constants (study id, key label/limit, budget)

so every digest it emits is exactly what the launcher recomputes at preflight, and
each intent it builds is checked against `_validate_current_intent` before it is
written. Author to `stella-tb21-run-ledger-v2` ONLY — never the v3 hybrid contract.

Run inside the adapter venv:
    cd bench/harbor_adapter
    uv run --no-sync python ../tb21_preregistration.py frozen
    uv run --no-sync python ../tb21_preregistration.py emit --host-inputs h.json --out-dir ../evidence/prereg

Modes
-----
frozen   Print every offline-knowable value (study id, per-stage dataset identity,
         engine-posture hashes, adapter/analyzer/timing/harbor source hashes). No
         inputs, no network, no spend. Use it to fill the study manifest + sanity
         check the tree before you touch a key.

emit     Given a host-inputs JSON (the values only knowable on the built host — see
         --host-inputs schema below), write:
           issue-body.md            the preregistration issue body
           comment-<kind>-prereg.json / comment-<kind>-intent.json   the six comments
           run-ledger.json          the v2 ledger (preregistrations + intents;
                                     publications/outcomes are appended live per stage)
         and print the three intent_sha256 digests (they go into each stage's
         --intent-sha256 and the intent comment subject_id). Each intent is
         self-validated against the launcher contract before writing.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

# --- make stella_harbor importable whether run from repo root or bench/ ---
_HERE = Path(__file__).resolve().parent
for _p in (_HERE / "harbor_adapter", _HERE.parent):
    if (Path(_p) / "stella_harbor").is_dir() and str(_p) not in sys.path:
        sys.path.insert(0, str(_p))

from stella_harbor import (  # noqa: E402
    _adapter_content_sha256,
    _benchmark_engine_posture,
    _harbor_content_sha256,
    _harbor_version,
)
from stella_harbor.secure_launcher import (  # noqa: E402
    _CANONICAL_BUDGET,
    _DEDICATED_KEY_HARD_LIMIT_USD,
    _DEDICATED_KEY_LABEL,
    _FIXED_ANALYZER_PATH,
    _FIXED_PUBLIC_TIMING_PATH,
    _FIXED_STUDY_ID,
    _canonical_payload_sha256,
    _expected_stage_dataset,
    _validate_current_intent,
)

# ---------------------------------------------------------------------------
# Frozen, stage-invariant facts (mirrored from the launcher's command contract).
# ---------------------------------------------------------------------------
LEDGER_SCHEMA = "stella-tb21-run-ledger-v2"
COMMENT_SCHEMA = "stella-tb21-github-attestation-v2"
LEDGER_PATH = "bench/evidence/stella-tb21-run-ledger.json"
BASE_URL = "https://openrouter.ai/api/v1"
ROUTE_POLICY = "openrouter-auto"
HISTORICAL_SPEND = {
    "known_lower_bound_usd": 0.2429614978,
    "unknown_cancellation_spend": True,
    "new_authorized_budget_usd": 200.0,
}

# stage -> (prereg kind, job-name key, models, attempts, n_concurrent, requested)
STAGES = {
    "readiness": {
        "prereg_kind": "readiness",
        "models": ["openrouter/deepseek/deepseek-v4-pro"],
        "attempts": 1,
        "n_concurrent": 1,
        "requested": 1,
        "path_based": True,
        "default_job": "stella-readiness-synthetic-v1",
    },
    "calibration": {
        "prereg_kind": "calibration",
        "models": [
            "openrouter/deepseek/deepseek-v4-pro",
            "openrouter/z-ai/glm-5.2",
            "openrouter/x-ai/grok-4.5",
        ],
        "attempts": 2,
        "n_concurrent": 3,
        "requested": 60,
        "path_based": False,
        "default_job": "stella-tb21-calibration-20260721",
    },
    "confirmatory": {
        "prereg_kind": "confirmatory_freeze",
        "models": ["openrouter/z-ai/glm-5.1"],
        "attempts": 5,
        "n_concurrent": 1,
        "requested": 445,
        "path_based": False,
        "default_job": None,  # PRIMARY_JOB_NAME — host input
    },
}
_CANONICAL_DATASET = (
    "terminal-bench/terminal-bench-2-1@sha256:"
    "7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a"
)
_READINESS_PATH = "bench/readiness/synthetic-adapter-sentinel"


_REPO_ROOT = _HERE.parent  # this script lives at <repo>/bench/tb21_preregistration.py


def _sha256_file(rel: str) -> str:
    return hashlib.sha256((_REPO_ROOT / rel).read_bytes()).hexdigest()


def _frozen_source_hashes() -> dict:
    """The artifact hashes the launcher would compute from this source tree."""
    return {
        "adapter_sha256": _adapter_content_sha256(),
        "analysis_sha256": _sha256_file(_FIXED_ANALYZER_PATH),
        "public_timing_sha256": _sha256_file(_FIXED_PUBLIC_TIMING_PATH),
        "harbor_version": _harbor_version(),
        "harbor_sha256": _harbor_content_sha256(),
    }


def _posture_map(models: list[str]) -> dict:
    return {m: _benchmark_engine_posture(m)[2] for m in models}


def _stage_command(stage: str, job_name: str, intent_sha256: str) -> list[str]:
    """Reconstruct the exact launcher argv so _validate_current_intent parses it."""
    s = STAGES[stage]
    cmd = ["harbor", "run", "--env", "docker"]
    if s["path_based"]:
        cmd += ["--path", _READINESS_PATH]
    else:
        cmd += ["--dataset", _CANONICAL_DATASET]
        if stage == "calibration":
            from stella_harbor.secure_launcher import _CALIBRATION_TASK_FILTERS

            for t in _CALIBRATION_TASK_FILTERS:
                cmd += ["--include-task-name", t]
    cmd += ["--agent-import-path", "stella_harbor:StellaAgent"]
    for m in s["models"]:
        cmd += ["--model", m]
    cmd += [
        "--job-name", job_name,
        "--jobs-dir", "/srv/stella-tb21-jobs",
        "--intent-sha256", intent_sha256,
        "--intent-comment-url",
        "https://github.com/macanderson/stella/issues/1#issuecomment-1",
        "--n-attempts", str(s["attempts"]),
        "--n-concurrent", str(s["n_concurrent"]),
        "--max-retries", "0",
    ]
    return cmd


def _build_intent(stage: str, hi: dict, source_hashes: dict) -> dict:
    s = STAGES[stage]
    job_name = hi["primary_job_name"] if stage == "confirmatory" else s["default_job"]
    per = hi["per_stage"][stage]
    artifacts = {
        "binary_sha256": hi["binary_sha256"],
        "source_commit": hi["source_commit"],
        "agent_version": hi["agent_version"],
        "adapter_version": hi["adapter_version"],
        **source_hashes,
        "engine_posture_sha256_by_model": _posture_map(s["models"]),
    }
    return {
        "intent_id": f"stella-tb21-{stage}-v6",
        "stage": stage,
        "historical": False,
        "job_name": job_name,
        "models": list(s["models"]),
        "dataset": _expected_stage_dataset(stage),
        "requested_trials": s["requested"],
        "attempts_per_task": s["attempts"],
        "n_concurrent_trials": s["n_concurrent"],
        "retry_max_retries": 0,
        "per_trial_budget_usd": float(_CANONICAL_BUDGET),
        "artifacts": artifacts,
        "execution": {
            "base_url": BASE_URL,
            "provider_route_policy": ROUTE_POLICY,
            "disable_reflection": True,
        },
        "provider_key": {
            "fingerprint_sha256": hi["provider_key_fingerprint_sha256"],
            "label": _DEDICATED_KEY_LABEL,
            "limit_usd": _DEDICATED_KEY_HARD_LIMIT_USD,
            "usage_before_usd": per["usage_before_usd"],
            "snapshot_at": per["snapshot_at"],
        },
        "declared_at": per["declared_at"],
        "preregistration_commit": hi["subject_commit"],
    }


def _runtime_identity(intent: dict, hi: dict) -> dict:
    a = intent["artifacts"]
    return {
        **{k: a[k] for k in a},
        "base_url": BASE_URL,
        "provider_route_policy": ROUTE_POLICY,
        "disable_reflection": True,
        "provider_key_fingerprint_sha256": hi["provider_key_fingerprint_sha256"],
        "source_commit": hi["source_commit"],
    }


def _comment(subject_type: str, subject_id: str, kind: str, hi: dict, extra: dict) -> dict:
    return {
        "schema_version": COMMENT_SCHEMA,
        "study_id": _FIXED_STUDY_ID,
        "subject_type": subject_type,
        "subject_id": subject_id,
        "kind": kind,
        "subject_commit": hi["subject_commit"],
        "ledger_commit": "REPLACE_WITH_LEDGER_COMMIT_SHA40",
        "ledger_path": LEDGER_PATH,
        **extra,
    }


def do_frozen() -> int:
    print(f"study_id                : {_FIXED_STUDY_ID}")
    print(f"ledger schema           : {LEDGER_SCHEMA}")
    print(f"ledger path             : {LEDGER_PATH}")
    print(f"per-trial budget (usd)  : {_CANONICAL_BUDGET}")
    print(f"dedicated key label     : {_DEDICATED_KEY_LABEL}")
    print(f"dedicated key limit usd : {_DEDICATED_KEY_HARD_LIMIT_USD}")
    print("\nfrozen source/artifact hashes (as the launcher computes them):")
    for k, v in _frozen_source_hashes().items():
        print(f"  {k:22}: {v}")
    print("\nper-stage dataset identity + engine-posture hashes:")
    for stage, s in STAGES.items():
        print(f"  [{stage}]  dataset = {json.dumps(_expected_stage_dataset(stage))}")
        for m, h in _posture_map(s["models"]).items():
            print(f"           posture {m} = {h}")
    return 0


def do_emit(host_inputs: Path, out_dir: Path) -> int:
    hi = json.loads(host_inputs.read_text())
    out_dir.mkdir(parents=True, exist_ok=True)
    source_hashes = _frozen_source_hashes()

    digests: dict[str, str] = {}
    intents_array = []
    intent_comments = []
    for seq, stage in enumerate(("readiness", "calibration", "confirmatory"), start=1):
        intent = _build_intent(stage, hi, source_hashes)
        digest = _canonical_payload_sha256(intent)
        digests[stage] = digest
        job_name = intent["job_name"]
        # correct-by-construction check against the real launch contract:
        _validate_current_intent(
            intent,
            command=_stage_command(stage, job_name, digest),
            subject_commit=hi["subject_commit"],
            runtime_identity=_runtime_identity(intent, hi),
        )
        intents_array.append({"sequence": seq, "intent": intent, "intent_sha256": digest})
        intent_comments.append(
            _comment("intent", digest, stage, hi, {"intent_sha256": digest})
        )

    # three preregistration comments (readiness, calibration, confirmatory_freeze)
    prereg_entries = []
    prereg_comments = []
    for seq, stage in enumerate(("readiness", "calibration", "confirmatory"), start=1):
        kind = STAGES[stage]["prereg_kind"]
        commit = (
            hi["confirmatory_freeze_commit"] if kind == "confirmatory_freeze"
            else hi["subject_commit"]
        )
        entry = {
            "sequence": seq,
            "kind": kind,
            "commit": commit,
            "study_manifest_sha256": hi["study_manifest_sha256"],
            "declared_at": hi["per_stage"][stage]["declared_at"],
        }
        prereg_entries.append(entry)
        payload_digest = _canonical_payload_sha256(entry)
        prereg_comments.append(
            _comment("preregistration", f"{kind}-prereg", kind, hi,
                     {"canonical_payload_sha256": payload_digest})
        )

    ledger = {
        "schema_version": LEDGER_SCHEMA,
        "study_id": _FIXED_STUDY_ID,
        "ledger_path": LEDGER_PATH,
        "historical_spend_disclosure": HISTORICAL_SPEND,
        "preregistrations": prereg_entries,
        "intents": intents_array,
        "publications": [],  # appended live at each publish (subject_type/id, ledger_commit, public_url, published_at)
        "outcomes": [],      # appended live after each job completes
    }

    (out_dir / "run-ledger.json").write_text(json.dumps(ledger, indent=2) + "\n")
    for c in prereg_comments:
        (out_dir / f"comment-{c['kind']}-prereg.json").write_text(json.dumps(c, indent=2) + "\n")
    for c in intent_comments:
        (out_dir / f"comment-{c['kind']}-intent.json").write_text(json.dumps(c, indent=2) + "\n")
    (out_dir / "issue-body.md").write_text(
        f"# Stella Terminal-Bench 2.1 preregistration: {_FIXED_STUDY_ID}\n\n"
        "This issue is the immutable, owner-authored preregistration for the public\n"
        "Stella Terminal-Bench 2.1 row. It carries exactly six machine-readable\n"
        "comments (three preregistrations, three paid intents); each is unedited\n"
        "(`created_at == updated_at`). The append-only ledger is\n"
        f"`{LEDGER_PATH}`. Do not edit any comment after posting.\n"
    )

    print("Emitted preregistration package to", out_dir)
    print("Intent digests (each is one stage's --intent-sha256 and its intent comment subject_id):")
    for stage, d in digests.items():
        print(f"  {stage:12}: {d}")
    print("\nEach intent passed the launcher's _validate_current_intent contract.")
    print("Fill each comment's ledger_commit before posting; append publications/outcomes live.")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="TB2.1 v2 preregistration package generator")
    sub = ap.add_subparsers(dest="mode", required=True)
    sub.add_parser("frozen", help="print offline-knowable frozen values")
    e = sub.add_parser("emit", help="emit the full package from a host-inputs JSON")
    e.add_argument("--host-inputs", required=True, type=Path)
    e.add_argument("--out-dir", required=True, type=Path)
    args = ap.parse_args()
    if args.mode == "frozen":
        return do_frozen()
    return do_emit(args.host_inputs, args.out_dir)


if __name__ == "__main__":
    raise SystemExit(main())
