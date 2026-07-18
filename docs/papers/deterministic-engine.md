# The Deterministic Engine: Why Single-Thread Beats the Swarm

> **Research note.** This paper is a focused analysis of one defensible
> property from the [capstone analysis](./stella-defensible-position.md):
> Stella's decision to build a **deterministic single-thread engine** rather
> than a multi-agent swarm. It is written for architects evaluating agent
> orchestration strategies and researchers studying multi-agent system failure
> modes.

---

## Abstract

The dominant trend in AI agent architecture is toward multi-agent systems —
swarms of specialized agents coordinated by a planner. This paper argues,
drawing on the MAST (Multi-Agent System Failure Taxonomy) findings from UC
Berkeley and the practical success of single-agent systems like Agentless and
SWE-agent, that **a deterministic single-thread engine is a superior
architectural choice for coding agents**, and that this choice is a defensible
property of Stella's design because it is expensive to reverse. We analyze the
failure modes that multi-agent systems exhibit, show how Stella's single-thread
engine avoids them by construction, and explain why the discipline of a
deterministic loop produces better outcomes than the flexibility of a swarm.

---

## Table of contents

1. [The multi-agent allure](#1-the-multi-agent-allure)
2. [What the research says: MAST](#2-what-the-research-says-mast)
3. [The single-thread alternative: Agentless and SWE-agent](#3-the-single-thread-alternative-agentless-and-swe-agent)
4. [Stella's engine: deterministic by construction](#4-stellas-engine-deterministic-by-construction)
5. [Why determinism is a feature, not a limitation](#5-why-determinism-is-a-feature-not-a-limitation)
6. [When multi-agent is the wrong answer](#6-when-multi-agent-is-the-wrong-answer)
7. [The deferred layer: stella-fleet](#7-the-deferred-layer-stella-fleet)
8. [Conclusion](#8-conclusion)

---

## 1. The multi-agent allure

The multi-agent pattern is intuitively appealing: divide a complex task into
subtasks, assign each to a specialized agent, and let a coordinator compose
the results. For software engineering, this might mean:

- A *navigator* agent that explores the codebase.
- A *coder* agent that writes the fix.
- A *reviewer* agent that checks the change.
- A *tester* agent that writes and runs tests.
- A *coordinator* that orchestrates the above.

The appeal is **division of labor** — each agent has a focused role, a
tailored prompt, and a smaller context window. Theoretical productivity seems
to scale with the number of agents.

In practice, the research tells a different story.

---

## 2. What the research says: MAST

Cemri et al. (UC Berkeley, NeurIPS 2025) conducted the first comprehensive
empirical study of multi-agent LLM system failures in their paper *Why Do
Multi-Agent LLM Systems Fail?* They developed the **Multi-Agent System Failure
Taxonomy (MAST)** by analyzing failures across multiple MAS frameworks
(AutoGen, MetaGPT, ChatDev, etc.) and identified three dominant failure
categories:

### 2.1 Inter-agent misalignment (the most common failure)

Agents in a multi-agent system frequently **disagree about the task state, the
goal, or each other's outputs.** The navigator's understanding of the codebase
diverges from the coder's. The reviewer's feedback contradicts the tester's
expectations. The coordinator's plan becomes stale as agents make progress
independently. This misalignment produces:

- Contradictory actions (one agent undoes another's work).
- Wasted effort (agents re-explore territory already covered by a peer).
- Cascading errors (one agent's mistake propagates through the coordinator to
  other agents).

### 2.2 Verification and validation difficulty

In a multi-agent system, **verifying that the system as a whole has reached
the correct state is harder than in a single-agent system.** Each agent has a
partial view. The coordinator must aggregate partial states into a global
assessment. There is no single thread of execution to replay or audit. When a
multi-agent system produces a wrong answer, diagnosing *which agent* went wrong
requires tracing inter-agent message logs — a significantly harder debugging
problem than tracing a single agent's step-by-step reasoning.

### 2.3 Information degradation

As information passes between agents, it degrades. The navigator's detailed
understanding of the codebase is summarized into a message to the coordinator,
which is summarized again into a directive to the coder. Each summarization
loses information. By the time the coder acts, the detailed structural
knowledge the navigator acquired is attenuated to a vague directive.

### The cost dimension

Beyond correctness, multi-agent systems are **more expensive.** Each agent
consumes tokens independently. The coordinator consumes tokens orchestrating.
Inter-agent messages consume tokens transmitting. Kapoor et al. (Princeton,
TMLR 2025) show that agent systems must be evaluated on cost, not just
accuracy — and multi-agent systems are systematically more expensive per task
than single-agent systems, often without commensurate accuracy gains.

---

## 3. The single-thread alternative: Agentless and SWE-agent

Two of the most successful systems on the SWE-bench benchmark are explicitly
**single-agent**:

### Agentless

Xia, Deng, Dunn, and Zhang (FSE 2025) designed *Agentless* as a deliberately
simple, single-agent approach to automated software engineering. Their key
finding: **a well-designed single-agent pipeline — localize, repair, verify —
outperforms multi-agent systems on SWE-bench at lower cost.** By avoiding the
coordinator tax and inter-agent misalignment, Agentless achieves strong results
with a fraction of the token budget.

Their argument is architectural: the complexity of multi-agent coordination is
a liability, not an asset. Each agent-to-agent message is an opportunity for
misalignment, and each coordinator decision is overhead. A single agent that
follows a well-designed pipeline has no coordination overhead and no
misalignment surface.

### SWE-agent

Yang, Jimenez et al. (NeurIPS 2024) showed that the **Agent-Computer
Interface (ACI)** — the design of the tools and observations the agent uses —
matters more than the number of agents. A single SWE-agent with a
well-designed ACI (simple, predictable tools with clear feedback) outperforms
systems with more agents but worse interfaces. The implication: investment in
the tool/observation interface pays off more than investment in multi-agent
coordination.

### The Cognition argument

Walden Yan (Cognition, 2025) made the practical case in *Don't Build
Multi-Agents*: multi-agent systems are hard to build correctly, hard to debug,
and hard to control. The complexity is rarely justified by the task. A single
deterministic agent with good tools is more reliable, more debuggable, and
cheaper to operate.

---

## 4. Stella's engine: deterministic by construction

Stella's engine (`stella-core`) is a single deterministic loop:

```
plan a step → execute tools (read-only concurrently, mutating in order)
            → observe → compact → loop-detect → check budget → repeat
```

There is no coordinator. There are no peer agents. There is one thread of
execution, one context window, one reasoning chain. The only parallelism is
**within a step**: read-only tools (file reads, grep, glob) execute
concurrently, while mutating tools (edit, write, bash) execute in call order
behind a barrier.

### Why this avoids every MAST failure mode

| MAST failure category | How Stella avoids it |
|---|---|
| **Inter-agent misalignment** | There is one agent. It cannot disagree with itself. Its understanding of the codebase, its plan, and its progress are in one context window — there is no inter-agent message to misinterpret. |
| **Verification difficulty** | There is one thread of execution. Every step is in the event stream. The verify_done witness test (Property III) runs against this single thread. Goal-mode judging inspects the same repository the worker modified. |
| **Information degradation** | There is no summarization between agents. The agent's full context — every file read, every command output, every compaction decision — is in one window. Information is lost only through explicit compaction (which is a deliberate, property-tested decision, not an accidental side effect of inter-agent messaging). |

### The parallelism that matters

Stella does parallelize where it is safe and beneficial:

- **Read-only tool concurrency.** When the model requests multiple read-only
  operations (read three files, grep for two patterns), they execute
  concurrently. This is safe because read-only tools have no side effects.
- **Context fan-out.** When the context plane queries multiple OCP providers,
  they are fanned out concurrently (`Host::query_all`). Each provider is
  isolated; one provider's failure never affects another.

This is **tool-level and retrieval-level parallelism**, not agent-level
parallelism. The decision logic — what to do next, whether to compact, whether
the budget allows another step — is strictly sequential. This is deliberate:
parallelism in decision logic is exactly what introduces misalignment.

---

## 5. Why determinism is a feature, not a limitation

The objection to a single-thread engine is: "what if the task is too complex
for one agent?" The research says this objection is rarely valid:

### 5.1 Complexity is in the tooling, not the agent count

SWE-agent's key finding is that **ACI quality matters more than agent count.**
A single agent with precise tools (read a specific line range, run a specific
test, verify a specific change) handles complex tasks better than multiple
agents with imprecise tools. Stella invests in ACI: workspace-root-pinned file
tools, exact-substring edits, a witness-test gate, an evidence-gathering judge.

### 5.2 Context compaction replaces context partitioning

The multi-agent argument for partitioning context (each agent gets a smaller
window) is addressed by **context compaction** within a single agent. Stella's
compaction (`stella-core::compaction`) keeps the signal and drops the noise —
property-tested to ensure compaction is monotonic (information is only removed,
never corrupted) and budget-aware (compaction triggers when the context
approaches the window limit). This achieves the same goal as context
partitioning (fitting within the window) without the misalignment cost.

### 5.3 Determinism enables property testing

The single-thread engine's decision logic is **pure functions over owned data**
(Property II). This means:

- Loop detection is property-tested (`loop_detect` strategies generate random
  histories and verify detection correctness).
- Compaction is property-tested (`compaction` strategies verify that compaction
  preserves essential information within a budget).
- Budget enforcement is property-tested (retry never re-executes a tool).
- Skill selection is property-tested (`skills` strategies verify selection is
  deterministic given the same inputs).

A multi-agent engine cannot be property-tested this way — the inter-agent
interactions are nondeterministic, and the state space is exponentially larger.
The determinism that makes Stella's engine simple is the same property that
makes it **verifiable.**

### 5.4 Reproducibility

A single-thread engine produces a **reproducible execution trace.** Every
event (model call, tool execution, compaction, budget check) is recorded in
the SQLite event stream. A failed run can be replayed: the same inputs produce
the same trace (modulo model nondeterminism, which is the model's
nondeterminism, not the engine's). This makes debugging tractable and makes
benchmarking meaningful — the harness is the variable, not the agent's
internal coordination chaos.

---

## 6. When multi-agent is the wrong answer

This paper does not claim that multi-agent systems are *never* useful. There
are domains where parallel exploration, independent verification, or
specialized role separation genuinely help. But for **coding agents operating
on a single repository with a single goal**, the research is clear:

- **The task is inherently sequential.** You cannot write a fix before you
  understand the bug. You cannot run a test before you write the fix. The
  pipeline is: localize → repair → verify. Parallelizing these stages produces
  stale or contradictory results.
- **The repository is shared state.** Multiple agents editing the same
  codebase need a coordination protocol (locking, merging, conflict
  resolution) that is itself a source of failures. A single agent needs none
  of this.
- **The verification is global.** "Is this task done?" is a question about the
  entire repository state (does the test pass, is the build green). A single
  agent can answer this directly. A multi-agent system must aggregate partial
  assessments, which is where MAST's verification-difficulty failure mode
  strikes.

The defensible insight is not "single-agent is always better" but "for the
specific domain of coding agents, single-agent determinism is a better
trade-off, and the research supports it."

---

## 7. The deferred layer: stella-fleet

Stella's workspace includes `stella-fleet` — a multi-agent fan-out system with
a DAG planner, git-worktree isolation per task, and a SQLite lineage + spend
ledger. It is complete and property-tested but **not wired into the CLI.**

This is not a contradiction. `stella-fleet` addresses a different problem:
**multi-task parallelism**, not multi-agent-per-task coordination. When you
have ten independent issues to resolve, `stella-fleet`'s DAG planner can assign
each to an isolated worktree, run them concurrently, and track lineage and
spend. Each task within the fleet is still solved by a **single deterministic
engine instance** — the fleet coordinates tasks, not agents within a task.

The architecture is:

```
stella-fleet (multi-task DAG)
  └── per-task worktree
       └── stella-core (single deterministic engine)
            └── Provider + ToolExecutor ports
```

This is the correct use of parallelism: at the task level, where tasks are
independent and the shared state (the repository) is isolated per worktree.
The single-thread engine remains the decision-maker within each task. The
fleet is a scheduler, not a coordinator.

---

## 8. Conclusion

Stella's single-thread deterministic engine is a defensible property not
because single-threadedness is inherently superior, but because **the research
on multi-agent failure modes (MAST), the success of single-agent systems
(Agentless, SWE-agent), and the engineering advantages of determinism
(property testability, reproducibility, no coordinator tax) all converge on
the same conclusion: for coding agents, a deterministic single-thread engine
is the better architecture.**

A competitor who chooses multi-agent pays the MAST tax — misalignment,
verification difficulty, information degradation — and gains flexibility that
the domain does not reward. A competitor who switches from multi-agent to
single-agent after launch faces a full engine rewrite, because the
coordination logic is structural, not modular.

Stella's engine is simple, deterministic, property-tested, and grounded in the
research. That combination is the moat.

---

### References

1. Cemri, M., et al. — *Why Do Multi-Agent LLM Systems Fail?* NeurIPS 2025.
   [arXiv:2503.13657](https://arxiv.org/abs/2503.13657)
2. Xia, C. S., et al. — *Agentless: Demystifying LLM-based Software Engineering
   Agents.* FSE 2025. [arXiv:2407.01489](https://arxiv.org/abs/2407.01489)
3. Yang, J., et al. — *SWE-agent: Agent-Computer Interfaces Enable Automated
   Software Engineering.* NeurIPS 2024.
   [arXiv:2405.15793](https://arxiv.org/abs/2405.15793)
4. Kapoor, S., et al. — *AI Agents That Matter.* TMLR 2025.
   [arXiv:2407.01502](https://arxiv.org/abs/2407.01502)
5. Yan, W. (Cognition) — *Don't Build Multi-Agents.* 2025.
   [cognition.ai](https://cognition.ai/blog/dont-build-multi-agents)
6. Anderson, M. — *Engineering Deterministic AI Coding Agents — A Field Manual
    in 14 Parts.* Oxagen Inc., 2026.
    [oxagen.sh/#field-manual](https://oxagen.sh/#field-manual)

---

*See also: [Stella: A Defensible Technology Position](./stella-defensible-position.md)
for the capstone analysis of all seven defensible properties.*
