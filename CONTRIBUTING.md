# Contributing to Stella

```text
   ·  .  ✦   ·        ·   ✦        .   ·      ✦   .        ·
   verified done, not claimed done — and that includes your PR.
```

Thanks for wanting to make Stella better. This document is the whole game:
how to set up, where your change goes, what "done" means here, and how to
get it merged. It's long because it's honest — but the short version is:

> **Ship a witness test, keep the gates green, sign your commits. That's it.**

- [Ways to contribute](#ways-to-contribute)
- [Development setup](#development-setup)
- [Where does my change go? — a workspace tour](#where-does-my-change-go--a-workspace-tour)
- [The ground rules](#the-ground-rules)
- [The definition of done — witness tests](#the-definition-of-done--witness-tests)
- [Style](#style)
- [Commits, DCO, and PRs](#commits-dco-and-prs)
- [Issues and labels](#issues-and-labels)
- [Security](#security)
- [License](#license)

## Ways to contribute

Every one of these is genuinely valued — pick the one that fits your energy:

| Contribution | Where to start | Effort |
|---|---|---|
| 🐛 **A bug report with a repro** | [Bug report form](https://github.com/macanderson/stella/issues/new?template=bug_report.yml) | 10 minutes |
| 🧭 **Docs & examples** — fix a lie in the README before it fools someone else | Any `*.md` file, `--help` text, doc comments | Small |
| 🔌 **A new provider adapter** — Stella is BYOK; every model provider we speak makes it more useful | `stella-model/src/` — copy the shape of an existing adapter | Medium |
| 🛠 **A new built-in tool** | `stella-tools/src/` — implement the tool trait, register it | Medium |
| 🌐 **An OCP provider** — implement the Open Context Protocol in your language and prove it green | [macanderson/opencontextprotocol](https://github.com/macanderson/opencontextprotocol) — its own repo, no Stella code required | Medium |
| 🏗 **Core engine work** | `good first issue` / `help wanted` labels | Varies |

If you're not sure where something fits, open an issue first — a ten-line
sketch of the idea saves a thousand-line PR that can't merge.

## Development setup

**Prerequisites:** Rust **1.90+** via [rustup](https://rustup.rs) (the toolchain
is pinned in `rust-toolchain.toml`, so rustup will fetch the right one
automatically), `git`, and optionally [`ripgrep`](https://github.com/BurntSushi/ripgrep)
and [`fd`](https://github.com/sharkdp/fd) (the agent's `grep`/`glob` tools shell
out to them at runtime).

```bash
git clone https://github.com/macanderson/stella.git
cd stella

cargo build --workspace          # first build compiles bundled SQLite — quick
cargo test  --workspace          # the full suite
cargo run -p stella-cli -- models   # smoke-check your build
```

Iterating on a single crate is much faster than the whole workspace:

```bash
cargo test  -p stella-core       # just the engine
cargo clippy -p stella-tools --all-targets -- -D warnings
```

### The gate — run before every push

CI runs exactly this, and a red gate is an automatic "not yet":

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Where does my change go? — a workspace tour

Fourteen crates sounds like a lot; the rule of thumb is one sentence each:

| You want to… | Go to |
|---|---|
| Change how the agent loop plans / retries / compacts / budgets | `stella-core` (**no I/O allowed here** — see ground rules) |
| Add or fix a model provider (SSE, tool-call dialect, pricing) | `stella-model` |
| Add or fix a built-in tool (`read_file`, `bash`, `verify_done`, …) | `stella-tools` |
| Change a CLI command, flag, or the agent wiring | `stella-cli` |
| Change the REPL rendering / panels / keybindings | `stella-tui` |
| Touch shared types crossing a crate boundary | `stella-protocol` (zero logic, zero I/O — types only) |
| Persistence: executions, events, telemetry (SQLite) | `stella-store` |
| Retrieval: graph, embeddings, episodic memory | `stella-context` |
| Tree-sitter code indexing | `stella-graph` |
| The triage → … → judge orchestration plane | `stella-pipeline` |
| MCP client (external tool servers) | `stella-mcp` |
| Multimodal generation | `stella-media` |
| Multi-agent fan-out, worktree isolation | `stella-fleet` |
| The Observatory telemetry dashboard (`stella observe`) | `stella-observatory` |
| The Open Context Protocol (wire types / host / conformance) | external repo: [`opencontextprotocol`](https://github.com/macanderson/opencontextprotocol) |

All of the crates ship in the CLI today: `stella-pipeline` drives the default
`stella run` path, `stella-fleet` powers `stella fleet`, `stella-tui` is the
Command Deck (the default interactive shell on a TTY), and `stella-media`
provides image generation via the `generate_image` tool. The context/graph
plane is wired too — `stella init` builds the code-graph index and recall fans
out through the OCP host. See the **status table** in the
[README](README.md#workspace-layout).

## The ground rules

These are the architectural invariants the whole design hangs on. PRs that
break them will be asked to restructure, no matter how good the feature is:

1. **Ports, not concretions.** `stella-core` never imports a provider SDK, a
   filesystem API, or a terminal library. Models go through the `Provider`
   port, tools through `ToolExecutor`. A new vendor is an adapter, never a rewrite.
2. **No I/O in the engine.** Decision logic (compaction, eviction, loop
   detection, budget) stays synchronous functions over owned data — that's
   what makes it property-testable.
3. **No phone-home. Ever.** The only network traffic Stella may produce is to
   the model provider the user chose. A PR adding any other outbound call —
   telemetry, update checks, anything — will be rejected outright.
4. **Serde-first.** Every type crossing a crate boundary round-trips through
   `serde_json` byte-for-byte. Add a round-trip test when you add a type.
5. **Typed errors, no panics.** Library code returns typed, named errors —
   never a bare `String`, never `.unwrap()`/`.expect()` on runtime data
   (network payloads, tool arguments, and parsed source files are all runtime
   data). `unwrap` is fine in tests.
6. **Budget aborts at safe boundaries only** — never mid-tool.
7. **Byte-stable prompts.** Anything that feeds the system prompt must be
   deterministic — prompt-cache hits are a feature, and nondeterminism there
   is a cost regression.

## The definition of done — witness tests

Stella refuses to call a task done until a test **fails on the old code and
passes on the new** — and we hold contributions to the same contract, because
a merely-green suite can hide unwired features and vacuous tests.

For a behavior change or feature, your PR should include a **witness test**:

- it **fails** on `main` without your change (the feature is genuinely absent),
- it **passes** with your change (the feature is genuinely present).

You can check this the artisanal way (`git stash && cargo test -p <crate>`),
or let Stella verify Stella — build it and run your task through the
`verify_done` gate, which automates exactly this in a shadow worktree.

Pure refactors, docs, and CI changes don't need a witness — say so in the PR
template and move on. If a witness is genuinely impractical (e.g. TUI
rendering), explain how you verified the change instead.

## Style

- **`rustfmt` settles all formatting arguments** — default config, no debates.
- **Clippy at `-D warnings`** across all targets. Don't `#[allow]` your way
  past a lint without a comment saying why the lint is wrong here.
- **Name things for what they are**, not what they were. If you rename a
  concept, chase it through comments and docs in the same PR — stale comments
  are treated as bugs in review.
- **Doc comments on public items**, and on any function whose *why* isn't
  obvious from its body. No comments that narrate the next line.
- **No new dependencies casually.** Every new crate in `Cargo.toml` gets a
  sentence in the PR description justifying it. `ocp-types` stays
  dependency-light as a matter of contract.
- **Match the neighborhood.** Every crate has an established idiom — copy the
  patterns around you before inventing new ones.

## Commits, DCO, and PRs

**Commit format** — [Conventional Commits](https://www.conventionalcommits.org),
with the crate (or surface) as scope, matching the existing history:

```text
feat(stella-model): add mistral provider adapter
fix(stella-tui): restore terminal on panic in raw mode
docs(readme): correct provider table
ci(release): sign macOS binaries
```

**DCO, not CLA.** Sign every commit (`git commit -s`) to certify the
[Developer Certificate of Origin](https://developercertificate.org/). You keep
your copyright; no assignment, ever.

**PR checklist** (the template walks you through it):

1. One logical change per PR — smaller lands faster.
2. The gate is green locally (`fmt` / `clippy -D warnings` / `test`).
3. A witness test, or a stated reason there isn't one.
4. Docs updated in the same PR if behavior or flags changed (`README.md`,
   `--help` text, doc comments).
5. Commits signed off (`-s`).

Maintainers aim for a first response within a few days. "Needs work" is a
normal part of the loop here, not a rejection.

## Issues and labels

- **[Bug report](https://github.com/macanderson/stella/issues/new?template=bug_report.yml)** — include `stella --version`, OS, provider/model, and a repro.
- **[Feature request](https://github.com/macanderson/stella/issues/new?template=feature_request.yml)** — say what you're trying to do, not just what to add.

Labels you'll see: `area:*` routes an issue to a crate; `P0`–`P2` is priority;
`good first issue` and `help wanted` mean what they say; `needs-witness` means
a PR is waiting on its witness test.

## Security

Found a vulnerability? **Don't open a public issue.** See
[`SECURITY.md`](SECURITY.md) — we use GitHub's private vulnerability
reporting.

## License

Stella is dual-licensed **MIT OR Apache-2.0**. By contributing, you agree your
contributions are licensed under the same terms, as certified by your DCO
sign-off. No CLA, no copyright assignment.

```text
   ·  .  ✦   ·        see you in the diff.        ·   ✦  .  ·
```
