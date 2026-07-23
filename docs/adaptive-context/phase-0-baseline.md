# Phase 0 — Baseline & Characterization

Status: baseline of record for the adaptive-context work
Date: 2026-07-23
Branch: `feat/phase-0-adaptive-context-baseline` (off `main` @ `1a8d872`)

This document is the **frozen baseline** the adaptive-context lifecycle is built
on: the current toolchain, the reusable persistence substrate, the exact
read/write paths a migration must not break, and — most importantly — the
**characterized semantics of today's `as_of`** (which the plan forbids
*assuming*). Every claim is grounded in a `file:line` reference against the tree
at `1a8d872`.

Its companion decision records live in [`../adr/`](../adr/README.md).

---

## 1. Toolchain & baseline checks

| | |
|---|---|
| Rust toolchain (`rust-toolchain.toml`) | `1.97.0` (+ `clippy`, `rustfmt`) |
| Edition | 2024 |
| Workspace version (`Cargo.toml`) | `0.4.77` |
| `cargo fmt --all --check` | clean at baseline |

**Nominal per-phase gate** (the plan's non-negotiable witnesses):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

For this purely-additive phase the effectively-load-bearing subset is the two
crates it touches — `stella-context` and `stella-cli`:

```sh
cargo test -p stella-context
cargo test -p stella-cli
```

> Honesty note: a full `cargo test --workspace` on this repo (15 crates, a 133 GB
> warm `target/`) is expensive. The phase-0 change adds only tests, an inert
> settings schema, fixtures, and docs; the scoped `stella-context` /
> `stella-cli` runs are what was actually executed for this branch. Do not
> record the full-workspace gate as passed unless it was run.

## 2. Cargo dependency graph

15 workspace members:

```
stella-cli  stella-context  stella-core  stella-fleet  stella-graph
stella-mcp  stella-media    stella-model stella-observatory stella-pipeline
stella-protocol stella-serve stella-store stella-tools stella-tui
```

The Context Graph Exchange Protocol types are consumed as a **pinned git
dependency**, not a path dep on the sibling checkout:

```toml
# stella-context/Cargo.toml, stella-cli/Cargo.toml
contextgraph-types  = { git = ".../context-graph-protocol", rev = "9fb559aa4d3ec4cf062e59dab113eae4e175c5fa" }
contextgraph-host   = { git = ".../context-graph-protocol", rev = "9fb559aa4d3ec4cf062e59dab113eae4e175c5fa" }
contextgraph-conformance = { git = ".../context-graph-protocol", rev = "9fb559aa4d3ec4cf062e59dab113eae4e175c5fa" } # dev-dep
```

The pinned rev (`9fb559a`) equals the sibling repo's current `main` HEAD.
`ContextQuery`, `ContextFrame`, and `Representation` come from that checkout, not
from `stella-*` — the types below are quoted from the rev that actually
compiles. **Phase 10 (CGEP export) is gated on this rev pin being allowed to
move**; treat it as fixed until then.

## 3. Two SQLite authorities (do not cross-wire)

Two independent databases, each with its own `SCHEMA_VERSION` constant of the
same name in a different crate:

| DB | Owner crate | `SCHEMA_VERSION` | Holds |
|---|---|---|---|
| `.stella/private/context.db` | `stella-context` | **3** (`store.rs:28`) | graph plane: `node`, `edge`, `episode`, `memory`, `embedding`, `domain*` |
| `.stella/private/store.db` | `stella-store` | **12** (`migrations.rs:76` = `MIGRATIONS.len()`) | telemetry/rules plane: `rules`, `memory_citations`, executions, … |

### 3a. `context.db` schema & migrations (`stella-context/src/store.rs`)

`migrate()` (`store.rs:526`) reads `PRAGMA user_version`, early-returns if `>= 3`,
applies each pending step in **one transaction**, then stamps `user_version = 3`
(`store.rs:541`). This is the harness Phase 2 extends.

- **v0→v1** `MIGRATION_V1` (`store.rs:38-104`): `node`, `edge`, `embedding`,
  `episode`, `embedder_fingerprint` + indexes. Both `node` and `edge` carry the
  four bitemporal columns `valid_from`, `valid_to`, `recorded_at`,
  `superseded_at`.
- **v1→v2** `MIGRATION_V2` (`store.rs:113-144`): `domain`, `node_domains`,
  `edge_domains`, and the `memory` record table (+ `idx_memory_kind`).
- **v2→v3** `MIGRATION_V3` (`store.rs:155-159`): `DROP TABLE IF EXISTS
  code_graph_{symbols,imports,files}` — evicts orphaned tree-sitter tables (the
  code graph now lives in its own `codegraph.db`).

### 3b. `context.db` read/write paths (the migration must preserve these)

**No `DELETE` exists on any of `node`/`edge`/`episode`/`memory`.** Nodes
overwrite in place; edges "close" via supersede-UPDATE; the only drop is the V3
`code_graph_*` eviction.

- `node` — `upsert_node` (`store.rs:667`, `INSERT … ON CONFLICT(public_id) DO
  UPDATE`, content-on-touch). Readers: `node_count` (`:450`), `memory_nodes`
  (`:471`, reads `node WHERE kind='memory'`, **not** the `memory` table),
  `node_by_public_id` (`:488`), `live_nodes` (`:720`, `WHERE superseded_at IS
  NULL`), `node_ids_for_uris` (`:734`), `node_by_id` (`:774`).
- `edge` — `insert_edge` (`:889`), `close_edge` (`:940`, `UPDATE … SET
  superseded_at=?, valid_to=COALESCE(valid_to,?)` — the supersession mechanism,
  never a delete), `currently_valid_edge` (`:920`), `neighbors` (`:789`),
  `edges_as_of` (`:956`).
- `episode` / `memory` — **write-only tables.** `insert_episode` (`:996`),
  `insert_memory` (`:1024`) both `INSERT … ON CONFLICT DO UPDATE`. There is **no
  `SELECT` and no `DELETE`** on either table anywhere; read-back is exclusively
  through the mirror `node` row (`episode://<id>`, `memory://<id>`) written in
  the same transaction.
- The one persist entry point is `ContextStore::upsert(delta)` (async,
  `writeback.rs:343`): one transaction — domains → nodes → episodes (+mirror) →
  memories (+mirror) → facts via `apply_fact` (`:503`) → embeddings → commit.

> **Migration consequence:** every memory/episode is stored twice (canonical row
> + mirror node), and the mirror node is the only thing ever read. `node`
> content history is unrecoverable (`upsert_node` overwrites in place). Phase 2
> must migrate memories losslessly and decide whether the write-only
> `memory`/`episode` tables survive as projections.

### 3c. `store.db` rules & citations (`stella-store`)

- `rules` table — DDL `RULES_TABLE` (`ddl.rs:172-178`): `rule_id` PK, `contents`
  (the full `.stella/rules/*.md` markdown, opaque), `source`, timestamps. Added
  in store.db migration v2→v3 (`migrations.rs:306-310`). Writes:
  `Store::upsert_rule` (`lib.rs:1518`), `Store::delete_rule` (`lib.rs:1529`, a
  **real** `DELETE` — unlike context.db). Reads: `Store::list_rules`
  (`lib.rs:1539`), consumed by `stella-cli/src/rules.rs:117` as
  `store://rules/<id>.md`.
- `memory_citations` table — DDL (`ddl.rs:196-206`): `UNIQUE(execution_id,
  memory_id)`. Write `Store::record_memory_citations` (`lib.rs:740`) is a **plain
  INSERT** (not an upsert) — a second drain under the same `execution_id` errors
  on the UNIQUE constraint; dedup relies on the in-memory `CitationLedger`
  (`stella-tools/src/memory.rs:124`, `retain` keep-latest at `:210`) and
  per-execution drain (`stella-cli/src/agent/persistence.rs:145`). Read:
  `memory_citation_stats` (`lib.rs:801`) → per-memory promotion eligibility.

### 3d. Rules live in two surfaces

Hand-authored / promoted Markdown `.stella/rules/*.md` (+ `.claude/rules`, user
`~/.config/stella/rules`) **and** extension-authored rows in `store.db.rules`.
They merge at load time in `stella-cli/src/rules.rs` (`SessionRuleSource`, store
rules lowest precedence). `stella-core/src/rules/` is I/O-free and operates on an
injected `RuleSource`; `render_rule_markdown` (`rules.rs:786`) and
`rule_from_file` (`:300`) are mirror encode/decode. `RuleRecordKind` currently
has a **single** variant, `Directive` (`metadata.rs:8-12`) — Phase 1 subsumes it.

The `stella memory promote` command writes `.stella/rules/<id>.md`
(`memory_cmd.rs:185`) and refuses to clobber an existing file. (Distinct from the
lesson→`SKILL.md` auto-promotion at `memory.rs:571` — do not conflate.)

## 4. Characterized: what `as_of` means today

**Finding (pinned by tests, not assumed): `as_of` filters on
TRANSACTION / BELIEF time only** — the half-open interval
`[recorded_at, superseded_at)`. It **never** consults world-validity
(`valid_from` / `valid_to`).

The two temporal readers (`neighbors` `Some` arm `store.rs:802-808`;
`edges_as_of` `Some` arm `store.rs:981-985`) share one filter:

```sql
WHERE recorded_at <= ?t AND (superseded_at IS NULL OR superseded_at > ?t)
```

- start is **inclusive** (`t == recorded_at` → visible);
- end is **exclusive** (`t == superseded_at` → not visible);
- `as_of = None` collapses to `superseded_at IS NULL` ("currently believed").

An exhaustive grep confirms **no `WHERE` clause in `stella-context/src` ever
references `valid_from`/`valid_to`** — those columns are written (`insert_edge`,
`close_edge`) but **dead on read**. The store's practical shape is therefore
**uni-temporal on transaction time, edges only** (node time columns are inert
because `upsert_node` overwrites in place).

This contract is pinned by three tests added in this phase
(`store.rs`, `mod tests`):

- `as_of_none_returns_only_currently_believed_edges`
- `as_of_reconstructs_half_open_transaction_interval` (the boundary test:
  inclusive start, exclusive end)
- `as_of_ignores_world_validity_valid_from_valid_to` (**the discriminator** —
  an edge whose `valid_to` closed in the past but which is still believed
  remains visible; this is what distinguishes "transaction time" from "world
  validity / both")

**Load-bearing nuance for Phase 3:** inside `recall`, `as_of` time-travels
**only the 1-hop graph-adjacency signal** (`retrieval.rs:256` → `neighbors`).
Vector similarity, recency, domain overlap, and the lexical fallback all draw
from *current* `live_nodes` (`WHERE superseded_at IS NULL`). So "recall as-of T"
is **not** a true historical snapshot today. See ADR
[`0003-bitemporal-semantics`](../adr/0003-bitemporal-semantics.md): the new API
must separate `known_at` (transaction/belief) from `valid_at` (world validity),
with `as_of` mapping to `known_at`.

## 5. Characterized: event decoder forward-compatibility

`stella-protocol/src/event.rs` — the top-level `AgentEvent` (`event.rs:151`) is a
Serde **internally-tagged** enum (`#[serde(tag = "type", rename_all =
"snake_case")]`) with **no `#[serde(other)]` / catch-all variant**. Therefore:

- **Adding a field** to an existing variant is bidirectionally safe (unknown
  fields ignored on read; `#[serde(default)]` fills missing) — extensively
  tested.
- **Adding a variant** is safe for new-code-reads-old-data, but **old code
  hard-fails on new data**: the only in-tree JSONL reader, `parse_jsonl`
  (`stella-pipeline/src/replay.rs:349`), treats an unparseable interior line as a
  **fatal** `MalformedLine`.

**Phase 1 consequence:** before adding `AgentEvent` variants (the internal
`ObservationRecorded`, `RecordProposalCreated`, … events), add a versioned
unknown-event envelope / reader tolerance, or the change breaks replay of mixed
streams. The plan already flags "characterize the existing decoder" — this is
the answer: it rejects unknown tags.

## 6. Characterized: frame representations

`Representation` (`contextgraph-types … frame.rs:47`) is `{ Full (default),
Compact, Reference }`. stella emits **`Full` exclusively** — a grep of
`Representation::` across all `.rs` finds only `::Full` (production emitters
`retrieval.rs:370`, `stella-graph/src/frames.rs:302`). `Compact`/`Reference` and
the `content_fidelity` / `canonical_content_hash` / `ContentRef` machinery are
defined in the protocol types but **unused** here. Phase 4 wires them.

## 7. Artifacts added in this phase

- **Characterization tests** — `stella-context/src/store.rs` `mod tests` (the
  three `as_of_*` tests above).
- **Disabled settings schema** — `stella-cli/src/settings/context.rs`
  (`ContextSettings` and friends), wired into `Settings` as an inert
  `Option<ContextSettings>` (whole-block last-wins across scopes). `off` /
  `record_only` / `advisory` learning modes and `solo` / `team` / `regulated`
  governance modes are kept as distinct dimensions; enums are loud on typos.
  `context.lifecycle.enabled` defaults `false` — the value that preserves
  behavior. **Nothing reads these settings yet.**
- **Canonical settings fixture** — `stella-cli/tests/fixtures/context_settings.json`
  (the plan's suggested block). `.stella/` is gitignored, so the canonical
  example is a committed fixture, round-trip-tested against the code defaults so
  the two cannot drift.
- **ADRs** — [`docs/adr/`](../adr/README.md) (8 records; `0002` SharingScope
  arity and `0007` enforcement 4→2 carry open questions requiring human
  confirmation).
- **Schema-version fixtures** — `stella-context/src/store.rs` `mod tests`:
  `migrates_v1_context_db_preserving_bitemporal_edges` and
  `migrates_v2_context_db_preserving_memories` build a legacy v1/v2 `context.db`
  carrying representative rows (a superseded edge, an edge whose world-validity
  closed in the past, a memory + mirror node), open it through
  `ContextStore::open`, and assert the migration to `SCHEMA_VERSION` is a
  lossless, idempotent replay.
- **Rule-markdown fixtures** — `stella-cli/tests/fixtures/rules/`
  (`no-hand-edited-migrations.md` = a promoted guard rule;
  `api-integration-coverage.md` = a full context-as-code directive with
  inferred/advisory metadata). Parsed under today's `stella_core::rules` parser
  by `rules::tests::{guard,directive}_rule_fixture_*` so the format Phase 2's
  importer reads is pinned.

> **Scope note (no silent cap):** schema-version DB fixtures cover `context.db`
> (the Phase 2 migration authority) at v1→v3 and v2→v3. `store.db`'s 12-version
> migration harness (`stella-store`, gated at `lib.rs:615` with a version-gap
> error) is owned and tested inside `stella-store`; per-version `store.db`
> fixtures are deferred to the phase that actually touches `store.db` rows.

## 8. Open questions carried into later phases

- ~~**SharingScope arity**~~ (→ ADR 0002): **Resolved 2026-07-23** — owner
  ratified the 4-value set (`user, repository, workspace, organization`); §21's
  3-value line superseded. Phase 1 enum freezes on four.
- ~~**`Origin` arity**~~: **Resolved 2026-07-23 (spec-verified)** — the full
  5-value set (`user, system, observed, inferred, imported`) is authoritative for
  all families; the normative origin→derivation_kind table (§ line 952) is
  family-uniform and admits `observed`. The §8.6 directive example (four values)
  is illustrative, not a per-kind narrowing.
- ~~**Enforcement 4→2 mapping**~~ (→ ADR 0007): **Resolved 2026-07-23** — owner
  ratified the 4→2 mapping; `DirectiveEnforcement` is 2-value (`advisory`,
  `blocking`); the four levels survive only as UI labels.
- **World-time columns**: `valid_from`/`valid_to` are written but never read.
  Decide in Phase 3 whether to make the store truly bi-temporal (valid-time
  queries) or document them as advisory metadata.
- **Point-in-time recall is edges-only**: making "recall as-of T" a true
  snapshot requires versioned node content, which the current schema cannot
  provide.
- **Contextgraph rev pin**: whether the `9fb559a` pin is meant to move is a
  Phase 10 question; keep fixed until then.
