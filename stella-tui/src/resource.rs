//! Global + per-agent CPU/MEM sampling.
//!
//! One of the two labeled **out-of-band read-models**: these numbers are
//! sampled from the OS on the shell tick, never folded from `AgentEvent`s. The
//! same sample feeds both the dashboard/status-bar gauges and (later) dispatch
//! backpressure, so there is one source of truth for "how loaded are we".
//!
//! Backed by `sysinfo`. CPU usage is a diff over time: the first `sample()`
//! call after construction reports 0% (there is no prior snapshot to diff
//! against) and subsequent calls report real utilization. This mirrors the
//! sysinfo-recommended pattern without an artificial startup sleep.
//!
//! Sampling is **throttled** to [`SAMPLE_INTERVAL_MS`] rather than run on every
//! ~30 fps deck tick, and the per-process refresh is **narrowed** to just the
//! tracked agent pids — see those items for why (in short: a full 30×/s refresh
//! is both wasteful and, below `sysinfo`'s minimum diff interval, inaccurate).

use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::deck::{ResourceSample, WorkspaceModel};

/// Minimum deck-clock spacing between real OS refreshes.
///
/// The deck ticks at ~30 fps (33 ms) so animations and elapsed timers stay
/// smooth, but refreshing every process in the system 30×/s is both wasteful
/// (it can itself drive the CPU load the gauge is meant to report, making the
/// TUI feel sluggish) and *inaccurate*: `sysinfo`'s CPU usage is a diff over
/// time and is only meaningful past `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL`
/// (200 ms on macOS). A ~1 s cadence — htop-class — keeps the numbers honest
/// and the refresh cost negligible; the model keeps the last reading between
/// refreshes, so the gauges stay populated.
const SAMPLE_INTERVAL_MS: u64 = 1_000;

/// Samples system + per-process resource usage.
pub struct ResourceMonitor {
    sys: System,
    /// Deck-clock ms of the last real refresh, or `None` before the first one.
    /// Sampling is throttled to [`SAMPLE_INTERVAL_MS`] against this.
    last_sample_ms: Option<u64>,
}

impl Default for ResourceMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceMonitor {
    pub fn new() -> Self {
        // `System::new()` starts with nothing loaded; the first `sample()`
        // populates the CPU list and process table and establishes the
        // baseline diff, so it reports zeroed usage by construction.
        Self {
            sys: System::new(),
            last_sample_ms: None,
        }
    }

    /// Refresh the sample and stamp it onto the model: set
    /// `model.global_cpu_pct` and each agent's `res` from its `meta.pid`.
    ///
    /// Throttled to [`SAMPLE_INTERVAL_MS`] against `model.now_ms`: a call inside
    /// the window is a no-op that leaves the previous reading in place. Returns
    /// whether a real refresh happened — the shell ignores it; the throttle
    /// test asserts on it.
    pub fn sample(&mut self, model: &mut WorkspaceModel) -> bool {
        if let Some(last) = self.last_sample_ms
            && model.now_ms.saturating_sub(last) < SAMPLE_INTERVAL_MS
        {
            return false;
        }
        self.last_sample_ms = Some(model.now_ms);

        self.sys.refresh_cpu_all();

        // Narrow the process refresh to just the agents we track: the global
        // gauge comes from `refresh_cpu_all`, and the only per-process reads
        // below are the agent pids, so refreshing the whole system's process
        // table is wasted work. Agents without a pid contribute nothing.
        let pids: Vec<Pid> = model
            .agents
            .iter()
            .filter_map(|a| a.meta.pid)
            .map(Pid::from_u32)
            .collect();
        self.sys
            .refresh_processes(ProcessesToUpdate::Some(&pids), true);

        model.global_cpu_pct = self.sys.global_cpu_usage();

        for agent in &mut model.agents {
            agent.res = agent
                .meta
                .pid
                .and_then(|pid| self.sys.process(Pid::from_u32(pid)))
                .map(|process| ResourceSample {
                    cpu_pct: process.cpu_usage(),
                    mem_bytes: process.memory(),
                })
                .unwrap_or_default();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{AgentMeta, Inbound};

    #[test]
    fn sample_on_empty_model_does_not_panic() {
        let mut monitor = ResourceMonitor::new();
        let mut model = WorkspaceModel::new();
        monitor.sample(&mut model);
        assert_eq!(model.agents.len(), 0);
    }

    #[test]
    fn sampling_is_throttled_to_the_interval() {
        let mut monitor = ResourceMonitor::new();
        let mut model = WorkspaceModel::new();

        model.now_ms = 10_000;
        assert!(monitor.sample(&mut model), "the first sample always refreshes");

        model.now_ms = 10_000 + SAMPLE_INTERVAL_MS - 1;
        assert!(
            !monitor.sample(&mut model),
            "a call inside the interval is a no-op"
        );

        model.now_ms = 10_000 + SAMPLE_INTERVAL_MS;
        assert!(
            monitor.sample(&mut model),
            "a call at/after the interval refreshes again"
        );
    }

    #[test]
    fn sample_dead_or_missing_pid_zeroes_the_sample() {
        let mut monitor = ResourceMonitor::new();
        let mut model = WorkspaceModel::new();
        model.apply_inbound(&Inbound::Register(AgentMeta::new("a1", "agent one", 0)));

        // No pid set — sampling must not panic and must leave a zeroed
        // reading.
        monitor.sample(&mut model);
        assert_eq!(model.agents[0].res, ResourceSample::default());
    }

    #[test]
    fn sample_current_process_reports_nonzero_memory() {
        let mut monitor = ResourceMonitor::new();
        let mut model = WorkspaceModel::new();
        let mut meta = AgentMeta::new("self", "current process", 0);
        meta.pid = Some(std::process::id());
        model.apply_inbound(&Inbound::Register(meta));

        // sysinfo needs the process table populated before `process()` can
        // resolve the pid; one `sample()` call does that.
        monitor.sample(&mut model);

        assert!(model.agents[0].res.mem_bytes > 0);
    }
}
