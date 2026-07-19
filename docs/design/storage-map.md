# Design: The Storage Map — Vendor-Agnostic Storage Layer Indexing for Zero Drift

**Status:** Draft v1 · **Date:** 2026-07-19
**Goal:** On very long-horizon tasks in complex projects, the agent (a) always
knows every storage layer the project uses, what belongs in each, and the full
schema down to individual fields — retrievable by name *and* by meaning — and
(b) finds it structurally hard to create a duplicate or misplaced schema,
table, or column, in **any** storage technology, not just SQL.
**Prior art:** `docs/design/schema-graph.md`. What actually shipped from it:
the live SQL DDL index in `stella-graph` (tree-sitter-sequel; `SymbolKind::
{Table, Column, SchemaEnum, View}`), the Diesel `table!` detector, and the
pre-write gate in `stella-tools/src/schema_gate.rs` (regex scan, `.sql` files
only, exact-name conflicts on tables/types/views). Its Phase 3 canonical model
(`schema.rs`, `code_graph_schema_links`) and Phase 4 semantic edges are
**unbuilt** — this spec absorbs and supersedes them.

---

## 1. Problem: drift and bloat outlive awareness

The shipped schema gate stops the agent from writing `CREATE TABLE payments`
twice. It does not stop any of the real failure modes on a 300-turn task:

1. **Turn 40:** Agent adds a Prisma model `PaymentRecord`. The gate never
   fires — it only reads `.sql` files. The code graph indexes it as a plain
   `Class`, not a table.
2. **Turn 90:** Compaction evicts the early turns. Nothing in context mentions
   payments.
3. **Turn 160:** Agent needs refund tracking. Recall surfaces payment *code*
   (handlers, tests) but table names like `payments` rank poorly against the
   query "where do we persist refund state" — the index knows the name, not
   the meaning. The agent creates a `refunds` **collection in MongoDB**
   because the last file it read was a Mongoose model — the wrong layer
   entirely, and invisible to a SQL-only gate.
4. **Turn 240:** A third agent in the fleet adds `refund_amount NUMERIC` to
   `payments` — a column that duplicates `refunds.amount` — because
   column-level conflicts are not checked at all.
5. **Result:** three storage technologies disagree about refunds. Nothing
   "broke," so nothing was caught. This is bloat: every future turn now pays
   context, retrieval, and reasoning tax on schema that shouldn't exist.

Four gaps, each mapped to a section of this spec:

| Gap | Why the shipped system misses it | Fix |
|---|---|---|
| **Vendor blindness** | Index + gate understand SQL DDL (plus Diesel). Prisma, Django, SQLAlchemy, Mongoose, DynamoDB definitions are invisible as *schema*. | Canonical storage model + source adapters (§3, §4) |
| **No column granularity** | Gate checks table/type/view names only. Duplicate or conflicting columns land silently. | Field-level index + gate rings (§3, §8) |
| **Name ≠ meaning** | `payment_records` vs `payments` is not an exact-name conflict. Recall ranks schema by trigram similarity to names, not by purpose. | Intent sentences + embedded schema cards (§5, §7) |
| **No notion of boundary** | Nothing records *what belongs where* — which layer holds durable truth vs cache, which table owns which rows. Misplaced data is not detectable even in principle. | Boundary contracts + redirects (§5, §8) |

**Design constraints** (house rules that shape everything below):

- **No daemons.** Everything is a synchronous fold: index on mount + watcher
  events + post-write triggers, meaning-generation at the `ContextWrite`
  finalize stage. Same pattern as `stella-graph/src/watch.rs`.
- **Nothing durable lives only in a rebuildable store.** `codegraph.db` is
  disposable by design. Intent and boundary sentences are expensive to produce
  and must never be lost to a cache rebuild — they live in a committed
  manifest (§5) and are only *mirrored* into the index.
- **Deterministic core.** Blocking decisions are made by parsers and name
  lookups, never by an LLM. Embedding similarity may *warn and demand
  justification*; it may not silently block (§8).

---

## 2. Non-goals

- **Live database introspection as the source of truth.** The repo is the
  ground truth (migrations, schema-as-code, manifest) because the repo is what
  agents mutate and what travels. A one-shot `stella storage import` that
  bootstraps the manifest from a running database's `information_schema` (or
  equivalent) is a Phase 5 convenience, not the model.
- **Schema migration generation or execution.** The map describes; it does not
  apply DDL.
- **Auto-deletion of anything.** The drift report (§9) proposes; humans
  dispose. No destructive automation, ever.
- **Query-shape modeling** (which SELECTs are slow, index advice). Out of
  scope; the `code_graph_storage_links` edges (§6) leave room for it later.

---

## 3. The canonical storage model

Four levels, vendor-neutral. Every storage technology maps into this shape; a
level that a technology doesn't have collapses to a single implicit entry.

```
StorageLayer            one configured storage technology instance
  └── Namespace         a grouping of relations inside the layer
        └── Relation    a named set of records
              └── Field a named, typed slot within records
```

Vendor vocabulary mapping:

| Level | Postgres/MySQL | SQLite | MongoDB | DynamoDB | Redis | S3/Blob |
|---|---|---|---|---|---|---|
| StorageLayer | the database server | the `.db` file family | the deployment | the account/region | the instance | the account |
| Namespace | schema (`public`) | one database file | database | *(implicit)* | logical db / key prefix | bucket |
| Relation | table, view, matview, enum type | table, view | collection | table + GSIs | key pattern (`sess:*`) | key prefix (`avatars/`) |
| Field | column | column | document path (dotted) | attribute | value shape / hash field | object metadata |

### 3a. Identity: stable addresses

Every entity has one canonical address, used everywhere — index rows, graph
nodes, embed cards, gate errors, CLI:

```
store://<layer-key>/<namespace>/<relation>[/<field>]

store://primary-pg/public/payments
store://primary-pg/public/payments/amount
store://session-redis/0/sess:*
store://docs-mongo/app/invoices/line_items.sku
```

`layer-key` is the manifest-declared key (§5). Path segments are the
**normalized name**: lowercased, `camelCase`/`kebab-case` folded to
`snake_case`. The display name is kept separately; normalization exists so
that `userId`, `user_id`, and `UserID` collide *by construction* (§8).

### 3b. What each level records

Every level carries three groups: **structural facts** (parsed, rebuildable),
**meaning** (intent + boundary, durable, §5), and **provenance** (which files
and lines defined it, which adapter parsed it).

```rust
// stella-graph/src/storage_model.rs (new)

pub struct StorageLayer {
    pub key: String,              // "primary-pg" — manifest key, stable
    pub engine: String,           // "postgres" | "sqlite" | "mongodb" | "redis" | ...
    pub class: StorageClass,      // Relational | Document | KeyValue | Blob |
                                  // Columnar | Graph | Vector | Queue
    pub durability: Durability,   // DurableTruth | DerivedCache | Ephemeral | Archive
    pub meaning: Meaning,
    pub provenance: Provenance,   // how it was discovered (§4d)
}

pub struct Namespace {
    pub name: String,
    pub meaning: Meaning,
}

pub struct Relation {
    pub name: String,             // display name as written in source
    pub kind: RelationKind,       // Table | View | MaterializedView | EnumType |
                                  // Collection | KeyPattern | Index | Stream | Prefix
    pub primary_key: Vec<String>,
    pub uniques: Vec<Vec<String>>,
    pub foreign_keys: Vec<ForeignKey>,   // → other relation addresses
    pub meaning: Meaning,                // intent = "one row per <grain>" sentence
    pub provenance: Provenance,
}

pub struct Field {
    pub name: String,
    pub data_type: Option<String>, // vendor-literal: "NUMERIC(10,2)", "ObjectId"
    pub nullable: bool,
    pub default_value: Option<String>,
    pub constraints: Vec<String>,  // "PRIMARY KEY", "REFERENCES users(id)",
                                   // "CHECK (amount >= 0)", "enum: ok|fail"
    pub meaning: Meaning,
    pub provenance: Provenance,
}

/// Meaning is the durable half. It is never produced by parsing alone.
pub struct Meaning {
    pub intent: Option<Sentence>,      // one sentence: what this IS for
    pub boundary: Option<Sentence>,    // one sentence: what belongs here / not
    pub redirects: Vec<Redirect>,      // "refund*" -> store://primary-pg/public/refunds
}

pub struct Sentence {
    pub text: String,
    pub origin: MeaningOrigin,         // Declared | Harvested | Inferred
    pub stale: bool,                   // structure changed since text written
}

pub struct Redirect {
    pub pattern: String,               // glob over proposed names: "refund*"
    pub target: String,                // canonical address that owns this concept
}
```

Intent at every level answers one question in one sentence:

| Level | The sentence answers | Example |
|---|---|---|
| Layer | why this technology exists in the project | "Durable transactional truth for all billing and account state." |
| Namespace | what domain this grouping owns | "Billing: charges, invoices, refunds — money movement only." |
| Relation | what one record *is* (the grain) | "One row per completed or attempted charge." |
| Field | what the value means, incl. units/currency | "Gross amount charged, in the currency named by `currency`." |

Boundary at every level answers the inverse — what does **not** belong — and
redirects say where it goes instead. Boundaries are what turn the gate from
"is this name taken?" into "does this data belong here?" (§8, Ring 2).

---

## 4. Sources: adapters and precedence

The map is assembled from four source kinds. Structure and meaning have
**separate precedence chains** — a Prisma file can win on structure while a
human manifest edit wins on meaning.

### 4a. Structural sources (highest wins per entity)

1. **SQL DDL** — migrations and `schema.sql`, parsed by the existing
   tree-sitter-sequel pipeline, extended to capture per-column type / nullable
   / default / constraints and table-level PK/unique/FK (today only names and
   spans are kept).
2. **Schema-as-code adapters** — one adapter per ecosystem, each a tree-sitter
   query pass over languages the graph already parses, plus dedicated grammars
   for schema DSLs:

   | Adapter | Detects | Notes |
   |---|---|---|
   | `prisma` | `model` / `enum` blocks, `datasource` (→ layer engine) | own grammar; `.prisma` files |
   | `drizzle` | `pgTable(...)`, `sqliteTable(...)` calls | TS, already parsed |
   | `diesel` | `table! { ... }` | shipped detector, upgraded to emit fields |
   | `django` | `class X(models.Model)` + field attrs | Python, already parsed |
   | `sqlalchemy` | `__tablename__`, `mapped_column`/`Column(...)` | Python |
   | `activerecord` | `create_table` in `db/schema.rb`, migrations | needs Ruby grammar (later phase) |
   | `typeorm` | `@Entity()`/`@Column()` decorators | TS |
   | `mongoose` | `new Schema({...})` + `model("X", ...)` | document fields incl. nested paths |
   | `mongo-jsonschema` | `$jsonSchema` validators in setup code | best-effort |
   | `dynamodb` | `CreateTable` params / CDK `Table` constructs | attributes + GSIs |

   Adapters emit the same canonical structs. Unknown ecosystems contribute
   nothing rather than garbage — false negatives are repaired by the manifest.
3. **Manifest structural stubs** (§5) — for storage no parser can see (a Redis
   key pattern, an S3 prefix, a service that owns its own DB), the manifest
   declares relations/fields by hand. Stubs lose to parsed structure when both
   exist for the same address.

When SQL DDL and an ORM model describe the same relation, DDL wins field by
field; the ORM definition is kept as a `code_ref` link (§6), not discarded.
Conflicts between the two (type mismatch, column present in one only) are not
silently merged — they surface in the drift report (§9).

### 4b. Meaning sources (highest wins per sentence)

1. **Declared** — written by a human (or by the agent through the gate's
   justification path, §8) into the manifest.
2. **Harvested** — extracted from the source itself: SQL `COMMENT ON`,
   Prisma `///` doc comments, Python docstrings, Mongoose `// comments` on
   schema fields, Drizzle/TypeORM JSDoc.
3. **Inferred** — generated once by the model (§5c) for entities that still
   have no sentence. Marked `origin = inferred` so humans know it's a guess.

### 4c. Layer discovery

Layers are declared in the manifest (authoritative). To make first-run
practical, `stella storage init` proposes a draft by scanning:

- dependency manifests (`Cargo.toml`, `package.json`, `pyproject.toml`) for
  driver crates/packages (`rusqlite`, `pg`, `mongoose`, `ioredis`, …),
- `datasource` blocks (Prisma), `DATABASE_URL`-shaped keys in `.env.example`,
- `docker-compose.yml` service images (`postgres:`, `redis:`, `mongo:`),
- migration directory conventions (`migrations/`, `db/migrate/`, `prisma/`).

Each hit becomes a proposed layer with engine + class prefilled and
`durability`/`boundary` left for the human to confirm. Discovery never runs
implicitly after init; layers changing is rare and manifest edits are cheap.

### 4d. Provenance

Every entity records `(source_file, line_span, adapter, content_sha256)`.
Gate errors and recall frames always cite provenance — the agent is told
*where* the existing definition lives, so "go read it" is one tool call.

---

## 5. The storage manifest: durable meaning, committed to git

`.stella/` is gitignored; `codegraph.db` is a rebuildable cache. Meaning must
survive both a cache rebuild and a fresh clone, and must be reviewable in a
PR. Therefore meaning lives in **`stella.storage.toml` at the repo root**,
committed, human-editable, agent-appendable.

### 5a. Format

```toml
# stella.storage.toml — the storage map's durable half.
# Structure (types, columns, constraints) is parsed from source and NOT
# duplicated here. This file holds what parsers cannot know: layers,
# boundaries, intent, redirects.

version = 1

[layers.primary-pg]
engine     = "postgres"
class      = "relational"
durability = "durable-truth"
boundary   = "All transactional application state. Nothing derivable, nothing ephemeral."

[layers.session-redis]
engine     = "redis"
class      = "key-value"
durability = "ephemeral"
boundary   = "Sessions and rate-limit counters only; every key has a TTL; loss is always survivable."

# Structural stub: no parser sees Redis key patterns, so declare them.
[[layers.session-redis.relations]]
name    = "sess:*"
kind    = "key-pattern"
intent  = "One key per active browser session; value is the serialized session doc."

[namespaces.primary-pg.billing]
intent   = "Money movement: charges, invoices, refunds."
boundary = "No user-profile or content data; billing rows reference users by id only."

[relations."primary-pg/billing/payments"]
intent   = "One row per charge attempt, successful or not."
boundary = "Refund state lives in `refunds`, not here."
redirects = [{ pattern = "refund*", target = "store://primary-pg/billing/refunds" }]

[fields."primary-pg/billing/payments/amount"]
intent = "Gross amount charged, in the currency named by `currency`."
# origin defaults to "declared" when a human writes it; the inference
# fold writes origin = "inferred" explicitly:

[fields."primary-pg/billing/payments/idempotency_key"]
intent = "Client-supplied key that makes charge creation retry-safe."
origin = "inferred"
```

### 5b. Durability rules (the less-is-more contract)

- The fold **appends and updates; it never deletes** a meaning entry. An
  entity that disappears from source keeps its manifest entry, flagged
  `orphaned = true` by the drift report — because on long-horizon tasks,
  "the agent deleted the migration file" must not erase the knowledge that
  the table existed and why.
- Structure changing under a sentence sets `stale = true` (a marker, not a
  deletion). The old text stays until replaced — a stale sentence beats none.
- Inferred sentences are **never regenerated over** — inference fills blanks
  and refreshes entries that are both `stale` and `origin = "inferred"`.
  Declared and harvested text is only ever changed by humans or by the gate's
  explicit justification path.

### 5c. The inference fold (no daemon)

At the `ContextWrite` finalize stage — the same synchronous hook where
`materialize_tool_calls()` already runs — the fold:

1. Diffs the structural index against the manifest: entities with no intent,
   or `stale + inferred` intents.
2. Batches up to N (default 16) of them into **one** model call per turn:
   each entity's structural card + provenance excerpt in, one sentence per
   entity out. Bounded, budget-visible (it's a normal `StepUsage` event).
3. Writes results to the manifest with `origin = "inferred"`, and emits a
   `storage_map_updated` event so the TUI/observatory folds can react.

Backlog drains across turns; a fresh project with 200 unintented columns
converges in ~13 turns without ever blocking one. `stella storage describe`
runs the same fold on demand for humans who want the map filled now.

---

## 6. Persistence: what lives where

Three stores, three roles — matching the existing split exactly:

| Store | Role | Contents |
|---|---|---|
| `stella.storage.toml` (git) | durable meaning | layers, boundaries, intents, redirects, stubs |
| `codegraph.db` (rebuildable) | structural index | parsed entities, provenance, code links; manifest mirrored in for one-query joins |
| `context.db` (durable local) | retrieval | one node per entity + embedding vectors (§7) |

New tables in `stella-graph/src/store.rs` (same `IF NOT EXISTS` migration
style; `kind` columns TEXT, so future kinds cost no schema change):

```sql
CREATE TABLE IF NOT EXISTS code_graph_storage_layers (
    id          INTEGER PRIMARY KEY,
    key         TEXT NOT NULL UNIQUE,      -- "primary-pg"
    engine      TEXT NOT NULL,
    class       TEXT NOT NULL,
    durability  TEXT NOT NULL,
    boundary    TEXT                        -- mirrored from manifest
);

CREATE TABLE IF NOT EXISTS code_graph_storage_objects (
    id            INTEGER PRIMARY KEY,
    layer_id      INTEGER NOT NULL REFERENCES code_graph_storage_layers(id) ON DELETE CASCADE,
    parent_id     INTEGER REFERENCES code_graph_storage_objects(id) ON DELETE CASCADE,
    address       TEXT NOT NULL UNIQUE,    -- store://... (normalized)
    level         TEXT NOT NULL,           -- 'namespace' | 'relation' | 'field'
    kind          TEXT NOT NULL,           -- RelationKind / 'column' / ...
    display_name  TEXT NOT NULL,
    data_type     TEXT,
    nullable      INTEGER,
    default_value TEXT,
    constraints   TEXT,                    -- JSON array
    intent        TEXT,                    -- mirrored from manifest
    boundary      TEXT,
    file_id       INTEGER REFERENCES code_graph_files(id) ON DELETE CASCADE,
    start_line    INTEGER,
    end_line      INTEGER,
    adapter       TEXT NOT NULL,           -- 'sql-ddl' | 'prisma' | 'manifest' | ...
    struct_sha256 TEXT NOT NULL            -- hash of structural facts (staleness)
);
CREATE INDEX IF NOT EXISTS idx_storage_objects_name
    ON code_graph_storage_objects(level, display_name);

CREATE TABLE IF NOT EXISTS code_graph_storage_links (
    id         INTEGER PRIMARY KEY,
    object_id  INTEGER NOT NULL REFERENCES code_graph_storage_objects(id) ON DELETE CASCADE,
    symbol_id  INTEGER REFERENCES code_graph_symbols(id) ON DELETE CASCADE,
    file_id    INTEGER REFERENCES code_graph_files(id) ON DELETE CASCADE,
    link_kind  TEXT NOT NULL               -- 'code_ref' | 'query' | 'mutation' | 'fk'
);
```

The existing `SymbolKind::{Table, Column, SchemaEnum, View}` rows keep being
written (nothing downstream breaks); the storage tables are the richer model
layered beside them. `graph_nodes`/`graph_edges` in `stella-store` remain
reserved and untouched — this feature does not adopt dead scaffolding.

Rebuild invariant: `rm .stella/codegraph.db` followed by a mount reproduces
the structural index byte-for-byte from source + manifest. Anything that
would fail that test belongs in the manifest instead.

---

## 7. Embedding and retrieval: schema you can find by meaning

### 7a. Embed cards

Every layer, namespace, relation, and field gets one **embed card** — a
deterministic textual rendering. Deterministic matters: the card's content
hash keys the vector in the existing `embedding (content_hash, fingerprint)`
table, so unchanged entities are never re-embedded and swapping embedders
re-embeds incrementally, exactly like code.

```
storage field store://primary-pg/billing/payments/amount
layer primary-pg: postgres, relational, durable transactional truth
table payments: one row per charge attempt, successful or not
column amount NUMERIC(10,2) NOT NULL
purpose: gross amount charged, in the currency named by `currency`.
boundary of table: refund state lives in `refunds`, not here.
```

Cards deliberately include the parent chain's intent sentences — a field card
must be findable from the query "where do we store how much a customer was
charged", which shares no tokens with `amount`. Under the default
`HashEmbedder` (character trigrams) intents already help; under the tracked
ONNX/API upgrade they carry the retrieval.

### 7b. Graph nodes

One `context.db` node per entity: new `NodeKind::Storage` (additive serde
variant), canonical name = the address, domain-tagged by the same proximity
heuristic that tags code files, with `contains` edges layer → namespace →
relation → field and `references` edges for FKs and `storage_links`.

### 7c. Recall integration

No new retrieval machinery. Storage nodes flow through `recall_scoped`'s
existing three-signal fusion (cosine + recency + 1-hop adjacency):

- The query "how do we track refunds" now surfaces `store://…/refunds`
  (cosine on the intent sentence) *plus* its FK-adjacent `payments` *plus*
  the mutation-linked handler code (1-hop) — schema and code in one frame.
- Storage frames render with provenance ("defined in
  `migrations/012_refunds.sql:3`") so the agent can jump to source.
- `stella graph` gains a `storage` op family (`stella storage show <address>`,
  `stella storage grep <name>`, `stella storage tree <layer>`) beside the
  existing `definitions`/`neighbors` ops, reading `codegraph.db` directly.

This closes the passive half of the loop: on turn 160, *before* the agent
decides anything, refund-related recall already contains the refunds table,
its boundary, and its layer — compaction cannot evict what is re-retrieved.

---

## 8. Gate v2: making duplication hard

The active half. The gate stays deterministic-core, but grows from one check
to three rings, and from `.sql` files to every adapter surface. Crucially,
**extraction is shared**: the gate calls the same adapter parsers as the
indexer (moved into `stella-graph`'s storage module; `stella-tools` takes the
dependency), so the gate and the index cannot drift apart — today's separate
regex scan in `schema_gate.rs` is retired.

The gate fires when `write_file`/`edit_file` **proposed content** yields
storage entities (any adapter — a new Prisma model, a Mongoose schema, a
`CREATE TABLE`), and compares them against the live index.

### Ring 1 — deterministic conflicts → block

| Conflict | Detection |
|---|---|
| Duplicate relation | normalized name equal, same namespace — `PaymentRecords`, `payment-records`, and `payment_records` all collide with `payment_records`; singular/plural folds too (`payment_record`) |
| Duplicate field | normalized name equal on the same relation |
| Redefinition mismatch | same address, different type/nullability/default than indexed |
| Orphaned FK | REFERENCES a relation address not in the index |
| Cross-layer duplicate relation | normalized name equal in a *different* layer (the MongoDB-`refunds`-when-Postgres-has-`refunds` case) — blocked with the existing address cited |

Error format follows the shipped gate: what exists, full provenance, the
existing definition inline, and the question ("ALTER the existing object? or
is this genuinely new — then rename and declare intent").

### Ring 2 — boundary redirects → block with a pointer

Proposed names are matched against `redirects` patterns on same-namespace
relations and on the target layer/namespace boundary. Writing a
`refund_status` column into `payments` when its boundary declares
`refund* → store://primary-pg/billing/refunds` fails with:

```
BOUNDARY: `payments` does not hold refund state.
  boundary: "Refund state lives in `refunds`, not here."
  redirect: refund* → store://primary-pg/billing/refunds (12 columns, defined
            in migrations/012_refunds.sql:3)
Add the field there, or update the boundary in stella.storage.toml if the
contract itself is wrong (that edit is visible in review).
```

The escape hatch is explicit and *reviewable*: changing a boundary is a
manifest edit that shows up in the PR diff, not a silent override.

### Ring 3 — semantic near-duplicates → fail once, pass with declared intent

Exact names can't catch `payment_records` vs `payments` when normalization
differs enough, or `invoice_lines` vs `line_items`. For each proposed
relation/field, the gate scores existing same-level entities by embed-card
cosine (via `context.db`) fused with name token-set overlap. Above threshold
(default 0.80, tunable in the manifest):

1. The write **fails once**, listing the top-k similar entities *with their
   intent sentences and provenance* — the agent sees what already exists and
   what it's for, at the exact moment it matters.
2. The retry passes only if the tool call carries a `storage_intent` argument:
   one sentence of purpose **and** one clause of why the existing entities
   don't fit. The gate records it in the manifest (`origin = "declared"`,
   attributed to the execution id) and lets the write through.

This is the "hard but arguable" property: duplication now costs an explicit,
durable, reviewable justification instead of being the path of least
resistance — and the justification itself populates the map, so every new
object is born with intent. Ring 3 never blocks outright: a similarity score
is evidence, not proof, and a false-positive hard block on a 300-turn task is
worse than a challenged write. When `context.db` or embeddings are
unavailable, Ring 3 degrades to name-overlap only; Rings 1–2 never degrade.

### Fleet coherence

The index is per-workspace and live (§10), so fleet agents sharing a worktree
share one gate view; agents in separate worktrees each gate against their own
branch's schema plus manifest — merge conflicts in `stella.storage.toml` then
surface cross-branch drift at merge time, in review, instead of at runtime.

---

## 9. The drift report: bloat made visible

`stella storage drift` (and an Observatory panel over the same query) — pure
reads, report-only:

| Signal | Detection |
|---|---|
| Near-duplicate pairs | embed-card cosine above threshold between existing same-level entities |
| Dead fields/relations | no `query`/`mutation`/`code_ref` link after N full indexes |
| DDL ↔ ORM disagreement | same address, conflicting structure from two adapters |
| Boundary strays | entity whose own intent embeds closer to a *different* namespace's boundary than its own |
| Orphaned meaning | manifest entries whose source entity vanished |
| Coverage | % of entities with intent; % declared vs inferred; stale count |

Nothing is auto-fixed. The report is the agenda for a human (or an explicitly
tasked agent) to consolidate — the anti-bloat loop closes through review, not
automation.

---

## 10. Liveness: the map is never stale

All shipped mechanisms, extended — no new processes:

1. **Mount warm-up:** full adapter sweep during `CodeGraph::mount()`, then
   manifest mirror, then embed-card catch-up via the existing `warm_index`.
2. **Watcher:** `is_watch_relevant` extended to adapter patterns (`.sql`,
   `.prisma`, `stella.storage.toml`, plus any file that produced storage
   entities on its last index — self-maintaining watchlist).
3. **Post-write injector:** the shipped `WatchInjector` hook covers the
   agent's own writes; the file-pattern check switches from `.sql`-only to
   the adapter-pattern set.
4. **Inference fold:** meaning backlog drains at `ContextWrite` finalize (§5c).

Every pass is one SQLite transaction; byte-identical files short-circuit on
`content_sha256`, unchanged entities short-circuit on `struct_sha256`, and
unchanged embed cards short-circuit on the embedding content-hash key. Cost
on a no-schema-change turn: a few hash lookups.

---

## 11. Phased implementation

**Phase A — canonical model + SQL depth** (foundation)
`stella-graph/src/storage_model.rs` (new), `storage_store.rs` (new tables §6),
deepen the SQL query pass to capture types/nullability/defaults/constraints/
PK/FK, addresses + normalization. Retire nothing yet.
*Delivers:* full-fidelity SQL schema in the index, addressable and queryable.

**Phase B — manifest + mirror** 
`stella-graph/src/manifest.rs` (parse/serialize `stella.storage.toml`),
layer discovery in `stella storage init`, mirror-on-mount, `stella storage
show|tree|grep`.
*Delivers:* layers, boundaries, durable meaning; the vendor-agnostic frame.

**Phase C — gate v2 rings 1–2** 
Move extraction into `stella-graph`; `stella-tools` gate consumes it across
all adapter patterns; normalized-name + cross-layer + field-level conflicts;
boundary redirects. Retire the regex scanner.
*Delivers:* deterministic duplicate/misplacement blocking, all technologies.

**Phase D — embedding + inference fold** 
Embed cards, `NodeKind::Storage`, recall integration, `ContextWrite` meaning
inference, `storage_map_updated` event.
*Delivers:* semantic retrieval of schema; intents self-populate.

**Phase E — ring 3 + adapters beyond SQL** 
Similarity challenge + `storage_intent` argument; Prisma, Drizzle, Django,
SQLAlchemy, Mongoose, TypeORM, DynamoDB adapters (in observed-frequency
order); drift report + Observatory panel.
*Delivers:* the full "hard to duplicate" property, vendor-agnostic.

**Phase F (optional) — `stella storage import`** 
One-shot live-DB bootstrap for projects whose schema is not in-repo.

Each phase ships independently behind the same principle as the current gate:
an empty index/manifest makes every new mechanism a no-op, so projects
without storage never see any of it.

---

## 12. Risks and tradeoffs

| Risk | Mitigation |
|---|---|
| Adapter sprawl — ten ecosystems, each a moving target | Adapters are additive tree-sitter passes emitting one canonical struct; an unrecognized pattern yields a false negative repaired by a manifest stub, never a false block. Ship in observed-frequency order. |
| Ring 3 false positives annoy on legitimate new tables | Fail-once-then-declare costs one retry with one sentence — bounded, and the sentence is value, not waste (it populates the map). Threshold tunable per-project in the manifest. |
| Manifest merge conflicts in fleets | TOML with one entity per table-key merges cleanly except when two branches touch the *same* entity — which is precisely the drift we want surfaced in review. |
| Inferred intents are wrong | Marked `origin = "inferred"`, visible in PR diffs, never overwrite human text, refreshed only when stale. A wrong sentence is one edit away; a missing one is invisible. |
| Embed quality under the default `HashEmbedder` | Names + type tokens carry trigram matching acceptably; intent sentences make cards strictly better than today's name-only nodes. The `EmbedderFingerprint` seam upgrades everything incrementally when a real model lands. |
| Normalization folds too aggressively (`user_ids` vs `user_id`) | Normalization only *blocks* on relation/field name equality within a namespace — same-namespace near-names are almost always genuine drift. The error message shows both spellings; a genuinely distinct object renames or declares intent. |
| Gate latency on large schemas | Rings 1–2 are hash-map lookups against the in-memory index. Ring 3 is top-k cosine over relations in one layer (hundreds, not millions) — sub-millisecond at brute force. |

---

## 13. What this gets you

| Capability | Mechanism | Phase |
|---|---|---|
| Every storage technology mapped, not just SQL | canonical model + adapters + manifest stubs | A, B, E |
| "What gets saved where" is a queryable fact | layer/namespace/relation boundaries + `stella storage tree` | B |
| Full schema indexed to field granularity with types/constraints/defaults | deepened SQL pass + adapter emission | A, E |
| Every schema/table/column carries a one-sentence purpose | harvest + inference fold + gate justification path | B, D, E |
| Schema in semantic search beside code | embed cards + `NodeKind::Storage` + existing recall fusion | D |
| Duplicate table/column impossible to write silently, any vendor | rings 1–2, shared extraction | C, E |
| Near-duplicate concepts challenged with evidence | ring 3 fail-once + declared intent | E |
| Misplaced data (wrong table/layer) caught at write time | boundary redirects + cross-layer check | C |
| Bloat visible and actionable | drift report + Observatory panel | E |
| Survives 300-turn compaction, fleet fan-out, cache deletion | live index + committed manifest + durable meaning rules | A–D |
