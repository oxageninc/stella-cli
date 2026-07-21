---
name: host-media-operation-journal-private-sqlite
type: bug
domain: media-authority
severity: P1
linear: none
date: 2026-07-21
---

**Symptom:** The durable paid-media replay journal accepted permissive or aliased SQLite files and could mutate a substituted database before rejecting it.
**Root cause:** Journal startup used `create_dir_all` plus ordinary path-based SQLite open, and the first hardening pass closed its validated file before SQLite initialization.
**Fix:** Create host parents at 0700, atomically precreate the database at 0600, hold and revalidate its opened identity across SQLite open, validate WAL/SHM state before and after initialization, and exclude the resolved workspace before creating state.
**Guard:** Unix regressions cover modes, symlinks, hardlinks, unsafe sidecars, concurrent initialization, replay, workspace containment, and injected main-file/sidecar substitution before initialization.
**Watch-outs:** Any new authority or idempotency SQLite store must preserve the opened-file identity across the handoff to SQLite; a check-then-close helper still permits mutation-before-rejection races.
