//! Multimodal *input* attachments — images, documents, audio, and video a
//! user hands to the model as context (pasted from the clipboard or named by
//! path). The chat-side counterpart of `stella-media`'s generation types:
//! that crate makes media, this type carries media *into* a completion.
//!
//! ## Payloads live on disk, not in the envelope
//!
//! An [`Attachment`] normally carries an [`AttachmentSource::Path`] pointing
//! at the stored payload; the base64 bytes are hydrated just-in-time by the
//! model layer when a request is built. This keeps the conversation
//! envelope (persisted transcripts, event logs, the store) small no matter
//! how large the underlying file is — Stella imposes no size cap of its own;
//! only the provider's own request limits apply.
//!
//! ## Kind is derived, not stored
//!
//! [`AttachmentKind`] is classified from the MIME type on demand so a
//! persisted attachment can never disagree with its own `media_type`.

use serde::{Deserialize, Serialize};

/// What a completion attachment is, classified from its MIME type. Decides
/// which provider wire shape (image block, document block, inline text, …)
/// the model layer emits — and which kinds a given dialect must degrade to a
/// descriptive text note instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    /// A raster image (`image/*`).
    Image,
    /// An audio clip (`audio/*`).
    Audio,
    /// A video clip (`video/*`).
    Video,
    /// A PDF document (`application/pdf`) — the one binary document format
    /// providers ingest natively.
    Pdf,
    /// A text-like document (`text/*`, JSON, XML, YAML, TOML, CSV, source
    /// code…). Inlined as plain text on every provider.
    Text,
    /// Anything else — no provider can ingest it; always degraded to a
    /// descriptive note.
    Binary,
}

/// Where an attachment's payload lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentSource {
    /// The payload is a file on disk, hydrated to bytes only when a provider
    /// request is built. The canonical form for anything persisted.
    Path { path: String },
    /// The payload is inline base64 — the post-hydration form the provider
    /// adapters consume. Never the at-rest form for anything large.
    Data { base64: String },
}

/// One multimodal input attached to a user message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Display name — the original filename, or a synthetic one for
    /// clipboard pastes (`pasted-image.png`).
    pub name: String,
    /// MIME type (`image/png`, `application/pdf`, `video/mp4`, …).
    pub media_type: String,
    /// Payload size in bytes (pre-base64), for display and telemetry.
    pub byte_len: u64,
    pub source: AttachmentSource,
}

impl Attachment {
    /// An attachment whose payload lives at `path`.
    pub fn from_path(
        name: impl Into<String>,
        media_type: impl Into<String>,
        byte_len: u64,
        path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            media_type: media_type.into(),
            byte_len,
            source: AttachmentSource::Path { path: path.into() },
        }
    }

    /// The attachment's kind, classified from its MIME type.
    pub fn kind(&self) -> AttachmentKind {
        classify_media_type(&self.media_type)
    }

    /// The human-readable label used by chips, transcripts, and degrade
    /// notes: `screenshot.png (image/png, 1.2 MB)`.
    pub fn label(&self) -> String {
        format!(
            "{} ({}, {})",
            self.name,
            self.media_type,
            human_bytes(self.byte_len)
        )
    }
}

/// Classify a MIME type into the attachment kind the wire mapping switches
/// on. Suffix-aware for structured-syntax types (`application/ld+json` is
/// text, not binary).
pub fn classify_media_type(media_type: &str) -> AttachmentKind {
    let mime = media_type
        .split(';')
        .next()
        .unwrap_or(media_type)
        .trim()
        .to_ascii_lowercase();
    if let Some(rest) = mime.strip_prefix("image/") {
        // SVG is XML source, not a raster payload — providers reject it as
        // an image block, but it is perfectly readable as text.
        return if rest == "svg+xml" {
            AttachmentKind::Text
        } else {
            AttachmentKind::Image
        };
    }
    if mime.starts_with("audio/") {
        return AttachmentKind::Audio;
    }
    if mime.starts_with("video/") {
        return AttachmentKind::Video;
    }
    if mime == "application/pdf" {
        return AttachmentKind::Pdf;
    }
    if mime.starts_with("text/") {
        return AttachmentKind::Text;
    }
    const TEXTY_APPLICATION: &[&str] = &[
        "application/json",
        "application/xml",
        "application/yaml",
        "application/x-yaml",
        "application/toml",
        "application/javascript",
        "application/typescript",
        "application/x-sh",
        "application/sql",
        "application/graphql",
        "application/x-httpd-php",
        "application/xhtml+xml",
    ];
    if TEXTY_APPLICATION.contains(&mime.as_str())
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
    {
        return AttachmentKind::Text;
    }
    AttachmentKind::Binary
}

/// MIME type for a file path, from its extension. `None` for extensions we
/// don't recognize — callers decide whether to fall back to
/// `application/octet-stream` (attach-anything flows) or to skip (paste
/// auto-detection, where a false positive would swallow typed text).
pub fn media_type_for_path(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        // Images
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "svg" => "image/svg+xml",
        // Documents
        "pdf" => "application/pdf",
        // Audio
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "aiff" | "aif" => "audio/aiff",
        // Video
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mpeg" | "mpg" => "video/mpeg",
        "m4v" => "video/mp4",
        "3gp" => "video/3gpp",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        // Text-like (a deliberate, non-exhaustive set: unknown text formats
        // still work via the octet-stream → degrade-note path)
        "txt" | "text" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "json" => "application/json",
        "jsonl" | "ndjson" => "application/json",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "js" | "mjs" | "cjs" => "application/javascript",
        "ts" | "tsx" | "jsx" => "application/typescript",
        "sh" | "bash" | "zsh" => "application/x-sh",
        "sql" => "application/sql",
        "graphql" | "gql" => "application/graphql",
        "py" | "rs" | "go" | "java" | "kt" | "swift" | "c" | "h" | "cpp" | "hpp" | "cc" | "rb"
        | "php" | "cs" | "scala" | "lua" | "r" | "pl" | "ex" | "exs" | "erl" | "hs" | "ml"
        | "clj" | "zig" | "nim" | "d" | "dart" | "vue" | "svelte" | "astro" | "tf" | "proto"
        | "cfg" | "conf" | "ini" | "env" | "properties" | "gradle" | "cmake" | "make" | "mk"
        | "dockerfile" | "diff" | "patch" | "tex" | "rst" | "adoc" | "org" => "text/plain",
        _ => return None,
    };
    Some(mime)
}

/// `1.2 MB`-style size formatting for chips and degrade notes.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_covers_every_kind() {
        assert_eq!(classify_media_type("image/png"), AttachmentKind::Image);
        assert_eq!(classify_media_type("image/svg+xml"), AttachmentKind::Text);
        assert_eq!(classify_media_type("audio/mpeg"), AttachmentKind::Audio);
        assert_eq!(classify_media_type("video/mp4"), AttachmentKind::Video);
        assert_eq!(classify_media_type("application/pdf"), AttachmentKind::Pdf);
        assert_eq!(classify_media_type("text/markdown"), AttachmentKind::Text);
        assert_eq!(
            classify_media_type("application/json"),
            AttachmentKind::Text
        );
        assert_eq!(
            classify_media_type("application/ld+json"),
            AttachmentKind::Text
        );
        assert_eq!(
            classify_media_type("application/zip"),
            AttachmentKind::Binary
        );
    }

    #[test]
    fn classify_strips_parameters_and_case() {
        assert_eq!(
            classify_media_type("Text/Plain; charset=utf-8"),
            AttachmentKind::Text
        );
        assert_eq!(
            classify_media_type("IMAGE/JPEG; q=0.9"),
            AttachmentKind::Image
        );
    }

    #[test]
    fn media_type_for_path_maps_common_extensions() {
        assert_eq!(media_type_for_path("shot.PNG"), Some("image/png"));
        assert_eq!(
            media_type_for_path("/tmp/demo.mov"),
            Some("video/quicktime")
        );
        assert_eq!(media_type_for_path("notes.md"), Some("text/markdown"));
        assert_eq!(media_type_for_path("spec.pdf"), Some("application/pdf"));
        assert_eq!(media_type_for_path("voice.m4a"), Some("audio/mp4"));
        assert_eq!(media_type_for_path("mystery.xyz"), None);
        assert_eq!(media_type_for_path("no_extension"), None);
    }

    #[test]
    fn attachment_roundtrips_through_json() {
        let att = Attachment::from_path("shot.png", "image/png", 2048, "/tmp/a.png");
        let json = serde_json::to_string(&att).unwrap();
        let back: Attachment = serde_json::from_str(&json).unwrap();
        assert_eq!(back, att);
        assert_eq!(back.kind(), AttachmentKind::Image);
    }

    #[test]
    fn label_is_human_readable() {
        let att = Attachment::from_path("shot.png", "image/png", 1_300_000, "/x");
        assert_eq!(att.label(), "shot.png (image/png, 1.2 MB)");
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
