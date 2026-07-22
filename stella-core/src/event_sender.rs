//! Cloneable event sender with an optional synchronous, ordered boundary.
//!
//! The core remains I/O-free: callers may supply a closure that durably
//! journals an event before it is admitted to the ordinary Tokio channel.
//! Because every clone shares that closure (and any mutex it captures), the
//! durable order and channel order can be made identical across concurrent
//! producers. A paid-call producer does not return from [`EventSender::send`]
//! until the caller's persistence boundary has completed.

use std::fmt;
use std::sync::Arc;

use stella_protocol::AgentEvent;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventSendError;

impl fmt::Display for EventSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("agent event receiver is closed")
    }
}

type SendFn = dyn Fn(AgentEvent) -> Result<(), EventSendError> + Send + Sync;

#[derive(Clone)]
pub struct EventSender {
    send: Arc<SendFn>,
}

impl EventSender {
    /// Wrap an ordinary Tokio sender without a persistence boundary.
    pub fn new(sender: UnboundedSender<AgentEvent>) -> Self {
        Self::from_fn(move |event| sender.send(event).map_err(|_| EventSendError))
    }

    /// Build a sender from a caller-owned synchronous admission closure.
    ///
    /// Benchmark callers use this to append+flush under a shared mutex and
    /// only then enqueue the same event. The closure must not return success
    /// unless the event crossed its required durability boundary.
    pub fn from_fn(
        send: impl Fn(AgentEvent) -> Result<(), EventSendError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            send: Arc::new(send),
        }
    }

    pub fn send(&self, event: AgentEvent) -> Result<(), EventSendError> {
        (self.send)(event)
    }
}

impl From<UnboundedSender<AgentEvent>> for EventSender {
    fn from(sender: UnboundedSender<AgentEvent>) -> Self {
        Self::new(sender)
    }
}
