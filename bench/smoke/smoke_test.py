#!/usr/bin/env python3
"""Offline smoke test for the Stella benchmark adapters.

Drives the compiled ``stella`` binary and the SWE-bench harness with **no API
key and no paid model calls**, asserting that the pieces the benchmark
adapters depend on behave as expected:

- ``stella --version`` / ``--help`` / ``models`` work offline;
- the exact one-shot invocation shape the adapters use is accepted by the CLI
  and, with no provider credentials, **degrades gracefully** (a clean non-zero
  exit with a provider/credential error) rather than crashing;
- ``run_swebench.py --dry-run`` wires up end-to-end against the bundled sample.

The point is CI-ability: a contributor (or a CI job) can verify adapter wiring
without spending a cent. A missing API key is treated as an EXPECTED
environment condition, not a failure — only a real crash (panic / signal) or a
broken CLI contract fails the smoke test.

Usage::

    python3 bench/smoke/smoke_test.py                 # auto-locate the binary
    python3 bench/smoke/smoke_test.py --stella-bin ./target/release/stella
    STELLA_BINARY=/path/to/stella python3 bench/smoke/smoke_test.py

Exit code is 0 iff every check passed (skips count as passes).
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# Provider credential / addressing vars scrubbed for the no-key check, so a key
# present in the developer's shell does not mask the graceful-degradation path.
_PROVIDER_ENV_VARS = (
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "XAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "ZAI_API_KEY",
    "ZAI_GLM_CODING_PLAN",
    "OPENROUTER_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "AI_GATEWAY_API_KEY",
    "VERTEX_ACCESS_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "LOCAL_API_KEY",
    "STELLA_MODEL",
    "STELLA_BASE_URL",
    "STELLA_API_KEY",
)

# Substrings that identify a graceful "no credentials / provider" failure.
_PROVIDER_ERROR_MARKERS = (
    "no api key",
    "api key",
    "credential",
    "provider",
    "stella:",
)

# Substrings that identify a genuine crash (never acceptable).
_CRASH_MARKERS = (
    "panicked",
    "rust_backtrace",
    "segmentation fault",
    "core dumped",
)

_VERSION_RE = re.compile(r"^stella\s+\d+\.\d+\.\d+")

# bench/smoke/smoke_test.py -> repo root is three parents up.
_BENCH_DIR = Path(__file__).resolve().parent.parent
_REPO_ROOT = _BENCH_DIR.parent
_SAMPLE_INSTANCES = _BENCH_DIR / "instances.sample.jsonl"
_RUN_SWEBENCH = _BENCH_DIR / "run_swebench.py"


@dataclass
class Check:
    name: str
    ok: bool
    detail: str
    skipped: bool = False


def locate_stella(explicit: str | None) -> Path | None:
    """Resolve the stella binary: explicit arg, STELLA_BINARY, PATH, target/."""
    if explicit:
        candidate = Path(explicit)
        return candidate if candidate.is_file() else None
    env = os.environ.get("STELLA_BINARY")
    if env and Path(env).is_file():
        return Path(env)
    on_path = shutil.which("stella")
    if on_path:
        return Path(on_path)
    local = _REPO_ROOT / "target" / "release" / "stella"
    return local if local.is_file() else None


def _scrubbed_env() -> dict[str, str]:
    """A copy of the environment with every provider credential removed."""
    env = dict(os.environ)
    for key in _PROVIDER_ENV_VARS:
        env.pop(key, None)
    return env


def _run(
    cmd: list[str],
    *,
    env: dict[str, str] | None = None,
    cwd: str | None = None,
    timeout: int = 90,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd,
        env=env,
        cwd=cwd,
        stdin=subprocess.DEVNULL,  # never block on an interactive prompt
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=timeout,
        check=False,
    )


def check_version(stella: Path) -> Check:
    try:
        proc = _run([str(stella), "--version"])
    except subprocess.TimeoutExpired:
        return Check("stella --version", False, "timed out")
    out = (proc.stdout or "").strip()
    ok = proc.returncode == 0 and bool(_VERSION_RE.match(out))
    return Check("stella --version", ok, out or f"exit={proc.returncode}")


def check_help(stella: Path) -> Check:
    try:
        proc = _run([str(stella), "--help"])
    except subprocess.TimeoutExpired:
        return Check("stella --help", False, "timed out")
    text = (proc.stdout or "") + (proc.stderr or "")
    lowered = text.lower()
    ok = proc.returncode == 0 and "run" in lowered and "--model" in lowered
    return Check("stella --help", ok, "lists run + --model" if ok else "missing usage")


def check_models(stella: Path) -> Check:
    """`stella models` lists providers with zero API keys required."""
    try:
        proc = _run([str(stella), "models"], env=_scrubbed_env())
    except subprocess.TimeoutExpired:
        return Check("stella models", False, "timed out")
    ok = proc.returncode == 0
    detail = "listed providers offline" if ok else f"exit={proc.returncode}"
    return Check("stella models", ok, detail)


def check_graceful_no_key(stella: Path) -> Check:
    """The adapter's exact one-shot shape must degrade gracefully with no key.

    Runs ``stella --model <m> --budget 5.0 --output-format json run "<p>"`` in a
    scrubbed environment. Classifies the result:

    - crash (signal death / panic text)           -> FAIL (real bug)
    - clean non-zero exit + provider-error text    -> PASS (expected env state)
    - clean non-zero exit, no recognizable text    -> PASS (did not crash) + note
    - exit 0 (a key was somehow present)           -> SKIP
    """
    cmd = [
        str(stella),
        "--model",
        "anthropic/claude-fable-5",
        "--budget",
        "5.0",
        "--output-format",
        "json",
        "run",
        "print the word hello and stop",
    ]
    try:
        proc = _run(cmd, env=_scrubbed_env(), timeout=60)
    except subprocess.TimeoutExpired:
        return Check(
            "graceful no-key run",
            False,
            "hung waiting on input/network (no clean error path)",
        )

    combined = ((proc.stdout or "") + (proc.stderr or "")).lower()

    if any(marker in combined for marker in _CRASH_MARKERS) or proc.returncode < 0:
        return Check(
            "graceful no-key run",
            False,
            f"crash: exit={proc.returncode} (panic/signal, not a clean error)",
        )

    if proc.returncode == 0:
        return Check(
            "graceful no-key run",
            True,
            "exit 0 — a provider key was present; degradation path not exercised",
            skipped=True,
        )

    matched = any(marker in combined for marker in _PROVIDER_ERROR_MARKERS)
    detail = (
        f"clean provider error (exit={proc.returncode})"
        if matched
        else f"clean non-zero exit={proc.returncode} (no crash; message unrecognized)"
    )
    # A clean non-zero exit is the pass condition; the recognizable message is a
    # bonus, not a requirement (message wording may evolve).
    return Check("graceful no-key run", True, detail)


def check_swebench_dry_run() -> Check:
    if not _RUN_SWEBENCH.is_file():
        return Check("run_swebench --dry-run", False, f"missing {_RUN_SWEBENCH}")
    if not _SAMPLE_INSTANCES.is_file():
        return Check("run_swebench --dry-run", False, f"missing {_SAMPLE_INSTANCES}")
    cmd = [
        sys.executable,
        str(_RUN_SWEBENCH),
        "--instances",
        str(_SAMPLE_INSTANCES),
        "--dry-run",
    ]
    try:
        proc = _run(cmd, timeout=60)
    except subprocess.TimeoutExpired:
        return Check("run_swebench --dry-run", False, "timed out")
    ok = proc.returncode == 0 and "DRY RUN complete" in (proc.stdout or "")
    detail = "wired end-to-end (no clone, no cost)" if ok else f"exit={proc.returncode}"
    return Check("run_swebench --dry-run", ok, detail)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--stella-bin",
        default=None,
        help="Path to the stella binary (default: STELLA_BINARY, PATH, "
        "or ./target/release/stella).",
    )
    args = parser.parse_args(argv)

    print("== Stella bench adapter smoke test ==")

    checks: list[Check] = []

    # The SWE-bench dry-run needs neither the binary nor a key.
    swebench_check = check_swebench_dry_run()

    stella = locate_stella(args.stella_bin)
    if stella is None:
        print(
            "  stella binary not found — skipping binary checks.\n"
            "  Build it with `cargo build --release -p stella-cli`, put it on "
            "PATH, or pass --stella-bin.",
            file=sys.stderr,
        )
        checks.append(
            Check("locate stella binary", True, "not found (binary checks skipped)", skipped=True)
        )
    else:
        print(f"  using stella binary: {stella}")
        checks.append(check_version(stella))
        checks.append(check_help(stella))
        checks.append(check_models(stella))
        checks.append(check_graceful_no_key(stella))

    checks.append(swebench_check)

    print()
    failed = 0
    for c in checks:
        if c.skipped:
            status = "SKIP"
        elif c.ok:
            status = "PASS"
        else:
            status = "FAIL"
            failed += 1
        print(f"  [{status}] {c.name}: {c.detail}")

    print()
    total = len(checks)
    passed = sum(1 for c in checks if c.ok and not c.skipped)
    skipped = sum(1 for c in checks if c.skipped)
    print(f"summary: {passed} passed, {failed} failed, {skipped} skipped ({total} total)")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
