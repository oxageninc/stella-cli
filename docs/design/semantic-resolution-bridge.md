# Semantic-resolution bridge — evaluation

**Status:** proposed (evaluation, decision-ready). **Date:** 2026-07-23.
**Owner:** Mac Anderson. **Issue:** #335 (parent #336; the Rust slice of #334
is gated on this). Citations re-verified against the working tree at `fef8e4b`
(§ appendix).

---

## One sentence

The code graph is deliberately **syntactic** — tree-sitter symbols + import
edges, no types, no resolved references — and that single gap is the shared
blocker behind resolved find-references, a true call graph, symbol-aware
rename, Rust importer resolution, and the Rust slice of test-impact selection
(#334); this document weighs the three ways to close (or decline to close) it
as one decision.

## Problem: what the syntactic substrate cannot answer

The graph indexes symbols and import edges only
(`stella-graph/src/store.rs:42-91` — `code_graph_files` / `code_graph_symbols`
/ `code_graph_imports`; there is no call-edge table). No LSP, type-checker, or
rust-analyzer integration exists anywhere in the workspace; the sole mention of
language servers is a competitor trade-off note in
`docs/papers/stella-defensible-position.md:590-592`. Four capabilities are
blocked, all by the same missing ingredient:

| Blocked capability | Today's behavior | Evidence |
|---|---|---|
| Resolved find-references | `references` is a best-effort **textual** whole-word scan over every indexed file, capped at 50 hits — false-hits comments/strings/unrelated same-name symbols, false-misses aliased imports | `stella-graph/src/frames.rs:65-109` (scan), `frames.rs:400-425` (`line_contains_word`) |
| True call graph (`callers`/`callees`) | No call edges exist; the op surface is exactly `definitions/references/imports/importers/neighbors`, so "who calls this?" degrades to the textual scan above | `stella-tools/src/graph.rs:70` (op enum), `store.rs:42-91` (schema) |
| Symbol-aware rename | Unsafe by construction: `edit_file` over textual references corrupts code on any false hit/miss | consequence of the two rows above |
| Rust importer resolution | Rust `use` paths are recorded **unresolved** (`ImportKind::Absolute`, `to_path = NULL`), so `importers` returns empty for every Rust file — the tool even ships a canned apology for it | `stella-graph/src/import.rs:78-80,99-101`, `queries.rs:36-42` (`RUST_IMPORTS` + "out of scope" note), `stella-tools/src/graph.rs:157-171` (empty-importers message) |

Downstream, #334's test-impact selection walks `importers_of` transitively —
deliverable today for TS/JS/Python (resolved relative imports,
`import.rs:102-115,132-176`) and blocked for Rust, which is this repo's own
language: 376 `.rs` files, ~214k lines. The dogfooding workload suffers the
gap most.

Explicitly **out of scope**: embeddings-based semantic search is a different
bet (vectors, not resolution) with its own tracked follow-up — the ONNX
bge-small embedder (`stella-context/src/lib.rs:40`, `embed.rs:5-18`). Nothing
here conflates the two.

## Options

### A. Out-of-process LSP adapter behind a resolution port

A `SymbolResolution` port (trait) at the tools boundary; an adapter crate
drives `rust-analyzer` (and `tsserver`/`pyright` per language) as a
session-scoped child speaking LSP over stdio — the same child-process +
JSON-RPC lifecycle `stella-mcp` already manages for MCP servers, which is the
in-repo precedent for both the process supervision and the fixture-server test
strategy (`stella-mcp/tests/`).

- **Unlocks:** everything in the table — resolved references,
  `callHierarchy` (callers *and* callees), semantically safe `rename`, Rust
  `use` resolution, #334-Rust. The only option that delivers rename.
- **Dependency / process weight:** heavy. An external toolchain binary per
  language that we cannot vendor (`rust-analyzer` standalone; `tsserver` and
  `pyright` drag a Node runtime), version skew across user machines, and a
  long-lived child per session — a daemon in everything but name, against the
  no-daemon ethos and "no new dependencies casually" (`AGENTS.md`).
- **Determinism:** answers vary with server version, toolchain, and cargo
  metadata state. Tool output is volatile context (never the byte-stable
  prompt prefix), so invariant 7 survives — but witness tests can't pin real
  server output; they need a fake LSP fixture server, and per-server quirks
  (rust-analyzer's readiness notifications, tsserver's non-standard protocol
  framing) each cost adapter code.
- **Latency:** rust-analyzer cold-indexes a workspace this size in tens of
  seconds to minutes at GB-scale RSS. First-query latency violates the L-C1
  "never add latency to a query" discipline unless warmed at session start —
  which spends the memory whether or not a resolution query ever arrives.
- **Invariant fit:** clean on "ports, not concretions" (it *is* an adapter
  behind a trait; `stella-core` never sees it); hostile to dependency-light /
  no-daemon unless opt-in gated exactly like the shell and web tools
  (`tools.bash: "on"` — off by default in every scope, `AGENTS.md`;
  `stella-tools/src/bash.rs:1-12` "Opt-in, never ambient";
  `stella-cli/src/settings.rs:552` for `web`) and registered conditionally
  like `graph_query` (no server, no schema burning tokens).
- **Rough size:** LSP client core (initialize handshake, capability
  negotiation, document sync, request correlation) plus lifecycle supervision,
  the port, tool surface, settings gate, and a fixture server: ~2-3k LOC
  across a new adapter crate and `stella-tools`, before per-language quirks.

### B. Tree-sitter extension: unresolved call edges + a pure-Rust `use` resolver

Two independent slices, both index-time, both pure functions over parse trees
and the file tree — no process, no new dependency, fully deterministic.

**B1 — unresolved call edges.** New per-language capture
(`call_expression` callee name), a `code_graph_calls` table, and `callees` /
`callers` ops. `callees` of a symbol = calls inside its stored line span —
name-based but honest and mostly right. `callers` = reverse name lookup —
noisy for common names and trait methods, so it ships labeled best-effort,
exactly the discipline `references` already applies (its frames score
`SCORE_REFERENCE`, the weakest band, `frames.rs:46-50`). ~300-600 LOC in
`stella-graph`.

**B2 — Rust `use`→file resolver on the module tree.** What it would actually
take, given the data `stella-graph` already has (file paths + languages, raw
`use` specifier text, symbol spans — but **no** `mod` items in `RUST_SYMBOLS`
(`queries.rs:27-34`) and **no** Cargo awareness anywhere in the crate):

1. **Crate roots:** parse workspace `Cargo.toml`s for member names →
   `src/lib.rs` / `src/main.rs` / `src/bin/*` roots, hyphen→underscore
   normalization. `toml` is already a `stella-graph` dependency
   (`stella-graph/Cargo.toml:30`, used by the storage manifest) — zero new
   deps.
2. **Module tree:** capture `mod foo;` declarations (a new
   `(mod_item name: (_) @name)` pattern, declaration-only), resolve each to
   `foo.rs` / `foo/mod.rs` beside the declaring file. `#[path]`, macro-generated
   and `cfg`-divergent modules are documented gaps that resolve to whichever
   file exists or stay unresolved.
3. **Path walk:** `crate::` from the owning crate root, `super::`/`self::`
   relative, a bare leading segment matched against workspace member names;
   walk segments while they name modules and emit a **file-level** edge to the
   last module file. External crates stay unresolved — today's correct
   behavior. File-level edges deliberately skip `pub use` re-export chasing:
   an edge to the re-exporting file is still a true dependency edge, and
   item-level identity is precisely the semantics this option does not claim.

- **Unlocks:** Rust `importers` (and richer `neighbors`) — which is the exact
  reverse-dependency data #334's impact selection needs, at the file
  granularity it needs. B1 adds honest `callees` and best-effort `callers`.
- **Does not unlock:** resolved references, a *true* call graph, or safe
  rename — those need types, trait resolution, and method receivers. No
  amount of tree-walking gets there; claiming otherwise would be the dishonest
  version of this option.
- **Determinism / latency / weight:** total / index-time only (zero query
  cost) / zero processes. Unresolvable paths keep the existing
  unresolved-edge shape (`import.rs:35-56`), so failure is visible, never
  silent.
- **Rough size:** B2 ~500-900 LOC + tests (mod-tree builder, path walker,
  Cargo layout detection), entirely inside `stella-graph`; B1 as above.

### C. Do nothing / defer

Zero cost; every row of the blocked-capability table stays blocked, including
#334-Rust — on a Rust repo, that concedes the largest wall-clock lever on the
edit→green loop. Defensible only if dogfooding shows the agent doesn't
actually lose turns to wrong references or missing reverse-deps; current
evidence (the canned Rust-importers apology shipping in the tool,
`stella-tools/src/graph.rs:157-171`) says we already pay for the gap in agent
confusion-avoidance prose.

## Recommendation

**B first, A held behind explicit trigger + kill criteria, C rejected.**

1. **Ship B2 now** (Rust `use`→file resolver). Smallest slice with the
   highest-leverage unlock: it converts #334 from "TS/JS/Python only" to
   covering this repo's own language, with zero new dependencies, zero
   processes, and index-time-only cost. **Exit criteria:** `importers` on
   `stella-core/src/driver.rs` (and sibling crate files) returns the real
   dependent set; #334's impacted mode selects correctly on a one-file change
   in this workspace; unresolved rate for workspace-internal `use` paths
   drops below ~5% (macro/`#[path]` remainder), measured by the existing
   unresolved-edge kind counts.
2. **Ship B1 next** (call edges) as an independent increment: `callees`
   honest, `callers` labeled best-effort — parity with the `references`
   scoring discipline, not a false claim of resolution.
3. **Hold A** — do not build it now, do not close the door. **Trigger:** after
   B lands, dogfooding/telemetry still shows turns lost to wrong-reference
   answers, or symbol-aware **rename** becomes a required capability (B can
   never deliver rename; that demand alone justifies A). **Shape if
   triggered:** session-scoped child via the `stella-mcp` lifecycle pattern,
   opt-in `tools.lsp: "on"` (off in every scope, the bash/web pattern),
   conditionally registered like `graph_query`, degrading to the graph answer
   on any server failure. **Kill criteria:** if on this repo a warm-up budget
   of ≤30s background / ≤1.5 GB RSS / ≤200ms p50 per resolution query can't be
   met, it does not ship even as an opt-in; if behavior can't be pinned by a
   fixture LSP server well enough to write witness tests, it fails the
   definition of done and does not ship at all.

Sequencing B before A is also the cheap information play: B2's resolver tells
us how much of the pain was ever *reference resolution* versus plain
*reverse-dependency edges* — data that makes the A decision honest instead of
speculative.

## Non-goals

- Item-level Rust name resolution (types, trait dispatch, re-export chasing)
  in Option B — file-level edges only; the doc says so wherever it matters.
- Embeddings/semantic search — separate bet, tracked at
  `stella-context/src/embed.rs:5-18`.
- Any always-on background language server — nothing in this doc proposes an
  ambient daemon under any option.

## Appendix: what we verified in-tree (2026-07-23, `fef8e4b`)

| Claim | Where |
|---|---|
| Only language-server mention in the repo is a competitor trade-off note; no LSP/rust-analyzer code or doc exists | `docs/papers/stella-defensible-position.md:590-592`; repo-wide search finds no other genuine hit |
| Graph schema is files + symbols + imports (+ storage objects); no call-edge table | `stella-graph/src/store.rs:42-91` |
| `references` is a capped textual whole-word scan | `stella-graph/src/frames.rs:39-40,65-109,400-425` |
| Query surface is exactly `definitions/references/imports/importers/neighbors` | `stella-tools/src/graph.rs:70` |
| Rust `use` recorded unresolved; module→file resolution declared out of scope | `stella-graph/src/import.rs:78-80,99-101`; `queries.rs:36-42` |
| Tool ships a canned empty-importers explanation for Rust | `stella-tools/src/graph.rs:157-171` |
| `RUST_SYMBOLS` captures no `mod` items; no Cargo.toml parsing in `stella-graph` (manifest.rs is the storage map) | `stella-graph/src/queries.rs:27-34`; `stella-graph/src/manifest.rs:1-11` |
| TS/JS/Python relative imports resolve to real files (the #334-today path) | `stella-graph/src/import.rs:102-115,132-227` |
| `toml` already a `stella-graph` dependency | `stella-graph/Cargo.toml:30` |
| Opt-in gating precedent: bash/web off by default in every scope | `stella-tools/src/bash.rs:1-12`; `stella-cli/src/settings.rs:552`; `AGENTS.md` (`.stella/` table) |
| Child-process JSON-RPC lifecycle + fixture-server test precedent | `stella-mcp/src/client.rs`, `stella-mcp/tests/` |
| Invariants weighed: ports-not-concretions, no I/O in engine, byte-stable prompts, no casual dependencies | `AGENTS.md` §"Architecture: ports, not concretions", §"Code style" |
| Workspace scale used for latency framing: 376 `.rs` files, ~214k lines | `fd -e rs` count at `fef8e4b` |
| ONNX embedder is a separate tracked follow-up | `stella-context/src/lib.rs:40`, `embed.rs:5-18` |

Line numbers in issue #335 predate recent refactors; the table above is the
re-verified set (e.g. the issue's `graph.rs:171-178` for the Rust-importers
gap is `stella-tools/src/graph.rs:157-171` today).
