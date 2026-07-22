---
name: successful-call-usage-completeness-survives-local-abort
type: bug
domain: agent-telemetry
severity: P2
linear: none
date: 2026-07-21
---

**Symptom:** CI expected a successful triage call's `StepUsage.complete` field to become false when the pipeline subsequently aborted at its local budget gate.
**Root cause:** The regression test conflated provider-call usage completeness with the eventual outcome of the enclosing pipeline.
**Fix:** Assert that truthful terminal provider usage remains complete across a later local budget abort, and that no synthetic `UsageIncomplete` event is emitted for the settled call.
**Guard:** `pipeline::tests::usage::triage_success_emits_usage_before_budget_abort` fails if a later local abort retroactively invalidates complete provider usage.
**Watch-outs:** `StepUsage.complete` is a per-call accounting property. Pipeline, execution, and export eligibility are tracked by their own outcome and lifecycle fields.
