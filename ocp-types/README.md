# ocp-types

The wire types for the **Open Context Protocol (OCP)**: context frames,
queries, capabilities, and provenance.

`ocp-types` is the industry-facing artifact of the OCP crates: **MIT
licensed, zero dependencies beyond `serde`.** You can implement an OCP
provider or host in Rust against this crate alone, with no dependency on
[Stella](https://github.com/macanderson/stella) or any of its other crates.

Protocol version: `ocp/1.0-draft` (see [`stability.md`][stability] for what
that means for this crate's semver).

## What's in here

- [`ContextFrame`], [`FrameKind`], [`Provenance`], [`Relation`] — the unit of
  exchange a provider returns from a query: budgeted, scored, and
  provenance-carrying, so a host can compose and cite it honestly.
- [`ContextQuery`], [`ContextQueryResult`] — the retrieval request/response
  shape, budget-aware by construction (`max_tokens`, `max_frames`).
- [`Capabilities`], [`ProviderInfo`], [`DataFlow`] — the handshake shapes: what
  a provider can do, and what it does with your data (`reads` / `writes` /
  `egress`).

## Example

```rust
use ocp_types::{ContextFrame, FrameKind};

let frame = ContextFrame {
    id: "frm_1".into(),
    kind: FrameKind::Doc,
    title: "Getting Started".into(),
    content: "Install with `cargo add ocp-types`.".into(),
    uri: Some("file:///docs/getting-started.md".into()),
    score: 0.82,
    token_cost: 64,
    valid_from: None,
    valid_to: None,
    recorded_at: None,
    provenance: vec![],
    citation_label: Some("getting-started.md L1-40".into()),
    embedding: None,
    relations: vec![],
};

assert!(frame.has_valid_score());
let json = serde_json::to_string(&frame)?;
# Ok::<(), serde_json::Error>(())
```

Every type here round-trips through `serde_json` — that JSON shape *is* the
protocol; there is no separate IDL.

## Building on this crate

- [`ocp-host`](https://crates.io/crates/ocp-host) — a host runtime (provider
  discovery, stdio/HTTP transports, consent gating, fan-out routing) built on
  these types, for anyone who wants a ready-made OCP host rather than hand-
  rolling the wire protocol.
- [`ocp-conformance`](https://crates.io/crates/ocp-conformance) — the public
  conformance suite. Green on it is what "OCP conformant" means for your
  declared capability set.

## Docs

- [Protocol surface][protocol-surface] — the full wire shape, field by field.
- [Implementing a provider][implementing] — how to build an OCP provider
  against `ocp-types` (with or without `ocp-host`).
- [Running conformance][conformance] — proving your provider is conformant.
- [Stability][stability] — the crate-semver vs. protocol-version relationship.

[protocol-surface]: https://github.com/macanderson/stella/blob/main/docs/ocp/protocol-surface.md
[implementing]: https://github.com/macanderson/stella/blob/main/docs/ocp/implementing-a-provider.md
[conformance]: https://github.com/macanderson/stella/blob/main/docs/ocp/running-conformance.md
[stability]: https://github.com/macanderson/stella/blob/main/docs/ocp/stability.md
[`ContextFrame`]: https://docs.rs/ocp-types/latest/ocp_types/frame/struct.ContextFrame.html
[`FrameKind`]: https://docs.rs/ocp-types/latest/ocp_types/frame/enum.FrameKind.html
[`Provenance`]: https://docs.rs/ocp-types/latest/ocp_types/frame/struct.Provenance.html
[`Relation`]: https://docs.rs/ocp-types/latest/ocp_types/frame/struct.Relation.html
[`ContextQuery`]: https://docs.rs/ocp-types/latest/ocp_types/query/struct.ContextQuery.html
[`ContextQueryResult`]: https://docs.rs/ocp-types/latest/ocp_types/query/struct.ContextQueryResult.html
[`Capabilities`]: https://docs.rs/ocp-types/latest/ocp_types/capability/struct.Capabilities.html
[`ProviderInfo`]: https://docs.rs/ocp-types/latest/ocp_types/capability/struct.ProviderInfo.html
[`DataFlow`]: https://docs.rs/ocp-types/latest/ocp_types/capability/struct.DataFlow.html

## License

MIT — see [`LICENSE-MIT`](https://github.com/macanderson/stella/blob/main/LICENSE-MIT)
in the workspace root.
