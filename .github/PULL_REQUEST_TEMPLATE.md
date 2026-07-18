<!--
  Thanks for contributing to Stella! Keep it to one logical change per PR.
  Full expectations: CONTRIBUTING.md — the short version is the checklist below.
-->

## What & why

<!-- What does this PR do, and what problem does it solve? Link the issue if one exists. -->

Closes #

## The witness

Stella's definition of done is a test that **fails on the old code and passes on the new**.

- [ ] This PR includes a witness test (fails on `main`, passes here), **or**
- [ ] No witness needed (pure refactor / docs / CI) — because:

<!-- If the witness is impractical (e.g. TUI rendering), say how you verified instead. -->

## The gate

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Docs updated where behavior/flags changed (README, `--help`, doc comments)
- [ ] Commits signed off for DCO (`git commit -s`)

## Ground-rule check

<!-- Delete lines that don't apply. -->

- [ ] No I/O added to `stella-core`; no new deps without justification below
- [ ] No new outbound network calls (Stella never phones home)
- [ ] New cross-boundary types round-trip through serde (test included)

## Anything reviewers should know?

<!-- Risky areas, follow-ups deferred, alternative approaches you rejected. -->
