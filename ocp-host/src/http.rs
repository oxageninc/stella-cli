//! Streamable-HTTP transport: a remote OCP provider reached by POSTing the
//! envelope to its URL ( "remote providers:
//! streamable HTTP"). The reference host uses request/response JSON — the
//! [`Envelope`] as the POST body, one [`Envelope`] back as the response body
//! — which any streamable-HTTP server satisfies; chunked frame streaming is
//! a documented forward extension, not needed for the v1 shape.
//!
//! Unlike stdio's process isolation, an HTTP provider is remote by nature, so
//! its `egress` posture is decided by the URL host and gated through the same
//! [`crate::consent`] store at the [`crate::host::Host`] layer.

use std::time::Duration;

use async_trait::async_trait;
use ocp_types::{Capabilities, ContextQuery, ContextQueryResult, PROTOCOL_VERSION, ProviderInfo};

use crate::error::HostError;
use crate::provider::ContextProvider;
use crate::wire::{Envelope, envelope_kind, versions_compatible};

/// Total per-request budget for an HTTP exchange (handshake or query).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// A [`ContextProvider`] backed by a remote HTTP endpoint. Handshakes once on
/// [`HttpProvider::connect`] and caches the negotiated identity + capabilities.
pub struct HttpProvider {
    id: String,
    url: String,
    client: reqwest::Client,
    info: ProviderInfo,
    capabilities: Capabilities,
}

impl HttpProvider {
    /// Connect to a remote provider: POST a `handshake`, expect a compatible
    /// `handshake_ack`, and cache its identity + capabilities. `id` is the
    /// host-facing routing/consent key.
    pub async fn connect(id: impl Into<String>, url: impl Into<String>) -> Result<Self, HostError> {
        let id = id.into();
        let url = url.into();
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| HostError::Transport {
                id: id.clone(),
                message: format!("building HTTP client: {e}"),
            })?;

        let ack = post_envelope(
            &client,
            &url,
            &Envelope::Handshake {
                protocol_version: PROTOCOL_VERSION.to_string(),
            },
            &id,
        )
        .await?;

        match ack {
            Envelope::HandshakeAck {
                protocol_version,
                provider,
                capabilities,
            } => {
                if !versions_compatible(PROTOCOL_VERSION, &protocol_version) {
                    return Err(HostError::VersionMismatch {
                        host: PROTOCOL_VERSION.to_string(),
                        provider: provider.name,
                        provider_version: protocol_version,
                    });
                }
                // An HTTP transport is egress by definition: every query is
                // POSTed off-box to a remote URL. So the consent gate must key
                // off transport, not the remote's self-report — a remote that
                // handshakes `egress:false` would otherwise be queried with no
                // consent. Force it on here regardless of what it declared.
                // (The stdio path keeps its declared posture; a local child
                // that doesn't reach the network genuinely may not be egress.)
                let mut info = provider;
                info.data_flow.egress = true;
                Ok(Self {
                    id,
                    url,
                    client,
                    info,
                    capabilities,
                })
            }
            other => Err(HostError::UnexpectedEnvelope {
                id,
                expected: "handshake_ack".into(),
                got: envelope_kind(&other).into(),
            }),
        }
    }
}

/// POST one envelope to the provider URL and decode the response as one
/// envelope. A non-2xx status or a non-envelope body is a clean named error,
/// never a panic (task deliverable 5).
async fn post_envelope(
    client: &reqwest::Client,
    url: &str,
    env: &Envelope,
    id: &str,
) -> Result<Envelope, HostError> {
    let response = client
        .post(url)
        .json(env)
        .send()
        .await
        .map_err(|e| HostError::Transport {
            id: id.to_string(),
            message: e.to_string(),
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(HostError::Transport {
            id: id.to_string(),
            message: format!("HTTP {status}: {body}"),
        });
    }

    response.json::<Envelope>().await.map_err(|e| {
        HostError::Wire(format!(
            "provider {id} returned a non-envelope HTTP body: {e}"
        ))
    })
}

#[async_trait]
impl ContextProvider for HttpProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn query(&self, query: &ContextQuery) -> Result<ContextQueryResult, HostError> {
        let reply = post_envelope(
            &self.client,
            &self.url,
            &Envelope::Query {
                query: query.clone(),
            },
            &self.id,
        )
        .await?;
        match reply {
            Envelope::Frames { result } => Ok(result),
            Envelope::Error { message } => Err(HostError::Provider {
                id: self.id.clone(),
                message,
            }),
            other => Err(HostError::UnexpectedEnvelope {
                id: self.id.clone(),
                expected: "frames".into(),
                got: envelope_kind(&other).into(),
            }),
        }
    }

    async fn shutdown(&self) -> Result<(), HostError> {
        // Best-effort teardown notice; a remote endpoint is not ours to reap.
        let _ = post_envelope(&self.client, &self.url, &Envelope::Shutdown, &self.id).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocp_types::capability::QueryCapability;
    use ocp_types::{ContextFrame, DataFlow, FrameKind};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ack_body(version: &str) -> serde_json::Value {
        serde_json::to_value(Envelope::HandshakeAck {
            protocol_version: version.to_string(),
            provider: ProviderInfo {
                name: "remote-docs".into(),
                version: "0.1.0".into(),
                data_flow: DataFlow {
                    reads: true,
                    writes: false,
                    egress: true,
                },
            },
            capabilities: Capabilities {
                query: QueryCapability {
                    kinds: vec!["doc".into()],
                    filters: vec![],
                },
                ..Capabilities::default()
            },
        })
        .unwrap()
    }

    fn frames_body() -> serde_json::Value {
        serde_json::to_value(Envelope::Frames {
            result: ContextQueryResult {
                frames: vec![ContextFrame {
                    id: "frm_h".into(),
                    kind: FrameKind::Doc,
                    title: "remote doc".into(),
                    content: "remote content".into(),
                    uri: Some("https://example.test/doc".into()),
                    score: 0.6,
                    token_cost: 20,
                    valid_from: None,
                    valid_to: None,
                    recorded_at: None,
                    provenance: vec![],
                    citation_label: Some("remote doc".into()),
                    embedding: None,
                    relations: vec![],
                }],
                truncated: false,
                dropped_estimate: None,
            },
        })
        .unwrap()
    }

    fn sample_query() -> ContextQuery {
        ContextQuery {
            goal: "g".into(),
            query_text: None,
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames: 5,
            max_tokens: 4000,
            as_of: None,
        }
    }

    #[tokio::test]
    async fn http_handshake_then_query_round_trips_via_wiremock() {
        let server = MockServer::start().await;
        // Both the handshake and the query POST to the same URL; the responder
        // dispatches on the request envelope's `type`.
        Mock::given(method("POST"))
            .respond_with(|req: &wiremock::Request| {
                let body = match serde_json::from_slice::<Envelope>(&req.body) {
                    Ok(Envelope::Handshake { .. }) => ack_body(PROTOCOL_VERSION),
                    Ok(Envelope::Query { .. }) => frames_body(),
                    _ => serde_json::to_value(Envelope::Error {
                        message: "unexpected request".into(),
                    })
                    .unwrap(),
                };
                ResponseTemplate::new(200).set_body_json(body)
            })
            .mount(&server)
            .await;

        let provider = HttpProvider::connect("remote", server.uri())
            .await
            .expect("handshake ok");
        assert_eq!(provider.info().name, "remote-docs");
        assert!(provider.info().data_flow.egress);

        let result = provider.query(&sample_query()).await.expect("query ok");
        assert_eq!(result.frames.len(), 1);
        assert_eq!(result.frames[0].title, "remote doc");
    }

    #[tokio::test]
    async fn http_version_mismatch_rejects_the_provider() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ack_body("ocp/2.0")))
            .mount(&server)
            .await;

        let err = match HttpProvider::connect("remote", server.uri()).await {
            Ok(_) => panic!("incompatible version must reject"),
            Err(e) => e,
        };
        assert!(matches!(err, HostError::VersionMismatch { .. }));
    }

    #[tokio::test]
    async fn a_non_envelope_http_body_is_a_clean_wire_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>not ocp</html>"))
            .mount(&server)
            .await;

        let err = match HttpProvider::connect("remote", server.uri()).await {
            Ok(_) => panic!("garbage body must not panic the host"),
            Err(e) => e,
        };
        assert!(matches!(err, HostError::Wire(_)));
    }

    #[tokio::test]
    async fn http_transport_forces_egress_even_when_the_remote_claims_local() {
        // A remote self-declares `egress:false` in its handshake. Using an HTTP
        // transport IS egress (the query is POSTed off-box), so the host must
        // override the claim and still gate consent — otherwise a remote could
        // opt itself out of the consent gate by lying.
        let server = MockServer::start().await;
        let sneaky_ack = serde_json::to_value(Envelope::HandshakeAck {
            protocol_version: PROTOCOL_VERSION.to_string(),
            provider: ProviderInfo {
                name: "sneaky-remote".into(),
                version: "0.1.0".into(),
                data_flow: DataFlow {
                    reads: true,
                    writes: false,
                    egress: false, // the lie the host must not trust
                },
            },
            capabilities: Capabilities {
                query: QueryCapability {
                    kinds: vec!["doc".into()],
                    filters: vec![],
                },
                ..Capabilities::default()
            },
        })
        .unwrap();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(sneaky_ack))
            .mount(&server)
            .await;

        let provider = HttpProvider::connect("remote", server.uri())
            .await
            .expect("handshake ok");

        assert!(
            provider.info().data_flow.egress,
            "an HTTP transport must be treated as egress regardless of the remote's claim"
        );
        assert!(
            crate::consent::ConsentStore::requires_consent(provider.info()),
            "an HTTP provider must always require consent, even claiming egress:false"
        );
    }
}
