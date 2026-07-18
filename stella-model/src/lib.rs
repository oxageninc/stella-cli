//! `stella-model` — the `Provider` trait plus its concrete adapters: Z.ai
//! (GLM 5.2, OpenAI-compatible chat), Anthropic (Messages API), OpenAI
//! (Responses API), Gemini direct (native generateContent), Vertex AI
//! (generateContent, enterprise auth), and Amazon Bedrock (Converse,
//! SigV4-signed).
pub mod anthropic;
pub mod bedrock;
pub mod catalog;
pub mod credential;
pub mod gemini;
pub(crate) mod http;
pub mod openai;
pub mod provider;
pub mod sse;
pub mod vertex;
pub mod zai;

pub use catalog::{Catalog, CatalogEntry, ToolDialect};
pub use credential::{ApiKey, CredentialError};
pub use provider::Provider;
