//! Amazon Bedrock adapter — the Converse API
//! (`07-model-matrix.md` §2: "Bedrock | AWS chain | Converse/ConverseStream |
//! catalog-driven … Model Garden by ARN"). Two deliberate v1 scopings, both
//! consistent with postures already recorded elsewhere in this crate:
//!
//! - **Non-streaming `Converse`, not `ConverseStream`.** The streaming
//!   variant speaks `application/vnd.amazon.eventstream` — a binary framing
//!   with per-message CRC32 prologues, not SSE — an entirely separate
//!   transport decoder. `Provider::complete` aggregates internally either
//!   way (no caller renders partial tokens yet), so `Converse` returns an
//!   identical `CompletionResult`; the event-stream decoder lands when the
//!   TUI actually streams partial output.
//! - **Explicit credentials, not the full AWS chain.** The adapter takes
//!   access key / secret / optional session token directly; `stella-cli`
//!   resolves them from the standard `AWS_ACCESS_KEY_ID` /
//!   `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` env vars. Profile files,
//!   SSO, and IMDS are the "provider-native config" step the credential
//!   chain doc (`credential.rs`) already records as deferred alongside this
//!   adapter.
//!
//! Requests are signed with SigV4 implemented in [`sigv4`] below — pure
//! functions over explicit inputs, pinned by golden vectors generated from
//! botocore's reference implementation (see `sigv4::tests`), because
//! request signing is exactly the kind of code that "looks right" while
//! producing signatures a real endpoint rejects.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stella_protocol::{
    CompletionMessage, CompletionRequest, CompletionResult, CompletionUsage, MessageRole,
    ProviderError, ToolCall,
};

use crate::catalog::{Catalog, Pricing};
use crate::credential::ApiKey;
use crate::http;
use crate::provider::Provider;

pub struct BedrockProvider {
    client: reqwest::Client,
    access_key: ApiKey,
    secret_key: ApiKey,
    session_token: Option<ApiKey>,
    region: String,
    model: String,
    base_url_override: Option<String>,
    /// List pricing for `model`, resolved from the catalog at construction so
    /// `cost_usd` is computed on the real request path — never a hard-coded
    /// zero (which would silently disable budget enforcement for Bedrock).
    pricing: Option<Pricing>,
}

impl BedrockProvider {
    /// Build an adapter for `model` (a catalog-resolved Bedrock model id or
    /// inference-profile id, e.g. `us.anthropic.claude-sonnet-4-5-20250929-v1:0`)
    /// in `region`.
    pub fn new(
        access_key: ApiKey,
        secret_key: ApiKey,
        session_token: Option<ApiKey>,
        region: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let model = model.into();
        let pricing = Catalog::seed()
            .resolve_for("bedrock", &model)
            .ok()
            .map(|e| e.pricing);
        Self {
            client: http::client(),
            access_key,
            secret_key,
            session_token,
            region: region.into(),
            model,
            base_url_override: None,
            pricing,
        }
    }

    /// Override the scheme+host — used by conformance tests against a mock
    /// server, and by anyone routing through a private proxy. Signing is
    /// unaffected beyond the `host` header (a mock server never verifies
    /// signatures; a proxy forwards them).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url_override = Some(base_url.into());
        self
    }

    /// The wire path for this model's Converse call. Bedrock model ids
    /// routinely contain `:` (version suffixes) and ARNs contain `/` — both
    /// must be percent-encoded in the request path, exactly as the AWS SDKs
    /// send them, and the canonical URI signs the same encoded form.
    fn wire_path(&self) -> String {
        format!("/model/{}/converse", sigv4::uri_encode(&self.model))
    }

    fn base_url(&self) -> String {
        match &self.base_url_override {
            Some(base) => base.clone(),
            None => format!("https://bedrock-runtime.{}.amazonaws.com", self.region),
        }
    }
}

// ── Wire types (Bedrock Converse API) ────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConverseRequest {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<BedrockTextBlock>,
    messages: Vec<BedrockMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inference_config: Option<BedrockInferenceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<BedrockToolConfig>,
}

#[derive(Serialize, Debug)]
struct BedrockTextBlock {
    text: String,
}

#[derive(Serialize, Debug)]
struct BedrockMessage {
    role: &'static str,
    content: Vec<BedrockContentBlock>,
}

/// One content block. Exactly one field is set per block (the wire treats
/// them as a union) — same shape convention as `gemini.rs`'s outbound part.
#[derive(Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct BedrockContentBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use: Option<BedrockToolUse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_result: Option<BedrockToolResult>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct BedrockToolUse {
    tool_use_id: String,
    name: String,
    #[serde(default)]
    input: Value,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct BedrockToolResult {
    tool_use_id: String,
    content: Vec<BedrockTextBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockInferenceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Serialize)]
struct BedrockToolConfig {
    tools: Vec<BedrockToolEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolEntry {
    tool_spec: BedrockToolSpec,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolSpec {
    name: String,
    description: String,
    input_schema: BedrockInputSchema,
}

#[derive(Serialize)]
struct BedrockInputSchema {
    json: Value,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ConverseResponse {
    #[serde(default)]
    output: Option<ConverseOutput>,
    #[serde(default)]
    usage: Option<BedrockUsage>,
}

#[derive(Deserialize, Debug)]
struct ConverseOutput {
    #[serde(default)]
    message: Option<ConverseOutputMessage>,
}

#[derive(Deserialize, Debug)]
struct ConverseOutputMessage {
    #[serde(default)]
    content: Vec<InboundContentBlock>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct InboundContentBlock {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_use: Option<BedrockToolUse>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct BedrockUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

/// Translate the engine's one message list into Converse `system` +
/// `messages`. Dialect rules: system turns hoist into the dedicated
/// `system` array; an assistant's tool calls are `toolUse` blocks on its
/// own message; tool results are `toolResult` blocks on a **user**-role
/// message (Converse's framing for "the environment answered"), each
/// correlated by `toolUseId` and carrying `status: "error"` for failed
/// tools instead of the text-prefix convention the OpenAI dialects use.
fn to_bedrock_messages(
    messages: &[CompletionMessage],
) -> (Vec<BedrockTextBlock>, Vec<BedrockMessage>) {
    let mut system = Vec::new();
    let mut out: Vec<BedrockMessage> = Vec::new();
    for message in messages {
        match message.role {
            MessageRole::System => system.push(BedrockTextBlock {
                text: message.content.clone(),
            }),
            MessageRole::User => out.push(BedrockMessage {
                role: "user",
                content: vec![BedrockContentBlock {
                    text: Some(message.content.clone()),
                    ..Default::default()
                }],
            }),
            MessageRole::Assistant => {
                let mut content = Vec::new();
                if !message.content.is_empty() {
                    content.push(BedrockContentBlock {
                        text: Some(message.content.clone()),
                        ..Default::default()
                    });
                }
                for call in &message.tool_calls {
                    content.push(BedrockContentBlock {
                        tool_use: Some(BedrockToolUse {
                            tool_use_id: call.call_id.clone(),
                            name: call.name.clone(),
                            input: call.input.clone(),
                        }),
                        ..Default::default()
                    });
                }
                if !content.is_empty() {
                    out.push(BedrockMessage {
                        role: "assistant",
                        content,
                    });
                }
            }
            MessageRole::Tool => {
                let content: Vec<BedrockContentBlock> = message
                    .tool_results
                    .iter()
                    .map(|result| {
                        let (text, status) = match &result.output {
                            stella_protocol::ToolOutput::Ok { content } => (content.clone(), None),
                            stella_protocol::ToolOutput::Error { message } => {
                                (message.clone(), Some("error"))
                            }
                        };
                        BedrockContentBlock {
                            tool_result: Some(BedrockToolResult {
                                tool_use_id: result.call_id.clone(),
                                content: vec![BedrockTextBlock { text }],
                                status,
                            }),
                            ..Default::default()
                        }
                    })
                    .collect();
                if !content.is_empty() {
                    out.push(BedrockMessage {
                        role: "user",
                        content,
                    });
                }
            }
        }
    }
    (system, out)
}

fn to_bedrock_tool_config(
    tools: &[stella_protocol::tool::ToolSchema],
) -> Option<BedrockToolConfig> {
    if tools.is_empty() {
        return None;
    }
    Some(BedrockToolConfig {
        tools: tools
            .iter()
            .map(|tool| BedrockToolEntry {
                tool_spec: BedrockToolSpec {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: BedrockInputSchema {
                        json: tool.input_schema.clone(),
                    },
                },
            })
            .collect(),
    })
}

#[async_trait]
impl Provider for BedrockProvider {
    fn id(&self) -> &str {
        "bedrock"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResult, ProviderError> {
        let (system, messages) = to_bedrock_messages(&req.messages);
        let inference_config = if req.max_output_tokens.is_none() && req.temperature.is_none() {
            None
        } else {
            Some(BedrockInferenceConfig {
                max_tokens: req.max_output_tokens,
                temperature: req.temperature,
            })
        };
        let body = ConverseRequest {
            system,
            messages,
            inference_config,
            tool_config: to_bedrock_tool_config(&req.tools),
        };
        let payload =
            serde_json::to_vec(&body).map_err(|e| ProviderError::Malformed(e.to_string()))?;

        let base_url = self.base_url();
        let wire_path = self.wire_path();
        let url = format!("{base_url}{wire_path}");
        let host = sigv4::host_from_url(&url)
            .ok_or_else(|| ProviderError::Transport(format!("unparseable Bedrock URL: {url}")))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| ProviderError::Transport(e.to_string()))?
            .as_secs();
        let amz_date = sigv4::format_amz_date(now as i64);

        let signing = sigv4::sign(&sigv4::SigningInput {
            method: "POST",
            wire_path: &wire_path,
            canonical_query: "",
            host: &host,
            content_type: "application/json",
            payload: &payload,
            region: &self.region,
            service: "bedrock",
            access_key: self.access_key.reveal(),
            secret_key: self.secret_key.reveal(),
            session_token: self.session_token.as_ref().map(|t| t.reveal()),
            amz_date: &amz_date,
        });

        let mut request = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-amz-date", &amz_date)
            .header("authorization", signing.authorization)
            .body(payload);
        if let Some(token) = &self.session_token {
            request = request.header("x-amz-security-token", token.reveal());
        }

        let response = request
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ProviderError::Auth(format!(
                "Bedrock rejected the AWS credentials (HTTP {status})"
            )));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Surface the server's own message and any Retry-After hint rather
            // than a fixed string (matches `zai.rs`), so the driver can honor
            // the provider's stated backoff.
            let retry_after_ms = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|s| s.saturating_mul(1000));
            let text = response.text().await.unwrap_or_default();
            let message = if text.trim().is_empty() {
                "Bedrock throttled the request".to_string()
            } else {
                format!("Bedrock throttled the request: {text}")
            };
            return Err(ProviderError::RateLimited {
                message,
                retry_after_ms,
            });
        }
        // 5xx (500 InternalServerException, 503 ServiceUnavailableException,
        // 529) are transient — retryable Transport, not a permanent Terminal,
        // matching every sibling adapter. Without this a momentary blip aborts
        // the whole turn (Terminal.is_retryable() == false).
        if status.is_server_error() {
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Transport(format!(
                "Bedrock HTTP {status}: {text}"
            )));
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Terminal(format!(
                "Bedrock HTTP {status}: {text}"
            )));
        }

        let parsed: ConverseResponse = response
            .json()
            .await
            .map_err(|e| ProviderError::Malformed(e.to_string()))?;

        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        if let Some(message) = parsed.output.and_then(|o| o.message) {
            for block in message.content {
                if let Some(t) = block.text {
                    text.push_str(&t);
                }
                if let Some(tool_use) = block.tool_use {
                    tool_calls.push(ToolCall {
                        call_id: tool_use.tool_use_id,
                        name: tool_use.name,
                        input: tool_use.input,
                    });
                }
            }
        }
        let usage = parsed.usage.unwrap_or_default();
        let usage = CompletionUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_input_tokens: usage.cache_read_input_tokens,
        };
        let cost_usd = self.pricing.map(|p| p.cost_usd(&usage)).unwrap_or(0.0);

        Ok(CompletionResult {
            text,
            tool_calls,
            usage,
            model: self.model.clone(),
            cost_usd,
        })
    }
}

/// AWS Signature Version 4 over explicit inputs — no ambient clock, no
/// ambient env — so every step is unit-testable and the composed result can
/// be pinned against botocore-generated golden vectors. Only what Converse
/// needs is implemented (POST, empty query string, four signed headers);
/// growing it further should extend the golden-vector suite in lockstep.
pub(crate) mod sigv4 {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest, Sha256};

    type HmacSha256 = Hmac<Sha256>;

    pub(crate) struct SigningInput<'a> {
        pub method: &'a str,
        /// The request path exactly as sent on the wire (already
        /// percent-encoded once — see [`uri_encode`]). Per the SigV4 spec,
        /// every service except S3 signs a canonical URI whose segments are
        /// URI-encoded *twice*; [`sign`] derives that second encoding from
        /// this wire form (`%3A` → `%253A`), matching botocore — the
        /// golden-vector tests pin this exact behavior, which a plausible
        /// "sign what you send" implementation gets wrong.
        pub wire_path: &'a str,
        pub canonical_query: &'a str,
        pub host: &'a str,
        pub content_type: &'a str,
        pub payload: &'a [u8],
        pub region: &'a str,
        pub service: &'a str,
        pub access_key: &'a str,
        pub secret_key: &'a str,
        pub session_token: Option<&'a str>,
        /// `YYYYMMDD'T'HHMMSS'Z'` — from [`format_amz_date`] in production,
        /// fixed in tests.
        pub amz_date: &'a str,
    }

    pub(crate) struct SigningOutput {
        pub authorization: String,
    }

    /// Percent-encode one URI path segment per RFC 3986: unreserved
    /// characters (`A-Z a-z 0-9 - . _ ~`) pass through, everything else —
    /// including `:` in Bedrock version suffixes and `/` in ARNs — becomes
    /// uppercase `%XX`.
    pub(crate) fn uri_encode(segment: &str) -> String {
        let mut out = String::with_capacity(segment.len());
        for byte in segment.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(byte as char)
                }
                other => out.push_str(&format!("%{other:02X}")),
            }
        }
        out
    }

    /// The canonical URI derived from a wire path: the SigV4 second
    /// encoding pass for non-S3 services. `/` passes through (it separates
    /// segments), unreserved bytes pass through, and everything else —
    /// including the `%` of existing escapes — is re-encoded.
    fn canonical_uri_from_wire_path(wire_path: &str) -> String {
        let mut out = String::with_capacity(wire_path.len());
        for byte in wire_path.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                    out.push(byte as char)
                }
                other => out.push_str(&format!("%{other:02X}")),
            }
        }
        out
    }

    /// `host[:port]` from a URL, the exact value reqwest will send as the
    /// `Host` header (port included only when non-default) — the canonical
    /// headers must sign precisely what goes on the wire.
    pub(crate) fn host_from_url(url: &str) -> Option<String> {
        let parsed: reqwest::Url = url.parse().ok()?;
        let host = parsed.host_str()?;
        match parsed.port() {
            Some(port) => Some(format!("{host}:{port}")),
            None => Some(host.to_string()),
        }
    }

    /// Format seconds-since-epoch as SigV4's `YYYYMMDD'T'HHMMSS'Z'`.
    /// Implemented directly (days-to-civil-date, Howard Hinnant's
    /// algorithm) rather than pulling a date crate for one format.
    pub(crate) fn format_amz_date(epoch_secs: i64) -> String {
        let days = epoch_secs.div_euclid(86_400);
        let secs_of_day = epoch_secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let hour = secs_of_day / 3_600;
        let minute = (secs_of_day % 3_600) / 60;
        let second = secs_of_day % 60;
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
    }

    /// Days since 1970-01-01 → (year, month, day) in the proleptic
    /// Gregorian calendar.
    fn civil_from_days(days: i64) -> (i64, u32, u32) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let year = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if month <= 2 { year + 1 } else { year }, month, day)
    }

    fn sha256_hex(data: &[u8]) -> String {
        hex(&Sha256::digest(data))
    }

    fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Derive the per-day signing key: the documented
    /// `HMAC(HMAC(HMAC(HMAC("AWS4"+secret, date), region), service), "aws4_request")`
    /// chain.
    pub(crate) fn derive_signing_key(
        secret_key: &str,
        date: &str,
        region: &str,
        service: &str,
    ) -> Vec<u8> {
        let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
        let k_region = hmac_sha256(&k_date, region.as_bytes());
        let k_service = hmac_sha256(&k_region, service.as_bytes());
        hmac_sha256(&k_service, b"aws4_request")
    }

    pub(crate) fn sign(input: &SigningInput<'_>) -> SigningOutput {
        let date = &input.amz_date[..8];
        let payload_hash = sha256_hex(input.payload);
        let canonical_uri = canonical_uri_from_wire_path(input.wire_path);

        // Canonical headers: lowercase names, trimmed values, sorted by
        // name, one per line. The fixed set Converse needs sorts as
        // content-type < host < x-amz-date < x-amz-security-token.
        let mut canonical_headers = format!(
            "content-type:{}\nhost:{}\nx-amz-date:{}\n",
            input.content_type, input.host, input.amz_date
        );
        let mut signed_headers = "content-type;host;x-amz-date".to_string();
        if let Some(token) = input.session_token {
            canonical_headers.push_str(&format!("x-amz-security-token:{token}\n"));
            signed_headers.push_str(";x-amz-security-token");
        }

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            input.method,
            canonical_uri,
            input.canonical_query,
            canonical_headers,
            signed_headers,
            payload_hash
        );

        let scope = format!("{date}/{}/{}/aws4_request", input.region, input.service);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
            input.amz_date,
            sha256_hex(canonical_request.as_bytes())
        );

        let signing_key = derive_signing_key(input.secret_key, date, input.region, input.service);
        let signature = hex(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

        SigningOutput {
            authorization: format!(
                "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
                input.access_key
            ),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn uri_encode_passes_unreserved_and_encodes_the_rest_uppercase() {
            assert_eq!(
                uri_encode("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
                "us.anthropic.claude-sonnet-4-5-20250929-v1%3A0"
            );
            assert_eq!(uri_encode("a/b c~d_e"), "a%2Fb%20c~d_e");
        }

        #[test]
        fn format_amz_date_handles_epoch_and_a_known_recent_instant() {
            assert_eq!(format_amz_date(0), "19700101T000000Z");
            // 2026-07-11 12:34:56 UTC — cross-checked with
            // `date -u -r 1783773296 +%Y%m%dT%H%M%SZ`.
            assert_eq!(format_amz_date(1_783_773_296), "20260711T123456Z");
        }

        #[test]
        fn derive_signing_key_matches_the_documented_aws_example() {
            // The worked example from AWS's SigV4 documentation
            // ("Deriving a signing key"): secret wJalr…, 20120215,
            // us-east-1, iam.
            let key = derive_signing_key(
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                "20120215",
                "us-east-1",
                "iam",
            );
            assert_eq!(
                hex(&key),
                "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
            );
        }

        #[test]
        fn host_from_url_includes_non_default_ports_and_drops_default_ones() {
            assert_eq!(
                host_from_url("https://bedrock-runtime.us-east-1.amazonaws.com/model/x/converse"),
                Some("bedrock-runtime.us-east-1.amazonaws.com".to_string())
            );
            assert_eq!(
                host_from_url("http://127.0.0.1:9099/model/x/converse"),
                Some("127.0.0.1:9099".to_string())
            );
        }

        #[test]
        fn canonical_uri_re_encodes_the_wire_paths_existing_escapes() {
            // The SigV4 double-encoding rule for non-S3 services: the `%3A`
            // sent on the wire signs as `%253A`. Botocore does this; a
            // "sign what you send" implementation fails against the real
            // endpoint with SignatureDoesNotMatch.
            assert_eq!(
                canonical_uri_from_wire_path(
                    "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse"
                ),
                "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%253A0/converse"
            );
        }

        /// Golden vector generated with botocore 1.43.46's `SigV4Auth` (the
        /// AWS reference implementation) for this exact request shape, with
        /// the clock frozen to `20260711T123456Z` — the generator script and
        /// its output are archived under `verifications/` for the PR. Pins
        /// the full composition: canonical request (double-encoded model id
        /// in the path), string to sign, key derivation, and header
        /// formatting.
        #[test]
        fn sign_matches_botocore_golden_vector_without_session_token() {
            let output = sign(&SigningInput {
                method: "POST",
                wire_path: "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse",
                canonical_query: "",
                host: "bedrock-runtime.us-east-1.amazonaws.com",
                content_type: "application/json",
                payload: br#"{"messages":[{"role":"user","content":[{"text":"hi"}]}]}"#,
                region: "us-east-1",
                service: "bedrock",
                access_key: "AKIDEXAMPLE",
                secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                session_token: None,
                amz_date: "20260711T123456Z",
            });
            assert_eq!(
                output.authorization,
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260711/us-east-1/bedrock/aws4_request, \
                 SignedHeaders=content-type;host;x-amz-date, \
                 Signature=ae768b88a6982fa3a8811e2286c4360bf143e55da1d1aac37851bbf7a0b78773"
            );
        }

        /// Same request signed with a session token — the token joins both
        /// the canonical headers and the signed-headers list.
        #[test]
        fn sign_matches_botocore_golden_vector_with_session_token() {
            let output = sign(&SigningInput {
                method: "POST",
                wire_path: "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse",
                canonical_query: "",
                host: "bedrock-runtime.us-east-1.amazonaws.com",
                content_type: "application/json",
                payload: br#"{"messages":[{"role":"user","content":[{"text":"hi"}]}]}"#,
                region: "us-east-1",
                service: "bedrock",
                access_key: "AKIDEXAMPLE",
                secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                session_token: Some("IQoJb3JpZ2luX2VjEXAMPLETOKEN"),
                amz_date: "20260711T123456Z",
            });
            assert_eq!(
                output.authorization,
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260711/us-east-1/bedrock/aws4_request, \
                 SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, \
                 Signature=97c2b7044a3c4687c95e5e23e82d9d895c638ad815ab2d13743d6ad1e1d7b4ac"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::tool::ToolSchema;
    use stella_protocol::{ToolOutput, ToolResult};
    use wiremock::matchers::{header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_provider(server_uri: &str) -> BedrockProvider {
        BedrockProvider::new(
            ApiKey::new("AKIDEXAMPLE"),
            ApiKey::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            None,
            "us-east-1",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        )
        .with_base_url(server_uri.to_string())
    }

    #[test]
    fn to_bedrock_messages_hoists_system_and_frames_tool_round_trips() {
        let messages = vec![
            CompletionMessage::system("You are a coding agent."),
            CompletionMessage::user("read a.rs"),
            CompletionMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    call_id: "tooluse_abc".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                }],
                tool_results: vec![],
            },
            CompletionMessage {
                role: MessageRole::Tool,
                content: String::new(),
                tool_calls: vec![],
                tool_results: vec![ToolResult {
                    call_id: "tooluse_abc".into(),
                    output: ToolOutput::Ok {
                        content: "fn main(){}".into(),
                    },
                }],
            },
        ];
        let (system, mapped) = to_bedrock_messages(&messages);
        assert_eq!(system.len(), 1);
        assert_eq!(mapped.len(), 3);
        assert_eq!(mapped[1].role, "assistant");
        let tool_use = mapped[1].content[0].tool_use.as_ref().unwrap();
        assert_eq!(tool_use.tool_use_id, "tooluse_abc");
        // Converse frames tool results as a user-role message.
        assert_eq!(mapped[2].role, "user");
        let tool_result = mapped[2].content[0].tool_result.as_ref().unwrap();
        assert_eq!(tool_result.tool_use_id, "tooluse_abc");
        assert_eq!(tool_result.content[0].text, "fn main(){}");
        assert_eq!(tool_result.status, None);
    }

    #[test]
    fn failed_tool_results_carry_error_status_not_a_text_prefix() {
        let messages = vec![CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: "tooluse_1".into(),
                output: ToolOutput::Error {
                    message: "no such file".into(),
                },
            }],
        }];
        let (_, mapped) = to_bedrock_messages(&messages);
        let tool_result = mapped[0].content[0].tool_result.as_ref().unwrap();
        assert_eq!(tool_result.status, Some("error"));
        assert_eq!(tool_result.content[0].text, "no such file");
    }

    #[tokio::test]
    async fn complete_posts_a_signed_converse_request_and_parses_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse",
            ))
            .and(header_regex(
                "authorization",
                r"^AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/\d{8}/us-east-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=[0-9a-f]{64}$",
            ))
            .and(header_regex("x-amz-date", r"^\d{8}T\d{6}Z$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": {"message": {"role": "assistant", "content": [{"text": "Hello from Bedrock"}]}},
                "stopReason": "end_turn",
                "usage": {"inputTokens": 9, "outputTokens": 5, "cacheReadInputTokens": 3}
            })))
            .mount(&server)
            .await;

        let provider = test_provider(&server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: Some(1024),
            temperature: Some(0.0),
            effort: None,
            tools: vec![],
        };

        let result = provider
            .complete(req)
            .await
            .expect("completion should succeed");
        assert_eq!(result.text, "Hello from Bedrock");
        assert_eq!(result.usage.input_tokens, 9);
        assert_eq!(result.usage.output_tokens, 5);
        assert_eq!(result.usage.cached_input_tokens, 3);
    }

    #[tokio::test]
    async fn complete_parses_tool_use_blocks_into_tool_calls() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": {"message": {"role": "assistant", "content": [
                    {"text": "Let me read that."},
                    {"toolUse": {"toolUseId": "tooluse_xyz", "name": "read_file", "input": {"path": "src/lib.rs"}}}
                ]}},
                "stopReason": "tool_use",
                "usage": {"inputTokens": 30, "outputTokens": 12}
            })))
            .mount(&server)
            .await;

        let provider = test_provider(&server.uri());
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
        };

        let result = provider.complete(req).await.expect("should succeed");
        assert_eq!(result.text, "Let me read that.");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].call_id, "tooluse_xyz");
        assert_eq!(result.tool_calls[0].name, "read_file");
        assert_eq!(
            result.tool_calls[0].input,
            serde_json::json!({"path": "src/lib.rs"})
        );
    }

    #[tokio::test]
    async fn complete_maps_403_to_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string(
                "{\"message\":\"The security token included in the request is invalid.\"}",
            ))
            .mount(&server)
            .await;

        let provider = test_provider(&server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        assert!(matches!(err, ProviderError::Auth(_)));
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn complete_maps_429_throttling_to_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429).set_body_string("{\"message\":\"Too many requests\"}"),
            )
            .mount(&server)
            .await;

        let provider = test_provider(&server.uri());
        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let err = provider.complete(req).await.unwrap_err();
        assert!(matches!(err, ProviderError::RateLimited { .. }));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn complete_sends_the_session_token_header_when_configured() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header_regex(
                "x-amz-security-token",
                r"^IQoJb3JpZ2luX2VjEXAMPLETOKEN$",
            ))
            .and(header_regex(
                "authorization",
                r"SignedHeaders=content-type;host;x-amz-date;x-amz-security-token,",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": {"message": {"role": "assistant", "content": [{"text": "ok"}]}},
                "usage": {"inputTokens": 1, "outputTokens": 1}
            })))
            .mount(&server)
            .await;

        let provider = BedrockProvider::new(
            ApiKey::new("AKIDEXAMPLE"),
            ApiKey::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            Some(ApiKey::new("IQoJb3JpZ2luX2VjEXAMPLETOKEN")),
            "us-east-1",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        )
        .with_base_url(server.uri());

        let req = CompletionRequest {
            messages: vec![CompletionMessage::user("hi")],
            max_output_tokens: None,
            temperature: None,
            effort: None,
            tools: vec![],
        };

        let result = provider.complete(req).await.expect("should succeed");
        assert_eq!(result.text, "ok");
    }
}
