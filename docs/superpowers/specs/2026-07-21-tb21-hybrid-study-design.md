# Terminal-Bench 2.1 hybrid study design

Status: approved for implementation

Date: 2026-07-21

## Decision

Run a three-part Terminal-Bench 2.1 program:

1. iterative tuning on ten disclosed development tasks;
2. one sealed go/no-go screen on twenty different tasks;
3. only after the screen passes and the owner separately authorizes it, one
   official 445-trial run whose primary scientific analysis uses the remaining
   fifty-nine untouched tasks.

The current OpenRouter authorization is capped at $100 and does not authorize
the official 445-trial run. The previously approved AWS infrastructure cap is
$55 and is separate from the OpenRouter cap. A later statement that funds have
been added does not itself authorize confirmatory execution; the owner must
explicitly authorize the frozen confirmatory intent after reviewing the screen.

This design replaces the unexecuted v6 plan that assumed an immediate $200
study and a $180 provider key. No v6 paid readiness, calibration, or primary
call was made, so the v6 artifacts may be superseded rather than mixed with the
new study.

The implementation introduces new identifiers rather than changing the meaning
of existing v6 bytes:

- study: `stella-tb21-hybrid-study-v1`;
- task partition: `stella-tb21-task-partition-v1`;
- run ledger: `stella-tb21-run-ledger-v3`;
- study manifest: `stella-tb21-study-manifest-v7`;
- GitHub comment body: `stella-tb21-github-attestation-v3`; and
- secure launch receipt/public preflight: version 3.

The v3 ledger has exact top-level fields for schema/study identity, fixed
paths, task-partition digest, budget authorizations, prior-exploration
disclosure, preregistrations, candidates, intents, publications, and outcomes.
All append-only records share one globally unique positive sequence space.

## Objective and claim boundary

The target is a public Terminal-Bench 2.1 leaderboard row and defensible
evidence that Stella outperforms the public Claude Code + GLM-5.1 comparator by
at least 10% on both eligible dimensions:

- verifier accuracy; and
- token spend.

Historical wall-clock data is ineligible because it was not measured on the
same hardware, provider route, load window, and orchestration boundary. Wall
clock remains descriptive unless a separately preregistered same-host Claude
Code run is completed.

The official leaderboard result uses all 89 tasks and is a secondary
descriptive result. The primary inferential claim uses only the 59 tasks that
were never run or used for selection before the official run. Their canonical
identities and metadata are necessarily published by the deterministic split;
their instructions, trajectories, verifier outcomes, and other task contents
must not be inspected before the official run.

The study can support a same-model claim when Stella's confirmatory model is
GLM-5.1. Therefore every paid candidate, the screen, and the official run use
the exact `openrouter/z-ai/glm-5.1` model; model selection is not a tuning
variable in this study. It does not, by itself, establish general multi-agent fleet
superiority. If the frozen winning Stella configuration uses a fleet, the
result may accurately describe that tested fleet configuration but must not be
generalized to all fleet workloads without a separate fleet study.

## Benchmark and comparator

- Dataset: `terminal-bench/terminal-bench-2-1` at the repository's pinned
  content digest.
- Official design: 89 tasks, 5 attempts per task, 445 trials.
- Comparator job: `fd8707bb-51e8-56fa-8e46-769a82a531ae`.
- Comparator agent/model: Claude Code 2.1.123 + GLM-5.1, effort `max`.
- Comparator totals: 261 verifier passes and 398,783,761 tokens across 445
  trials.

The comparator's immutable manifest, public trial data, leaderboard
submission, and task-level rows remain pinned and hash-verified as in the
existing analyzer. All subset comparisons select rows by exact task identity
from those same immutable bytes.

## Deterministic task partition

The ten development tasks remain the already disclosed v6 calibration set:

1. `fix-git`
2. `filter-js-from-html`
3. `kv-store-grpc`
4. `large-scale-text-editing`
5. `regex-log`
6. `schemelike-metacircular-eval`
7. `sqlite-with-gcov`
8. `bn-fit-modify`
9. `make-mips-interpreter`
10. `train-fasttext`

The other 79 canonical task identities are partitioned without outcome-based
choice. For each exact canonical task reference, calculate:

```text
sha256("stella-tb21-hybrid-study-v1\0" || canonical_task_reference)
```

Sort ascending by digest, then by canonical task reference as a deterministic
tie-breaker. The first 20 become the sealed screen; the remaining 59 become the
untouched confirmatory subset. The implementation publishes the exact three
lists, canonical task-reference bytes, per-split SHA-256 digests, and one
whole-partition digest before the first paid call.

During tuning, the runner and analysis commands reject screen or untouched
task identities. Humans and agents may inspect development instructions and
trajectories. They must not inspect screen or untouched instructions before
their authorized stage. The screen winner sees screen instructions only during
the sealed screen. The 59-task subset remains unobserved until the official
run.

## Current $100 phase

The dedicated OpenRouter runtime key is:

- named `stella-tb21-tuning-key-v1`;
- limited to exactly $100;
- configured with no automatic reset;
- configured so BYOK usage counts toward the limit; and
- initially at zero usage.

The management key is distinct and cannot invoke models. Both credentials stay
outside argv, tracked files, comments, receipts, trajectories, and public
artifacts.

The planned allocation is:

| Purpose | Maximum provider spend |
|---|---:|
| Paid readiness and boundary checks | $1 |
| Development-set successive halving | $54 |
| Sealed 20-task screen | $30 |
| Uncommitted safety reserve | $15 |
| **Hard total** | **$100** |

The reserve cannot be used for another screen, an official run, or additional
behavioral tuning without a public amendment and owner approval. Account-level
credits do not expand this study authorization. The study stops before any API
request if key usage plus the maximum requested stage spend can exceed the
hard cap or the relevant allocation.

AWS infrastructure spend is separately capped at $55. Provisioning must use a
short-lived, MFA-backed, non-root AWS role. The currently authenticated root
identity is not an acceptable mutation identity. The tuning runner is an
On-Demand `m7i.2xlarge` native x86_64 Linux host with one encrypted 250 GiB gp3
volume at baseline performance, at least 31 GiB observed RAM, 150 GiB free on
the jobs filesystem, no instance profile, IMDS disabled, and zero running
containers at each launch. Spot is ineligible because an interruption would
invalidate a non-resumable stage.

Only one tagged runner, one attached volume, and one in-use public IPv4 address
may exist for this phase. NAT gateways, Elastic IP reservations, snapshots,
extra volumes, and paid support services are prohibited. Before provisioning,
the control plane recomputes the worst-case instance, gp3, IPv4, scheduler, and
outbound-transfer charge from live AWS prices through a fail-closed lifetime of
at most 115 hours. The transfer term assumes up to 100 GiB of evidence export;
the export command meters bytes and cannot exceed that allowance. Creation is
refused unless the complete projection through automatic cleanup is at most
$52, preserving at least $3 for price rounding or cleanup latency. Both an
instance-side poweroff timer and an independent control-plane stop watchdog use
the frozen compute deadline.

At completion or deadline, whichever comes first, the operator stops the
runner, exports two hash-verified evidence copies, terminates the exact tagged
instance, deletes the retained tagged volume and temporary network/key
resources, and verifies that no chargeable resource with the run ID remains.
A separate least-privileged control-plane cleanup schedule, verified before the
instance starts and carrying no benchmark credential, sets an absolute cleanup
deadline inside the $52 projection. At that deadline it terminates the exact
tagged instance and deletes its retained tagged volume even if an operator is
absent or evidence export is incomplete. Any best-effort incomplete artifact
export happens before deletion; the hard $55 cap takes precedence over retaining
an invalid partial job. The scheduler and its temporary IAM role are removed by
the same cleanup workflow. This runner and authorization do not cover the later
confirmatory run.

## Tuning protocol

Tuning is explicitly exploratory. Development-set results are not used in the
primary confirmatory inference.

Every candidate is identified before its paid round by:

- source commit and stamped binary digest;
- adapter, analyzer, Harbor, and evidence-contract digests;
- exact `openrouter/z-ai/glm-5.1` model and provider-route policy;
- direct, pipeline, or fleet topology;
- role model, effort, reasoning, and concurrency posture;
- prompt/configuration digest;
- per-trial token/cost budget, no greater than $0.30; and
- exact development task list, attempts, retries, and job name.

Candidate creation may use development trajectories and failures. Any Stella
behavior change requires a witness test that fails on the prior source and
passes on the candidate. Each paid round freezes all entrants before its first
call; candidates cannot be edited mid-round.

Successive halving uses these maximum shapes:

| Round | Entrants | Attempts per development task | Maximum trials | Maximum spend |
|---|---:|---:|---:|---:|
| 1 | 8 | 1 | 80 | $24 |
| 2 | 4 | 1 | 40 | $12 |
| 3 | 2 | 3 | 60 | $18 |
| **Total** |  |  | **180** | **$54** |

Fewer candidates or cheaper per-trial budgets leave funds unspent; they do not
authorize extra rounds. Infrastructure failures do not silently become
replacement trials. A public amendment must name any invalid job and preserve
its artifacts before a replacement intent is created.

Round ranking is deterministic:

1. complete canonical round with no failed or accounting-incomplete trial;
2. more verifier passes;
3. fewer total normalized tokens;
4. lower reconciled provider cost;
5. lexicographically smaller candidate ID.

An incomplete candidate is recorded but cannot advance, regardless of its
apparent pass, token, or cost totals. If a round has too few complete candidates
to fill the next round, the study stops for a public amendment instead of
promoting an incomplete candidate.

Round 3 selects one winner. Before the sealed screen, its source, binary,
configuration, model, budget, and analyzer are frozen. If the winner changes
after seeing any screen result, the screen is failed and this study cannot make
the planned confirmatory claim.

The winner is screen-eligible only if its complete Round 3 development result
has point accuracy improvement of at least 10% and point token improvement of
at least 10% against the same ten-task Claude comparator subset, with no failed
or accounting-incomplete trial. This development gate is exploratory and does
not establish the claim; it prevents spending the sealed screen on a candidate
that does not yet look competitive.

Round 3 and the comparator have unequal replicate counts, so this gate uses
task-balanced per-attempt estimands rather than raw totals. For each of the ten
tasks, calculate the mean verifier reward and mean normalized tokens over all
three Stella attempts and separately over all five Claude attempts, then sum
the ten task means for each system. Accuracy is the mean of the ten task reward
means. Apply the same relative-improvement formulas used by the screen. Every
attempt, including a failed attempt, remains in its denominator; an incomplete
attempt makes the candidate ineligible.

## Bootstrap reproducibility

Both bootstrap stages use a dependency-independent SHA-256 counter sampler.
For replicate `r` starting at zero, draw position `j` starting at zero, and
rejection counter `k` starting at zero, hash the ASCII bytes:

```text
<domain> "\0" "20260721" "\0" <r> "\0" <j> "\0" <k>
```

The decimal integers have no sign or leading zero. Interpret the first eight
digest bytes as an unsigned big-endian integer `u`. For `n` tasks, reject and
increment `k` while `u >= floor(2^64 / n) * n`; otherwise select zero-based
task index `u mod n`. The screen domain is
`stella-tb21-screen-bootstrap-v1`; the confirmatory domain is
`stella-tb21-confirmatory-bootstrap-v1`. Task arrays are in the published
canonical split order. This completely fixes sampling across languages,
library versions, locales, and hash seeds.

Each resampled task occurrence carries all attempts for that task and counts
with its sampled multiplicity. The implementation emits the sampler test
vectors and the SHA-256 digest of all selected indices before any outcome is
analyzed.

## Sealed screen

The screen runs only the frozen winner on the 20 screen tasks, with five
attempts per task, zero Harbor retries, and no stitched or resumed trials. The
per-trial budget is the winner's frozen value and cannot exceed $0.30, giving a
maximum screen spend of $30.

The screen is complete only when all 100 canonical trials exist, every
verifier result and trajectory is present, token accounting is complete, the
provider-usage reconciliation is within the frozen tolerance, and the secret
scanner passes.

For each bootstrap replicate, sample the 20 tasks with replacement and retain
all attempts for the sampled tasks. Compute against the exact same task subset
from the pinned Claude Code comparator:

```text
accuracy_improvement = stella_accuracy / claude_accuracy - 1
token_improvement = 1 - stella_tokens / claude_tokens
```

For both systems, accuracy is verifier passes divided by all canonical attempts
in the sampled task multiset. Tokens are the analyzer's normalized input-plus-
output token counts summed over those same attempts. Failed attempts remain in
both denominators and token sums; missing or unaccounted attempts fail the
stage. Because screen and confirmatory data both have five attempts per task,
raw totals and task-balanced per-attempt totals have the same ratio.

Use exactly 50,000 task-cluster bootstrap replicates under the frozen sampler.
The screen passes only if:

- point accuracy improvement is at least 10%;
- point token improvement is at least 10%;
- at least 35,000 replicates meet both 10% thresholds simultaneously;
- there are no failed, missing, retried, resumed, or accounting-incomplete
  trials; and
- all public timing, intent, source, provider, host, and artifact gates pass.

This is a go/no-go rule, not a confirmatory significance claim. A failed screen
ends this study before the official run. Continuing after a failure requires a
new versioned study, new authorization, and a newly justified untouched set;
the failed screen and spend remain public.

The observed Claude subset accuracy and token total must both be positive. A
bootstrap replicate with zero Claude accuracy is conservatively counted as not
meeting the joint threshold; a replicate with nonpositive comparator tokens is
invalid and fails analysis.

## Later confirmatory authorization

A passing screen causes a pause. Before the official run, the owner must:

1. review the complete screen evidence and frozen winner;
2. explicitly authorize a new provider and infrastructure budget;
3. create or approve a distinct no-reset confirmatory runtime key;
4. approve the confirmatory preregistration and public-main commit; and
5. approve the exact 445-trial intent.

The official run uses the frozen screen winner on all 89 tasks, five attempts
per task, zero Harbor retries, concurrency one, and one fresh job. It cannot
reuse the tuning key, dev jobs, screen job, or their artifacts as replacement
trials.

The confirmatory run also requires a separately approved infrastructure budget;
the current $55 runner authorization covers only tuning and the sealed screen.

The primary inference filters both Stella and comparator data to the untouched
59 tasks, retaining all five attempts for each task. The official all-89 result
is reported separately and submitted to the leaderboard.

## Confirmatory estimands and success rule

For the untouched subset:

```text
accuracy_improvement = stella_accuracy / claude_accuracy - 1
token_improvement = 1 - stella_tokens / claude_tokens
```

Use a paired task-cluster bootstrap with 50,000 replicates and seed `20260721`.
Each replicate samples the 59 task identities with replacement and carries all
five Stella and all five Claude attempts for each sampled task.

The observed comparator accuracy and token total must be positive. A bootstrap
replicate with zero Claude accuracy is assigned negative-infinite accuracy
improvement and therefore cannot pass. A nonpositive comparator token total is
an analysis error. These rules avoid undefined ratios and are conservative for
Stella.

Two hypotheses are tested. To control family-wise one-sided alpha at 0.05,
each metric uses a 97.5% one-sided lower confidence bound. The scientific claim
is established only if, for both accuracy and tokens:

- the point improvement is at least 10%; and
- the one-sided 97.5% lower bound is strictly greater than 10%.

The lower bound is the noninterpolated empirical percentile bound: sort all
50,000 bootstrap improvements ascending, including assigned negative-infinite
accuracy improvements, and take the 1-indexed 1,250th value, equivalent to
zero-based index 1,249. An unexpected NaN or positive-infinite value, or any
replicate-analysis error, invalidates the claim rather than being dropped.

All-89 accuracy, tokens, cost, and wall time are descriptive secondary
statistics. Failures, missing trials, incomplete accounting, secret-scan
failure, task drift, source/config drift, or an ineligible upload make the
claim unestablished; they are never silently excluded.

## Public evidence chain

Before the first paid call, public `main` must contain:

- this study design's implemented protocol and exact task partition;
- the pinned comparator and dataset evidence;
- the adapter, analyzer, public-timing verifier, host attestation, secure
  launcher, and local evidence helper;
- the initialized append-only run ledger and budget authorization;
- the tuning-readiness preregistration and intent; and
- unedited owner-authored GitHub attestations binding immutable commits and
  exact ledger bytes.

Each paid intent records the cumulative provider usage before launch, maximum
stage spend, candidate identity, task split, job identity, host report, and all
runtime digests. Each outcome records complete artifacts, provider usage after,
reconciled telemetry, and status. All unsuccessful or superseded dev jobs stay
in the ledger and are excluded explicitly from screen and confirmatory data.

The secure launcher remains the only component allowed to reserve a job and
cross the paid-launch boundary. The local helper can author canonical evidence
but cannot authorize launch.

## Publication and marketing

After a successful official run:

1. scan the complete job and evidence trees for both OpenRouter secrets;
2. validate ATIF paths and all rewarded trajectories;
3. upload the unchanged job to Harbor as public;
4. run the current Terminal-Bench submission filter, metadata generator, and
   static analyzer in a separate current leaderboard environment;
5. open the authorized submission PR;
6. wait for official verification and leaderboard publication; and
7. capture the public leaderboard page showing Stella's row.

Permitted launch language must match the evidence. If both untouched-subset
gates pass, the strongest claim is that Stella, with the frozen tested
configuration and same GLM-5.1 model, exceeded Claude Code by more than 10% on
held-out Terminal-Bench 2.1 accuracy and token efficiency. The official all-89
score and rank are reported exactly. Claims of absolute leaderboard leadership
or general fleet superiority require their own evidence.

## Implementation decomposition

This design produces three sequential workstreams:

1. **Protocol and evidence tooling.** Version the old unexecuted v6 contract,
   implement the hybrid schemas, task split, analyzer, launcher gates, and
   local helper with witness tests.
2. **Runner and tuning operations.** Provision the approved non-root AWS
   runner, run readiness and the three development rounds, and preserve every
   job.
3. **Candidate behavior changes.** Each Stella change discovered during dev
   tuning receives its own small design, witness test, review, and frozen
   candidate commit.

The first implementation plan covers only workstream 1. Paid tuning cannot
start until that plan is complete, reviewed, published on `main`, and locally
reverified.
