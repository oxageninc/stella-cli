# Terminal-Bench 2.1 local evidence helper design

Status: approved direction; implementation pending written-spec review

Date: 2026-07-21

## Purpose

The hybrid Terminal-Bench study requires an append-only public chain of task
partitions, budget authorizations, preregistrations, paid intents,
publications, host reports, and outcomes. Hand-editing exact JSON, timestamps,
sequence numbers, digests, and GitHub comment bodies creates avoidable risk.

Add one local CLI that generates and validates those bytes without acquiring
the authority to publish or spend. The helper is not evidence by itself. Git
commits, unedited GitHub comments, secure-launch receipts, Harbor artifacts,
ATIF trajectories, analyzer reports, and the official leaderboard remain
authoritative.

## Boundary

Implement a thin console entry point named `stella-tb21-evidence`, backed by a
focused evidence-contract module. The helper is deterministic, network-free,
credential-free, and incapable of launching Harbor.

Here, network-free means no IP socket, remote connection, or name resolution.
The one permitted socket path is the configured local Docker daemon's Unix
socket, reached only through the existing read-only host probe. That probe may
inspect daemon identity and running-container count but may not pull an image,
start a container, or mutate daemon state.

The canonical pure contract lives at
`bench/terminal_bench_analysis/tb21_evidence_contract.py`, beside the analyzer.
The CLI lives at
`bench/harbor_adapter/stella_harbor/tb21_evidence.py`, beside the host probe.
The analyzer imports the sibling contract normally; the adapter loads the exact
contract file from the repository only after validating its frozen path and
digest. Neither module may grow into the existing large launcher file.

It may:

- read canonical repository files and local Git objects;
- read explicitly supplied nonsecret snapshots exported by separate online
  control-plane commands;
- inspect completed local Harbor jobs without changing them;
- collect the existing credential-free native-host report without pulling an
  image or starting a container;
- calculate canonical bytes and SHA-256 digests;
- atomically write fixed evidence files after complete validation; and
- render an exact GitHub comment body to stdout or an explicit local path.

It must never:

- read `OPENROUTER_API_KEY`, a management key, or any credential-like
  environment variable;
- open an IP socket or remote connection, use any Unix socket other than the
  configured local Docker daemon, or instantiate GitHub, provider, cloud, or
  Harbor clients;
- invoke `git add`, commit, push, merge, or mutate a ref;
- create or edit an issue, comment, PR, key, cloud resource, job, upload, or
  submission;
- reserve a job name, invoke a model, run Harbor, or resume a job;
- write inside a Harbor job/trial tree; or
- describe a local result as online-verified or launch-authorized.

Network and secret tripwires in tests make these boundaries executable.

## Contract ownership

The hybrid protocol, secure launcher, analyzer, and host-attestation module are
the sources of truth. The helper uses one shared pure evidence-contract module
rather than copying field sets into independent validators. The launcher and
analyzer consume the same contract and revalidate every generated artifact.

The helper's version and source digest are frozen in each paid intent, even
though it remains non-authoritative. This prevents an undisclosed generator
change between stages.

The fixed schema identifiers are:

- `stella-tb21-task-partition-v1`;
- `stella-tb21-run-ledger-v3`;
- `stella-tb21-study-manifest-v7`; and
- `stella-tb21-github-attestation-v3`.

The v3 ledger's exact top level contains schema/study identity, fixed paths,
task-partition digest, budget authorizations, prior-exploration disclosure,
preregistrations, candidates, intents, publications, and outcomes. All record
arrays use one globally unique positive sequence space.

## Fixed paths and encoding

Tracked evidence is limited to:

- `bench/evidence/stella-tb21-task-partition.json`;
- `bench/evidence/stella-tb21-run-ledger.json`;
- `bench/evidence/stella-tb21-study-manifest.json`; and
- `bench/evidence/host-attestations/<intent_sha256>.json`.

No command writes a partial `github-comments.json`; that final export is valid
only when every required comment and final ledger commit exist.

Tracked JSON is UTF-8, duplicate-key-free, finite-number-only, sorted by key,
encoded with compact separators, and terminated by exactly one newline.
GitHub comment bodies use the same JSON encoding with no BOM and no trailing
newline because the public body bytes are hashed exactly.

All tracked writes use compare-and-swap against an expected preimage digest,
a mode-0600 temporary file in the destination directory, flush plus `fsync`,
and atomic same-filesystem replacement. Directories and files must be
canonical, non-symlinked, repository-contained, and owned by the current user.
There is no `--force` mode.

## Commands

### `init-study`

Creates the first hybrid task-partition and run-ledger files from the canonical
89-task dataset metadata and a reviewed static study seed.

It:

- verifies the pinned dataset bytes and exact 89-task inventory;
- freezes the disclosed ten-task development list;
- derives the 20/59 screen/untouched split using the approved hash algorithm;
- records the $100 provider authorization, separate $55 infrastructure cap,
  prior-exploration disclosure, study ID, schema versions, and fixed paths;
- refuses to run when either fixed file already exists; and
- prints only the new file paths and SHA-256 digests.

The static seed is reviewed source, not runtime input. The helper does not
fabricate historical records from ambient files.

### `append-preregistration`

Appends one exact lifecycle freeze, including tuning readiness, a development
round, the sealed screen, or confirmatory freeze.

Required inputs include kind, subject commit, explicit timezone-aware
`declared_at`, expected ledger preimage digest, and every kind-specific frozen
artifact. A confirmatory freeze also requires the exact study-manifest file and
stores its digest.

The command assigns the next globally unique positive sequence, enforces the
legal lifecycle prefix, validates source/ledger commit distinctness, and never
synthesizes a timestamp presented as external evidence.

### `render-comment`

Reads a committed ledger snapshot from a local Git object and renders the exact
GitHub attestation for one preregistration or paid intent.

It requires:

- subject type and ID;
- subject commit;
- distinct ledger snapshot commit;
- exact local Git ancestry; and
- an output path or stdout.

The committed ledger must contain the subject exactly once. The output is the
exact canonical body expected by the online secure-launch verifier. By default
the command changes no tracked file.

### `record-publication`

Appends one publication after the operator has posted an unedited comment and
exported its nonsecret GitHub comment and issue API objects to local files.

The helper verifies locally that the paired exports contain the fixed
repository and issue identity, owner author/association,
`created_at == updated_at`, exact comment body bytes, and expected comment-body
digest. Online authenticity is deferred to the secure launcher.

The ledger publication's `public_url` is
`https://github.com/macanderson/stella/commit/<ledger_commit>`, not the comment
URL. `published_at` is the exported GitHub server `created_at`, never local
time. The comment URL is returned in a noncanonical local receipt and becomes
part of the final complete comment export, not a hidden ledger field.

The final ledger must differ from the referenced snapshot only by this one
publication record. Duplicate subjects and lifecycle reordering fail.

### `prepare-intent`

Appends one paid intent and creates its public host report without reading a
secret or reserving a job.

Inputs include:

- stage, candidate, exact canonical Harbor argv, and subject commit;
- nonsecret frozen runtime-identity JSON;
- nonsecret provider snapshot JSON containing fingerprint, key name, limit,
  cumulative usage, remaining limit, account credits, and server snapshot time;
- jobs directory and Docker executable;
- explicit intent declaration time; and
- expected ledger preimage digest.

The runtime and provider snapshots are produced by separate read-only
control-plane utilities owned by the secure-launcher boundary. The helper
validates their exact schemas and frozen source digests but does not claim they
are still live; the secure launcher repeats the runtime checks and every online
provider check immediately before launch.

The exporters are distinct `stella-tb21-runtime-snapshot` and
`stella-tb21-provider-snapshot` entry points inside the secure-launcher package.
The runtime exporter may read only `OPENROUTER_API_KEY`, solely to bind its
SHA-256 fingerprint while performing the frozen local runtime-identity checks;
it opens no socket. The provider exporter may read that runtime key plus the
distinct management key and may call only the three frozen GET endpoints. Each
writes one nonsecret, canonical, digest-bound snapshot and cannot reserve or
launch a job. Neither is a subcommand of `stella-tb21-evidence`.

Those provider endpoints are exactly `GET /api/v1/key` with the runtime key,
plus `GET /api/v1/keys/<runtime-key-sha256>` and `GET /api/v1/credits` with the
management key. Redirects, alternate origins, and response-shape drift fail.

The command validates the stage/task/model/trial/concurrency/retry/budget shape,
computes the exact intent digest, collects the existing host report for that
digest, and appends the intent. It writes the ledger and create-only host report
as one logical transaction. On failure it restores the ledger preimage and
never overwrites or deletes existing evidence. An identical create-only retry
may report success; differing existing bytes fail.

### `record-outcome`

Appends the outcome required before a later stage can be prepared.

It reads an immutable completed local job, runs the shared analyzer and pattern
scanner in read-only mode, and derives rather than accepts:

- job ID and terminal status;
- started/completed times;
- artifact-tree and trial-data digests;
- trial counts, failures, retries, and accounting completeness;
- telemetry cost sum;
- provider usage before/after/delta from nonsecret digest-bound snapshots; and
- reconciliation status and tolerance.

The command writes nothing inside the job tree. Because it is credential-free,
it runs structural and credential-pattern scanning but does not claim an exact
raw-secret comparison. It refuses incomplete, resumed, stitched,
pattern-bearing, or intent-mismatched artifacts and appends one outcome with
the next sequence. Exploratory failures are recorded rather than erased. The
separate pre-publication gate later scans the complete tree against both exact
credential values.

### `freeze-manifest`

Creates the exact screen or confirmatory study manifest from the task
partition, ledger, selected candidate, completed prerequisite outcomes, runtime
identity, comparator, analyzer, and Harbor contract.

Every value is derived from committed or immutable inputs. Placeholders,
unknown IDs, extra fields, and manual overrides fail. Screen and confirmatory
manifests are distinct and versioned; the confirmatory manifest binds the 59
untouched-task primary estimand and the all-89 official secondary result.

### Command ordering

`init-study` runs once. Each paid stage then follows the immutable cycle:
append the preregistration, commit and externally publish its attestation,
record that publication, prepare the intent, commit and externally publish the
intent attestation inside the host-report freshness window, record that
publication, launch only through the secure launcher, and record the outcome.

After the development winner passes its eligibility gate, `freeze-manifest`
creates the screen-phase manifest at the fixed path before the sealed-screen
preregistration is appended. After the screen outcome passes and the owner
separately authorizes confirmatory funding, `freeze-manifest` compare-and-swap
replaces that file with the distinct confirmatory-phase manifest; Git history
preserves the exact screen bytes and digest. Only then may the confirmatory
freeze be appended and published. No preregistration may refer to a manifest
that was created or changed afterward.

### `validate-local`

Performs all structural checks available without a network connection or
credential:

- canonical schemas and fixed paths;
- exact task partition and disjointness;
- append-only lifecycle and global sequence ordering;
- snapshot-to-publication transitions;
- local Git object contents and ancestry;
- supplied comment exports;
- runtime/provider snapshot schemas and source digests;
- stage budgets and cumulative authorization;
- prior outcome and immutable job artifacts;
- fresh host report and current-host agreement; and
- absence of credential patterns from the complete proposed publication tree.

Its canonical result separates:

```json
{
  "local_ready": true,
  "launch_authorized": false,
  "deferred_online_checks": [
    "public_repository_and_main",
    "owner_unedited_comment",
    "publication_safety_wait_and_final_get",
    "live_runtime_and_provider_identity",
    "live_key_limit_usage_and_credits",
    "fresh_secure_launch_binding",
    "exact_secret_value_scan"
  ]
}
```

Exit zero means only local structural readiness. Exit one means contract
failure. Exit two means usage or I/O failure. No helper result can replace the
secure launcher's online attestation or receipt.

## Error handling

Errors are typed and deterministic:

- `usage`: incomplete or invalid CLI input;
- `preimage`: compare-and-swap mismatch;
- `contract`: schema, sequence, digest, split, budget, stage, or command drift;
- `publication`: invalid local comment export or snapshot transition;
- `artifact`: incomplete, mutated, resumed, credential-pattern-bearing, or
  mismatched job;
- `host`: architecture, memory, disk, Docker, container, freshness, or same-host
  failure; and
- `filesystem`: unsafe path, ownership, symlink, exclusivity, or persistence
  failure.

There is no fallback to a looser schema, local publication timestamp, alternate
branch, guessed value, partial write, or warning-only mode.

## Testing

Add focused witness tests that fail on the current branch because the helper
and shared hybrid contract do not exist. Required coverage includes:

- golden bytes for every file and both comment subject types;
- deterministic 10/20/59 partition generation;
- missing, extra, duplicate, reordered, and noncanonical JSON fields;
- NaN, infinity, booleans-as-integers, naive timestamps, and digest mutation;
- compare-and-swap, atomic write, symlink, failpoint, and idempotent retry;
- every legal and illegal lifecycle transition;
- exact stage models, tasks, trials, concurrency, retries, and budget math;
- outcome derivation without any job-tree mutation;
- host report binding and rollback;
- local-ready versus launch-authorized semantics;
- tripwires for IP and unexpected Unix sockets, credential environment access,
  Git mutation, GitHub or provider clients, Harbor launch, upload, submission,
  and cloud commands;
- integration coverage proving the separate pre-publication scanner checks
  both exact credential values;
- proof that the local helper itself performs pattern scanning only and leaves
  exact-value scanning to the online pre-publication boundary;
- round-trip acceptance by the production launcher and analyzer; and
- deterministic bytes across hash seeds, locales, and time zones.

Verification before publication includes the focused helper suite, full
adapter and analyzer suites, Ruff check/format, `git diff --check`, file-size
ratchet, and the repository's full `make gate`.

## Operational ownership

The helper prepares bytes. External authority remains separated:

- the operator reviews and commits generated evidence;
- Git/GitHub authentication performs public mutations;
- read-only online preflight utilities export nonsecret snapshots;
- the secure launcher alone revalidates online state, reserves a job, and
  crosses the paid boundary;
- Harbor upload occurs only after local completion and secret scanning; and
- Terminal-Bench's current static analyzer and maintainers determine
  leaderboard eligibility.

This separation prevents the evidence authoring tool from silently publishing
a claim, spending provider credits, provisioning infrastructure, or launching
an experiment.
