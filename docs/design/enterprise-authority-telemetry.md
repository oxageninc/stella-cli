# Enterprise Authority and Telemetry

Status: approved for implementation

## Purpose

Stella remains a local-first, provider-neutral execution engine. Oxagen
Enterprise adds the governed control plane: principal identity, tenant policy,
approval, lineage, audit, retention, and operational telemetry. Community
Stella never sends telemetry by default. Enterprise export exists only after a
managed, signed enrollment is installed outside the repository.

This design implements the first two delivery phases:

- Phase 0 closes authority and verification bypasses.
- Phase 1 makes budget, context, privacy, and enterprise operational telemetry
  reliable enough for an enrolled deployment.

## Non-negotiable invariants

1. Repository content is evidence, never authority. An untrusted repository
   cannot enable tools, replace privileged prompts, register executable custom
   tools, approve spend, approve scope, or configure telemetry.
2. Effective authority is the intersection of the built-in default, managed
   ceiling, explicit repository trust, session/host grant, and role-specific
   restriction. Lower-precedence input may narrow authority but never widen it.
3. Machine output format never changes approval policy.
4. A verifier has less authority than a worker. Witness authoring and baseline
   execution happen in a disposable snapshot and never in the user's tree.
5. Model output is never interpreted as an unrestricted shell program.
6. `Completed` means verification passed or verification was not required.
   Failed, aborted, cancelled, and indeterminate outcomes are distinct.
7. Every settled model call contributes to the returned and persisted cost,
   including calls made before an abort.
8. Local raw execution data stays local. Enterprise export is a strict,
   content-free schema derived from a finalized local execution rollup.
9. Operational telemetry is bounded and fail-open. Compliance audit delivery
   is not claimed by this phase and cannot be enabled accidentally.
10. The core engine remains I/O-free; transports and persistence remain ports
    and adapters.

## Authority model

`AuthorityPolicy` is computed once while loading settings. Only the managed
settings file may define a ceiling. Project scope is untrusted unless the user
explicitly enables repository trust, and managed denial always wins.

Untrusted project scope may retain cosmetic provider metadata and may narrow
an already granted capability. It may not:

- enable `bash`, web, process, paid media, or other effectful tools;
- replace agent system prompts;
- load workspace custom tools, commands, agents, skills, memories, or rules as
  privileged instructions;
- configure or redirect enterprise telemetry.

### Paid-media host-data isolation

An approving `MediaSpendGate` is necessary but not sufficient. The registry
constructs approving image/video tools only when the host also supplies a
retry-stable operation ID source, the host-owned operation journal, the
managed approval ceiling, and `HostDataIsolation::ProcessFree`. That isolation
mode removes every built-in process-launching, process-control, delegation,
and process-backed issue tool from the same registry, including fixed-command
search and repository helpers.

Hosts must not add arbitrary-process MCP or custom tools around that registry.
The shipping local CLI supplies neither an approving spend gate nor the
process-free attestation, so paid generation remains fail-closed. Keeping the
journal outside the workspace and validating paths are secondary defenses;
they are not treated as process confinement.

### Enrolled telemetry process boundary

An enrolled telemetry claim must request `process_free`, and the secure
administrator-managed policy must independently permit exactly that mode.
Before an HTTP client is constructed, Stella builds and inspects a process-free
`ToolRegistry`. While that authority is active, the only admitted execution
surface is the raw one-shot engine directly over that concrete registry.
Pipeline one-shot, goal, fleet, deck, interactive, workspace-port, and candidate
workspace constructors return a named authority error before provider or
subprocess-capable port construction. The raw path does not construct MCP,
custom, interactive, skill, discovery-action, hook, typed-test, Git diagnostic,
or candidate-workspace wrappers. This is a production composition property,
not an environment-variable or synthetic-registry attestation.

Child-environment scrubbing is defense in depth, not a same-user security
boundary. Deployments that provide telemetry credentials in environment
variables MUST isolate enrolled Stella from untrusted same-UID processes with
an OS account, container, or equivalent host boundary. A credential broker
that issues request-scoped delivery authority is the preferred longer-term
replacement for ambient bearer credentials.

## Verification model

Witness preparation uses the existing candidate-workspace abstraction. When
authored witnesses are enabled, even a single candidate runs in a disposable
snapshot. The witness author, baseline test, worker, revision, and final test
all observe that snapshot. Only a passing candidate can be adopted.

Test execution uses a typed invocation containing a program and argument
vector. Shell operators, redirection, interpolation, and pipelines are not a
test protocol. Existing free-form commands remain available only as explicit,
user-supplied legacy configuration and require host approval.

## Enterprise operational telemetry

The local SQLite store remains authoritative. After an execution is finalized,
Stella derives one `StellaOperationalEventV1` containing bounded identifiers,
outcome, timing, token/cost totals, tool-call counts, and aggregate file-change
counts. Its type has no fields capable of carrying prompts, source, paths,
arguments, results, reasoning, errors, git metadata, memories, or rules.

Enrollment is managed-only and signed using explicit domain-separated,
length-framed canonical bytes. It binds issuer, audience, enrollment,
organization, workspace, endpoint, distinct signing/bearer credential
references, allowed event classes, the closed provider/model catalog,
process-free isolation, issue time, and expiry. The administrator-managed file
is opened without following a terminal symlink and must be an owner/root-owned,
single-link regular file that is not group/other writable. Privileged startup
environment values and credential references are snapshotted before project
dotenv loading and restored afterward. Invalid enrollment bytes cannot register
arbitrary scrub names. Every delivery rechecks expiry and credential-domain
separation.

The event id is domain-separated and length-framed over schema, class,
enrollment, organization, workspace, a persistent random installation UUID, a
persistent random store UUID, the local execution row id, and a CSPRNG nonce
persisted when that export-ledger row is first created. A copied store therefore
creates a different first-export ID, while retries of one ledger row remain
stable. Installation/store identities and the nonce never serialize.
Provider/model dimensions are admitted only
by the signed catalog; every unknown pair projects to the closed `other/other`
dimension.

Every terminal path, including cancellation, first passes through one closeout
aggregator. A per-store enrollment boundary excludes pre-enrollment history; a
persistent local ledger records post-enrollment export intent before spool I/O
and backfills at most 256 missed enqueues per runtime construction. Repeated
startups make bounded progress without allocating an outage-sized vector.
Legacy ledgers add the nonce column in place and persist a versioned rowid
cursor. Each startup commits four independent batches of at most 256 rows, so
both memory and write-lock duration stay fixed while a large migration resumes
without losing rows or changing an already-issued nonce.
Spooled ledger rows retain the newest 2,048 records; older completed rows compact
behind a durable per-enrollment execution boundary so closeout cannot mint a new
nonce for compacted history. Events then enter a separate bounded
SQLite spool after local finalization. Delivery is at-least-once with
deterministic event IDs. Each row is immutably bound to a cryptographic sink
fingerprint; claim, acknowledgement, and retry require that exact sink. Rotated
rows are stranded rather than sent to the new sink, and only the explicit
`stella telemetry rollover-discard` command removes them while incrementing a
durable rollover counter.

A bounded detached startup flush and `stella telemetry flush` attempt delivery.
Graceful shutdown guarantees durable local enqueue intent but never blocks on
network I/O; the next startup or explicit flush retries it. Failure remains
locally visible and never changes the agent outcome. The spool reports retained,
duplicate, and dropped-new outcomes separately, tracks database/WAL/SHM disk
bytes, repairs retry/lease deadlines after clock rollback, orders equal-time
rows by a monotonic insertion sequence, and applies bounded retry jitter. Clock
rollback translates created/retry/lease deadlines once against a persisted
per-sink clock anchor and generation, returns that generation with every claim,
and preserves live lease ownership. Delivery rereads wall time before retry;
the retry transaction checks the current generation so a claimant racing a
rollback cannot restore a pre-repair deadline. Retry delay including jitter is
capped at an inclusive 375 seconds. Capacity enforcement may evict
only an unleased row belonging to the inserting sink; if another sink consumes
the global budget, the new row is dropped. Malformed rows are validated before
lease, counted durably, and cannot block a later valid row. Their payload is
deleted; diagnostics retain only a fixed event fingerprint and bounded metadata
in a 128-row newest-first sample pruned by the same transaction. Status reports
the sample's row/metadata-byte footprint separately and includes the real
SQLite/WAL/SHM footprint; WAL checkpoint and journal-size limits prevent
repeated corruption from growing the diagnostic file without bound. Cost
conversion rejects non-finite, negative, and rounded `u64` upper-bound values
before casting. Delivery disables ambient HTTP proxies, redirects, and
unbounded request/response bodies.

`compliance_audit` enrollment is rejected in this phase. Compliance delivery
requires a non-evicting ledger, server receipts, retention/hold semantics, and
an explicit managed fail-closed rule.

## Error semantics

- Policy denial is typed and names the source of the ceiling.
- Headless scope review returns `ScopeReviewRequiredHeadless`.
- Verification failure returns `PipelineStatus::VerificationFailed`.
- Budget aborts retain settled spend and stop before another paid call.
- Telemetry enrollment, spool, and delivery failures are observable but do not
  fail an agent turn.
- Unsupported compliance enrollment is a configuration error, not a silent
  downgrade to operational telemetry.

## Acceptance evidence

- Every behavior change has a test observed failing before implementation.
- Narrow crate tests pass after each task.
- The full `make gate` passes before push.
- GitHub CI passes on the PR head.
- A whole-branch security and correctness review has no Critical or Important
  findings.
