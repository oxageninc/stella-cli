# Research papers

Original analysis on Stella's architecture, the Open Context Protocol, and
the defensible properties of deterministic coding-agent design. These are
research-grade documents — written for engineers and architecture reviewers,
grounded in primary research, and referencing the shipping implementation by
file and line.

## Papers

- [**Stella: A Defensible Technology Position**](./stella-defensible-position.md)
  — the capstone analysis. Identifies seven architectural invariants that make
  Stella's design expensive to replicate and shows why their *combination* —
  not any single property — constitutes the moat. Covers ports-not-concretions,
  no-I/O-in-the-engine, the witness-test contract, BYOK + no-phone-home,
  prompt-cache-native memory, budget enforcement at safe boundaries, and the
  Open Context Protocol.

- [**The Deterministic Engine: Why Single-Thread Beats the Swarm**](./deterministic-engine.md)
  — a focused analysis of one defensible property: Stella's decision to build
  a single-thread deterministic engine rather than a multi-agent swarm. Draws
  on the MAST (Multi-Agent System Failure Taxonomy) findings from UC Berkeley
  and the success of Agentless and SWE-agent to argue that determinism is a
  feature, not a limitation.

## Related

- [**The Open Context Protocol: Advantages and Uniqueness**](https://github.com/macanderson/opencontextprotocol/blob/main/docs/protocol-advantages.md)
  — standalone analysis of the OCP's trust architecture: the seven advantages
  (provenance, budget honesty, consent enforcement, conformance verification,
  citation guarantees, version stability, temporal validity) and why the
  combination is irreducible.

---

*Every claim in these papers is grounded in the shipping implementation. When
a paper references a property, it links to the code that enforces it.*
