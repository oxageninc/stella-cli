# Code graph tool: output shape and utility

A worked evaluation of `graph_query` against the stella codebase (a Rust
monorepo, 207 indexed files / 4,870 symbols / 1,485 imports at time of
writing). Each query below is one an agent would realistically run during a
task, followed by the raw output and a verdict on whether it beats the
grep/glob/find equivalent.

---

## The five operations

| op | input | question it answers |
|---|---|---|
| `definitions` | symbol name | "where is X defined?" |
| `references` | symbol name | "who calls/uses X?" |
| `imports` | file path | "what does this file depend on?" |
| `importers` | file path | "what depends on this file?" |
| `neighbors` | file path | "what's defined in / around this file?" |

---

## 1. `definitions` — where is a symbol defined?

```
op: definitions
target: graph_snapshot
```

```text
- fn graph_snapshot (stella-cli/src/agent.rs:1488)
  pub(crate) fn graph_snapshot(
      workspace_root: &std::path::Path,
  ) -> Option<stella_tui::GraphSnapshot> {
      graph_snapshot_focus(workspace_root, None)
  }
```

**Verdict: clearly better than grep.** A `grep "fn graph_snapshot"` would
find it, but the graph returns the exact definition site with the full
signature and body in one hop — no scanning a results list, no opening a
file to read the surrounding context. It also disambiguates *kind* (fn vs
struct vs trait vs enum) for free. Grep gives you lines; the graph gives
you definitions.

---

## 2. `references` — who uses a symbol?

```
op: references
target: graph_snapshot
```

```text
- stella-cli/src/agent.rs:1488      pub(crate) fn graph_snapshot(
- stella-cli/src/agent.rs:2886      let snap = graph_snapshot(root.path()).expect("snapshot");
- stella-cli/src/agent.rs:2913      assert!(graph_snapshot(root.path()).is_none());
- stella-cli/src/command_deck.rs:299   initial_graph: agent::graph_snapshot(&cfg.workspace_root),
- stella-cli/src/command_deck.rs:333   if let Some(snapshot) = agent::graph_snapshot(&ready_root) {
- stella-cli/src/command_deck.rs:560   if let Some(snapshot) = agent::graph_snapshot(&cfg.workspace_root) {
```

**Verdict: better than grep.** Grep would match `graph_snapshot` inside
comments, doc-links, and string literals too. The graph returns only
semantic references — call sites, re-exports, type annotations. For impact
analysis ("if I change this signature, what breaks?") this is the right
signal and grep is the noisy one.

---

## 3. `references` on a type — the full usage surface

```
op: references
target: FlipOracle
```

```text
- stella-pipeline/src/lib.rs:77     pub use verify::{FlipOracle, FlipState, LadderDecision, LadderInputs};
- stella-pipeline/src/pipeline.rs:63    FlipOracle, JudgeVerdict as ModelJudgeVerdict, ...
- stella-pipeline/src/pipeline.rs:324   oracle: FlipOracle,
- stella-pipeline/src/pipeline.rs:743   let mut oracle = FlipOracle::new();
- stella-pipeline/src/pipeline.rs:1015  oracle: &mut FlipOracle,
- stella-pipeline/src/verify.rs:8       //! # The flip oracle ([`FlipOracle`])
- stella-pipeline/src/verify.rs:70      pub struct FlipOracle {
  ... (12 more, including all test sites)
```

**Verdict: much better than grep.** This spans two crates and correctly
catches the `pub use` re-export, struct-field type annotations, function
parameters, and every test instantiation. Grep would need case-sensitive
matching and would still mix in doc comments. The graph distinguishes a
definition line from a reference line at each hit.

---

## 4. `neighbors` — table of contents for a file

```
op: neighbors
target: stella-pipeline/src/verify.rs
```

```text
- enum FlipState (verify.rs:39)
- enum ObserveOutcome (verify.rs:50)
- struct FlipOracle (verify.rs:70)
- fn new / fn state / fn is_flipped / fn observe / fn normalize_command
- enum LadderDecision (verify.rs:161)
- struct LadderInputs (verify.rs:178)
- fn ladder_decision (verify.rs:201)
- fn deterministic_pass_evidence / fn deterministic_fail_evidence
- struct JudgeVerdict / fn parse_judge_response / fn heuristic_fallback
- fn judge_prompt / fn guidance_prompt
  ... (all test fns included)
```

**Verdict: no grep equivalent.** This is the graph's killer feature for
orientation. `grep "^fn \|^struct \|^enum "` is a crude approximation that
misses `impl` blocks, `pub(crate)` visibility, doc-hidden items, and
inlines. The neighbor list is the file's public+private surface in one
call — the fastest way to answer "what does this file do?" before reading
it. `ls` / `find` can't do this at all; `read_file` forces you to load the
whole file.

---

## 5. `imports` — what does a file pull in?

```
op: imports
target: stella-cli/src/agent.rs
```

```text
- colored::Colorize (unresolved)
- crate::config::Config (unresolved)
- crate::config::{ConfiguredProvider, PROVIDERS, ProviderConfig} (unresolved)
- stella_core::{BudgetGuard, Engine, EngineConfig, ...} (unresolved)
- stella_pipeline::{Pipeline, PipelineConfig, ...} (unresolved)
- stella_tools::ToolRegistry (unresolved)
- stella_tui::{GraphEdge, GraphNode, GraphSnapshot} (unresolved)
  ... (30 use-statements total, all unresolved)
```

**Verdict: weaker than grep, and the `(unresolved)` is the problem.** The
specifiers are indexed faithfully, but for Rust the `use` paths are never
resolved to file paths — every entry is tagged `(unresolved)`. A `grep
"^use " agent.rs` returns the same information in a more familiar format.
The value of an `imports` query is supposed to be the *resolved* edge ("this
file depends on `stella-tools/src/registry.rs`"), and for Rust that edge
doesn't exist in the index. This operation works for TS/JS/Python relative
imports; for Rust it's a flat list.

---

## 6. `importers` — the broken operation for Rust

```
op: importers
target: stella-core/src/bus.rs
```

```text
no importers found for `stella-core/src/bus.rs` — importer edges exist only
where import resolution succeeds (relative TS/JS/Python imports); Rust `use`
paths are indexed unresolved. Try `references` on the module name instead.
```

**Verdict: does not work for this repo.** `importers` is the reverse of
`imports` — "who depends on this file?" — and it is one of the most
valuable questions in impact analysis. For Rust it returns nothing, because
the import edges it needs are never resolved (see query 5). The tool's own
error message correctly redirects to `references`, but that is a workaround,
not the feature. **This is the single biggest reason the tool underperforms
in practice:** the operation that answers "what breaks if I change this
file?" silently returns empty on the dominant language in the workspace.

---

## 7. `definitions` on a heavily-used struct

```
op: definitions
target: ToolRegistry
```

```text
- struct ToolRegistry (stella-tools/src/registry.rs:47)
  pub struct ToolRegistry {
      tools: HashMap<String, Arc<dyn Tool>>,
      late_tools: std::sync::RwLock<HashMap<String, Arc<dyn Tool>>>,
      root: PathBuf,
      touched: std::sync::Mutex<FileTouchLedger>,
      citations: crate::memory::CitationLedger,
      agent_uses: std::sync::Mutex<crate::agent_use::AgentUseLedger>,
      mcp_usage: stella_core::mcp_usage::McpUsageLedger,
      schema_index: std::sync::Mutex<SchemaIndex>,
      bus: std::sync::RwLock<Option<HookBus>>,
  }
```

**Verdict: better than grep.** One call returns the struct definition with
all fields and their doc comments. Grep would find `struct ToolRegistry`
but you'd then have to `read_file` to see the fields. The graph collapses
two operations into one.

---

## 8. `references` on the same struct — the fan-out

```
op: references
target: ToolRegistry
```

```text
- stella-cli/src/agent.rs:35     use stella_tools::ToolRegistry;
- stella-cli/src/agent.rs:287    let registry: Arc<ToolRegistry> =
- stella-cli/src/agent.rs:748    // Concrete `Arc<ToolRegistry>` ...
- stella-cli/src/command_deck.rs:70   use stella_tools::ToolRegistry;
  ... (30+ hits across agent.rs and command_deck.rs, truncated)
```

**Verdict: better than grep.** Same advantage as query 2/3 — semantic
references only, across crates, with the definition line flagged. The
truncation at ~30 hits with a "narrow the query" hint is good ergonomics;
grep would dump everything and force the agent to page through it.

---

## 9. `neighbors` on a large orchestration file

```
op: neighbors
target: stella-cli/src/command_deck.rs
```

```text
- fn now_ms / fn debug_log_path
- enum TurnEnd { Finished, Cancelled { hold: bool }, Quit }
- struct HoldState + fn held / fn release / fn cancelled / fn stop_and_hold
- fn requeue_front
- fn run_deck_session (the 100-line entry point, full body)
- fn mcp_outcome_report / fn mcp_snapshot / fn send_mcp_snapshot
- fn run_mcp_search / fn service_mcp_action
- enum DeckCommand + fn deck_reserved
- fn handle_agents_input / fn save_agent / fn pin_agent / fn create_agent
- fn record_agent_invocation / fn deck_slash_commands
  ... (47 more, truncated)
```

**Verdict: better than `ls` or `read_file`.** This file is ~2000 lines.
`neighbors` gives you the structural map — every function, enum, and entry
point with its signature — without loading the whole file into context.
For a "what's in here and where do I start?" question this is the right
tool. `ls` tells you the file exists; `read_file` costs you the whole file.

---

## 10. `imports` on a core module

```
op: imports
target: stella-core/src/bus.rs
```

```text
- serde::{Deserialize, Serialize} (unresolved)
- serde_json::Value (unresolved)
- std::sync::atomic::{AtomicU64, Ordering} (unresolved)
- std::sync::{Arc, Mutex, Weak} (unresolved)
- std::panic::{AssertUnwindSafe, catch_unwind} (unresolved)
- std::time::{SystemTime, UNIX_EPOCH} (unresolved)
```

**Verdict: same limitation as query 5.** Faithful specifiers, but all
`(unresolved)` — no file-level dependency edge. For a "what does this module
need?" question, `grep "^use "` is equally informative and more familiar.

---

## 11. `definitions` / `references` on a symbol that doesn't exist

```
op: references
target: NonExistentSymbolXYZ123
```

```text
no references found for `NonExistentSymbolXYZ123`
(index may be stale — `stella init` re-indexes)
```

**Verdict: good error handling.** A clean empty result with an actionable
hint, not a silent zero-row return. Grep would also return nothing, but the
graph's message tells you *why* (stale index) and *what to do* (re-index).

---

## Summary scorecard

| operation | works for Rust? | beats grep/glob? | agent should prefer it? |
|---|---|---|---|
| `definitions` | ✅ yes | ✅ yes — semantic, includes body | **yes** |
| `references` | ✅ yes | ✅ yes — semantic, cross-crate, no comment noise | **yes** |
| `neighbors` | ✅ yes | ✅ yes — no grep equivalent for "file contents map" | **yes** |
| `imports` | ⚠️ partial — specifiers only, all `(unresolved)` | ❌ no — `grep "^use "` is equivalent | no |
| `importers` | ❌ no — returns empty | ❌ no — grep on the module name is the only path | **no** |

---

## Why agents aren't using it (diagnosis)

Three factors, in descending order of impact:

### 1. `importers` is broken for Rust — and that's the highest-value question

"What depends on this file?" is the most important dependency question for
safe refactors, and `importers` silently returns empty on every Rust file in
this repo. An agent that tries it once, gets nothing, and falls back to
grep learns "the graph tool doesn't work." That generalizes: if one of the
five operations is a no-op, trust in the whole tool drops. The redirect to
`references` is correct but the agent has to read the error and adapt —
which it often won't.

### 2. The two best operations (`definitions`, `references`) overlap with grep

Grep is familiar, always available, and good enough for most "where is X"
questions. The graph is *better* (semantic, no comment noise, includes the
definition body), but the marginal improvement isn't large enough to
overcome habit — especially for an agent that doesn't know the graph's
advantages. The tool description says "cheaper and more precise than grep,"
but `references` on a popular symbol returns 30+ hits that still need
scanning.

### 3. Availability gating

`graph_query` is only registered after `stella init` builds the index
(`enable_code_graph_if_available`). If an agent's session starts before the
index exists — or the background build hasn't finished — the tool isn't even
in the schema. The agent never sees it and defaults to grep for the entire
session.

### What would move adoption

- **Fix Rust import resolution** so `importers` returns real file-level
  edges. This is the highest-leverage fix: it makes the one broken operation
  work, and it makes `imports` genuinely better than grep (resolved edges,
  not just specifiers).
- **Lead with `neighbors` in the tool description.** It's the one operation
  with no grep/glob equivalent — "the fastest way to see what's in a file
  without reading it" is a compelling, unique value prop.
- **Keep the availability gating** — a stale index is worse than no index —
  but consider auto-building on first session start so the tool exists from
  turn 1.
