# stella-harbor

A [Harbor](https://github.com/laude-institute/harbor) installed-agent adapter
for the [Stella](https://github.com/macanderson/stella) coding CLI. It lets you
benchmark the `stella` binary on Terminal-Bench 2.x and SWE-bench in the same
container and under the same verifier as Claude Code, Codex CLI, Terminus, and
any other Harbor-supported agent.

## Install

```bash
pip install -e .          # from bench/harbor_adapter/, pulls in `harbor`
```

Build the Stella binary the adapter installs into each task container:

```bash
cargo build --release -p stella-cli    # produces ./target/release/stella
```

## Run

```bash
export ANTHROPIC_API_KEY=...           # or any provider Stella supports

harbor run \
  --dataset terminal-bench/terminal-bench-2-1 \
  --agent-import-path stella_harbor:StellaAgent \
  --model anthropic/claude-fable-5 \
  --n-concurrent 4
```

## What it does

1. **Locates** the `stella` binary on the host (`STELLA_BINARY` → `PATH` →
   `./target/release/stella`). The binary is never imported at load time, so
   `import stella_harbor` works even where Stella isn't built.
2. **Installs** it as `/usr/local/bin/stella` in the task container, and
   best-effort provisions `rg`/`fd` when the host has them.
3. **Runs** Stella one-shot, headless, in JSON mode:
   `stella --model <m> --budget <usd> --output-format json run "<task>"`.
4. **Reports** cost, tokens, model, and status back to Harbor's result context
   by parsing Stella's JSON envelope. A non-zero Stella exit is recorded, never
   raised — the benchmark verifier decides task success, not the agent.

## Configuration

| Variable | Effect |
|---|---|
| `--model` (Harbor) / `STELLA_MODEL` | Model passed to `stella --model` (`provider/model_id`). |
| `STELLA_BUDGET` | Hard per-task USD cap (default `5.0`). |
| `STELLA_BASE_URL` | Base-URL override — required for local endpoints and Z.ai coding plans. |
| `STELLA_BINARY` | Explicit path to the `stella` binary on the host. |
| provider keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `ZAI_API_KEY`, …) | Forwarded into the container so Stella can authenticate (BYOK). |

Every `STELLA_*` variable and the provider credential/addressing variables are
forwarded from host to container automatically.

**Z.ai (GLM) coding plans:** set
`STELLA_BASE_URL=https://api.z.ai/api/coding/paas/v4` — the endpoint must
include `/coding/`, or the API returns HTTP 429 "insufficient balance."

## Develop

```bash
pip install -e ".[dev]"
pytest            # adapter unit tests (no binary, no network)
ruff check .
```

See [`../README.md`](../README.md) for the standalone SWE-bench harness and the
offline smoke test.
