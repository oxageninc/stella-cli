# Project scripts index

Every workspace has the same six jobs — install, build, start, test, lint,
format — spelled differently by every package manager. Today the agent
rediscovers that spelling with model calls: read `package.json`, guess the
package manager, compose a `bash` command. This spec makes the mapping a
deterministic index computed by Stella itself, so listing and running a
project script costs **zero model turns of discovery**: the canonical verbs
ride the byte-stable system prompt, and one `run_script` tool call executes
them.

Three surfaces share one detection function, the same way the code graph
shares `stella_tools::graph::run_query` between the `graph_query` tool and
the `stella graph` subcommand:

1. **Prompt section** — a compact `## Project scripts` block appended by
   `assemble_system_prompt` (`stella-cli/src/agent.rs:141`), computed once at
   session start, byte-stable within the session.
2. **Tools** — `list_scripts` (read-only) and `run_script` in
   `stella-tools`, registered in `ToolRegistry::with_backends` and added to
   `custom::RESERVED_NAMES` (`stella-tools/src/custom.rs:93`).
3. **CLI** — `stella scripts list` / `stella scripts run <id> [-- args]`,
   mirroring the `Graph` subcommand (`stella-cli/src/main.rs:238`), offline
   (short-circuits before provider resolution).

The detection core lives in a new `stella-tools/src/scripts.rs` and
**replaces** the private `detect()`/`Toolchain` in
`stella-tools/src/project.rs:33` — `build_project` and `run_tests` become
thin verb shortcuts over the same index, so there is exactly one detection
code path in the workspace.

## Design invariants

- **Detection is static and side-effect free.** The index is built from file
  stats and manifest parses only. No package-manager binary is ever invoked
  at index time (no `just --list`, no `npm run`, no corepack trigger).
  Detection cost is a handful of file reads — cheap enough to recompute per
  CLI invocation and per `list_scripts` call, so there is **no cache file,
  no database, no watcher**. (The code graph needs `.stella/private/codegraph.db`
  because tree-sitter over the tree is expensive; manifest parsing is not.
  Nothing persisted means nothing to go stale or be lost.)
- **Byte-stable output.** Canonical verbs render in the fixed order
  `install, build, start, test, lint, format`; all other entries sort by
  `id`. Same workspace state ⇒ byte-identical prompt section, tool frame,
  and CLI output. The prompt section is computed once at session start and
  never mutated mid-session (same contract as memories, `AGENTS.md`
  "Byte-stable prompts").
- **`run_script` executes only indexed entries, argv-exec, no shell.**
  Input is resolved against the index; an unknown name is a
  `ToolOutput::Error` that lists the declared vocabulary (the error is the
  discovery surface). The tool execs the entry's argv directly
  (`exec::run_argv` — no `sh -c` anywhere), so no model-supplied string is
  ever shell-interpreted. Arbitrary commands remain the job of the opt-in
  `bash` tool / the `command` override on `build_project`/`run_tests`.
- **Same trust and gating as `bash`.** Indexed scripts are workspace-authored
  code, "the same trust level as a `package.json` script or a Makefile
  target" (`stella-tools/src/custom.rs`). `run_script` emits the blocking
  `command.started` hook chain with the fully resolved command, exactly like
  `bash` (`ToolRegistry::gate_side_effects`,
  `stella-tools/src/registry.rs:479`), so settings-driven policy can deny or
  require approval per command.
- **No new telemetry tables.** `run_script` invocations ride the existing
  `events` → `tool_calls` projection in `.stella/private/store.db`. No `store.db`
  migration slot is consumed.

## Detection semantics

A **package** is the workspace root plus each workspace member directory
declared by the root manifests (`pnpm-workspace.yaml` globs, `package.json`
`workspaces`, `[workspace] members` in `Cargo.toml`), capped at 50 members
(overflow is counted, not enumerated). Each package is scanned for markers
in the fixed order below; **all** matching ecosystems contribute entries
(a Cargo + pnpm + Make repo — this one — indexes all three).

| # | Marker (in package dir) | Runner | Script enumeration (static parse) | Synthesized verbs |
| --- | --- | --- | --- | --- |
| 1 | `Cargo.toml` | `cargo` | `[alias]` entries from `.cargo/config.toml` (root only) | install→`cargo fetch`, build→`cargo build --workspace`, start→`cargo run` (only if a default-run binary exists), test→`cargo test --workspace`, lint→`cargo clippy --workspace --all-targets`, format→`cargo fmt` |
| 2 | `package.json` | `pnpm` if `pnpm-lock.yaml`, `yarn` if `yarn.lock`, `bun` if `bun.lock`/`bun.lockb`, else `npm` | the `scripts` object → `<pm> run <name>` | install→`<pm> install` |
| 3 | `deno.json` / `deno.jsonc` | `deno` | the `tasks` object → `deno task <name>` | install→`deno install` |
| 4 | `pyproject.toml` | `uv` if `uv.lock` or `[tool.uv]`, `poetry` if `poetry.lock` or `[tool.poetry]`, else `uv` | `[project.scripts]` entry points → `<runner> run <name>` | install→`uv sync` / `poetry install`; test→`<runner> run pytest`, lint→`<runner> run ruff check`, format→`<runner> run ruff format` — each only if that tool appears in the declared dependencies (incl. dependency groups) |
| 5 | `go.mod` | `go` | — | install→`go mod download`, build→`go build ./...`, test→`go test ./...`, lint→`go vet ./...`, format→`go fmt ./...` |
| 6 | `Makefile` | `make` | non-pattern targets matching `^[A-Za-z0-9_.-]+:` (first rule line wins; `.PHONY`-declared targets first) → `make <name>` | — |
| 7 | `justfile` / `.justfile` | `just` | recipe headers (unindented `name args...:` lines, `_`-prefixed recipes excluded) → `just <name>` | — |
| 8 | `Taskfile.yml` / `Taskfile.yaml` | `task` | top-level `tasks:` keys → `task <name>` | — |
| 9 | `composer.json` | `composer` | the `scripts` object → `composer run <name>` | install→`composer install` |

Gradle, Maven, Bazel, and Nx are explicitly out of scope for v1; the table
is the extension point (one row = one detector, each a pure function of the
package dir).

Every entry gets a qualified id `<runner>:<name>` (`pnpm:build`,
`make:lint`, `cargo:test`). Synthesized verbs get the verb as name
(`cargo:install` ⇒ `cargo fetch`). Ids are unique per package; entries from
member packages carry their `dir` and execute with cwd = that directory.

### Canonical verb resolution

The six verbs are a resolution layer over qualified ids, computed
deterministically per package:

| Verb | Explicit script names matched (exact, first match wins) | Fallback |
| --- | --- | --- |
| `install` | `install`, `setup`, `bootstrap` | synthesized install of each ecosystem |
| `build` | `build`, `compile`, `dist` | synthesized (cargo/go) |
| `start` | `start`, `dev`, `serve` | synthesized (`cargo run`) |
| `test` | `test`, `tests` | synthesized (cargo/go/uv) |
| `lint` | `lint` | synthesized (clippy/vet/ruff) |
| `format` | `format`, `fmt` | synthesized (fmt/gofmt/ruff) |

Binding order for a bare verb: (1) an explicitly named script in the
ecosystem whose marker ranks first in the table above, (2) that ecosystem's
synthesized default, (3) explicit scripts of later-ranked ecosystems. An
explicit script always beats a synthesized one *within* an ecosystem —
a `package.json` `build` script encodes project intent; `cargo build` is a
guess. All losing candidates remain listed and runnable by qualified id;
only the bare-verb binding is exclusive.

Names are **never** verb-bound when they contain `watch`, or equal
`publish`, `deploy`, `release`, or `clean` — those stay qualified-only, so
a canonical verb can never implicitly trigger an outward-facing or
destructive action. `run_tests`'s existing kind mapping (`test:unit`,
`test:e2e`, `e2e`, and its refusal to pass unit tests off as e2e,
`stella-tools/src/project.rs:183`) is preserved unchanged on top of the
index.

## Tool surface

`list_scripts` — `read_only: true`, input `{ dir?: string }` (default: all
packages). Returns the human frame shown under **Example rendering**; the
same function serves `stella scripts list`, and `--json` emits the schema
below.

`run_script` — `read_only: false`, input:

```json
{
  "type": "object",
  "required": ["script"],
  "properties": {
    "script": { "type": "string", "description": "Canonical verb (install|build|start|test|lint|format), qualified id like pnpm:build, or unique declared script/target/alias name" },
    "dir": { "type": "string", "description": "Package dir when the id exists in several packages; default workspace root" },
    "args": { "type": "array", "items": { "type": "string" }, "description": "Appended runner-natively (after `--` for npm-family)" },
    "timeout_secs": { "type": "integer" }
  }
}
```

Execution reuses `exec::run` with the `run_and_report` framing
(`stella-tools/src/project.rs:75`): `` `<command>` PASSED (exit 0) `` /
`` FAILED (exit <code>) `` plus truncated output — the model reads success
without a follow-up question. Default timeout 600 s, same as
`build_project`.

## Prompt section

`assemble_system_prompt` inserts the block immediately after the base
instructions (before memories — ground truth before recalled lessons):

```
## Project scripts

Detected: cargo, pnpm, make. Run these with the run_script tool — do not
rediscover them with bash/cat.

install → cargo fetch
build   → cargo build --workspace
test    → cargo test --workspace
lint    → cargo clippy --workspace --all-targets
format  → cargo fmt

23 more scripts (make:docs, pnpm:deck, …): call list_scripts.
```

Only the six verb bindings render inline; everything else is a count plus
up to three sorted teaser ids. The section is capped at 1,500 characters
(oversized verb commands truncate with `…`), keeping the stable prefix
cheap. An empty index renders nothing — no section, no noise.

## Index JSON Schema

Emitted by `stella scripts list --json`; `schema_version` bumps on any
shape change.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Project Scripts Index",
  "type": "object",
  "additionalProperties": false,
  "required": ["schema_version", "verbs", "scripts"],
  "properties": {
    "schema_version": { "const": 1 },
    "verbs": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "install": { "type": "string" }, "build": { "type": "string" },
        "start": { "type": "string" }, "test": { "type": "string" },
        "lint": { "type": "string" }, "format": { "type": "string" }
      },
      "description": "Canonical verb → qualified id of the winning entry. Absent key = no binding."
    },
    "scripts": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "runner", "name", "command", "dir", "source"],
        "properties": {
          "id": { "type": "string", "pattern": "^[a-z]+:.+$" },
          "runner": { "type": "string", "enum": ["cargo", "npm", "pnpm", "yarn", "bun", "deno", "uv", "poetry", "go", "make", "just", "task", "composer"] },
          "name": { "type": "string", "minLength": 1 },
          "command": { "type": "string", "minLength": 1, "description": "Exact command run_script executes, cwd = dir" },
          "dir": { "type": "string", "description": "Workspace-relative package dir; \".\" = root" },
          "source": { "type": "string", "description": "Workspace-relative manifest path, or \"synthesized\"" },
          "verb": { "type": "string", "enum": ["install", "build", "start", "test", "lint", "format"], "description": "Present only on the entry each verb binds to" },
          "raw": { "type": "string", "description": "The manifest's own definition (e.g. the package.json script body), when one exists" }
        }
      }
    }
  }
}
```

## Example

Stella's own workspace (`Cargo.toml` + `package.json`/pnpm + `Makefile`):

```json
{
  "schema_version": 1,
  "verbs": { "install": "cargo:install", "build": "cargo:build", "test": "cargo:test", "lint": "cargo:lint", "format": "cargo:format" },
  "scripts": [
    { "id": "cargo:build", "runner": "cargo", "name": "build", "command": "cargo build --workspace", "dir": ".", "source": "synthesized", "verb": "build" },
    { "id": "cargo:test", "runner": "cargo", "name": "test", "command": "cargo test --workspace", "dir": ".", "source": "synthesized", "verb": "test" },
    { "id": "make:deck-snapshots", "runner": "make", "name": "deck-snapshots", "command": "make deck-snapshots", "dir": ".", "source": "Makefile" },
    { "id": "pnpm:docs:dev", "runner": "pnpm", "name": "docs:dev", "command": "pnpm run docs:dev", "dir": ".", "source": "package.json", "raw": "pnpm --dir stella-docs dev" }
  ]
}
```

The agent turn that motivated this spec — "install this project" — becomes:
the model already sees `install → cargo fetch` in its stable prefix and
issues one `run_script {"script": "install"}` call. No manifest reads, no
package-manager guessing, no bash composition.

## Configuration

An optional `scripts` section in `settings.json`
(`stella-cli/src/settings.rs`, following the `McpSettings` pattern; 3-scope
merge applies):

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | `false` removes both tools, the CLI data, and the prompt section |
| `deny` | `[]` | Glob list of qualified ids `run_script` refuses (listed, marked `denied`) |
| `verbs` | `{}` | Explicit verb → qualified-id overrides, beating the resolution rules |

`verbs` is the escape hatch for exotic setups (e.g. `"test": "make:check"`)
— configuration over heuristics, no new detection code.

## Delivery

1. `stella-tools/src/scripts.rs`: `ScriptIndex::detect(root)` + rendering +
   resolution, superseding `script.rs`'s original three-source vocabulary
   (Makefile / package.json / cargo aliases) with the full ecosystem table
   while keeping its contracts: argv exec and the
   unknown-name-lists-the-vocabulary discovery error; `project.rs`
   (`build_project`/`run_tests`) rewired onto it so one detection code
   path remains. ✅ shipped
2. Register `list_scripts` beside `run_script`; extend `RESERVED_NAMES`
   (the collision test in `stella-tools/src/registry.rs` enforces it);
   gate `run_script` on the `command.started` chain with its resolved
   command line. ✅ shipped
3. `stella scripts` subcommand + offline short-circuit; prompt section in
   `assemble_system_prompt` with a byte-stability test (two builds, same
   fixture ⇒ identical bytes). ✅ shipped
4. Follow-ups: the optional `scripts` settings section; a docs page under
   `stella-docs`.
