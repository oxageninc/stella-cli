//! Lightweight markdown-to-styled-lines renderer for the transcript.
//!
//! Agent responses are markdown-capable, so every `Text` transcript entry is
//! parsed for common markdown constructs before rendering. Block-level syntax
//! (headings, lists, fenced code, blockquotes, rules) is detected per-line;
//! inline syntax (**bold**, *italic*, `code`, [links](url)) is parsed within
//! each line. The output is a vector of styled [`Line`]s that the transcript
//! renderer and word-wrapper consume unchanged.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::syntax;
use crate::theme;

/// Render a markdown string into styled lines.
///
/// Each input line is classified into a block type; inline formatting is parsed
/// within non-code lines. Fenced code blocks (```...```) are rendered verbatim
/// in a distinct style.
pub fn render(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // `Some(lang)` while inside a fenced block: the opening fence's info
    // string (```rust, ```toml, …) picks the language its lines highlight in.
    let mut code_block: Option<Option<syntax::Lang>> = None;

    for raw in text.lines() {
        // ── Fenced code block toggle ────────────────────────────────────────
        if raw.trim_start().starts_with("```") {
            code_block = match code_block {
                Some(_) => None,
                None => Some(syntax::lang_from_fence(
                    raw.trim_start().trim_start_matches('`'),
                )),
            };
            continue;
        }

        if let Some(lang) = code_block {
            out.push(code_block_line(raw, lang));
            continue;
        }

        // ── Horizontal rule ────────────────────────────────────────────────
        if is_hr(raw) {
            out.push(Line::from(Span::styled(
                "───────────────────────────────────────────",
                Style::new().fg(theme::RULE),
            )));
            continue;
        }

        // ── Headings (# .. ######) ─────────────────────────────────────────
        if let Some(rest) = strip_heading(raw) {
            let (level, content) = rest;
            out.push(heading_line(content, level));
            continue;
        }

        // ── Blockquote (> ...) ─────────────────────────────────────────────
        if let Some(rest) = raw.strip_prefix("> ") {
            let mut spans = vec![Span::styled("▎ ", Style::new().fg(theme::MUTED))];
            spans.extend(parse_inline_spans(rest));
            out.push(Line::from(spans));
            continue;
        }

        // ── Bullet list (- / * / +) ────────────────────────────────────────
        let lead = raw.trim_start();
        let indent = raw.len() - lead.len();
        if let Some(rest) = lead
            .strip_prefix("- ")
            .or_else(|| lead.strip_prefix("* "))
            .or_else(|| lead.strip_prefix("+ "))
        {
            let prefix = format!("{}• ", " ".repeat(indent));
            let mut spans = vec![Span::styled(prefix, Style::new().fg(theme::MUTED))];
            spans.extend(parse_inline_spans(rest));
            out.push(Line::from(spans));
            continue;
        }

        // ── Numbered list (1. / 42. ) ──────────────────────────────────────
        if let Some(rest) = strip_numbered(lead) {
            let prefix = format!("{}  ", " ".repeat(indent));
            let mut spans = vec![Span::styled(prefix, Style::new().fg(theme::MUTED))];
            spans.extend(parse_inline_spans(rest));
            out.push(Line::from(spans));
            continue;
        }

        // ── Blank line ─────────────────────────────────────────────────────
        if raw.trim().is_empty() {
            out.push(Line::raw(""));
            continue;
        }

        // ── Regular paragraph text ─────────────────────────────────────────
        out.push(Line::from(parse_inline_spans(raw)));
    }

    out
}

// ── Inline parsing ─────────────────────────────────────────────────────────

/// Parse inline markdown within a single line into styled spans.
///
/// Supports `**bold**`, `*italic*`, `_italic_`, `__bold__`, `` `code` ``,
/// `[text](url)`, and `~~strike~~`. Unmatched delimiters pass through as
/// literal text.
fn parse_inline_spans(text: &str) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::raw(std::mem::take(buf)));
        }
    };

    while i < chars.len() {
        // **bold** or __bold__
        if (chars[i] == '*' || chars[i] == '_') && i + 1 < chars.len() && chars[i + 1] == chars[i] {
            let delim: String = std::iter::repeat_n(chars[i], 2).collect();
            let Some(end) = find_str(&chars, i + 2, &delim) else {
                buf.push(chars[i]);
                i += 1;
                continue;
            };
            flush(&mut buf, &mut spans);
            let content: String = chars[i + 2..end].iter().collect();
            for inner in parse_inline_spans(&content) {
                let new_style = inner.style.add_modifier(Modifier::BOLD);
                spans.push(Span::styled(inner.content.into_owned(), new_style));
            }
            i = end + 2;
            continue;
        }

        // *italic* or _italic_ (single delimiter, not part of ** or __)
        if (chars[i] == '*' || chars[i] == '_')
            && (i + 1 >= chars.len() || chars[i + 1] != chars[i])
        {
            let close = chars[i];
            let Some(end) = find_single_delim(&chars, i + 1, close) else {
                buf.push(chars[i]);
                i += 1;
                continue;
            };
            flush(&mut buf, &mut spans);
            let content: String = chars[i + 1..end].iter().collect();
            for inner in parse_inline_spans(&content) {
                let new_style = inner.style.add_modifier(Modifier::ITALIC);
                spans.push(Span::styled(inner.content.into_owned(), new_style));
            }
            i = end + 1;
            continue;
        }

        // `code`
        if chars[i] == '`' {
            let Some(end) = find_char(&chars, i + 1, '`') else {
                buf.push(chars[i]);
                i += 1;
                continue;
            };
            flush(&mut buf, &mut spans);
            let content: String = chars[i + 1..end].iter().collect();
            spans.push(Span::styled(content, code_style()));
            i = end + 1;
            continue;
        }

        // ~~strike~~
        if chars[i] == '~' && i + 1 < chars.len() && chars[i + 1] == '~' {
            let Some(end) = find_str(&chars, i + 2, "~~") else {
                buf.push(chars[i]);
                i += 1;
                continue;
            };
            flush(&mut buf, &mut spans);
            let content: String = chars[i + 2..end].iter().collect();
            spans.push(Span::styled(
                content,
                Style::new().add_modifier(Modifier::CROSSED_OUT),
            ));
            i = end + 2;
            continue;
        }

        // [text](url)
        if chars[i] == '['
            && let Some(close) = find_char(&chars, i + 1, ']')
            && close + 1 < chars.len()
            && chars[close + 1] == '('
            && let Some(paren) = find_char(&chars, close + 2, ')')
        {
            flush(&mut buf, &mut spans);
            let link_text: String = chars[i + 1..close].iter().collect();
            let url: String = chars[close + 2..paren].iter().collect();
            spans.push(Span::styled(
                if url.is_empty() {
                    link_text
                } else {
                    format!("{link_text} ({url})")
                },
                Style::new()
                    .fg(theme::RUN)
                    .add_modifier(Modifier::UNDERLINED),
            ));
            i = paren + 1;
            continue;
        }

        buf.push(chars[i]);
        i += 1;
    }

    flush(&mut buf, &mut spans);
    spans
}

// ── Block helpers ──────────────────────────────────────────────────────────

/// True if `line` is a horizontal rule (`---`, `***`, `___` with 3+ chars).
fn is_hr(line: &str) -> bool {
    let t = line.trim();
    if t.len() < 3 {
        return false;
    }
    let first = match t.chars().next() {
        Some(c) => c,
        None => return false,
    };
    (first == '-' || first == '*' || first == '_') && t.chars().all(|c| c == first || c == ' ')
}

/// Strip a heading prefix (`# ` .. `###### `) and return `(level, content)`.
fn strip_heading(line: &str) -> Option<(usize, &str)> {
    let rest = line.trim_start_matches('#');
    let level = line.len() - rest.len();
    if level == 0 || level > 6 {
        return None;
    }
    // ATX headings require a space after the `#`s.
    let content = rest.strip_prefix(' ')?;
    if content.trim().is_empty() {
        return None;
    }
    Some((level, content))
}

/// Strip a numbered list prefix (`1. `, `42. `) and return the remainder.
fn strip_numbered(lead: &str) -> Option<&str> {
    let digits_end = lead.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_end == 0 || digits_end >= lead.len() {
        return None;
    }
    lead.get(digits_end..)?
        .strip_prefix(". ")
        .or_else(|| lead.get(digits_end..)?.strip_prefix(") "))
}

/// Build a heading line with level-appropriate styling.
///
/// The hierarchy is gold → gold → white, all bold:
/// * **H1** is a filled ember-gold pill — near-black [`theme::GROUND`] text on
///   an [`theme::AURORA_CYAN`] background, with a space of padding each side so
///   it reads as a solid title bar. This is the deliberate high-contrast
///   replacement for the old washed-out heading.
/// * **H2** is bold ember-gold text (no fill).
/// * **H3+** is bold primary-ink text.
fn heading_line(content: &str, level: usize) -> Line<'static> {
    if level == 1 {
        // One span so the gold fill is a single unbroken pill behind the text.
        let pill = Style::new()
            .bg(theme::AURORA_CYAN)
            .fg(theme::GROUND)
            .add_modifier(Modifier::BOLD);
        return Line::from(Span::styled(format!(" ◆ {content} "), pill));
    }
    let (prefix, style) = match level {
        2 => (
            "◈ ",
            Style::new()
                .fg(theme::AURORA_CYAN)
                .add_modifier(Modifier::BOLD),
        ),
        _ => (
            "· ",
            Style::new().fg(theme::INK).add_modifier(Modifier::BOLD),
        ),
    };
    Line::from(vec![
        Span::styled(prefix.to_string(), style),
        Span::styled(content.to_string(), style),
    ])
}

/// Style for inline code spans and fenced code blocks.
fn code_style() -> Style {
    Style::new().fg(theme::WARN)
}

/// One line inside a fenced code block: indented two spaces, tokenized in the
/// fence's language when it named one we highlight (keywords/strings/numbers/
/// comments take their syntax colors; plain runs keep the code amber), or
/// rendered verbatim in the code style otherwise.
fn code_block_line(raw: &str, lang: Option<syntax::Lang>) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", code_style())];
    match lang {
        Some(lang) => {
            for (text, tok) in syntax::tokenize(raw, lang) {
                spans.push(match tok {
                    Some(t) => Span::styled(text, syntax::tok_style(t)),
                    None => Span::styled(text, code_style()),
                });
            }
        }
        None => spans.push(Span::styled(raw.to_string(), code_style())),
    }
    Line::from(spans)
}

// ── Search helpers ─────────────────────────────────────────────────────────

/// Find the index of `needle` in `chars[start..]`, or `None`.
fn find_str(chars: &[char], start: usize, needle: &str) -> Option<usize> {
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() || start >= chars.len() {
        return None;
    }
    let end_bound = chars.len().saturating_sub(needle_chars.len());
    for i in start..=end_bound {
        if chars[i..i + needle_chars.len()] == needle_chars[..] {
            return Some(i);
        }
    }
    None
}

/// Find the index of `target` in `chars[start..]`, or `None`.
fn find_char(chars: &[char], start: usize, target: char) -> Option<usize> {
    chars[start..]
        .iter()
        .position(|&c| c == target)
        .map(|p| start + p)
}

/// Find a single delimiter (e.g. `*`) while skipping doubled pairs (`**`).
/// This prevents `*italic **bold** text*` from matching the first `*` of `**`.
fn find_single_delim(chars: &[char], start: usize, target: char) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == target {
            if i + 1 < chars.len() && chars[i + 1] == target {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_spans_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn plain_text_passes_through() {
        let lines = render("hello world");
        assert_eq!(lines.len(), 1);
        assert_eq!(collect_spans_text(&lines[0]), "hello world");
    }

    #[test]
    fn bold_is_bolded() {
        let lines = render("**important**");
        let span = &lines[0].spans[0];
        assert!(
            span.style.add_modifier.contains(Modifier::BOLD),
            "bold modifier set"
        );
    }

    #[test]
    fn italic_is_italicized() {
        let lines = render("*emphasis*");
        let span = &lines[0].spans[0];
        assert!(
            span.style.add_modifier.contains(Modifier::ITALIC),
            "italic modifier set"
        );
    }

    #[test]
    fn code_span_has_yellow_fg() {
        let lines = render("inline `code` here");
        // Three spans: "inline ", code, " here"
        let code_span = &lines[0].spans[1];
        assert_eq!(code_span.content, "code");
        assert_eq!(code_span.style.fg, Some(theme::WARN));
    }

    #[test]
    fn headings_get_bold() {
        let lines = render("# Title\n## Subtitle\n### Section");
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let span = &line.spans[0];
            assert!(
                span.style.add_modifier.contains(Modifier::BOLD),
                "heading is bold"
            );
        }
        // H1 is a padded pill; H2/H3 keep their glyph prefixes.
        assert_eq!(collect_spans_text(&lines[0]), " \u{25c6} Title ");
        assert_eq!(collect_spans_text(&lines[1]), "\u{25c8} Subtitle");
        assert_eq!(collect_spans_text(&lines[2]), "\u{b7} Section");
    }

    #[test]
    fn h1_is_a_high_contrast_gold_pill() {
        // The exact fix the user asked for: the H1 must be a bold, filled,
        // high-contrast bar — near-black ink on ember gold — never washed-out
        // or a light-text-on-pale-background combination.
        let lines = render("# Rust Async Patterns");
        let span = &lines[0].spans[0];
        assert_eq!(span.style.bg, Some(theme::AURORA_CYAN), "gold fill");
        assert_eq!(span.style.fg, Some(theme::GROUND), "near-black text");
        assert!(span.style.add_modifier.contains(Modifier::BOLD), "bold");
    }

    #[test]
    fn no_baby_blue_cyan_in_rendered_markdown() {
        // Nothing markdown emits should carry these old baby-blue cyan values.
        const OLD_CYANS: [ratatui::style::Color; 2] = [
            ratatui::style::Color::Rgb(0x60, 0xBF, 0xD6),
            ratatui::style::Color::Rgb(126, 197, 214),
        ];
        let lines = render(
            "# Heading\n\nBody with a [link](https://example.com) and `code`.\n\n```\nlet n = 42;\n```",
        );
        for line in &lines {
            for span in &line.spans {
                assert!(
                    !OLD_CYANS.contains(&span.style.fg.unwrap_or(ratatui::style::Color::Reset)),
                    "a span still uses a baby-blue fg"
                );
                assert!(
                    !OLD_CYANS.contains(&span.style.bg.unwrap_or(ratatui::style::Color::Reset)),
                    "a span still uses a baby-blue bg"
                );
            }
        }
    }

    #[test]
    fn bullet_list_items_get_bullet_prefix() {
        let lines = render("- first\n- second");
        assert_eq!(lines.len(), 2);
        assert!(
            collect_spans_text(&lines[0]).contains('•'),
            "bullet prefix rendered"
        );
    }

    #[test]
    fn numbered_list_items_are_indented() {
        let lines = render("1. first\n2. second");
        assert_eq!(lines.len(), 2);
        assert!(collect_spans_text(&lines[0]).contains("first"));
    }

    #[test]
    fn fenced_code_block_renders_indented() {
        let lines = render("```\nfn main() {}\n```");
        assert_eq!(lines.len(), 1);
        let text = collect_spans_text(&lines[0]);
        assert!(
            text.contains("fn main"),
            "code block content visible: {text}"
        );
    }

    #[test]
    fn link_shows_text_and_url() {
        let lines = render("[docs](https://example.com)");
        let text = collect_spans_text(&lines[0]);
        assert!(
            text.contains("docs") && text.contains("example.com"),
            "link text and url visible: {text}"
        );
    }

    #[test]
    fn blockquote_gets_bar_prefix() {
        let lines = render("> quoted text");
        let text = collect_spans_text(&lines[0]);
        assert!(text.contains('▎'), "blockquote bar rendered: {text}");
        assert!(text.contains("quoted text"));
    }

    #[test]
    fn horizontal_rule_renders_as_line() {
        let lines = render("---");
        assert_eq!(lines.len(), 1);
        let text = collect_spans_text(&lines[0]);
        assert!(text.contains('─'), "rule rendered as dashes: {text}");
    }

    #[test]
    fn unmatched_delimiters_are_literal() {
        let lines = render("this *is not closed");
        assert_eq!(lines.len(), 1);
        let text = collect_spans_text(&lines[0]);
        assert_eq!(text, "this *is not closed");
    }

    #[test]
    fn mixed_inline_formatting() {
        let lines = render("This is **bold** and `code` and *italic*");
        assert_eq!(lines.len(), 1);
        // Should have multiple spans with different styles
        assert!(lines[0].spans.len() > 1);
    }

    #[test]
    fn empty_string_produces_no_lines() {
        let lines = render("");
        assert!(lines.is_empty());
    }

    #[test]
    fn nested_bold_in_italic() {
        let lines = render("*outer **bold** outer*");
        assert_eq!(lines.len(), 1);
        // The "bold" span should have both ITALIC and BOLD
        let bold_span = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "bold")
            .expect("bold span exists");
        assert!(
            bold_span
                .style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC),
            "nested formatting: bold inside italic has both modifiers"
        );
    }

    #[test]
    fn tagged_fences_highlight_their_language() {
        let lines = render("```rust\nfn main() {}\n```");
        assert_eq!(lines.len(), 1);
        let kw = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "fn")
            .expect("keyword is its own span");
        assert_eq!(kw.style.fg, Some(theme::SYNTAX_KEYWORD), "keyword colored");
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.style.fg == Some(theme::WARN)),
            "plain runs keep the amber code style: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn untagged_fences_keep_the_uniform_code_style() {
        let lines = render("```\nfn main() {}\n```");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|s| s.style.fg == Some(theme::WARN)),
            "no tag, no tokenizing: {:?}",
            lines[0].spans
        );
    }

    #[test]
    fn toml_fences_highlight_keys_and_values() {
        let lines = render("```toml\n[server]\nport = 8080\n```");
        assert_eq!(lines.len(), 2);
        let header = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "[server]")
            .expect("table header is its own span");
        assert_eq!(header.style.fg, Some(theme::SYNTAX_KEYWORD));
        let num = lines[1]
            .spans
            .iter()
            .find(|s| s.content == "8080")
            .expect("value is its own span");
        assert_eq!(num.style.fg, Some(theme::SYNTAX_NUMBER));
    }
}
