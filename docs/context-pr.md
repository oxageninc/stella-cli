# Context PRs

**Status:** Canonical specification
**Owner:** Stella
**Principle:** Git is the authoritative, human-governed policy ledger. Every
other representation of durable steering — the context store, the compiled
context frame, a provider cache — is derived from it or subordinate to it.

This document is the single source of truth for what a Context PR is, what it
changes, and how one moves through proposal, review, publication, and
retirement. It consolidates and supersedes earlier Context-PR drafts. It is
consistent with, and defers to, the record semantics defined in
[`docs/design/stella-adaptive-context-lifecycle.md`](design/stella-adaptive-context-lifecycle.md)
(sections 13–15) and the frame/sharing model in
[`docs/design/context-frame-spec.md`](design/context-frame-spec.md). Per
[`docs/design/context-graph-protocol-build-prompt.md`](design/context-graph-protocol-build-prompt.md),
Context PR workflows are **host policy, not protocol mechanism**: nothing in
this document belongs in the context graph wire protocol.

---

## 1. Problem

Teams accumulate durable engineering knowledge in repository instructions,
review habits, incidents, and repeated implementation patterns. Agents and
developers rediscover that knowledge turn by turn. A local context graph
improves one CLI session; it fragments organizational learning.

Context PRs turn sufficiently justified, durable context into **reviewable
code**. They reuse the workflows engineers already trust — diffs, ownership,
approval, revert, audit history — to create, amend, approve, and roll back
machine-readable steering.

The feature must serve both ends of the spectrum:

- a single developer and a single repository, with nearly no ceremony; and
- an organization with many developers and repositories, with ownership,
  scope, access control, and auditability.

## 2. Definition

> A **Context PR** is a reviewable promotion of context into durable steering.

It is **not necessarily a GitHub pull request**. The review intensity and the
concrete representation are set by governance mode:

| Governance mode | Representation of the Context PR |
| --- | --- |
| `solo` | An in-product prompt (Keep / Edit / Ignore) whose acceptance produces an ordinary local commit to a rule file. |
| `team` | A real Git pull request changing `.stella/rules/*.md`, reviewed and merged under normal branch protection. |
| `regulated` | A Git pull request plus an accountable approval record: explicit approver identity, reason, immutable promotion history. |

Two things are always true regardless of mode:

1. **The published artifact is a Git-tracked rule file.** The promotion event
   may be lightweight; the result is versioned, diffable steering.
2. **The graph never silently outranks Git.** A compiled rule carries its
   source commit and path as provenance. Context that exists only as
   observation or inference remains non-authoritative.

Workspace publication (sharing a record to a provider-hosted workspace scope)
is a **separate channel**, not a Context PR: the provider-hosted record is
authoritative for workspace scope and is never materialized into
`.stella/rules/*.md` unless a separate repository publication is approved. See
`stella-adaptive-context-lifecycle.md` §13.4.

## 3. The implemented substrate

A Context PR is not built on a hypothetical rules engine. Everything below is
shipped today and is the surface a Context PR changes:

- **Rule files.** One markdown document per rule: optional single-line
  `key: value` frontmatter between `---` fences, then the rule statement as
  the body. Parsed by `stella_core::rules::rule_from_file`.
- **Locations and precedence.** `.stella/rules/*.md` (repository), plus
  `.claude/rules/` (compatibility) and `~/.stella/rules/` (personal).
  Directory walking and store reads live in `stella-cli/src/rules.rs`; all
  rule semantics — frontmatter parsing, precedence merging, rendering,
  guard evaluation — live in `stella-core::rules` (pure, no I/O).
- **Two enforcement tiers.**
  - **Tier 1 (prompt):** a rule with no `guard-*` keys is rendered into the
    system prompt (`agent::assemble_system_prompt`), riding the prompt cache
    with workspace memories.
  - **Tier 2 (hard-enforced):** any `guard-*` key (`guard-tool`,
    `guard-deny-path`, `guard-deny-command`) makes the rule a guard evaluated
    at the tool boundary — `evaluate_guards` is threaded into
    `ToolRegistry::execute`'s `tool.call.requested` hook chain and can block
    the call outright.
- **Store-published rules.** Extension providers publish the same markdown
  through `stella_store::Store::upsert_rule`; those rules merge with
  provenance path `store://rules/<id>.md`.
- **An evidence-gated promotion primitive.** `stella memory promote <id>`
  promotes a memory from `.stella/private/context.db` into `.stella/rules/<id>.md` —
  but only when the memory has earned strictly more than the required number
  of consecutive positive citations since its last negative remark. It
  refuses to clobber an existing (possibly hand-edited) rule file.

That promotion primitive **is** the embryonic solo-mode Context PR: evidence
threshold in, reviewed file out, human retains the diff. This specification
generalizes it.

## 4. Lifecycle

```text
observation
  -> candidate (deduplicated, evidence-scored)
  -> Context PR (mode-appropriate reviewable promotion)
  -> approval (Keep / merge / accountable approval)
  -> published rule (.stella/rules/*.md, Git-tracked)
  -> compiled selection (context frames cite rule + provenance)
  -> efficacy tracking (citations, outcomes, bypasses)
  -> review_after / expiry / supersession / revert
```

Stage semantics:

- **Observation.** Time-bounded evidence: an explicit instruction, a merged
  change pattern, a correction, an incident link, a repeated tool outcome.
  Observations are never silently treated as instructions.
- **Candidate.** A deduplicated hypothesis keyed by a stable semantic
  fingerprint (normalized intent + scope + condition). Candidates aggregate
  *independent* evidence — repeated variants of one PR are one datum — and
  must carry counterexamples and uncertainty ("14 of 16 matched" beats
  "observed 14 times").
- **Context PR.** The reviewable promotion, per §2.
- **Published rule.** An immutable revision. Editing a published rule creates
  a superseding revision; it never rewrites history (see §6 record hashing).
- **Compiled selection.** Merged rules are selected into context frames with
  source provenance; enforcement follows the rule's tier and level.
- **Efficacy.** Selection, citation, helpful/not-helpful outcomes, guard
  hits, and bypasses are tracked per rule (lifecycle §15.2) and feed review.
- **Retirement.** Rules decay via `review_after`, expire via `valid_until`,
  are superseded by later revisions, or are removed by reverting the commit
  that published them. A revert removes the rule from all future selection.

Every promotion or retirement appends a **PromotionEvent** linking the source
and result revisions, with actor, reason, and timestamp. Ignoring an
auto-activated solo-mode rule must actually deactivate it: a retracted
superseding revision, a reverted PromotionEvent, negative induction evidence,
and a re-proposal cooldown (lifecycle §13.2).

## 5. Governance modes

### 5.1 Solo

```text
observation -> personal draft -> automatic advisory rule -> Keep / Edit / Ignore
```

- Acceptance is one action and produces an ordinary local commit — in the UI
  it reads "Add rule to this repository," not "open a pull request."
- **Keep** confirms the rule and extends retention. **Edit** creates a
  user-authored superseding revision, preserving candidate evidence.
  **Ignore** retracts the rule, records negative evidence, and starts a
  re-proposal cooldown.
- Nothing inferred becomes blocking without explicit confirmation.
- No central service is required; the workflow is local-first.

### 5.2 Team

```text
observation -> proposed Context PR -> owner approval -> published rule
```

- The Context PR is a normal Git pull request changing `.stella/rules/*.md`.
  Git supplies authorship, diff, discussion, ownership, and audit history.
- Owner routing activates only when maintainers or code owners can actually
  be resolved; until then proposals remain advisory and unowned.
- Personal observations and user-scoped directives are excluded from the
  change (§10).

### 5.3 Regulated

Everything in team mode, plus: explicit approval for repository and
organization promotion, recorded approver identity and reason, immutable
promotion history, policy versioning, retained evidence, optional separation
of proposer and approver, and no automatic archival of published policy. A
system or critical constraint is never changed by trace mining alone.

### 5.4 Solo-to-team transition

When multiple active repository identities are detected: ask before changing
governance mode; keep user-scoped records private; list repository-applicable
proposals eligible for publication; convert local evidence into proposals,
never into enforced team policy; publish only selected rules through Git;
enable owner routing only once owners exist. No record-schema migration is
required — only the governance mode and promotion workflow change.

## 6. The payload — published rule files

A Context PR's diff touches only rule files (and, when configured, ownership
metadata). One rule per file; the smallest useful diff; no churn merely
because a pattern was observed.

### 6.1 Format

The file remains the shipped markdown contract (§3), extended with canonical
promotion metadata from lifecycle §14:

```markdown
---
name: api-integration-coverage
description: Require integration coverage for API endpoint changes
schema_version: 1.0-draft
record_id: dir_api_integration_coverage_v3
lineage_id: lin_api_integration_coverage
record_kind: directive
directive_kind: rule
record_status: active
origin: inferred
scope:
  repository_id: repo_stella
sharing_scope: repository
enforcement: advisory
confidence: 91
observed_at: 2026-07-20T18:30:00Z
valid_from: 2026-07-20T18:30:00Z
supporting_evidence_ids:
  - ev_task_101
  - ev_task_118
  - ev_task_144
record_hash: sha256:record...
---

API endpoint changes should include integration coverage.
```

Rules:

- **Metadata is lowercase snake_case.** New generated files emit canonical
  `guard_tool`, `guard_deny_path`, `guard_deny_command`; the loader accepts
  the legacy hyphenated keys during migration.
- **Confidence is an integer 0–100** (never 0.85-style floats).
- **Time semantics** follow the common schema: `observed_at` (when Stella
  learned it), `valid_from` (when it became applicable), `valid_until`
  (exclusive end, only when known). `review_after` marks decay review, not
  invalidity.
- `directive_kind` is one of the directive schema's kinds — `memory`, `fact`,
  `rule`, `preference`, `constraint`, `procedure`
  ([`docs/design/directive-schema.md`](design/directive-schema.md)).

### 6.2 Record hashing and immutability

`record_hash` is the semantic ContextRecord hash, **not** a hash of the
markdown file. The loader reconstructs the canonical record from the
normatively mapped frontmatter fields plus the body as `statement`, omits
`record_hash` from the preimage, and recomputes. Presentational `name` and
`description` are excluded from the canonical record and must never affect
behavior. A manual semantic edit with a mismatched stored hash creates a new
immutable revision and hash after validation — Stella never silently
overwrites the prior record.

### 6.3 Evidence projection

`supporting_evidence_ids` is a safe markdown projection of canonical
`evidence_links` whose relation is `supports`. Contradicting or otherwise
qualified links remain typed in `context.db`. **Git files contain reviewable
statements and safe stable evidence IDs — never raw evidence** (§10).

## 7. Evidence thresholds

Default thresholds for what may become a candidate and what may open a
Context PR (configurable per governance mode):

| Evidence type | May create candidate | May open Context PR |
| --- | --- | --- |
| Explicit instruction in a tracked repo file (`AGENTS.md`, contributor docs, CI config) | Yes | Immediately, if no equivalent structured rule exists |
| Explicit owner request ("this is repo policy") | Yes | Immediately |
| Repeated accepted change pattern | Yes | ≥ 8 independent examples, ≥ 75% consistency, confidence ≥ 80 |
| Positively cited memory | Yes | Citation streak per `stella memory promote` eligibility |
| Incident / postmortem | Yes | Only with a narrowly stated prevention and a resolvable owner route |
| A single agent decision or single user behavior | Yes | Never alone |

Rejected candidates are retained with a structured reason and an expiry, so
the same weak rule is not re-proposed on cooldown.

## 8. Creating a Context PR

### 8.1 Preconditions

Before creating a branch, Stella must be able to:

1. identify the target repository;
2. produce a schema-valid, minimal, single-concern diff;
3. state the proposed scope and resolve likely owners;
4. attach evidence IDs and the confidence calculation;
5. explain the expected runtime effect (which paths/tools will match); and
6. confirm no restricted raw data enters Git.

If any precondition fails, Stella records a candidate only. It must not
manufacture a PR around vague policy.

### 8.2 Branch, title, body

```text
Branch: stella/context/api-integration-coverage
Title:  context: require integration coverage for API changes
Labels: context, proposed-policy
```

```markdown
## Proposed steering
API endpoint changes should include integration coverage.

## Scope
repository: this repo — src/api/**, routes/**

## Suggested enforcement
advisory (Tier 1 — prompt only)

## Evidence
- AGENTS.md — Testing section
- 14 / 16 comparable merged PRs added integration coverage (ev_task_101, …)
- INC-482, INC-519 were endpoint regressions without coverage

## Expected runtime effect
Agents modifying matching paths are prompted to add or update integration
tests. No tool call is blocked.

## Alternatives considered
- No rule: rejected — an explicit repo instruction already exists.
- Tier-2 guard: deferred until advisory-mode accuracy is measured.
```

The diff and the body are generated independently from the candidate record,
so review stays legible even if evidence links become unavailable.

## 9. Review and governance

Use the host Git provider's existing machinery: branch protection,
CODEOWNERS, approvals, checks, merge queue, revert. Optional owner routing
maps rule kinds to required reviewers (e.g. any `constraint` with guard keys
requires the platform owners; security-scoped rules require the security
owner).

Review actions and their record semantics:

| Action | Effect |
| --- | --- |
| **Approve / merge** | Publishes the revision; PromotionEvent `published`. |
| **Edit** | Amends wording, scope, enforcement, or evidence in the PR; merge publishes the edited revision. |
| **Reject** | Close with a structured reason: `one_time_pattern`, `insufficient_evidence`, `wrong_scope`, `duplicate`, `not_policy`. Candidate retained with cooldown. |
| **Defer** | Keep the candidate; ask for more observations by a date or event. |
| **Supersede** | Point to the replacing rule or PR; old revision becomes `superseded` on merge. |
| **Revert** | Revert the publishing commit; the rule leaves all future selection once the revert is observed. |

## 10. Privacy boundaries

- Raw prompts, tool outputs, customer content, secrets, and credentials never
  enter Git policy files. Evidence appears only as stable IDs (§6.3); full
  evidence stays in `context.db` under its own retention and access policy.
- **Personal directives are private by default and are never promoted
  automatically.** `sharing_scope: personal` content cannot appear in a
  Context PR; user-scoped records are excluded from team-mode diffs.
- A rule crosses a sharing scope (`personal` → `repository` →
  `organization`) only through an explicit, mode-appropriate approval.
- Administrative analytics report system quality, never individual developer
  productivity.

## 11. Enforcement ladder

Enforcement levels map onto the shipped two-tier engine:

| Level | Mechanism | Agent behavior | Promotion requirement |
| --- | --- | --- | --- |
| `observe` | Tracking only | Record matches; do not interrupt | Automatic |
| `advisory` | Tier 1 (prompt) | Rule rendered into the system prompt with provenance | Solo: auto with Keep/Edit/Ignore. Team: merge. |
| `required` | Tier 1 + completion check | Task may not claim completion without the requirement or a recorded exception | Owner approval |
| `blocking` | Tier 2 (`guard_*` keys) | Matching tool calls are denied at the tool boundary | Explicit approval **and** measured advisory-mode precision; never inferred silently |

Safeguards:

- A rule enters at `observe` or `advisory` unless it is an explicit
  security/legal constraint written by an owner.
- A rule becomes blocking only when a **real enforcer exists** — concrete
  `guard_tool` / `guard_deny_path` / `guard_deny_command` values that
  `evaluate_guards` can evaluate. Natural-language statements never silently
  become blocking behavior.
- Promotion to `required`/`blocking` requires reviewing false positives,
  bypasses, and match volume from the lower level.
- Rules carry `review_after`; stale or frequently bypassed rules decay back
  to advisory via a deprecation Context PR, not by silent mutation.
- Exceptions are explicit, time-limited, attributable records — not prompt
  text.

## 12. Validation checks

Every Context PR (and every solo-mode acceptance) runs:

- markdown/frontmatter schema validation, including snake_case canonical keys;
- rule `name`/`record_id` uniqueness and lineage validation;
- `record_hash` recomputation against the canonical preimage (§6.2);
- guard-key lint: valid tool names, parseable globs, bounded scope;
- forbidden-data scan: secrets, PII patterns, raw prompt/tool text;
- evidence-ID resolution against the candidate store;
- ownership and enforcement-level approval checks;
- overlap/conflict detection against existing rules at equal precedence;
- impact preview: representative paths/commands that would and would not
  match; optionally a simulation against recent merged changes to estimate
  false-positive rate.

Check output must be concrete and actionable, e.g.:

```text
Overlap: api-integration-coverage conflicts with api-test-exemption —
both match routes/internal/** with different enforcement.
Resolution: add an explicit exclusion or a supersession relationship.
```

Conflicts at equal precedence are never silently resolved: the compiled frame
carries a conflict record and enforcement falls back to the less restrictive
behavior until an owner resolves it.

## 13. Compilation, selection, and explainability

On publication (merge or local commit), the rule loader picks up the new
revision through the normal rule directories; store-published and Git-published
rules merge under one precedence order. Compiled context frames include, per
selected rule: enforcement level, the match reason, and source provenance
(`.stella/rules/<name>.md` @ commit).

Every material agent action records which rules were considered, selected,
matched, and bypassed, so this exchange always works:

```text
> Why did you add an integration test?

Applied rule: api-integration-coverage
Reason: src/api/refunds.ts matched this rule's scope.
Authority: merged Context PR #184 (.stella/rules/api-integration-coverage.md @ 6ee3d4a).
Enforcement: advisory.
```

Audit classes are separated — raw local telemetry, candidate evidence, Git
policy history, frame selection, enforcement outcome — each with its own
retention and access policy.

## 14. Failure modes

| Failure | Mitigation |
| --- | --- |
| Noisy inferred rules | Thresholds (§7), counterexamples, rejection memory + cooldown, human gate. |
| Rule explosion / review noise | One-concern diffs, semantic dedup, `review_after` decay, per-repo volume guardrail. |
| Overbroad scope | Impact preview with required negative examples; guard-key lint. |
| Conflicting policy | Equal-precedence conflict record; less-restrictive fallback; owner resolution. |
| Stale policy | `review_after`, efficacy telemetry, bypass signals, deprecation Context PRs. |
| Sensitive evidence leaks | Evidence-ID projection only; forbidden-data scan; `context.db` stays local. |
| Hand-edit drift | `record_hash` mismatch forces a new validated revision — never a silent overwrite. |
| Clobbered manual rules | Promotion refuses to overwrite an existing rule file (shipped behavior). |
| Git/provider outage | Local candidates queue; published local rules keep working; nothing becomes authoritative until merged. |

## 15. Product surfaces

### CLI

Shipped today:

```text
stella memory list          # citation counts and promotion eligibility
stella memory promote <id>  # evidence-gated promotion to .stella/rules/<id>.md
```

Specified by this document (the `stella context` family):

```text
stella context propose               # candidate -> diff -> local commit or PR
stella context explain <rule>        # provenance, evidence, efficacy for a rule
stella context simulate <file>       # match preview against recent history
stella context promote <candidate>   # promote with explicit enforcement level
```

Each flow presents candidate, evidence, exact diff, owners, and expected
effect before anything is committed or opened.

### Pull-request integration

- The bot opens PRs only under an authorized user/service identity.
- The PR carries the evidence summary and a link to protected evidence
  detail; checks run §12.
- The merge comment confirms the published revision and its provenance.

### Agent and CI integration

- Agents retrieve the applicable rule slice before planning and cite applied
  rules in their output (§13).
- CI may evaluate merged Tier-2 guards against changed files and upload
  machine-readable match results.
- CI never treats a candidate or an unmerged rule as authoritative.

## 16. Rollout

Aligned with the adaptive-context plan (lifecycle "Phase 4: team governance"):

1. **Solo promotion (shipped).** Citation-gated `stella memory promote`,
   markdown rule files, Tier 1/Tier 2 engine, store-published rules.
2. **Canonical metadata.** Emit snake_case promotion frontmatter (§6.1) and
   `record_hash` from the promotion path; loader accepts legacy keys.
3. **Candidate store + Keep/Edit/Ignore.** Deduplicated candidates with
   evidence scoring, cooldowns, and the solo acceptance flow.
4. **Context PR materialization through Git.** `stella context propose`
   generates branch + diff + PR body; validation checks as PR checks; owner
   routing; promotion audit (PromotionEvents).
5. **Team enforcement.** `required` level, exceptions workflow, simulation,
   efficacy-driven decay and deprecation PRs; solo-to-team migration.
6. **Regulated / organization policy.** Accountable approval records,
   organization scope behind an explicit managed source, retention and
   residency controls. Blocking only for mature, owner-approved rules.

## 17. Acceptance criteria

- A developer turns an explicit `AGENTS.md` instruction into a schema-valid
  Context PR (or solo commit) in one CLI flow.
- The diff touches exactly one rule file, carries evidence IDs, expected
  effect, and correct reviewers.
- A merged Context PR's rule is selected into context frames and cited with
  commit-level provenance.
- A reverted Context PR removes the rule from all subsequent selection.
- Ignore on a solo auto-activated rule retracts it, records negative
  evidence, and enforces a re-proposal cooldown.
- Validation rejects: forbidden data, invalid guard keys, hash-mismatched
  semantic edits, unowned blocking rules, and unresolved equal-precedence
  conflicts.
- A solo developer can do all of the above with no central service.

## 18. Non-goals

- Replacing code review, issue tracking, incident management, or ADRs.
- Publishing inferred policy without a configured promotion policy.
- Storing prompts, raw tool logs, or customer data in Git.
- Evaluating individual developer productivity.
- Requiring a centralized service for the local-first solo path.
- Adding Context PR mechanics to the context graph wire protocol — mechanism
  in the protocol; policy in the host.
