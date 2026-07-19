# Stella CLI Docs

The documentation site for the [Stella CLI](https://github.com/macanderson/stella) —
destined for **stella.oxagen.sh**.

Built with [Next.js](https://nextjs.org) (App Router) + [Fumadocs](https://fumadocs.dev)
(`fumadocs-core` / `fumadocs-ui` / `fumadocs-mdx`) + Tailwind CSS v4. Branded with the
Stella identity — the aurora chevron+cells mark on a navy-black/Ice palette (see
`src/app/global.css` for the tokens, `public/brand/` for the logo lockups, and
`../docs/brand/BRAND_GUIDELINES.md` for the brand system).

## Develop

```bash
pnpm install
pnpm dev          # http://localhost:3400
```

## Build

```bash
pnpm build        # static export-friendly Next build
pnpm start        # serve the production build on :3400
pnpm typecheck    # tsc --noEmit
```

## Structure

```
content/docs/            # all documentation (MDX + meta.json ordering)
  index.mdx              # Introduction
  getting-started/       # installation, initialization, providers
  api-providers/         # per-provider pages + the live model catalog
  inference-pipeline.mdx # the staged pipeline: triage → … → judge
  context-engine.mdx     # bi-temporal memory, recall, citation loop
  agent-modes/           # chat / run / goal / monitor + goal-mode deep dive
  agent-fleets.mdx       # parallel worker fleets in git worktrees
  agent-tools/           # built-in tools, skills, permissions, custom, MCP, hooks
  configuration/         # settings.json scopes, agent-engine config, credentials
  examples/              # cost/quality profiles (dirt-cheap → max-quality)
  telemetry/             # local SQLite metering, Observatory, files-touched
  principles/            # determinism + the papers
  commands/              # per-command reference (run, chat, goal, fleet, …)
  extensions.mdx         # the extension event bus
  scripting.mdx          # headless JSON output for CI

src/app/                 # Next.js App Router
  (home)/                # marketing landing page
  docs/                  # Fumadocs docs shell
  api/search/            # Fumadocs search route
src/lib/source.ts        # Fumadocs content source loader
src/mdx-components.tsx   # MDX component map
```

## Add or edit a page

1. Create/edit an `.mdx` file under `content/docs/`. Every page starts with frontmatter:

   ```mdx
   ---
   title: Page Title
   description: One-sentence summary shown in search and metadata.
   ---
   ```

2. Add its slug to the nearest `meta.json` `pages` array to place it in the sidebar. Use
   `"---Label---"` entries for section separators.

3. In prose, wrap any `<placeholder>` or `{brace}` in backticks — a bare `<` or `{`
   breaks MDX parsing.

## Deploy

Deploys as a standard Next.js app. On Vercel, the project auto-detects Next.js + pnpm; set
the production domain to `stella.oxagen.sh`. `pnpm-workspace.yaml` approves the `esbuild` /
`sharp` build scripts so `pnpm install` exits cleanly in CI.
