//! The artifact store: the single writer to `.stella/artifacts/`
//! ("the agent cannot overwrite arbitrary paths via
//! a generation tool"). Every generated file lands as `<id>.<ext>` *inside*
//! the caller-supplied root, and a manifest row records its provenance
//!
//!
//! Path-traversal safety is structural: ids are generated here (never
//! caller-supplied), and the filename is sanitized to `[a-z0-9_.-]` with any
//! path separator or `..` stripped, so no id or label can escape the root.
//! The sanitizer is exercised directly by a hostile-input test even though
//! the generated-id path already makes escape impossible.
//!
//! The manifest is `manifest.json` (a JSON array). Writes are crash-atomic:
//! the store reads the current array, appends, serializes to a sibling temp
//! file, then `rename`s it over the manifest — so a crash mid-write never
//! leaves a partially written or corrupt manifest ("append-safe" in the
//! crash-consistency sense for the single-process CLI).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stella_protocol::{MediaArtifactRef, MediaKind};

use crate::error::MediaError;
use crate::provider::MediaArtifact;

const MANIFEST_NAME: &str = "manifest.json";

/// One recorded artifact in `manifest.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub id: String,
    pub kind: MediaKind,
    /// Path relative to the artifact root, e.g. `med_ab12cd34ef56.png`.
    pub path: String,
    /// Lowercase hex SHA-256 of the file bytes.
    pub sha256: String,
    pub label: String,
    /// Unix seconds at write time.
    pub created_at: u64,
    pub byte_size: u64,
}

/// A filesystem-jailed writer + manifest for one workspace's
/// `.stella/artifacts/` directory.
pub struct ArtifactStore {
    root: PathBuf,
    /// Monotonic component of generated ids, so two saves of byte-identical
    /// content in the same process still get distinct filenames.
    seq: AtomicU64,
}

impl ArtifactStore {
    /// Open (creating if absent) an artifact store rooted at `root`. `root`
    /// is trusted — it is the session's pinned `.stella/artifacts/` path, not
    /// anything derived from model output.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, MediaError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| {
            MediaError::Artifact(format!(
                "cannot create artifact root {}: {e}",
                root.display()
            ))
        })?;
        Ok(Self {
            root,
            seq: AtomicU64::new(0),
        })
    }

    /// The artifact root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Persist a [`MediaArtifact`], using its declared extension and label.
    /// The id is generated; returns the citation-friendly
    /// [`MediaArtifactRef`].
    pub fn save_artifact(&self, art: &MediaArtifact) -> Result<MediaArtifactRef, MediaError> {
        self.save_with_ext(&art.bytes, art.kind, &art.extension, &art.label)
    }

    /// Persist raw bytes, deriving the extension from the kind
    /// (`Image`→`png`, `Svg`→`svg`, `Video`→`mp4`).
    pub fn save(
        &self,
        bytes: &[u8],
        kind: MediaKind,
        label: &str,
    ) -> Result<MediaArtifactRef, MediaError> {
        self.save_with_ext(bytes, kind, default_extension(kind), label)
    }

    /// Persist raw bytes with an explicit extension (for adapters that emit
    /// e.g. `jpeg`). Id is generated here.
    pub fn save_with_ext(
        &self,
        bytes: &[u8],
        kind: MediaKind,
        extension: &str,
        label: &str,
    ) -> Result<MediaArtifactRef, MediaError> {
        let id = self.generate_id(bytes);
        self.save_with_id(&id, bytes, kind, extension, label)
    }

    /// Persist under a specific id — used for video, whose artifact id was
    /// assigned at submit so events and the final file share one identity.
    /// The id is sanitized here, so even a hostile id cannot escape the root.
    pub fn save_with_id(
        &self,
        id: &str,
        bytes: &[u8],
        kind: MediaKind,
        extension: &str,
        label: &str,
    ) -> Result<MediaArtifactRef, MediaError> {
        let safe_id = sanitize_component(id);
        let safe_ext = sanitize_component(extension);
        let filename = format!("{safe_id}.{safe_ext}");
        let dest = self.root.join(&filename);

        // Structural jail check: the resolved parent must be the root.
        if dest.parent() != Some(self.root.as_path()) {
            return Err(MediaError::Artifact(format!(
                "refusing to write outside the artifact root: {}",
                dest.display()
            )));
        }

        std::fs::write(&dest, bytes)
            .map_err(|e| MediaError::Artifact(format!("cannot write {}: {e}", dest.display())))?;

        let entry = ManifestEntry {
            id: safe_id.clone(),
            kind,
            path: filename.clone(),
            sha256: sha256_hex(bytes),
            label: label.to_string(),
            created_at: now_unix_secs(),
            byte_size: bytes.len() as u64,
        };
        self.append_manifest(&entry)?;

        Ok(MediaArtifactRef {
            id: safe_id,
            kind,
            path: filename,
            label: label.to_string(),
        })
    }

    /// Read the manifest back (for a `gen list`-style listing). A missing
    /// manifest is an empty list, not an error.
    pub fn entries(&self) -> Result<Vec<ManifestEntry>, MediaError> {
        let path = self.root.join(MANIFEST_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => Ok(Vec::new()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                MediaError::Artifact(format!("manifest {} is corrupt: {e}", path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(MediaError::Artifact(format!(
                "cannot read manifest {}: {e}",
                path.display()
            ))),
        }
    }

    /// Generate a collision-resistant, filesystem-safe id: `med_` + 16 hex
    /// chars of `SHA-256(bytes ‖ seq ‖ nanos)`. Content need not be unique;
    /// the seq+time salt keeps identical bytes from sharing a filename.
    fn generate_id(&self, bytes: &[u8]) -> String {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.update(seq.to_le_bytes());
        hasher.update(nanos.to_le_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(16);
        for byte in &digest[..8] {
            hex.push_str(&format!("{byte:02x}"));
        }
        format!("med_{hex}")
    }

    /// Append a row via read → append → temp-write → atomic rename, so the
    /// manifest is never observed half-written.
    fn append_manifest(&self, entry: &ManifestEntry) -> Result<(), MediaError> {
        let mut entries = self.entries()?;
        entries.push(entry.clone());
        let body = serde_json::to_string_pretty(&entries)
            .map_err(|e| MediaError::Artifact(format!("cannot serialize manifest: {e}")))?;

        let final_path = self.root.join(MANIFEST_NAME);
        let tmp_path = self.root.join(format!(".{MANIFEST_NAME}.tmp"));
        std::fs::write(&tmp_path, body).map_err(|e| {
            MediaError::Artifact(format!(
                "cannot write temp manifest {}: {e}",
                tmp_path.display()
            ))
        })?;
        std::fs::rename(&tmp_path, &final_path).map_err(|e| {
            MediaError::Artifact(format!(
                "cannot commit manifest {}: {e}",
                final_path.display()
            ))
        })?;
        Ok(())
    }
}

/// Default file extension per media kind.
fn default_extension(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "png",
        MediaKind::Svg => "svg",
        MediaKind::Video => "mp4",
    }
}

/// Reduce a path component to a safe filename fragment: keep only
/// `[a-z0-9_.-]` (lowercased), drop everything else — in particular path
/// separators and `.` runs that could form `..`. Guarantees the result
/// contains no separator and is not a traversal token.
fn sanitize_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let lowered = ch.to_ascii_lowercase();
        if lowered.is_ascii_alphanumeric() || matches!(lowered, '_' | '-') {
            out.push(lowered);
        }
        // '.', '/', '\\', and everything else are dropped: a dropped '.'
        // means `..` and `.` can never survive into a filename fragment.
    }
    if out.is_empty() { "x".to_string() } else { out }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, ArtifactStore) {
        let dir = TempDir::new().unwrap();
        let store = ArtifactStore::open(dir.path().join("artifacts")).unwrap();
        (dir, store)
    }

    #[test]
    fn save_writes_inside_root_and_records_a_manifest_row() {
        let (_dir, store) = store();
        let art_ref = store
            .save(b"\x89PNG fake bytes", MediaKind::Image, "a-logo")
            .unwrap();

        assert!(art_ref.path.ends_with(".png"));
        assert!(store.root().join(&art_ref.path).exists());

        let entries = store.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, art_ref.id);
        assert_eq!(entries[0].label, "a-logo");
        assert_eq!(entries[0].byte_size, b"\x89PNG fake bytes".len() as u64);
        assert_eq!(entries[0].sha256.len(), 64);
    }

    #[test]
    fn identical_bytes_saved_twice_get_distinct_files() {
        let (_dir, store) = store();
        let a = store.save(b"same", MediaKind::Image, "x").unwrap();
        let b = store.save(b"same", MediaKind::Image, "x").unwrap();
        assert_ne!(a.id, b.id, "seq+time salt must break the tie");
        assert!(store.root().join(&a.path).exists());
        assert!(store.root().join(&b.path).exists());
        assert_eq!(store.entries().unwrap().len(), 2);
    }

    #[test]
    fn save_artifact_uses_declared_extension() {
        let (_dir, store) = store();
        let art = MediaArtifact {
            kind: MediaKind::Video,
            bytes: b"mp4 bytes".to_vec(),
            extension: "mp4".into(),
            label: "teaser".into(),
            model: "cogvideox".into(),
            cost_usd: 2.0,
        };
        let art_ref = store.save_artifact(&art).unwrap();
        assert!(art_ref.path.ends_with(".mp4"));
    }

    #[test]
    fn hostile_id_cannot_escape_the_root() {
        let (_dir, store) = store();
        // A path-traversal id must be neutralized to a flat filename inside
        // the root — the file lands in the root, and nothing is written to
        // the parent.
        let art_ref = store
            .save_with_id("../../etc/passwd", b"x", MediaKind::Image, "png", "evil")
            .unwrap();
        assert!(!art_ref.path.contains('/'));
        assert!(!art_ref.path.contains(".."));
        let written = store.root().join(&art_ref.path);
        assert!(written.exists());
        assert_eq!(written.parent(), Some(store.root()));
        // The traversal target must not exist.
        assert!(!store.root().join("../../etc/passwd").exists());
    }

    #[test]
    fn hostile_extension_is_sanitized() {
        let (_dir, store) = store();
        let art_ref = store
            .save_with_id("med_1", b"x", MediaKind::Image, "../sh", "x")
            .unwrap();
        assert!(!art_ref.path.contains('/'));
        assert!(!art_ref.path.contains(".."));
    }

    #[test]
    fn sanitize_component_strips_separators_and_dots() {
        assert_eq!(sanitize_component("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_component("med_ab12-CD"), "med_ab12-cd");
        assert_eq!(sanitize_component("...."), "x");
        assert_eq!(sanitize_component("a/b\\c"), "abc");
    }

    #[test]
    fn missing_manifest_reads_as_empty() {
        let (_dir, store) = store();
        assert!(store.entries().unwrap().is_empty());
    }

    #[test]
    fn manifest_survives_many_appends_in_order() {
        let (_dir, store) = store();
        for i in 0..5 {
            store
                .save(format!("bytes-{i}").as_bytes(), MediaKind::Image, "x")
                .unwrap();
        }
        let entries = store.entries().unwrap();
        assert_eq!(entries.len(), 5);
        // No stray temp file left behind after the atomic renames.
        assert!(!store.root().join(".manifest.json.tmp").exists());
    }

    #[test]
    fn corrupt_manifest_is_a_named_error() {
        let (_dir, store) = store();
        std::fs::write(store.root().join(MANIFEST_NAME), "{ not an array").unwrap();
        let err = store.entries().unwrap_err();
        assert!(matches!(err, MediaError::Artifact(_)));
    }
}
