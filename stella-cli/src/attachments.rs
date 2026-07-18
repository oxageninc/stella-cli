//! Prompt-text → multimodal-attachment extraction.
//!
//! The one chokepoint that makes attachments work on *every* input surface:
//! any token in a user prompt that names an existing image / audio / video /
//! PDF file on disk is attached to the message as a payload the model can
//! actually ingest. That covers the deck (whose prompt queue is text-shaped:
//! a `⌃V` clipboard image arrives here as its stored payload path), the
//! plain stdin REPL, and headless one-shot runs — with zero changes to how
//! prompts are queued, edited, or persisted.
//!
//! Text-like files (source, markdown, JSON…) are deliberately NOT attached:
//! reading those is what the agent's own tools are for, and silently
//! inlining every path-shaped token would bloat context. Unknown binaries
//! are skipped for the same reason. There is no size cap — Stella imposes no
//! limit of its own on attachment payloads; only providers do.

use std::collections::HashSet;
use std::path::PathBuf;

use stella_protocol::{
    Attachment, AttachmentKind, CompletionMessage, classify_media_type, media_type_for_path,
};

/// Attachments per prompt — a runaway-token backstop, not a size limit.
const MAX_ATTACHMENTS: usize = 32;

/// Build the user [`CompletionMessage`] for a prompt, attaching any media
/// files the text names.
pub fn user_message(text: impl Into<String>) -> CompletionMessage {
    let text = text.into();
    let attachments = extract_path_attachments(&text);
    CompletionMessage::user_with_attachments(text, attachments)
}

/// Scan prompt text for tokens naming existing media/document files. Each
/// hit becomes a path-sourced [`Attachment`] (payload hydrated at
/// request-build time), deduplicated by canonical path.
pub fn extract_path_attachments(text: &str) -> Vec<Attachment> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out = Vec::new();
    for raw in text.split_whitespace() {
        if out.len() >= MAX_ATTACHMENTS {
            break;
        }
        let token = raw
            .trim_start_matches(['(', '<', '[', '{', '"', '\'', '`'])
            .trim_end_matches([
                ')', '>', ']', '}', '"', '\'', '`', ',', '.', ';', ':', '!', '?',
            ]);
        if token.is_empty() {
            continue;
        }
        let Some(media_type) = media_type_for_path(token) else {
            continue;
        };
        if !attachable(classify_media_type(media_type)) {
            continue;
        }
        let path = expand_home(token);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen.insert(canonical) {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| token.to_string());
        out.push(Attachment::from_path(
            name,
            media_type,
            meta.len(),
            path.to_string_lossy(),
        ));
    }
    out
}

/// Kinds worth attaching from a path mention — the model can ingest these
/// payloads (or a provider degrade note names them); text-like files stay
/// the tools' job.
fn attachable(kind: AttachmentKind) -> bool {
    matches!(
        kind,
        AttachmentKind::Image | AttachmentKind::Audio | AttachmentKind::Video | AttachmentKind::Pdf
    )
}

/// Expand a leading `~/` against `$HOME`.
fn expand_home(token: &str) -> PathBuf {
    if let Some(rest) = token.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_paths_in_a_prompt_become_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("shot.png");
        let pdf = dir.path().join("spec.pdf");
        std::fs::write(&img, b"i").unwrap();
        std::fs::write(&pdf, b"p").unwrap();
        let prompt = format!(
            "compare {} with the spec ({}) please",
            img.display(),
            pdf.display()
        );
        let msg = user_message(&prompt);
        assert_eq!(msg.attachments.len(), 2, "{:?}", msg.attachments);
        assert_eq!(msg.attachments[0].media_type, "image/png");
        assert_eq!(msg.attachments[1].media_type, "application/pdf");
        assert_eq!(msg.content, prompt, "prompt text is untouched");
    }

    #[test]
    fn duplicates_missing_files_and_text_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.png");
        let md = dir.path().join("notes.md");
        std::fs::write(&img, b"i").unwrap();
        std::fs::write(&md, b"m").unwrap();
        let prompt = format!(
            "{img} {img} {missing} {md}",
            img = img.display(),
            missing = dir.path().join("gone.mp4").display(),
            md = md.display()
        );
        let atts = extract_path_attachments(&prompt);
        assert_eq!(atts.len(), 1, "{atts:?}");
        assert_eq!(atts[0].name, "a.png");
    }

    #[test]
    fn punctuation_around_paths_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("b.jpg");
        std::fs::write(&img, b"i").unwrap();
        let prompt = format!("look at \"{}\", then report", img.display());
        assert_eq!(extract_path_attachments(&prompt).len(), 1);
    }

    #[test]
    fn plain_prose_attaches_nothing() {
        assert!(
            extract_path_attachments("explain how video/mp4 handling works in http.rs").is_empty()
        );
    }
}
