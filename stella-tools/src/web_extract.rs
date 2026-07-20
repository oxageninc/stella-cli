//! Pure HTML/CSS extraction behind the web tools — no I/O in this module.
//!
//! Three capabilities, each a plain function over strings so the whole
//! surface is unit-testable without a network:
//!
//! - [`html_to_markdown`] / [`html_to_text`] — a readable rendering of a
//!   fetched page for `web_fetch`, links and images preserved as absolute
//!   URLs so the model can follow them with another call.
//! - [`extract_assets`] — the page's asset graph (stylesheets, scripts,
//!   images, preloaded fonts, meta) for `web_extract_assets`.
//! - [`CssAccumulator`] — design-token mining over any number of CSS
//!   sources: color literals and font families ranked by frequency, custom
//!   properties (the closest thing the web has to a published token set),
//!   and `@font-face` families with their source files.
//!
//! Parsing is html5ever via `scraper`, so real-world tag soup resolves the
//! way a browser reads it; the CSS side is a hand-rolled scanner (colors,
//! declarations, blocks) rather than a full CSS parser — tokens, not
//! cascade semantics, are what design-system extraction needs.

use std::collections::{HashMap, HashSet};

use reqwest::Url;
use scraper::{ElementRef, Html, Selector};

/// Tags whose subtree contributes nothing to a readable rendering.
const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "iframe", "svg", "canvas", "object", "embed",
];

/// Tags that separate paragraphs in the markdown rendering.
const BLOCK_TAGS: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "main",
    "header",
    "footer",
    "aside",
    "figure",
    "figcaption",
    "details",
    "summary",
    "form",
    "fieldset",
    "address",
    "dl",
    "dt",
    "dd",
    "nav",
];

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("static selector")
}

fn doc_title(doc: &Html) -> Option<String> {
    let title = doc
        .select(&selector("title"))
        .next()
        .map(|el| collapse_ws(&el.text().collect::<String>()))?;
    (!title.is_empty()).then_some(title)
}

/// Collapse every whitespace run to a single space and trim.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Absolutize `href` against `base` when one is given; keeps the raw value
/// when it already is absolute or the join fails.
fn absolutize(base: Option<&Url>, href: &str) -> String {
    match base {
        Some(base) => base
            .join(href)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| href.to_string()),
        None => href.to_string(),
    }
}

/// Render fetched HTML as markdown. Returns `(title, markdown)`; links and
/// images are absolutized against `base` so they stay fetchable.
pub fn html_to_markdown(html: &str, base: Option<&Url>) -> (Option<String>, String) {
    let doc = Html::parse_document(html);
    let title = doc_title(&doc);
    let root = doc
        .select(&selector("body"))
        .next()
        .unwrap_or_else(|| doc.root_element());
    let mut st = MdState {
        out: String::new(),
        list_stack: Vec::new(),
        base,
    };
    st.render_children(root);
    (title, tidy_blocks(&st.out))
}

/// Render fetched HTML as plain text (no markdown syntax): block tags break
/// paragraphs, everything in [`SKIP_TAGS`] is dropped.
pub fn html_to_text(html: &str) -> (Option<String>, String) {
    let doc = Html::parse_document(html);
    let title = doc_title(&doc);
    let root = doc
        .select(&selector("body"))
        .next()
        .unwrap_or_else(|| doc.root_element());
    let mut out = String::new();
    text_walk(root, &mut out);
    (title, tidy_blocks(&out))
}

fn text_walk(el: ElementRef<'_>, out: &mut String) {
    for child in el.children() {
        match child.value() {
            scraper::Node::Text(t) => push_inline_text(out, t),
            scraper::Node::Element(_) => {
                let Some(child_el) = ElementRef::wrap(child) else {
                    continue;
                };
                let name = child_el.value().name();
                if SKIP_TAGS.contains(&name) {
                    continue;
                }
                let block = BLOCK_TAGS.contains(&name)
                    || matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "tr")
                    || matches!(name, "ul" | "ol" | "table" | "blockquote" | "pre");
                if block {
                    ensure_block(out);
                }
                if name == "br" {
                    out.push('\n');
                }
                text_walk(child_el, out);
                if block {
                    ensure_block(out);
                }
            }
            _ => {}
        }
    }
}

/// Append a text node, collapsing whitespace runs; a leading run collapses
/// away entirely when the output already ends at a boundary.
fn push_inline_text(out: &mut String, text: &str) {
    let collapsed = collapse_ws(text);
    if collapsed.is_empty() {
        return;
    }
    let boundary = out.is_empty() || out.ends_with([' ', '\n', '(', '[', '*', '`', '#', '>']);
    if text.starts_with(char::is_whitespace) && !boundary {
        out.push(' ');
    }
    out.push_str(&collapsed);
    if text.ends_with(char::is_whitespace) {
        out.push(' ');
    }
}

/// Guarantee the output sits at a paragraph boundary (`…\n\n`).
fn ensure_block(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        return;
    }
    while !out.ends_with("\n\n") {
        out.push('\n');
    }
}

/// Trim the rendering: collapse 3+ newlines to a paragraph break, trim ends.
fn tidy_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut newlines = 0usize;
    for ch in s.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push('\n');
            }
        } else {
            newlines = 0;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

struct MdState<'a> {
    out: String,
    /// `None` = unordered list level, `Some(next)` = ordered list counter.
    list_stack: Vec<Option<u32>>,
    base: Option<&'a Url>,
}

impl MdState<'_> {
    fn render_children(&mut self, el: ElementRef<'_>) {
        for child in el.children() {
            match child.value() {
                scraper::Node::Text(t) => push_inline_text(&mut self.out, t),
                scraper::Node::Element(_) => {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        self.render_element(child_el);
                    }
                }
                _ => {}
            }
        }
    }

    /// Render `el`'s subtree into a detached buffer (for link texts,
    /// blockquote bodies, table cells).
    fn render_detached(&mut self, el: ElementRef<'_>) -> String {
        let taken = std::mem::take(&mut self.out);
        self.render_children(el);
        std::mem::replace(&mut self.out, taken)
    }

    fn render_element(&mut self, el: ElementRef<'_>) {
        let name = el.value().name();
        if SKIP_TAGS.contains(&name) {
            return;
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name[1..].parse::<usize>().unwrap_or(1);
                ensure_block(&mut self.out);
                self.out.push_str(&"#".repeat(level));
                self.out.push(' ');
                self.render_children(el);
                ensure_block(&mut self.out);
            }
            "br" => self.out.push('\n'),
            "hr" => {
                ensure_block(&mut self.out);
                self.out.push_str("---");
                ensure_block(&mut self.out);
            }
            "ul" | "ol" => {
                ensure_block(&mut self.out);
                self.list_stack.push((name == "ol").then_some(1u32));
                self.render_children(el);
                self.list_stack.pop();
                ensure_block(&mut self.out);
            }
            "li" => {
                while !self.out.is_empty() && !self.out.ends_with('\n') {
                    self.out.push('\n');
                }
                let depth = self.list_stack.len().saturating_sub(1);
                self.out.push_str(&"  ".repeat(depth));
                match self.list_stack.last_mut() {
                    Some(Some(counter)) => {
                        self.out.push_str(&format!("{counter}. "));
                        *counter += 1;
                    }
                    _ => self.out.push_str("- "),
                }
                self.render_children(el);
            }
            "a" => {
                let text = collapse_ws(&self.render_detached(el));
                match el.value().attr("href") {
                    // Fragment/js pseudo-links carry no fetchable target.
                    Some(href) if !href.starts_with('#') && !href.starts_with("javascript:") => {
                        let href = absolutize(self.base, href);
                        let label = if text.is_empty() { href.clone() } else { text };
                        self.out.push_str(&format!("[{label}]({href})"));
                    }
                    _ => self.out.push_str(&text),
                }
            }
            "img" => {
                if let Some(src) = el.value().attr("src") {
                    let alt = el.value().attr("alt").unwrap_or("");
                    let src = absolutize(self.base, src);
                    self.out
                        .push_str(&format!("![{}]({src})", collapse_ws(alt)));
                }
            }
            "strong" | "b" => {
                let inner = self.render_detached(el);
                let trimmed = inner.trim();
                if !trimmed.is_empty() {
                    self.out.push_str(&format!("**{trimmed}** "));
                }
            }
            "em" | "i" => {
                let inner = self.render_detached(el);
                let trimmed = inner.trim();
                if !trimmed.is_empty() {
                    self.out.push_str(&format!("*{trimmed}* "));
                }
            }
            "code" => {
                let raw = collapse_ws(&el.text().collect::<String>());
                if !raw.is_empty() {
                    self.out.push_str(&format!("`{raw}`"));
                }
            }
            "pre" => {
                ensure_block(&mut self.out);
                let raw: String = el.text().collect();
                self.out.push_str("```\n");
                self.out.push_str(raw.trim_matches('\n'));
                self.out.push_str("\n```");
                ensure_block(&mut self.out);
            }
            "blockquote" => {
                ensure_block(&mut self.out);
                let inner = self.render_detached(el);
                for line in tidy_blocks(&inner).lines() {
                    self.out.push_str("> ");
                    self.out.push_str(line);
                    self.out.push('\n');
                }
                ensure_block(&mut self.out);
            }
            "table" => {
                ensure_block(&mut self.out);
                let row_sel = selector("tr");
                let cell_sel = selector("th, td");
                let mut first = true;
                // Direct iteration over `tr` handles thead/tbody uniformly.
                let rows: Vec<_> = el.select(&row_sel).collect();
                for row in rows {
                    let cells: Vec<String> = row
                        .select(&cell_sel)
                        .map(|cell| collapse_ws(&self.render_detached(cell)))
                        .collect();
                    if cells.is_empty() {
                        continue;
                    }
                    self.out.push_str(&format!("| {} |\n", cells.join(" | ")));
                    if first {
                        self.out
                            .push_str(&format!("|{}\n", " --- |".repeat(cells.len())));
                        first = false;
                    }
                }
                ensure_block(&mut self.out);
            }
            // A table subtree is rendered by the `table` arm above; reaching
            // these directly (orphaned markup) falls through to children.
            _ if BLOCK_TAGS.contains(&name) => {
                ensure_block(&mut self.out);
                self.render_children(el);
                ensure_block(&mut self.out);
            }
            _ => self.render_children(el),
        }
    }
}

/// Everything `web_extract_assets` reads off the page itself (the CSS side
/// is [`CssAccumulator`], fed separately with each stylesheet's text).
#[derive(Debug, Default)]
pub struct AssetManifest {
    pub title: Option<String>,
    /// Interesting `<meta>` values as `(name, content)` —
    /// description, og:site_name, og:title, theme-color.
    pub meta: Vec<(String, String)>,
    /// External stylesheet URLs, in document order.
    pub stylesheets: Vec<String>,
    /// Inline `<style>` block contents, in document order.
    pub inline_css: Vec<String>,
    pub scripts: Vec<String>,
    /// `<img>` src + srcset entries, `<picture>` sources, icons, og:image.
    pub images: Vec<String>,
    /// Fonts the page preloads (`<link rel="preload" as="font">`).
    pub fonts: Vec<String>,
}

/// Parse the page's asset graph. URLs are absolutized against `base`.
pub fn extract_assets(html: &str, base: &Url) -> AssetManifest {
    let doc = Html::parse_document(html);
    let mut manifest = AssetManifest {
        title: doc_title(&doc),
        ..Default::default()
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut push_unique = |list: &mut Vec<String>, url: String| {
        if seen.insert(url.clone()) {
            list.push(url);
        }
    };

    for link in doc.select(&selector("link[href]")) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let rel = link.value().attr("rel").unwrap_or("").to_ascii_lowercase();
        let url = absolutize(Some(base), href);
        if rel.split_whitespace().any(|r| r == "stylesheet") {
            push_unique(&mut manifest.stylesheets, url);
        } else if rel.contains("icon") {
            push_unique(&mut manifest.images, url);
        } else if rel.split_whitespace().any(|r| r == "preload")
            && link.value().attr("as") == Some("font")
        {
            push_unique(&mut manifest.fonts, url);
        }
    }
    for style in doc.select(&selector("style")) {
        manifest.inline_css.push(style.text().collect());
    }
    for script in doc.select(&selector("script[src]")) {
        if let Some(src) = script.value().attr("src") {
            push_unique(&mut manifest.scripts, absolutize(Some(base), src));
        }
    }
    for img in doc.select(&selector("img, source")) {
        if let Some(src) = img.value().attr("src") {
            push_unique(&mut manifest.images, absolutize(Some(base), src));
        }
        if let Some(srcset) = img.value().attr("srcset") {
            for candidate in parse_srcset(srcset) {
                push_unique(&mut manifest.images, absolutize(Some(base), &candidate));
            }
        }
    }
    for meta in doc.select(&selector("meta")) {
        let key = meta
            .value()
            .attr("name")
            .or_else(|| meta.value().attr("property"))
            .unwrap_or("");
        let Some(content) = meta.value().attr("content") else {
            continue;
        };
        match key {
            "description" | "og:site_name" | "og:title" | "theme-color" => {
                manifest.meta.push((key.to_string(), collapse_ws(content)))
            }
            "og:image" => push_unique(&mut manifest.images, absolutize(Some(base), content)),
            _ => {}
        }
    }
    manifest
}

/// The URL half of each srcset candidate (`url 2x, url 640w` → urls).
fn parse_srcset(srcset: &str) -> Vec<String> {
    srcset
        .split(',')
        .filter_map(|part| part.split_whitespace().next())
        .filter(|url| !url.is_empty())
        .map(str::to_string)
        .collect()
}

/// One `@font-face` block: the declared family and its source files.
#[derive(Debug, Clone, PartialEq)]
pub struct FontFace {
    pub family: String,
    pub sources: Vec<String>,
}

/// The design-token summary distilled from every CSS source fed in.
#[derive(Debug, Default)]
pub struct CssTokens {
    /// Color literals by frequency, descending (hex + functional notations).
    pub colors: Vec<(String, usize)>,
    /// Concrete font families by frequency (generic keywords excluded).
    pub font_families: Vec<(String, usize)>,
    /// Custom-property declarations, first definition wins.
    pub custom_props: Vec<(String, String)>,
    pub font_faces: Vec<FontFace>,
}

/// Streaming accumulator: feed each stylesheet with [`CssAccumulator::add_css`],
/// then [`CssAccumulator::finish`] to rank.
#[derive(Debug, Default)]
pub struct CssAccumulator {
    color_counts: HashMap<String, usize>,
    family_counts: HashMap<String, usize>,
    custom_props: Vec<(String, String)>,
    custom_seen: HashSet<String>,
    font_faces: Vec<FontFace>,
}

/// CSS-wide generic/utility family keywords that aren't design tokens.
const GENERIC_FAMILIES: &[&str] = &[
    "serif",
    "sans-serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    "ui-sans-serif",
    "ui-serif",
    "ui-monospace",
    "ui-rounded",
    "math",
    "emoji",
    "inherit",
    "initial",
    "unset",
    "revert",
];

impl CssAccumulator {
    pub fn add_css(&mut self, css: &str, base: Option<&Url>) {
        // Byte-parallel lowercase copy: scans are case-insensitive, slices
        // into the original keep the site's own casing for values.
        let lower = css.to_ascii_lowercase();
        self.scan_colors(css, &lower);
        self.scan_font_families(css, &lower);
        self.scan_custom_props(css, &lower);
        self.scan_font_faces(css, &lower, base);
    }

    pub fn finish(self) -> CssTokens {
        let rank = |counts: HashMap<String, usize>| {
            let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            ranked
        };
        CssTokens {
            colors: rank(self.color_counts),
            font_families: rank(self.family_counts),
            custom_props: self.custom_props,
            font_faces: self.font_faces,
        }
    }

    fn scan_colors(&mut self, css: &str, lower: &str) {
        let bytes = css.as_bytes();
        // Hex literals: 3/4/6/8 hex digits ending at a non-token boundary.
        let mut i = 0;
        while i < css.len() {
            let Some(off) = css[i..].find('#') else { break };
            let start = i + off + 1;
            if start >= css.len() {
                break;
            }
            let len = css[start..]
                .bytes()
                .take_while(|b| b.is_ascii_hexdigit())
                .count();
            let boundary_ok = start + len >= bytes.len()
                || !(bytes[start + len].is_ascii_alphanumeric() || bytes[start + len] == b'-');
            if matches!(len, 3 | 4 | 6 | 8) && boundary_ok {
                let hex = format!("#{}", css[start..start + len].to_ascii_lowercase());
                *self.color_counts.entry(hex).or_default() += 1;
            }
            // Advance a full char when nothing matched — the byte after `#`
            // may be multibyte (e.g. `content: "#я"`).
            i = if len > 0 {
                start + len
            } else {
                start + css[start..].chars().next().map_or(1, char::len_utf8)
            };
        }
        // Functional notations, matched paren-aware so nested `var()` and
        // `calc()` don't truncate the capture. A capture that still contains
        // `var(` is a token *reference*, not a literal color — skipped.
        for func in [
            "rgb(", "rgba(", "hsl(", "hsla(", "oklch(", "oklab(", "lab(", "lch(",
        ] {
            let mut from = 0;
            while let Some(off) = lower[from..].find(func) {
                let open = from + off + func.len() - 1;
                // Reject identifier tails like `--brand-rgb(` typos.
                let head = from + off;
                if head > 0 {
                    let prev = bytes[head - 1];
                    if prev.is_ascii_alphanumeric() || prev == b'-' || prev == b'_' {
                        from = head + func.len();
                        continue;
                    }
                }
                let Some(close) = matching_paren(bytes, open) else {
                    break;
                };
                let literal = collapse_ws(&css[head..=close]).to_ascii_lowercase();
                if !literal.contains("var(") {
                    *self.color_counts.entry(literal).or_default() += 1;
                }
                from = close + 1;
            }
        }
    }

    fn scan_font_families(&mut self, css: &str, lower: &str) {
        let mut from = 0;
        while let Some(off) = lower[from..].find("font-family") {
            let after = from + off + "font-family".len();
            from = after;
            let rest = &css[after..];
            let Some(colon) = rest.find(':') else {
                continue;
            };
            if !rest[..colon].trim().is_empty() {
                continue; // e.g. a selector like `[data-font-family-x]`
            }
            let value = &rest[colon + 1..];
            let end = value.find([';', '}']).unwrap_or(value.len());
            for family in value[..end].split(',') {
                let family = family.trim().trim_matches(['"', '\'']).trim();
                if family.is_empty()
                    || family.contains("var(")
                    || GENERIC_FAMILIES.contains(&family.to_ascii_lowercase().as_str())
                {
                    continue;
                }
                *self.family_counts.entry(family.to_string()).or_default() += 1;
            }
        }
    }

    fn scan_custom_props(&mut self, css: &str, lower: &str) {
        let bytes = css.as_bytes();
        let mut from = 0;
        while let Some(off) = lower[from..].find("--") {
            let start = from + off;
            from = start + 2;
            // A declaration site: preceded by a block/statement boundary
            // (never inside `var(--x)` — that's preceded by `(`).
            let at_boundary = start == 0
                || matches!(bytes[start - 1], b'{' | b';' | b'\n' | b'\r' | b' ' | b'\t');
            if !at_boundary {
                continue;
            }
            let name_len = css[start + 2..]
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
                .count();
            if name_len == 0 {
                continue;
            }
            let name_end = start + 2 + name_len;
            let rest = &css[name_end..];
            let Some(colon) = rest.find(':') else {
                continue;
            };
            if !rest[..colon].trim().is_empty() {
                continue;
            }
            // Value runs to the first top-level `;`/`}` — parens tracked so
            // `url(data:image/png;base64,…)` doesn't split early.
            let value_src = &rest[colon + 1..];
            let mut depth = 0usize;
            let mut end = value_src.len();
            for (idx, b) in value_src.bytes().enumerate() {
                match b {
                    b'(' => depth += 1,
                    b')' => depth = depth.saturating_sub(1),
                    b';' | b'}' if depth == 0 => {
                        end = idx;
                        break;
                    }
                    _ => {}
                }
            }
            let name = css[start..name_end].to_string();
            if !self.custom_seen.insert(name.clone()) {
                continue; // first definition wins (`:root` comes first)
            }
            let mut value = collapse_ws(&value_src[..end]);
            if value.len() > 160 {
                value.truncate(157);
                value.push('…');
            }
            self.custom_props.push((name, value));
            from = name_end;
        }
    }

    fn scan_font_faces(&mut self, css: &str, lower: &str, base: Option<&Url>) {
        let mut from = 0;
        while let Some(off) = lower[from..].find("@font-face") {
            let start = from + off;
            let Some(open_rel) = css[start..].find('{') else {
                break;
            };
            let open = start + open_rel;
            let Some(close) = matching_brace(css.as_bytes(), open) else {
                break;
            };
            let block = &css[open + 1..close];
            let block_lower = &lower[open + 1..close];
            let family = block_lower.find("font-family").and_then(|p| {
                let rest = &block[p + "font-family".len()..];
                let colon = rest.find(':')?;
                let value = &rest[colon + 1..];
                let end = value.find([';', '}']).unwrap_or(value.len());
                let family = value[..end].trim().trim_matches(['"', '\'']).trim();
                (!family.is_empty()).then(|| family.to_string())
            });
            let sources = extract_css_urls(block, base);
            if let Some(family) = family {
                self.font_faces.push(FontFace { family, sources });
            }
            from = close + 1;
        }
    }
}

/// Every `url(...)` inside `css`, unquoted and absolutized; `data:` URIs are
/// summarized rather than inlined.
pub fn extract_css_urls(css: &str, base: Option<&Url>) -> Vec<String> {
    let lower = css.to_ascii_lowercase();
    let mut urls = Vec::new();
    let mut from = 0;
    while let Some(off) = lower[from..].find("url(") {
        let open = from + off + 3;
        let Some(close) = matching_paren(css.as_bytes(), open) else {
            break;
        };
        let raw = css[open + 1..close].trim().trim_matches(['"', '\'']).trim();
        if raw.starts_with("data:") {
            let kind = raw.split([';', ',']).next().unwrap_or("data:");
            urls.push(format!("<inline {kind} ({} bytes)>", raw.len()));
        } else if !raw.is_empty() {
            urls.push(absolutize(base, raw));
        }
        from = close + 1;
    }
    urls
}

/// Index of the `)` matching the `(` at `open`, tracking nesting.
fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    debug_assert_eq!(bytes[open], b'(');
    let mut depth = 0usize;
    for (idx, b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

/// Index of the `}` matching the `{` at `open`, tracking nesting.
fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    debug_assert_eq!(bytes[open], b'{');
    let mut depth = 0usize;
    for (idx, b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/blog/post").unwrap()
    }

    #[test]
    fn markdown_renders_headings_paragraphs_links_and_lists() {
        let html = r#"<html><head><title> The   Title </title></head><body>
            <h1>Hello</h1>
            <p>A <strong>bold</strong> move with <a href="/docs">the docs</a>.</p>
            <ul><li>one</li><li>two <em>fancy</em></li></ul>
            <ol><li>first</li><li>second</li></ol>
            <script>ignore_me();</script>
        </body></html>"#;
        let (title, md) = html_to_markdown(html, Some(&base()));
        assert_eq!(title.as_deref(), Some("The Title"));
        assert!(md.starts_with("# Hello"), "{md}");
        assert!(md.contains("**bold**"), "{md}");
        assert!(md.contains("[the docs](https://example.com/docs)"), "{md}");
        assert!(md.contains("- one"), "{md}");
        assert!(md.contains("1. first"), "{md}");
        assert!(md.contains("2. second"), "{md}");
        assert!(
            !md.contains("ignore_me"),
            "script text must be dropped: {md}"
        );
    }

    #[test]
    fn markdown_renders_code_quotes_images_and_tables() {
        let html = r#"<body>
            <pre><code>let x = 1;</code></pre>
            <blockquote><p>quoted</p></blockquote>
            <img src="hero.png" alt="Hero shot">
            <table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>
        </body>"#;
        let (_, md) = html_to_markdown(html, Some(&base()));
        assert!(md.contains("```\nlet x = 1;\n```"), "{md}");
        assert!(md.contains("> quoted"), "{md}");
        assert!(
            md.contains("![Hero shot](https://example.com/blog/hero.png)"),
            "{md}"
        );
        assert!(md.contains("| A | B |"), "{md}");
        assert!(md.contains("| --- | --- |"), "{md}");
        assert!(md.contains("| 1 | 2 |"), "{md}");
    }

    #[test]
    fn plain_text_drops_markup_but_keeps_blocks() {
        let html = "<body><h1>Top</h1><p>One</p><p>Two <b>bold</b></p><style>.x{}</style></body>";
        let (_, text) = html_to_text(html);
        assert_eq!(text, "Top\n\nOne\n\nTwo bold");
    }

    #[test]
    fn assets_extract_stylesheets_scripts_images_fonts_and_meta() {
        let html = r##"<html><head>
            <title>Shop</title>
            <meta name="description" content="Buy things">
            <meta property="og:image" content="/og.png">
            <meta name="theme-color" content="#ff6a3d">
            <link rel="stylesheet" href="/css/main.css">
            <link rel="icon" href="/favicon.svg">
            <link rel="preload" as="font" href="/fonts/inter.woff2">
            <style>.inline { color: #fff }</style>
            <script src="/js/app.js"></script>
        </head><body>
            <img src="a.png" srcset="a@2x.png 2x, a@3x.png 3x">
            <picture><source srcset="b.webp"></picture>
        </body></html>"##;
        let manifest = extract_assets(html, &base());
        assert_eq!(manifest.title.as_deref(), Some("Shop"));
        assert_eq!(
            manifest.stylesheets,
            vec!["https://example.com/css/main.css"]
        );
        assert_eq!(manifest.scripts, vec!["https://example.com/js/app.js"]);
        assert_eq!(
            manifest.fonts,
            vec!["https://example.com/fonts/inter.woff2"]
        );
        assert_eq!(manifest.inline_css.len(), 1);
        for expected in [
            "https://example.com/favicon.svg",
            "https://example.com/blog/a.png",
            "https://example.com/blog/a@2x.png",
            "https://example.com/blog/a@3x.png",
            "https://example.com/blog/b.webp",
            "https://example.com/og.png",
        ] {
            assert!(
                manifest.images.contains(&expected.to_string()),
                "missing {expected}: {:?}",
                manifest.images
            );
        }
        assert!(
            manifest
                .meta
                .contains(&("description".into(), "Buy things".into()))
        );
        assert!(
            manifest
                .meta
                .contains(&("theme-color".into(), "#ff6a3d".into()))
        );
    }

    #[test]
    fn css_tokens_rank_colors_and_families_and_capture_custom_props() {
        let css = r#"
            :root { --color-accent: #FF6A3D; --spacing: calc(1rem - 2px); --wall: url(data:image/png;base64,AAAA); }
            .a { color: #ff6a3d; background: rgb(15, 23, 42); font-family: "Inter", system-ui, sans-serif; }
            .b { color: #ff6a3d; border-color: rgba(15,23,42,.5); font-family: Inter, sans-serif; }
            .c { accent-color: rgb(var(--color-accent-rgb)); }
            @font-face { font-family: "Inter"; src: url("/fonts/inter.woff2") format("woff2"); }
        "#;
        let mut acc = CssAccumulator::default();
        acc.add_css(css, Some(&base()));
        let tokens = acc.finish();

        assert_eq!(tokens.colors[0], ("#ff6a3d".into(), 3));
        assert!(
            tokens
                .colors
                .iter()
                .any(|(c, n)| c == "rgb(15, 23, 42)" && *n == 1),
            "{:?}",
            tokens.colors
        );
        assert!(
            !tokens.colors.iter().any(|(c, _)| c.contains("var(")),
            "var() references are not literal colors: {:?}",
            tokens.colors
        );
        // Two rule declarations + the @font-face block itself.
        assert_eq!(tokens.font_families[0], ("Inter".into(), 3));
        assert!(
            !tokens
                .font_families
                .iter()
                .any(|(f, _)| f == "sans-serif" || f == "system-ui")
        );
        assert!(
            tokens
                .custom_props
                .contains(&("--color-accent".into(), "#FF6A3D".into())),
            "{:?}",
            tokens.custom_props
        );
        assert!(
            tokens
                .custom_props
                .iter()
                .any(|(name, value)| name == "--wall" && value.contains("base64")),
            "paren-aware value scan must survive the data URI: {:?}",
            tokens.custom_props
        );
        assert_eq!(tokens.font_faces.len(), 1);
        assert_eq!(tokens.font_faces[0].family, "Inter");
        assert_eq!(
            tokens.font_faces[0].sources,
            vec!["https://example.com/fonts/inter.woff2"]
        );
    }

    #[test]
    fn css_urls_absolutize_and_summarize_data_uris() {
        let urls = extract_css_urls(
            "src: url('../f/a.woff2'), url(data:font/woff2;base64,AAAABBBB);",
            Some(&base()),
        );
        assert_eq!(urls[0], "https://example.com/f/a.woff2");
        assert!(urls[1].starts_with("<inline data:font/woff2"), "{:?}", urls);
    }

    #[test]
    fn hex_scan_rejects_url_fragments_and_odd_lengths() {
        let mut acc = CssAccumulator::default();
        acc.add_css(
            ".x { fill: url(#gradient); color: #abcde; border: #аbc }",
            None,
        );
        let tokens = acc.finish();
        assert!(tokens.colors.is_empty(), "{:?}", tokens.colors);
    }
}
