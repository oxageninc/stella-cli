//! Attachment → wire-part resolution shared by every provider adapter.
//!
//! One function ([`wire_parts`]) turns a user message's
//! [`Attachment`]s into dialect-neutral [`WirePart`]s, so classification,
//! payload hydration, text inlining, and graceful degradation live in ONE
//! place and each adapter only maps parts onto its own JSON shapes.
//!
//! ## The degradation contract
//!
//! An attachment a dialect cannot ingest natively (audio on Anthropic, video
//! on OpenAI, an arbitrary binary anywhere) NEVER fails the request. It
//! becomes a [`WirePart::Text`] note describing exactly what was attached and
//! why it is not visible, so the model can tell the user instead of the turn
//! erroring out. The same applies to a payload file that cannot be read —
//! the conversation replays every turn, so a hard error here would brick the
//! session permanently.
//!
//! ## Size
//!
//! Stella imposes no size cap of its own. Payloads are hydrated from disk at
//! request-build time and handed to the provider as base64; a payload larger
//! than the provider's own request limit surfaces as that provider's error.
//! Text-like files are inlined in full for the same reason.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use stella_protocol::{Attachment, AttachmentKind, AttachmentSource};

/// What a dialect can ingest natively. Anything switched off degrades to a
/// descriptive [`WirePart::Text`] note.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DialectCaps {
    pub images: bool,
    pub pdfs: bool,
    pub audio: bool,
    pub video: bool,
}

/// One dialect-neutral content part resolved from an attachment. Binary
/// payloads arrive already base64-encoded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WirePart {
    Image {
        media_type: String,
        base64: String,
    },
    Pdf {
        name: String,
        base64: String,
    },
    Audio {
        media_type: String,
        base64: String,
    },
    Video {
        media_type: String,
        base64: String,
    },
    /// Inlined text — a text-like file's contents, or a degrade note for
    /// anything the dialect cannot ingest.
    Text {
        text: String,
    },
}

/// Resolve a message's attachments into wire parts for a dialect. Infallible
/// by design: unreadable payloads and unsupported kinds come back as
/// [`WirePart::Text`] notes rather than errors (see module docs).
pub(crate) fn wire_parts(attachments: &[Attachment], caps: DialectCaps) -> Vec<WirePart> {
    attachments
        .iter()
        .map(|attachment| resolve_one(attachment, caps))
        .collect()
}

fn resolve_one(attachment: &Attachment, caps: DialectCaps) -> WirePart {
    let bytes = match &attachment.source {
        AttachmentSource::Path { path } => match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                return WirePart::Text {
                    text: format!(
                        "[attachment {} was provided by the user but its payload could not \
                         be read: {err}]",
                        attachment.label()
                    ),
                };
            }
        },
        AttachmentSource::Data { base64 } => match BASE64.decode(base64) {
            Ok(bytes) => bytes,
            Err(err) => {
                return WirePart::Text {
                    text: format!(
                        "[attachment {} was provided by the user but its inline payload is \
                         not valid base64: {err}]",
                        attachment.label()
                    ),
                };
            }
        },
    };
    match attachment.kind() {
        AttachmentKind::Text => WirePart::Text {
            text: inline_text(attachment, &bytes),
        },
        AttachmentKind::Image if caps.images => WirePart::Image {
            media_type: attachment.media_type.clone(),
            base64: BASE64.encode(&bytes),
        },
        AttachmentKind::Pdf if caps.pdfs => WirePart::Pdf {
            name: attachment.name.clone(),
            base64: BASE64.encode(&bytes),
        },
        AttachmentKind::Audio if caps.audio => WirePart::Audio {
            media_type: attachment.media_type.clone(),
            base64: BASE64.encode(&bytes),
        },
        AttachmentKind::Video if caps.video => WirePart::Video {
            media_type: attachment.media_type.clone(),
            base64: BASE64.encode(&bytes),
        },
        kind => WirePart::Text {
            text: degrade_note(attachment, kind),
        },
    }
}

/// A text-like file inlined in full, framed so the model knows what it is
/// looking at and where it ends.
fn inline_text(attachment: &Attachment, bytes: &[u8]) -> String {
    let content = String::from_utf8_lossy(bytes);
    format!(
        "[attached file: {}]\n<attached-file name=\"{}\">\n{}\n</attached-file>",
        attachment.label(),
        attachment.name,
        content.trim_end_matches('\n'),
    )
}

/// The note emitted for a kind this dialect cannot ingest. Names the
/// attachment precisely so the model can tell the user what it cannot see.
fn degrade_note(attachment: &Attachment, kind: AttachmentKind) -> String {
    let noun = match kind {
        AttachmentKind::Image => "images",
        AttachmentKind::Audio => "audio",
        AttachmentKind::Video => "video",
        AttachmentKind::Pdf => "PDF documents",
        AttachmentKind::Binary => "this file format",
        AttachmentKind::Text => unreachable!("text always inlines"),
    };
    format!(
        "[the user attached {}, but the current provider cannot ingest {noun} natively; \
         acknowledge the attachment and suggest a provider or format that can be read]",
        attachment.label()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    const ALL: DialectCaps = DialectCaps {
        images: true,
        pdfs: true,
        audio: true,
        video: true,
    };
    const NONE: DialectCaps = DialectCaps {
        images: false,
        pdfs: false,
        audio: false,
        video: false,
    };

    fn file_attachment(
        name: &str,
        media_type: &str,
        payload: &[u8],
    ) -> (Attachment, tempfile::NamedTempFile) {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(payload).expect("write payload");
        let att = Attachment::from_path(
            name,
            media_type,
            payload.len() as u64,
            file.path().to_string_lossy(),
        );
        (att, file)
    }

    #[test]
    fn image_hydrates_from_disk_to_base64() {
        let (att, _guard) = file_attachment("shot.png", "image/png", b"\x89PNGdata");
        let parts = wire_parts(std::slice::from_ref(&att), ALL);
        assert_eq!(
            parts,
            vec![WirePart::Image {
                media_type: "image/png".into(),
                base64: BASE64.encode(b"\x89PNGdata"),
            }]
        );
    }

    #[test]
    fn inline_data_source_passes_through_decoded() {
        let att = Attachment {
            name: "clip.mp4".into(),
            media_type: "video/mp4".into(),
            byte_len: 4,
            source: AttachmentSource::Data {
                base64: BASE64.encode(b"vvvv"),
            },
        };
        let parts = wire_parts(&[att], ALL);
        assert_eq!(
            parts,
            vec![WirePart::Video {
                media_type: "video/mp4".into(),
                base64: BASE64.encode(b"vvvv"),
            }]
        );
    }

    #[test]
    fn texty_files_inline_their_content_regardless_of_caps() {
        let (att, _guard) = file_attachment("notes.md", "text/markdown", b"# Title\nbody\n");
        let parts = wire_parts(std::slice::from_ref(&att), NONE);
        let WirePart::Text { text } = &parts[0] else {
            panic!("expected text part, got {parts:?}");
        };
        assert!(text.contains("# Title\nbody"), "{text}");
        assert!(text.contains("notes.md"), "{text}");
    }

    #[test]
    fn unsupported_kinds_degrade_to_a_note_not_an_error() {
        let (att, _guard) = file_attachment("song.mp3", "audio/mpeg", b"ID3xxxx");
        let parts = wire_parts(std::slice::from_ref(&att), NONE);
        let WirePart::Text { text } = &parts[0] else {
            panic!("expected degrade note, got {parts:?}");
        };
        assert!(text.contains("song.mp3"), "{text}");
        assert!(text.contains("cannot ingest audio"), "{text}");
    }

    #[test]
    fn unknown_binary_always_degrades() {
        let (att, _guard) = file_attachment("bundle.zip", "application/zip", b"PK\x03\x04");
        let parts = wire_parts(std::slice::from_ref(&att), ALL);
        assert!(
            matches!(&parts[0], WirePart::Text { text } if text.contains("bundle.zip")),
            "{parts:?}"
        );
    }

    #[test]
    fn unreadable_payload_becomes_a_note_never_an_error() {
        let att = Attachment::from_path("gone.png", "image/png", 10, "/nonexistent/gone.png");
        let parts = wire_parts(&[att], ALL);
        let WirePart::Text { text } = &parts[0] else {
            panic!("expected note, got {parts:?}");
        };
        assert!(text.contains("could not be read"), "{text}");
        assert!(text.contains("gone.png"), "{text}");
    }

    #[test]
    fn pdf_maps_to_pdf_part_with_its_name() {
        let (att, _guard) = file_attachment("spec.pdf", "application/pdf", b"%PDF-1.7");
        let parts = wire_parts(std::slice::from_ref(&att), ALL);
        assert_eq!(
            parts,
            vec![WirePart::Pdf {
                name: "spec.pdf".into(),
                base64: BASE64.encode(b"%PDF-1.7"),
            }]
        );
    }
}
