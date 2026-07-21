# Adaptive Context Implementation Plan

Status: implementation draft  
Primary repository: `macanderson/stella`  
Companion specification: `stella-adaptive-context-lifecycle.md`

This plan turns the adaptive-context specification into a staged Stella
implementation. It is intentionally repository-specific. Context Graph
Exchange Protocol (CGEP; proposed public name, current repository
`context-graph-protocol`) changes are a later interoperability layer, not a
prerequisite for local learning.

Complete one releasable phase per branch or change set and run its gate before
continuing. Cross multiple phases only on an explicitly designated long-running
implementation branch or at the user's direction.

## 1. Outcomes

Stella should be able to:

1. preserve observations, knowledge, memories, directives, evidence, and
   artifact contracts without conflating their authority;
2. reconstruct what it knew and what was valid at a requested time;
3. compile a deterministic, inspectable context package for each invocation;
4. learn advisory user behavior from independent evidence;
5. promote that behavior through solo, team, or regulated governance;
6. detect objectively incomplete deliverables through reusable contracts;
7. attribute selected context to outcomes without treating correlation as
   causation;
8. suppress stale or repeatedly unhelpful advisory context reversibly;
9. remain local-first and fully functional without a server or protocol
   lifecycle provider; and
10. exchange portable lifecycle records when a provider advertises the
    relevant Context Graph Exchange Protocol capabilities.

## 2. Decisions to freeze before implementation

### 2.1 Canonical semantic families

Use these separate record families:

```text
ContextRecord
├── observation
├── knowledge
│   ├── fact
│   ├── assumption
│   └── decision
├── memory
│   ├── episode
│   └── summary
├── directive
│   ├── preference
│   ├── rule
│   ├── constraint
│   └── procedure
├── record_proposal
├── evidence
├── artifact_contract
├── contract_validation
├── outcome_assessment
├── promotion_event
├── context_use
└── context_use_feedback
```

The boundaries are normative:

| Concept | Meaning | Instruction authority |
| --- | --- | --- |
| observation | An immutable interpreted occurrence | None |
| evidence | Addressable source material | None |
| knowledge | A believed, assumed, or decided proposition | None |
| memory | A bounded historical episode or summary | None |
| directive | Normative steering for future behavior | Advisory or blocking |
| record_proposal | A possible future record | None |
| artifact_contract | Machine-checkable definition of accepted completion | Completion authority when selected; never execution or tool authorization |
| contract_validation | Result of evaluating a contract | None |
| outcome_assessment | Qualified conclusion about an outcome | None |
| promotion_event | Immutable governance history | None by itself |
| context_use | Selection, rendering, or citation telemetry | None |
| context_use_feedback | Evaluation of a prior use | None |

Do not restore `memory` or `fact` as directive kinds. Do not treat a source-code
map or active task state as a lifecycle record by default. Do not allow a
learned record to grant tool, filesystem, network, identity, or security
authorization.

Outcome truth uses two independent dimensions:

```text
completion_assessment.status: complete | incomplete | unknown
correctness_assessment.status: correct | incorrect | unknown
```

Each dimension carries its own `assessment_level`: `verified`,
`user_confirmed`, `externally_confirmed`, `inferred`, or `unknown`. A complete
artifact can be incorrect; a correct partial artifact can be incomplete.

### 2.2 Record identity and lifecycle

Use:

```text
record_id               immutable revision identity
lineage_id              conceptual identity across revisions
supersedes_record_id    immediate prior revision
record_status           active | retracted | archived
effective_status        active | superseded | retracted | archived | expired
```

Every exchanged record has structured origin provenance:
origin_provider_id, origin_authority_id, producer_kind, producer_ref,
derivation_kind, and bounded source_refs. Stable provenance participates in the
record hash. Detached signatures, channel authentication, receiver accepted_at,
and receipts live in the ingestion ledger and are excluded from record_hash.
They prove integrity or identity, not instruction authority.

Validate origin/derivation combinations exactly as the lifecycle specification
defines them. Receiving a canonical provider record preserves both values and
adds ingestion metadata; it does not rewrite origin to imported.

`expired` is derived from `valid_until`. `stale` is a derived selection-health
assessment. `superseded` is derived from a later revision's
`supersedes_record_id`. None of these projections is written into an old record
or included in its hash.

Every change to status, proposal status, semantic content, scope, sharing, or
enforcement creates a new immutable record_id in the same lineage. A retracted
or archived terminal revision supersedes the previous revision. Publication
creates a new revision with the target sharing_scope plus a PromotionEvent that
links source_record_id and result_record_id.

Do not put `promotion_stage` on a directive. Governance history is represented
by immutable `promotion_event` records with these actions:

```text
proposed
auto_activated
confirmed
published
rejected
retired
reverted
```

There is no required sequence shared by every governance mode.

### 2.3 Applicability and sharing

`scope` answers where a record applies. `sharing_scope` answers who may receive
or inherit it. They are independent.

Core scope fields:

```text
user_id
organization_id
repository_id
workspace_id
environment_id
session_id
task_id
```

Core sharing values:

```text
user
repository
workspace
organization
```

The UI may render `user` as “Personal.” Repository sharing uses ordinary Git
and remains available offline. Workspace sharing is enabled only for a durable
provider-managed workspace with membership and RBAC; a temporary checkout is
not such a workspace. `project_id` remains a namespaced extension until Stella
has a durable project registry. A path, checkout, IDE project, or GitHub Project
must not be treated as a portable project identity.

All populated scope dimensions are conjunctive. Missing scope never widens an
inferred record. Require the matching identity for each audience: user_id for
user, repository_id for repository, workspace_id for workspace, and
organization_id for organization. Sharing values are not assumed to form one
linear hierarchy.

Durable portable scope IDs are globally unique and authority-qualified. Never
use display names, usernames, paths, folder names, or remote aliases as
portable identity. Preserve source IDs during import. Explicitly record every
authorized local-to-destination identity mapping in ContextExportManifest or a
provider receipt; mapping never mutates the source record or hash.

Sensitivity is common record classification: public, internal, confidential,
or restricted. Require it before export. Treat an absent legacy/local value as
restricted and create a classified immutable revision before any exchange.

### 2.4 Temporal vocabulary

Canonical record properties:

```text
observed_at
valid_from
valid_until
```

Canonical temporal query properties:

```text
known_at
valid_at
observed.from
observed.until
valid_overlaps.from
valid_overlaps.until
```

Intervals are half-open: `[from, until)`. `valid_until` is exclusive.
`recorded_at` and `valid_to` may be accepted as legacy input aliases but must
never be emitted by canonical writers.

`observed_at` is canonical origin-observer time and survives exchange.
`known_at` is evaluated from the answering store's knowledge vantage. In
Stella, define `local_knowledge_time` as `observed_at` for locally originated
records and the earliest `context_record_ingestions.received_at` for imported
records. The `observed` range always filters canonical origin `observed_at`.
Reject a `known_at` query if durable local ingestion history is unavailable;
never substitute origin time for receiver knowledge time silently.

### 2.5 Frame boundary

Keep four concepts separate:

```text
provider ContextFrame[]
        ↓
CompiledContextFrame
        ↓
PromptContext
        ↓
model invocation
```

- `ContextFrame` is the protocol's atomic retrieval envelope.
- `CompiledContextFrame` is Stella's complete bounded aggregate for one
  invocation.
- `PromptContext` is Stella's deterministic model-facing rendering.
- A snapshot or delta is a Stella-local cache optimization, not a replacement
  for either canonical type.

Frame representations are `full`, `compact`, and `reference`. Representation
and content fidelity are separate dimensions.

### 2.6 Existing storage remains authoritative

Use the current Stella stores rather than creating parallel files:

```text
.stella/private/context.db       lifecycle records, temporal links, retrieval metadata
.stella/private/store.db         executions, events, tool calls, operational telemetry
.stella/private/codegraph.db     source-code graph
.stella/rules/*.md       published repository steering
.stella/settings.json    governance, learning, retention, and sharing settings
.stella/context-snapshots/  optional derived cache, gitignored
```

Do not add `context-rules.yaml`, a second context database, or a second source
code graph. A published repository rule is canonical in Markdown; any row in
`context.db` is an indexed mirror with source identity and content hash, not an
independently editable copy.

### 2.7 Product and protocol boundary

Stella owns:

- evidence extraction and observation farming;
- proposal induction and confidence scoring;
- solo, team, and regulated governance;
- promotion thresholds and UI;
- artifact-contract execution;
- prompt compilation and token allocation;
- efficacy attribution, staleness, and pruning;
- local SQLite schema and Git publication.

Context Graph Exchange Protocol owns only portable exchange mechanisms:

- typed lifecycle records and links;
- scope, sharing, provenance, and temporal semantics;
- frame representations and reference resolution;
- capability negotiation;
- immutable append/get operations, idempotency, receipts, and typed failures.

### 2.8 Stella and Oxagen remain independently deployable

Stella is the complete local/BYOK data plane. Oxagen is an optional commercial
provider and control plane, not a runtime dependency. Oxagen may add durable
cloud workspaces, RBAC, organization policy, encrypted sync, audit, residency,
and enterprise integrations while implementing the same open provider
contract.

Do not copy SQLite files to a provider. Add a Stella-owned export compiler that
selects canonical record revisions after scope, sharing, data classification,
consent, provider capability, retention, and destination checks. Raw
`store.db`, journal, reflections, code source, BYOK credentials, and secrets do
not leave the machine by default.

Local extraction maps sources into semantic records before any export:

| Source | Local extraction or projection |
| --- | --- |
| `store.db` and journal | observations plus evidence locators and hashes |
| `reflections.jsonl` | candidate memories, knowledge, directives, or proposals |
| Git diffs and code graph | observations, code anchors, links, and hashes |
| `.stella/rules/*.md` | active repository directive revisions |
| contracts and validators | contracts, validations, and outcome assessments |
| context-use telemetry | use records; efficacy aggregates remain derived |

Database rows and files are never provider payloads. The export compiler selects
exact immutable ContextRecord revisions after local normalization.

Every outbound batch produces a `ContextExportManifest` with provider,
destination kind user/workspace/organization and matching identity, purpose,
policy version, actor or consent
reference, explicit identity mappings, exact record IDs and hashes, redactions, omissions, requested
retention/deletion behavior, timestamp, and batch hash. The first export is
previewable. A saved policy may authorize later matching batches, but any
widening requires a new decision.

Call this portable behavior export and provider retrieval in v1. The current
append/get/query/resolve surface is not a complete synchronization protocol.
Oxagen may provide product-specific encrypted sync, but CGEP must not claim
portable sync until a capability defines cursors, ordered change feeds,
acknowledgements, tombstones, conflict rules, deletion propagation, and offline
replay.

Repository sharing and workspace sharing remain independent:

- repository: ordinary Git publication, available offline and without Oxagen;
- workspace: optional provider-managed membership and RBAC, possibly spanning
  repositories;
- organization: inherited administrative policy and enterprise audience.

The protocol and conformance suite must permit a non-Oxagen provider.

## 3. Dependency and implementation rules

Implement in this order:

```text
domain vocabulary and compatibility contract
    ↓
internal additive events
    ↓
context.db migration and repositories
    ↓
temporal retrieval and CompiledContextFrame
    ↓
PromptContext compaction
    ↓
observation extraction
    ↓
record proposals and governance
    ↓
artifact contracts and completion gating
    ↓
repository publication and team governance
    ↓
efficacy, pruning, and Observatory
    ↓
optional protocol lifecycle adapter
```

Preserve the existing Cargo dependency graph:

- keep SQLite, Git, filesystem, terminal, and network I/O out of `stella-core`;
- do not make `stella-protocol` depend on `stella-context`;
- use stable IDs and small protocol-local payloads when importing a core type
  would create a cycle;
- do not make external Context Graph Exchange Protocol structs Stella's internal domain
  model;
- keep every new behavior behind settings until its phase gates pass;
- make event replay idempotent before enabling automated learning.

## 4. Phase 0 — Baseline, ADRs, and fixtures

### Work

1. Read repository instructions and record the current Cargo dependency graph.
2. Run and record the existing formatting, lint, test, and migration baselines.
3. Inventory current memory, fact, rule-mining, journal, event, provider, and
   context-frame code paths.
   Explicitly inspect legacy rule persistence such as `store.db` rule tables and
   `Store::list_rules` if present; record every current read and write path.
   Characterize the current `ContextQuery.as_of` behavior with tests; do not
   assume whether it means knowledge cutoff, world validity, or both.
4. Capture fixture copies for every supported `context.db` and `store.db`
   schema version.
5. Capture representative `.stella/rules/*.md` files, including existing guard
   frontmatter and aliases.
6. Add ADRs for:
   - semantic taxonomy;
   - scope versus sharing;
   - bitemporal semantics;
   - record revision identity;
   - storage authority;
   - `ContextFrame` versus `CompiledContextFrame`;
   - immutable promotion history;
   - Markdown repository rules remaining canonical.
7. Add disabled settings under `.stella/settings.json`.

Suggested initial settings:

```json
{
  "context": {
    "lifecycle": {
      "enabled": false
    },
    "learning": {
      "mode": "off"
    },
    "governance": {
      "mode": "solo"
    },
    "promotion": {
      "inferred_directive": {
        "min_observations": 3,
        "min_distinct_tasks": 3,
        "auto_activate_at_confidence": 85,
        "initial_enforcement": "advisory"
      },
      "blocking_directive": {
        "requires_explicit_confirmation": true
      }
    },
    "retention": {
      "raw_observation_days": 30,
      "proposal_days": 30,
      "inferred_directive_review_days": 180
    }
  }
}
```

Learning modes should be `off`, `record_only`, and `advisory`. Governance modes
should be `solo`, `team`, and `regulated`. Keep those dimensions separate.
When lifecycle is disabled, ignore the new learning and promotion settings and
preserve existing behavior. `off` disables mining, proposal induction, and
efficacy learning; `record_only` captures observations, proposals, uses, and
outcomes without selecting or promoting inferred records; `advisory` enables
governed inferred advisory use. Review age produces `review_due`, never stale
without efficacy or contradiction evidence.

### Gate

- The existing workspace passes its documented checks before feature changes.
- Legacy database and rule fixtures are committed to tests.
- New settings deserialize with defaults that preserve current behavior.
- No network or repository mutation is introduced.

## 5. Phase 1 — Domain types and internal events

### `stella-core`

Add pure domain types and validators for:

- `ContextRecordKind`;
- `KnowledgeKind`;
- `MemoryKind`;
- `DirectiveKind`;
- `ConstraintEffect`;
- `RecordStatus`;
- `RecordProposalKind` and `RecordProposalStatus`;
- `PromotionAction`;
- `DirectiveEnforcement`;
- `Scope` and `SharingScope`;
- `TemporalInterval` and `TemporalQuery`;
- `ContextUseKind` and `ContextUseEvaluation`;
- `ArtifactContract`, requirement kinds, and validation result types;
- `OutcomeAssessmentLevel`;
- `CompletionStatus` and `CorrectnessStatus`;
- frame representation, content fidelity, and minimum fidelity.

Use explicit constructors or validation functions for cross-field invariants.
Examples:

- `valid_until` must be later than `valid_from`;
- an inferred directive cannot be created with blocking enforcement;
- an inferred record must have a nonempty scope;
- a constraint effect is `require` or `forbid`, never `allow`;
- a procedure must preserve a unique ordered step sequence;
- confidence is in `0..=100`;
- a reference representation has no inline-content placeholder;
- a blocking or guarded directive requires exact minimum fidelity.

### `stella-protocol`

Add internal, replay-safe events without changing public Context Graph Exchange Protocol
wire semantics yet:

```text
ObservationRecorded
RecordProposalCreated
PromotionRecorded
ContextUseRecorded
ContextUseFeedbackRecorded
ArtifactContractSelected
ContractValidationCompleted
OutcomeAssessed
CompiledContextFrameBuilt
```

Each event carries an event ID, schema version, task/invocation identity when
applicable, `observed_at`, and only the stable IDs needed by consumers.

### Compatibility

- Accept legacy `recorded_at` as `observed_at` and `valid_to` as
  `valid_until` at ingestion boundaries.
- Emit only lowercase snake_case canonical fields.
- Encode every SHA-256 field as `sha256:<64 lowercase hexadecimal characters>`.
  Hash canonical records over RFC 8785 JCS bytes after alias normalization, absent-option omission, input-null
  normalization, and UTC timestamp normalization; omit `record_hash` from its
  own preimage.
- Treat ellipsized `sha256:...` values in prose examples as non-conformant
  placeholders. Golden vectors and machine-readable fixtures must contain real
  64-character lowercase hexadecimal digests.
- Hash inline and canonical content as exact UTF-8 bytes, not record JCS, unless
  a content schema explicitly defines canonical JSON.
- Preserve unknown namespaced extensions without granting them instruction
  authority.
- Keep legacy memory and fact APIs working through adapters until callers are
  migrated.
- Characterize the current `AgentEvent` decoder before adding variants. If
  ordinary tagged-enum Serde would reject an unknown event, add a versioned
  unknown-event envelope or gate emission until every replay consumer is
  upgraded. Do not assume enum additivity is wire compatibility.

### Gate

- Exhaustive type validation tests pass.
- Serde round trips are byte-stable for canonical fixtures.
- Alias fixtures deserialize and reserialize with canonical names.
- Legacy `as_of` behavior is preserved by a documented adapter and
  characterization fixture.
- Unknown extensions round-trip.
- Cross-repository canonical hash golden vectors pass.
- No I/O is added to `stella-core`.
- A legacy/compatibility decoder fixture preserves or safely skips a new event
  without failing journal replay.

## 6. Phase 2 — `context.db` schema and migration

### Canonical storage shape

Extend the existing migration system; do not create a replacement database.
The exact table prefix should follow current repository conventions. The
logical model needs:

```text
context_records
  record_id primary key
  lineage_id nullable for immutable event-only records
  schema_version
  record_kind
  record_status
  scope_json
  sharing_scope
  sensitivity
  observed_at
  valid_from nullable
  valid_until nullable
  confidence nullable
  supersedes_record_id nullable
  record_hash
  canonical_json
  source_of_truth_kind
  source_of_truth_ref nullable

context_record_links
  source_record_id
  relation
  target_record_id
  target_provider_id nullable
  expected_target_record_hash nullable

context_evidence_links
  record_id
  evidence_id
  relation
  evidence_provider_id nullable
  expected_evidence_record_hash nullable

context_record_ingestions
  record_id
  provider_id
  received_at
  authenticated_channel_ref nullable
  attestation_json nullable
  unique (record_id, provider_id)

extraction_cursors
  source_kind
  source_id
  extractor_id
  extractor_version
  cursor_json
  source_hash
  updated_at
  unique (source_kind, source_id, extractor_id, extractor_version)

compiled_context_frames
  compiled_frame_id
  task_id
  invocation_id
  compiler_version
  tokenizer_ref
  known_at
  valid_at
  input_hash
  frame_hash
  frame_json
  budget_json
  manifest_json
  compiled_at

compiled_context_frame_items
  compiled_frame_id
  ordinal
  record_id
  section_kind
  record_kind
  record_subtype nullable
  representation
  content_fidelity
  content nullable for reference
  content_hash nullable for reference
  canonical_content_hash
  content_ref_json nullable for full
  canonical_token_cost nullable
  minimum_content_fidelity
  inline_content_requirement
  transform_json nullable
  selection_reason
  token_cost

context_health_projection
  record_id
  selection_health
  opportunity_count
  selected_count
  rendered_count
  cited_count
  evaluated_count
  helpful_count
  not_helpful_count
  neutral_count
  last_cited_at nullable
  last_helpful_at nullable
  last_not_helpful_at nullable
  review_after nullable
  updated_at

context_exports
  export_id primary key
  provider_id
  authenticated_authority_id
  client_id
  destination_kind
  destination_id
  policy_version
  actor_ref nullable
  consent_ref nullable
  manifest_json
  manifest_hash
  created_at
  dispatch_status
  check (actor_ref is not null or consent_ref is not null)

context_export_items
  export_id
  ordinal
  source_record_id
  source_record_hash
  export_record_id
  export_record_hash
  idempotency_key
  command_hash
  requested_retention_json
  receipt_status nullable
  provider_error_json nullable
  accepted_at nullable
  idempotency_replay_until nullable
  accepted_retention_json nullable
  unique (export_id, ordinal)
  unique (provider_id, idempotency_key) through export join or equivalent
```

`canonical_json` is the one authoritative local byte representation of a
ContextRecord and contains the complete canonical record, including common,
type-specific, link, provenance, extension, and record_hash fields. Indexed
columns and link tables are validated, transactionally derived projections and
must rebuild exactly from canonical_json. Do not maintain separately editable
payload_json or extensions_json authorities. Hash verification parses
canonical_json, removes record_hash, applies the normative normalization and
JCS rules, and compares the digest.

`frame_json` is the complete immutable CompiledContextFrame, including task,
state, scope, code map, evidence references, selected-item inline content, and
manifest. Frame/item columns, budget_json, and manifest_json are query
projections that rebuild from it. `section_kind` describes placement such as
knowledge, memories, or directives; reserve `use_kind` exclusively for
ContextUse events.

Compute frame_hash from the RFC 8785 canonical semantic frame body with
compiled_frame_id, frame_hash, and compiled_at omitted. Identical semantic
inputs must produce the same body and hash even when a new envelope ID/time is
assigned.

`context_health_projection` is rebuildable from immutable `context_use` and
`context_use_feedback` records. It is never the source of historical truth.
`extraction_cursors` is owned by `stella-context`; `stella-store` and the journal
remain read-only extraction sources.

The export compiler writes context_exports and all context_export_items as a
durable outbox transaction before network dispatch. Retries reuse the same
idempotency_key and command_hash. Per-item receipts update status and accepted
retention without changing the manifest or source records. Crash recovery can
therefore replay only unacknowledged items and preserve a complete audit trail.

Use `source_of_truth_kind` only as Stella-internal storage metadata:

```text
context_db
repository_rule_file
organization_policy
external_provider
```

For `repository_rule_file`, `source_of_truth_ref` is the normalized repository
path and the database record is read-only from the lifecycle API. Semantic
authority and precedence remain separate domain concepts.
For external_provider, source_of_truth_ref identifies provider ID, source record
ID, and expected record hash. Cached provider records remain read-only until a
local governed revision is deliberately created.

### Indexes

Add indexes based on measured query plans, at minimum for:

- record kind, status, and sharing scope;
- lineage and supersession;
- `observed_at`;
- `valid_from` and `valid_until`;
- task, session, environment, repository, workspace, organization, and user
  scope projections;
- evidence and record links;
- frame task/invocation identity;
- context-use target record and time.
- export provider/destination/status and idempotency keys.

If scope remains JSON, maintain validated indexed projection columns for hot
dimensions. Do not rely on unindexed JSON scans in invocation compilation.

### Migration

1. Preserve current memory and explicitly typed fact IDs when possible.
2. Migrate every ambiguous legacy memory losslessly as a memory. Never run an
   LLM or semantic reclassification inside a schema migration. A later
   reclassification is a reviewable RecordProposal with evidence.
3. Make `context_records` the single canonical local authority. Existing
   memory/node/edge tables become compatibility views or rebuildable
   projections; legacy APIs become adapters over the canonical repository.
4. Write a canonical record and its graph/index projections in one transaction.
   Add a projection-rebuild command and test that starts from canonical rows.
5. Link migrated revisions through lineage rather than mutating history.
6. Import existing `.stella/rules/*.md` as authoritative read-only mirrors.
7. If legacy `store.db` rule rows or `Store::list_rules` exist, prohibit new
   lifecycle writes there. Rows tied to a rule file mirror that file. Rows with
   no file migrate as imported, repository-applicable but user-shared local
   directives so migration does not create Git changes or widen sharing.
8. During one compatibility release, keep legacy readers behind a documented
   precedence adapter: current Markdown rule wins for repository publication;
   canonical context record wins for local lifecycle data; unmigrated legacy
   rows are fallback-only and produce diagnostics.
9. Store migration version and content hashes so reruns are no-ops.
10. Run the migration in a transaction and retain existing backup behavior.

### Gate

- Every legacy fixture migrates transactionally.
- Record counts and content checksums reconcile.
- Existing memory/fact recall tests remain valid or have documented semantic
  replacements.
- Replaying a migration creates no duplicate records.
- SQLite integrity and foreign-key checks pass.
- Published rule mirrors cannot be edited through the database repository.
- Projection rebuild reproduces the graph/index rows byte-for-byte.
- Legacy store-backed rules retain behavior without remaining a new-write
  authority.

## 7. Phase 3 — Temporal retrieval and `CompiledContextFrame`

### Temporal repository API

Implement one typed query object with these semantics:

```text
known_at:
  local_knowledge_time <= known_at

local_knowledge_time:
  observed_at for a locally originated record
  minimum context_record_ingestions.received_at for an imported record

valid_at:
  valid_from <= valid_at
  and (valid_until is null or valid_at < valid_until)

observed [from, until):
  from <= observed_at < until

valid_overlaps [from, until):
  valid_from < until
  and (valid_until is null or from < valid_until)
```

Claim-bearing knowledge, directives, and contracts require non-null valid_from.
Event-only records may omit validity and are excluded from valid_at or
valid_overlaps unless `include_records_without_validity=true`; use observed or
occurred-time queries for events. Reject invalid or contradictory temporal
filters rather than silently reinterpreting them. Add truth-table tests for
boundary instants and SQLite null behavior.

Historical reconstruction first limits records and lifecycle events to
`local_knowledge_time <= known_at`, derives status from only that prefix,
applies the validity filter, then selects the maximal applicable revision per
lineage using supersedes_record_id. A revision received or created after
known_at cannot affect the result. Imported canonical `observed_at` remains
available to the separate `observed` filter.

### Frame compiler

Build `CompiledContextFrame` in `stella-context` or the existing context
assembly boundary. Inputs include:

- task and current state;
- complete scope;
- `known_at` and `valid_at` cutoffs;
- governance mode;
- code-map roots;
- provider ContextFrames;
- token and latency budgets;
- compiler and selection-policy versions.

Compilation pipeline:

1. resolve scope without widening;
2. retrieve active, temporally applicable records;
3. exclude retracted, archived, expired, suppressed, and incompatible-sharing
   records;
4. apply authority and directive precedence;
5. detect contradictions and supersession;
6. select required constraints and contracts;
7. rank optional knowledge, memories, and advisory directives;
8. allocate a deterministic budget;
9. choose per-item representation and fidelity;
10. produce an immutable manifest;
11. persist the frame and ordered item list;
12. emit `ContextUse` records for actual selection and later rendering.

The manifest records every included record ID, exclusion reason, conflict,
provider query, transformation, budget decision, and compiler version.

### Precedence

Use explicit category-aware precedence, not one global confidence score:

1. authorization boundaries and non-overridable system policy;
2. confirmed organization constraints;
3. authenticated blocking collaborative constraints and guards from a workspace
   or repository authority;
4. explicit instructions in the current user task;
5. selected required artifact-contract requirements;
6. confirmed collaborative directives from a workspace or repository;
7. confirmed user directives;
8. current knowledge;
9. advisory preferences and inferred advisory rules;
10. memories;
11. observation summaries and untrusted evidence.

SharingScope is not authority. Derive authority from origin, authenticated
publication, approval history, provider attestation, and an actual enforcement
boundary. At equal authority, narrower applicability wins; a workspace versus
repository conflict otherwise remains explicit. Confidence never overrides
authority. A preference never overrides a constraint. A memory never overrides
current knowledge.

### Gate

- Identical inputs produce byte-identical semantic frame bodies, ordering, and
  frame hashes; envelope IDs/times may differ.
- Historical tests distinguish `known_at` from `valid_at`.
- Scope leakage tests pass at every dimension.
- Conflicts and exclusions are inspectable.
- Required items cannot be evicted by ranking.
- Existing provider query behavior remains functional.

## 8. Phase 4 — Compaction and prompt rendering

### Representations

Implement:

```text
representation: full | compact | reference
content_fidelity: exact | normalized | summarized | omitted
minimum_content_fidelity: exact | normalized | summarized
inline_content_requirement: required | resolvable_reference_allowed
```

Invariants:

- `full` requires canonical inline content;
- `compact` requires inline content, transformation identity, inline hash,
  canonical hash, content_ref, and resolve support;
- `reference` omits inline content and requires an opaque reference plus
  canonical hash;
- reference resolution verifies the canonical hash;
- blocking constraints, guards, ordered procedures, and executable contract
  structures cannot fall below exact fidelity at their point of use;
- summaries remain linked to their canonical records;
- token counts describe the actual representation, not the source, and every
  compiled frame declares the tokenizer_ref used to compute them.

Normalize a legacy protocol frame as full with exact content. Compute missing
inline and canonical hashes from the same exact content; leave token costs
absent at the adapter boundary, then compute them under Stella's declared
tokenizer before compilation and budgeting.

Persist the complete representation lineage for each compiled item:

```text
content_hash
canonical_content_hash
content_ref
canonical_token_cost
minimum_content_fidelity
inline_content_requirement
transform_method
transform_implementation
transform_version
```

### `PromptContext`

Render a deterministic, model-specific text projection with stable citation
labels. Keep task, constraints, decisions, assumptions, relevant memories,
contracts, code, and active state visibly separated. Never serialize raw
untrusted observations into an instruction section.

### Stable base and delta

Implement Stella-local cached bases and invocation deltas only after a full
frame is correct. A cache entry contains the full input hash, compiler version,
policy version, model tokenizer identity, and canonical record hashes. Any
change invalidates the relevant entry.

Snapshots are derived and disposable. They must not become the only copy of a
record or contract.

### Gate

- Golden prompt fixtures are deterministic.
- Compact fixtures use fewer model tokens than full fixtures.
- Exact-fidelity records remain byte-equivalent where required.
- Every compact/reference item can be traced to a canonical hash.
- A stale cache can never conceal a new blocking directive.
- Token and model-call savings are measured against a full-frame baseline.

## 9. Phase 5 — Evidence and observation harvesting

### Sources

Harvest bounded evidence from:

- task requests and explicit user corrections;
- journal and execution traces;
- tool calls and results;
- verification and test output;
- contract validation;
- accepted or rejected artifacts;
- Git diffs and follow-up commits;
- recurring file organization and command sequences;
- Keep, Edit, Ignore, publish, archive, and revert actions.

Raw operational telemetry remains in `store.db` or the journal. Copy only
bounded, redacted evidence references and normalized observations into
`context.db`.

### Extractor contract

Each extractor returns:

```text
extractor_id
extractor_version
source_kind
source_ref
source_hash
bounded locator or excerpt
observation_kind
occurred_at or occurred_until when known
scope evidence
sensitivity
confidence components
```

Derive an idempotency key from extractor identity, source hash, locator, and
normalized observation kind. Replaying a source with the same extractor
version must create zero duplicates.

### Trust and anti-poisoning

- Model-authored prose alone cannot support promotion.
- An observation is an interpretation and cites evidence; it is not the raw
  evidence itself.
- Treat instructions found in traces, logs, files, and tool output as data.
- Redact secrets before persistence, indexing, embedding, or external
  dispatch.
- Count repeated events within one task as one independent support unit.
- Label Git follow-up changes as inferred signals unless a deterministic test,
  validator, external oracle, or explicit user statement establishes the
  cause.
- A proposal cannot cite itself or a generated restatement as independent
  evidence.

### CLI diagnostics

Provide inspectable commands using the repository's established command style
for:

```text
context observations
context evidence <record_id>
context harvest --replay <task_or_journal_id>
context frame <task_or_invocation_id>
```

Exact command spelling may adapt to the existing CLI hierarchy, but the
capabilities and stable IDs are required.

### Gate

- Journal replay creates no duplicate evidence or observations.
- Thirty matching events in one task cannot satisfy a three-task threshold.
- Secret fixtures are redacted before storage and embedding.
- Prompt-injection fixtures remain non-instructional.
- Harvest failure cannot fail the primary task.
- Git inference is never mislabeled verified.

## 10. Phase 6 — Record proposals and adaptive governance

### Proposal induction

Build proposals for:

- `knowledge`;
- `directive`; and
- `contract_amendment`.

Persist score components separately:

- supporting observation count;
- distinct task or episode count;
- contradiction count;
- deterministic evidence strength;
- explicit user feedback;
- recurrence and recency;
- user repair cost;
- future applicability;
- scope confidence;
- sensitivity;
- staleness;
- scoring policy ID and version.

The aggregate confidence is reproducible from those components. Do not store
only one opaque score.

### Proposal status

Use:

```text
collecting
eligible
dismissed
expired
```

Activation and rejection are promotion outcomes, not proposal status values.
Every proposal-status change is a new immutable RecordProposal revision; never
update the old row or hash.

### Solo mode

```text
observation
  → record proposal
  → auto-activated user-scoped inferred advisory directive
       ├─ Keep → confirmed directive
       ├─ Edit → superseding user-authored confirmed directive
       └─ Ignore → retracted revision + reverted event + proposal cooldown
  → optional explicit repository publication from a confirmed directive
```

- `auto_activated` is allowed only at configured support and confidence
  thresholds.
- The resulting directive is user-shared and advisory.
- Keep appends `confirmed`.
- Edit creates a superseding user-authored revision and appends the appropriate
  confirmation event.
- Ignore after auto-activation creates a retracted superseding directive
  revision, appends `reverted`, creates a dismissed proposal revision, records
  negative induction evidence, and starts a configurable re-proposal cooldown.
  The auto-activated revision must no longer be selected. If confirmation is
  requested before any activation, Ignore appends `rejected` and creates no
  directive.

### Team mode

```text
observation
  → record proposal
  → proposed repository directive
  → owner review
  → published .stella/rules/*.md
```

No inferred record publishes automatically.

### Workspace publication

```text
user or repository-applicable proposal
  → proposed workspace record
  → workspace owner or RBAC approval
  → immutable workspace-scoped published revision
  → provider receipt, attestation, audit, and read-only local cache
```

Do not write workspace steering into `.stella/rules/*.md`; Git remains the
repository authority. Require durable workspace identity, explicit approver,
reason, policy version, source/result PromotionEvent, and provider receipt.
Revocation publishes a retracted superseding workspace revision and invalidates
local caches. Blocking requires authenticated workspace policy, local opt-in,
and a real enforcer; membership alone is not instruction authority.

### Regulated mode

Require actor identity, reason, policy version, retained evidence, explicit
approval, and optional proposer/approver separation. Do not auto-archive
published policy.

### User interface

The notice should disclose the exact inferred statement, evidence count,
distinct task count, confidence, enforcement, current sharing scope, and the
effect of each action.

Example:

> I observed this in three separate tasks: you add an integration test whenever
> this route changes. I will treat it as an advisory rule for you. [Keep]
> [Edit] [Ignore]

### Gate

- Three distinct tasks can produce one eligible directive proposal.
- Repetition inside one task cannot.
- No inferred directive becomes blocking.
- No sharing scope broadens automatically.
- Keep, Edit, Ignore, and publication are replayable and auditable.
- A user-shared directive never appears in Git.
- Existing rule mining and guards remain compatible.

## 11. Phase 7 — Artifact contracts and completion truth

### Selection

Select a versioned `artifact_contract` during task triage using intent, scope,
validity, authority, and explicit user choice. Put the exact contract ID,
version, and content hash into `CompiledContextFrame` before execution.

### Validation

Run validators in this order:

1. deterministic filesystem, manifest, schema, dimension, and command checks;
2. externally verified checks;
3. semantic judges, labeled inferred.

A semantic judge cannot override a deterministic required failure. The worker
cannot pass validation by changing the selected contract because validation
uses the pre-execution ID, version, and hash.

Treat command requirements as untrusted data. Use structured argv, never an
implicit shell string. Require contract origin and execution_approval_ref;
resolve the ref to explicit current-user confirmation or authenticated
applicable organization policy. Unknown, inferred, unattested imported, or
unresolved contracts remain non-executable. Separately require ordinary Stella
tool authorization, artifact-root-contained cwd, sanitized environment,
timeout, output cap, normalized paths, and no network by default. Selecting or
exporting a contract never grants permission.

Require one result for every requirement in the bound contract version, with no
duplicates or unknown IDs. Any mismatch makes validation_status error and blocks
completion.

Emit immutable `contract_validation` and `outcome_assessment` records. A
required validation failure means Stella cannot claim the artifact is complete.
Populate `completion_assessment` and `correctness_assessment` independently;
never infer correctness merely because the output is complete or infer
completion merely because the produced portion is correct.

### Brand-kit witness

Create an end-to-end fixture where a user-scoped contract requires:

- editable SVG logo, wordmark, and mark;
- required PNG variants;
- favicons;
- design tokens matching a schema;
- brand guidelines with required sections;
- a file manifest and preview sheet;
- a stable directory structure.

Witness sequence:

1. The prompt asks only for a brand kit.
2. Stella retrieves the confirmed contract.
3. The first result omits `logos/wordmark.svg` and `social/og-image.png`.
4. Deterministic validation fails both requirements.
5. Stella does not report completion.
6. The failure creates one idempotent observation.
7. The repaired result passes all requirements.
8. A later task retrieves the same contract even when the prompt omits the
   checklist.

### Gate

- Required deterministic failures block completion claims.
- Contract and result hashes are inspectable.
- Validation replay is idempotent.
- Personal contracts remain user-shared unless explicitly published.
- Semantic judges are qualified as inferred.

## 12. Phase 8 — Repository publication and team transition

### Repository source of truth

Extend the existing `.stella/rules/*.md` format. Do not introduce YAML as a
second rule authority.

New rule frontmatter may contain:

```text
schema_version
record_id
lineage_id
record_kind
record_status
directive_kind
origin
scope.repository_id
sharing_scope
enforcement
confidence
observed_at
valid_from
valid_until
supporting_evidence_ids
record_hash
```

Do not include `promotion_stage`. Preserve legacy guard keys as readable
aliases and emit lowercase snake_case for generated files.

`supporting_evidence_ids` is the safe file projection of canonical
`evidence_links` whose relation is `supports`; keep all other typed relations in
`context.db`. Full private evidence remains there. Git contains the reviewable
statement, safe provenance references, and hashes needed to detect drift.

Define one normative rule-file-to-ContextRecord mapping. The Markdown body maps
to `statement`; presentational `name` and `description` are excluded from the
ContextRecord and record_hash and must not affect behavior. `record_hash` is
the canonical semantic record hash with that property omitted, never the whole
file hash. Store any source-file hash separately in `context.db`. When a manual
semantic edit no longer matches `record_hash`, validate it and create a new
immutable revision; never mutate the mirrored prior revision.

Creating a file, staging, committing, pushing, or opening a pull request are
separate actions. Each requires the authority already established by the user
or existing workflow.

### Solo-to-team transition

1. Treat multiple recent Git identities as a signal, not proof.
2. Ask before changing governance mode.
3. Keep user-shared records private.
4. Offer repository-applicable proposals for explicit publication.
5. Convert existing local evidence into proposals, not enforced team policy.
6. Enable owner routing only when maintainers or code owners resolve.

The transition changes policy, not record schema or identity.

### Gate

- Legacy rule files load unchanged.
- Generated rules round-trip through the current loader and guard engine.
- Personal evidence never enters Git.
- Adding a collaborator requires no database migration.
- Repository mutation never occurs merely because a proposal is eligible.

## 13. Phase 9 — Efficacy, staleness, and pruning

### Immutable use records

Use `context_use` to distinguish:

```text
selected
rendered
cited
```

Use `context_use_feedback` to record:

```text
helpful
not_helpful
neutral
```

Every feedback record identifies the exact `context_use`, evaluation method,
attribution confidence, opportunity, evaluator, and `observed_at`. An
unsuccessful task must not mark every selected record unhelpful.

Use one stable `use_trace_id` for the selected, rendered, and cited events for a
particular record/compiled-frame/invocation. Feedback carries both use_trace_id
and the exact context_use_id being evaluated so projections cannot double-count
stages.

### Derived efficacy

Rebuild projections for:

- opportunity count;
- selection, rendering, and citation counts;
- evaluated use count;
- helpful, not-helpful, and neutral counts;
- validation pass rate;
- contradictions;
- repair cost;
- last confirmed use;
- review due date;
- selection health.

Recommended internal `selection_health` values:

```text
healthy
review_due
stale
suppressed
```

These values are projections, not canonical record status.

### Initial reversible policy

An advisory inferred directive becomes eligible for suppression only when:

- it had a real opportunity to influence at least five evaluated uses;
- at least 80 percent of attributable evaluations are `not_helpful`;
- attribution confidence meets the configured threshold; and
- a confidence interval or Bayesian estimate rejects ordinary noise.

The first action is `selection_health = stale`, followed by exclusion from
automatic selection and a review notice. Archive only after a grace period.
Never apply this automatically to blocking, critical, organization, pinned, or
user-confirmed directives.

Physical deletion is a separate retention/privacy workflow. Archival is
reversible and retains provenance.

### Observatory

Add views for:

- frame lineage, selections, exclusions, and conflicts;
- evidence → observation → proposal → promoted record lineage;
- context use and efficacy;
- stale, review-due, and expiring records;
- contract selection and validation;
- provider contribution and latency;
- scope and sharing transitions.

### Gate

- Aggregates rebuild exactly from immutable records.
- Negative attribution requires a relevant opportunity and method.
- Stale records remain inspectable and restorable.
- Suppressed records are excluded from automatic retrieval.
- Safety and blocking directives are never withheld for experiments.

## 14. Phase 10 — Context Graph Exchange Protocol interoperability

Do this only after the local schema passes replay evaluation and a second
provider use case validates portability.

### Stella adapter

- Map protocol `ContextRecord` values into Stella domain types through an
  adapter boundary.
- Continue operating locally when lifecycle capability is absent.
- Preserve existing query-only providers.
- Check scope, sharing, and consent before dispatch.
- Never dispatch user-shared records solely because a provider can accept
  lifecycle writes.
- Keep remote append failure isolated from primary task success.
- Compile outbound records through `ContextExportManifest`; never expose a
  database-file sync API.
- Require an initial visible export decision and re-consent when destination,
  sharing boundary, data class, retention, or provider changes.
- Keep BYOK credentials, secrets, raw traces, journals, snapshots, and source
  code out of context-lifecycle exchange by default.
- Treat Oxagen as one optional provider. Do not hard-code Oxagen identity,
  endpoints, workspace semantics, or product policy into core record types.

### Expected portable capability

Consume only these mechanisms:

```text
context/query
context/records/append
context/records/get
context/resolve
```

The protocol append operation carries immutable record facts. Stella still
performs observation extraction, proposal scoring, promotion, validation,
enforcement, compaction policy, and pruning.

For append, send command-level `requested_retention` and persist per-item
receipt status, computed `record_hash`, receiver `accepted_at`, and
`accepted_retention`. Preserve the canonical origin `observed_at` and hash on
import; accepted_at is provider-ledger metadata, not a record rewrite. Map that
receipt time to the local ingestion ledger so Stella's `known_at` reconstruction
cannot make an imported record appear before receipt. Refuse export when a
provider cannot honor the requested retention and expiry behavior.

These mechanisms are export and provider retrieval only. Keep product-specific
Oxagen synchronization behind a separate adapter until a portable capability
defines cursors, ordered changes, acknowledgements, tombstones, conflicts,
deletion propagation, and offline replay.

### Gate

- Query-only provider witnesses remain unchanged.
- Unsupported lifecycle capability is nonfatal.
- Reference resolution verifies canonical content hash.
- Repeated append is idempotent.
- Partial failures return per-item receipts and do not lose successes.
- Sharing and consent rejections are covered by negative tests.
- Legacy temporal aliases are input-only.
- A non-Stella provider fixture validates that the wire schema is general.
- Export-manifest fixtures prove that raw local stores and secrets are never
  selected by default.

## 15. Crate ownership matrix

Adapt names if the repository has moved, but preserve the boundaries.

| Area | Primary responsibility |
| --- | --- |
| `stella-core` | Pure taxonomy, invariants, scoring inputs, governance decisions, contract result semantics |
| `stella-context` | `context.db`, temporal queries, record repositories, frame compilation, compaction metadata, health projections |
| `stella-store` | Raw executions, events, tool calls, journal replay inputs |
| `stella-pipeline` | Context assembly, contract selection, validation gating, outcome emission |
| `stella-protocol` | Internal additive events only; no external provider runtime or storage dependencies |
| `stella-graph` | Source-code graph retrieval only; no duplicate lifecycle graph |
| `stella-cli` or dedicated integration crate | Settings, diagnostics, proposal actions, explicit publication, and external provider adapters implementing `stella-context` ports |
| `stella-tui` | Lightweight notices, Keep/Edit/Ignore, evidence and sharing disclosure |
| `stella-observatory` | Frame lineage, efficacy, contract, scope, and promotion inspection |

## 16. Required witness matrix

| Witness | Expected result |
| --- | --- |
| Feature disabled | Existing Stella behavior is unchanged |
| Legacy context database | Transactional migration with no lost memories or facts |
| Historical knowledge | `known_at` excludes local records created later and imported records received later |
| Historical validity | `valid_at` excludes nonapplicable records |
| Validity overlap | Any intersection with `[from, until)` matches |
| One noisy task | Cannot satisfy a three-task threshold |
| Three API tasks | Produce one eligible advisory rule proposal |
| Blocking inference | Always requires explicit confirmation |
| User preference | Never enters Git automatically |
| Existing rule files | Load and guard behavior remain unchanged |
| Prompt injection in logs | Remains untrusted evidence/observation data |
| Git follow-up edit | Is inferred, not a verified error |
| Journal replay | Creates no duplicate observation or use records |
| Compact frame | Is smaller, citable, rehydratable, and preserves exact constraints |
| Brand-kit omission | Contract fails and completion cannot be claimed |
| Brand-kit repair | All required output passes and outcome is verified |
| Negative context outcome | Becomes stale only with attributable evidence |
| Solo-to-team transition | Requires no schema migration and leaks no user data |
| Query-only provider | Operates unchanged |
| Lifecycle provider | Passes consent, sharing, idempotency, alias, and partial-failure tests |

## 17. Verification and release gates

Run repository-documented checks plus, where applicable:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Add:

- migration fixtures for every supported schema;
- canonical serialization snapshots;
- property tests for temporal intervals and scope non-widening;
- replay and idempotency tests;
- SQLite integrity checks;
- deterministic frame and prompt goldens;
- a token-efficiency benchmark;
- privacy and sharing negative tests;
- rule-frontmatter compatibility tests;
- the brand-kit end-to-end witness;
- a query-only and lifecycle-provider conformance fixture.

Roll out through explicit configuration tuples; learning and governance are
independent dimensions:

```text
context.lifecycle.enabled = false

context.lifecycle.enabled = true
context.learning.mode = record_only
context.governance.mode = solo | team | regulated

context.lifecycle.enabled = true
context.learning.mode = advisory
context.governance.mode = solo | team | regulated
```

Team and regulated governance are deployment choices, not later learning
stages. Promotion and enforcement remain separately configured.

Rollback disables new selection, mining, or promotion. It never deletes
historical records.

## 18. Risk register

| Risk | Required mitigation |
| --- | --- |
| Memory and knowledge become two truth stores | Normalize into one context-record authority; treat indexes and rule mirrors as projections |
| Scope leakage | Central non-widening validator plus negative tests at storage, compiler, publication, and provider boundaries |
| Temporal semantics drift | One typed query API, half-open intervals, boundary truth tables |
| Compaction weakens a constraint | Per-item minimum fidelity, exact-content tests, canonical hash |
| User edits are called agent errors | Require deterministic, external, or explicit user evidence for verified conclusions |
| Generated prose poisons learning | Give observations no instruction authority and require independent evidence |
| One task manufactures recurrence | Count distinct task or episode IDs, not raw events |
| Rule formats diverge | Keep `.stella/rules/*.md` canonical for repository steering |
| Observation volume causes SQLite contention | Post-task batching, bounded excerpts, retention, measured indexes |
| Attribution punishes useful context | Require opportunity, method, and confidence; make suppression reversible |
| User confirmation becomes noisy | Batch notices and use advisory behavior before lightweight confirmation |
| Git identities trigger team mode incorrectly | Treat identity count as a signal and require user confirmation |
| Personal contracts leak | Default to user sharing and require explicit publication |
| New types create crate cycles | Pure core types, bounded internal event payloads, adapter boundaries |
| Deletion leaves derived private data | Track derivation edges and invalidate or rebuild projections |
| Protocol freezes Stella policy | Prove the lifecycle locally and keep governance host-side |
| Ambiguous project scope spreads | Omit `project_id` from core until a real registry exists |

## 19. Completion criteria

The implementation is complete only when:

- existing databases, rules, settings, provider queries, and code graph remain
  compatible;
- observations, evidence, knowledge, memories, directives, and contracts have
  unambiguous authority boundaries;
- every model-relevant durable record has stable identity, provenance, scope,
  sharing, and temporal meaning;
- every invocation has an inspectable deterministic `CompiledContextFrame`;
- `PromptContext` is compact without silently weakening exact semantics;
- extraction and replay are idempotent;
- inferred behavior cannot become blocking or shared automatically;
- artifact contracts prevent objectively incomplete completion claims;
- promotion and context-use histories are immutable and auditable;
- stale advisory context can be reversibly suppressed;
- solo-to-team migration changes policy, not schema;
- external lifecycle support remains optional; and
- no new account, server dependency, repository mutation, or phone-home
  behavior is introduced by default.
