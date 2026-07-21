---
name: telemetry-auth-command-merge-delimiter
type: bug
domain: cli
severity: P1
linear: none
date: 2026-07-21
---

**Symptom:** After merging the authentication command surface, every `stella-cli` test failed to compile because `TelemetryCmd` had no closing brace before `AuthCmd`.
**Root cause:** Adjacent enum additions were merged without compiling the integrated branch.
**Fix:** Close `TelemetryCmd` before the `AuthCmd` declaration.
**Guard:** `main_tests::telemetry_status_remains_a_distinct_top_level_command` parses `stella telemetry status`; the old source cannot compile and the repaired source passes.
**Watch-outs:** Always compile the exact post-merge SHA when adjacent Clap command enums change.
