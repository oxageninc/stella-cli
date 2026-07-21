# Task 4 report: truthful outcomes, settled cost, and stage budgets

## Outcome

Task 4 is complete. Aborted engine turns now carry their settled cost, a red
pipeline verdict has a distinct `VerificationFailed` terminal status, and an
enforced budget breach in any paid pipeline stage stops the next paid stage.
CLI, goal, fleet, deck, subsession, and serve consumers preserve the new
terminal truth instead of converting it to success or `$0`.

## TDD evidence

RED was established before implementation:

- `cargo test -p stella-core summary_induced_budget_breach_aborts_with_cost_before_next_provider_call -- --nocapture`
  failed to compile with `E0026` because `TurnOutcome::Aborted` had no
  `cost_usd` field.
- `cargo test -p stella-pipeline red_final_verdict_is_verification_failed_not_completed -- --nocapture`
  failed to compile with `E0599` because
  `PipelineStatus::VerificationFailed` did not exist.
- `cargo test -p stella-pipeline enforced_budget_breach_in_triage_stops_before_the_next_paid_stage -- --nocapture`
  then failed behaviorally because the pipeline continued from an over-cap
  triage call into planning.

The focused tests became green only after cost and budget-stop propagation was
implemented. The core regression proves a paid summarization pass is included
in an abort and prevents the next provider call. The pipeline regressions prove
red verification is not completion, its evidence and cost survive, and an
over-cap triage result stops before planning.

## Implementation notes

- `TurnOutcome::Aborted` now carries `cost_usd`; every engine abort is created
  at the outer turn boundary from the accumulated settled spend.
- Summary, model, soft-stop, loop, retry-exhaustion, empty-result, budget, and
  step-cap exits retain spend already incurred.
- Raw pipeline role calls return a required budget result after recording
  spend. Triage, plan, plan repair, witness, judge guidance, and judge cannot
  ignore an enforced stop. Observed mode still emits a warning and continues.
- Engine-backed execute, revision, and witness turns fold aborted cost into the
  pipeline total.
- A final red verdict returns
  `PipelineStatus::VerificationFailed { verdict }`, while `Completed` is
  reserved for passed or unnecessary verification.
- CLI JSON/audit records, the interactive deck, fleet summaries, goal loops,
  subsessions, and serve wire frames distinguish failure and retain settled
  cost. Goal persistence uses the shared budget ledger at teardown so a paid
  judge abort cannot disappear.
- New helper/test modules keep the legacy `driver.rs` and `pipeline.rs` files
  within the repository size ratchet.

## Verification

- `cargo test -p stella-core`: 316 passed.
- `cargo test -p stella-pipeline`: 113 unit tests and 4 replay tests passed.
- `cargo test -p stella-fleet`: 69 passed.
- `cargo test -p stella-cli`: 323 passed.
- `cargo test -p stella-serve`: 1 unit, 3 bridge, and 2 HTTP tests passed.
  The HTTP tests required permission to bind a local loopback port.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `scripts/check-file-sizes.sh`: passed for all 285 tracked Rust files.
- `git diff --check`: passed.

## Self-review

- I removed an internal placeholder `$0` abort value after noticing it could
  obscure where cost truth is established. Provider-call failures now return a
  reason to the outer turn boundary, which attaches the accumulated cost once.
- I changed goal execution persistence to read the settled budget ledger at
  teardown after noticing that an aborted judge has no successful
  `judge_cost` return value even though its spend is already metered.
- I searched all `TurnOutcome::Aborted` and `PipelineStatus` matches and all
  raw pipeline budget-recording sites. Remaining explicit zero-cost terminal
  paths are cancellations or runtime-construction failures that occur before a
  provider call.

## Concerns

No known correctness concerns remain in Task 4 scope. The serve HTTP suite's
loopback permission requirement is environmental, not a product failure.
No push was performed.
