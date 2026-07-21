---
name: renamed-dependency-aliases-need-lockfile-refresh
type: bug
domain: build
severity: P1
linear: none
date: 2026-07-21
---

**Symptom:** The post-merge lockfile listed both the renamed `contextgraph-*` packages and obsolete `ocp-*` dependency names for `stella-cli`.
**Root cause:** The dependency-repin merge resolved `Cargo.lock` textually without regenerating the affected package entry.
**Fix:** Let Cargo regenerate the `stella-cli` dependency list, removing the obsolete lockfile names while preserving the manifest aliases.
**Guard:** Focused CLI tests compile with the regenerated lockfile; future CI should use `--locked` after dependency-renaming merges.
**Watch-outs:** Cargo manifest aliases are source-level names, while lockfile dependency entries use the resolved package names.
