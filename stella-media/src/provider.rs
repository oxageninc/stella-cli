//! The `MediaProvider` port and its request/response types
//! (`02-architecture.md` §3). One trait, many vendor adapters behind it —
//! the same ports-not-concretions discipline the chat `Provider` uses.
//!
//! `generate_image` is sync-ish (one HTTP round trip, bytes back);
//! `generate_video` submits an async job and returns a [`MediaJob`] handle
//! that `poll_video` reconciles against the provider (`08-multimodal.md`
//! §3–§6). Neither method touches the filesystem: they return in-memory
//! [`MediaArtifact`] bytes, and the caller persists through
//! [`crate::artifact::ArtifactStore`] — so the artifact-root jail
//! (`02-architecture.md` §8) is enforced in exactly one place.
//!
//! Audio/3D are explicitly future (`08-multimodal.md` §8): the trait reserves
//! that method-space by contract but ships no v1 stub, to avoid churning the
//! port surface before there's a real adapter.

use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use stella_protocol::MediaKind;

use crate::error::MediaError;

/// A pixel dimension for image generation, e.g. `1024x1024`. Parses from and
/// renders to the `WxH` wire form providers use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

impl ImageSize {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// A square size, the common default.
    pub fn square(side: u32) -> Self {
        Self::new(side, side)
    }
}

impl fmt::Display for ImageSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

impl FromStr for ImageSize {
    type Err = MediaError;

    /// Parse `WxH` (e.g. `1024x1024`). A malformed size is a caller error,
    /// surfaced as [`MediaError::Terminal`] so it fails loudly rather than
    /// silently defaulting.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (w, h) = s.split_once(['x', 'X']).ok_or_else(|| {
            MediaError::Terminal(format!("invalid image size `{s}` (expected WxH)"))
        })?;
        let width = w
            .trim()
            .parse()
            .map_err(|_| MediaError::Terminal(format!("invalid image width in `{s}`")))?;
        let height = h
            .trim()
            .parse()
            .map_err(|_| MediaError::Terminal(format!("invalid image height in `{s}`")))?;
        Ok(Self { width, height })
    }
}

/// A per-job cost estimate drawn from the provider's rate card
/// (`08-multimodal.md` §6). `detail` is the human explanation the cost gate
/// shows before charging money.
#[derive(Clone, Debug, PartialEq)]
pub struct CostEstimate {
    pub kind: MediaKind,
    pub model: String,
    pub estimated_usd: f64,
    pub detail: String,
}

/// What a provider can do and what it charges (`08-multimodal.md` §2). Per
/// the spec these are catalog data refreshed from the vendor, never truly
/// hard-coded; until the catalog layer lands, each adapter fills this with
/// documented default rates the CLI/catalog can override.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaCapabilities {
    pub provider_id: String,
    pub image: bool,
    pub video: bool,
    /// Image edit / variation from an input image (not v1 for every vendor).
    pub image_edit: bool,
    /// Sizes the image endpoint accepts, if constrained.
    pub sizes: Vec<ImageSize>,
    /// Rate-card price per generated image, if this provider does images.
    pub image_usd_each: Option<f64>,
    /// Rate-card price per second of generated video, if this provider does
    /// video.
    pub video_usd_per_second: Option<f64>,
}

impl MediaCapabilities {
    /// Estimate the cost of an image request (`n` candidates). `None` when
    /// this provider has no image rate card (i.e. does not do images).
    pub fn estimate_image(&self, n: u32, size: ImageSize) -> Option<CostEstimate> {
        let each = self.image_usd_each?;
        let n = n.max(1);
        Some(CostEstimate {
            kind: MediaKind::Image,
            model: self.provider_id.clone(),
            estimated_usd: each * f64::from(n),
            detail: format!("{n} image(s) @ {size} × ${each:.4}"),
        })
    }

    /// Estimate the cost of a video request of `duration_secs`. `None` when
    /// this provider has no video rate card.
    pub fn estimate_video(&self, duration_secs: u32) -> Option<CostEstimate> {
        let per_sec = self.video_usd_per_second?;
        let secs = duration_secs.max(1);
        Some(CostEstimate {
            kind: MediaKind::Video,
            model: self.provider_id.clone(),
            estimated_usd: per_sec * f64::from(secs),
            detail: format!("{secs}s video @ ${per_sec:.4}/s"),
        })
    }
}

/// An image generation request (`08-multimodal.md` §3).
#[derive(Clone, Debug)]
pub struct ImageRequest {
    pub prompt: String,
    pub size: ImageSize,
    /// Number of candidate images (`--n`); at least 1.
    pub n: u32,
    /// A short slug used to label the resulting artifact; the caller derives
    /// it from the prompt.
    pub label: String,
}

impl ImageRequest {
    pub fn new(prompt: impl Into<String>, size: ImageSize) -> Self {
        let prompt = prompt.into();
        let label = default_label(&prompt);
        Self {
            prompt,
            size,
            n: 1,
            label,
        }
    }

    pub fn with_n(mut self, n: u32) -> Self {
        self.n = n.max(1);
        self
    }
}

/// A video generation request (`08-multimodal.md` §3, cost-gated §6).
#[derive(Clone, Debug)]
pub struct VideoRequest {
    pub prompt: String,
    pub duration_secs: u32,
    pub label: String,
}

impl VideoRequest {
    pub fn new(prompt: impl Into<String>, duration_secs: u32) -> Self {
        let prompt = prompt.into();
        let label = default_label(&prompt);
        Self {
            prompt,
            duration_secs: duration_secs.max(1),
            label,
        }
    }
}

/// A short, filesystem-friendly label derived from a prompt: lowercased,
/// non-alphanumerics collapsed to `-`, truncated. Not an id (ids are
/// generated by the artifact store); just a human hint.
fn default_label(prompt: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in prompt.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "artifact".to_string()
    } else {
        trimmed
    }
}

/// Freshly generated media, in memory, not yet on disk. The caller writes it
/// through [`crate::artifact::ArtifactStore::save_artifact`], which is the
/// only code allowed to touch `.stella/artifacts/` (`02-architecture.md` §8).
/// `Debug` shows the byte length, never the bytes.
#[derive(Clone)]
pub struct MediaArtifact {
    pub kind: MediaKind,
    pub bytes: Vec<u8>,
    /// File extension without the dot, e.g. `png`, `mp4`, `svg`.
    pub extension: String,
    pub label: String,
    pub model: String,
    pub cost_usd: f64,
}

impl fmt::Debug for MediaArtifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaArtifact")
            .field("kind", &self.kind)
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .field("extension", &self.extension)
            .field("label", &self.label)
            .field("model", &self.model)
            .field("cost_usd", &self.cost_usd)
            .finish()
    }
}

/// A handle to a submitted async video job (`08-multimodal.md` §6). Persisted
/// to the job store so a dropped terminal never orphans a dollar-cost job;
/// `poll_video`/resume reconcile it live against the provider (L-V3).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MediaJob {
    /// Our artifact id — assigned at submit so the eventual output and the
    /// `MediaProgress` events share one identity across a process restart.
    pub artifact_id: String,
    /// Which adapter owns this job (`"zai"`, ...); routes a resume back to
    /// the right provider.
    pub provider_id: String,
    /// The id the provider assigned; the poll endpoint's key.
    pub provider_job_id: String,
    pub kind: MediaKind,
    pub model: String,
    pub estimated_cost_usd: f64,
    /// Unix seconds at submit.
    pub submitted_at: u64,
    pub label: String,
}

/// The live status of a video job as reported by `poll_video`. `state` is
/// the protocol enum (so it flows straight into a `MediaProgress` event);
/// `artifact` is `Some` only on `Succeeded`, carrying the downloaded bytes.
#[derive(Debug)]
pub struct MediaJobStatus {
    pub state: stella_protocol::MediaJobState,
    /// Progress in `0.0..=1.0` when the provider reports it.
    pub progress: Option<f32>,
    /// The finished artifact, present only when `state` is `Succeeded`.
    pub artifact: Option<MediaArtifact>,
}

impl MediaJobStatus {
    /// A terminal status is one no further poll can change (`Succeeded` or
    /// `Failed`); a resume loop stops here.
    pub fn is_terminal(&self) -> bool {
        use stella_protocol::MediaJobState;
        matches!(
            self.state,
            MediaJobState::Succeeded | MediaJobState::Failed { .. }
        )
    }
}

/// The media generation port (`02-architecture.md` §3). Every vendor adapter
/// implements it; the engine/CLI drives through `&dyn MediaProvider`.
#[async_trait]
pub trait MediaProvider: Send + Sync {
    /// Stable id for this adapter (`"zai"`, `"openai"`), used in job routing
    /// and error messages.
    fn id(&self) -> &str;

    /// What this provider can do and what it charges (`08-multimodal.md` §2).
    fn capabilities(&self) -> MediaCapabilities;

    /// Generate an image (`08-multimodal.md` §3). One HTTP round trip;
    /// returns bytes in memory for the caller to persist.
    async fn generate_image(&self, req: ImageRequest) -> Result<MediaArtifact, MediaError>;

    /// Submit an async video job (`08-multimodal.md` §6). Returns a handle to
    /// persist and poll; does not wait for completion.
    async fn generate_video(&self, req: VideoRequest) -> Result<MediaJob, MediaError>;

    /// Reconcile a job's state live against the provider (L-V3). A job the
    /// provider reports as gone (404) is returned as
    /// `MediaJobState::Failed`, never a cached "running".
    async fn poll_video(&self, job: &MediaJob) -> Result<MediaJobStatus, MediaError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_size_round_trips_through_wire_form() {
        let size = ImageSize::square(1024);
        assert_eq!(size.to_string(), "1024x1024");
        assert_eq!("1024x1024".parse::<ImageSize>().unwrap(), size);
        assert_eq!(
            "512X768".parse::<ImageSize>().unwrap(),
            ImageSize::new(512, 768)
        );
    }

    #[test]
    fn image_size_rejects_malformed_input_loudly() {
        assert!("1024".parse::<ImageSize>().is_err());
        assert!("axb".parse::<ImageSize>().is_err());
        assert!("x".parse::<ImageSize>().is_err());
    }

    #[test]
    fn estimate_image_scales_with_candidate_count() {
        let caps = MediaCapabilities {
            provider_id: "zai".into(),
            image: true,
            image_usd_each: Some(0.05),
            ..Default::default()
        };
        let est = caps.estimate_image(3, ImageSize::square(1024)).unwrap();
        assert!((est.estimated_usd - 0.15).abs() < 1e-9);
        assert_eq!(est.kind, MediaKind::Image);
    }

    #[test]
    fn estimate_video_scales_with_duration() {
        let caps = MediaCapabilities {
            provider_id: "zai".into(),
            video: true,
            video_usd_per_second: Some(0.2),
            ..Default::default()
        };
        let est = caps.estimate_video(10).unwrap();
        assert!((est.estimated_usd - 2.0).abs() < 1e-9);
        assert_eq!(est.kind, MediaKind::Video);
    }

    #[test]
    fn estimate_returns_none_when_capability_absent() {
        let image_only = MediaCapabilities {
            provider_id: "x".into(),
            image: true,
            image_usd_each: Some(0.05),
            ..Default::default()
        };
        assert!(image_only.estimate_video(5).is_none());
    }

    #[test]
    fn default_label_is_filesystem_friendly() {
        assert_eq!(
            default_label("A Wordmark: ember/orange!"),
            "a-wordmark-ember-orange"
        );
        assert_eq!(default_label("   "), "artifact");
        assert_eq!(default_label("!!!"), "artifact");
    }

    #[test]
    fn image_request_defaults_n_to_at_least_one() {
        let req = ImageRequest::new("logo", ImageSize::square(512)).with_n(0);
        assert_eq!(req.n, 1);
    }

    #[test]
    fn media_artifact_debug_hides_bytes() {
        let art = MediaArtifact {
            kind: MediaKind::Image,
            bytes: vec![1, 2, 3, 4, 5],
            extension: "png".into(),
            label: "x".into(),
            model: "cogview".into(),
            cost_usd: 0.05,
        };
        let debug = format!("{art:?}");
        assert!(debug.contains("5 bytes"), "{debug}");
        assert!(!debug.contains("[1, 2, 3"), "{debug}");
    }

    #[test]
    fn job_status_terminality() {
        use stella_protocol::MediaJobState;
        let running = MediaJobStatus {
            state: MediaJobState::Running,
            progress: Some(0.5),
            artifact: None,
        };
        assert!(!running.is_terminal());
        let gone = MediaJobStatus {
            state: MediaJobState::Failed {
                reason: "gone".into(),
            },
            progress: None,
            artifact: None,
        };
        assert!(gone.is_terminal());
    }
}
