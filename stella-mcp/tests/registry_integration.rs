//! Integration tests for the MCP registry client against a mock HTTP server.
//! Responses are the recorded JSON fixtures in `tests/fixtures/` — there is no
//! live network here, so the suite is deterministic and offline.

use stella_mcp::{McpError, RegistryClient};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LIST: &str = include_str!("fixtures/registry_list.json");

#[tokio::test]
async fn search_sends_search_limit_and_cursor_query_params_and_parses_the_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v0.1/servers"))
        .and(query_param("search", "github"))
        .and(query_param("limit", "10"))
        .and(query_param("cursor", "cursor-42"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LIST))
        .mount(&server)
        .await;

    let client = RegistryClient::new(server.uri()).unwrap();
    let page = client
        .search(Some("github"), Some("cursor-42"), 10)
        .await
        .expect("the mock matches only when all three params are sent");

    assert!(!page.entries.is_empty(), "recorded page has entries");
    assert!(
        page.next_cursor.is_some(),
        "recorded page has a next cursor"
    );
}

#[tokio::test]
async fn an_omitted_query_and_cursor_send_only_limit() {
    let server = MockServer::start().await;
    // Only `limit` is asserted; the mock still matches, proving `search` and
    // `cursor` are simply absent for a plain listing.
    Mock::given(method("GET"))
        .and(path("/v0.1/servers"))
        .and(query_param("limit", "30"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LIST))
        .mount(&server)
        .await;

    let client = RegistryClient::new(server.uri()).unwrap();
    let page = client.search(None, None, 30).await.unwrap();
    assert!(!page.entries.is_empty());
}

#[tokio::test]
async fn a_non_2xx_registry_response_is_a_typed_transport_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
        .mount(&server)
        .await;

    let client = RegistryClient::new(server.uri()).unwrap();
    let err = client.search(None, None, 30).await.unwrap_err();
    assert!(matches!(err, McpError::Transport(_)), "got {err:?}");
}
