use super::*;
use stella_protocol::tool::ToolSchema;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn to_zai_messages_maps_all_roles() {
    let messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("hi"),
    ];
    let mapped = to_zai_messages(&messages);
    assert_eq!(mapped.len(), 2);
    assert_eq!(mapped[0].role, "system");
    assert_eq!(mapped[1].role, "user");
}

#[test]
fn to_zai_messages_frames_tool_results_with_call_ids() {
    use stella_protocol::{ToolOutput, ToolResult};
    let messages = vec![
        CompletionMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![stella_protocol::ToolCall {
                call_id: "call_9".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "a.rs"}),
            }],
            tool_results: vec![],
            attachments: Vec::new(),
        },
        CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: "call_9".into(),
                output: ToolOutput::Ok {
                    content: "fn main(){}".into(),
                },
            }],
            attachments: Vec::new(),
        },
    ];
    let mapped = to_zai_messages(&messages);
    assert_eq!(mapped.len(), 2);
    assert_eq!(mapped[0].role, "assistant");
    assert_eq!(mapped[0].tool_calls.len(), 1);
    assert_eq!(mapped[1].role, "tool");
    assert_eq!(mapped[1].tool_call_id.as_deref(), Some("call_9"));
    assert_eq!(mapped[1].content.as_text(), "fn main(){}");
}

#[test]
fn user_attachments_widen_content_to_parts_with_data_uri_images() {
    use stella_protocol::{Attachment, AttachmentSource};
    let att = |name: &str, mime: &str, b64: &str| Attachment {
        name: name.into(),
        media_type: mime.into(),
        byte_len: 3,
        source: AttachmentSource::Data { base64: b64.into() },
    };
    let messages = vec![
        CompletionMessage::user("plain"),
        CompletionMessage::user_with_attachments(
            "look",
            vec![
                att("a.png", "image/png", "aW1n"),
                att("b.pdf", "application/pdf", "cGRm"),
            ],
        ),
    ];
    let mapped = to_zai_messages(&messages);
    // A text-only user turn stays a plain string — byte-stable with the
    // pre-attachment wire format.
    let plain = serde_json::to_value(&mapped[0]).unwrap();
    assert_eq!(plain["content"], "plain");
    let multi = serde_json::to_value(&mapped[1]).unwrap();
    let parts = multi["content"].as_array().unwrap();
    assert_eq!(parts.len(), 3, "{multi}");
    assert_eq!(parts[0]["type"], "image_url");
    assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,aW1n");
    // PDF degrades to a note on this dialect.
    assert_eq!(parts[1]["type"], "text");
    assert!(parts[1]["text"].as_str().unwrap().contains("b.pdf"));
    assert_eq!(parts[2]["type"], "text");
    assert_eq!(parts[2]["text"], "look");
}

#[test]
fn to_zai_messages_marks_error_results_loudly() {
    use stella_protocol::{ToolOutput, ToolResult};
    let messages = vec![CompletionMessage {
        role: MessageRole::Tool,
        content: String::new(),
        tool_calls: vec![],
        tool_results: vec![ToolResult {
            call_id: "call_1".into(),
            output: ToolOutput::Error {
                message: "no such file".into(),
            },
        }],
        attachments: Vec::new(),
    }];
    let mapped = to_zai_messages(&messages);
    assert_eq!(mapped.len(), 1);
    assert!(mapped[0].content.as_text().starts_with("ERROR:"));
}

#[tokio::test]
async fn complete_aggregates_text_deltas_from_a_mock_server() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo!\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("say hello")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let result = provider
        .complete(req)
        .await
        .expect("completion should succeed");
    assert_eq!(result.text, "Hello!");
    assert_eq!(result.usage.input_tokens, 8);
    assert_eq!(result.usage.output_tokens, 3);
}

#[tokio::test]
async fn complete_reassembles_a_streamed_tool_call_split_across_many_chunks() {
    let server = MockServer::start().await;
    // GLM 5.2 streams tool_calls as index-keyed fragments; arguments
    // arrive as partial JSON string pieces across several chunks —
    // exactly the dialect quirk this test proves the adapter handles.
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"file\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"src/lib.rs\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("read src/lib.rs")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![ToolSchema {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: false,
        }],
        reasoning: None,
        params: None,
    };

    let result = provider
        .complete(req)
        .await
        .expect("completion should succeed");
    assert_eq!(result.tool_calls.len(), 1);
    let call = &result.tool_calls[0];
    assert_eq!(call.call_id, "call_1");
    assert_eq!(call.name, "read_file");
    assert_eq!(call.input, serde_json::json!({"path": "src/lib.rs"}));
}

/// Records announcements for comparison with the committed result.
struct RecordingObserver {
    calls: std::sync::Mutex<Vec<ToolCall>>,
    deltas: std::sync::Mutex<Vec<String>>,
}

impl RecordingObserver {
    fn new() -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            deltas: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl ToolCallObserver for RecordingObserver {
    fn tool_call_streamed(&self, call: &ToolCall) {
        self.calls.lock().unwrap().push(call.clone());
    }
    fn text_delta(&self, delta: &str) {
        self.deltas.lock().unwrap().push(delta.to_string());
    }
}

#[tokio::test]
async fn complete_observed_streams_content_deltas_in_order_never_reasoning() {
    let server = MockServer::start().await;
    // Answer `content` interleaved with `reasoning_content`: the observer
    // must see exactly the visible fragments, in stream order — the
    // chain-of-thought never streams to it.
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"let me think\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"more thought\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo!\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("say hello")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let observer = RecordingObserver::new();
    let result = provider
        .complete_observed(req, &observer)
        .await
        .expect("should succeed");

    let deltas = observer.deltas.lock().unwrap();
    assert_eq!(
        *deltas,
        vec!["Hel".to_string(), "lo!".to_string()],
        "answer fragments only, in order — reasoning excluded"
    );
    assert_eq!(
        result.text, "Hello!",
        "the committed text is the announced deltas' concatenation"
    );
}

#[tokio::test]
async fn complete_observed_announces_a_call_when_the_next_index_starts() {
    let server = MockServer::start().await;
    // Two sequential tool calls: index 0 is complete the moment index 1
    // appears — the only mid-stream completion boundary the OpenAI
    // dialect offers. Index 1 (the last call) has no boundary and is
    // never announced.
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"b.rs\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("read both")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let observer = RecordingObserver::new();
    let result = provider
        .complete_observed(req, &observer)
        .await
        .expect("completion should succeed");

    let announced = observer.calls.lock().unwrap();
    assert_eq!(result.tool_calls.len(), 2);
    assert_eq!(
        announced.len(),
        1,
        "only index 0 has a completion boundary mid-stream"
    );
    assert_eq!(
        announced[0], result.tool_calls[0],
        "an announced call must be identical to its committed twin"
    );
}

#[tokio::test]
async fn usage_accounting_sends_the_include_field_and_takes_the_reported_cost() {
    let server = MockServer::start().await;
    // OpenRouter's final usage frame carries the routed call's actual
    // cost; for an unseeded slug (no catalog pricing) that report is the
    // ONLY real price. The request must opt in via `usage.include`.
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"cost\":0.0123}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"usage\":{\"include\":true}"))
        .and(header("HTTP-Referer", "https://stella.oxagen.sh"))
        .and(header("X-Title", "Stella"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = ZaiProvider::new(ApiKey::new("sk-or-test"), "anthropic/claude-sonnet-4.5")
        .with_base_url(server.uri())
        .with_identity("openrouter", "OpenRouter")
        .with_attribution("https://stella.oxagen.sh", "Stella")
        .with_usage_accounting();
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    // The mock's matchers make a missing usage field / missing headers a
    // 404 — reaching a successful result proves the request shape.
    let result = provider.complete(req).await.expect("should succeed");
    assert!(
        (result.cost_usd - 0.0123).abs() < 1e-12,
        "gateway-reported cost is authoritative, got {}",
        result.cost_usd
    );
    assert_eq!(result.usage.input_tokens, 100);
}

#[tokio::test]
async fn openrouter_reasoning_deltas_surface_like_reasoning_content() {
    let server = MockServer::start().await;
    // OpenRouter normalizes chain-of-thought to `delta.reasoning`. A
    // reasoning-only turn must surface it, same as GLM's
    // `reasoning_content`, instead of returning a blank turn.
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning\":\"thinking \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning\":\"hard\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = ZaiProvider::new(ApiKey::new("sk-or-test"), "openrouter/auto")
        .with_base_url(server.uri())
        .with_identity("openrouter", "OpenRouter");
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };
    let result = provider.complete(req).await.expect("should succeed");
    assert_eq!(result.text, "thinking hard");
}

#[tokio::test]
async fn complete_falls_back_to_null_when_streamed_tool_arguments_never_parse() {
    let server = MockServer::start().await;
    // GLM streams a tool call whose argument fragments never form valid
    // JSON. The adapter must fall back to `Value::Null` — the exact
    // sentinel `driver.rs::execute_with_repair` checks for — so the repair
    // loop (tuned to GLM's failure shapes) can ask the model to retry,
    // rather than hard-erroring the whole turn before any `ToolCall` is
    // produced.
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_7\",\"function\":{\"name\":\"bash\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{not valid json\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("run ls")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![ToolSchema {
            name: "bash".into(),
            description: "Run a command".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: false,
        }],
        reasoning: None,
        params: None,
    };

    let result = provider
        .complete(req)
        .await
        .expect("malformed args must not error the turn — they become the repair sentinel");
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].name, "bash");
    assert_eq!(result.tool_calls[0].input, Value::Null);
}

#[tokio::test]
async fn complete_surfaces_a_tool_call_truncated_at_the_token_limit_not_null() {
    let server = MockServer::start().await;
    // GLM streams a large tool call, but the output is cut off at the token
    // limit MID-arguments: the final chunk carries `finish_reason: "length"`
    // and the accumulated `arguments` is an unterminated JSON fragment.
    // Unlike the malformed-but-complete case (which becomes the repair
    // sentinel), a truncation is a terminal, actionable failure that must
    // surface with a raw snippet — never a silent null the repair loop
    // spins on forever (the reported "stuck-loop").
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write_file\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"README.md\\\",\\\"content\\\":\\\"# Title\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("write the readme")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![ToolSchema {
            name: "write_file".into(),
            description: "Write a file".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: false,
        }],
        reasoning: None,
        params: None,
    };

    let err = provider.complete(req).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::Terminal(_)),
        "a truncated tool call must be terminal, got {err:?}"
    );
    assert!(!err.is_retryable(), "re-issuing the request re-truncates");
    let msg = err.to_string();
    assert!(msg.contains("write_file"), "names the tool: {msg}");
    assert!(
        msg.contains("finish_reason=length"),
        "names the cause: {msg}"
    );
    assert!(msg.contains("README.md"), "carries a raw snippet: {msg}");
}

/// With parallel tool calls and a `finish_reason: "length"` cut, the
/// truncation error must blame the call that was actually cut (the
/// highest index — the one still streaming) — an earlier call whose
/// complete-but-broken JSON is GLM's own output must not be misreported
/// as truncated.
#[tokio::test]
async fn truncation_is_blamed_on_the_last_call_not_an_earlier_broken_one() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{not valid json\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"README\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    let err = provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("do both")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        })
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("write_file"),
        "blames the truncated call: {msg}"
    );
    assert!(
        !msg.contains("bash"),
        "must not blame the earlier repairable call: {msg}"
    );
}

/// `finish_reason: "length"` landing after a call's id/name but before
/// any argument fragment must surface the terminal truncation error —
/// never a silent `{}` that executes with missing parameters.
#[tokio::test]
async fn a_tool_call_with_no_arguments_cut_at_the_token_limit_is_terminal() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write_file\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    let err = provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("write the readme")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::Terminal(_)),
        "expected Terminal, got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("write_file"), "names the tool: {msg}");
}

#[tokio::test]
async fn complete_maps_a_real_throttle_429_to_retryable_rate_limited() {
    let server = MockServer::start().await;
    // A genuine throttle body (no billing/quota keywords) stays a
    // retryable RateLimited — the normal, correct 429 path.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string(r#"{"error":{"code":"1302","message":"API rate limit reached"}}"#),
        )
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let err = provider.complete(req).await.unwrap_err();
    assert!(matches!(err, ProviderError::RateLimited { .. }));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn complete_maps_insufficient_balance_429_to_terminal_not_rate_limited() {
    let server = MockServer::start().await;
    // The exact body a live, empty-balance Z.ai account returns for its
    // very first request. HTTP 429, but NOT a rate limit — a billing
    // failure that no retry can clear. It must classify as terminal, be
    // NON-retryable, and surface Z.ai's own message rather than a
    // hard-coded "rate limit" string.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string(
            r#"{"error":{"code":"1113","message":"Insufficient balance or no resource package. Please recharge."}}"#,
        ))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let err = provider.complete(req).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::Terminal(_)),
        "insufficient-balance 429 must be terminal, got {err:?}"
    );
    assert!(
        !err.is_retryable(),
        "a billing failure must never be retried"
    );
    assert!(
        err.to_string().contains("Insufficient balance"),
        "the real Z.ai message must be surfaced, got: {err}"
    );
}

#[tokio::test]
async fn complete_honors_retry_after_header_on_a_throttle_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "2")
                .set_body_string("too many requests"),
        )
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let err = provider.complete(req).await.unwrap_err();
    match err {
        ProviderError::RateLimited { retry_after_ms, .. } => {
            assert_eq!(retry_after_ms, Some(2_000), "Retry-After: 2s → 2000ms");
        }
        other => panic!("expected RateLimited with a retry hint, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_computes_nonzero_cost_from_catalog_pricing() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":500}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let result = provider.complete(req).await.expect("should succeed");
    // Budget metering is no longer a no-op: cost is derived from the
    // catalog's glm-5.2 pricing and the streamed usage, not hard-coded 0.
    let expected = Catalog::seed()
        .resolve("glm-5.2")
        .unwrap()
        .pricing
        .cost_usd(&CompletionUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
        });
    assert!(result.cost_usd > 0.0, "cost must be non-zero");
    assert_eq!(result.cost_usd, expected);
}

/// Z.ai's implicit prompt cache reports hits via the OpenAI-compatible
/// `prompt_tokens_details.cached_tokens` field. Dropping it (the prior
/// behavior) overbilled cached tokens at the full input rate and pinned
/// the cache-hit stat at zero for every Z.ai run.
#[tokio::test]
async fn complete_surfaces_cached_tokens_and_bills_them_at_the_cached_rate() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":500,\"prompt_tokens_details\":{\"cached_tokens\":200}}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let result = provider.complete(req).await.expect("should succeed");
    assert_eq!(result.usage.cached_input_tokens, 200);
    let expected = Catalog::seed()
        .resolve("glm-5.2")
        .unwrap()
        .pricing
        .cost_usd(&CompletionUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cached_input_tokens: 200,
            cache_write_tokens: 0,
        });
    assert_eq!(result.cost_usd, expected, "200 cached tokens bill cheaper");
}

#[tokio::test]
async fn complete_maps_5xx_to_retryable_transport() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("service unavailable"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let err = provider.complete(req).await.unwrap_err();
    assert!(matches!(err, ProviderError::Transport(_)));
    assert!(err.is_retryable(), "5xx must be retryable");
}

#[tokio::test]
async fn complete_returns_err_on_mid_stream_error_frame_not_truncated_ok() {
    let server = MockServer::start().await;
    // Some text arrives, THEN an in-band error frame: the turn must fail
    // rather than return the partial "Hel" as a success.
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"error\":{\"type\":\"server_error\",\"message\":\"upstream overloaded\"}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let err = provider.complete(req).await.unwrap_err();
    // server_error / overloaded ⇒ retryable Transport.
    assert!(matches!(err, ProviderError::Transport(_)));
    assert!(err.is_retryable());
}

/// Minimal happy-path SSE body for tests that only inspect the request.
const OK_SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";

async fn mock_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(OK_SSE, "text/event-stream"))
        .mount(server)
        .await;
}

async fn first_request_body(server: &MockServer) -> String {
    let requests = server.received_requests().await.expect("recorded requests");
    String::from_utf8_lossy(&requests[0].body).into_owned()
}

#[tokio::test]
async fn generation_params_land_in_the_wire_body_including_nonstandard_fields() {
    use stella_protocol::GenerationParams;
    let server = MockServer::start().await;
    mock_ok(&server).await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: Some(GenerationParams {
                top_p: Some(0.9),
                top_k: Some(40),
                frequency_penalty: Some(0.5),
                presence_penalty: Some(0.25),
                repetition_penalty: Some(1.1),
                seed: Some(7),
                // Not part of this dialect — must never reach the wire.
                verbosity: Some(stella_protocol::Verbosity::High),
                service_tier: Some(stella_protocol::ServiceTier::Priority),
            }),
        })
        .await
        .expect("should succeed");

    let body = first_request_body(&server).await;
    assert!(body.contains("\"top_p\":0.9"), "{body}");
    // top_k / repetition_penalty are non-standard but the user opted in.
    assert!(body.contains("\"top_k\":40"), "{body}");
    assert!(body.contains("\"frequency_penalty\":0.5"), "{body}");
    assert!(body.contains("\"presence_penalty\":0.25"), "{body}");
    assert!(body.contains("\"repetition_penalty\":1.1"), "{body}");
    assert!(body.contains("\"seed\":7"), "{body}");
    // verbosity/service_tier have no Chat Completions slot: dropped.
    assert!(!body.contains("verbosity"), "{body}");
    assert!(!body.contains("service_tier"), "{body}");
}

/// The prompt-cache stability contract: a request without params or a
/// reasoning preference must serialize with none of the new keys — the
/// body is byte-identical to what this adapter sent before they existed.
#[tokio::test]
async fn absent_params_and_reasoning_add_no_keys_to_the_body() {
    let server = MockServer::start().await;
    mock_ok(&server).await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        })
        .await
        .expect("should succeed");

    let body = first_request_body(&server).await;
    for key in [
        "top_p",
        "top_k",
        "frequency_penalty",
        "presence_penalty",
        "repetition_penalty",
        "seed",
        "thinking",
        "reasoning",
        "cache_control",
    ] {
        assert!(!body.contains(key), "unexpected `{key}` in: {body}");
    }
}

/// Anthropic models routed through OpenRouter have explicit opt-in
/// caching: without a `cache_control` breakpoint the gateway never
/// caches a byte, so every agent turn re-bills its full growing prefix
/// at the uncached input rate (the "CACHE 0%" defect). The root-level
/// object opts in and lets OpenRouter advance the breakpoint per turn.
#[tokio::test]
async fn openrouter_identity_sends_root_level_cache_control() {
    let server = MockServer::start().await;
    mock_ok(&server).await;

    let provider = ZaiProvider::new(ApiKey::new("sk-or-test"), "anthropic/claude-sonnet-5")
        .with_base_url(server.uri())
        .with_identity("openrouter", "OpenRouter");
    provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        })
        .await
        .expect("should succeed");

    let body = first_request_body(&server).await;
    assert!(
        body.contains("\"cache_control\":{\"type\":\"ephemeral\"}"),
        "{body}"
    );
}

/// Cache writes surfaced by OpenRouter usage accounting
/// (`prompt_tokens_details.cache_write_tokens`) land on the normalized
/// envelope's `cache_write_tokens` — dropping them hid the 1.25x-billed
/// write volume from the engine's cache telemetry.
#[tokio::test]
async fn complete_surfaces_cache_write_tokens() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":500,\"prompt_tokens_details\":{\"cached_tokens\":200,\"cache_write_tokens\":600}}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = ZaiProvider::new(ApiKey::new("sk-or-test"), "anthropic/claude-sonnet-5")
        .with_base_url(server.uri())
        .with_identity("openrouter", "OpenRouter");
    let result = provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        })
        .await
        .expect("should succeed");

    assert_eq!(result.usage.cached_input_tokens, 200);
    assert_eq!(result.usage.cache_write_tokens, 600);
}

/// DeepSeek's native endpoint reports cache hits as TOP-LEVEL
/// `prompt_cache_hit_tokens` and sends no OpenAI-style
/// `prompt_tokens_details` object at all. Dropping the native field (the
/// prior behavior) pinned the cache stat at 0% and billed every cached
/// token at the full input rate for every DeepSeek run — the same defect
/// class as the OpenRouter CACHE-0% bug, one provider over.
#[tokio::test]
async fn deepseek_native_cache_hit_tokens_surface_as_cached_input() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":100,\"prompt_cache_hit_tokens\":300,\"prompt_cache_miss_tokens\":700}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = ZaiProvider::new(ApiKey::new("sk-ds-test"), "deepseek-chat")
        .with_base_url(server.uri())
        .with_identity("deepseek", "DeepSeek");
    let result = provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        })
        .await
        .expect("should succeed");

    assert_eq!(result.usage.input_tokens, 1000);
    assert_eq!(result.usage.cached_input_tokens, 300);
}

#[tokio::test]
async fn zai_identity_maps_reasoning_to_glm_thinking_object() {
    let server = MockServer::start().await;
    mock_ok(&server).await;

    let provider =
        ZaiProvider::new(ApiKey::new("sk-test-zai"), "glm-5.2").with_base_url(server.uri());
    let req = |reasoning| CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning,
        params: None,
    };
    provider.complete(req(Some(true))).await.expect("on");
    provider.complete(req(Some(false))).await.expect("off");

    let requests = server.received_requests().await.expect("recorded requests");
    let on = String::from_utf8_lossy(&requests[0].body);
    let off = String::from_utf8_lossy(&requests[1].body);
    assert!(on.contains("\"thinking\":{\"type\":\"enabled\"}"), "{on}");
    assert!(
        off.contains("\"thinking\":{\"type\":\"disabled\"}"),
        "{off}"
    );
}

#[tokio::test]
async fn openrouter_identity_maps_reasoning_to_the_gateway_object() {
    let server = MockServer::start().await;
    mock_ok(&server).await;

    let provider = ZaiProvider::new(ApiKey::new("sk-or-test"), "openrouter/auto")
        .with_base_url(server.uri())
        .with_identity("openrouter", "OpenRouter");
    let req = |reasoning, effort| CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: None,
        effort,
        tools: vec![],
        reasoning,
        params: None,
    };
    // Pinned effort (reasoning not suppressed) → {"effort": …}.
    provider
        .complete(req(None, Some(ReasoningEffort::Xhigh)))
        .await
        .expect("effort");
    // Explicit off wins even over a pinned effort → {"enabled": false}.
    provider
        .complete(req(Some(false), Some(ReasoningEffort::High)))
        .await
        .expect("off");
    // Bare on with no effort → {"enabled": true}.
    provider.complete(req(Some(true), None)).await.expect("on");

    let requests = server.received_requests().await.expect("recorded requests");
    let bodies: Vec<String> = requests
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    // Xhigh collapses to "high", the top tier OpenRouter models.
    assert!(
        bodies[0].contains("\"reasoning\":{\"effort\":\"high\"}"),
        "{}",
        bodies[0]
    );
    assert!(
        bodies[1].contains("\"reasoning\":{\"enabled\":false}"),
        "{}",
        bodies[1]
    );
    assert!(
        bodies[2].contains("\"reasoning\":{\"enabled\":true}"),
        "{}",
        bodies[2]
    );
    // GLM's thinking object is Z.ai-only; OpenRouter never sees it.
    assert!(!bodies[0].contains("thinking"), "{}", bodies[0]);
}

/// Identities other than Z.ai and OpenRouter (xAI, DeepSeek, local, …)
/// have no known reasoning field on this dialect: the hint is ignored
/// rather than guessed at — an unknown key risks a hard 400.
#[tokio::test]
async fn other_identities_ignore_reasoning_entirely() {
    let server = MockServer::start().await;
    mock_ok(&server).await;

    let provider = ZaiProvider::new(ApiKey::new("xai-test"), "grok-4")
        .with_base_url(server.uri())
        .with_identity("xai", "xAI");
    provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: Some(ReasoningEffort::High),
            tools: vec![],
            reasoning: Some(true),
            params: None,
        })
        .await
        .expect("should succeed");

    let body = first_request_body(&server).await;
    assert!(!body.contains("thinking"), "{body}");
    assert!(!body.contains("reasoning"), "{body}");
    // cache_control is OpenRouter-only, same 400 risk as the fields above.
    assert!(!body.contains("cache_control"), "{body}");
}
