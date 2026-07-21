# Stella: A Defensible Technology Position

> **Research note.** This paper analyzes the structural and architectural
> properties that make the Stella CLI a uniquely defensible open-source coding
> agent. It is written for technical evaluators, architecture reviewers, and
> engineering leaders assessing the sustainability of an agent's design — not
> for feature comparison. Every claim is grounded in the shipping
> implementation; file and line references point to the code that enforces the
> property being described.
>
> **Reading order.** This is the capstone paper. For domain-specific depth,
> read alongside:
> - [The Open Context Protocol: Advantages and Uniqueness](https://github.com/macanderson/opencontextprotocol/blob/main/docs/protocol-advantages.md) — the
>   retrieval protocol's trust architecture.
> - [`stella-core/src/lib.rs`](../../stella-core/src/lib.rs) and
>   [`stella-core/src/driver.rs`](../../stella-core/src/driver.rs) — the engine.
> - [`stella-core/src/ports.rs`](../../stella-core/src/ports.rs) — the port
>   boundary.

---

## Abstract

The market for AI coding agents is converging on a shared surface: an LLM,
a tool-calling loop, a context window, and a CLI. On this surface, most agents
are functionally substitutable — a user can move from one to another with
marginal friction. This paper argues that Stella occupies a defensible position
not because of any single feature, but because of a set of **architectural
invariants** that are expensive to replicate, mutually reinforcing, and
grounded in primary research on software-engineering agents, context window
economics, and multi-agent system failure modes. We identify seven defensible
properties, analyze why each is hard to copy, and show why the *combination* —
not any individual property — constitutes the moat.

---

## Table of contents

1. [The convergence problem](#1-the-convergence-problem)
2. [The seven defensible properties](#2-the-seven-defensible-properties)
3. [Property I: Ports, not concretions — the adapter boundary](#3-property-i-ports-not-concretions--the-adapter-boundary)
4. [Property II: No I/O in the engine — decision logic is pure](#4-property-ii-no-io-in-the-engine--decision-logic-is-pure)
5. [Property III: The witness-test contract — verified done](#5-property-iii-the-witness-test-contract--verified-done)
6. [Property IV: BYOK + zero telemetry egress by default — the trust perimeter](#6-property-iv-byok--zero-telemetry-egress-by-default--the-trust-perimeter)
7. [Property V: Prompt-cache-native memory — the cost discipline](#7-property-v-prompt-cache-native-memory--the-cost-discipline)
8. [Property VI: Budget enforcement at safe boundaries](#8-property-vi-budget-enforcement-at-safe-boundaries)
9. [Property VII: The Open Context Protocol — an open standard](#9-property-vii-the-open-context-protocol--an-open-standard)
10. [Why the combination is the moat](#10-why-the-combination-is-the-moat)
11. [Competitive analysis](#11-competitive-analysis)
12. [Threats to defensibility](#12-threats-to-defensibility)
13. [Conclusion](#13-conclusion)

---

## 1. The convergence problem

Consider the architecture of a typical coding agent in 2026:

```
User prompt → LLM (tool-calling) → tool execution → observe → repeat
```

This loop is now a commodity. OpenAI, Anthropic, Google, and a dozen
open-source projects implement some variant of it. The model does the heavy
lifting; the harness is plumbing. On this surface, differentiation is
ephemeral: a better prompt, a slicker UI, a faster streaming parser — all
copyable in a sprint.

The question this paper addresses is not "what features does Stella have?" but
"what structural properties of Stella's design are *expensive to replicate*,
and why?"

A defensible technology position requires properties that satisfy three
criteria:

1. **Hard to copy.** The property requires architectural decisions that are
   costly to retrofit. It is not a feature; it is a constraint that shapes
   every downstream decision.
2. **Mutually reinforcing.** Each property makes the others more valuable.
   Removing one degrades the others, so a competitor cannot cherry-pick.
3. **Grounded in research.** The property is not arbitrary; it is the
   consequence of a design principle that primary research validates.

We identify seven such properties in Stella's design.

---

## 2. The seven defensible properties

| # | Property | Core invariant | Enforced by |
|---|---|---|---|
| I | Ports, not concretions | The engine (`stella-core`) never imports a provider SDK, filesystem API, or terminal library | Crate-level dependency boundary; `Provider` trait (`stella-protocol`) and `ToolExecutor` trait (`stella-core::ports`) |
| II | No I/O in the engine | All decision logic is synchronous functions over owned data | Architectural discipline; property-tested in `stella-core` |
| III | Witness-test contract | A task is done only when a test fails on old code and passes on new | `verify_done` tool (`stella-tools::verify`) |
| IV | BYOK + zero telemetry egress by default | Community/default telemetry is local; only an explicitly enrolled Oxagen Enterprise managed deployment may send a signed-policy-authorized, content-free operational rollup | Architectural invariant; local SQLite (`stella-store`) plus the [managed enrollment boundary](../../stella-docs/content/docs/telemetry/index.mdx#oxagen-enterprise-managed-export) |
| V | Prompt-cache-native memory | Lessons load into a byte-stable system prompt prefix at ~0.1x input price | `build_system_prompt` (`stella-cli::agent`); L-E8 cache discipline |
| VI | Budget at safe boundaries | The budget guard consults only between model calls, never interrupts a tool | `run_turn` budget check (`stella-core::driver`); property-tested |
| VII | Open Context Protocol | Retrieval is a typed, budgeted, provenance-carrying, consent-gated, conformance-verified protocol | `ocp-types`, `ocp-host`, `ocp-conformance` |

Each property is examined below.

---

## 3. Property I: Ports, not concretions — the adapter boundary

### The invariant

`stella-core` — the engine that drives the agent loop (planning, tool
execution, compaction, loop detection, budget, retry, goal-mode judging) —
**never imports a provider SDK, a filesystem API, or a terminal library.** It
drives every external dependency through a trait:

- Model calls go through the `Provider` trait (`stella-protocol`).
- Tool execution goes through the `ToolExecutor` trait (`stella-core::ports`).
- Time goes through a `Clock` port (injectable for deterministic testing).
- Persistence goes through the store adapter (`stella-store`).

A new vendor, a new tool, or a new storage backend is an **adapter**, never a
rewrite of the engine. The engine is the same code whether it talks to
Anthropic, OpenAI, Gemini, Bedrock, a local Ollama server, or a custom
gateway.

### Why it is hard to copy

Most coding agents embed their provider integration directly in the agent
loop. The agent calls the OpenAI SDK, parses the response, decides what to do,
and calls the SDK again. This creates **vertical coupling**: changing the
provider means changing the loop. Over time, the loop accumulates
provider-specific logic (handling Anthropic's content blocks vs. OpenAI's
function calls vs. Gemini's function declarations), and the engine becomes a
tangle of adapter code interleaved with decision logic.

Stella's port boundary is a **constraint that shapes every downstream
decision.** Because `stella-core` cannot import an SDK, every provider-specific
detail is pushed into `stella-model`'s adapters. Because the engine is
SDK-free, it is trivially property-testable: the test harness injects mock
providers and tools, and the decision logic runs deterministically without any
I/O.

Retrofitting this boundary onto an existing agent is a **full rewrite of the
engine**, because the coupling is structural, not modular. You cannot extract
the adapter boundary from a loop that was never designed to have one; you have
to rebuild the loop around it.

### Research grounding

The ports-and-adapters pattern (hexagonal architecture, Cockburn 2005) is
well-established in software engineering. Its application to AI agents is less
common but directly motivated by the failure mode Cemri et al. (UC Berkeley)
identify in *Why Do Multi-Agent LLM Systems Fail?* (MAST, NeurIPS 2025):
multi-agent systems fail most often due to **inter-agent misalignment and
difficulty in verification** — problems that compound when the engine is
tightly coupled to its I/O surface. A pure, I/O-free engine is verifiable;
a coupled engine is not.

---

## 4. Property II: No I/O in the engine — decision logic is pure

### The invariant

All decision logic in `stella-core` — compaction, eviction, loop detection,
budget guard, retry strategy, skill selection, hook matching — is implemented
as **plain synchronous functions over owned data.** Nothing spawns a process,
reads a file, hits the network, or awaits an I/O future inside the engine.

### Why it is hard to copy

This is the property that makes `stella-core` **property-testable**. The
engine's compaction strategy, loop detection, budget enforcement, and retry
logic are exercised by `proptest` strategies that generate random
conversation histories, budgets, and retry scenarios. Regression seeds in
`stella-core/proptest-regressions/` capture failures and prevent regressions.

An engine with embedded I/O cannot be property-tested this way. You cannot
generate a random conversation history and run it through a loop that makes
real API calls — the cost is unbounded, the nondeterminism is unmanageable,
and the test would be an integration test, not a property test. The purity
constraint is what makes exhaustive testing of the decision logic tractable.

### The specific advantage

Consider **loop detection** (`stella-core::loop_detect`). This is the logic
that detects when an agent is stuck repeating the same actions. In Stella, it
is a pure function: given a conversation history, return whether a loop is
detected. This function is property-tested with strategies that generate
histories with and without loops, and the test verifies that detection is
monotonic, complete, and never false-positive on non-looping histories.

In a typical agent, loop detection is interleaved with I/O — it reads the
conversation from a store, checks the model's recent outputs, and may call an
API to summarize. It cannot be property-tested in isolation, so it is tested
only by integration runs, if at all. This means loop-detection bugs surface in
production, not in CI — and in production, they cost real money (the agent
loops on your API budget).

---

## 5. Property III: The witness-test contract — verified done

### The invariant

Stella refuses to call a task done until a **witness test** proves it: a test
that **fails on the old code** (the feature is genuinely absent) and **passes
on the new code** (the feature is genuinely present). This is enforced by the
`verify_done` tool (`stella-tools/src/verify.rs`).

The verification runs in a **detached shadow git worktree** at `HEAD`:

1. The agent writes the code change and a witness test in the working tree.
2. `verify_done` creates a shadow worktree at `git HEAD` (the pre-change
   state).
3. It copies *only the test files* from the working tree into the shadow.
4. It runs the suite in the shadow. The witness test must **fail** there
   (the feature is absent).
5. It runs the suite on the working tree. The witness test must **pass**
   (the feature is present).
6. Both conditions hold = `WITNESS CONFIRMED`. Either fails = the agent
   continues.

**The working tree is never mutated** — no stash, no checkout. The shadow
worktree is created and destroyed in isolation.

### Why it is hard to copy

The witness-test contract is not a feature; it is a **discipline**. Most agents
stop at "the test suite is green." But a green suite can hide:

- **Unwired features**: the code exists but is never called.
- **Vacuous tests**: the test passes trivially (e.g., `assert!(true)`).
- **Pre-existing green**: the suite was already green before the change; the
  change added nothing.

A witness test catches all three because it requires the test to *fail without
the change*. This is the difference between "the suite passes" and "the change
is proven necessary and sufficient for the test to pass."

Implementing this requires:

1. **Git worktree isolation** — the shadow worktree must be a true detached
   checkout, not a stash-and-revert that could mutate the working tree.
2. **Test-file extraction** — only the *new or modified test files* are copied
   into the shadow, not the entire working tree. This requires canonical
   root-relative path resolution (not the raw model-supplied string, which
   could be an absolute path that truncates the shadow copy).
3. **Timeout + process-group kill** — the shadow test run is bounded by
   `tokio::time::timeout` with process-group `SIGKILL` on expiry, so a hung
   test doesn't stall the agent.

Each of these is an engineering investment. Together, they represent a
**correctness contract** that is expensive to replicate because it requires
deep integration between the agent's definition of done and the project's test
infrastructure.

### Research grounding

The witness-test contract is directly motivated by SWE-bench (Jimenez et al.,
ICLR 2024), which evaluates agents on whether they can resolve real GitHub
issues — where "resolved" is defined by held-out tests that the agent cannot
see or author. Stella's witness test is the local, per-task analog: the agent
proves its own work by demonstrating the test's dependence on the change.

AutoCodeRover (Zhang et al., ISSTA 2024) uses a similar principle —
reproducing the bug before fixing it — as a correctness strategy. Stella
generalizes this: every task, not just bug fixes, is held to the witness
standard.

---

## 6. Property IV: BYOK + zero telemetry egress by default — the trust perimeter

### The invariant

Two architectural constraints, enforced together:

1. **BYOK (Bring Your Own Key):** Stella auto-detects the provider from
   whichever API keys the user has set. No account, no sign-up, no vendor
   lock-in. Nine providers plus any local server.
2. **Zero telemetry egress by default:** Community/default Stella sends no
   telemetry, update checks, or "anonymous" analytics. Events are recorded,
   best-effort, to a local SQLite file (`.stella/private/store.db`) — the store
   is never a dependency of a turn: a session runs even when it can't be
   opened, and an individual event write that fails degrades the local record
   rather than the run. The sole telemetry-egress exception is an
   [explicitly enrolled Oxagen Enterprise managed deployment](../../stella-docs/content/docs/telemetry/index.mdx#oxagen-enterprise-managed-export):
   a current signed org policy may authorize one minimal content-free
   operational rollup to one exact allowlisted HTTPS sink.

### Why it is hard to copy

These are **business-model constraints**, not technical features. Most
commercial coding agents are funded by a SaaS subscription or a metered API.
Their business model requires:

- Phone-home telemetry (to meter usage, track engagement, upsell features).
- Vendor lock-in (to keep users on the platform).
- Account-based access (to enforce subscription tiers).

Stella's Community/default no-telemetry-egress and BYOK constraints are
incompatible with this business model. A commercial agent *cannot* adopt the
default without abandoning telemetry-funded product assumptions. Oxagen
Enterprise preserves the boundary by requiring explicit managed enrollment,
signed process-free authority, a closed content-free schema, and one exact
sink rather than turning analytics on silently.

The enforcement is architectural, not policy-based. The `stella-core` engine
has no network code. Outbound HTTP is confined to a small, enumerable set of
crates, and every call targets an endpoint the user chose or configured:
`stella-model` (your model provider), `stella-mcp` (MCP servers you configure,
plus the MCP registry when you run `stella mcp search`), `stella-tools` (your
issue tracker — GitHub or Linear — only when you invoke the issue tools), and
`stella-media` (your image/video provider, only when you invoke the media
tools), and `stella-cli` only for the signed Oxagen Enterprise managed
operational sink described above. Community/default builds activate no
telemetry client or analytics endpoint, and there is no update checker.

### The specific advantage

For enterprise adoption, the trust perimeter is not a feature — it is a
**gate**. Community/default Stella satisfies a zero-telemetry-egress policy by
design. Organizations that deliberately permit operational egress can instead
audit and enroll the signed Oxagen Enterprise boundary rather than accepting
ambient analytics.

For individual developers using Community/default Stella, the trust perimeter
means **your telemetry stays on your machine.** The telemetry file is local
SQLite — query it, delete it, back it up, share it. Managed Enterprise
enrollment can export only the documented content-free operational fields,
never code, prompts, paths, tool payloads, reasoning, errors, memories, or
local identifiers.

---

## 7. Property V: Prompt-cache-native memory — the cost discipline

### The invariant

Lessons saved with `save_memory` (or written as markdown in
`.stella/memories/`) load once at session start into a **byte-stable system
prompt prefix**. Every model call in the session considers them at
prompt-cache-hit prices (~0.1x input token cost).

New memories take effect the *next* session by design: hot-injection would
invalidate the prompt cache on every save, converting a cache hit into a cache
miss and multiplying cost by 10x.

### Why it is hard to copy

Most agents treat memory as a dynamic injection — they retrieve relevant
memories per-turn and insert them into the prompt. This maximizes relevance
but destroys prompt-cache locality: every turn's prompt is different, so the
cache misses every time, and the full system prompt is re-billed at full
input price.

Stella's design is the opposite: memories are a **stable prefix**, loaded once
and never modified during the session. The recalled context (per-turn
retrieval from the context plane) rides as a **volatile message after the
stable prefix**, so the prefix stays cached and only the volatile tail is
re-billed.

The cost difference is structural. With prompt caching (Anthropic, 2024),
cache-read is approximately 0.1x the input token price. A system prompt of
10,000 tokens that is cache-hit on every call costs ~0.1x per call. The same
prompt without cache locality costs 1.0x per call — a 10x cost difference
that compounds across a long session.

Implementing this requires:

1. **Byte-stability discipline.** The system prompt prefix must be
   deterministic — same memories, same order (sorted by filename), same
   formatting, every time. Any nondeterminism (whitespace variation, order
   instability, dynamic content) invalidates the cache.
2. **Session-boundary loading.** Memories load once at session start, not
   per-turn. New memories wait for the next session.
3. **Volatile/context-stable separation.** Recalled context (which changes
   per-turn) must be in a separate message from the stable prefix, so the
   prefix's cache entry is not invalidated.

Each is an engineering constraint. Together, they represent a **cost
discipline** that is expensive to retrofit onto an agent that was designed
around dynamic memory injection.

### Research grounding

Anthropic's prompt caching documentation (2024) establishes the ~0.1x
cache-read price. MemGPT (Packer et al., UC Berkeley, 2023) proposes an
OS-like memory hierarchy (main context vs. external context) but does not
address prompt-cache locality. Stella's contribution is the recognition that
prompt-cache locality is not just a performance optimization — it is a
**cost architecture** that shapes how memory must be structured.

---

## 8. Property VI: Budget enforcement at safe boundaries

### The invariant

Stella enforces a hard `--budget` (in USD) that aborts the agent cleanly. The
critical constraint: **the budget guard consults only between model calls,
never interrupts a tool in flight.** A tool that is executing when the budget
is exhausted completes normally; the abort is acted on before the next model
call.

This is property-tested: a regression test proves that retry never re-executes
a tool call (`retry_with_backoff` wraps only `Provider::complete`, not tool
execution).

### Why it is hard to copy

Budget enforcement sounds simple until you consider the failure modes:

1. **Mid-tool interruption.** If the budget guard can interrupt a tool
   mid-execution, a `write_file` or `bash` command may be left half-complete,
   corrupting the workspace. The agent's "done" state is now undefined.
2. **Retry amplification.** If a retried step re-executes a tool call, a
   non-idempotent tool (e.g., `create_issue`) may execute twice. The budget
   leak is not just tokens — it is side effects.
3. **Budget lie from a provider.** If the model provider misreports token
   counts, the budget guard may think it has room when it doesn't.

Stella's design addresses all three:

1. **Safe-boundary abort.** The budget check happens in `run_turn` between
   model calls. A tool in flight is never interrupted. The abort is clean.
2. **No tool re-execution on retry.** `retry_with_backoff` wraps only the
   `Provider::complete` call. Tool execution happens exactly once, after a
   successful model call. A property test proves this.
3. **Per-call telemetry.** Each model call's token count is recorded from the
   provider's own response (not estimated), and cost is computed from the
   model card's pricing. The budget tracks real cost, not approximate cost.

Implementing safe-boundary budget enforcement requires the budget guard to be
**inside the engine's turn loop, not outside it.** An agent that wraps the
engine in a budget watcher (e.g., a sidecar that kills the process when spend
exceeds a threshold) cannot guarantee safe-boundary abort — it can only kill
mid-flight. Stella's budget guard is integral to the turn loop, which means
the engine was designed around it.

### Research grounding

Kapoor et al. (Princeton, TMLR 2025, *AI Agents That Matter*) argue that cost
is a first-class metric for agent evaluation, and that systems which are more
accurate but more expensive are not necessarily better. Stella's budget
enforcement makes cost a first-class, hard-enforced constraint — not a
reported metric, but an abort condition.

---

## 9. Property VII: The Open Context Protocol — an open standard

### The invariant

Retrieval in Stella is designed as an open, versioned wire protocol (OCP,
`ocp/1.0-draft`): the `ocp-types` crate (zero dependencies beyond `serde`),
the `ocp-host` host runtime, and the `ocp-conformance` conformance suite. See
[The Open Context Protocol: Advantages and Uniqueness](https://github.com/macanderson/opencontextprotocol/blob/main/docs/protocol-advantages.md) for the
full analysis.

### Why it is hard to copy

An open standard is defensible for the same reason TCP/IP is defensible: once
ecosystem participants build against the standard, switching costs become
prohibitive. OCP's specific defensibility comes from three properties:

1. **Zero-dependency wire types.** `ocp-types` depends only on `serde`. A
   third party can implement an OCP provider in any language without pulling
   in Stella code. The barrier to entry is a JSON codec and the wire table.

2. **Machine-checked conformance.** "OCP conformant" is defined as green on
   `ocp-conformance`'s suite — a checkable claim. This makes
   interoperability a testable property. A provider that passes conformance
   works with any OCP host; a provider that fails is caught at CI time, not
   at integration time.

3. **The trust architecture.** OCP's seven durability properties (provenance,
   budget honesty, consent, conformance, citation, version stability,
   temporal validity) are irreducible. A competitor proposing an alternative
   retrieval protocol must match all seven or accept a weaker trust model. See
   [The Open Context Protocol: Advantages and Uniqueness](https://github.com/macanderson/opencontextprotocol/blob/main/docs/protocol-advantages.md) §10 for
   why the combination is irreducible.

### The ecosystem play

OCP positions Stella not just as a coding agent but as the **reference
implementation of a standard.** If OCP becomes the standard for context
retrieval in AI coding tools — the way MCP is becoming the standard for tool
invocation — then Stella is the first and most mature host. Every OCP
provider built by the ecosystem (a Jira context provider, a Figma context
provider, a Confluence context provider) works with Stella for free. The
standard creates network effects that compound with each adoption.

---

## 10. Why the combination is the moat

Each property above is individually valuable. But the claim of this paper is
that **the combination is the moat**, not any single property. The properties
are mutually reinforcing:

- **Ports (I) + no I/O (II)** make the engine property-testable, which makes
  budget enforcement (VI) and loop detection reliable. Without the port
  boundary, the engine cannot be property-tested; without property testing,
  the budget guard's edge cases (retry re-execution, mid-tool abort) are
  unverified.

- **Witness-test (III) + budget (VI)** define a correctness *and* a cost
  contract. The witness test ensures the agent doesn't claim false completion;
  the budget guard ensures the agent doesn't spend unboundedly trying. Together,
  they define "done" as both *verified* and *bounded*.

- **BYOK + zero telemetry egress by default (IV) + OCP (VII)** define a trust
  perimeter. BYOK means the user controls the model; Community/default local
  telemetry plus the explicit signed Enterprise exception makes operational
  egress inspectable and governed; OCP means the user controls the context
  sources. The trust perimeter is not one property but three, each closing a
  gap the others don't address.

- **Prompt-cache-native memory (V) + budget (VI)** define a cost discipline.
  Memory is structured for cache locality (0.1x input price); budget is
  hard-enforced at safe boundaries. Together, they make Stella's
  cost-per-task competitive — not because the model is cheaper, but because
  the system around the model wastes less.

A competitor who copies one property (e.g., adds a budget flag) without the
others gets a fraction of the benefit. A competitor who copies all seven must
rebuild the architecture from scratch — because the properties are not
features bolted onto a loop; they are constraints that shape the loop's
design.

---

## 11. Competitive analysis

### vs. Claude Code (Anthropic)

Claude Code is a closed-source, Anthropic-locked CLI agent. It cannot match
Stella on:

- **BYOK**: Claude Code is locked to Anthropic models. Stella supports nine
  providers plus local.
- **Zero telemetry egress by default**: Claude Code is a client of Anthropic's
  platform; usage is metered server-side. Stella Community/default telemetry is
  local SQLite; signed Oxagen Enterprise enrollment is the sole governed
  operational exception.
- **Ports**: Claude Code's engine is not separable from its provider
  integration. Stella's engine is SDK-free.
- **Witness-test**: Claude Code does not have a verify_done-equivalent. It
  stops at "the suite is green."
- **OCP**: Claude Code does not expose a context-retrieval protocol. Its
  retrieval is opaque and vendor-locked.

Claude Code's advantage is deep integration with Anthropic's model
capabilities (extended thinking, artifact rendering, prompt caching). Stella
matches the prompt-cache advantage through its byte-stable memory discipline
and can use Anthropic models via BYOK, neutralizing the model advantage.

### vs. Aider

Aider is an open-source, BYOK coding agent with a strong repository-map
feature (tree-sitter-based). It is Stella's closest open-source peer.

- **Shared**: Both are open-source, BYOK, terminal-based.
- **Stella's unique advantages**: Witness-test contract, budget enforcement
  at safe boundaries, zero Community/default telemetry egress (Aider has
  optional analytics), the governed Oxagen Enterprise exception, the Open
  Context Protocol, and goal-mode with evidence-gathering judge.
- **Aider's unique advantages**: Mature git-integration workflow (edit,
  commit, undo), broader language support in repo-map, larger community.

Aider pioneered the tree-sitter repo map that Stella's `stella-graph` builds
on. The key differentiator is that Stella treats retrieval as a *protocol*
(OCP), not a *feature* — making it extensible by third parties and
conformance-verified.

### vs. Cursor / Windsurf (IDE-based agents)

Cursor and Windsurf are IDE-integrated agents with deep editor coupling. They
cannot match Stella on:

- **BYOK**: Cursor and Windsurf route through their own backend (or a
  metered BYOK path). Stella is pure BYOK.
- **Zero telemetry egress by default**: Both are cloud-connected products.
  Stella Community/default telemetry stays local; an explicitly enrolled
  Oxagen Enterprise seat has only the signed, content-free operational
  exception documented above.
- **Ports and I/O purity**: Both embed provider integration in the agent loop.
  Stella's engine is SDK-free and property-testable.
- **Witness-test, budget, OCP**: Neither has these properties.

Their advantage is IDE integration (inline diffs, multi-file editing,
language-server awareness). Stella operates in the terminal, which is a
trade-off: less editor polish, more scriptability and CI integrability.

---

## 12. Threats to defensibility

A rigorous analysis must consider what could erode Stella's position:

1. **Model convergence.** If a single model becomes so dominant that
   model-agnosticism stops mattering, BYOK loses its edge. *Mitigation:*
   Stella's other properties (zero telemetry egress by default, witness-test,
   budget, OCP) do not depend on model diversity.

2. **OCP non-adoption.** If the ecosystem does not build OCP providers, the
   protocol's network effects don't materialize, and OCP becomes an internal
   architecture rather than a standard. *Mitigation:* the zero-dependency wire
   types, the public conformance suite, and the MIT license lower the barrier
   to adoption. But adoption is not guaranteed.

3. **Feature parity from competitors.** A well-resourced competitor could
   implement budget flags, witness-test-like verification, and local
   telemetry. *Mitigation:* the properties are architecturally coupled. A
   competitor who adds a budget flag without restructuring the engine around
   safe-boundary abort gets a weaker version. The combination is harder to
   copy than the sum of its parts.

4. **Scope creep.** Wiring in the remaining crates (`stella-pipeline`,
   `stella-fleet`, `stella-tui`, `stella-graph` retrieval) is high-impact but
   high-effort. If these layers don't ship, Stella's architecture is
   partially realized. *Mitigation:* the shipping CLI already exercises the
   core path; the library crates are complete and property-tested.

---

## 13. Conclusion

Stella's defensible technology position is not a feature list. It is a set of
**architectural invariants** — ports not concretions, no I/O in the engine,
witness-test definition of done, BYOK with zero Community/default telemetry
egress and one explicit signed Oxagen Enterprise operational exception,
prompt-cache-native memory, budget enforcement at safe boundaries, and the Open
Context Protocol — each grounded in primary research, each expensive to
replicate, and each mutually reinforcing. A competitor who copies one property
gets a fraction of the benefit; a competitor who copies all seven must rebuild
the architecture from scratch. The combination, not any individual property,
is the moat.

The field manual (Anderson, 2026) articulates the theory: "the next leap in
AI coding isn't a bigger model, it's a better system around the model." Stella
is the running code that proves it.

---

### References

1. Cemri, M., et al. — *Why Do Multi-Agent LLM Systems Fail?* NeurIPS 2025.
   [arXiv:2503.13657](https://arxiv.org/abs/2503.13657)
2. Xia, C. S., et al. — *Agentless: Demystifying LLM-based Software Engineering
   Agents.* FSE 2025. [arXiv:2407.01489](https://arxiv.org/abs/2407.01489)
3. Jimenez, C. E., et al. — *SWE-bench: Can Language Models Resolve Real-World
   GitHub Issues?* ICLR 2024. [arXiv:2310.06770](https://arxiv.org/abs/2310.06770)
4. Yang, J., et al. — *SWE-agent: Agent-Computer Interfaces Enable Automated
   Software Engineering.* NeurIPS 2024.
   [arXiv:2405.15793](https://arxiv.org/abs/2405.15793)
5. Zhang, Y., et al. — *AutoCodeRover: Autonomous Program Improvement.* ISSTA
   2024. [arXiv:2404.05427](https://arxiv.org/abs/2404.05427)
6. Kapoor, S., et al. — *AI Agents That Matter.* TMLR 2025.
   [arXiv:2407.01502](https://arxiv.org/abs/2407.01502)
7. Liu, N. F., et al. — *Lost in the Middle: How Language Models Use Long
   Contexts.* TACL 2024. [arXiv:2307.03172](https://arxiv.org/abs/2307.03172)
8. Hong, G., et al. — *Context Rot: How Increasing Input Tokens Impacts LLM
   Performance.* Chroma, 2025.
   [research.trychroma.com](https://research.trychroma.com/context-rot)
9. Edge, D., et al. — *From Local to Global: A Graph RAG Approach.* Microsoft
   Research, 2024. [arXiv:2404.16130](https://arxiv.org/abs/2404.16130)
10. Packer, C., et al. — *MemGPT: Towards LLMs as Operating Systems.* UC
    Berkeley, 2023. [arXiv:2310.08560](https://arxiv.org/abs/2310.08560)
11. Cockburn, A. — *Hexagonal Architecture.* 2005.
12. Anthropic — *Prompt caching with Claude.* 2024.
    [anthropic.com](https://www.anthropic.com/news/prompt-caching)
13. Gauthier, P. — *Building a better repository map with tree-sitter.* Aider,
    2023. [aider.chat](https://aider.chat/2023/10/22/repomap.html)
14. Anderson, M. — *Engineering Deterministic AI Coding Agents — A Field Manual
    in 14 Parts.* Oxagen Inc., 2026.
    [oxagen.sh/#field-manual](https://oxagen.sh/#field-manual)

---

*See also: [The Open Context Protocol: Advantages and Uniqueness](https://github.com/macanderson/opencontextprotocol/blob/main/docs/protocol-advantages.md)
for the retrieval protocol's trust architecture, and the
[OCP reference docs](./README.md) for implementation guides.*
