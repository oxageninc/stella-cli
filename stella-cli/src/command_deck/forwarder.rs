//! Durable event forwarding from engine turns into command-deck lanes —
//! split out of `command_deck.rs` (already over its file-size ratchet; not a
//! file to grow) so `spawn_forwarder`, the one seam shared by every deck
//! lane (the lead's turns and every `crate::subsession` worker), only needs
//! a call site.

use std::sync::Arc;

use stella_protocol::AgentEvent;
use stella_store::Store;
use stella_tui::Inbound;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::agent;
use crate::cache_insight::cache_insight_for;

/// Persist each event (via the shared [`agent::persist_event`] write path)
/// and forward it to the deck as `agent`'s `Inbound::Event`, plus a derived
/// `Inbound::CacheInsight` when the event carries pricing-relevant usage
/// (issues #267/#269). The returned bit is false after any persistence
/// failure and must be carried into execution closeout — callers fail
/// closed on incomplete telemetry rather than silently treating a partial
/// record as complete.
pub(crate) fn spawn_forwarder(
    mut rx: UnboundedReceiver<AgentEvent>,
    execution: Option<(Arc<Store>, i64)>,
    provider_id: String,
    inbound: UnboundedSender<Inbound>,
    lane: String,
) -> tokio::task::JoinHandle<bool> {
    tokio::spawn(async move {
        let mut seq = 0u64;
        let mut store_warned = false;
        let mut persistence_complete = true;
        while let Some(event) = rx.recv().await {
            if let Some((store, id)) = &execution {
                if !agent::persist_event(store, *id, seq, &event, &provider_id) {
                    persistence_complete = false;
                    if !store_warned {
                        store_warned = true;
                        let _ = inbound.send(Inbound::Event {
                            agent: lane.clone(),
                            event: AgentEvent::Error {
                                message: "store write failed — the persisted event/telemetry \
                                          record for this session is incomplete"
                                    .to_string(),
                                retryable: true,
                            },
                        });
                    }
                }
                seq += 1;
            }
            // Sent AFTER StepUsage below so the lane is already registered.
            let cache_insight = cache_insight_for(&provider_id, &lane, &event);
            let _ = inbound.send(Inbound::Event {
                agent: lane.clone(),
                event,
            });
            if let Some(insight) = cache_insight {
                let _ = inbound.send(insight);
            }
        }
        persistence_complete
    })
}
