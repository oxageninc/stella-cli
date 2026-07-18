# Command Deck — Help

## Prompt & submitting

| Key | Action |
|---|---|
| `type` | Compose a prompt |
| `⌘⏎` / `Ctrl⏎` | **Queue** the prompt (never blocks a running turn) |
| `⏎` | Insert a line break (multi-line prompt) |
| `!cmd` | Run a shell command **now** (skips the queue) |
| `/` | Slash-command popup (↑/↓ · `Tab` complete · `⏎` run) |
| `Ctrl-V` | Paste from the clipboard — a copied **image** is stored and attached |

## Attachments

Stella understands images, PDFs, audio, and video as context. Attach them by:

- `Ctrl-V` after copying an image (screenshot, browser copy) — the bitmap is
  saved under `.stella/attachments/` and rides your next prompt.
- Naming a file in the prompt (or dragging it onto the terminal): any path to
  an existing image / PDF / audio / video file is attached automatically.

What each provider ingests natively varies (Gemini reads all four; Anthropic
and OpenAI read images + PDFs; the OpenAI-compatible gateways read images).
Anything a provider can't read is described to the model instead of failing
the turn. Stella imposes no size limit of its own — provider request limits
still apply.

## Turn control

| Key | Action |
|---|---|
| `Esc` | Stop the running turn (next queued prompt runs) |
| `Esc Esc` | Stop **& hold** — cancel, requeue, park until your next input |
| `Ctrl-T` | Toggle the queue editor (also `↑` from an empty prompt) |
| `Ctrl-C` | Quit Stella |

## Reading the transcript

| Key | Action |
|---|---|
| `↑` `↓` | Select a message (Session tab) · `Esc` clears |
| `Ctrl-O` | Expand/collapse the selected message (args, output, thoughts) |
| `Ctrl-O` ×2 | From the prompt: toggle **all** chain-of-thought |
| `Ctrl-R` | Expand/collapse all thinking globally |

## Tabs — switch & navigate

`Tab` / `⇧Tab` cycles tabs. Each tab also has its own keys (empty composer):

**Session** — the conversation. `↑` `↓` select; `Esc` clears; scroll with
`PageUp`/`PageDown`/`Home`/`End`.

**Agents** — live executions + installed agents.
`←` `→` switch panes · `s` stop agent · `⏎` edit installed · `v` versions ·
`n` new (LLM) · `r` reload.

**Traces** — the step-by-step event log. `f` cycles the per-agent filter.

**Files** — every file touched this session. `⏎` opens/closes the diff.

**Graph** — the code graph (needs `stella init`). `/` or `⏎` opens the file
picker to re-root the neighborhood on any indexed file.

**SKILLS** — manage, search, create skills.
`Space` enable/disable · `Ctrl-X` ×2 delete · `Ctrl-O` preview · `e` edit ·
`p` pin · `n` new (LLM) · `←` `→` switch panes.

**MCP** — MCP servers. `e`/`Space` toggle · `a` auth · `x` remove · `/` search.

## Slash commands

| Command | Action |
|---|---|
| `/help` | This overlay |
| `/clear` | Reset the conversation |
| `/models` | List providers & models |
| `/init` | Index the workspace (domains + code graph) |
| `/pipeline` | Toggle the staged pipeline (witness-verified turns) |
| `/export` | Export session telemetry — ZIP + HTML dashboard |
| `/donate` | Support stella — GitHub Sponsors link |
| `/files` `/diff` | Open the Files tab / diff viewer |
| `/graph` | Open the code-graph tab |
| `/agents` | Open the Agents tab |
| `/skills` | Open the SKILLS tab |
| `/mcp` | Open the MCP tab |

Custom ⚡ commands and skills from `.stella/agents` and `.stella/skills` also
appear in the `/` popup — type `/<name> <args>` to run them.

---

`↑` `↓` scroll · `Esc` / `q` / `?` close
