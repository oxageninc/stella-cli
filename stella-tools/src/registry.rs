//! Tool trait + registry. The agent loop drives every tool through
//! `ToolRegistry::execute` — no tool-specific code lives outside this crate.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use stella_core::ports::ToolExecutor;
use stella_protocol::tool::{ToolOutput, ToolSchema};

/// One tool the agent can call. Input arrives as the model-produced JSON;
/// output is always a typed `ToolOutput` (never a bare string).
#[async_trait]
pub trait Tool: Send + Sync {
    /// Schema advertised to the model: name, description, JSON Schema.
    fn schema(&self) -> ToolSchema;

    /// Execute the tool. `root` is the workspace root for path resolution;
    /// tools must never read or write outside it without explicit opt-in.
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput;
}

/// One file-lifecycle op, in CRUD display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    Create,
    Read,
    Update,
    Delete,
}

impl FileOp {
    pub fn letter(self) -> char {
        match self {
            FileOp::Create => 'C',
            FileOp::Read => 'R',
            FileOp::Update => 'U',
            FileOp::Delete => 'D',
        }
    }
}

/// Registry of all built-in tools, keyed by name. Also the session's
/// file-lifecycle ledger: every successful read/write/edit/delete is
/// recorded per path (insertion-ordered), rendered by the TUI as the
/// "Files Touched" panel and persisted as telemetry.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    root: PathBuf,
    touched: std::sync::Mutex<Vec<(String, Vec<FileOp>)>>,
    schema_index: std::sync::Mutex<SchemaIndex>,
}

/// The known schema objects in the workspace, used by the schema gate to
/// prevent duplicate table/type/view creation. Populated from the code graph
/// or empty when no graph is open.
#[derive(Debug, Clone, Default)]
pub struct SchemaIndex {
    pub tables: HashSet<String>,
    pub types: HashSet<String>,
    pub views: HashSet<String>,
}

impl ToolRegistry {
    /// Construct with auto-detected optional backends (issue tracker).
    pub fn new(root: PathBuf) -> Self {
        Self::with_issue_backend(root, crate::issues::detect_issue_backend())
    }

    /// Construct with an explicit issue backend (or none) — the seam unit
    /// tests use so tool counts don't depend on the host's `gh` auth.
    pub fn with_issue_backend(
        root: PathBuf,
        issue_backend: Option<crate::issues::IssueBackend>,
    ) -> Self {
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        let mut entries: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::read::ReadFile),
            Arc::new(crate::write::WriteFile),
            Arc::new(crate::edit::EditFile),
            Arc::new(crate::delete::DeleteFile),
            Arc::new(crate::bash::Bash),
            Arc::new(crate::grep::Grep),
            Arc::new(crate::glob::Glob),
            Arc::new(crate::exploration::Explorations),
            Arc::new(crate::exploration::SaveExploration),
            Arc::new(crate::memory::SaveMemory),
            Arc::new(crate::verify::VerifyDone),
            Arc::new(crate::project::BuildProject),
            Arc::new(crate::project::RunTests),
            Arc::new(crate::ci::CiStatus),
            Arc::new(crate::screenshot::Screenshot),
        ];
        // Issue tools exist only when a tracker is configured — no dead
        // schema entries burning tokens, no surface that errors on use.
        if let Some(backend) = issue_backend {
            let backend = Arc::new(backend);
            entries.push(Arc::new(crate::issues::CreateIssue(backend.clone())));
            entries.push(Arc::new(crate::issues::UpdateIssue(backend.clone())));
            entries.push(Arc::new(crate::issues::CloseIssue(backend.clone())));
            entries.push(Arc::new(crate::issues::SearchIssues(backend.clone())));
            entries.push(Arc::new(crate::issues::StartWorkOnIssue(backend)));
        }
        for tool in entries {
            let name = tool.schema().name;
            tools.insert(name, tool);
        }
        Self {
            tools,
            root,
            touched: std::sync::Mutex::new(Vec::new()),
            schema_index: std::sync::Mutex::new(SchemaIndex::default()),
        }
    }

    /// All tool schemas, for advertising to the model.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    /// Execute a tool by name. Returns an error `ToolOutput` if the name is
    /// unknown or the input is malformed — never panics.
    pub async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        // Classify the file op BEFORE executing: create-vs-update depends
        // on whether the file exists now, not after the write.
        let pending_op = self.classify_file_op(name, input);

        // Schema gate: if the target is a SQL file, parse the proposed
        // content for DDL objects and check them against the known schema
        // index before the write/edit lands. Objects the target file already
        // defines on disk are exempt — rewriting an existing migration in
        // place is not a duplicate.
        let mut pending_schema: Vec<crate::schema_gate::DdlObject> = Vec::new();
        if let Some(path) = input.get("path").and_then(|v| v.as_str())
            && matches!(name, "write_file" | "edit_file")
            && crate::schema_gate::is_schema_file(path)
        {
            // write_file carries the full file in `content`; edit_file
            // introduces new DDL only through `new_string`.
            let proposed_src = match name {
                "write_file" => input.get("content").and_then(|v| v.as_str()),
                _ => input.get("new_string").and_then(|v| v.as_str()),
            };
            let proposed = proposed_src
                .map(crate::schema_gate::extract_ddl_objects)
                .unwrap_or_default();
            if !proposed.is_empty() {
                let own: HashSet<String> = crate::resolve_within_root(&self.root, path)
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .map(|current| {
                        crate::schema_gate::extract_ddl_objects(&current)
                            .into_iter()
                            .map(|o| o.name.to_lowercase())
                            .collect()
                    })
                    .unwrap_or_default();
                let fresh: Vec<crate::schema_gate::DdlObject> = proposed
                    .into_iter()
                    .filter(|o| !own.contains(&o.name.to_lowercase()))
                    .collect();
                if !fresh.is_empty() {
                    let index = self.schema_index.lock().unwrap_or_else(|p| p.into_inner());
                    let conflicts = crate::schema_gate::find_conflicts(
                        &fresh,
                        &index.tables,
                        &index.types,
                        &index.views,
                    );
                    if !conflicts.is_empty() {
                        return ToolOutput::Error {
                            message: crate::schema_gate::format_conflicts(&conflicts),
                        };
                    }
                    pending_schema = fresh;
                }
            }
        }

        let output = match self.tools.get(name) {
            Some(tool) => tool.execute(input, &self.root).await,
            None => ToolOutput::Error {
                message: format!(
                    "unknown tool `{name}` — available: {}",
                    self.available_names()
                ),
            },
        };
        if !output.is_error() {
            // The write landed, so the objects it creates now exist: grow
            // the index so a duplicate later this session conflicts even
            // before any re-index from the code graph.
            if !pending_schema.is_empty() {
                self.record_schema_objects(&pending_schema);
            }
            if let Some((path, op)) = pending_op {
                self.record_touch(path, op);
            }
        }
        output
    }

    /// Merge just-created DDL objects into the schema index (lowercased —
    /// `find_conflicts` matches case-insensitively against lowercase names).
    fn record_schema_objects(&self, objects: &[crate::schema_gate::DdlObject]) {
        let mut index = self.schema_index.lock().unwrap_or_else(|p| p.into_inner());
        for obj in objects {
            let name = obj.name.to_lowercase();
            match obj.kind {
                crate::schema_gate::DdlKind::Table => index.tables.insert(name),
                crate::schema_gate::DdlKind::Type => index.types.insert(name),
                crate::schema_gate::DdlKind::View => index.views.insert(name),
            };
        }
    }

    /// `[C|R|U|D]`-classify a call: reads → R, writes → C (new) or U
    /// (existing), edits → U, deletes → D. `bash` is opaque — file ops done
    /// through the shell aren't attributable, which is why the CRUD tools
    /// exist and the prompt steers agents toward them.
    fn classify_file_op(&self, tool: &str, input: &Value) -> Option<(String, FileOp)> {
        let path = input.get("path").and_then(|v| v.as_str())?.to_string();
        let op = match tool {
            "read_file" => FileOp::Read,
            "edit_file" => FileOp::Update,
            "delete_file" => FileOp::Delete,
            "write_file" => {
                let exists = crate::resolve_within_root(&self.root, &path)
                    .map(|p| p.exists())
                    .unwrap_or(false);
                if exists {
                    FileOp::Update
                } else {
                    FileOp::Create
                }
            }
            _ => return None,
        };
        Some((path, op))
    }

    fn record_touch(&self, path: String, op: FileOp) {
        let mut touched = self.touched.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, ops)) = touched.iter_mut().find(|(p, _)| *p == path) {
            if !ops.contains(&op) {
                ops.push(op);
            }
        } else {
            touched.push((path, vec![op]));
        }
    }

    /// Snapshot of every file touched this session, insertion-ordered,
    /// with its ops as a compact CRUD string (e.g. `"RU"`).
    pub fn files_touched(&self) -> Vec<(String, String)> {
        let touched = self.touched.lock().unwrap_or_else(|p| p.into_inner());
        touched
            .iter()
            .map(|(path, ops)| {
                let mut ordered = [FileOp::Create, FileOp::Read, FileOp::Update, FileOp::Delete]
                    .into_iter()
                    .filter(|op| ops.contains(op))
                    .map(FileOp::letter)
                    .collect::<String>();
                if ordered.is_empty() {
                    ordered.push('?');
                }
                (path.clone(), ordered)
            })
            .collect()
    }

    /// Comma-separated sorted list of registered tool names, for error
    /// messages.
    pub fn available_names(&self) -> String {
        let mut names: Vec<&str> = self.tools.keys().map(|s| s.as_str()).collect();
        names.sort();
        names.join(", ")
    }

    /// The workspace root this registry resolves paths against.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Update the known schema index from the code graph. Called at session
    /// start and whenever schema files are re-indexed. The schema gate uses
    /// this to prevent duplicate table/type/view creation.
    pub fn update_schema_index(
        &self,
        tables: HashSet<String>,
        types: HashSet<String>,
        views: HashSet<String>,
    ) {
        let mut index = self.schema_index.lock().unwrap_or_else(|p| p.into_inner());
        index.tables = tables;
        index.types = types;
        index.views = views;
    }
}

/// `ToolRegistry` is the production implementation of `stella-core`'s
/// `ToolExecutor` port — the engine drives every tool call through this
/// impl, never through `stella-tools` types directly.
#[async_trait]
impl ToolExecutor for ToolRegistry {
    fn schemas(&self) -> Vec<ToolSchema> {
        ToolRegistry::schemas(self)
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        ToolRegistry::execute(self, name, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::issues::IssueBackend;

    #[tokio::test]
    async fn unknown_tool_returns_error_not_panic() {
        let reg = ToolRegistry::with_issue_backend(PathBuf::from("/tmp"), None);
        let result = reg.execute("nonexistent", &Value::Null).await;
        assert!(result.is_error());
    }

    #[test]
    fn registry_advertises_the_full_tool_set() {
        let reg =
            ToolRegistry::with_issue_backend(PathBuf::from("/tmp"), Some(IssueBackend::GitHub));
        let names: Vec<String> = reg.schemas().iter().map(|s| s.name.clone()).collect();
        for expected in [
            "read_file",
            "write_file",
            "edit_file",
            "delete_file",
            "bash",
            "grep",
            "glob",
            "explorations",
            "save_exploration",
            "save_memory",
            "verify_done",
            "build_project",
            "run_tests",
            "ci_status",
            "screenshot",
            "create_issue",
            "update_issue",
            "close_issue",
            "search_issues",
            "start_work_on_issue",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert_eq!(names.len(), 20, "unexpected tool count: {names:?}");
    }

    #[test]
    fn issue_tools_absent_without_a_configured_backend() {
        let reg = ToolRegistry::with_issue_backend(PathBuf::from("/tmp"), None);
        let names: Vec<String> = reg.schemas().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names.len(), 15, "unexpected tool count: {names:?}");
        for absent in [
            "create_issue",
            "update_issue",
            "close_issue",
            "search_issues",
            "start_work_on_issue",
        ] {
            assert!(
                !names.contains(&absent.to_string()),
                "{absent} must be absent"
            );
        }
    }

    #[test]
    fn read_only_flags_partition_the_registry_correctly() {
        // The engine parallelizes on this flag — a mutating tool marked
        // read-only would race writes; a read-only tool marked mutating
        // just loses concurrency. Pin the partition explicitly.
        let reg =
            ToolRegistry::with_issue_backend(PathBuf::from("/tmp"), Some(IssueBackend::GitHub));
        for schema in reg.schemas() {
            let expected = matches!(
                schema.name.as_str(),
                "read_file" | "grep" | "glob" | "explorations" | "ci_status" | "search_issues"
            );
            assert_eq!(
                schema.read_only, expected,
                "read_only flag wrong for {}",
                schema.name
            );
        }
    }

    #[tokio::test]
    async fn schema_gate_blocks_duplicate_table_on_write() {
        let dir = std::env::temp_dir().join(format!("stella_gate_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        // Populate the schema index with a known table.
        let mut tables = HashSet::new();
        tables.insert("users".to_string());
        reg.update_schema_index(tables, HashSet::new(), HashSet::new());

        // Attempt to write a SQL file that creates `users` again.
        let result = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "migrations/002.sql",
                    "content": "CREATE TABLE users (id INT);\n"
                }),
            )
            .await;
        assert!(result.is_error());
        match result {
            ToolOutput::Error { message } => {
                assert!(message.contains("Table `users` already exists"));
                assert!(message.contains("ALTER"));
            }
            _ => panic!("expected error"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_gate_allows_new_table_on_write() {
        let dir = std::env::temp_dir().join(format!("stella_gate_ok_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        let mut tables = HashSet::new();
        tables.insert("users".to_string());
        reg.update_schema_index(tables, HashSet::new(), HashSet::new());

        // Write a SQL file with a genuinely new table.
        let result = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "migrations/003.sql",
                    "content": "CREATE TABLE orders (id INT);\n"
                }),
            )
            .await;
        assert!(!result.is_error(), "new table should pass the gate");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_gate_exempts_rewriting_a_files_own_objects() {
        let dir = std::env::temp_dir().join(format!("stella_gate_own_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("migrations")).unwrap();
        std::fs::write(
            dir.join("migrations/001.sql"),
            "CREATE TABLE users (id INT);\n",
        )
        .unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        let mut tables = HashSet::new();
        tables.insert("users".to_string());
        reg.update_schema_index(tables, HashSet::new(), HashSet::new());

        // Rewriting the file that defines `users` is an in-place change to
        // the existing object, not a duplicate creation.
        let result = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "migrations/001.sql",
                    "content": "CREATE TABLE users (id INT, email TEXT);\n"
                }),
            )
            .await;
        assert!(!result.is_error(), "same-file rewrite must pass the gate");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_gate_grows_the_index_after_a_successful_write() {
        let dir = std::env::temp_dir().join(format!("stella_gate_grow_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        // Empty index: the first migration passes and registers `users`.
        let first = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "migrations/001.sql",
                    "content": "CREATE TABLE users (id INT);\n"
                }),
            )
            .await;
        assert!(!first.is_error());

        // A second file re-creating `users` now conflicts — caught by the
        // in-session index growth, no graph re-index needed.
        let second = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "migrations/002.sql",
                    "content": "CREATE TABLE users (id INT);\n"
                }),
            )
            .await;
        assert!(second.is_error(), "in-session duplicate must be caught");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_gate_checks_edit_file_new_string() {
        let dir = std::env::temp_dir().join(format!("stella_gate_edit_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("migrations")).unwrap();
        std::fs::write(dir.join("migrations/002.sql"), "-- add payments\n").unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        let mut tables = HashSet::new();
        tables.insert("users".to_string());
        reg.update_schema_index(tables, HashSet::new(), HashSet::new());

        // An edit whose replacement introduces a duplicate CREATE is gated
        // exactly like a write.
        let result = reg
            .execute(
                "edit_file",
                &serde_json::json!({
                    "path": "migrations/002.sql",
                    "old_string": "-- add payments",
                    "new_string": "CREATE TABLE users (id INT);"
                }),
            )
            .await;
        assert!(
            result.is_error(),
            "edit introducing a duplicate must be gated"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_gate_allows_non_sql_write() {
        let dir = std::env::temp_dir().join(format!("stella_gate_nosql_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = ToolRegistry::with_issue_backend(dir.clone(), None);

        let mut tables = HashSet::new();
        tables.insert("users".to_string());
        reg.update_schema_index(tables, HashSet::new(), HashSet::new());

        // Writing a Rust file should never trigger the schema gate.
        let result = reg
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": "src/main.rs",
                    "content": "fn main() {}\n"
                }),
            )
            .await;
        assert!(!result.is_error());
        std::fs::remove_dir_all(&dir).ok();
    }
}
