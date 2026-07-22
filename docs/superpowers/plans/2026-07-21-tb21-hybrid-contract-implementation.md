# Terminal-Bench 2.1 hybrid contract implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the unexecuted v6 benchmark machinery with one shared hybrid-study contract and make the analyzer, public-timing verifier, host attestation, adapter, and secure launcher enforce it.

**Architecture:** A new pure `tb21_evidence_contract.py` owns all versioned schemas, fixed paths, lifecycle rules, budget rules, task splitting, and the SHA-256 sampler. A small `tb21_hybrid_analysis.py` owns the development, screen, and confirmatory estimands. Existing production acceptors import or digest-verified-load those modules, while the local authoring CLI is implemented separately in the dependent evidence-helper plan.

**Tech Stack:** Python 3.12/3.13 standard library, Harbor 0.6.1, pytest, Ruff, existing Rust workspace gates.

## Global Constraints

- Approved specs: `docs/superpowers/specs/2026-07-21-tb21-hybrid-study-design.md` and `docs/superpowers/specs/2026-07-21-tb21-evidence-helper-design.md`.
- Study/schema IDs are exactly `stella-tb21-hybrid-study-v1`, `stella-tb21-task-partition-v1`, `stella-tb21-run-ledger-v3`, `stella-tb21-study-manifest-v7`, and `stella-tb21-github-attestation-v3`.
- The dataset is exactly `terminal-bench/terminal-bench-2-1@sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a`; local materializations must reproduce every frozen Harbor task checksum.
- The comparator is public job `fd8707bb-51e8-56fa-8e46-769a82a531ae`, Claude Code 2.1.123 + GLM-5.1 at effort `max`, with exactly 445 trials, 261 verifier passes, and 398,783,761 normalized tokens; subset rows come only from its digest-pinned trial bytes.
- Readiness, development, screen, and confirmatory intents use exactly `openrouter/z-ai/glm-5.1`; model selection is not a tuning variable.
- The current provider authorization is exactly $100: $1 readiness, $54 development, $30 screen, and $15 unavailable reserve. The AWS authorization is a separate $55 tuning/screen cap.
- Development shapes are at most 80, 40, and 60 trials; screen is exactly 20 tasks x 5 attempts; official is exactly 89 x 5; primary inference is exactly the untouched 59 x 5.
- Every failed attempt stays in accuracy and token denominators. Missing or incomplete token/cost telemetry fails eligibility; no spend may be estimated from aggregates.
- Per-call accounting includes every successful, failed, and aborted paid-call envelope. Provider-delta reconciliation uses exact decimals and an absolute tolerance no greater than `$0.01`; every missing call ID is enumerated and makes the stage ineligible.
- Wall clock is descriptive and cannot establish a win without a separate same-host preregistration.
- Screen uses 50,000 paired task-cluster draws and requires at least 35,000 joint threshold passes. Confirmatory uses two one-sided 97.5% lower bounds at zero-based order index 1,249.
- Every 10% threshold and bootstrap ordering decision uses exact integer cross-products or `fractions.Fraction`, never binary floating-point. Currency/cost validation and ranking use integer cents for fixed caps and exact `decimal.Decimal` values for metered amounts. Reports carry exact numerator/denominator or decimal-string fields plus a finite display string.
- No Harbor retries, resumes, stitching, or individual replacement trials are allowed. The only replacement path is a fresh whole development job under the separately published amendment and unchanged-cap rules below. No NaN serialization, secret values, network calls in unit tests, cloud mutations, paid calls, uploads, pushes, or publication occur in this plan.
- The candidate contract can describe `direct`, `pipeline`, or `fleet`, but the launcher accepts only a topology with an implemented, witness-tested Stella execution path; production tuning cannot register or launch a merely declared topology.
- Paid host/launcher evidence fixes Docker to local `unix:///var/run/docker.sock`; ambient contexts, alternate Unix sockets, TCP, and SSH endpoints fail before host collection.
- All commits use Conventional Commits and `git commit -s`.

---

## File structure

### Create

- `bench/terminal_bench_analysis/freeze_tb21_study_seed.py` — one-purpose extractor that verifies the pinned comparator and writes the reviewed 89-record identity seed.
- `bench/terminal_bench_analysis/tb21_study_seed.py` — immutable task name/reference/checksum records plus pinned comparator provenance.
- `bench/terminal_bench_analysis/tb21_evidence_contract.py` — canonical schemas, strict JSON, fixed paths, task partition, lifecycle, budgets, and SHA-256 sampler.
- `bench/terminal_bench_analysis/tb21_hybrid_analysis.py` — pure development ranking, screen gate, and confirmatory estimands.
- `bench/terminal_bench_analysis/tests/test_tb21_evidence_contract.py` — golden contract, partition, lifecycle, and sampler witnesses.
- `bench/terminal_bench_analysis/tests/test_tb21_hybrid_analysis.py` — statistical and stage-shape witnesses.
- `bench/harbor_adapter/stella_harbor/contract_loader.py` — digest-verified loader for the pure analysis modules.

### Modify

- `bench/terminal_bench_analysis/pyproject.toml` and `uv.lock` — package the three new analysis modules and bump package version to `0.2.0`.
- `bench/terminal_bench_analysis/tb21_analysis.py` — consume the shared contract, validate hybrid jobs, expose read-only outcome derivation, and report screen/59-task/all-89 results.
- `bench/terminal_bench_analysis/github_public_timing.py` — derive required public subjects from v3 ledger records.
- `bench/terminal_bench_analysis/tests/test_tb21_analysis.py` and `test_github_public_timing.py` — migrate v6/v2 fixtures and claims.
- `bench/harbor_adapter/stella_harbor/secure_launcher.py` — consume the shared contract and gate dynamic hybrid intents with v3 receipts/preflight.
- `bench/harbor_adapter/stella_harbor/host_attestation.py` — bind the hybrid study/stages and one explicit local Docker Unix socket.
- `bench/harbor_adapter/stella_harbor/__init__.py` and `atif.py` — emit candidate/stage/config identities in result metadata and trajectory evidence.
- `bench/harbor_adapter/tests/test_secure_launcher.py`, `test_host_attestation.py`, and `test_adapter.py` — hybrid launch and metadata witnesses.
- `bench/harbor_adapter/pyproject.toml` and `uv.lock` — bump adapter version to `0.7.0` and package `contract_loader.py`.
- `bench/terminal-bench-2.1-protocol.md`, `bench/terminal_bench_analysis/README.md`, `bench/harbor_adapter/README.md`, and `bench/README.md` — replace v6 methodology and commands.
- `Makefile`, `.github/workflows/ci.yml`, and `AGENTS.md` — include locked Python benchmark gates.

---

### Task 1: Freeze the real task identity seed and canonical contract foundation

**Files:**
- Create: `bench/terminal_bench_analysis/freeze_tb21_study_seed.py`
- Create: `bench/terminal_bench_analysis/tb21_study_seed.py`
- Create: `bench/terminal_bench_analysis/tb21_evidence_contract.py`
- Create: `bench/terminal_bench_analysis/tests/test_tb21_evidence_contract.py`
- Modify: `bench/terminal_bench_analysis/pyproject.toml`

**Interfaces:**
- Produces: `TaskIdentity = tuple[str, str, str]`, `TASK_IDENTITIES`, `task_set_sha256(identities)`, `canonical_file_bytes(value) -> bytes`, `canonical_body_bytes(value) -> bytes`, `parse_canonical_object(raw, *, label) -> dict[str, object]`, `build_task_partition(identities) -> dict[str, object]`, and `validate_task_partition(value) -> dict[str, object]`.
- Consumes: the pinned local comparator at `../comparators/claude-code-glm-5.1`, whose manifest/submission/audit SHA-256 values are `7963a7af2b306fd4b6e82963fdadf9374e701ec16f47b194300e2843c8002a76`, `36d20c181be246dc55965bf4320a3005f292c737f31511bbde19ba1808a2bd2c`, and `2c214bbeff6963a8a4e54f7bf0c2f8e76c8dac31053a3d40d036c2e284f12687`; comparator job ID is `fd8707bb-51e8-56fa-8e46-769a82a531ae`.
- Consumes: the locally materialized pinned dataset at `../datasets/terminal-bench-2-1`; Harbor 0.6.1 must derive exactly the same 89 task names, refs, and task checksums as the reviewed seed.

- [ ] **Step 1: Write the failing seed and partition witnesses**

```python
def test_real_seed_and_partition_are_frozen() -> None:
    assert len(TASK_IDENTITIES) == 89
    assert task_set_sha256(TASK_IDENTITIES) == (
        "7e495afe0a86eaf572be1c2da2b9929c24e502adc888e550385d915cc0125ece"
    )
    partition = build_task_partition(TASK_IDENTITIES)
    assert [len(partition[name]) for name in ("development", "screen", "untouched")] == [10, 20, 59]
    assert partition["split_sha256"] == {
        "development": "265ef7896a287493fd846b5835d8eecb83e0e1dd74036aebd4c8e603cf5d3105",
        "screen": "48828ea2c4fab2b7791a1b4e76e7d764c18cc94efb631bc944325aa91ace9866",
        "untouched": "324cfb122eb8220b4f7a177a932f1af45e5e4948fc22c9294156477d157bc26e",
    }
    assert [item["task_name"] for item in partition["screen"]] == [
        "extract-moves-from-video", "pytorch-model-recovery", "dna-assembly",
        "path-tracing-reverse", "extract-elf", "build-cython-ext",
        "polyglot-c-py", "sparql-university", "polyglot-rust-c",
        "sqlite-db-truncate", "password-recovery", "build-pmars",
        "qemu-startup", "largest-eigenval", "regex-chess",
        "model-extraction-relu-logits", "mailman", "git-multibranch",
        "nginx-request-logging", "protein-assembly",
    ]
    assert validate_task_partition(partition) == partition
```

- [ ] **Step 2: Run the witness and verify the missing modules fail**

Run: `cd bench/terminal_bench_analysis && uv run --frozen --extra dev pytest -q tests/test_tb21_evidence_contract.py`

Expected: collection fails with `ModuleNotFoundError` for the not-yet-created contract/seed modules.

- [ ] **Step 3: Implement strict canonical JSON and the approved split**

```python
STUDY_ID = "stella-tb21-hybrid-study-v1"
TASK_PARTITION_SCHEMA = "stella-tb21-task-partition-v1"
DEVELOPMENT_TASK_NAMES = (
    "fix-git", "filter-js-from-html", "kv-store-grpc",
    "large-scale-text-editing", "regex-log", "schemelike-metacircular-eval",
    "sqlite-with-gcov", "bn-fit-modify", "make-mips-interpreter",
    "train-fasttext",
)

def canonical_file_bytes(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"),
                       ensure_ascii=False, allow_nan=False) + "\n").encode()

def canonical_body_bytes(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, allow_nan=False).encode()

def build_task_partition(identities: Sequence[TaskIdentity]) -> dict[str, object]:
    by_name = {name: (name, ref, checksum) for name, ref, checksum in identities}
    development = [by_name[name] for name in DEVELOPMENT_TASK_NAMES]
    remaining = [item for item in identities if item[0] not in DEVELOPMENT_TASK_NAMES]
    remaining.sort(key=lambda item: (
        hashlib.sha256((STUDY_ID + "\0" + item[1]).encode()).digest(), item[1]
    ))
    record = lambda item: {
        "task_name": item[0], "canonical_task_reference": item[1],
        "task_checksum": item[2],
    }
    splits = {
        "development": [record(item) for item in development],
        "screen": [record(item) for item in remaining[:20]],
        "untouched": [record(item) for item in remaining[20:]],
    }
    return {
        "schema_version": TASK_PARTITION_SCHEMA,
        "study_id": STUDY_ID,
        **splits,
        "split_sha256": {
            name: hashlib.sha256(canonical_body_bytes(records)).hexdigest()
            for name, records in splits.items()
        },
    }
```

The strict parser must reject duplicate keys, non-object roots, noncanonical bytes, NaN/infinity, extra fields, missing fields, duplicate task names/references, incorrect per-split digests, and any split not equal to the frozen seed. The whole-partition digest is the SHA-256 of `canonical_file_bytes(partition)` and is stored by the ledger/helper rather than self-embedded.

Implement `freeze_tb21_study_seed.py` in the same step as a one-purpose offline extractor: fixed control-file digests/job ID, strict manifest/result allowlists, no network/environment fallback, Harbor 0.6.1 local task-checksum verification, deterministic source formatting, and create-or-identical output semantics.

- [ ] **Step 4: Generate and review the 89-record source seed**

Run:

```bash
cd bench/terminal_bench_analysis
uv run --frozen python freeze_tb21_study_seed.py \
  --comparator-dir ../../../comparators/claude-code-glm-5.1 \
  --dataset-dir ../../../datasets/terminal-bench-2-1 \
  --output tb21_study_seed.py
uv run --frozen --extra dev pytest -q tests/test_tb21_evidence_contract.py
```

Expected: the extractor verifies the three pinned comparator control-file digests, all 445 manifest `result_sha256` values with no extra/missing trials, the public job ID, and every local Harbor 0.6.1 task checksum against the pinned dataset ref; it obtains exactly one stable name/ref/checksum triple per task, writes deterministic Python source, and the test passes.

- [ ] **Step 5: Commit the contract foundation**

```bash
git add bench/terminal_bench_analysis/freeze_tb21_study_seed.py \
  bench/terminal_bench_analysis/tb21_study_seed.py \
  bench/terminal_bench_analysis/tb21_evidence_contract.py \
  bench/terminal_bench_analysis/tests/test_tb21_evidence_contract.py \
  bench/terminal_bench_analysis/pyproject.toml
git commit -s -m "feat(bench): add hybrid study evidence contract"
```

### Task 2: Add exact v3 ledger, candidate, budget, and lifecycle validation

**Files:**
- Modify: `bench/terminal_bench_analysis/tb21_evidence_contract.py`
- Modify: `bench/terminal_bench_analysis/tests/test_tb21_evidence_contract.py`

**Interfaces:**
- Produces: `build_initial_ledger(partition_sha256, *, authorization_commit, declared_at)`, `validate_run_ledger(value)`, `next_sequence(ledger)`, `append_candidate`, `append_budget_authorization`, `append_preregistration`, `append_intent`, `append_publication`, `append_outcome`, `required_public_subjects`, and `stage_shape(stage)`.
- Produces exact paid stage IDs: `readiness`, `development_round_1`, `development_round_2`, `development_round_3`, `screen`, and `confirmatory`.

- [ ] **Step 1: Write lifecycle witnesses before implementation**

```python
def test_hybrid_lifecycle_accepts_many_intents_and_one_global_sequence() -> None:
    ledger = build_initial_ledger(
        "a" * 64, authorization_commit="a" * 40,
        declared_at="2026-07-21T12:00:00-07:00",
    )
    candidate = candidate_record(sequence=3, candidate_id="dev-r1-a")
    ledger = append_candidate(ledger, candidate)
    ledger = append_preregistration(ledger, preregistration_record(sequence=4, kind="development_round_1"))
    ledger = append_publication(ledger, publication_record(sequence=5, subject_type="preregistration", subject_id="development_round_1"))
    ledger = append_intent(ledger, intent_record(sequence=6, stage="development_round_1", candidate_id="dev-r1-a"))
    ledger = append_publication(ledger, publication_record(sequence=7, subject_type="intent", subject_id="b" * 64))
    assert validate_run_ledger(ledger) == ledger
    assert required_public_subjects(ledger) == (
        ("preregistration", "development_round_1"), ("intent", "b" * 64)
    )
```

The required parameterized mutation matrix is exactly: sequence reuse, booleans as integers, naive timestamps, publication-only delta drift, candidate edits, round overfill, unamended development replacement, incomplete-candidate promotion, wrong key authorization, reserve reuse, and confirmatory records without a new explicit authorization.

- [ ] **Step 2: Run the focused test and verify contract failures**

Run: `cd bench/terminal_bench_analysis && uv run --frozen --extra dev pytest -q tests/test_tb21_evidence_contract.py -k lifecycle`

Expected: the legal lifecycle test fails because the append/validation functions do not exist.

- [ ] **Step 3: Implement the exact top-level schema and stage table**

```python
RUN_LEDGER_SCHEMA = "stella-tb21-run-ledger-v3"
RUN_LEDGER_FIELDS = frozenset({
    "schema_version", "study_id", "paths", "task_partition_sha256",
    "budget_authorizations", "prior_exploration_disclosure", "preregistrations",
    "candidates", "intents", "publications", "outcomes",
})
STAGE_SHAPES = {
    "readiness": {"tasks": 1, "attempts": 1, "max_candidates": 1, "max_intents": 1, "max_trials": 1, "max_spend_cents": 100, "harbor_concurrency": 1},
    "development_round_1": {"tasks": 10, "attempts": 1, "max_candidates": 8, "max_intents": 16, "max_trials": 80, "max_spend_cents": 2_400, "harbor_concurrency": 3},
    "development_round_2": {"tasks": 10, "attempts": 1, "max_candidates": 4, "max_intents": 8, "max_trials": 40, "max_spend_cents": 1_200, "harbor_concurrency": 3},
    "development_round_3": {"tasks": 10, "attempts": 3, "max_candidates": 2, "max_intents": 4, "max_trials": 60, "max_spend_cents": 1_800, "harbor_concurrency": 3},
    "screen": {"tasks": 20, "attempts": 5, "max_candidates": 1, "max_intents": 1, "max_trials": 100, "max_spend_cents": 3_000, "harbor_concurrency": 1},
    "confirmatory": {"tasks": 89, "attempts": 5, "max_candidates": 1, "max_intents": 1, "max_trials": 445, "max_spend_cents": None, "harbor_concurrency": 1},
}
```

The initial authorization must encode provider 10,000 cents and infrastructure 5,500 cents exactly and make the 1,500-cent reserve unusable. `None` in the confirmatory stage table means not authorized, never unlimited: confirmatory validation must require a later budget record with its own key name, finite hard limit, finite provider cap, and finite infrastructure cap. Metered provider/telemetry values are parsed as bounded, nonnegative decimal strings and never through binary float. Every record array carries a globally unique positive `sequence`.

Candidate validation freezes the source commit, binary/source/config/adapter/analyzer/Harbor/contract digests, exact GLM-5.1 route policy, topology, role/effort/reasoning/concurrency posture, per-trial limit no greater than $0.30, task split, attempts, retries, and job name. `max_candidates` limits distinct entrants; the larger development-only `max_intents` allows at most one replacement intent per entrant. A replacement is legal only when a completed ineligible outcome and separately published `development_amendment` name the invalid job, preserved artifact digest, reason, exact unchanged candidate/config digests, and new job name. Attempted-trial and spend caps still include the invalid job, so a full-entrant round can replace only a zero-trial failure unless enough canonical capacity remains. Screen and confirmatory never admit replacements.

- [ ] **Step 4: Run all contract witnesses**

Run: `cd bench/terminal_bench_analysis && uv run --frozen --extra dev pytest -q tests/test_tb21_evidence_contract.py`

Expected: all contract tests pass with no network, filesystem, environment, or subprocess access from the contract module.

- [ ] **Step 5: Commit the lifecycle contract**

```bash
git add bench/terminal_bench_analysis/tb21_evidence_contract.py \
  bench/terminal_bench_analysis/tests/test_tb21_evidence_contract.py
git commit -s -m "feat(bench): validate hybrid study lifecycle"
```

### Task 3: Implement deterministic hybrid statistics

**Files:**
- Modify: `bench/terminal_bench_analysis/tb21_evidence_contract.py`
- Create: `bench/terminal_bench_analysis/tb21_hybrid_analysis.py`
- Create: `bench/terminal_bench_analysis/tests/test_tb21_hybrid_analysis.py`
- Modify: `bench/terminal_bench_analysis/pyproject.toml`

**Interfaces:**
- Consumes: the contract-owned `sampler_preimage`, `sha256_counter_index`, and `bootstrap_index_stream_sha256`, ordered partition records, and complete normalized integer trial rows.
- Produces in `tb21_hybrid_analysis.py`: `rank_development_candidates`, `development_gate`, `analyze_screen`, and `analyze_confirmatory`.

- [ ] **Step 1: Write sampler, development, screen, and confirmatory witnesses**

```python
def test_frozen_sampler_vectors_and_stream_digests() -> None:
    assert sampler_preimage(SCREEN_DOMAIN, 20260721, 0, 0, 0) == (
        b"stella-tb21-screen-bootstrap-v1\x0020260721\x000\x000\x000"
    )
    assert [sha256_counter_index(SCREEN_DOMAIN, 20260721, 0, j, 20) for j in range(5)] == [18, 19, 15, 8, 5]
    assert [sha256_counter_index(CONFIRMATORY_DOMAIN, 20260721, 0, j, 59) for j in range(5)] == [32, 18, 34, 52, 32]
    assert bootstrap_index_stream_sha256(SCREEN_DOMAIN, 20, 50_000) == "c2215fe72122fba82f0259ef751d970dd0a69eace5f067d45e4e2de7c36abe14"
    assert bootstrap_index_stream_sha256(CONFIRMATORY_DOMAIN, 59, 50_000) == "d192cb83b44eb6c9777b62bf9839e05d7f618b253c2156313c0c81efdc3f9152"

def test_confirmatory_requires_both_strict_lower_bounds() -> None:
    result = analyze_confirmatory(stella_rows(), claude_rows(), untouched_partition())
    assert result["accuracy"]["point_improvement"] >= Fraction(1, 10)
    assert result["tokens"]["point_improvement"] >= Fraction(1, 10)
    assert result["accuracy"]["lower_bound_order_index"] == 1249
    assert result["claim_established"] is True

def test_exact_ten_percent_boundary_is_not_binary_float_dependent() -> None:
    assert meets_threshold(Fraction(1, 10), strict=False) is True
    assert meets_threshold(Fraction(1, 10), strict=True) is False
    assert accuracy_threshold(stella_passes=11, claude_passes=10, strict=False)
    assert not accuracy_threshold(stella_passes=11, claude_passes=10, strict=True)
    assert token_threshold(stella_tokens=9, claude_tokens=10, strict=False)
    assert not token_threshold(stella_tokens=9, claude_tokens=10, strict=True)
```

Also witness the 3-vs-5 task-balanced development denominator, complete-first ranking, 35,000 joint screen boundary, zero-Claude-accuracy miss, nonpositive Claude-token failure, assigned negative infinity sorting, and unexpected NaN/positive infinity failure.

- [ ] **Step 2: Run tests and observe missing statistics module**

Run: `cd bench/terminal_bench_analysis && uv run --frozen --extra dev pytest -q tests/test_tb21_hybrid_analysis.py`

Expected: collection fails because `tb21_hybrid_analysis` does not exist.

- [ ] **Step 3: Implement the contract sampler and analysis lower bound**

```python
def sampler_preimage(domain: str, seed: int, replicate: int,
                     draw: int, retry: int) -> bytes:
    if domain not in {SCREEN_DOMAIN, CONFIRMATORY_DOMAIN}:
        raise ContractError("contract", "sampler domain is not registered")
    integers = (seed, replicate, draw, retry)
    if any(not isinstance(value, int) or isinstance(value, bool) or value < 0
           for value in integers):
        raise ContractError("contract", "sampler counters must be nonnegative integers")
    return b"\x00".join(
        [domain.encode("ascii"), *(str(value).encode("ascii") for value in integers)]
    )

def sha256_counter_index(domain: str, seed: int, replicate: int, draw: int, n: int) -> int:
    if not isinstance(n, int) or isinstance(n, bool) or not 0 < n <= 65_535:
        raise ContractError("contract", "sampler task count is invalid")
    retry = 0
    limit = (1 << 64) // n * n
    while True:
        raw = sampler_preimage(domain, seed, replicate, draw, retry)
        value = int.from_bytes(hashlib.sha256(raw).digest()[:8], "big")
        if value < limit:
            return value % n
        retry += 1

def accuracy_improvement(stella_passes: int, claude_passes: int) -> Fraction:
    if claude_passes <= 0:
        raise HybridAnalysisError("observed comparator accuracy must be positive")
    return Fraction(stella_passes, claude_passes) - 1

def token_improvement(stella_tokens: int, claude_tokens: int) -> Fraction:
    if claude_tokens <= 0:
        raise HybridAnalysisError("comparator token total must be positive")
    return 1 - Fraction(stella_tokens, claude_tokens)

def lower_percentile(values: Sequence[ExactImprovement]) -> ExactImprovement:
    if len(values) != 50_000 or any(not is_exact_improvement(v) for v in values):
        raise HybridAnalysisError("confirmatory bootstrap produced an invalid value")
    return sorted(values, key=exact_improvement_sort_key)[1249]
```

Place `sampler_preimage`, `sha256_counter_index`, and `bootstrap_index_stream_sha256` in `tb21_evidence_contract.py`; `tb21_hybrid_analysis.py` imports them and owns only estimands and gates. `sampler_preimage` joins the five ASCII decimal fields with actual byte `0x00`, never the two characters backslash-plus-zero. The index-stream digest encoding is exactly prefix ASCII `stella-tb21-bootstrap-index-stream-v1` plus `0x00`, stage-domain ASCII plus `0x00`, unsigned big-endian u64 seed, u32 task count, u64 replicate count, then every selected index as unsigned big-endian u16 in replicate-major/draw-minor order.

- [ ] **Step 4: Implement and verify all estimands**

`ExactImprovement` is either a `Fraction` or the one internal negative-infinity sentinel allowed only for zero-comparator-accuracy bootstrap replicates; it is never serialized as a JSON number. Development uses exact fractional ten-task means with three Stella and five Claude attempts. Screen samples the same indices for accuracy and tokens and counts a joint pass through integer cross-products only when both improvements are at least exactly 1/10. Confirmatory compares point estimates to `>= Fraction(1, 10)` and both lower bounds to `> Fraction(1, 10)`, computes the scientific result only on 59 tasks, and emits a separate descriptive all-89 result. Because the pinned comparator is not a same-host run, every report sets `wall_clock_claim_eligible:false`; wall time is never included in claim logic.

Run: `cd bench/terminal_bench_analysis && uv run --frozen --extra dev pytest -q tests/test_tb21_hybrid_analysis.py`

Expected: all statistical witnesses pass.

- [ ] **Step 5: Commit hybrid statistics**

```bash
git add bench/terminal_bench_analysis/tb21_hybrid_analysis.py \
  bench/terminal_bench_analysis/tb21_evidence_contract.py \
  bench/terminal_bench_analysis/tests/test_tb21_hybrid_analysis.py \
  bench/terminal_bench_analysis/pyproject.toml
git commit -s -m "feat(bench): add deterministic hybrid analysis"
```

### Task 4: Migrate the analyzer to v3/v7 and expose outcome derivation

**Files:**
- Modify: `bench/terminal_bench_analysis/tb21_analysis.py`
- Modify: `bench/terminal_bench_analysis/tests/test_tb21_analysis.py`
- Modify: `bench/terminal_bench_analysis/pyproject.toml`
- Modify: `bench/terminal_bench_analysis/uv.lock`

**Interfaces:**
- Consumes: `--task-partition`, `--study-manifest`, `--run-ledger`, repeatable `--ledger-job`, Stella job inputs, and pinned comparator inputs.
- Produces: `derive_completed_job_evidence(job_dir, *, expected_intent) -> dict[str, object]`, `derive_stage_result(job_evidence, *, stage, partition, comparator_rows) -> dict[str, object]`, and report sections `development`, `screen`, `confirmatory_primary`, and `official_secondary`.

- [ ] **Step 1: Replace obsolete v6 fixtures with failing hybrid witnesses**

```python
def test_full_89_is_secondary_and_only_59_tasks_drive_the_claim(tmp_path: Path) -> None:
    report = build_hybrid_report(tmp_path, passing=True)
    assert report["confirmatory_primary"]["task_count"] == 59
    assert report["official_secondary"]["task_count"] == 89
    assert report["confirmatory_primary"]["claim_established"] is True
    assert "wins" not in report["confirmatory_primary"]

def test_completed_job_evidence_is_derived_without_mutation(tmp_path: Path) -> None:
    job = complete_hybrid_job(tmp_path)
    before = artifact_tree_sha256(job)
    evidence = derive_completed_job_evidence(job, expected_intent=hybrid_intent())
    assert evidence["trial_count"] == 100
    assert evidence["accounting_complete"] is True
    assert artifact_tree_sha256(job) == before

def test_complete_but_statistically_failed_screen_is_a_valid_result(tmp_path: Path) -> None:
    evidence = derive_completed_job_evidence(complete_screen_job(tmp_path),
                                             expected_intent=screen_intent())
    result = derive_stage_result(evidence, stage="screen",
                                 partition=partition(), comparator_rows=comparator())
    assert result["artifact_eligible"] is True
    assert result["gate_passed"] is False
    assert result["joint_threshold_passes"] < 35_000
```

Replace tests that assert v6, 79 inference tasks, three dimensions, cross-model calibration, fixed six publications, or `wins >= 2`. Preserve ingestion, ATIF, comparator, retry, telemetry, and secret-failure tests.

- [ ] **Step 2: Run targeted analyzer witnesses and confirm red state**

Run: `cd bench/terminal_bench_analysis && uv run --frozen --extra dev pytest -q tests/test_tb21_analysis.py -k 'hybrid or completed_job or 59_tasks'`

Expected: failures show the analyzer still reports v6 calibration/79-task semantics.

- [ ] **Step 3: Wire the shared modules into analyzer validation**

```python
from tb21_evidence_contract import (
    STUDY_MANIFEST_SCHEMA, parse_canonical_object, validate_run_ledger,
    validate_study_manifest, validate_task_partition,
)
from tb21_hybrid_analysis import (
    analyze_confirmatory, analyze_screen, development_gate,
    rank_development_candidates,
)
```

Implement the analyzer's filesystem-facing `load_task_partition(path)` by reading strict canonical bytes, calling `parse_canonical_object`, and then `validate_task_partition`; the pure contract remains filesystem-free. `derive_stage_result` selects the registered split and exact pinned comparator rows, invokes the shared deterministic statistics, and returns exact rational fields, sampler/stream digests, artifact eligibility, stage-gate status, verifier/token totals, and the analysis-input digest. Paid-call accounting enumerates expected call IDs plus abort envelopes, reports every missing ID, and never estimates spend from aggregate provider totals. Delete duplicated v2/v6 field sets. Keep `_validate_run_ledger()` only for binding validated ledger records to observed jobs, receipts, telemetry, provider reconciliation, and publication evidence. Derive required jobs and comments from ledger records instead of fixed counts.

- [ ] **Step 4: Add CLI/report inputs and fail-closed serialization**

`report.json` must use `allow_nan=False`; each `Fraction` is converted to an exact `{numerator, denominator, decimal}` record before serialization, with `decimal` quantized to 12 fractional places using `ROUND_HALF_EVEN` and never used for a gate. Assigned negative infinity remains internal and is reported as a counted conservative miss, never as a JSON number. Add `--task-partition` and repeatable `--ledger-job`; remove claim dependence on the old `--calibration-job` and `--calibration-ledger-job` flags.

Run: `cd bench/terminal_bench_analysis && uv run --frozen --extra dev pytest -q`

Expected: all analyzer tests pass.

- [ ] **Step 5: Commit analyzer migration**

```bash
git add bench/terminal_bench_analysis/tb21_analysis.py \
  bench/terminal_bench_analysis/tests/test_tb21_analysis.py \
  bench/terminal_bench_analysis/pyproject.toml \
  bench/terminal_bench_analysis/uv.lock
git commit -s -m "feat(bench): analyze hybrid Terminal-Bench stages"
```

### Task 5: Make public-timing evidence lifecycle-derived

**Files:**
- Modify: `bench/terminal_bench_analysis/github_public_timing.py`
- Modify: `bench/terminal_bench_analysis/tests/test_github_public_timing.py`

**Interfaces:**
- Consumes: the shared v3 ledger/comment-body contract and locally supplied GitHub exports for every required subject.
- Produces: `stella-tb21-github-comments-v3` input validation and `stella-tb21-public-timing-audit-v4` reports.

- [ ] **Step 1: Write a variable-subject witness**

```python
def test_public_timing_derives_every_subject_from_the_ledger() -> None:
    ledger = ledger_with_two_development_candidates()
    audit = verify_public_timing(ledger=ledger, comment_exports=exports_for(ledger))
    assert audit.report["required_subject_count"] == 3
    assert audit.report["verified_subject_count"] == 3
    assert audit.report["all_verified"] is True
```

- [ ] **Step 2: Run the witness and confirm the old six-comment assumption fails**

Run: `cd bench/terminal_bench_analysis && uv run --frozen --extra dev pytest -q tests/test_github_public_timing.py`

Expected: the new witness fails because the verifier requires exactly three preregistrations and three intents.

- [ ] **Step 3: Reuse shared parsing and derive subjects**

```python
required = contract.required_public_subjects(contract.validate_run_ledger(ledger))
if set(exports_by_subject) != set(required):
    raise PublicTimingError("GitHub exports do not exactly cover ledger subjects")
```

Preserve owner association, unchanged body, server timestamps, strict ancestry, two-second margin, final GET, anonymous visibility, and current-main checks.

- [ ] **Step 4: Run public-timing and analyzer suites**

Run: `cd bench/terminal_bench_analysis && uv run --frozen --extra dev pytest -q tests/test_github_public_timing.py tests/test_tb21_analysis.py`

Expected: both suites pass.

- [ ] **Step 5: Commit public-timing migration**

```bash
git add bench/terminal_bench_analysis/github_public_timing.py \
  bench/terminal_bench_analysis/tests/test_github_public_timing.py
git commit -s -m "feat(bench): derive public attestations from ledger"
```

### Task 6: Gate hybrid intents in the host, adapter, and secure launcher

**Files:**
- Create: `bench/harbor_adapter/stella_harbor/contract_loader.py`
- Modify: `bench/harbor_adapter/stella_harbor/secure_launcher.py`
- Modify: `bench/harbor_adapter/stella_harbor/host_attestation.py`
- Modify: `bench/harbor_adapter/stella_harbor/__init__.py`
- Modify: `bench/harbor_adapter/stella_harbor/atif.py`
- Modify: `bench/harbor_adapter/tests/test_secure_launcher.py`
- Modify: `bench/harbor_adapter/tests/test_host_attestation.py`
- Modify: `bench/harbor_adapter/tests/test_adapter.py`
- Modify: `bench/harbor_adapter/pyproject.toml`
- Modify: `bench/harbor_adapter/uv.lock`

**Interfaces:**
- Produces: `load_frozen_analysis_modules(repo_root)`, wrapper option `--stage`, v3 secure receipt/public preflight, dynamic intent shape validation, and exact candidate metadata.
- Consumes: `stage_shape`, `validate_run_ledger`, `validate_study_manifest`, and source digests from Task 1.

- [ ] **Step 1: Write launch witnesses before changing production code**

```python
@pytest.mark.parametrize("stage", [
    "readiness", "development_round_1", "development_round_2",
    "development_round_3", "screen", "confirmatory",
])
def test_secure_launcher_accepts_only_contract_derived_stage_shapes(stage: str) -> None:
    command, ledger = canonical_launch(stage)
    validate_stage_shape(command, contract_intent(ledger, stage))

def test_adapter_emits_candidate_identity_in_every_trial() -> None:
    result = run_adapter_fixture(candidate_id="dev-r1-a", config_sha256="a" * 64)
    assert result.metadata["stella_tb21_candidate_id"] == "dev-r1-a"
    assert result.metadata["stella_tb21_candidate_config_sha256"] == "a" * 64
```

The required parameterized red-test matrix is exactly: old v2/v6 bytes, alternate models, task split drift, over-budget intents, reserve use, retries, arbitrary job names not bound by the intent, missing candidate identity, declared-but-unimplemented topology, ambient seed/contract/hybrid module shadowing, restoration after a failed dynamic import, remote Docker contexts, extra adapter Python sources, and source digest drift.

- [ ] **Step 2: Run focused launcher tests and confirm red state**

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q tests/test_secure_launcher.py tests/test_host_attestation.py tests/test_adapter.py`

Expected: new hybrid cases fail against the fixed readiness/calibration/confirmatory launcher.

- [ ] **Step 3: Implement the digest-verified loader and source binding**

```python
def load_frozen_analysis_modules(repository_root: Path) -> FrozenAnalysisModules:
    seed = load_frozen_source(repository_root, "tb21_study_seed.py",
                              "stella_tb21_frozen_seed")
    with temporary_exact_module("tb21_study_seed", seed):
        contract = load_frozen_source(repository_root, "tb21_evidence_contract.py",
                                      "stella_tb21_frozen_contract")
    with temporary_exact_module("tb21_evidence_contract", contract):
        analysis = load_frozen_source(repository_root, "tb21_hybrid_analysis.py",
                                      "stella_tb21_frozen_hybrid_analysis")
    return FrozenAnalysisModules(seed=seed, contract=contract, analysis=analysis)
```

`load_frozen_source` first verifies the resolved regular-file path and source-frozen expected digest, then compiles the exact bytes. `temporary_exact_module` restores the prior `sys.modules` entry even on failure, so an ambient package cannot satisfy or poison either import. When the full analyzer is executed, the loader injects these exact contract and hybrid-analysis module objects for that execution and restores all prior entries afterward. The runtime identity and every paid intent bind `evidence_contract_sha256`, `study_seed_sha256`, and `hybrid_analysis_sha256`. `_FIXED_ADAPTER_SOURCE_PATHS` includes `contract_loader.py`, and public source verification rejects any unbound extra Python file.

- [ ] **Step 4: Replace fixed stage inference with intent-derived validation**

Parse `--stage` as a launcher-only option, validate it against the v3 public intent, and forward only the canonical Harbor command. The launcher injects nonsecret, intent-derived `STELLA_TB21_STAGE`, `STELLA_TB21_CANDIDATE_ID`, `STELLA_TB21_CANDIDATE_CONFIG_SHA256`, and `STELLA_TB21_INTENT_SHA256`; ambient values with those names are rejected before injection.

Validate candidate topology against both the frozen `direct|pipeline|fleet` vocabulary and a launcher-owned set of topologies whose concrete execution paths have passing adapter witnesses. A candidate using a recognized but unavailable topology fails before host collection or any paid call.

Host reports bind the hybrid study ID, paid stage, job name, and exact local `unix:///var/run/docker.sock`. Every Docker probe passes that explicit host; `DOCKER_HOST`, Docker contexts, TCP/SSH endpoints, and alternate Unix paths fail.

- [ ] **Step 5: Bump exact receipt/preflight and package versions**

Use `stella-harbor-secure-launch-receipt-v3` and `stella-harbor-public-intent-preflight-v3`. Keep the runtime and management credentials distinct, host-only, scrubbed, and excluded from all public bytes. Bump the adapter package to `0.7.0` and update the lock.

Run: `cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q`

Expected: all adapter/host/launcher tests pass without Docker or network access.

- [ ] **Step 6: Commit the production acceptor migration**

```bash
git add bench/harbor_adapter/stella_harbor/contract_loader.py \
  bench/harbor_adapter/stella_harbor/secure_launcher.py \
  bench/harbor_adapter/stella_harbor/host_attestation.py \
  bench/harbor_adapter/stella_harbor/__init__.py \
  bench/harbor_adapter/stella_harbor/atif.py \
  bench/harbor_adapter/tests/test_secure_launcher.py \
  bench/harbor_adapter/tests/test_host_attestation.py \
  bench/harbor_adapter/tests/test_adapter.py \
  bench/harbor_adapter/pyproject.toml bench/harbor_adapter/uv.lock
git commit -s -m "feat(bench): gate hybrid paid stages"
```

### Task 7: Put Python evidence tooling in the repository gate and publish the workflow docs

**Files:**
- Modify: `Makefile`
- Modify: `.github/workflows/ci.yml`
- Modify: `AGENTS.md`
- Modify: `bench/terminal-bench-2.1-protocol.md`
- Modify: `bench/terminal_bench_analysis/README.md`
- Modify: `bench/harbor_adapter/README.md`
- Modify: `bench/README.md`

**Interfaces:**
- Produces: `make bench-sync`, `bench-format`, `bench-format-check`, `bench-lint`, `bench-test`, and `bench-gate`.

- [ ] **Step 1: Add a failing gate-surface check**

Run: `make bench-gate`

Expected: Make exits with `No rule to make target 'bench-gate'`.

- [ ] **Step 2: Add exact locked Python targets**

```make
.PHONY: bench-sync bench-format bench-format-check bench-lint bench-test bench-gate
bench-sync:
	cd bench/terminal_bench_analysis && uv sync --frozen --extra dev
	cd bench/harbor_adapter && uv sync --frozen --extra dev
bench-format:
	cd bench/terminal_bench_analysis && uv run --frozen --extra dev ruff format .
	cd bench/harbor_adapter && uv run --frozen --extra dev ruff format .
bench-format-check:
	cd bench/terminal_bench_analysis && uv run --frozen --extra dev ruff format --check .
	cd bench/harbor_adapter && uv run --frozen --extra dev ruff format --check .
bench-lint:
	cd bench/terminal_bench_analysis && uv run --frozen --extra dev ruff check .
	cd bench/harbor_adapter && uv run --frozen --extra dev ruff check .
bench-test:
	cd bench/terminal_bench_analysis && uv run --frozen --extra dev pytest -q
	cd bench/harbor_adapter && uv run --frozen --extra dev pytest -q
bench-gate: bench-format-check bench-lint bench-test
```

Make the existing `gate` depend on `bench-gate`; CI installs `uv`, runs `uv sync --frozen` for both projects, then calls the same Make target.

- [ ] **Step 3: Rewrite the protocol and READMEs from the approved specs**

Document the 10/20/59 split, $100/$55 scope, same-model candidate rule, dynamic lifecycle, no-retry policy, dev ranking, 35,000/50,000 screen gate, 59-task two-metric confirmatory inference, descriptive all-89 row, local Docker socket, v3 receipt/preflight, and the separate helper plan. Remove every operational v6/$180/cross-model/79-task/three-dimension claim.

- [ ] **Step 4: Run the complete fresh verification gate**

```bash
make bench-gate
git diff --check
make sizes
make gate
```

Expected: both Python suites and Ruff checks pass; Rust format/size/clippy/tests pass; `git diff --check` is silent.

- [ ] **Step 5: Commit gates and docs separately**

```bash
git add Makefile .github/workflows/ci.yml AGENTS.md
git commit -s -m "ci(bench): gate Python evidence tooling"
git add bench/terminal-bench-2.1-protocol.md \
  bench/terminal_bench_analysis/README.md bench/harbor_adapter/README.md \
  bench/README.md
git commit -s -m "docs(bench): publish hybrid study workflow"
```

## Completion gate

Before starting the dependent evidence-helper plan, verify all of the following from a clean tree:

```bash
cd bench/terminal_bench_analysis
uv run --frozen --extra dev pytest -q
uv run --frozen --extra dev ruff check .
uv run --frozen --extra dev ruff format --check .
cd ../harbor_adapter
uv run --frozen --extra dev pytest -q
uv run --frozen --extra dev ruff check .
uv run --frozen --extra dev ruff format --check .
cd ../..
git diff --check
make gate
git status --short --branch
```

Expected: all commands exit zero, the tree is clean, and no `bench/evidence` production artifact, OpenRouter request, AWS resource, GitHub mutation, Harbor job, upload, or leaderboard submission has been created.
