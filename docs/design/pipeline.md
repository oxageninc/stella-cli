# The staged pipeline

The orchestration plane in `stella-pipeline` that drives one prompt through
governed stages instead of a raw engine loop. It is the default `stella run`
path (`--no-pipeline` opts out), and in the interactive Command Deck it is
toggled per-session with **`/pipeline`** (the `PIPELINE` stat box tracks it).

The defining contract it enforces is the repo's own: **verified done, not
claimed done** — a task is finished when a witness test that failed before the
work passes after it.

## The stage flow

```
evaluate (triage) ─ enhance (recall) ─ [plan ─ scope review] ─ witness
        ─ execute ─ verify ─ [judge] ─ [revise ↩ execute] ─ complete
```

| Stage | What happens | Skipped when |
|---|---|---|
| Triage | A cheap model call + a deterministic pattern floor classify the prompt as `lookup` / `single` / `multi`. The floor can only *raise* the class (errs toward planning); the call runs with zero retries under a hard latency ceiling. | never |
| Context recall | The context plane recalls grounding for the goal: vector similarity + recency + 1-hop graph adjacency fused by reciprocal-rank fusion, MMR-diversified, packed to a token budget with drops reported. | no context plane wired |
| Plan | Split-context planning (goal + recall + repo structure, never the transcript) into an ordered step list, with one bounded JSON-repair retry. | `lookup`, `single` |
| Scope review | Plans above thresholds pause for interactive approval/trim. Headless runs require an explicit bypass flag — never a silent auto-approve. | small plans |
| **Witness** | An independent model authors the **witness test** — see below. | `lookup`; `--test-command` given; `witness_writer` off |
| Execute | The engine's tool loop, one turn per plan step. | never |
| Verify | The deterministic evidence ladder — see below. | clean `lookup` (zero-diff guard revokes the skip if files were touched) |
| Judge | A model judge (judge ≠ worker) — **only** on inconclusive evidence. | strong deterministic evidence either way |
| Revise | Failure evidence feeds a bounded revision loop (`max_revisions`). | verification passed |

## Verification: the flip oracle and the witness author

Only a **fail→pass flip of the same normalized test command** counts as
deterministic verification (`verify.rs`). A test that never failed proves
nothing; a pass of a different command proves nothing.

Three ways the oracle gets its command, in precedence order:

1. `--test-command` — the user's explicit choice, always wins.
2. **The witness author** (`witness.rs`, on by default): when nothing is
   configured and the task will be verified, an independent model — the
   *judge's* resolution, never the worker's transcript — writes a minimal test
   that fails on the current code, and names it in a `TEST_COMMAND:` line. The
   pipeline checks it genuinely fails (one bounded repair retry, then the
   witness is discarded with a warning), then tracks it in the flip oracle.
3. Neither — verification degrades to diff-budget + model judge.

### Visible, not hidden — integrity by tamper exclusion

The witness is deliberately **visible to the worker**. Iterating against a
failing test is where convergence comes from; hiding the test converts the
best feedback signal into a one-shot lottery, and a file on disk is
discoverable by any worker with a shell anyway. The failure mode that hiding
tries to prevent — the worker gaming the test — is handled the way SWE-bench
itself handles it, by **excluding the test from the worker's editable
surface**:

- The untracked-file fingerprints the witness turn created form a
  **watchlist** (observed delta, never the author's claims).
- At verify time, any watchlisted file whose fingerprint changed or vanished
  is **tampered**: the flip is not credited, the evidence degrades to
  inconclusive, and the judge is told exactly which paths were touched.
- Because the witness files are snapshotted *before* the worker's
  `untracked_before` baseline, they are also excluded from the worker's
  diff-budget accounting.

A tampered witness never silently passes (no SubmitFast) and never hard-fails
work that may still be correct (the judge decides, informed).

## Distress-triggered guidance (not a midpoint judge)

On the **second consecutive** deterministic verification failure, the pipeline
spends one judge call on course-correction (`guidance_prompt`) that rides with
the next revision prompt. Design intent, recorded for posterity:

- A mandatory "halfway" judge burns a near-worker-sized call on the majority
  of runs that were going fine, and "halfway" has no honest denominator
  mid-run.
- A deterministic red test needs no verdict — re-judging it is spend without
  information (L-E11). What a stuck worker needs is *steering*, so the
  guidance call reacts to a distress signal (evidence alone didn't fix it) and
  is bounded by `max_revisions`.

The model-judge escalation path already feeds its verdict reasoning into
revisions, so guidance is only added where it was missing: the
deterministic-failure loop.

## Cache discipline (L-E8)

The system prefix is byte-stable per session; recalled context + the goal ride
as **one volatile user message after it**, so prompt-cache hits on the prefix
survive across turns. Toggling `/pipeline` mid-session deliberately does not
rewrite the session's system prompt for the same reason.

## Best-of-N (L-E7)

`--candidates N` runs execute+verify N times and selects by **verification
strength** (deterministic flip > judge pass > unverified > failed; smaller
diff breaks ties; index 0 wins all-equal fields so best-of-N never pays for a
different answer than single-shot without strictly better evidence). The
witness composes: all candidates are measured against the same authored test.
Opt-in only — single-shot beat multi-attempt on cost-per-resolve in
head-to-head runs.

## The Command Deck toggle

`/pipeline` flips staged routing per-session (default off in the deck). Named
seams of the deck integration (`command_deck.rs::run_lead_pipeline_turn`):

- **Scope review auto-approves** — the deck cannot block a turn on a stdio
  gate; the `ScopeReview` event is narrated in the transcript, not gated.
  Deck-native scope review is a follow-up (same seam as the driver's
  `ScopeDecision` no-op).
- **The session's system prompt stays** (cache discipline; see above).
- **Recall is the pipeline's own port** — the driver skips its recall-block
  injection for pipeline turns so frames are never doubled.

## Cost profile

| Stage | Marginal cost | Bounded by |
|---|---|---|
| Triage | one cheap-tier call | 10s latency ceiling, 0 retries |
| Recall | local (store + embeddings) | token/frame budget |
| Plan | one call (+1 repair) | multi-step only |
| Witness | one engine turn + ≤2 test runs (+1 repair turn) | discarded, never looped |
| Verify | test + diff commands | deterministic, no model spend |
| Judge | one call | only on inconclusive evidence |
| Guidance | one call | only on 2nd+ consecutive deterministic failure |
| Revise | one engine turn each | `max_revisions` (default 2) |

## Rejected designs (and why)

Recorded so they aren't re-proposed without new evidence:

- **Hidden verification test.** Correlated errors (the test author reads the
  same ambiguous prompt as the worker), loses the iterate-against-failure
  signal that produces most accuracy gains, and requires an unwinnable
  hide-the-file mechanism. Tamper exclusion buys the same integrity at a
  fraction of the cost.
- **Mandatory midpoint judge.** Undefined "midpoint", pays on every run,
  contradicts the deterministic-first ladder (L-E11). Replaced by
  distress-triggered guidance.
- **Prompt completeness gate.** Adds a call + latency to every task; only
  pays off interactively where a human can answer a clarifying question —
  benchmark prompts are fixed. Not built; interactive clarification is a
  product feature, not a pipeline stage.
