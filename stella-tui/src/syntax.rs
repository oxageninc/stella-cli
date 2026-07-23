//! Lightweight syntax highlighting — the deck's one source-coloring engine.
//!
//! A compact, dependency-free lexer (no `syntect`, no tree-sitter — stella-tui
//! stays decoupled) that colors keywords, string/char literals, line comments,
//! and numbers for the languages the deck edits most, plus structural coloring
//! for Markdown and TOML sources. It is intentionally *not* a parser: one
//! left-to-right scan per line, so a line sliced out of context (a diff hunk
//! cutting a block comment in half) degrades to slightly-off coloring, never a
//! wrong render or a panic.
//!
//! Consumers: diff bodies ([`crate::diff`]), fenced code blocks in rendered
//! markdown ([`crate::markdown`]), and the skills / agents definition editors,
//! which highlight `SKILL.md` / `<agent>.md` *source* while it is edited. The
//! editors hold whole files, where cross-line facts (YAML frontmatter, fenced
//! code) are knowable — they feed lines through a [`Highlighter`], which
//! tracks that state and lights fence interiors up in their own language.
//! Colors come from [`crate::theme`] only.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::theme;

/// A language we can syntax-highlight.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Rust,
    /// TypeScript / JavaScript (and their `x`/module variants).
    TsJs,
    Python,
    /// Markdown *source* (headings, list markers, fences, inline code) — the
    /// skills/agents definition format.
    Markdown,
    Toml,
}

/// A token class we give a syntax color; `None` runs stay the base color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    /// Language keywords; also structural markup — markdown headings, TOML
    /// table headers and keys, frontmatter keys — the "shape" of a document.
    Keyword,
    /// String literals; also markdown inline code spans.
    Str,
    /// Numeric literals; also markdown list markers and link URLs (violet is
    /// the theme's link hue).
    Number,
    /// Line comments; also markdown fences, rules, and blockquote markers.
    Comment,
}

/// The tagged runs for one source line: consecutive text slices, each with
/// an optional token class. Concatenating the texts reproduces the line.
pub type Runs = Vec<(String, Option<Tok>)>;

/// The foreground style for a token class (comments also italicize).
pub fn tok_style(t: Tok) -> Style {
    match t {
        Tok::Keyword => Style::default().fg(theme::SYNTAX_KEYWORD),
        Tok::Str => Style::default().fg(theme::SYNTAX_STRING),
        Tok::Number => Style::default().fg(theme::SYNTAX_NUMBER),
        Tok::Comment => Style::default()
            .fg(theme::SYNTAX_COMMENT)
            .add_modifier(Modifier::ITALIC),
    }
}

/// Map a file extension to a language, or `None` if we don't highlight it.
pub fn lang_from_ext(ext: &str) -> Option<Lang> {
    match ext {
        "rs" => Some(Lang::Rust),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => Some(Lang::TsJs),
        "py" | "pyi" => Some(Lang::Python),
        "md" | "markdown" => Some(Lang::Markdown),
        "toml" => Some(Lang::Toml),
        _ => None,
    }
}

/// The language for a file path via its extension (the segment after the last
/// `.`), guarding against a dot that lives in a parent directory rather than
/// the filename.
pub fn lang_from_path(path: &str) -> Option<Lang> {
    let (_, ext) = path.rsplit_once('.')?;
    if ext.contains('/') {
        return None;
    }
    lang_from_ext(ext)
}

/// Map a fenced-code info string (the word after the opening ```) to a
/// language. Unknown tags (`sh`, `json`, …) render plain rather than wrong.
pub fn lang_from_fence(tag: &str) -> Option<Lang> {
    let tag = tag
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match tag.as_str() {
        "rust" | "rs" => Some(Lang::Rust),
        "ts" | "tsx" | "typescript" | "js" | "jsx" | "javascript" | "mjs" | "cjs" => {
            Some(Lang::TsJs)
        }
        "py" | "python" | "python3" => Some(Lang::Python),
        "md" | "markdown" => Some(Lang::Markdown),
        "toml" => Some(Lang::Toml),
        _ => None,
    }
}

/// Split one line of source `code` into consecutive runs, each tagged with an
/// optional token class (`None` = punctuation/whitespace/plain text).
/// Lossless — concatenating the run texts reproduces `code` exactly — and
/// panic-free. Stateless: markdown scans as body prose (fence lines read as
/// markers, but their interiors are unknown here — [`Highlighter`] knows).
pub fn tokenize(code: &str, lang: Lang) -> Runs {
    match lang {
        Lang::Markdown => md_line(code).0,
        Lang::Toml => toml_runs(code),
        Lang::Rust | Lang::TsJs | Lang::Python => code_runs(code, lang),
    }
}

// ── Whole-buffer highlighting (the edit surfaces) ───────────────────────────

/// Markdown cross-line position: only meaningful when the language is
/// [`Lang::Markdown`]; every other language scans line-by-line stateless.
#[derive(Clone, Copy)]
enum MdState {
    /// Before the first line — only there can `---` open YAML frontmatter.
    Lead,
    Frontmatter,
    Body,
    /// Inside a fenced code block, with the fence tag's language (if any).
    Fence(Option<Lang>),
}

/// Cross-line highlighting for a whole buffer, fed top to bottom.
///
/// The per-line [`tokenize`] deliberately keeps no state (a diff hunk can
/// slice a file anywhere); an *editor* holds the entire file, where YAML
/// frontmatter and fenced code blocks are knowable. Feeding each line through
/// one of these colors frontmatter keys, tracks fences, and highlights fence
/// interiors in their own language. `None` passes lines through unstyled, so
/// callers need no unknown-language special case.
pub struct Highlighter {
    lang: Option<Lang>,
    md: MdState,
}

impl Highlighter {
    pub fn new(lang: Option<Lang>) -> Self {
        Self {
            lang,
            md: MdState::Lead,
        }
    }

    /// Tokenize the next line. Lines must arrive in buffer order.
    pub fn runs(&mut self, line: &str) -> Runs {
        let Some(lang) = self.lang else {
            return vec![(line.to_string(), None)];
        };
        if lang != Lang::Markdown {
            return tokenize(line, lang);
        }
        match self.md {
            MdState::Lead if line.trim() == "---" => {
                self.md = MdState::Frontmatter;
                vec![(line.to_string(), Some(Tok::Comment))]
            }
            MdState::Lead => {
                self.md = MdState::Body;
                self.body_runs(line)
            }
            MdState::Frontmatter if line.trim() == "---" || line.trim() == "..." => {
                self.md = MdState::Body;
                vec![(line.to_string(), Some(Tok::Comment))]
            }
            MdState::Frontmatter => frontmatter_runs(line),
            MdState::Fence(inner) if !line.trim_start().starts_with("```") => match inner {
                Some(l) => tokenize(line, l),
                None => vec![(line.to_string(), None)],
            },
            MdState::Fence(_) => {
                self.md = MdState::Body;
                vec![(line.to_string(), Some(Tok::Comment))]
            }
            MdState::Body => self.body_runs(line),
        }
    }

    /// The styled spans for the next line — token colors from [`tok_style`],
    /// plain runs in `base`.
    pub fn spans(&mut self, line: &str, base: Style) -> Vec<Span<'static>> {
        self.runs(line)
            .into_iter()
            .map(|(text, tok)| match tok {
                Some(t) => Span::styled(text, tok_style(t)),
                None => Span::styled(text, base),
            })
            .collect()
    }

    fn body_runs(&mut self, line: &str) -> Runs {
        let (runs, fence) = md_line(line);
        if let Some(inner) = fence {
            self.md = MdState::Fence(inner);
        }
        runs
    }
}

// ── Markdown source ─────────────────────────────────────────────────────────

/// One markdown source line without cross-line context. Returns the runs plus
/// the fence language when the line opens (or closes — the caller's state
/// decides which) a fenced block; the stateless [`tokenize`] path ignores it.
fn md_line(line: &str) -> (Runs, Option<Option<Lang>>) {
    let lead = line.trim_start();
    let indent = &line[..line.len() - lead.len()];
    if let Some(rest) = lead.strip_prefix("```") {
        let tag = rest.trim_start_matches('`');
        return (
            vec![(line.to_string(), Some(Tok::Comment))],
            Some(lang_from_fence(tag)),
        );
    }
    if is_md_hr(lead) {
        return (vec![(line.to_string(), Some(Tok::Comment))], None);
    }
    if is_md_heading(lead) {
        return (vec![(line.to_string(), Some(Tok::Keyword))], None);
    }
    if lead.starts_with('>') {
        // The `>` marker(s) dim; the quoted text scans as prose.
        let rest = line.trim_start_matches([' ', '>']);
        let marker = &line[..line.len() - rest.len()];
        let mut runs = vec![(marker.to_string(), Some(Tok::Comment))];
        runs.extend(md_inline_runs(rest));
        return (runs, None);
    }
    if let Some(rest) = lead
        .strip_prefix("- ")
        .or_else(|| lead.strip_prefix("* "))
        .or_else(|| lead.strip_prefix("+ "))
    {
        return (bullet_runs(indent, &lead[..2], rest), None);
    }
    if let Some((marker, rest)) = split_ordered_marker(lead) {
        return (bullet_runs(indent, marker, rest), None);
    }
    (md_inline_runs(line), None)
}

/// Runs for a list line: plain indent, the marker in the list-marker color,
/// then the item text as prose.
fn bullet_runs(indent: &str, marker: &str, rest: &str) -> Runs {
    let mut runs = Vec::new();
    if !indent.is_empty() {
        runs.push((indent.to_string(), None));
    }
    runs.push((marker.to_string(), Some(Tok::Number)));
    runs.extend(md_inline_runs(rest));
    runs
}

/// Inline markdown prose: `` `code` `` spans (backticks included) and the
/// `(url)` of a `[text](url)` link get color; everything else stays plain —
/// prose should read calm in an editor, not light up like source code.
fn md_inline_runs(text: &str) -> Runs {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut runs: Runs = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < n {
        // An unterminated backtick is prose, not a code span that swallows
        // the rest of the line.
        if chars[i] == '`'
            && let Some(close) = chars[i + 1..].iter().position(|&c| c == '`')
        {
            flush(&mut plain, &mut runs);
            let end = i + 1 + close;
            runs.push((chars[i..=end].iter().collect(), Some(Tok::Str)));
            i = end + 1;
            continue;
        }
        if chars[i] == '['
            && let Some(close) = chars[i + 1..].iter().position(|&c| c == ']')
            && let bracket = i + 1 + close
            && chars.get(bracket + 1) == Some(&'(')
            && let Some(paren) = chars[bracket + 2..].iter().position(|&c| c == ')')
        {
            let end = bracket + 2 + paren;
            plain.extend(chars[i..=bracket].iter()); // `[text]` stays prose
            flush(&mut plain, &mut runs);
            runs.push((chars[bracket + 1..=end].iter().collect(), Some(Tok::Number)));
            i = end + 1;
            continue;
        }
        plain.push(chars[i]);
        i += 1;
    }
    flush(&mut plain, &mut runs);
    runs
}

/// A frontmatter (YAML) line: `key:` colors as structure; the value scans as
/// a config value — quoted strings, numbers, booleans, `#` comments — via the
/// TOML value rules, which YAML scalars share closely enough.
fn frontmatter_runs(line: &str) -> Runs {
    let lead = line.trim_start();
    let indent = &line[..line.len() - lead.len()];
    if let Some((key, rest)) = lead.split_once(':')
        && !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        let mut runs = Vec::new();
        if !indent.is_empty() {
            runs.push((indent.to_string(), None));
        }
        runs.push((key.to_string(), Some(Tok::Keyword)));
        runs.push((":".to_string(), None));
        runs.extend(code_runs(rest, Lang::Toml));
        return runs;
    }
    code_runs(line, Lang::Toml)
}

/// A horizontal rule: 3+ of the same `-`/`*`/`_` (spaces allowed between).
fn is_md_hr(lead: &str) -> bool {
    let t = lead.trim_end();
    let Some(first) = t.chars().next() else {
        return false;
    };
    t.len() >= 3 && matches!(first, '-' | '*' | '_') && t.chars().all(|c| c == first || c == ' ')
}

/// An ATX heading: 1–6 `#`s followed by a space.
fn is_md_heading(lead: &str) -> bool {
    let rest = lead.trim_start_matches('#');
    let level = lead.len() - rest.len();
    (1..=6).contains(&level) && rest.starts_with(' ')
}

/// Split an ordered-list marker (`1. `, `42) `) off `lead`, if present.
fn split_ordered_marker(lead: &str) -> Option<(&str, &str)> {
    let digits = lead.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = lead[digits..]
        .strip_prefix(". ")
        .or_else(|| lead[digits..].strip_prefix(") "))?;
    Some((&lead[..digits + 2], rest))
}

// ── TOML source ─────────────────────────────────────────────────────────────

/// One TOML line: full-line and inline `#` comments, `[table]` / `[[array]]`
/// headers and the key of a `key = value` pair as structure, and values via
/// the generic scan (strings, numbers, booleans). Array/inline-table
/// continuation lines fall through to the generic scan.
fn toml_runs(code: &str) -> Runs {
    let lead = code.trim_start();
    let indent = &code[..code.len() - lead.len()];
    if lead.starts_with('#') {
        return vec![(code.to_string(), Some(Tok::Comment))];
    }
    if lead.starts_with('[') {
        // A table header runs to its matching close bracket; a comma inside
        // means this is really an array value line (`[1, 2],`), not a header.
        let chars: Vec<char> = lead.chars().collect();
        let mut depth = 0usize;
        let mut end = chars.len();
        for (i, c) in chars.iter().enumerate() {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !chars[..end].contains(&',') {
            let mut runs = Vec::new();
            if !indent.is_empty() {
                runs.push((indent.to_string(), None));
            }
            runs.push((chars[..end].iter().collect(), Some(Tok::Keyword)));
            let rest: String = chars[end..].iter().collect();
            if !rest.is_empty() {
                runs.extend(code_runs(&rest, Lang::Toml));
            }
            return runs;
        }
    }
    if let Some(eq) = lead.find('=') {
        let key_part = &lead[..eq];
        let key = key_part.trim_end();
        // Bare, dotted, or quoted keys only — anything else (a `=` inside an
        // array continuation, say) is not a key/value line.
        if !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '"' | '\'' | ' '))
        {
            let mut runs = Vec::new();
            if !indent.is_empty() {
                runs.push((indent.to_string(), None));
            }
            runs.push((key.to_string(), Some(Tok::Keyword)));
            runs.push((format!("{}=", &key_part[key.len()..]), None));
            runs.extend(code_runs(&lead[eq + 1..], Lang::Toml));
            return runs;
        }
    }
    code_runs(code, Lang::Toml)
}

// ── The generic code scan ───────────────────────────────────────────────────

/// The left-to-right run scan for code-shaped languages (Rust, TS/JS, Python,
/// and TOML values): line comments, string/char literals, numbers, keywords.
fn code_runs(code: &str, lang: Lang) -> Runs {
    let chars: Vec<char> = code.chars().collect();
    let n = chars.len();
    let mut runs: Runs = Vec::new();
    let mut plain = String::new();
    let mut i = 0;

    while i < n {
        let c = chars[i];

        // Line comment: `//` (Rust/TS/JS) or `#` (Python/TOML) to end of line.
        if is_comment_start(&chars, i, lang) {
            flush(&mut plain, &mut runs);
            runs.push((chars[i..].iter().collect(), Some(Tok::Comment)));
            return runs;
        }

        // String / char literal.
        if is_string_start(&chars, i, lang) {
            let (end, closed) = scan_string(&chars, i);
            // An unterminated single quote is far more often a contraction in
            // prose ("Don't" in JSX text or a docstring) than a string the
            // hunk cut in half — leave it plain instead of swallowing the
            // rest of the line. Double quotes and backticks keep the
            // string-to-end-of-line reading (a cut hunk is the likely cause).
            if closed || c != '\'' {
                flush(&mut plain, &mut runs);
                runs.push((chars[i..end].iter().collect(), Some(Tok::Str)));
                i = end;
                continue;
            }
            plain.push(c);
            i += 1;
            continue;
        }

        // Number literal (only at a run boundary — identifiers below consume
        // their own trailing digits, so a leading digit here starts a number).
        if c.is_ascii_digit() {
            flush(&mut plain, &mut runs);
            let end = scan_number(&chars, i);
            runs.push((chars[i..end].iter().collect(), Some(Tok::Number)));
            i = end;
            continue;
        }

        // Identifier / keyword.
        if is_ident_start(c) {
            let mut j = i + 1;
            while j < n && is_ident_continue(chars[j]) {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            if is_keyword(&word, lang) {
                flush(&mut plain, &mut runs);
                runs.push((word, Some(Tok::Keyword)));
            } else {
                plain.push_str(&word);
            }
            i = j;
            continue;
        }

        // Anything else accumulates into the current plain run.
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut runs);
    runs
}

/// Push the accumulated plain run (if any) and clear the buffer.
fn flush(plain: &mut String, runs: &mut Runs) {
    if !plain.is_empty() {
        runs.push((std::mem::take(plain), None));
    }
}

fn is_comment_start(chars: &[char], i: usize, lang: Lang) -> bool {
    match lang {
        Lang::Python | Lang::Toml => chars[i] == '#',
        Lang::Rust | Lang::TsJs => chars[i] == '/' && chars.get(i + 1) == Some(&'/'),
        // Markdown never reaches the generic scan ([`tokenize`] dispatches it
        // to [`md_line`]); the arm exists for exhaustiveness only.
        Lang::Markdown => false,
    }
}

/// Whether position `i` opens a string/char literal. Double quotes (and TS/JS
/// template backticks) always do; the single quote is ambiguous in Rust
/// (lifetimes like `&'a T`, `derive('...')`), so there it only counts when it
/// matches a char-literal shape — otherwise the whole line would mis-color.
fn is_string_start(chars: &[char], i: usize, lang: Lang) -> bool {
    match chars[i] {
        '"' => true,
        '`' => lang == Lang::TsJs,
        '\'' => match lang {
            Lang::Rust => is_rust_char_literal(chars, i),
            Lang::Markdown => false,
            _ => true,
        },
        _ => false,
    }
}

/// A Rust char literal at `i`: `'x'` or an escaped `'\n'` / `'\''` / `'\\'`.
fn is_rust_char_literal(chars: &[char], i: usize) -> bool {
    if chars.get(i + 1) == Some(&'\\') {
        chars.get(i + 3) == Some(&'\'')
    } else {
        matches!(chars.get(i + 1), Some(c) if *c != '\'') && chars.get(i + 2) == Some(&'\'')
    }
}

/// Scan a string opened at `i`, honoring backslash escapes. Returns the end
/// index (just past the closing quote, or end of line when unterminated) and
/// whether the closing quote was actually found — the caller uses that to
/// tell a real string from a lone apostrophe in prose.
fn scan_string(chars: &[char], i: usize) -> (usize, bool) {
    let quote = chars[i];
    let n = chars.len();
    let mut j = i + 1;
    while j < n {
        match chars[j] {
            '\\' => j = (j + 2).min(n),
            c if c == quote => return (j + 1, true),
            _ => j += 1,
        }
    }
    (n, false)
}

/// Scan a number opened at `i`: a run of alphanumerics/underscores (covering
/// hex `0xFF`, suffixes `10u64`, separators `1_000`), plus one embedded
/// decimal point followed by more digits (`1.5`), so a `1..2` range keeps its
/// `..` intact. Returns the end index.
fn scan_number(chars: &[char], i: usize) -> usize {
    let n = chars.len();
    let mut j = i;
    while j < n && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if j < n && chars[j] == '.' && chars.get(j + 1).is_some_and(|c| c.is_ascii_digit()) {
        j += 1;
        while j < n && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
    }
    j
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `word` is a keyword in `lang`. Linear scans over small, fixed
/// slices — cheap enough for the handful of identifiers on a line.
fn is_keyword(word: &str, lang: Lang) -> bool {
    let table: &[&str] = match lang {
        Lang::Rust => &RUST_KEYWORDS,
        Lang::TsJs => &TSJS_KEYWORDS,
        Lang::Python => &PYTHON_KEYWORDS,
        Lang::Toml => &TOML_KEYWORDS,
        Lang::Markdown => &[],
    };
    table.contains(&word)
}

const RUST_KEYWORDS: [&str; 39] = [
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "yield",
];

const TSJS_KEYWORDS: [&str; 45] = [
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "of",
    "readonly",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
];

const PYTHON_KEYWORDS: [&str; 35] = [
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield",
];

/// TOML value constants (booleans and the special floats).
const TOML_KEYWORDS: [&str; 4] = ["true", "false", "inf", "nan"];

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten runs back to their source text.
    fn rebuilt(runs: &[(String, Option<Tok>)]) -> String {
        runs.iter().map(|(t, _)| t.clone()).collect()
    }

    /// The first run whose exact text is `text`, if any.
    fn run_tok<'a>(runs: &'a [(String, Option<Tok>)], text: &str) -> Option<&'a Option<Tok>> {
        runs.iter().find(|(t, _)| t == text).map(|(_, tok)| tok)
    }

    #[test]
    fn tokenizer_is_lossless_across_languages() {
        for (code, lang) in [
            ("let x = \"a\\\"b\"; // c", Lang::TsJs),
            ("fn main() { 0xFF_u8; 'z' }", Lang::Rust),
            ("def f(): return 1.5 # x", Lang::Python),
            ("<p>Don't have an account?</p>", Lang::TsJs),
            ("", Lang::Rust),
            ("# Heading with `code` and [a](b)", Lang::Markdown),
            ("  - item with `span`", Lang::Markdown),
            ("port = 8080 # local", Lang::Toml),
            ("  [server.tls]  # section", Lang::Toml),
        ] {
            let rebuilt: String = tokenize(code, lang).into_iter().map(|(t, _)| t).collect();
            assert_eq!(rebuilt, code, "tokenizer dropped/added chars for {code:?}");
        }
    }

    #[test]
    fn a_contraction_apostrophe_does_not_swallow_the_line_as_a_string() {
        // An unpaired apostrophe in JSX prose must not open a string run —
        // the old behavior painted everything after "Don" in string sand.
        let runs = tokenize("<p>Don't have an account?</p>", Lang::TsJs);
        assert!(
            runs.iter().all(|(_, tok)| *tok != Some(Tok::Str)),
            "no string run in prose: {runs:?}"
        );
        // A real single-quoted string on the same language still highlights.
        let runs = tokenize("const x = 'ok';", Lang::TsJs);
        assert!(
            runs.iter()
                .any(|(t, tok)| t == "'ok'" && *tok == Some(Tok::Str)),
            "terminated strings keep their color: {runs:?}"
        );
    }

    #[test]
    fn markdown_and_toml_map_from_extensions_and_fence_tags() {
        assert_eq!(
            lang_from_path("skills/review/SKILL.md"),
            Some(Lang::Markdown)
        );
        assert_eq!(lang_from_path("docs/guide.markdown"), Some(Lang::Markdown));
        assert_eq!(lang_from_path(".stella/mcp.toml"), Some(Lang::Toml));
        assert_eq!(lang_from_fence("toml"), Some(Lang::Toml));
        assert_eq!(lang_from_fence("rust ignore"), Some(Lang::Rust));
        assert_eq!(lang_from_fence(""), None);
        assert_eq!(lang_from_fence("mermaid"), None);
    }

    #[test]
    fn toml_keys_values_and_comments_get_their_colors() {
        let runs = tokenize("port = 8080 # local", Lang::Toml);
        assert_eq!(
            run_tok(&runs, "port"),
            Some(&Some(Tok::Keyword)),
            "{runs:?}"
        );
        assert_eq!(run_tok(&runs, "8080"), Some(&Some(Tok::Number)), "{runs:?}");
        assert_eq!(
            run_tok(&runs, "# local"),
            Some(&Some(Tok::Comment)),
            "{runs:?}"
        );
        let runs = tokenize("name = \"stella\"", Lang::Toml);
        assert_eq!(
            run_tok(&runs, "\"stella\""),
            Some(&Some(Tok::Str)),
            "{runs:?}"
        );
        let runs = tokenize("enabled = true", Lang::Toml);
        assert_eq!(
            run_tok(&runs, "true"),
            Some(&Some(Tok::Keyword)),
            "{runs:?}"
        );
    }

    #[test]
    fn toml_table_headers_color_as_structure_but_array_lines_do_not() {
        let runs = tokenize("[server]", Lang::Toml);
        assert_eq!(
            run_tok(&runs, "[server]"),
            Some(&Some(Tok::Keyword)),
            "{runs:?}"
        );
        let runs = tokenize("[[bin]]", Lang::Toml);
        assert_eq!(
            run_tok(&runs, "[[bin]]"),
            Some(&Some(Tok::Keyword)),
            "{runs:?}"
        );
        // An array continuation line is a value, not a header.
        let runs = tokenize("  [1, 2],", Lang::Toml);
        assert!(
            runs.iter().all(|(_, tok)| *tok != Some(Tok::Keyword)),
            "no header run in an array line: {runs:?}"
        );
        assert_eq!(run_tok(&runs, "1"), Some(&Some(Tok::Number)), "{runs:?}");
    }

    #[test]
    fn markdown_structure_colors_and_prose_stays_calm() {
        let runs = tokenize("## Usage", Lang::Markdown);
        assert_eq!(
            run_tok(&runs, "## Usage"),
            Some(&Some(Tok::Keyword)),
            "{runs:?}"
        );
        let runs = tokenize("- item with `span` inside", Lang::Markdown);
        assert_eq!(run_tok(&runs, "- "), Some(&Some(Tok::Number)), "{runs:?}");
        assert_eq!(run_tok(&runs, "`span`"), Some(&Some(Tok::Str)), "{runs:?}");
        let runs = tokenize("> a quote", Lang::Markdown);
        assert_eq!(run_tok(&runs, "> "), Some(&Some(Tok::Comment)), "{runs:?}");
        let runs = tokenize("see [docs](https://example.com).", Lang::Markdown);
        assert_eq!(
            run_tok(&runs, "(https://example.com)"),
            Some(&Some(Tok::Number)),
            "{runs:?}"
        );
        // Plain prose — including keywords of other languages — stays plain.
        let runs = tokenize("fn let def return in prose", Lang::Markdown);
        assert!(
            runs.iter().all(|(_, tok)| tok.is_none()),
            "prose never lights up like code: {runs:?}"
        );
        // An unterminated backtick is prose, not a runaway code span.
        let runs = tokenize("a stray ` backtick", Lang::Markdown);
        assert!(
            runs.iter().all(|(_, tok)| tok.is_none()),
            "unterminated backtick stays plain: {runs:?}"
        );
    }

    #[test]
    fn highlighter_tracks_frontmatter_fences_and_their_interiors() {
        let doc = [
            "---",
            "name: reviewer",
            "---",
            "# Reviewer",
            "prose with fn and let staying plain",
            "```toml",
            "port = 8080",
            "```",
            "```sh",
            "echo hi",
            "```",
        ];
        let mut hl = Highlighter::new(Some(Lang::Markdown));
        let all: Vec<Runs> = doc.iter().map(|l| hl.runs(l)).collect();
        assert_eq!(run_tok(&all[0], "---"), Some(&Some(Tok::Comment)), "open");
        assert_eq!(
            run_tok(&all[1], "name"),
            Some(&Some(Tok::Keyword)),
            "frontmatter key: {:?}",
            all[1]
        );
        assert_eq!(run_tok(&all[2], "---"), Some(&Some(Tok::Comment)), "close");
        assert_eq!(
            run_tok(&all[3], "# Reviewer"),
            Some(&Some(Tok::Keyword)),
            "heading"
        );
        assert!(all[4].iter().all(|(_, tok)| tok.is_none()), "{:?}", all[4]);
        assert_eq!(run_tok(&all[5], "```toml"), Some(&Some(Tok::Comment)));
        assert_eq!(
            run_tok(&all[6], "port"),
            Some(&Some(Tok::Keyword)),
            "fence interior highlights in its own language: {:?}",
            all[6]
        );
        assert_eq!(run_tok(&all[7], "```"), Some(&Some(Tok::Comment)), "close");
        assert!(
            all[9].iter().all(|(_, tok)| tok.is_none()),
            "unknown fence language renders plain: {:?}",
            all[9]
        );
    }

    #[test]
    fn highlighter_without_frontmatter_treats_the_first_line_as_body() {
        let mut hl = Highlighter::new(Some(Lang::Markdown));
        let runs = hl.runs("# Straight to a heading");
        assert_eq!(
            run_tok(&runs, "# Straight to a heading"),
            Some(&Some(Tok::Keyword)),
            "{runs:?}"
        );
        // A later `---` is a rule, not a frontmatter open.
        let runs = hl.runs("---");
        assert_eq!(run_tok(&runs, "---"), Some(&Some(Tok::Comment)));
        let runs = hl.runs("still body prose");
        assert!(runs.iter().all(|(_, tok)| tok.is_none()), "{runs:?}");
    }

    #[test]
    fn highlighter_is_lossless_over_a_whole_document() {
        let doc = "---\nname: x\ntools: [\"Read\", \"Grep\"]\n---\n# T\n\n> q\n\n```rust\nfn main() {}\n```\nplain [a](b) `c` end";
        let mut hl = Highlighter::new(Some(Lang::Markdown));
        for line in doc.split('\n') {
            assert_eq!(rebuilt(&hl.runs(line)), line, "line mangled: {line:?}");
        }
    }

    #[test]
    fn highlighter_with_no_language_passes_lines_through_in_base_style() {
        let mut hl = Highlighter::new(None);
        let base = Style::default().fg(theme::INK);
        let spans = hl.spans("anything at all", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "anything at all");
        assert_eq!(spans[0].style, base);
    }
}
