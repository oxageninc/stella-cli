# Directive schema

A **Directive** is the single, typed unit of information in the context engine. It represents information that may affect an agent's decisions or behavior without calling the unit itself "context."

The supported directive types are:

| Type | Use it for |
| --- | --- |
| `memory` | A retained observation or past interaction. |
| `fact` | A statement believed to be true about a subject or environment. |
| `rule` | A general behavioral instruction. |
| `preference` | A non-mandatory desired behavior or style. |
| `constraint` | A hard limit, prohibition, or required condition. |
| `procedure` | A defined sequence of steps to accomplish something. |

End-user behaviors are intentionally **not** a directive type. They are observations that may provide evidence for a future inferred directive, but are not instructions by themselves.

## Type definition

```ts
type Directive = {
  id: string
  kind:
    | "memory"
    | "fact"
    | "rule"
    | "preference"
    | "constraint"
    | "procedure"

  subject?: string
  statement: string
  value?: unknown

  priority: "low" | "normal" | "high" | "critical"
  confidence?: number
  source: "user" | "system" | "inferred" | "imported"
  status: "active" | "superseded" | "archived"

  citation_stats: {
    cited_at: string[]
    cited_count: number
    helpful_count: number
    not_helpful_count: number
    last_cited_at?: string
    last_helpful_at?: string
    last_not_helpful_at?: string
  }

  scope?: {
    workspace_id?: string
    project_id?: string
    session_id?: string
  }

  created_at: string
  updated_at: string
  expires_at?: string
}
```

## Field guide

- `id`: Stable unique identifier.
- `kind`: The directive's semantic category.
- `subject`: The entity the directive concerns, such as `user`, `agent`, or `project:apollo`.
- `statement`: Required human- and model-readable representation of the directive. Keep it clear and self-contained.
- `value`: Optional structured payload for retrieval, validation, execution, or UI rendering.
- `priority`: Its relative importance when directives conflict. A `constraint` will normally be `high` or `critical`.
- `confidence`: A number from `0` to `1`, especially useful for inferred or uncertain information.
- `source`: Where the directive came from.
- `status`: `active` directives may be used; preserve replaced information with `superseded` rather than deleting it when provenance matters.
- `citation_stats`: Audit and quality signals from retrieval. `cited_at` records every time the directive was included in an agent's working context; the counts track citations judged helpful or not helpful. `last_cited_at`, `last_helpful_at`, and `last_not_helpful_at` support recency-based pruning without scanning every event.
- `scope`: Narrows applicability. Omit it for a global directive.
- `created_at` and `updated_at`: ISO 8601 timestamps.
- `expires_at`: Optional time after which the directive should no longer apply.

## Examples

### Memory

```json
{
  "id": "dir_mem_001",
  "kind": "memory",
  "subject": "user",
  "statement": "The user previously chose PostgreSQL for the analytics service.",
  "value": {
    "topic": "analytics-database",
    "chosen": "PostgreSQL"
  },
  "priority": "normal",
  "confidence": 1,
  "source": "user",
  "status": "active",
  "citation_stats": {
    "cited_at": ["2026-07-20T16:05:00Z"],
    "cited_count": 1,
    "helpful_count": 1,
    "not_helpful_count": 0,
    "last_cited_at": "2026-07-20T16:05:00Z",
    "last_helpful_at": "2026-07-20T16:05:00Z"
  },
  "scope": { "project_id": "analytics" },
  "created_at": "2026-07-20T16:00:00Z",
  "updated_at": "2026-07-20T16:00:00Z"
}
```

### Fact

```json
{
  "id": "dir_fact_001",
  "kind": "fact",
  "subject": "project:analytics",
  "statement": "The analytics API is deployed in the us-west-2 region.",
  "value": {
    "service": "analytics-api",
    "region": "us-west-2"
  },
  "priority": "normal",
  "confidence": 1,
  "source": "imported",
  "status": "active",
  "citation_stats": {
    "cited_at": ["2026-07-20T16:10:00Z"],
    "cited_count": 1,
    "helpful_count": 1,
    "not_helpful_count": 0,
    "last_cited_at": "2026-07-20T16:10:00Z",
    "last_helpful_at": "2026-07-20T16:10:00Z"
  },
  "scope": { "project_id": "analytics" },
  "created_at": "2026-07-20T16:00:00Z",
  "updated_at": "2026-07-20T16:00:00Z"
}
```

### Rule

```json
{
  "id": "dir_rule_001",
  "kind": "rule",
  "subject": "agent",
  "statement": "Explain a proposed production change before making it.",
  "value": {
    "when": "before_production_change",
    "action": "explain_change"
  },
  "priority": "high",
  "confidence": 1,
  "source": "system",
  "status": "active",
  "citation_stats": {
    "cited_at": [],
    "cited_count": 0,
    "helpful_count": 0,
    "not_helpful_count": 0
  },
  "created_at": "2026-07-20T16:00:00Z",
  "updated_at": "2026-07-20T16:00:00Z"
}
```

### Preference

```json
{
  "id": "dir_pref_001",
  "kind": "preference",
  "subject": "user",
  "statement": "The user prefers concise answers with minimal formatting.",
  "value": {
    "verbosity": "low",
    "formatting": "minimal"
  },
  "priority": "normal",
  "confidence": 1,
  "source": "user",
  "status": "active",
  "citation_stats": {
    "cited_at": ["2026-07-20T16:12:00Z"],
    "cited_count": 1,
    "helpful_count": 1,
    "not_helpful_count": 0,
    "last_cited_at": "2026-07-20T16:12:00Z",
    "last_helpful_at": "2026-07-20T16:12:00Z"
  },
  "created_at": "2026-07-20T16:00:00Z",
  "updated_at": "2026-07-20T16:00:00Z"
}
```

### Constraint

```json
{
  "id": "dir_constraint_001",
  "kind": "constraint",
  "subject": "agent",
  "statement": "Do not send customer data to third-party services without explicit approval.",
  "value": {
    "prohibited_action": "send_customer_data_to_third_party",
    "unless": "explicit_approval"
  },
  "priority": "critical",
  "confidence": 1,
  "source": "system",
  "status": "active",
  "citation_stats": {
    "cited_at": [],
    "cited_count": 0,
    "helpful_count": 0,
    "not_helpful_count": 0
  },
  "created_at": "2026-07-20T16:00:00Z",
  "updated_at": "2026-07-20T16:00:00Z"
}
```

### Procedure

```json
{
  "id": "dir_proc_001",
  "kind": "procedure",
  "subject": "project:analytics",
  "statement": "Before deploying, run tests, review failures, request approval, then deploy.",
  "value": {
    "steps": [
      { "order": 1, "action": "run_test_suite" },
      { "order": 2, "action": "review_failures" },
      { "order": 3, "action": "request_deployment_approval" },
      { "order": 4, "action": "deploy" }
    ]
  },
  "priority": "high",
  "confidence": 1,
  "source": "user",
  "status": "active",
  "citation_stats": {
    "cited_at": [],
    "cited_count": 0,
    "helpful_count": 0,
    "not_helpful_count": 0
  },
  "scope": { "project_id": "analytics" },
  "created_at": "2026-07-20T16:00:00Z",
  "updated_at": "2026-07-20T16:00:00Z"
}
```

## Suggested resolution behavior

Use `kind` to interpret a directive, not as the sole conflict rule. A practical default is:

1. Apply active directives in matching scope.
2. Favor higher `priority` directives.
3. Treat `constraint` as a hard boundary.
4. Treat `rule` as a general instruction and `procedure` as an ordered workflow.
5. Use `preference` when it does not conflict with a higher-priority directive.
6. Favor higher-confidence facts and memories; retain provenance through `source` and `status`.

## Citation and pruning requirements

### Record citation outcomes

Whenever a directive is included in the context delivered to an agent, record the citation timestamp in `citation_stats.cited_at`, increment `cited_count`, and update `last_cited_at`.

After the agent finishes, evaluate whether that citation helped. The outcome may come from an explicit user rating, an evaluator, or an application-defined success signal. Increment exactly one of `helpful_count` or `not_helpful_count`, and set its corresponding `last*At` field. Do not count a directive as helpful merely because it was retrieved.

For high-volume systems, store complete citation events in a separate append-only event table and keep only a bounded recent timestamp list plus the aggregate counters on the directive. This keeps the main directive record compact while preserving auditability.

### Automatic pruning

Run a scheduled lifecycle job that evaluates only `active` directives. The job should normally **archive**, rather than delete, a directive so it can be inspected or restored later.

- Archive an eligible directive for poor usefulness when it has at least `5` evaluated citations and `not_helpful_count / (helpful_count + not_helpful_count) >= 0.8`.
- Do not automatically prune `critical` directives or directives whose `source` is `system`; send them for review instead.
- Archive a directive after `expires_at` has passed, unless it has been renewed by updating `expires_at`.
- Mark a non-system directive as stale for review when it has not been cited or updated for a configured age threshold (for example, 180 days). Archive it after a longer grace period (for example, 30 additional days) if it remains unused.
- For inferred facts and memories, lower `confidence` or archive them sooner when they receive repeated not-helpful outcomes.

An application may keep these thresholds as engine-level configuration:

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

## End-user observations

The engine should capture potentially helpful user behavior as a separate `Observation` record. An observation can also represent agent, system, tool, or environmental events. It supports personalization and retrieval without treating a one-off action as a durable preference, fact, or rule.

```ts
type Observation = {
  id: string
  subject: string
  event: string
  observed_at: string
  scope?: {
    workspace_id?: string
    project_id?: string
    session_id?: string
  }
  attributes?: Record<string, unknown>
  source: "observed" | "imported"
  expires_at?: string
  privacy: "essential" | "personalization" | "sensitive"
}
```

End-user behavior is a subset of observations. Example: a behavior the engine may later infer as a preference.

```json
{
  "id": "behavior_001",
  "subject": "user",
  "event": "response_format_selected",
  "observed_at": "2026-07-20T16:30:00Z",
  "scope": { "workspace_id": "acme" },
  "attributes": {
    "format": "markdown",
    "verbosity": "concise"
  },
  "source": "observed",
  "expires_at": "2026-10-18T16:30:00Z",
  "privacy": "personalization"
}
```

### Promotion requirements

- An observation may inform retrieval immediately only when it is in scope and has not expired.
- Do not promote one observation into a directive. Promote only repeated, consistent signals within a defined time window.
- A promoted directive must use `source: "inferred"`, include an appropriate `confidence`, and preserve links to its underlying observations in `value` or an external provenance record.
- Explicit user directives always override behavioral inferences. For example, a user's stated preference overrides a pattern of observed clicks.
- Never infer sensitive attributes, identity, health, protected characteristics, or consequential preferences from behavioral signals.
- Collect only observations needed for a defined product purpose, respect consent and retention settings, and expire raw observations sooner than their promoted directives.
- If an inferred directive is repeatedly cited as not helpful, lower its confidence or archive it using the lifecycle policy above.

Example promotion after a sustained pattern:

```json
{
  "id": "dir_pref_inferred_001",
  "kind": "preference",
  "subject": "user",
  "statement": "The user appears to prefer concise Markdown responses.",
  "value": {
    "verbosity": "low",
    "format": "markdown",
    "evidence": {
      "observation_ids": ["behavior_001", "behavior_014", "behavior_027"],
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
  "scope": { "workspace_id": "acme" },
  "created_at": "2026-07-20T16:30:00Z",
  "updated_at": "2026-07-20T16:30:00Z",
  "expires_at": "2026-10-18T16:30:00Z"
}
```
