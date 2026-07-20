//! The models.dev master list — the public, unauthenticated catalog of
//! provider/model slugs and their pricing that `stella models refresh`
//! syncs into the on-disk model-card catalog.
//!
//! Why models.dev: it is a single JSON document
//! (<https://models.dev/api.json>) covering every major API provider —
//! including all the gateways stella can select (Anthropic, OpenAI, Google,
//! Vertex, Bedrock, OpenRouter, Z.ai, xAI, DeepSeek) — with per-model list
//! pricing (`cost.input`/`output`/`cache_read`/`cache_write` in USD per
//! million tokens), context/output limits, and release/update dates. It
//! needs no API key, and it serves a strong `ETag`, which is what makes the
//! refresh *incremental*: a re-fetch with `If-None-Match` answers `304 Not
//! Modified` and transfers nothing when the master list hasn't changed.
//!
//! This module only fetches and parses; deciding what to store (and the
//! provider-id mapping onto stella's own provider table) belongs to
//! `stella-cli`, which owns both vocabularies.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::http;

/// The master-list endpoint. Public and unauthenticated — but a THIRD
/// party, not the user's chosen provider, so it is never fetched
/// implicitly: stella's no-phone-home rule means the first fetch is always
/// an explicit `stella models refresh` (auto-refresh only re-arms after
/// that opt-in; see `stella-cli`'s catalog bootstrap and its
/// `STELLA_CATALOG_AUTO_REFRESH=0` kill switch). Provider-native model
/// discovery, which does auto-sync, talks to the user's own provider
/// instead — see `stella_model::provider_listing`.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// One provider block: `{ id, name, models: { <model-id>: {...} } }`.
/// Every field is defaulted — the document is third-party data and a
/// missing field must degrade to "unknown", never fail the whole sync.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ProviderEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
}

/// One model row under a provider. `cost` is USD per million tokens
/// (matching [`crate::catalog::Pricing`]'s unit); `limit.context` is the
/// context window in tokens.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ModelEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub cost: Option<ModelCost>,
    #[serde(default)]
    pub limit: Option<ModelLimit>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub last_updated: Option<String>,
    /// Whether the model supports reasoning / extended thinking. Absent in
    /// the document means unknown — degrade to "unknown", never assume.
    #[serde(default)]
    pub reasoning: Option<bool>,
    /// Whether the model accepts tool definitions.
    #[serde(default)]
    pub tool_call: Option<bool>,
}

/// List pricing in USD per million tokens. All optional: free/local models
/// carry no cost block at all, and some rows price only input/output.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct ModelCost {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

/// Token limits: `context` is the window, `output` the max completion.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct ModelLimit {
    #[serde(default)]
    pub context: Option<u64>,
    #[serde(default)]
    pub output: Option<u64>,
}

/// A successfully fetched + parsed master list.
#[derive(Debug, Clone)]
pub struct FetchedCatalog {
    /// The response `ETag`, to be persisted and replayed as `If-None-Match`
    /// on the next refresh (the incremental half of the sync).
    pub etag: Option<String>,
    /// SHA-256 of the raw response body — a change detector independent of
    /// the ETag (proxies sometimes strip validation headers).
    pub payload_hash: String,
    /// Provider blocks keyed by the models.dev provider id.
    pub providers: BTreeMap<String, ProviderEntry>,
}

impl FetchedCatalog {
    pub fn model_count(&self) -> usize {
        self.providers.values().map(|p| p.models.len()).sum()
    }
}

/// What a conditional fetch produced. `NotModified` is the cheap steady
/// state: the persisted ETag still matches, nothing was transferred, and
/// the on-disk catalog is already current.
#[derive(Debug)]
pub enum FetchOutcome {
    NotModified,
    Fetched(Box<FetchedCatalog>),
}

/// Parse the master-list document. Tolerant per provider: one malformed
/// provider blob is skipped (third-party schema drift must cost that
/// provider's rows, never the whole refresh), but a document with NO
/// parseable providers is an error — that shape means we're not looking at
/// the master list at all, and "0 providers" must not masquerade as a
/// successful sync that then quietly rejects every model slug.
pub fn parse_catalog(body: &str) -> Result<BTreeMap<String, ProviderEntry>, String> {
    let raw: BTreeMap<String, serde_json::Value> = serde_json::from_str(body)
        .map_err(|e| format!("models.dev returned unparseable JSON: {e}"))?;
    let mut providers = BTreeMap::new();
    for (id, value) in raw {
        let Ok(mut entry) = serde_json::from_value::<ProviderEntry>(value) else {
            continue;
        };
        if entry.id.is_empty() {
            entry.id = id.clone();
        }
        // Model ids: the map key is authoritative (some rows omit the
        // redundant inner `id`).
        for (model_id, model) in entry.models.iter_mut() {
            if model.id.is_empty() {
                model.id = model_id.clone();
            }
        }
        providers.insert(id, entry);
    }
    if providers.is_empty() {
        return Err(
            "models.dev document contained no parseable providers — refusing to sync an empty \
             master list"
                .to_string(),
        );
    }
    Ok(providers)
}

/// Fetch the master list from `url` (callers pass [`MODELS_DEV_URL`]; tests
/// pass a mock server). `etag` is the previously persisted validator —
/// when the document is unchanged the server answers `304` and this
/// returns [`FetchOutcome::NotModified`] without transferring the body.
pub async fn fetch_catalog(url: &str, etag: Option<&str>) -> Result<FetchOutcome, String> {
    let client = http::client();
    let mut request = client.get(url).header("Accept", "application/json");
    if let Some(etag) = etag.filter(|e| !e.is_empty()) {
        request = request.header("If-None-Match", etag);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("could not reach models.dev: {e}"))?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(FetchOutcome::NotModified);
    }
    if !response.status().is_success() {
        return Err(format!(
            "models.dev answered HTTP {} — try again later",
            response.status()
        ));
    }
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = response
        .text()
        .await
        .map_err(|e| format!("could not read the models.dev response: {e}"))?;
    let payload_hash = {
        let digest = Sha256::digest(body.as_bytes());
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let providers = parse_catalog(&body)?;
    Ok(FetchOutcome::Fetched(Box::new(FetchedCatalog {
        etag,
        payload_hash,
        providers,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "anthropic": {
            "id": "anthropic",
            "name": "Anthropic",
            "models": {
                "claude-sonnet-4-5": {
                    "id": "claude-sonnet-4-5",
                    "name": "Claude Sonnet 4.5",
                    "family": "claude-sonnet",
                    "release_date": "2025-09-29",
                    "last_updated": "2025-09-29",
                    "reasoning": true,
                    "tool_call": true,
                    "limit": { "context": 1000000, "output": 64000 },
                    "cost": { "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75 }
                },
                "claude-free-preview": {
                    "name": "No cost block, no inner id"
                }
            }
        },
        "openrouter": {
            "id": "openrouter",
            "name": "OpenRouter",
            "models": {
                "anthropic/claude-sonnet-4.5": {
                    "id": "anthropic/claude-sonnet-4.5",
                    "cost": { "input": 3, "output": 15 }
                }
            }
        },
        "malformed": "not an object"
    }"#;

    #[test]
    fn parse_reads_pricing_limits_and_dates() {
        let providers = parse_catalog(FIXTURE).expect("fixture parses");
        let anthropic = &providers["anthropic"];
        let sonnet = &anthropic.models["claude-sonnet-4-5"];
        let cost = sonnet.cost.expect("cost block present");
        assert_eq!(cost.input, Some(3.0));
        assert_eq!(cost.output, Some(15.0));
        assert_eq!(cost.cache_read, Some(0.3));
        assert_eq!(cost.cache_write, Some(3.75));
        assert_eq!(sonnet.limit.unwrap().context, Some(1_000_000));
        assert_eq!(sonnet.release_date.as_deref(), Some("2025-09-29"));
        assert_eq!(sonnet.reasoning, Some(true));
        assert_eq!(sonnet.tool_call, Some(true));
        assert_eq!(anthropic.name.as_deref(), Some("Anthropic"));
    }

    #[test]
    fn parse_defaults_missing_fields_and_backfills_ids_from_map_keys() {
        let providers = parse_catalog(FIXTURE).expect("fixture parses");
        let preview = &providers["anthropic"].models["claude-free-preview"];
        // Inner `id` was omitted — the map key fills it.
        assert_eq!(preview.id, "claude-free-preview");
        assert!(preview.cost.is_none());
        assert!(preview.limit.is_none());
        assert_eq!(preview.reasoning, None, "absent capability stays unknown");
        assert_eq!(preview.tool_call, None);
    }

    #[test]
    fn parse_skips_malformed_provider_blobs_without_failing_the_sync() {
        let providers = parse_catalog(FIXTURE).expect("fixture parses");
        assert!(!providers.contains_key("malformed"));
        assert_eq!(providers.len(), 2);
    }

    #[test]
    fn parse_rejects_a_document_with_no_parseable_providers() {
        assert!(parse_catalog("{}").is_err());
        assert!(parse_catalog(r#"{"a": 1, "b": "x"}"#).is_err());
        assert!(parse_catalog("[1,2,3]").is_err());
    }

    #[tokio::test]
    async fn fetch_replays_etag_and_honors_304() {
        use wiremock::matchers::{header, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // First fetch: no validator → 200 with an ETag.
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"v1\"")
                    .set_body_string(FIXTURE),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let outcome = fetch_catalog(&server.uri(), None).await.expect("fetches");
        let fetched = match outcome {
            FetchOutcome::Fetched(fetched) => fetched,
            FetchOutcome::NotModified => panic!("first fetch can never be NotModified"),
        };
        assert_eq!(fetched.etag.as_deref(), Some("\"v1\""));
        assert_eq!(fetched.model_count(), 3);
        assert_eq!(fetched.payload_hash.len(), 64);

        // Second fetch replays the validator → 304, nothing transferred.
        server.reset().await;
        Mock::given(method("GET"))
            .and(header("If-None-Match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        let outcome = fetch_catalog(&server.uri(), fetched.etag.as_deref())
            .await
            .expect("304 is a success");
        assert!(matches!(outcome, FetchOutcome::NotModified));
    }

    #[tokio::test]
    async fn fetch_surfaces_http_errors_as_named_errors() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = fetch_catalog(&server.uri(), None).await.unwrap_err();
        assert!(err.contains("500"), "error names the status: {err}");
    }
}
