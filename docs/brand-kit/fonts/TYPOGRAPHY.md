# Typography

## Wordmark typeface — DejaVu Sans Mono

The **STELLA** wordmark is set in **DejaVu Sans Mono**, Regular weight, all caps,
tracked. Its even monospace rhythm and clean, geometric sans forms pair naturally
with the geometric star mark, and it reads as confident and modern at any size.

- **Standalone / horizontal lockup:** letter-spacing `0.30em`
- **Vertical lockup:** letter-spacing `0.22em`
- **Case:** always uppercase — never mixed case, italic, or condensed
- **Minimum:** cap height ≥ 12px; below that use the mark alone

The wordmark is delivered as **outlined vector paths** (`wordmark-*.svg`), so it
renders identically on every device with no web-font loading and no licensing
dependency at runtime.

### Licensing
DejaVu Sans Mono is released under the **DejaVu Fonts License** (a permissive,
Bitstream-Vera–derived license). It is free to use, embed, and distribute,
including for commercial work. See https://dejavu-fonts.github.io/License.html

## Want a different monospace?

This kit was produced in a build environment without access to Google Fonts or
package registries, so the wordmark was set in the best professional monospace
available there (DejaVu Sans Mono). The pipeline is **font-agnostic**: provide a
licensed `.ttf`/`.otf` — e.g. **Space Mono**, **DM Mono**, **JetBrains Mono**,
**IBM Plex Mono**, or **Commit Mono** — and every wordmark and lockup regenerates
from the identical metrics and spacing rules. Nothing else in the system changes.

## UI / body pairing (recommendation)

For product UI, pair the mark with a neutral humanist sans for body text
(e.g. Inter, or the system UI stack) and reserve a monospace for small labels,
code, and metadata to echo the wordmark.
