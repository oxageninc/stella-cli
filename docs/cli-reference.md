# Stella CLI — Command Reference

Every command the `stella` binary exposes: top-level subcommands, the two
interactive surfaces (the plain line-based REPL and the tabbed Command Deck
TUI), and the output formats. Each entry lists its invocation, argument hints,
where it runs, and what it produces.

> **Two interactive surfaces.** `stella chat` (the default) launches the
> **Command Deck** — a tabbed TUI — when stdin and stdout are both real
> terminals and `--plain`/`STELLA_PLAIN` is unset; otherwise it falls back to
> the **plain REPL** (a line-based prompt). Most slash commands exist in both;
> the deck adds a large keyboard surface that has no REPL equivalent.

---

## Table of contents

1. [Global flags](#1-global-flags)
2. [Top-level subcommands](#2-top-level-subcommands)
3. [Output formats](#3-output-formats)
4. [Plain REPL commands](#4-plain-repl-commands)
5. [Command Deck (TUI) — global keys](#5-command-deck-tui--global-keys)
6. [Command Deck — slash commands](#6-command-deck--slash-commands)
7. [Command Deck — per-tab keys](#7-command-deck--per-tab-keys)
8. [Command Deck — modal overlays](#8-command-deck--modal-overlays)

---

## 1. Global flags

These precede the subcommand and apply to every invocation.

| Flag | Env var | Arg hint | Description |
|------|---------|----------|-------------|
| `--model` | `STELLA_MODEL` | `provider/model_id` | Override the worker model (e.g. `zai/glm-5.2`, `anthropic/claude-fable-5`, `openai/gpt-5.5`). |
| `--api-key` | — | `KEY` | Provider API key (highest precedence in the credential chain). Prefer an env var or `~/.config/stella/credentials.toml` — a flag value is visible in shell history and `ps`. |
| `--base-url` | `STELLA_BASE_URL` | `URL` | Base URL override. Required for `local/<model>` (Ollama, vLLM, LM Studio — e.g. `http://localhost:11434/v1`); optional proxy for other providers. |
| `--output-format` | `STELLA_OUTPUT_FORMAT` | `text\|json\|stream-json` | How turn output reaches the caller. Default `text`. See [§3](#3-output-formats). |
| `--budget` | `STELLA_BUDGET` | `USD` | Hard USD spend cap — work aborts cleanly (never mid-tool) once exceeded. Omit to meter-only. Must be a positive finite number. |
| `--plain` | `STELLA_PLAIN=1` | (flag) | Use the plain line-based REPL instead of the Command Deck TUI. Also forced when stdin/stdout is not a terminal. |
| `--no-anim` | `STELLA_NO_ANIM` / `NO_COLOR` | (flag) | Freeze all deck animation (progress-bar shimmer, caret blink) — for CI and recordings. |
| `--version` / `-V` | — | (flag) | Print version and exit. |
| `--help` / `-h` | — | (flag) | Print clap help and exit. |

---

## 2. Top-level subcommands

When no subcommand is given, `stella` defaults to `chat`.

### `stella run <PROMPT>`

Send a one-shot prompt (non-interactive). Exits when the turn completes.

| Argument / flag | Hint | Description |
|-----------------|------|-------------|
| `prompt` | _text_ (required) | The prompt to send. |
| `--no-pipeline` | (flag) | Use the raw step-loop instead of the staged pipeline. The pipeline (triage → plan → execute → verify → judge) is the default. |
| `--test-command` | `CMD` | The deterministic test the verify stage runs (e.g. `cargo test -p my-crate`). Arms the fail→pass flip oracle; omitted → always escalates to the judge. |

**Output:** honors `--output-format` (`text` human, `json` one final object, `stream-json` one line per event).

### `stella goal <GOAL>`

Work in judged rounds until a judge model confirms the goal is met.

| Argument | Hint | Description |
|----------|------|-------------|
| `goal` | _text_ (required) | What must be true when done — assessed by the judge each round. |

**Output:** human-readable streaming (interactive).

### `stella monitor [TARGET]`

Watch CI for a branch/PR and fix failures until fully green. Implemented as a
goal whose judge calls `ci_status` itself.

| Argument | Hint | Description |
|----------|------|-------------|
| `target` | branch name or PR # (optional) | Default: `main`. |

**Output:** human-readable streaming (interactive).

### `stella chat`

Start an interactive session. Launches the **Command Deck** (tabbed TUI) on a
real terminal, or the **plain REPL** with `--plain`/non-TTY.

| Argument | Hint | Description |
|----------|------|-------------|
| (none) | — | No arguments; uses the global flags. |

**Output:** the chosen interactive surface.

### `stella init`

Analyze the workspace and infer its domain taxonomy
(`.stella/domains.toml`) plus build the code graph (`.stella/codegraph.db`) —
the tagging vocabulary for memories, reflections, and every code-graph
node/edge. Works offline (heuristic fallback); needs no API key.

**Output:** progress lines to stdout; writes `.stella/domains.toml` and
`.stella/codegraph.db`.

### `stella tools [--validate [DIR]]`

List every tool available to the agent this session — built-ins, developer
custom tools (`.stella/tools/`), and manifest diagnostics.

| Argument / flag | Hint | Description |
|-----------------|------|-------------|
| `--validate` | `[DIR]` | Validate custom tool manifests instead of listing: parse every `<name>.toml`, check names/fields/timeouts/collisions, exit non-zero on any error. Pass a directory (defaults to the discovery scan dirs). |

**Output:** aligned table (listing) or a pass/fail report (`--validate`). No
API key needed.

### `stella fleet`

Fan tasks out to a fleet of worker agents — one git worktree per isolated
task, wave-scheduled by dependency, every attempt/commit/dollar recorded in
`.stella/fleet.db`.

| Argument / flag | Hint | Description |
|-----------------|------|-------------|
| `tasks` | _text…_ | Task prompts — each becomes an independent isolated task. Required unless `--plan` is given. |
| `--plan` | `FILE` | A `.json`/`.toml` plan with `[[tasks]]` entries (id, title, prompt, optional `depends_on` + `isolation` + `claims`). Conflicts with `tasks`. |
| `--max-concurrency` | `N` (default `4`) | Max tasks dispatched concurrently within one wave. |
| `--base-ref` | `REF` | Git ref isolated worktrees branch from (default: current HEAD). |
| `--watch` | (flag) | After fan-out, watch each fleet branch's CI to completion and reconcile PR status via `gh`. Exits non-zero if any watched branch ends red. |

**Output:** live progress to stdout; writes `.stella/fleet.db`. Worktrees and
`fleet/<task>` branches are left in place for review.

### `stella graph <OP> <TARGET>`

Query the code graph built by `stella init`. Offline: reads
`.stella/codegraph.db`; needs no API key.

| Argument | Hint | Description |
|----------|------|-------------|
| `op` | `definitions\|references\|imports\|importers\|neighbors` | What to ask the graph. |
| `target` | symbol name _or_ workspace-relative file path | Symbol (definitions/references) or file (imports/importers/neighbors). |

**Output:** the exact frames the `graph_query` tool would return to the model.

### `stella models`

List configured providers and available models. Needs no API key.

**Output:** aligned table.

### `stella stats [--format FMT] [--provider ID]`

Summarize cost, tokens, and resolve rate per provider/model from local
telemetry (`.stella/store.db`) — $/resolved-task receipts.

| Argument / flag | Hint | Description |
|-----------------|------|-------------|
| `--format` | `table\|json\|csv` (default `table`) | Output format (table has a TOTAL row). |
| `--provider` | `ID` | Only show executions for this provider id (e.g. `zai`, `anthropic`, `local`). |

**Output:** table, JSON, or CSV. Needs no API key.

### `stella memory <CMD>`

Inspect the project's memories through the citation feedback loop and promote
eligible memories to project rules. Reads local state; needs no API key.

| Subcommand | Arg hint | Description |
|------------|----------|-------------|
| `memory list` | `[--format table\|json]` | Memories ranked by citation count, with average usefulness, truthfulness rate, and rule-promotion eligibility. |
| `memory promote` | `id` (`nod_…`) | Promote an eligible memory to `.stella/rules/<slug>.md`. Eligibility: cited successfully **>10** consecutive times since its last negative citation. |
| `memory validate` | (none) | Re-validate old memories against the current codebase — flags those whose file-path anchors no longer exist. |

### `stella mcp <CMD>`

Manage MCP servers: search a registry, install into `.stella/mcp.toml`, list
configured servers, show tool-usage telemetry. Reads/writes local state (+ the
registry over HTTP); needs no API key. Per-session enable/disable lives only
in the deck's MCP tab.

| Subcommand | Arg hint | Description |
|------------|----------|-------------|
| `mcp list` | (none) | List configured MCP servers (`.stella/mcp.toml`). |
| `mcp search` | `[QUERY...] [--limit N]` | Search the MCP server registry (omit query to list). |
| `mcp install` | `name [--alias ALIAS]` | Install a registry server into `.stella/mcp.toml` (overwrites — servers are not versioned). |
| `mcp remove` | `name` | Remove a configured server. |
| `mcp usage` | (none) | Show MCP tool-usage telemetry: calls per server/tool. |

### `stella config`

Show current configuration.

**Output:** the resolved provider, model, base URL, and credential source.

### `stella version`

Print the version and exit. Needs no API key.

**Output:** `stella v<version>` (dev builds append `-dev.<git-sha>`).

---

## 3. Output formats

Applies to `stella run` (and other turn-producing commands when scripted). Set
via `--output-format` or `STELLA_OUTPUT_FORMAT`.

| Value | Where | What it produces |
|-------|-------|------------------|
| `text` (default) | interactive | Human-oriented rendering — the same live streaming the REPL shows, line-by-line. |
| `json` | headless | One final JSON object summarizing the whole turn (outcome, cost, files touched, tool calls). |
| `stream-json` | headless | One JSON line per `AgentEvent` as it happens — a stable machine interface (the exact protocol enum, serialized line-by-line). |

---

## 4. Plain REPL commands

Available **only** inside `stella chat` when running the plain line-based REPL
(`--plain` or non-TTY). Type these at the `>` prompt. Custom ⚡ commands and
skills (from `.stella/agents` / `.stella/skills`) are also invocable as
`/<name> <args>` and expand into the prompt the model runs; reserved names
below can never be shadowed.

| Command | Arg hint | REPL | Description |
|---------|----------|:----:|-------------|
| (free text) | _prompt_ | ✅ | Send a prompt to the agent. |
| `/help` | (none) | ✅ | Show the command list. |
| `/clear` | (none) | ✅ | Clear conversation history (resets to the system prompt). |
| `/models` | (none) | ✅ | List configured providers and models. |
| `/config` | (none) | ✅ | Show current configuration. |
| `/files` | (none) | ✅ | Show files touched this session. |
| `/agents` | (none) | ✅ | List custom agents (⚡ from `.stella/agents` or `~/.config/stella/agents`). |
| `/init` | (none) | ✅ | Index the workspace: domain taxonomy + code graph. Reloads custom extensions afterward. |
| `/goal` | `<text>` | ✅ | Work in judged rounds until a judge confirms the goal is met. Bare `/goal` prints usage. |
| `/rename` | `<name>` | ✅ | Rename this terminal tab. |
| `/color` | `<name>` | ✅ | Change the accent color (multi-window). |
| `/exit` · `/quit` · `exit` | (none) | ✅ | Exit Stella. |
| Ctrl+D | — | ✅ | Exit Stella (EOF). |
| `/<custom>` | `<args>` | ✅ | A custom ⚡ command/skill/agent — expands its template with the args into the prompt. |

---

## 5. Command Deck (TUI) — global keys

Available everywhere inside the Command Deck (the default `stella chat`
surface). These are keyboard actions, not slash commands.

### Quit & help

| Key | Description |
|-----|-------------|
| `Ctrl-C` | Quit (clean cancel from anywhere). |
| `?` (empty composer) | Open the help overlay. Any key closes it. |

### Composing & submitting

| Key | Description |
|-----|-------------|
| type | Append to the prompt. |
| `⌘⏎` / `Ctrl⏎` | **Queue** the prompt — it never blocks; runs when the current turn finishes. |
| `⏎` (plain Enter) | Insert a line break (kept in the prompt). |
| `⌥[` / `⌥]` | Cursor to start / end of the prompt. |
| `↑` `↓` `←` `→` | Cursor motion (when the composer has text). |
| `Home` / `End` | Line start / end. |
| `!cmd` + submit | Run a shell command **immediately** — bypasses the queue and any busy agent. |

### Turn & queue control

| Key | Description |
|-----|-------------|
| `Esc` | Stop the in-flight turn (the next queued prompt then runs). |
| `Esc Esc` | Stop **& hold** — cancel, requeue the interrupted prompt at the front, and hold dispatch until your next submission (which runs ahead of it). |
| `Ctrl-T` | Toggle the queue editor. |
| `↑` (empty composer, Session tab, prompts queued) | Open the queue editor on the newest prompt. |

### Transcript inspection

| Key | Description |
|-----|-------------|
| `↑` `↓` (Session tab) | Select a message. `Esc` clears the selection. |
| `Ctrl-O` | Expand/collapse the selected message (tool args, tool output, or a thought). From the prompt: expands the newest; press ×2 = toggle **all** thinking. |
| `Ctrl-R` | Expand/collapse all chain-of-thought (thinking) globally. |

### Tabs

| Key | Description |
|-----|-------------|
| `Tab` / `⇧Tab` | Switch to the next / previous tab. |

The tabs: **Session**, **Agents**, **Traces**, **Files**, **Graph**,
**SKILLS**, **MCP**.

---

## 6. Command Deck — slash commands

Type `/` in the composer to open the command popup (↑/↓ select, Tab completes,
Enter dispatches). The 🔒 commands are built-in; ⚡ commands are your custom
extensions. Tab-switch commands (`/files`, `/diff`, `/graph`, `/agents`,
`/skills`, `/mcp`) are consumed TUI-side; the rest are dispatched to the
session driver and answered into the transcript.

| Command | Arg hint | Description |
|---------|----------|-------------|
| `/help` | (none) | Show commands (into the transcript). |
| `/clear` | (none) | Reset the conversation (clears transcript + cost, returns progress to idle). |
| `/models` | (none) | List providers & models (into the transcript). |
| `/init` | (none) | Index the workspace: domains + code graph. Reloads custom extensions and refreshes the Graph tab. |
| `/pipeline` | (none) | Toggle the staged pipeline (witness-verified turns: triage → recall → plan → scope → witness → execute → verify → judge). |
| `/files` | (none) | Open the **Files** tab. |
| `/diff` | (none) | Open the **diff viewer** (Files tab, diff open). |
| `/graph` | (none) | Open the **code-graph** tab. |
| `/agents` | (none) | Open the **Agents** tab (Installed Agents pane) and refresh the list. |
| `/skills` | (none) | Open the **SKILLS** tab and refresh the installed list. |
| `/mcp` | (none) | Open the **MCP servers** tab. |
| `/<custom>` | `<args>` | A custom ⚡ command/skill/agent — expands its template into the prompt the model runs. |

> A bare unknown `/word` (no arguments) is flagged as unknown rather than sent
> to the model; anything with arguments (e.g. `/src/main.rs explain`) falls
> through and stays a prompt.

---

## 7. Command Deck — per-tab keys

Keys that act on the active tab's content. Letter verbs are gated on an empty
composer so they don't shadow the first character of a prompt.

### Agents tab

Two panes: **Executions** and **Installed Agents** (←/→ switch).

| Key | Pane | Description |
|-----|------|-------------|
| `↑` `↓` | Executions | Focus an agent. |
| `⏎` | Executions | Jump to the Session tab for the focused agent. |
| `s` | Executions | Stop the focused agent. |
| `←` / `→` | both | Switch between Executions ↔ Installed Agents. |
| `↑` `↓` | Installed | Select an installed agent. |
| `⏎` | Installed | Open the editor on the selected agent's pinned version. |
| `v` | Installed | Open the version picker (pin an existing version). |
| `n` | Installed | Start the create-from-prompt flow (LLM-assisted). |
| `r` | Installed | Reload the list from disk. |

### Traces tab

| Key | Description |
|-----|-------------|
| `↑` `↓` `PageUp` `PageDown` | Scroll the trace log. |
| `f` | Cycle the per-agent filter. |

### Files tab

| Key | Description |
|-----|-------------|
| `↑` `↓` | Select a changed file. |
| `⏎` | Toggle the diff viewer open/closed. |
| (diff open) `↑` `↓` | Scroll the diff. |
| (diff open) `Esc` | Close the diff. |

### Graph tab

| Key | Description |
|-----|-------------|
| `←` `↑` / `→` `↓` | Move the graph cursor. |
| `/` or `⏎` | Open the file picker (filter & re-root the neighborhood on any indexed file). |

### SKILLS tab

Two panes: **Installed** (manage) and **Search** (registry). ←/→ switch.

| Key | Pane | Description |
|-----|------|-------------|
| `↑` `↓` | Installed | Select a skill. |
| `Space` | Installed | Toggle enabled/disabled (live). |
| `Ctrl-X` `Ctrl-X` | Installed | Delete the selected skill from disk (two presses). |
| `Ctrl-O` | Installed | Preview the skill's rendered `SKILL.md`. |
| `e` | Installed | Edit the body (saving makes a new pinned version). |
| `p` | Installed | Pin a specific version. |
| `n` | Installed | Create a new skill with LLM assistance (description → scope). |
| `→` | Installed | Cross to the Search pane. |
| (type) | Search | Enter a registry query. |
| `⏎` | Search | Run the search (or install the highlighted result if results already match). |
| `Ctrl-O` | Search | Preview a registry hit's `SKILL.md` (fetched). |
| `←` | Search | Return to the Installed pane. |

### MCP tab

Three sub-modes: **Browse**, **Search** (modal), **Auth** (modal).

| Key | Mode | Description |
|-----|------|-------------|
| `↑` `↓` | Browse | Select a configured server. |
| `e` / `Space` | Browse | Enable/disable the selected server (session-scoped, live). |
| `a` | Browse | Enter the auth (credential) prompt for the selected server. |
| `x` | Browse | Remove the selected server from `mcp.toml`. |
| `r` | Browse | Refresh the snapshot. |
| `/` | Browse | Enter search mode. |
| (type) | Search | Enter a registry query. |
| `⏎` | Search | Search, then `⏎` again installs the highlighted result. |
| `Esc` | Search | Back to Browse. |
| (type) | Auth | Field name, then value (two-step, value masked). |
| `⏎` | Auth | Advance field → value → submit credential. |
| `Esc` | Auth | Cancel. |

### Session tab

| Key | Description |
|-----|-------------|
| `↑` `↓` | Select / navigate messages. |
| `Esc` | Clear the selection. |
| (scroll) `↑` `↓` `PageUp` `PageDown` `Home` `End` | Scroll the transcript. |

---

## 8. Command Deck — modal overlays

While open these own the keyboard (only `Ctrl-C` quit precedes them).

| Overlay | Opened by | Keys |
|---------|-----------|------|
| **Help overlay** | `?` (empty composer) | Any key closes it. |
| **Queue editor** | `Ctrl-T` / `↑` (empty, queued) | `↑` `↓` select · `⏎` edit · `Ctrl-X` delete · `Ctrl-D Ctrl-D` clear all · `Esc` close. |
| **Graph file picker** | `/` or `⏎` (Graph tab) | Type to filter · `↑` `↓` select · `⏎` re-root · `Esc` close. |
| **Skill preview** | `Ctrl-O` (SKILLS tab) | `↑` `↓` `PageUp` `PageDown` `Home` `End` scroll · `Esc` / `Ctrl-O` / `q` close. |
| **Skill create/edit/pin** | `n` / `e` / `p` (SKILLS tab) | Type · `Ctrl-S` save (new version) · `Esc` cancel. |
| **Agent editor** | `⏎` (Installed Agents) | Type · `Ctrl-S` saves a new pinned version · `Esc` discards. |
| **Agent version picker** | `v` (Installed Agents) | `↑` `↓` select · `⏎` pin · `Esc` close. |
| **Agent create flow** | `n` (Installed Agents) | Type description · pick scope · `⏎` draft & install. |
| **Scope-review gate** | (auto, pipeline) | `a` approve · `t` trim · `x`/`Esc` abort. (In the deck, scope review is narrated, not gated — auto-approved.) |
| **Ask-user gate** | (auto, agent asks) | `1`–`9` quick-pick an option, or type free text + submit chord. |

---

### Scope-review & ask-user (interactive gates)

These appear inline when the agent requests a decision:

- **Scope review** (`a`pprove / `t`rim / `x`/`Esc` abort) — only in pipeline
  mode and only in surfaces that can gate. The deck auto-approves and narrates.
- **Ask-user** — digit keys `1`–`9` quick-pick the numbered option when the
  composer is empty; otherwise type free text and submit with the submit chord
  (`⌘⏎`/`Ctrl⏎`). A `!`-prefixed line stays a shell command even while a
  question is pending.
