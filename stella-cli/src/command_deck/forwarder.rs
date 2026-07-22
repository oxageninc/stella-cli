//! Deck event forwarding with execution-store persistence.

use std::sync::Arc;

use stella_protocol::AgentEvent;
use stella_store::Store;
use stella_tui::Inbound;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::agent;
use crate::cache_insight::cache_insight_for;

/// Drain one turn's engine events: persist each (via the shared
/// [`agent::persist_event`] write path) and forward it to the deck as
/// `agent`'s `Inbound::Event`. The deck-mode replacement for the agent's
/// stdout renderer, shared by the lead's turns and every sub-session worker
/// (`crate::subsession`). stderr belongs to the alternate screen here, so a
/// persistence failure warns *through the deck* instead — once — as a
/// transcript-visible error event; silently losing the audit trail (disk full,
/// DB locked) is not acceptable.
pub(crate) fn spawn_forwarder(
    mut rx: UnboundedReceiver<AgentEvent>,
    execution: Option<(Arc<Store>, i64)>,
    provider_id: String,
    inbound: UnboundedSender<Inbound>,
    lane: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seq = 0u64;
        let mut store_warned = false;
        while let Some(event) = rx.recv().await {
            if let Some((store, id)) = &execution {
                if !agent::persist_event(store, *id, seq, &event, &provider_id) && !store_warned {
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
    })
}
