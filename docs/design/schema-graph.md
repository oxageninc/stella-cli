# Design: Schema-Aware Code Graph for Zero Schema Drift

**Status:** Draft
**Goal:** On super-long-horizon tasks (200+ turns), the agent never creates a
table/column/type that already exists, never proposes a conflicting definition,
and retrieves schema alongside code when the domain matches.

---

## Challenge: what you proposed won't fully work

The "schema map + edges" approach addresses **awareness** but not **enforcement**.
Here's the actual failure mode on a 200-turn task:

1. **Turn 5:** Agent reads `schema.sql`, context has the full schema.
2. **Turn 50:** Agent creates `payments` table via a migration. Conversation
   has it.
3. **Turn 100:** Compaction evicts early turns. The conversation no longer
   mentions `payments`. The code graph was built at `stella init` (turn 0) and
   doesn't know about it.
4. **Turn 150:** Agent needs to create `refunds`. It queries the graph for
   payment-related code. No `payments` table in the graph. Agent creates
   `payment_records` — a duplicate concept.
5. **Turn 200:** Schema drift. Two tables for the same domain.

Three problems:

| Problem | Why "schema map + edges" alone doesn't fix it |
|---|---|
| **Staleness** | The graph built at `stella init` is a snapshot. When the agent itself writes migrations mid-task, the graph doesn't know. **The schema index must be live — re-indexed on every schema file change, including the agent's own writes.** |
| **Passive vs active** | Retrieval is passive — the agent must think to look. On turn 150, the agent doesn't know to search for existing payment tables. **Zero drift requires a pre-write gate** — a deterministic check that fires when the agent is about to write schema, not when it decides to search. |
| **Tree-sitter ≠ semantics** | Tree-sitter parses structure, not meaning. `table! { payments (id) { ... } }` is parseable. But `#[derive(Insertable)] #[table_name = "payments"] struct Payment { ... }` requires understanding the derive macro attribute to know this maps to the `payments` table. **SQL migrations are the ground truth; ORM models are hints.** |

---

## Architecture: three layers

```
Layer 3: Pre-Write Gate (stella-tools)
  ┌──────────────────────────────────────────┐
  │  write_file / edit_file targets a schema  │
  │  file → parse proposed content → conflict │
  │  check against Layer 1 → warn or block    │
  └──────────────────────────────────────────┘
                    ▲ reads from
Layer 2: Schema-Aware Retrieval (stella-context, no new code)
  ┌──────────────────────────────────────────┐
  │  Schema nodes are just graph nodes. They  │
  │  participate in recall_scoped alongside   │
  │  code nodes. Domain-tagged automatically. │
  └──────────────────────────────────────────┘
                    ▲ fed by
Layer 1: Live Schema Index (stella-graph)
  ┌──────────────────────────────────────────┐
  │  SQL DDL parser + ORM pattern detector    │
  │  Canonical schema model (Table/Column/Enum)│
  │  Schema-to-code edges (ORM model → table) │
  │  Re-indexed on every schema file change    │
  │  (watcher + post-write trigger)            │
  └──────────────────────────────────────────┘
```

---

## Layer 1: Live Schema Index (`stella-graph`)

### 1a. New node types

Extend `SymbolKind` with schema kinds:

```rust
// stella-graph/src/symbol.rs
pub enum SymbolKind {
    // existing...
    Function, Method, Struct, Class, Enum, Trait, Interface,
    // new:
    Table,      // CREATE TABLE / ORM model class
    Column,     // column definition within a table
    SchemaEnum, // CREATE TYPE / enum constraint
    View,       // CREATE VIEW
}
```

These are stored in the existing `code_graph_symbols` table (kind is TEXT).
No schema change needed — the store is kind-agnostic.

### 1b. SQL DDL parser

Add `tree-sitter-sql` as a workspace dependency and a new `Language::Sql`:

```rust
// stella-graph/src/lang.rs
pub enum Language {
    Rust, Python, JavaScript, TypeScript, Tsx,
    Sql,  // new
}
```

Tree-sitter query for SQL DDL:

```scheme
; stella-graph/src/queries.rs
pub const SQL_SYMBOLS: &str = r#"
(create_table statement name: (_) @name) @table
(column_definition name: (_) @column_name) @column
(create_type statement name: (_) @name) @schema_enum
"#;
```

This captures `CREATE TABLE payments (...)`, individual columns, and
`CREATE TYPE payment_status AS ENUM (...)`.

### 1c. ORM pattern detector

ORM models live in already-parsed languages. Add a **second query pass** that
detects schema-related patterns:

**Diesel (Rust):**
```scheme
; diesel table! macro
(macro_invocation macro: (identifier) @macro_name
  (#eq? @macro_name "table")
  (token_tree
    (identifier) @table_name)) @table
```

**Django/SQLAlchemy (Python):**
```scheme
; Django: class Payment(models.Model)
; SQLAlchemy: class Payment(Base): __tablename__ = "payments"
(class_definition name: (_) @name
  superclasses: (argument_list) @bases) @table
```

The detector extracts:
- Table name (from `table!` macro name, `__tablename__` assignment, or class name)
- Column names (from struct fields or class attributes with type annotations)
- Source location (file + line range for provenance)

### 1d. Canonical schema model

Merge SQL DDL + ORM hints into a canonical view:

```rust
// stella-graph/src/schema.rs (new file)
pub struct SchemaObject {
    pub name: String,           // "payments"
    pub kind: SchemaKind,       // Table, Enum, View
    pub columns: Vec<ColumnDef>, // empty for enums/views
    pub source: SourceSpan,     // file + line range
    pub source_type: SourceType, // SqlDdl, DieselMacro, DjangoModel, etc.
}

pub struct ColumnDef {
    pub name: String,
    pub sql_type: Option<String>,
    pub nullable: bool,
    pub constraints: Vec<String>, // PRIMARY KEY, REFERENCES users(id), etc.
}
```

When SQL DDL and an ORM model define the same table, SQL DDL wins (it's the
ground truth). The ORM model is kept as a "code reference" edge.

### 1e. Schema-to-code edges

Add a new edge table:

```sql
-- stella-graph/src/store.rs MIGRATION
CREATE TABLE code_graph_schema_links (
    id INTEGER PRIMARY KEY,
    schema_symbol_id INTEGER NOT NULL REFERENCES code_graph_symbols(id) ON DELETE CASCADE,
    code_symbol_id INTEGER REFERENCES code_graph_symbols(id) ON DELETE CASCADE,
    code_file_id INTEGER REFERENCES code_graph_files(id) ON DELETE CASCADE,
    link_kind TEXT NOT NULL  -- 'orm_model', 'query', 'mutation', 'handler'
);
```

Edge types:
- `orm_model`: ORM model class → table (e.g., `Payment` struct → `payments` table)
- `query`: Function containing a SELECT on the table
- `mutation`: Function containing INSERT/UPDATE/DELETE on the table
- `handler`: API route handler that uses the ORM model

These edges are inferred during parsing:
- ORM models are detected by the pattern detector (1c)
- Query/mutation/handler edges require detecting SQL string literals or ORM
  call patterns within function bodies (e.g., `Payment::all()`, `diesel::insert_into(payments)`)

### 1f. Live re-indexing

Two triggers:

**File watcher** (already exists in `watch.rs`):
- Extend `is_watch_relevant` to pass through `.sql` files and schema-related
  source files (files matching ORM patterns on their last index)
- The existing debounce → `apply_changes` pipeline handles the rest

**Post-write trigger** (new, in `stella-cli`):
- After `write_file` or `edit_file` succeeds, check if the target file matches
  schema patterns (`.sql`, `*.prisma`, or the file already has schema nodes)
- If so, inject the changed path into the graph's `WatchInjector`
- This is the critical fix for staleness: the agent's own writes trigger re-indexing

```rust
// stella-cli/src/agent.rs — after tool execution
if matches!(op, FileOp::Create | FileOp::Update) {
    if is_schema_file(&path) {
        if let Some(injector) = &graph_injector {
            injector.inject(&path);
        }
    }
}
```

---

## Layer 2: Schema-Aware Retrieval (no new code)

Schema nodes are just `SymbolKind::Table` / `Column` nodes in the existing
graph. They participate in `recall_scoped` and `query()` automatically:

- **Domain match:** Schema nodes get domain tags from proximity (a `payments`
  table in `migrations/` is domain-tagged `payments` by the same heuristic that
  tags code files)
- **Name match:** `query("payment")` already does `definitions("payment")` →
  finds the `payments` table node
- **Neighborhood:** `neighbors(migrations/001_payments.sql)` returns the table,
  its columns, and linked ORM model code
- **Score:** Schema nodes get `SCORE_DEFINITION` (0.9) — they appear early in
  recall results

No `stella-context` changes needed. The schema nodes flow through the existing
OCP frame pipeline.

---

## Layer 3: Pre-Write Schema Gate (`stella-tools`)

This is what makes "zero drift" achievable. The gate is deterministic, not
LLM-dependent.

### Design

When `write_file` or `edit_file` targets a schema file, parse the proposed
content for schema objects BEFORE the write completes. Check each object
against the live schema index. If a conflict exists, return a structured
warning.

```rust
// stella-tools/src/schema_gate.rs (new file)
pub struct SchemaConflict {
    pub kind: ConflictKind,
    pub existing: String,  // "Table `payments` with columns: id, amount, currency, user_id"
    pub proposed: String,  // "CREATE TABLE payments (...)"
    pub source: String,    // "migrations/003_payments.sql:1"
}

pub enum ConflictKind {
    DuplicateTable,     // table name already exists
    DuplicateColumn,    // column name already exists on this table
    TypeMismatch,       // same column, different type
    OrphanedFk,         // FK references a table that doesn't exist
}
```

### Integration point

The gate runs as a **pre-write check** inside `write_file` and `edit_file`:

```rust
// stella-tools/src/write.rs
async fn execute(&self, input: &Value, root: &Path) -> ToolOutput {
    let path = /* ... */;
    let content = /* ... */;

    // If this is a schema file, check for conflicts
    if is_schema_file(&path) {
        if let Some(graph) = self.schema_index.as_ref() {
            if let Some(conflicts) = graph.check_schema_conflicts(&content, &path) {
                return ToolOutput::Error {
                    message: format_schema_conflicts(&conflicts),
                };
            }
        }
    }

    // Proceed with the write
    tokio::fs::write(&resolved, &content).await...
}
```

The `ToolRegistry` holds a `Mutex<SchemaIndex>` that defaults to empty — an
empty index makes the gate a no-op; `update_schema_index` refreshes it when
schema objects are known.

### What the model sees

```
ToolOutput::Error {
    message: "Schema conflict detected before write:\n\
              \n\
              CONFLICT: Table `payments` already exists\n\
              Existing: migrations/001_init.sql:15 — CREATE TABLE payments (\n\
                        id SERIAL PRIMARY KEY,\n\
                        amount NUMERIC(10,2) NOT NULL,\n\
                        currency TEXT DEFAULT 'USD',\n\
                        user_id INTEGER REFERENCES users(id)\n\
                      )\n\
              Proposed: migrations/003_add_payments.sql:1 — CREATE TABLE payments (...)\n\
              \n\
              Did you mean to ALTER TABLE instead? Or is this a different table\
              that needs a different name?"
}
```

The model sees the conflict BEFORE the write. It can then:
- Choose a different table name
- ALTER the existing table instead
- Ask the user

This is the `verify_done` pattern applied to schema: a deterministic gate that
catches the error before it lands.

### Scope control

The gate is unconditional and blocking: whenever `write_file` or `edit_file`
targets a `.sql` file (`schema_gate::is_schema_file`) and the proposed content
creates a table, type, or view whose name is already in the schema index, the
tool call fails with the conflict error above. There is no enable/disable
flag and no warn mode.

Scope is bounded by the index rather than by configuration: when the schema
index is empty — no schema objects known for the workspace — `find_conflicts`
returns nothing and every write proceeds, so workspaces without SQL schemas
never see the gate.

A per-workspace override in `.stella/settings.json` (a `schema_gate.enabled`
flag, or a `"warn"` strictness where the tool succeeds with a warning) is a
possible future extension — the three-scope settings loader in
`stella-cli/src/settings.rs` is the natural parse point — but it is
deliberately not implemented today.

---

## Phased implementation

### Phase 1: Live SQL index (highest impact, smallest scope)

**Files:**
- `stella-graph/src/lang.rs` — add `Language::Sql`
- `stella-graph/src/queries.rs` — add `SQL_SYMBOLS`, `SQL_IMPORTS` (empty)
- `stella-graph/src/parse.rs` — add SQL grammar to `Grammars`
- `stella-graph/src/walk.rs` — allow `.sql` files through
- `Cargo.toml` (workspace) — add `tree-sitter-sql`
- `stella-cli/src/agent.rs` — post-write trigger for `.sql` files

**Delivers:** Every `CREATE TABLE` / `CREATE TYPE` in `.sql` files is in the
code graph, live-indexed, and retrievable by name and domain.

### Phase 2: Pre-write gate

**Files:**
- `stella-tools/src/schema_gate.rs` — conflict detection logic
- `stella-tools/src/write.rs` / `edit.rs` — call the gate
- `stella-tools/src/registry.rs` — hold `Option<Arc<SchemaIndex>>`

**Delivers:** Agent cannot write a `.sql` file that creates an existing table
without seeing a conflict error.

### Phase 3: ORM pattern detection

**Files:**
- `stella-graph/src/queries.rs` — ORM-specific query patterns per language
- `stella-graph/src/schema.rs` — canonical merge of SQL DDL + ORM hints
- `stella-graph/src/store.rs` — `code_graph_schema_links` table

**Delivers:** ORM models (Diesel, Django, SQLAlchemy, Prisma) are linked to
their SQL tables. "All payment code" retrieval returns both the migration AND
the ORM model AND the handler functions.

### Phase 4: Semantic edges (calls, FKs)

**Files:**
- `stella-graph/src/parse.rs` — detect SQL string literals in function bodies
- `stella-graph/src/schema.rs` — infer `query`/`mutation`/`handler` edges

**Delivers:** The graph knows which functions read/write each table. Foreign
keys create inter-table edges. The schema graph is navigable bidirectionally.

---

## What this gets you

| Capability | Phase | Mechanism |
|---|---|---|
| Agent retrieves schema alongside code | 1 | Schema nodes in recall results |
| Agent can't create duplicate tables | 2 | Pre-write gate |
| ORM models linked to schema | 3 | Canonical merge + edges |
| "All payment code" returns schema + code + handlers | 3+4 | Domain-tagged schema-to-code edges |
| Schema stays current during 200-turn tasks | 1 | Post-write re-indexing trigger |
| Zero schema drift | 2+ | Gate enforces, index is live |

---

## Risks and tradeoffs

| Risk | Mitigation |
|---|---|
| SQL dialect variance (Postgres vs MySQL vs SQLite) | Start with ANSI SQL + Postgres extensions (most common in migrations). `tree-sitter-sql` handles the common subset. |
| ORM patterns change across versions | The detector is heuristic, not semantic. False positives (a class that looks like an ORM model but isn't) are harmless — they just add an extra graph node. False negatives (unrecognized ORM pattern) mean the table isn't linked, but SQL DDL is still the primary source. |
| Gate false positives on rename/refactor | The gate returns an error, not a hard block. The model can read the error, understand the conflict, and choose to proceed with a different approach. The `strictness` setting lets users downgrade to a warning. |
| Performance on large schema files | SQL DDL files are small (rarely >1000 lines). The conflict check is a name lookup against an in-memory HashMap, not a parse. |
| Agent writes non-SQL schema (Prisma, GraphQL) | Phase 3 adds ORM pattern detection. The gate in Phase 2 starts with SQL only and extends as parsers are added. |
