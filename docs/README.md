# docs/

The user-facing documentation lives in the [`stella-docs/`](../stella-docs/)
Next.js + Fumadocs site (deployed at <https://stella.oxagen.sh>) — edit
`stella-docs/content/docs/` for anything a Stella user should read.

What remains here is the material that isn't site content:

- [`papers/`](papers/README.md) — the research notes behind Stella's design:
  [The Deterministic Engine](papers/deterministic-engine.md) and
  [Stella's Defensible Position](papers/stella-defensible-position.md). The
  live site links to these at their exact paths — don't move or rename them.
- [`brand/`](brand/BRAND_GUIDELINES.md) — brand guidelines and the logo,
  mark, wordmark, and icon assets (current aurora-on-navy cuts, plus the
  retired ember-gold originals under `brand/legacy/`).

Historical design notes for features that have since shipped (pipeline,
hooks, file-touch telemetry, memory citations, code graph, schema gate) were
removed once the features and their site docs superseded them; recover them
from git history if needed.
