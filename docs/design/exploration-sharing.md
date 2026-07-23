# Design: Session-Shared Exploration Maps

**Status:** Draft — not yet implemented.
**Goal:** Any Stella session, at turn 1, knows every exploration map any session
has ever produced in this workspace, knows *per file* whether each map is still
valid, and knows what other **live** sessions are currently mapping — so the
orientation cost of a task is paid once per repo-state, not once per session.
**Mandate:** This is a consolidation spec. Every existing context
gathering/shaping/caching asset in the workspace is accounted for — extended,
wired, or explicitly bounded — in §11; nothing valuable is left unwired.

---

## 0. The problem, observed

Three sessions were started back-to-back in the same workspace within one hour
(tool-search feature, storage-layer schema spec, docs-site audit). Each began
with the same ritual: `ls` the root, `cat` the manifest, read the docs tree,
re-derive the crate map. Each burned roughly 10–40k input/output tokens and
1–3 minutes before doing any task-specific work.

Meanwhile `.stella/explorations/workspace-overview.json` — a 21 KB,
16-crate architecture map covering exactly that orientation — had been sitting
on disk for days. **No session read it.** That is not a model failure; it is a
systems failure with five concrete causes, all verifiable in code:

| # | Cause | Evidence |
|---|---|---|
| 1 | **Discovery is pull-only.** The `explorations` tool exists but nothing surfaces the store at session start; the model must *think* to call it before exploring. | `stella-tools/src/exploration.rs:128-216`; system prompt assembly injects memories and rules, never the exploration index (`stella-cli/src/agent.rs:141-152`) |
| 2 | **Staleness is repo-global and binary.** A map's only validity signal is `git rev-parse HEAD` equality — one commit anywhere degrades *every* map, and a mismatch says nothing about *which* covered files actually moved. | `exploration.rs:162-171` |
| 3 | **No in-flight visibility.** The session registry knows pids, titles, statuses — not what a session is exploring. Two live sessions can map the same slice concurrently and neither knows. | `stella-store/src/sessions.rs:79-93` |
| 4 | **The context plane cannot see explorations.** Maps are not embedded, not nodes in `context.db`, never recalled, and not linked to the code graph. The per-turn recall block carries only memories/episodes (5 frames / 1,200 tokens). | `stella-cli/src/memory.rs:661-670`; provider seam "designed here but not yet built" (`stella-context/src/provider.rs:6-10`) |
| 5 | **Two artifact systems, two staleness models.** `gather_context` packs already carry a per-file `path → sha256` manifest re-hashed on read; explorations carry only `git_head`. Same problem, disjoint oracles. | `stella-tools/src/gather.rs:79-92, 230-240` vs `exploration.rs:51-54` |

This is the same *passive-vs-active* failure the schema-graph spec identified
for schema drift (`docs/design/schema-graph.md`, "Retrieval is passive — the
agent must think to look"), and the same two-surfaces problem the telemetry
spec diagnosed for the code graph (`docs/design/telemetry-data-plane-spec.md`
§0.1). The fix follows the same playbook: make the knowledge **live** (staleness
tracked per file, continuously), make consultation **active** (injected and
hinted, not merely available), and make production **expected** (nudged at the
moment the ledger shows unmapped exploration happened).

---

## 1. Correcting the premise: what exists today

The original framing was "use the existing filelock system backed by git
hashes to determine staleness." Ground truth from the code — these are **two
unrelated subsystems**, and this spec deliberately recombines their ideas:

- **`file_locks`** (`stella-store/src/lib.rs:516-520, 1835-1875`) is a
  path-keyed, holder-string, cooperative claims table used only by
  `stella-fleet` dispatch (`stella-fleet/src/fleet.rs:356-406`). It contains
  **no hashes of any kind**, has no renew and no expiry, and a crashed run
  leaks its rows. What it *does* contribute is a proven pattern: **claim
  identity embeds liveness evidence** (holder = `run_id/task_id`, where
  `run_id` embeds start-time + pid, `fleet.rs:349-354`).
- **Per-file content hashing** exists in two places, neither of them a lock:
  pack manifests (`gather.rs:141-143, 230-240` — SHA-256 of file bytes,
  re-hashed on read, exact drift report) and the code graph's incremental
  index (`stella-graph/src/store.rs:194-200` — `content_sha256` per file,
  kept live by the 200 ms-debounced watcher, `stella-graph/src/watch.rs`).
- **Git hashes proper** appear only as `git rev-parse HEAD` provenance
  (packs: display-only; explorations: the coarse compare of cause #2).

**Design consequence:** validity comes from the *pack-style per-file SHA-256
manifest* (precise, already implemented, git-independent so it works with
dirty working trees); `git_head` stays as provenance and as a cheap
short-circuit; and the *claims pattern* (identity + pid-derived liveness) is
reused for in-flight exploration signaling — without writing lock rows at all
(§4).

---

## 2. Architecture: one store, four surfaces

No new daemons, no new databases, no deletion of anything. JSON files under
`.stella/explorations/` remain the single source of truth (inspectable,
diffable, durable); everything else is a rebuildable index or a read-time
computation. This rides the existing event-sourced session lifecycle — every
new behavior hangs off an existing hook point (session start, tool dispatch,
turn finalize).

```
                       ┌──────────────────────────────────────────┐
        Surface D      │ Code-graph integration                    │
        (§6)           │ graph_query neighbors → "covered by map X"│
                       │ coverage hints on grep/read/glob results  │
                       └──────────────▲───────────────────────────┘
                       ┌──────────────┴───────────────────────────┐
        Surface C      │ Context-plane ingestion (wires the seam)  │
        (§5)           │ exploration → node in context.db, embedded│
                       │ summary, File edges → rides recall_scoped │
                       └──────────────▲───────────────────────────┘
                       ┌──────────────┴───────────────────────────┐
        Surface B      │ Session awareness                         │
        (§4)           │ startup index injection · draft-as-claim  │
                       │ registry `exploring` field · deck overlay │
                       └──────────────▲───────────────────────────┘
                       ┌──────────────┴───────────────────────────┐
        Layer A        │ Record v2 + shared staleness oracle       │
        (§3)           │ per-file sha256 manifest · Fresh/Drifted/ │
                       │ Unknown verdicts · auto-manifest from the │
                       │ file-touch ledger                         │
                       └──────────────────────────────────────────┘
```

---

## 3. Layer A — `ExplorationRecord` v2 and the shared staleness oracle

### 3a. Record schema (additive; v1 records keep deserializing via defaults)

```rust
// stella-tools/src/exploration.rs
struct ExplorationRecord {
    // ── existing v1 fields, unchanged ──
    slice: String,
    title: String,
    summary: String,
    content: String,          // markdown map
    files: Vec<String>,
    created_at_ms: u64,
    git_head: Option<String>,

    // ── new in v2 (all #[serde(default)]) ──
    /// Per-file validity oracle: workspace-relative path → SHA-256 of file
    /// bytes at save time. Superset of `files` (may include configs/docs the
    /// map depends on). THE staleness signal.
    manifest: BTreeMap<String, String>,
    /// `draft` = exploration in progress (doubles as the in-flight claim, §4c);
    /// `complete` = finished map. Default `complete` for v1 compat.
    status: ExplorationStatus,
    /// Session that produced/last updated it (`ses-<ms>-<pid>` from the
    /// registry, stella-store/src/sessions.rs:97-110) — provenance + liveness.
    session_id: Option<String>,
    /// Execution row in .stella/private/store.db that produced it — provenance into
    /// the event log.
    execution_id: Option<i64>,
    /// Key symbols the map covers (graph anchors for §6). Optional.
    symbols: Vec<String>,
}
```

### 3b. The freshness verdict

Extract the manifest/staleness code already shipped in `gather.rs`
(`hex_sha256`, `stale_paths` — `gather.rs:141-143, 230-240`) into a shared
module (`stella-tools/src/staleness.rs`) used by **both** packs and
explorations. One oracle, two artifact types (resolves cause #5).

```rust
pub enum Freshness {
    /// Every manifest entry re-hashes to the same value.
    Fresh,
    /// Some files moved or vanished. The map is degraded, not dead — the
    /// listing says exactly which parts to re-verify.
    Drifted { changed: Vec<String>, missing: Vec<String> },
    /// v1 record with no manifest — fall back to the old git_head compare.
    Unknown { head_moved: bool },
}
```

Verdict rules, in order:

1. `manifest` empty → `Unknown { head_moved: saved_head != current_head }`
   (exactly today's behavior; v1 records never get *worse*).
2. `git_head` matches current HEAD **and** `git status --porcelain` is empty →
   `Fresh` without hashing anything (the short-circuit git gives us for free).
3. Otherwise re-hash every manifest entry (identical to `read_pack`,
   `gather.rs:665-693`). All match → `Fresh`; else `Drifted` with the exact
   lists.

Cost note: SHA-256 over a few dozen source files is single-digit milliseconds;
this runs at session start (once) and on explicit reads, never per turn. A
later optimization can join the manifest against the graph's live
`code_graph_files.content_sha256` (`stella-graph/src/store.rs:40-47`) to skip
I/O for indexed files — but the graph only covers six languages and skips
`.md`/config files, so direct hashing stays the correctness path. Do not build
the join until profiling demands it.

### 3c. Rendering staleness to the model

Replace the current binary warning (`exploration.rs:162-171`) with the
verdict:

- `Fresh` → `(fresh)` — and *say so affirmatively*; today a map with a moved
  HEAD reads as untrustworthy even when nothing it covers changed. That
  over-invalidation is cause #2 and actively trains agents to ignore the store.
- `Drifted` → `(drifted: 2/9 files changed — sections touching
  stella-tui/src/render.rs, stella-cli/src/agent.rs need re-verification;
  rest is current)`.
- `Unknown` → today's soft HEAD warning.

**Stale maps are never deleted** and never hidden. A drifted map plus the
exact list of moved files is still a 90% token saving over re-exploring; the
agent re-verifies two files instead of thirty. Refresh happens by overwrite
(`save_exploration` same-slice semantics, unchanged).

### 3d. Auto-manifest from the file-touch ledger

The model should not hand-author the manifest. `SaveExploration::execute`
computes it:

- Union of: the `files` array the model passed, plus every path with an `R`
  event in the session's `FileTouchLedger`
  (`ToolRegistry::file_touch_telemetry`, `stella-tools/src/file_touch.rs`) at
  save time, filtered to paths that still exist.
- Hash each; store `path → sha256`. Cap at 200 entries, largest-degree first
  (an exploration that read 500 files is really several slices — the save
  response should say so).

The ledger already records every `read_file`/`grep`-adjacent touch with
reasons (`docs/design/file-touch-telemetry.md`), so the manifest is exactly
"the evidence this map was built from" — with zero extra model tokens.

---

## 4. Surface B — session awareness

### 4a. Startup index injection (the active half of discovery)

At system-prompt assembly (`assemble_system_prompt`,
`stella-cli/src/agent.rs:141-152`), after workspace memories, inject a
**workspace-maps index** — *metadata only, never map bodies*:

```
## Workspace maps (shared across all Stella sessions)
- `workspace-overview` — Stella workspace: full architecture map (fresh, 34 files, 3d old)
- `stella-media-wiring` — media pipeline + TUI wiring (drifted: 2/9 files changed, 4d old)
- `cli-agent-loop` — IN PROGRESS by session ses-…-41782 (live, started 4m ago) — read the draft before duplicating
Read one with explorations({"slice": …}). Check this list BEFORE exploring any
area; after mapping unmapped territory, persist it with save_exploration —
the next session reuses your map instead of re-paying for it.
```

- Budget: `EXPLORATION_INDEX_BUDGET_CHARS = 2_000` (~40–70 tokens per record;
  newest-first, drop-with-notice past the cap — same pattern as
  `MEMORY_PROMPT_BUDGET_CHARS`, `agent.rs:119-213`).
- Computed **once** at session start, so the prompt stays byte-stable for the
  prompt cache — the same reason memories don't hot-inject mid-session
  (`agent.rs:134-140`). Maps saved mid-session by *other* sessions surface
  through recall (§5) and coverage hints (§6), not by mutating the prompt.
- Freshness verdicts for the index come from one staleness pass over all
  records at startup (§3b), reported in the existing startup summary line
  alongside the graph stats: `✓ maps: 4 saved (3 fresh, 1 drifted)`.

`gather_context` packs get the same treatment in the same section (they are
sweep results, not maps, but they're equally reusable and share the oracle):
one line per pack — goal, freshness verdict from its manifest, age — so a
session knows a deterministic sweep already ran before it re-runs one. The
pack's `request_key` idempotency (`gather.rs:363-441`) already dedups
*identical* re-sweeps; the index closes the remaining gap where a session
never thinks to call `gather_context` with those inputs at all.

### 4b. Registry: what is each live session exploring?

Extend `SessionRecord` (`stella-store/src/sessions.rs:79-93`) with:

```rust
/// Slices this session is currently mapping (draft explorations it holds).
#[serde(default)]
pub exploring: Vec<String>,
```

Written on draft save / finalize (§4c) through the existing per-turn registry
refresh (`stella-cli/src/command_deck.rs:660-665`). The deck's SESSIONS
overlay (`command_deck.rs:1336-1351`) renders it, so a human running three
decks sees "session 2 is mapping `cli-agent-loop`" before typing a prompt
that would re-map it. Liveness stays derived from `kill(pid, 0)` at read time
(`sessions.rs:183-185`) — no new lifecycle machinery.

### 4c. Draft records ARE the in-flight claim

The obvious design — extend `file_locks` for exploration claims — inherits its
gaps (no expiry, leaked rows, and a lock that says nothing about *progress*).
Instead, the claim is the artifact itself:

- When a session begins substantial exploration of a slice (triage/plan stage
  decides, or the model does), it calls `save_exploration` with
  `status: "draft"` — title + one-line summary + whatever partial notes exist.
  Cost: one tool call it was going to make anyway, just earlier.
- Every other session's index (§4a) and `explorations` listing shows
  `IN PROGRESS by ses-…-41782 (live)` — liveness computed by parsing the pid
  from the recorded `session_id` and probing `kill(pid, 0)`, exactly the
  registry's trick. Dead pid → rendered as `(abandoned draft — partial notes
  usable, safe to take over)`.
- Finishing the map = the normal overwrite save with `status: "complete"`.

Why this beats a lock table: **nothing is ever lost** (a crashed session's
partial notes survive as a readable draft instead of a leaked lock row);
there is no expiry policy to invent (liveness is derived, not declared);
takeover is a plain overwrite; and readers get partial value immediately.
Coordination is advisory — same trust model as `file_locks` and the whole
`.stella/` store — which is correct: two sessions racing to map the same
slice wastes tokens, it does not corrupt anything. Last-writer-wins on the
JSON file is already the store's semantics (`exploration.rs:16-18`).

### 4d. The save-side nudge (production, not just consumption)

Consumption fixes only half the waste — maps must reliably get *written*. At
turn finalize (`record_execution_end`, `stella-cli/src/agent.rs:496`), when
the session's file-touch ledger shows ≥ N distinct files read (default 12)
whose paths are not covered by any fresh exploration manifest, and no
`save_exploration` happened this session, emit a one-line reflection prompt
into the existing Reflect stage: "you explored unmapped territory
(stella-fleet/*, 14 files) — save an exploration so other sessions reuse it."
This is a nudge, not a gate; it reuses the reflection machinery
(`stella-cli/src/memory.rs:800-879`) and costs nothing when the session
stayed inside mapped territory.

### 4e. Observatory view

The Observatory already renders memories, reflections, and the code graph
from read-only DB opens (`stella-observatory/src/lib.rs:119-151`). Add
`/api/explorations`: the record inventory with per-map freshness verdicts,
drift lists, draft/complete status, producing session, and manifest sizes —
the human-facing twin of the §4a index. Reads the JSON directory directly
(read-only, loopback-only, like everything else there); no schema work.

---

## 5. Surface C — context-plane ingestion (wiring the seam)

The `ProviderRegistry` seam (`stella-context/src/provider.rs`) was never
finished because cross-provider fusion is real design work: the registry can
only concatenate + dedup, while all scoring (RRF, MMR, budget packing) lives
inside `ContextStore::recall` (`stella-context/src/retrieval.rs:112-305`).
**We sidestep that entirely: exploration metadata becomes nodes *in* the
store, so the existing single-provider pipeline scores them.** No
multi-provider fusion needed; the seam gets its second content type for free.

On every `save_exploration` (and once at session start, reconciling the
directory against the index):

- Upsert a node: `NodeKind::Artifact`, label `exploration:<slice>`, content =
  `title + "\n" + summary` (NOT the full map — see below), via the existing
  `NodeInput` write path (`stella-context/src/writeback.rs`). The
  `HashEmbedder` embeds it like any other node; when the real ONNX embedder
  lands, explorations upgrade with everything else.
- Upsert edges: exploration-node → File node for each manifest path
  (close-and-supersede on overwrite, the store's normal edge versioning).
  Domain tags flow from the files' domains (`.stella/domains.toml`), so
  domain-scoped recall picks maps up automatically.
- Delete nothing on drift; freshness is computed at render time.

Recall then serves **pointer frames**: when a task's query lands near an
exploration node, the frame injected into the per-turn recall block is
`exploration `stella-media-wiring` covers this area (fresh) — read in full:
explorations({"slice": "stella-media-wiring"})` — ~30 tokens against the
existing 5-frame/1,200-token budget (`memory.rs:661-670`), with the full map
one cheap tool call away. Big content stays pull; discovery becomes push.
This is why the 21 KB overview map and the tight recall budget stop being in
conflict.

Packs are ingested the same way (node content = `goal` + section titles,
edges from the manifest), so a semantically-close task recalls "a sweep for X
already exists" exactly like it recalls a map.

**Closing the loop — was the map actually useful?** Memories already have a
feedback economy: recalled frames carry an id and a `cite_memory` instruction,
and citations aggregate into promotion eligibility
(`stella-cli/src/memory.rs:703-722`; `memory_citations` in `store.db`). Reuse
it verbatim: a recalled exploration pointer frame carries its node id; when
the agent then reads that map, the `explorations` tool records an implicit
citation (usefulness = it was read and the session proceeded without
re-exploring; a session that reads a map and re-explores the same files anyway
is an implicit negative). This feeds §9's reuse metrics from the same tables
the memory economy already uses — no new feedback plumbing, and drifting maps
that stop earning citations become the natural candidates the §4d nudge asks
sessions to refresh.

The JSON files remain the source of truth; `context.db` rows are a derived,
rebuildable index (a startup reconcile handles files added/edited/deleted out
of band — including by humans in an editor, which the zero-coordination
design explicitly invites).

---

## 6. Surface D — code-graph integration

### 6a. Coverage answers "is there a map for this?" at the moment it matters

The graph is where agents (are supposed to) start: `neighbors` is the
documented orientation op with no grep equivalent
(`docs/design/graph-tool-analysis.md`). Surface coverage there:

- `graph_query {op: neighbors, target: <file>}` appends, when the file
  appears in any exploration manifest:
  `maps: covered by `stella-media-wiring` (fresh) — explorations({"slice": …})`.
- Implementation: an in-memory **coverage index** (`path → [(slice,
  freshness)]`) built in `ToolRegistry` at construction and refreshed on
  `save_exploration` — the read-side analogue of the schema gate's
  `SchemaIndex`, which already lives there
  (`stella-tools/src/registry.rs:47-77`, field `schema_index`). Freshness in
  the coverage index is the session-start verdict; it degrades to "computed
  Ns ago" rather than re-hashing per query.

### 6b. Coverage hints on the classic tools (the guardrail for habit)

The telemetry spec's core diagnosis: models satisfy orientation with
`grep`/`read_file` out of habit (§0.1). Meet them where they are. In
`ToolRegistry::execute` (`registry.rs:261-422`), when a `grep`/`glob`/
`read_file` result contains ≥ 1 path covered by a **fresh** exploration whose
hint hasn't fired yet this session, append one line:

```
note: `workspace-overview` maps this area (fresh) — explorations({"slice": "workspace-overview"}) may save you this search
```

Once per (session, slice) — a `HashSet` next to the ledgers, so it can never
nag. Tool-result text is outside the cached prompt prefix, so this has zero
prompt-cache cost. This single hook is what would have converted all three
sessions in §0 on their very first `ls`-equivalent.

### 6c. Graph enhancements (the "enhance if need be" part)

Two upgrades earn their keep here; neither blocks Phase 1:

1. **Rust import resolution** — the known `importers` gap
   (`graph-tool-analysis.md` §6: import edges for Rust are stored unresolved,
   so "what depends on this file" returns empty on the dominant language).
   Fixing it (longest-prefix module-path resolution, as the observatory
   already does for its snapshot, `stella-observatory/src/codegraph.rs:189-214`)
   makes *drift triage* mechanical: when a map is `Drifted` on file F, the
   graph can answer "which other manifest files import F" and the re-verify
   list shrinks from "2 changed files" to "2 changed files + 1 dependent."
2. **Slice-boundary suggestion** — `save_exploration`'s response can propose
   splitting oversized manifests (§3d cap) along import-graph communities
   instead of alphabetically. Nice-to-have; do last.

Explicit non-goal: symbols/manifests do **not** get new tables in
`codegraph.db`, and the reserved `graph_nodes`/`graph_edges` tables in
`store.db` (`stella-store/src/lib.rs:36-38, 521-530`) stay unused. The
coverage index is derived in memory from the JSON records; adding a third
persistence home for exploration data would recreate cause #5.

---

## 7. Fleet and worktree sessions

Fleet workers run in per-task git worktrees (`stella-fleet/src/fleet.rs`,
`Isolation::Isolated`), whose cwd-derived workspace root would resolve
`.stella/explorations/` to the worktree — an empty store. The fleet
orchestrator already owns the primary root and the single `Store`; it must
pass the primary exploration directory to workers (read-only view is
sufficient: workers *consume* maps; the orchestrator saves any maps produced
by a wave, stamped with the fleet run's holder identity). One plumbing
parameter through `FleetWorker`, no new coordination.

Sessions in sibling checkouts of the same repo share nothing today (workspace
identity is the canonicalized cwd path, `stella-store/src/usage.rs:55-64`) and
this spec does not change that: manifests are content-hashes of *this*
checkout's bytes, so cross-checkout sharing would need a remote-keyed store —
out of scope, noted for the future.

---

## 8. What this spec deliberately does NOT do

| Non-goal | Why |
|---|---|
| Background daemons / watchers for staleness | Verdicts are computed at session start and on read — the only moments they're consumed. The event-sourced lifecycle already provides every hook needed. |
| Delete or expire stale maps | Drifted maps with exact drift lists retain most of their value. Nothing is ever lost that code could have preserved. |
| Enforced locking of slices | Duplicate exploration wastes tokens; it corrupts nothing. Advisory drafts + liveness beats inventing an expiry policy for a real lock. |
| Push full map bodies into the prompt or recall block | 21 KB maps vs a 1,200-token recall budget and a byte-stable cached prefix. Bodies stay pull (one tool call); discovery becomes push. |
| Mutate the system prompt mid-session | Prompt-cache stability is load-bearing (`agent.rs:134-140`). Mid-session awareness arrives via recall frames and tool-result hints instead. |
| A third storage home for exploration data | JSON files stay the single source of truth; `context.db` nodes and the coverage index are derived and rebuildable. |

---

## 9. Measuring success

All signals already flow through `store.db`; no new telemetry plumbing:

- **Reuse rate:** `explorations` tool calls before the first `grep`/`read_file`
  of a session (from `tool_calls`, ordered by seq). Target: >70% of sessions
  in a mapped workspace open with the index or a map read.
- **Duplication rate:** sessions whose file-touch ledger shows ≥ 12 reads
  inside a fresh map's manifest *without* reading that map. Target: near zero.
- **Orientation cost:** input+output tokens before the first `write_file`/
  `edit_file` (from `telemetry` × `events`), compared across mapped vs
  unmapped workspaces. This is the number the three sessions in §0 would have
  moved.
- **Production rate:** sessions that triggered the §4d nudge and saved vs
  ignored.
- **Causal check:** the existing recall A/B suppression
  (`STELLA_AB_RECALL_RATE`, `stella-cli/src/agent.rs:123-126`) already
  suppresses the recall block on ~1/10 turns; since exploration pointer frames
  ride that block, the same mechanism measures their marginal value with zero
  new experiment code.

---

## 10. Rollout

**Phase 1 — the oracle and the index (highest value, no new surfaces):**
manifest v2 + shared staleness module + auto-manifest from the file-touch
ledger + startup index injection + affirmative freshness rendering.
*Acceptance: a v1 record still round-trips; a map saved by session A appears
with a correct Fresh/Drifted verdict in session B's turn-1 prompt; editing one
covered file flips only that map to Drifted and names the file.*

**Phase 2 — awareness and the seam:** draft-as-claim + registry `exploring` +
deck overlay + context.db ingestion with pointer frames + turn-finalize save
nudge.
*Acceptance: two concurrent sessions — B's index shows A's draft as live
in-progress; kill A, B's next session shows an abandoned, readable draft. A
task prompt semantically near a saved map recalls its pointer frame within
budget.*

**Phase 3 — graph integration:** coverage index + `neighbors` coverage lines +
classic-tool hints + Rust import resolution + slice-boundary suggestions.
*Acceptance: `grep` over a mapped area yields exactly one hint per session;
`importers` returns non-empty for a Rust file with in-repo dependents.*

Each phase is additive, independently shippable, and reversible by not
reading the new fields — v1 readers of v2 records see the fields they know.

---

## 11. Consolidation inventory — nothing left behind

Stella has accumulated substantial context gathering/shaping/caching
machinery across five crates. This spec is a *consolidation*: every existing
asset either gains a defined role here, is already wired and untouched, or is
explicitly bounded with a reason. Nothing is deprecated, forked, or
re-implemented.

| Existing asset (where) | Role in this design | Status |
|---|---|---|
| `explorations` / `save_exploration` + `.stella/explorations/*.json` (`stella-tools/src/exploration.rs`) | The core artifact. v2 manifest, draft status, provenance (§3); injected, claimed, recalled, hinted (§4–6) | **Extended** |
| `gather_context` packs + sha256 manifests + `request_key` dedup (`stella-tools/src/gather.rs`) | Manifest/staleness code extracted as the shared oracle (§3b); packs join the startup index (§4a), context.db ingestion (§5), and coverage hints | **Unified** — oracle promoted from one consumer to two |
| Code graph: `codegraph.db`, `graph_query`, live watcher, per-file `content_sha256` (`stella-graph`) | Orientation surface carries coverage lines (§6a); watcher hashes are the later verdict optimization (§3b); import-resolution fix powers drift triage (§6c) | **Extended** |
| Context plane: nodes/edges/embeddings/episodes/recall RRF+MMR (`stella-context`) | Explorations and packs become embedded, domain-tagged nodes with File edges; pointer frames ride `recall_scoped` unchanged (§5) | **Wired** — gains two content types with no pipeline changes |
| `ProviderRegistry` seam, "designed but not yet built" (`stella-context/src/provider.rs`) | Fulfilled by ingestion rather than a new provider: the store remains the one scored provider and now carries the content the seam was reserved for (§5) | **Wired** — the deferred fusion problem is dissolved, not solved |
| File-touch ledger (`stella-tools/src/file_touch.rs`, `files_touched` in store.db) | Evidence source: auto-manifest at save (§3d), unmapped-exploration detection for the nudge (§4d), duplication metric (§9) | **Wired** — first consumer beyond telemetry |
| Session registry + SESSIONS overlay (`stella-store/src/sessions.rs`, deck) | Liveness oracle for drafts; new `exploring` field; overlay shows in-flight maps (§4b–c) | **Extended** |
| `file_locks` claims + fleet holder pattern (`stella-store`, `stella-fleet`) | Pattern donor: identity-embeds-liveness reused by draft records; table itself untouched, fleet claims untouched (§1, §4c) | **Untouched** — pattern reused, no new rows |
| Fleet worktree isolation (`stella-fleet/src/fleet.rs`) | Workers get read access to the primary map store; orchestrator saves wave-produced maps (§7) | **Extended** — one plumbed parameter |
| Memory citations economy (`cite_memory`, `memory_citations`) | Reused verbatim for map-usefulness feedback; feeds refresh prioritization (§5) | **Wired** |
| Reflection machinery + `reflections.jsonl` (`stella-cli/src/memory.rs`) | Carries the save-side nudge (§4d) | **Wired** |
| Recall A/B suppression (`STELLA_AB_RECALL_RATE`) | Measures pointer-frame value for free (§9) | **Wired** |
| Workspace memories `.stella/memories/*.md` → system prompt | Distinct by design: memories are durable *lessons*, explorations are *maps of code as of a state*. Boundary: content describing "how area X works" belongs in an exploration (stalenessable), not a memory (not) | **Untouched** — boundary now stated |
| Episodes (`stella-context/src/writeback.rs`) | Already recalled by the store; unchanged. Explorations are the durable, refreshable complement to episodic one-turn summaries | **Untouched** |
| Compaction & dedup (`stella-core/src/compaction.rs`) | Already dedups byte-identical repeated tool outputs, so re-reading a map inside one session is near-free; unchanged | **Untouched** |
| Speculative read-only execution (`stella-core/src/driver.rs:517-543`) | `explorations` is `read_only: true`, so map reads already parallelize with streaming; unchanged | **Untouched** |
| Schema gate + `SchemaIndex` (`stella-tools/src/schema_gate.rs`) | Precedent: the coverage index (§6a) is its read-side analogue in the same struct; gate itself unchanged | **Untouched** — pattern reused |
| Observatory (`stella-observatory`) | Gains `/api/explorations` (§4e) | **Extended** |
| Skills frontloading, `usage.db` cross-project rollup, hooks | Out of this spec's scope: skills are behavioral, not cartographic; cross-checkout map sharing needs a remote-keyed store (§7); SessionStart hooks remain available for user-side injection | **Bounded** — reasons stated |

The audit rule this table enforces going forward: any future context-caching
feature must either extend one of these rows or add a new row with a stated
relationship to the shared staleness oracle (§3b) and the single-source-of-
truth JSON store — that is how the two-surfaces problem (`telemetry-data-plane-spec.md`
§0.1) stays fixed instead of recurring.
