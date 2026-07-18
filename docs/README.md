# Stella documentation

The map of Stella's docs. Start with the [README](../README.md) for install and
usage; this tree is the deeper material.

## Guides

- [`why-stella.md`](why-stella.md) — the case for Stella: deterministic done,
  a reason-about-able engine, local-only telemetry, BYOK.
- [`cli-reference.md`](cli-reference.md) — every command, flag, and environment
  variable.

## Architecture & design

Internal design notes — how a subsystem works and why it is built that way.

- [`design/pipeline.md`](design/pipeline.md) — the staged
  triage → plan → scope-review → witness → execute → verify → judge pipeline
  (the default `stella run` path), the distress loop, and the `/pipeline` toggle.
- [`design/memory-citations.md`](design/memory-citations.md) — the memory
  citation feedback loop: usefulness/truthfulness scoring, quarantine, and
  rule promotion.
- [`design/self-improvement-ideas.md`](design/self-improvement-ideas.md) — the
  reflection-mining and skill auto-promotion design.
- [`design/hooks.md`](design/hooks.md) — lifecycle hooks (`SessionStart`,
  `PreToolUse`, `PostToolUse`) and the project-scope trust boundary.
- [`design/file-touch-telemetry.md`](design/file-touch-telemetry.md) — the
  files-touched CRUD ledger.
- [`design/graph-tool-analysis.md`](design/graph-tool-analysis.md) — the
  tree-sitter code-graph tool.
- [`design/schema-graph.md`](design/schema-graph.md) — the graph schema.
- [`design/telemetry-data-plane-spec.md`](design/telemetry-data-plane-spec.md) —
  the local telemetry data plane that the `stella observe` dashboard reads.

## Open Context Protocol (OCP)

The wire protocol that lets Stella fan retrieval out to context providers.

- [`ocp/README.md`](ocp/README.md) — start here.
- [`ocp/overview.md`](ocp/overview.md) · [`ocp/protocol-surface.md`](ocp/protocol-surface.md)
  · [`ocp/protocol-advantages.md`](ocp/protocol-advantages.md)
- [`ocp/implementing-a-provider.md`](ocp/implementing-a-provider.md) ·
  [`ocp/running-conformance.md`](ocp/running-conformance.md) ·
  [`ocp/stability.md`](ocp/stability.md)

## Papers

Positioning and the deeper "why."

- [`papers/README.md`](papers/README.md)
- [`papers/deterministic-engine.md`](papers/deterministic-engine.md)
- [`papers/stella-defensible-position.md`](papers/stella-defensible-position.md)

## For contributors

- [`../AGENTS.md`](../AGENTS.md) — conventions and invariants for anyone (human
  or agent) working in this repo.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — how to contribute.
- [`../RELEASING.md`](../RELEASING.md) · [`../PUBLISHING.md`](../PUBLISHING.md) —
  the release and publish process.
- [`../SECURITY.md`](../SECURITY.md) — reporting vulnerabilities.
