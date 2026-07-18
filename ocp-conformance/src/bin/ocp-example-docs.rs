//! `ocp-example-docs` — a minimal reference OCP provider over stdio.
//!
//! It serves a couple of canned "documentation" frames, proving the external
//! child-process path end-to-end ( seed
//! providers). It is also the child-process **test fixture** for the
//! conformance suite: `--misbehave <mode>` deliberately breaks one protocol
//! guarantee at a time so tests can prove the suite catches a broken
//! provider (task deliverable). It reuses `ocp-host`'s `wire::Envelope` for
//! (de)serialization since both live in this workspace; a real out-of-tree
//! provider — in any language — would instead implement the line-oriented
//! wire format directly against `ocp-types` (the frame/query types) plus a
//! JSON codec, which is the only contract it must honor.

use std::io::{BufRead, Write};

use clap::{Parser, ValueEnum};
use ocp_host::wire::Envelope;
use ocp_types::capability::QueryCapability;
use ocp_types::{
    Capabilities, ContextFrame, ContextQueryResult, DataFlow, FrameKind, PROTOCOL_VERSION,
    Provenance, ProviderInfo,
};

/// Ways this fixture can deliberately violate the protocol, each tripping a
/// different conformance check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum Misbehave {
    /// Return frames whose summed `token_cost` blows the query budget
    /// (trips `budget-honesty`).
    LyingCosts,
    /// Return a frame with a score outside `[0,1]` (trips `frame-validity`).
    BadScore,
    /// Return a frame with an empty citation label (trips `frame-validity`).
    EmptyCitation,
    /// Ack an incompatible protocol version (trips `handshake`).
    BadVersion,
    /// Exit on receiving a query (trips `frame-validity`/`budget-honesty`
    /// and exercises the host's child-death isolation).
    CrashOnQuery,
    /// Exit on receiving a malformed line (trips
    /// `malformed-input-tolerance`).
    CrashOnGarbage,
}

#[derive(Parser)]
#[command(
    name = "ocp-example-docs",
    about = "A tiny reference OCP provider serving canned documentation frames over stdio."
)]
struct Args {
    /// Deliberately break one protocol guarantee (for conformance testing).
    #[arg(long, value_enum)]
    misbehave: Option<Misbehave>,
}

fn main() {
    let args = Args::parse();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => break, // EOF or a broken pipe — the host is gone.
            Ok(_) => {}
        }

        let envelope = match serde_json::from_str::<Envelope>(line.trim_end()) {
            Ok(envelope) => envelope,
            Err(_) => {
                // A malformed line: a robust provider ignores it and stays
                // alive; the misbehaving one dies (to prove the suite notices).
                if args.misbehave == Some(Misbehave::CrashOnGarbage) {
                    std::process::exit(1);
                }
                continue;
            }
        };

        match envelope {
            Envelope::Handshake { .. } => {
                let protocol_version = if args.misbehave == Some(Misbehave::BadVersion) {
                    "ocp/2.0".to_string()
                } else {
                    PROTOCOL_VERSION.to_string()
                };
                write_envelope(
                    &mut stdout,
                    &Envelope::HandshakeAck {
                        protocol_version,
                        provider: provider_info(),
                        capabilities: capabilities(),
                    },
                );
            }
            Envelope::Query { .. } => {
                if args.misbehave == Some(Misbehave::CrashOnQuery) {
                    std::process::exit(1);
                }
                write_envelope(
                    &mut stdout,
                    &Envelope::Frames {
                        result: ContextQueryResult {
                            frames: canned_frames(args.misbehave),
                            truncated: false,
                            dropped_estimate: None,
                        },
                    },
                );
            }
            Envelope::Shutdown => std::process::exit(0),
            // handshake_ack / frames / error are host→provider-invalid inputs;
            // a provider ignores them.
            _ => {}
        }
    }
}

fn write_envelope(stdout: &mut std::io::Stdout, envelope: &Envelope) {
    // A provider is a plain pipe writer; if the host has gone, give up quietly.
    if let Ok(line) = serde_json::to_string(envelope) {
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

fn provider_info() -> ProviderInfo {
    ProviderInfo {
        name: "ocp-example-docs".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        // A docs index reads the query and serves local frames; nothing
        // leaves the machine.
        data_flow: DataFlow {
            reads: true,
            writes: false,
            egress: false,
        },
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        query: QueryCapability {
            kinds: vec!["doc".into(), "snippet".into()],
            filters: vec!["path".into()],
        },
        upsert: false,
        graph: false,
        embeddings_fingerprint: None,
        subscribe: false,
    }
}

fn canned_frames(misbehave: Option<Misbehave>) -> Vec<ContextFrame> {
    let bad_score = misbehave == Some(Misbehave::BadScore);
    let empty_citation = misbehave == Some(Misbehave::EmptyCitation);
    // A budget liar claims an absurd cost per frame so the sum blows any sane
    // query budget; an honest frame is a few dozen tokens.
    let token_cost = if misbehave == Some(Misbehave::LyingCosts) {
        99_999
    } else {
        64
    };

    vec![
        ContextFrame {
            id: "frm_getting_started".into(),
            kind: FrameKind::Doc,
            title: "Getting Started".into(),
            content: "Install the reference binding with `cargo add ocp-types`, then implement \
                      the four required methods."
                .into(),
            uri: Some("file:///docs/getting-started.md".into()),
            score: if bad_score { 1.5 } else { 0.82 },
            token_cost,
            valid_from: None,
            valid_to: None,
            recorded_at: None,
            provenance: vec![Provenance {
                kind: "file".into(),
                uri: Some("file:///docs/getting-started.md".into()),
                range: Some("L1-40".into()),
                digest: None,
                method: None,
                by: Some("ocp-example-docs".into()),
            }],
            citation_label: Some(if empty_citation {
                String::new()
            } else {
                "getting-started.md L1-40".into()
            }),
            embedding: None,
            relations: vec![],
        },
        ContextFrame {
            id: "frm_configuration".into(),
            kind: FrameKind::Doc,
            title: "Configuration".into(),
            content: "Providers declare their data-flow direction at the handshake so hosts can \
                      gate consent before sending any query."
                .into(),
            uri: Some("file:///docs/configuration.md".into()),
            score: 0.61,
            token_cost,
            valid_from: None,
            valid_to: None,
            recorded_at: None,
            provenance: vec![Provenance {
                kind: "file".into(),
                uri: Some("file:///docs/configuration.md".into()),
                range: Some("L1-25".into()),
                digest: None,
                method: None,
                by: Some("ocp-example-docs".into()),
            }],
            citation_label: Some("configuration.md L1-25".into()),
            embedding: None,
            relations: vec![],
        },
    ]
}
