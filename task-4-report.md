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

## Review follow-up

Parent review found six additional truth-boundary defects. They were addressed
in a second strict RED/GREEN pass without rewriting the original commit.

### Additional RED evidence

- Paid scope-review and post-witness worker-routing tests failed to compile
  because the pipeline returned a cost-free `PipelineError`; the wished-for
  `PipelineRunError.cause` and `total_cost_usd` fields did not exist.
- The verification event-stream test failed because a red verdict still
  emitted `AgentEvent::Complete`.
- A preseeded, over-session-cap overflow turn failed because the summarizer
  provider was called before the budget check.
- A preseeded goal reported `0.761` instead of the goal-local `0.011` delta.
- Cancellation closeout tests failed because no dispatch-ledger delta helper
  existed, and the deck/subsession paths persisted literal zero.
- Deserializing a legacy aborted serve frame failed with
  `missing field cost_usd`.
- A CLI closeout regression separately failed before the shared helper existed,
  proving hard pipeline errors would otherwise still be persisted as `$0` by
  surface code.

### Review fixes

- `Pipeline::run` now returns `PipelineRunError { cause, total_cost_usd }` for
  hard errors. Paid scope-review and post-witness worker-routing failures retain
  every prior stage's settled cost, and CLI/deck use one tested closeout mapper.
- Verification failure emits a non-retryable error containing the verdict
  summary and never emits the success-only `AgentEvent::Complete`.
- The engine checks an existing enforced breach before paid compaction, while
  keeping the existing post-compaction check for a summarizer-induced breach.
- Every `GoalOutcome` exit reports the session-ledger delta from goal entry,
  excluding spend that predates the goal.
- Deck and subsession cancellation capture the session ledger at dispatch and
  persist its settled delta. Cancellation before spend remains exactly zero.
- `TurnOutcomeWire::Aborted.cost_usd` has a serde default so pre-cost wire
  records remain readable as zero while new frames retain explicit cost.
- Pipeline hard-error types moved into a child module to keep the orchestrator
  below the file-size ratchet.

### Expanded verification

- `cargo test -p stella-core`: 318 passed.
- `cargo test -p stella-pipeline`: 114 unit tests and 4 replay tests passed.
- `cargo test -p stella-fleet`: 69 passed.
- `cargo test -p stella-cli`: 326 passed.
- `cargo test -p stella-serve`: 2 unit, 3 bridge, and 2 HTTP tests passed.
- `cargo test -p stella-tui`: 486 passed, 1 ignored; 4 deck snapshots and 1
  progress-gold integration test passed.
- `cargo test -p stella-protocol`: 42 passed.
- Workspace clippy with `-D warnings`, formatting, file-size ratchet, and diff
  whitespace checks passed.

## Speculative-settlement follow-up

The final review found one remaining cancellation window: a provider could
finish a billed `complete_observed` call while a speculative read remained
blocked, but the engine joined both futures before recording the cost. Dropping
the turn in that interval discarded the successful provider result and left
the caller-owned ledger at zero.

### Final RED/GREEN evidence

- `cancellation_after_billed_completion_before_speculation_finishes_keeps_the_cost`
  deterministically failed RED with a `$0` session ledger after the provider
  returned a `$0.25` result and the turn was cancelled while the speculative
  read stayed blocked.
- A first settlement-channel prototype remained RED because cancellation
  could win before the outer receiver was repolled. It was discarded rather
  than papering over the race.
- The retry attempt now returns the successful completion together with its
  still-live speculation future. `run_model_call` records that result in the
  borrowed `BudgetGuard` synchronously, with no await point, and only then
  awaits speculative work. The later bookkeeping path consumes the carried
  `BudgetOutcome` and cannot charge again.
- `a_normal_completion_charges_the_budget_exactly_once` pins the non-cancelled
  path and confirms the ledger receives one, and only one, charge.
- The pipeline event-ownership documentation now names both terminal event
  outcomes: success is `Complete`; terminal status failure is a non-retryable
  `Error`. Hard infrastructure failures remain typed `PipelineRunError`
  returns for caller closeout.

### Final verification

- `cargo test -p stella-core`: 320 passed.
- `cargo test -p stella-cli`: 326 passed, including all command-deck and
  subsession tests.
- `cargo test -p stella-pipeline`: 114 unit tests and 4 replay tests passed.
- `cargo test -p stella-fleet`: 69 passed.
- `cargo test -p stella-serve`: 2 unit, 3 bridge, and 2 HTTP tests passed; the
  HTTP tests required permission to bind a local loopback port.
- Workspace clippy with `-D warnings`, formatting, file-size ratchet, and diff
  whitespace checks passed.

No push was performed.
