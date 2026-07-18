//! Issue-tracking tools with a configured backend: Linear (when
//! `LINEAR_API_KEY` is set — it wins) or GitHub Issues (when the `gh` CLI
//! is authenticated). If neither is configured the tools are NOT
//! registered at all — no dead schema entries, no per-call token cost, no
//! surface that errors on use (see `ToolRegistry::new`).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use stella_protocol::tool::{ToolOutput, ToolSchema};

use crate::exec;
use crate::registry::Tool;

const TIMEOUT_SECS: u64 = 60;
const LINEAR_API_URL: &str = "https://api.linear.app/graphql";

/// Which tracker the issue tools talk to.
#[derive(Debug, Clone)]
pub enum IssueBackend {
    /// GitHub Issues via the authenticated `gh` CLI.
    GitHub,
    /// Linear via its GraphQL API.
    Linear { api_key: String },
}

/// [`detect_issue_backend`] off the async executor: the `gh auth status`
/// probe spawns a process (tens to hundreds of ms), which must not block a
/// runtime worker thread (#64). The env-var fast path never reaches the
/// blocking pool.
pub async fn detect_issue_backend_async() -> Option<IssueBackend> {
    if let Some(linear) = detect_linear_backend() {
        return Some(linear);
    }
    tokio::task::spawn_blocking(detect_issue_backend)
        .await
        .unwrap_or(None)
}

fn detect_linear_backend() -> Option<IssueBackend> {
    match std::env::var("LINEAR_API_KEY") {
        Ok(key) if !key.trim().is_empty() => Some(IssueBackend::Linear { api_key: key }),
        _ => None,
    }
}

/// Detect the configured backend at startup: `LINEAR_API_KEY` beats an
/// authenticated `gh`; neither → `None` and the tools stay unregistered.
pub fn detect_issue_backend() -> Option<IssueBackend> {
    if let Some(linear) = detect_linear_backend() {
        return Some(linear);
    }
    let gh_authed = std::process::Command::new("gh")
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

async fn gh(args: String, root: &std::path::Path) -> ToolOutput {
    match exec::run(&args, root, TIMEOUT_SECS).await {
        Ok((0, output)) => ToolOutput::Ok { content: output },
        Ok((code, output)) => ToolOutput::Error {
            message: format!("gh failed (exit {code}): {output}"),
        },
        Err(e) => ToolOutput::Error { message: e },
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

async fn linear_graphql(api_key: &str, query: &str, variables: Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let response = client
        .post(LINEAR_API_URL)
        .header("Authorization", api_key)
        .header("Content-Type", "application/json")
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .await
        .map_err(|e| format!("Linear request failed: {e}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("Linear returned non-JSON (HTTP {status}): {e}"))?;
    if let Some(errors) = body.get("errors").and_then(|e| e.as_array())
        && !errors.is_empty()
    {
        let messages: Vec<&str> = errors
            .iter()
            .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
            .collect();
        return Err(format!("Linear error: {}", messages.join("; ")));
    }
    body.get("data")
        .cloned()
        .ok_or_else(|| format!("Linear response had no data (HTTP {status})"))
}

/// Split a Linear identifier like `OXA-123` into (team key, number).
fn parse_linear_identifier(identifier: &str) -> Option<(String, f64)> {
    let (team, number) = identifier.rsplit_once('-')?;
    let number: f64 = number.parse().ok()?;
    if team.is_empty() {
        return None;
    }
    Some((team.to_uppercase(), number))
}

/// Resolve a Linear issue's node id (and team id) from its identifier.
async fn linear_issue_id(api_key: &str, identifier: &str) -> Result<(String, String), String> {
    let Some((team, number)) = parse_linear_identifier(identifier) else {
        return Err(format!(
            "`{identifier}` is not a Linear identifier (expected e.g. ENG-123)"
        ));
    };
    let data = linear_graphql(
        api_key,
        "query($team: String!, $number: Float!) {\
           issues(filter: { team: { key: { eq: $team } }, number: { eq: $number } }, first: 1) {\
             nodes { id team { id } } } }",
        json!({ "team": team, "number": number }),
    )
    .await?;
    let node = data["issues"]["nodes"]
        .get(0)
        .ok_or_else(|| format!("no Linear issue found for `{identifier}`"))?;
    Ok((
        node["id"].as_str().unwrap_or_default().to_string(),
        node["team"]["id"].as_str().unwrap_or_default().to_string(),
    ))
}

/// First workflow state of `state_type` for a team (e.g. "started",
/// "completed").
async fn linear_state_id(api_key: &str, team_id: &str, state_type: &str) -> Result<String, String> {
    let data = linear_graphql(
        api_key,
        "query($team: String!) { team(id: $team) { states { nodes { id name type } } } }",
        json!({ "team": team_id }),
    )
    .await?;
    data["team"]["states"]["nodes"]
        .as_array()
        .and_then(|nodes| {
            nodes
                .iter()
                .find(|n| n["type"].as_str() == Some(state_type))
                .and_then(|n| n["id"].as_str())
                .map(str::to_string)
        })
        .ok_or_else(|| format!("team has no workflow state of type `{state_type}`"))
}

fn issue_ref_description(backend: &IssueBackend) -> &'static str {
    match backend {
        IssueBackend::GitHub => "GitHub issue number",
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

// ---- the five tools --------------------------------------------------------

pub struct CreateIssue(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for CreateIssue {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "create_issue".into(),
            description: "Create an issue in the configured tracker.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "labels": { "type": "array", "items": { "type": "string" } },
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
        let body = input.get("body").and_then(|v| v.as_str()).unwrap_or("");
        match self.0.as_ref() {
            IssueBackend::GitHub => {
                let mut cmd = format!(
                    "gh issue create --title {} --body {}",
                    quote(title),
                    quote(body)
                );
                if let Some(labels) = input.get("labels").and_then(|v| v.as_array()) {
                    for label in labels.iter().filter_map(|l| l.as_str()) {
                        cmd.push_str(&format!(" --label {}", quote(label)));
                    }
                }
                gh(cmd, root).await
            }
            IssueBackend::Linear { api_key } => {
                // Resolve the team: input key, else the first team.
                let team_id = async {
                    let data = linear_graphql(
                        api_key,
                        "query { teams(first: 50) { nodes { id key } } }",
                        json!({}),
                    )
                    .await?;
                    let nodes = data["teams"]["nodes"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    let wanted = input.get("team").and_then(|v| v.as_str());
                    let team = match wanted {
                        Some(key) => nodes
                            .iter()
                            .find(|n| n["key"].as_str() == Some(&key.to_uppercase()))
                            .ok_or_else(|| format!("no Linear team with key `{key}`"))?,
                        None => nodes.first().ok_or("no Linear teams visible to this key")?,
                    };
                    Ok::<String, String>(team["id"].as_str().unwrap_or_default().to_string())
                }
                .await;
                let team_id = match team_id {
                    Ok(id) => id,
                    Err(e) => return ToolOutput::Error { message: e },
                };
                match linear_graphql(
                    api_key,
                    "mutation($input: IssueCreateInput!) { issueCreate(input: $input) { \
                       issue { identifier url } } }",
                    json!({ "input": { "teamId": team_id, "title": title, "description": body } }),
                )
                .await
                {
                    Ok(data) => ToolOutput::Ok {
                        content: format!(
                            "created {} {}",
                            data["issueCreate"]["issue"]["identifier"]
                                .as_str()
                                .unwrap_or("?"),
                            data["issueCreate"]["issue"]["url"].as_str().unwrap_or("")
                        ),
                    },
                    Err(e) => ToolOutput::Error { message: e },
                }
            }
        }
    }
}

pub struct UpdateIssue(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for UpdateIssue {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "update_issue".into(),
            description: "Update an issue's title/body or add a comment.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "issue": { "type": "string", "description": issue_ref_description(&self.0) },
                    "title": { "type": "string" },
                    "body": { "type": "string" },
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
        let title = input.get("title").and_then(|v| v.as_str());
        let body = input.get("body").and_then(|v| v.as_str());
        let comment = input.get("comment").and_then(|v| v.as_str());
        match self.0.as_ref() {
            IssueBackend::GitHub => {
                if let Some(comment) = comment {
                    let out = gh(
                        format!(
                            "gh issue comment {} --body {}",
                            quote(&issue),
                            quote(comment)
                        ),
                        root,
                    )
                    .await;
                    if out.is_error() {
                        return out;
                    }
                }
                let mut edits = String::new();
                if let Some(title) = title {
                    edits.push_str(&format!(" --title {}", quote(title)));
                }
                if let Some(body) = body {
                    edits.push_str(&format!(" --body {}", quote(body)));
                }
                if edits.is_empty() {
                    return ToolOutput::Ok {
                        content: format!("issue {issue} updated"),
                    };
                }
                gh(format!("gh issue edit {}{edits}", quote(&issue)), root).await
            }
            IssueBackend::Linear { api_key } => {
                let (id, _team) = match linear_issue_id(api_key, &issue).await {
                    Ok(pair) => pair,
                    Err(e) => return ToolOutput::Error { message: e },
                };
                if let Some(comment) = comment
                    && let Err(e) = linear_graphql(
                        api_key,
                        "mutation($input: CommentCreateInput!) { commentCreate(input: $input) { \
                           success } }",
                        json!({ "input": { "issueId": id, "body": comment } }),
                    )
                    .await
                {
                    return ToolOutput::Error { message: e };
                }
                let mut update = serde_json::Map::new();
                if let Some(title) = title {
                    update.insert("title".into(), json!(title));
                }
                if let Some(body) = body {
                    update.insert("description".into(), json!(body));
                }
                if !update.is_empty()
                    && let Err(e) = linear_graphql(
                        api_key,
                        "mutation($id: String!, $input: IssueUpdateInput!) { \
                           issueUpdate(id: $id, input: $input) { success } }",
                        json!({ "id": id, "input": update }),
                    )
                    .await
                {
                    return ToolOutput::Error { message: e };
                }
                ToolOutput::Ok {
                    content: format!("updated {issue}"),
                }
            }
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
        match self.0.as_ref() {
            IssueBackend::GitHub => {
                let mut cmd = format!("gh issue close {}", quote(&issue));
                if let Some(comment) = input.get("comment").and_then(|v| v.as_str()) {
                    cmd.push_str(&format!(" --comment {}", quote(comment)));
                }
                gh(cmd, root).await
            }
            IssueBackend::Linear { api_key } => {
                let (id, team) = match linear_issue_id(api_key, &issue).await {
                    Ok(pair) => pair,
                    Err(e) => return ToolOutput::Error { message: e },
                };
                if let Some(comment) = input.get("comment").and_then(|v| v.as_str())
                    && let Err(e) = linear_graphql(
                        api_key,
                        "mutation($input: CommentCreateInput!) { commentCreate(input: $input) { \
                           success } }",
                        json!({ "input": { "issueId": id, "body": comment } }),
                    )
                    .await
                {
                    return ToolOutput::Error { message: e };
                }
                let state = match linear_state_id(api_key, &team, "completed").await {
                    Ok(state) => state,
                    Err(e) => return ToolOutput::Error { message: e },
                };
                match linear_graphql(
                    api_key,
                    "mutation($id: String!, $input: IssueUpdateInput!) { \
                       issueUpdate(id: $id, input: $input) { success } }",
                    json!({ "id": id, "input": { "stateId": state } }),
                )
                .await
                {
                    Ok(_) => ToolOutput::Ok {
                        content: format!("closed {issue}"),
                    },
                    Err(e) => ToolOutput::Error { message: e },
                }
            }
        }
    }
}

pub struct SearchIssues(pub Arc<IssueBackend>);
#[async_trait]
impl Tool for SearchIssues {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "search_issues".into(),
            description: "Search issues in the configured tracker.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "state": { "type": "string", "enum": ["open", "closed", "all"] },
                    "limit": { "type": "integer" }
                }
            }),
            read_only: true,
        }
    }
    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(20);
        match self.0.as_ref() {
            IssueBackend::GitHub => {
                let state = input
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("open");
                let mut cmd = format!(
                    "gh issue list --state {} --limit {limit} \
                     --json number,title,state,labels,updatedAt",
                    quote(state)
                );
                if let Some(query) = input.get("query").and_then(|v| v.as_str()) {
                    cmd.push_str(&format!(" --search {}", quote(query)));
                }
                gh(cmd, root).await
            }
            IssueBackend::Linear { api_key } => {
                let term = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let result = if term.is_empty() {
                    linear_graphql(
                        api_key,
                        "query($first: Int!) { issues(first: $first, orderBy: updatedAt) { \
                           nodes { identifier title state { name } url } } }",
                        json!({ "first": limit }),
                    )
                    .await
                    .map(|d| d["issues"]["nodes"].clone())
                } else {
                    linear_graphql(
                        api_key,
                        "query($term: String!, $first: Int!) { \
                           searchIssues(term: $term, first: $first) { \
                             nodes { identifier title state { name } url } } }",
                        json!({ "term": term, "first": limit }),
                    )
                    .await
                    .map(|d| d["searchIssues"]["nodes"].clone())
                };
                match result {
                    Ok(nodes) => {
                        let lines: Vec<String> = nodes
                            .as_array()
                            .map(|nodes| {
                                nodes
                                    .iter()
                                    .map(|n| {
                                        format!(
                                            "{} [{}] {} — {}",
                                            n["identifier"].as_str().unwrap_or("?"),
                                            n["state"]["name"].as_str().unwrap_or("?"),
                                            n["title"].as_str().unwrap_or(""),
                                            n["url"].as_str().unwrap_or("")
                                        )
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        ToolOutput::Ok {
                            content: if lines.is_empty() {
                                "no issues matched".into()
                            } else {
                                lines.join("\n")
                            },
                        }
                    }
                    Err(e) => ToolOutput::Error { message: e },
                }
            }
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
                gh(cmd, root).await
            }
            IssueBackend::Linear { api_key } => {
                let (id, team) = match linear_issue_id(api_key, &issue).await {
                    Ok(pair) => pair,
                    Err(e) => return ToolOutput::Error { message: e },
                };
                // Linear supplies the canonical branch name per issue.
                let branch = match input.get("branch").and_then(|v| v.as_str()) {
                    Some(branch) => branch.to_string(),
                    None => match linear_graphql(
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
                let checkout = format!(
                    "git checkout -b {b} 2>/dev/null || git checkout {b}",
                    b = quote(&branch)
                );
                // `exec::run` returns Ok((exit_code, _)) on any completion, so
                // checking only its Err (spawn/timeout) let a FAILED checkout
                // (non-zero exit — dirty tree, protected branch) slip through
                // and still move the issue to in-progress. Gate on the code.
                match exec::run(&checkout, root, 30).await {
                    Ok((0, _)) => {}
                    Ok((code, output)) => {
                        return ToolOutput::Error {
                            message: format!(
                                "git checkout of `{branch}` failed (exit {code}) — \
                                 issue left unchanged: {output}"
                            ),
                        };
                    }
                    Err(e) => return ToolOutput::Error { message: e },
                }
                let state = match linear_state_id(api_key, &team, "started").await {
                    Ok(state) => state,
                    Err(e) => return ToolOutput::Error { message: e },
                };
                match linear_graphql(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> Arc<IssueBackend> {
        Arc::new(IssueBackend::GitHub)
    }

    #[test]
    fn schemas_partition_correctly() {
        assert!(SearchIssues(backend()).schema().read_only);
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
            parse_linear_identifier("OXA-123"),
            Some(("OXA".into(), 123.0))
        );
        assert_eq!(parse_linear_identifier("eng-7"), Some(("ENG".into(), 7.0)));
        assert!(parse_linear_identifier("123").is_none());
        assert!(parse_linear_identifier("-123").is_none());
        assert!(parse_linear_identifier("OXA-").is_none());
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
}
