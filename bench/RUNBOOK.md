# Terminal-Bench 2.1 — Official Run Runbook

The end-to-end, copy-paste procedure to produce the **audit-eligible public
Stella row**. Companion to [`terminal-bench-2.1-protocol.md`](terminal-bench-2.1-protocol.md)
(the frozen design) and [`READINESS.md`](READINESS.md) (what's already verified).

Everything up to Part 4 is free/reversible. Parts 5+ spend real money and make a
public claim — **do them in order, and never improvise between the protocol, the
paid intent, and the command line.**

## Golden rules

1. **Preregister before you spend.** The public GitHub issue + intent ledger +
   timestamps are what make the number defensible. A paid job launched before its
   intent is public is **claim-ineligible** — money spent for nothing.
2. **Native x86_64 Linux Docker host only.** Not macOS. The frozen binary is
   Linux/glibc-2.17 (it runs *inside* the task containers); the *host* driving
   Harbor must be real Linux.
3. **Never print or shell-history either key.** Load them into the environment
   only (see Part 5).
4. **One-shot job directories.** The launcher refuses an existing job dir — a
   claim job never resumes. A genuinely new run gets a new disclosed job name.
5. **No outcome-selected reruns.** A failed/timed-out trial stays in the ledger.
   You may only stop for a non-performance operational failure.

---

## Part 0 — Provision the host

A dedicated cloud VM, nothing else running on it:

- **x86_64/amd64**, **≥4 effective vCPU**, **32 GiB memory class** (measured Linux
  `MemTotal` must be **≥31 GiB**), **≥150 GiB free** on the jobs filesystem.
- Ubuntu 22.04+ with: Docker Engine + the Compose plugin, `git`, `gh` (logged in
  as the `macanderson/stella` owner), `curl`, `rustup`, `zig` + `cargo-zigbuild`,
  and `uv`.
- Zero other Docker containers at each paid launch (`docker ps` empty) — the host
  attestation requires it.

```bash
# sanity
nproc                                   # >= 4
awk '/MemTotal/{print $2/1024/1024" GiB"}' /proc/meminfo   # >= 31
df -h --output=avail /var/lib/docker | tail -1             # >= 150G
docker ps                               # empty
harbor --version                        # 0.6.1 (installed via uv in Part 1)
```

---

## Part 1 — Clone + build the frozen claim binary

The SUT is finalized to **`fa2ec5b`** (public 0.5.1; see READINESS.md §1). Use the
**stock, unmodified** claim build (`harbor_adapter/README.md#install`) against the
current public `origin/main` tip — the `==@{upstream}` guard must pass unchanged.

```bash
git clone https://github.com/macanderson/stella.git && cd stella
export claim_repo="$PWD"
export claim_venv="$claim_repo/bench/harbor_adapter/.venv"
uv sync --project "$claim_repo/bench/harbor_adapter" --locked --extra dev

# Stock claim build → x86_64 glibc-2.17, full-SHA stamped (see README#install):
git fetch --quiet
test -z "$(git status --porcelain)"
claim_sha="$(git rev-parse HEAD)"
test "$claim_sha" = "$(git rev-parse '@{upstream}')"
claim_rustc="$(rustup which rustc)"; claim_cargo="$(rustup which cargo)"
claim_zig_cache="$(mktemp -d)"; mkdir -p "$claim_zig_cache/global" "$claim_zig_cache/local"
rustup target add x86_64-unknown-linux-gnu
RUSTC="$claim_rustc" \
ZIG_GLOBAL_CACHE_DIR="$claim_zig_cache/global" ZIG_LOCAL_CACHE_DIR="$claim_zig_cache/local" \
STELLA_BUILD_GIT_SHA="$claim_sha" \
"$claim_cargo" zigbuild --release --locked \
  --target x86_64-unknown-linux-gnu.2.17 --package stella-cli --bin stella

export STELLA_BINARY="$claim_repo/target/x86_64-unknown-linux-gnu/release/stella"
export STELLA_SOURCE_COMMIT="$claim_sha"
file "$STELLA_BINARY"                    # ELF ... x86-64 ... GNU/Linux 2.0.0
sha256sum "$STELLA_BINARY"               # record this — the run manifest freezes it
```

> The binary SHA is host-specific (it embeds the builder's rustup/cargo paths) —
> that's fine: the adapter re-verifies the *uploaded* binary's SHA against this
> host binary per trial, and the manifest freezes whatever this build produced.
> Identity is the source-commit stamp, not a cross-machine reproducible SHA.

**Offline smoke before spending anything:**
```bash
python3 bench/smoke/smoke_test.py --stella-bin "$STELLA_BINARY" || true  # linux bin won't run on the host's arch check if cross — see note
make bench-test                          # adapter + analyzer suites must be green
```

---

## Part 2 — Create the dedicated, spend-capped key

Use a **separate OpenRouter Management API key** to mint the benchmark key with
the exact frozen controls. Do this once; record only the non-secret fingerprint.

```bash
# OPENROUTER_MANAGEMENT_API_KEY must be in your env (never echoed).
curl -sS https://openrouter.ai/api/v1/keys \
  -H "Authorization: Bearer $OPENROUTER_MANAGEMENT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"name":"stella-tb21-dedicated-key-v1","limit":180,"limit_reset":null,"include_byok_in_limit":true}' \
  > /root/.stella-tb21-key.json           # contains the secret — chmod 600, never commit
jq '{name,label,limit,limit_reset,disabled}' /root/.stella-tb21-key.json   # must show disabled:false, limit:180
```

- Load the returned key as `OPENROUTER_API_KEY` (distinct from the management key).
- The **$180** hard cap sits below your $200 protocol authorization; your account
  now holds $500, so the live `/credits` ceiling is comfortable. The launcher
  fetches `/credits` before each job and refuses launch if the nominal allocation
  would cross the balance.

---

## Part 3 — Materialize the dataset + comparator, freeze identities + manifest

```bash
# The pinned 89-task dataset (Harbor also auto-pulls it at run time, but the freeze
# script needs it locally to recompute task checksums). `--export -o` writes
# <dir>/terminal-bench-2-1/<task>/ :
harbor download \
  terminal-bench/terminal-bench-2-1@sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a \
  --export -o "$claim_repo/.tb21-dataset"

# The comparator is MAINTAINER-PROVIDED (there is no in-repo downloader): the
# public Claude Code GLM-5.1 submission, Harbor job fd8707bb-…, pinned at
# terminal-bench commit 327a5a0b… /
# leaderboard/submissions/2026-05-01-glm-5-1-max-claude-code.json. Assemble
# $claim_repo/.tb21-comparator with manifest.json / submission.json / audit.json
# (their SHA-256 are hard-pinned) + trials/<id>/result.json for all 445 entries.

# Freeze the 89-task identity module (verifies dataset+comparator, must hash to
# 7e495afe…0125ece). This regenerates terminal_bench_analysis/tb21_study_seed.py:
cd "$claim_repo/bench/terminal_bench_analysis" && uv sync --locked --extra dev
uv run --no-sync python freeze_tb21_study_seed.py \
  --comparator-dir "$claim_repo/.tb21-comparator" \
  --dataset-dir "$claim_repo/.tb21-dataset" \
  --output "$claim_repo/bench/terminal_bench_analysis/tb21_study_seed.py"
```

Then **hand-fill the v6 study manifest** `bench/evidence/stella-tb21-study-manifest.json`
(schema `stella-tb21-study-manifest-v6`): replace its `REPLACE_AFTER_FREEZE` fields
with the frozen identities. Get every value from the generator's `frozen` mode —
it prints them exactly as the launcher computes them:

```bash
cd "$claim_repo/bench/harbor_adapter"
uv run --no-sync python ../tb21_preregistration.py frozen
```

The manifest's canonical SHA-256 is what the `confirmatory_freeze` preregistration
binds (`study_manifest_sha256`). Record it.

---

## Part 4 — Publish the preregistration  ⚠️ this is what makes it official

Produce the dedicated GitHub issue + **six** machine-readable comments (three
preregistrations: `readiness`, `calibration`, `confirmatory_freeze`; three paid
intents: `readiness`, `calibration`, `confirmatory`) and the append-only run
ledger (`preregistrations` / `intents` / `publications` / `outcomes`).

> **Author to `stella-tb21-run-ledger-v2` ONLY.** There is a *different* v3 schema
> in `tb21_evidence_contract.py` for a separate hybrid pipeline — the launcher and
> the timing verifier reject v3. Never hand-author these bodies: use the generator.
>
> **Generator:** `bench/tb21_preregistration.py` (provided). It reuses the
> launcher's OWN `_canonical_payload_sha256`, `_validate_current_intent`,
> `_expected_stage_dataset`, `_benchmark_engine_posture`, and the frozen constants,
> so every digest matches what the launcher recomputes at preflight, and each
> intent is checked against the launch contract before it is written.
>
> ```bash
> cd "$claim_repo/bench/harbor_adapter"
> # 1) after the host build + key + freeze, gather run-time values into host-inputs.json:
> #    subject_commit, confirmatory_freeze_commit, study_manifest_sha256,
> #    primary_job_name, binary_sha256, source_commit, agent_version,
> #    adapter_version, provider_key_fingerprint_sha256, and per_stage
> #    {usage_before_usd, snapshot_at, declared_at} (snapshot_at <= declared_at).
> uv run --no-sync python ../tb21_preregistration.py \
>   emit --host-inputs host-inputs.json --out-dir ../evidence/prereg
> ```
> It emits `issue-body.md`, the six `comment-*.json` bodies, and `run-ledger.json`
> (preregistrations + intents populated; `publications`/`outcomes` you append live),
> and prints the three `intent_sha256` values — each is that stage's
> `--intent-sha256` **and** its intent comment's `subject_id`. The only field left
> for you per comment is `ledger_commit` (the SHA of the commit that adds the
> ledger state you're publishing).

Ordering per stage transition (readiness → calibration → primary), because the
host report has a **15-minute freshness window**:

1. Compute the stage intent digest → `collect_public_host_report(...)` on this
   runner with that digest, stage, job name, absolute jobs root, `/usr/bin/docker`.
2. Write `canonical_json_bytes(report)` to
   `bench/evidence/host-attestations/<intent_sha256>.json`.
3. Commit that file **in the same `ledger_commit`** that appends the intent to the
   ledger; push it.
4. Create the unedited public intent comment on the preregistration issue.
5. Launch (Part 6/7/9) — within 15 minutes of the host report.

---

## Part 5 — Environment + isolated launcher bootstrap

```bash
export STELLA_JOBS_DIR="/srv/stella-tb21-jobs"      # absolute, private, not a symlink
mkdir -p "$STELLA_JOBS_DIR"; chmod 700 "$STELLA_JOBS_DIR"
export STELLA_BUDGET=0.17
export STELLA_DISABLE_REFLECTION=1
# OPENROUTER_API_KEY + OPENROUTER_MANAGEMENT_API_KEY already loaded (distinct).

# Preflight guards (from harbor_adapter/README.md#run — all must pass):
claim_jobs="$STELLA_JOBS_DIR"; case "$claim_jobs" in /*) ;; *) false ;; esac
test -d "$claim_jobs" && test ! -L "$claim_jobs"
test "$(command -v harbor)" = "$claim_venv/bin/harbor"
test "$(harbor --version)" = 0.6.1
test -x "$STELLA_BINARY"; test "${#STELLA_SOURCE_COMMIT}" = 40
umask 077
claim_adapter_root="$claim_repo/bench/harbor_adapter"
claim_site_root="$claim_venv/lib/python3.13/site-packages"

run_secure_launcher() {
  local pc; pc="$(mktemp -d "$claim_jobs/.stella-launcher-pycache.XXXXXX")"
  "$claim_venv/bin/python" -I -S -B -X "pycache_prefix=$pc" -c 'import sys
adapter_root, site_root = sys.argv[1:3]; del sys.argv[1:3]
sys.path[:0] = [adapter_root, site_root]
from stella_harbor.secure_launcher import main
raise SystemExit(main())' "$claim_adapter_root" "$claim_site_root" -- "$@"
}
```

---

## Part 6 — Readiness sentinel  (~$0.17)

One paid synthetic task; **permanently excluded** from model selection.

```bash
export READINESS_INTENT_SHA256=...            # the committed readiness intent digest
export READINESS_INTENT_COMMENT_URL=...       # its public comment URL
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
  --n-attempts 1 --n-concurrent 1 --max-retries 0
```

**GATE:** proceed only if it completes with no agent exception, terminal
`complete`, return code 0, and **external-verifier reward exactly `1.0`**. Anything
else → stop, diagnose, publicly commit an instrumentation fix + a new readiness
preregistration; do **not** change tasks/thresholds/selection.

---

## Part 7 — Calibration  (60 trials, ~$10.20)

```bash
export CALIBRATION_INTENT_SHA256=...; export CALIBRATION_INTENT_COMMENT_URL=...
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
  --n-attempts 2 --n-concurrent 3 --max-retries 0
```

**Selection (mechanical, run the analyzer):** ≥14/20 passes to be eligible, then
rank by more passes → lower projected 445-trial USD cost → frozen roster order
(DeepSeek V4 Pro, GLM-5.2, Grok-4.5). Tokens/wall-time are descriptive only. The
primary still uses **GLM-5.1** regardless of the calibration winner.

---

## Part 8 — Confirmatory freeze

Before observing any primary reward, publish the distinct `confirmatory_freeze`
preregistration recording the mechanically-selected calibration winner and the
**primary job name** (`PRIMARY_JOB_NAME`). It binds the exact study-manifest
SHA-256 but not the (future) Harbor job ID.

---

## Part 9 — Primary  (89 tasks × 5 = 445 trials, GLM-5.1, ~$75.65)

```bash
export PRIMARY_INTENT_SHA256=...; export PRIMARY_INTENT_COMMENT_URL=...
export PRIMARY_JOB_NAME=...                   # the publicly frozen name
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
  --n-attempts 5 --n-concurrent 1 --max-retries 0
```

Run all five attempts unconditionally — do **not** peek at attempt 1 and decide
whether to continue.

---

## Part 10 — Analyze + publish

1. **Secret-scan the whole job tree with the live key present**, then in a
   separate key-free step:
   `python bench/terminal_bench_analysis/artifact_secret_scan.py "$claim_jobs/$PRIMARY_JOB_NAME" --require-env OPENROUTER_API_KEY`
2. Run the analyzer over the 445 primary trials + the 60 calibration slots + the
   excluded-run ledger. It re-runs the public GitHub GETs in-process, validates the
   dataset ref/checksums/concurrency/retries/token accounting, recomputes accuracy
   and the 79-task bootstrap (seed `20260721`, 50,000 draws), and applies the
   registered `64.72%` / `358,905,384`-token thresholds.
3. Append the outcome records (Harbor job IDs, usage deltas, artifact digests) to
   the ledger and publish the evidence package (trajectories, ATIF, analysis
   script, machine-readable result table) — **including failures**.
4. Submit for the external Terminal-Bench maintainer trajectory review.

---

## Budget ledger & stop conditions

| Stage | Nominal model spend |
|---|---:|
| Readiness sentinel | $0.17 |
| Calibration (60 trials) | $10.20 |
| GLM-5.1 primary (445 trials) | $75.65 |
| **Total new-call plan** | **$86.02** |

Live `/credits` is checked before each job; a job is refused if its nominal
allocation would cross the balance or the remaining $200 authorization. Stop only
for a non-performance operational failure (bad/missing telemetry, credential
compromise, unavailable infra, insufficient balance) — any such stop makes the
confirmatory study incomplete and establishes no claim.

## One decision that's yours

**`headless_scope_bypass: on`** is a score-affecting posture flag (off → headless
plans over 5 steps self-terminate, so most tasks become unwinnable). It's kept
`on` because the task container is disposable and the per-trial budget cap is the
real guard. It's disclosed in the protocol's posture prose. Confirm you accept it
before Part 4 — it's the only judgment call in the whole run.
