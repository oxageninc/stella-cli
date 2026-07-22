---
name: successful-retries-preserve-failed-attempt-usage-gaps
type: bug
domain: agent-runtime-telemetry
severity: P1
linear: none
date: 2026-07-21
---

**Symptom:** A provider attempt could fail after dispatch, retry successfully, and leave only the successful attempt's usage envelope, falsely making the execution look complete.
**Root cause:** Retry history was exposed only after a committed success, while failed attempts had no synchronous accounting observation point.
**Fix:** The retry primitive now exposes every failed dispatched attempt to accounting callers; both the engine driver and standalone accounted-call path emit content-free `UsageIncomplete` events immediately while retaining the successful attempt's known cost and usage.
**Guard:** Focused core tests cover fail-then-success for both engine and standalone calls, require one incomplete envelope plus the successful `StepUsage`, and verify private provider error content is absent from the accounting event.
**Watch-outs:** A successful retry never restores execution completeness; usage completeness is monotonic false once any dispatched attempt has unknown usage.
