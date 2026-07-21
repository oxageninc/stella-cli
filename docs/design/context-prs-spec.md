# Context Pull Requests — Feature Specification

**Status:** Proposed  
**Owner:** Stella Platform  
**Primary principle:** Git is the authoritative, human-governed policy ledger. The context graph is a derived runtime index.

## 1. Problem

Teams accumulate durable engineering knowledge in repository instructions, code-review habits, incidents, architecture decisions, and repeated implementation patterns. Agents and developers currently rediscover that knowledge turn by turn. Local context graphs improve an individual CLI but fragment organizational learning.

Context Pull Requests (Context PRs) turn sufficiently justified, durable context into reviewable code. They use existing pull-request workflows to create, amend, approve, audit, roll back, and distribute machine-readable steering.

This feature must work for both:

- a single developer and a single repository, with nearly no ceremony; and
- an organization with many developers and repositories, with ownership, scope, access control, and auditability.

## 2. Goals and non-goals

### Goals

- Make durable context reviewable and versioned through Git.
- Preserve evidence and explain why a rule exists and when it applies.
- Compile merged context into low-latency, permission-aware context graph slices.
- Support local repository, team, and organization-level steering.
- Keep raw telemetry and personal preferences private by default.
- Support safe evolution from observe → advisory → required → blocking enforcement.
- Make the smallest useful diff; never create policy churn merely because a pattern was observed.

### Non-goals

- Replacing code review, issue tracking, incident management, or architectural decision records.
- Automatically publishing inferred policy without a configured auto-promotion policy.
- Storing full prompts, unredacted tool logs, or sensitive customer data in Git.
- Using Context PRs to evaluate individual developer productivity.
- Requiring a centralized service for an individual developer’s local-first use case.

## 3. Terminology

| Term | Meaning |
|---|---|
| Observation | A time-bounded piece of evidence: an instruction, merged change pattern, correction, incident, or explicit user preference. |
| Candidate | A deduplicated hypothesis that might become durable steering. Not yet authoritative. |
| Context rule | A versioned, declarative policy statement that can be retrieved and optionally evaluated. |
| Context PR | A standard Git pull request that changes context-as-code files. |
| Evidence | The attributable sources supporting a candidate or rule. |
| Runtime graph | A derived, query-optimized representation of merged rules and allowed metadata. |
| Context slice | The exact, bounded set of rules and graph nodes supplied to an agent for one task. |
| Enforcement | The behavior when a rule matches: observe, advisory, required, or blocking. |

## 4. Product thesis

```
Local signals + repository artifacts
          ↓
Candidate aggregation and evidence scoring
          ↓
Real Git Context PR (the governed source of truth)
          ↓
Merge / revert / amend through normal repository controls
          ↓
Indexer compiles policy files into a shared runtime graph
          ↓
CLI, agents, and CI retrieve a small authorized context slice
```

The graph never silently outranks Git. A graph node sourced from a merged rule carries the commit SHA and source path. A graph node based only on observation remains non-authoritative.

## 5. Where Context PRs live

### Repository-local policy

Rules that describe one codebase live in that repository. This puts review with the people who own the affected code.

```text
payments-api/
  .stella/
    context-policy.yaml
    ownership.yaml
    rules/
      api.yaml
      testing.yaml
      security.yaml
      architecture.yaml
```

### Shared policy registry

Rules that intentionally span repositories live in a dedicated `stella-context` repository. It is not a second source of truth for local rules; it is the source for organization and team policy.

```text
stella-context/
  organization/
    security.yaml
    data-handling.yaml
  teams/
    payments.yaml
  repositories/
    payments-api.yaml        # registry-owned metadata only
  schemas/
    context-rule.schema.json
```

### Scope precedence

At runtime, applicable rules are merged by specificity and authority:

```text
Explicit current-turn instruction
  > repository directory/file rule
  > repository rule
  > team rule
  > organization rule
  > approved personal preference
  > inferred, non-authoritative suggestion
```

Conflicting rules at equal precedence are never silently resolved: the context slice includes a conflict record, and enforcement falls back to the less restrictive behavior until an owner resolves it.

## 6. Context-as-code format

Rules are grouped by stable domain, not one file per rule. Grouping minimizes diff churn and makes related policy readable together.

```yaml
# .stella/rules/testing.yaml
version: 1
rules:
  - id: payments-api-integration-coverage
    kind: repository_rule
    statement: API endpoint changes require integration coverage.
    scope:
      repository: payments-api
      include_paths: ["src/api/**", "routes/**"]
      exclude_paths: ["generated/**", "docs/**"]
    policy:
      when:
        any_changed_path_matches: ["src/api/**", "routes/**"]
      require:
        any_changed_path_matches: ["tests/integration/**"]
    enforcement: advisory
    enforcement_weight: 80
    confidence: 94
    true_since: "2025-10-14"
    evidence:
      - type: repository_instruction
        source: AGENTS.md
        locator: "Testing"
      - type: merged_change_pattern
        population: 16
        matching: 14
        query_ref: "evidence://payments-api/api-test-pattern/v3"
      - type: incident
        references: ["INC-482", "INC-519"]
    owners: ["team:payments-platform"]
    review:
      last_reviewed_at: "2026-07-20"
      review_after: "2027-01-20"
```

Required fields: `id`, `kind`, `statement`, `scope`, `enforcement`, `confidence`, and at least one source-backed `evidence` item. The schema rejects secrets, raw prompts, PII fields, and unknown rule keys.

## 7. Candidate generation

### Sources

- Explicit repository artifacts: `AGENTS.md`, `CLAUDE.md`, contributor docs, CI configuration, code owners, architecture records.
- Repeated accepted change patterns: merged PRs and their test/code relationships.
- Explicit human correction: “always do this,” “this is repo policy,” or “do not infer this.”
- Incident and postmortem outcomes, only when linked to a clear preventive rule.
- Tool outcomes: repeated review feedback or validation failures.

### Candidate requirements

The candidate service must deduplicate by a stable semantic fingerprint: normalized rule intent + scope + policy condition. It must aggregate independent evidence rather than count repeated variants of one PR as independent confirmation.

Suggested proposal thresholds:

| Evidence type | May create candidate | May open Context PR |
|---|---:|---:|
| Explicit instruction in tracked repo file | Yes | Immediately, if source does not already have equivalent structured rule |
| Explicit owner request | Yes | Immediately |
| Repeated accepted pattern | Yes | ≥8 independent examples, ≥75% consistency, confidence ≥80 |
| Incident | Yes | Only with a narrowly stated proposed prevention and owner route |
| One agent decision or one user behavior | Yes | Never alone |

Candidates must include counterexamples and uncertainty. “14 of 16 matched” is materially better than “observed 14 times.”

## 8. Creating a Context PR

### Preconditions

Before creating a branch, Stella must be able to:

1. identify the target repository or registry repository;
2. produce a schema-valid minimal diff;
3. identify the proposed scope and likely owners;
4. attach evidence references and confidence calculation;
5. explain expected runtime effect; and
6. confirm that no restricted raw data is added to Git.

If any precondition is missing, Stella creates a candidate only. It must not manufacture a PR with vague policy.

### Branch and PR

```text
Branch: stella/context/payments-api-integration-coverage
Title: context(payments-api): require integration coverage for API changes
Labels: context, proposed-policy, testing
```

PR body template:

```md
## Proposed steering
API endpoint changes require integration coverage.

## Scope
`payments-api`: `src/api/**`, `routes/**`

## Suggested enforcement
Advisory

## Evidence
- `AGENTS.md` — Testing section
- 14 / 16 comparable merged PRs added integration coverage
- INC-482 and INC-519 were endpoint regressions without coverage

## Expected runtime effect
Agents modifying matching paths will be prompted to add or update integration tests.

## Alternatives considered
- No rule: rejected because an explicit repo instruction already exists.
- Blocking CI check: deferred until observe-mode accuracy is established.
```

The PR diff and body are independently generated from the candidate record so review remains legible even if evidence links become unavailable.

## 9. Review and governance

Use the host Git provider’s existing branch protection, CODEOWNERS, approvals, comments, checks, merge queue, and revert mechanics.

`ownership.yaml` optionally maps rule kinds and paths to required reviewers:

```yaml
rules:
  repository_rule:
    default_reviewers: ["team:payments-platform"]
  security_constraint:
    required_reviewers: ["team:security"]
  architecture_constraint:
    required_reviewers: ["role:principal-engineer"]
enforcement:
  blocking:
    min_approvals: 2
    required_reviewers: ["team:platform"]
```

Review actions:

- **Approve:** merge under normal branch protection.
- **Edit:** change wording, scope, policy DSL, evidence, owners, or enforcement in the PR.
- **Reject:** close with a structured reason (`one_time_pattern`, `insufficient_evidence`, `wrong_scope`, `duplicate`, `not_policy`).
- **Defer:** retain the candidate and ask for more observations by a date or event.
- **Supersede:** point to a replacement rule or PR.
- **Revert:** revert the merged commit; the indexer removes the rule after the revert is observed.

Rejected candidates are retained with reason and expiry so Stella does not propose the same weak rule repeatedly.

## 10. Solo-developer mode

The data model is identical; only promotion and hosting change.

- Default to local graph + repository-local `.stella/` files.
- A developer can accept an advisory rule with one action, which creates an ordinary commit instead of a PR.
- Personal preferences remain outside the repository unless explicitly promoted.
- Rules may be maintained locally with no remote service; optional encrypted sync backs up candidates and preferences.
- When a second active contributor is detected, offer—not force—migration to review-required repository policy.

```yaml
# .stella/context-policy.yaml
governance:
  mode: solo # solo | team | regulated
promotion:
  inferred_rule:
    min_observations: 3
    auto_publish_at_confidence: 85
    initial_enforcement: advisory
  blocking_rule:
    requires_explicit_confirmation: true
```

In solo mode, “Context PR” names the conceptual promotion. In the UI it should read “Add rule to this repository” unless a remote PR will actually be created.

## 11. Enforcement model

| Level | Agent behavior | CI behavior | Promotion requirement |
|---|---|---|---|
| `observe` | Record matches and outcomes; do not interrupt | None | Merge allowed |
| `advisory` | Show the applicable rule and recommended action | Optional annotation | Merge allowed |
| `required` | Do not claim completion without the requirement or a recorded exception | Optional soft check | Owner approval |
| `blocking` | Prevent finalization absent an approved exception | Required check may fail | Stronger approval policy + observe accuracy |

The rule engine evaluates only declared structured conditions. Natural-language statements provide guidance but must not silently become blocking behavior.

Promotion safeguards:

- A rule starts at `observe` or `advisory` unless it is an explicit security/legal requirement.
- Promotion requires a review of false positives, bypasses, and match volume.
- Rules have `review_after` dates and may decay from required to advisory if stale or frequently bypassed.
- Exceptions are explicit, time-limited, attributable records—not prompt text.

## 12. Runtime compilation and retrieval

On merge, a webhook triggers the indexer:

1. validate schema and policy DSL;
2. resolve source commit, scope, owner, and inheritance;
3. materialize a graph node and source edges;
4. increment a policy version and publish invalidation events;
5. make the new version available to CLI caches and CI;
6. retain the Git SHA, path, line range, and parsed rule ID as provenance.

Each agent context request receives only the applicable slice:

```json
{
  "task": "Add refund status to the customer API",
  "repository": "payments-api",
  "changed_paths": ["src/api/refunds.ts"],
  "mode": "implementation",
  "token_budget": 6000
}
```

The response contains selected rules, source provenance, policy version, conflicts, and inclusion reasons. It never includes unauthorized graph nodes merely because they are adjacent in the graph.

```json
{
  "context_snapshot_id": "ctx_01J...",
  "policy_version": "payments-api@6ee3d4a",
  "rules": [{
    "id": "payments-api-integration-coverage",
    "enforcement": "advisory",
    "reason": "src/api/refunds.ts matches include_paths",
    "source": ".stella/rules/testing.yaml@6ee3d4a"
  }],
  "conflicts": []
}
```

## 13. Explainability and audit

For every material agent action, Stella records a context snapshot ID, rules considered, rules included, match outcomes, and any exception. A user can ask:

```text
Why did you add an integration test?
```

and receive:

```text
Applied rule: payments-api-integration-coverage
Reason: src/api/refunds.ts matched this rule’s API scope.
Authority: merged repository Context PR #184.
Enforcement: advisory.
```

The audit log is append-only and separates: raw local telemetry, candidate evidence, Git policy history, context retrieval, and enforcement outcome. Retention and access controls vary by class.

## 14. Security and privacy

- Raw prompts, tool outputs, customer content, secrets, and credentials do not enter Git policy files.
- Candidate evidence is redacted before central aggregation and may reference a protected source without copying it.
- Personal preferences are private by default and cannot be promoted automatically.
- Retrieval applies tenant, repository, team, and source-document authorization.
- Git commit signing and provider audit logs are supported where available.
- Policy linting rejects secrets, PII patterns, unsupported executable expressions, and unbounded scopes.
- Administrative analytics report system quality, not individual productivity.

## 15. APIs and events

Minimum service interfaces:

```text
POST /v1/candidates                 create/update local inference candidate
POST /v1/candidates/{id}/propose    generate diff and open Git PR when authorized
GET  /v1/context-slices             retrieve bounded authorized runtime context
POST /v1/rule-matches               record observe/advisory/required outcomes
POST /v1/exceptions                 request or grant time-bounded exception
POST /v1/webhooks/git               ingest merge, revert, and review events
```

Append-only event types:

```text
ObservationRecorded
CandidateCreated
CandidateEvidenceAdded
ContextPRCreated
ContextPRReviewed
PolicyMerged
PolicyReverted
PolicyCompiled
ContextRetrieved
RuleMatched
RuleBypassed
ExceptionGranted
RuleDeprecated
```

Every event carries tenant, actor/service identity, timestamp, source reference, scope, correlation ID, and sensitivity classification.

## 16. Validation and checks

Every Context PR runs:

- YAML/schema validation;
- rule ID uniqueness and stable-ID validation;
- policy DSL parse and static evaluation;
- scope overlap/conflict detection;
- forbidden-data scanning;
- evidence-reference validation;
- ownership and enforcement approval checks;
- impact preview: representative changed files that would match and not match;
- optional simulation against recent merged changes to estimate false-positive rate.

Required check output should be concrete:

```text
Rule overlap: payments-api-integration-coverage conflicts with api-test-exemption.
Both apply to routes/internal/** with different enforcement.
Resolution: add an explicit exclusion or supersession relationship.
```

## 17. Product surfaces

### CLI

```text
stella context propose
stella context explain payments-api-integration-coverage
stella context simulate .stella/rules/testing.yaml --against last-90-days
stella context promote candidate_123 --enforcement advisory
```

The CLI presents candidate, evidence, exact diff, owners, and expected effect before opening a PR or committing locally.

### Pull-request integration

- Bot opens the PR only with an authorized user/service identity.
- Bot posts evidence summary and a link to protected evidence details.
- Checks validate and simulate policy.
- Merged PR comment confirms the indexed policy version.

### Agent and CI integration

- Agent retrieves rule slice before planning and includes applicable instructions in its action plan.
- CI evaluates merged policies against changed files and uploads machine-readable match results.
- CI never evaluates a candidate or an unmerged policy as authoritative.

## 18. Failure modes and mitigations

| Failure | Mitigation |
|---|---|
| Noisy inferred rules | Thresholds, counterexamples, rejection memory, and human merge gate. |
| Rule explosion | Grouped files, semantic deduplication, expiry/review dates, and rule-health reporting. |
| Overbroad scope | Path-level simulation and required negative examples in PR checks. |
| Conflicting policy | Explicit precedence, conflict check, less-restrictive fallback, owner resolution. |
| Stale policy | Review dates, access telemetry, bypass signals, and deprecation PRs. |
| Sensitive evidence leaks | Redacted references, protected evidence store, Git scanner, least-privilege retrieval. |
| Central service outage | Last-known-good signed policy cache; local repository rules remain usable. |
| Git outage | Local candidates queue; no policy becomes authoritative until merged. |

## 19. Rollout plan

### Phase 1 — Context as code (MVP)

- Repository-local `.stella/rules/*.yaml` schema and linter.
- Import existing instruction files into proposed structured rules.
- Manual CLI generation of a branch, diff, and real PR.
- Merged-rule indexer and read-only runtime retrieval.
- Advisory behavior only.

### Phase 2 — Evidence and review quality

- Candidate store, evidence aggregation, and duplicate detection.
- Git provider integration, owner routing, check runs, and explanation traces.
- Rule simulation against historical merged changes.
- Solo-mode local commits and optional sync.

### Phase 3 — Enforcement and cross-repo context

- Shared registry repository and scope inheritance.
- Exception workflow, required enforcement, CI annotations.
- Rule-health metrics, expiry, and deprecation proposals.

### Phase 4 — Enterprise control plane

- SSO/RBAC, audit export, data residency, private deployment, and retention controls.
- Blocking enforcement only for mature, owner-approved rules.

## 20. Success metrics

Primary metrics:

- Context PR approval rate without material scope edits.
- Median time from candidate to merged durable rule.
- Rule match precision: accepted matches ÷ all matches.
- False-positive and exception rates by rule.
- Reduction in repeated review feedback for covered rule categories.
- Agent task completion quality with versus without retrieved policy.

Guardrails:

- Context PR volume per repository must not become review noise.
- No centralization of personal/private raw telemetry by default.
- No blocking-rule rollout without measured observe-mode precision.

## 21. Decisions to lock before implementation

1. Git provider(s) for first release and whether PR creation uses a bot or the developer’s OAuth identity.
2. Canonical rule format: YAML initially; JSON Schema as the validator contract.
3. Whether organization policy lives in a dedicated registry repository from day one.
4. Default candidate thresholds and review-date policy.
5. What evidence may be stored centrally versus represented only as a protected reference.
6. Which CI systems participate in Phase 3.
7. Whether personal preferences can be optionally synchronized, and the encryption/consent model for doing so.

## 22. Acceptance criteria for MVP

- A developer can turn an explicit `AGENTS.md` instruction into a schema-valid repository Context PR with one CLI flow.
- The PR contains a minimal policy-file diff, evidence references, expected effect, and correct reviewers.
- A merge compiles to a versioned runtime graph node within the defined freshness target.
- An agent modifying a matching file retrieves and explains the merged advisory rule.
- A reverted Context PR removes the rule from new context slices.
- The system rejects policy files containing secrets, invalid DSL, unowned blocking rules, or unresolved equal-precedence conflicts.
- A solo developer can create the same rule as a local Git commit without operating a central service.
