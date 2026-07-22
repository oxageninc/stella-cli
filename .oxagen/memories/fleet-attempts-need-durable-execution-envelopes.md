---
name: fleet-attempts-need-durable-execution-envelopes
type: bug
domain: agent-runtime-telemetry
severity: P1
linear: none
date: 2026-07-21
---

**Symptom:** Fleet workers drained all agent events into a null consumer, so their paid-call usage, incomplete-attempt markers, and execution closeouts never reached the workspace telemetry store.
**Root cause:** The fleet composition root treated the event channel as a liveness requirement instead of the durable accounting stream used by every other execution surface.
**Fix:** Every fleet attempt now opens a worktree-local execution, uses the shared persistent renderer, waits for its drain barrier, and records an outcome/cost closeout; stopped and hard-error attempts force incomplete.
**Guard:** Focused fleet tests require successful attempt usage to be persisted before complete closeout and stopped attempts to remain non-rollupable.
**Watch-outs:** Never replace a fleet event consumer with a drain-only task. Finalization must occur after the receiver finishes so late `StepUsage` and `UsageIncomplete` events are durable.
