//! The session task board — pure decision logic for the `task_*` tools
//! (`task_create` / `task_list` / `task_complete` / `task_cancel` /
//! `task_assign`).
//!
//! The board is owned data with no I/O: the tools crate mutates it through
//! these methods, the CLI tap snapshots it into `AgentEvent::TaskUpdate`
//! events (the sole render path — the TUI folds snapshots, never reads this
//! struct), and the store mirrors snapshots for cross-session findability.
//! Keeping the transition rules here makes them property-testable without a
//! registry or a runtime.
//!
//! `task_assign` does not spawn anything itself — spawning is I/O. It
//! validates the transition and records a [`SpawnRequest`]; the session
//! driver drains requests and dispatches sub-agents through the fleet seam
//! (L-E9: `Fleet::dispatch` is THE dispatch seam).

use stella_protocol::{TaskItem, TaskStatus};

/// Why a board mutation was rejected. Named errors, never a bare string —
/// the tools surface these verbatim to the model so it can self-correct.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskBoardError {
    #[error("no task with id {id} — call task_list to see the board")]
    UnknownTask { id: String },
    #[error("task {id} is already {status:?} — terminal tasks cannot change state")]
    Terminal { id: String, status: TaskStatus },
}

/// One queued request to spawn a dedicated sub-agent for a task
/// (`task_assign`). Pure data; the driver owns the actual spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    /// The board task this sub-agent works on.
    pub task_id: String,
    pub subject: String,
    pub description: Option<String>,
    /// What the assigning agent wants the sub-agent to know — carried into
    /// the sub-agent's prompt verbatim (the "communication" of
    /// `task_assign`).
    pub briefing: String,
}

/// The task board: an insertion-ordered list of [`TaskItem`]s with ordinal
/// string ids ("1", "2", …). All mutation goes through the methods below so
/// the transition rules hold by construction.
#[derive(Debug, Clone, Default)]
pub struct TaskBoard {
    items: Vec<TaskItem>,
}

impl TaskBoard {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current board, oldest first — what `TaskUpdate` snapshots carry.
    pub fn items(&self) -> &[TaskItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Create a task in `Pending`; returns the new item. Ids are ordinal and
    /// never reused — cancelling task "2" does not renumber task "3", so ids
    /// in the transcript stay valid for the whole session.
    pub fn create(&mut self, subject: impl Into<String>, description: Option<String>) -> &TaskItem {
        let id = (self.items.len() + 1).to_string();
        self.items.push(TaskItem {
            id,
            subject: subject.into(),
            description,
            status: TaskStatus::Pending,
            owner: None,
        });
        self.items.last().expect("just pushed")
    }

    /// Move a task to a new status. Terminal tasks (completed / cancelled)
    /// reject every transition; re-asserting the same open status is a no-op
    /// that succeeds (idempotent `task_complete` retries stay terminal-safe
    /// because terminality is checked first).
    pub fn set_status(
        &mut self,
        id: &str,
        status: TaskStatus,
    ) -> Result<&TaskItem, TaskBoardError> {
        let item = Self::find(&mut self.items, id)?;
        item.status = status;
        Ok(item)
    }

    /// Record an owner lane for a task and mark it in progress — the board
    /// half of `task_assign` (the spawn half is the driver's, via
    /// [`SpawnRequest`]).
    pub fn assign(
        &mut self,
        id: &str,
        owner: impl Into<String>,
    ) -> Result<&TaskItem, TaskBoardError> {
        let item = Self::find(&mut self.items, id)?;
        item.owner = Some(owner.into());
        item.status = TaskStatus::InProgress;
        Ok(item)
    }

    /// Claim a task for a lane without forcing it in progress (the lead
    /// marking itself as owner on create).
    pub fn set_owner(
        &mut self,
        id: &str,
        owner: impl Into<String>,
    ) -> Result<&TaskItem, TaskBoardError> {
        let item = Self::find(&mut self.items, id)?;
        item.owner = Some(owner.into());
        Ok(item)
    }

    fn find<'a>(items: &'a mut [TaskItem], id: &str) -> Result<&'a mut TaskItem, TaskBoardError> {
        let item = items
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| TaskBoardError::UnknownTask { id: id.to_string() })?;
        if !item.status.is_open() {
            return Err(TaskBoardError::Terminal {
                id: item.id.clone(),
                status: item.status,
            });
        }
        Ok(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn ids_are_ordinal_and_survive_cancellation() {
        let mut board = TaskBoard::new();
        board.create("one", None);
        board.create("two", None);
        board.set_status("1", TaskStatus::Cancelled).unwrap();
        board.create("three", None);
        let ids: Vec<&str> = board.items().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["1", "2", "3"]);
        assert_eq!(board.items()[2].subject, "three");
    }

    #[test]
    fn terminal_tasks_reject_every_transition() {
        let mut board = TaskBoard::new();
        board.create("t", None);
        board.set_status("1", TaskStatus::Completed).unwrap();
        for attempt in [
            board.set_status("1", TaskStatus::Pending).unwrap_err(),
            board.set_status("1", TaskStatus::InProgress).unwrap_err(),
            board.assign("1", "sub:1").unwrap_err(),
        ] {
            assert_eq!(
                attempt,
                TaskBoardError::Terminal {
                    id: "1".into(),
                    status: TaskStatus::Completed
                }
            );
        }
    }

    #[test]
    fn unknown_ids_name_the_id_in_the_error() {
        let mut board = TaskBoard::new();
        assert_eq!(
            board.set_status("7", TaskStatus::Completed).unwrap_err(),
            TaskBoardError::UnknownTask { id: "7".into() }
        );
    }

    #[test]
    fn assign_records_owner_and_marks_in_progress() {
        let mut board = TaskBoard::new();
        board.create("t", None);
        let item = board.assign("1", "sub:1").unwrap();
        assert_eq!(item.owner.as_deref(), Some("sub:1"));
        assert_eq!(item.status, TaskStatus::InProgress);
    }

    /// One random board operation, for the fold property below.
    #[derive(Debug, Clone)]
    enum Op {
        Create(String),
        SetStatus(usize, TaskStatus),
        Assign(usize, String),
    }

    fn arb_op() -> impl Strategy<Value = Op> {
        prop_oneof![
            "[a-z]{1,8}".prop_map(Op::Create),
            (0usize..12, arb_status()).prop_map(|(i, s)| Op::SetStatus(i, s)),
            (0usize..12, "[a-z]{1,6}").prop_map(|(i, o)| Op::Assign(i, o)),
        ]
    }

    fn arb_status() -> impl Strategy<Value = TaskStatus> {
        prop_oneof![
            Just(TaskStatus::Pending),
            Just(TaskStatus::InProgress),
            Just(TaskStatus::Completed),
            Just(TaskStatus::Cancelled),
        ]
    }

    proptest! {
        /// Replaying the same op sequence on a fresh board yields an
        /// identical board — the board is a deterministic fold, which is
        /// what lets `TaskUpdate` snapshots reconstruct it anywhere.
        #[test]
        fn board_is_a_deterministic_fold(ops in proptest::collection::vec(arb_op(), 0..40)) {
            let mut a = TaskBoard::new();
            let mut b = TaskBoard::new();
            for board in [&mut a, &mut b] {
                for op in &ops {
                    match op {
                        Op::Create(s) => { board.create(s.clone(), None); }
                        Op::SetStatus(i, s) => { let _ = board.set_status(&(i + 1).to_string(), *s); }
                        Op::Assign(i, o) => { let _ = board.assign(&(i + 1).to_string(), o.clone()); }
                    }
                }
            }
            prop_assert_eq!(a.items(), b.items());
        }

        /// Terminality is absorbing: once a task leaves the open states, no
        /// op sequence can ever change it again.
        #[test]
        fn terminal_states_are_absorbing(ops in proptest::collection::vec(arb_op(), 0..40)) {
            let mut board = TaskBoard::new();
            board.create("pinned", None);
            board.set_status("1", TaskStatus::Cancelled).unwrap();
            let frozen = board.items()[0].clone();
            for op in &ops {
                match op {
                    Op::Create(s) => { board.create(s.clone(), None); }
                    Op::SetStatus(i, s) => { let _ = board.set_status(&(i + 1).to_string(), *s); }
                    Op::Assign(i, o) => { let _ = board.assign(&(i + 1).to_string(), o.clone()); }
                }
            }
            prop_assert_eq!(&board.items()[0], &frozen);
        }
    }
}
