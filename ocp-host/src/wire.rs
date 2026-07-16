//! The OCP wire envelope and its framing.
//!
//! `06-context-protocol.md` §1/§3.1 states OCP "rides MCP's transport and
//! lifecycle conventions (JSON-RPC 2.0, stdio + streamable HTTP,
//! initialize/capabilities handshake)". The spec leaves the concrete
//! reference-host framing to the binding; this host frames every message as
//! **newline-delimited JSON (NDJSON) — exactly one `serde_json` value per
//! line** — because it is the simplest thing that is unambiguous over a pipe
//! and trivially reimplementable in the TS/Python provider kits
//! (`06-context-protocol.md` §3.6). The `type`-tagged variants below are the
//! OCP method vocabulary the spec calls "the delta": `handshake` ↔
//! `initialize`, `query` ↔ `context/query`, `frames` ↔ its response,
//! `shutdown` ↔ lifecycle teardown (§3.2, §3.3). HTTP providers receive the
//! same envelope as a JSON request body and reply with one as the response
//! body (§3.2 "streamable HTTP").
//!
//! The envelope is **versioned**: the handshake exchange negotiates the
//! protocol family up front (§3.2), and a mismatch is a named error, never a
//! hang.

use ocp_types::{Capabilities, ContextQuery, ContextQueryResult, ProviderInfo};
use serde::{Deserialize, Serialize};

use crate::error::HostError;

/// One OCP message. Every variant is a small, versioned, `type`-tagged JSON
/// object; the host writes exactly one per line (NDJSON) over stdio and one
/// per HTTP body (`06-context-protocol.md` §3.1-§3.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Envelope {
    /// Host hello: opens the exchange with the protocol version the host
    /// speaks (§3.2 `initialize`).
    Handshake { protocol_version: String },
    /// Provider hello-back: its protocol version, identity + declared
    /// data-flow direction, and negotiated capabilities (§3.2). The host
    /// checks the version and surfaces `provider.data_flow` at consent time.
    HandshakeAck {
        protocol_version: String,
        provider: ProviderInfo,
        capabilities: Capabilities,
    },
    /// Host → provider retrieval request (§3.3 `context/query`).
    Query { query: ContextQuery },
    /// Provider → host budgeted, provenance-carrying frames (§3.3 response).
    Frames { result: ContextQueryResult },
    /// Lifecycle teardown; the provider should exit cleanly (§3.2).
    Shutdown,
    /// Provider-reported failure — lets a provider report a bad request
    /// without dying (§3.5 "fail loud"). The host maps this to
    /// [`HostError::Provider`].
    Error { message: String },
}

/// The human name of an envelope variant, for error messages that report
/// "expected X, got Y".
pub fn envelope_kind(env: &Envelope) -> &'static str {
    match env {
        Envelope::Handshake { .. } => "handshake",
        Envelope::HandshakeAck { .. } => "handshake_ack",
        Envelope::Query { .. } => "query",
        Envelope::Frames { .. } => "frames",
        Envelope::Shutdown => "shutdown",
        Envelope::Error { .. } => "error",
    }
}

/// Serialize an envelope to a single NDJSON line (trailing `\n` included).
pub fn encode_line(env: &Envelope) -> Result<String, HostError> {
    let mut line = serde_json::to_string(env).map_err(|e| HostError::Wire(e.to_string()))?;
    line.push('\n');
    Ok(line)
}

/// Parse one NDJSON line into an envelope. A garbage line is a clean
/// [`HostError::Wire`], never a panic — the crash-consistency contract
/// (task deliverable 5).
pub fn decode_line(line: &str) -> Result<Envelope, HostError> {
    serde_json::from_str(line.trim_end()).map_err(|e| HostError::Wire(e.to_string()))
}

/// Two protocol version strings interoperate when they share a **major
/// family** — the substring up to the first `.`. So `ocp/1.0-draft` and
/// `ocp/1.0` interoperate (both `ocp/1`), while `ocp/2.0` does not. This is
/// what lets the public v1.0 freeze drop the `-draft` suffix without a flag
/// day (`06-context-protocol.md` §3).
pub fn versions_compatible(a: &str, b: &str) -> bool {
    protocol_family(a) == protocol_family(b)
}

fn protocol_family(version: &str) -> &str {
    match version.split_once('.') {
        Some((family, _)) => family,
        None => version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocp_types::capability::QueryCapability;
    use ocp_types::{DataFlow, FrameKind, PROTOCOL_VERSION};

    fn sample_ack() -> Envelope {
        Envelope::HandshakeAck {
            protocol_version: PROTOCOL_VERSION.to_string(),
            provider: ProviderInfo {
                name: "ocp-docs".into(),
                version: "0.1.0".into(),
                data_flow: DataFlow {
                    reads: true,
                    writes: false,
                    egress: false,
                },
            },
            capabilities: Capabilities {
                query: QueryCapability {
                    kinds: vec!["doc".into()],
                    filters: vec![],
                },
                ..Capabilities::default()
            },
        }
    }

    #[test]
    fn envelope_kind_matches_the_serialized_type_tag_for_every_variant() {
        // `envelope_kind` hand-writes the strings serde derives from
        // `rename_all = "snake_case"`; they feed "expected X, got Y" wire
        // errors. Without this test, renaming a variant would silently
        // desynchronize the error text from the actual wire tag.
        let variants: Vec<Envelope> = vec![
            Envelope::Handshake {
                protocol_version: PROTOCOL_VERSION.to_string(),
            },
            sample_ack(),
            Envelope::Query {
                query: ContextQuery {
                    goal: "g".into(),
                    query_text: None,
                    embedding: None,
                    kinds: vec![],
                    anchors: vec![],
                    max_frames: 1,
                    max_tokens: 1,
                    as_of: None,
                },
            },
            Envelope::Frames {
                result: ContextQueryResult {
                    frames: vec![],
                    truncated: false,
                    dropped_estimate: None,
                },
            },
            Envelope::Shutdown,
            Envelope::Error {
                message: "m".into(),
            },
        ];
        for env in &variants {
            let value: serde_json::Value =
                serde_json::from_str(encode_line(env).unwrap().trim_end()).unwrap();
            assert_eq!(
                value["type"].as_str(),
                Some(envelope_kind(env)),
                "envelope_kind drifted from the serde tag for {env:?}"
            );
        }
    }

    #[test]
    fn envelope_roundtrips_through_a_single_ndjson_line() {
        let env = sample_ack();
        let line = encode_line(&env).unwrap();
        assert!(line.ends_with('\n'), "a frame is exactly one line");
        assert_eq!(line.matches('\n').count(), 1, "no embedded newlines");
        let back = decode_line(&line).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn query_envelope_carries_ocp_types_shapes_verbatim() {
        let query = ContextQuery {
            goal: "fix the failing test".into(),
            query_text: None,
            embedding: None,
            kinds: vec![FrameKind::Doc],
            anchors: vec![],
            max_frames: 5,
            max_tokens: 2000,
            as_of: None,
        };
        let env = Envelope::Query {
            query: query.clone(),
        };
        let line = encode_line(&env).unwrap();
        match decode_line(&line).unwrap() {
            Envelope::Query { query: back } => assert_eq!(back, query),
            other => panic!("expected query, got {}", envelope_kind(&other)),
        }
    }

    #[test]
    fn a_garbage_line_is_a_clean_wire_error_never_a_panic() {
        let err = decode_line("this is not json {{{").unwrap_err();
        assert!(matches!(err, HostError::Wire(_)));
    }

    #[test]
    fn version_families_interoperate_within_a_major_but_not_across() {
        assert!(versions_compatible("ocp/1.0-draft", "ocp/1.0"));
        assert!(versions_compatible("ocp/1.0-draft", "ocp/1.0-draft"));
        assert!(versions_compatible(PROTOCOL_VERSION, "ocp/1.9"));
        assert!(!versions_compatible("ocp/1.0-draft", "ocp/2.0"));
        assert!(!versions_compatible("ocp/1.0", "mcp/1.0"));
    }
}
