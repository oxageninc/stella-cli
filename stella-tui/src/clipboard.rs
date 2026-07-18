//! Clipboard capture for `⌃V`: pull an image (or, failing that, text) off
//! the system clipboard.
//!
//! Terminals cannot deliver a *bitmap* through bracketed paste — `⌘V` of a
//! screenshot arrives as nothing at all. `⌃V` is the explicit affordance:
//! the shell asks the OS clipboard directly (via `arboard`), encodes any
//! image as PNG into the workspace's attachment store
//! (`.stella/attachments/`), and hands back an [`Attachment`] the composer
//! shows as a chip. When the clipboard holds text instead, the caller falls
//! back to a normal paste so `⌃V` is a universal paste key.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use stella_protocol::Attachment;

/// Where pasted payloads are stored, relative to the working directory the
/// session runs in.
pub fn default_attachments_dir() -> PathBuf {
    PathBuf::from(".stella").join("attachments")
}

/// What `⌃V` found on the clipboard.
#[derive(Debug, PartialEq, Eq)]
pub enum ClipboardPaste {
    /// An image, stored to disk and ready to attach.
    Image(Attachment),
    /// Plain text — the caller pastes it like any bracketed paste.
    Text(String),
    /// Nothing usable.
    Empty,
}

/// Read the system clipboard: an image wins over text. The image payload is
/// PNG-encoded and written under `dir` so the attachment (and the session
/// transcript replaying it every turn) has a stable file to hydrate from.
pub fn capture(dir: &Path) -> Result<ClipboardPaste, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    match clipboard.get_image() {
        Ok(image) => store_image(dir, &image).map(ClipboardPaste::Image),
        Err(arboard::Error::ContentNotAvailable) => match clipboard.get_text() {
            Ok(text) if !text.is_empty() => Ok(ClipboardPaste::Text(text)),
            _ => Ok(ClipboardPaste::Empty),
        },
        Err(e) => Err(format!("clipboard image read failed: {e}")),
    }
}

/// PNG-encode an RGBA clipboard bitmap and write it to the attachment store.
fn store_image(dir: &Path, image: &arboard::ImageData<'_>) -> Result<Attachment, String> {
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(
            &mut encoded,
            u32::try_from(image.width).map_err(|_| "image too wide".to_string())?,
            u32::try_from(image.height).map_err(|_| "image too tall".to_string())?,
        );
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("png encode failed: {e}"))?;
        writer
            .write_image_data(&image.bytes)
            .map_err(|e| format!("png encode failed: {e}"))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("pasted-{millis}.png");
    let path = dir.join(&name);
    std::fs::write(&path, &encoded).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(Attachment::from_path(
        name,
        "image/png",
        encoded.len() as u64,
        path.to_string_lossy(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `capture` needs a real OS clipboard; only the pure encode/store half is
    // unit-tested.
    #[test]
    fn store_image_writes_a_png_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let image = arboard::ImageData {
            width: 2,
            height: 1,
            bytes: vec![255, 0, 0, 255, 0, 255, 0, 255].into(),
        };
        let att = store_image(dir.path(), &image).expect("stored");
        assert_eq!(att.media_type, "image/png");
        assert!(att.name.starts_with("pasted-") && att.name.ends_with(".png"));
        let stella_protocol::AttachmentSource::Path { path } = &att.source else {
            panic!("expected path source");
        };
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        assert_eq!(att.byte_len, bytes.len() as u64);
    }
}
