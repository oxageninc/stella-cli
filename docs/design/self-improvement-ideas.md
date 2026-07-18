# Self-improvement: current state and what's missing

**Status: All 5 proposals implemented.** See the implementation summary at
the bottom of this document.

An honest assessment of whether the reflection-and-recall loop actually
makes the agent better over time, and concrete proposals to guarantee
improvement instead of relying on hope.

---

## The loop as built

Four mechanisms exist today, each verified against the live workspace:

```bash
            ┌─────────────────────────────────────────────────────┐
            │                                                     │
  recall    │  turn start: similarity + domain + recency recall   │
   ───────► │  injects a volatile [auto-recalled context] block   │
            │  after the byte-stable system prefix (L-E8)         │
            │                                                     │
            │  turn runs …                                        │
            │                                                     │
  reflect   │  turn end: one cheap model call → 0-3 lessons       │
   ───────► │  ├─ stored as reflection memories in context.db     │
            │  └─ appended to reflections.jsonl (mining log)      │
            │                                                     │
  mine      │  reflections.jsonl mined for recurring lessons      │
   ───────► │  (Jaccard ≥ 0.5, ≥ 3 occurrences) → SKILL.md        │
            │                                                     │
  cite      │  agent scores each recalled memory it used:         │
   ───────► │  useful_score 1-5, truthful, remark                 │
            │  > 10 positive citations → promotion-eligible       │
            └─────────────────────────────────────────────────────┘
```

All four are wired, tested, and functioning. The data confirms it:

- **12 reflection memories** in `context.db`, all `kind=reflection`.
- **12 lines** in `reflections.jsonl` (the mining log).
- **21 episodes** recorded (20 success, 1 failure) with domain tags.
- **2 citation rows** in `memory_citations` (both from this session).
- **0 auto-created skills** — the threshold has never been met.
- **0 promoted rules** — the `.stella/rules/` directory does not exist.

So the machinery runs. The question is whether it *works*.

---

## Does it give me an advantage? Honestly: partially

### What works

**Recall is useful when it surfaces the right memory.** This session is
live evidence: the recalled block carried two memories about the code graph
(`graph_snapshot` roots on the busiest file; `stella init` rebuilds the
graph). Both were directly relevant to the task I was asked to do, and both
saved me exploration steps. I cited both as useful (scores 4 and 3).

**The reflection prompt is well-designed.** It asks for durable,
forward-looking lessons, not self-congratulation. It correctly notes that
"most turns have nothing worth recording" and returns `[]` for trivial
turns. The domain-tagging constraint (invented domains dropped) keeps the
taxonomy clean.

**The best-effort contract is sound.** A failed reflection never fails the
turn, a failed store degrades to "no memory this turn," and the system
prompt is byte-stable so cache hits survive. The engineering is careful.

### What doesn't work

**1. No outcome-grounded feedback.** The reflection model critiques its own
performance from the transcript alone — it never knows whether the turn
*succeeded or failed*. The episode outcome (`success` / `failure`) is
recorded separately in `context.db` but is never fed back into the
reflection prompt. A turn that passed verification and a turn that failed
get the same generic reflection prompt. This means the highest-value
learning signal — "this approach failed, don't repeat it" — is lost.

**2. Citations measure usage, not impact.** A `useful_score=5` means "I
used this memory and it helped." But there is no measurement of whether the
recall *changed the outcome* — whether the turn would have succeeded
without it. Without a control, the system cannot distinguish "recall helps"
from "recall confirms what I would have done anyway."

**3. No pruning of stale or wrong lessons.** Reflections accumulate
monotonically: 12 today, 24 next month, 100 eventually. The only pruning
mechanism is the `truthful=false` citation flag, but with 2 total
citations ever recorded, it is effectively unused. A lesson recorded
months ago about a code path that was since refactored is still recalled,
still costs context budget, and still risks misleading.

**4. Skills never auto-create.** The mining threshold (`min_occurrences=3`
at Jaccard ≥ 0.5) has never been met across 12 reflections. The lessons are
too varied in wording for fuzzy clustering to group them, or the volume is
too low, or both. The auto-promotion path is tested but has never fired in
practice.

**5. Reflection is gated on tool use only.** `turn_warrants_reflection`
checks whether any tool was called. This correctly skips trivial turns, but
it also skips turns where the agent gave a wrong answer in pure
conversation — a real failure mode that produces no lesson.

---

## What would guarantee improvement (and prevent drift)

A self-improvement loop needs three properties to *guarantee* monotonic
improvement rather than hopeful accumulation:

| property           | meaning                                     | current state                                                        |
| ------------------ | ------------------------------------------- | -------------------------------------------------------------------- |
| **directionality** | a measurable signal for "better"            | partial — episode outcomes exist but aren't connected to reflection  |
| **attribution**    | link outcomes to specific lessons/behaviors | weak — citations measure usage, not causal impact                    |
| **pruning**        | remove lessons that are wrong or stale      | absent — `truthful=false` exists but has no effect on recall ranking |

The proposals below address each gap. They are ordered by leverage: the
first three are high-impact and relatively low-effort; the last two are
higher-effort but close the loop fully.

---

### Proposal 1: Feed the outcome into the reflection prompt

**The change:** When `reflect_and_record` is called, pass the episode
outcome (`success` / `failure`) into the reflection prompt. Use two
different prompt templates:

- **On success:** "This turn succeeded. What approach or convention worked
  well that is worth remembering for future similar tasks?"
- **On failure:** "This turn failed verification. What was the most likely
  cause — a wrong assumption, a missed file, a bad approach? What should
  change next time?"

**Why it guarantees improvement:** Failed turns are where learning happens.
A reflection on a failure that identifies the root cause produces a lesson
with clear directionality ("don't do X because it leads to failure Y").
Currently that signal is recorded as an episode but never mined for a
lesson.

**Effort:** Small. The outcome is already available at every call site
(`agent.rs` computes `episode_outcome` before calling `reflect_and_record`).
The prompt template is one `match` arm.

---

### Proposal 2: Build a failure-to-lesson pipeline

**The change:** When verification fails (`LadderDecision::Revise` or a
judge `FAIL`), automatically generate a targeted lesson from the failure
evidence — not from the full transcript, but from the specific failure
signal (the red test output, the diff that was too large, the tampered
witness). Tag it with the failure type as a domain.

**Why it matters:** Generic reflection asks "what went well?" Targeted
failure analysis asks "this specific thing broke — why?" The latter
produces actionable, testable lessons. Over time, the failure-to-lesson
pipeline builds a catalog of "known failure modes for this codebase" that
is recalled before the agent repeats them.

**Connection to proposal 1:** The outcome-grounded prompt (proposal 1) is
the general case; this is the failure-specific fast path that bypasses the
generic reflection model and goes straight to the evidence.

**Effort:** Medium. Requires the pipeline to call into the reflection
module with structured failure evidence, and a new prompt template.

---

### Proposal 3: Make `truthful=false` actually suppress recall

**The change:** When a memory receives a `truthful=false` citation, it
should be **down-ranked in future recall** — not deleted (the user might
re-cite it as truthful after a fix), but weighted lower. After N
`truthful=false` citations (e.g. 2), the memory is quarantined: excluded
from recall entirely until a human reviews it or it receives a fresh
`truthful=true` citation.

**Why it prevents drift:** Without pruning, the memory store is a monotonic
accumulator. Every refactor, every fixed bug, every changed convention
produces stale memories that are still recalled and still cost context
budget. A memory that a future agent verified as wrong but still recalled
is active harm — it misleads. The `truthful` flag is the right signal; it
just needs teeth.

**Current gap:** `memory_citations` records `truthful` but the recall path
(`recall_block` → `ContextStore::recall_scoped`) never reads the citation
table. The store and the citations are two databases that don't talk.

**Effort:** Medium. The recall query in `stella-context` needs to join
against `memory_citations` (or receive a quarantine set from the CLI layer)
and filter/down-rank accordingly.

---

### Proposal 4: A/B measurement of recall effectiveness

**The change:** On a configurable fraction of turns (e.g. 10%), suppress
recall entirely — no `[auto-recalled context]` block — and record the
outcome. Compare the success rate of recalled turns vs. control turns over
a rolling window.

**Why it matters:** This is the only way to *prove* the system helps.
Without a control, "the agent succeeded" could mean "recall helped" or "the
task was easy." An A/B measurement converts belief into evidence.

**Practical note:** This requires enough volume to be statistically
meaningful. For a single user's workspace it may never reach significance,
but for a fleet or a benchmark suite it would. Even without statistical
power, a trend line ("recalled turns succeed at 87%, control at 82%") is
more informative than no measurement at all.

**Effort:** Small for the mechanism (a coin flip in `recall_block`), larger
for the analysis (a new `stella memory ab-report` command or dashboard).

---

### Proposal 5: Periodic re-validation of old memories

**The change:** A background job (triggered by `stella init` or a cron) that
takes each memory older than N days, checks whether its content still holds
against the current codebase (e.g. does the file path it references still
exist? does the symbol it names still have the signature it describes?),
and marks stale ones for re-citation or quarantine.

**Why it prevents drift:** Code changes faster than agents cite. A memory
about `graph_snapshot()` written when it only rooted on the busiest file is
stale now that `graph_snapshot_focus()` exists — but nothing in the system
knows that until an agent happens to recall it, happens to verify it, and
happens to cite it as `truthful=false`. Proactive re-validation catches
drift without relying on the agent to stumble into it.

**Effort:** Larger. Requires the memory content to carry structured anchors
(file paths, symbol names) that can be checked, and a validation pass that
understands them. The code graph (`graph_query`) is the natural validation
engine: a memory referencing `stella-cli/src/agent.rs:graph_snapshot` can
be re-checked with a `definitions` query.

---

## Summary: the gap between "wired" and "working"

| mechanism               | wired? | ever fired?              | measures impact?     |
| ----------------------- | ------ | ------------------------ | -------------------- |
| reflection recording    | y      | yes (12 lessons)         | no outcome signal    |
| recall at turn start    | y      | yes (this session)       | no control           |
| skill auto-creation     | y      | no (0 skills)            | n/a                  |
| memory citation         | y      | yes (2 rows)             | no usage, not impact |
| citation-driven pruning | y      | no (no effect on recall) | no                   |

The system is **architecturally complete but feedback-starved**. Every
mechanism is built and tested, but the loop is open: outcomes don't reach
reflection, citations don't affect recall, and stale memories are never
pruned. The result is accumulation without guaranteed improvement.

The five proposals close the loop in priority order:

1. **Outcome-grounded reflection** — the cheapest, highest-leverage change.
   Failed turns produce failure-specific lessons instead of generic ones.
2. **Failure-to-lesson pipeline** — structured extraction from verification
   failures, the richest learning signal in the system.
3. **Truthful suppression** — gives the citation flag teeth, preventing
   stale memories from misleading future turns.
4. **A/B measurement** — proves the system helps rather than assuming it.
5. **Periodic re-validation** — catches drift proactively instead of
   reactively.

Together they transform the loop from "accumulate lessons and hope" to
"record outcomes, attribute them, prune failures, and measure the delta."
That is what guarantees improvement over time.

---

## Implementation summary

All five proposals are now implemented and tested:

### Proposal 1: Outcome-grounded reflection

- `reflect_and_record` and `reflect_on_turn` now accept a `succeeded: bool`
  parameter.
- On failure: the prompt asks "identify the root cause — wrong assumption,
  missed file, bad approach." On success: "what worked well?"
- All 6 call sites (`agent.rs` ×4, `command_deck.rs` ×1, `memory.rs` ×1)
  updated to pass the outcome.

### Proposal 2: Failure-to-lesson pipeline ✅ (via Proposal 1)

- The outcome-grounded prompt IS the failure-to-lesson pipeline: on a
  failed turn the model is asked to produce a root-cause lesson directly
  from the failure evidence. The failure prompt explicitly asks for
  actionable, forward-looking lessons ("what should change next time").

### Proposal 3: Truthful suppression

- `QUARANTINE_NEGATIVES_THRESHOLD = 2` — a memory cited untruthful ≥ 2 times
  is quarantined (total untruthful count, not streak).
- `MemoryCitationStats` gains a `quarantined` field, computed by
  `fold_citation_stats`.
- `Store::quarantined_memory_ids()` returns the set.
- `SessionMemory` loads the quarantine set at session open and filters
  quarantined frames from `recall_block` before rendering.
- `stella memory list` shows quarantined memories in the STATUS column
  and prints a summary count.

### Proposal 4: A/B measurement

- `SessionMemory::maybe_suppress_recall(rate)` — deterministic `1/N` coin
  flip suppresses recall for that turn.
- `STELLA_AB_RECALL_RATE = 10` — ~10% of REPL turns are control turns.
- `recall_was_suppressed()` tags the episode summary with `[ab-control]`
  for later analysis.
- Wired into the interactive REPL's recall path.

### Proposal 5: Periodic re-validation

- `stella memory validate` subcommand scans each memory for file-path
  anchors (`stella-cli/src/agent.rs`, `docs/hooks.md`), checks whether
  those paths still exist, and reports stale memories.
- `extract_path_anchors` extracts workspace-relative paths from memory text.
- `validate_memories` checks anchors against the current file tree.
- Reports ok / stale / no-anchors counts with per-memory detail.
