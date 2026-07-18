//! Pasted-path detection: turning a paste that *names a media file* into an
//! [`Attachment`] instead of literal text.
//!
//! Dragging a file onto a terminal pastes its path (often shell-escaped or
//! quoted). When that path points at an existing image / audio / video / PDF
//! file, the composer should attach the file itself — the model can ingest
//! the payload, while the raw path string tells it nothing. Text-like files
//! and unknown formats deliberately stay literal text: source files are
//! better read by the agent's own tools, and a false positive here would
//! swallow typed input.

use std::path::PathBuf;

use stella_protocol::{Attachment, AttachmentKind, classify_media_type, media_type_for_path};

/// Kinds worth auto-attaching from a pasted path. Text-like files are the
/// tools' job; unknown binaries would only degrade to a note.
pub fn attachable_kind(kind: AttachmentKind) -> bool {
    matches!(
        kind,
        AttachmentKind::Image | AttachmentKind::Audio | AttachmentKind::Video | AttachmentKind::Pdf
    )
}

/// If `pasted` is a single line naming an existing media/document file,
/// return it as an [`Attachment`]. `None` means "treat the paste as text".
pub fn probe_path_attachment(pasted: &str) -> Option<Attachment> {
    let line = pasted.trim();
    if line.is_empty() || line.contains('\n') {
        return None;
    }
    let candidate = normalize_dropped_path(line);
    let media_type = media_type_for_path(&candidate)?;
    if !attachable_kind(classify_media_type(media_type)) {
        return None;
    }
    let path = expand_home(&candidate);
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| candidate.clone());
    Some(Attachment::from_path(
        name,
        media_type,
        meta.len(),
        path.to_string_lossy(),
    ))
}

/// Undo the quoting/escaping terminals apply when a file is dragged in:
/// matching single/double quotes are stripped, and backslash-escapes
/// (`My\ File.png`) are unescaped.
fn normalize_dropped_path(line: &str) -> String {
    let unquoted = if (line.starts_with('\'') && line.ends_with('\'') && line.len() >= 2)
        || (line.starts_with('"') && line.ends_with('"') && line.len() >= 2)
    {
        &line[1..line.len() - 1]
    } else {
        line
    };
    let mut out = String::with_capacity(unquoted.len());
    let mut chars = unquoted.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(escaped) => out.push(escaped),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Expand a leading `~/` against `$HOME` (dropped paths are usually absolute,
/// but a hand-typed `~/shot.png` should work too).
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn temp_media(ext: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new()
            .suffix(&format!(".{ext}"))
            .tempfile()
            .expect("temp file");
        file.write_all(b"payload").expect("write");
        file
    }

    #[test]
    fn plain_image_path_attaches() {
        let file = temp_media("png");
        let att = probe_path_attachment(&file.path().to_string_lossy()).expect("attachment");
        assert_eq!(att.media_type, "image/png");
        assert_eq!(att.byte_len, 7);
    }

    #[test]
    fn quoted_and_escaped_paths_normalize() {
        let file = temp_media("pdf");
        let path = file.path().to_string_lossy().into_owned();
        assert!(probe_path_attachment(&format!("'{path}'")).is_some());
        assert!(probe_path_attachment(&format!("\"{path}\"")).is_some());
    }

    #[test]
    fn escaped_spaces_unescape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my shot.png");
        std::fs::write(&path, b"x").unwrap();
        let escaped = path.to_string_lossy().replace(' ', "\\ ");
        let att = probe_path_attachment(&escaped).expect("attachment");
        assert_eq!(att.name, "my shot.png");
    }

    #[test]
    fn text_and_unknown_files_stay_text() {
        let md = temp_media("md");
        assert!(
            probe_path_attachment(&md.path().to_string_lossy()).is_none(),
            "text-like files are the tools' job"
        );
        let bin = temp_media("bin");
        assert!(probe_path_attachment(&bin.path().to_string_lossy()).is_none());
    }

    #[test]
    fn nonexistent_or_multiline_pastes_stay_text() {
        assert!(probe_path_attachment("/definitely/not/here.png").is_none());
        assert!(probe_path_attachment("line one\n/tmp/x.png").is_none());
        assert!(probe_path_attachment("   ").is_none());
    }
}
