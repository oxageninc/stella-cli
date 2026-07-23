//! GitHub-PR-style diff presentation, shared by every diff surface (the
//! session REPL's right pane and the deck's Files tab) so there is exactly one
//! implementation of "how a diff looks". The layout is the design-doc
//! contract: the full file path inline in a horizontal rule **above** the
//! body, a line-number gutter on the body itself, and a closing rule **below**
//! that counts the added/removed lines. Colors come from [`crate::theme`]
//! only — the add/remove/hunk semantics stay consistent with the rest of the
//! deck (and with any future light variant of the theme) by construction.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::syntax::{Lang, lang_from_path, tok_style, tokenize};
use crate::theme;

/// Width of the right-aligned line-number gutter, excluding its trailing
/// space. Four digits covers files to 9999 lines; longer files clip the
/// gutter, never the code.
const GUTTER_W: usize = 4;

/// Count added/removed source lines in a unified diff. File headers (`+++ `,
/// `--- `) and hunk markers (`@@`) are ignored; only real `+`/`-` body lines
/// count. Headers are recognized **structurally** (only before the first
/// hunk of a file) rather than by text prefix alone: an added/removed body
/// line whose source text itself starts with `++ `/`-- ` arrives as
/// `+++ `/`--- ` once the diff adds its own marker — textually identical to
/// a real file header — so only "have we seen a hunk yet" disambiguates it.
/// Robust to `None`/partial diffs — a malformed diff yields `(0, 0)`, never a
/// panic.
pub fn count_diff_lines(diff: &str) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff ") {
            in_hunk = false;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk && (line.starts_with("+++ ") || line.starts_with("--- ")) {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => added += 1,
            Some(b'-') => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

/// The rule above a diff: `── path/to/file.rs ─────…` — the full path inline
/// with the horizontal rule, left-elided (keeping the meaningful tail) when
/// the panel is narrower than the path.
pub fn header_line(path: &str, width: usize) -> Line<'static> {
    let lead = "── ";
    let path = elide_left(path, width.saturating_sub(lead.chars().count() + 4));
    let used = lead.chars().count() + path.chars().count() + 1; // trailing space before the fill join
    Line::from(vec![
        Span::styled(lead.to_string(), theme::rule()),
        Span::styled(path, theme::heading()),
        Span::styled(format!(" {}", rule_fill(used, width)), theme::rule()),
    ])
}

/// The rule below a diff: `── +4 additions · -1 removal ─────…` — the line
/// counts the body actually shows, colored with the add/remove semantics.
pub fn footer_line(added: u32, removed: u32, width: usize) -> Line<'static> {
    let lead = "── ";
    let add_txt = format!("+{added} {}", plural(added, "addition"));
    let sep = " · ";
    let rem_txt = format!("-{removed} {}", plural(removed, "removal"));
    // trailing space before the fill join; `sep` is measured in chars (not
    // `.len()` bytes) since it contains the multi-byte `·` glyph.
    let used = lead.chars().count()
        + add_txt.chars().count()
        + sep.chars().count()
        + rem_txt.chars().count()
        + 1;
    Line::from(vec![
        Span::styled(lead.to_string(), theme::rule()),
        Span::styled(add_txt, Style::default().fg(theme::OK)),
        Span::styled(sep.to_string(), theme::rule()),
        Span::styled(rem_txt, Style::default().fg(theme::BAD)),
        Span::styled(format!(" {}", rule_fill(used, width)), theme::rule()),
    ])
}

/// The styled diff body: one `Line` per diff line, with a line-number gutter
/// tracked from the `@@ -a,b +c,d @@` hunk headers — added/context lines are
/// numbered on the new side, removed lines on the old side, exactly like a
/// PR view. Lines outside any hunk (`diff --git`, `index`, `+++`/`---`
/// headers, or a diff with no hunk header at all) simply get no number —
/// malformed input degrades to unnumbered styled text, never a panic.
///
/// `path` is the file the diff belongs to (the diffs on stella's event path
/// are bare hunks with no `diff --git` header, so the caller supplies it);
/// when it names a language we recognize, the code tokens inside each body
/// line get syntax colors *layered under* the add/remove semantics (the
/// `+`/`-` background is preserved). A supplied path *alone* decides the
/// language — an unknown extension disables highlighting rather than falling
/// back to header sniffing, because on the event path the "diff" is a
/// headerless pseudo-diff whose own content (`--- an SQL comment`) can spoof
/// a header. Only with no path at all is the diff's real `diff --git` /
/// `+++` header consulted. An unknown language renders plain, byte-for-byte.
pub fn body_lines(diff: &str, path: Option<&str>) -> Vec<Line<'static>> {
    body_lines_capped(diff, path, usize::MAX).0
}

/// Like [`body_lines`], but styles at most `cap` lines, returning the styled
/// prefix plus the total line count the full render would produce — so a
/// caller that collapses long diffs (the inline transcript view) never pays
/// tokenizing cost for lines it drops.
pub fn body_lines_capped(
    diff: &str,
    path: Option<&str>,
    cap: usize,
) -> (Vec<Line<'static>>, usize) {
    let lang = match path {
        Some(p) => lang_from_path(p),
        None => lang_from_diff_header(diff),
    };
    let mut old_no: Option<u32> = None;
    let mut new_no: Option<u32> = None;
    let mut in_hunk = false;
    let mut lines = Vec::new();
    let mut total = 0usize;
    // `.lines()`, not `.split('\n')`: a diff ending in a trailing newline
    // must not render (and count against hunk state) a spurious empty row.
    for raw in diff.lines() {
        total += 1;
        if lines.len() < cap {
            lines.push(body_line(raw, lang, &mut old_no, &mut new_no, &mut in_hunk));
        }
    }
    (lines, total)
}

fn body_line(
    raw: &str,
    lang: Option<Lang>,
    old_no: &mut Option<u32>,
    new_no: &mut Option<u32>,
    in_hunk: &mut bool,
) -> Line<'static> {
    if raw.starts_with("diff ") {
        // A new file section: the next `+++ `/`--- ` pair are headers again.
        *in_hunk = false;
        *old_no = None;
        *new_no = None;
        return Line::from(vec![
            gutter(None),
            Span::styled(raw.to_string(), theme::muted()),
        ]);
    }
    if raw.starts_with("@@") {
        *in_hunk = true;
        if let Some((old, new)) = parse_hunk(raw) {
            *old_no = Some(old);
            *new_no = Some(new);
        } else {
            *old_no = None;
            *new_no = None;
        }
        return Line::from(vec![
            gutter(None),
            Span::styled(raw.to_string(), Style::default().fg(theme::RUN)),
        ]);
    }
    // Diff-tool metadata ("\ No newline at end of file"), not a source
    // line — must not consume a gutter number on either side.
    if raw.starts_with('\\') {
        return Line::from(vec![
            gutter(None),
            Span::styled(raw.to_string(), theme::muted()),
        ]);
    }
    // File headers are recognized structurally (only before the first hunk
    // of a file): once inside a hunk, added/removed source text that starts
    // with `++ `/`-- ` arrives as `+++ `/`--- ` — textually identical to a
    // real header — and must render as body content instead.
    if !*in_hunk
        && (raw.starts_with("+++ ") || raw.starts_with("--- ") || raw.starts_with("index "))
    {
        return Line::from(vec![
            gutter(None),
            Span::styled(raw.to_string(), theme::muted()),
        ]);
    }
    match raw.as_bytes().first() {
        // `+`/`-`/` ` are ASCII (one byte), so `raw[1..]` splits the diff
        // marker off the code safely for tokenizing.
        Some(b'+') => {
            let n = *new_no;
            *new_no = new_no.map(|n| n + 1);
            let mut spans = vec![gutter(n)];
            spans.extend(code_spans(
                "+",
                &raw[1..],
                theme::OK,
                Some(theme::DIFF_ADD_BG),
                lang,
            ));
            Line::from(spans)
        }
        Some(b'-') => {
            let n = *old_no;
            *old_no = old_no.map(|n| n + 1);
            let mut spans = vec![gutter(n)];
            spans.extend(code_spans(
                "-",
                &raw[1..],
                theme::BAD,
                Some(theme::DIFF_DEL_BG),
                lang,
            ));
            Line::from(spans)
        }
        _ => {
            let n = *new_no;
            *old_no = old_no.map(|n| n + 1);
            *new_no = new_no.map(|n| n + 1);
            // Real unified-diff context lines carry a leading-space marker;
            // headerless pseudo-diffs may not — keep whatever prefix exists in
            // the (uncolored) marker span so the code column stays aligned.
            let (marker, code) = match raw.strip_prefix(' ') {
                Some(rest) => (" ", rest),
                None => ("", raw),
            };
            let mut spans = vec![gutter(n)];
            // Context lines stay fully muted (no syntax colors — `lang` is
            // deliberately not passed): de-emphasis is what separates the
            // unchanged surroundings from the change itself, and a keyword
            // glowing brand-amber on both would erase that distinction.
            spans.extend(code_spans(marker, code, theme::MUTED, None, None));
            Line::from(spans)
        }
    }
}

/// Build the styled spans for one diff body line's content: the uncolored
/// `marker` (`+`/`-`/` `) followed by the `code`. With no known language the
/// code is one span in `base`/`bg` — byte-identical to the plain rendering.
/// With a language, the code is tokenized and each recognized token overrides
/// the foreground with its syntax color while keeping the same `bg`, so the
/// add/remove tint is never lost.
fn code_spans(
    marker: &str,
    code: &str,
    base: Color,
    bg: Option<Color>,
    lang: Option<Lang>,
) -> Vec<Span<'static>> {
    let base_style = with_bg(Style::default().fg(base), bg);
    let Some(lang) = lang else {
        return vec![Span::styled(format!("{marker}{code}"), base_style)];
    };
    let mut spans = Vec::new();
    if !marker.is_empty() {
        spans.push(Span::styled(marker.to_string(), base_style));
    }
    for (text, tok) in tokenize(code, lang) {
        let style = match tok {
            Some(t) => with_bg(tok_style(t), bg),
            None => base_style,
        };
        spans.push(Span::styled(text, style));
    }
    spans
}

/// Apply an optional background to a style (identity when `bg` is `None`).
fn with_bg(style: Style, bg: Option<Color>) -> Style {
    match bg {
        Some(c) => style.bg(c),
        None => style,
    }
}

/// The gutter cell: a right-aligned line number (or blank) plus one space.
fn gutter(n: Option<u32>) -> Span<'static> {
    let text = match n {
        Some(n) => format!("{n:>GUTTER_W$} "),
        None => " ".repeat(GUTTER_W + 1),
    };
    Span::styled(text, theme::muted())
}

/// Parse `@@ -a[,b] +c[,d] @@ …` into the starting `(old, new)` line numbers.
fn parse_hunk(line: &str) -> Option<(u32, u32)> {
    let mut old = None;
    let mut new = None;
    for tok in line.split(' ') {
        if let Some(rest) = tok.strip_prefix('-') {
            old = rest.split(',').next().and_then(|n| n.parse().ok());
        } else if let Some(rest) = tok.strip_prefix('+') {
            new = rest.split(',').next().and_then(|n| n.parse().ok());
        }
    }
    Some((old?, new?))
}

fn plural(n: u32, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// `─` fill from `used` columns out to `width` (empty when already full).
fn rule_fill(used: usize, width: usize) -> String {
    "─".repeat(width.saturating_sub(used))
}

/// Left-elide `text` to at most `max` chars, keeping the tail (the meaningful
/// end of a path) and marking the cut with `…`.
fn elide_left(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let tail: String = chars[chars.len() - (max - 1)..].iter().collect();
    format!("…{tail}")
}

// ── Diff-header language inference ──────────────────────────────────────────
//
// The lexer itself lives in [`crate::syntax`], shared with the markdown
// renderer and the skills/agents definition editors. What stays here is the
// one diff-specific concern: inferring a language from the diff's own headers.

/// Infer the language from a diff's own header lines (`diff --git a/f.rs …`,
/// `+++ b/f.rs`, `--- a/f.rs`), scanning only up to the first hunk.
fn lang_from_diff_header(diff: &str) -> Option<Lang> {
    for line in diff.lines() {
        if line.starts_with("@@") {
            break; // headers only ever precede the first hunk
        }
        if let Some(rest) = line.strip_prefix("diff --git ")
            && let Some(lang) = rest.split_whitespace().find_map(header_path_lang)
        {
            return Some(lang);
        } else if let Some(rest) = line
            .strip_prefix("+++ ")
            .or_else(|| line.strip_prefix("--- "))
            && let Some(lang) = header_path_lang(rest)
        {
            return Some(lang);
        }
    }
    None
}

/// Language of a single diff-header path token, stripping the `a/`/`b/` prefix
/// and any trailing `\t`-separated metadata; `/dev/null` yields `None`.
fn header_path_lang(token: &str) -> Option<Lang> {
    let token = token.split('\t').next().unwrap_or(token).trim();
    if token == "/dev/null" {
        return None;
    }
    let token = token
        .strip_prefix("a/")
        .or_else(|| token.strip_prefix("b/"))
        .unwrap_or(token);
    lang_from_path(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    /// Flatten one styled line back to its text content.
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.clone()).collect()
    }

    const SAMPLE: &str =
        "--- a/x.rs\n+++ b/x.rs\n@@ -1,3 +1,4 @@\n context\n-old line\n+new line\n+another add";

    #[test]
    fn header_carries_the_full_path_inside_a_rule() {
        let text = line_text(&header_line("src/deep/nested/file.rs", 60));
        assert!(text.contains("src/deep/nested/file.rs"), "{text}");
        assert!(text.starts_with("── "), "{text}");
        assert!(text.contains("─────"), "rule fill present: {text}");
    }

    #[test]
    fn header_left_elides_a_path_wider_than_the_panel() {
        let text = line_text(&header_line("a/very/long/path/that/wont/fit.rs", 24));
        assert!(text.contains('…'), "{text}");
        assert!(text.contains("fit.rs"), "the tail survives: {text}");
    }

    #[test]
    fn footer_counts_and_pluralizes() {
        let text = line_text(&footer_line(4, 1, 60));
        assert!(text.contains("+4 additions"), "{text}");
        assert!(text.contains("-1 removal"), "{text}");
        assert!(!text.contains("removals"), "singular for 1: {text}");
    }

    #[test]
    fn body_numbers_added_lines_on_the_new_side_and_removed_on_the_old() {
        let lines = body_lines(SAMPLE, None);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // "@@ -1,3 +1,4 @@" starts old=1/new=1; context takes new 1.
        assert!(texts[3].starts_with("   1  context"), "{:?}", texts[3]);
        // The removal is numbered on the OLD side (old line 2).
        assert!(texts[4].starts_with("   2 -old line"), "{:?}", texts[4]);
        // Additions continue on the NEW side (new lines 2, 3).
        assert!(texts[5].starts_with("   2 +new line"), "{:?}", texts[5]);
        assert!(texts[6].starts_with("   3 +another add"), "{:?}", texts[6]);
    }

    #[test]
    fn file_headers_and_hunks_get_no_number() {
        let lines = body_lines(SAMPLE, None);
        for (i, line) in lines.iter().take(3).enumerate() {
            assert!(
                line_text(line).starts_with("     "),
                "line {i} has a blank gutter: {:?}",
                line_text(line)
            );
        }
    }

    #[test]
    fn a_diff_without_hunk_headers_degrades_to_unnumbered_lines() {
        let lines = body_lines("+first\n-gone", None);
        assert!(line_text(&lines[0]).starts_with("     +first"));
        assert!(line_text(&lines[1]).starts_with("     -gone"));
    }

    #[test]
    fn malformed_hunk_header_resets_numbering_without_panic() {
        let lines = body_lines("@@ nonsense @@\n+x", None);
        assert!(line_text(&lines[1]).starts_with("     +x"));
    }

    #[test]
    fn count_diff_lines_ignores_headers_and_hunks() {
        assert_eq!(count_diff_lines(SAMPLE), (2, 1));
        assert_eq!(count_diff_lines(""), (0, 0));
        assert_eq!(count_diff_lines("no markers"), (0, 0));
    }

    #[test]
    fn count_diff_lines_counts_hunk_body_text_matching_header_syntax() {
        // Added/removed source text starting with `++ `/`-- ` arrives as
        // `+++ `/`--- ` once the diff adds its own marker — textually
        // identical to a real file header. Only hunk position (we're
        // already inside a hunk) can tell them apart.
        let diff = "--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n--- was a rule\n+++ is a rule\n";
        assert_eq!(count_diff_lines(diff), (1, 1));
    }

    #[test]
    fn body_lines_number_hunk_text_matching_header_syntax_instead_of_hiding_it() {
        let diff = "--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n--- was a rule\n+++ is a rule\n";
        let lines = body_lines(diff, None);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            !texts[3].starts_with("     "),
            "removed body line should get a gutter number, not read as a header: {:?}",
            texts[3]
        );
        assert!(
            !texts[4].starts_with("     "),
            "added body line should get a gutter number, not read as a header: {:?}",
            texts[4]
        );
        assert!(texts[3].contains("was a rule"), "{:?}", texts[3]);
        assert!(texts[4].contains("is a rule"), "{:?}", texts[4]);
    }

    #[test]
    fn body_lines_ignores_a_trailing_newline() {
        let with_trailing_newline = format!("{SAMPLE}\n");
        assert_eq!(
            body_lines(SAMPLE, None).len(),
            body_lines(&with_trailing_newline, None).len(),
            "a trailing newline must not render a spurious extra row"
        );
    }

    #[test]
    fn no_newline_marker_gets_no_number_and_does_not_shift_later_numbering() {
        let diff =
            "--- a/x.rs\n+++ b/x.rs\n@@ -1,1 +1,1 @@\n-old\n\\ No newline at end of file\n+new\n";
        let lines = body_lines(diff, None);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts[4].starts_with("     "),
            "the marker line itself gets a blank gutter: {:?}",
            texts[4]
        );
        assert!(
            texts[5].starts_with("   1 +new"),
            "the marker must not have consumed a line number: {:?}",
            texts[5]
        );
    }

    #[test]
    fn header_rule_fills_to_the_full_panel_width() {
        let width = 60;
        let total: usize = header_line("src/main.rs", width)
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(total, width, "the rule should reach the panel's right edge");
    }

    #[test]
    fn footer_rule_fills_to_the_full_panel_width() {
        let width = 60;
        let total: usize = footer_line(4, 1, width)
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(total, width, "the rule should reach the panel's right edge");
    }

    #[test]
    fn header_uses_char_count_not_byte_length_for_the_lead_when_eliding() {
        // 70 chars: longer than the old byte-length-based cap (80 - 7 - 4 =
        // 69, since "── " is 7 bytes) but shorter than the correct
        // char-count-based cap (80 - 3 - 4 = 73). Only survives un-elided
        // once the lead is measured in chars, not bytes.
        let path = "a".repeat(70);
        let text = line_text(&header_line(&path, 80));
        assert!(!text.contains('…'), "path elided too early: {text}");
    }

    // ── Syntax highlighting ─────────────────────────────────────────────

    /// Find the span whose exact content is `text`, if any.
    fn span_with<'a>(line: &'a Line<'a>, text: &str) -> Option<&'a Span<'a>> {
        line.spans.iter().find(|s| s.content == text)
    }

    #[test]
    fn highlighted_added_line_keeps_add_background_and_colors_tokens() {
        // A Rust `+` line: the add background must survive on the code (never
        // lost) AND `fn`/`let` get the keyword color, `42` the number color —
        // syntax layered *under* the diff semantics.
        let diff = "@@ -1,1 +1,1 @@\n+    fn f() { let x = 42; }";
        let line = body_lines(diff, Some("src/x.rs")).pop().unwrap();

        // The add background is present somewhere on the code spans.
        assert!(
            line.spans
                .iter()
                .any(|s| s.style.bg == Some(theme::DIFF_ADD_BG)),
            "add background preserved: {:?}",
            line.spans
        );
        let kw = span_with(&line, "fn").expect("`fn` is its own span");
        assert_eq!(kw.style.fg, Some(theme::SYNTAX_KEYWORD), "keyword colored");
        assert_eq!(
            kw.style.bg,
            Some(theme::DIFF_ADD_BG),
            "keyword still on the add background"
        );
        assert!(span_with(&line, "let").is_some(), "second keyword present");
        let num = span_with(&line, "42").expect("`42` is its own span");
        assert_eq!(num.style.fg, Some(theme::SYNTAX_NUMBER), "number colored");
        // Lossless: the text still reads back intact, marker included.
        assert!(line_text(&line).contains("+    fn f() { let x = 42; }"));
    }

    #[test]
    fn highlighted_removed_line_keeps_del_background_and_colors_keywords() {
        let diff = "@@ -1,1 +1,1 @@\n-def go():";
        let line = body_lines(diff, Some("app.py")).pop().unwrap();
        let kw = span_with(&line, "def").expect("`def` is its own span");
        assert_eq!(kw.style.fg, Some(theme::SYNTAX_KEYWORD));
        assert_eq!(
            kw.style.bg,
            Some(theme::DIFF_DEL_BG),
            "keyword on the del background, so removal is never lost"
        );
    }

    #[test]
    fn strings_and_comments_get_their_syntax_colors() {
        let diff = "@@ -1,1 +1,1 @@\n+let s = \"hi\"; // note";
        let line = body_lines(diff, Some("x.ts")).pop().unwrap();
        let s = span_with(&line, "\"hi\"").expect("string is its own span");
        assert_eq!(s.style.fg, Some(theme::SYNTAX_STRING));
        let c = span_with(&line, "// note").expect("comment runs to end of line");
        assert_eq!(c.style.fg, Some(theme::SYNTAX_COMMENT));
        assert!(c.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn unknown_language_falls_back_to_a_single_plain_code_span() {
        // No path and no inferable header → one code span per line, exactly
        // like the pre-highlighting rendering (gutter + one styled span).
        let line = body_lines("@@ -1 +1 @@\n+fn x", None).pop().unwrap();
        assert_eq!(
            line.spans.len(),
            2,
            "gutter + one plain span: {:?}",
            line.spans
        );
        assert_eq!(line.spans[1].content, "+fn x");
        assert_eq!(line.spans[1].style.fg, Some(theme::OK));
        assert_eq!(line.spans[1].style.bg, Some(theme::DIFF_ADD_BG));
    }

    #[test]
    fn language_is_inferred_from_the_diff_header_when_no_path_is_given() {
        let diff = "diff --git a/m.rs b/m.rs\n@@ -1 +1 @@\n+fn q() {}";
        let line = body_lines(diff, None).pop().unwrap();
        assert_eq!(
            span_with(&line, "fn").map(|s| s.style.fg),
            Some(Some(theme::SYNTAX_KEYWORD)),
            "header-inferred Rust highlights `fn`: {:?}",
            line.spans
        );
    }

    #[test]
    fn rust_lifetimes_do_not_swallow_the_line_as_a_string() {
        // `'a` is a lifetime, not a char literal — it must not open a string
        // run that eats the rest of the line.
        let diff = "@@ -1 +1 @@\n+fn f<'a>(x: &'a str) {}";
        let line = body_lines(diff, Some("x.rs")).pop().unwrap();
        assert!(
            !line
                .spans
                .iter()
                .any(|s| s.style.fg == Some(theme::SYNTAX_STRING)),
            "no string span from a lifetime: {:?}",
            line.spans
        );
        assert!(line_text(&line).contains("&'a str"), "text intact");
    }

    #[test]
    fn an_explicit_path_alone_decides_the_language_never_the_diff_content() {
        // Event-path pseudo-diffs have no headers, so their *content* must
        // never be sniffed as one: a removed SQL comment `-- see load.py`
        // renders as `--- see load.py`, which looks exactly like a header
        // naming a Python file. With a path supplied (unknown extension),
        // highlighting must simply stay off.
        let diff = "--- see scripts/load.py\n+import x";
        let line = body_lines(diff, Some("q.sql")).pop().unwrap();
        assert_eq!(
            line.spans.len(),
            2,
            "gutter + one plain span, no Python keywords: {:?}",
            line.spans
        );
    }

    #[test]
    fn context_lines_stay_fully_muted_without_syntax_colors() {
        // De-emphasis is the whole point of a context line: even with a
        // recognized language its keywords keep the muted foreground.
        let diff = "@@ -1,2 +1,2 @@\n fn unchanged() {}\n+fn added() {}";
        let lines = body_lines(diff, Some("m.rs"));
        let context = &lines[1];
        assert_eq!(
            context.spans.len(),
            2,
            "gutter + one muted span: {:?}",
            context.spans
        );
        assert_eq!(context.spans[1].style.fg, Some(theme::MUTED));
        let added = &lines[2];
        assert_eq!(
            span_with(added, "fn").map(|s| s.style.fg),
            Some(Some(theme::SYNTAX_KEYWORD)),
            "added lines still highlight: {:?}",
            added.spans
        );
    }

    #[test]
    fn capped_rendering_styles_only_the_cap_but_counts_every_line() {
        let diff = "+one\n+two\n+three\n+four\n+five";
        let (lines, total) = body_lines_capped(diff, Some("x.rs"), 2);
        assert_eq!(lines.len(), 2, "styles stop at the cap");
        assert_eq!(total, 5, "the footer math sees the full length");
        // An uncapped call is byte-identical to `body_lines`.
        let (all, n) = body_lines_capped(diff, Some("x.rs"), usize::MAX);
        assert_eq!(n, 5);
        assert_eq!(all, body_lines(diff, Some("x.rs")));
    }

    #[test]
    fn markdown_and_toml_files_highlight_by_extension() {
        // A skill/agent definition diff: markdown structure colors, with the
        // add background preserved under it.
        let line = body_lines("@@ -1 +1 @@\n+## Setup", Some("skills/x/SKILL.md"))
            .pop()
            .unwrap();
        let kw = span_with(&line, "## Setup").expect("heading is its own span");
        assert_eq!(kw.style.fg, Some(theme::SYNTAX_KEYWORD), "heading colored");
        assert_eq!(kw.style.bg, Some(theme::DIFF_ADD_BG), "add tint preserved");
        // A config diff: TOML keys and values color.
        let line = body_lines("@@ -1 +1 @@\n+port = 8080", Some(".stella/mcp.toml"))
            .pop()
            .unwrap();
        assert_eq!(
            span_with(&line, "port").map(|s| s.style.fg),
            Some(Some(theme::SYNTAX_KEYWORD)),
            "key colored: {:?}",
            line.spans
        );
        assert_eq!(
            span_with(&line, "8080").map(|s| s.style.fg),
            Some(Some(theme::SYNTAX_NUMBER)),
            "value colored: {:?}",
            line.spans
        );
    }
}
