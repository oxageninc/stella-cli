# ADR 0009: Enum-Freeze Resolutions for Flagged Phase-1 Decisions

- Status: **Accepted** — ratified by repository owner 2026-07-24. Four items
  resolved by existing spec/ADR; three (decisions 1, 3, and 5a) ratified here.
- Date: 2026-07-24
- Deciders: repository owner (ratified 2026-07-24)
- Tracking: [issue #483](https://github.com/macanderson/stella/issues/483)
  (part of Epic #469)

## Context

Phase 1 finishes the adaptive-context *type layer* (installments #470–#473).
Several enums and validators cannot freeze until seven decisions flagged during
Phase 1 are resolved. Issue #483 records them so they are settled by evidence,
**not by fiat**.

This ADR resolves each item in one of two ways:

- **Resolved by spec/ADR** — an existing normative source already answers it; we
  cite it and, where the flag's premise was mistaken, correct the record.
- **Owner ratification** — no normative source answers it; we record a
  recommendation with its evidence for the owner to ratify. Once ratified, the
  cited enum/validator freezes on the ratified value.

The canonical, normative pair is `adaptive-context-implementation-plan.md` +
`stella-adaptive-context-lifecycle.md` (referred to below as *plan* and
*lifecycle*). `directive-schema.md` is the **superseded** Jul-20 taxonomy (ADR
0001) and grounds nothing. `context-frame-spec.md` is an older divergent draft;
where it disagrees with the normative pair it is a reconciliation flag, not
co-equal evidence.

## Summary

| # | Decision | Disposition | Resolution |
|---|---|---|---|
| 1 | `RuleEnforcement::Informational` migration target | **Ratified** 2026-07-24 | `informational → advisory` |
| 2 | Directive origin arity (4 vs uniform-5) | **Resolved** (ADR 0001) | Uniform **5-value** `Origin` for all families; §8.6's four are illustrative |
| 3 | Define "guarded" | **Ratified** 2026-07-24 | A directive carrying an `enforcer_ref` (executable guard), independent of enforcement level |
| 4 | `constraint_effect` closed set | **Resolved** (lifecycle:501–506) | `{require, forbid}`; `allow` deliberately excluded. Flag's premise was false |
| 5a | ContractValidation result `method` token | **Ratified** 2026-07-24 | `method ∈ {deterministic, semantic_judge}` |
| 5b | `requirement_status` vs `validation_status` | **Resolved** (build-prompt:753–755) | Two distinct fields sharing `{passed, failed, error, skipped}` |
| 6 | Procedure step `order` semantics | **Resolved** (plan:502) | Unique + sortable; contiguous `1..N` not required |
| 7 | `retracted` vs `archived` semantics | **Resolved** (lifecycle:886, 2663, 2857) | retracted = deliberate revocation; archived = reversible efficacy/expiry retirement |

---

## Decision 1 — `RuleEnforcement::Informational` → `DirectiveEnforcement` (RATIFIED 2026-07-24)

The legacy context-as-code enum `RuleEnforcement`
(`stella-core/src/rules/metadata.rs:31-38`) has **three** values —
`informational | advisory | blocking`. The ratified `DirectiveEnforcement` has
**two** (`advisory | blocking`, ADR 0007). ADR 0007's ratified 4→2 mapping was
over a *different* vocabulary (`observe | advisory | required | blocking`);
`informational` appears in neither the four- nor the two-value set, so no
ratified edge reaches it. The code already self-flags this gap
(`stella-core/src/context_record.rs:37-41`).

`informational` is defined as "Inform reviewers without adding an enforcement
expectation" (`metadata.rs:32`) — passive, no enforcement — which is
semantically identical to `observe` ("Record matches and outcomes; do not
interrupt", `context-prs-spec.md:304`). `observe → advisory` is a ratified ADR
0007 edge, and by elimination `informational` cannot map to `blocking`.

**Ratified: `informational → advisory`.** No spec names `informational`, so this
was a genuine decision, not a reading of the spec; the owner ratified the
recommended edge on 2026-07-24. It applies at migration only —
`DirectiveEnforcement` stays two-valued (ADR 0007). See the ADR 0007 amendment,
which records this edge in the enforcement mapping.

## Decision 2 — Directive origin arity (RESOLVED by ADR 0001)

The flag reported that the directive schema enumerates origin as four values
(`user, system, inferred, imported`), narrowing a ratified uniform-5 `Origin`.

**This is resolved and the premise is partly mistaken.** ADR 0001
(`docs/adr/0001-semantic-taxonomy.md:47-54`, spec-verified 2026-07-23):

> `Origin` has the **five** portable values `user, system, observed, inferred,
> imported` for **all** record families, including directives … The §8.6
> directive example using only four is **illustrative, not a per-kind
> narrowing** — there is no rule forbidding an `observed` directive. Freeze the
> 5-value enum in Phase 1.

Corroborated by lifecycle:628 ("Portable Origin values are user, system,
observed, inferred, and imported") and `context-frame-spec.md:96`. The code
already implements the 5-value set (`stella-core/src/context_record/kind.rs:147-155`).
The four-value list at `directive-schema.md:37` is on the *superseded* schema;
its neighbour `source: "observed" | "imported"` at line 318 belongs to a
different type (`Observation`), not to directives.

**Follow-up (not a decision):** the "deferred per-family validator" comments in
`stella-core/src/context_record.rs:42-46` and `kind.rs:143-146` are now **stale**
relative to ADR 0001 (which resolves *against* a per-family narrowing). They
should be removed or updated when installment #470 lands; a directive-specific
narrowing would be a *new* decision, not implied by current spec.

## Decision 3 — Define "guarded" (RATIFIED 2026-07-24)

The validator "a blocking or **guarded** directive requires exact minimum
fidelity" (`plan:524`) and the fidelity default-policy table reference a
`guarded` state that is **not** in the enforcement enum (`advisory | blocking`,
lifecycle:1248) and is **defined nowhere**. The fidelity table lists them as
*separate* rows:

> `| Blocking constraint | exact, required |`
> `| Guarded rule | exact, required |`
> `| Procedure whose order matters | exact, required |`
> — lifecycle:2360-2362

Because "Blocking constraint" and "Guarded rule" are separate rows, and the
validator says "blocking **or** guarded", `guarded` is not simply a synonym for
`blocking` (that would make the "or" redundant). The best-supported reading ties
it to the enforcement machinery: a blocking directive "names enforcement_boundary
and `enforcer_ref`" (lifecycle:1257, example `"enforcer_ref":
"stella_log_guard_v1"` at :1279), and the plan's parallel noun for that slot is
"guards" (plan:924, :973).

**Ratified:** a *guarded* directive is one that carries an `enforcer_ref` (an
executable guard), **independent of** its `advisory | blocking` enforcement
level. The fidelity validator then reads: exact minimum fidelity is required when
enforcement is `blocking` **or** an `enforcer_ref` is present. `guarded` stays a
*derived predicate* (`enforcer_ref.is_some()`), **not** a new enforcement enum
value — so it adds nothing to freeze in `DirectiveEnforcement`.

## Decision 4 — `constraint_effect` closed set (RESOLVED by lifecycle:501–506)

The flag claimed the set is "asserted only in the plan/delta, not enumerated in
the lifecycle spec." **The premise is false.** The lifecycle spec enumerates it
normatively under `#### Constraint`:

> `A hard requirement or prohibition. Constraint effects are:`
> `- require;`
> `- forbid.`
> `Allow is deliberately excluded. Learned context cannot grant authorization.`
> — lifecycle:501-506

Reinforced by invariant 8 (lifecycle:1262, "Authorization cannot be granted by a
directive") and matched verbatim by plan:501, delta:129-130, and both
build-prompts. **Authoritative closed set: `{require, forbid}`**, `allow`
excluded.

The flag's *parenthetical* is accurate: no example anywhere exercises
`constraint_effect: "require"` — the only example uses `forbid` (lifecycle:1275).
That is a documentation gap, not an enum ambiguity. **Follow-up (optional):** add
a `require`-effect example to the lifecycle spec so the valid-but-unexemplified
value is exercised.

## Decision 5 — ContractValidation enums

**5a — the semantic-judge `method` token (RATIFIED 2026-07-24).** `semantic_judge`
is enumerated as a **requirement_kind**, not a result method (lifecycle:1673,
:1689 `| semantic_judge | criterion, rubric_ref, judge_policy_ref | minimum_score |`).
Each per-requirement result carries a `method` field (build-prompt:514), but its
value set is **never enumerated** — the only value that ever appears in any
example is `"method": "deterministic"` (lifecycle:1731 ff.). The spec says
"Deterministic validators run before semantic judges" (lifecycle:1696) and that
semantic judges are "labeled inferred" (plan:1244, :1305), but never names the
`method` token a semantic result carries.

**Ratified: `method ∈ {deterministic, semantic_judge}`** — the two result methods
mirror the two evaluation modes, and a `semantic_judge` result is qualified as
`inferred` provenance. The `method` field reuses the `semantic_judge` token so
requirement_kind and result method align. (Do not confuse `method` with
`OutcomeAssessment.evaluation_method`, lifecycle:1960, a different field with its
own seven-value set.)

**5b — `requirement_status` vs `validation_status` (RESOLVED).** These are **two
distinct fields sharing one value set**, not one-vs-two enums:

> "Contract validation uses `validation_status`; individual requirement results
> use `requirement_status`. Both support `passed`, `failed`, `error`, and
> `skipped` where applicable." — build-prompt:753-755

`validation_status` is contract-level (lifecycle:1726), `requirement_status` is
per-result (lifecycle:1730); "Validation statuses are passed, failed, error, and
skipped" (lifecycle:1828). Ignore `context-frame-spec.md:893-908`, whose
`needs_review`/`not_run` tokens are stale.

## Decision 6 — Procedure step `order` semantics (RESOLVED by plan:502)

> "a procedure must preserve a **unique ordered step sequence**;" — plan:502

**Authoritative rule: `order` must be unique and define a sortable sequence.**
Contiguous `1..N` and strict "increment by 1" are **not** required — the example
`order: 1,2,3,4` (lifecycle:1313-1325) is illustrative. Corroboration:
"Procedures preserve ordered steps" (lifecycle:1260, :3230). **Follow-up
(optional):** the uniqueness requirement is stated only in the plan; echo it in
the lifecycle spec so the normative pair agrees.

## Decision 7 — `retracted` vs `archived` (RESOLVED; add crisp one-liners)

Both are stored `RecordStatus` values: "Stored RecordStatus values are active,
retracted, and archived" (lifecycle:886, restated :2784-2788, plan:108). Usage is
consistent though never stated as side-by-side one-liners:

- **retracted** = *deliberate withdrawal / revocation* — an authority decides the
  steering no longer applies. "Ignore must make the steering inactive: it creates
  a retracted superseding directive revision" (lifecycle:2663-2664); revocation
  "creates a retracted superseding workspace revision" (:2701).
- **archived** = *reversible, efficacy/expiry-driven retirement, retained for
  history*. Auto-archival "append[s] an archived revision unless reaffirmed"
  (lifecycle:2841-2857) and "Archival is reversible" (:2865).

**Follow-up (documentation, not a decision):** add these two one-sentence
definitions to the lifecycle spec's RecordStatus section so the distinction does
not rely on inference.

## Consequences

- Installments #470–#473 may freeze the affected enums/validators; decisions 1,
  3, and 5a are ratified above, so nothing in #483 blocks the freeze.
- `DirectiveEnforcement` stays two-valued (ADR 0007); `informational` is a
  migration-source value only, mapped on the way in.
- `Origin` freezes at the uniform 5-value set (ADR 0001); no per-family narrowing.
- `constraint_effect` freezes at `{require, forbid}`.
- `guarded` (if ratified as recommended) is a derived predicate over
  `enforcer_ref`, adding no enum value.
- Documentation follow-ups (a `require` example, echoing the step-uniqueness rule,
  the retracted/archived one-liners, removing the stale per-family-validator
  comments) are tracked for the relevant installments; none block the freeze.

## Open questions

Resolved 2026-07-24: the repository owner ratified all three open decisions —
`informational → advisory` (1), `guarded` = carries an `enforcer_ref` (3), and
`method ∈ {deterministic, semantic_judge}` (5a). No item in #483 remains open;
the enums/validators may freeze in their installments.
