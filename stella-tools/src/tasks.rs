//! The `task_*` tools — the model's handle on the session task board
//! (`stella_core::tasks::TaskBoard`).
//!
//! Six tools share one board: `task_create` / `task_list` / `task_start` /
//! `task_complete` / `task_cancel` mutate or read it, and `task_assign`
//! additionally queues a [`SpawnRequest`] for the session driver to turn
//! into a real sub-agent spawn (spawning is I/O and never happens here —
//! see the module docs on `stella_core::tasks`). The board and the queue
//! follow the [`crate::memory::CitationLedger`] idiom: `Arc<Mutex<…>>`
//! handles shared between the tool instances and the [`crate::ToolRegistry`]
//! that exposes/drains them.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use stella_core::tasks::{SpawnRequest, TaskBoard};
use stella_protocol::tool::{ToolOutput, ToolSchema};
use stella_protocol::{TaskItem, TaskStatus};

use crate::registry::Tool;

/// The session's task board, shared between the six `task_*` tool instances
/// and the `ToolRegistry` (which snapshots it into `AgentEvent::TaskUpdate`
/// via [`crate::ToolRegistry::task_board`]).
pub type TaskBoardHandle = Arc<Mutex<TaskBoard>>;

/// Queued sub-agent spawns recorded by `task_assign`, drained by the session
/// driver via [`crate::ToolRegistry::take_spawn_requests`] — the tool only
/// records intent; the driver owns the actual spawn.
pub type SpawnQueue = Arc<Mutex<Vec<SpawnRequest>>>;

/// Extract a required non-empty string field from model-supplied JSON.
/// Model input is runtime data: missing or mistyped fields return a tool
/// error the model can self-correct from, never a panic.
fn require_str<'a>(input: &'a Value, field: &str) -> Result<&'a str, ToolOutput> {
    match input.get(field).and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        Some(_) => Err(ToolOutput::Error {
            message: format!("field `{field}` must be a non-empty string"),
        }),
        None => Err(ToolOutput::Error {
            message: format!("missing required string field `{field}`"),
        }),
    }
}

/// Optional string field: absent, non-string, or blank all collapse to
/// `None` (defensive — the model sometimes sends `""` for "no value").
fn optional_str(input: &Value, field: &str) -> Option<String> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// One-character board glyph per status, for `task_list` lines.
fn glyph(status: TaskStatus) -> char {
    match status {
        TaskStatus::Pending => ' ',
        TaskStatus::InProgress => '~',
        TaskStatus::Completed => 'x',
        TaskStatus::Cancelled => '-',
    }
}

/// Render one board row: `[x] #1 Fix the redirect loop (sub:1)`.
fn render_line(item: &TaskItem) -> String {
    let mut line = format!("[{}] #{} {}", glyph(item.status), item.id, item.subject);
    if let Some(owner) = &item.owner {
        line.push_str(&format!(" ({owner})"));
    }
    line
}

/// `task_create` — add a `Pending` task to the shared board.
pub struct TaskCreate(pub TaskBoardHandle);

#[async_trait]
impl Tool for TaskCreate {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "task_create".into(),
            description: "Add a task to the session task board — the board is the session's \
                          visible plan, so create tasks BEFORE starting multi-step work, one \
                          per concrete deliverable. New tasks start pending; mark the one you \
                          work on with task_start (or delegate it with task_assign)."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "subject": { "type": "string", "description": "Imperative title (e.g. \"Fix the auth redirect loop\")" },
                    "description": { "type": "string", "description": "What needs to be done, if the subject alone is not enough" }
                },
                "required": ["subject"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let subject = match require_str(input, "subject") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let description = optional_str(input, "description");
        let mut board = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let item = board.create(subject, description);
        let payload = serde_json::json!({
            "id": item.id,
            "subject": item.subject,
            "status": item.status,
        });
        ToolOutput::Ok {
            content: format!("created task {payload}"),
        }
    }
}

/// `task_list` — the whole board, one line per task.
pub struct TaskList(pub TaskBoardHandle);

#[async_trait]
impl Tool for TaskList {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "task_list".into(),
            description: "Show the session task board: every task as `[status] #id subject \
                          (owner)` — [ ] pending, [~] in_progress, [x] completed, [-] \
                          cancelled. Use it to re-check the plan or find a task id."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            read_only: true,
        }
    }

    async fn execute(&self, _input: &Value, _root: &std::path::Path) -> ToolOutput {
        let board = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if board.is_empty() {
            return ToolOutput::Ok {
                content: "no tasks yet — create the plan with task_create before starting \
                          multi-step work"
                    .into(),
            };
        }
        let content = board
            .items()
            .iter()
            .map(render_line)
            .collect::<Vec<_>>()
            .join("\n");
        ToolOutput::Ok { content }
    }
}

/// `task_start` — the lead marking the task it is personally working on.
pub struct TaskStart(pub TaskBoardHandle);

#[async_trait]
impl Tool for TaskStart {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "task_start".into(),
            description: "Mark a board task in_progress — the task you are personally working \
                          on right now. Keep exactly ONE task in_progress at a time: complete \
                          the current task before starting the next. (task_assign marks \
                          delegated tasks in_progress by itself — task_start is for your own \
                          work.)"
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The task id from task_create / task_list" }
                },
                "required": ["id"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let id = match require_str(input, "id") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let mut board = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match board.set_status(id, TaskStatus::InProgress) {
            Ok(item) => ToolOutput::Ok {
                content: format!("task #{} `{}` is now in_progress", item.id, item.subject),
            },
            Err(e) => ToolOutput::Error {
                message: e.to_string(),
            },
        }
    }
}

/// `task_complete` — move a task to the terminal `Completed` state.
pub struct TaskComplete(pub TaskBoardHandle);

#[async_trait]
impl Tool for TaskComplete {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "task_complete".into(),
            description: "Mark a board task completed the moment its work is done and \
                          verified. Complete tasks as you finish them — that is what keeps \
                          exactly one task in_progress and the board honest. Completed is \
                          terminal."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The task id from task_create / task_list" }
                },
                "required": ["id"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let id = match require_str(input, "id") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let mut board = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match board.set_status(id, TaskStatus::Completed) {
            Ok(item) => ToolOutput::Ok {
                content: format!("task #{} `{}` completed", item.id, item.subject),
            },
            Err(e) => ToolOutput::Error {
                message: e.to_string(),
            },
        }
    }
}

/// `task_cancel` — move a task to the terminal `Cancelled` state.
pub struct TaskCancel(pub TaskBoardHandle);

#[async_trait]
impl Tool for TaskCancel {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "task_cancel".into(),
            description: "Cancel a board task that is no longer worth doing (superseded, out \
                          of scope, impossible). Give a short reason. Cancelled tasks keep \
                          their row as an audit trail; cancelled is terminal."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The task id from task_create / task_list" },
                    "reason": { "type": "string", "description": "Why the task is being dropped" }
                },
                "required": ["id"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let id = match require_str(input, "id") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let reason = optional_str(input, "reason");
        let mut board = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match board.set_status(id, TaskStatus::Cancelled) {
            Ok(item) => {
                let mut content = format!("task #{} `{}` cancelled", item.id, item.subject);
                if let Some(reason) = reason {
                    content.push_str(&format!(" — {reason}"));
                }
                ToolOutput::Ok { content }
            }
            Err(e) => ToolOutput::Error {
                message: e.to_string(),
            },
        }
    }
}

/// `task_assign` — delegate a task: mark it in progress under an owner lane
/// AND queue a [`SpawnRequest`] for the driver to spawn a dedicated
/// sub-agent from.
pub struct TaskAssign(pub TaskBoardHandle, pub SpawnQueue);

#[async_trait]
impl Tool for TaskAssign {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "task_assign".into(),
            description: "Delegate a board task to a parallel sub-agent. The task is marked \
                          in_progress under the new owner and a dedicated sub-agent is \
                          spawned for it; the briefing is passed to that sub-agent VERBATIM, \
                          so write it self-contained (context, file paths, definition of \
                          done). The sub-agent's results land back in this session."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The task id from task_create / task_list" },
                    "briefing": { "type": "string", "description": "Everything the sub-agent needs to do the task, passed to it verbatim" },
                    "owner": { "type": "string", "description": "Owner lane label (default: sub:<id>)" }
                },
                "required": ["id", "briefing"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, _root: &std::path::Path) -> ToolOutput {
        let id = match require_str(input, "id") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let briefing = match require_str(input, "briefing") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let owner = optional_str(input, "owner").unwrap_or_else(|| format!("sub:{id}"));
        // Validate the transition on the board first; only a successful
        // assignment may queue a spawn (a terminal/unknown task must never
        // spawn a worker).
        let (subject, description) = {
            let mut board = self.0.lock().unwrap_or_else(|p| p.into_inner());
            match board.assign(id, owner.clone()) {
                Ok(item) => (item.subject.clone(), item.description.clone()),
                Err(e) => {
                    return ToolOutput::Error {
                        message: e.to_string(),
                    };
                }
            }
        };
        self.1
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(SpawnRequest {
                task_id: id.to_string(),
                subject,
                description,
                briefing: briefing.to_string(),
            });
        ToolOutput::Ok {
            content: format!(
                "task #{id} assigned to {owner} — a dedicated sub-agent will be spawned for \
                 it with your briefing passed verbatim; its results will land back in this \
                 session"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handles() -> (TaskBoardHandle, SpawnQueue) {
        (Arc::default(), Arc::default())
    }

    fn root() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    async fn exec(tool: &dyn Tool, input: Value) -> ToolOutput {
        tool.execute(&input, &root()).await
    }

    fn content(output: ToolOutput) -> String {
        match output {
            ToolOutput::Ok { content } => content,
            ToolOutput::Error { message } => panic!("expected ok, got error: {message}"),
        }
    }

    fn error(output: ToolOutput) -> String {
        match output {
            ToolOutput::Error { message } => message,
            ToolOutput::Ok { content } => panic!("expected error, got ok: {content}"),
        }
    }

    #[test]
    fn all_six_tools_advertise_task_schemas() {
        let (board, queue) = handles();
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(TaskCreate(board.clone())),
            Box::new(TaskList(board.clone())),
            Box::new(TaskStart(board.clone())),
            Box::new(TaskComplete(board.clone())),
            Box::new(TaskCancel(board.clone())),
            Box::new(TaskAssign(board, queue)),
        ];
        let schemas: Vec<ToolSchema> = tools.iter().map(|t| t.schema()).collect();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "task_create",
                "task_list",
                "task_start",
                "task_complete",
                "task_cancel",
                "task_assign",
            ]
        );
        for schema in &schemas {
            // Only the pure board read may be parallelized by the engine.
            assert_eq!(
                schema.read_only,
                schema.name == "task_list",
                "{}",
                schema.name
            );
            assert_eq!(schema.input_schema["type"], "object", "{}", schema.name);
        }
        assert_eq!(
            schemas[0].input_schema["required"],
            serde_json::json!(["subject"])
        );
        assert_eq!(
            schemas[5].input_schema["required"],
            serde_json::json!(["id", "briefing"])
        );
    }

    #[tokio::test]
    async fn create_returns_ids_and_list_shows_the_board() {
        let (board, _queue) = handles();
        let created = content(
            exec(
                &TaskCreate(board.clone()),
                serde_json::json!({"subject": "Fix the redirect loop", "description": "302 loops on /login"}),
            )
            .await,
        );
        assert!(created.contains("\"id\":\"1\""), "{created}");
        assert!(created.contains("\"status\":\"pending\""), "{created}");
        content(
            exec(
                &TaskCreate(board.clone()),
                serde_json::json!({"subject": "Add tests"}),
            )
            .await,
        );

        let listed = content(exec(&TaskList(board), serde_json::json!({})).await);
        assert_eq!(listed, "[ ] #1 Fix the redirect loop\n[ ] #2 Add tests");
    }

    #[tokio::test]
    async fn empty_board_lists_no_tasks_yet() {
        let (board, _queue) = handles();
        let listed = content(exec(&TaskList(board), serde_json::json!({})).await);
        assert!(listed.contains("no tasks yet"), "{listed}");
    }

    #[tokio::test]
    async fn complete_is_terminal_and_a_repeat_errors_naming_the_id() {
        let (board, _queue) = handles();
        content(
            exec(
                &TaskCreate(board.clone()),
                serde_json::json!({"subject": "t"}),
            )
            .await,
        );
        let done =
            content(exec(&TaskComplete(board.clone()), serde_json::json!({"id": "1"})).await);
        assert!(done.contains("#1"), "{done}");
        assert_eq!(
            board.lock().unwrap().items()[0].status,
            TaskStatus::Completed
        );

        // A second complete hits the terminal guard; the error names the id
        // (TaskBoardError Display, surfaced verbatim).
        let again = error(exec(&TaskComplete(board.clone()), serde_json::json!({"id": "1"})).await);
        assert!(again.contains("1"), "{again}");
        assert!(again.contains("terminal"), "{again}");

        // Unknown ids also surface the board's own message.
        let unknown = error(exec(&TaskComplete(board), serde_json::json!({"id": "9"})).await);
        assert!(unknown.contains("no task with id 9"), "{unknown}");
    }

    #[tokio::test]
    async fn start_marks_in_progress_for_the_leads_own_work() {
        let (board, _queue) = handles();
        content(
            exec(
                &TaskCreate(board.clone()),
                serde_json::json!({"subject": "t"}),
            )
            .await,
        );
        let started =
            content(exec(&TaskStart(board.clone()), serde_json::json!({"id": "1"})).await);
        assert!(started.contains("in_progress"), "{started}");
        assert_eq!(
            board.lock().unwrap().items()[0].status,
            TaskStatus::InProgress
        );
    }

    #[tokio::test]
    async fn cancel_echoes_the_reason_in_the_confirmation() {
        let (board, _queue) = handles();
        content(
            exec(
                &TaskCreate(board.clone()),
                serde_json::json!({"subject": "t"}),
            )
            .await,
        );
        let cancelled = content(
            exec(
                &TaskCancel(board.clone()),
                serde_json::json!({"id": "1", "reason": "superseded by task 2"}),
            )
            .await,
        );
        assert!(cancelled.contains("cancelled"), "{cancelled}");
        assert!(cancelled.contains("superseded by task 2"), "{cancelled}");
        // The reason lives in the confirmation only — the board item carries
        // no reason field.
        assert_eq!(
            board.lock().unwrap().items()[0].status,
            TaskStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn assign_marks_the_board_and_queues_a_spawn_request() {
        let (board, queue) = handles();
        content(
            exec(
                &TaskCreate(board.clone()),
                serde_json::json!({"subject": "Port the parser", "description": "see docs/parser.md"}),
            )
            .await,
        );
        let assigned = content(
            exec(
                &TaskAssign(board.clone(), queue.clone()),
                serde_json::json!({"id": "1", "briefing": "Port src/parse.rs to the new AST"}),
            )
            .await,
        );
        assert!(assigned.contains("sub-agent"), "{assigned}");
        assert!(assigned.contains("sub:1"), "{assigned}");

        let item = board.lock().unwrap().items()[0].clone();
        assert_eq!(item.owner.as_deref(), Some("sub:1"));
        assert_eq!(item.status, TaskStatus::InProgress);

        let queued = queue.lock().unwrap().clone();
        assert_eq!(
            queued,
            vec![SpawnRequest {
                task_id: "1".into(),
                subject: "Port the parser".into(),
                description: Some("see docs/parser.md".into()),
                briefing: "Port src/parse.rs to the new AST".into(),
            }]
        );
    }

    #[tokio::test]
    async fn assign_honors_an_explicit_owner_and_a_terminal_task_never_spawns() {
        let (board, queue) = handles();
        content(
            exec(
                &TaskCreate(board.clone()),
                serde_json::json!({"subject": "a"}),
            )
            .await,
        );
        content(
            exec(
                &TaskCreate(board.clone()),
                serde_json::json!({"subject": "b"}),
            )
            .await,
        );
        content(exec(&TaskCancel(board.clone()), serde_json::json!({"id": "2"})).await);

        let ok = content(
            exec(
                &TaskAssign(board.clone(), queue.clone()),
                serde_json::json!({"id": "1", "briefing": "go", "owner": "worker-7"}),
            )
            .await,
        );
        assert!(ok.contains("worker-7"), "{ok}");
        assert_eq!(
            board.lock().unwrap().items()[0].owner.as_deref(),
            Some("worker-7")
        );

        // Assigning a cancelled task fails on the board and must not queue.
        let bad = error(
            exec(
                &TaskAssign(board, queue.clone()),
                serde_json::json!({"id": "2", "briefing": "go"}),
            )
            .await,
        );
        assert!(bad.contains("terminal"), "{bad}");
        assert_eq!(queue.lock().unwrap().len(), 1, "no spawn for terminal task");
    }

    #[tokio::test]
    async fn malformed_input_errors_without_panicking() {
        let (board, queue) = handles();
        // Missing / mistyped subject on create.
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"subject": ""}),
            serde_json::json!({"subject": 7}),
        ] {
            let msg = error(exec(&TaskCreate(board.clone()), bad).await);
            assert!(msg.contains("subject"), "{msg}");
        }
        // Missing id on the id-taking tools.
        let start = error(exec(&TaskStart(board.clone()), serde_json::json!({})).await);
        assert!(start.contains("id"), "{start}");
        let complete = error(exec(&TaskComplete(board.clone()), serde_json::json!({})).await);
        assert!(complete.contains("id"), "{complete}");
        let cancel = error(exec(&TaskCancel(board.clone()), serde_json::json!({})).await);
        assert!(cancel.contains("id"), "{cancel}");
        // Assign without a briefing must not touch board or queue.
        let assign = error(
            exec(
                &TaskAssign(board.clone(), queue.clone()),
                serde_json::json!({"id": "1"}),
            )
            .await,
        );
        assert!(assign.contains("briefing"), "{assign}");
        assert!(board.lock().unwrap().is_empty());
        assert!(queue.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn registry_dispatches_task_tools_and_drains_spawn_requests() {
        let dir = tempfile::tempdir().unwrap();
        let reg = crate::ToolRegistry::with_issue_backend(dir.path().to_path_buf(), None);

        let out = reg
            .execute(
                "task_create",
                &serde_json::json!({"subject": "delegate me"}),
            )
            .await;
        assert!(!out.is_error(), "{out:?}");
        let out = reg
            .execute(
                "task_assign",
                &serde_json::json!({"id": "1", "briefing": "do the thing"}),
            )
            .await;
        assert!(!out.is_error(), "{out:?}");

        // The shared handles see what the tools did...
        let board = reg.task_board();
        assert_eq!(
            board.lock().unwrap().items()[0].status,
            TaskStatus::InProgress
        );
        // ...and the drain hands each request over exactly once.
        let drained = reg.take_spawn_requests();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].briefing, "do the thing");
        assert!(
            reg.take_spawn_requests().is_empty(),
            "a drained spawn request must never be dispatched twice"
        );
        assert!(reg.spawn_queue().lock().unwrap().is_empty());
    }
}
