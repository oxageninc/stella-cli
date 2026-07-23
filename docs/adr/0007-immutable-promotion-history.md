# ADR 0007: Immutable Promotion History

- Status: Accepted — ratified by repository owner 2026-07-23 (was: Proposed)
- Date: 2026-07-23
- Deciders: repository owner (ratified 2026-07-23)

## Context

The adaptive loop promotes observations into proposals into governed records.
For that loop to be auditable and reversible, the promotion trail cannot be
editable after the fact. The canonical plan makes promotions immutable, driven
by a governance state machine. This ADR RECORDS that decision — and FLAGS one
surface conflict between the canonical plan and `context-prs-spec.md` that must
be resolved before Phase 6, because it fixes an enforcement enum.

## Decision

Promotions (`promotion_event`) are **append-only and immutable**. A governance
state machine with modes `solo`, `team`, and `regulated` records every
transition; state changes create new immutable events (consistent with ADR
0004), never in-place edits.

**Enforcement-level mapping (proposed, not ratified):** `context-prs-spec.md`
uses four levels (`observe | advisory | required | blocking`); the canonical
plan uses two (`advisory | blocking`). Record the roadmap's proposed 4→2
mapping — `observe`/`advisory` → `advisory`; `required`/`blocking` → `blocking`
— while leaving open the alternative of keeping four as a UI ramp over two
enforcement states. Also frame "Context PR" as UX over the
`record_proposal → promotion_event` pipeline, not a second mechanism.

**Ratified by the repository owner on 2026-07-23:** adopt the 4→2 mapping
(`observe`/`advisory` → `advisory`; `required`/`blocking` → `blocking`).
`DirectiveEnforcement` has exactly two values (`advisory`, `blocking`); the four
levels may survive only as UI labels over those two enforcement states, never as
a second enforcement enum.

## Consequences

Phase 6 emits immutable `PromotionRecorded` events with a re-proposal cooldown;
no inferred directive may auto-activate as blocking, and no sharing widens
automatically (M-B gate). Whichever enforcement resolution is ratified fixes the
`DirectiveEnforcement` value set and the proposal-review UX, so it must be
locked first.

## Open questions

Resolved 2026-07-23: the repository owner ratified the **4→2** enforcement
mapping. `DirectiveEnforcement` freezes on two values (`advisory`, `blocking`);
the four levels may appear only as UI labels over them.
