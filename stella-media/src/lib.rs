//! # stella-media — multimodal generation (Phase 5)
//!
//! Image, SVG, and video generation for the Oxagen CLI, all client-side and
//! all BYOK, through one [`MediaProvider`] port and the same artifact
//! discipline as the rest of the engine (`08-multimodal.md`,
//! `02-architecture.md` §3, §8).
//!
//! ## What's here
//! * [`provider`] — the [`MediaProvider`] trait plus its request/response
//!   types and per-job cost estimation ([`MediaCapabilities`]).
//! * [`adapters`] — vendor adapters: Z.ai CogView (image), Z.ai CogVideoX
//!   (async video), OpenAI gpt-image (image). Recorded-fixture tested;
//!   runtime-skipped live smokes (L-V4).
//! * [`artifact`] — [`ArtifactStore`]: the single writer to
//!   `.stella/artifacts/`, path-traversal-proof, with a crash-atomic manifest
//!   (`02-architecture.md` §8).
//! * [`svg`] — the [`SvgPipeline`]: validate → sanitize → optimize with a
//!   bounded model-repair loop, treating LLM SVG as untrusted code (L-V2).
//! * [`preview`] — the terminal preview ladder (kitty / iTerm2 / plain), pure
//!   string builders, no TTY writes (`08-multimodal.md` §5).
//! * [`cost_gate`] — the video confirmation gate (`08-multimodal.md` §6):
//!   deny-by-default headless, threshold-configurable.
//! * [`jobs`] — persisted video-job state + live reconciliation so a
//!   dollar-cost job survives a dropped terminal and is never reported from
//!   cache (L-V3).
//! * [`emit`] — helpers turning job transitions into `stella_protocol` event
//!   *values* (`MediaProgress` / `MediaComplete`), no channel dependency.
//!
//! ## Deliberate architecture deviation (recorded follow-up)
//! `02-architecture.md` §2 nominally places vendor media HTTP clients in
//! `stella-model` (alongside the chat/embedding adapters). They live **here**
//! instead, so this Phase-5 workstream stays self-contained and this crate
//! does **not** depend on `stella-model`. The consequences of that isolation:
//! the redacted [`credential::ApiKey`] and the [`error::MediaError`] category
//! set are minimal self-contained copies of `stella-model`'s `ApiKey` /
//! `ProviderError` shapes rather than imports. Folding these adapters into
//! `stella-model`'s provider set (sharing one secret type and one HTTP layer)
//! is the recorded migration follow-up.
//!
//! ## Non-goals in v1 (`08-multimodal.md` §8)
//! Audio (TTS/STT/music) and 3D fit the [`MediaProvider`] shape but are not
//! scoped; the trait reserves that method-space by contract without shipping a
//! stub. Image *understanding* (screenshots as input) is the chat `vision`
//! role, not this crate.

pub mod adapters;
pub mod artifact;
pub mod cost_gate;
pub mod credential;
pub mod emit;
pub mod error;
mod http;
pub mod jobs;
pub mod preview;
pub mod provider;
pub mod svg;

pub use artifact::{ArtifactStore, ManifestEntry};
pub use cost_gate::{
    CostDecision, CostGate, DEFAULT_VIDEO_COST_THRESHOLD_USD, HeadlessCostGate, evaluate_video_cost,
};
pub use credential::{ApiKey, CredentialError};
pub use error::MediaError;
pub use jobs::{JobStore, resume};
pub use preview::{
    PreviewRung, detect as detect_preview, detect_from_env, render as render_preview,
};
pub use provider::{
    CostEstimate, ImageRequest, ImageSize, MediaArtifact, MediaCapabilities, MediaJob,
    MediaJobStatus, MediaProvider, VideoRequest,
};
pub use svg::{DEFAULT_SVG_ATTEMPTS, ProcessedSvg, SvgError, SvgGenerator, SvgPipeline};

// Re-export the protocol media types the public API surfaces, so callers get
// them from one place without a second `stella_protocol` import.
pub use stella_protocol::{MediaArtifactRef, MediaJobState, MediaKind};
