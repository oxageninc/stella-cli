# Stella — Brand Kit

A complete, production-ready identity kit for **Stella**: an abstract four-point
celestial star in warm gold, with a monospace wordmark, animated loader,
full PWA icon set, and native-resolution wallpapers.

Open **`brand-guidelines.html`** in any browser for the full visual guide with
exact colors, ratios, and type specs (the loader animates live there).

---

## Contents

```
stella-brand-kit/
├── brand-guidelines.html        Full brand guide (open this first)
├── logo/
│   ├── logomark.svg             Primary mark (gold gradient + inner facet)
│   ├── logomark-gold-flat.svg   1-color gold
│   ├── logomark-ink.svg         Solid ink
│   ├── logomark-paper.svg       Solid paper (reversed)
│   ├── logomark-1024.png        Transparent raster, 1024²
│   ├── wordmark-ink.svg         "STELLA" outlined, ink   (tracking .30)
│   ├── wordmark-paper.svg       "STELLA" outlined, paper (tracking .30)
│   ├── wordmark-*-tight.svg     tracking .22 (for vertical lockup)
│   ├── lockup-horizontal-{light,dark}.svg   + 2048px PNGs
│   └── lockup-vertical-{light,dark}.svg     + 1024px PNG
├── loader/
│   └── stella-loader.svg        Animated build-on star (2.4s loop, SMIL, no JS)
├── icons/
│   ├── favicon.svg  favicon.ico  favicon-{16,32,48}.png
│   ├── icon-{192,512}.png        PWA "any" (transparent)
│   ├── maskable-{192,512}.png    PWA maskable (full-bleed, safe centre)
│   ├── apple-touch-icon-180.png  iOS home screen
│   └── appicon-{512,1024}.png    Rounded product icon
├── wallpapers/
│   ├── wallpaper-iphone-promax-1290x2796.png
│   ├── wallpaper-iphone-16promax-1320x2868.png
│   ├── wallpaper-desktop-4k-3840x2160.png
│   └── wallpaper-desktop-5k-5120x2880.png
├── social/
│   ├── og-card-1200x630.png      Open Graph / Twitter card
│   └── pwa-splash-1290x2796.png
├── web/
│   ├── manifest.webmanifest      PWA manifest
│   └── head-snippet.html         Drop-in <head> tags
└── fonts/
    └── TYPOGRAPHY.md             Font choice + licensing
```

## Colors

| Token     | Hex       | Use                       |
|-----------|-----------|---------------------------|
| Highlight | `#FFD97A` | gradient top / glints     |
| Gold      | `#F5B33C` | core brand color          |
| Amber     | `#E08A20` | gradient base / accents   |
| Ink       | `#15120C` | text on light             |
| Night     | `#0B0A08` | dark surfaces / PWA theme |
| Paper     | `#FBF7EF` | light surface             |

Gradient: `#FFD97A → #F5B33C (48%) → #E08A20`, top-left → bottom-right.

## Wire up the PWA

1. Copy `web/`, `icons/`, and `social/` to your web root.
2. Paste the tags from `web/head-snippet.html` into your `<head>`
   (adjust the paths if you don't deploy at the root).
3. Serve `manifest.webmanifest` and you're installable.

## Regenerating with a different font

The wordmark ships as **vector outlines**, so it needs no font at runtime.
To rebrand onto a licensed monospace (Space Mono, DM Mono, JetBrains Mono, …),
supply the `.ttf` and the lockups regenerate from the same metrics — every
ratio in the guide stays intact.

---
© Stella. Assets are yours to use. Font licensing in `fonts/TYPOGRAPHY.md`.
