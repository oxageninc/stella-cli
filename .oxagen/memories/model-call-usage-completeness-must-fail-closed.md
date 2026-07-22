---
name: model-call-usage-completeness-must-fail-closed
type: bug
domain: telemetry
severity: P1
linear: none
date: 2026-07-21
---

**Symptom:** Direct pipeline model calls emitted only aggregate budget ticks, multi-turn engine calls reused telemetry identities, and persistence failures could still produce export-eligible execution rollups.
**Root cause:** Paid-call accounting was split across engine, pipeline, renderer, and store boundaries without a durable monotonic completeness invariant.
**Fix:** Emit one role/provider-attributed call envelope at the no-await settlement boundary, emit content-free incompleteness on unknown failures, use event sequence as execution-global call identity, and carry persistence completeness monotonically into execution closeout and export eligibility. Route engine-adjacent and standalone CLI calls through one I/O-free core accounting primitive; give each standalone operation its own execution row and settle exact cost before applying model output.
**Guard:** Focused protocol, engine cancellation/failure, pipeline role matrix, all four standalone call roles, over-budget output rejection, reflection error-cost preservation, CLI persistence, migration, and bounded-backfill tests fail on the old behavior and pass on the corrected path.
**Watch-outs:** Production `Provider::complete` dispatch belongs only in the engine's committed-step boundary or shared core accounting primitive; auxiliary engine calls such as overflow summarization are separate paid roles too. Standalone callers must supply their actual model hint and cumulative remaining budget, persist their own execution, and propagate settled cost and full usage events even on errors and across headless/deck surfaces. Before forwarding a standalone sub-call's budget event, charge its cost to the caller guard and rebase the tick to caller/session totals; operation-relative ticks can move dashboards backward and permit overspend. Aggregate budget totals cannot reconstruct missing per-call telemetry.
