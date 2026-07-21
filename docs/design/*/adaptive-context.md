# Adaptive Context Lifecycle

**Status:** Draft RFC
**Target repository:** macanderson/stella
**Related repository:** macanderson/context-graph-protocol
**Audience:** Stella maintainers, protocol maintainers, and extension authors

## 1. Executive summary

Stella should learn durable, user-specific and repository-specific steering from
its own work without requiring a solo developer to review a daily queue of
synthetic pull requests.

The proposed system turns traces, working logs, user corrections, verification
results, artifact validation, and Git diffs into typed observations. Repeated or
high-salience observations become candidate directives. Governance policy then
controls whether a candidate remains private, becomes an automatic advisory
rule, requires a lightweight confirmation, or is proposed through a team review
workflow.

The core lifecycle is:

~~~text
execution evidence
  -> observation
  -> candidate directive
  -> advisory or proposed directive
  -> confirmed and published steering
  -> cited use in a compiled context frame
  -> measured outcome
  -> confidence, expiry, supersession, or pruning
~~~

For a solo repository:

~~~text
observation -> personal draft -> automatic advisory rule -> confirmed rule
~~~

For a team repository:

~~~text
observation -> proposed Context PR -> owner approval -> published rule
~~~

A Context PR is a reviewable promotion of an observation into durable steering.
It does not have to be a GitHub pull request. In solo mode it can be an
automatic advisory rule plus a one-click Keep, Edit, or Ignore prompt. In team
mode, repository-scoped steering is naturally represented by an ordinary Git
change to a rule file.

The implementation belongs primarily in Stella. The context graph protocol
should expose only the portable wire mechanism needed by external providers:
typed records, time and provenance fields, capability negotiation, lifecycle
writeback, and conformance tests. Governance, thresholds, local storage, mining,
user experience, and enforcement remain Stella policy.

This split is the architectural rule:

> Mechanism in the protocol; policy in the host.

## 2. Decision

### 2.1 Build the feature in Stella first

The first shippable version must not depend on a new protocol release. Stella
already owns the execution trace, context database, rule engine, verifier, Git
workspace, TUI, and user relationship. It can implement the complete local
learning loop behind existing context provider interfaces.

The initial release should:

1. Extend Stella's local context schema and event vocabulary.
2. Compile a complete, inspectable context frame for each agent or tool call.
3. Mine observations from Stella-owned traces and outcomes.
4. Promote advisory rules according to solo, team, or regulated governance.
5. Publish repository steering through Stella's existing Markdown rule files.
6. Validate reusable artifact contracts and use their results as high-quality
   learning evidence.

### 2.2 Extend context-graph-protocol only for interoperability

The protocol extension is valuable when an external context provider needs to
read or write observations, directives, feedback, or validation results. It is
not required for Stella's local implementation.

Normal protocols should ship:

- a stable wire vocabulary;
- request, response, and error semantics;
- capability negotiation;
- compatibility rules;
- security and consent boundaries;
- examples, schemas, fixtures, and conformance tests.

Normal protocols should not ship:

- Stella's confidence algorithm;
- solo or team promotion thresholds;
- a particular database;
- a GitHub review workflow;
- TUI copy;
- product packaging;
- pruning policy defaults;
- organization-specific enforcement policy.

The proposed protocol work is therefore an optional lifecycle extension, not a
replacement for the existing retrieval protocol.

## 3. Goals

- Give solo developers durable learning with almost no maintenance burden.
- Preserve a clean migration from one developer to a team or regulated
  environment.
- Separate private personal preferences from shareable repository steering and
  inherited organization policy.
- Detect incomplete deliverables deterministically when an artifact contract
  exists.
- Detect likely inaccuracies using evidence, while clearly distinguishing
  verified failures from inferences.
- Mine candidate directives from traces, journals, tool calls, verification
  results, user corrections, and Git diffs.
- Track exactly which context items influenced an execution and whether their
  use was helpful.
- Expire, supersede, or archive low-value context without silently deleting
  policy or user intent.
- Keep raw telemetry and personal learning local by default.
- Preserve Stella's no-phone-home, ports-not-concretions, deterministic prompt,
  and witness-test invariants.
- Let external providers participate through context-graph-protocol without
  importing Stella's governance policy.

## 4. Non-goals

- Treating model-generated text as truth merely because the model produced it.
- Automatically creating blocking rules from inferred behavior.
- Uploading raw traces, personal preferences, or repository contents.
- Replacing Stella's existing rule engine.
- Replacing the code graph, execution store, journal, or verifier.
- Making every observation directly retrievable by the model.
- Claiming that semantic correctness can always be inferred without an oracle.
- Making context snapshots a source of truth.
- Requiring a GitHub account, cloud service, or server for individual use.

## 5. Existing Stella seams

This proposal extends current architecture rather than creating a parallel
system.

| Existing seam           | Current responsibility                                                                            | Proposed extension                                                                                       |
| ----------------------- | ------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| stella-context          | SQLite context plane, bi-temporal graph, memories, hybrid retrieval, citations, provider registry | Typed observations, directives, evidence, contracts, outcomes, lifecycle events, compiled context frames |
| stella-core::mining     | Pure clustering and stable candidate IDs                                                          | Typed extractors' normalized inputs, confidence, contradictions, scope inference                         |
| stella-core::rules      | Markdown rules, prompt and guarded tiers, candidate mining and promotion                          | Directive metadata, governance decisions, advisory state, efficacy and expiry decisions                  |
| stella-cli::rules       | Rule file I/O and store integration                                                               | Keep, Edit, Ignore, Share, archive, and Context PR orchestration                                         |
| stella-cli context host | Context provider host and workspace/code providers                                                | Lifecycle capability adapter and compiled-frame orchestration                                            |
| stella-store            | Execution records, tool calls, telemetry, rules, citations, reflections                           | Read-only evidence source for observation extraction; no duplicate context source of truth               |
| stella-store::journal   | Append-only session journal and replay                                                            | Trace cursoring, extraction checkpoints, and idempotent reprocessing                                     |
| stella-pipeline         | Plan, witness, execute, verify, and judge flow                                                    | Outcome assessment and artifact-contract validation events                                               |
| stella-protocol         | Stable internal AgentEvent vocabulary                                                             | Additive observation, promotion, frame, feedback, and validation events                                  |
| stella-graph            | Tree-sitter code graph and protocol frames                                                        | Code-scoped evidence and Git-diff anchors                                                                |
| stella-tui              | Interactive agent experience                                                                      | Lightweight learning notices and one-click actions                                                       |
| stella-observatory      | Execution inspection                                                                              | Context lineage, directive health, promotion, and contract dashboards                                    |

### 5.1 Canonical storage decision

Stella already uses:

- .stella/context.db for the context plane;
- .stella/store.db for execution and telemetry;
- .stella/codegraph.db for the source-code graph;
- .stella/rules/*.md for repository-authored durable rules;
- .stella/reflections.jsonl for a loose mining log.

The new design must preserve those boundaries.

**Decision:** confirmed repository rules remain canonical Markdown under
.stella/rules/*.md. Do not add .stella/context-rules.yaml as a second
authoritative rule format. A second format would create ambiguous precedence,
duplicate parsing and enforcement semantics, and drift between two Git-backed
sources.

Draft observations, candidates, feedback, evidence, and private directives live
in .stella/context.db. Published repository rules are materialized to
.stella/rules/*.md and mirrored into the context graph for retrieval. The file
remains the source of truth for published repository steering.

Suggested layout:

~~~text
.stella/
  context.db                 # local context graph and lifecycle records
  store.db                   # execution and telemetry evidence
  codegraph.db               # source-code graph
  settings.json              # governance and sharing configuration
  reflections.jsonl          # compatibility input to observation mining
  rules/
    api-integration-coverage.md
  context-snapshots/         # optional cached compilations, gitignored
~~~

Snapshots are cached explorations used to reduce token use and duplicate model
calls. They are disposable, versioned by their inputs, and never authoritative.

## 6. Terminology and invariants

### 6.1 Observation

An observation is a timestamped claim that something happened or was detected.
It is evidence for possible learning, not an instruction. Examples include a
user correcting the same omission, an integration test being added after every
route change, a contract validator finding a missing file, or a verifier
recording a fail-to-pass witness.

Observations are immutable. A correction creates another observation linked by
supersedes or contradicts.

### 6.2 Directive

A directive is a typed, durable unit that can influence behavior or
interpretation.

Directive kinds are:

| Kind       | Meaning                                            |
| ---------- | -------------------------------------------------- |
| memory     | Retained prior information relevant to future work |
| fact       | A scoped assertion believed to be true             |
| rule       | General behavioral steering                        |
| preference | Desired but non-mandatory behavior                 |
| constraint | A hard boundary or required condition              |
| procedure  | An ordered workflow                                |

End-user behavior is not a directive kind. It is observed first and may later
support a preference, rule, constraint, procedure, fact, or memory.

### 6.3 Memory

A memory is represented canonically as a directive whose kind is memory. The
compiled context frame may place memory directives in a separate memories
section for readability, but it must retain the same directive IDs. There must
not be two independently editable sources of truth called memory and directive.

### 6.4 Evidence

Evidence is an immutable, addressable source that supports or challenges an
observation, directive, validation, or outcome assessment. Evidence includes
trace events, file spans, Git hunks, tool results, user feedback, validator
results, and policy documents.

### 6.5 Artifact contract

An artifact contract is a reusable, versioned, machine-checkable definition of
an acceptable deliverable. It captures the output shape the user otherwise has
to remember to repeat in every prompt.

### 6.6 Context frame

A compiled context frame is the bounded, task-specific package Stella supplies
to an agent or tool invocation. It contains selected task state, directives,
memories, observation summaries, code-map fragments, artifact contracts, and
evidence references plus a manifest explaining selection and conflict
resolution.

The external context-graph-protocol currently uses ContextFrame for an atomic
retrieved fragment. Stella's aggregate type should therefore be named
CompiledContextFrame in code to avoid a semantic collision. Existing protocol
ContextFrame remains compatible.

### 6.7 Context PR

A Context PR is a reviewable promotion object. In solo mode it can be an
in-product prompt. In team mode it can produce an ordinary Git change. In
regulated mode it can route through an accountable approval record.

## 7. Common schema rules

All serialized property names must be lowercase snake_case.

All durable records must include:

- schema_version;
- a stable, opaque ID;
- scope and sharing scope;
- provenance or evidence links;
- observed_at;
- valid_from;
- valid_until when applicability ends;
- status;
- a content hash or idempotency key where records can be replayed.

Time semantics:

- observed_at is when Stella learned or recorded the claim;
- valid_from is when the claim became applicable in the world;
- valid_until is the exclusive end of its applicability;
- an absent valid_from means valid from observed_at;
- an absent valid_until means no known end;
- query cutoffs use as_of_observed_at and as_of_valid_at.

The names transaction_time, valid_time, recorded_at, valid_to, camelCase
variants, and mixed-case properties are not canonical in the new schema.
Compatibility readers may accept them during migration, but writers emit only
observed_at, valid_from, and valid_until.

Confidence is an integer from 0 through 100. This matches governance
configuration and avoids ambiguity between 0.85 and 85.

~~~json
{
  "schema_version": "1.0-draft",
  "scope": {
    "user_id": "usr_01",
    "repository_id": "repo_stella",
    "workspace_id": "wrk_01"
  },
  "sharing_scope": "repository",
  "observed_at": "2026-07-20T18:00:00Z",
  "valid_from": "2026-07-20T18:00:00Z",
  "valid_until": null
}
~~~

## 8. Record schemas

The following JSON shapes define the logical contract. Rust types use
serde rename_all snake_case and must round-trip through serde_json.

### 8.1 Scope

~~~json
{
  "user_id": "usr_01",
  "organization_id": "org_01",
  "workspace_id": "wrk_01",
  "project_id": "prj_01",
  "repository_id": "repo_stella",
  "environment_id": "env_local",
  "session_id": "ses_01",
  "task_id": "task_01"
}
~~~

Every field is optional, but an unscoped inferred directive is invalid. A
directive may only inherit into a broader scope through an explicit promotion.

### 8.2 Observation

~~~json
{
  "schema_version": "1.0-draft",
  "observation_id": "obs_01j2brand_missing",
  "observation_kind": "artifact_contract_failure",
  "actor_ref": "agent_default",
  "subject_ref": "contract_brand_kit_v3",
  "predicate": "missing_required_artifact",
  "object": {
    "path": "logos/wordmark.svg",
    "requirement_id": "brand_wordmark_svg"
  },
  "source_kind": "artifact_validator",
  "source_ref": "validation_01",
  "evidence_ids": [
    "ev_manifest_01",
    "ev_workspace_01"
  ],
  "scope": {
    "user_id": "usr_01",
    "repository_id": "repo_brand"
  },
  "sharing_scope": "personal",
  "sensitivity": "private",
  "confidence": 100,
  "status": "active",
  "observed_at": "2026-07-20T18:10:00Z",
  "valid_from": "2026-07-20T18:10:00Z",
  "valid_until": null,
  "idempotency_key": "sha256:..."
}
~~~

Observation kinds are open and namespaced. Stella initially defines:

- user_correction;
- user_acceptance;
- user_rejection;
- repeated_action;
- repeated_omission;
- tool_failure;
- tool_recovery;
- verification_pass;
- verification_failure;
- artifact_contract_pass;
- artifact_contract_failure;
- git_followup_change;
- rule_conflict;
- context_helpful;
- context_not_helpful.

Raw observations never gain instruction authority. They may appear in a context
frame only as clearly labeled evidence or an aggregate summary.

### 8.3 Directive

~~~json
{
  "schema_version": "1.0-draft",
  "directive_id": "dir_api_integration_coverage",
  "kind": "rule",
  "statement": "API endpoint changes require integration coverage.",
  "value": {
    "when": {
      "changed_paths": [
        "src/api/**"
      ]
    },
    "required_action": "add_or_update_integration_test"
  },
  "subject_refs": [
    "repo_stella"
  ],
  "priority": "high",
  "confidence": 91,
  "source": "inferred",
  "status": "active",
  "promotion_stage": "advisory",
  "enforcement": "advisory",
  "scope": {
    "repository_id": "repo_stella"
  },
  "sharing_scope": "repository",
  "evidence_ids": [
    "ev_task_101",
    "ev_task_118",
    "ev_task_144"
  ],
  "supersedes": [],
  "observed_at": "2026-07-20T18:30:00Z",
  "valid_from": "2026-07-20T18:30:00Z",
  "valid_until": null,
  "expires_at": "2027-01-16T18:30:00Z",
  "created_at": "2026-07-20T18:30:00Z",
  "updated_at": "2026-07-20T18:30:00Z"
}
~~~

Allowed values:

- kind: `memory`, `fact`, `rule`, `preference`, `constraint`, `procedure`;
- priority: `low`, `normal`, `high`, `critical`;
- source: `user`, `system`, `inferred`, `imported`;
- status: `active`, `stale`, `superseded`, `archived`, `rejected`;
- promotion_stage: `draft`, `proposed`, `advisory`, `confirmed`, `published`;
- enforcement: `informational`, `advisory`, `blocking`;
- sharing_scope: `personal`, `repository`, `organization`.

Rules:

1. Inferred directives start as draft or advisory.
2. Inferred directives may not become blocking without explicit confirmation.
3. A personal directive may not be written to the repository.
4. A repository directive may not be promoted to organization scope
   automatically.
5. A critical, system, organization, or blocking directive may not be
   auto-archived.
6. Supersession preserves the prior record and its evidence.
7. A fact or memory can expire or become stale without being judged unhelpful.
8. A constraint must describe what it constrains and how enforcement occurs.
9. A procedure should carry structured ordered steps when deterministic
   validation is possible.

### 8.4 Evidence

~~~json
{
  "schema_version": "1.0-draft",
  "evidence_id": "ev_git_01",
  "source_kind": "git_diff",
  "source_ref": "git:4ac2f9e",
  "locator": {
    "path": "tests/api_routes.rs",
    "line_start": 82,
    "line_end": 119,
    "commit": "4ac2f9e"
  },
  "content_hash": "sha256:...",
  "excerpt": "Adds an integration test for the changed route.",
  "trust": "workspace_artifact",
  "sensitivity": "repository",
  "retention": "durable",
  "scope": {
    "repository_id": "repo_stella"
  },
  "observed_at": "2026-07-20T18:20:00Z",
  "valid_from": "2026-07-20T18:20:00Z",
  "valid_until": null
}
~~~

Evidence excerpts are bounded. Large payloads remain at their source and are
addressed by locator and content_hash. Secrets are redacted before persistence.

### 8.5 Candidate directive

~~~json
{
  "schema_version": "1.0-draft",
  "candidate_id": "cand_api_integration_coverage",
  "proposed_directive": {
    "kind": "rule",
    "statement": "API endpoint changes require integration coverage.",
    "scope": {
      "repository_id": "repo_stella"
    },
    "sharing_scope": "repository",
    "enforcement": "advisory"
  },
  "observation_ids": [
    "obs_101",
    "obs_118",
    "obs_144"
  ],
  "distinct_task_count": 3,
  "supporting_count": 3,
  "contradicting_count": 0,
  "confidence": 91,
  "status": "eligible",
  "created_at": "2026-07-20T18:30:00Z",
  "expires_at": "2026-08-19T18:30:00Z"
}
~~~

Repeated events from one model turn do not count as independent observations.
The promotion threshold uses distinct tasks or episodes unless a single event
has explicit user confirmation or deterministic high-salience evidence.

### 8.6 Citation event and health aggregate

Citation statistics are derived from immutable events, not incremented as the
sole source of truth.

~~~json
{
  "schema_version": "1.0-draft",
  "citation_event_id": "cite_01",
  "frame_id": "frame_01",
  "record_id": "dir_api_integration_coverage",
  "task_id": "task_200",
  "invocation_id": "inv_04",
  "selection_reason": "changed_paths matched src/api/**",
  "evaluation": "helpful",
  "evaluation_method": "contract_and_user_acceptance",
  "attribution_confidence": 87,
  "observed_at": "2026-07-20T19:00:00Z"
}
~~~

~~~json
{
  "record_id": "dir_api_integration_coverage",
  "cited_count": 12,
  "evaluated_count": 9,
  "helpful_count": 8,
  "not_helpful_count": 1,
  "neutral_count": 3,
  "last_cited_at": "2026-07-20T19:00:00Z",
  "last_helpful_at": "2026-07-20T19:00:00Z",
  "last_not_helpful_at": "2026-07-11T15:00:00Z"
}
~~~

An unsuccessful task is not enough to mark every cited directive unhelpful.
Evaluation requires an attribution method and an opportunity for the directive
to influence the relevant outcome.

### 8.7 Artifact contract

~~~json
{
  "schema_version": "1.0-draft",
  "contract_id": "contract_brand_kit_v3",
  "name": "brand_kit",
  "version": 3,
  "description": "Complete reusable brand kit deliverable.",
  "scope": {
    "user_id": "usr_01"
  },
  "sharing_scope": "personal",
  "applies_when": {
    "task_intents": [
      "create_brand_kit",
      "refresh_brand_assets"
    ]
  },
  "output_root": "brand/",
  "requirements": [
    {
      "requirement_id": "brand_readme",
      "kind": "file_exists",
      "path": "README.md",
      "required": true
    },
    {
      "requirement_id": "brand_logo_svg",
      "kind": "file_exists",
      "path": "logos/logo.svg",
      "required": true
    },
    {
      "requirement_id": "brand_wordmark_svg",
      "kind": "file_exists",
      "path": "logos/wordmark.svg",
      "required": true
    },
    {
      "requirement_id": "brand_mark_svg",
      "kind": "file_exists",
      "path": "logos/mark.svg",
      "required": true
    },
    {
      "requirement_id": "brand_png_variants",
      "kind": "glob_min_count",
      "glob": "logos/png/**/*.png",
      "minimum": 6,
      "required": true
    },
    {
      "requirement_id": "brand_favicons",
      "kind": "glob_min_count",
      "glob": "favicons/*",
      "minimum": 4,
      "required": true
    },
    {
      "requirement_id": "brand_tokens",
      "kind": "json_schema",
      "path": "tokens/brand.tokens.json",
      "schema_ref": "stella://contracts/design-tokens/v1",
      "required": true
    },
    {
      "requirement_id": "brand_guidelines",
      "kind": "markdown_sections",
      "path": "BRAND_GUIDELINES.md",
      "sections": [
        "Logo usage",
        "Color",
        "Typography",
        "Spacing",
        "Accessibility",
        "File inventory"
      ],
      "required": true
    }
  ],
  "presentation": {
    "include_manifest": true,
    "include_preview_sheet": true,
    "directory_order": [
      "logos",
      "favicons",
      "social",
      "tokens",
      "templates"
    ]
  },
  "observed_at": "2026-07-20T17:00:00Z",
  "valid_from": "2026-07-20T17:00:00Z",
  "valid_until": null,
  "status": "active"
}
~~~

Requirement kinds initially include:

- file_exists;
- directory_exists;
- glob_min_count;
- mime_type;
- image_dimensions;
- file_size;
- json_schema;
- markdown_sections;
- command;
- semantic_judge.

Deterministic validators run first. Semantic judgment is explicitly weaker
evidence and must include model identity, prompt version, confidence, and input
hash.

### 8.8 Contract validation

~~~json
{
  "schema_version": "1.0-draft",
  "validation_id": "validation_01",
  "contract_id": "contract_brand_kit_v3",
  "contract_version": 3,
  "task_id": "task_brand_21",
  "artifact_root": "brand/",
  "status": "failed",
  "results": [
    {
      "requirement_id": "brand_logo_svg",
      "status": "passed",
      "method": "deterministic"
    },
    {
      "requirement_id": "brand_wordmark_svg",
      "status": "failed",
      "method": "deterministic",
      "message": "logos/wordmark.svg was not found"
    }
  ],
  "evidence_ids": [
    "ev_manifest_01"
  ],
  "observed_at": "2026-07-20T18:10:00Z"
}
~~~

### 8.9 Outcome assessment

~~~json
{
  "schema_version": "1.0-draft",
  "outcome_id": "outcome_task_brand_21",
  "task_id": "task_brand_21",
  "status": "incomplete",
  "assessment_level": "verified",
  "reasons": [
    {
      "kind": "contract_failure",
      "ref": "validation_01"
    }
  ],
  "user_feedback": null,
  "final_commit": null,
  "observed_at": "2026-07-20T18:10:00Z"
}
~~~

Assessment levels are:

- verified: deterministic test, validator, or explicit policy check;
- user_confirmed: explicit user acceptance, rejection, or correction;
- externally_confirmed: trusted CI, review, or external system result;
- inferred: behavioral or semantic signal without a definitive oracle;
- unknown: no reliable conclusion.

Stella may say a response was incomplete when a required contract item failed.
It may say a response was inaccurate only when a trusted fact check, test,
validator, user correction, or equivalent oracle supports that conclusion.
A later Git edit alone means the output changed; it does not prove the original
was wrong.

## 9. Compiled context frame

### 9.1 Logical schema

~~~json
{
  "schema_version": "1.0-draft",
  "frame_id": "frame_01",
  "task": {
    "task_id": "task_brand_22",
    "goal": "Create a complete brand kit",
    "success_criteria": [
      "All required contract checks pass"
    ]
  },
  "state": {
    "phase": "execute",
    "plan_step": 3,
    "open_questions": [],
    "recent_event_refs": [
      "event_201"
    ]
  },
  "scope": {
    "user_id": "usr_01",
    "repository_id": "repo_brand",
    "session_id": "ses_22",
    "task_id": "task_brand_22"
  },
  "as_of_observed_at": "2026-07-20T20:00:00Z",
  "as_of_valid_at": "2026-07-20T20:00:00Z",
  "directives": [
    {
      "directive_id": "dir_brand_output_shape",
      "kind": "procedure",
      "statement": "Generate and verify every required brand-kit artifact.",
      "citation_label": "Personal procedure: brand-kit output",
      "selection_reason": "intent matched create_brand_kit",
      "token_cost": 32
    }
  ],
  "memories": [
    {
      "directive_id": "dir_mem_brand_svg",
      "kind": "memory",
      "statement": "The user expects editable SVG masters.",
      "citation_label": "Brand-kit memory: editable masters",
      "selection_reason": "brand contract and user scope matched",
      "token_cost": 16
    }
  ],
  "observation_summaries": [],
  "artifact_contracts": [
    {
      "contract_id": "contract_brand_kit_v3",
      "version": 3,
      "selection_reason": "intent matched create_brand_kit"
    }
  ],
  "code_map": {
    "nodes": [],
    "edges": [],
    "root_refs": []
  },
  "evidence": [
    {
      "evidence_id": "ev_user_confirmation_09",
      "citation_label": "User-confirmed brand-kit requirements"
    }
  ],
  "manifest": {
    "compiler_version": "stella-context-frame/1.0-draft",
    "input_hash": "sha256:...",
    "budget": {
      "max_tokens": 8000,
      "used_tokens": 5240
    },
    "included_record_ids": [
      "dir_brand_output_shape",
      "dir_mem_brand_svg",
      "contract_brand_kit_v3"
    ],
    "excluded": [
      {
        "record_id": "dir_old_brand_shape",
        "reason": "superseded"
      }
    ],
    "conflicts": [],
    "snapshot_ref": null
  },
  "compiled_at": "2026-07-20T20:00:00Z"
}
~~~

### 9.2 Frame assembly pipeline

1. Resolve `actor`, `task`, `repository`, `workspace`, `organization`, and `environment`.
2. Resolve authorization and inherited organization policy.
3. Load current task state and applicable artifact contracts.
4. Select directives whose scope matches and whose temporal interval contains
   as_of_valid_at, considering only records visible by as_of_observed_at.
5. Split memory directives into the memories presentation section.
6. Retrieve the smallest relevant code-map subgraph.
7. Retrieve evidence-backed memories and facts through the existing hybrid
   vector, recency, graph, and budget pipeline.
8. Summarize observations only when the summary is useful and safe. Never
   promote raw behavior to instruction authority during compilation.
9. Resolve conflicts deterministically and record the decision.
10. Deduplicate, diversify, compress, and pack to budget.
11. Emit a byte-stable manifest and citation labels.
12. Record citation events after the invocation and attach outcome evidence
    when it becomes available.

### 9.3 Precedence and conflicts

Default precedence, highest first:

1. authorization boundaries and non-overridable organization policy;
2. confirmed blocking constraints and guarded rules;
3. explicit instructions in the current user task;
4. applicable artifact contracts and confirmed procedures;
5. confirmed repository rules;
6. confirmed personal preferences and memories;
7. advisory inferred directives;
8. observations and untrusted evidence.

Specific scope wins over broad scope at the same authority, newer valid
information wins only when it explicitly supersedes older information, and a
preference never overrides a constraint.

Conflicts are data. The compiler records both items, the selected item, the
reason, and whether human resolution is required. It never silently merges
incompatible rules.

### 9.4 Trust and prompt injection

Source code, issue text, logs, web content, diffs, and imported documents are
untrusted evidence. They have no instruction capability merely because they
contain imperative language. The frame renderer must separate policy and
evidence sections and label trust boundaries.

## 10. Observation farming

### 10.1 Sources

Stella should extract observations from:

- stella-protocol AgentEvent streams;
- .stella session journal JSONL and replay records;
- .stella/reflections.jsonl;
- tool calls, failures, retries, and recovery actions;
- verification and witness-test results;
- judge evidence;
- artifact-contract validation;
- explicit user corrections, acceptance, rejection, Keep, Edit, and Ignore;
- Git working-tree diffs, staged diffs, commits, and follow-up edits;
- repeated file organization and naming patterns;
- rule conflicts and guard denials;
- context citation feedback.

### 10.2 Extractor contract

Each extractor is an adapter. It accepts its source record and emits zero or
more normalized observations. It must:

- preserve a source cursor and evidence locator;
- generate an idempotency key;
- redact secrets before persistence;
- assign sensitivity and initial sharing scope;
- distinguish direct evidence from model inference;
- avoid turning agent prose into a directive;
- use observed_at, valid_from, and valid_until;
- remain replayable and deterministic where possible.

Decision logic belongs in stella-core. File, database, Git, and process I/O
belongs in stella-cli, stella-store, stella-tools, or a dedicated adapter.

### 10.3 Git diff mining

Git diffs are especially valuable when they compare the agent's output with the
accepted repository state.

The miner should derive features such as:

- files the user added after the agent stopped;
- tests added alongside repeated categories of code changes;
- files repeatedly renamed or reorganized;
- formatting or asset variants repeatedly restored;
- generated output removed before acceptance;
- guardrail files the user repeatedly protects;
- final commit shape compared with the agent's last patch.

Interpretation remains conservative:

- a follow-up edit is an observation, not proof of error;
- repeated edits across distinct tasks can support a candidate preference or
  procedure;
- a deterministic test or contract failure can support a verified omission;
- changes unrelated to the agent's task are excluded;
- merge commits, formatting-only churn, generated files, and vendor files are
  filtered by default;
- the base and head commits are stored in evidence.

### 10.4 Candidate induction

The existing lexical clustering and stable-ID machinery in
stella-core::mining and stella-core::rules should be extended, not replaced.

Initial candidate scoring should consider:

- distinct tasks or episodes;
- recency;
- explicit user confirmation;
- deterministic validation strength;
- user effort required to repair the result;
- consistency across observations;
- contradictions;
- scope confidence;
- likely future applicability;
- sensitivity and sharing constraints.

An illustrative score is:

~~~text
support
  = distinct_task_weight
  + explicit_feedback_weight
  + deterministic_evidence_weight
  + recency_weight
  + repair_cost_weight
  - contradiction_weight
  - ambiguity_weight
  - staleness_weight
~~~

The exact formula is Stella policy and must be versioned. Store the score
components so changes are explainable and old candidates can be recomputed.

### 10.5 Anti-poisoning requirements

- Never promote solely from model-authored prose.
- Require independent evidence: user action, deterministic outcome, trusted
  source, or repetition across distinct tasks.
- Do not count a generated directive as evidence for itself.
- Treat imported repository content as untrusted until provenance and scope
  checks pass.
- Detect contradictory observations before promotion.
- Prevent one runaway task from manufacturing the minimum occurrence count.
- Rate-limit candidate creation.
- Never auto-promote an inferred guard or blocking constraint.
- Retain the evidence chain for every promoted directive.

## 11. Adaptive governance

### 11.1 Configuration

Use Stella's existing .stella/settings.json scope chain. Do not introduce a
parallel config.yaml.

~~~json
{
  "context": {
    "governance": {
      "mode": "solo"
    },
    "promotion": {
      "inferred_rule": {
        "min_observations": 3,
        "min_distinct_tasks": 3,
        "auto_publish_at_confidence": 85,
        "initial_enforcement": "advisory"
      },
      "blocking_rule": {
        "requires_explicit_confirmation": true
      }
    },
    "retention": {
      "raw_observation_days": 30,
      "candidate_days": 30,
      "stale_directive_days": 180
    }
  }
}
~~~

Allowed governance modes are solo, team, and regulated.

### 11.2 Solo mode

The default ladder is:

~~~text
observation -> personal draft -> automatic advisory rule -> confirmed rule
~~~

When a candidate reaches the configured thresholds, Stella may use it
immediately as advisory steering and show:

> I have observed this three times: you add an integration test whenever this
> route changes. I will treat it as an advisory preference going forward.
> [Keep] [Edit] [Ignore]

Actions:

- Keep confirms the directive and extends its retention.
- Edit creates a superseding user-authored directive and preserves the
  candidate evidence.
- Ignore rejects the candidate, prevents immediate re-proposal, and records
  negative mining evidence.

No inferred item becomes blocking in solo mode without explicit confirmation.

### 11.3 Team mode

The default ladder is:

~~~text
observation -> proposed Context PR -> owner approval -> published rule
~~~

Repository-scoped proposals materialize as changes under .stella/rules/*.md.
Normal Git review supplies authorship, diff, ownership, discussion, and audit
history. Personal observations and directives are excluded from the change.

Owner routing is enabled only when maintainers or code owners can be resolved.
Until then, proposals remain advisory and unowned.

### 11.4 Regulated mode

Regulated mode requires:

- explicit approval for repository and organization promotion;
- immutable promotion and enforcement events;
- actor identity and reason;
- separation of proposer and approver when configured;
- policy version and evidence retention;
- no automatic archival of published policy;
- exportable audit history;
- signed or content-addressed published artifacts where required.

### 11.5 Sharing scopes

| Scope        | Example                               | Default sharing                                     |
| ------------ | ------------------------------------- | --------------------------------------------------- |
| personal     | Prefer concise status updates         | Never shared automatically                          |
| repository   | Run integration tests for API changes | Shared only through explicit repository publication |
| organization | Never expose PII in logs              | Inherited from approved organization policy         |

Personal records remain private when collaborators join. Repository candidates
do not become shared merely because they were inferred in a repository.
Promotion is the sharing boundary.

### 11.6 Solo-to-team migration

When Stella detects more than one active repository identity:

1. Offer to switch governance mode; do not switch silently.
2. List local repository-scoped directives eligible to share.
3. Keep personal directives private.
4. Convert local evidence into proposals, not enforced team policy.
5. Materialize selected published rules through ordinary Git changes.
6. Enable owner routing only when maintainers or code owners exist.

No data-model migration is required. Only governance and sharing decisions
change.

## 12. Published rule format

Repository steering continues to use .stella/rules/*.md. Existing files without
new metadata remain valid.

<!-- Example frontmatter for a rule.md file -->
~~~yaml
---
name: api-integration-coverage
description: Require integration coverage for API endpoint changes
kind: rule
sharing_scope: repository
promotion_stage: published
enforcement: advisory
confidence: 91
observed_at: 2026-07-20T18:30:00Z
valid_from: 2026-07-20T18:30:00Z
evidence_ids:
  - ev_task_101
  - ev_task_118
  - ev_task_144
---

API endpoint changes require integration coverage.
~~~

New frontmatter keys are lowercase snake_case. Existing hyphenated guard keys
remain readable for compatibility. Stella should add canonical snake_case
aliases such as guard_tool, guard_deny_path, and guard_deny_command and emit
only the canonical form for newly generated files after the parser supports it.

The file body remains the human-readable rule statement. Complex values and
full evidence stay in context.db; the Git file contains stable IDs and reviewable
metadata, not raw private telemetry.

## 13. Lifecycle, efficacy, and pruning

### 13.1 Citation tracking

Every included directive, memory, contract, or evidence item produces a
frame-item record. Each later feedback event references the exact frame,
invocation, task, and record.

Useful evaluation methods include:

- explicit user Keep or acceptance;
- explicit user correction, rejection, or Ignore;
- deterministic contract pass or failure tied to the directive;
- witness or verification outcome;
- guard prevented a prohibited action;
- final accepted Git state satisfied the directive;
- semantic judge assessment, marked inferred;
- matched historical control, marked inferred.

### 13.2 Efficacy

The most defensible differentiator is not merely remembering rules; it is
measuring whether a cited rule improved an independently observable result.

For each directive, track:

- opportunities;
- citations;
- evaluated citations;
- helpful, not helpful, and neutral outcomes;
- attribution confidence;
- task acceptance;
- validation pass rate;
- repair cost;
- contradictions;
- time since last confirmed use.

Where safe, compare similar tasks with and without an advisory directive. This
can be an observational matched control rather than randomized withholding.
Blocking or safety directives are never withheld for experimentation.

### 13.3 Staleness and expiry

- Raw observations expire according to sensitivity and policy.
- Candidate directives expire when not promoted.
- Inferred advisory directives receive an expiry by default.
- User-confirmed directives do not expire merely because they were not cited.
- Facts and memories can become stale when their valid interval or source
  changes.
- Superseded records remain queryable for audit and as-of reconstruction.

### 13.4 Automatic archival

Default advisory policy:

~~~text
eligible when:
  evaluated_count >= 5
  and not_helpful_count / evaluated_count >= 0.80
  and attribution confidence is sufficient

action:
  mark stale
  stop automatic retrieval
  retain for a grace period
  archive after the grace period unless reaffirmed
~~~

Use a confidence interval or Bayesian estimate once sample sizes justify it;
the raw ratio is only the initial policy.

Never automatically archive:

- system directives;
- critical directives;
- blocking directives;
- organization policy;
- explicitly pinned directives;
- records under legal or audit hold.

Archival is reversible. Physical deletion follows separate retention, privacy,
or user-deletion policy.

## 14. Detecting inaccurate or incomplete work

The proposed system can reliably detect some incomplete responses and can
produce calibrated evidence about likely inaccuracies. It cannot infer all
correctness from behavior alone.

### 14.1 High-confidence detection

Stella can label an outcome verified incomplete or incorrect when:

- a required artifact-contract check fails;
- a witness test or deterministic verification fails;
- a schema, type, lint, or policy check fails;
- the user explicitly identifies the error or missing item;
- a trusted external result contradicts the output;
- required files listed in an approved contract are absent;
- a generated artifact manifest does not match the workspace.

### 14.2 Lower-confidence signals

The following support inferred, not verified, assessments:

- the user substantially rewrites the result;
- the same missing artifact is added after multiple tasks;
- the user repeatedly asks for a redo;
- a semantic judge flags inconsistency;
- a later commit changes an agent-authored fact;
- the agent retries the same tool path repeatedly.

### 14.3 Brand-kit outcome

With a confirmed brand-kit contract, Stella can:

1. Match the task to the contract.
2. Put the exact required output shape in the context frame.
3. Validate every required path and machine-checkable property.
4. Refuse to call the task complete while required checks fail.
5. Record each omission as an observation.
6. Promote recurring user-specific requirements into the contract or a
   supporting directive.
7. Reuse the same versioned contract next time even when the prompt omits the
   full checklist.

This solves the recurrent output-shape problem much more reliably than memory
retrieval alone. Memory reminds the model; the contract verifies the result.

## 15. Storage and migration

### 15.1 Context database

Add normalized lifecycle tables to .stella/context.db and mirror only
retrievable summaries into the existing node and edge graph.

Recommended canonical tables:

- observation;
- directive;
- directive_evidence;
- candidate_directive;
- candidate_observation;
- citation_event;
- promotion_event;
- artifact_contract;
- contract_validation;
- outcome_assessment;
- compiled_context_frame;
- compiled_context_frame_item;
- extraction_cursor.

Do not put high-cardinality raw observations into graph nodes by default. Index
active directives, memory views, contracts, evidence summaries, and promoted
observation summaries.

### 15.2 Time-column migration

The current context schema uses recorded_at and valid_to in several graph
tables. Migrate context-plane temporal columns to:

- observed_at;
- valid_from;
- valid_until.

Migration requirements:

1. Preserve all existing values.
2. Rebuild affected indexes and foreign keys transactionally.
3. Update queries and Rust rows in the same schema version.
4. Accept legacy serialized aliases at protocol boundaries during the draft
   transition.
5. Emit only canonical names.
6. Add as-of tests covering records learned after the requested observation
   cutoff and facts invalid at the requested validity cutoff.

This naming change applies to contextual claim semantics. Execution audit
tables may retain created_at, started_at, ended_at, or event_at when those names
describe different concepts.

### 15.3 Execution store boundary

stella-store remains the source for executions, events, tool calls, reflections,
and operational telemetry. Observation extractors read it through a port and
persist normalized learning records to context.db.

Do not duplicate raw execution tables in context.db. Store source references,
cursors, and hashes sufficient for idempotent replay.

### 15.4 Concurrency and replay

- Extraction uses monotonic per-source cursors.
- Every emitted observation has an idempotency key.
- Replaying a journal creates no duplicate observation or citation.
- Context writes use transactions and existing WAL behavior.
- A frame manifest is immutable after compilation.
- Late outcomes append feedback; they do not mutate the historical frame.

## 16. Stella implementation plan

### 16.1 stella-core

Add or extend pure types and decision logic:

- observation normalization;
- directive and candidate types;
- scope and sharing checks;
- governance policy;
- promotion ladder;
- contradiction resolution;
- confidence components;
- lifecycle and pruning decisions;
- artifact-contract matching;
- outcome attribution;
- deterministic frame conflict ordering.

Extend existing rule mining instead of creating a new miner. Existing rule
loading and guard behavior remain backwards compatible.

No filesystem, SQLite, Git, process, terminal, or network I/O is allowed here.

### 16.2 stella-context

Own the context lifecycle:

- database migrations and typed repositories;
- active-record and as-of queries;
- evidence and provenance links;
- contract storage and validation-result storage;
- frame compilation;
- citation events and aggregates;
- retrieval mirrors;
- provider lifecycle adapter;
- pruning and expiry execution through injected clock and policy.

Keep the current hybrid retrieval, citation labels, token budget, MMR, and
provider registry. Add record-type diversity and instruction-capability labels.

### 16.3 stella-cli

Own workspace integration:

- mount .stella/context.db;
- extract from journal, store, reflections, and Git;
- run incremental observation harvesting after safe task boundaries;
- expose context status, list, explain, keep, edit, ignore, share, archive, and
  validate commands;
- materialize published repository rules to .stella/rules/*.md;
- create a normal Git change for team Context PRs;
- adapt context-graph-protocol lifecycle capabilities when present.

Suggested command surface:

~~~text
stella context status
stella context observations
stella context candidates
stella context explain <id>
stella context keep <id>
stella context edit <id>
stella context ignore <id>
stella context share <id> --scope repository
stella context archive <id>
stella context validate [contract_id]
stella context frame <task_or_invocation_id>
stella context harvest [--from <cursor>]
~~~

Commands that mutate Git require an explicit user action in team mode. Local
observation harvesting is best-effort and never blocks the primary task.

### 16.4 stella-protocol

Add additive internal events:

- ObservationRecorded;
- CandidateDirectiveCreated;
- DirectivePromoted;
- DirectiveSuperseded;
- DirectiveFeedbackRecorded;
- ContextFrameCompiled;
- ArtifactContractMatched;
- ContractValidationCompleted;
- OutcomeAssessed.

Every new event must round-trip byte-for-byte through serde_json and carry
stable IDs rather than embedding unbounded payloads.

### 16.5 stella-pipeline

- Resolve applicable artifact contracts during triage or planning.
- Render requirements into the execution context.
- Run deterministic contract validation before semantic judging.
- Include contract failure in the definition of not done.
- Emit verified or inferred outcome assessments.
- Feed witness, verify, judge, and candidate results to observation extractors.
- Ensure the worker cannot satisfy a contract by modifying its definition.

### 16.6 stella-graph

- Attach symbol, file, and change anchors to observations.
- Expose the smallest relevant subgraph to the frame compiler.
- Help distinguish repeated patterns in the same code domain from unrelated
  tasks.
- Keep codegraph.db as the sole source of code-graph truth.

### 16.7 stella-tui

- Show advisory learning notices without interrupting every task.
- Support Keep, Edit, and Ignore.
- Show why a directive was inferred and the number of distinct tasks.
- Clearly label personal versus repository sharing.
- Require explicit confirmation before blocking enforcement or Git publication.
- Provide a compact view of contract failures before Stella claims completion.

### 16.8 stella-observatory

Add views for:

- exact context frame lineage per invocation;
- included and excluded items;
- selection reasons and conflicts;
- directive citations and efficacy;
- observation-to-directive evidence graphs;
- promotions and supersessions;
- stale and expiring records;
- artifact contract results;
- protocol-provider contribution and latency.

### 16.9 Documentation

After implementation, publish user-facing documentation under
stella-docs/content/docs. This RFC remains in docs/design because it is a design
and implementation specification, not live product documentation.

## 17. context-graph-protocol extension

### 17.1 Compatibility strategy

The existing retrieval protocol remains sufficient for query-only providers.
Add an optional capability, tentatively named:

~~~text
contextgraph/lifecycle/1.0-draft
~~~

Providers that do not advertise it continue to support context/query with no
behavior change. Stella must not assume lifecycle write support.

### 17.2 Portable record envelope

Add a lifecycle record envelope with:

- record_id;
- record_kind;
- schema_version;
- scope;
- sharing_scope;
- observed_at;
- valid_from;
- valid_until;
- status;
- confidence;
- evidence references;
- content hash;
- extension data.

Portable record kinds initially include:

- observation;
- directive;
- evidence;
- artifact_contract;
- contract_validation;
- outcome_assessment;
- promotion_event;
- citation_event.

The protocol should define directive kind and enforcement vocabulary but not
promotion thresholds or governance behavior.

### 17.3 Optional operations

Capability-negotiated operations:

- context/observe: append idempotent observations;
- context/propose: create or return a candidate directive;
- context/promote: record a lifecycle promotion;
- context/feedback: append citation or outcome feedback;
- context/validate: return artifact-contract validation results.

The existing context/query operation remains the retrieval path. Batch forms are
preferred for observation and feedback writeback.

Every mutation must define:

- idempotency behavior;
- consent requirement;
- partial failure behavior;
- typed errors;
- provider timeout and isolation;
- maximum payload size;
- sensitivity and sharing rejection;
- whether the provider can retain raw content.

### 17.4 Naming migration

The protocol's current temporal fields include recorded_at and valid_to. During
the draft period:

- readers accept recorded_at as an alias for observed_at;
- readers accept valid_to as an alias for valid_until;
- writers emit observed_at and valid_until;
- schemas and examples use only canonical names;
- the conformance suite tests both compatibility reads and canonical writes.

### 17.5 Protocol repository deliverables

The related context-graph-protocol change should include:

- lifecycle types in contextgraph-types;
- capability declarations;
- host dispatch and consent boundaries in contextgraph-host;
- JSON schemas and examples;
- additive error codes;
- conformance fixtures for query-only and lifecycle providers;
- idempotency, alias, time-cutoff, and partial-failure tests;
- a security and privacy section;
- an upgrade guide.

Stella may land its local lifecycle before this protocol work. The protocol PR
should be driven by at least one second provider or concrete interoperability
need so Stella policy is not prematurely frozen into a public wire contract.

## 18. Security, privacy, and ownership

- Local-first is mandatory for individual use.
- No account or server is required.
- No new outbound network traffic is introduced.
- Personal directives and raw observations are never shared automatically.
- Repository publication includes reviewed statements and evidence IDs, not raw
  personal telemetry.
- Secrets are redacted before persistence and embeddings.
- Users can inspect, export, correct, archive, and delete eligible personal
  records.
- Deletion policy distinguishes source deletion, derived-record invalidation,
  audit retention, and Git history.
- Provider capability and consent checks run before lifecycle writeback.
- Organization policy cannot silently read personal observations.
- Context frames carry trust and instruction-capability boundaries.
- Content hashes support tamper detection without exposing full evidence.

## 19. Determinism and performance

The compiler must be deterministic for identical:

- task and state;
- as-of cutoffs;
- active records;
- provider results;
- compiler version;
- token budget;
- ranking configuration.

Every frame stores input_hash and compiler_version. Ordering is stable.
Snapshots are keyed by these inputs.

Initial performance budgets:

- local observation append p95 under 20 ms excluding embedding;
- incremental harvest should not add more than 100 ms to task completion;
- frame compilation p95 under 150 ms on a warm local store excluding external
  providers;
- lifecycle mining runs after safe boundaries or in a bounded background job;
- external provider timeouts retain existing fan-out isolation;
- raw observation volume is bounded by TTL, compaction, and rate limits.

These values are targets and should be benchmarked before becoming release
gates.

## 20. Test and conformance plan

### 20.1 Unit and property tests

- all new serde types round-trip;
- serialized keys are lowercase snake_case;
- confidence is bounded from 0 through 100;
- temporal interval and as-of semantics;
- scope never widens implicitly;
- personal records never publish to repository files;
- inferred directives never become blocking automatically;
- conflicts resolve deterministically;
- replay is idempotent;
- one task cannot fabricate three independent observations;
- pruning exclusions for critical and blocking records;
- rule frontmatter reads legacy and canonical guard keys;
- compiled frame ordering and input hashes are stable.

### 20.2 Migration tests

- migrate existing context.db without data loss;
- preserve recorded_at as observed_at;
- preserve valid_to as valid_until;
- old memory and fact recall remains equivalent;
- existing .stella/rules/*.md load unchanged;
- rollback on interrupted migration;
- SQLite integrity_check after migration.

### 20.3 Witness scenarios

1. Three distinct API tasks each add integration coverage. Solo mode creates one
   advisory candidate and shows one Keep, Edit, Ignore notice.
2. Repeated events in one task do not cross the promotion threshold.
3. A blocking candidate always requires explicit confirmation.
4. A personal preference never appears in a Git diff.
5. Adding a collaborator offers repository promotion and leaves personal
   records local.
6. A missing brand wordmark fails the contract and prevents a done outcome.
7. A later successful brand task cites the confirmed contract and passes every
   requirement.
8. A user correction becomes evidence, but a model's self-critique alone does
   not promote a rule.
9. Git follow-up changes produce inferred observations, not verified errors.
10. A directive with repeated attributable negative outcomes becomes stale but
    is recoverable.
11. A query-only protocol provider continues to work unchanged.
12. A lifecycle provider passes idempotency and legacy-alias conformance.

### 20.4 Evaluation suite

Build a replay corpus with anonymized synthetic tasks covering:

- recurring output-shape preferences;
- rule contradictions;
- changing facts;
- stale preferences;
- code-scoped procedures;
- false-positive Git inference;
- repeated omissions;
- solo-to-team migration;
- malicious instructions embedded in evidence;
- high-volume noisy traces.

Measure:

- candidate precision and recall;
- false blocking rate, which must remain zero for inferred items;
- user correction rate;
- contract completion rate;
- context token efficiency;
- citation attribution coverage;
- time to promote useful steering;
- stale-context regression rate;
- privacy and scope violations.

## 21. Rollout

### Phase 0: schema and lineage

- Finalize record vocabulary and temporal naming.
- Add internal events.
- Add context database migrations.
- Add compiled frame manifests and citation lineage.
- Keep all new learning disabled by default behind a feature flag.

### Phase 1: local observations

- Harvest journals, reflections, tool outcomes, verification, and Git diffs.
- Add inspection commands.
- Add idempotent replay and retention.
- Do not promote automatically.

### Phase 2: solo advisory learning

- Extend existing rule mining.
- Enable the three-observation promotion ladder.
- Add Keep, Edit, and Ignore.
- Publish confirmed repository rules through existing Markdown files.
- Never generate inferred blocking rules.

### Phase 3: artifact contracts

- Add contract matching and deterministic validators.
- Gate completion on required contract checks.
- Ship the brand-kit contract scenario as a full witness fixture.
- Feed validation outcomes into learning.

### Phase 4: team governance

- Add Context PR materialization through Git.
- Add owner routing and promotion audit.
- Add solo-to-team migration.
- Keep organization policy behind an explicit provider or managed source.

### Phase 5: protocol interoperability

- Propose the optional lifecycle extension upstream.
- Add a second provider fixture.
- Implement capability negotiation and writeback adapters.
- Add protocol conformance and compatibility tests.

### Phase 6: efficacy and advanced pruning

- Add directive health and attribution dashboards.
- Add matched-control analysis where safe.
- Tune expiry and archival using replay data.
- Publish stable lifecycle schemas only after evidence from real usage.

## 22. Acceptance criteria

The feature is complete when:

- Stella compiles an inspectable, deterministic context frame linked to every
  relevant invocation.
- Every included durable record has provenance, scope, temporal semantics, and
  a citation label.
- Observations can be replayed without duplication.
- Three independent observations can create one advisory candidate in solo
  mode.
- Keep, Edit, and Ignore work without a formal review queue.
- No inferred directive becomes blocking without explicit confirmation.
- Personal learning never enters a repository change automatically.
- Confirmed repository rules use .stella/rules/*.md and existing rule loading.
- A solo repository can become a team repository without data migration or
  personal-data leakage.
- Artifact contracts deterministically catch missing required deliverables.
- The brand-kit fixture prevents completion when a required asset is absent.
- Verified and inferred outcome labels are never conflated.
- Citation helpfulness and unhelpfulness are stored as attributable events.
- Stale or repeatedly unhelpful advisory directives can be reversibly archived.
- Existing query-only context providers and existing rule files remain
  compatible.
- All new serialized properties use lowercase snake_case.
- Context temporal fields use observed_at, valid_from, and valid_until.
- No new phone-home behavior exists.

## 23. Architectural differentiators

The defensible advantage is the closed, evidence-backed learning loop rather
than a larger vector store.

1. **Verified context efficacy.** Stella links the exact frame used by an
   invocation to witness tests, contract results, user feedback, and accepted
   Git state.
2. **Artifact contracts.** Durable preferences become machine-checkable output
   requirements, closing the gap between remembering and completing.
3. **Bi-temporal context lineage.** Stella can reconstruct what it knew, when
   it learned it, and when the claim was applicable.
4. **Adaptive governance.** The same record graph serves one developer, a team,
   and a regulated organization without forcing one workflow on all three.
5. **Privacy-preserving scope transitions.** Personal learning stays private;
   repository knowledge crosses the boundary only through promotion.
6. **Protocol-native interoperability.** Providers can participate in retrieval
   and, later, lifecycle writeback without owning Stella policy.
7. **Deterministic-first self-improvement.** Tests, validators, manifests, and
   diffs outrank model self-judgment.
8. **Anti-poisoning by construction.** Observations have no instruction
   authority, and promotion requires independent evidence.
9. **Reproducible context compilation.** Input hashes, versions, conflict logs,
   and budget manifests make context behavior debuggable and benchmarkable.
10. **Git-native steering.** Published repository behavior remains legible in
    normal code review instead of disappearing into an opaque hosted memory
    service.

## 24. Remaining gaps and open decisions

The design is complete enough to implement, but these decisions should be
settled before stabilizing the public schema:

1. **Identity:** define how Stella recognizes active repository identities
   locally without network calls or false team-mode transitions.
2. **Organization policy source:** define a signed local or provider-backed
   source that preserves no-phone-home defaults.
3. **Contract authoring:** decide whether contracts are stored only in
   context.db, in a user config directory, or optionally in a Git-backed
   contracts directory. Personal contracts must not leak.
4. **Rule metadata parser:** extend the current lightweight frontmatter parser
   or adopt a bounded YAML parser while keeping old files byte-compatible.
5. **Evidence retention:** establish defaults by sensitivity and define
   deletion propagation into derived candidates.
6. **Attribution:** calibrate when an outcome can fairly be assigned to a cited
   directive.
7. **Contradictions:** define UI and policy for oscillating preferences and
   mutually exclusive repository rules.
8. **Semantic validators:** define provider, prompt, version, cost, privacy, and
   confidence rules before treating them as evidence.
9. **Multi-repository scope:** decide how a personal procedure becomes a
   reusable opt-in template without accidental organization sharing.
10. **Schema collision:** keep the protocol's atomic ContextFrame separate from
    Stella's CompiledContextFrame aggregate.
11. **Protocol timing:** do not standardize lifecycle writeback until a real
    external provider validates the shape.
12. **User control:** add export, correction, pin, reset, and deletion flows
    before calling the system durable personal learning.

Recommended defaults for the open decisions are:

- detect identities from local Git authors over a configurable recent window
  and ask before changing mode;
- store personal contracts locally and permit explicit repository export;
- extend the existing frontmatter parser before adding a general YAML
  dependency;
- treat deterministic and explicit-user evidence as promotion accelerators;
- keep semantic judgments advisory;
- defer the public lifecycle extension until Stella's local schema survives
  replay evaluation.

## 25. Final recommendation

Implement the adaptive context lifecycle as a Stella feature using the context
graph as its durable substrate. Keep repository rule publication in the
existing .stella/rules/*.md path, local learning in .stella/context.db, and
governance in .stella/settings.json.

Do not block Stella on protocol expansion. Once the local lifecycle is proven,
upstream the smallest optional context-graph-protocol extension needed for
external providers to exchange typed records and feedback. This produces a
strong product immediately without freezing Stella-specific policy into a
general protocol.
