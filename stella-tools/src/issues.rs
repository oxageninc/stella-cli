//! Issue-tracking tools with a configured backend: Linear (`LINEAR_API_KEY`
//! or a `stella connect linear` connection — Linear wins) or GitHub Issues
//! (a `stella connect github` OAuth connection, else the authenticated `gh`
//! CLI). If nothing is configured the tools are NOT registered at all — no
//! dead schema entries, no per-call token cost, no surface that errors on
//! use (see `ToolRegistry::new`).
//!
//! The operations themselves live in [`crate::issue_ops`] (shared with the
//! Command Deck); this module is detection + the model-facing tool layer.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::exec;
use crate::issue_ops::{self as ops, CreateParams, IssueFilters, quote};
use crate::registry::Tool;
use crate::tracker_auth::{
    TrackerProvider, TrackerStore, auth_header_value, github_api_base, linear_api_url, now_secs,
    refresh_connection,
};

/// Which tracker the issue tools talk to.
#[derive(Clone)]
pub enum IssueBackend {
    /// GitHub Issues via the authenticated `gh` CLI.
    GitHub,
    /// GitHub Issues via the REST API with a `stella connect github` token —
    /// no `gh` binary required.
    GitHubApi { token: String, api_base: String },
    /// Linear via its GraphQL API. `api_key` is the full Authorization
    /// header value: a personal API key verbatim, or `Bearer <token>` for an
    /// OAuth connection.
    Linear { api_key: String, api_url: String },
}

// Manual Debug so a stray `{backend:?}` (or a future `tracing::debug!(?backend)`)
// can never print the GitHub token or Linear key — the rest of the codebase
// holds secrets in a redacted `ApiKey`; these two variants carry raw Strings and
// must be redacted here to honor SECURITY.md's "credentials never in logs".
impl std::fmt::Debug for IssueBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitHub => f.write_str("GitHub"),
            Self::GitHubApi { api_base, .. } => f
                .debug_struct("GitHubApi")
                .field("token", &"<redacted>")
                .field("api_base", api_base)
                .finish(),
            Self::Linear { api_url, .. } => f
                .debug_struct("Linear")
                .field("api_key", &"<redacted>")
                .field("api_url", api_url)
                .finish(),
        }
    }
}

/// [`detect_issue_backend`] off the async executor: the `gh auth status`
/// probe spawns a process (tens to hundreds of ms), which must not block a
/// runtime worker thread (#64). The env-var fast path never reaches the
/// blocking pool. Stale OAuth connections get a best-effort refresh first —
/// the only context where refreshing is possible.
pub async fn detect_issue_backend_async() -> Option<IssueBackend> {
    refresh_stale_connections().await;
    if let Some(linear) = detect_linear_backend() {
        return Some(linear);
    }
    tokio::task::spawn_blocking(detect_issue_backend)
        .await
        .unwrap_or(None)
}

/// Best-effort refresh of any stored OAuth connection near expiry; failures
/// leave the stale token in place (its 401 error tells the user to
/// re-connect).
async fn refresh_stale_connections() {
    let Some(store) = TrackerStore::open_default() else {
        return;
    };
    let Ok(connections) = store.connections() else {
        return;
    };
    let now = now_secs();
    for (provider, connection) in connections {
        if connection.needs_refresh(now)
            && connection.refresh_token.is_some()
            && let Ok(refreshed) = refresh_connection(&connection).await
        {
            let _ = store.put(provider, &refreshed);
        }
    }
}

fn detect_linear_backend() -> Option<IssueBackend> {
    if let Ok(key) = std::env::var("LINEAR_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(IssueBackend::Linear {
            api_key: key,
            api_url: linear_api_url(),
        });
    }
    let store = TrackerStore::open_default()?;
    let connection = store.get(TrackerProvider::Linear).ok()??;
    Some(IssueBackend::Linear {
        api_key: auth_header_value(TrackerProvider::Linear, &connection),
        api_url: linear_api_url(),
    })
}

fn detect_github_connected_backend() -> Option<IssueBackend> {
    let store = TrackerStore::open_default()?;
    let connection = store.get(TrackerProvider::GitHub).ok()??;
    Some(IssueBackend::GitHubApi {
        token: connection.access_token,
        api_base: github_api_base(),
    })
}

/// Detect the configured backend at startup: Linear (env key, then a stored
/// connection) beats GitHub; a stored GitHub connection (explicit
/// `stella connect`) beats ambient `gh` auth; neither → `None` and the
/// tools stay unregistered.
pub fn detect_issue_backend() -> Option<IssueBackend> {
    if let Some(linear) = detect_linear_backend() {
        return Some(linear);
    }
    if let Some(github) = detect_github_connected_backend() {
        return Some(github);
    }
    let mut gh = std::process::Command::new("gh");
    crate::exec::scrub_sensitive_std_env(&mut gh);
    let gh_authed = gh
        .args(["auth", "status"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if gh_authed {
        return Some(IssueBackend::GitHub);
    }
    None
}

// ---- shared helpers --------------------------------------------------------

fn issue_ref_description(backend: &IssueBackend) -> &'static str {
    match backend {
        IssueBackend::GitHub | IssueBackend::GitHubApi { .. } => "GitHub issue number",
        IssueBackend::Linear { .. } => "Linear identifier, e.g. ENG-123",
    }
}

fn require_issue_ref(input: &Value) -> Result<String, ToolOutput> {
    match input.get("issue") {
        Some(Value::String(s)) if !s.is_empty() => {
            // The ref is used POSITIONALLY in gh commands. `quote()` stops
            // shell injection but not argument injection: a flag-shaped
            // value like `--web` or `-R other/repo` would reach gh as an
            // option. Real refs (123, #123, TEAM-123, full issue URLs)
            // never start with `-`.
            if s.starts_with('-') {
                return Err(ToolOutput::Error {
                    message: format!(
                        "invalid issue ref `{s}` — expected an issue number, #number, \
                         key, or URL"
                    ),
                });
            }
            Ok(s.clone())
        }
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => Err(ToolOutput::Error {
            message: "missing required field `issue`".into(),
        }),
    }
}

fn string_list(input: &Value, field: &str) -> Vec<String> {
    input
        .get(field)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn format_summary(issue: &ops::IssueSummary) -> String {
    let mut line = format!("{} [{}] {}", issue.key, issue.state, issue.title);
    if let Some(assignee) = &issue.assignee {
        line.push_str(&format!(" · {assignee}"));
    }
    if !issue.labels.is_empty() {
        line.push_str(&format!(" · {}", issue.labels.join(", ")));
    }
    if !issue.url.is_empty() {
        line.push_str(&format!(" — {}", issue.url));
    }
    line
}

// ---- the eight tools -------------------------------------------------------

pub struct CreateIssue(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for CreateIssue {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "create_issue".into(),
            description: "Create an issue in the configured tracker. Labels and assignee must \
                          exist — search them first with list_labels / list_members."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "labels": { "type": "array", "items": { "type": "string" } },
                    "assignee": { "type": "string", "description": "@login (GitHub) or email/name (Linear)" },
                    "team": { "type": "string", "description": "Linear team key (defaults to the first team)" }
                },
                "required": ["title"]
            }),
            read_only: false,
        }
    }
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let Some(title) = input.get("title").and_then(|v| v.as_str()) else {
            return ToolOutput::Error {
                message: "missing required field `title`".into(),
            };
        };
        let params = CreateParams {
            title: title.to_string(),
            body: input
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            labels: string_list(input, "labels"),
            assignee: input
                .get("assignee")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            team: input
                .get("team")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        };
        match ops::create_issue(&self.0, root, &params).await {
            Ok(issue) => ToolOutput::Ok {
                content: format!("created {} {}", issue.key, issue.url),
            },
            Err(e) => ToolOutput::Error { message: e },
        }
    }
}

pub struct UpdateIssue(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for UpdateIssue {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "update_issue".into(),
            description: "Update an issue: title/body, add a comment, change status, add \
                          labels, or set the assignee — any combination."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "issue": { "type": "string", "description": issue_ref_description(&self.0) },
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "comment": { "type": "string" },
                    "status": { "type": "string", "description": "GitHub: open|closed. Linear: a workflow state name or type (backlog, in progress, done, …)" },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "Labels to ADD (existing labels are kept)" },
                    "assignee": { "type": "string", "description": "@login (GitHub) or email/name (Linear)" }
                },
                "required": ["issue"]
            }),
            read_only: false,
        }
    }
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let issue = match require_issue_ref(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        if let Some(comment) = input.get("comment").and_then(|v| v.as_str())
            && let Err(e) = ops::add_comment(&self.0, root, &issue, comment).await
        {
            return ToolOutput::Error { message: e };
        }
        let labels = string_list(input, "labels");
        if let Err(e) = ops::update_issue(
            &self.0,
            root,
            &issue,
            input.get("title").and_then(|v| v.as_str()),
            input.get("body").and_then(|v| v.as_str()),
            &labels,
            input.get("assignee").and_then(|v| v.as_str()),
        )
        .await
        {
            return ToolOutput::Error { message: e };
        }
        if let Some(status) = input.get("status").and_then(|v| v.as_str())
            && let Err(e) = ops::set_status(&self.0, root, &issue, status).await
        {
            return ToolOutput::Error { message: e };
        }
        ToolOutput::Ok {
            content: format!("updated {issue}"),
        }
    }
}

pub struct CloseIssue(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for CloseIssue {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "close_issue".into(),
            description: "Close an issue, optionally with a closing comment.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "issue": { "type": "string", "description": issue_ref_description(&self.0) },
                    "comment": { "type": "string" }
                },
                "required": ["issue"]
            }),
            read_only: false,
        }
    }
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let issue = match require_issue_ref(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        if let Some(comment) = input.get("comment").and_then(|v| v.as_str())
            && let Err(e) = ops::add_comment(&self.0, root, &issue, comment).await
        {
            return ToolOutput::Error { message: e };
        }
        match ops::set_status(&self.0, root, &issue, "closed").await {
            Ok(_) => ToolOutput::Ok {
                content: format!("closed {issue}"),
            },
            Err(e) => ToolOutput::Error { message: e },
        }
    }
}

pub struct SearchIssues(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for SearchIssues {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "search_issues".into(),
            description: "Search or list issues in the configured tracker, with optional \
                          state/assignee/label filters."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "state": { "type": "string", "enum": ["open", "closed", "all"] },
                    "assignee": { "type": "string" },
                    "label": { "type": "string" },
                    "limit": { "type": "integer" }
                }
            }),
            read_only: true,
        }
    }
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let filters = IssueFilters {
            query: input
                .get("query")
                .and_then(|v| v.as_str())
                .filter(|q| !q.is_empty())
                .map(str::to_string),
            state: input
                .get("state")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            assignee: input
                .get("assignee")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            label: input
                .get("label")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            limit: input.get("limit").and_then(|v| v.as_u64()).unwrap_or(20),
        };
        match ops::list_issues(&self.0, root, &filters).await {
            Ok(issues) if issues.is_empty() => ToolOutput::Ok {
                content: "no issues matched".into(),
            },
            Ok(issues) => ToolOutput::Ok {
                content: issues
                    .iter()
                    .map(format_summary)
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
            Err(e) => ToolOutput::Error { message: e },
        }
    }
}

pub struct GetIssue(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for GetIssue {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "get_issue".into(),
            description: "Read one issue in full: title, body, state, labels, assignee, and \
                          the comment thread."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "issue": { "type": "string", "description": issue_ref_description(&self.0) }
                },
                "required": ["issue"]
            }),
            read_only: true,
        }
    }
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let issue = match require_issue_ref(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        match ops::get_issue(&self.0, root, &issue).await {
            Ok(detail) => {
                let mut content = format_summary(&detail.summary);
                if !detail.body.is_empty() {
                    content.push_str(&format!("\n\n{}", detail.body));
                }
                for comment in &detail.comments {
                    content.push_str(&format!(
                        "\n\n--- comment · {} · {}\n{}",
                        comment.author, comment.created_at, comment.body
                    ));
                }
                ToolOutput::Ok { content }
            }
            Err(e) => ToolOutput::Error { message: e },
        }
    }
}

pub struct ListLabels(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for ListLabels {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_labels".into(),
            description: "Search the tracker's labels by substring (empty query lists all) — \
                          use before adding labels so exact names aren't guessed."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                }
            }),
            read_only: true,
        }
    }
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(30);
        match ops::search_labels(&self.0, root, query, limit).await {
            Ok(labels) if labels.is_empty() => ToolOutput::Ok {
                content: "no labels matched".into(),
            },
            Ok(labels) => ToolOutput::Ok {
                content: labels
                    .iter()
                    .map(|l| match &l.description {
                        Some(description) => format!("{} — {description}", l.name),
                        None => l.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
            Err(e) => ToolOutput::Error { message: e },
        }
    }
}

pub struct ListMembers(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for ListMembers {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_members".into(),
            description: "Search assignable people by login, name, or email substring (empty \
                          query lists all) — use before setting an assignee."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                }
            }),
            read_only: true,
        }
    }
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(30);
        match ops::search_members(&self.0, root, query, limit).await {
            Ok(members) if members.is_empty() => ToolOutput::Ok {
                content: "no members matched".into(),
            },
            Ok(members) => ToolOutput::Ok {
                content: members
                    .iter()
                    .map(|m| match &m.name {
                        Some(name) => format!("{} — {name}", m.handle),
                        None => m.handle.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
            Err(e) => ToolOutput::Error { message: e },
        }
    }
}

pub struct StartWorkOnIssue(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for StartWorkOnIssue {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "start_work_on_issue".into(),
            description: "Move an issue to in-progress and create/check out its branch.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "issue": { "type": "string", "description": issue_ref_description(&self.0) },
                    "branch": { "type": "string", "description": "Branch name override" }
                },
                "required": ["issue"]
            }),
            read_only: false,
        }
    }
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let issue = match require_issue_ref(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        match self.0.as_ref() {
            IssueBackend::GitHub => {
                let mut cmd = format!("gh issue develop {} --checkout", quote(&issue));
                if let Some(branch) = input.get("branch").and_then(|v| v.as_str()) {
                    cmd.push_str(&format!(" --name {}", quote(branch)));
                }
                match exec::run(&cmd, root, 60).await {
                    Ok((0, output)) => ToolOutput::Ok { content: output },
                    Ok((code, output)) => ToolOutput::Error {
                        message: format!("gh failed (exit {code}): {output}"),
                    },
                    Err(e) => ToolOutput::Error { message: e },
                }
            }
            IssueBackend::GitHubApi { .. } => {
                // No `gh issue develop` without the CLI: derive a branch name
                // and check it out locally. GitHub has no in-progress state
                // to move to.
                let number = issue.trim_start_matches('#');
                let branch = match input.get("branch").and_then(|v| v.as_str()) {
                    Some(branch) => branch.to_string(),
                    None => format!("issue-{number}"),
                };
                match checkout_branch(&branch, root).await {
                    Ok(()) => ToolOutput::Ok {
                        content: format!("started {issue} on branch {branch}"),
                    },
                    Err(message) => ToolOutput::Error { message },
                }
            }
            IssueBackend::Linear { api_key, api_url } => {
                let (id, team) = match ops::linear_issue_id(api_url, api_key, &issue).await {
                    Ok(pair) => pair,
                    Err(e) => return ToolOutput::Error { message: e },
                };
                // Linear supplies the canonical branch name per issue.
                let branch = match input.get("branch").and_then(|v| v.as_str()) {
                    Some(branch) => branch.to_string(),
                    None => match ops::linear_graphql(
                        api_url,
                        api_key,
                        "query($id: String!) { issue(id: $id) { branchName } }",
                        json!({ "id": id }),
                    )
                    .await
                    {
                        Ok(d) => d["issue"]["branchName"].as_str().unwrap_or("").to_string(),
                        Err(e) => return ToolOutput::Error { message: e },
                    },
                };
                if branch.is_empty() {
                    return ToolOutput::Error {
                        message: "could not determine a branch name — pass `branch`".into(),
                    };
                }
                if let Err(message) = checkout_branch(&branch, root).await {
                    return ToolOutput::Error { message };
                }
                let state = match ops::linear_state_id(
                    api_url,
                    api_key,
                    &team,
                    Some("started"),
                    None,
                )
                .await
                {
                    Ok(state) => state,
                    Err(e) => return ToolOutput::Error { message: e },
                };
                match ops::linear_graphql(
                    api_url,
                    api_key,
                    "mutation($id: String!, $input: IssueUpdateInput!) { \
                       issueUpdate(id: $id, input: $input) { success } }",
                    json!({ "id": id, "input": { "stateId": state } }),
                )
                .await
                {
                    Ok(_) => ToolOutput::Ok {
                        content: format!("started {issue} on branch {branch}"),
                    },
                    Err(e) => ToolOutput::Error { message: e },
                }
            }
        }
    }
}

/// Create-or-checkout a branch, gating on the exit code — a failed checkout
/// (dirty tree, protected branch) must not be silently ignored.
async fn checkout_branch(branch: &str, root: &std::path::Path) -> Result<(), String> {
    let checkout = format!(
        "git checkout -b {b} 2>/dev/null || git checkout {b}",
        b = quote(branch)
    );
    match exec::run(&checkout, root, 30).await {
        Ok((0, _)) => Ok(()),
        Ok((code, output)) => Err(format!(
            "git checkout of `{branch}` failed (exit {code}) — issue left unchanged: {output}"
        )),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> Arc<IssueBackend> {
        Arc::new(IssueBackend::GitHub)
    }

    #[test]
    fn schemas_partition_correctly() {
        assert!(SearchIssues(backend()).schema().read_only);
        assert!(GetIssue(backend()).schema().read_only);
        assert!(ListLabels(backend()).schema().read_only);
        assert!(ListMembers(backend()).schema().read_only);
        assert!(!CreateIssue(backend()).schema().read_only);
        assert!(!UpdateIssue(backend()).schema().read_only);
        assert!(!CloseIssue(backend()).schema().read_only);
        assert!(!StartWorkOnIssue(backend()).schema().read_only);
    }

    #[tokio::test]
    async fn missing_required_fields_are_named_errors() {
        let root = std::env::temp_dir();
        assert!(
            CreateIssue(backend())
                .execute(&json!({}), &root)
                .await
                .is_error()
        );
        for tool_output in [
            UpdateIssue(backend()).execute(&json!({}), &root).await,
            CloseIssue(backend()).execute(&json!({}), &root).await,
            GetIssue(backend()).execute(&json!({}), &root).await,
            StartWorkOnIssue(backend()).execute(&json!({}), &root).await,
        ] {
            match tool_output {
                ToolOutput::Error { message } => assert!(message.contains("issue"), "{message}"),
                other => panic!("expected error, got {other:?}"),
            }
        }
    }

    #[test]
    fn linear_identifiers_parse_and_reject() {
        assert_eq!(
            ops::parse_linear_identifier("OXA-123"),
            Some(("OXA".into(), 123.0))
        );
        assert_eq!(
            ops::parse_linear_identifier("eng-7"),
            Some(("ENG".into(), 7.0))
        );
        assert!(ops::parse_linear_identifier("123").is_none());
        assert!(ops::parse_linear_identifier("-123").is_none());
        assert!(ops::parse_linear_identifier("OXA-").is_none());
    }

    #[test]
    fn quote_neutralizes_single_quotes() {
        assert_eq!(quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn issue_refs_reject_flag_shaped_values_and_accept_real_refs() {
        // The ref is passed positionally to gh, so a flag-shaped value is
        // argument injection — this pins the `-` guard in require_issue_ref.
        for injected in ["--web", "-R other/repo"] {
            match require_issue_ref(&json!({ "issue": injected })) {
                Err(ToolOutput::Error { message }) => {
                    assert!(message.contains("invalid issue ref"), "{message}");
                }
                other => panic!("expected error for `{injected}`, got {other:?}"),
            }
        }

        assert_eq!(
            require_issue_ref(&json!({ "issue": "123" })).unwrap(),
            "123"
        );
        assert_eq!(
            require_issue_ref(&json!({ "issue": "#123" })).unwrap(),
            "#123"
        );
        assert_eq!(require_issue_ref(&json!({ "issue": 123 })).unwrap(), "123");
    }

    #[test]
    fn summaries_format_with_assignee_and_labels() {
        let issue = ops::IssueSummary {
            key: "ENG-42".into(),
            title: "Fix flaky test".into(),
            state: "In Progress".into(),
            labels: vec!["bug".into(), "ci".into()],
            assignee: Some("mona@example.com".into()),
            url: "https://linear.app/x/issue/ENG-42".into(),
            updated_at: None,
        };
        let line = format_summary(&issue);
        assert!(
            line.contains("ENG-42 [In Progress] Fix flaky test"),
            "{line}"
        );
        assert!(line.contains("mona@example.com"), "{line}");
        assert!(line.contains("bug, ci"), "{line}");
    }
}
