# AGENTS.md

Guidance for AI agents (and humans) working in this repository. This is a
condensed orientation focused on the non-obvious conventions and invariants
that aren't immediately apparent from reading a single file. The authoritative
sources for the details behind each section are `README.md` and `CONTRIBUTING.md`.

Stella is a fast, BYOK ("bring your own key"), model-agnostic terminal coding
agent, written in Rust. Its defining contract: a task is **done** only when a
**witness test** (a test that fails on the old code and passes on the new code)
proves it — "verified done, not claimed done." It is the open-source reference
implementation of Oxagen's *Engineering Deterministic AI Coding Agents* field
manual.

---

## Essential commands

The repo is a Cargo workspace. Rust is **pinned to a concrete version**
(currently 1.97.0) via `rust-toolchain.toml` (rustup fetches it automatically).
Floating on `channel = "stable"` was tried and reverted — each new stable
release ships a slightly different rustfmt, which silently reformats
previously-clean files and turns the CI fmt gate red with zero code changes.
When bumping the pin for a new Rust release, do it as one dedicated PR that
updates the version in `rust-toolchain.toml` and runs `cargo fmt --all` in the
same commit (or the next one) so drift never accumulates. A **`Makefile`**
wraps the common commands with the correct flags — run `make help` for the
full list.

```bash
make build               # cargo build --workspace
make test                # cargo test --workspace
make format              # cargo fmt
make lint                # cargo clippy --workspace --all-targets -- -D warnings
make smoke               # compile check — runs `stella models` (no API key needed)
make help                # list every target
```

**Iterate on a single crate** (much faster than the whole workspace):

```bash
make test-core           # or: cargo test -p stella-core
make test-model          # or: cargo test -p stella-model
make test-tools          # or: cargo test -p stella-tools
```

**Watch mode** (requires `cargo install cargo-watch`):

```bash
make watch               # re-run workspace tests on every save
make watch-core          # re-test stella-core only (fastest loop)
make watch-lint          # re-run clippy on every save
```

### The gate — run before every push

CI (`/.github/workflows/ci.yml`) runs exactly these three plus a release
smoke build (thin LTO) in the same required job, and a red gate is an
automatic "not yet":

```bash
make gate                # = fmt --check + clippy -D warnings + test --workspace
```

For a faster pre-push sanity check (no tests): `make check`.

**Run `make hooks` once per clone.** It installs a `pre-push` git hook
(`core.hooksPath=.githooks`) that runs `make gate` automatically on every push
and aborts the push if it fails. This is the gate's real enforcement point
today: the org's GitHub Actions is **billing-locked**, so the required CI checks
fast-fail in seconds without ever running, and with `enforce_admins` off,
gate-failing code otherwise slips onto `main` through admin/auto merges. The
hook catches it on the author's push instead. It is advisory (per-clone,
bypassable with `SKIP_GATE=1 git push` or `git push --no-verify`) — not a
server-side guarantee, which is impossible while the checks can't execute.

Supply-chain checks run as a separate CI job: `make supply-chain` (or
`cargo deny check advisories bans sources` + `cargo audit`). Note `deny.toml`
intentionally does **not** gate on licenses; advisories, bans, and source
provenance are the real gates.

---

## Architecture: ports, not concretions

The central architectural invariant. Every design decision in the codebase
flows from this. If a PR breaks one of these, it will be asked to restructure
regardless of how good the feature is:

1. **Ports, not concretions.** `stella-core` never imports a provider SDK, a
   filesystem API, or a terminal library. Models go through the `Provider`
   trait (`stella-protocol`), tools through `ToolExecutor` (`stella-core::ports`).
   A new vendor or tool is an adapter, never a rewrite.
2. **No I/O in the engine.** Decision logic (compaction, eviction, loop
   detection, budget, skill selection, hook matching) is plain synchronous
   functions over owned data inside `stella-core`. That's what makes it
   property-testable. Anything that spawns processes, reads files, or hits the
   network belongs in `stella-tools`, `stella-model`, `stella-cli`, or
   `stella-store` — injected as a port/trait, not called directly.
3. **No phone-home. Ever.** The only outbound network traffic Stella may
   produce is to the model provider the user chose. A PR adding any other
   outbound call (telemetry, update checks, "anonymous" analytics) is
   rejected outright.
4. **Serde-first.** Every type crossing a crate boundary round-trips through
   `serde_json` byte-for-byte. Add a round-trip test when you add a type to
   `stella-protocol`.
5. **Typed errors, no panics.** Library code returns typed, named errors —
   never a bare `String`, never `.unwrap()`/`.expect()` on runtime data
   (network payloads, tool arguments, parsed source files are all runtime
   data). `unwrap` is fine in tests.
6. **Budget aborts at safe boundaries only** — never mid-tool. `run_turn`
   consults the budget guard only between model calls, never interrupts a
   tool in flight.
7. **Byte-stable prompts.** Anything that feeds the system prompt must be
   deterministic — prompt-cache hits are a feature, and nondeterminism there
   is a cost regression. Memories are loaded once per session and concatenated
   in sorted filename order; recalled context rides as a volatile message
   *after* the stable prefix (see `stella-cli/src/agent.rs::build_system_prompt`
   and `stella-cli/src/memory.rs` for the L-E8 discipline).

---

## The definition of done: witness tests

Stella refuses to call a task done until a test **fails on the old code and
passes on the new** — and contributions are held to the same contract.

For a behavior change or feature, a PR should include a **witness test**:

- It **fails** on `main` without your change (the feature is genuinely absent).
- It **passes** with your change (the feature is genuinely present).

Check it the artisanal way (`git stash && cargo test -p <crate>`). Pure
refactors, docs, and CI changes don't need a witness — say so in the PR
template. If a witness is genuinely impractical (e.g. TUI rendering), explain
how you verified the change instead.

The `verify_done` tool (`stella-tools/src/verify.rs`) automates this in a
detached shadow git worktree at `HEAD` — it copies only the test files from the
working tree into the shadow, runs the suite, and expects a failure there.
**The working tree is never mutated** (no stash, no checkout). Path resolution
is derived from the canonical root-relative path, never the raw model-supplied
string (an absolute path would make the shadow copy truncate the real file).

The staged pipeline enforces the same contract at runtime: when no
`--test-command` is configured, its **witness stage** has an independent model
(the judge's resolution, never the worker) author the failing witness test up
front, tracks its fail→pass flip in the flip oracle, and refuses to credit the
flip if the worker modified the witness files (tamper exclusion). See
`docs/design/pipeline.md` for the full stage flow, the distress-triggered guidance
loop, and the `/pipeline` deck toggle.

---

## Workspace layout — where a change goes

Sixteen crates. The one-sentence rule of thumb:

| You want to… | Crate | Notes |
|---|---|---|
| Change the agent loop (plan / retry / compact / budget / loop-detect / hooks / skills / rules) | `stella-core` | **No I/O allowed.** Decision logic only. |
| Add/fix a model provider (SSE, tool-call dialect, pricing) | `stella-model` | One file per adapter (`anthropic.rs`, `openai.rs`, `gemini.rs`, `vertex.rs`, `bedrock.rs`, `zai.rs`). Copy an existing adapter's shape. |
| Add/fix a built-in tool (`read_file`, `verify_done`, the opt-in `bash`, …) | `stella-tools` | Implement the `Tool` trait, register in `ToolRegistry`. |
| Change CLI commands, flags, or agent wiring | `stella-cli` | This is the shipping binary. |
| Change REPL rendering / panels / keybindings | `stella-tui` | Pure-fold ratatui REPL — the Command Deck, the default interactive shell on a TTY. |
| Touch shared types crossing a crate boundary | `stella-protocol` | **Zero logic, zero I/O — types only.** |
| Persistence: executions, events, telemetry (SQLite) | `stella-store` | |
| Retrieval: graph, embeddings, episodic memory | `stella-context` | |
| Tree-sitter code indexing | `stella-graph` | |
| Triage → … → judge orchestration plane | `stella-pipeline` | |
| MCP client (external tool servers) | `stella-mcp` | |
| Multimodal generation | `stella-media` | |
| Multi-agent fan-out, worktree isolation | `stella-fleet` | |
| Open Context Protocol (wire types / host / conformance) | external repo: [`opencontextprotocol`](https://github.com/macanderson/opencontextprotocol) | Split out of this workspace; Stella depends on it via git. `ocp-types` stays dependency-light by contract. |

**Status — what ships.** The live runtime path is
`stella-cli` → `stella-core` → `stella-model` / `stella-tools` / `stella-store` /
`stella-context` (recall only) / `stella-mcp`, and the CLI also drives
`stella-pipeline` (the default `stella run` path), `stella-fleet` (`stella fleet`),
`stella-tui` (the Command Deck, the default interactive shell on a TTY), and
`stella-media` (image generation via the `generate_image` tool). The fuller
`stella-graph` retrieval + context plane (`stella init` builds the code-graph
index; recall fans out through the OCP host) is also wired.

---

## The `.stella/` directory (per-workspace state)

The CLI reads and writes a `.stella/` directory at the workspace root. An agent
editing Stella's own code should know what lives where:

| Path | Purpose |
|---|---|
| `.stella/memories/*.md` | Durable lessons baked into the byte-stable system prompt prefix. Sorted by filename, loaded once per session. (Write side: the `save_memory` tool.) |
| `.stella/skills/<slug>/SKILL.md` | Auto-promoted skills from recurring reflection lessons. Never enforced — selected and injected as volatile context. |
| `.stella/tools/*.toml` | Developer-defined custom script tools. Also scanned at `~/.config/stella/tools/`. |
| `.stella/settings.json` | Project-scope provider config (overrides built-ins or defines new providers) and tool switches (`tools.bash: "on"` opts the shell tool in — it is off by default in every scope). Merged per-field with org-managed and user scopes. |
| `.stella/mcp.toml` | MCP server config — extra tools merged into the registry at session start. |
| `.stella/domains.toml` | Domain taxonomy for memory/reflection tagging, inferred by `stella init`. |
| `.stella/reflections.jsonl` | Per-turn reflection mining log (one JSON object per line). |
| `.stella/store.db` | Local SQLite telemetry (executions, events, cost/tokens). No phone-home. |
| `.stella/codegraph.db` | Tree-sitter code-graph index, built on `stella init`. |

---

## Code style and conventions

- **`rustfmt` settles all formatting** — default config, no arguments. Don't
  hand-format. CI runs `cargo fmt --check`.
- **Clippy at `-D warnings`** across all targets. Do **not** `#[allow]` your way
  past a lint without a comment saying why the lint is wrong *here*.
- **Name things for what they are, not what they were.** If you rename a
  concept, chase it through comments and docs in the same PR — stale comments
  are treated as bugs in review.
- **Doc comments on public items**, and on any function whose *why* isn't
  obvious from its body. No comments that narrate the next line.
- **No new dependencies casually.** Every new crate in `Cargo.toml` gets a
  sentence in the PR description justifying it.
- **Match the neighborhood.** Every crate has an established idiom — copy the
  patterns around you before inventing new ones. The module-level doc comment
  (`//!`) is the established entry point for each file; study a sibling before
  writing a new one.
- **Edition 2024, MSRV 1.90.** Workspace deps are centralized in the root
  `Cargo.toml` `[workspace.dependencies]` — reference them as
  `serde.workspace = true` in per-crate manifests.

### Commits

[Conventional Commits](https://www.conventionalcommits.org), with the crate or
surface as the scope, matching the existing history:

```text
feat(stella-model): add mistral provider adapter
fix(stella-tui): restore terminal on panic in raw mode
docs(readme): correct provider table
ci(release): sign macOS binaries
```

Commits must be **DCO-signed** (`git commit -s`). One logical change per PR.

---

## Testing approach

- **Property tests** for pure engine logic (`proptest`): compaction,
  loop detection, budget arithmetic, retry history, calibration drift.
  These run on every `cargo test`.
- **Witness tests** for features — see above.
- **Wiremock-based adapter tests** for provider SSE parsing and HTTP error
  classification (`stella-model`, `stella-mcp`, `stella-media`).
- **Integration tests** with fixture MCP servers (`stella-mcp/tests/`).
- **Replay fixtures** for pipeline stages (`stella-pipeline/tests/`).

When iterating, run a single crate's tests — `cargo test -p stella-core` is
seconds; `cargo test --workspace` rebuilds everything.

---

## Gotchas

- **`Cargo.lock` is tracked.** Stella ships a binary and `install.sh` builds
  with `--locked`, so the lockfile must be committed and reproducible.
- **`.cargo/config.toml` is gitignored** — it holds per-developer cargo aliases
  (`tc` = test stella-core, etc.). It's not committed.
- **Settings 3-scope merge**: user → org-managed (`STELLA_MANAGED_SETTINGS`) →
  project (`.stella/settings.json`). Project wins per-field.
- **`context.db` vs `codegraph.db`**: `stella-context` and `stella-graph` used
  to share `.stella/context.db` — they now use separate files
  (`context.db` and `codegraph.db` respectively). Don't revert this.
