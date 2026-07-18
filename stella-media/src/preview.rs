//! Terminal preview ladder. A protocol ladder,
//! feature-detected once per session: kitty graphics protocol → iTerm2 inline
//! images → a plain file-path line. Never assume a capability; never emit raw
//! escape sequences to a terminal that did not advertise them (L-T*: degrade
//! politely, keep raw bytes out of the scrollback).
//!
//! Everything here is a **pure string builder** — this crate never writes to
//! a TTY. `detect` chooses a rung from environment variables; the encoders
//! turn image bytes into the escape-sequence string a caller prints. That
//! keeps the ladder fully testable without a terminal (sixel and unicode-mosaic
//! rungs from the spec are deferred; the plain-path fallback covers every
//! terminal in between until they land).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

/// A chosen rung of the preview ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewRung {
    /// kitty graphics protocol (APC `_G…`).
    Kitty,
    /// iTerm2 inline images (OSC 1337 `File=`).
    Iterm2,
    /// No inline-image capability detected — print a file-path line only.
    Plain,
}

/// Feature-detect the best rung from an environment lookup. Injecting the
/// lookup (rather than reading `std::env` directly) keeps detection pure and
/// testable. Precedence: kitty (most capable) → iTerm2 → plain.
///
/// Signals:
/// * kitty: `KITTY_WINDOW_ID` set, or `TERM` contains `kitty`.
/// * iTerm2: `TERM_PROGRAM` is `iTerm.app`, or `WezTerm` (which implements the
///   iTerm2 inline-image protocol).
pub fn detect(get: impl Fn(&str) -> Option<String>) -> PreviewRung {
    let term = get("TERM").unwrap_or_default().to_ascii_lowercase();
    if get("KITTY_WINDOW_ID").is_some() || term.contains("kitty") {
        return PreviewRung::Kitty;
    }
    let term_program = get("TERM_PROGRAM").unwrap_or_default();
    if term_program == "iTerm.app" || term_program == "WezTerm" {
        return PreviewRung::Iterm2;
    }
    PreviewRung::Plain
}

/// Detect from the real process environment — the one-line convenience over
/// [`detect`] for callers that aren't testing.
pub fn detect_from_env() -> PreviewRung {
    detect(|key| std::env::var(key).ok())
}

/// Maximum base64 payload per kitty transmission chunk. The protocol requires
/// chunking large images; 4096 is the conventional safe chunk size.
const KITTY_CHUNK: usize = 4096;

/// Encode image bytes as a kitty graphics-protocol escape string
/// (`\x1b_G…\x1b\\`). `f=100` declares a PNG payload; `a=T` transmits and
/// displays. The base64 payload is chunked with `m=1` on every chunk but the
/// last (`m=0`), per the protocol. Returns the full multi-chunk string ready
/// to print.
pub fn kitty_image(png_bytes: &[u8]) -> String {
    let encoded = BASE64.encode(png_bytes);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(KITTY_CHUNK).collect();
    let mut out = String::new();
    if chunks.is_empty() {
        // Empty payload: still emit a well-formed, do-nothing transmission.
        out.push_str("\x1b_Gf=100,a=T,m=0;\x1b\\");
        return out;
    }
    let last = chunks.len() - 1;
    for (index, chunk) in chunks.iter().enumerate() {
        let more = if index == last { 0 } else { 1 };
        if index == 0 {
            out.push_str(&format!("\x1b_Gf=100,a=T,m={more};"));
        } else {
            out.push_str(&format!("\x1b_Gm={more};"));
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push_str("\x1b\\");
    }
    out
}

/// Encode image bytes as an iTerm2 inline-image escape string
/// (OSC `1337;File=…:<base64>\x07`). `inline=1` renders in place; `size` is
/// the byte length; `name` (base64-encoded) is an optional display name.
pub fn iterm2_image(bytes: &[u8], name: Option<&str>) -> String {
    let payload = BASE64.encode(bytes);
    let mut args = format!("inline=1;size={}", bytes.len());
    if let Some(name) = name {
        args.push_str(";name=");
        args.push_str(&BASE64.encode(name.as_bytes()));
    }
    format!("\x1b]1337;File={args}:{payload}\x07")
}

/// The plain-path fallback rung: a single human line pointing at the on-disk
/// artifact. Previews are always additive to the artifact, never a substitute
///, so this is the safe floor for `--no-preview`, CI,
/// and unknown terminals.
pub fn plain_line(path: &str) -> String {
    format!("[image] saved to {path}")
}

/// Render an image to the chosen rung, falling back to the plain path line
/// when the rung has no inline-image capability. `path` is always available
/// so the plain rung (and `--no-preview`) can cite the on-disk artifact.
pub fn render(rung: PreviewRung, bytes: &[u8], path: &str, name: Option<&str>) -> String {
    match rung {
        PreviewRung::Kitty => kitty_image(bytes),
        PreviewRung::Iterm2 => iterm2_image(bytes, name),
        PreviewRung::Plain => plain_line(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detect_prefers_kitty_when_window_id_present() {
        let rung = detect(env_from(&[
            ("KITTY_WINDOW_ID", "1"),
            ("TERM_PROGRAM", "iTerm.app"),
        ]));
        assert_eq!(rung, PreviewRung::Kitty);
    }

    #[test]
    fn detect_kitty_from_term_string() {
        assert_eq!(
            detect(env_from(&[("TERM", "xterm-kitty")])),
            PreviewRung::Kitty
        );
    }

    #[test]
    fn detect_iterm2_from_term_program() {
        assert_eq!(
            detect(env_from(&[("TERM_PROGRAM", "iTerm.app")])),
            PreviewRung::Iterm2
        );
        assert_eq!(
            detect(env_from(&[("TERM_PROGRAM", "WezTerm")])),
            PreviewRung::Iterm2
        );
    }

    #[test]
    fn detect_falls_back_to_plain_for_unknown_terminal() {
        assert_eq!(
            detect(env_from(&[
                ("TERM", "xterm-256color"),
                ("TERM_PROGRAM", "Apple_Terminal")
            ])),
            PreviewRung::Plain
        );
        assert_eq!(detect(env_from(&[])), PreviewRung::Plain);
    }

    #[test]
    fn kitty_encoding_is_wrapped_and_chunked() {
        // Bytes large enough to force multiple base64 chunks.
        let bytes = vec![0xABu8; KITTY_CHUNK * 2];
        let out = kitty_image(&bytes);
        assert!(
            out.starts_with("\x1b_Gf=100,a=T,m=1;"),
            "first chunk header"
        );
        assert!(out.ends_with("\x1b\\"), "APC terminator");
        assert!(out.contains("\x1b_Gm=0;"), "final chunk marks m=0");
        // continuation chunks carry only m=, not the f=/a= header again
        assert_eq!(out.matches("f=100").count(), 1);
    }

    #[test]
    fn kitty_single_chunk_marks_m0_immediately() {
        let out = kitty_image(b"tiny");
        assert!(out.starts_with("\x1b_Gf=100,a=T,m=0;"), "{out:?}");
        assert!(out.ends_with("\x1b\\"));
    }

    #[test]
    fn kitty_empty_payload_is_well_formed() {
        let out = kitty_image(&[]);
        assert_eq!(out, "\x1b_Gf=100,a=T,m=0;\x1b\\");
    }

    #[test]
    fn iterm2_encoding_carries_size_and_optional_name() {
        let out = iterm2_image(b"hello", Some("logo.png"));
        assert!(out.starts_with("\x1b]1337;File=inline=1;size=5"), "{out:?}");
        assert!(out.contains(";name="));
        assert!(out.ends_with('\x07'));
        // name is base64-encoded, not raw
        assert!(!out.contains("logo.png"));

        let no_name = iterm2_image(b"hello", None);
        assert!(!no_name.contains(";name="));
    }

    #[test]
    fn plain_line_cites_the_path() {
        assert_eq!(
            plain_line(".stella/artifacts/med_x.png"),
            "[image] saved to .stella/artifacts/med_x.png"
        );
    }

    #[test]
    fn render_dispatches_by_rung() {
        assert!(render(PreviewRung::Kitty, b"x", "p", None).contains("\x1b_G"));
        assert!(render(PreviewRung::Iterm2, b"x", "p", None).contains("1337;File"));
        assert_eq!(
            render(PreviewRung::Plain, b"x", "p", None),
            "[image] saved to p"
        );
    }
}
