# Memory citations and rule promotion

The self-improvement loop, end to end: memories the agent recalls into a
turn are scored by the agent that used them, the scores accumulate locally,
and a memory that keeps proving itself can be promoted — explicitly, by a
human — into a binding workspace rule.

```text
recall  ── stella-cli recall block: each Memory frame carries its stable
│          [nod_…] id (stella-context node public id) + the citation ask
cite    ── cite_memory tool (stella-tools): memory_id, useful_score 1-5,
│          truthful flag, one-sentence remark; session ledger, latest
│          judgment per memory wins
persist ── drained at execution end into .stella/store.db
│          (memory_citations table, UNIQUE (execution_id, memory_id))
inspect ── `stella memory list`: most-cited first, avg score, truthful
│          rate, positive streak, promotion eligibility
promote ── `stella memory promote <id>`: writes .stella/rules/<slug>.md in
           the rules engine's authoring format (markdown + frontmatter,
           parsed by stella_core::rules::rule_from_file)
```

Only Memory-kind frames are citable — code-graph hits and episodes in the
same recall block are grounding, not memories, and stay outside the loop.
The citation instruction rides the volatile recall block itself, so the
model is asked to cite exactly when something citable was injected, and the
byte-stable system prefix (the prompt-cache contract) never changes.

## Citation semantics

A citation is the agent's judgment of a memory it *actually used*:

- `useful_score` (1–5) — how much the memory helped the actual work
  (1 = misleading/wasted effort, 5 = decisive).
- `truthful` — whether the memory's content still holds, verified against
  the workspace this turn (a stale path, a changed convention ⇒ `false`).
- `remark` — one sentence of free text explaining the judgment.

A citation is **positive** when `truthful` is set AND `useful_score >= 3`
(`stella_store::POSITIVE_SCORE_MIN`). Anything else — a low score or an
untruthful verdict, regardless of score — is a **negative remark**.

Re-citing the same memory within one turn replaces the earlier judgment
(the model's final word wins); each citation persists under exactly one
execution, so counts are never inflated by re-persisting.

## Promotion eligibility (strict by specification)

A memory becomes promotion-eligible once it has been used successfully
**strictly more than 10 times with all-positive remarks**
(`stella_store::PROMOTION_CITATIONS_REQUIRED`). The literal semantics,
implemented in `stella_store::fold_citation_stats`:

- The gate reads the **positive streak**: consecutive positive citations
  since (and not counting) the memory's most recent negative one, in
  citation order.
- Exactly 10 all-positive citations: **not** eligible. 11: eligible.
- One negative remark — anywhere — resets the streak to zero. The memory is
  disqualified until it **re-earns** more than 10 fresh all-positive
  citations after that negative.

Eligibility is computed automatically and flagged in `stella memory list`;
the promotion itself is deliberately explicit and human-invoked. Promoted
rules are prompt-only (Tier 1): the citation loop scores usefulness and
carries no file evidence from which to infer a safe Tier-2 deny guard.
Promotion never overwrites an existing rule file — delete the file first to
re-promote.

## Storage

- `.stella/store.db`, `memory_citations` table (schema v3, additive):
  `execution_id`, `memory_id`, `ts`, `useful_score`, `truthful`, `remark`,
  `UNIQUE (execution_id, memory_id)`.
- Memory content/identity lives where it always did: `.stella/context.db`
  (`stella-context`), keyed by the same `nod_…` public id the citations
  reference. `stella memory list` joins the two; citation rows whose id no
  longer resolves to a live memory node are not shown and can never be
  promoted.
