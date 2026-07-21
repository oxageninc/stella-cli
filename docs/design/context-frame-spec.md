# Context Frame Specification

## 1. Purpose

A **Context Frame** is the bounded, task-specific package of trusted information
supplied to an agent, model, or tool invocation. It is compiled from durable
stores and ephemeral execution state; it is not a copy of the entire context engine.

The frame answers six questions:

1. What is the agent trying to do?
2. What must, should, or should not influence its behavior?
3. What is known, when was it true, and how well supported is it?
4. What has just happened and what work is in progress?
5. What parts of the codebase or environment matter to this task?
6. Why was each item included, and can its effect be evaluated later?

## 2. Design principles

- **Bounded:** Every frame has an explicit token/byte budget and contains only
  task-relevant material.
- **Typed:** Instructions, facts, memories, observations, code relationships,
  state, and evidence retain different semantics.
- **Provenance-native:** Material can be traced to its sources and the
  retrieval decision that admitted it.
- **Time-aware:** Long-lived knowledge records both when it was true in the
  world and when the engine believed it.
- **Policy-first:** Constraints and authorization checks are applied before
  generation or tool use.
- **Inspectable:** The engine can explain which items were included, excluded,
  superseded, or pruned.
- **Self-correcting:** Citation outcomes, feedback, and expiry rules improve
  retrieval over time without silently rewriting user intent.
- **Privacy-bounded:** Observations are purpose-limited, scoped, consent-aware,
  and short-lived by default.

## 3. System model

The Context Frame is assembled from the following sources.

| Source             | Purpose                                                                                               | Retention                          | Included directly in frame?                 |
| ------------------ | ----------------------------------------------------------------------------------------------------- | ---------------------------------- | ------------------------------------------- |
| Task               | The current request, goal, and success criteria.                                                      | Per task                           | Yes                                         |
| State              | Plan, progress, open questions, recent actions, tool results.                                         | Ephemeral / task-scoped            | Yes                                         |
| Artifact contract  | Reusable, machine-checkable definition of an acceptable deliverable.                                  | Durable, versioned                 | Selected items                              |
| Directives         | Typed information that guides behavior: memories, facts, rules, preferences, constraints, procedures. | Durable with lifecycle             | Selected items                              |
| Bi-temporal memory | Historical knowledge with valid-time and transaction-time semantics.                                  | Durable with lifecycle             | Selected summaries or records               |
| Observations       | Raw or normalized events about user, agent, system, or environment behavior.                          | Short-lived by default             | Rarely raw; normally summarized or promoted |
| Source-code map    | Code entities and relationships such as symbols, dependencies, ownership, and changes.                | Durable, repository-scoped         | Selected subgraph                           |
| Evidence           | Source artifacts, spans, provenance, and trust metadata.                                              | Durable / immutable where possible | Citations and selected excerpts             |
| Entities           | Canonical identity of users, projects, services, files, documents, and concepts.                      | Durable                            | Bindings and references                     |

### 3.1 Relationship between stores

```text
Task + State + Entity resolution
          │
          ▼
Policy resolution (directives and authorization)
          │
          ├── retrieve bi-temporal memories
          ├── retrieve relevant code-map subgraph
          ├── summarize or promote observations
          └── attach supporting evidence
          │
          ▼
Rank, resolve conflicts, compress, and budget
          │
          ▼
      Context Frame
          │
          ▼
Agent / model / tool execution → outcomes → citation telemetry and state updates
```

## 4. Common types

```ts
type ISODateTime = string
type ID = string

type Scope = {
  tenant_id?: ID
  organization_id?: ID
  workspace_id?: ID
  project_id?: ID
  repository_id?: ID
  environment_id?: ID
  session_id?: ID
  task_id?: ID
  user_id?: ID
}

type SharingScope = "personal" | "repository" | "organization"

type Source = "user" | "system" | "inferred" | "imported" | "observed"
type Status = "active" | "stale" | "superseded" | "archived"
type Priority = "low" | "normal" | "high" | "critical"

type Reference = {
  id: ID
  type: "entity" | "directive" | "memory" | "observation" | "evidence" | "code" | "state" | "task" | "contract" | "artifact" | "trace"
  label?: string
}
```

## 5. Entity registry

Entities provide canonical references that connect every other store. Do not rely on display names alone.

```ts
type Entity = {
  id: ID
  type: "user" | "agent" | "organization" | "workspace" | "project" | "repository" |
        "service" | "environment" | "file" | "symbol" | "document" | "concept"
  canonical_name: string
  aliases?: string[]
  scope?: Scope
  status: "active" | "merged" | "archived"
  merged_into?: ID
  created_at: ISODateTime
  updated_at: ISODateTime
}
```

Example:

```json
{
  "id": "project_analytics",
  "type": "project",
  "canonical_name": "Analytics",
  "aliases": ["analytics-service"],
  "scope": { "workspace_id": "workspace_acme" },
  "status": "active",
  "created_at": "2026-07-20T16:00:00Z",
  "updated_at": "2026-07-20T16:00:00Z"
}
```

## 6. Directives

A **Directive** is a typed unit that may influence behavior or interpretation.
This schema preserves the agreed categories, including `fact`. In this design, a
`fact` directive is a task-relevant factual assertion selected for the frame;
the detailed historical record can live in the bi-temporal memory store.

```ts
type DirectiveKind =
  | "memory"
  | "fact"
  | "rule"
  | "preference"
  | "constraint"
  | "procedure"

type CitationStats = {
  /** Bounded recent list; retain the full event history in CitationEvent storage. */
  cited_at: ISODateTime[]
  cited_count: number
  helpful_count: number
  not_helpful_count: number
  last_cited_at?: ISODateTime
  last_helpful_at?: ISODateTime
  last_not_helpful_at?: ISODateTime
}

type Directive = {
  id: ID
  kind: DirectiveKind
  subject?: ID
  statement: string
  value?: unknown

  priority: Priority
  confidence?: number
  source: Extract<Source, "user" | "system" | "inferred" | "imported">
  status: Status
  promotion_status?: "draft" | "proposed" | "advisory" | "confirmed"
  enforcement?: "advisory" | "blocking"
  scope?: Scope
  /** Controls who may receive or inherit this directive. */
  sharing_scope?: SharingScope

  evidence_ids?: ID[]
  supersedes?: ID[]
  citation_stats: CitationStats

  created_at: ISODateTime
  updated_at: ISODateTime
  expires_at?: ISODateTime
}
```

### 6.1 Directive interpretation

| Kind         | Meaning                                                                | Typical priority | Example                                                  |
| ------------ | ---------------------------------------------------------------------- | ---------------- | -------------------------------------------------------- |
| `memory`     | Retained observation or past interaction that guides the present task. | normal           | “The user chose PostgreSQL for this service.”            |
| `fact`       | A task-relevant assertion believed true now.                           | normal           | “The API deploys in `us-west-2`.”                        |
| `rule`       | General behavioral instruction.                                        | high             | “Explain production changes before making them.”         |
| `preference` | Desired but non-mandatory behavior.                                    | low–normal       | “Prefer concise Markdown.”                               |
| `constraint` | Hard boundary, prohibition, or required condition.                     | high–critical    | “Do not send customer data externally without approval.” |
| `procedure`  | Ordered workflow.                                                      | normal–high      | “Test, review, approve, deploy.”                         |

### 6.2 Directive example

```json
{
  "id": "dir_constraint_001",
  "kind": "constraint",
  "subject": "agent_primary",
  "statement": "Do not send customer data to third-party services without explicit approval.",
  "value": {
    "prohibited_action": "send_customer_data_to_third_party",
    "unless": "explicit_approval"
  },
  "priority": "critical",
  "confidence": 1,
  "source": "system",
  "status": "active",
  "scope": { "workspace_id": "workspace_acme" },
  "evidence_ids": ["evidence_policy_033"],
  "citation_stats": {
    "cited_at": ["2026-07-20T16:05:00Z"],
    "cited_count": 1,
    "helpful_count": 1,
    "not_helpful_count": 0,
    "last_cited_at": "2026-07-20T16:05:00Z",
    "last_helpful_at": "2026-07-20T16:05:00Z"
  },
  "created_at": "2026-07-20T16:00:00Z",
  "updated_at": "2026-07-20T16:00:00Z"
}
```

### 6.3 Directive lifecycle

1. Only `active` directives are eligible for frame selection.
2. A newer directive may `supersede` an older one; retain the older one with
   `status: "superseded"` for auditability.
3. Expire a directive after `expires_at`; set `status: "archived"` rather than deleting it.
4. Mark an unused non-system directive as `stale` after the configured age threshold.
5. Archive a candidate with at least five evaluated citations when:

   ```text
   not_helpful_count / (helpful_count + not_helpful_count) >= 0.80
   ```

6. Never auto-archive `critical` or `system` directives. Route them to human or policy review.
7. Inferred directives should be lower priority by default, carry confidence
   and evidence, and expire unless reaffirmed.

```json
{
  "directive_lifecycle": {
    "minimum_evaluated_citations": 5,
    "not_helpful_rate_to_archive": 0.8,
    "stale_after_days": 180,
    "stale_grace_days": 30,
    "protected_priorities": ["critical"],
    "protected_sources": ["system"],
    "action": "archive"
  }
}
```

## 7. Bi-temporal memories

The memory store preserves what was asserted or known, when it was true in the
world, and when the system recorded or believed it. This enables time-travel
queries, corrections, and auditability.

```ts
type BiTemporalMemory = {
  id: ID
  subject: ID
  predicate: string
  object: unknown
  statement: string

  /** When the assertion became true in the modeled world. */
  valid_from: ISODateTime
  /** When the assertion stopped being true; omit when still valid. */
  valid_until?: ISODateTime
  /** When the engine observed and recorded this version. */
  observed_at: ISODateTime

  confidence: number
  source: Extract<Source, "user" | "system" | "inferred" | "imported">
  status: Status
  scope?: Scope
  evidence_ids: ID[]
  supersedes?: ID[]
  created_at: ISODateTime
  updated_at: ISODateTime
}
```

Example correction: the current value is revised while the prior version
remains queryable.

```json
{
  "id": "memory_044_v2",
  "subject": "service_analytics_api",
  "predicate": "deployed_region",
  "object": "us-west-2",
  "statement": "The Analytics API is deployed in us-west-2.",
  "valid_from": "2026-06-01T00:00:00Z",
  "observed_at": "2026-07-20T16:10:00Z",
  "confidence": 1,
  "source": "imported",
  "status": "active",
  "scope": { "project_id": "project_analytics" },
  "evidence_ids": ["evidence_deploy_802"],
  "supersedes": ["memory_044_v1"],
  "created_at": "2026-07-20T16:10:00Z",
  "updated_at": "2026-07-20T16:10:00Z"
}
```

### 7.1 Memory query requirements

- Support `as_of_valid_at` and `as_of_observed_at` independently.
- Return current active versions by default.
- Surface contradictions rather than overwriting them silently.
- Never use a memory outside its scope or authorization boundary.
- Include provenance and confidence when a memory materially affects an answer or action.

## 8. Observations

An **Observation** is a captured event about a user, agent, system, tool, or
environment. It is evidence, not an instruction. End-user behavior is a
subset of observations.

```ts
type Observation = {
  id: ID
  subject: ID
  actor?: ID
  event: string
  observed_at: ISODateTime
  attributes?: Record<string, unknown>

  source: "observed" | "imported"
  scope?: Scope
  evidence_ids?: ID[]
  expires_at?: ISODateTime
  privacy: "essential" | "personalization" | "sensitive"
  retention_class: "ephemeral" | "short_term" | "long_term"
}
```

Example:

```json
{
  "id": "observation_001",
  "subject": "user_ana",
  "event": "response_format_selected",
  "observed_at": "2026-07-20T16:30:00Z",
  "attributes": {
    "format": "markdown",
    "verbosity": "concise"
  },
  "source": "observed",
  "scope": { "workspace_id": "workspace_acme" },
  "expires_at": "2026-10-18T16:30:00Z",
  "privacy": "personalization",
  "retention_class": "short_term"
}
```

### 8.1 Observation rules

- Do not represent raw observations as directives.
- Do not promote a single observation into a preference, fact, or rule.
- Promote only repeated and consistent observations from a defined time window.
- A promoted directive must use `source: "inferred"`, retain links to its
  observation evidence, have bounded confidence, and normally expire.
- Explicit user directives override behavioral inferences.
- Never infer sensitive attributes, protected characteristics, health, identity,
  or consequential preferences from observations.
- Collect only observations necessary for a stated product purpose;
  respect consent, authorization, and deletion requests.

Example inferred preference:

```json
{
  "id": "dir_pref_inferred_001",
  "kind": "preference",
  "subject": "user_ana",
  "statement": "The user appears to prefer concise Markdown responses.",
  "value": {
    "verbosity": "low",
    "format": "markdown",
    "evidence": {
      "observation_ids": ["observation_001", "observation_014", "observation_027"],
      "observations": 3,
      "window_days": 30
    }
  },
  "priority": "low",
  "confidence": 0.75,
  "source": "inferred",
  "status": "active",
  "citation_stats": {
    "cited_at": [],
    "cited_count": 0,
    "helpful_count": 0,
    "not_helpful_count": 0
  },
  "created_at": "2026-07-20T16:30:00Z",
  "updated_at": "2026-07-20T16:30:00Z",
  "expires_at": "2026-10-18T16:30:00Z"
}
```

## 9. State

State is the current task's working memory. It is mutable, short-lived, and never
becomes durable memory automatically.

```ts
type TaskState = {
  id: ID
  task_id: ID
  status: "draft" | "in_progress" | "blocked" | "completed" | "failed" | "cancelled"
  goal: string
  success_criteria?: string[]
  plan?: Array<{
    id: ID
    description: string
    status: "pending" | "in_progress" | "completed" | "skipped" | "blocked"
  }>
  decisions?: Array<{
    statement: string
    decided_at: ISODateTime
    evidence_ids?: ID[]
  }>
  open_questions?: string[]
  recent_actions?: Array<{
    type: "tool_call" | "agent_action" | "user_message" | "system_event"
    summary: string
    occurred_at: ISODateTime
    reference_ids?: ID[]
  }>
  tool_result_refs?: ID[]
  scope?: Scope
  updated_at: ISODateTime
  expires_at: ISODateTime
}
```

State promotion is governed by the active promotion policy. A user-confirmed
decision, verified outcome, or reviewed inference may create a memory or
directive with evidence. In solo mode, qualifying low-risk candidates may be
published as advisory rules after the configured confidence threshold, with a
lightweight keep/edit/ignore action. Raw tool output and intermediate reasoning
should not automatically become durable knowledge.

## 10. Source-code map

The code map is a versioned graph of code entities and relationships. The
frame should contain only the subgraph needed for the task.

```ts
type CodeNode = {
  id: ID
  kind: "repository" | "package" | "directory" | "file" | "module" | "class" |
        "function" | "method" | "type" | "endpoint" | "database_table" | "test" | "config"
  name: string
  path?: string
  symbol?: string
  repository_id: ID
  revision: string
  owner_ids?: ID[]
  summary?: string
  attributes?: Record<string, unknown>
}

type CodeEdge = {
  id: ID
  from: ID
  to: ID
  relation: "imports" | "calls" | "defines" | "implements" | "extends" | "tests" |
            "reads" | "writes" | "depends_on" | "owned_by" | "configured_by" | "changed_with"
  repository_id: ID
  revision: string
  confidence?: number
}

type CodeMapSlice = {
  repository_id: ID
  revision: string
  nodes: CodeNode[]
  edges: CodeEdge[]
  entry_points?: ID[]
  rationale: string
}
```

### 10.1 Code map requirements

- Index at symbol level, not only file level.
- Version every node and edge by repository revision.
- Support impact queries, such as “what calls this function?” and “which tests
  cover this endpoint?”
- Preserve ownership, deployment, and configuration relationships where possible.
- Attach code references as evidence for code-derived claims.

## 11. Evidence and provenance

Evidence makes the frame defensible. It points to the source artifact and, when
possible, the exact span or structured record supporting an item.

```ts
type Evidence = {
  id: ID
  type: "user_message" | "document" | "code" | "tool_result" | "database_record" |
        "api_response" | "policy" | "observation" | "evaluation" | "agent_trace" |
        "working_log" | "git_diff" | "artifact" | "validator_output"
  source_uri?: string
  source_system?: string
  content_hash?: string
  excerpt?: string
  locator?: {
    path?: string
    revision?: string
    line_start?: number
    line_end?: number
    page?: number
    record_id?: string
  }
  captured_at: ISODateTime
  author_id?: ID
  trust: "authoritative" | "verified" | "unverified" | "generated"
  scope?: Scope
  access: "public" | "internal" | "restricted" | "sensitive"
  expires_at?: ISODateTime
}
```

```ts
type CitationEvent = {
  id: ID
  frame_id: ID
  item: Reference
  cited_at: ISODateTime
  reason: "policy" | "semantic_relevance" | "graph_proximity" | "recency" | "user_pin" | "state_dependency"
  rank?: number
  outcome?: "helpful" | "not_helpful" | "unevaluated"
  evaluated_at?: ISODateTime
  evaluator?: "user" | "system" | "model" | "human_reviewer"
  feedback?: string
}
```

`CitationEvent` is append-only. `Directive.citation_stats` is a materialized
aggregate for ranking and pruning; never treat it as the only audit record.

## 12. Retrieval and conflict resolution

### 12.1 Default precedence

Apply precedence in this order; a lower layer cannot override a higher one.

1. Authorization, law, platform safety, and non-bypassable system constraints.
2. Active `critical` and `high` constraints within matching scope.
3. Explicit user constraints and rules within matching scope.
4. Active procedures when their preconditions match.
5. Current, high-confidence facts and bi-temporal memories.
6. Explicit preferences.
7. Inferred directives.
8. Observations and behavioral summaries.

Within one layer, prefer narrower scope, newer active versions, higher
confidence, stronger evidence, and more helpful citation history. If
the conflict remains material, surface it or ask for clarification rather
than silently choosing.

### 12.2 Retrieval requirements

- Resolve entities and authorization before retrieval.
- Filter by scope, status, valid interval, observation time, `expires_at`, and
  access permissions.
- Retrieve by hybrid relevance: semantic similarity, graph proximity, structured
  predicates, recency, and task/state dependency.
- Enforce diversity: do not fill a frame with near-duplicate memories.
- Prefer summaries with stable references when raw source material is too large.
- Include evidence for directives or facts that affect material decisions or
  tool actions.
- Respect a token budget by scoring utility per token, not relevance alone.
- Keep deterministic frame manifests so a past decision can be reproduced.

## 13. Context Frame schema

```ts
type FrameItem<T> = {
  item: T
  relevance_score: number
  selection_reasons: Array<
    "policy" | "semantic_relevance" | "graph_proximity" | "recency" |
    "state_dependency" | "user_pin" | "conflict_resolution"
  >
  evidence_ids?: ID[]
  compressed?: boolean
  token_estimate?: number
}

type FrameMetadata = {
  schema_version: "1.0"
  compiler_version: string
  created_at: ISODateTime
  as_of_valid_at: ISODateTime
  as_of_observed_at: ISODateTime
  scope: Scope
  token_budget: number
  token_estimate: number
  selection_policy_version: string
  authorization_policy_version: string
  governance_mode: "solo" | "team" | "regulated"
  promotion_policy_version: string
  retrieval_query: string
  warnings?: string[]
}

type ContextFrame = {
  id: ID
  metadata: FrameMetadata

  task: {
    id: ID
    request: string
    goal?: string
    success_criteria?: string[]
    actor_id?: ID
  }

  entity_bindings: Array<{
    mention: string
    entity_id: ID
    confidence: number
  }>

  state?: TaskState

  directives: {
    constraints: FrameItem<Directive>[]
    rules: FrameItem<Directive>[]
    procedures: FrameItem<Directive>[]
    preferences: FrameItem<Directive>[]
    facts: FrameItem<Directive>[]
    memories: FrameItem<Directive>[]
  }

  bi_temporal_memories: FrameItem<BiTemporalMemory>[]
  observations?: FrameItem<Observation>[]
  contracts?: FrameItem<ArtifactContract>[]
  code_map?: CodeMapSlice
  evidence: Evidence[]

  excluded: Array<{
    item: Reference
    reason: "out_of_scope" | "expired" | "stale" | "superseded" | "unauthorized" |
            "conflicted" | "low_utility" | "budget"
  }>

  citations: CitationEvent[]
}
```

### 13.1 Example context frame

```json
{
  "id": "frame_20260720_001",
  "metadata": {
    "schema_version": "1.0",
    "compiler_version": "context-compiler/1.0.0",
    "created_at": "2026-07-20T17:00:00Z",
    "as_of_valid_at": "2026-07-20T17:00:00Z",
    "as_of_observed_at": "2026-07-20T17:00:00Z",
    "scope": {
      "workspace_id": "workspace_acme",
      "project_id": "project_analytics",
      "task_id": "task_882"
    },
    "token_budget": 8000,
    "token_estimate": 3620,
    "selection_policy_version": "retrieval-policy/3.1",
    "authorization_policy_version": "access-policy/2.4",
    "governance_mode": "solo",
    "promotion_policy_version": "promotion-policy/1.0",
    "retrieval_query": "Add retry handling to the analytics export endpoint."
  },
  "task": {
    "id": "task_882",
    "request": "Add retry handling to the analytics export endpoint.",
    "goal": "Implement and verify a bounded retry strategy without exposing customer data.",
    "success_criteria": ["Tests pass", "Retries are bounded", "No external data transfer"],
    "actor_id": "user_ana"
  },
  "entity_bindings": [
    {
      "mention": "analytics export endpoint",
      "entity_id": "symbol_export_analytics",
      "confidence": 0.98
    }
  ],
  "directives": {
    "constraints": [
      {
        "item": { "id": "dir_constraint_001", "kind": "constraint", "statement": "Do not send customer data to third-party services without explicit approval.", "priority": "critical", "source": "system", "status": "active", "citation_stats": { "cited_at": [], "cited_count": 0, "helpful_count": 0, "not_helpful_count": 0 }, "created_at": "2026-07-20T16:00:00Z", "updated_at": "2026-07-20T16:00:00Z" },
        "relevance_score": 1,
        "selection_reasons": ["policy"],
        "evidence_ids": ["evidence_policy_033"]
      }
    ],
    "rules": [],
    "procedures": [],
    "preferences": [],
    "facts": [],
    "memories": []
  },
  "bi_temporal_memories": [
    {
      "item": { "id": "memory_retry_009", "subject": "symbol_export_analytics", "predicate": "uses_retry_policy", "object": "none", "statement": "The export endpoint currently has no retry policy.", "valid_from": "2026-07-19T10:00:00Z", "observed_at": "2026-07-20T10:00:00Z", "confidence": 0.95, "source": "imported", "status": "active", "evidence_ids": ["evidence_code_992"], "created_at": "2026-07-20T10:00:00Z", "updated_at": "2026-07-20T10:00:00Z" },
      "relevance_score": 0.96,
      "selection_reasons": ["semantic_relevance", "graph_proximity"],
      "evidence_ids": ["evidence_code_992"]
    }
  ],
  "observations": [],
  "code_map": {
    "repository_id": "repo_analytics",
    "revision": "a8d2c41",
    "nodes": [{ "id": "symbol_export_analytics", "kind": "endpoint", "name": "exportAnalytics", "path": "src/routes/export.ts", "repository_id": "repo_analytics", "revision": "a8d2c41" }],
    "edges": [],
    "entry_points": ["symbol_export_analytics"],
    "rationale": "The task names this endpoint directly."
  },
  "evidence": [
    { "id": "evidence_policy_033", "type": "policy", "source_system": "policy-service", "captured_at": "2026-07-20T16:00:00Z", "trust": "authoritative", "access": "internal" },
    { "id": "evidence_code_992", "type": "code", "source_uri": "repo://analytics/src/routes/export.ts", "locator": { "path": "src/routes/export.ts", "revision": "a8d2c41", "line_start": 20, "line_end": 48 }, "captured_at": "2026-07-20T10:00:00Z", "trust": "verified", "access": "internal" }
  ],
  "excluded": [],
  "citations": []
}
```

## 14. Context compiler contract

The context compiler is the architectural center of the system. It should
implement this pipeline:

1. **Normalize the request:** Extract task, actor, requested action, target entities, and expected outcome.
2. **Resolve identity and scope:** Bind names to canonical entities; establish tenant, workspace, project, repository, session, and authorization boundaries.
3. **Load state:** Read the active task state and prior frame checkpoint.
4. **Resolve policy and contracts:** Retrieve and validate active constraints, rules, procedures, and reusable deliverable contracts before other recall.
5. **Retrieve knowledge:** Query directives, bi-temporal memories, code-map slices, observations, contracts, and evidence using the appropriate semantics for each source.
6. **Resolve conflicts:** Apply precedence, scope, temporal rules, confidence, and source trust. Emit warnings for unresolved material conflicts.
7. **Summarize and compress:** Preserve identifiers and evidence references while reducing content to the budget.
8. **Create a deterministic manifest:** Persist what was included and excluded, selection reasons, policy versions, and time bounds.
9. **Execute:** Send the frame to the model or tool executor.
10. **Validate and evaluate:** Run contract validators, capture user feedback and task outcomes, and attach the result to the trace.
11. **Learn safely:** Capture outcomes and citations; update state; create durable records only through explicit promotion rules.

### 14.1 Compiler invariants

- No unauthorized item enters a frame.
- No expired, superseded, or out-of-scope item is treated as active.
- Every material claim or instruction has an identifier and evidence or a declared lack of evidence.
- Every tool action can be traced to the frame and relevant policy inputs that authorized it.
- Any deliverable covered by an active artifact contract is validated before it is presented as complete.
- The same request, scope, store snapshot, policy versions, and budget should produce a reproducible frame manifest.
- A model may propose memory/directive writes, but a validator must enforce schema, policy, provenance, and lifecycle rules before persistence.

## 15. Gaps and recommended additions

The foundation is complete enough to build. These items are not new knowledge categories; they are the capabilities that make the design reliable at scale.

### 15.1 Required before production

| Gap                               | Why it matters                                                     | Requirement                                                                                            |
| --------------------------------- | ------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------ |
| Authorization model               | A frame can leak data if scope is only advisory.                   | Enforce row/item-level access checks before ranking and again before tool use.                         |
| Write gate                        | Models can create plausible but incorrect memories.                | Use schema validation, provenance requirements, deduplication, confidence policy, and optional review. |
| Contradiction handling            | Time-aware memories will disagree.                                 | Build a contradiction index and return conflicts rather than overwriting.                              |
| Outcome evaluator                 | “Helpful” must be measurable.                                      | Define user, automated, and task-success signals; distinguish absent feedback from negative feedback.  |
| Artifact contracts and validators | The engine cannot determine “complete” from prose or traces alone. | Define required outputs and machine-checkable acceptance tests for repeatable deliverables.            |
| Deletion and consent              | Observations create privacy obligations.                           | Support purpose, consent, retention, erasure, and audit records from day one.                          |
| Frame replay                      | Debugging without historical frames is guesswork.                  | Persist immutable frame manifests and versions of policies/retrievers.                                 |

### 15.2 Strong architectural differentiators

1. **Context as a compiled, replayable artifact.** Treat each frame like a build artifact with source inputs, compiler version, policy version, token budget, output manifest, and evaluation outcome. This is far stronger than “vector search plus prompt assembly.”

2. **Provenance and time as first-class query dimensions.** Bi-temporal memory plus evidence spans lets users ask: “What did the system believe last Tuesday, why, and what changed?” This is unusually valuable in engineering, enterprise, and regulated workflows.

3. **Policy-aware retrieval instead of post-hoc guardrails.** Resolve constraints before recall, ranking, or action. That prevents prohibited context from shaping the response at all, rather than trying to filter it after the fact.

4. **Learning that is conservative and reversible.** Observations become inferred directives only after repeated evidence; inferences expire; negative citation outcomes prune them; explicit user direction overrides them. This produces personalization without an opaque “AI personality profile.”

5. **Code map linked to decisions and outcomes.** Connect symbols, dependencies, owners, tests, deploys, incidents, directives, and evidence. This enables impact-aware coding agents that can explain not only *what* changed but *why this code was selected*.

6. **Utility-per-token optimization.** Rank candidates by expected task utility, trust, novelty, and policy importance per token. Measure whether each item improved the result. Over time, the system learns to produce smaller, better frames.

7. **Explicit context deltas.** For long-running and multi-agent tasks, send frame changes (`added`, `removed`, `superseded`, `invalidated`) rather than rebuilding and re-explaining the world on every turn.

8. **User-visible control plane.** Let users inspect, correct, pin, unpin, expire, and delete directives and inferred preferences, with a simple explanation of why each appeared. Trust and controllability become product advantages.

9. **Contract-backed completion.** For recurring work, distinguish “the agent produced an answer” from “the requested artifact satisfies a versioned acceptance contract.” This turns repeated user corrections into durable, testable product knowledge.

### 15.3 Recommended implementation order

1. Entity registry, scope, authorization, and evidence records.
2. Directives with lifecycle, conflict resolution, and citation events.
3. Bi-temporal memory with immutable versions and corrections.
4. Task state and replayable frame manifests.
5. Context compiler with hybrid retrieval and token budgeting.
6. Code-map integration.
7. Observation ingestion, consent controls, and conservative promotion.
8. Outcome evaluation, pruning, and learned retrieval ranking.

## 16. Artifact contracts, trace mining, and safe self-improvement

### 16.1 Why traces alone are insufficient

Agent traces, working logs, tool output, and Git diffs are valuable
evidence: they reveal rework, failed commands, repeated user corrections,
missing files, and abandoned plans. They do **not**, by themselves,
establish that an answer or artifact was inaccurate or incomplete. A
trace can show that the agent created four image files; only an explicit
expectation can establish whether it should have created six files, a
manifest, source files, or a particular directory structure.

For repeated deliverables, capture that expectation as an **Artifact Contract**. The contract is the reference against which outputs are validated and from which future agents retrieve requirements.

### 16.2 Artifact Contract schema

An artifact contract is a versioned, reusable acceptance specification.
It is not another directive kind: it is structured data that a `procedure`,
`rule`, or task can reference. Its requirements should be represented in
ways validators can check without relying on model judgment.

```ts
type ContractRequirement = {
  id: ID
  description: string
  severity: "required" | "recommended"
  validator: {
    type:
      | "file_exists"
      | "file_count"
      | "path_matches"
      | "mime_type"
      | "image_dimensions"
      | "json_schema"
      | "text_contains"
      | "git_diff_check"
      | "command"
      | "human_review"
    config: Record<string, unknown>
  }
}

type ArtifactContract = {
  id: ID
  name: string
  version: string
  applies_to: {
    task_kinds?: string[]
    entity_ids?: ID[]
    scope?: Scope
  }
  description: string
  requirements: ContractRequirement[]
  output_layout?: {
    root: string
    required_paths: string[]
  }
  evidence_ids?: ID[]
  source: "user" | "system" | "imported"
  status: "active" | "superseded" | "archived"
  supersedes?: ID[]
  created_at: ISODateTime
  updated_at: ISODateTime
}

type ContractValidation = {
  id: ID
  contract_id: ID
  task_id: ID
  artifact_refs: Reference[]
  status: "passed" | "failed" | "needs_review" | "not_run"
  results: Array<{
    requirement_id: ID
    status: "passed" | "failed" | "skipped" | "needs_review"
    actual?: unknown
    message?: string
    evidence_ids?: ID[]
  }>
  validated_at: ISODateTime
  validator_version: string
}
```

### 16.3 Brand kit example

This is the missing bridge for the brand-kit problem: the expected shape is
saved once, retrieved on every relevant task, and verified before the agent
claims completion.

```json
{
  "id": "contract_brand_kit_acme_v1",
  "name": "Acme brand-kit delivery",
  "version": "1.0.0",
  "applies_to": {
    "task_kinds": ["brand_kit_generation"],
    "scope": { "workspace_id": "workspace_acme" }
  },
  "description": "A complete brand kit must use the approved folder layout, contain all required exports, and include a machine-readable manifest.",
  "output_layout": {
    "root": "outputs/brand-kit",
    "required_paths": [
      "outputs/brand-kit/manifest.json",
      "outputs/brand-kit/logos/primary.svg",
      "outputs/brand-kit/logos/primary.png",
      "outputs/brand-kit/colors/palette.json",
      "outputs/brand-kit/typography/type-scale.md",
      "outputs/brand-kit/usage/brand-guidelines.pdf"
    ]
  },
  "requirements": [
    {
      "id": "req_manifest",
      "description": "A manifest exists and conforms to the brand-kit manifest schema.",
      "severity": "required",
      "validator": {
        "type": "json_schema",
        "config": {
          "path": "outputs/brand-kit/manifest.json",
          "schema": "brand-kit-manifest/v1"
        }
      }
    },
    {
      "id": "req_layout",
      "description": "Every required path is present.",
      "severity": "required",
      "validator": {
        "type": "file_exists",
        "config": {
          "paths": [
            "outputs/brand-kit/logos/primary.svg",
            "outputs/brand-kit/logos/primary.png",
            "outputs/brand-kit/colors/palette.json",
            "outputs/brand-kit/typography/type-scale.md",
            "outputs/brand-kit/usage/brand-guidelines.pdf"
          ]
        }
      }
    },
    {
      "id": "req_primary_logo_png",
      "description": "The primary PNG logo has the required transparent raster export dimensions.",
      "severity": "required",
      "validator": {
        "type": "image_dimensions",
        "config": {
          "path": "outputs/brand-kit/logos/primary.png",
          "width": 2048,
          "height": 2048,
          "alpha": true
        }
      }
    },
    {
      "id": "req_guidelines_review",
      "description": "Guidelines use the approved brand voice and visual direction.",
      "severity": "recommended",
      "validator": {
        "type": "human_review",
        "config": {
          "rubric": "brand-kit-guidelines/v1"
        }
      }
    }
  ],
  "source": "user",
  "status": "active",
  "created_at": "2026-07-20T18:00:00Z",
  "updated_at": "2026-07-20T18:00:00Z"
}
```

The example names illustrative files and requirements only. In the real system,
the user defines the authoritative shape once, reviews it, and can version it
as the brand process evolves.

### 16.4 Execution trace schema

Store structured execution traces rather than unbounded hidden reasoning. A
trace describes observable actions, inputs, outputs, decisions, validations,
and errors; it can safely point to protected raw logs where access is permitted.

```ts
type AgentTrace = {
  id: ID
  task_id: ID
  frame_id: ID
  agent_id: ID
  started_at: ISODateTime
  finished_at?: ISODateTime
  status: "completed" | "failed" | "blocked" | "cancelled"
  actions: Array<{
    id: ID
    kind: "tool_call" | "file_change" | "git_diff" | "artifact_created" | "validation" | "user_feedback"
    summary: string
    started_at: ISODateTime
    finished_at?: ISODateTime
    input_refs?: Reference[]
    output_refs?: Reference[]
    evidence_ids?: ID[]
    status: "succeeded" | "failed" | "skipped"
  }>
  contract_validation_ids?: ID[]
  outcome_id?: ID
  log_evidence_ids?: ID[]
  git_diff_evidence_ids?: ID[]
  created_at: ISODateTime
}

type TaskOutcome = {
  id: ID
  task_id: ID
  frame_id: ID
  status: "successful" | "partially_successful" | "unsuccessful" | "unevaluated"
  contract_validation_ids?: ID[]
  user_feedback?: "positive" | "negative" | "none"
  user_feedback_text?: string
  evaluator: "user" | "system" | "model" | "human_reviewer"
  evaluated_at: ISODateTime
  evidence_ids?: ID[]
}
```

### 16.5 Trace-mining pipeline

The engine may mine traces, logs, Git diffs, artifacts, validations, and user
corrections to propose better context. It must treat the result as a **candidate**,
not an automatic truth.

```text
Trace / logs / diff / validation / user correction
                     │
                     ▼
        Detect a recurring, explainable pattern
                     │
                     ▼
       Create evidence-backed candidate directive
                     │
                     ▼
  Deduplicate, scope, risk-score, and check conflicts
                     │
                     ▼
     Auto-promote only low-risk validated candidates
       or request user / reviewer confirmation
                     │
                     ▼
  Cite in future frames → evaluate usefulness → retain, revise, or archive
```

```ts
type CandidateDirective = {
  id: ID
  proposed: Omit<Directive, "id" | "status" | "citation_stats" | "created_at" | "updated_at">
  reason: string
  source_trace_ids: ID[]
  evidence_ids: ID[]
  pattern: {
    occurrences: number
    window_days: number
    signal: "repeated_user_correction" | "contract_failure" | "rework" |
            "git_revert" | "tool_failure" | "positive_outcome"
  }
  risk: "low" | "medium" | "high"
  promotion: "auto_eligible" | "review_required" | "rejected"
  created_at: ISODateTime
}
```

Candidate examples:

- A brand-kit contract fails three times because `manifest.json` is missing: propose a high-priority `procedure` directive to validate the active contract before delivery, and retain the failure evidence.
- A user repeatedly reorganizes generated assets into the same folders: propose a scoped, low-priority preference or an update to the brand-kit contract; request confirmation before making it authoritative.
- A Git diff repeatedly adds the same retry wrapper after an agent initially omits it: propose a code-scoped memory or procedure tied to the affected service and tests.
- A user says “you forgot the SVG export” twice: propose a contract requirement, not merely a natural-language preference.

### 16.6 Promotion policy

- **Never automatically promote** a candidate to a `constraint`, security rule, legal policy, or irreversible procedure.
- **Never infer** sensitive attributes or consequential preferences from traces or observations.
- Auto-promotion is acceptable only for low-risk, narrow-scope, evidence-backed candidates with deterministic validation or repeated explicit user correction, and only when the active governance mode permits it.
- In `solo` mode, a qualifying candidate may become an advisory rule at the configured confidence threshold; surface a one-click `keep`, `edit`, or `ignore` action and do not block work.
- In `team` mode, repository-scoped candidates become proposed Context PRs routed to an owner; they are not published until approved.
- In `regulated` mode, inferred candidates remain proposals until the required explicit approval and audit steps complete.
- Require confirmation or owner approval for new output shapes, folder conventions, visual brand choices, or other subjective deliverable requirements unless they are already present in a reviewed contract.
- Link every promoted directive to the trace, diff, validation, observation, or user message that supports it.
- Give inferred promotions an expiry date, lower priority, and a clear UI path to inspect, edit, or delete them.
- Negative outcome signals and repeated not-helpful citations must reduce confidence, trigger review, or archive the promotion.

### 16.7 What the system can know

| Question                                                 | Can the engine detect it?                             | How                                                                                        |
| -------------------------------------------------------- | ----------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| “Did the agent omit a required file?”                    | Yes, reliably.                                        | Artifact contract and `file_exists` validation.                                            |
| “Did it create the wrong directory layout or file type?” | Yes, reliably.                                        | Path, MIME, manifest, and schema validation.                                               |
| “Did it change the wrong code or miss a test?”           | Often.                                                | Code-map impact analysis, Git diff policy, tests, and contract checks.                     |
| “Was the response factually inaccurate?”                 | Sometimes.                                            | Evidence checks, trusted sources, contradiction detection, and domain-specific validators. |
| “Was the visual identity good?”                          | Partly.                                               | Rubric-based review and user feedback; retain human judgment for subjective quality.       |
| “Did the user consider the deliverable complete?”        | Yes, when they provide feedback or accept the result. | Explicit acceptance/rejection and task outcome capture.                                    |

This is self-improvement at the **context and workflow layer**, rather than unbounded self-training. From the end-user perspective, future agents become more consistent because they retrieve the approved contract, follow the relevant procedure, validate the result, and learn only through evidence-backed, reversible updates.

### 16.8 Adaptive governance and promotion ladder

Governance is a deployment policy, not a different graph or event model. The same observations, candidate directives, evidence, promotion events, and directive records work for an individual, a team, or a regulated organization. Only the promotion and sharing policy changes.

```ts
type GovernanceMode = "solo" | "team" | "regulated"

type GovernanceConfig = {
  mode: GovernanceMode
  promotion: {
    inferred_rule: {
      min_observations: number
      auto_publish_at_confidence: number // 0–100
      initial_enforcement: "advisory" | "blocking"
    }
    blocking_rule: {
      requires_explicit_confirmation: boolean
    }
  }
  sharing: {
    personal_default: "local_only"
    repository_rule_requires_owner_approval: boolean
    organization_rule_requires_explicit_approval: boolean
  }
  storage: {
    local_first: boolean
    encrypted_cloud_backup: "disabled" | "optional" | "required"
    snapshots: "disabled" | "gitignored_local" | "versioned"
  }
}

type PromotionEvent = {
  id: ID
  candidate_directive_id: ID
  from_status: "draft" | "proposed" | "advisory" | "confirmed" | "rejected"
  to_status: "draft" | "proposed" | "advisory" | "confirmed" | "rejected"
  sharing_scope: SharingScope
  governance_mode: GovernanceMode
  actor_id?: ID
  reason: string
  evidence_ids: ID[]
  created_at: ISODateTime
}
```

Recommended defaults:

```yaml
governance:
  mode: solo # solo | team | regulated

promotion:
  inferred_rule:
    min_observations: 3
    auto_publish_at_confidence: 85
    initial_enforcement: advisory
  blocking_rule:
    requires_explicit_confirmation: true

sharing:
  personal_default: local_only
  repository_rule_requires_owner_approval: false
  organization_rule_requires_explicit_approval: true

storage:
  local_first: true
  encrypted_cloud_backup: optional
  snapshots: gitignored_local
```

#### Solo mode

The default path is:

```text
Observation → personal draft → automatic advisory rule → confirmed rule
```

At least `min_observations` consistent observations within the configured
window can produce a low-risk candidate. If confidence reaches the threshold,
the system may publish it as an `advisory` rule without a mandatory review
workflow. Stella should provide a one-click, user-visible action such as:

> “I’ve observed this three times: you add an integration test whenever this route changes. I’ll treat it as an advisory preference going forward. [Keep] [Edit] [Ignore]”

`Keep` confirms or maintains the rule, `Edit` creates a new version with the
user's wording or scope, and `Ignore` rejects the candidate and records the
negative outcome. Advisory rules may guide retrieval and recommendations, but
cannot block an action. A blocking rule always requires explicit confirmation.

Solo mode should be local-first and usable without an account or server:

```text
.stella/
  context-rules.yaml       # approved durable repository steering
  context-observations.db  # local evidence and inferred candidates
  context-snapshots/       # optional, normally gitignored
  config.yaml              # governance and sharing settings
```

Use local SQLite or DuckDB for the graph, event, and vector indexes. Keep raw
telemetry and personal preferences local by default; support optional encrypted
backup/sync. Snapshots are cached explorations used to reduce duplicate model
calls and token usage, not authoritative knowledge records.

#### Team mode

The default path is:

```text
Observation → proposed Context PR → owner approval → published rule
```

The “Context PR” is a reviewable promotion event. It may be represented by a
normal Git change, a UI proposal, or both. Repository-scoped rules route to a
repository owner or code owner. Personal preferences remain private and are
never shared automatically. Organization rules require the organization's
policy workflow.

#### Regulated mode

Regulated mode disables automatic publication of inferred steering, requires
explicit approval for blocking or consequential rules, preserves immutable
audit records, and applies the strictest retention, access, residency, and
review policies. A system or critical constraint must never be silently changed
by trace mining.

#### Sharing scopes

Every durable directive and artifact contract should have an explicit sharing
scope, even when the storage layer is local:

| Sharing scope | Example | Default sharing |
| --- | --- | --- |
| `personal` | “Prefer concise status updates.” | Never shared automatically. |
| `repository` | “Run integration tests for API changes.” | Shared when approved under repository governance. |
| `organization` | “Never expose PII in logs.” | Inherited from organization policy; explicit approval required. |

When a second developer joins, detect multiple active identities, offer to
promote selected repository rules from local to shared, convert local evidence
into proposals rather than enforced policy, and enable owner routing once
maintainers or code owners exist. No data model migration is required; only
the governance mode, sharing policy, and promotion workflow change.

#### Git-backed rules

For repository sharing, approved rules should be understandable through normal
Git workflows. A source-controlled `.stella/context-rules.yaml` can contain:

```yaml
rules:
  - id: api-integration-coverage
    kind: procedure
    sharing_scope: repository
    statement: API endpoint changes require integration coverage.
    status: active
    promotion_status: confirmed
    enforcement: advisory
    confidence: 91
    evidence:
      - source: observed_pattern
        occurrences: 5
```

If the user prefers zero repository churn, approved rules remain local until
they explicitly choose `share_with_repository`. A Context PR is therefore not
necessarily a pull request: it is the promotion of an observation into durable
steering, with review intensity determined by governance mode.

## 17. Explicit non-goals

- Do not treat raw agent reasoning as durable memory.
- Do not use observations as silent user instructions.
- Do not let low-confidence inferences override explicit directives.
- Do not delete historical knowledge merely because a newer version exists; supersede and retain it subject to retention policy.
- Do not rely on embedding similarity alone for policy, temporal, code, or identity-sensitive retrieval.
- Do not let the model persist context items without validation and provenance.
