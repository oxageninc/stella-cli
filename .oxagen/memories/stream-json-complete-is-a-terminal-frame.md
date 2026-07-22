---
name: stream-json-complete-is-a-terminal-frame
type: bug
domain: agent-runtime-protocol
severity: P1
linear: none
date: 2026-07-21
---

**Symptom:** Pipeline and raw one-shot reflection events could be written to stdout after a `stream-json` `Complete` event, violating terminal-frame consumers and omitting reflection cost from the execution closeout.
**Root cause:** The renderer treated `Complete` like ordinary narration and the pipeline closed its channel before dispatching post-turn reflection.
**Fix:** The stream renderer defers and deduplicates `Complete`; pipeline reflection events flow through the still-open durable event channel, a final all-call cost frame replaces the early terminal, and closeout happens after the drain. Raw stream mode does not dispatch an unframed post-terminal reflection call.
**Guard:** Sequencer and durable-renderer tests require exactly one `Complete`, require it last, and prove reflection precedes it in the persisted session journal.
**Watch-outs:** Any future post-turn paid call must occur before the renderer drain barrier or use a distinct execution envelope; never print machine events directly after `Complete`.
