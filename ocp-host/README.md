# ocp-host

The host runtime for the **Open Context Protocol (OCP)**: provider discovery,
stdio + streamable-HTTP transports, capability negotiation, budget-honest
fan-out routing, and egress consent gating.

An OCP **host** is the side of the protocol that asks for context. `ocp-host`
is a ready-made host you can embed in any Rust agent: register providers
(in-process, a child process over stdio, or a remote HTTP endpoint), fan a
query out to all of them concurrently, and get back frames that passed
consent, a timeout, and a budget-honesty audit — a provider that lies about
its `token_cost` has its frames dropped and reported, never silently trusted.

Depends only on [`ocp-types`](https://crates.io/crates/ocp-types) plus
ordinary async/transport crates (`tokio`, `reqwest`) — no dependency on
[Stella](https://github.com/macanderson/stella) or any of its other crates.

## Example: query a stdio provider

```rust,no_run
use ocp_host::Host;
use ocp_types::ContextQuery;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let mut host = Host::new();
host.add_stdio("docs", "ocp-example-docs", &[]).await?;

let query = ContextQuery {
    goal: "how do I configure it".into(),
    query_text: Some("configure".into()),
    embedding: None,
    kinds: vec![],
    anchors: vec![],
    max_frames: 8,
    max_tokens: 4096,
    as_of: None,
};

let fanout = host.query_all(&query).await;
for frame in fanout.accepted_frames() {
    println!("{} [{:.2}] {}", frame.citation_label.as_deref().unwrap_or(&frame.title), frame.score, frame.token_cost);
}
# Ok(())
# }
```

## Example: implement a provider

Any type that implements [`ContextProvider`] can be `host.register()`-ed as
an in-process provider — no child process or network hop required:

```rust
use async_trait::async_trait;
use ocp_host::{ContextProvider, HostError};
use ocp_types::{Capabilities, ContextQuery, ContextQueryResult, ProviderInfo};

struct MyProvider {
    info: ProviderInfo,
    capabilities: Capabilities,
}

#[async_trait]
impl ContextProvider for MyProvider {
    fn id(&self) -> &str { "my-provider" }
    fn info(&self) -> &ProviderInfo { &self.info }
    fn capabilities(&self) -> &Capabilities { &self.capabilities }

    async fn query(&self, query: &ContextQuery) -> Result<ContextQueryResult, HostError> {
        Ok(ContextQueryResult { frames: vec![], truncated: false, dropped_estimate: None })
    }
}
```

See [Implementing a provider][implementing] for the full guide, including the
stdio/HTTP transports (for providers written in *any* language, not just
Rust) and the consent-gating contract for `egress` providers.

## Docs

- [Protocol surface][protocol-surface] — the wire types this crate transports.
- [Implementing a provider][implementing] — the `ContextProvider` trait, the
  stdio/HTTP wire format, and the consent gate.
- [Running conformance][conformance] — proving your provider is conformant
  with `ocp-conformance`.
- [Stability][stability] — the crate-semver vs. protocol-version relationship.

[protocol-surface]: https://github.com/macanderson/stella/blob/main/docs/ocp/protocol-surface.md
[implementing]: https://github.com/macanderson/stella/blob/main/docs/ocp/implementing-a-provider.md
[conformance]: https://github.com/macanderson/stella/blob/main/docs/ocp/running-conformance.md
[stability]: https://github.com/macanderson/stella/blob/main/docs/ocp/stability.md
[`ContextProvider`]: https://docs.rs/ocp-host/latest/ocp_host/provider/trait.ContextProvider.html

## License

MIT OR Apache-2.0 — see [`LICENSE-MIT`](https://github.com/macanderson/stella/blob/main/LICENSE-MIT)
/ [`LICENSE-APACHE`](https://github.com/macanderson/stella/blob/main/LICENSE-APACHE)
in the workspace root.
