# Stella paid-call usage completeness

The defect crossed four boundaries: provider dispatch, cancellation, local persistence, and enterprise backfill. Fixing only the visible aggregate would still allow unknown usage into trusted rollups.

The durable invariant is monotonic: each dispatched call yields either one complete role/provider envelope or one content-free incomplete marker, and any later write failure permanently downgrades the execution. Export selection filters eligibility without consuming incomplete rows, so a bounded page remains live.

The most useful regression shape combined exact-once cancellation at the cost-settlement no-await boundary with a duplicate event/telemetry write failure followed by an otherwise successful closeout.

A follow-up inventory found four standalone CLI dispatches outside the engine and pipeline chokepoints. The reusable boundary is an I/O-free core accounting primitive; CLI adapters own execution persistence and closeout, while exact settled cost crosses both success and structured error paths so over-budget output is never applied before its paid call is recorded.

Independent review exposed three secondary seams: engine overflow summarization was also a paid role, multi-attempt callers must decrement one cumulative operation budget, and persisting a private usage channel is insufficient when machine/deck consumers enumerate the public event stream. A must-use reflection report now carries the full envelope and cost to every surface.

Standalone sub-call events cannot be forwarded verbatim into a caller-scoped stream: their budget guard starts at zero and uses the caller's remaining limit. The caller must record the settled sub-call cost, discard the operation-relative tick, and emit one cumulative caller/session tick so HUD state and future admission checks stay monotonic.
