# Task 7 report: enrolled enterprise operational telemetry

## Outcome

Task 7 is complete. Community Stella still creates no enterprise telemetry
state, HTTP client, socket, or egress by default. A managed deployment can opt
in only with a currently valid HMAC-SHA256-signed enrollment whose issuer,
audience, exact HTTPS endpoint, event class, organization, workspace, and
credential references satisfy managed allowlists and the closed schema.

After every centralized terminal closeout, Stella derives one content-free
`StellaOperationalEventV1` and durably inserts it into a separate owner-only,
host-data SQLite spool. Delivery is at-least-once with deterministic event IDs,
sink-scoped claims/acks/retries, leases, jittered retry backoff, hard row/byte
capacity, sink-local oldest-unleased eviction, and durable
drop/corruption/rollover counters.
`stella telemetry status` reports payload and physical SQLite/WAL/SHM health,
including the bounded quarantine sample's row and metadata-byte footprint plus
durable categorized legacy-ledger skip counters;
`flush` performs one explicit bounded delivery, and `rollover-discard` is the
only path that discards rows stranded by sink rotation.

Graceful shutdown guarantees durable local enqueue, not network delivery. A
detached bounded startup flush and the explicit command retry pending events;
shutdown never blocks on a network call. This deliberate safe deviation is now
part of the design contract.

## Privacy and authority invariants

- The wire type cannot represent prompts, source, paths, tool arguments or
  results, reasoning, errors, git metadata, memories, rules, project names,
  local project IDs, or local execution IDs.
- Events contain only bounded managed identifiers, provider/model dimensions,
  finalized outcome, duration, token/cost totals, tool-call/file-change counts,
  and whether output was produced.
- The deterministic event ID hashes a domain-separated framed schema/class,
  enrollment/tenant, persistent random installation/store UUIDs, execution row
  identity, and a per-export CSPRNG nonce persisted in the ledger. Copied stores
  mint different first-export IDs while one ledger retry remains stable; local
  inputs are never serialized.
- Provider/model dimensions come from a signed closed catalog. Unknown or
  custom values serialize only as `other`.
- Enrollment is accepted only from the managed settings snapshot. User and
  project copies cannot opt in, redirect the endpoint, or add event classes.
- Every endpoint allowlist entry and the enrolled endpoint must be exact,
  credential-free HTTPS URLs without query strings, fragments, or redirects.
- `compliance_audit` is rejected rather than silently downgraded to an
  evictable operational event.
- The verification secret and bearer token are environment references, never
  configuration values. Both references must resolve from the host environment;
  a project `.env`/`.env.local` origin is rejected even when the enrollment is
  otherwise valid and correctly signed.
- The managed file is no-follow, owner/root-controlled, and not group/other
  writable. Privileged controls and credential references are captured before
  project dotenv loading and restored afterward. Only a fully verified
  enrollment may register scrub names.
- Active enrollment requires signed and host-permitted `process_free`
  authority. Only raw one-shot execution is admitted, directly over the
  concrete process-free registry. Pipeline, goal, fleet, deck, interactive,
  workspace-port, and candidate constructors fail by name before provider or
  subprocess-port construction; MCP, custom, interactive, skill, discovery,
  hook, typed-test, and Git-diagnostic wrappers are not constructed.
- All session/model-controlled child spawns scrub registered credentials.
  Scrubbing is not a same-user boundary: production enrollment requires an OS
  account/container boundary from untrusted co-tenants; a request-scoped
  credential broker remains the preferred follow-on.
- Host delivery may fail, retry, or lose an oldest record from the inserting
  sink under the explicit capacity policy, but it never evicts another sink or
  changes a completed agent outcome.
- Signing and bearer references and values must be distinct. Enrollment expiry
  is checked again on every send, and HTTP ignores ambient proxy variables.
- A persistent post-enrollment export ledger records retry intent before spool
  I/O and backfills missed enqueues in pages of at most 256 without exporting
  pre-enrollment history. Completed rows compact behind a durable idempotency
  boundary while retaining the newest 2,048 records.
- A legacy ledger migration adds its nonce column in place and persists a
  versioned rowid cursor. Startup commits four independent 256-row batches, so
  50,000-row histories resume without an unbounded vector or transaction. A
  mark or bounded pending-page read transactionally repairs any empty legacy
  nonce beyond that startup budget before runtime event construction.
- Malformed legacy pending candidates move atomically from the retry ledger into
  a distinct durable skip table with a closed content-free reason category.
  Aggregate counters survive bounded completed-row compaction; a skipped row
  never masquerades as a successful spool enqueue or re-enters pending work.

## TDD evidence

RED was observed before implementation for the missing store module, CLI
module/dependencies, managed-only settings accessor, telemetry command, and
redirect helper. Focused regressions then established and closed these cases:

- deterministic IDs, content-free serialization, unknown-field rejection, and
  invalid or unfinished rollups;
- row/byte eviction, durable drops, owner-only permissions, disjoint concurrent
  claims, retry backoff, lease recovery, and hard batch-request bounds;
- immutable sink fingerprints, rotation stranding, explicit rollover discard,
  legacy-spool migration, clock rollback repair, FIFO insertion sequencing,
  retry jitter, and physical disk accounting;
- persistent random installation/store identity boundaries, closed managed
  model dimensions, exhaustive runtime terminal outcomes, and checked SQLite
  integer conversions;
- absent, invalid, expired, wrongly signed, wrong issuer/audience/schema,
  forbidden-scheme, non-allowlisted, and unsupported compliance enrollments;
- rejection of the entire endpoint allowlist when any entry violates the
  strict HTTPS policy;
- community/default construction producing no client and no host state;
- managed-only source precedence and workspace/symlinked spool-path rejection;
- redirect/non-success retry behavior, failed-delivery persistence, and
  successful acknowledgement;
- execution success when telemetry host state is rejected;
- an enrolled host successfully flushing through a fake transport while the
  exact `run_tests { command: "env" }` adversarial tool cannot observe either
  credential name or value; and
- the project-dotenv credential provenance witness is GREEN: enrollment is
  rejected when either the HMAC verification secret or bearer token came from
  project dotenv state, with neither reference disclosed in the error.
- the process-free surface enumeration is GREEN: every built-in process/search
  action and `search_skills`/`install_skill` is absent; and
- the export-ledger witness is GREEN: pre-enrollment executions are excluded,
  post-enrollment pending intent survives reopen, a 10,050-row outage is paged,
  two startup runs advance by exactly 256 rows each, 1,026 pending legacy rows
  remain consumable on the first post-upgrade runtime, and completed history
  compacts without minting a replacement nonce;
- the spool hardening witnesses are GREEN: capacity never evicts another sink,
  rollback repair preserves a concurrent live lease and rebases once, stale
  claim generations cannot restore an old-epoch retry deadline or let a delayed
  pre-rollback high-clock claimant overwrite the repaired anchor and steal its
  lease, and the full observed clock anchor also fences a delayed pre-forward
  claimant from creating an already-expired lease. Delivery reads a fresh retry
  clock, retry horizon is at most 375 seconds, and malformed rows quarantine
  before lease while later valid rows continue;
- the 50,257-row legacy-ledger witness is GREEN: first open preserves the
  ledger's SQLite root page instead of rebuilding or copying it, each startup
  migrates exactly four committed 256-row batches, progress resumes from
  durable version/cursor state, and completion preserves every row with a
  distinct nonce;
- the repeated-corruption witness is GREEN: the aggregate count reaches 8,000,
  diagnostics stay at 128 rows and under 32 KiB even with a 100 KiB corrupt
  identifier, and WAL/journal limits bound physical growth; and
- the numeric boundary witness is GREEN: the rounded `u64` upper edge is
  rejected before float-to-integer conversion.

## Implementation notes

- `stella-store::enterprise_telemetry` owns the transport-neutral event and
  spool. The CLI adapter alone owns enrollment verification and HTTP.
- The spool defaults to 10,000 rows and 16 MiB. Claims are additionally capped
  at 1,000 events and 16 MiB; production delivery uses 50 events and 256 KiB.
- Corrupt payloads and raw identifiers are deleted. Quarantine retains only a
  fixed SHA-256 event fingerprint and bounded metadata for the newest 128
  diagnostics; status exposes that row/byte footprint and the durable aggregate
  corruption-drop count.
- HTTP disables proxies and redirects, uses 2-second connect and 5-second total timeouts,
  and caps response bodies at 64 KiB while streaming.
- SQLite uses a 100 ms busy timeout so telemetry contention fails open quickly,
  plus a 256-page WAL autocheckpoint and 1 MiB journal-size limit.
- New direct dependencies are existing workspace crates: `sha2` for event IDs,
  `hmac` for signed enrollment, `reqwest` for the bounded HTTPS adapter, and
  `futures-util` for capped streaming response reads.

## Verification

- `cargo test -p stella-store`: 87 unit and 31 enterprise telemetry integration
  tests passed.
- `cargo test -p stella-tools`: 335 unit tests passed; 1 existing sandbox test
  remained ignored; 4 media replay tests passed. The 6 tracker and 8 web
  localhost integration tests passed outside the restricted network sandbox.
- `cargo test -p stella-cli`: 381 tests passed, including the project-dotenv
  provenance and credential non-disclosure witnesses.
- `cargo clippy -p stella-store -p stella-tools -p stella-cli --all-targets --
  -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `make sizes`: all 307 tracked Rust files passed the ratchet.
- `git diff --check`: passed.
- The final public-claim audit qualifies Community/default zero telemetry
  egress with the explicit signed Oxagen Enterprise managed exception and a
  link to its governed boundary, including the strict air-gapped guide.

## Final-review remediation

- C1: credential scrub names are registered immediately after signature
  verification. Authority enters a failed-closed state before the concrete
  process-free registry proof and becomes active before host path, local store,
  enrollment ledger, identity, spool, sender, or backfill setup. Proof failure
  blocks every execution surface; later delivery/storage failure cannot restore
  full authority.
- I6: `VerifiedEnrollment` now stores bounded validated identifiers before any
  store or ledger mutation. Malformed legacy pending candidates enter a
  distinct durable skip table with closed reason counters; later valid rows in
  the same bounded page continue to the spool. The legacy ledger is never
  rebuilt or copied during first-open migration.
- I8: all assigned public privacy claims now state the Community/default
  zero-egress contract and link the sole explicit signed Oxagen Enterprise
  operational exception. Fully air-gapped instructions explicitly require no
  managed enrollment and loopback-only configured endpoints.
