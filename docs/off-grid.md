# Off-grid: run and benchmark Stella on local models

The **Off-grid** Arena division is Stella on local models only: any
OpenAI-compatible server via `--base-url`, zero API keys, **$0 marginal cost**.
This is the one documented flow from "server running" to "recorded, receipt-ready
benchmark" — every command here was verified end to end against a live local
server before landing.

Stella's `local/<model>` pseudo-provider speaks the OpenAI Chat Completions
dialect, which all four common local stacks serve. No API key is needed
(`LOCAL_API_KEY` exists for gateways that demand one; a placeholder is sent
otherwise).

## 1 · Point Stella at your server

### Ollama

```bash
ollama pull qwen2.5-coder:7b
stella --model local/qwen2.5-coder:7b \
       --base-url http://localhost:11434/v1 \
       run "fix the failing test"
```

### vLLM

```bash
vllm serve Qwen/Qwen2.5-Coder-7B-Instruct --port 8000
stella --model local/Qwen/Qwen2.5-Coder-7B-Instruct \
       --base-url http://localhost:8000/v1 \
       run "fix the failing test"
```

### LM Studio

Load a model in the LM Studio UI, enable the local server (default port 1234),
then use the model id the server lists (`curl http://localhost:1234/v1/models`):

```bash
stella --model local/<model-id> --base-url http://localhost:1234/v1 chat
```

### llama.cpp (server mode)

```bash
llama-server -m qwen2.5-coder-7b-instruct-q5_k_m.gguf --port 8080
stella --model local/<any-name> --base-url http://localhost:8080/v1 chat
```

(`llama-server` serves whatever model it was started with; the slug after
`local/` is used for display and telemetry keys.)

**Sanity check first** — a one-shot that proves the wiring before you spend a
benchmark's wall-clock on it:

```bash
stella --model local/qwen2.5-coder:7b --base-url http://localhost:11434/v1 \
       run "Reply with exactly one word: ready"
# → ready · $0.0000
```

Notes that matter for benchmarking:

- The model slug after `local/` must be exactly what the server reports in
  `GET /v1/models` (for Ollama, the tag: `qwen2.5-coder:7b`, not
  `qwen2.5-coder`).
- The catalog check is bypassed for `local/` — your server, not Stella's seed
  data, is the authority on which models exist.
- Cost is metered at $0 (no pricing card), so `--budget` is not a useful cap
  off-grid; use the harness `--timeout` instead.
- Local runs land in the same SQLite telemetry as hosted runs; `stella stats`
  reports them under the `off-grid` division.

## 2 · Run the benchmark

`bench/run_swebench.py` accepts `--base-url` and stamps the division into the
run's receipts. A `local/<model>` run without `--base-url` fails fast, before
any instance is attempted.

```bash
cargo build --release -p stella-cli

# Validate the wiring — spends nothing, prints the exact plan:
python3 bench/run_swebench.py \
  --instances bench/instances.sample.jsonl \
  --model local/qwen2.5-coder:7b \
  --base-url http://localhost:11434/v1 \
  --stella-bin target/release/stella \
  --dry-run

# The real run (SWE-bench Verified via HuggingFace; drop --limit to go full):
python3 bench/run_swebench.py \
  --model local/qwen2.5-coder:7b \
  --base-url http://localhost:11434/v1 \
  --stella-bin target/release/stella \
  --timeout 3600 \
  --limit 50
```

`summary.json` records the division alongside the results:

```json
{
  "run_id": "stella-local-qwen2.5-coder-...",
  "model_name_or_path": "local/qwen2.5-coder:7b",
  "division": "off-grid",
  "base_url": "http://localhost:11434/v1",
  ...
}
```

Off-grid is auto-detected from `local/<model>` + `--base-url`; the other
divisions are explicit claims (`--division heavyweight|featherweight`), because
the harness can't infer a model's class.

Score the predictions with the **official SWE-bench Docker evaluator,
unmodified** (see `bench/README.md`) — self-graded results don't count in any
division.

## 3 · Record it — the results track

Submit through the Arena run issue template
([new issue → Arena run](../../../issues/new?template=arena_run.yml)) with
division **Off-grid** and the standard receipts:

- `predictions.jsonl`, `summary.json` (with the `off-grid` division stamp),
  per-instance logs from the run directory;
- the official evaluator's output;
- `stella stats --format csv` for the token/latency numbers straight out of
  local SQLite — the "resolve rate at $0 marginal cost" claim is the number
  that decides this division.

Reproducibility bar: name the exact server (Ollama/vLLM/LM Studio/llama.cpp +
version), the model file or tag (with quantization), and the hardware. A local
run nobody can re-run is not a result.
