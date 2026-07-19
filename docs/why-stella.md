<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="../assets/stella-logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="../assets/stella-logo-light.svg">
    <img src="../assets/stella-logo-light.svg" alt="Stella" width="340">
  </picture>
</p>

<p align="center"><strong>Why Stella — a technical overview</strong></p>
<p align="center"><em>A fast, BYOK, model-agnostic terminal coding agent that proves its own work. Built in Rust.</em></p>

<p align="center">
  <img src="https://img.shields.io/badge/engine-zero--I%2FO%20core-FFAC26?style=flat-square" alt="Zero-I/O core">
  <img src="https://img.shields.io/badge/providers-9%20%2B%20local-FFAC26?style=flat-square" alt="9 providers + local">
  <img src="https://img.shields.io/badge/telemetry-local--only-FFAC26?style=flat-square" alt="Local-only telemetry">
  <img src="https://img.shields.io/badge/rust-1.90%2B-FFAC26?style=flat-square&logo=rust&logoColor=white" alt="Rust 1.90+">
</p>

---

Stella is an open-source coding agent that runs in your terminal. It is not a
SaaS, not a swarm, and not another wrapper that declares victory on a green test
suite. It is a single static Rust binary built around one uncommon idea: **an
agent should have to prove the change it made is the change that fixed the
problem** — and everything else in the design exists to make that proof cheap,
local, and reproducible.

## The guarantee other agents don't make

Most agents decide they are *done* when a test suite passes. That accepts two
failure modes silently: a suite that was already green, and an edit that doesn't
actually exercise the fix. Stella rejects both.

`verify_done` replays your **new** test files against the **previous** code in a
shadow worktree pinned at `git HEAD`. The test must **fail there** and **pass on
your change**. A green suite alone is never accepted — the fail → pass
transition *is* the evidence. When you don't hand it a test, the staged pipeline
(`stella run`, on by default) spawns an independent **witness author** that
writes the failing test, tamper-excluded from the code under change, so the flip
cannot be gamed. Deterministic definition of done, enforced by construction —
see [`pipeline.md`](design/pipeline.md).

## An engine you can actually reason about

`stella-core` performs **no I/O**. It drives every model call through a
`Provider` port and every tool through a `ToolExecutor` port, emitting an
`AgentEvent` stream over a channel. Compaction, eviction, loop detection,
routing, retry, and budget are plain **synchronous functions over owned data** —
so the whole decision core is property-testable with no network and no
filesystem, and adding a vendor or a tool is an *adapter, never a rewrite*. The
workspace is sixteen focused crates; `stella-protocol` is a zero-logic stability
contract every boundary round-trips through `serde_json` byte-for-byte. There is
one deterministic step loop — plan, fan tools out in parallel, observe, compact,
repeat — that you can read top to bottom. No coordinator, no hidden control
plane.

## Trust boundaries that are actually boundaries

| Property | How it works |
|---|---|
| **BYOK, model-agnostic** | Nine hosted providers (Anthropic, OpenAI, Gemini, xAI, DeepSeek, Z.ai, OpenRouter, Vertex, Bedrock) plus **any** OpenAI-compatible local server (Ollama, vLLM, LM Studio, llama.cpp). No account, no gateway. Pin per run with `--model provider/id`. |
| **No phone-home** | The only network traffic Stella emits goes to the provider *you* chose. Executions, the full event stream, per-call token/cost telemetry, and a `[C·R·U·D] path` files-touched ledger land in a local `.stella/store.db` you can open with any SQLite client — and the store is never a dependency of a turn. |
| **Budget you can trust** | `--budget <usd>` aborts cleanly **between** steps, never mid-tool, so a cap can't corrupt a half-written edit. |
| **Bounded blast radius** | File tools are workspace-root-pinned; the `bash` tool is **off by default** (settings `tools.bash: "on"` to opt in — the default surface is enumerable argv, no shell); an opt-in `bash` sandbox (Seatbelt / bubblewrap) contains prompt-injection damage and **fails closed**; a cloned repo's own hooks never auto-execute (`STELLA_PROJECT_HOOKS=1` to opt in). |

## Also in the box

An **offline tree-sitter code graph** queried instead of grepping (`stella
graph`, the `graph_query` tool; Rust/TS/JS/Python/SQL, no key needed) ·
**prompt-cache-native memory** that loads once into a byte-stable system prompt
at ~0.1× input cost · a **fleet mode** that fans a task DAG out to
git-worktree-isolated workers, wave-scheduled by dependency · **lifecycle
hooks** and an **MCP client** that merges external tools into the registry · and
the **Command Deck** TUI with PR-style diffs and an editable prompt queue. Deep
dives: [`hooks.md`](design/hooks.md), [`file-touch-telemetry.md`](design/file-touch-telemetry.md),
[`memory-citations.md`](design/memory-citations.md).

## What it optimizes for

Provable, reproducible progress over flashy autonomy. If you want an agent whose
every decision is a synchronous function you can test, whose telemetry never
leaves your disk, whose budget is a hard boundary, and whose "done" is a fact
you can re-run — Stella is built for you.

```bash
curl -fsSL https://raw.githubusercontent.com/macanderson/stella/main/install.sh | sh
export ANTHROPIC_API_KEY=…        # or OPENAI_API_KEY, GEMINI_API_KEY, a local server, …
stella run "fix the failing test in src/auth.rs"
```

<sub>Dual-licensed MIT OR Apache-2.0 · Rust 1.90+ · <a href="https://github.com/macanderson/stella">github.com/macanderson/stella</a></sub>
