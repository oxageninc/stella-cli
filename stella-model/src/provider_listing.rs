//! Provider-native model discovery — "what does this provider's own API
//! say it serves *right now*". The models.dev master list
//! ([`crate::modelsdev`]) is broad but third-party: it can lag a release
//! by days and its gateway coverage (OpenRouter especially) is a curated
//! subset. Each provider's own `/models` endpoint is authoritative and
//! instant, so `stella models refresh` (and the startup auto-sync) overlay
//! it on top of the master list for every provider whose credential is
//! configured.
//!
//! Four wire shapes cover every built-in provider:
//! - **OpenRouter** — `GET {base}/models`, public. The richest listing:
//!   per-token pricing strings, context/completion limits, and
//!   `supported_parameters` (which is where per-model reasoning/tool
//!   support comes from).
//! - **Anthropic** — `GET {base}/v1/models`, `x-api-key` + versioned,
//!   paginated via `after_id`/`has_more`. Ids and display names only.
//! - **Gemini** — `GET {base}/models`, `x-goog-api-key`, paginated via
//!   `pageToken`. Carries token limits, `thinking`, and the generation
//!   methods used to filter out non-chat rows (embeddings, imagen, aqa).
//! - **OpenAI-compatible** — `GET {base}/models`, bearer auth: OpenAI,
//!   xAI, DeepSeek, Z.ai, local servers, custom gateways. Ids only.
//!
//! This module only fetches and parses (same division of labor as
//! `modelsdev`); merging into the on-disk catalog belongs to `stella-cli`.
//! Every function is best-effort by contract: a provider whose listing
//! endpoint is down, missing (404 on a gateway that never implemented it),
//! or shape-drifted returns an `Err(String)` the caller reports and moves
//! past — discovery failure must never fail a refresh of OTHER providers,
//! and never a turn.

use crate::credential::ApiKey;
use crate::http;

/// One model as reported by its serving provider's own listing endpoint.
/// Everything except the id is optional: most providers report far less
/// than the master list knows, and a missing field must stay "unknown" so
/// the catalog merge can keep the better value it already has.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderModel {
    /// Provider-native slug, sent verbatim as a request's `model`.
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// USD per million tokens (the catalog's unit).
    pub input_usd_per_mtok: Option<f64>,
    pub output_usd_per_mtok: Option<f64>,
    pub cached_input_usd_per_mtok: Option<f64>,
    pub cache_write_usd_per_mtok: Option<f64>,
    /// Whether the model supports reasoning / extended thinking — `None`
    /// when the provider's listing doesn't say.
    pub supports_reasoning: Option<bool>,
    /// Whether the model accepts tool definitions.
    pub supports_tools: Option<bool>,
}

/// GET `url` and hand back the body, with the provider's own error text on
/// a non-success status. `headers` are (name, value) pairs — the auth
/// vocabulary differs per provider and none of it may end up in the URL
/// (query-string keys leak into logs and proxies).
async fn get_json(label: &str, url: &str, headers: &[(&str, &str)]) -> Result<String, String> {
    let client = http::client();
    let mut request = client.get(url).header("Accept", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("could not reach {label}: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("could not read the {label} response: {e}"))?;
    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("{label} answered HTTP {status}: {snippet}"));
    }
    Ok(body)
}

/// Hard cap on pagination rounds for the providers that page. Ten pages at
/// the page sizes requested (≥100/page everywhere) is far beyond any real
/// listing; the cap exists so a server echoing the same page token forever
/// cannot spin the refresh.
const MAX_PAGES: usize = 10;

// ---------------------------------------------------------------------
// OpenRouter
// ---------------------------------------------------------------------

/// A USD-per-TOKEN decimal string ("0.000003") → USD per million tokens.
/// OpenRouter prices are strings, sometimes "-1" for dynamically-priced
/// rows (the `openrouter/auto` meta-router) — negative means unknown.
fn per_token_str_to_per_mtok(raw: Option<&serde_json::Value>) -> Option<f64> {
    let value = raw?;
    let per_token = match value {
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    (per_token >= 0.0).then_some(per_token * 1_000_000.0)
}

/// Parse OpenRouter's `GET /models` document.
pub fn parse_openrouter(body: &str) -> Result<Vec<ProviderModel>, String> {
    let root: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("OpenRouter model list is unparseable JSON: {e}"))?;
    let Some(data) = root.get("data").and_then(|d| d.as_array()) else {
        return Err("OpenRouter model list has no `data` array".to_string());
    };
    let mut models = Vec::new();
    for row in data {
        let Some(id) = row
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let pricing = row.get("pricing");
        // `supported_parameters` is per-model ground truth: a model that
        // lists `reasoning` thinks, one that lists `tools` accepts tool
        // definitions — and one that lists neither genuinely supports
        // neither (the field enumerates everything the model accepts).
        let supported: Option<Vec<&str>> = row
            .get("supported_parameters")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|p| p.as_str()).collect());
        let (supports_reasoning, supports_tools) = match &supported {
            Some(params) => (
                Some(
                    params
                        .iter()
                        .any(|p| *p == "reasoning" || *p == "include_reasoning"),
                ),
                Some(params.iter().any(|p| *p == "tools" || *p == "tool_choice")),
            ),
            None => (None, None),
        };
        models.push(ProviderModel {
            id: id.to_string(),
            display_name: row.get("name").and_then(|v| v.as_str()).map(str::to_string),
            context_window: row.get("context_length").and_then(|v| v.as_u64()),
            max_output_tokens: row
                .get("top_provider")
                .and_then(|t| t.get("max_completion_tokens"))
                .and_then(|v| v.as_u64()),
            input_usd_per_mtok: per_token_str_to_per_mtok(pricing.and_then(|p| p.get("prompt"))),
            output_usd_per_mtok: per_token_str_to_per_mtok(
                pricing.and_then(|p| p.get("completion")),
            ),
            cached_input_usd_per_mtok: per_token_str_to_per_mtok(
                pricing.and_then(|p| p.get("input_cache_read")),
            ),
            cache_write_usd_per_mtok: per_token_str_to_per_mtok(
                pricing.and_then(|p| p.get("input_cache_write")),
            ),
            supports_reasoning,
            supports_tools,
        });
    }
    if models.is_empty() {
        return Err("OpenRouter model list contained no models — refusing an empty sync".into());
    }
    Ok(models)
}

/// Fetch OpenRouter's full live model list. The endpoint is public — no
/// key required — but it is only ever called when the user has configured
/// OpenRouter (key present), so discovery never phones a provider the
/// user isn't using.
pub async fn fetch_openrouter(base_url: &str) -> Result<Vec<ProviderModel>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let body = get_json("OpenRouter", &url, &[]).await?;
    parse_openrouter(&body)
}

// ---------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------

/// One page of Anthropic's `GET /v1/models`.
fn parse_anthropic_page(body: &str) -> Result<(Vec<ProviderModel>, Option<String>), String> {
    let root: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("Anthropic model list is unparseable JSON: {e}"))?;
    let Some(data) = root.get("data").and_then(|d| d.as_array()) else {
        return Err("Anthropic model list has no `data` array".to_string());
    };
    let models = data
        .iter()
        .filter_map(|row| {
            let id = row
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())?;
            Some(ProviderModel {
                id: id.to_string(),
                display_name: row
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                ..ProviderModel::default()
            })
        })
        .collect();
    let next = (root.get("has_more").and_then(|v| v.as_bool()) == Some(true))
        .then(|| {
            root.get("last_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .flatten();
    Ok((models, next))
}

/// Fetch every model the Anthropic API serves this key. Ids and display
/// names only — the API's listing carries no pricing or capability data,
/// so the catalog merge keeps whatever the master list already knows.
pub async fn fetch_anthropic(
    base_url: &str,
    api_key: &ApiKey,
) -> Result<Vec<ProviderModel>, String> {
    let base = base_url.trim_end_matches('/');
    let mut models = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let url = match &after {
            Some(id) => format!("{base}/v1/models?limit=1000&after_id={id}"),
            None => format!("{base}/v1/models?limit=1000"),
        };
        let body = get_json(
            "Anthropic",
            &url,
            &[
                ("x-api-key", api_key.reveal()),
                ("anthropic-version", "2023-06-01"),
            ],
        )
        .await?;
        let (mut page, next) = parse_anthropic_page(&body)?;
        models.append(&mut page);
        match next {
            Some(id) => after = Some(id),
            None => break,
        }
    }
    if models.is_empty() {
        return Err("Anthropic model list contained no models — refusing an empty sync".into());
    }
    Ok(models)
}

// ---------------------------------------------------------------------
// Gemini
// ---------------------------------------------------------------------

/// One page of Gemini's `GET /models` (the `ListModels` surface). Rows
/// that can't serve chat (`generateContent` absent from
/// `supportedGenerationMethods`: embeddings, imagen, aqa) are dropped —
/// they would be unusable-but-selectable, the exact bug this module fixes.
fn parse_gemini_page(body: &str) -> Result<(Vec<ProviderModel>, Option<String>), String> {
    let root: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("Gemini model list is unparseable JSON: {e}"))?;
    let Some(rows) = root.get("models").and_then(|d| d.as_array()) else {
        return Err("Gemini model list has no `models` array".to_string());
    };
    let models = rows
        .iter()
        .filter_map(|row| {
            let name = row.get("name").and_then(|v| v.as_str())?;
            let id = name.strip_prefix("models/").unwrap_or(name);
            if id.is_empty() {
                return None;
            }
            let methods: Vec<&str> = row
                .get("supportedGenerationMethods")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|m| m.as_str()).collect())
                .unwrap_or_default();
            if !methods.contains(&"generateContent") {
                return None;
            }
            Some(ProviderModel {
                id: id.to_string(),
                display_name: row
                    .get("displayName")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                context_window: row.get("inputTokenLimit").and_then(|v| v.as_u64()),
                max_output_tokens: row.get("outputTokenLimit").and_then(|v| v.as_u64()),
                // The listing's `thinking` flag (present on 2.5+ era rows);
                // absent means unknown, not "no".
                supports_reasoning: row.get("thinking").and_then(|v| v.as_bool()),
                ..ProviderModel::default()
            })
        })
        .collect();
    let next = root
        .get("nextPageToken")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Ok((models, next))
}

/// Fetch every chat-capable model the Gemini API serves this key.
pub async fn fetch_gemini(base_url: &str, api_key: &ApiKey) -> Result<Vec<ProviderModel>, String> {
    let base = base_url.trim_end_matches('/');
    let mut models: Vec<ProviderModel> = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let url = match &token {
            Some(t) => format!("{base}/models?pageSize=1000&pageToken={t}"),
            None => format!("{base}/models?pageSize=1000"),
        };
        let body = get_json("Gemini", &url, &[("x-goog-api-key", api_key.reveal())]).await?;
        let (mut page, next) = parse_gemini_page(&body)?;
        models.append(&mut page);
        match next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    if models.is_empty() {
        return Err("Gemini model list contained no chat models — refusing an empty sync".into());
    }
    Ok(models)
}

// ---------------------------------------------------------------------
// OpenAI-compatible (OpenAI, xAI, DeepSeek, Z.ai, local, custom gateways)
// ---------------------------------------------------------------------

/// Parse the OpenAI-shape `GET /models` document: `{"data": [{"id": …}]}`.
pub fn parse_openai_compatible(label: &str, body: &str) -> Result<Vec<ProviderModel>, String> {
    let root: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("{label} model list is unparseable JSON: {e}"))?;
    let Some(data) = root.get("data").and_then(|d| d.as_array()) else {
        return Err(format!("{label} model list has no `data` array"));
    };
    let models: Vec<ProviderModel> = data
        .iter()
        .filter_map(|row| {
            let id = row
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())?;
            Some(ProviderModel {
                id: id.to_string(),
                display_name: row
                    .get("display_name")
                    .or_else(|| row.get("name"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                context_window: row.get("context_length").and_then(|v| v.as_u64()),
                ..ProviderModel::default()
            })
        })
        .collect();
    if models.is_empty() {
        return Err(format!(
            "{label} model list contained no models — refusing an empty sync"
        ));
    }
    Ok(models)
}

/// Fetch the model list from any OpenAI-compatible endpoint. `label` names
/// the provider in error messages.
pub async fn fetch_openai_compatible(
    label: &str,
    base_url: &str,
    api_key: &ApiKey,
) -> Result<Vec<ProviderModel>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let auth = format!("Bearer {}", api_key.reveal());
    let body = get_json(label, &url, &[("Authorization", auth.as_str())]).await?;
    parse_openai_compatible(label, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENROUTER_FIXTURE: &str = r#"{
        "data": [
            {
                "id": "anthropic/claude-sonnet-4.5",
                "name": "Anthropic: Claude Sonnet 4.5",
                "context_length": 1000000,
                "pricing": {
                    "prompt": "0.000003",
                    "completion": "0.000015",
                    "input_cache_read": "0.0000003",
                    "input_cache_write": "0.00000375"
                },
                "top_provider": { "max_completion_tokens": 64000 },
                "supported_parameters": ["tools", "tool_choice", "reasoning", "max_tokens"]
            },
            {
                "id": "mistralai/mistral-7b-instruct",
                "name": "Mistral 7B Instruct",
                "context_length": 32768,
                "pricing": { "prompt": "0.00000006", "completion": "0.00000006" },
                "supported_parameters": ["max_tokens", "temperature"]
            },
            {
                "id": "openrouter/auto",
                "name": "Auto Router",
                "pricing": { "prompt": "-1", "completion": "-1" }
            },
            { "name": "row with no id is skipped" }
        ]
    }"#;

    #[test]
    fn openrouter_parses_pricing_limits_and_capability_parameters() {
        let models = parse_openrouter(OPENROUTER_FIXTURE).expect("fixture parses");
        assert_eq!(models.len(), 3);
        let sonnet = &models[0];
        assert_eq!(sonnet.id, "anthropic/claude-sonnet-4.5");
        // Per-token strings scaled to the catalog's USD-per-Mtok unit.
        assert_eq!(sonnet.input_usd_per_mtok, Some(3.0));
        assert_eq!(sonnet.output_usd_per_mtok, Some(15.0));
        assert_eq!(sonnet.cached_input_usd_per_mtok, Some(0.3));
        assert_eq!(sonnet.cache_write_usd_per_mtok, Some(3.75));
        assert_eq!(sonnet.context_window, Some(1_000_000));
        assert_eq!(sonnet.max_output_tokens, Some(64_000));
        assert_eq!(sonnet.supports_reasoning, Some(true));
        assert_eq!(sonnet.supports_tools, Some(true));

        // supported_parameters present without reasoning/tools → hard "no",
        // which is what lets the effort picker exclude these models.
        let mistral = &models[1];
        assert_eq!(mistral.supports_reasoning, Some(false));
        assert_eq!(mistral.supports_tools, Some(false));

        // Dynamic pricing ("-1") and no supported_parameters → unknown.
        let auto = &models[2];
        assert_eq!(auto.input_usd_per_mtok, None);
        assert_eq!(auto.supports_reasoning, None);
    }

    #[test]
    fn openrouter_rejects_shapes_that_are_not_the_model_list() {
        assert!(parse_openrouter("{}").is_err());
        assert!(parse_openrouter(r#"{"data": []}"#).is_err());
        assert!(parse_openrouter("not json").is_err());
    }

    #[test]
    fn anthropic_page_parses_ids_and_pagination_cursor() {
        let body = r#"{
            "data": [
                {"type": "model", "id": "claude-fable-5", "display_name": "Claude Fable 5"},
                {"type": "model", "id": "claude-haiku-4-5-20251001"}
            ],
            "has_more": true,
            "last_id": "claude-haiku-4-5-20251001"
        }"#;
        let (models, next) = parse_anthropic_page(body).expect("page parses");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-fable-5");
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Fable 5"));
        // The listing carries no capability/pricing data — stays unknown.
        assert_eq!(models[0].supports_reasoning, None);
        assert_eq!(next.as_deref(), Some("claude-haiku-4-5-20251001"));

        let (_, done) =
            parse_anthropic_page(r#"{"data": [{"id": "m"}], "has_more": false}"#).expect("parses");
        assert_eq!(done, None);
    }

    #[test]
    fn gemini_page_keeps_chat_models_and_drops_the_rest() {
        let body = r#"{
            "models": [
                {
                    "name": "models/gemini-3-pro",
                    "displayName": "Gemini 3 Pro",
                    "inputTokenLimit": 1000000,
                    "outputTokenLimit": 65536,
                    "thinking": true,
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/text-embedding-004",
                    "supportedGenerationMethods": ["embedContent"]
                },
                {
                    "name": "models/gemini-2.0-flash-lite",
                    "supportedGenerationMethods": ["generateContent"]
                }
            ],
            "nextPageToken": "tok-2"
        }"#;
        let (models, next) = parse_gemini_page(body).expect("page parses");
        assert_eq!(models.len(), 2, "embedding row is dropped");
        assert_eq!(models[0].id, "gemini-3-pro", "models/ prefix stripped");
        assert_eq!(models[0].context_window, Some(1_000_000));
        assert_eq!(models[0].max_output_tokens, Some(65_536));
        assert_eq!(models[0].supports_reasoning, Some(true));
        assert_eq!(
            models[1].supports_reasoning, None,
            "no thinking flag → unknown"
        );
        assert_eq!(next.as_deref(), Some("tok-2"));
    }

    #[test]
    fn openai_compatible_parses_bare_id_lists() {
        let body = r#"{"object": "list", "data": [
            {"id": "gpt-5.5", "object": "model", "owned_by": "openai"},
            {"id": "grok-4"},
            {"id": ""}
        ]}"#;
        let models = parse_openai_compatible("OpenAI", body).expect("parses");
        assert_eq!(models.len(), 2, "empty id dropped");
        assert_eq!(models[0].id, "gpt-5.5");
        assert_eq!(models[0].supports_reasoning, None);
        assert!(parse_openai_compatible("OpenAI", r#"{"data": []}"#).is_err());
        assert!(parse_openai_compatible("OpenAI", r#"{"models": []}"#).is_err());
    }

    #[tokio::test]
    async fn fetch_openrouter_hits_the_models_route_and_parses() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string(OPENROUTER_FIXTURE))
            .mount(&server)
            .await;
        let models = fetch_openrouter(&format!("{}/api/v1", server.uri()))
            .await
            .expect("fetches");
        assert_eq!(models.len(), 3);
    }

    #[tokio::test]
    async fn fetch_anthropic_paginates_with_after_id_and_sends_auth_headers() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "sk-test"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(query_param("after_id", "claude-a"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"data": [{"id": "claude-b"}], "has_more": false}"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data": [{"id": "claude-a"}], "has_more": true, "last_id": "claude-a"}"#,
            ))
            .mount(&server)
            .await;

        let models = fetch_anthropic(&server.uri(), &ApiKey::new("sk-test"))
            .await
            .expect("fetches both pages");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-a", "claude-b"]);
    }

    #[tokio::test]
    async fn fetch_surfaces_http_errors_with_the_provider_named() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error": "bad key"}"#))
            .mount(&server)
            .await;
        let err = fetch_openai_compatible("xAI", &server.uri(), &ApiKey::new("k"))
            .await
            .unwrap_err();
        assert!(
            err.contains("xAI") && err.contains("401"),
            "named error: {err}"
        );
    }
}
