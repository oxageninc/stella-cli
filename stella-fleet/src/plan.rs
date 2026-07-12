//! The planner DAG (`02-architecture.md` §2 "planner DAG";
//! `03-plan.md` Phase 5 item 2). Pure scheduling logic over owned data — no
//! I/O, no async — so every property is table- and proptest-checkable
//! (`02-architecture.md` §1.3).
//!
//! A [`Plan`] is a set of [`Task`]s with `depends_on` edges. The fleet
//! executes it wave by wave: [`Plan::ready_tasks`] returns the tasks whose
//! dependencies are all satisfied, the caller dispatches that wave
//! concurrently, records the completions, and asks again — until the plan is
//! drained. [`Plan::topological_order`] is the linear-schedule view of the
//! same graph (used for display and single-threaded execution).
//!
//! Tasks isolate by default ([`Isolation::Isolated`] — a dedicated git
//! worktree per task): a fleet worker must never be able to see or clobber a
//! sibling's uncommitted files unless the plan author *explicitly* opts a
//! task into [`Isolation::SharedTree`] (`02-architecture.md` §2
//! "git-worktree isolation").

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// A task's stable identifier within a plan. Unique per [`Plan`] (enforced by
/// [`Plan::validate`]); used as the deterministic tie-break key when several
/// tasks are simultaneously ready, and as the seed for the worktree branch /
/// directory name.
pub type TaskId = String;

/// How a task's workspace is provisioned. **Isolate by default**: unless a
/// plan author deliberately chooses [`SharedTree`](Isolation::SharedTree), a
/// task runs in its own git worktree so nothing it writes is visible to (or
/// clobberable by) a concurrently-running sibling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A dedicated git worktree on a fresh branch (the default). Sibling
    /// tasks in the same wave cannot see each other's uncommitted changes.
    #[default]
    Isolated,
    /// Runs directly in the shared repository root. Only for tasks a plan
    /// author has explicitly decided are safe to interleave in one tree
    /// (e.g. strictly-sequential tasks, or read-only ones).
    SharedTree,
}

/// One unit of fleet work: a prompt to run against a workspace, plus the
/// tasks it must wait for and how its workspace is isolated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub prompt: String,
    /// Ids of tasks that must complete before this one may start. Every id
    /// here must name a task in the same plan (enforced by
    /// [`Plan::validate`]).
    #[serde(default)]
    pub depends_on: Vec<TaskId>,
    /// Isolation strategy — defaults to [`Isolation::Isolated`] when absent
    /// from serialized input.
    #[serde(default)]
    pub isolation: Isolation,
}

impl Task {
    /// A dependency-free, isolate-by-default task.
    pub fn new(id: impl Into<TaskId>, title: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            prompt: prompt.into(),
            depends_on: Vec::new(),
            isolation: Isolation::Isolated,
        }
    }

    /// Add dependency edges (builder style).
    pub fn depends_on(mut self, deps: impl IntoIterator<Item = impl Into<TaskId>>) -> Self {
        self.depends_on.extend(deps.into_iter().map(Into::into));
        self
    }

    /// Opt this task into running in the shared tree instead of an isolated
    /// worktree (builder style). The deliberate exception to isolate-by-
    /// default.
    pub fn shared_tree(mut self) -> Self {
        self.isolation = Isolation::SharedTree;
        self
    }
}

/// A DAG of [`Task`]s. Construct with [`Plan::new`], then validate/schedule.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Plan {
    pub tasks: Vec<Task>,
}

/// Why a plan is not schedulable — always a named, actionable error, never a
/// silent drop (the DAG analog of the router's "unknown slug is a loud
/// error" discipline).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// Two tasks share an id — ids must be unique for dependencies to
    /// reference unambiguously.
    #[error("duplicate task id `{id}` — every task in a plan must have a unique id")]
    DuplicateTaskId { id: TaskId },

    /// A `depends_on` names a task that isn't in the plan.
    #[error("task `{task}` depends on `{depends_on}`, which is not a task in this plan")]
    UnknownDependency { task: TaskId, depends_on: TaskId },

    /// A task lists itself in `depends_on`.
    #[error("task `{id}` depends on itself")]
    SelfDependency { id: TaskId },

    /// The dependency graph contains a cycle. `cycle` lists the ids of the
    /// cycle in order (each depends on the next; the last depends on the
    /// first) so the author can see exactly which edges to cut.
    #[error("dependency cycle: {}", format_cycle(.cycle))]
    Cycle { cycle: Vec<TaskId> },
}

fn format_cycle(cycle: &[TaskId]) -> String {
    if cycle.is_empty() {
        return "<empty>".to_string();
    }
    let mut chain = cycle.join(" -> ");
    chain.push_str(" -> ");
    chain.push_str(&cycle[0]);
    chain
}

impl Plan {
    pub fn new(tasks: Vec<Task>) -> Self {
        Self { tasks }
    }

    /// Look a task up by id.
    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Check structural integrity: unique ids, no self-dependency, every
    /// `depends_on` resolvable, and no cycles. Returns the first problem
    /// found (checked in that order). [`topological_order`](Self::topological_order)
    /// calls this first.
    pub fn validate(&self) -> Result<(), PlanError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.tasks.len());
        for task in &self.tasks {
            if !seen.insert(task.id.as_str()) {
                return Err(PlanError::DuplicateTaskId {
                    id: task.id.clone(),
                });
            }
        }
        for task in &self.tasks {
            for dep in &task.depends_on {
                if dep == &task.id {
                    return Err(PlanError::SelfDependency {
                        id: task.id.clone(),
                    });
                }
                if !seen.contains(dep.as_str()) {
                    return Err(PlanError::UnknownDependency {
                        task: task.id.clone(),
                        depends_on: dep.clone(),
                    });
                }
            }
        }
        if let Some(cycle) = self.find_cycle() {
            return Err(PlanError::Cycle { cycle });
        }
        Ok(())
    }

    /// A single linear schedule respecting every dependency edge, with a
    /// deterministic tie-break (lexicographically smallest ready id first)
    /// so the same plan always yields the same order. Errors exactly as
    /// [`validate`](Self::validate).
    pub fn topological_order(&self) -> Result<Vec<&Task>, PlanError> {
        self.validate()?;

        let index: HashMap<&str, &Task> = self.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        // BTreeMap keeps a stable id ordering for reproducible iteration.
        let mut indegree: BTreeMap<&str, usize> =
            self.tasks.iter().map(|t| (t.id.as_str(), 0usize)).collect();
        let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
        for task in &self.tasks {
            for dep in &task.depends_on {
                dependents
                    .entry(dep.as_str())
                    .or_default()
                    .push(task.id.as_str());
                if let Some(deg) = indegree.get_mut(task.id.as_str()) {
                    *deg += 1;
                }
            }
        }

        // Min-heap over &str (via Reverse) → deterministic smallest-id-first.
        let mut ready: BinaryHeap<Reverse<&str>> = indegree
            .iter()
            .filter(|&(_, &deg)| deg == 0)
            .map(|(&id, _)| Reverse(id))
            .collect();

        let mut order = Vec::with_capacity(self.tasks.len());
        while let Some(Reverse(id)) = ready.pop() {
            if let Some(&task) = index.get(id) {
                order.push(task);
            }
            if let Some(children) = dependents.get(id) {
                for &child in children {
                    if let Some(deg) = indegree.get_mut(child) {
                        *deg -= 1;
                        if *deg == 0 {
                            ready.push(Reverse(child));
                        }
                    }
                }
            }
        }

        // validate() already rejects cycles, so this is defensive only.
        if order.len() != self.tasks.len() {
            return Err(PlanError::Cycle {
                cycle: self.find_cycle().unwrap_or_default(),
            });
        }
        Ok(order)
    }

    /// The tasks that may start now: not yet in `completed`, and with every
    /// dependency in `completed`. Sorted by id for a deterministic dispatch
    /// order. This is the wave-scheduling primitive — call it, dispatch the
    /// returned wave, add the finished ids to `completed`, and repeat until
    /// it returns empty.
    pub fn ready_tasks(&self, completed: &HashSet<TaskId>) -> Vec<&Task> {
        let mut ready: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|t| !completed.contains(&t.id))
            .filter(|t| t.depends_on.iter().all(|d| completed.contains(d)))
            .collect();
        ready.sort_by(|a, b| a.id.cmp(&b.id));
        ready
    }

    /// Find one dependency cycle, if any, as an ordered id list (each depends
    /// on the next). Iterative colored DFS (white=unseen, gray=on the current
    /// path, black=done); a gray target is a back edge — the cycle is the
    /// current path from that target onward. Panic-free: every map access is
    /// guarded.
    fn find_cycle(&self) -> Option<Vec<TaskId>> {
        // Adjacency: task -> its dependencies (edges we walk).
        let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut ids: Vec<&str> = Vec::with_capacity(self.tasks.len());
        for task in &self.tasks {
            ids.push(task.id.as_str());
            deps.insert(
                task.id.as_str(),
                task.depends_on.iter().map(|d| d.as_str()).collect(),
            );
        }

        const WHITE: u8 = 0;
        const GRAY: u8 = 1;
        const BLACK: u8 = 2;
        let mut color: HashMap<&str, u8> = ids.iter().map(|&id| (id, WHITE)).collect();

        for &start in &ids {
            if color.get(start).copied().unwrap_or(BLACK) != WHITE {
                continue;
            }
            // Explicit stack of (node, next-child-index) plus the gray path.
            let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
            let mut path: Vec<&str> = vec![start];
            color.insert(start, GRAY);

            while let Some(&(node, idx)) = stack.last() {
                let children: &[&str] = deps.get(node).map(Vec::as_slice).unwrap_or(&[]);
                if idx < children.len() {
                    if let Some(top) = stack.last_mut() {
                        top.1 = idx + 1;
                    }
                    let child = children[idx];
                    match color.get(child).copied().unwrap_or(BLACK) {
                        WHITE => {
                            color.insert(child, GRAY);
                            path.push(child);
                            stack.push((child, 0));
                        }
                        GRAY => {
                            // Back edge → cycle is path[pos..] where the
                            // target first appears on the current path.
                            if let Some(pos) = path.iter().position(|&x| x == child) {
                                return Some(path[pos..].iter().map(|s| s.to_string()).collect());
                            }
                        }
                        _ => {}
                    }
                } else {
                    color.insert(node, BLACK);
                    path.pop();
                    stack.pop();
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ids(tasks: &[&Task]) -> Vec<String> {
        tasks.iter().map(|t| t.id.clone()).collect()
    }

    fn completed(ids: &[&str]) -> HashSet<TaskId> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_plan_is_valid_and_schedules_to_nothing() {
        let plan = Plan::default();
        assert!(plan.validate().is_ok());
        assert!(plan.topological_order().unwrap().is_empty());
        assert!(plan.ready_tasks(&HashSet::new()).is_empty());
    }

    #[test]
    fn linear_chain_schedules_in_dependency_order() {
        let plan = Plan::new(vec![
            Task::new("c", "c", "p").depends_on(["b"]),
            Task::new("b", "b", "p").depends_on(["a"]),
            Task::new("a", "a", "p"),
        ]);
        let order = ids(&plan.topological_order().unwrap());
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn independent_tasks_break_ties_lexicographically_and_deterministically() {
        let plan = Plan::new(vec![
            Task::new("z", "z", "p"),
            Task::new("m", "m", "p"),
            Task::new("a", "a", "p"),
        ]);
        // Deterministic across repeated calls.
        for _ in 0..5 {
            assert_eq!(ids(&plan.topological_order().unwrap()), vec!["a", "m", "z"]);
        }
    }

    #[test]
    fn diamond_dependency_puts_root_first_and_join_last() {
        // a -> {b, c} -> d
        let plan = Plan::new(vec![
            Task::new("d", "d", "p").depends_on(["b", "c"]),
            Task::new("b", "b", "p").depends_on(["a"]),
            Task::new("c", "c", "p").depends_on(["a"]),
            Task::new("a", "a", "p"),
        ]);
        let order = ids(&plan.topological_order().unwrap());
        assert_eq!(order.first().map(String::as_str), Some("a"));
        assert_eq!(order.last().map(String::as_str), Some("d"));
        // b before d, c before d.
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("b") < pos("d"));
        assert!(pos("c") < pos("d"));
    }

    #[test]
    fn ready_tasks_only_returns_dependency_satisfied_uncompleted_tasks() {
        let plan = Plan::new(vec![
            Task::new("a", "a", "p"),
            Task::new("b", "b", "p").depends_on(["a"]),
            Task::new("c", "c", "p").depends_on(["a"]),
        ]);
        // Nothing done: only a is ready.
        assert_eq!(ids(&plan.ready_tasks(&HashSet::new())), vec!["a"]);
        // a done: b and c ready, sorted.
        assert_eq!(ids(&plan.ready_tasks(&completed(&["a"]))), vec!["b", "c"]);
        // a, b done: only c (a not re-offered).
        assert_eq!(ids(&plan.ready_tasks(&completed(&["a", "b"]))), vec!["c"]);
        // all done: empty.
        assert!(plan.ready_tasks(&completed(&["a", "b", "c"])).is_empty());
    }

    #[test]
    fn duplicate_task_id_is_a_named_error() {
        let plan = Plan::new(vec![
            Task::new("a", "a", "p"),
            Task::new("a", "a again", "p"),
        ]);
        assert_eq!(
            plan.validate().unwrap_err(),
            PlanError::DuplicateTaskId { id: "a".into() }
        );
    }

    #[test]
    fn unknown_dependency_is_a_named_error() {
        let plan = Plan::new(vec![Task::new("a", "a", "p").depends_on(["ghost"])]);
        assert_eq!(
            plan.validate().unwrap_err(),
            PlanError::UnknownDependency {
                task: "a".into(),
                depends_on: "ghost".into(),
            }
        );
    }

    #[test]
    fn self_dependency_is_a_named_error() {
        let plan = Plan::new(vec![Task::new("a", "a", "p").depends_on(["a"])]);
        assert_eq!(
            plan.validate().unwrap_err(),
            PlanError::SelfDependency { id: "a".into() }
        );
    }

    #[test]
    fn a_two_node_cycle_is_detected_and_named() {
        let plan = Plan::new(vec![
            Task::new("a", "a", "p").depends_on(["b"]),
            Task::new("b", "b", "p").depends_on(["a"]),
        ]);
        match plan.validate().unwrap_err() {
            PlanError::Cycle { cycle } => {
                assert_eq!(cycle.len(), 2);
                assert!(cycle.contains(&"a".to_string()));
                assert!(cycle.contains(&"b".to_string()));
            }
            other => panic!("expected a cycle error, got {other:?}"),
        }
    }

    #[test]
    fn a_three_node_cycle_is_detected() {
        // a -> b -> c -> a
        let plan = Plan::new(vec![
            Task::new("a", "a", "p").depends_on(["c"]),
            Task::new("b", "b", "p").depends_on(["a"]),
            Task::new("c", "c", "p").depends_on(["b"]),
        ]);
        assert!(matches!(
            plan.topological_order().unwrap_err(),
            PlanError::Cycle { .. }
        ));
    }

    #[test]
    fn cycle_error_message_shows_the_edge_loop() {
        let plan = Plan::new(vec![
            Task::new("a", "a", "p").depends_on(["b"]),
            Task::new("b", "b", "p").depends_on(["a"]),
        ]);
        let msg = plan.validate().unwrap_err().to_string();
        assert!(
            msg.contains("->"),
            "cycle message must show the loop: {msg}"
        );
    }

    #[test]
    fn plan_json_roundtrips_with_isolation_defaulting_to_isolated() {
        // A task serialized without an `isolation` field deserializes as
        // Isolated (isolate-by-default), and one without `depends_on` gets an
        // empty edge list.
        let json = r#"{"tasks":[{"id":"a","title":"t","prompt":"p"}]}"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.tasks[0].isolation, Isolation::Isolated);
        assert!(plan.tasks[0].depends_on.is_empty());

        let shared = Task::new("b", "t", "p").shared_tree();
        let back: Task = serde_json::from_str(&serde_json::to_string(&shared).unwrap()).unwrap();
        assert_eq!(back.isolation, Isolation::SharedTree);
    }

    // ---- Properties ---------------------------------------------------

    /// Build an *acyclic* plan of `n` tasks where task `i` may only depend on
    /// strictly-lower-indexed tasks — a construction that is acyclic by
    /// design, letting us assert the scheduler respects every edge.
    fn acyclic_plan_strategy() -> impl Strategy<Value = Plan> {
        (1usize..12).prop_flat_map(|n| {
            // For each task i, a subset of {0..i} as dependencies.
            let per_task: Vec<_> = (0..n)
                .map(|i| proptest::collection::vec(0..i.max(1), 0..=i))
                .collect();
            per_task.prop_map(move |deps_by_index| {
                let tasks = (0..n)
                    .map(|i| {
                        let deps: Vec<String> = deps_by_index[i]
                            .iter()
                            .copied()
                            .filter(|&d| d < i) // strictly lower → acyclic, no self-dep
                            .collect::<HashSet<_>>()
                            .into_iter()
                            .map(|d| format!("t{d}"))
                            .collect();
                        Task::new(format!("t{i}"), format!("task {i}"), "p").depends_on(deps)
                    })
                    .collect();
                Plan::new(tasks)
            })
        })
    }

    proptest! {
        /// Every acyclic plan validates, and its topological order lists each
        /// task strictly after all of its dependencies.
        #[test]
        fn topological_order_respects_all_edges(plan in acyclic_plan_strategy()) {
            prop_assert!(plan.validate().is_ok());
            let order = plan.topological_order().expect("acyclic plan schedules");
            prop_assert_eq!(order.len(), plan.tasks.len());
            let position: HashMap<&str, usize> = order
                .iter()
                .enumerate()
                .map(|(i, t)| (t.id.as_str(), i))
                .collect();
            for task in &plan.tasks {
                for dep in &task.depends_on {
                    prop_assert!(
                        position[dep.as_str()] < position[task.id.as_str()],
                        "dependency {} must schedule before {}",
                        dep,
                        task.id
                    );
                }
            }
        }

        /// Wave scheduling drains the whole plan and never offers a task
        /// before its dependencies are complete — the exact loop the fleet
        /// runs. Terminates for every acyclic plan.
        #[test]
        fn wave_scheduling_drains_the_plan_respecting_edges(plan in acyclic_plan_strategy()) {
            let mut done: HashSet<TaskId> = HashSet::new();
            let mut guard = 0;
            while done.len() < plan.tasks.len() {
                let wave = plan.ready_tasks(&done);
                prop_assert!(!wave.is_empty(), "an acyclic plan must always have progress");
                for task in wave {
                    // Every dependency was completed in an earlier wave.
                    for dep in &task.depends_on {
                        prop_assert!(done.contains(dep));
                    }
                    done.insert(task.id.clone());
                }
                guard += 1;
                prop_assert!(guard <= plan.tasks.len() + 1, "must terminate in <= n waves");
            }
            prop_assert_eq!(done.len(), plan.tasks.len());
        }

        /// A deliberately-injected 2-cycle between the first two tasks (each
        /// depends on the other) — guaranteed cyclic on top of any acyclic
        /// forward edges — is always detected.
        #[test]
        fn injected_cycles_are_always_detected(plan in acyclic_plan_strategy()) {
            prop_assume!(plan.tasks.len() >= 2);
            let mut cyclic = plan.clone();
            let id0 = cyclic.tasks[0].id.clone();
            let id1 = cyclic.tasks[1].id.clone();
            cyclic.tasks[0].depends_on.push(id1); // t0 -> t1
            cyclic.tasks[1].depends_on.push(id0); // t1 -> t0  => a 2-cycle
            let detected = matches!(cyclic.validate(), Err(PlanError::Cycle { .. }));
            prop_assert!(detected, "an injected 2-cycle must be detected");
        }
    }
}
