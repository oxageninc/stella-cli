---
name: executions-start-pending-and-export-only-after-clean-finalization
type: bug
domain: agent-runtime-telemetry
severity: P1
linear: none
date: 2026-07-21
---

**Symptom:** A newly opened execution was immediately marked usage-complete, so a crash, quit, or stop before finalization could leave an optimistic row eligible for downstream truth claims.
**Root cause:** The store modeled completeness as a default-true boolean and did not distinguish pending from finalized complete or finalized incomplete.
**Fix:** Schema v10 adds an explicit pending/complete/incomplete lifecycle; new rows start pending and non-complete, finalization is monotonic, cancelled closeouts force incomplete, and rollups require a finished complete row.
**Guard:** Store tests cover all three lifecycle states and migration derivation; the producer rollup fixture proves pending executions are refused until clean finalization; CLI closeout coverage proves cancellation stays incomplete even when every local write succeeds.
**Watch-outs:** `usage_complete` is a compatibility projection of `usage_status`, not an independent authority. Never export or roll up a pending execution.
