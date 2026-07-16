//! Per-session agent-usage telemetry: one [`AgentUseEvent`] per invocation
//! of an installed agent definition, recorded at the moment the definition
//! is applied to a turn and **drained** once per execution into the store's
//! `agent_uses` table (see `stella-cli`'s `record_execution_end`).
//!
//! The shape mirrors the file-touch ledger ([`crate::file_touch`]): the
//! registry holds one [`AgentUseLedger`] behind a mutex, invocation sites
//! call `ToolRegistry::record_agent_use`, and the persistence layer takes
//! everything recorded since the previous drain. Unlike file touches —
//! which aggregate by path — agent uses are an **event log**: every
//! invocation is its own row, because the unit of analysis is
//! "agent-version X was invoked by execution Y at time T", not a per-file
//! aggregate.
//!
//! `version` is the agent's *pinned* version at invocation time (see
//! `stella-cli::agents_installed` for the on-disk versioning scheme), so the
//! telemetry can attribute outcomes to the exact definition text that ran.

use serde::{Deserialize, Serialize};

/// One invocation of an installed agent definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentUseEvent {
    /// The agent's name (its loaded definition name).
    pub agent: String,
    /// The pinned version of the definition at invocation time (1-based;
    /// an un-versioned agent is version 1).
    pub version: u32,
    /// Why / how it was invoked — free text, kept short by the recorder
    /// (e.g. a snipped task line). Empty when no reason was available.
    pub reason: String,
}

/// The session's agent-use ledger. One instance lives behind the registry's
/// mutex; [`AgentUseLedger::drain`] hands the accumulated events to the
/// per-execution persistence step and resets the ledger, so each execution
/// records exactly the invocations that happened on its watch.
#[derive(Debug, Default)]
pub struct AgentUseLedger {
    events: Vec<AgentUseEvent>,
}

impl AgentUseLedger {
    /// Append one invocation.
    pub fn record(&mut self, event: AgentUseEvent) {
        self.events.push(event);
    }

    /// Take every event recorded since the last drain, in record order,
    /// leaving the ledger empty.
    pub fn drain(&mut self) -> Vec<AgentUseEvent> {
        std::mem::take(&mut self.events)
    }

    /// Events currently pending a drain (test/inspection accessor).
    pub fn pending(&self) -> usize {
        self.events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(agent: &str, version: u32, reason: &str) -> AgentUseEvent {
        AgentUseEvent {
            agent: agent.to_string(),
            version,
            reason: reason.to_string(),
        }
    }

    #[test]
    fn drain_returns_events_in_record_order_and_resets() {
        let mut ledger = AgentUseLedger::default();
        ledger.record(ev("reviewer", 2, "review the diff"));
        ledger.record(ev("reviewer", 2, "second pass"));
        ledger.record(ev("planner", 1, ""));
        assert_eq!(ledger.pending(), 3);

        let drained = ledger.drain();
        assert_eq!(
            drained,
            vec![
                ev("reviewer", 2, "review the diff"),
                ev("reviewer", 2, "second pass"),
                ev("planner", 1, ""),
            ],
            "an event log, never aggregated — repeat invocations stay distinct rows"
        );
        assert_eq!(ledger.pending(), 0, "drain resets the ledger");
        assert!(ledger.drain().is_empty(), "a second drain has nothing");
    }
}
