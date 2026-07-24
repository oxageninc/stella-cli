# Stella on Terminal-Bench 2.1

Status: pre-execution frozen design, pending immutable public publication before
the first paid readiness call. It specifies exploratory calibration followed by
a mandatory, separately frozen fixed-GLM-5.1 primary run. The v6 study ends with
that primary; an eligible calibration winner is recorded only for
reproducibility unless a later, separately versioned public protocol authorizes
another study.

Protocol freeze date: 2026-07-21

## Objective

Produce a public, maintainer-audited Terminal-Bench 2.1 row for Stella and test
the preregistered claim:

> The frozen Stella configuration outperforms the public Claude Code 2.1.123 +
> GLM-5.1 max Terminal-Bench 2.1 submission by at least 10% on at least two of
> accuracy, total token spend, and valid agent-phase wall clock.

Ten percent always means relative improvement. Accuracy is also reported as a
percentage-point difference. A leaderboard screenshot proves only the public
score shown there; the comparative claim is supported by the frozen metrics,
raw trials, and disclosed comparator below.

## Immutable system under test

- Stella system-under-test commit: `fa2ec5bdae6db739628f2c37bad2ffb3ce6fe4ef`
  (release lineage `0.5.1`; `git describe` = `v0.5.1-5-gfa2ec5b`; a public
  ancestor of `origin/main`). This supersedes the original design anchor
  `ec7ee03afc187050d8334403b1893af71f65b053` (`0.4.49`). **Honest disclosure:**
  the finalized SUT is *not* a telemetry-only correction of `ec7ee03`. Between
  `ec7ee03` and `fa2ec5b`, public `main` advanced 146 commits (+111,716 /
  -23,110 across 550 files, a `0.4`→`0.5` minor bump) including feature work
  (transactional `apply_edits`, `stella arena`, adaptive-context Phase 0/1,
  graph-derived planner, syntax highlighting, HookBus resilience). The SUT is
  therefore the current public 0.5.1 Stella, frozen deliberately at `fa2ec5b` —
  the public `main` HEAD at protocol-finalization time that passes the
  telemetry-completeness gate. It is finalized here **before** any
  preregistration or paid run: the audit clock has not started, and the earlier
  `#301` protocol push was pre-publication scaffolding, not the readiness
  preregistration. The telemetry-completeness blocker demanded by the run-ledger
  amendment is verified green on `fa2ec5b` — focused usage-completeness tests
  pass (stella-store 5/5, stella-cli 3/3, stella-pipeline 8/8, including
  abort-spend retention and per-paid-call metering) and the
  `STELLA_DISABLE_REFLECTION` opt-out is present. A reference claim binary from
  the preparation build (see Binary target) is
  `sha256:9069b990088834af8cf7be17e29aca897cbd5e92b3e153dddaec60fe20b1c047`
  (`x86_64-unknown-linux-gnu`, glibc 2.17 floor, stripped, stamped
  `STELLA_BUILD_GIT_SHA=fa2ec5bdae6db739628f2c37bad2ffb3ce6fe4ef`). Because
  release builds bake in the builder's rustup/cargo source paths, that byte-exact
  SHA is host-specific and is *not* a reproducible cross-machine identity — the
  authoritative binary identity is the source-commit stamp above plus the SHA
  frozen in the run manifest for the exact binary the launcher uploads (the
  adapter verifies the uploaded binary's SHA against the host binary per trial).
- Binary target: `x86_64-unknown-linux-gnu`, built against glibc 2.17 from a
  clean checkout equal to its freshly fetched upstream. The claim build exports
  the full `git rev-parse HEAD` value as `STELLA_BUILD_GIT_SHA`; ordinary
  unstamped release builds and `scripts/dev.sh` short/dirty stamps are
  claim-ineligible. The command resolves both Cargo and rustc through `rustup`
  explicitly and uses per-build Zig global/local caches; a bare Homebrew Cargo
  sysroot is not an admissible or reliable cross-build toolchain.
- Benchmark: canonical `terminal-bench/terminal-bench-2-1`, 89 tasks. Every
  calibration and confirmatory launch must pass the single literal
  `--dataset terminal-bench/terminal-bench-2-1@sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a`;
  an unversioned dataset name is not an executable freeze.
- Harbor: `0.6.1` for calibration; the final run records the exact version
- Adapter: repository `bench/harbor_adapter`, including public ATIF-v1.7 output
- Provider endpoint: `https://openrouter.ai/api/v1`; userinfo, queries, and
  fragments are forbidden so route provenance cannot contain a credential
- Provider routing policy: `openrouter-auto`. No upstream provider is pinned,
  so results are for OpenRouter's automatic routing policy rather than a fixed
  upstream inference host; this policy is emitted per trial and frozen
- Credential boundary: Harbor is launched only through the repository's secure
  host wrapper. Its canonical bootstrap uses Python `-I -S -B` with an
  atomically created empty `-X pycache_prefix`: ambient `PYTHONPATH`, user site,
  `sitecustomize`, `.pth` startup, and adjacent unchecked bytecode are not
  executable launch inputs. Only the explicit adapter and pinned Harbor
  site-package roots enter `sys.path`. Two distinct OpenRouter credentials are
  required. The normal benchmark key is the selected provider secret: the
  wrapper writes it to a typed bundle on an anonymous unlinked seekable file
  descriptor. `OPENROUTER_MANAGEMENT_API_KEY` remains host-only and is used
  only for launch-time control-plane GETs. The wrapper removes named and
  aliased copies of both secrets from Harbor's environment and argv, then
  replaces itself with Harbor. The adapter reads only the benchmark-key bundle
  without consuming shared offset state and passes only that key to Stella over
  a second anonymous descriptor. The management key never enters the bundle,
  child environment, argv, public evidence, receipts, sidecars, or runtime
  identity. Before installing Stella, a preflight must prove
  that the exact active key is absent from every project container's complete
  Docker `Config`, including stopped containers.
  Harbor metadata and ATIF must agree on source
  `anonymous-seekable-fd-v1`, credential name `OPENROUTER_API_KEY`, bundle
  count `1`, and container-configuration absence verification `true`.
  Environment-variable credential fallback is claim-ineligible.
- Launch freshness: the secure wrapper requires exactly one explicit
  `--job-name` and `--jobs-dir`, atomically creates that previously absent job
  directory, and writes `stella-secure-launch-receipt.json` with schema
  `stella-harbor-secure-launch-receipt-v2`, the job name, model roster,
  paid-intent SHA-256, an exact `public_intent_attestation`, and launcher
  controls. Immediately before reserving the job, the launcher anonymously
  GETs the fixed public repository, dedicated owner-authored issue, exact
  owner-authored unedited intent comment, and bound ledger snapshot; waits
  until at least two seconds after GitHub's server `created_at`; and GETs the
  comment again. Nested schema `stella-harbor-public-intent-preflight-v2`
  binds the intent digest, stage, exact ledger bytes, strict
  source-to-subject-to-ledger ancestry, comment-body digest, GitHub timestamps,
  and completion times for the safety wait and final GET. It also binds the
  completed prior-stage outcome, exact runtime identity, live no-reset `$180`
  key and account-credit evidence, and a full runtime rehash after the final
  GET. The normal key authenticates `GET /api/v1/key`; the management key
  authenticates `GET /api/v1/keys/<benchmark-key-sha256>` and
  `GET /api/v1/credits`. The management key-record `hash` must equal the
  runtime benchmark-key fingerprint. The v2 evidence field `label` is retained
  unchanged but means the management-verified key-record `name`; the masked
  current-key `label` is not trusted. Before a later-stage reservation, it
  reopens every referenced prior Harbor job under the same fixed jobs root,
  recomputes each artifact-tree
  digest, and replays readiness plus the full 60-slot calibration and excluded
  ledger. A ledger assertion without matching local artifacts cannot authorize
  more spend. Those controls attest that
  filesystem settings, filesystem credentials, and project env files are
  disabled and subprocess credential scrubbing is enabled. Existing
  directories are rejected; claim jobs never resume, and the receipt remains
  in the evidence tree. The required launcher-only `--intent-sha256` is exactly
  one lowercase 64-hex digest, and `--intent-comment-url` identifies that
  public comment; both are removed before Harbor parses argv.
- Benchmark filesystem isolation: `STELLA_NO_SETTINGS=1` is a process-wide
  claim boundary, not merely a settings-file shortcut. Stella skips project,
  user, and managed `settings.json`, the user credential store, `.stella`
  memories/explorations and `context.db`/`store.db`, repository/user rules and
  skills, custom commands/agents/tools, MCP and public-registry discovery, and
  optional host-backed tool detection. It neither reads nor creates those
  persisted databases. One-shot, candidate, subsession, goal, fleet, and
  command-deck paths use the same isolated registry constructor. The
  launcher-owned engine JSON is then applied to the default settings value,
  while the already-consumed anonymous handoff is the only credential
  authority; hostile task/user files cannot alter prompts, providers, tools,
  hooks, role configuration, or startup. Normal non-benchmark behavior is
  unchanged when the gate is absent.
- Engine posture: the adapter atomically replaces merged repository/user
  `agent_engine_config` through the trusted `STELLA_ENGINE_CONFIG_JSON` seam
  after all Harbor extras. Every role inherits the exact selected model from
  `default_model`; default/worker/judge use reasoning `on` at effort `high`,
  triage uses reasoning `off` at effort `low`, all auto modes are `off`, and no
  role has a provider/model/prompt/parameter override. The posture also sets
  `headless_scope_bypass: on` (a string toggle) — a **deliberate,
  score-affecting** setting, disclosed here explicitly. A headless trial has no
  operator to approve an over-threshold plan; with this flag *off*, any plan
  exceeding the step threshold (>5 steps) self-terminates the run, which would
  make most multi-step Terminal-Bench tasks unwinnable. It is kept `on` because
  a task container is disposable and the per-trial budget cap is the real guard.
  This flag was added to `AgentEngineConfig` and to the canonical posture in
  #322, **after** the #301 protocol-scaffolding push and after the original
  engine-posture hashes were first written; the adapter docstring's claim that
  the posture "cannot drift across Stella versions" is therefore qualified — the
  field set itself grew, so the frozen hashes were recomputed for the 0.5.1 SUT
  (see the calibration manifest below). Stella parses the exact posture through
  its strict launcher seam (`config::tests::the_benchmark_engine_posture_survives_the_trusted_launcher_seam`),
  so an unknown key would fail closed rather than run misconfigured. The
  canonical JSON and its SHA-256 are emitted in Harbor context and ATIF and
  claim-gated per trial.
- Agent entry point: `/usr/local/bin/stella run` through
  `stella_harbor:StellaAgent`; the secure launcher requires exactly one
  explicit `--env docker` so its Docker receipt never relies on a Harbor
  default, and the adapter invokes the verified installation by absolute path
- Container utilities: the adapter uploads only the frozen Stella binary; it
  never provisions host `rg`, `fd`, or other convenience tools, so each trial
  retains the canonical task image's utility set
- Per-trial target budget: `$0.17`, enforced after each completed model call;
  the call that crosses the boundary can overshoot, so this is not called a
  strict billing ceiling
- Reflection setting: `STELLA_DISABLE_REFLECTION=1`. This opt-out disables only
  Stella's post-answer headless memory-reflection call. Terminal-Bench trials
  are isolated and ephemeral, so that call cannot improve a later trial; the
  setting is nevertheless frozen, emitted in every trial's metadata, and
  disclosed as part of the evaluated whole-product configuration.
- Task timeouts, verifier timeouts, CPU, RAM, storage, and task images: canonical
  dataset values with no overrides
- Host runner: dedicated native `x86_64` Linux Docker host with at least four
  effective vCPUs, a provider 32-GiB memory class with Linux `MemTotal` at
  least 31 GiB after reserved-memory accounting, and 150 GiB free on the jobs
  filesystem. No unrelated containers may run at paid-stage launch. Before
  each intent is published, include canonical report
  `bench/evidence/host-attestations/<intent_sha256>.json` with schema
  `stella-tb21-host-report-v1`, a domain-separated machine/boot/Docker-daemon
  fingerprint, the actual OS, architecture, CPU, memory, disk, Docker identity,
  and empty running-container inventory in the same immutable `ledger_commit`
  that publishes that intent. The launcher anonymously fetches the exact
  report from that commit, rejects reports older than 15 minutes,
  probes again immediately before reservation, and requires the same host and
  boot with all thresholds still passing. It writes the report bytes, public
  commit/hash, live recheck, and launch-receipt hash to mode-0600 job sidecar
  `stella-host-attestation.json` using schema
  `stella-tb21-host-launch-binding-v1`. These host controls do not replace the
  canonical per-task resource limits; zero-container launch is also not a
  substitute for reporting any later infrastructure contention.

Before primary execution, the v6 study manifest freezes the exact binary,
embedded source commit, executable adapter source-tree hash, Harbor source-tree
hash/version, analysis-file hash, endpoint, and allowed raw `step_usage` model
IDs, plus the exact engine-posture object and hash for the fixed GLM-5.1 primary
and all three calibration configurations. The calibration-selected model is a
separate field and does not replace the primary SUT. Human-readable versions
are not accepted as artifact identity. The adapter derives the source commit from the
compile-time `STELLA_BUILD_GIT_SHA` stamp and aborts if a caller-supplied commit
assertion differs.

This is a single-agent Stella evaluation. It must not be described as evidence
that Stella Fleet outperforms another multi-agent system.

The protocol, analyzer, readiness fixture, adapter, and launcher must be
committed and pushed to the public Stella branch before the readiness call.
That pushed commit and timestamp are the `readiness` preregistration. If the
sentinel exposes an instrumentation defect, its outcome stays immutable and a
public replacement `calibration` preregistration must freeze the corrected
source before calibration; when no correction is needed the readiness and
calibration commit may be identical. After calibration, the distinct public
`confirmatory_freeze` commit records the mechanically selected calibration
winner and the mandatory fixed-GLM-5.1 primary Harbor job name before any
primary reward is observed. It
binds the exact v6 manifest file, including separate SHA-256 identities for
the analyzer and its live public-timing verifier, but deliberately cannot contain the future
Harbor-generated primary job ID; that ID is appended only in the outcome.
The freeze may not change the task set, scoring code, thresholds, or engine
posture.

Every paid job also requires a public append-only intent entry committed before
launch. The entry fixes stage, job name, task/dataset identity, model roster,
attempts, concurrency, retry count, budget, source/binary identity, and its
pre-run provider-usage snapshot. Its canonical SHA-256 is copied into the
secure launch receipt. Public commit attestations are separate
`publications[]` records keyed by the immutable intent SHA-256; they are never
inserted into or used to rewrite an existing intent. After completion, a later
outcome record appends the Harbor job ID, observed provider-usage delta, and
artifact digest; prior records are never rewritten. An unregistered paid call,
missing intent, receipt mismatch,
or provider usage that cannot be reconciled makes the performance claim
ineligible. The analyzer requires the public commit URLs, 40-character commit
IDs, and publication timestamps for the calibration preregistration and the
post-calibration confirmatory freeze, and checks that their timestamps precede
the corresponding Harbor job start times. Public Git history remains the
independent timing evidence; a timestamp asserted only inside a later manifest
is insufficient. The analyzer requires separate public attestations for all
three preregistration kinds and all three nonhistorical paid intents.

Public chronology is established by the stdlib-only live verifier, never by a
manifest boolean or a replayed audit JSON. Its evidence map covers exactly six
unedited GitHub issue comments in `macanderson/stella`: the three
preregistrations and three paid intents. Each machine-readable comment body
binds study ID, subject type/ID, stage or preregistration kind, the frozen
`subject_commit`, a distinct later `ledger_commit`, ledger path, and canonical
preregistration-payload SHA-256 or intent SHA-256. All six comments are
unedited, owner-authored, and on one dedicated owner-authored preregistration
issue. The verifier ignores credentials and proves the fixed repository is
public through anonymous GETs. Its transport disables ambient proxies and CA
overrides, loads only the interpreter's compiled public trust roots, requires
TLS 1.2 or newer, refuses redirects, accepts only HTTP 200 at the exact
requested URL, caps each response at 8 MiB, and rejects duplicate JSON keys.
The verifier requires the REST `html_url` to match exactly,
`created_at == updated_at`, and the server `created_at` to equal the ledger
publication time; that server timestamp plus a conservative two-second margin
must not exceed the authoritative Harbor root-job start. It fetches commit and
compare API records plus the exact ledger, this protocol, analyzer, and live
verifier bytes. Each ledger snapshot must strictly descend from its subject,
contain the bound payload, be an exact append-only prefix/projection of the
completed ledger, and follow prior publication snapshots by ancestry. The
exact manifest bytes are fetched from the confirmatory *subject* freeze. After
all outcomes are recorded, the evidence map names a separate public
`final_ledger_commit`; it is not written into the ledger it binds, must descend
from the last publication snapshot, and must contain byte-for-byte the supplied
completed ledger. Its v3 report emits the exact body digest for every comment;
claim analysis requires every paid intent's live body digest, comment URL/ID,
server timestamp, stage, payload digest, and subject/ledger commit pair to equal
the pre-execution secure-launch receipt proof. Saved reports are review
artifacts only; claim analysis
reruns those read-only GitHub GETs in-process.

Harbor chronology uses job-level `result.json` start/finish timestamps, not the
minimum and maximum agent-execution interval. Every instantiated or error slot
must also carry top-level trial start/finish boundaries within the root job
interval, so setup, verification, and failure time remain covered. The secure
launcher forces and attests `harbor_clock_timezone: UTC`; naive Harbor
timestamps are accepted as UTC only under that receipt control. A later stage
may begin only after the prior root job has fully finished grading and its
outcome has been recorded.

## Hard budget ledger

The user-authorized all-in OpenRouter spend ceiling for this project is
`$200.00`. The provider account balance is mutable and is therefore not frozen
as a static protocol number. Immediately before every paid job, the secure
launcher must fetch `/credits` and require current available credit to cover
that job's full nominal allocation. The live provider balance is the immediate
operational ceiling even though the user's authorization may be higher.
Shared-key usage outside this study is not silently attributed to Stella.

The executable v6 study has exactly three paid stages: readiness, calibration,
and the fixed-GLM-5.1 primary. All three use one dedicated, spend-limited
OpenRouter benchmark key created after this protocol freeze by a separate
Management API key. The benchmark key creation controls are exact:
`name = "stella-tb21-dedicated-key-v1"`, `limit = 180`,
`limit_reset = null`, and `include_byok_in_limit = true`; the returned key
record must remain `disabled = false`. Its non-secret key fingerprint,
management-verified name (stored in the existing `label` evidence field),
limit, and usage snapshots are recorded; raw values for both credentials are
never published. The Management API key is
supplied only as host environment variable `OPENROUTER_MANAGEMENT_API_KEY` and
must be non-empty and distinct from `OPENROUTER_API_KEY`. The historical
excluded jobs used the prior shared key and
remain explicitly separated. If a dedicated-key baseline and final usage cannot
be obtained, the comparative claim is ineligible because completeness of the
paid-run ledger cannot be established.

The run ledger's exact top-level `historical_spend_disclosure` object is
`{"known_lower_bound_usd":0.2429614978,"unknown_cancellation_spend":true,`
`"new_authorized_budget_usd":200.0}`. Historical outcomes retain null per-job
spend because they used the old shared key; the object prevents that
unrecoverable spend from being misrepresented as zero. For new jobs,
`provider_key.usage_before_usd` means cumulative usage of the dedicated key,
not current account balance. The launcher separately fetches `/credits` and
checks current available account credit before each job as the immediate
operational ceiling.

The conservative historical bound is frozen as `H_safe=$15.00`: it exceeds the
known `$0.2429614978` plus nine canceled DeepSeek trials bounded by one completed
`$0.17` call and one maximum-context `$0.9123` in-flight call apiece. The
dedicated key therefore has one no-reset hard limit of `$180.00`, which is below
`$200.00 - H_safe` and leaves at least `$5.00` further safety under the all-in
authorization. An unknown cancellation charge is not silently treated as zero.

| Stage | Planned model spend at the nominal $0.17 target |
|---|---:|
| One synthetic readiness attempt | $0.17 |
| 60-trial calibration (3 models x 10 tasks x 2 x $0.17) | $10.20 |
| Fixed GLM-5.1 primary (89 tasks x 5 x $0.17) | $75.65 |
| **Executable v6 nominal new-call plan** | **$86.02** |

A possible selected-winner run would add up to `$75.65`, but v6 deliberately
provides no executable contract for it. It is not part of this plan and cannot
be launched by the secure launcher. A future run requires a separately
versioned public protocol, manifest, analyzer, ledger contract, and reviewed
launcher after the primary is complete.

Infrastructure retries do not authorize outcome-selected model calls. A failed
or timed-out agent trial remains in the audit results and is not selectively
rerun. Because Stella checks its budget after a paid call completes, the table
is a planning bound rather than a strict provider-billing cap. Before every job,
the live `/credits` balance is checked; no benchmark job is launched if its
nominal allocation would cross either that balance or the remaining portion of
the user's `$200.00` authorization. The primary always uses GLM-5.1; calibration
does not replace it. Before the primary, the observed model-cost projection,
continuous dedicated-key usage, conservatively reconciled prior project spend,
and then-current provider balance determine whether launch is allowed. Actual
benchmark-attributable spend is reconciled from call telemetry, while unrelated
concurrent use of the shared OpenRouter key is reported separately.

### Run ledger and post-freeze telemetry amendment

Two launch attempts failed before any trial began: the first used a short task
filter that Harbor rejected, and the second used an isolated Docker
configuration that initially omitted the Compose plugin. Both had zero model
calls and zero model spend.

The first valid infrastructure sentinel, DeepSeek V4 Pro on `fix-git`, passed
the external verifier. It then exposed that harmless startup diagnostics made
the adapter's raw stdout file unsuitable as strict JSON, although its ATIF
trajectory and Harbor metrics were complete. Its exact spend was
`$0.1237454128`. The adapter was amended to retain exact stdout separately and
write a parsed JSON envelope.

The following nine-task calibration job exposed a more serious defect: Harbor's
installed-agent wrapper converted a nonzero Stella process exit into an agent
exception before the adapter could persist the complete usage envelope or run
the external verifier. The block was stopped as soon as this instrumentation
failure was confirmed. Every completed or partial trial and all recoverable
spend remain in the audit ledger, but this interrupted block is excluded from
configuration selection because outcome and token observability depended on
the process exit code.

That interrupted nine-task job instantiated six trials and externally graded
two: one pass and one failure. Its exact recoverable spend is at least
`$0.0671561468`; two executing cancellations have unknown spend. All nine slots
remain excluded from configuration selection.

This section is a transparent post-freeze amendment, not part of the original
preregistration. The adapter now retains the actual exit code in metadata and
ATIF while allowing the canonical verifier to determine reward. The complete
ten-task DeepSeek block was restarted under the amended adapter, but a graded
nonzero-exit trial revealed that a trailing diagnostic containing `{...}` could
defeat the adapter's outermost-brace JSON fallback. That job was stopped after
one graded failure and three executing trials. Its exact recoverable spend is
at least `$0.0389689994`; the executing cancellations are unknown. A streaming
JSON decoder was then regression-tested against the exact captured 37 KB
stdout.

A second complete-block restart proved the parser and produced a schema-valid
ATIF trajectory, but exposed a Stella-core accounting defect. The graded trial
recorded 85,390 worker/witness tokens while omitting paid triage/plan token
usage, and its top-level `$0.0098216156` cost was stale relative to the final
`$0.0130899388` budget accumulator after the worker aborted. Four executing
trials were canceled immediately; their usage is unknown. No further paid run
may begin until Stella emits usage for every paid model call, retains aborted
turn spend, suppresses or meters post-turn headless reflection, passes focused
tests, and the Linux binary is rebuilt and frozen.

Every restart above was a whole-block response to a newly observed
instrumentation failure, never a task- or outcome-selected rerun. All attempted
trials and recoverable spend remain in the published audit ledger. No
configuration-selection or confirmatory 89-task analysis may use data from
before the final telemetry-complete binary and adapter are frozen.
The attributable pre-freeze spend lower bound is `$0.2429614978`; executing
cancellations with unknown usage make the true value weakly higher.
The five resulting Harbor job IDs are mandatory members of the analyzer's
excluded calibration ledger; the final ledger also includes any later
infrastructure sentinel excluded from selection. Omitting or altering that
ledger blocks the comparative claim.

### One paid readiness sentinel

The only post-hardening readiness call is one attempt on the tracked local task
`bench/readiness/synthetic-adapter-sentinel`, using
`openrouter/deepseek/deepseek-v4-pro`, in the single Harbor job named
`stella-readiness-synthetic-v1`. Its Harbor task-directory SHA-256 is
`05a040c7df0fd77f66f533ba023cb5f16e2dd0f89957440b099374210e475ad6`.
Because this is a path-only Harbor `LocalTaskId`, the ingested trial ref must be
null; identity is established by the tracked repository path plus that observed
checksum. The ledger may use the corresponding synthetic `sha256:` value as its
own dataset identity, but the analyzer does not claim Harbor emitted it as a
registry ref.
This is not a Terminal-Bench task and its
reward, cost, tokens, and timing are permanently excluded from calibration,
selection, confirmatory analysis, and marketing claims. Its purpose is limited
to witnessing provider access, tool execution, live container secret absence,
trusted engine posture, durably persisted `step_usage`, ATIF generation, and
external verification. Execution stops after that single attempt regardless of
reward. Calibration may begin only if the sentinel completes without an agent
exception, emits terminal `complete` with return code zero, and receives
canonical external-verifier reward exactly `1.0`; this is an infrastructure
admission gate, never a benchmark score. A failure can justify a publicly
committed instrumentation correction,
but cannot change benchmark tasks, thresholds, or the selection rule.

Timeout survival itself is established separately by the deterministic
`completed_provider_usage_precedes_a_hung_speculation_pump` cancellation test,
which cancels a turn after provider completion while a speculative tool remains
hung and requires exactly one recoverable `step_usage` record.

## Calibration manifest

Task selection is stratified by Terminal-Bench difficulty with seed `20260721`:
one easy, six medium, and three hard tasks. One
`sampling_rng = random.Random(seed)` instance was reused, in fixed
easy -> medium -> hard stratum order, to sample each lexicographically sorted
task-name list before any model was run.

| Difficulty | Task | Canonical Harbor package ref |
|---|---|---|
| easy | `fix-git` | `sha256:16948b980df9d96de616a205f5acca1c5d395de83ff4f8ffabcafacb93226f2e` |
| medium | `filter-js-from-html` | `sha256:2d1496b6fc62adeccdba7a56f4bc24e5ef265840434d2011234ed20b6c240759` |
| medium | `kv-store-grpc` | `sha256:973c5d4c111fb61a344457936f1c36400acd2d9e44389e7b319586fe23a7a307` |
| medium | `large-scale-text-editing` | `sha256:1f1cddc3df15e452fe2d3c6928f6b1e5b5330a7ae67cab373a0d089ea7d334a2` |
| medium | `regex-log` | `sha256:802c16cfd132e6c457529cb864be5a757c1b23b6cadc57f2d01983cb0110292a` |
| medium | `schemelike-metacircular-eval` | `sha256:58130c2166c3115276dc8592f358e326ff2d81ea852e3d88636c82fd1dff57e6` |
| medium | `sqlite-with-gcov` | `sha256:9f9bd57fbf9f4831e9031755e83aea6b9d60d2b2d54e8a12d48cff4dca3c231d` |
| hard | `bn-fit-modify` | `sha256:b5f9644970c17ad9ddb46b7266f7bcd87c761d77d7e6f55d7cfe7284d5ff66e9` |
| hard | `make-mips-interpreter` | `sha256:41a55da0abec5d7b32a0c2321f8b18e84000ca8074ae62c6874d6ed4a3a1cd3c` |
| hard | `train-fasttext` | `sha256:460fc0818971ec83545a76805267b65459128fad52e68c26a199a0d74022badb` |

The three candidate model configurations, all through the configured OpenRouter
account, are:

1. `openrouter/deepseek/deepseek-v4-pro`
2. `openrouter/z-ai/glm-5.2`
3. `openrouter/x-ai/grok-4.5`

Every model runs exactly twice on every pilot task. All 60 slots run in one
Harbor job named `stella-tb21-calibration-20260721`, with the three agents
listed below, `n_concurrent=3`, and `retry.max_retries=0`. Harbor
materializes slots with attempt outermost, then task, then agent; consequently
each model triplet for a given task/attempt becomes concurrently eligible and
reduces provider-time confounding. A separate fresh
`shuffle_rng = random.Random(seed)` instance shuffled the agent list before
execution; it did not reuse the sampling RNG's advanced state:

1. `openrouter/deepseek/deepseek-v4-pro`
2. `openrouter/z-ai/glm-5.2`
3. `openrouter/x-ai/grok-4.5`

The distinct raw per-call model IDs are frozen, respectively, as
`deepseek/deepseek-v4-pro`, `z-ai/glm-5.2`, and `x-ai/grok-4.5`. The analyzer
checks every `step_usage` record against the explicit mapping; it does not infer
the mapping from the final envelope model.

The registered engine-posture SHA-256 values (canonical JSON with sorted keys
and no insignificant whitespace), recomputed for the 0.5.1 SUT posture that
includes `headless_scope_bypass: on` (see Engine posture above), are:

| Configuration model | Engine-posture SHA-256 |
|---|---|
| `openrouter/deepseek/deepseek-v4-pro` | `1740fa2f3f1bea66c348c7ffca151f526019ef0278829d23acb391e7b2f07159` |
| `openrouter/z-ai/glm-5.2` | `9b94f231d91e66c9793e2f61dd8c6edbb4472ea38e431681b5e854d9d22191ea` |
| `openrouter/x-ai/grok-4.5` | `3c7d61553b7a4665ed974e6b32a7a20c1f8c59acaae2bcab3848eec2a39ca8dc` |

Each posture differs only in the inherited selected model and its one-entry
`allowed_models` list. A repository setting or Harbor extra that attempts to
change a role model, effort, reasoning, prompt, or request parameter is
overwritten before Stella starts; a malformed trusted JSON value makes Stella
fail closed without echoing its contents.

Calibration is for configuration selection, not a confirmatory performance
claim.

### Configuration-selection rule

Rank configurations by:

1. verifier passes out of 20;
2. lower projected 445-trial USD cost from observed mean calibration cost if
   pass counts tie;
3. earlier position in the frozen roster above if projected cost also ties.

Calibration tokens and wall time remain descriptive diagnostics. They are not
selection tie-breakers because raw token counts are not comparable across
tokenizer families and wall time is not claim-eligible on this runner.

A configuration may advance only when:

- it earns at least 14 passes in its 20 calibration trials;
- no successful trajectory is missing or invalid;
- all manager/model calls appear in token totals; and
- projected five-attempt spend from observed mean cost is no more than `$75`.

If none advances, publish the calibration miss. The fixed GLM-5.1 primary still proceeds
when its own telemetry, public-freeze, and budget gates pass; configuration
selection does not determine the primary model. Any later calibration is a
separately preregistered study whose observations cannot be pooled into or
substituted for these 60 slots.

The claim analyzer ingests all 60 frozen selection slots from the single
registered calibration job, plus the excluded-run ledger. It validates the
canonical dataset reference, task checksums, adapter import path, default
resources/timeouts, exact concurrency, zero retries, rewards in `[0,1]`, and
raw-call accounting; then it
recomputes pass count and projected-cost ranking, applies the frozen roster-order
tie-break, and mechanically requires `calibration.selected_model` to be the
eligible winner. The selected configuration is recorded for reproducibility;
v6 provides no executable follow-up contract. Selection does not change the
fixed same-model primary. Tokens and wall time are reported but do not
participate in selection. That primary is
a non-causal whole-agent product-configuration comparison because its public
Claude Code comparator is historical rather than concurrently randomized.

## Fixed confirmatory public run and stop rule

After calibration, freeze the binary, source commit, adapter, provider route,
budget, benchmark ref, resources, and analysis code before any confirmatory
reward is observed. The mandatory primary claim job uses
`openrouter/z-ai/glm-5.1`, matching the Claude Code comparator's GLM-5.1 model,
and launches all 89 tasks and five attempts per task (`445` trials),
`n_concurrent=1`, and no Harbor retries. Serial execution avoids cross-trial
host contention on the dedicated native-x86 runner; wall clock remains
ineligible because the Claude Code comparator is historical and was not run
contemporaneously on the same host.
The decision to run all five attempts is unconditional on intermediate
accuracy, token, cost-per-task, or wall-clock results; the study does not inspect
a favorable first attempt and then decide whether to continue.

The job may stop only for a non-performance operational failure such as invalid
or missing telemetry, credential compromise, unavailable infrastructure, or
insufficient provider balance. Any such stop makes the confirmatory study
incomplete and establishes no performance claim. Claim analysis accepts exactly
one logical and physical Harbor job with 445 requested, instantiated, and
attempted unique trial IDs covering each frozen task-attempt slot once. It
rejects resumes, multiple input job directories, stitched jobs, replacement
trials, duplicate IDs, and completed-slot reruns. A new attempt at the study
requires a new public preregistration and must retain the failed job in the run
ledger; it cannot be presented as continuation of this confirmatory run.
The secure launcher atomically creates a one-time job directory and launch
receipt and refuses an existing directory, so Harbor's same-directory resume
path cannot produce claim-eligible provenance. The analyzer also requires
`retry.max_retries=0` and validates that receipt.

The v6 study stops after the mandatory GLM-5.1 primary and its immutable
outcome. Its launcher rejects a selected-winner run. A future descriptive study
would require a new public protocol, manifest, analyzer, ledger contract, and
reviewed launcher; it could not be substituted into the same-model
accuracy/token claim or contribute a token win against Claude Code because
different model and tokenizer families confound token counts.

Only after all 445 trials and trajectories are frozen does the analysis apply
the registered `64.52%` point threshold, `358,905,384` integer token threshold,
and confidence procedure below. No outcome-driven task reruns, exclusions,
prompt changes, model changes, or timeout/resource changes are allowed.

## Comparator and thresholds

The public comparator is the maintainer-audited Terminal-Bench 2.1 Claude Code
submission using GLM-5.1 at max effort (PR #67). The leaderboard-owned public
Harbor job is `fd8707bb-51e8-56fa-8e46-769a82a531ae`; the submitter's original
mixed job is `ea5f7281-c2c2-4a1f-afb5-db89db24fdad`. The immutable submission
record is pinned to repository commit
`327a5a0b2ee4675871dc57e1d53fff2d2cf974e1` at
`leaderboard/submissions/2026-05-01-glm-5-1-max-claude-code.json`:

- public comparator manifest SHA-256:
  `7963a7af2b306fd4b6e82963fdadf9374e701ec16f47b194300e2843c8002a76`
- normalized 445-trial analysis data SHA-256:
  `f7b916c7d3028c62003bb12eeb1fff3df0bb41a82ce21ba6e59b3a1b50139a99`
  (including each task's one consistent Harbor package ref and directory
  checksum across all five trials)
- canonical 89-task ref/checksum set SHA-256:
  `7e495afe0a86eaf572be1c2da2b9929c24e502adc888e550385d915cc0125ece`
- immutable submission JSON SHA-256:
  `36d20c181be246dc55965bf4320a3005f292c737f31511bbde19ba1808a2bd2c`

- accuracy: `58.65%`
- tokens: `398,783,761`
- cost: `$277.14`
- trials: `445`

The preregistered 10% relative thresholds are therefore:

- accuracy: Stella `>= 64.5168539%`; for binary rewards the first attainable
  passing score is `288/445 = 64.7191%` (reported as `64.72%`), and
- total tokens: Stella `<= 358,905,384`.

When Stella uses GLM-5.1, this is a same-model, different-provider-route
comparison and the route is disclosed. When another model wins calibration, it
is explicitly a whole-product configuration comparison, not a causal estimate
of the Stella scaffold alone.

The existing public Claude result is not contemporaneous. It is suitable for a
transparent public-leaderboard product comparison, but not for claiming that
all difference is caused by the CLI. A later matched study must run both agents
contemporaneously with the same model endpoint and account tier.

Model selection observed ten Terminal-Bench tasks, so those tasks are not used
as independent confirmation. The full 89-task/445-trial aggregates still
determine the public leaderboard score and the registered point thresholds.
For confidence bounds, exclude the ten calibration tasks and resample only the
remaining 79 untouched tasks as clusters with 50,000 fixed-seed bootstrap
draws, retaining all five trials from each product within each sampled task.
Compute one-sided 98.33% lower confidence bounds for the favorable relative
accuracy, token, and eligible wall-clock effects (Bonferroni family-wise 5%
control across three registered dimensions). A dimension wins only when its
full-89 point improvement is at least `0.10` and its selection-adjusted 79-task
lower bound is greater than `0.10`; the launch claim requires two wins. In claim
mode, the analyzer refuses any bootstrap seed other than `20260721`, any draw
count other than `50000`, a non-79-task inferential set, or wall-clock
eligibility for this historical comparator. If full-89 point estimates clear
10% but confidence bounds do not, report an observed advantage, not an
established >=10% advantage.

## Metric definitions

- **Accuracy:** mean canonical external verifier reward over every attempted
  trial. If an errored or timed-out agent still leaves work that the canonical
  verifier grades, that reward is retained exactly as on the official
  leaderboard; only trials with no verifier reward receive zero.
- **Token spend:** Harbor total prompt tokens (which already include cache hits)
  plus completion tokens, summed over every Stella model call. Cached prompt
  tokens are reported as a subset and are not added a second time. Missing
  usage is never treated as zero.

Token eligibility is fail-closed per trial: the durable stream must have a
recognized terminal event; accounting must be complete; Harbor prompt,
completion, cache, and cost totals must equal raw `step_usage` totals; every
call must report one of the frozen raw model IDs; and any independent terminal
cost must reconcile. An interrupted stream or canceled request with unknown
usage blocks the token claim rather than turning its observed prefix into an
exact total.
- **Wall clock:** Harbor's external agent-execution interval from instruction
  handoff until Stella exits. It includes model latency, tools, retries, and
  orchestration; it excludes environment build/install and verifier execution.

Wall clock counts toward the two-of-three claim only for a matched,
contemporaneous comparison with identical sandbox resources, concurrency,
timeout boundaries, and complete timing coverage. For this public-row study it
is descriptive. The headline decision therefore requires both accuracy and
token thresholds to pass.

USD cost is reported but is not substituted post hoc for the token endpoint.

## Audit and publication

Publish all of the following, including failures:

- Harbor job IDs and public trial links;
- ATIF-v1.7 trajectories for every successful trial;
- Stella commit, binary SHA-256, adapter diff, model IDs, and per-task budget;
- task names/digests, rewards, token categories, cost, timing, and errors;
- calibration job, mandatory primary job, deviations, and budget ledger;
- any future separately versioned post-primary study as a distinct evidence
  package outside the primary analyzer snapshot;
- the analysis script and machine-readable result table.

Before any job, trajectory, log, or derived artifact is made public, run the
repository's artifact secret scanner against the complete publication tree.
Any exact active credential, encoded credential variant, or recognized token
pattern blocks publication until the artifact is regenerated and the scan is
clean. A clean secret scan and strict ATIF validation are mandatory publication
gates, not advisory warnings.

As a separate pre-execution control, the pinned 89-task dataset tree is scanned
across every `Dockerfile`, Compose YAML, and `task.toml` for Stella controls,
provider credential names, proxy variables, `BASH_ENV`, and `LD_PRELOAD`. The
frozen local tree produced zero matches on 2026-07-21; the same zero-match scan
must be repeated after final cache resolution before the primary job.

Every leaderboard submission uses all 89 tasks with at least five attempts,
default resources/timeouts, public Harbor visibility, and the official
Terminal-Bench maintainer trajectory review. Any reward-hacking or harness-
cheating disqualification remains a zero with its cost and tokens counted.

## Fleet evidence boundary

After the single-agent public row, Stella Fleet needs a separate capacity-
controlled study against Claude Code Agent Teams and a fleet-specific public
benchmark such as CooperBench. Match the number of concurrent agents, aggregate
token budget, model roster, sandbox resources, tools, and permission policy;
sum tokens across every worker and measure fleet makespan. Until that study
exists, use the Terminal-Bench result only for Stella's coding-agent claim.
