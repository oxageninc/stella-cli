# Stella serve surface — the headless engine for Oxagen

**Status:** proposed. **Date:** 2026-07-20. **Owner:** Mac Anderson.
**Companion:** `oxagen-platform/docs/specs/agent-engine-v2/` (ADR-033 + spec) — the
host side. This document is the *Stella* side of the same integration.

---

## One sentence

Expose the Stella engine as a long-lived, multi-session **service** — a
step-scoped `stella-engine` facade wrapped in a `stella-serve` HTTP/SSE sidecar —
so Oxagen's web app drives the Rust core over a wire protocol whose payload is
already Stella's serialized `AgentEvent` stream, while every side effect the
engine requests round-trips back to Oxagen's kernel over the same connection.

This is **Option B** of ADR-033 (the Rust sidecar), which that ADR keeps as the
documented fallback with "identical port surface, transport swappable." The user
has now elected the sidecar model — "Oxagen's web app uses the rust app under the
hood, with infra provisioned to support it." Option A (the napi embed) and Option
B share the *same* upstream Stella work; this doc scopes that shared work plus the
serve/transport layer B additionally needs.

## Why the engine is already 90% ready (evidence from the line-by-line sweep)

A full read of `stella-protocol`, `stella-core`, `stella-cli`, `stella-model`,
`stella-store`, `stella-context`, `stella-graph`, `stella-tools`, `stella-mcp`,
`stella-pipeline`, `stella-fleet`, and `stella-observatory` (2026-07-20) confirms
the engine is structured as a headless library, not a terminal program:

1. **The core is I/O-free by construction.** `stella-core` depends on tokio with
   `features = ["sync"]` only — no `rt`/`io`/`net`/`fs`/`process`/`time`. No
   `println!`, no `std::env`, no `current_dir`, no `process::exit`, no globals,
   no `unsafe`. `Engine::run_turn(&mut messages, &mut budget, &events)`
   (`stella-core/src/driver.rs:329`) holds no conversation state — the caller
   owns history, budget, and calibration. The engine is driven entirely through
   **10 injected trait ports** (`Provider`, `ToolExecutor`, `Clock`, `TurnGate`,
   `TurnSteering`, `Sleeper`, `HookRunner`, `RuleSource`, `SkillSource`,
   `ToolCallObserver`) with zero process-global state.

2. **`AgentEvent` is already the wire format.** `stella-protocol/src/event.rs`
   defines a ~30-variant, `#[serde(tag = "type", rename_all = "snake_case")]`
   enum. The `--output-format stream-json` mode is literally
   `serde_json::to_string(&event)` per line, additive-only, with round-trip
   tests. The TUI consumes *only* `AgentEvent` (`stella-tui` depends on nothing
   but `stella-protocol`). **A web client is a drop-in peer of the TUI**: attach
   an SSE pump to the same `UnboundedReceiver<AgentEvent>`.

3. **The pipeline is headless-first.** `stella-pipeline` has no TTY coupling in
   its core; `PipelineConfig.headless` and `headless_bypass_scope_review` are
   first-class, and a headless scope-review over threshold returns the named
   error `ScopeReviewRequiredHeadless` — **never a silent auto-approve**.
   `AutoApproveGate` / `AlwaysAbortGate` are the headless approval ports.

4. **Multi-workspace in one process already works.** `stella-fleet` runs N
   workers concurrently in one process, each with `cfg.workspace_root` overridden
   per task; nothing below `Config::load` reads `current_dir()`. The three
   SQLite stores (`store.db`, `context.db`, `codegraph.db`) are all path-injected
   (`Store::open(root)`, `ContextStore::open(path)`, `CodeGraph::open(root,
   db_path)`) with WAL + `busy_timeout=5000`.

5. **The Observatory is the in-repo HTTP precedent.** `stella-observatory`
   serves a loopback-only, read-only dashboard over a hand-rolled HTTP/1.1
   responder on `tokio::net::TcpListener` (no axum/hyper). `respond(root, path)`
   is a pure function unit-tested with no sockets. `stella-serve` mirrors these
   idioms and adds the write path.

### The three real gaps (what "prepare it to be the engine" actually means)

| Gap | Evidence | Fixed by |
|---|---|---|
| **Bin-only crate.** `stella-cli` has no `[lib]`; nothing is callable externally. The engine *wiring* (provider build, tool registry, store, MCP, prompt, budget) is duplicated across 5–6 drivers in the bin. | `stella-cli/Cargo.toml` (no `[lib]`); `agent.rs`, `agent/goal.rs`, `command_deck.rs`, `fleet_cmd.rs` each re-assemble the stack. | **`stella-runtime`** — extract the wiring into one reusable builder (Step 0). |
| **Whole-loop API only.** `Engine::run_turn` owns the entire step loop; there is no step-scoped entry to checkpoint or cancel between committed steps from outside. | `driver.rs:329`; phase functions are already separate. | **`stella-engine`** facade — `run_step(&mut TurnState) -> StepOutcome` (extraction, not redesign). |
| **No transport / no server.** The event channel and control ports exist but are only wired to stdin/TUI. No process hosts them over a socket; no graceful shutdown. | Observatory is read-only; `run_turn` future is `!Send`. | **`stella-serve`** — HTTP/SSE + reverse-tool-RPC sidecar, thread-per-session. |

## The `!Send` constraint drives the server shape

Three independent sweeps flagged it: the engine turn future is **deliberately
`!Send`** (it holds provider futures and the retry-jitter RNG across awaits) —
documented at `stella-cli/src/fleet_cmd.rs:375-380`. **A server cannot
`tokio::spawn(engine.run_turn())`** on a multi-thread runtime. The fleet already
solved this: each worker gets a **dedicated OS thread running a current-thread
tokio runtime**, bridged to the async side by a `Send` oneshot
(`fleet_cmd.rs:388-405`). `stella-serve` adopts the same pattern: **one OS thread
+ current-thread runtime per session**, the accept loop and SSE pumps on the main
multi-thread runtime, sessions addressed by id.

## Architecture

```
┌────────────────────────── stella-serve (new crate, the sidecar) ──────────────────────────┐
│  tokio multi-thread runtime: TcpListener accept loop (Observatory idiom + write path)      │
│  bearer-token auth · bind 0.0.0.0:PORT (containerized) or 127.0.0.1 (local)                 │
│                                                                                            │
│  POST /v1/sessions            → create Session { id, workspace_root, provider cfg, ... }    │
│  POST /v1/sessions/:id/turn   → drive one turn/pipeline; returns run id                     │
│  GET  /v1/sessions/:id/events → SSE stream of AgentEvent (resumable via ?after=<seq>)       │
│  POST /v1/sessions/:id/steer  → TurnSteering::drain_steering  (mid-turn message)            │
│  POST /v1/sessions/:id/pause  → TurnGate::wait_if_paused                                     │
│  POST /v1/sessions/:id/cancel → soft-stop (keep work) | hard-cancel (drop future)           │
│  POST /v1/sessions/:id/tool-result → reverse-RPC: host returns a ToolResult by call_id      │
│  POST /v1/sessions/:id/approval    → reverse-RPC: host resolves a scope/approval by id      │
│  DELETE /v1/sessions/:id      → tear down thread + runtime + stores                          │
│  GET  /healthz  /readyz  /metrics                                                           │
│                                                                                            │
│  per session:  std::thread + current-thread runtime  ── drives ──►  stella-engine.run_step  │
└──────────────────────────────────────────────┬─────────────────────────────────────────────┘
                                               │  10 trait ports, but remoted:
                     ┌─────────────────────────┼──────────────────────────────┐
                     ▼                         ▼                              ▼
             RemoteProvider            RemoteToolExecutor            RemoteApprovalGate
        (Provider port → the host    (ToolExecutor port → each      (ApprovalGate →
         streams model deltas back    tool call becomes a           a scope-review
         over the events channel;     `tool_call` AgentEvent; the   AgentEvent; the host
         the host owns @oxagen/ai)    host runs kernel.invoke()      resolves via approval
                                      and POSTs the ToolResult)      rows + POSTs back)
```

The engine keeps its full local port set when run as the CLI. In the serve
sidecar, the ports that must stay governed by Oxagen (model calls, tool
execution, approvals, recall, command runs) become **remote ports**: the engine
emits a request as an `AgentEvent`, blocks that step's tool/model future on a
oneshot, and the host fulfills it over a reverse endpoint keyed by `call_id`.
This is exactly the "reverse tool-call protocol" ADR-033 Option B names, and it
is why the sovereignty rule holds: **the engine never gains ambient authority —
every effect re-enters `kernel.invoke()` on the host.**

### The step-scoped facade (`stella-engine`)

```rust
// stella-engine/src/lib.rs (facade over stella-core + stella-pipeline)
pub struct TurnState { /* messages, budget, oracle_state, calibration, seq */ }

impl Engine {
    pub fn new_turn(&self, spec: TurnSpec, resume: Option<Checkpoint>) -> TurnState;
    pub async fn run_step(&self, state: &mut TurnState) -> Result<StepOutcome, EngineError>;
    //                                                     ^ one committed step, then return
}

pub enum StepOutcome { Continue, Done { text, cost_usd }, Aborted { reason } }
```

`run_step` is an **extraction** of the body of `driver.rs`'s `for step in
0..max_steps` loop — the phase functions (compaction, loop-detect, budget check,
model call, dispatch) are already separate. After each `run_step` the host
persists `(messages_digest, budget_state, oracle_state, calibration_state)` +
the event seq in one transaction, giving Oxagen's durable runner its
per-step checkpoint and crash-resume. This is ADR-033 §6 item 1 and §4.3.

### Wire protocol

- **Events (engine → host):** newline-delimited `AgentEvent` JSON over SSE,
  identical to `stream-json`. Each carries a monotonic `seq`; the SSE endpoint
  replays from `?after=<seq>` so a reconnect resumes losslessly (mirrors the
  Observatory's read model and Oxagen's `agent_events` log discipline).
- **Reverse RPC (host → engine):** the engine surfaces a `tool_start` /
  `scope_review` / `ask_user` `AgentEvent` carrying a `call_id`; the host runs
  the governed work and POSTs the result back to
  `/v1/sessions/:id/{tool-result,approval}` with that `call_id`. The engine's
  `RemoteToolExecutor::execute` awaits a per-`call_id` oneshot.
- **Provider deltas:** `RemoteProvider::complete_observed` forwards `text_delta`
  / `tool_call_streamed` as `AgentEvent`s so the browser gets token-level
  streaming (Anthropic + all OpenAI-compatible adapters already emit these;
  OpenAI/Gemini/Vertex/Bedrock need `complete_observed` overrides — a small,
  scoped addition at their existing SSE delta-parse sites).

## Containment posture (why the sidecar runs *inside* Oxagen's sandbox)

The tools sweep found that turning `tools.bash: off` does **not** remove
arbitrary shell execution — `build_project`, `run_tests`, `verify_done`, and
`run_script` all shell out via `bash -c`, and the built-in OS sandbox
(`STELLA_BASH_SANDBOX`) covers only the `bash` tool. The web tools are an
unguarded SSRF primitive when enabled. Several credentials/config knobs are
process-global (sandbox mode, web auth, provider keys), so **multi-tenant in one
process is a non-starter.**

Therefore the serve model is **one engine process per trust boundary, run inside
Oxagen's existing Firecracker/Modal sandbox** (the same isolation
`agent.code.execute` and durable sandbox sessions already use). The engine's
`CommandRunner`/`ToolExecutor` ports resolve to the host's governed sandbox exec
— the engine does not spawn its own shells server-side. `stella-serve` in server
mode:

- **binds to a token-gated port only** (no ambient trust); reuses the
  Observatory's DNS-rebinding `Host`-header guard.
- **disables the local shell/web tool surface by default** (`--tools remote`),
  delegating all execution to the host's `RemoteToolExecutor`.
- **does not use `stella-store` or `stella-cli` shell hooks server-side** (per
  ADR-033 §4.1) — persistence and policy are the platform's.
- adds the **graceful shutdown** the CLI lacks: SIGTERM drains in-flight turns to
  the next step boundary (soft-stop), then exits; a per-session lifecycle tears
  down cleanly (the CLI only has SIGPIPE + TUI-drop cleanup).

## Metering parity

The host owns metering (it implements the `Provider` port over
`@oxagen/ai::streamAgentReply`, which writes `token_usage` rows to ClickHouse).
The engine still emits `StepUsage` / `BudgetTick` / `Complete` events carrying
`{input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
cost_usd, model, duration_ms}` for cross-check. Because tool/command execution
runs on the host sandbox, the host also emits the sandbox-runtime cost event —
so both the per-step model cost and the compute cost are priced (the gap
ADR-033 §7 names).

## Upstream Stella work items (shared by Option A and B; ADR-033 §6)

1. `stella-runtime` — extract the CLI's engine wiring into one reusable builder
   (Step 0; unblocks everything).
2. `stella-engine` facade + `run_step` state-scoped API + serializable
   `Checkpoint` (external checkpointing).
3. `stella-serve` sidecar crate — HTTP/SSE + reverse-tool-RPC, thread-per-session,
   graceful shutdown, bearer auth.
4. Serde-stable `AgentEvent` → TS codegen (ts-rs/schemars → JSON Schema → zod) so
   the host consumes typed events; wire `replay.rs::validate_stream` as a CI
   conformance gate.
5. Host-emitted bus lifecycle events (`emit_named` helpers) — closes "the bus is
   only emitted from the tool registry."
6. A real cancellation token threaded through `run_step` (retire the dead
   `ProviderError::Cancelled`), plus documented hard-drop semantics.
7. `complete_observed` overrides for OpenAI/Gemini/Vertex/Bedrock adapters so
   token streaming is uniform across providers.
8. Per-session isolation audit of `stella-tools` process-global state
   (file-touch mutex, `STELLA_*` env reads) — server mode must inject these, not
   read the environment.

## Non-goals

- Not re-implementing isolation inside Stella — reuse Oxagen's sandbox.
- Not persisting turns in `stella-store` server-side — Oxagen owns durability.
- Not exposing the local shell/web tools to tenants — delegate to `RemoteToolExecutor`.
- Not gating the payoff on Rust: the host-side durable runner (ADR-033 Track 1)
  lands independently and this sidecar swaps in at the `executeTurn` seam.

## Build decomposition

See `serve-surface.fleet.toml` (this directory) — a `stella fleet --plan` file
that fans the eight upstream items into gate-verified tasks, each of which passes
`make gate` (fmt + file-size ratchet + clippy `-D warnings` + `cargo test`)
before its commit lands.
