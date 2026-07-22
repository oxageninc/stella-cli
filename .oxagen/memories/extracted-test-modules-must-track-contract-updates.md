---
name: extracted-test-modules-must-track-contract-updates
type: bug
domain: tui
severity: P2
linear: none
date: 2026-07-21
---

**Symptom:** CI's extracted deck-render tests expected the retired cache display `cached/input tokens`, while the canonical cache panel and production renderer emitted the current `read rd · write wr` economics format.
**Root cause:** A test-module extraction and a concurrent cache-contract update left duplicated render assertions with only the canonical module updated.
**Fix:** Align the extracted assertions with the cache panel's read/write-volume contract; retain the canonical nonzero-write coverage as the detailed economics witness.
**Guard:** The focused deck-render cache-box tests and canonical cache-panel tests run together and assert the same output contract.
**Watch-outs:** When tests are extracted solely to satisfy the file-size ratchet, search for duplicated behavioral assertions before merging concurrent feature changes.
