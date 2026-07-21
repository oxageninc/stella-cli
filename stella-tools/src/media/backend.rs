use std::sync::Arc;

use stella_media::MediaProvider;

/// Media providers resolved from the host's BYOK environment.
pub struct MediaBackend {
    pub image: Arc<dyn MediaProvider>,
    pub video: Option<Arc<dyn MediaProvider>>,
}

/// Z.ai wins when multiple providers are configured and is the only v1
/// backend that exposes video generation.
pub fn detect_media_backend() -> Option<MediaBackend> {
    if let Ok(key) = stella_media::ApiKey::from_env("ZAI_API_KEY") {
        return Some(MediaBackend {
            image: Arc::new(stella_media::adapters::ZaiImageProvider::new(
                key.clone(),
                stella_media::adapters::zai_image::DEFAULT_MODEL,
            )),
            video: Some(Arc::new(stella_media::adapters::ZaiVideoProvider::new(
                key,
                stella_media::adapters::zai_video::DEFAULT_MODEL,
            ))),
        });
    }
    if let Ok(key) = stella_media::ApiKey::from_env("OPENAI_API_KEY") {
        return Some(MediaBackend {
            image: Arc::new(stella_media::adapters::OpenAiImageProvider::new(
                key,
                stella_media::adapters::openai_image::DEFAULT_MODEL,
            )),
            video: None,
        });
    }
    None
}
