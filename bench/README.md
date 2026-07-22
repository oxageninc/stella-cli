# Benchmarking Stella

Adapters for running the `stella` CLI on public coding benchmarks — head-to-head
with other agents, and standalone. Everything here is **BYOK** (bring your own
key), makes **no phone-home**, and never hard-codes a secret. Claim runs use a
secure launcher that consumes the selected credential before Harbor starts.

Three entry points:

| Path | What it does | Needs |
|---|---|---|
| [`harbor_adapter/`](harbor_adapter/) | A Harbor *installed-agent* adapter — run Stella on Terminal-Bench 2.x / SWE-bench in the same container + verifier as Claude Code, Codex CLI, Terminus, etc. | Docker, `harbor`, a provider key |
| [`run_swebench.py`](run_swebench.py) | A standalone SWE-bench *prediction* harness — clone each instance, run Stella, emit the official predictions JSONL. No Harbor. | `git`, a provider key (Docker only for the official scoring step) |
| [`smoke/smoke_test.py`](smoke/smoke_test.py) | An **offline, zero-cost** self-test of the adapter wiring for CI. | just the built `stella` binary |

For development and the offline smoke test, build the native binary:

```bash
cargo build --release -p stella-cli   # produces ./target/release/stella
```

This ordinary build is not claim-eligible. A Harbor claim run requires the
clean/pushed commit, full `STELLA_BUILD_GIT_SHA`, and the
`x86_64-unknown-linux-gnu` glibc 2.17 artifact; use the exact
[claim build procedure](harbor_adapter/README.md#install).

## Smoke test (no API key, no cost)

Proves the CLI contract the adapters depend on — `--version`, `--help`,
`models`, and the exact non-interactive invocation shape — without spending a cent. A
missing provider key is treated as an **expected** condition (Stella exits
cleanly with a credential error); only a real crash or a broken CLI contract
fails the check. This is what CI runs.

```bash
python3 bench/smoke/smoke_test.py                       # auto-locate the binary
python3 bench/smoke/smoke_test.py --stella-bin ./target/release/stella
```

## Harbor (Terminal-Bench 2.x, containerized head-to-head)

[Harbor](https://github.com/laude-institute/harbor) is the runner behind
Terminal-Bench 2.x. The adapter is a *third-party* agent loaded by import path.

```bash
cd bench/harbor_adapter
uv sync --locked --extra dev
```

Use the single canonical, fail-closed claim command in the adapter's
[Run section](harbor_adapter/README.md#run). It includes the frozen dataset
digest, fully qualified Harbor task IDs, Docker environment, exact cross-built
binary/source stamp, `$0.17` per-trial target, reflection opt-out, and venv
interpreter. This overview deliberately does not duplicate that executable
template.

The secure launcher is mandatory for claim-eligible paid runs. It removes the
credential from Harbor's environment before Compose interpolation and rejects
config/env/custom-agent/custom-environment and run-time upload flags. Plain
`harbor run` is development-only and cannot produce claim-eligible evidence.
It also atomically reserves a never-before-used job directory and writes the
launch receipt the analyzer gates; an existing job name is never resumed.
Run locally, secret-scan the complete job tree with the active key, and only
then publish it in a separate key-free command.

The adapter uploads only the host's frozen `stella` binary into each task
container and installs it on `PATH`; it never adds host utilities such as
`rg` or `fd`, so the canonical task image remains the utility authority. It
runs Stella non-interactively in `stream-json` mode and reports
cost/tokens/model back to Harbor's result context. For claim runs, model
selection comes only from each literal Harbor `--model provider/model_id`
argument; ambient `STELLA_MODEL` is not accepted by the secure launcher.

**Z.ai (GLM) coding plans:** set
`STELLA_BASE_URL=https://api.z.ai/api/coding/paas/v4` — the endpoint must
include `/coding/`, or the API returns HTTP 429 "insufficient balance."

## SWE-bench (standalone predictions)

```bash
# 1. Validate wiring end-to-end with zero network / zero cost:
python3 bench/run_swebench.py --instances bench/instances.sample.jsonl --dry-run

# 2. Generate predictions against SWE-bench-Verified (needs a provider key):
python3 bench/run_swebench.py \
  --dataset princeton-nlp/SWE-bench_Verified \
  --model anthropic/claude-fable-5 \
  --output predictions.jsonl
```

Each input record needs at least `instance_id`, `repo`, `base_commit`, and
`problem_statement` (a subset of the SWE-bench schema); `--dataset` pulls the
full set from HuggingFace (`pip install datasets`). The harness clones each repo
at `base_commit`, runs Stella in the pristine checkout, and captures the
resulting `git diff` as `model_patch`.

### Scoring

`run_swebench.py` **only generates predictions** — it does not judge them.
A validated resolve rate requires Docker and the official
[`swebench`](https://github.com/princeton-nlp/SWE-bench) evaluation harness:

```bash
python -m swebench.harness.run_evaluation \
  --predictions_path predictions.jsonl \
  --dataset_name princeton-nlp/SWE-bench_Verified \
  --run_id stella-$(date +%Y%m%d)
```

## How benchmark cost maps to Stella's own telemetry

Every Stella run also writes to the workspace's local `.stella/private/store.db` — the
same executions, tokens, and `$`/resolved-task receipts you can read with
`stella stats` or browse in the [Observatory](../README.md#observatory)
dashboard (`stella observe`). A benchmark run and Stella's own metering agree by
construction: both read the JSON envelope this harness parses.
