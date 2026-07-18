# The Open Context Protocol: A Technical Overview

> A marketing overview written for engineers. If you want the deep research
> analysis, read [Advantages and Uniqueness](./protocol-advantages.md). If you
> want to build a provider today, read
> [Implementing a provider](./implementing-a-provider.md). This page is the
> one-read explanation of what OCP is, why it exists, and why you would build
> against it.

---

## The problem: your agent's context is an unaccountable blob-pipe

Every AI coding agent retrieves context. It runs a vector search, a grep, or a
symbol lookup, and it pastes the result into the prompt. Then five questions
have no answer:

- **How many tokens did that cost?** The agent guesses, or ignores it.
- **Where did it come from?** A file path, if you are lucky. A digest you can
  verify against disk, almost never.
- **Did it leave the machine?** If retrieval called a cloud embedding API, your
  workspace content was transmitted. There is no record that you agreed.
- **Can the agent cite it?** Only if it reconstructs a label from raw metadata.
- **Is it still true?** A snippet from a stale index may not match the file on
  disk anymore.

This is the blob-pipe: retrieval as an opaque firehose of text that nobody can
account for. It works until the budget silently overflows, a provider lies about
cost, workspace content leaks to a third party, or an auditor asks "where did
this answer come from?" and there is no trail.

The Open Context Protocol (OCP) makes every one of those questions answerable.
Not by convention, but by contract.

---

## What OCP is, in one paragraph

OCP is an open wire protocol for context retrieval. It treats a piece of context
as a typed, budgeted, provenance-carrying, consent-gated, and
conformance-verified unit of exchange called a **frame**. A host asks providers
for frames relevant to a goal, under a token budget. Each provider returns
frames that carry their own origin, their honest cost, and a human-readable
citation label. The host composes them into a prompt it can trust as evidence,
not accept on faith. The protocol is three Rust crates you can build against
today: [`ocp-types`](https://crates.io/crates/ocp-types) (the wire types, zero
dependencies beyond `serde`), [`ocp-host`](https://crates.io/crates/ocp-host)
(the host runtime), and
[`ocp-conformance`](https://crates.io/crates/ocp-conformance) (the machine-checked
conformance suite). All three are MIT licensed.

---

## The seven guarantees

OCP makes seven promises about every frame that enters a prompt. Each one is a
type in `ocp-types` and an enforcement path in `ocp-host` or `ocp-conformance`,
not a line in a style guide.

| Guarantee | What you get | Enforced by |
|---|---|---|
| **Provenance** | Every frame carries its origin: URI, line range, cryptographic digest, method, and the agent that produced it | `ContextFrame.provenance` |
| **Budget honesty** | A provider's frames never sum above the query's `max_tokens`. A provider that lies is detected and its frames are dropped, loudly | Host budget audit + `budget-honesty` conformance check |
| **Consent enforcement** | A provider that sends data off-machine is never queried until you record named, revocable consent. The query payload is not transmitted first | `ConsentStore` gate in `ocp-host` |
| **Conformance** | "OCP conformant" is a checkable claim, not a self-attestation. The suite is adversarial and ships a mode that trips every failure on purpose | `ocp-conformance`, 5 checks |
| **Citation** | Every frame has a non-empty title and citation label. Raw ids are never the on-screen identifier | `frame-validity` conformance check |
| **Version stability** | The protocol evolves inside a major family. The draft-to-stable freeze needs no flag day and breaks no deployed provider | `versions_compatible` in `ocp-host` |
| **Temporal validity** | Facts carry `valid_from` and `valid_to` windows. A query can pin retrieval to a point in time with `as_of` | `ContextFrame` temporal fields |

The properties compose, and the combination is the point. Provenance without
budget honesty means you can trace a frame but not control its cost. Budget
honesty without consent means costs are honest but data can still leak. Remove
any one and the trust model collapses back to the blob-pipe. That is why OCP is
specified as one integrated protocol, not a menu of options.

---

## The wire surface

Three shapes carry the whole protocol. Every one lives in `ocp-types`,
round-trips through `serde_json`, and is the protocol. There is no separate IDL.

**Capability (the handshake).** A provider says who it is and what it does with
data before a host sends it anything.

```rust
pub struct DataFlow {
    pub reads: bool,   // sees workspace content in query payloads
    pub writes: bool,  // persists writes
    pub egress: bool,  // sends anything off the local machine
}
```

`egress` is the security-critical field. A conforming host must not auto-enable a
provider that declares `egress: true`. It gates that provider behind explicit,
named, one-time consent. The HTTP transport goes further and treats every remote
provider as egress, so a provider cannot lie its way past the gate.

**Query (the request).** A retrieval request that always carries a budget.

```rust
pub struct ContextQuery {
    pub goal: String,
    pub query_text: Option<String>,
    pub kinds: Vec<FrameKind>,   // empty means "your best frames of any kind"
    pub anchors: Vec<String>,    // open files, mentioned symbols
    pub max_frames: u32,
    pub max_tokens: u32,         // a hard contract, not a hint
    pub as_of: Option<String>,   // pin retrieval to a point in time
    // ...
}
```

**Frame (the answer).** The unit of exchange. A frame is not a string. It is a
structured record of relevance, cost, provenance, and validity.

```rust
pub enum FrameKind { Snippet, Symbol, Fact, Doc, Memory, Episode, Graph }

pub struct ContextFrame {
    pub id: String,                  // stable, for dedup, never the on-screen label
    pub kind: FrameKind,
    pub title: String,               // human label, required
    pub content: String,             // untrusted data, host quotes it, never executes it
    pub score: f32,                  // relevance in [0, 1]
    pub token_cost: u32,             // honest, conformance-audited
    pub provenance: Vec<Provenance>,
    pub citation_label: Option<String>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    // ...
}
```

Frame content is transported as untrusted data. A conforming host delimits it as
quoted material and never treats it as instructions, the same way a mail client
separates a message body from its headers.

---

## How OCP relates to MCP

They are complementary, not competing. The Model Context Protocol (MCP) connects
**tools**: functions an agent calls to take an action. OCP connects **context**:
typed, budgeted, cited evidence a host composes into the prompt before the agent
acts. MCP has no budget-honesty contract, no egress consent gate, no provenance
chain, and no conformance suite, because those are outside its scope, not
deficiencies in it. An agent that needs both composes them. OCP frames feed the
prompt. MCP tools do the work.

---

## Why you would build against it

- **Any language, no lock-in.** `ocp-types` depends only on `serde`. The barrier
  to writing a provider is a JSON codec and the wire table. In-process, over
  stdio, or over HTTP.
- **Conformance is a test you run in CI.** Point `ocp-inspect` at your provider.
  Green means it works with any OCP host. A broken provider is caught at CI time,
  not at integration time. The suite ships a `--misbehave` mode that trips every
  check on purpose, so you know the checks are real.
- **Stability you can pin.** The protocol version is `ocp/1.0-draft`. Two versions
  interoperate when they share a major family, the part before the first dot. So
  `ocp/1.0-draft` and `ocp/1.0` both belong to family `ocp/1` and interoperate.
  When the draft freezes, every deployed provider keeps working. No flag day.

---

## Status

OCP is `ocp/1.0-draft` today. The wire types are stable enough to build against,
the host runtime enforces the guarantees, and the conformance suite verifies them.
The path from "open context as an idea" to "open context as a standard" is the
conformance suite: anyone can build a provider, anyone can verify it, and the
protocol evolves inside a stable family without breaking what is already
deployed.

Start with [Protocol surface](./protocol-surface.md) to read the types,
[Implementing a provider](./implementing-a-provider.md) to build one, and
[Running conformance](./running-conformance.md) to prove it.
