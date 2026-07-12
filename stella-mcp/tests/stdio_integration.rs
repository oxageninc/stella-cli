//! End-to-end stdio transport tests driving the real `mcp-fixture-server`
//! binary (located via `CARGO_BIN_EXE_*`). Covers the full round-trip,
//! pagination, the §8 environment scrub, content mapping, error mapping, and
//! the resilience matrix (hang → timeout, mid-call death, garbage, protocol
//! negotiation).

use std::collections::BTreeMap;
use std::time::Duration;

use stella_mcp::{McpClient, McpError, McpServerConfig, McpToolSet, McpTransport};
use stella_protocol::ToolOutput;

/// The fixture server built by this crate.
const FIXTURE: &str = env!("CARGO_BIN_EXE_mcp-fixture-server");

fn stdio_config(name: &str, args: &[&str], env: BTreeMap<String, String>) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        transport: McpTransport::Stdio {
            cmd: FIXTURE.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env,
        },
    }
}

#[tokio::test]
async fn full_round_trip_initialize_list_call() {
    let cfg = stdio_config("fx", &[], BTreeMap::new());
    let client = McpClient::connect(&cfg, Duration::from_secs(5))
        .await
        .unwrap();

    assert_eq!(client.negotiated_version(), "2025-06-18");
    assert_eq!(client.server_info().unwrap().name, "mcp-fixture-server");
    let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"echo"));
    assert!(names.contains(&"env_probe"));

    let out = client
        .call_tool("echo", serde_json::json!({ "x": 1 }))
        .await
        .unwrap();
    match out {
        ToolOutput::Ok { content } => assert!(content.contains("echo:"), "got {content}"),
        other => panic!("expected Ok, got {other:?}"),
    }
    client.close().await.unwrap();
}

#[tokio::test]
async fn tools_list_follows_pagination() {
    let cfg = stdio_config("fx", &["--paginate"], BTreeMap::new());
    let client = McpClient::connect(&cfg, Duration::from_secs(5))
        .await
        .unwrap();
    let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["alpha", "beta"]);
    client.close().await.unwrap();
}

#[tokio::test]
async fn environment_is_scrubbed_but_configured_vars_pass_through() {
    // The parent test process essentially always has PATH; that's the ambient
    // credential-class variable we prove the child does NOT inherit.
    assert!(
        std::env::var("PATH").is_ok(),
        "expected PATH in the test env"
    );

    let mut env = BTreeMap::new();
    env.insert("ALLOWED".to_string(), "yes".to_string());
    let cfg = stdio_config("fx", &[], env);
    let client = McpClient::connect(&cfg, Duration::from_secs(5))
        .await
        .unwrap();

    // Explicitly-configured var reaches the child…
    let allowed = client
        .call_tool("env_probe", serde_json::json!({ "var": "ALLOWED" }))
        .await
        .unwrap();
    assert_eq!(
        allowed,
        ToolOutput::Ok {
            content: "yes".into()
        }
    );

    // …but the ambient PATH does not — the environment was scrubbed (§8).
    let path = client
        .call_tool("env_probe", serde_json::json!({ "var": "PATH" }))
        .await
        .unwrap();
    assert_eq!(
        path,
        ToolOutput::Ok {
            content: "unset".into()
        }
    );

    client.close().await.unwrap();
}

#[tokio::test]
async fn non_text_content_is_summarized() {
    let cfg = stdio_config("fx", &[], BTreeMap::new());
    let client = McpClient::connect(&cfg, Duration::from_secs(5))
        .await
        .unwrap();

    let image = client
        .call_tool("make_image", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        image,
        ToolOutput::Ok {
            content: "[image]".into()
        }
    );

    let resource = client
        .call_tool("make_resource", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        resource,
        ToolOutput::Ok {
            content: "[resource: file:///r.txt]".into()
        }
    );
    client.close().await.unwrap();
}

#[tokio::test]
async fn is_error_and_jsonrpc_error_map_distinctly() {
    let cfg = stdio_config("fx", &[], BTreeMap::new());
    let client = McpClient::connect(&cfg, Duration::from_secs(5))
        .await
        .unwrap();

    // isError:true is an Ok JSON-RPC response for a tool that ran and failed.
    let failed = client
        .call_tool("fail", serde_json::json!({}))
        .await
        .unwrap();
    assert!(failed.is_error());

    // A JSON-RPC error object is a rejected request — a hard McpError.
    let err = client
        .call_tool("jsonrpc_error", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::JsonRpc { code: -32000, .. }));

    client.close().await.unwrap();
}

#[tokio::test]
async fn garbage_result_is_a_protocol_error() {
    let cfg = stdio_config("fx", &["--garbage"], BTreeMap::new());
    let client = McpClient::connect(&cfg, Duration::from_secs(5))
        .await
        .unwrap();
    let err = client
        .call_tool("echo", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::Protocol(_)));
    client.close().await.unwrap();
}

#[tokio::test]
async fn accepts_older_protocol_counter_offer() {
    let cfg = stdio_config("fx", &["--protocol-version", "2024-11-05"], BTreeMap::new());
    let client = McpClient::connect(&cfg, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(client.negotiated_version(), "2024-11-05");
    client.close().await.unwrap();
}

#[tokio::test]
async fn rejects_an_unspeakable_protocol_version() {
    let cfg = stdio_config("fx", &["--protocol-version", "1999-01-01"], BTreeMap::new());
    let result = McpClient::connect(&cfg, Duration::from_secs(5)).await;
    assert!(matches!(result, Err(McpError::UnsupportedProtocol { .. })));
}

#[tokio::test]
async fn toolset_over_stdio_namespaces_and_routes() {
    let cfg = stdio_config("fx", &[], BTreeMap::new());
    let set = McpToolSet::connect(std::slice::from_ref(&cfg), Duration::from_secs(5)).await;

    assert_eq!(set.connected_count(), 1);
    assert!(set.failed_servers().is_empty());

    let names: Vec<String> = stella_core::ports::ToolExecutor::schemas(&set)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(names.iter().any(|n| n == "mcp__fx__echo"), "got {names:?}");

    let out = stella_core::ports::ToolExecutor::execute(
        &set,
        "mcp__fx__echo",
        &serde_json::json!({ "a": 1 }),
    )
    .await;
    assert!(matches!(out, ToolOutput::Ok { .. }));
    set.close_all().await;
}

#[tokio::test]
async fn hung_server_times_out_naming_it_without_poisoning_the_set() {
    let cfg = stdio_config("hangsrv", &["--hang"], BTreeMap::new());
    let set = McpToolSet::connect(std::slice::from_ref(&cfg), Duration::from_secs(5))
        .await
        .with_call_timeout(Duration::from_millis(200));

    // Handshake answered normally; only tools/call hangs.
    assert_eq!(set.connected_count(), 1);

    let out = stella_core::ports::ToolExecutor::execute(
        &set,
        "mcp__hangsrv__echo",
        &serde_json::json!({}),
    )
    .await;
    match out {
        ToolOutput::Error { message } => {
            assert!(message.contains("hangsrv"), "names the server: {message}");
            assert!(message.contains("timed out"), "got {message}");
        }
        other => panic!("expected a timeout error, got {other:?}"),
    }
    set.close_all().await;
}

#[tokio::test]
async fn server_death_mid_call_errors_naming_it() {
    // `--die-after 2`: initialize (1) + tools/list (2) are answered, so
    // connect succeeds; the first tools/call kills the server before it
    // responds.
    let cfg = stdio_config("diesrv", &["--die-after", "2"], BTreeMap::new());
    let set = McpToolSet::connect(std::slice::from_ref(&cfg), Duration::from_secs(5))
        .await
        .with_call_timeout(Duration::from_secs(2));

    assert_eq!(set.connected_count(), 1);

    let out = stella_core::ports::ToolExecutor::execute(
        &set,
        "mcp__diesrv__echo",
        &serde_json::json!({}),
    )
    .await;
    match out {
        ToolOutput::Error { message } => assert!(message.contains("diesrv"), "got {message}"),
        other => panic!("expected an error naming the dead server, got {other:?}"),
    }
    set.close_all().await;
}

#[tokio::test]
async fn a_failed_server_does_not_block_a_healthy_one() {
    // One server whose command cannot be spawned, and one healthy fixture.
    let bad = McpServerConfig {
        name: "broken".into(),
        transport: McpTransport::Stdio {
            cmd: "definitely-not-a-real-binary-xyzzy".into(),
            args: vec![],
            env: BTreeMap::new(),
        },
    };
    let good = stdio_config("fx", &[], BTreeMap::new());
    let set = McpToolSet::connect(&[bad, good], Duration::from_secs(5)).await;

    assert_eq!(
        set.connected_count(),
        1,
        "the healthy server still connects"
    );
    assert_eq!(set.failed_servers().len(), 1);
    assert_eq!(set.failed_servers()[0].0, "broken");

    let out =
        stella_core::ports::ToolExecutor::execute(&set, "mcp__fx__echo", &serde_json::json!({}))
            .await;
    assert!(matches!(out, ToolOutput::Ok { .. }));
    set.close_all().await;
}
