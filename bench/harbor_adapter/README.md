# stella-harbor

A [Harbor](https://github.com/laude-institute/harbor) installed-agent adapter
for the [Stella](https://github.com/macanderson/stella) coding CLI. It lets you
benchmark the `stella` binary on Terminal-Bench 2.x and SWE-bench in the same
container and under the same verifier as Claude Code, Codex CLI, Terminus, and
any other Harbor-supported agent.

## Install

```bash
claim_repo="$(git rev-parse --show-toplevel)"
claim_venv="$claim_repo/bench/harbor_adapter/.venv"
uv sync --project "$claim_repo/bench/harbor_adapter" --locked --extra dev
cd "$claim_repo"
```

An ordinary native build is sufficient only for development and the offline
smoke test:

```bash
cargo build --release -p stella-cli    # produces ./target/release/stella
```

For a claim run, build the exact clean, pushed commit as a full-SHA-stamped
`x86_64` Linux/glibc 2.17 binary. `cargo-zigbuild` and Zig must already be
installed. These checks fail if the tree is dirty, detached from an upstream,
or not equal to the freshly fetched upstream commit:

```bash
cd "$claim_repo"
git fetch --quiet
test -z "$(git status --porcelain)"
claim_sha="$(git rev-parse HEAD)"
upstream_sha="$(git rev-parse '@{upstream}')"
test "$claim_sha" = "$upstream_sha"

claim_rustc="$(rustup which rustc)"
claim_cargo="$(rustup which cargo)"
claim_zig_cache="$(mktemp -d "${TMPDIR:-/tmp}/stella-tb21-zig.XXXXXX")"
mkdir -p "$claim_zig_cache/global" "$claim_zig_cache/local"
rustup target add x86_64-unknown-linux-gnu

RUSTC="$claim_rustc" \
ZIG_GLOBAL_CACHE_DIR="$claim_zig_cache/global" \
ZIG_LOCAL_CACHE_DIR="$claim_zig_cache/local" \
STELLA_BUILD_GIT_SHA="$claim_sha" \
"$claim_cargo" zigbuild --release --locked \
  --target x86_64-unknown-linux-gnu.2.17 \
  --package stella-cli --bin stella

export STELLA_BINARY="$PWD/target/x86_64-unknown-linux-gnu/release/stella"
export STELLA_SOURCE_COMMIT="$claim_sha"
export STELLA_BUDGET=0.17
export STELLA_DISABLE_REFLECTION=1
export PATH="$claim_venv/bin:$PATH"
```

The adapter rejects a mismatched `STELLA_SOURCE_COMMIT`, and the claim analyzer
rejects an unstamped binary. Do not use `scripts/dev.sh`: its short/dirty stamp
is intentionally useful for local development but is not a 40-character claim
identity. Do not replace the explicit `rustup which` paths with a bare
Homebrew `cargo`/`rustc`: its sysroot can lack the installed Linux target even
when `rustup target list` shows it. Per-build Zig caches also prevent another
build from contaminating the claim artifact.

## Run

This is the one canonical command source for the three primary-study stages:
readiness, calibration, and the mandatory fixed-GLM-5.1 primary. Complete the
public preregistration and paid-intent records first, then provide their digests
below. Use a separate OpenRouter Management API key to create the normal
benchmark key with exact name `stella-tb21-dedicated-key-v1`, limit `180`,
`limit_reset: null`, and `include_byok_in_limit: true`. The launcher also
requires the returned key record to remain `disabled: false`.
Load the resulting benchmark key as `OPENROUTER_API_KEY` and the distinct
management key as `OPENROUTER_MANAGEMENT_API_KEY` without printing either or
placing either in shell history. The management key is host-only control-plane
authority; it never enters Harbor's anonymous credential bundle.

For each stage, compute the immutable intent digest, then call
`stella_harbor.host_attestation.collect_public_host_report` on the actual runner
with that digest, stage, job name, absolute jobs root, and `/usr/bin/docker`.
Write only `canonical_json_bytes(report)` to
`bench/evidence/host-attestations/<intent_sha256>.json`; commit that file in the
same `ledger_commit` that publishes the intent, push it, and create the unedited
public intent comment before launching. The report has a 15-minute freshness
window, so automate report generation, commit/push, comment publication, and
launch as one stage transition. This collection step is credential-free and
must happen while Docker reports zero running containers. Use a provider
32-GiB memory class; the measured Linux `MemTotal` eligibility floor is 31 GiB
after reserved-memory accounting.

```bash
claim_jobs="${STELLA_JOBS_DIR:?set an absolute, private Harbor jobs directory}"
case "$claim_jobs" in /*) ;; *) false ;; esac
test -d "$claim_jobs"
test ! -L "$claim_jobs"
chmod 700 "$claim_jobs"
test -n "${OPENROUTER_API_KEY:-}"
test -n "${OPENROUTER_MANAGEMENT_API_KEY:-}"
test "$OPENROUTER_API_KEY" != "$OPENROUTER_MANAGEMENT_API_KEY"
test "$(command -v harbor)" = "$claim_venv/bin/harbor"
test "$(harbor --version)" = 0.6.1
test -x "$STELLA_BINARY"
test "${#STELLA_SOURCE_COMMIT}" = 40
test "$STELLA_BUDGET" = 0.17
test "$STELLA_DISABLE_REFLECTION" = 1
umask 077
claim_adapter_root="$claim_repo/bench/harbor_adapter"
claim_site_root="$claim_venv/lib/python3.13/site-packages"
test -f "$claim_adapter_root/stella_harbor/secure_launcher.py"
test -f "$claim_site_root/harbor/__init__.py"

```

Every paid command below uses this exact isolated bootstrap (the two explicit
roots are the only import roots added before the launcher runs):

```bash
run_secure_launcher() {
  local claim_launcher_pycache
  # Create a fresh, initially empty cache root for this process.  -I -S
  # prevents PYTHONPATH, user-site, sitecustomize, and .pth startup; the
  # explicit cache prefix prevents adjacent unchecked-hash .pyc files from
  # shadowing the frozen Python sources.  -B keeps this root empty.
  claim_launcher_pycache="$(mktemp -d "$claim_jobs/.stella-launcher-pycache.XXXXXX")"
  test -d "$claim_launcher_pycache"
  "$claim_venv/bin/python" \
    -I -S -B -X "pycache_prefix=$claim_launcher_pycache" \
    -c 'import sys
adapter_root, site_root = sys.argv[1:3]
del sys.argv[1:3]
sys.path[:0] = [adapter_root, site_root]
from stella_harbor.secure_launcher import main
raise SystemExit(main())' \
    "$claim_adapter_root" "$claim_site_root" -- "$@"
}
```

Readiness is one paid synthetic sentinel and is permanently excluded from
model selection:

```bash
test "${READINESS_INTENT_SHA256:?set the committed readiness intent digest}" \
  = "$(printf '%s' "$READINESS_INTENT_SHA256" | tr 'A-F' 'a-f')"
test -n "${READINESS_INTENT_COMMENT_URL:?set the public readiness intent comment URL}"
test ! -e "$claim_jobs/stella-readiness-synthetic-v1"

run_secure_launcher harbor run \
  --env docker \
  --path "$claim_repo/bench/readiness/synthetic-adapter-sentinel" \
  --agent-import-path stella_harbor:StellaAgent \
  --model openrouter/deepseek/deepseek-v4-pro \
  --job-name stella-readiness-synthetic-v1 \
  --jobs-dir "$claim_jobs" \
  --intent-sha256 "$READINESS_INTENT_SHA256" \
  --intent-comment-url "$READINESS_INTENT_COMMENT_URL" \
  --n-attempts 1 \
  --n-concurrent 1 \
  --max-retries 0
```

Calibration is exactly 60 trials: three frozen models, ten fully qualified
tasks, and two attempts:

```bash
test "${CALIBRATION_INTENT_SHA256:?set the committed calibration intent digest}" \
  = "$(printf '%s' "$CALIBRATION_INTENT_SHA256" | tr 'A-F' 'a-f')"
test -n "${CALIBRATION_INTENT_COMMENT_URL:?set the public calibration intent comment URL}"
test ! -e "$claim_jobs/stella-tb21-calibration-20260721"

run_secure_launcher harbor run \
  --env docker \
  --dataset terminal-bench/terminal-bench-2-1@sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a \
  --include-task-name terminal-bench/fix-git \
  --include-task-name terminal-bench/filter-js-from-html \
  --include-task-name terminal-bench/kv-store-grpc \
  --include-task-name terminal-bench/large-scale-text-editing \
  --include-task-name terminal-bench/regex-log \
  --include-task-name terminal-bench/schemelike-metacircular-eval \
  --include-task-name terminal-bench/sqlite-with-gcov \
  --include-task-name terminal-bench/bn-fit-modify \
  --include-task-name terminal-bench/make-mips-interpreter \
  --include-task-name terminal-bench/train-fasttext \
  --agent-import-path stella_harbor:StellaAgent \
  --model openrouter/deepseek/deepseek-v4-pro \
  --model openrouter/z-ai/glm-5.2 \
  --model openrouter/x-ai/grok-4.5 \
  --job-name stella-tb21-calibration-20260721 \
  --jobs-dir "$claim_jobs" \
  --intent-sha256 "$CALIBRATION_INTENT_SHA256" \
  --intent-comment-url "$CALIBRATION_INTENT_COMMENT_URL" \
  --n-attempts 2 \
  --n-concurrent 3 \
  --max-retries 0
```

The analyzer requires at least 14 verifier passes out of 20, then ranks
eligible models by more passes, lower projected 445-trial USD cost, and finally
the frozen DeepSeek V4 Pro, GLM-5.2, Grok-4.5 roster order. Calibration tokens
and wall time are descriptive only and do not break ties.

After calibration, a distinct public freeze records the independently selected
calibration winner and the mandatory primary job identity. The primary remains
fixed to `openrouter/z-ai/glm-5.1` and is exactly 445 trials:

```bash
test "${PRIMARY_INTENT_SHA256:?set the committed primary intent digest}" \
  = "$(printf '%s' "$PRIMARY_INTENT_SHA256" | tr 'A-F' 'a-f')"
test -n "${PRIMARY_INTENT_COMMENT_URL:?set the public primary intent comment URL}"
test -n "${PRIMARY_JOB_NAME:?set the publicly frozen primary job name}"
test ! -e "$claim_jobs/$PRIMARY_JOB_NAME"

run_secure_launcher harbor run \
  --env docker \
  --dataset terminal-bench/terminal-bench-2-1@sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a \
  --agent-import-path stella_harbor:StellaAgent \
  --model openrouter/z-ai/glm-5.1 \
  --job-name "$PRIMARY_JOB_NAME" \
  --jobs-dir "$claim_jobs" \
  --intent-sha256 "$PRIMARY_INTENT_SHA256" \
  --intent-comment-url "$PRIMARY_INTENT_COMMENT_URL" \
  --n-attempts 5 \
  --n-concurrent 1 \
  --max-retries 0
```

This is the exact preregistered calibration control shape: the frozen ten-task
subset is explicit in argv and bound by its paid-intent entry, all three model
rows run twice, concurrency is three, and Harbor retries are zero. It is not the
leaderboard submission. The mandatory primary uses all 89 tasks, five attempts,
`--n-concurrent 1`, and its own precommitted job name/intent. Never improvise
between the protocol, paid intent, and command line, and do not override task
resources or timeouts.

The v6 secure launcher intentionally supports exactly these three paid stages:
readiness, calibration, and the fixed-GLM-5.1 primary. It rejects a selected-
winner follow-up; there is no executable v6 command for one. A future
descriptive winner run would require a separately versioned public protocol,
manifest, analyzer, ledger contract, and launcher review after the primary is
complete. Until that separate contract exists, do not attempt such a run or
merge any secondary rows into the primary evidence package.

The `stella_harbor.secure_launcher` module is mandatory for claim-eligible paid
runs. Do not invoke plain `harbor run` with a provider key: Compose can
interpolate the host environment before the adapter starts. Before reserving a
one-shot job name, the launcher verifies the pinned Harbor/adapter runtime,
cross-built executable and embedded source SHA, exact budget/reflection
settings, dataset/task naming, and command shape. It also rejects
`--env-file`, config files, agent/verifier env, custom agents/environments,
environment/agent kwargs, mounts, and run-time upload/export flags. A plain
Harbor invocation is development-only and is marked non-claim by the analyzer.
The canonical `run_secure_launcher` bootstrap above is part of the control: it
uses isolated/no-site Python, an empty alternate bytecode-cache prefix, and only
the two explicit frozen import roots. Do not replace it with `python -m`, the
generated console script, or a normal virtual-environment startup.
Claim launch also requires exactly one explicit `--env docker`, `--job-name`,
and `--jobs-dir`. The
launcher atomically creates a fresh job directory and a mode-0600
`stella-secure-launch-receipt.json`; an existing directory is rejected, so a
claim job cannot silently resume. Use a new disclosed job name for a genuinely
new run, never delete the receipt from an evidence tree. The required
`--intent-sha256` is copied into that receipt and removed from argv before
Harbor parses the command; missing, uppercase, or non-64-hex values fail before
launch. A matching `--intent-comment-url` is also mandatory. Immediately before
reserving the job, the launcher performs credential-free GitHub API GETs with
ambient proxies disabled. It requires the fixed `macanderson/stella` repository
to be public, verifies the dedicated owner-authored issue and unedited owner
comment, fetches the ledger bytes at the comment's public ledger commit, and
recomputes the exact intent digest while matching stage, job, and model roster.
It waits until at least two seconds after GitHub's server `created_at`, GETs the
comment again, then writes receipt schema
`stella-harbor-secure-launch-receipt-v2`. The receipt's
`public_intent_attestation` uses exact schema
`stella-harbor-public-intent-preflight-v2`. It records the URL and IDs, GitHub
timestamps, strict source-to-subject-to-ledger ancestry, exact ledger bytes,
safety-wait/final-GET completion times, and SHA-256 of the
exact UTF-8 comment-body string returned by GitHub, including whitespace.
It also binds the completed prior-stage outcome, binary/source/adapter/Harbor/
analyzer/verifier/engine runtime identity, exact `$180` no-reset key identity
and usage, current account credits, and the post-GET runtime rehash. The normal
benchmark key authenticates only `GET /api/v1/key`; the host-only management
key authenticates `GET /api/v1/keys/<benchmark-key-sha256>` and
`GET /api/v1/credits`. The key-record `hash` must equal the runtime benchmark
fingerprint. The existing evidence field `label` means the management-verified
key-record `name`, not the masked current-key `label`. Both
launcher-only options are stripped before Harbor parses argv. Any private,
edited, changed, auth-only, mismatched, or unreadable evidence fails without
reserving a job directory. A calibration or primary launch additionally opens
the exact prior job directories under this same `--jobs-dir`, recomputes their
artifact-tree digests, and replays the readiness/calibration/excluded evidence;
a syntactically valid ledger outcome with no matching Harbor artifacts fails.
The launcher also fetches the intent-specific public native-host report and
requires a fresh live same-boot recheck before reservation. It binds that report,
public commit, receipt hash, and recheck in mode-0600
`stella-host-attestation.json` beside the receipt. Its non-secret
`launcher_controls` attestation also records
`filesystem_settings=disabled`, `filesystem_credentials=disabled`,
`project_env_files=disabled`, and `subprocess_credential_scrub=enabled`; the
claim analyzer requires these exact values.

## What it does

1. **Locates** the `stella` binary on the host (`STELLA_BINARY` → `PATH` →
   `./target/release/stella`). Claim runs set `STELLA_BINARY` to the stamped
   `target/x86_64-unknown-linux-gnu/release/stella` artifact above. The binary
   is never imported at load time, so `import stella_harbor` works even where
   Stella isn't built.
2. **Installs** only the frozen binary as `/usr/local/bin/stella` in the task
   container. Host `rg`, `fd`, and other convenience tools are never uploaded;
   each task retains the exact utility set from its canonical image.
3. **Hashes** the exact host binary before upload. Harbor's reported agent
   version and public ATIF version include the full SHA-256 build identity;
   setup verifies the uploaded bytes have the same digest, and trial metadata
   also carries the digest. The source commit is derived from the binary's
   compile-time `STELLA_BUILD_GIT_SHA`; a caller-supplied
   `STELLA_SOURCE_COMMIT` is only an assertion and a mismatch aborts setup.
   Deterministic hashes of the executable adapter and Harbor Python source trees
   are recorded separately from their human-readable versions.
4. **Holds** exactly one provider key in an unlinked, owner-only, seekable host
   temporary-file descriptor, then execs Harbor with every named or aliased
   copy of either OpenRouter credential removed from its environment. The
   distinct management key is used only by the host launcher's control-plane
   GETs and never enters the bundle, child environment, argv, receipt, sidecar,
   runtime identity, or public evidence. On macOS this temporary backing can be
   disk-backed, so claim runs require a trusted, encrypted host/temp volume.
   No pathname is exposed. The adapter verifies every project container's full
   Docker `Config` before binary upload and again before handing that one key to
   Stella through Compose exec's anonymous stdin pipe. The key is never put in
   container argv/environment or a benchmark log.
5. **Runs** Stella one-shot as a direct Compose argv: `main`,
   `/usr/local/bin/stella`, global
   flags, `run`, and the complete task instruction as one final argument.
   Stella itself appends and flushes every completed event to
   `/logs/agent/stella-events.jsonl`; no launcher shell or `tee` process exists.
6. **Reports** cost, tokens, model, status, and accounting completeness back to
   Harbor. It reconstructs a strict envelope from JSONL even when diagnostics
   are interleaved. On timeout/cancellation it recovers only durable events and
   marks the stream/accounting incomplete rather than inventing terminal data.
7. **Classifies** a real nonzero Stella exit as Harbor's
   `NonZeroAgentExitCodeError`, but only after output, metrics, and ATIF have
   been persisted. Harbor still runs the canonical benchmark verifier, which
   independently determines task correctness.
8. **Writes** a validated ATIF-v1.7 trace to
   `<trial>/agent/trajectory.json` for later post-scan publication.

## Public trajectory (ATIF v1.7)

`StellaAgent.SUPPORTS_ATIF` is enabled. On every run with at least one durable
event, including an interrupted run, the adapter writes:

- `stella-events.jsonl`: the exact stream Stella writes and flushes per event,
  preserving completed-call telemetry across an outer timeout;
- `stella-run.stdout.txt`: the exact captured process stdout on normal return;
- `stella-run.json`: the strict synthetic envelope parsed from the stream;
- `trajectory.json`: Harbor's public Agent Trajectory Interchange Format.

The ATIF trace includes the rendered benchmark instruction as its first user
step. Each committed Stella model call becomes one agent step containing:

- concatenated `reasoning` deltas and authoritative output: the full `text`
  event for execute calls, or `step_usage.output_text` for non-user-facing
  triage/plan/judge/guidance/compaction calls (`text_delta` previews are
  deliberately excluded);
- the call purpose from `step_usage.purpose`, so public reviewers can
  distinguish management work from task execution;
- structured tool calls and call-ID-correlated results, including error state,
  execution duration, and whether execution was speculative;
- prompt, completion, cached, and cache-write tokens; cost; model-call
  duration; retry count; estimated input tokens; and tool-call count.

Final metrics contain totals for prompt/completion/cached tokens, authoritative
envelope cost, model/tool duration, cache writes, and ATIF step count. The
`stella_accounting` record compares envelope cost with the sum of all
`step_usage` costs and marks every usage field `complete`, `incomplete`, or
`unknown`; when an interrupted/aborted envelope total is itself reconstructed
from those calls, the comparison is labeled `derived_from_step_usage` rather
than presented as an independent match. If an old or interrupted stream lacks
a usage record or field, its available output is retained and the affected
step is marked `usage_missing`; the adapter never substitutes zero for an
unknown value.
It also records how many `step_usage` records carry a model and the exact set of
per-call model IDs. The claim analyzer rejects a missing or off-manifest call,
even if the envelope's final model happens to match.

## Disclosed benchmark configuration

Post-turn reflection is disabled by default for Harbor trials with
`STELLA_DISABLE_REFLECTION=1`. These trials are ephemeral, and a reflection
call after the task turn would add model spend outside the benchmark-relevant
trajectory. This is an explicit experimental configuration recorded in Harbor
metadata and ATIF—not a telemetry-only suppression. Set
`STELLA_DISABLE_REFLECTION=0` (or `false`) to run a separately labeled
reflection-enabled experiment.

The adapter replaces the complete merged `agent_engine_config` through Stella's
trusted `STELLA_ENGINE_CONFIG_JSON` launcher seam. Unregistered `STELLA_*`
variables and arbitrary Harbor agent extras fail closed; the secure Docker
execution boundary regenerates the config from the literal `--model` argument.
For every selected model, the normalized posture is:

```json
{
  "default_model": "<the exact Harbor-selected provider/model>",
  "allowed_models": ["<the same exact model>"],
  "auto_mode": "off",
  "effort_auto": "off",
  "reasoning_auto": "off",
  "agents": {
    "default": {"effort": "high", "reasoning": "on"},
    "worker": {"effort": "high", "reasoning": "on"},
    "judge": {"effort": "high", "reasoning": "on"},
    "triage": {"effort": "low", "reasoning": "off"}
  }
}
```

No role has a provider or model override; all four inherit `default_model`.
Repository/user settings therefore cannot change routing, reasoning, effort,
prompts, or generation parameters for a benchmark trial. The exact canonical
JSON, parsed object, schema version, and SHA-256 are emitted in both Harbor
context and ATIF.

Every result exposes manifest-ready metadata keys:

- `stella_adapter_version`, `stella_adapter_sha256`, and
  `stella_agent_version`;
- `stella_binary_sha256`, its in-container verification flag, the embedded
  `stella_source_commit`, and its verification flag;
- `stella_harbor_version`, `stella_harbor_sha256`, and the credential-free
  `stella_base_url` route identity plus `stella_provider_route_policy`;
- `stella_budget_usd`, `stella_disable_reflection`, and
  `stella_reflection_policy`;
- `stella_output_format`, `stella_return_code_state`, `stella_stream`, and
  `stella_accounting`;
- `stella_credential_handoff=anonymous-fd` (the mode only—never the value);
- `stella_host_credential_source=anonymous-seekable-fd-v1`,
  `stella_host_credential_name`, `stella_host_credential_bundle_count=1`, and
  `stella_container_credential_absence_verified=true`;
- `stella_launcher_controls`, recording direct-argv execution, disabled
  project env files, filesystem settings/credentials, repository hooks,
  catalog auto-refresh, and proxies; subprocess credential scrubbing; plus
  CLI-authoritative base URLs.
- `stella_engine_posture_version`, `stella_engine_posture`,
  `stella_engine_posture_json`, and `stella_engine_posture_sha256`.

## Configuration

| Variable | Effect |
|---|---|
| `--model` (Harbor) | Required literal `provider/model_id`. Repeated models are allowed only when all use one provider roster. |
| `STELLA_BUDGET` | Per-task USD target. Development defaults to `5.0`; the claim launcher requires the exact frozen value `0.17`. |
| `STELLA_BASE_URL` | Base-URL override for non-claim provider experiments. Claim-eligible OpenRouter runs reject anything except `https://openrouter.ai/api/v1`. |
| `STELLA_BINARY` | Development may locate a binary automatically; the claim launcher requires a canonical absolute executable path to an ELF64 little-endian x86_64 artifact. |
| `STELLA_SOURCE_COMMIT` | Development-only runs may omit it. The claim launcher requires an exact lowercase 40-hex value and verifies that it is the unique commit embedded by `STELLA_BUILD_GIT_SHA`. |
| `STELLA_DISABLE_REFLECTION` | Disable post-turn reflection for ephemeral trials. The claim launcher requires exact `1`; development can explicitly set `0`/`false` to enable. |
| `STELLA_CATALOG_AUTO_REFRESH` | Forced to `0` by the adapter so benchmark startup cannot make an unmetered model-list request or drift the frozen catalog. |
| `STELLA_ENGINE_CONFIG_JSON` | Internal trusted-launcher seam. The adapter discards ambient/extra values and authoritatively supplies the canonical posture above. |
| selected provider key (`OPENROUTER_API_KEY`, etc.) | Consumed by the secure host launcher; one selected key is bundled, then delivered to Stella through inherited anonymous stdin. |

Only the registered budget/reflection settings plus launcher-owned controls
enter the task container. Unregistered `STELLA_*` knobs and arbitrary Harbor
agent extras abort a claim run; host-only binary/source/base/model settings are
not forwarded. Exactly one selected provider key is resolved separately and
sent only over the anonymous FD. As final launcher overrides,
`STELLA_NO_ENV_FILE=1` and `STELLA_NO_SETTINGS=1` prevent task repositories and
task-image user/managed scopes from loading any `.env`, settings or credential
store; the same benchmark gate excludes memories/explorations, persisted
context/store databases, repository/user rules and skills, custom
commands/agents/tools, MCP/registry discovery, and optional host-backed tools
from every agent construction path;
`STELLA_TRUST_PROJECT=0` and `STELLA_PROJECT_HOOKS=0` additionally pin the
ordinary trust gates closed. The trusted `STELLA_ENGINE_CONFIG_JSON` posture is
applied after the empty settings value. `STELLA_CATALOG_AUTO_REFRESH=0`
prevents bootstrap
from making an unmetered live model-list request or changing the frozen model
catalog. Upper- and lowercase HTTP(S)/ALL proxy variables are emptied and
`NO_PROXY`/`no_proxy` are pinned to `*`. Compose invokes Stella as a literal
argv with no launcher shell. For OpenRouter, the validated effective base URL
is always passed as `--base-url`, including the default
`https://openrouter.ai/api/v1`.

**Non-claim Z.ai (GLM) experiments:** set
`STELLA_BASE_URL=https://api.z.ai/api/coding/paas/v4` — the endpoint must
include `/coding/`, or the API returns HTTP 429 "insufficient balance."

## Scan before publication

The secure launcher rejects `harbor run --upload` and hidden export-push
channels. After the local job finishes, scan the complete tree while the active
key is still available; the scanner fails closed and never prints a match:

```bash
OPENROUTER_API_KEY=... \
python ../terminal_bench_analysis/artifact_secret_scan.py \
  /path/to/harbor-job \
  --require-env OPENROUTER_API_KEY
```

Only after that command exits zero may the job be uploaded/published in a
separate process with `OPENROUTER_API_KEY` unset.

## Develop

```bash
pip install -e ".[dev]"
pytest            # adapter unit tests (no binary, no network)
ruff check .
```

See [`../README.md`](../README.md) for the standalone SWE-bench harness and the
offline smoke test.
