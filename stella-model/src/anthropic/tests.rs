use super::*;
use stella_protocol::MessageRole;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn user_attachments_map_to_image_and_document_blocks_before_text() {
    use stella_protocol::{Attachment, AttachmentSource};
    let attachments = vec![
        Attachment {
            name: "shot.png".into(),
            media_type: "image/png".into(),
            byte_len: 3,
            source: AttachmentSource::Data {
                base64: "aW1n".into(), // "img"
            },
        },
        Attachment {
            name: "spec.pdf".into(),
            media_type: "application/pdf".into(),
            byte_len: 3,
            source: AttachmentSource::Data {
                base64: "cGRm".into(), // "pdf"
            },
        },
        Attachment {
            name: "clip.mp4".into(),
            media_type: "video/mp4".into(),
            byte_len: 3,
            source: AttachmentSource::Data {
                base64: "dmlk".into(), // "vid"
            },
        },
    ];
    let messages = vec![CompletionMessage::user_with_attachments(
        "what do you see?",
        attachments,
    )];
    let (_, mapped) = to_anthropic_messages(&messages);
    assert_eq!(mapped.len(), 1);
    let json = serde_json::to_value(&mapped[0]).unwrap();
    let blocks = json["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 4, "{json}");
    assert_eq!(blocks[0]["type"], "image");
    assert_eq!(blocks[0]["source"]["type"], "base64");
    assert_eq!(blocks[0]["source"]["media_type"], "image/png");
    assert_eq!(blocks[0]["source"]["data"], "aW1n");
    assert_eq!(blocks[1]["type"], "document");
    assert_eq!(blocks[1]["source"]["media_type"], "application/pdf");
    // Video is not natively ingestible on this dialect: degrade note.
    assert_eq!(blocks[2]["type"], "text");
    let note = blocks[2]["text"].as_str().unwrap();
    assert!(note.contains("clip.mp4"), "{note}");
    // The typed text comes after the media blocks.
    assert_eq!(blocks[3]["type"], "text");
    assert_eq!(blocks[3]["text"], "what do you see?");
}

#[test]
fn attachment_only_user_message_survives_without_text() {
    use stella_protocol::{Attachment, AttachmentSource};
    let messages = vec![CompletionMessage::user_with_attachments(
        "",
        vec![Attachment {
            name: "shot.png".into(),
            media_type: "image/png".into(),
            byte_len: 3,
            source: AttachmentSource::Data {
                base64: "aW1n".into(),
            },
        }],
    )];
    let (_, mapped) = to_anthropic_messages(&messages);
    assert_eq!(mapped.len(), 1, "attachment-only message must not drop");
    let json = serde_json::to_value(&mapped[0]).unwrap();
    let blocks = json["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "image");
}

/// The reported failure's happy path: a multi-kilobyte `write_file` tool
/// call whose argument JSON is split across HUNDREDS of `input_json_delta`
/// fragments (and thus many SSE events / network chunks) must reassemble
/// to the exact JSON object — never a truncated buffer or `null`. Proves
/// the assembly itself is sound for large payloads; the null only ever
/// came from genuine truncation, handled by the test below.
#[tokio::test]
async fn complete_reassembles_a_large_multi_fragment_tool_call() {
    let server = MockServer::start().await;
    // A realistic large `write_file` content field: several KB, with the
    // newlines, quotes and backslashes a real README carries.
    let mut content = String::new();
    for i in 0..200 {
        content.push_str(&format!(
            "## Section {i}\n\nSome \"quoted\" text with a backslash \\ and a tab\there.\n\n"
        ));
    }
    let full_input = serde_json::json!({"path": "README.md", "content": content});
    let full_input_json = serde_json::to_string(&full_input).unwrap();

    // Fragment the JSON at arbitrary byte-ish boundaries (chars), exactly
    // as Anthropic streams `partial_json` — including splits mid-escape.
    let chars: Vec<char> = full_input_json.chars().collect();
    let mut body = String::from(
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":40,\"output_tokens\":0}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"write_file\"}}\n\n",
    );
    for piece in chars.chunks(29) {
        let frag: String = piece.iter().collect();
        let escaped = serde_json::to_string(&frag).unwrap();
        body.push_str("event: content_block_delta\n");
        body.push_str(&format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{escaped}}}}}\n\n"
        ));
    }
    body.push_str(
        "event: message_delta\n\
         data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":0,\"output_tokens\":15}}\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("write the readme")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![stella_protocol::tool::ToolSchema {
            name: "write_file".into(),
            description: "Write a file".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: false,
        }],
        reasoning: None,
        params: None,
    };

    let result = provider.complete(req).await.expect("should succeed");
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(
        result.tool_calls[0].input, full_input,
        "large multi-fragment tool input must reassemble to the exact JSON, not null"
    );
}

/// The reported bug's failure path: the model starts a large `write_file`
/// tool call but the stream stops at the output-token limit MID-JSON —
/// `partial_json` is an unterminated fragment and `message_delta` carries
/// `stop_reason: "max_tokens"`. The adapter must NOT silently emit a
/// tool call with `null` input (which the driver's repair loop can never
/// fix, producing the observed "stuck-loop"); it must surface a clear,
/// non-retryable Terminal error naming the truncation and carrying a raw
/// snippet of what was accumulated.
#[tokio::test]
async fn complete_surfaces_a_truncated_tool_call_instead_of_silent_null() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"write_file\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"README.md\\\",\\\"content\\\":\\\"# Title\\\\nlots of\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":8192}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("write the readme")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![stella_protocol::tool::ToolSchema {
            name: "write_file".into(),
            description: "Write a file".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: false,
        }],
        reasoning: None,
        params: None,
    };

    let err = provider.complete(req).await.unwrap_err();
    // A truncated-at-max_tokens tool call is a terminal, non-retryable
    // failure — retrying with the same output budget re-truncates.
    assert!(
        matches!(err, ProviderError::Terminal(_)),
        "expected Terminal, got {err:?}"
    );
    assert!(!err.is_retryable());
    let msg = err.to_string();
    assert!(msg.contains("write_file"), "names the tool: {msg}");
    assert!(msg.contains("max_tokens"), "names the cause: {msg}");
    // The raw accumulated snippet must be surfaced, never dropped to null.
    assert!(msg.contains("README.md"), "carries a raw snippet: {msg}");
}

/// Broken JSON on a call that *finished* streaming (the stream did not
/// stop at `max_tokens`) is the model's own malformed output — the
/// adapter must keep the `Value::Null` repair sentinel the driver's
/// documented repair loop consumes, not abort the turn.
#[tokio::test]
async fn malformed_but_complete_tool_json_falls_back_to_the_null_repair_sentinel() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"edit_file\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": not json,}\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":40}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
    let result = provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("edit the file")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        })
        .await
        .expect("a repairable malformed call must not abort the turn");
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].input, serde_json::Value::Null);
}

/// `max_tokens` landing between a tool call's `content_block_start` and
/// its first `input_json_delta` must surface the same terminal truncation
/// error as a mid-payload cut — never a silent `{}` that executes with
/// missing parameters and re-enters the retry-retruncate loop.
#[tokio::test]
async fn a_tool_call_with_no_arguments_cut_at_max_tokens_is_terminal() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"write_file\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":8192}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
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
    assert!(msg.contains("max_tokens"), "names the cause: {msg}");
}

/// With parallel tool calls, the truncation error must blame the call the
/// token limit actually cut (the last block started) — an earlier call
/// whose complete-but-broken JSON is the model's own output must not be
/// misreported as truncated.
#[tokio::test]
async fn truncation_is_blamed_on_the_call_that_was_cut_not_an_earlier_broken_one() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"edit_file\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": broken,}\"}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"write_file\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"README\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":8192}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
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
        !msg.contains("edit_file"),
        "must not blame the earlier repairable call: {msg}"
    );
}

#[test]
fn to_anthropic_messages_hoists_system_and_maps_roles() {
    let messages = vec![
        CompletionMessage::system("You are a coding agent."),
        CompletionMessage::user("Fix the bug."),
    ];
    let (system, mapped) = to_anthropic_messages(&messages);
    assert_eq!(system, Some("You are a coding agent.".to_string()));
    assert_eq!(mapped.len(), 1);
    assert_eq!(mapped[0].role, "user");
}

/// The Anthropic API rejects empty / whitespace-only text blocks with a
/// 400, and since history is replayed every turn, one such block bricks
/// the session forever. `to_anthropic_messages` must never emit one, and
/// a message that would carry only such a block is dropped rather than
/// padded.
#[test]
fn empty_and_whitespace_text_blocks_are_never_emitted() {
    use stella_protocol::ToolCall;
    let messages = vec![
        CompletionMessage::system("sys"),
        CompletionMessage::user("real question"),
        // Assistant turn with only whitespace text plus a tool call:
        // the whitespace text must be dropped, the tool_use kept.
        CompletionMessage {
            role: MessageRole::Assistant,
            content: "   \n\t ".into(),
            tool_calls: vec![ToolCall {
                call_id: "toolu_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "a"}),
            }],
            tool_results: vec![],
            attachments: Vec::new(),
        },
        // A fully content-less assistant turn — must vanish entirely,
        // NOT become an empty text block.
        CompletionMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![],
            attachments: Vec::new(),
        },
        // An empty user turn — dropped, never an empty text block.
        CompletionMessage {
            role: MessageRole::User,
            content: "  ".into(),
            tool_calls: vec![],
            tool_results: vec![],
            attachments: Vec::new(),
        },
    ];
    let (_, mapped) = to_anthropic_messages(&messages);

    // Not one emitted text block is empty or whitespace-only.
    for m in &mapped {
        for block in &m.content {
            if let AnthropicContentBlock::Text { text, .. } = block {
                assert!(
                    !text.trim().is_empty(),
                    "emitted an empty/whitespace text block: {text:?}"
                );
            }
        }
    }
    // The whitespace-plus-tool assistant turn kept exactly its tool_use.
    let assistant = mapped
        .iter()
        .find(|m| m.role == "assistant")
        .expect("the tool-calling assistant turn survives");
    assert_eq!(assistant.content.len(), 1);
    assert!(matches!(
        assistant.content[0],
        AnthropicContentBlock::ToolUse { .. }
    ));
    // The content-less assistant turn and the empty user turn are gone.
    assert_eq!(
        mapped.iter().filter(|m| m.role == "assistant").count(),
        1,
        "the content-less assistant turn is dropped, not padded"
    );
    // Only the real user question survives among user-role messages.
    let user_texts: Vec<&AnthropicMessage> = mapped.iter().filter(|m| m.role == "user").collect();
    assert_eq!(user_texts.len(), 1);
}

/// Prompt caching is opt-in per request: the serialized body must carry
/// both breakpoints — one on the system block (tools+system tier), one
/// on the final content block of the last message (conversation tail) —
/// or the API silently caches nothing and every turn re-pays the full
/// replayed prefix. And it must carry them at BLOCK level only: a
/// top-level `cache_control` request field is an unknown parameter the
/// live API rejects with a 400, killing every Anthropic call.
#[test]
fn request_serializes_both_cache_breakpoints() {
    let mut messages = vec![
        AnthropicMessage {
            role: "user",
            content: vec![AnthropicContentBlock::Text {
                text: "earlier turn".into(),
                cache_control: None,
            }],
        },
        AnthropicMessage {
            role: "user",
            content: vec![AnthropicContentBlock::Text {
                text: "hi".into(),
                cache_control: None,
            }],
        },
    ];
    stamp_tail_cache_breakpoint(&mut messages);
    let body = AnthropicRequest {
        model: "claude-fable-5",
        max_tokens: 64,
        system: Some(vec![AnthropicSystemBlock {
            kind: "text",
            text: "You are a coding agent.",
            cache_control: EPHEMERAL_CACHE,
        }]),
        messages,
        stream: true,
        temperature: None,
        top_p: None,
        top_k: None,
        thinking: None,
        output_config: None,
        tools: vec![],
    };
    let v = serde_json::to_value(&body).expect("request serializes");
    assert_eq!(v["system"][0]["type"], "text");
    assert_eq!(v["system"][0]["cache_control"]["type"], "ephemeral");
    // Tail breakpoint on the LAST block of the LAST message only.
    assert_eq!(
        v["messages"][1]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert!(
        v["messages"][0]["content"][0]
            .get("cache_control")
            .is_none(),
        "only the tail block carries a breakpoint"
    );
    assert!(
        v.get("cache_control").is_none(),
        "top-level cache_control is not a Messages API parameter"
    );
    // The optional params/thinking fields must vanish when unset — the
    // byte-stability contract for requests without overrides.
    let body_json = serde_json::to_string(&v).unwrap();
    for key in ["top_p", "top_k", "thinking", "output_config"] {
        assert!(!body_json.contains(key), "unexpected `{key}`: {body_json}");
    }
}

#[test]
fn uses_adaptive_thinking_classifies_current_vs_legacy_models() {
    // Current generation (4.6+, the 5-family) → adaptive shape. Unknown /
    // future models default to modern so the next launch doesn't silently
    // 400 on the legacy `budget_tokens` shape (how this bug shipped).
    for model in [
        "claude-fable-5",
        "claude-mythos-5",
        "claude-sonnet-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-fable-6",
        "claude-opus-5",
        "some-future-model",
    ] {
        assert!(
            uses_adaptive_thinking(model),
            "expected adaptive shape for `{model}`"
        );
    }
    // Legacy generations (≤ 4.5 / 3.x / 2.x) → enabled+budget_tokens shape.
    for model in [
        "claude-opus-4-5",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
        "claude-opus-4-5-20251101",
        "claude-opus-4-1",
        "claude-opus-4-0",
        "claude-opus-4-20250514",
        "claude-3-5-haiku-20241022",
        "claude-3-opus-20240229",
        "claude-2.1",
    ] {
        assert!(
            !uses_adaptive_thinking(model),
            "expected legacy shape for `{model}`"
        );
    }
}

/// The reported regression: a current-generation model (`claude-fable-5`)
/// with reasoning on must send the ADAPTIVE thinking shape plus
/// `output_config.effort`, and must NOT send `budget_tokens` or any sampling
/// parameter — the live API rejects all three with a 400. Also pins the
/// raised default `max_tokens` (adaptive thinking shares the answer's budget,
/// so the old 4096 default would truncate a max-effort turn).
#[tokio::test]
async fn fable5_sends_adaptive_shape_and_drops_budget_tokens_and_sampling() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
    provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("say ok")],
            max_output_tokens: None,
            // Passed by the caller but MUST be dropped on this model family.
            temperature: Some(0.0),
            effort: Some(stella_protocol::ReasoningEffort::Max),
            tools: vec![],
            reasoning: Some(true),
            params: None,
        })
        .await
        .expect("adaptive request should match and stream a reply");

    let requests = server
        .received_requests()
        .await
        .expect("mock server records requests");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    assert_eq!(body["thinking"]["type"], "adaptive", "{body}");
    assert!(
        body["thinking"].get("budget_tokens").is_none(),
        "budget_tokens must not be sent on current models: {body}"
    );
    assert_eq!(body["output_config"]["effort"], "max", "{body}");
    assert!(
        body.get("temperature").is_none(),
        "sampling params 400 on current models and must be dropped: {body}"
    );
    assert_eq!(
        body["max_tokens"], 32_000,
        "un-capped adaptive reasoning turn gets the raised default: {body}"
    );
}

/// Legacy models keep the old contract: `{type:"enabled",budget_tokens}` and
/// no `output_config` (which they reject).
#[tokio::test]
async fn legacy_model_still_sends_budget_tokens_and_no_output_config() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-haiku-4-5")
        .with_base_url(server.uri());
    provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("say ok")],
            max_output_tokens: None,
            temperature: None,
            effort: Some(stella_protocol::ReasoningEffort::High),
            tools: vec![],
            reasoning: Some(true),
            params: None,
        })
        .await
        .expect("legacy request should stream a reply");

    let requests = server
        .received_requests()
        .await
        .expect("mock server records requests");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    assert_eq!(body["thinking"]["type"], "enabled", "{body}");
    assert!(
        body["thinking"]["budget_tokens"].as_u64().unwrap() >= 1024,
        "legacy models still use budget_tokens: {body}"
    );
    assert!(
        body.get("output_config").is_none(),
        "output_config is rejected by legacy models: {body}"
    );
}

#[tokio::test]
async fn complete_streams_and_aggregates_text_deltas_from_a_mock_server() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"lo!\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":12,\"output_tokens\":2}}\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-test-anthropic"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test-anthropic"), "claude-fable-5")
        .with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![
            CompletionMessage::system("system"),
            CompletionMessage::user("say hello"),
        ],
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
    assert_eq!(result.usage.input_tokens, 12);
    assert_eq!(result.usage.output_tokens, 2);
    assert_eq!(result.model, "claude-fable-5");
}

/// Cache accounting: `message_start` reports `cache_read_input_tokens`
/// and `cache_creation_input_tokens` separately from `input_tokens`.
/// Reads fold into `input_tokens` (subset invariant) and surface as
/// `cached_input_tokens`; writes surface as `cache_write_tokens` WITHOUT
/// folding in — the catalog has no cache-write rate, so pricing them as
/// plain input would be wrong in the other direction (issue #97).
#[tokio::test]
async fn complete_reports_cache_write_tokens_from_the_usage_envelope() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":0,\"cache_read_input_tokens\":900,\"cache_creation_input_tokens\":650}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
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
    // Reads folded in: 100 fresh + 900 read-from-cache.
    assert_eq!(result.usage.input_tokens, 1_000);
    assert_eq!(result.usage.cached_input_tokens, 900);
    // Writes surfaced, not folded: input_tokens stays 1_000.
    assert_eq!(result.usage.cache_write_tokens, 650);
    assert_eq!(result.usage.output_tokens, 7);
}

#[tokio::test]
async fn complete_reassembles_a_streamed_tool_use_block() {
    let server = MockServer::start().await;
    // Anthropic streams tool input as `input_json_delta` fragments after
    // a `content_block_start` announces the tool_use block.
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":40,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"src/lib.rs\\\"}\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":0,\"output_tokens\":15}}\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("read src/lib.rs")],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![stella_protocol::tool::ToolSchema {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: false,
        }],
        reasoning: None,
        params: None,
    };

    let result = provider.complete(req).await.expect("should succeed");
    assert_eq!(result.tool_calls.len(), 1);
    let call = &result.tool_calls[0];
    assert_eq!(call.call_id, "toolu_1");
    assert_eq!(call.name, "read_file");
    assert_eq!(call.input, serde_json::json!({"path": "src/lib.rs"}));
    assert_eq!(result.usage.input_tokens, 40);
    assert_eq!(result.usage.output_tokens, 15);
}

/// Shared with the zai tests in spirit: records every announcement so a
/// test can compare them against the final `CompletionResult`.
struct RecordingObserver {
    calls: std::sync::Mutex<Vec<stella_protocol::ToolCall>>,
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
    fn tool_call_streamed(&self, call: &stella_protocol::ToolCall) {
        self.calls.lock().unwrap().push(call.clone());
    }
    fn text_delta(&self, delta: &str) {
        self.deltas.lock().unwrap().push(delta.to_string());
    }
}

#[tokio::test]
async fn complete_observed_streams_answer_deltas_in_order_never_thinking() {
    let server = MockServer::start().await;
    // Answer fragments interleaved with a thinking delta: the observer
    // must see exactly the user-visible fragments, in stream order —
    // thinking deltas parse as `Other` and never reach it.
    let sse_body = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me think\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo!\"}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
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
        "answer fragments only, in order — thinking excluded"
    );
    assert_eq!(
        result.text, "Hello!",
        "the committed text is the announced deltas' concatenation"
    );
}

#[tokio::test]
async fn complete_observed_announces_a_tool_call_at_its_block_stop() {
    let server = MockServer::start().await;
    // The tool_use block CLOSES (content_block_stop) while the message
    // continues with a text block — the exact window speculation exists
    // to exploit.
    let sse_body = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"reading it now\"}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("read src/lib.rs")],
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

    let announced = observer.calls.lock().unwrap();
    assert_eq!(announced.len(), 1, "exactly one announcement per block");
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(
        announced[0], result.tool_calls[0],
        "an announced call must be identical to its committed twin — \
         harvest matches by exact equality"
    );
    assert_eq!(
        announced[0].input,
        serde_json::json!({"path": "src/lib.rs"})
    );
}

#[tokio::test]
async fn complete_observed_never_announces_a_block_whose_json_is_broken() {
    let server = MockServer::start().await;
    // The block closes but its accumulated JSON does not parse — the
    // end-of-stream repair path owns it; speculation must never see it.
    let sse_body = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": not json,}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("go")],
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
        .expect("broken JSON on a finished block is the repair sentinel, not an error");
    assert!(
        observer.calls.lock().unwrap().is_empty(),
        "unparseable input must not be announced"
    );
    // The committed call still carries the Null repair sentinel.
    assert_eq!(result.tool_calls.len(), 1);
    assert!(result.tool_calls[0].input.is_null());
}

#[test]
fn to_anthropic_messages_frames_tool_results_as_user_blocks() {
    use stella_protocol::{ToolCall, ToolOutput, ToolResult};
    let messages = vec![
        CompletionMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                call_id: "toolu_9".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
            tool_results: vec![],
            attachments: Vec::new(),
        },
        CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: "toolu_9".into(),
                output: ToolOutput::Error {
                    message: "command failed".into(),
                },
            }],
            attachments: Vec::new(),
        },
    ];
    let (_, mapped) = to_anthropic_messages(&messages);
    assert_eq!(mapped.len(), 2);
    assert_eq!(mapped[0].role, "assistant");
    assert_eq!(mapped[1].role, "user");
    // The tool result block must carry the id and the error flag.
    match &mapped[1].content[0] {
        AnthropicContentBlock::ToolResult {
            tool_use_id,
            is_error,
            ..
        } => {
            assert_eq!(tool_use_id, "toolu_9");
            assert!(is_error);
        }
        other => panic!("expected tool_result block, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_maps_401_to_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("bad-key"), "claude-fable-5")
        .with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![CompletionMessage {
            role: MessageRole::User,
            content: "hi".into(),
            tool_calls: vec![],
            tool_results: vec![],
            attachments: Vec::new(),
        }],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let err = provider.complete(req).await.unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)));
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn complete_maps_403_to_auth_error() {
    // A permission-denied key (403 permission_error — e.g. a key without
    // access to the requested model) is a credential failure the user
    // must fix, not a generic terminal error. Regression for the drift
    // where only 401 was mapped to Auth here while sibling adapters
    // mapped 401|403.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(403).set_body_string("permission_error"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("limited-key"), "claude-fable-5")
        .with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![CompletionMessage {
            role: MessageRole::User,
            content: "hi".into(),
            tool_calls: vec![],
            tool_results: vec![],
            attachments: Vec::new(),
        }],
        max_output_tokens: None,
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let err = provider.complete(req).await.unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)));
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn complete_honors_retry_after_header_on_a_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "3")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("test-key"), "claude-fable-5")
        .with_base_url(server.uri());

    let req = CompletionRequest {
        messages: vec![CompletionMessage {
            role: MessageRole::User,
            content: "hi".into(),
            tool_calls: vec![],
            tool_results: vec![],
            attachments: Vec::new(),
        }],
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
            assert_eq!(retry_after_ms, Some(3_000), "Retry-After: 3s → 3000ms");
        }
        other => panic!("expected RateLimited with a retry hint, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_computes_nonzero_cost_from_catalog_pricing() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":0}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":0,\"output_tokens\":500}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
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
    let expected = Catalog::seed()
        .resolve("claude-fable-5")
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

#[tokio::test]
async fn complete_forwards_temperature_to_the_request_body() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    );
    // Legacy models accept sampling params; the mock only matches if the
    // serialized body carries the temperature — proving it isn't dropped on
    // that path. (Current models reject temperature — covered separately by
    // `fable5_sends_adaptive_shape_and_drops_budget_tokens_and_sampling`.)
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains("\"temperature\":0.3"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-opus-4-5")
        .with_base_url(server.uri());
    let req = CompletionRequest {
        messages: vec![CompletionMessage::user("hi")],
        max_output_tokens: None,
        temperature: Some(0.3),
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };

    let result = provider.complete(req).await.expect("should succeed");
    assert_eq!(result.text, "ok");
}

#[tokio::test]
async fn complete_maps_5xx_to_retryable_transport() {
    let server = MockServer::start().await;
    // 529 is Anthropic's "overloaded" load-shedding status.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
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
    assert!(err.is_retryable(), "5xx/529 must be retryable");
}

#[tokio::test]
async fn complete_returns_err_on_mid_stream_error_event_not_truncated_ok() {
    let server = MockServer::start().await;
    // Text arrives, then an `error` event: the turn must fail, not return
    // the partial "Hel".
    let sse_body = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"server overloaded\"}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-fable-5")
        .with_base_url(server.uri());
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
    // overloaded_error ⇒ retryable Transport.
    assert!(matches!(err, ProviderError::Transport(_)));
    assert!(err.is_retryable());
}

#[test]
fn thinking_budgets_map_effort_tiers_and_default_to_medium() {
    use stella_protocol::ReasoningEffort::*;
    assert_eq!(thinking_budget_tokens(Some(Low)), 2_048);
    assert_eq!(thinking_budget_tokens(Some(Medium)), 8_192);
    assert_eq!(thinking_budget_tokens(None), 8_192, "None defaults Medium");
    assert_eq!(thinking_budget_tokens(Some(High)), 16_384);
    assert_eq!(thinking_budget_tokens(Some(Xhigh)), 32_768);
    assert_eq!(thinking_budget_tokens(Some(Max)), 49_152);
}

/// Minimal happy-path SSE body for tests that only inspect the request.
const OK_SSE: &str = concat!(
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
);

async fn mock_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(OK_SSE, "text/event-stream"))
        .mount(server)
        .await;
}

async fn first_request_body(server: &MockServer) -> String {
    let requests = server.received_requests().await.expect("recorded requests");
    String::from_utf8_lossy(&requests[0].body).into_owned()
}

// Legacy models accept sampling params, so top_p/top_k forward on that path.
#[tokio::test]
async fn generation_params_forward_top_p_and_top_k_and_drop_the_rest() {
    use stella_protocol::GenerationParams;
    let server = MockServer::start().await;
    mock_ok(&server).await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-opus-4-5")
        .with_base_url(server.uri());
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
                // No Messages API slot — silently dropped, never a 400.
                frequency_penalty: Some(0.5),
                presence_penalty: Some(0.25),
                repetition_penalty: Some(1.1),
                seed: Some(7),
                verbosity: None,
                service_tier: None,
            }),
        })
        .await
        .expect("should succeed");

    let body = first_request_body(&server).await;
    assert!(body.contains("\"top_p\":0.9"), "{body}");
    assert!(body.contains("\"top_k\":40"), "{body}");
    for dropped in [
        "frequency_penalty",
        "presence_penalty",
        "repetition_penalty",
        "seed",
    ] {
        assert!(!body.contains(dropped), "`{dropped}` leaked into: {body}");
    }
}

/// Legacy path — `reasoning: Some(true)` with no caller output cap: the
/// budget maps from effort, the defaulted max_tokens rises to budget + 8192
/// so thinking can't consume the whole output allowance, and temperature is
/// omitted entirely (the API rejects temperature != 1 with thinking).
#[tokio::test]
async fn reasoning_true_enables_thinking_raises_max_tokens_and_omits_temperature() {
    let server = MockServer::start().await;
    mock_ok(&server).await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-opus-4-5")
        .with_base_url(server.uri());
    provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("think hard")],
            max_output_tokens: None,
            temperature: Some(0.0), // the engine default that would 400
            effort: Some(stella_protocol::ReasoningEffort::Max),
            tools: vec![],
            reasoning: Some(true),
            params: None,
        })
        .await
        .expect("should succeed");

    let body = first_request_body(&server).await;
    assert!(
        body.contains("\"thinking\":{\"type\":\"enabled\",\"budget_tokens\":49152}"),
        "{body}"
    );
    assert!(
        body.contains("\"max_tokens\":57344"),
        "49152 + 8192: {body}"
    );
    assert!(!body.contains("temperature"), "{body}");
}

/// Legacy path — a caller-set max_tokens is honored, and the budget clamps
/// under it: at most max_tokens - 1024 (the API requires budget < max_tokens).
#[tokio::test]
async fn thinking_budget_clamps_under_a_caller_set_max_tokens() {
    let server = MockServer::start().await;
    mock_ok(&server).await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-opus-4-5")
        .with_base_url(server.uri());
    provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("think")],
            max_output_tokens: Some(4096),
            temperature: None,
            effort: None, // Medium default: 8192, which exceeds the cap
            tools: vec![],
            reasoning: Some(true),
            params: None,
        })
        .await
        .expect("should succeed");

    let body = first_request_body(&server).await;
    assert!(body.contains("\"max_tokens\":4096"), "{body}");
    assert!(
        body.contains("\"budget_tokens\":3072"),
        "4096 - 1024: {body}"
    );
}

/// Legacy path — `Some(false)` and `None` are the same wire shape: no
/// thinking block (thinking is opt-in on this API), temperature forwarded as
/// always.
#[tokio::test]
async fn reasoning_false_sends_no_thinking_block_and_keeps_temperature() {
    let server = MockServer::start().await;
    mock_ok(&server).await;

    let provider = AnthropicProvider::new(ApiKey::new("sk-test"), "claude-opus-4-5")
        .with_base_url(server.uri());
    provider
        .complete(CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: Some(0.3),
            effort: Some(stella_protocol::ReasoningEffort::High),
            tools: vec![],
            reasoning: Some(false),
            params: None,
        })
        .await
        .expect("should succeed");

    let body = first_request_body(&server).await;
    assert!(!body.contains("thinking"), "{body}");
    assert!(!body.contains("budget_tokens"), "{body}");
    assert!(body.contains("\"temperature\":0.3"), "{body}");
    assert!(body.contains("\"max_tokens\":4096"), "default cap: {body}");
}
