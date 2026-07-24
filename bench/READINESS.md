# Terminal-Bench 2.1 — Readiness Report

Prepared 2026-07-23 as the offline preparation for the maintainer-audited public
Stella row described in [`terminal-bench-2.1-protocol.md`](terminal-bench-2.1-protocol.md).

**Status: submission-ready pending maintainer sign-off + the paid run.** Every
*offline, reversible* preparation gate is green. The three *irreversible* steps
(create the spend-limited key, publish the preregistration, launch paid Harbor
jobs) are deliberately **not** done here — they are the maintainer's to execute.
"Official" additionally requires an external Terminal-Bench maintainer trajectory
review, which is outside this repository's control; this report makes the run
*submission-ready*, not "already official."

---

## 1. Frozen system under test (SUT)

| Field | Value |
|---|---|
| SUT commit | `fa2ec5bdae6db739628f2c37bad2ffb3ce6fe4ef` |
| `git describe` | `v0.5.1-5-gfa2ec5b` (release lineage **0.5.1**) |
| Public? | Yes — ancestor of `origin/main` (verified `git merge-base --is-ancestor`) |
| Original design anchor | `ec7ee03…` (0.4.49) — **superseded** |
| Frozen claim binary | `target/x86_64-unknown-linux-gnu/release/stella` |
| Binary format | ELF 64-bit x86-64, glibc 2.17 floor (`…-gnu.2.17`), stripped, 27,754,176 bytes |
| **Binary SHA-256** | `9069b990088834af8cf7be17e29aca897cbd5e92b3e153dddaec60fe20b1c047` (reference — *host-specific*, see note) |
| Build stamp | `STELLA_BUILD_GIT_SHA=fa2ec5bdae6db739628f2c37bad2ffb3ce6fe4ef` |

> **Reproducibility:** release builds bake in the builder's rustup/cargo source
> paths (under `/Users/macanderson/…` here), so the byte-exact SHA above is
> host-specific and will differ on another machine. It is a *reference* proving
> the toolchain works and the stamp is correct — the authoritative binary
> identity is the source-commit stamp plus the SHA the run manifest freezes for
> the exact uploaded binary (the adapter re-verifies the upload SHA per trial).
| Toolchain | rustc/cargo 1.97.0 via `rustup which`; zig 0.16.0 + `cargo-zigbuild`; per-build Zig caches |

### Freeze decision (maintainer-approved)

The protocol was originally frozen against `ec7ee03` (0.4.49) with re-freeze
allowed only on a *telemetry-only-corrected* commit. Public `main` has since
advanced to **0.5.1** — 146 commits, +111,716/-23,110 across 550 files, a minor
bump that is **not** telemetry-only (it adds `apply_edits`, `stella arena`,
adaptive-context Phase 0/1, graph-derived planner, etc.). Because **no
preregistration has started** (no dedicated preregistration issue, no
`bench/evidence/`, no paid run — the earlier `#301` push was pre-publication
scaffolding), pinning the SUT to the current public release is *design
finalization before the audit clock starts*, not tampering. The maintainer chose
to finalize the SUT to the current public 0.5.1 commit `fa2ec5b`. The protocol's
"Immutable system under test" section has been updated to disclose this honestly.

> **Run-time note:** the SUT binary is stamped with, and must be run with,
> `STELLA_SOURCE_COMMIT=fa2ec5bdae6db739628f2c37bad2ffb3ce6fe4ef`. The
> preregistration-reconciliation commit (this PR) is a *descendant* that records
> the SUT; do **not** stamp/verify the binary against the PR commit SHA.

---

## 2. Blocker clearance (run-ledger amendment)

The protocol amendment forbids any further paid run until Stella "emits usage for
every paid model call, retains aborted turn spend, suppresses or meters post-turn
headless reflection, passes focused tests, and the Linux binary is rebuilt and
frozen." Verified on `fa2ec5b`:

| Criterion | Evidence |
|---|---|
| Usage emitted per paid call; abort-spend retained | Focused tests green: `stella-store` usage_completeness 5/5, `stella-cli` usage_completeness 3/3, `stella-pipeline` usage 8/8 — incl. `triage_success_emits_usage_before_budget_abort` and `aborted_pipeline_totals_match_every_management_and_execute_usage_record` |
| Fail-closed accounting | `AgentEvent::UsageIncomplete`; store migration v8→v9 "fail-closed paid-call accounting" (`usage_complete` column) |
| Reflection suppression | Behavior-verified at the gate: `agent::tests::explicit_reflection_opt_out_suppresses_every_one_shot_format` sets `STELLA_DISABLE_REFLECTION` and asserts `one_shot_reflection_enabled()` is `false` for Text/Json/StreamJson (2/2 green, incl. truthy-value parsing). End-to-end "zero post-answer model call" is first confirmed behaviorally by the paid readiness sentinel — this offline gate proves the decision, not the full round trip. |
| Linux binary rebuilt & frozen | See §1 (SHA `9069b990…`) |

---

## 3. Offline audit gates

| Gate | Result |
|---|---|
| Focused telemetry/usage-completeness tests | ✅ store 5/5, cli 3/3, pipeline 8/8 |
| CLI-contract smoke test (`bench/smoke/smoke_test.py`) | ✅ 5/5; `stella --version` = `stella 0.5.1` |
| Analyzer self-tests (`terminal_bench_analysis/tests`) | ✅ 234 passed |
| Harbor adapter self-tests (`harbor_adapter/tests`) | ✅ 226 passed (after the two fixes in §4; was 224/2) |
| Engine-posture parses through Stella's strict seam | ✅ `config::tests::the_benchmark_engine_posture_survives_the_trusted_launcher_seam` — proves `headless_scope_bypass:"on"` is accepted, not fail-closed |
| Readiness-fixture integrity (`synthetic-adapter-sentinel`) | ✅ no drift since the `#301` freeze (only commit touching it) → still hashes to pinned `05a040c7…`; value pinned in `test_secure_launcher.py` |
| Secret scan over git-tracked publication source | ✅ 1 finding = the scanner's *own synthetic test fixture* (`test_artifact_secret_scan.py`), by design; no real credential. (`.venv` hits are third-party dependency data, not publication content.) |
| Cross-build toolchain reachable from macOS host | ✅ produced the frozen ELF (§1) |

---

## 4. Reconciliation performed (SUT-drift consequences)

Freezing on 0.5.1 surfaced post-freeze drift in the frozen preregistration
artifacts. All reconciled in this PR; the complete coupled surface was verified
(the only SUT-derived sha256 literals in the protocol are the 3 posture hashes —
every other digest is an external dataset/comparator/fixture value, unchanged).

1. **Engine-posture SHA-256 recomputed for 0.5.1.** PR #322 added
   `headless_scope_bypass: "on"` to the canonical posture *after* the #301
   freeze, changing every posture hash. Recomputed via the adapter's own
   `_benchmark_engine_posture` (the same function `secure_launcher.py` uses to
   emit the manifest hashes, so hand values cannot diverge from the machine
   manifest):

   | model | frozen (stale) | recomputed 0.5.1 |
   |---|---|---|
   | deepseek-v4-pro | `fb18233a…` | `1740fa2f3f1bea66c348c7ffca151f526019ef0278829d23acb391e7b2f07159` |
   | z-ai/glm-5.2 | `de2a3109…` | `9b94f231d91e66c9793e2f61dd8c6edbb4472ea38e431681b5e854d9d22191ea` |
   | x-ai/grok-4.5 | `f43d8a25…` | `3c7d61553b7a4665ed974e6b32a7a20c1f8c59acaae2bcab3848eec2a39ca8dc` |
   | z-ai/glm-5.1 (primary) | — (manifest-generated at freeze) | `55fdf3421ae4c8625ab8bdedb11a59867b6d81a20ad378a2080e7e944229f4bd` |

   Updated in: protocol calibration table, protocol posture prose,
   `terminal_bench_analysis/README.md` example, and the adapter test assertion.

2. **Stale adapter test fixtures fixed.** `test_hashes_exact_uploaded_binary_and_records_source_commit`
   used an `_Environment` stub lacking `task_env_config`; `install()` →
   `_build_code_graph()` legitimately reads `task_env_config.workdir` to run
   `stella init` (code-graph indexing added after the fixture was written). Added
   the attribute. The posture assertion was updated to the recomputed hash.

> ⚠️ **CI gap discovered:** stella's CI is Rust-only (`fmt + clippy + test`), so
> the Python adapter/analyzer suites are **not CI-gated** — which is how the
> adapter suite drifted red unnoticed. Recommend adding these `pytest` suites to
> CI before or alongside publication.

---

## 5. ⚠️ MAINTAINER SIGN-OFF REQUIRED — `headless_scope_bypass: "on"`

This is a **score-determining** posture setting, not hash bookkeeping. With it
**off**, a headless trial with no operator to approve an over-threshold plan
self-terminates any plan exceeding the step threshold (>5 steps) — most
multi-step Terminal-Bench tasks would be unwinnable. #322 set it **on** (after
the #301 freeze); that is the *entire* reason the posture hashes moved. It is
kept **on** (defensible — a disposable container with a per-trial budget cap as
the real guard) and is now disclosed in the protocol's posture prose. **The
value is defensible; the disclosure + your explicit sign-off is what's
mandatory** — an undisclosed score-affecting posture change is exactly what a
maintainer trajectory review fails.

---

## 6. Remaining steps — HUMAN-ONLY, in order

None of these were performed here; each is irreversible and/or spends real money
against the `$200` all-in OpenRouter authorization.

1. **Create the dedicated benchmark key** — via the Management API key:
   `name="stella-tb21-dedicated-key-v1"`, `limit=180`, `limit_reset=null`,
   `include_byok_in_limit=true`, `disabled=false`. Record fingerprint, verified
   name, and usage snapshots (never the raw value).
2. **Host attestation** — on a dedicated native x86_64 Linux Docker host meeting
   the thresholds (≥4 vCPU, ≥31 GiB `MemTotal`, ≥150 GiB free, zero unrelated
   containers). Commit `bench/evidence/host-attestations/<intent_sha256>.json`
   (`stella-tb21-host-report-v1`) in the intent's ledger commit.
3. **Publish the preregistration** — the dedicated owner-authored preregistration
   GitHub issue with its six unedited machine-readable comments (3
   preregistrations + 3 paid intents) and the append-only ledger snapshots. The
   single deliberate push of protocol + analyzer + readiness fixture + adapter +
   launcher **is** the readiness preregistration; get everything correct locally
   first so it is one push (a corrected re-push muddies the timestamp).
4. **Pre-launch scans on the resolved tree** — repeat the dataset scan
   (Dockerfiles/Compose/`task.toml` for Stella controls, cred names, `BASH_ENV`,
   `LD_PRELOAD`) after final cache resolution, and run `artifact_secret_scan.py`
   over the complete publication tree **with `--require-env`** (live key present).
5. **Readiness sentinel** — one attempt, `openrouter/deepseek/deepseek-v4-pro`,
   job `stella-readiness-synthetic-v1` (~$0.17). Gate to proceed: no agent
   exception, terminal `complete`, return code 0, external-verifier reward
   exactly `1.0`.
6. **Calibration** — 60 trials, job `stella-tb21-calibration-20260721` (~$10.20);
   apply the frozen selection rule.
7. **Primary (GLM-5.1)** — 445 trials, `n_concurrent=1`, `retry.max_retries=0`
   (~$75.65). Then apply the registered `64.72%` / `358,905,384`-token thresholds
   and the 79-task bootstrap confidence procedure.
8. **External review** — submit for the Terminal-Bench maintainer trajectory
   review (not in this repo's control).

Before each paid job the secure launcher fetches `/credits` and refuses launch if
the nominal allocation would cross the live balance or the remaining `$200`
authorization.

---

## 7. Artifact index

- Frozen binary: `target/x86_64-unknown-linux-gnu/release/stella` — sha256 `9069b990…` (host-specific reference, see §1 note). This preparation build relaxed the `#install` provenance guard from `==origin/main tip` to *ancestor-of-public-ref* so it could target the specific already-public release commit `fa2ec5b` rather than the moving tip. **The maintainer's actual paid claim build must use the stock, unmodified `#install` procedure (the `==@{upstream}` guard) against the final preregistration commit** — which will be the `origin/main` tip at that point, so the stock guard passes unchanged. Do not copy the relaxed guard into the paid run.
- Reconciled: `bench/terminal-bench-2.1-protocol.md`, `bench/terminal_bench_analysis/README.md`, `bench/harbor_adapter/tests/test_adapter.py`.
