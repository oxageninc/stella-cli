//! GitHub-PR-style diff presentation, shared by every diff surface (the
//! session REPL's right pane and the deck's Files tab) so there is exactly one
//! implementation of "how a diff looks". The layout is the design-doc
//! contract: the full file path inline in a horizontal rule **above** the
//! body, a line-number gutter on the body itself, and a closing rule **below**
//! that counts the added/removed lines. Colors come from [`crate::theme`]
//! only — the add/remove/hunk semantics stay consistent with the rest of the
//! deck (and with any future light variant of the theme) by construction.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

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
pub fn body_lines(diff: &str) -> Vec<Line<'static>> {
    let mut old_no: Option<u32> = None;
    let mut new_no: Option<u32> = None;
    let mut in_hunk = false;
    // `.lines()`, not `.split('\n')`: a diff ending in a trailing newline
    // must not render (and count against hunk state) a spurious empty row.
    diff.lines()
        .map(|raw| body_line(raw, &mut old_no, &mut new_no, &mut in_hunk))
        .collect()
}

fn body_line(
    raw: &str,
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
        Some(b'+') => {
            let n = *new_no;
            *new_no = new_no.map(|n| n + 1);
            Line::from(vec![
                gutter(n),
                Span::styled(
                    raw.to_string(),
                    Style::default().fg(theme::OK).bg(theme::DIFF_ADD_BG),
                ),
            ])
        }
        Some(b'-') => {
            let n = *old_no;
            *old_no = old_no.map(|n| n + 1);
            Line::from(vec![
                gutter(n),
                Span::styled(
                    raw.to_string(),
                    Style::default().fg(theme::BAD).bg(theme::DIFF_DEL_BG),
                ),
            ])
        }
        _ => {
            let n = *new_no;
            *old_no = old_no.map(|n| n + 1);
            *new_no = new_no.map(|n| n + 1);
            Line::from(vec![
                gutter(n),
                Span::styled(raw.to_string(), theme::muted()),
            ])
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let lines = body_lines(SAMPLE);
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
        let lines = body_lines(SAMPLE);
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
        let lines = body_lines("+first\n-gone");
        assert!(line_text(&lines[0]).starts_with("     +first"));
        assert!(line_text(&lines[1]).starts_with("     -gone"));
    }

    #[test]
    fn malformed_hunk_header_resets_numbering_without_panic() {
        let lines = body_lines("@@ nonsense @@\n+x");
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
        let lines = body_lines(diff);
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
            body_lines(SAMPLE).len(),
            body_lines(&with_trailing_newline).len(),
            "a trailing newline must not render a spurious extra row"
        );
    }

    #[test]
    fn no_newline_marker_gets_no_number_and_does_not_shift_later_numbering() {
        let diff =
            "--- a/x.rs\n+++ b/x.rs\n@@ -1,1 +1,1 @@\n-old\n\\ No newline at end of file\n+new\n";
        let lines = body_lines(diff);
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
}
