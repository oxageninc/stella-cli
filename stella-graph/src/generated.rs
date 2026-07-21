//! Generated / minified file exclusion (issue #272).
//!
//! Evidence: recall inside a large monorepo surfaced five frames all citing
//! `dist-standalone/oxagen.mjs`, a checked-in minified bundle — zero frames
//! from real source. Indexing generated artifacts wastes recall budget and
//! buries genuine hits, so this module keeps them out of the store entirely.
//!
//! Two independent signals feed the same exclusion, both evaluated in
//! [`crate::store::index_one`] against a file's already-read bytes:
//!
//! - **Declared**: `.gitattributes` `linguist-generated=true` patterns
//!   (root-level only — the same documented gap [`crate::walk`] already
//!   carries for `.gitignore`: per-directory `.gitattributes` files are not
//!   merged) and the `*.min.*` filename convention (`app.min.js`).
//! - **Heuristic** ([`looks_minified`]): line-shape sniffing for generated
//!   output that carries no path or attribute signal at all — a single
//!   monster line, or a file uniformly wider than hand-written source ever
//!   gets, is not something a human wrote.
//!
//! Directory-shaped generated output (`dist/`, `build/`, `out/`, `.next/`,
//! `node_modules/`, `dist-standalone/`, `vendor/`) is excluded earlier, at
//! the walk itself ([`crate::walk::DENY_DIRS`]) — cheaper, since the walk
//! never even opens those files.
//!
//! Both checks here run **before** the byte-compat skip
//! (`code_graph_files.content_sha256`), so a file already sitting in an
//! index built before this module existed is retroactively dropped on the
//! very next pass even though its bytes have not changed — the byte-compat
//! skip alone would otherwise hide it forever.

use std::path::Path;

use crate::manifest::glob_match;

/// A single line at or beyond this length is treated as minified — hand
/// authored source essentially never reaches it, while bundlers/minifiers
/// routinely emit output far past it (a whole module on one line).
pub(crate) const MINIFIED_LINE_LEN: usize = 2_000;

/// A file whose *average* line length is at or beyond this is treated as
/// minified even with no single monster line — catches output wrapped at a
/// fixed column instead of emitted as one giant line.
pub(crate) const MINIFIED_AVG_LINE_LEN: usize = 500;

/// Below this many bytes, line-length heuristics are unreliable (a short
/// one-line JSON config, a single re-export) — content sniffing is skipped
/// entirely rather than false-positive on a tiny legitimate file.
pub(crate) const MIN_HEURISTIC_BYTES: usize = 2_048;

/// Filename infix marking a minified build artifact by convention
/// (`app.min.js`, `styles.min.css`).
const MINIFIED_INFIX: &str = ".min.";

/// One `.gitattributes` pattern with its resolved `linguist-generated` value
/// (`true` for `linguist-generated` / `linguist-generated=true`, `false` for
/// `-linguist-generated` / `linguist-generated=false`).
struct AttrPattern {
    pattern: String,
    generated: bool,
}

/// Parsed `.gitattributes` `linguist-generated` rules for one workspace root,
/// loaded once per index pass — mirrors
/// [`crate::manifest::StorageManifest::load`]'s "loaded once, best-effort"
/// shape. A missing or unreadable file yields an empty (no-op) filter rather
/// than an error: most workspaces have no `.gitattributes` at all.
pub(crate) struct GeneratedFilter {
    patterns: Vec<AttrPattern>,
}

impl GeneratedFilter {
    pub(crate) fn load(root: &Path) -> GeneratedFilter {
        let text = std::fs::read_to_string(root.join(".gitattributes")).unwrap_or_default();
        let patterns = text.lines().filter_map(parse_attr_line).collect();
        GeneratedFilter { patterns }
    }

    /// Whether `.gitattributes` marks `rel_path` `linguist-generated=true`.
    /// Real gitattributes semantics apply: later matching patterns win, so a
    /// trailing `-linguist-generated` can un-mark an earlier blanket pattern.
    /// A pattern with no `/` follows git's own convention of matching the
    /// basename at any depth (`*.snap` hits `src/deep/foo.snap`); a pattern
    /// containing `/` is anchored to the workspace root, matched against the
    /// full relative path (`dist/**`).
    fn attr_marks_generated(&self, rel_path: &str) -> bool {
        let basename = Path::new(rel_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(rel_path);
        let mut generated = false;
        for p in &self.patterns {
            let matched = if p.pattern.contains('/') {
                glob_match(&p.pattern, rel_path)
            } else {
                glob_match(&p.pattern, basename)
            };
            if matched {
                generated = p.generated;
            }
        }
        generated
    }
}

/// Parse one `.gitattributes` line into a pattern + resolved
/// `linguist-generated` boolean, if that line mentions the attribute at all
/// (every other attribute is irrelevant to this module and ignored).
fn parse_attr_line(line: &str) -> Option<AttrPattern> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split_whitespace();
    let pattern = fields.next()?.to_string();
    for attr in fields {
        let (name, generated) = match attr.strip_prefix('-') {
            Some(name) => (name, false),
            None => match attr.split_once('=') {
                Some((name, value)) => (name, value.eq_ignore_ascii_case("true")),
                None => (attr, true),
            },
        };
        if name == "linguist-generated" {
            return Some(AttrPattern { pattern, generated });
        }
    }
    None
}

/// Filename-convention check: `*.min.*` (`app.min.js`, `styles.min.css`).
fn is_minified_filename(rel_path: &str) -> bool {
    Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.contains(MINIFIED_INFIX))
}

/// Line-shape sniff for generated/minified content with no path or attribute
/// signal at all. Operates on raw bytes rather than `str` so it runs ahead of
/// (and regardless of) UTF-8 validation — a minified bundle's byte-line shape
/// reads the same either way, and a binary file just never crosses either
/// threshold below the byte-count floor is meant to protect.
pub(crate) fn looks_minified(content: &[u8]) -> bool {
    if content.len() < MIN_HEURISTIC_BYTES {
        return false;
    }
    let mut lines = 0usize;
    for line in content.split(|&b| b == b'\n') {
        lines += 1;
        if line.len() >= MINIFIED_LINE_LEN {
            return true;
        }
    }
    content.len() / lines.max(1) >= MINIFIED_AVG_LINE_LEN
}

/// Whether `rel_path` (with `content` already read off disk) should be
/// excluded from the index. Cheap path-pattern signals first, content shape
/// last — `looks_minified` is the only signal that has to look at bytes.
pub(crate) fn is_excluded(filter: &GeneratedFilter, rel_path: &str, content: &[u8]) -> bool {
    is_minified_filename(rel_path)
        || filter.attr_marks_generated(rel_path)
        || looks_minified(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_infix_filenames_are_excluded() {
        assert!(is_minified_filename("dist/app.min.js"));
        assert!(is_minified_filename("styles.min.css"));
        // "minimal"/"administration" contain "min" but never the ".min."
        // infix — a substring match on "min" alone would false-positive here.
        assert!(!is_minified_filename("minimal.js"));
        assert!(!is_minified_filename("administration.rs"));
    }

    #[test]
    fn gitattributes_generated_true_excludes_matching_paths() {
        let filter = GeneratedFilter {
            patterns: vec![AttrPattern {
                pattern: "generated/**".into(),
                generated: true,
            }],
        };
        assert!(filter.attr_marks_generated("generated/schema.ts"));
        assert!(!filter.attr_marks_generated("src/schema.ts"));
    }

    #[test]
    fn a_later_pattern_can_unset_an_earlier_blanket_rule() {
        let filter = GeneratedFilter {
            patterns: vec![
                AttrPattern {
                    pattern: "vendor/**".into(),
                    generated: true,
                },
                AttrPattern {
                    pattern: "vendor/hand-edited.ts".into(),
                    generated: false,
                },
            ],
        };
        assert!(filter.attr_marks_generated("vendor/bundle.js"));
        assert!(!filter.attr_marks_generated("vendor/hand-edited.ts"));
    }

    #[test]
    fn parses_bare_negated_and_valued_linguist_generated_forms() {
        assert!(
            parse_attr_line("*.g.cs linguist-generated")
                .unwrap()
                .generated
        );
        assert!(
            parse_attr_line("*.g.cs linguist-generated=true")
                .unwrap()
                .generated
        );
        assert!(
            !parse_attr_line("*.g.cs -linguist-generated")
                .unwrap()
                .generated
        );
        assert!(
            !parse_attr_line("*.g.cs linguist-generated=false")
                .unwrap()
                .generated
        );
        assert!(parse_attr_line("*.g.cs linguist-language=C#").is_none());
        assert!(parse_attr_line("# a comment").is_none());
        assert!(parse_attr_line("   ").is_none());
    }

    #[test]
    fn load_is_a_no_op_without_a_gitattributes_file() {
        let root = tempfile::tempdir().unwrap();
        let filter = GeneratedFilter::load(root.path());
        assert!(!filter.attr_marks_generated("anything.ts"));
    }

    #[test]
    fn load_parses_a_real_gitattributes_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".gitattributes"),
            "dist/** linguist-generated=true\n*.snap linguist-generated\n",
        )
        .unwrap();
        let filter = GeneratedFilter::load(root.path());
        assert!(filter.attr_marks_generated("dist/bundle.js"));
        assert!(filter.attr_marks_generated("src/foo.snap"));
        assert!(!filter.attr_marks_generated("src/foo.rs"));
    }

    #[test]
    fn a_single_huge_line_is_minified() {
        // The huge line alone must clear both the line-length threshold AND
        // the overall byte floor (a short preamble plus exactly
        // `MINIFIED_LINE_LEN` would total just under `MIN_HEURISTIC_BYTES`).
        let content = format!("const x = 1;\n{}\n", "a".repeat(MINIFIED_LINE_LEN + 200));
        assert!(content.len() >= MIN_HEURISTIC_BYTES);
        assert!(looks_minified(content.as_bytes()));
    }

    #[test]
    fn a_uniformly_wide_file_is_minified_by_average() {
        // 10 lines averaging well past MINIFIED_AVG_LINE_LEN, none alone
        // reaching MINIFIED_LINE_LEN.
        let line = "x".repeat(MINIFIED_AVG_LINE_LEN + 50);
        let content = std::iter::repeat_n(line, 10).collect::<Vec<_>>().join("\n");
        assert!(content.len() >= MIN_HEURISTIC_BYTES);
        assert!(looks_minified(content.as_bytes()));
    }

    #[test]
    fn ordinary_hand_written_source_is_not_minified() {
        let content = "fn run_turn() {\n    // drive one turn\n}\npub struct Engine;\n".repeat(50);
        assert!(content.len() >= MIN_HEURISTIC_BYTES);
        assert!(!looks_minified(content.as_bytes()));
    }

    #[test]
    fn tiny_files_never_trip_the_heuristic() {
        let content = "a".repeat(MINIFIED_LINE_LEN); // long line, but under the byte floor
        assert!(content.len() < MIN_HEURISTIC_BYTES);
        assert!(!looks_minified(content.as_bytes()));
    }
}
