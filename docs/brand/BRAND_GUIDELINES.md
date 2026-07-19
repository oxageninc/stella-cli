# Stella Brand Guidelines

Stella's visual identity is **aurora light on navy black**: a near-black navy
ground with cool, bright accents that sweep cyan → azure → violet. The single
source of truth for the palette is the TUI theme
([`stella-tui/src/theme.rs`](../../stella-tui/src/theme.rs)) — every other
surface (CLI output, Observatory, docs site, brand assets) follows it. A
regression test there (`no_ember_amber_survives_anywhere`) permanently retires
the old ember-gold system.

## The mark

The mark is a terminal prompt: a `>` chevron beside a 3×3 dot grid — a
command line addressing a fleet. It renders in the aurora gradient on dark
grounds, or as a flat single-color glyph where gradients can't be used.

| Asset | Use |
| --- | --- |
| `marks/mark.svg` | Primary mark — aurora gradient. Default on any dark ground. |
| `marks/mark-cells.svg` | Gradient variant with per-cell emphasis. |
| `marks/mark-flat.svg` | Flat aurora-cyan mark for single-color contexts. |
| `marks/mark-navy.svg` | Solid navy mark for light grounds. |
| `marks/mark-ice.svg` | Solid ice mark for dark, low-chroma contexts. |
| `wordmarks/wordmark-aurora.svg` | "stella" wordmark in aurora cyan. |
| `wordmarks/wordmark-navy.svg` / `wordmarks/wordmark-ice.svg` | Wordmark for light / dark grounds. |
| `lockups/stella-logo-dark.svg` | Mark + wordmark lockup for dark backgrounds (README hero). |
| `lockups/stella-logo-light.svg` | Mark + wordmark lockup for light backgrounds. |
| `icons/appicon.svg` | App icon — gradient mark on a navy tile. |
| `icons/favicon.svg` | Favicon-scale mark. |

## Palette

### Grounds & text

| Token | Hex | Role |
| --- | --- | --- |
| `GROUND` | `#050A18` | App background — navy black. |
| `SURFACE` | `#0A1226` | Cards, panels. |
| `RAISED` | `#101A33` | Raised panels. |
| `HAIRLINE` | `#1B2A4A` | Borders, rules. |
| `TEXT_PRIMARY` | `#F2F6FF` | Primary text ("ice"). |
| `TEXT_SECONDARY` | `#A9B7D6` | Secondary text. |
| `TEXT_TERTIARY` | `#7285A8` | Labels, captions. |
| `TEXT_DIM` | `#5D6C8A` | Quietest legible text. |

### Aurora accents

| Token | Hex | Role |
| --- | --- | --- |
| `AURORA_CYAN` (`ACCENT`) | `#3FE0FF` | The brand accent — glyphs, headers, the prompt `>>>`. |
| `AURORA_AZURE` (`ACCENT_DEEP`) | `#4D9FFF` | Live/active states. |
| `VIOLET` | `#A78BFA` | Interactive chrome, links, focus. |
| `AGENT_ICE` | `#A8C7F0` | Agent transcript voice. |

The **aurora gradient** runs `#3FE0FF → #4D9FFF → #A78BFA` (cyan → azure →
violet) and is the brand's signature sweep — progress fills, the primary mark,
the app icon.

### Semantic colors

| Token | Hex | Role |
| --- | --- | --- |
| `SUCCESS` / `SUCCESS_BRIGHT` | `#1D9E75` / `#3FD69B` | Success. |
| `WARNING` / `WARNING_BRIGHT` | `#BA7517` / `#F4B24A` | Warnings — the **only** permitted warm tones. |
| `AURORA_MAGENTA` (`DANGER`) / `DANGER_BRIGHT` | `#E4408F` / `#FF5C8A` | Failure. |

### The rule: no warm brand tones

Warm gold/amber/orange accents are **banned** as brand colors everywhere in
Stella — TUI, CLI output, Observatory, docs styling, and these assets. Warm
tones may appear only with *semantic warning* meaning (`WARNING` /
`WARNING_BRIGHT`). The retired ember palette (`#FFD97A`, `#F5B33C`,
`#E08A20`, `#FFAC26`, ink `#15120C`, paper `#FBF7EF`) must not be reintroduced;
`theme.rs` enforces this with a test for the TUI.

## Typography

No webfonts. The brand is system-native:

- **Sans:** `-apple-system, BlinkMacSystemFont, "Segoe UI", Inter, Arial, sans-serif`
- **Mono (code, terminal, the wordmark's voice):** `ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace`

## Asset inventory & status

- `marks/`, `wordmarks/`, `lockups/`, `icons/` — **current**, aurora-palette
  SVGs. Use these.
- `legacy/` — the retired ember-gold cuts (SV/PNG rasters and the old
  `stella-logo.html` brand board), kept for provenance only. Do not ship them.
  The gold rasters (`appicon-512.png`, `favicon-32.png`,
  `lockup-horizontal-dark-2048.png`, `mark-primary-1600.png`) have no aurora
  re-render yet — regenerate from the current SVGs when raster sizes are
  needed.
- The docs site keeps its own copies under `stella-docs/public/brand/` and
  `stella-docs/public/icons/`, and the Observatory embeds
  `stella-observatory/src/assets/mark.svg`. When brand art changes, update
  those copies too.
