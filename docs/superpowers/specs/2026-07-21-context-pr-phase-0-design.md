# Context PRs Phase 0 — Context as Code Foundation

## Goal

Make `.stella/rules/*.md` a Git-native, reviewable source of repository steering while preserving Stella's existing local-first behavior.

## Decisions

- Git-backed Markdown rules are the only repository-policy authority; the context graph is derived runtime context.
- Existing rule files and legacy `guard-*` keys remain compatible.
- New rules may carry safe metadata: schema version, stable record ID, kind, scope paths, enforcement, confidence, origin, evidence IDs, and validity timestamps.
- Raw prompts, private telemetry, credentials, customer data, and personal preferences never enter rule files.
- `stella memory promote` remains explicit and local; it does not stage, commit, push, or open a pull request.

## Phase 0 delivery

1. Add a pure optional metadata parser/validator to `stella-core::rules`.
2. Preserve rule precedence and existing prompt/guard behavior.
3. Update promoted rules to emit canonical metadata.
4. Add read-only `stella context lint` and mandatory-dry-run `stella context propose memory <id> --dry-run`.
5. Render a deterministic rule diff and suggested PR body, but perform no Git or network action.
6. Document the commands and prove no-mutation behavior in tests.

## Explicitly deferred

Centralized graphs, automatic inference/promotion, GitHub PR creation, owner routing, CI checks, new blocking enforcement, and team governance.

## Acceptance criteria

- Existing rules load unchanged.
- Invalid metadata produces actionable lint output.
- Promoted rules are metadata-bearing and Git-reviewable.
- Proposal dry runs never mutate the workspace, Git, databases, or a remote.
