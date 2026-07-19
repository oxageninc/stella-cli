#!/usr/bin/env node
/**
 * Charlie brand asset generator.
 *
 * Emits every brand asset (marks, wordmarks, lockups, icons, glyphs, loader,
 * splash screens, wallpapers, textures) in six variants:
 *
 *   adaptive       — light geometry + <style> @media(prefers-color-scheme) overrides
 *   light / dark   — non-adaptive color variants
 *   mono-light     — single-color ink line art (for light grounds)
 *   mono-dark      — single-color milk line art (for dark grounds)
 *   mono-adaptive  — mono line art that flips ink↔milk with the OS theme
 *
 * Adaptive trick: every element carries BOTH concrete presentation attributes
 * (from the light paint set) and a class hook; the adaptive variant appends a
 * <style> block whose media-query rules override the attributes (CSS always
 * beats presentation attributes). Rasterizers that ignore CSS (librsvg) render
 * the light state, which is why PNGs are only cut from the non-adaptive files.
 *
 * Usage: node docs/brand/build.mjs [--svg-only]
 */

import { mkdirSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = dirname(fileURLToPath(import.meta.url));
const SVG_ONLY = process.argv.includes('--svg-only');

/* ─── palette ────────────────────────────────────────────────────────────── */

export const C = {
  // nebula gradient (the signature sweep — warm sunset, never ice)
  corona:  '#FF6D4D', // coral orange
  flare:   '#F5487F', // warm pink
  orchid:  '#A24BEA', // violet
  // accents
  starlight: '#FFC24D', // gold — stars, sparkles, antenna tips
  caramel:   '#DE8F55', // Charlie's ears
  fur:       '#F2BD79', // Charlie's golden coat
  muzzle:    '#F9EDDC', // Charlie's muzzle and chest
  // neutrals
  ink:  '#2A1A35', // warm plum-black — text/line art on light grounds
  milk: '#FFF6E9', // warm white — text/line art on dark grounds
  // dark grounds (deep space plum)
  void900: '#0E0916',
  void800: '#171021',
  void700: '#241833',
  // light grounds (warm cream)
  cream50:  '#FFFAF0',
  cream100: '#F9F0E1',
  cream200: '#EFE2CC',
};

/* Paint sets. `mono` sets use a single color `c`. */
const PAINTS = {
  light: {
    id: 'light', mono: false,
    text: C.ink, glass: C.void700, glassOp: 1,
    head: C.fur, muzzle: C.muzzle, ear: C.caramel, face: C.ink, star: C.starlight,
    ground: C.cream100, groundHi: C.cream50, groundLo: C.cream200,
    dot: C.ink, dotOp: 0.5, washOp: 0.35,
  },
  dark: {
    id: 'dark', mono: false,
    text: C.milk, glass: C.milk, glassOp: 0.07,
    head: C.fur, muzzle: C.muzzle, ear: C.caramel, face: C.ink, star: C.starlight,
    ground: C.void800, groundHi: C.void900, groundLo: C.void700,
    dot: C.milk, dotOp: 0.9, washOp: 0.5,
  },
  'mono-light': { id: 'mono-light', mono: true, c: C.ink },
  'mono-dark':  { id: 'mono-dark',  mono: true, c: C.milk },
};

const NEBULA_STOPS = `<stop offset="0" stop-color="${C.corona}"/><stop offset=".55" stop-color="${C.flare}"/><stop offset="1" stop-color="${C.orchid}"/>`;
const nebulaDef = (id, x1, y1, x2, y2) =>
  `<linearGradient id="${id}" x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}" gradientUnits="userSpaceOnUse">${NEBULA_STOPS}</linearGradient>`;

/* Adaptive <style> blocks: overrides applied when the OS is in dark mode.   */
const darkCss = {
  color: `.ch-glass{fill:${C.milk};fill-opacity:.07}.ch-text{stroke:${C.milk}}.ch-bg{fill:url(#bgDark)}.ch-dot{fill:${C.milk};opacity:.9}.ch-wash{opacity:.5}`,
  mono:  `.ch-cs{stroke:${C.milk}}.ch-cf{fill:${C.milk}}`,
};
const styleBlock = (mono) =>
  `<style>@media (prefers-color-scheme: dark){${mono ? darkCss.mono : darkCss.color}}</style>`;

/* ─── helpers ────────────────────────────────────────────────────────────── */

const r2 = (n) => Math.round(n * 100) / 100;

/** Deterministic PRNG so texture/starfield layouts are stable across builds. */
function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a |= 0; a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Four-point star (kite) path. */
function star4(cx, cy, r, ri = r * 0.36) {
  const p = [
    [cx, cy - r], [cx + ri, cy - ri], [cx + r, cy], [cx + ri, cy + ri],
    [cx, cy + r], [cx - ri, cy + ri], [cx - r, cy], [cx - ri, cy - ri],
  ].map(([x, y]) => `${r2(x)} ${r2(y)}`).join(' L ');
  return `M ${p} Z`;
}

/** Paw print centered at 0,0 within ~a 22-unit box (scale/rotate at use site). */
function pawShapes(fill, extra = '') {
  return `<g ${extra} fill="${fill}">
    <ellipse cx="0" cy="4" rx="5.1" ry="4.3"/>
    <circle cx="-5.7" cy="-2.4" r="2.2"/><circle cx="-2" cy="-5.2" r="2.2"/>
    <circle cx="2" cy="-5.2" r="2.2"/><circle cx="5.7" cy="-2.4" r="2.2"/>
  </g>`;
}

/** Scatter seeded stars in a w×h box. Returns { dots, sparks } markup. */
function starfield(seed, n, w, h, { fill, op = 1, cls = '', sparkEvery = 9, gold = C.starlight, monoC = null } = {}) {
  const rnd = mulberry32(seed);
  let dots = '', sparks = '';
  for (let i = 0; i < n; i++) {
    const x = r2(rnd() * w), y = r2(rnd() * h);
    const isSpark = i % sparkEvery === 0;
    if (isSpark) {
      const r = r2(3 + rnd() * 5);
      const c = monoC ?? (rnd() < 0.55 ? gold : fill);
      sparks += `<path class="${cls}" d="${star4(x, y, r)}" fill="${c}" opacity="${r2(0.5 + rnd() * 0.5)}"/>`;
    } else {
      const r = r2(0.7 + rnd() * 1.7);
      dots += `<circle class="${cls}" cx="${x}" cy="${y}" r="${r}" fill="${monoC ?? fill}" opacity="${r2(op * (0.25 + rnd() * 0.75))}"/>`;
    }
  }
  return dots + sparks;
}

const svgDoc = (viewBox, w, h, body) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="${viewBox}" width="${w}" height="${h}">\n${body}\n</svg>\n`;

/* ─── the logomark: Charlie, the astronaut pup ─────────────────────────────── */
/* viewBox 0 0 120 120; helmet center (60,63) r40, antenna to y≈6.           */

/* Mono building blocks, shared by markBody and loaderSvg. Ears are short
 * lobes masked at the head radius and drawn before the head outline, so the
 * outline overlaps the join and they sprout from it like the color version.
 * The sparkle is a star-shaped knockout punched through the ring at 45°. */
const MONO_DEFS = `<defs>
    <mask id="mEars"><rect width="120" height="120" fill="#fff"/><circle cx="60" cy="64" r="24" fill="#000"/></mask>
    <mask id="mRing"><rect width="120" height="120" fill="#fff"/><path d="${star4(88.3, 34.7, 10.5, 3.8)}" fill="#000"/></mask>
  </defs>`;
const monoEars = (c, extra = '') => `<g ${extra} mask="url(#mEars)">
    <rect class="ch-cf" x="30.5" y="38" width="13" height="30" rx="6.5" transform="rotate(20 37 53)" fill="${c}"/>
    <rect class="ch-cf" x="76.5" y="38" width="13" height="30" rx="6.5" transform="rotate(-20 83 53)" fill="${c}"/>
  </g>`;
const monoRing = (c, cls = 'ch-cs') =>
  `<g mask="url(#mRing)"><circle class="${cls}" cx="60" cy="63" r="40" fill="none" stroke="${c}" stroke-width="7"/></g>`;

/* Shared face geometry: eyes y60, cream muzzle behind an ink nose at y69.5. */
const mouthPath = (stroke, w, cls = '') =>
  `<path class="${cls}" d="M60 72.5 L60 76.5 M60 76.5 C57 79.6 54 78.6 53 76.4 M60 76.5 C63 79.6 66 78.6 67 76.4" fill="none" stroke="${stroke}" stroke-width="${w}" stroke-linecap="round"/>`;

function markBody(p, { gradId = 'nebula', simplified = false } = {}) {
  if (p.mono) {
    const c = p.c;
    return `
  ${MONO_DEFS}
  <line class="ch-cs" x1="60" y1="20" x2="60" y2="12" stroke="${c}" stroke-width="4" stroke-linecap="round"/>
  <circle class="ch-cf" cx="60" cy="9.5" r="3.4" fill="${c}"/>
  <rect class="ch-cf" x="42" y="96" width="36" height="13" rx="6" fill="${c}"/>
  ${monoEars(c)}
  <circle class="ch-cs" cx="60" cy="64" r="24" fill="none" stroke="${c}" stroke-width="4"/>
  <circle class="ch-cf" cx="51" cy="60" r="${simplified ? 4 : 3.3}" fill="${c}"/>
  <circle class="ch-cf" cx="69" cy="60" r="${simplified ? 4 : 3.3}" fill="${c}"/>
  <ellipse class="ch-cf" cx="60" cy="69.5" rx="${simplified ? 5.4 : 4.6}" ry="${simplified ? 4.2 : 3.6}" fill="${c}"/>
  ${mouthPath(c, simplified ? 2.4 : 1.8, 'ch-cs')}
  ${monoRing(c)}
  ${simplified ? '' : `<circle class="ch-cf" cx="38" cy="86" r="1.8" fill="${c}"/>`}`;
  }
  const eyeR = simplified ? 4 : 3.3, noseRx = simplified ? 5.4 : 4.6, noseRy = simplified ? 4.2 : 3.6;
  return `
  <line x1="60" y1="20" x2="60" y2="12" stroke="url(#${gradId})" stroke-width="4" stroke-linecap="round"/>
  <circle cx="60" cy="9.5" r="3.4" fill="${C.starlight}"/>
  <rect x="40" y="94" width="40" height="16" rx="7" fill="url(#${gradId})"/>
  <circle class="ch-glass" cx="60" cy="63" r="40" fill="${p.glass}" fill-opacity="${p.glassOp}"/>
  <g fill="${p.ear}">
    <rect x="34.5" y="40" width="14" height="37" rx="7" transform="rotate(16 41.5 58)"/>
    <rect x="71.5" y="40" width="14" height="37" rx="7" transform="rotate(-16 78.5 58)"/>
  </g>
  <circle cx="60" cy="64" r="24" fill="${p.head}"/>
  <ellipse cx="60" cy="71.5" rx="${simplified ? 12.5 : 11.5}" ry="${simplified ? 10 : 9}" fill="${p.muzzle}"/>
  <circle cx="51" cy="60" r="${eyeR}" fill="${p.face}"/>
  <circle cx="69" cy="60" r="${eyeR}" fill="${p.face}"/>
  <ellipse cx="60" cy="69.5" rx="${noseRx}" ry="${noseRy}" fill="${p.face}"/>
  ${mouthPath(p.face, simplified ? 2.4 : 1.8)}
  <path d="M31 43 A 34 34 0 0 1 46 29" fill="none" stroke="${C.milk}" stroke-opacity="0.4" stroke-width="4" stroke-linecap="round"/>
  <circle cx="60" cy="63" r="40" fill="none" stroke="url(#${gradId})" stroke-width="7"/>
  <path d="${star4(75.5, 36, simplified ? 9 : 7.5)}" fill="${p.star}"/>
  ${simplified ? '' : `<circle cx="38" cy="86" r="1.8" fill="${p.star}"/>`}`;
}

/* ─── the star mark: dogless compact logo (the wordmark's star, enlarged) ── */

function starmarkBody(p, gradId = 'nebula') {
  if (p.mono) {
    return `<path class="ch-cf" d="${star4(56, 64, 38, 13.7)}" fill="${p.c}"/>
  <path class="ch-cf" d="${star4(93, 29, 11, 4)}" fill="${p.c}"/>`;
  }
  return `<path d="${star4(56, 64, 38, 13.7)}" fill="url(#${gradId})"/>
  <path d="${star4(93, 29, 11, 4)}" fill="${C.starlight}"/>`;
}

const starmarkSvg = (p, adaptive) =>
  svgDoc('0 0 120 120', 512, 512,
    (adaptive ? styleBlock(p.mono) : '') +
    (p.mono ? '' : `<defs>${nebulaDef('nebula', 18, 26, 94, 102)}</defs>`) +
    starmarkBody(p));

/* ─── full-body Charlie poses ────────────────────────────────────────────── */
/* Local coords per pose; head cluster is shared (head center at 0,0, helmet
 * ring r24). Mono poses are solid silhouettes — no facial features. */

function poseHead(p) {
  if (p.mono) {
    const c = p.c;
    return `<g fill="${c}" class="ch-cf">
      <rect x="-15.5" y="-12" width="9" height="23" rx="4.5" transform="rotate(18 -11 -3)"/>
      <rect x="6.5" y="-12" width="9" height="23" rx="4.5" transform="rotate(-18 11 -3)"/>
      <circle cx="0" cy="0" r="15"/></g>
    <circle class="ch-cs" cx="0" cy="-1" r="24" fill="none" stroke="${c}" stroke-width="4.6"/>
    <line class="ch-cs" x1="0" y1="-25" x2="0" y2="-30" stroke="${c}" stroke-width="2.6" stroke-linecap="round"/>
    <circle class="ch-cf" cx="0" cy="-31.8" r="2.2" fill="${c}"/>`;
  }
  return `<g fill="${p.ear}">
      <rect x="-15.5" y="-12" width="9" height="23" rx="4.5" transform="rotate(18 -11 -3)"/>
      <rect x="6.5" y="-12" width="9" height="23" rx="4.5" transform="rotate(-18 11 -3)"/>
    </g>
    <circle cx="0" cy="0" r="15" fill="${p.head}"/>
    <ellipse cx="0" cy="4.5" rx="7.2" ry="5.6" fill="${p.muzzle}"/>
    <circle cx="-5.6" cy="-2.5" r="2.1" fill="${p.face}"/>
    <circle cx="5.6" cy="-2.5" r="2.1" fill="${p.face}"/>
    <ellipse cx="0" cy="3.4" rx="2.9" ry="2.3" fill="${p.face}"/>
    <path d="M0 5.3 L0 7.8 M0 7.8 C-1.9 9.7 -3.7 9.1 -4.3 7.7 M0 7.8 C1.9 9.7 3.7 9.1 4.3 7.7" fill="none" stroke="${p.face}" stroke-width="1.2" stroke-linecap="round"/>
    <path d="M-18 -13 A 19 19 0 0 1 -8 -21" fill="none" stroke="${C.milk}" stroke-opacity="0.4" stroke-width="2.6" stroke-linecap="round"/>
    <circle cx="0" cy="-1" r="24" fill="none" stroke="url(#nebula)" stroke-width="4.6"/>
    <line x1="0" y1="-25" x2="0" y2="-30" stroke="url(#nebula)" stroke-width="2.6" stroke-linecap="round"/>
    <circle cx="0" cy="-31.8" r="2.2" fill="${C.starlight}"/>`;
}

function poseBody(name, p) {
  const glass = (cx, cy) => p.mono ? '' :
    `<circle class="ch-glass" cx="${cx}" cy="${cy}" r="24" fill="${p.glass}" fill-opacity="${p.glassOp}"/>`;
  const fur = p.mono ? p.c : p.head;
  const chest = p.mono ? p.c : p.muzzle;
  const collar = p.mono ? p.c : 'url(#nebula)';
  const star = p.mono ? p.c : C.starlight;
  const F = `fill="${fur}" class="ch-cf"`, CH = `fill="${chest}" class="ch-cf"`;
  const T = `fill="none" class="ch-cs" stroke="${fur}" stroke-width="7" stroke-linecap="round"`;
  switch (name) {
    case 'float': return { viewBox: '0 0 168 126', body: `
  <path d="M128 96 C140 90 144 78 138 68" ${T}/>
  <rect x="108" y="98" width="8" height="19" rx="4" ${F} transform="rotate(40 112 100)"/>
  <rect x="118" y="84" width="8" height="19" rx="4" ${F} transform="rotate(75 122 86)"/>
  <rect x="62" y="62" width="66" height="34" rx="17" ${F} transform="rotate(18 95 79)"/>
  <ellipse cx="78" cy="76" rx="14" ry="11" ${CH} transform="rotate(18 78 76)"/>
  <rect x="64" y="82" width="8" height="20" rx="4" ${F} transform="rotate(30 68 84)"/>
  <rect x="79" y="89" width="8" height="20" rx="4" ${F} transform="rotate(14 83 91)"/>
  ${glass(40, 37)}
  <rect x="48" y="58" width="22" height="11" rx="5" fill="${collar}" class="ch-cf" transform="rotate(38 59 63)"/>
  <g transform="translate(40 38) rotate(-14)">${poseHead(p)}</g>
  <path d="${star4(12, 15, 9, 3.2)}" fill="${star}" class="ch-cf"/>` };
    case 'chase': return { viewBox: '0 -2 170 136', body: `
  <path d="M32 108 C20 112 10 108 6 98" ${T}/>
  <rect x="36" y="100" width="8" height="22" rx="4" ${F} transform="rotate(35 40 102)"/>
  <rect x="46" y="104" width="8" height="22" rx="4" ${F} transform="rotate(58 50 106)"/>
  <rect x="34" y="64" width="70" height="34" rx="17" ${F} transform="rotate(-26 69 81)"/>
  <ellipse cx="92" cy="66" rx="13" ry="11" ${CH} transform="rotate(-26 92 66)"/>
  <rect x="86" y="80" width="8" height="21" rx="4" ${F} transform="rotate(26 90 82)"/>
  <rect x="93" y="83" width="8" height="21" rx="4" ${F} transform="rotate(18 97 85)"/>
  ${glass(116, 33)}
  <rect x="94" y="50" width="22" height="11" rx="5" fill="${collar}" class="ch-cf" transform="rotate(-24 105 55)"/>
  <g transform="translate(116 34) rotate(18)">${poseHead(p)}</g>
  <path d="${star4(152, 19.2, 11.2, 4)}" fill="${star}" class="ch-cf"/>` };
    case 'sit': return { viewBox: '20 -8 100 146', body: `
  <path d="M76 128 C90 130 100 124 102 114" ${T}/>
  <ellipse cx="62" cy="112" rx="26" ry="22" ${F}/>
  <rect x="44" y="58" width="36" height="64" rx="18" ${F} transform="rotate(6 62 90)"/>
  <ellipse cx="57" cy="100" rx="13" ry="26" ${CH} transform="rotate(6 57 100)"/>
  <rect x="47" y="102" width="8" height="28" rx="4" ${F}/>
  <rect x="61" y="104" width="8" height="28" rx="4" ${F}/>
  ${glass(58, 29)}
  <rect x="44" y="46" width="24" height="11" rx="5" fill="${collar}" class="ch-cf" transform="rotate(10 56 51)"/>
  <g transform="translate(58 30) rotate(10)">${poseHead(p)}</g>
  <path d="${star4(100, 18, 10, 3.6)}" fill="${star}" class="ch-cf"/>` };
    default: throw new Error(`unknown pose ${name}`);
  }
}

function poseSvg(name, p, adaptive) {
  const { viewBox, body } = poseBody(name, p);
  const [, , vw] = viewBox.split(' ').map(Number);
  return svgDoc(viewBox, vw * 4, Math.round(Number(viewBox.split(' ')[3]) * 4),
    (adaptive ? styleBlock(p.mono) : '') +
    (p.mono ? '' : `<defs>${nebulaDef('nebula', 0, 0, 150, 140)}</defs>`) +
    body);
}

const markSvg = (p, adaptive, opts = {}) =>
  svgDoc('0 0 120 120', 512, 512,
    (adaptive ? styleBlock(p.mono) : '') +
    (p.mono ? '' : `<defs>${nebulaDef('nebula', 20, 23, 100, 103)}</defs>`) +
    markBody(p, opts));

/* ─── the wordmark: monoline rounded "stella" + gold sparkle ─────────────── */
/* viewBox 0 0 320 120; baseline 94, x-height 42, ascender 26.               */

function wordmarkGlyphs(strokeColor, cls) {
  return `<g class="${cls}" fill="none" stroke="${strokeColor}" stroke-width="13" stroke-linecap="round" stroke-linejoin="round">
    <path d="M50 54 C46.5 49 37 47.5 31 51.5 C25.5 55.5 26.5 63 33.5 66 L43.5 70 C50.5 73 51.5 81 46 85 C40 89.5 29.5 88 25 82.5"/>
    <path d="M76 26 L76 79 A 15 15 0 0 0 91 94"/>
    <path d="M64 44 L89 44"/>
    <path d="M107 68 L143 68 A 18 18 0 1 0 138 81"/>
    <path d="M160 26 L160 79 A 15 15 0 0 0 175 94"/>
    <path d="M193 26 L193 79 A 15 15 0 0 0 208 94"/>
    <circle cx="243" cy="68" r="19"/>
    <path d="M262 52 L262 94"/>
  </g>`;
}

function wordmarkBody(p) {
  const stroke = p.mono ? p.c : p.text;
  const starFill = p.mono ? p.c : C.starlight;
  const starCls = p.mono ? 'ch-cf' : 'ch-star';
  return wordmarkGlyphs(stroke, p.mono ? 'ch-cs' : 'ch-text') +
    `<path class="${starCls}" d="${star4(285, 39.5, 13, 4.3)}" fill="${starFill}"/>`;
}

const wordmarkSvg = (p, adaptive) =>
  svgDoc('0 0 320 120', 640, 240, (adaptive ? styleBlock(p.mono) : '') + wordmarkBody(p));

/* ─── lockups ────────────────────────────────────────────────────────────── */

const lockupHSvg = (p, adaptive) =>
  svgDoc('0 0 452 120', 904, 240,
    (adaptive ? styleBlock(p.mono) : '') +
    (p.mono ? '' : `<defs>${nebulaDef('nebula', 20, 23, 100, 103)}</defs>`) +
    `<g>${markBody(p)}</g>` +
    `<g transform="translate(124 6) scale(0.94)">${wordmarkBody(p)}</g>`);

const lockupVSvg = (p, adaptive) =>
  svgDoc('0 0 320 250', 640, 500,
    (adaptive ? styleBlock(p.mono) : '') +
    (p.mono ? '' : `<defs>${nebulaDef('nebula', 20, 23, 100, 103)}</defs>`) +
    `<g transform="translate(100 0)">${markBody(p)}</g>` +
    `<g transform="translate(40 130) scale(0.75)">${wordmarkBody(p)}</g>`);

/* ─── favicon (simplified mark) ──────────────────────────────────────────── */

const faviconSvg = (p, adaptive) =>
  svgDoc('0 0 120 120', 64, 64,
    (adaptive ? styleBlock(p.mono) : '') +
    (p.mono ? '' : `<defs>${nebulaDef('nebula', 20, 23, 100, 103)}</defs>`) +
    markBody(p, { simplified: true }));

/* ─── app icon + maskable icon (own grounds; light/dark only) ────────────── */

function iconGround(p, size, rx) {
  const rnd = mulberry32(77);
  return `
  <defs>
    ${nebulaDef('nebula', size * 0.17, size * 0.19, size * 0.83, size * 0.86)}
    <radialGradient id="wash1" cx="0.22" cy="0.2" r="0.75"><stop offset="0" stop-color="${C.corona}" stop-opacity="${p.id === 'light' ? 0.28 : 0.2}"/><stop offset="1" stop-color="${C.corona}" stop-opacity="0"/></radialGradient>
    <radialGradient id="wash2" cx="0.85" cy="0.9" r="0.85"><stop offset="0" stop-color="${C.orchid}" stop-opacity="${p.id === 'light' ? 0.24 : 0.3}"/><stop offset="1" stop-color="${C.orchid}" stop-opacity="0"/></radialGradient>
  </defs>
  <rect width="${size}" height="${size}" rx="${rx}" fill="${p.ground}"/>
  <rect width="${size}" height="${size}" rx="${rx}" fill="url(#wash1)"/>
  <rect width="${size}" height="${size}" rx="${rx}" fill="url(#wash2)"/>
  ${starfield(41, 26, size, size, { fill: p.dot, op: p.dotOp * 0.8, sparkEvery: 12 })}`;
}

/* App icon: rounded tile; the mark's own gradient ring on a plum/cream tile. */
const appiconSvg = (p) =>
  svgDoc('0 0 512 512', 512, 512,
    iconGround(p, 512, 116) +
    `<g transform="translate(64 58) scale(3.2)">${markBody(p, { gradId: 'nebula2' })}</g>` +
    `<defs>${nebulaDef('nebula2', 20, 23, 100, 103)}</defs>`);

/* Maskable: full-bleed square, pup within the inner 80% safe circle. */
const maskableSvg = (p) =>
  svgDoc('0 0 512 512', 512, 512,
    iconGround(p, 512, 0) +
    `<g transform="translate(104 96) scale(2.53)">${markBody(p, { gradId: 'nebula2', simplified: true })}</g>` +
    `<defs>${nebulaDef('nebula2', 20, 23, 100, 103)}</defs>`);

/* ─── glyph icon set (24×24, monoline stroke 1.8) ────────────────────────── */

function glyphBody(name, p) {
  const c = p.mono ? p.c : (p.id === 'dark' ? C.milk : C.ink);
  const gold = p.mono ? 'none' : C.starlight;
  const accent = (fill) => p.mono ? 'none' : fill;
  const S = `fill="none" stroke="${c}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"`;
  const CS = `class="ch-cs"`, CF = `class="ch-cf"`;
  switch (name) {
    case 'star':
      return `<path ${CS} ${CF.replace('class="ch-cf"', '')} d="${star4(12, 12, 8.5, 3.1)}" fill="${accent(C.starlight)}" stroke="${c}" stroke-width="1.8" stroke-linejoin="round"/>`;
    case 'sparkle':
      return `<path ${CS} d="${star4(11, 13, 7.5, 2.7)}" fill="${accent(C.starlight)}" stroke="${c}" stroke-width="1.8" stroke-linejoin="round"/><path ${CF} d="${star4(18.5, 5.5, 3, 1.1)}" fill="${c}"/>`;
    case 'orbit':
      return `<ellipse ${CS} cx="12" cy="12" rx="10" ry="5.6" transform="rotate(-24 12 12)" ${S}/><circle ${CF} cx="12" cy="12" r="3.1" fill="${accent(C.orchid) === 'none' ? c : C.orchid}"/><circle ${CF} cx="19.4" cy="6.2" r="1.9" fill="${c}"/>`;
    case 'rocket':
      return `<path ${CS} d="M12 2.8 C15 5.6 15.6 10.4 14.6 14.6 L9.4 14.6 C8.4 10.4 9 5.6 12 2.8 Z" ${S}/><circle ${CS} cx="12" cy="8.6" r="1.9" ${S}/><path ${CS} d="M9.6 12.4 L6.4 16.8 L9.4 15.9 M14.4 12.4 L17.6 16.8 L14.6 15.9" ${S}/><path ${CS} d="M12 17.2 L12 21" stroke="${p.mono ? c : C.corona}" stroke-width="1.8" stroke-linecap="round" fill="none"/>`;
    case 'planet':
      return `<circle ${CS} cx="12" cy="12" r="5.6" ${S}/><ellipse ${CS} cx="12" cy="12" rx="10.2" ry="3.1" transform="rotate(-16 12 12)" fill="none" stroke="${p.mono ? c : C.orchid}" stroke-width="1.8"/>`;
    case 'paw':
      return pawShapes(c, `${CF} transform="translate(12 12.5) scale(1.05)"`);
    case 'bone':
      return `<g ${CF} fill="${c}"><rect x="7" y="10.4" width="10" height="3.2" rx="1.6"/><circle cx="7" cy="10.2" r="2.5"/><circle cx="7" cy="13.8" r="2.5"/><circle cx="17" cy="10.2" r="2.5"/><circle cx="17" cy="13.8" r="2.5"/></g>`;
    case 'helmet':
      return `<circle ${CS} cx="12" cy="12" r="7.6" ${S}/><rect ${CF} x="8.4" y="19.4" width="7.2" height="2.6" rx="1.3" fill="${c}"/><path ${CS} d="M12 4.2 L12 2.6" ${S}/><circle ${CF} cx="12" cy="1.8" r="1.2" fill="${gold === 'none' ? c : gold}"/><path ${CF} d="${star4(14.8, 9.4, 2.4, 0.9)}" fill="${gold === 'none' ? c : gold}"/>`;
    default: throw new Error(`unknown glyph ${name}`);
  }
}

const GLYPHS = ['star', 'sparkle', 'orbit', 'rocket', 'planet', 'paw', 'bone', 'helmet'];
const glyphSvg = (name, p, adaptive) =>
  svgDoc('0 0 24 24', 24, 24, (adaptive ? styleBlock(true) : '') + glyphBody(name, p));

/* ─── animated loader: the pup assembles itself ──────────────────────────── */

function loaderSvg(p, adaptive) {
  const ringLen = r2(2 * Math.PI * 40); // 251.33
  const css = `
  <style>
    .ld-ring{stroke-dasharray:${ringLen};animation:ld-draw 3.4s cubic-bezier(.6,.05,.3,1) infinite}
    .ld-pop{transform-box:fill-box;transform-origin:center;animation:ld-pop 3.4s cubic-bezier(.34,1.56,.64,1) infinite}
    .ld-ant{animation-name:ld-ant}
    .ld-p1{animation-delay:.12s}.ld-p2{animation-delay:.2s}.ld-p3{animation-delay:.3s}
    .ld-p4{animation-delay:.42s}.ld-p5{animation-delay:.48s}.ld-p6{animation-delay:.55s}
    .ld-star{transform-box:fill-box;transform-origin:center;animation:ld-star 3.4s ease-in-out infinite}
    .ld-scene{animation:ld-fade 3.4s linear infinite}
    @keyframes ld-draw{0%{stroke-dashoffset:${ringLen}}22%,100%{stroke-dashoffset:0}}
    @keyframes ld-pop{0%,20%{transform:scale(0)}34%,100%{transform:scale(1)}}
    @keyframes ld-ant{0%,22%{transform:scale(0)}32%,100%{transform:scale(1)}}
    @keyframes ld-star{0%,58%{transform:scale(0) rotate(-90deg);opacity:0}70%{transform:scale(1.25) rotate(8deg);opacity:1}78%,100%{transform:scale(1) rotate(0deg);opacity:1}}
    @keyframes ld-fade{0%,88%{opacity:1}97%,100%{opacity:0}}
  </style>`;
  const c = p.mono ? p.c : null;
  const grad = p.mono ? '' : `<defs>${nebulaDef('nebula', 20, 23, 100, 103)}</defs>`;
  const adaptiveCss = adaptive ? styleBlock(p.mono) : '';
  // Rebuild the mark with loader classes on each stage.
  const ring = p.mono
    ? `<g mask="url(#mRing)"><circle class="ld-ring ch-cs" cx="60" cy="63" r="40" fill="none" stroke="${c}" stroke-width="7"/></g>`
    : `<circle class="ld-ring" cx="60" cy="63" r="40" fill="none" stroke="url(#nebula)" stroke-width="7"/>`;
  const antenna = p.mono
    ? `<g class="ld-pop ld-ant"><rect class="ch-cf" x="42" y="96" width="36" height="13" rx="6" fill="${c}"/><line class="ch-cs" x1="60" y1="20" x2="60" y2="12" stroke="${c}" stroke-width="4" stroke-linecap="round"/><circle class="ch-cf" cx="60" cy="9.5" r="3.4" fill="${c}"/></g>`
    : `<g class="ld-pop ld-ant"><rect x="40" y="94" width="40" height="16" rx="7" fill="url(#nebula)"/><line x1="60" y1="20" x2="60" y2="12" stroke="url(#nebula)" stroke-width="4" stroke-linecap="round"/><circle cx="60" cy="9.5" r="3.4" fill="${C.starlight}"/></g>`;
  const glass = p.mono ? '' :
    `<circle class="ch-glass" cx="60" cy="63" r="40" fill="${p.glass}" fill-opacity="${p.glassOp}"/>`;
  const ears = p.mono
    ? `<g class="ld-pop ld-p1">${monoEars(c)}</g>`
    : `<g class="ld-pop ld-p1" fill="${p.ear}"><rect x="34.5" y="40" width="14" height="37" rx="7" transform="rotate(16 41.5 58)"/><rect x="71.5" y="40" width="14" height="37" rx="7" transform="rotate(-16 78.5 58)"/></g>`;
  const head = p.mono
    ? `<circle class="ld-pop ld-p2 ch-cs" cx="60" cy="64" r="24" fill="none" stroke="${c}" stroke-width="4"/>`
    : `<circle class="ld-pop ld-p2" cx="60" cy="64" r="24" fill="${p.head}"/>`;
  const patch = p.mono ? '' :
    `<ellipse class="ld-pop ld-p3" cx="60" cy="71.5" rx="11.5" ry="9" fill="${p.muzzle}"/>`;
  const fc = p.mono ? c : p.face;
  const shine = p.mono ? '' :
    `<path class="ld-pop ld-p6" d="M31 43 A 34 34 0 0 1 46 29" fill="none" stroke="${C.milk}" stroke-opacity="0.4" stroke-width="4" stroke-linecap="round"/>`;
  const face = `
    <circle class="ld-pop ld-p4 ch-cf" cx="51" cy="60" r="3.3" fill="${fc}"/>
    <circle class="ld-pop ld-p4 ch-cf" cx="69" cy="60" r="3.3" fill="${fc}"/>
    <ellipse class="ld-pop ld-p5 ch-cf" cx="60" cy="69.5" rx="4.6" ry="3.6" fill="${fc}"/>
    ${mouthPath(fc, 1.8, 'ld-pop ld-p6 ch-cs')}${shine}`;
  // In mono the sparkle is the knockout in the ring (always "on"), so the
  // animated pop-in star only appears in the color variants.
  const star = p.mono ? '' : `<path class="ld-star" d="${star4(75.5, 36, 7.5)}" fill="${C.starlight}"/>`;
  const mask = p.mono ? MONO_DEFS : '';
  return svgDoc('0 0 120 120', 256, 256,
    adaptiveCss + css + grad + mask +
    `<g class="ld-scene">${glass}${ears}${head}${patch}${face}${ring}${antenna}${star}</g>`);
}

/* ─── splash screens ─────────────────────────────────────────────────────── */

function splashBody(p, w, h, adaptive) {
  const light = PAINTS.light, dark = PAINTS.dark;
  const defs = `<defs>
    ${nebulaDef('nebula', 20, 23, 100, 103)}
    <linearGradient id="bgLight" x1="0" y1="0" x2="0" y2="${h}" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="${light.groundHi}"/><stop offset="1" stop-color="${light.ground}"/></linearGradient>
    <linearGradient id="bgDark" x1="0" y1="0" x2="0" y2="${h}" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="${dark.groundHi}"/><stop offset="1" stop-color="${dark.ground}"/></linearGradient>
    <radialGradient id="spWash1" cx="0.15" cy="0.1" r="0.8"><stop offset="0" stop-color="${C.corona}" stop-opacity=".26"/><stop offset="1" stop-color="${C.corona}" stop-opacity="0"/></radialGradient>
    <radialGradient id="spWash2" cx="0.9" cy="0.95" r="0.9"><stop offset="0" stop-color="${C.orchid}" stop-opacity=".3"/><stop offset="1" stop-color="${C.orchid}" stop-opacity="0"/></radialGradient>
    <radialGradient id="spWash3" cx="0.85" cy="0.12" r="0.6"><stop offset="0" stop-color="${C.flare}" stop-opacity=".18"/><stop offset="1" stop-color="${C.flare}" stop-opacity="0"/></radialGradient>
  </defs>`;
  const bg = `<rect class="ch-bg" width="${w}" height="${h}" fill="url(#bg${p.id === 'dark' ? 'Dark' : 'Light'})"/>`;
  const washes = `<g class="ch-wash" opacity="${p.washOp}">
    <rect width="${w}" height="${h}" fill="url(#spWash1)"/>
    <rect width="${w}" height="${h}" fill="url(#spWash2)"/>
    <rect width="${w}" height="${h}" fill="url(#spWash3)"/>
  </g>`;
  const stars = `<g>${starfield(1969, Math.round(w * h / 26000), w, h, { fill: p.dot, op: p.dotOp * 0.7, cls: 'ch-dot', sparkEvery: 14 })}</g>`;
  const portrait = h > w;
  const lockupScale = portrait ? w / 560 : h / 640;
  const lw = 320 * lockupScale, lh = 250 * lockupScale;
  const lx = (w - lw) / 2, ly = portrait ? h * 0.40 - lh / 2 : h * 0.44 - lh / 2;
  const lockup = `<g transform="translate(${r2(lx)} ${r2(ly)}) scale(${r2(lockupScale)})">` +
    `<g transform="translate(100 0)">${markBody(p)}</g>` +
    `<g transform="translate(40 130) scale(0.75)">${wordmarkBody(p)}</g></g>`;
  const dotsY = portrait ? h * 0.82 : h * 0.84;
  const loadDots = `<g>
    <circle class="ch-dot" cx="${w / 2 - 26}" cy="${r2(dotsY)}" r="6" fill="${p.dot}" opacity="0.35"/>
    <circle cx="${w / 2}" cy="${r2(dotsY)}" r="6" fill="${C.starlight}"/>
    <circle class="ch-dot" cx="${w / 2 + 26}" cy="${r2(dotsY)}" r="6" fill="${p.dot}" opacity="0.35"/>
  </g>`;
  return (adaptive ? styleBlock(false) : '') + defs + bg + washes + stars + lockup + loadDots;
}

const splashSvg = (p, adaptive, w, h) => svgDoc(`0 0 ${w} ${h}`, w, h, splashBody(p, w, h, adaptive));

/* ─── wallpapers ─────────────────────────────────────────────────────────── */

function wallpaperBody(p, w, h, adaptive) {
  const light = PAINTS.light, dark = PAINTS.dark;
  const portrait = h > w;
  const defs = `<defs>
    ${nebulaDef('nebula', 20, 23, 100, 103)}
    <linearGradient id="bgLight" x1="0" y1="0" x2="${portrait ? 0 : w * 0.3}" y2="${h}" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="${light.groundHi}"/><stop offset=".6" stop-color="${light.ground}"/><stop offset="1" stop-color="${light.groundLo}"/></linearGradient>
    <linearGradient id="bgDark" x1="0" y1="0" x2="${portrait ? 0 : w * 0.3}" y2="${h}" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="${dark.groundHi}"/><stop offset=".55" stop-color="${dark.ground}"/><stop offset="1" stop-color="#221232"/></linearGradient>
    <radialGradient id="wpWash1" cx="0.18" cy="0.14" r="0.7"><stop offset="0" stop-color="${C.corona}" stop-opacity=".3"/><stop offset="1" stop-color="${C.corona}" stop-opacity="0"/></radialGradient>
    <radialGradient id="wpWash2" cx="${portrait ? 0.85 : 0.78}" cy="${portrait ? 0.8 : 0.72}" r="0.85"><stop offset="0" stop-color="${C.orchid}" stop-opacity=".34"/><stop offset="1" stop-color="${C.orchid}" stop-opacity="0"/></radialGradient>
    <radialGradient id="wpWash3" cx="0.6" cy="${portrait ? 0.45 : 0.35}" r="0.55"><stop offset="0" stop-color="${C.flare}" stop-opacity=".2"/><stop offset="1" stop-color="${C.flare}" stop-opacity="0"/></radialGradient>
  </defs>`;
  const bg = `<rect class="ch-bg" width="${w}" height="${h}" fill="url(#bg${p.id === 'dark' ? 'Dark' : 'Light'})"/>`;
  const washes = `<g class="ch-wash" opacity="${p.washOp}">
    <rect width="${w}" height="${h}" fill="url(#wpWash1)"/>
    <rect width="${w}" height="${h}" fill="url(#wpWash2)"/>
    <rect width="${w}" height="${h}" fill="url(#wpWash3)"/>
  </g>`;
  const stars = starfield(p.id === 'dark' ? 7 : 8, Math.round(w * h / 16500), w, h,
    { fill: p.dot, op: p.dotOp * (p.id === 'dark' ? 0.85 : 0.5), sparkEvery: 17 });
  // hero pup: right-of-center on desktop, lower-center on phone (clear of the clock)
  const markSize = portrait ? w * 0.5 : h * 0.42;
  const mx = portrait ? (w - markSize) / 2 : w * 0.66;
  const my = portrait ? h * 0.56 : (h - markSize) / 2.1;
  const s = r2(markSize / 120);
  const orbitRx = markSize * 1.15, orbitRy = markSize * 0.38;
  const ocx = mx + markSize / 2, ocy = my + markSize / 2;
  const orbit = `<g opacity="0.55">
    <ellipse cx="${r2(ocx)}" cy="${r2(ocy)}" rx="${r2(orbitRx)}" ry="${r2(orbitRy)}"
      transform="rotate(-16 ${r2(ocx)} ${r2(ocy)})" fill="none"
      stroke="${p.dot}" stroke-opacity="${p.id === 'dark' ? 0.5 : 0.4}"
      stroke-width="${r2(markSize * 0.012)}" stroke-dasharray="2 ${r2(markSize * 0.05)}" stroke-linecap="round"/>
    <circle cx="${r2(ocx + orbitRx * 0.92)}" cy="${r2(ocy - orbitRy * 0.55)}" r="${r2(markSize * 0.03)}" fill="${C.starlight}"/>
  </g>`;
  // paw-print constellation, upper-left quiet corner (~15% of markSize)
  const px = portrait ? w * 0.22 : w * 0.16, py = portrait ? h * 0.15 : h * 0.22;
  const pawScale = r2(markSize * 0.02);
  const pawStars = [[0, 4], [-5.7, -2.4], [-2, -5.2], [2, -5.2], [5.7, -2.4]];
  const pawConst = `<g opacity="${p.id === 'dark' ? 0.75 : 0.6}" transform="translate(${r2(px)} ${r2(py)}) scale(${pawScale}) rotate(-12)">
    ${pawStars.slice(1).map(([x, y]) => `<line x1="0" y1="6.4" x2="${r2(x * 1.6)}" y2="${r2(y * 1.6)}" stroke="${p.dot}" stroke-opacity="0.3" stroke-width="0.22"/>`).join('')}
    ${pawStars.map(([x, y], i) => `<path d="${star4(x * 1.6, y * 1.6, i === 0 ? 2.1 : 1.5, i === 0 ? 0.78 : 0.55)}" fill="${i === 0 ? C.starlight : p.dot}"/>`).join('')}
  </g>`;
  const hero = `<g transform="translate(${r2(mx)} ${r2(my)}) scale(${s}) rotate(-6 60 63)">${markBody(p)}</g>`;
  return (adaptive ? styleBlock(false) : '') + defs + bg + washes +
    `<g>${stars}</g>` + pawConst + orbit + hero;
}

const wallpaperSvg = (p, adaptive, w, h) => svgDoc(`0 0 ${w} ${h}`, w, h, wallpaperBody(p, w, h, adaptive));

/* ─── textures ───────────────────────────────────────────────────────────── */

function textureBody(name, p, adaptive) {
  const mono = p.mono;
  const c = mono ? p.c : null;
  const dot = mono ? c : (p.id === 'dark' ? C.milk : C.ink);
  const op = mono ? 0.8 : (p.id === 'dark' ? 0.85 : 0.5);
  switch (name) {
    case 'starfield':
      return starfield(1957, 90, 512, 512, { fill: dot, op, cls: 'ch-dot', sparkEvery: 11, monoC: mono ? c : null });
    case 'constellation': {
      const rnd = mulberry32(1961);
      const pts = Array.from({ length: 26 }, () => [r2(rnd() * 512), r2(rnd() * 512)]);
      let lines = '';
      for (let i = 0; i < pts.length; i++) {
        let best = -1, bd = 1e9;
        for (let j = 0; j < pts.length; j++) {
          if (j === i) continue;
          const d = (pts[i][0] - pts[j][0]) ** 2 + (pts[i][1] - pts[j][1]) ** 2;
          if (d < bd) { bd = d; best = j; }
        }
        if (best > i) lines += `<line class="ch-cs" x1="${pts[i][0]}" y1="${pts[i][1]}" x2="${pts[best][0]}" y2="${pts[best][1]}" stroke="${dot}" stroke-opacity="0.22" stroke-width="1"/>`;
      }
      const dots = pts.map(([x, y], i) =>
        i % 7 === 0
          ? `<path class="ch-cf" d="${star4(x, y, 6, 2.2)}" fill="${mono ? c : C.starlight}" opacity="0.9"/>`
          : `<circle class="ch-cf" cx="${x}" cy="${y}" r="2.2" fill="${dot}" opacity="0.6"/>`).join('');
      return lines + dots;
    }
    case 'paws': {
      const rnd = mulberry32(1971);
      let out = '';
      for (let i = 0; i < 14; i++) {
        const x = r2(rnd() * 512), y = r2(rnd() * 512), rot = Math.round(rnd() * 360), sc = r2(0.9 + rnd() * 1.4);
        out += pawShapes(dot, `class="ch-cf" opacity="${mono ? 0.5 : 0.14}" transform="translate(${x} ${y}) rotate(${rot}) scale(${sc})"`);
      }
      return out;
    }
    case 'grain':
      return `<filter id="gr" x="0" y="0" width="100%" height="100%">
        <feTurbulence type="fractalNoise" baseFrequency="0.9" numOctaves="2" seed="7" stitchTiles="stitch"/>
        <feColorMatrix type="matrix" values="0 0 0 0 ${hex01(dot, 0)}  0 0 0 0 ${hex01(dot, 1)}  0 0 0 0 ${hex01(dot, 2)}  0 0 0 0.28 0"/>
      </filter><rect class="ch-cf" width="512" height="512" filter="url(#gr)"/>`;
    default: throw new Error(`unknown texture ${name}`);
  }
}

function hex01(hex, i) {
  return r2(parseInt(hex.slice(1 + i * 2, 3 + i * 2), 16) / 255);
}

/* Opaque nebula wash background (hero sections, slides). Light/dark only. */
const nebulaWashSvg = (p) => svgDoc('0 0 1024 640', 1024, 640, `
  <defs>
    <linearGradient id="nb" x1="0" y1="0" x2="0" y2="640" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="${p.groundHi}"/><stop offset="1" stop-color="${p.ground}"/></linearGradient>
    <radialGradient id="n1" cx="0.12" cy="0.1" r="0.7"><stop offset="0" stop-color="${C.corona}" stop-opacity=".32"/><stop offset="1" stop-color="${C.corona}" stop-opacity="0"/></radialGradient>
    <radialGradient id="n2" cx="0.9" cy="0.85" r="0.8"><stop offset="0" stop-color="${C.orchid}" stop-opacity=".36"/><stop offset="1" stop-color="${C.orchid}" stop-opacity="0"/></radialGradient>
    <radialGradient id="n3" cx="0.55" cy="0.4" r="0.5"><stop offset="0" stop-color="${C.flare}" stop-opacity=".2"/><stop offset="1" stop-color="${C.flare}" stop-opacity="0"/></radialGradient>
  </defs>
  <rect width="1024" height="640" fill="url(#nb)"/>
  <g opacity="${p.washOp * 1.6}"><rect width="1024" height="640" fill="url(#n1)"/><rect width="1024" height="640" fill="url(#n2)"/><rect width="1024" height="640" fill="url(#n3)"/></g>
  ${starfield(2020, 60, 1024, 640, { fill: p.dot, op: p.dotOp * 0.7, sparkEvery: 13 })}`);

const textureSvg = (name, p, adaptive) =>
  svgDoc('0 0 512 512', 512, 512, (adaptive ? styleBlock(p.mono) : '') + textureBody(name, p, adaptive));

/* ─── tokens ─────────────────────────────────────────────────────────────── */

function tokensCss() {
  return `/* Charlie design tokens — generated by docs/brand/build.mjs; do not hand-edit. */
:root {
  --ch-corona: ${C.corona};
  --ch-flare: ${C.flare};
  --ch-orchid: ${C.orchid};
  --ch-starlight: ${C.starlight};
  --ch-caramel: ${C.caramel};
  --ch-fur: ${C.fur};
  --ch-muzzle: ${C.muzzle};
  --ch-ink: ${C.ink};
  --ch-milk: ${C.milk};
  --ch-nebula: linear-gradient(135deg, ${C.corona} 0%, ${C.flare} 55%, ${C.orchid} 100%);

  --ch-ground: ${C.cream100};
  --ch-ground-hi: ${C.cream50};
  --ch-ground-lo: ${C.cream200};
  --ch-text: ${C.ink};
  --ch-text-dim: color-mix(in oklab, ${C.ink} 62%, ${C.cream100});
}
@media (prefers-color-scheme: dark) {
  :root {
    --ch-ground: ${C.void800};
    --ch-ground-hi: ${C.void900};
    --ch-ground-lo: ${C.void700};
    --ch-text: ${C.milk};
    --ch-text-dim: color-mix(in oklab, ${C.milk} 60%, ${C.void800});
  }
}
`;
}

function tokensJson() {
  return JSON.stringify({
    name: 'Charlie',
    description: 'Stella brand tokens — warm cosmic sunset on deep space plum.',
    color: {
      nebula: { corona: C.corona, flare: C.flare, orchid: C.orchid },
      accent: { starlight: C.starlight, caramel: C.caramel, fur: C.fur, muzzle: C.muzzle },
      neutral: { ink: C.ink, milk: C.milk },
      ground: {
        dark: { deepest: C.void900, base: C.void800, raised: C.void700 },
        light: { highest: C.cream50, base: C.cream100, sunken: C.cream200 },
      },
    },
    gradient: { nebula: [C.corona, C.flare, C.orchid], angle: 135 },
  }, null, 2) + '\n';
}

/* ─── emit ───────────────────────────────────────────────────────────────── */

const written = [];
function emit(rel, content) {
  const path = join(ROOT, rel);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, content);
  written.push(rel);
}

function png(svgRel, pngRel, w, h) {
  if (SVG_ONLY) return;
  const out = join(ROOT, pngRel);
  mkdirSync(dirname(out), { recursive: true });
  const args = ['-w', String(w)];
  if (h) args.push('-h', String(h));
  execFileSync('rsvg-convert', [...args, join(ROOT, svgRel), '-o', out]);
  written.push(pngRel);
}

const VARIANTS = ['light', 'dark', 'mono-light', 'mono-dark'];
const RASTER_VARIANTS = VARIANTS; // adaptive variants render as light in librsvg — skip

/** Emit the six-variant set for a template + PNGs at given widths. */
function family(dir, name, tpl, sizes = [], opts = {}) {
  for (const v of VARIANTS) emit(`${dir}/${name}-${v}.svg`, tpl(PAINTS[v], false));
  emit(`${dir}/${name}-adaptive.svg`, tpl(PAINTS.light, true));
  emit(`${dir}/${name}-mono-adaptive.svg`, tpl(PAINTS['mono-light'], true));
  for (const v of RASTER_VARIANTS) {
    for (const s of sizes) {
      const [w, h] = Array.isArray(s) ? s : [s, null];
      png(`${dir}/${name}-${v}.svg`, `png/${dir}/${name}-${v}-${w}${h ? `x${h}` : ''}.png`, w, h);
    }
  }
}

console.log('Charlie brand build →', ROOT);

family('marks', 'mark', (p, a) => markSvg(p, a), [1024, 512, 256]);
family('marks', 'starmark', starmarkSvg, [1024, 512, 256]);
family('wordmarks', 'wordmark', wordmarkSvg, [2048, 1024]);
for (const pose of ['float', 'chase', 'sit']) {
  family('poses', `pose-${pose}`, (p, a) => poseSvg(pose, p, a), [1024]);
}
family('lockups', 'lockup-horizontal', lockupHSvg, [2048, 1024]);
family('lockups', 'lockup-stacked', lockupVSvg, [1024]);
family('icons', 'favicon', faviconSvg, [256, 64, 32]);
family('loader', 'loader', loaderSvg, [512]);

// App + maskable icons: opaque tiles, light/dark only (no mono, no adaptive —
// launcher surfaces can't consume adaptive SVGs).
for (const v of ['light', 'dark']) {
  emit(`icons/appicon-${v}.svg`, appiconSvg(PAINTS[v]));
  emit(`icons/maskable-${v}.svg`, maskableSvg(PAINTS[v]));
  for (const s of [1024, 512, 192]) {
    png(`icons/appicon-${v}.svg`, `png/icons/appicon-${v}-${s}.png`, s, s);
    png(`icons/maskable-${v}.svg`, `png/icons/maskable-${v}-${s}.png`, s, s);
  }
}

for (const g of GLYPHS) family('icons/glyphs', g, (p, a) => glyphSvg(g, p, a), [256]);

// Splash screens (PWA loading screens).
const SPLASH = [['portrait', 1320, 2868], ['landscape', 2880, 1800]];
for (const [tag, w, h] of SPLASH) {
  for (const v of ['light', 'dark']) {
    emit(`splash/splash-${tag}-${v}.svg`, splashSvg(PAINTS[v], false, w, h));
    png(`splash/splash-${tag}-${v}.svg`, `png/splash/splash-${tag}-${v}-${w}x${h}.png`, w, h);
  }
  emit(`splash/splash-${tag}-adaptive.svg`, splashSvg(PAINTS.light, true, w, h));
}

// Wallpapers — desktop 5K and iPhone (19.5:9) at 5K-class resolution.
const WALLS = [['desktop', 2560, 1440, 5120, 2880], ['phone', 1440, 3120, 2880, 6240]];
for (const [tag, vw, vh, pw, ph] of WALLS) {
  for (const v of ['light', 'dark']) {
    emit(`wallpapers/wallpaper-${tag}-${v}.svg`, wallpaperSvg(PAINTS[v], false, vw, vh));
    png(`wallpapers/wallpaper-${tag}-${v}.svg`, `png/wallpapers/wallpaper-${tag}-${v}-${pw}x${ph}.png`, pw, ph);
  }
  emit(`wallpapers/wallpaper-${tag}-adaptive.svg`, wallpaperSvg(PAINTS.light, true, vw, vh));
}

// Textures. Transparent overlays get the full variant set; the opaque nebula
// wash is light/dark only.
for (const t of ['starfield', 'constellation', 'paws', 'grain']) {
  // grain is stitch-tiled noise — ship a small tile; raster noise won't compress
  const sizes = t === 'grain' ? [[512, 512]] : [[2048, 2048]];
  family('textures', `texture-${t}`, (p, a) => textureSvg(t, p, a), sizes);
}
for (const v of ['light', 'dark']) {
  emit(`textures/texture-nebula-${v}.svg`, nebulaWashSvg(PAINTS[v]));
  png(`textures/texture-nebula-${v}.svg`, `png/textures/texture-nebula-${v}-2048x1280.png`, 2048, 1280);
}

emit('tokens.css', tokensCss());
emit('tokens.json', tokensJson());

console.log(`wrote ${written.length} files (${written.filter(f => f.endsWith('.svg')).length} svg, ${written.filter(f => f.endsWith('.png')).length} png)`);
