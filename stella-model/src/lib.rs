//! `stella-model` — the `Provider` trait plus its concrete adapters: Z.ai
//! (GLM 5.2, OpenAI-compatible chat), Anthropic (Messages API), OpenAI
//! (Responses API), Gemini direct (native generateContent), Vertex AI
//! (generateContent, enterprise auth), and Amazon Bedrock (Converse,
//! SigV4-signed).
pub mod anthropic;
pub(crate) mod attachment;
pub mod bedrock;
pub mod cache_economics;
pub mod catalog;
pub mod credential;
pub mod gemini;
pub(crate) mod http;
pub mod modelsdev;
pub mod openai;
pub mod provider;
pub mod provider_listing;
pub mod provider_parity;
pub mod sse;
pub mod vertex;
pub mod zai;

pub use cache_economics::{
    CacheWarmth, cache_write_premium_multiplier, diagnose_cache, hit_rate as cache_hit_rate,
    is_cache_expired_rewrite, provider_cache_ttl_secs,
};
pub use catalog::{Catalog, CatalogEntry, Pricing, ToolDialect};
pub use credential::{ApiKey, CredentialError};
pub use provider::Provider;
