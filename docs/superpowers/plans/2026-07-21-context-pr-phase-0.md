# Context PRs Phase 0 Implementation Plan

**Goal:** Add Git-native context-as-code metadata, validation, and non-mutating proposal previews.

## Task 1 — Pure rule metadata

- Extend `stella-core/src/rules.rs` with optional `RuleMetadata`, enum validation, duplicate-key detection, and deterministic canonical rendering.
- Keep metadata-less and legacy `guard-*` rules valid.
- Add tests for canonical, invalid, and legacy rules; commit `feat(stella-core): add context rule metadata`.

## Task 2 — Explicit promotion writer

- Update `stella-cli/src/memory_cmd.rs` to emit safe metadata for eligible, explicitly promoted memories.
- Keep the current eligibility gate and no-Git-write behavior.
- Add promotion round-trip tests; commit `feat(stella-cli): emit metadata for promoted rules`.

## Task 3 — Read-only Context CLI

- Add `stella-cli/src/context_cmd.rs` and wire `stella context lint` plus `stella context propose memory <id> --dry-run` in `main.rs`.
- Lint only `.stella/rules/*.md`; detect invalid metadata and duplicate IDs without side effects.
- Render a deterministic new-file diff and suggested PR body; reject proposal creation without `--dry-run`.
- Add CLI tests; commit `feat(stella-cli): add context rule lint and proposal dry run`.

## Task 4 — Docs and full verification

- Add `stella-docs/content/docs/commands/context.mdx` and navigation metadata.
- Run `cargo test -p stella-core -p stella-cli -p stella-context`, `cargo fmt --check`, `cargo clippy -p stella-core -p stella-cli -p stella-context --all-targets -- -D warnings`, and the docs build.
- Commit `docs: document context-as-code commands`.
