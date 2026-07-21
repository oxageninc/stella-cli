## Self-Evaluation — enterprise authority telemetry documentation — 2026-07-21

### What I set out to do

Replace absolute local-only and no-phone-home claims with the implemented boundary:
Community/default has zero telemetry egress, while explicitly enrolled Oxagen
Enterprise managed mode may export a minimal operational envelope from process-free
eligible execution paths.

### What I actually did (measurable deltas)

- Updated 11 public contributor, README, and documentation surfaces.
- Documented signed org-managed enrollment, exact HTTPS sink matching, the sole eligible
  raw one-shot path, exported and structurally excluded fields, spool/backfill/retry/
  rollover behavior, status and flush diagnostics, and detached shutdown behavior.
- Distinguished Stella's local execution responsibility from Oxagen's optional control
  plane and stated that production server intake is a companion requirement.
- Ran the contradiction audit, formatting and size checks, docs typecheck, and a full
  production docs build; obtained and fixed every Important independent-review finding.

### Quality of my decisions

- Best decision I made and why: I derived every numerical and security claim from the
  implementation before writing. That prevented the docs from promising a wider
  execution surface or richer envelope than the closed Rust types permit.
- Weakest decision I made and why: I initially described the 16 MiB limit as a spool
  bound instead of a retained-payload bound. SQLite overhead makes that materially
  different, and an independent reviewer had to catch it.

### What I could have done better

- I should have distinguished fleet-wide operational visibility from the rejected
  `stella fleet` execution surface in the first draft instead of using the ambiguous
  phrase “fleet operations.”
- I should have inspected the physical-size accounting beside `SpoolLimits` before
  drafting the capacity language, rather than relying on the limits struct alone.
- I could have started the independent review earlier, in parallel with the first docs
  build, to shorten the final checkpoint cycle.

### What surprised me about this codebase/product

The export boundary is enforced by both type shape and execution authority: sensitive
content is not merely redacted, and managed enrollment deliberately disables all but a
single process-free raw one-shot surface.

### Risks I am leaving behind (untouched on purpose, and why)

- The Stella repository contains a delivery client but not the production Oxagen intake;
  server authentication, tenancy, idempotency, retention, and observability remain a
  companion implementation requirement outside this documentation-owned task.
- `rollover-discard` is intentionally destructive and operator-triggered; the docs name
  it and its durable counter but do not invent an automated retention policy.

### Confidence in the result: high

Evidence: contradiction audit clean on all owned surfaces; `cargo fmt --all -- --check`,
`make sizes`, docs TypeScript checking, and the 81-page production docs build pass; the
independent read-only docs/security re-review reports no remaining Critical or Important
findings.
