//! Speculative execution of read-only tool calls.
//!
//! A step's tool calls normally wait for the entire model response to
//! finish streaming before any of them run. But a call is fully known the
//! moment its own block finishes streaming — often seconds before the
//! response ends — and a *read-only* call (per `ToolSchema::read_only`) can
//! be executed early with zero observable difference: it mutates nothing,
//! so running it during the stream instead of after commutes with
//! everything around it, and a result that ends up unused (stream error,
//! retry, input mismatch) is simply discarded work, never a wrong state.
//!
//! The flow: `Engine::run_model_call` hands the provider a
//! [`SpeculationGate`] (a `stella_protocol::ToolCallObserver`). As the
//! adapter announces finished tool-call blocks, the gate forwards the
//! speculation-safe ones over a channel to the engine's pump, which
//! executes them concurrently with the still-streaming model call and
//! collects their outputs into a [`SpeculationPool`]. Dispatch then
//! *harvests* pool entries instead of re-executing — but only when the
//! committed call is byte-identical (same id, name, and input) to what was
//! announced, so a divergent stream can never smuggle a stale result into
//! the transcript.
//!
//! # Ordering safety
//!
//! Dispatch preserves sequential semantics by running every mutating call
//! as its own barrier, in call order. Speculation must not weaken that: a
//! read-only call that appears AFTER a mutating call in the same step must
//! observe the mutation, so it cannot run early. Calls stream in order, so
//! the gate enforces this with a fence: the first non-read-only call it
//! sees permanently stops speculation for the rest of the step. Only the
//! all-read-only *prefix* of a step's calls is ever speculated — exactly
//! the calls dispatch would have started first anyway.
//!
//! # What speculation deliberately does NOT change
//!
//! Speculative execution goes through the same `execute_with_repair` path
//! as dispatch: the registry's policy-bus gates and the settings-declared
//! `PreToolUse`/`PostToolUse` hooks all fire exactly as they would have —
//! just earlier on the wall clock. A blocked call's error output is
//! harvested the same way a success is. The one semantic difference is
//! visible only on a stream that *fails after announcing*: those hooks
//! observed (and the tool executed) a read-only call that never reached
//! the transcript. That is the price of overlap, bounded to read-only
//! tools on purpose.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use stella_protocol::{AgentEvent, ToolCall, ToolCallObserver, ToolOutput};
use tokio::sync::mpsc::UnboundedSender;

use crate::event_sender::EventSender;

/// One speculatively-executed call's outcome, held until dispatch decides
/// whether to harvest it.
pub(crate) struct SpeculativeResult {
    /// Tool name as announced — harvest re-checks it against the committed
    /// call before trusting the output.
    pub name: String,
    /// Parsed input as announced — same re-check.
    pub input: Value,
    pub output: ToolOutput,
    /// Real execution time, which overlapped the model call instead of
    /// following it. Reported on the harvested `ToolResult` event so the
    /// timing stays honest.
    pub duration_ms: u64,
}

/// Completed speculative executions for one committed step, keyed by
/// `call_id`. Dropped wholesale when a stream attempt fails — read-only
/// work is safe to waste.
pub(crate) type SpeculationPool = HashMap<String, SpeculativeResult>;

/// The observer handed to `Provider::complete_observed` — the engine's one
/// stream-side seam. Tool calls: filtered down to the speculation-safe
/// prefix (read-only, well-formed, before any mutating call) and forwarded
/// to the engine's pump. Answer text: each fragment is forwarded to the
/// turn's event channel as a best-effort `TextDelta` preview (the step's
/// eventual `Text` event stays authoritative — see its protocol docs).
pub(crate) struct SpeculationGate {
    read_only_tools: HashSet<String>,
    /// Set on the first non-read-only announcement; never cleared. See the
    /// module docs' ordering-safety section.
    fenced: AtomicBool,
    tx: UnboundedSender<ToolCall>,
    /// The turn's event stream, for live `TextDelta` previews. Deltas from
    /// an attempt that later fails have already been emitted by design —
    /// no reset marker exists; consumers replace the preview when the
    /// authoritative `Text` lands.
    events: EventSender,
}

impl SpeculationGate {
    pub(crate) fn new(
        read_only_tools: HashSet<String>,
        tx: UnboundedSender<ToolCall>,
        events: impl Into<EventSender>,
    ) -> Self {
        Self {
            read_only_tools,
            fenced: AtomicBool::new(false),
            tx,
            events: events.into(),
        }
    }
}

impl ToolCallObserver for SpeculationGate {
    fn text_delta(&self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        // A send after the renderer hung up is fine — previews are lossy by
        // contract.
        let _ = self.events.send(AgentEvent::TextDelta {
            text: delta.to_string(),
        });
    }

    fn tool_call_streamed(&self, call: &ToolCall) {
        if self.fenced.load(Ordering::Relaxed) {
            return;
        }
        if !self.read_only_tools.contains(&call.name) {
            self.fenced.store(true, Ordering::Relaxed);
            return;
        }
        // Adapters never announce a call whose input failed to parse, but
        // the `Null` repair sentinel is load-bearing enough to re-check:
        // a malformed call belongs to dispatch's repair path, not to
        // execution of any kind.
        if call.input.is_null() {
            return;
        }
        // A send after the pump stopped (receiver dropped) is fine — the
        // announcement is simply lost, and dispatch executes normally.
        let _ = self.tx.send(call.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn call(name: &str, id: &str) -> ToolCall {
        ToolCall {
            call_id: id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({"path": "src/lib.rs"}),
        }
    }

    fn gate_with(
        names: &[&str],
    ) -> (
        SpeculationGate,
        tokio::sync::mpsc::UnboundedReceiver<ToolCall>,
        tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (tx, rx) = unbounded_channel();
        let (events_tx, events_rx) = unbounded_channel();
        let read_only: HashSet<String> = names.iter().map(|s| s.to_string()).collect();
        (
            SpeculationGate::new(read_only, tx, events_tx),
            rx,
            events_rx,
        )
    }

    #[test]
    fn text_deltas_forward_to_the_event_stream_skipping_empty_fragments() {
        let (gate, _rx, mut events_rx) = gate_with(&[]);
        gate.text_delta("Hel");
        gate.text_delta("");
        gate.text_delta("lo");

        let forwarded: Vec<String> = std::iter::from_fn(|| events_rx.try_recv().ok())
            .map(|e| match e {
                AgentEvent::TextDelta { text } => text,
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(forwarded, vec!["Hel".to_string(), "lo".to_string()]);
    }

    #[test]
    fn forwards_read_only_calls_and_drops_everything_after_a_mutating_one() {
        let (gate, mut rx, _events) = gate_with(&["read_file", "grep"]);
        gate.tool_call_streamed(&call("read_file", "c1"));
        gate.tool_call_streamed(&call("grep", "c2"));
        // The barrier: nothing after this may run early, including reads.
        gate.tool_call_streamed(&call("edit_file", "c3"));
        gate.tool_call_streamed(&call("read_file", "c4"));

        let forwarded: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|c| c.call_id)
            .collect();
        assert_eq!(
            forwarded,
            vec!["c1".to_string(), "c2".to_string()],
            "only the all-read-only prefix is speculation-safe"
        );
    }

    #[test]
    fn null_input_never_reaches_the_pump_but_does_not_fence() {
        let (gate, mut rx, _events) = gate_with(&["read_file"]);
        gate.tool_call_streamed(&ToolCall {
            call_id: "bad".to_string(),
            name: "read_file".to_string(),
            input: Value::Null,
        });
        gate.tool_call_streamed(&call("read_file", "good"));

        let forwarded: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|c| c.call_id)
            .collect();
        assert_eq!(
            forwarded,
            vec!["good".to_string()],
            "a malformed call belongs to the repair path; a read-only call \
             after it is still safe (nothing mutated)"
        );
    }

    #[test]
    fn send_after_receiver_dropped_is_silently_lost() {
        let (gate, rx, _events) = gate_with(&["read_file"]);
        drop(rx);
        // Must not panic — the announcement is simply lost and dispatch
        // executes the call normally.
        gate.tool_call_streamed(&call("read_file", "c1"));
    }
}
