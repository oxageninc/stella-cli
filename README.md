# Stella

A fast, BYOK, model-agnostic terminal coding agent built in Rust.

## Set your API key

Stella is bring-your-own-key. Set one or more of these environment variables:

| Provider | Env Var | Default Model |
|---|---|---|
| **Z.ai (GLM 5.2)** | `ZAI_API_KEY` | `glm-5.2` |
| **Anthropic (Claude)** | `ANTHROPIC_API_KEY` | `claude-fable-5` |
| **OpenAI (GPT)** | `OPENAI_API_KEY` | `gpt-5.5` |
| **xAI (Grok)** | `XAI_API_KEY` | `grok-4` |
| **DeepSeek** | `DEEPSEEK_API_KEY` | `deepseek-chat` |
| **Google Gemini** | `GEMINI_API_KEY` | `gemini-3-pro` |
| **OpenRouter** | `OPENROUTER_API_KEY` | `auto` |

```bash
export ZAI_API_KEY=your_key_here
# or
export ANTHROPIC_API_KEY=your_key_here
```

Stella auto-detects which provider to use based on which keys are set.
To pin a specific provider/model, use --model:

```bash
stella --model zai/glm-5.2 run "fix the failing test"
stella --model anthropic/claude-fable-5 chat
```

### Check what is configured

```bash
stella models    # list all providers, their models, and key status
stella config    # show current resolved configuration
```

## Usage

### Interactive chat (default)

```bash
stella
# or
stella chat
```

Starts an interactive REPL. Type your prompt, press Enter. Stella will:
1. Think (with a live spinner)
2. Call tools as needed (read files, run commands, search code)
3. Show its response
4. Display a cost/token summary

**In-chat commands:**
- `/goal <text>` - work in judged rounds until the goal is met
- `/files` - show the Files Touched panel ([C|R|U|D] per file)
- `/models` - list configured providers and models
- `/config` - show current configuration
- `/rename <name>` - rename this terminal tab
- `/color <name>` - switch the accent color (tell windows apart)
- `/clear` - clear conversation history
- `/help` - show help
- `/exit` or Ctrl+D - exit Stella

### One-shot run

```bash
stella run "fix the failing test in src/auth.rs"
stella run "add a health check endpoint to the API"
```

### Pin a model

```bash
stella --model anthropic/claude-fable-5 run "refactor the database layer"
```

## Built-in Tools

| Tool | Description |
|---|---|
| `read_file` | Read a file with line numbers (supports offset/limit) |
| `write_file` | Create or overwrite a file (creates parent dirs) |
| `edit_file` | Replace an exact substring in a file (surgical edits) |
| `delete_file` | Delete a workspace file (completes the CRUD ledger) |
| `bash` | Run a shell command (timeout kill; `trace: true` echoes each line) |
| `grep` | Search file contents with regex (shells to ripgrep) |
| `glob` | Find files matching a glob pattern (shells to fd) |
| `build_project` | Build with the workspace's own toolchain (cargo/npm/go/make) |
| `run_tests` | Run tests — kind unit/e2e/all + a runner-native filter |
| `verify_done` | The deterministic definition of done (see below) |
| `explorations` / `save_exploration` | Shared codebase maps — explore once, reuse everywhere |
| `save_memory` | Persist a lesson for every future session's prompt |
| `ci_status` | CI runs + failure logs via `gh` (judge-usable, read-only) |
| `screenshot` | Capture the screen as verification evidence |
| `create_issue` `update_issue` `close_issue` `search_issues` `start_work_on_issue` | Issue tracking — loaded only when configured (see below) |

All file tools are workspace-root-pinned. Every successful read/write/
edit/delete lands in the **Files Touched** ledger, rendered per turn as a
`[C|R|U|D] path` panel (also `/files` in the REPL).

**Issue tools are conditional:** set `LINEAR_API_KEY` for the Linear
backend (it wins), or have `gh auth login` done for GitHub Issues. With
neither, the tools aren't registered at all — no dead schema, no wasted
tokens.

## The deterministic definition of done

Stella works test-first by default and `verify_done` is the gate: your
witness test must **fail on the previous code** (git HEAD, in a shadow
worktree with only your new test files layered in) and **pass on your new
code**. `WITNESS CONFIRMED` is done; a merely green suite is not — green
suites can hide unwired features and vacuous tests, the witness cannot.

## Goal mode — don't stop until a judge says it's done

```bash
stella goal "the login flow has a passing e2e test and CI is green"
stella monitor main        # CI-to-green as a judged goal
# or in the REPL:
/goal make the parser handle CRLF files
```

Stella works in rounds; after each round an LLM judge assesses the goal
from EVIDENCE — it has its own read-only tools (read_file, grep, glob,
explorations, ci_status) to verify claims directly, and its feedback
drives the next round. Bounded by a round cap and your `--budget`.

## Self-improving, prompt-cache-native

Lessons saved with `save_memory` (and by you, as markdown in
`.stella/memories/`) load once at session start into a byte-stable system
prompt — every model call considers them at prompt-cache-hit prices. New
memories take effect next session by design: hot-injection would
invalidate the cache on every save.

## Local telemetry — DuckDB

Every execution is recorded in `.stella/stella.duckdb`: the full event
stream (chain-of-thought deltas included), per-model-call telemetry
(tokens in/out, cache read hits/misses, cost computed from the model
card's pricing), the files-touched ledger, plus `file_locks` and
`graph_nodes`/`graph_edges` tables that the upcoming context plane
(embeddings for md/mdx/txt/doc/docx) writes into. Query it with any
DuckDB client — it's your data, on your disk.

## Supported Providers

- **Z.ai** (GLM 5.2) - OpenAI-compatible
- **Anthropic** (Claude Fable 5) - Messages API
- **OpenAI** (GPT-5.5) - OpenAI-compatible
- **xAI** (Grok 4) - OpenAI-compatible
- **DeepSeek** - OpenAI-compatible
- **Google Gemini** - OpenAI-compatible
- **OpenRouter** - OpenAI-compatible multi-model gateway

Any OpenAI-compatible gateway (Vercel AI Gateway, Azure OpenAI, Together,
etc.) can be used by setting the appropriate base URL and key.

## Architecture

stella (stella-cli) = CLI binary + agent loop + TUI
  stella-core  = step-driver engine: parallel tools, goal loop, budget,
                 retry, compaction, loop detection, rules/hooks/router
  stella-tools = the 15-20 built-in tools (CRUD, exec, verify, issues, CI)
  stella-model = Provider trait + adapters (SSE, tool-call dialects, pricing)
  stella-store = DuckDB persistence (executions, events, telemetry, locks, graph)
  stella-protocol = Shared serde types + the Provider port
  ocp-types = Open Context Protocol wire types

Key design principles (from docs/specs/oxagen-rust-cli/):
- Ports, not concretions - the engine drives through traits
- No phone-home - zero network calls other than your model provider
- BYOK - any provider key, any combination, no account
- Serde-first - every cross-boundary type is versioned
- Fail loud, recover gracefully - typed errors, never panic

## Development

```bash
cd crates
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p stella-cli -- models
```

## License

MIT OR Apache-2.0

## Roadmap
- Phase 0: Workspace skeleton + provider spike (done)
- Phase 1: Built-in tools (done)
- CLI binary: stella with agent loop, REPL, TUI (done)
- Phase 2: Step-driver, role router, budget, model matrix, goal loop,
  parallel tools, witness verification, DuckDB telemetry (done — Bedrock/
  Vertex/native-Gemini/GGUF adapters and wiring the router+hooks into the
  turn path are the tracked follow-ups)
- Phase 3: Local context plane — embeddings for md/mdx/txt/doc/docx into
  the DuckDB graph tables, code graph, OCP host + conformance
- Phase 5: Fleet, TUI polish, media generation (vision-grade judge
  evidence: screenshots attached to judge calls)
- Phase 6: Benchmark proof (SWE-bench Verified)
- Phase 7: OSS release (cargo-dist, Homebrew, curl|sh)
