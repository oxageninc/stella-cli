//! Durable event forwarding from engine turns into command-deck lanes.

use std::sync::Arc;

use stella_protocol::AgentEvent;
use stella_store::Store;
use stella_tui::Inbound;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::agent;

/// Persist each event and forward it to the deck. The returned bit is false
/// after any persistence failure and must be carried into execution closeout.
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
            let _ = inbound.send(Inbound::Event {
                agent: lane.clone(),
                event,
            });
        }
        persistence_complete
    })
}
