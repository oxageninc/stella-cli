//! Integration tests for the web family over a wiremock server — the whole
//! HTTP surface (fetch rendering, per-domain auth injection, both search
//! providers, asset extraction, download confinement) without touching the
//! real network.

use std::sync::Arc;

use serde_json::json;
use stella_protocol::tool::ToolOutput;
use stella_tools::registry::Tool;
use stella_tools::web::{
    SearchBackend, SearchProvider, WebAuthConfig, WebAuthState, WebDownload, WebExtractAssets,
    WebFetch, WebSearch,
};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn no_auth() -> Arc<WebAuthState> {
    Arc::new(Ok(WebAuthConfig::default()))
}

fn auth_from(toml_text: &str) -> Arc<WebAuthState> {
    Arc::new(Ok(toml::from_str(toml_text).expect("test auth toml")))
}

fn expect_ok(output: ToolOutput) -> String {
    match output {
        ToolOutput::Ok { content } => content,
        ToolOutput::Error { message } => panic!("expected Ok, got error: {message}"),
    }
}

fn expect_error(output: ToolOutput) -> String {
    match output {
        ToolOutput::Error { message } => message,
        ToolOutput::Ok { content } => panic!("expected Error, got: {content}"),
    }
}

#[tokio::test]
async fn web_fetch_renders_html_as_markdown_with_absolute_links() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/post"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<html><head><title>My Post</title></head><body>\
             <h1>Welcome</h1><p>Read <a href=\"/more\">more here</a>.</p></body></html>",
            "text/html; charset=utf-8",
        ))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    let out = WebFetch(no_auth())
        .execute(
            &json!({"url": format!("{}/post", server.uri())}),
            root.path(),
        )
        .await;
    let content = expect_ok(out);
    assert!(content.contains("# My Post"), "{content}");
    assert!(content.contains("# Welcome"), "{content}");
    assert!(
        content.contains(&format!("[more here]({}/more)", server.uri())),
        "{content}"
    );
    assert!(content.contains("Source:"), "{content}");
}

#[tokio::test]
async fn web_fetch_sends_configured_domain_auth_and_reports_it_by_name_only() {
    let server = MockServer::start().await;
    // The mock only matches when the cookie and extra header arrive — a
    // fetch without the injected auth 404s and fails the test.
    Mock::given(method("GET"))
        .and(path("/paywalled"))
        .and(header("Cookie", "session=secret-cookie-value"))
        .and(header("X-Client", "stella-test"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("<html><body><p>Members only</p></body></html>", "text/html"),
        )
        .mount(&server)
        .await;

    let auth = auth_from(
        r#"
        [domains."127.0.0.1"]
        cookie = "session=secret-cookie-value"
        [domains."127.0.0.1".headers]
        X-Client = "stella-test"
        "#,
    );
    let root = tempfile::tempdir().unwrap();
    let out = WebFetch(auth)
        .execute(
            &json!({"url": format!("{}/paywalled", server.uri())}),
            root.path(),
        )
        .await;
    let content = expect_ok(out);
    assert!(content.contains("Members only"), "{content}");
    assert!(
        content.contains("authenticated via web_auth.toml for `127.0.0.1`"),
        "{content}"
    );
    assert!(
        !content.contains("secret-cookie-value"),
        "auth values must never appear in tool output: {content}"
    );
}

#[tokio::test]
async fn web_fetch_refuses_non_http_schemes_and_flags_binary_bodies() {
    let root = tempfile::tempdir().unwrap();
    let message = expect_error(
        WebFetch(no_auth())
            .execute(&json!({"url": "file:///etc/passwd"}), root.path())
            .await,
    );
    assert!(message.contains("only http/https"), "{message}");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/blob"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(vec![0u8, 159, 146, 150], "application/octet-stream"),
        )
        .mount(&server)
        .await;
    let content = expect_ok(
        WebFetch(no_auth())
            .execute(
                &json!({"url": format!("{}/blob", server.uri())}),
                root.path(),
            )
            .await,
    );
    assert!(content.contains("binary content"), "{content}");
    assert!(content.contains("web_download"), "{content}");
}

#[tokio::test]
async fn web_search_brave_sends_the_token_and_parses_results() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/res/v1/web/search"))
        .and(query_param("q", "stella agent"))
        .and(query_param("count", "2"))
        .and(header("X-Subscription-Token", "brave-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "web": { "results": [
                { "title": "Stella", "url": "https://stella.oxagen.sh", "description": "The agent." },
                { "title": "Repo", "url": "https://github.com/macanderson/stella", "description": "Source." }
            ]}
        })))
        .mount(&server)
        .await;

    let backend = SearchBackend::with_endpoint(
        SearchProvider::Brave,
        "brave-key",
        format!("{}/res/v1/web/search", server.uri()),
    );
    let root = tempfile::tempdir().unwrap();
    let content = expect_ok(
        WebSearch(backend)
            .execute(&json!({"query": "stella agent", "count": 2}), root.path())
            .await,
    );
    assert!(
        content.contains("2 results for \"stella agent\" (brave)"),
        "{content}"
    );
    assert!(content.contains("1. Stella"), "{content}");
    assert!(content.contains("https://stella.oxagen.sh"), "{content}");
    assert!(content.contains("2. Repo"), "{content}");
}

#[tokio::test]
async fn web_search_tavily_posts_the_query_with_a_bearer_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .and(header("Authorization", "Bearer tvly-key"))
        .and(wiremock::matchers::body_string_contains("\"query\":\"rust scraper\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "title": "scraper crate", "url": "https://docs.rs/scraper", "content": "HTML parsing." }
            ]
        })))
        .mount(&server)
        .await;

    let backend = SearchBackend::with_endpoint(
        SearchProvider::Tavily,
        "tvly-key",
        format!("{}/search", server.uri()),
    );
    let root = tempfile::tempdir().unwrap();
    let content = expect_ok(
        WebSearch(backend)
            .execute(&json!({"query": "rust scraper"}), root.path())
            .await,
    );
    assert!(
        content.contains("1 results for \"rust scraper\" (tavily)"),
        "{content}"
    );
    assert!(content.contains("https://docs.rs/scraper"), "{content}");
}

#[tokio::test]
async fn web_extract_assets_mines_stylesheets_into_design_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r##"<html><head><title>Brand</title>
                <link rel="stylesheet" href="/css/site.css">
                <style>.hero { color: #ff6a3d }</style>
                </head><body><img src="/img/logo.png"></body></html>"##,
            "text/html",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/css/site.css"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r##":root { --color-accent: #ff6a3d; }
                body { color: #ff6a3d; font-family: "Inter", sans-serif; }
                @font-face { font-family: "Inter"; src: url("/fonts/inter.woff2"); }"##,
            "text/css",
        ))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    let content = expect_ok(
        WebExtractAssets(no_auth())
            .execute(&json!({"url": server.uri()}), root.path())
            .await,
    );
    assert!(content.contains("Asset manifest"), "{content}");
    assert!(content.contains("site.css (fetched"), "{content}");
    // #ff6a3d appears in the inline block, the body rule, and the token.
    assert!(content.contains("- #ff6a3d (3)"), "{content}");
    assert!(content.contains("Inter"), "{content}");
    assert!(content.contains("--color-accent: #ff6a3d"), "{content}");
    assert!(content.contains("/fonts/inter.woff2"), "{content}");
    assert!(content.contains("/img/logo.png"), "{content}");
    assert!(content.contains("web_download"), "{content}");
}

#[tokio::test]
async fn web_download_writes_inside_the_root_and_rejects_escapes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fonts/inter.woff2"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![1u8, 2, 3, 4], "font/woff2"))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    let url = format!("{}/fonts/inter.woff2", server.uri());
    let content = expect_ok(
        WebDownload(no_auth())
            .execute(
                &json!({"url": url, "path": ".stella/artifacts/web/inter.woff2"}),
                root.path(),
            )
            .await,
    );
    assert!(content.contains("4 bytes"), "{content}");
    let written = root.path().join(".stella/artifacts/web/inter.woff2");
    assert_eq!(std::fs::read(&written).unwrap(), vec![1u8, 2, 3, 4]);

    let message = expect_error(
        WebDownload(no_auth())
            .execute(&json!({"url": url, "path": "../outside.bin"}), root.path())
            .await,
    );
    assert!(message.contains("escapes the workspace root"), "{message}");
}

#[tokio::test]
async fn web_fetch_names_the_auth_file_hint_on_a_401() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/private"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    let message = expect_error(
        WebFetch(no_auth())
            .execute(
                &json!({"url": format!("{}/private", server.uri())}),
                root.path(),
            )
            .await,
    );
    assert!(message.contains("HTTP 401"), "{message}");
    assert!(message.contains("web_auth.toml"), "{message}");
}
