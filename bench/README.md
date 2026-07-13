# SWE-bench Verified harness for Stella

This directory contains the **harness / infrastructure** for running the Stella
CLI against [SWE-bench Verified](https://www.swebench.com/) and producing
predictions in the official format.

> **This is NOT a completed benchmark run.** It is the runnable tooling only.
> Nothing here has been validated against the real dataset, and producing a
> *scored* result additionally requires Docker plus the official `swebench`
> evaluation harness (see [Scoring](#scoring-the-official-evaluation) below).
> The harness generates `predictions.jsonl`; it does **not** evaluate
> correctness.

## What it does

For each SWE-bench Verified instance the harness:

1. Creates a clean temporary working directory.
2. `git clone`s the target repo (`https://github.com/<repo>.git`), optionally
   reusing a local bare mirror under `--repo-cache` to avoid re-cloning the same
   repo across many instances.
3. Hard-resets the tree to a pristine checkout of `base_commit`
   (`git checkout -f && git reset --hard && git clean -fdx`).
4. Runs Stella one-shot inside that directory:

   ```
   <stella-bin> --model <model> --budget <usd> run "<problem_statement>"
   ```

   with a per-instance timeout, capturing stdout+stderr to a per-instance log.
5. Collects the model's patch as the `git diff` of the working tree after the
   run (new files included; empty diffs are recorded as an empty patch).
6. Appends `{"instance_id", "model_name_or_path", "model_patch"}` to
   `predictions.jsonl` — the official SWE-bench prediction format.

A failure on one instance (clone error, checkout error, timeout, non-zero
`stella` exit) is logged and never aborts the whole run. Every skip is logged
with its reason; instances are never silently dropped.

## Prerequisites

- **Python 3** (standard library only for the local-JSONL and `--dry-run`
  paths).
- **git** on PATH.
- The **`stella` binary**, either on PATH or at `./target/release/stella`
  (override with `--stella-bin`). Build it from the workspace root with
  `cargo build --release -p stella-cli`.
- A **Stella provider API key** in the environment (BYOK). Stella is
  bring-your-own-key; export whatever your `--model` provider needs, e.g.
  `export ANTHROPIC_API_KEY=...` or `export ZAI_API_KEY=...`.
- **Optional:** the `datasets` package (`pip install -r bench/requirements.txt`)
  — only needed to pull instances directly from HuggingFace instead of a local
  JSONL file.
- **For scoring only:** Docker and the official `swebench` package.

## Quickstart

Always start with a `--dry-run` against the bundled sample to validate wiring —
it clones nothing and invokes nothing:

```bash
python3 bench/run_swebench.py --instances bench/instances.sample.jsonl --dry-run
```

Then do a small real run against a single instance (this DOES clone a repo and
DOES invoke Stella, spending up to `--budget` USD of your provider credit):

```bash
export ANTHROPIC_API_KEY=...          # or your provider's key
python3 bench/run_swebench.py \
  --instances bench/instances.sample.jsonl \
  --instance-id octocat__Hello-World-1 \
  --model anthropic/claude-fable-5 \
  --budget 2.0
```

To run against the real dataset (requires `datasets`):

```bash
pip install -r bench/requirements.txt
python3 bench/run_swebench.py --limit 1 --model anthropic/claude-fable-5
```

### The bundled sample

`bench/instances.sample.jsonl` contains 1-2 small, syntactically-valid instance
objects with real-ish field shapes (using tiny public GitHub repos). They exist
**only to validate the harness wiring** (so `--instances ... --dry-run` works
end-to-end); they are not real SWE-bench tasks and their gold `patch` fields are
intentionally empty.

## CLI surface

Run `python3 bench/run_swebench.py --help` for the full list. Key options:

| Flag | Default | Purpose |
| --- | --- | --- |
| `--instances PATH` | — | Local JSONL of instances. If omitted, load from HuggingFace. |
| `--dataset-name NAME` | `princeton-nlp/SWE-bench_Verified` | HF dataset (HF path). |
| `--split NAME` | `test` | HF split (HF path). |
| `--limit N` | — | Only the first N instances. |
| `--instance-id ID` | — | Only this instance id (repeatable). |
| `--model M` | `anthropic/claude-fable-5` | Passed to `stella --model`. |
| `--budget USD` | `2.0` | Per-instance USD cap (`stella --budget`). |
| `--base-url URL` | — | Passed to `stella --base-url`; required for `local/<model>` (Ollama, vLLM, LM Studio, llama.cpp — see [docs/off-grid.md](../docs/off-grid.md)). |
| `--division D` | auto | Arena division stamped into `summary.json` (`heavyweight`, `featherweight`, `off-grid`, `cross-harness`). `local/<model>` runs auto-stamp `off-grid`. |
| `--timeout SEC` | `1800` | Per-instance timeout. |
| `--stella-bin PATH` | auto | `stella` on PATH, else `./target/release/stella`. |
| `--run-id ID` | auto | Run identifier (default: model + timestamp). |
| `--output-dir DIR` | `bench/results` | Base directory for results. |
| `--repo-cache DIR` | — | Reuse bare repo mirrors across instances. |
| `--exclude-path SPEC` | — | Pathspec to drop from the collected diff (repeatable). |
| `--dry-run` | off | Print the per-instance plan; clone/invoke nothing. |

## Output

Everything for a run lands under `--output-dir/<run-id>/`:

```
bench/results/<run-id>/
├── predictions.jsonl      # official format, one object per line
├── summary.json           # run counts and metadata
└── logs/
    └── <instance_id>.log  # captured stdout+stderr per instance
```

`predictions.jsonl` lines look like:

```json
{"instance_id": "astropy__astropy-12345", "model_name_or_path": "anthropic/claude-fable-5", "model_patch": "diff --git a/... b/...\n..."}
```

This is exactly the schema the official SWE-bench evaluator consumes.

### How predictions map to the official format

| Prediction key | Source in the harness |
| --- | --- |
| `instance_id` | copied verbatim from the input instance |
| `model_name_or_path` | the `--model` value used for the run |
| `model_patch` | `git diff --cached` of the post-run tree vs `base_commit` (empty string when Stella made no changes) |

Instances that fail *before* Stella runs (clone/checkout errors) produce **no**
prediction line and are counted as `failed` in `summary.json`. Instances where
Stella times out or exits non-zero still have their (possibly partial) patch
collected and written — SWE-bench scores the patch regardless of the agent's
exit code — and are additionally tallied under `stella_errors`.

## Scoring (the official evaluation)

Scoring is a **separate, Docker-based step** run by the official `swebench`
package. It is not performed by this harness. After you have a
`predictions.jsonl`:

```bash
pip install swebench   # official evaluation harness (needs Docker running)
python -m swebench.harness.run_evaluation \
  --predictions_path bench/results/<run-id>/predictions.jsonl \
  --run_id <run-id> \
  --dataset_name princeton-nlp/SWE-bench_Verified
```

This spins up per-instance Docker containers, applies each `model_patch`,
runs the instance's `test_patch`, and reports resolved/unresolved counts.

## Cost

Stella is **BYOK** — every real run spends your own provider credits. Each
instance is capped by `--budget` USD (also honored via the `STELLA_BUDGET` env
var). Worst-case spend for a run is roughly `--budget x number-of-instances`, so
start with `--limit 1` and a small `--budget` before scaling up. `--dry-run`
spends nothing.

## The CI drift gate

`.github/workflows/agent-regression-gate.yml` keeps Stella from silently
getting worse: on agent-behavior PRs and nightly, it builds the branch's
`stella`, benchmarks it with the [oxageninc/arena](https://github.com/oxageninc/arena)
calibration suite (same model both sides, pinned harness commit), and runs
`arena gate` against the committed `bench/arena-baseline.json` — a
statistically significant resolve-rate drop, or a token/cost blow-up past the
thresholds in `arena-gate.json`, is a red check the day it lands. Full
receipts (transcripts, diffs, report) upload as artifacts on every run.

Arming it takes a provider secret (`ANTHROPIC_API_KEY` or `ZAI_API_KEY`) and
one baseline bootstrap — run the workflow with `mode: save-baseline`, commit
the resulting artifact. Until then the gate skips green with a warning, so it
can be a required check from day one. The nightly `swebench-smoke` job also
pushes the committed sample instances through `run_swebench.py` end to end and
uploads those receipts.
