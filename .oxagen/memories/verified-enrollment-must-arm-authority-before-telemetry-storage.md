---
name: verified-enrollment-must-arm-authority-before-telemetry-storage
type: bug
domain: enterprise-telemetry
severity: P1
linear: none
date: 2026-07-21
---

**Symptom:** A valid signed managed enrollment could hit a spool, store, ledger, or registry-proof error before process-free authority was armed, silently leaving the agent with Community/full execution authority; malformed managed IDs or legacy pending nonces could also fail after ledger mutation and stop later backfill rows.
**Root cause:** Credential scrubbing and authority activation occurred at the end of fallible delivery setup, while managed identifiers remained raw strings until event construction and the backfill loop propagated projection errors.
**Fix:** Register verified credential names immediately, enter a restricted failed-closed authority state before proving the concrete registry, activate process-free authority before delivery/storage setup, retain restrictions on proof failure, validate bounded identifiers inside `VerifiedEnrollment`, and atomically move malformed legacy pending candidates into a distinct durable skip table with closed reason counters while continuing the bounded page.
**Guard:** Stella CLI witnesses cover proof failure, host-path/store/identity/spool/sender errors, pre-mutation identifier rejection, credential scrubbing, and a real malformed-before-valid legacy page; store witnesses prove skips are durable, distinct from spooled, never retryable, and introduced without rebuilding or copying a 50,257-row legacy ledger.
**Watch-outs:** Telemetry delivery is fail-open for the completed agent outcome; signed execution authority is not. Any new consumer of `verify_managed_enrollment` must activate authority before touching host or ledger state.
