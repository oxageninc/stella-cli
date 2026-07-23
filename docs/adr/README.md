# Architecture Decision Records — Phase 0 (Adaptive Context)

These ADRs capture the baseline decisions for Phase 0 of the adaptive-context
work in stella. Most RECORD decisions already made by the canonical planning
pair (`adaptive-context-implementation-plan.md` and
`stella-adaptive-context-lifecycle.md`). Each ADR grounds its claims in the
source docs and, where relevant, the current stella code.

ADRs 0002 and 0007 originally FLAGGED open questions for human sign-off; the
repository owner **ratified both on 2026-07-23**, so all Phase 0 ADRs are now
Accepted. The ratified resolutions: `SharingScope` is the 4-value set
(`user, repository, workspace, organization`); `DirectiveEnforcement` is the
2-value set from the 4→2 mapping. The related `Origin`-arity item (ADR 0001) was
spec-verified as the full 5-value set for all families.

| # | Title | Status |
|---|---|---|
| [0001](0001-semantic-taxonomy.md) | Semantic Taxonomy | Accepted (Phase 0) |
| [0002](0002-scope-vs-sharing.md) | Scope vs. Sharing | Accepted — ratified 2026-07-23 (4-value SharingScope) |
| [0003](0003-bitemporal-semantics.md) | Bitemporal Semantics | Accepted (Phase 0) |
| [0004](0004-record-revision-identity.md) | Record Revision Identity | Accepted (Phase 0) |
| [0005](0005-storage-authority.md) | Storage Authority | Accepted (Phase 0) |
| [0006](0006-contextframe-vs-compiledcontextframe.md) | ContextFrame vs. CompiledContextFrame | Accepted (Phase 0) |
| [0007](0007-immutable-promotion-history.md) | Immutable Promotion History | Accepted — ratified 2026-07-23 (enforcement 4→2) |
| [0008](0008-markdown-canonical-rules.md) | Markdown Repository Rules Remain Canonical | Accepted (Phase 0) |
