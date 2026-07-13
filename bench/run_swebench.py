#!/usr/bin/env python3
"""
SWE-bench Verified harness for the Stella CLI.

This script runs the `stella` agent against SWE-bench Verified instances and
emits predictions in the official SWE-bench format (one JSON object per line:
`{"instance_id", "model_name_or_path", "model_patch"}`).

IMPORTANT: This is the *harness/infrastructure*, not a completed benchmark run.
Producing a validated score additionally requires Docker and the official
`swebench` evaluation harness (see bench/README.md). This script only generates
the predictions file; it does not evaluate correctness.

Usage examples
--------------
  # Smoke test the wiring without cloning or invoking stella:
  python3 bench/run_swebench.py --instances bench/instances.sample.jsonl --dry-run

  # Real run against a single instance from a local JSONL file:
  python3 bench/run_swebench.py --instances instances.jsonl --limit 1

  # Real run against the HuggingFace dataset (requires `datasets` installed):
  python3 bench/run_swebench.py --limit 5 --model anthropic/claude-fable-5

Stella is invoked one-shot as:
    <stella-bin> --model <model> --budget <usd> run "<problem_statement>"
run inside a pristine checkout of the target repo at `base_commit`. The model's
patch is collected as the `git diff` of the working tree after the run.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Tuple

DEFAULT_DATASET = "princeton-nlp/SWE-bench_Verified"
DEFAULT_SPLIT = "test"
DEFAULT_MODEL = "anthropic/claude-fable-5"
DEFAULT_BUDGET = 2.0
DEFAULT_TIMEOUT = 1800  # seconds (30 min) per instance
DEFAULT_OUTPUT_DIR = "bench/results"

# Fields we expect on each instance. Only a subset is strictly required to run.
REQUIRED_FIELDS = ("instance_id", "repo", "base_commit", "problem_statement")


# --------------------------------------------------------------------------- #
# Small utilities
# --------------------------------------------------------------------------- #
def log(msg: str) -> None:
    """Print a timestamped line to stderr so it interleaves cleanly with logs."""
    ts = _dt.datetime.now().strftime("%H:%M:%S")
    print(f"[{ts}] {msg}", file=sys.stderr, flush=True)


def sanitize(text: str) -> str:
    """Make a string safe for use in a filename / run-id."""
    return "".join(c if c.isalnum() or c in "-._" else "-" for c in text).strip("-")


def default_run_id(model: str) -> str:
    stamp = _dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    return f"stella-{sanitize(model)}-{stamp}"


def discover_stella_bin(explicit: Optional[str]) -> Optional[str]:
    """Resolve the stella binary path.

    Order: explicit --stella-bin, then `stella` on PATH, then
    ./target/release/stella relative to the current working directory.
    Returns None if nothing is found (dry-run does not require it).
    """
    if explicit:
        return explicit
    on_path = shutil.which("stella")
    if on_path:
        return on_path
    local = Path("target/release/stella")
    if local.is_file() and os.access(local, os.X_OK):
        return str(local.resolve())
    return None


def run_cmd(
    cmd: List[str],
    cwd: Optional[str] = None,
    timeout: Optional[int] = None,
    log_path: Optional[Path] = None,
    env: Optional[Dict[str, str]] = None,
) -> Tuple[int, str]:
    """Run a command, capturing combined stdout+stderr.

    If `log_path` is given, the combined output is also appended to that file.
    Returns (returncode, combined_output). Raises subprocess.TimeoutExpired on
    timeout (the caller decides how to handle it).
    """
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=timeout,
    )
    if log_path is not None:
        with log_path.open("a", encoding="utf-8") as fh:
            fh.write(f"$ {' '.join(cmd)}\n")
            fh.write(proc.stdout or "")
            if not (proc.stdout or "").endswith("\n"):
                fh.write("\n")
    return proc.returncode, proc.stdout or ""


# --------------------------------------------------------------------------- #
# Instance loading
# --------------------------------------------------------------------------- #
def load_local_instances(path: str) -> List[Dict[str, Any]]:
    instances: List[Dict[str, Any]] = []
    with open(path, "r", encoding="utf-8") as fh:
        for lineno, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                instances.append(json.loads(line))
            except json.JSONDecodeError as exc:
                raise SystemExit(
                    f"error: {path}:{lineno}: invalid JSON: {exc}"
                ) from exc
    return instances


def load_hf_instances(dataset_name: str, split: str) -> List[Dict[str, Any]]:
    try:
        from datasets import load_dataset  # type: ignore
    except ImportError as exc:
        raise SystemExit(
            "error: the `datasets` package is required to load from HuggingFace.\n"
            "       Install it with `pip install -r bench/requirements.txt`,\n"
            "       or pass a local file with --instances <path.jsonl>."
        ) from exc
    log(f"loading HuggingFace dataset {dataset_name} (split={split}) ...")
    ds = load_dataset(dataset_name, split=split)
    return [dict(row) for row in ds]


def filter_instances(
    instances: List[Dict[str, Any]],
    instance_ids: Optional[List[str]],
    limit: Optional[int],
) -> List[Dict[str, Any]]:
    result = instances
    if instance_ids:
        wanted = set(instance_ids)
        result = [i for i in result if i.get("instance_id") in wanted]
        found = {i.get("instance_id") for i in result}
        for missing in wanted - found:
            log(f"warning: requested --instance-id {missing!r} not found in input")
    if limit is not None:
        result = result[:limit]
    return result


# --------------------------------------------------------------------------- #
# Git / repo preparation
# --------------------------------------------------------------------------- #
def clone_url(repo: str) -> str:
    return f"https://github.com/{repo}.git"


def ensure_cache_mirror(repo: str, repo_cache: str) -> str:
    """Ensure a bare mirror of `repo` exists under repo_cache; return its path."""
    owner_name = repo.replace("/", "__")
    mirror = os.path.join(repo_cache, f"{owner_name}.git")
    if not os.path.isdir(mirror):
        os.makedirs(repo_cache, exist_ok=True)
        log(f"  cache miss: mirroring {repo} into {mirror}")
        rc, out = run_cmd(["git", "clone", "--bare", "--quiet", clone_url(repo), mirror])
        if rc != 0:
            raise RuntimeError(f"git clone --bare failed for {repo}:\n{out}")
    return mirror


def prepare_workdir(
    instance: Dict[str, Any],
    workdir: str,
    repo_cache: Optional[str],
    log_path: Path,
) -> None:
    """Clone the repo into `workdir` and hard-reset to a pristine base_commit tree."""
    repo = instance["repo"]
    base_commit = instance["base_commit"]

    if repo_cache:
        mirror = ensure_cache_mirror(repo, repo_cache)
        rc, out = run_cmd(["git", "clone", "--quiet", mirror, workdir], log_path=log_path)
        if rc != 0:
            raise RuntimeError(f"git clone from cache failed:\n{out}")
        # The local clone's origin is the mirror; point it at GitHub so a
        # fallback fetch for a missing commit can reach the network if needed.
        run_cmd(["git", "-C", workdir, "remote", "set-url", "origin", clone_url(repo)],
                log_path=log_path)
    else:
        rc, out = run_cmd(
            ["git", "clone", "--quiet", clone_url(repo), workdir], log_path=log_path
        )
        if rc != 0:
            raise RuntimeError(f"git clone failed for {repo}:\n{out}")

    # Make sure the base commit is present; fetch it explicitly if not.
    rc, _ = run_cmd(["git", "-C", workdir, "cat-file", "-e", f"{base_commit}^{{commit}}"])
    if rc != 0:
        log(f"  base_commit {base_commit[:12]} not local; fetching from origin")
        rc, out = run_cmd(
            ["git", "-C", workdir, "fetch", "--quiet", "origin", base_commit],
            log_path=log_path,
        )
        if rc != 0:
            raise RuntimeError(
                f"could not fetch base_commit {base_commit} for {repo}:\n{out}"
            )

    # Pristine checkout: detach at base_commit, discard everything else.
    for args in (
        ["git", "-C", workdir, "checkout", "-f", base_commit],
        ["git", "-C", workdir, "reset", "--hard", base_commit],
        ["git", "-C", workdir, "clean", "-fdx"],
    ):
        rc, out = run_cmd(args, log_path=log_path)
        if rc != 0:
            raise RuntimeError(f"{' '.join(args)} failed:\n{out}")


def collect_patch(workdir: str, exclude_paths: List[str]) -> str:
    """Return the unified diff of the working tree relative to HEAD (base_commit).

    New files are included by staging everything first. Optional pathspec
    exclusions (e.g. agent scratch files) can be dropped from the diff.
    """
    # Stage all changes (including new files) so they show up in `git diff --cached`.
    run_cmd(["git", "-C", workdir, "add", "-A"])
    diff_cmd = ["git", "-C", workdir, "--no-pager", "diff", "--cached", "--no-color"]
    if exclude_paths:
        diff_cmd.append("--")
        diff_cmd.append(".")
        diff_cmd.extend(f":(exclude){p}" for p in exclude_paths)
    rc, out = run_cmd(diff_cmd)
    if rc != 0:
        # A non-zero diff here is unusual; surface it but don't crash the run.
        log(f"  warning: `git diff` returned {rc} in {workdir}")
    return out


# --------------------------------------------------------------------------- #
# Per-instance execution
# --------------------------------------------------------------------------- #
def build_stella_cmd(
    stella_bin: str, model: str, budget: float, prompt: str, base_url: Optional[str] = None
) -> List[str]:
    cmd = [stella_bin, "--model", model, "--budget", str(budget)]
    if base_url:
        cmd.extend(["--base-url", base_url])
    cmd.extend(["run", prompt])
    return cmd


def detect_division(model: str, base_url: Optional[str]) -> Optional[str]:
    """The Arena division a run's receipts belong to, when derivable.

    Only Off-grid is mechanically detectable: the `local/<model>` pseudo-
    provider (any OpenAI-compatible server via --base-url, zero API keys,
    $0 marginal cost). Heavyweight/Featherweight are model-class claims the
    harness cannot infer — pass --division explicitly for those.
    """
    if model.startswith("local/") and base_url:
        return "off-grid"
    return None


def describe_plan(
    instance: Dict[str, Any],
    stella_bin: Optional[str],
    model: str,
    budget: float,
    base_url: Optional[str],
    timeout: int,
    logs_dir: Path,
    repo_cache: Optional[str],
) -> None:
    """Print the exact plan for one instance without doing any work (dry-run)."""
    iid = instance.get("instance_id", "<missing instance_id>")
    repo = instance.get("repo", "<missing repo>")
    base = instance.get("base_commit", "<missing base_commit>")
    problem = instance.get("problem_statement", "") or ""
    preview = " ".join(problem.split())[:160]
    src = (
        f"local clone from cache {repo_cache}/{repo.replace('/', '__')}.git"
        if repo_cache
        else f"git clone {clone_url(repo)}"
    )
    cmd = build_stella_cmd(
        stella_bin or "<stella-bin>", model, budget, "<problem_statement>", base_url
    )
    print(f"--- instance: {iid}")
    print(f"    repo         : {repo}")
    print(f"    base_commit  : {base}")
    print(f"    fetch        : {src}")
    print(f"    checkout     : git checkout -f {base} && git reset --hard && git clean -fdx")
    print(f"    stella cmd   : {' '.join(cmd)}")
    print(f"    timeout      : {timeout}s")
    print(f"    log file     : {logs_dir / (sanitize(iid) + '.log')}")
    print(f"    problem      : {preview}{'...' if len(problem) > 160 else ''}")


def run_instance(
    instance: Dict[str, Any],
    *,
    stella_bin: str,
    model: str,
    budget: float,
    base_url: Optional[str],
    timeout: int,
    logs_dir: Path,
    repo_cache: Optional[str],
    exclude_paths: List[str],
) -> Dict[str, Any]:
    """Execute one instance end-to-end.

    Returns a status dict:
      {status: "succeeded"|"empty"|"failed", reason: str,
       stella_error: bool, prediction: Optional[dict]}
    - "failed" means no prediction was produced (infrastructure error before or
      during setup). Every non-failed instance yields a prediction (possibly an
      empty patch).
    """
    iid = instance["instance_id"]
    prompt = instance.get("problem_statement", "") or ""
    log_path = logs_dir / (sanitize(iid) + ".log")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(f"# stella swebench log for {iid}\n", encoding="utf-8")

    stella_error = False
    reason = ""

    tmp_root = tempfile.mkdtemp(prefix=f"stella-swebench-{sanitize(iid)}-")
    workdir = os.path.join(tmp_root, "repo")
    try:
        # 1) pristine checkout
        try:
            prepare_workdir(instance, workdir, repo_cache, log_path)
        except Exception as exc:  # clone/checkout failure -> hard skip, no prediction
            reason = f"repo setup failed: {exc}"
            log(f"  FAILED {iid}: {reason}")
            return {"status": "failed", "reason": reason, "stella_error": False,
                    "prediction": None}

        # 2) run stella
        cmd = build_stella_cmd(stella_bin, model, budget, prompt, base_url)
        env = os.environ.copy()
        env.setdefault("STELLA_BUDGET", str(budget))
        try:
            rc, _ = run_cmd(cmd, cwd=workdir, timeout=timeout, log_path=log_path, env=env)
            if rc != 0:
                stella_error = True
                reason = f"stella exited non-zero ({rc})"
                log(f"  warning {iid}: {reason} (patch still collected)")
        except subprocess.TimeoutExpired:
            stella_error = True
            reason = f"stella timed out after {timeout}s"
            log(f"  warning {iid}: {reason} (partial patch still collected)")
            with log_path.open("a", encoding="utf-8") as fh:
                fh.write(f"\n[harness] TIMEOUT after {timeout}s\n")

        # 3) collect patch (even after timeout / non-zero exit: partial work counts)
        patch = collect_patch(workdir, exclude_paths)
        prediction = {
            "instance_id": iid,
            "model_name_or_path": model,
            "model_patch": patch,
        }
        status = "empty" if not patch.strip() else "succeeded"
        if status == "empty":
            log(f"  EMPTY {iid}: no diff produced"
                + (f" ({reason})" if reason else ""))
        else:
            npatch = len(patch.splitlines())
            log(f"  OK    {iid}: patch collected ({npatch} diff lines)"
                + (f" [warning: {reason}]" if reason else ""))
        return {"status": status, "reason": reason, "stella_error": stella_error,
                "prediction": prediction}
    finally:
        shutil.rmtree(tmp_root, ignore_errors=True)


# --------------------------------------------------------------------------- #
# Main
# --------------------------------------------------------------------------- #
def parse_args(argv: Optional[List[str]] = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="run_swebench.py",
        description="Run Stella against SWE-bench Verified and emit predictions.jsonl.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
        epilog=(
            "This is the harness only; scoring predictions requires Docker and the\n"
            "official swebench evaluation harness. See bench/README.md."
        ),
    )
    src = p.add_argument_group("instance source")
    src.add_argument(
        "--instances",
        metavar="PATH",
        help="Local JSONL file of instances (one JSON object per line). "
        "If omitted, the HuggingFace dataset is used (requires `datasets`).",
    )
    src.add_argument(
        "--dataset-name",
        default=DEFAULT_DATASET,
        help="HuggingFace dataset name (used when --instances is not given).",
    )
    src.add_argument(
        "--split", default=DEFAULT_SPLIT, help="HuggingFace dataset split."
    )

    filt = p.add_argument_group("filters")
    filt.add_argument("--limit", type=int, default=None, help="Only run the first N instances.")
    filt.add_argument(
        "--instance-id",
        action="append",
        dest="instance_ids",
        metavar="ID",
        help="Only run this instance id (repeatable).",
    )

    run = p.add_argument_group("stella invocation")
    run.add_argument("--model", default=DEFAULT_MODEL, help="Provider/model passed to `stella --model`.")
    run.add_argument("--budget", type=float, default=DEFAULT_BUDGET, help="USD budget cap per instance (`stella --budget`).")
    run.add_argument(
        "--base-url",
        default=None,
        help="Endpoint passed to `stella --base-url` — required for the local/"
        "<model> pseudo-provider (Ollama, vLLM, LM Studio, llama.cpp server); "
        "an optional override for hosted providers.",
    )
    run.add_argument(
        "--division",
        choices=["heavyweight", "featherweight", "off-grid", "cross-harness"],
        default=None,
        help="Arena division to stamp into summary.json for the results track. "
        "Default: auto — `local/<model>` runs are stamped off-grid, hosted "
        "runs carry no division.",
    )
    run.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT, help="Per-instance timeout in seconds.")
    run.add_argument(
        "--stella-bin",
        default=None,
        help="Path to the stella binary (default: `stella` on PATH, else ./target/release/stella).",
    )

    out = p.add_argument_group("output")
    out.add_argument("--run-id", default=None, help="Run identifier (default: auto from model + timestamp).")
    out.add_argument("--output-dir", default=DEFAULT_OUTPUT_DIR, help="Base directory for results.")
    out.add_argument(
        "--repo-cache",
        default=None,
        metavar="DIR",
        help="Directory holding bare repo mirrors, reused across instances to avoid re-cloning.",
    )
    out.add_argument(
        "--exclude-path",
        action="append",
        dest="exclude_paths",
        default=[],
        metavar="PATHSPEC",
        help="Pathspec to exclude from the collected diff (repeatable).",
    )

    p.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the per-instance plan without cloning or invoking stella.",
    )
    return p.parse_args(argv)


def validate_instances(instances: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
    """Drop instances missing required fields (logged), return the valid ones."""
    valid: List[Dict[str, Any]] = []
    for idx, inst in enumerate(instances):
        missing = [f for f in REQUIRED_FIELDS if not inst.get(f)]
        if missing:
            iid = inst.get("instance_id", f"<index {idx}>")
            log(f"warning: skipping {iid}: missing required field(s): {', '.join(missing)}")
            continue
        valid.append(inst)
    return valid


def main(argv: Optional[List[str]] = None) -> int:
    args = parse_args(argv)

    if args.model.startswith("local/") and not args.base_url:
        log("error: --model local/<model> needs --base-url "
            "(e.g. --base-url http://localhost:11434/v1) — see docs/off-grid.md")
        return 2
    division = args.division or detect_division(args.model, args.base_url)

    run_id = args.run_id or default_run_id(args.model)
    out_base = Path(args.output_dir) / run_id
    logs_dir = out_base / "logs"
    predictions_path = out_base / "predictions.jsonl"

    # Load instances.
    if args.instances:
        log(f"loading instances from {args.instances}")
        instances = load_local_instances(args.instances)
    else:
        instances = load_hf_instances(args.dataset_name, args.split)
    log(f"loaded {len(instances)} instance(s)")

    instances = validate_instances(instances)
    instances = filter_instances(instances, args.instance_ids, args.limit)
    log(f"{len(instances)} instance(s) selected after filtering")
    if not instances:
        log("nothing to do; exiting")
        return 0

    stella_bin = discover_stella_bin(args.stella_bin)

    # ----- dry run -----
    if args.dry_run:
        print(f"DRY RUN: run_id={run_id}")
        print(f"         output dir : {out_base}")
        print(f"         predictions: {predictions_path}")
        print(f"         stella-bin : {stella_bin or '<not found; set --stella-bin>'}")
        print(f"         model      : {args.model}")
        print(f"         budget     : ${args.budget} per instance")
        print(f"         base-url   : {args.base_url or '<provider default>'}")
        print(f"         division   : {division or '<none>'}")
        print(f"         instances  : {len(instances)}")
        print()
        for inst in instances:
            describe_plan(
                inst, stella_bin, args.model, args.budget, args.base_url,
                args.timeout, logs_dir, args.repo_cache,
            )
        print()
        print("DRY RUN complete: no repos cloned, no stella invocations, no files written.")
        return 0

    # ----- real run -----
    if not stella_bin:
        raise SystemExit(
            "error: could not find the stella binary. Put `stella` on PATH, build "
            "./target/release/stella, or pass --stella-bin <path>."
        )

    out_base.mkdir(parents=True, exist_ok=True)
    logs_dir.mkdir(parents=True, exist_ok=True)
    log(f"run_id={run_id}")
    log(f"writing predictions to {predictions_path}")
    log(f"stella-bin={stella_bin}  model={args.model}  budget=${args.budget}")

    counts = {"attempted": 0, "succeeded": 0, "empty": 0, "failed": 0, "stella_errors": 0}
    total = len(instances)

    # Append predictions incrementally so a crash leaves partial results durable.
    with predictions_path.open("w", encoding="utf-8") as preds:
        for idx, inst in enumerate(instances, 1):
            iid = inst["instance_id"]
            counts["attempted"] += 1
            log(f"[{idx}/{total}] {iid} ({inst['repo']} @ {inst['base_commit'][:12]})")
            result = run_instance(
                inst,
                stella_bin=stella_bin,
                model=args.model,
                budget=args.budget,
                base_url=args.base_url,
                timeout=args.timeout,
                logs_dir=logs_dir,
                repo_cache=args.repo_cache,
                exclude_paths=args.exclude_paths,
            )
            if result["stella_error"]:
                counts["stella_errors"] += 1
            if result["prediction"] is not None:
                preds.write(json.dumps(result["prediction"]) + "\n")
                preds.flush()
            counts[result["status"]] += 1
            # running summary
            log(
                f"    progress: attempted={counts['attempted']} "
                f"succeeded={counts['succeeded']} empty={counts['empty']} "
                f"failed={counts['failed']} stella_errors={counts['stella_errors']}"
            )

    # Final summary
    predictions_written = counts["succeeded"] + counts["empty"]
    summary = {
        "run_id": run_id,
        "model_name_or_path": args.model,
        # The Arena results track: off-grid is auto-stamped for local/<model>
        # runs; heavyweight/featherweight are explicit claims (--division).
        "division": division,
        "base_url": args.base_url,
        "dataset": args.instances or f"{args.dataset_name}:{args.split}",
        "total_selected": total,
        **counts,
        "predictions_written": predictions_written,
        "predictions_path": str(predictions_path),
    }
    (out_base / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")

    print()
    print("=" * 60)
    print(f"RUN COMPLETE: {run_id}")
    print(f"  attempted        : {counts['attempted']}")
    print(f"  succeeded (patch): {counts['succeeded']}")
    print(f"  empty (no patch) : {counts['empty']}")
    print(f"  failed (no pred) : {counts['failed']}")
    print(f"  stella warnings  : {counts['stella_errors']} (timeout / non-zero exit)")
    print(f"  predictions      : {predictions_written} -> {predictions_path}")
    print(f"  logs             : {logs_dir}")
    print("=" * 60)
    print()
    print("Next: score with the official (Docker-based) swebench harness:")
    print(f"  python -m swebench.harness.run_evaluation \\")
    print(f"    --predictions_path {predictions_path} \\")
    print(f"    --run_id {run_id} \\")
    print(f"    --dataset_name {DEFAULT_DATASET}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
