---
name: config-test-fixture-credential-source
type: bug
domain: cli-config
severity: P1
linear: none
date: 2026-07-21
---

**Symptom:** The integrated `stella-cli` test target failed with a missing `credential_source` field in the `agent_tests::cfg_for` Config initializer.
**Root cause:** A required production `Config` field was added without updating a direct test fixture initializer.
**Fix:** Initialize `credential_source` to `None` in the offline provider fixture.
**Guard:** The focused CLI telemetry and command-parser tests compile through the shared fixture and pass on the integrated SHA.
**Watch-outs:** Prefer a canonical Config test builder, or update every direct struct literal whenever required provenance fields are added.
