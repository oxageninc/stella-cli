# OCP reference docs

Reference documentation for the **Open Context Protocol (OCP)** crates:
[`ocp-types`](https://crates.io/crates/ocp-types),
[`ocp-host`](https://crates.io/crates/ocp-host), and
[`ocp-conformance`](https://crates.io/crates/ocp-conformance).

- [**The Open Context Protocol: A Technical Overview**](./overview.md) — the
  one-read marketing overview for engineers: the problem OCP solves, the seven
  guarantees, the wire surface, how it relates to MCP, and why you would build
  against it. Start here if you are new to OCP.
- [**The Open Context Protocol: Advantages and Uniqueness**](./protocol-advantages.md)
  — standalone research analysis of the seven advantages that make OCP a
  qualitatively different approach to context retrieval (provenance, budget
  honesty, consent enforcement, conformance verification, citation guarantees,
  version stability, temporal validity), and why the combination is
  irreducible.
- [**Protocol surface**](./protocol-surface.md) — the wire types: context
  frames, queries, capabilities, provenance. Start here to understand *what*
  OCP is.
- [**Implementing a provider**](./implementing-a-provider.md) — how a third
  party builds an OCP provider, in Rust (via `ContextProvider`) or any other
  language (via the wire protocol directly). Start here to *build* something.
- [**Running conformance**](./running-conformance.md) — how to prove your
  provider (or host) is OCP conformant, via the `ocp-inspect` CLI or the
  `ocp-conformance` library. Start here to *verify* what you built.
- [**Stability**](./stability.md) — the crate-semver vs. protocol-version
  relationship, and what changes (and doesn't) as the protocol moves from
  `ocp/1.0-draft` to `ocp/1.0`.

See also [`PUBLISHING.md`](../../PUBLISHING.md) at the repo root for the
crates.io release process for these three crates specifically (distinct from
[`RELEASING.md`](../../RELEASING.md), which covers the `stella` binary
release).
