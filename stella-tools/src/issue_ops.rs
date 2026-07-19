//! Structured issue-tracker operations shared by the agent tools
//! ([`crate::issues`]) and the Command Deck's issue panel.
//!
//! Every operation dispatches on [`crate::issues::IssueBackend`]:
//! - `GitHub` — the authenticated `gh` CLI (shell-out, like `ci_status`).
//! - `GitHubApi` — the REST client in [`crate::github_rest`], used when the
//!   user ran `stella connect github` (works without the `gh` binary).
//! - `Linear` — the GraphQL API.
//!
//! Results are typed (`IssueSummary`, `LabelInfo`, `MemberInfo`, …) so the
//! TUI can render rows and the tools can format text, from one code path.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::exec;
use crate::github_rest::{GitHubRest, repo_slug};
use crate::issues::IssueBackend;

const TIMEOUT_SECS: u64 = 60;

// ── Typed results ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IssueSummary {
    /// `#123` (GitHub) or `ENG-123` (Linear).
    pub key: String,
    pub title: String,
    pub state: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    pub url: String,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommentInfo {
    pub author: String,
    pub created_at: String,
    pub body: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IssueDetail {
    #[serde(flatten)]
    pub summary: IssueSummary,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub comments: Vec<CommentInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LabelInfo {
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemberInfo {
    /// What goes into an assignee field: `@login` (GitHub) or an email
    /// (Linear).
    pub handle: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct IssueFilters {
    pub query: Option<String>,
    /// `open` (default), `closed`, or `all`.
    pub state: Option<String>,
    pub assignee: Option<String>,
    pub label: Option<String>,
    pub limit: u64,
}

impl IssueFilters {
    fn limit(&self) -> u64 {
        if self.limit == 0 {
            20
        } else {
            self.limit.min(100)
        }
    }
    fn state(&self) -> &str {
        self.state.as_deref().unwrap_or("open")
    }
}

#[derive(Debug, Clone, Default)]
pub struct CreateParams {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignee: Option<String>,
    /// Linear team key; defaults to the first visible team.
    pub team: Option<String>,
}

// ── Shared helpers ──────────────────────────────────────────────────────────

pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

async fn gh_json(args: String, root: &std::path::Path) -> Result<Value, String> {
    let (code, output) = exec::run(&args, root, TIMEOUT_SECS).await?;
    if code != 0 {
        return Err(format!("gh failed (exit {code}): {output}"));
    }
    serde_json::from_str(&strip_ansi(output.trim()))
        .map_err(|e| format!("gh returned unparsable JSON ({e}) — output may be truncated"))
}

/// Strip ANSI escape sequences from CLI output. A raw 0x1b byte is invalid
/// inside a JSON string, so this can never corrupt well-formed JSON — it
/// only rescues output from a `gh` that colorized despite the pipe (a
/// force-color override the exec env scrub didn't cover).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: `ESC [`, parameter/intermediate bytes, one final `@`–`~`.
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if matches!(c, '@'..='~') {
                        break;
                    }
                }
            }
            // OSC: `ESC ]` … terminated by BEL or `ESC \`.
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

async fn gh_run(args: String, root: &std::path::Path) -> Result<String, String> {
    let (code, output) = exec::run(&args, root, TIMEOUT_SECS).await?;
    if code != 0 {
        return Err(format!("gh failed (exit {code}): {output}"));
    }
    Ok(output)
}

pub(crate) async fn linear_graphql(
    api_url: &str,
    api_key: &str,
    query: &str,
    variables: Value,
) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let response = client
        .post(api_url)
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
pub(crate) fn parse_linear_identifier(identifier: &str) -> Option<(String, f64)> {
    let (team, number) = identifier.rsplit_once('-')?;
    let number: f64 = number.parse().ok()?;
    if team.is_empty() {
        return None;
    }
    Some((team.to_uppercase(), number))
}

/// Resolve a Linear issue's node id (and team id) from its identifier.
pub(crate) async fn linear_issue_id(
    api_url: &str,
    api_key: &str,
    identifier: &str,
) -> Result<(String, String), String> {
    let Some((team, number)) = parse_linear_identifier(identifier) else {
        return Err(format!(
            "`{identifier}` is not a Linear identifier (expected e.g. ENG-123)"
        ));
    };
    let data = linear_graphql(
        api_url,
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
/// "completed"), or a state whose *name* matches `name_hint`.
pub(crate) async fn linear_state_id(
    api_url: &str,
    api_key: &str,
    team_id: &str,
    state_type: Option<&str>,
    name_hint: Option<&str>,
) -> Result<String, String> {
    let data = linear_graphql(
        api_url,
        api_key,
        "query($team: String!) { team(id: $team) { states { nodes { id name type } } } }",
        json!({ "team": team_id }),
    )
    .await?;
    let nodes = data["team"]["states"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    // A name match beats a type match: "In Review" should hit the state
    // named In Review even though its type is "started".
    if let Some(hint) = name_hint {
        let wanted = hint.to_lowercase();
        if let Some(id) = nodes
            .iter()
            .find(|n| {
                n["name"]
                    .as_str()
                    .is_some_and(|name| name.to_lowercase() == wanted)
            })
            .and_then(|node| node["id"].as_str())
        {
            return Ok(id.to_string());
        }
    }
    if let Some(state_type) = state_type
        && let Some(id) = nodes
            .iter()
            .find(|n| n["type"].as_str() == Some(state_type))
            .and_then(|n| n["id"].as_str())
    {
        return Ok(id.to_string());
    }
    let available: Vec<&str> = nodes.iter().filter_map(|n| n["name"].as_str()).collect();
    Err(format!(
        "no matching workflow state — available: {}",
        available.join(", ")
    ))
}

/// Map a human status word onto a Linear state lookup: known type words map
/// by type; anything else matches a state name.
fn linear_status_lookup(status: &str) -> (Option<&str>, Option<&str>) {
    match status.to_lowercase().as_str() {
        "open" | "todo" | "unstarted" => (Some("unstarted"), Some(status)),
        "backlog" => (Some("backlog"), Some(status)),
        "triage" => (Some("triage"), Some(status)),
        "in progress" | "in_progress" | "started" => (Some("started"), Some(status)),
        "done" | "closed" | "completed" => (Some("completed"), Some(status)),
        "canceled" | "cancelled" => (Some("canceled"), Some(status)),
        _ => (None, Some(status)),
    }
}

/// Resolve a Linear user id from an assignee string (email, name, or
/// display name — `@` prefix tolerated).
async fn linear_user_id(api_url: &str, api_key: &str, assignee: &str) -> Result<String, String> {
    let term = assignee.trim_start_matches('@');
    let data = linear_graphql(
        api_url,
        api_key,
        "query($term: String!) { users(filter: { or: [\
           { email: { containsIgnoreCase: $term } },\
           { name: { containsIgnoreCase: $term } },\
           { displayName: { containsIgnoreCase: $term } }\
         ] }, first: 2) { nodes { id name email } } }",
        json!({ "term": term }),
    )
    .await?;
    let nodes = data["users"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    match nodes.len() {
        0 => Err(format!("no Linear user matches `{assignee}`")),
        1 => Ok(nodes[0]["id"].as_str().unwrap_or_default().to_string()),
        _ => {
            // Two hits: prefer an exact email match, else ambiguous.
            let exact = nodes.iter().find(|n| {
                n["email"]
                    .as_str()
                    .is_some_and(|e| e.eq_ignore_ascii_case(term))
            });
            match exact {
                Some(node) => Ok(node["id"].as_str().unwrap_or_default().to_string()),
                None => Err(format!(
                    "`{assignee}` is ambiguous — use the exact email (matches: {})",
                    nodes
                        .iter()
                        .filter_map(|n| n["email"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
    }
}

/// Resolve Linear label names to ids within a team (case-insensitive).
async fn linear_label_ids(
    api_url: &str,
    api_key: &str,
    names: &[String],
) -> Result<Vec<String>, String> {
    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        let data = linear_graphql(
            api_url,
            api_key,
            "query($name: String!) { issueLabels(filter: { name: { eqIgnoreCase: $name } }, \
               first: 1) { nodes { id } } }",
            json!({ "name": name }),
        )
        .await?;
        match data["issueLabels"]["nodes"]
            .get(0)
            .and_then(|n| n["id"].as_str())
        {
            Some(id) => ids.push(id.to_string()),
            None => {
                return Err(format!(
                    "no Linear label named `{name}` — use list_labels to search"
                ));
            }
        }
    }
    Ok(ids)
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

// ── GitHub JSON shaping (shared by CLI + REST backends) ─────────────────────

fn github_issue_summary(node: &Value) -> IssueSummary {
    let number = node["number"].as_u64().unwrap_or(0);
    IssueSummary {
        key: format!("#{number}"),
        title: node["title"].as_str().unwrap_or("").to_string(),
        state: node["state"].as_str().unwrap_or("").to_lowercase(),
        labels: node["labels"]
            .as_array()
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| l["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        assignee: node["assignees"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|a| a["login"].as_str())
            .map(|login| format!("@{login}")),
        url: node
            .get("url")
            .and_then(|v| v.as_str())
            .or_else(|| node.get("html_url").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string(),
        updated_at: node
            .get("updatedAt")
            .and_then(|v| v.as_str())
            .or_else(|| node.get("updated_at").and_then(|v| v.as_str()))
            .map(str::to_string),
    }
}

fn linear_issue_summary(node: &Value) -> IssueSummary {
    IssueSummary {
        key: node["identifier"].as_str().unwrap_or("?").to_string(),
        title: node["title"].as_str().unwrap_or("").to_string(),
        state: node["state"]["name"].as_str().unwrap_or("").to_string(),
        labels: node["labels"]["nodes"]
            .as_array()
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| l["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        assignee: node["assignee"]["displayName"]
            .as_str()
            .or_else(|| node["assignee"]["name"].as_str())
            .map(str::to_string),
        url: node["url"].as_str().unwrap_or("").to_string(),
        updated_at: node["updatedAt"].as_str().map(str::to_string),
    }
}

const LINEAR_SUMMARY_FIELDS: &str = "identifier title url updatedAt \
     state { name } assignee { name displayName } labels { nodes { name } }";

// ── Operations ──────────────────────────────────────────────────────────────

/// List or search issues, with optional state/assignee/label filters.
pub async fn list_issues(
    backend: &IssueBackend,
    root: &std::path::Path,
    filters: &IssueFilters,
) -> Result<Vec<IssueSummary>, String> {
    let limit = filters.limit();
    match backend {
        IssueBackend::GitHub => {
            let mut cmd = format!(
                "gh issue list --state {} --limit {limit} \
                 --json number,title,state,labels,assignees,url,updatedAt",
                quote(filters.state())
            );
            if let Some(query) = &filters.query {
                cmd.push_str(&format!(" --search {}", quote(query)));
            }
            if let Some(assignee) = &filters.assignee {
                cmd.push_str(&format!(
                    " --assignee {}",
                    quote(assignee.trim_start_matches('@'))
                ));
            }
            if let Some(label) = &filters.label {
                cmd.push_str(&format!(" --label {}", quote(label)));
            }
            let nodes = gh_json(cmd, root).await?;
            Ok(nodes
                .as_array()
                .map(|nodes| nodes.iter().map(github_issue_summary).collect())
                .unwrap_or_default())
        }
        IssueBackend::GitHubApi { token, api_base } => {
            let client = GitHubRest::with_base(token, api_base);
            let slug = repo_slug(root).await?;
            let mut path = format!(
                "/repos/{slug}/issues?state={}&per_page={limit}",
                match filters.state() {
                    "all" => "all",
                    "closed" => "closed",
                    _ => "open",
                }
            );
            if let Some(assignee) = &filters.assignee {
                path.push_str(&format!("&assignee={}", assignee.trim_start_matches('@')));
            }
            if let Some(label) = &filters.label {
                path.push_str(&format!("&labels={}", urlencode(label)));
            }
            let nodes = client.request(reqwest::Method::GET, &path, None).await?;
            let mut issues: Vec<IssueSummary> = nodes
                .as_array()
                .map(|nodes| {
                    nodes
                        .iter()
                        // The issues endpoint returns PRs too; drop them.
                        .filter(|n| n.get("pull_request").is_none())
                        .map(github_issue_summary)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(query) = &filters.query {
                issues.retain(|i| contains_ci(&i.title, query));
            }
            Ok(issues)
        }
        IssueBackend::Linear { api_key, api_url } => {
            let mut filter = serde_json::Map::new();
            match filters.state() {
                "closed" => {
                    filter.insert(
                        "state".into(),
                        json!({ "type": { "in": ["completed", "canceled"] } }),
                    );
                }
                "all" => {}
                _ => {
                    filter.insert(
                        "state".into(),
                        json!({ "type": { "nin": ["completed", "canceled"] } }),
                    );
                }
            }
            if let Some(assignee) = &filters.assignee {
                let term = assignee.trim_start_matches('@');
                filter.insert(
                    "assignee".into(),
                    json!({ "or": [
                        { "email": { "containsIgnoreCase": term } },
                        { "name": { "containsIgnoreCase": term } },
                        { "displayName": { "containsIgnoreCase": term } }
                    ] }),
                );
            }
            if let Some(label) = &filters.label {
                filter.insert(
                    "labels".into(),
                    json!({ "name": { "containsIgnoreCase": label } }),
                );
            }
            if let Some(query) = &filters.query {
                filter.insert("title".into(), json!({ "containsIgnoreCase": query }));
            }
            let query = format!(
                "query($filter: IssueFilter, $first: Int!) {{ \
                   issues(filter: $filter, first: $first, orderBy: updatedAt) {{ \
                     nodes {{ {LINEAR_SUMMARY_FIELDS} }} }} }}"
            );
            let data = linear_graphql(
                api_url,
                api_key,
                &query,
                json!({ "filter": Value::Object(filter), "first": limit }),
            )
            .await?;
            Ok(data["issues"]["nodes"]
                .as_array()
                .map(|nodes| nodes.iter().map(linear_issue_summary).collect())
                .unwrap_or_default())
        }
    }
}

/// Full detail for one issue, including its comment thread.
pub async fn get_issue(
    backend: &IssueBackend,
    root: &std::path::Path,
    key: &str,
) -> Result<IssueDetail, String> {
    match backend {
        IssueBackend::GitHub => {
            let node = gh_json(
                format!(
                    "gh issue view {} --json number,title,state,body,labels,assignees,url,updatedAt,comments",
                    quote(key)
                ),
                root,
            )
            .await?;
            let comments = node["comments"]
                .as_array()
                .map(|comments| {
                    comments
                        .iter()
                        .map(|c| CommentInfo {
                            author: c["author"]["login"].as_str().unwrap_or("?").to_string(),
                            created_at: c["createdAt"].as_str().unwrap_or("").to_string(),
                            body: c["body"].as_str().unwrap_or("").to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(IssueDetail {
                summary: github_issue_summary(&node),
                body: node["body"].as_str().unwrap_or("").to_string(),
                comments,
            })
        }
        IssueBackend::GitHubApi { token, api_base } => {
            let client = GitHubRest::with_base(token, api_base);
            let slug = repo_slug(root).await?;
            let number = key.trim_start_matches('#');
            let node = client
                .request(
                    reqwest::Method::GET,
                    &format!("/repos/{slug}/issues/{number}"),
                    None,
                )
                .await?;
            let raw_comments = client
                .request(
                    reqwest::Method::GET,
                    &format!("/repos/{slug}/issues/{number}/comments?per_page=50"),
                    None,
                )
                .await?;
            let comments = raw_comments
                .as_array()
                .map(|comments| {
                    comments
                        .iter()
                        .map(|c| CommentInfo {
                            author: c["user"]["login"].as_str().unwrap_or("?").to_string(),
                            created_at: c["created_at"].as_str().unwrap_or("").to_string(),
                            body: c["body"].as_str().unwrap_or("").to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(IssueDetail {
                summary: github_issue_summary(&node),
                body: node["body"].as_str().unwrap_or("").to_string(),
                comments,
            })
        }
        IssueBackend::Linear { api_key, api_url } => {
            let (id, _team) = linear_issue_id(api_url, api_key, key).await?;
            let query = format!(
                "query($id: String!) {{ issue(id: $id) {{ {LINEAR_SUMMARY_FIELDS} description \
                   comments(first: 50) {{ nodes {{ body createdAt user {{ displayName name }} }} }} }} }}"
            );
            let data = linear_graphql(api_url, api_key, &query, json!({ "id": id })).await?;
            let node = &data["issue"];
            let comments = node["comments"]["nodes"]
                .as_array()
                .map(|comments| {
                    comments
                        .iter()
                        .map(|c| CommentInfo {
                            author: c["user"]["displayName"]
                                .as_str()
                                .or_else(|| c["user"]["name"].as_str())
                                .unwrap_or("?")
                                .to_string(),
                            created_at: c["createdAt"].as_str().unwrap_or("").to_string(),
                            body: c["body"].as_str().unwrap_or("").to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(IssueDetail {
                summary: linear_issue_summary(node),
                body: node["description"].as_str().unwrap_or("").to_string(),
                comments,
            })
        }
    }
}

/// Create an issue; returns its key and URL.
pub async fn create_issue(
    backend: &IssueBackend,
    root: &std::path::Path,
    params: &CreateParams,
) -> Result<IssueSummary, String> {
    match backend {
        IssueBackend::GitHub => {
            let mut cmd = format!(
                "gh issue create --title {} --body {}",
                quote(&params.title),
                quote(&params.body)
            );
            for label in &params.labels {
                cmd.push_str(&format!(" --label {}", quote(label)));
            }
            if let Some(assignee) = &params.assignee {
                cmd.push_str(&format!(
                    " --assignee {}",
                    quote(assignee.trim_start_matches('@'))
                ));
            }
            let output = gh_run(cmd, root).await?;
            let url = output.trim().to_string();
            let key = url
                .rsplit('/')
                .next()
                .map(|n| format!("#{n}"))
                .unwrap_or_default();
            Ok(IssueSummary {
                key,
                title: params.title.clone(),
                state: "open".into(),
                labels: params.labels.clone(),
                assignee: params.assignee.clone(),
                url,
                updated_at: None,
            })
        }
        IssueBackend::GitHubApi { token, api_base } => {
            let client = GitHubRest::with_base(token, api_base);
            let slug = repo_slug(root).await?;
            let mut body = json!({ "title": params.title, "body": params.body });
            if !params.labels.is_empty() {
                body["labels"] = json!(params.labels);
            }
            if let Some(assignee) = &params.assignee {
                body["assignees"] = json!([assignee.trim_start_matches('@')]);
            }
            let node = client
                .request(
                    reqwest::Method::POST,
                    &format!("/repos/{slug}/issues"),
                    Some(&body),
                )
                .await?;
            Ok(github_issue_summary(&node))
        }
        IssueBackend::Linear { api_key, api_url } => {
            // Resolve the team: input key, else the first team.
            let data = linear_graphql(
                api_url,
                api_key,
                "query { teams(first: 50) { nodes { id key } } }",
                json!({}),
            )
            .await?;
            let nodes = data["teams"]["nodes"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let team = match &params.team {
                Some(key) => nodes
                    .iter()
                    .find(|n| n["key"].as_str() == Some(&key.to_uppercase()))
                    .ok_or_else(|| format!("no Linear team with key `{key}`"))?,
                None => nodes
                    .first()
                    .ok_or("no Linear teams visible to this credential")?,
            };
            let team_id = team["id"].as_str().unwrap_or_default().to_string();

            let mut input = json!({
                "teamId": team_id,
                "title": params.title,
                "description": params.body,
            });
            if !params.labels.is_empty() {
                let ids = linear_label_ids(api_url, api_key, &params.labels).await?;
                input["labelIds"] = json!(ids);
            }
            if let Some(assignee) = &params.assignee {
                input["assigneeId"] = json!(linear_user_id(api_url, api_key, assignee).await?);
            }
            let query = format!(
                "mutation($input: IssueCreateInput!) {{ issueCreate(input: $input) {{ \
                   issue {{ {LINEAR_SUMMARY_FIELDS} }} }} }}"
            );
            let data = linear_graphql(api_url, api_key, &query, json!({ "input": input })).await?;
            Ok(linear_issue_summary(&data["issueCreate"]["issue"]))
        }
    }
}

/// Move an issue to a named status. GitHub knows `open`/`closed`; Linear
/// matches a workflow-state name or type (`in progress`, `done`, `Backlog`,
/// a custom state name, …).
pub async fn set_status(
    backend: &IssueBackend,
    root: &std::path::Path,
    key: &str,
    status: &str,
) -> Result<String, String> {
    match backend {
        IssueBackend::GitHub => {
            let action = match status.to_lowercase().as_str() {
                "closed" | "close" | "done" | "completed" => "close",
                "open" | "reopen" | "reopened" => "reopen",
                other => {
                    return Err(format!(
                        "GitHub issues are `open` or `closed` — `{other}` is not a GitHub state"
                    ));
                }
            };
            gh_run(format!("gh issue {action} {}", quote(key)), root).await?;
            Ok(format!("{key} is now {status}"))
        }
        IssueBackend::GitHubApi { token, api_base } => {
            let state = match status.to_lowercase().as_str() {
                "closed" | "close" | "done" | "completed" => "closed",
                "open" | "reopen" | "reopened" => "open",
                other => {
                    return Err(format!(
                        "GitHub issues are `open` or `closed` — `{other}` is not a GitHub state"
                    ));
                }
            };
            let client = GitHubRest::with_base(token, api_base);
            let slug = repo_slug(root).await?;
            let number = key.trim_start_matches('#');
            client
                .request(
                    reqwest::Method::PATCH,
                    &format!("/repos/{slug}/issues/{number}"),
                    Some(&json!({ "state": state })),
                )
                .await?;
            Ok(format!("{key} is now {state}"))
        }
        IssueBackend::Linear { api_key, api_url } => {
            let (id, team) = linear_issue_id(api_url, api_key, key).await?;
            let (state_type, name_hint) = linear_status_lookup(status);
            let state_id = linear_state_id(api_url, api_key, &team, state_type, name_hint).await?;
            linear_graphql(
                api_url,
                api_key,
                "mutation($id: String!, $input: IssueUpdateInput!) { \
                   issueUpdate(id: $id, input: $input) { success } }",
                json!({ "id": id, "input": { "stateId": state_id } }),
            )
            .await?;
            Ok(format!("{key} is now {status}"))
        }
    }
}

/// Add a comment to an issue.
pub async fn add_comment(
    backend: &IssueBackend,
    root: &std::path::Path,
    key: &str,
    body: &str,
) -> Result<(), String> {
    match backend {
        IssueBackend::GitHub => gh_run(
            format!("gh issue comment {} --body {}", quote(key), quote(body)),
            root,
        )
        .await
        .map(|_| ()),
        IssueBackend::GitHubApi { token, api_base } => {
            let client = GitHubRest::with_base(token, api_base);
            let slug = repo_slug(root).await?;
            let number = key.trim_start_matches('#');
            client
                .request(
                    reqwest::Method::POST,
                    &format!("/repos/{slug}/issues/{number}/comments"),
                    Some(&json!({ "body": body })),
                )
                .await
                .map(|_| ())
        }
        IssueBackend::Linear { api_key, api_url } => {
            let (id, _team) = linear_issue_id(api_url, api_key, key).await?;
            linear_graphql(
                api_url,
                api_key,
                "mutation($input: CommentCreateInput!) { commentCreate(input: $input) { success } }",
                json!({ "input": { "issueId": id, "body": body } }),
            )
            .await
            .map(|_| ())
        }
    }
}

/// Update an issue's title/body, add labels, and/or set the assignee — any
/// combination; `None`/empty means "leave unchanged".
pub async fn update_issue(
    backend: &IssueBackend,
    root: &std::path::Path,
    key: &str,
    title: Option<&str>,
    body: Option<&str>,
    add_labels: &[String],
    assignee: Option<&str>,
) -> Result<(), String> {
    match backend {
        IssueBackend::GitHub => {
            let mut edits = String::new();
            if let Some(title) = title {
                edits.push_str(&format!(" --title {}", quote(title)));
            }
            if let Some(body) = body {
                edits.push_str(&format!(" --body {}", quote(body)));
            }
            for label in add_labels {
                edits.push_str(&format!(" --add-label {}", quote(label)));
            }
            if let Some(assignee) = assignee {
                edits.push_str(&format!(
                    " --add-assignee {}",
                    quote(assignee.trim_start_matches('@'))
                ));
            }
            if edits.is_empty() {
                return Ok(());
            }
            gh_run(format!("gh issue edit {}{edits}", quote(key)), root)
                .await
                .map(|_| ())
        }
        IssueBackend::GitHubApi { token, api_base } => {
            let client = GitHubRest::with_base(token, api_base);
            let slug = repo_slug(root).await?;
            let number = key.trim_start_matches('#');
            let mut patch = serde_json::Map::new();
            if let Some(title) = title {
                patch.insert("title".into(), json!(title));
            }
            if let Some(body) = body {
                patch.insert("body".into(), json!(body));
            }
            if !patch.is_empty() {
                client
                    .request(
                        reqwest::Method::PATCH,
                        &format!("/repos/{slug}/issues/{number}"),
                        Some(&Value::Object(patch)),
                    )
                    .await?;
            }
            if !add_labels.is_empty() {
                client
                    .request(
                        reqwest::Method::POST,
                        &format!("/repos/{slug}/issues/{number}/labels"),
                        Some(&json!({ "labels": add_labels })),
                    )
                    .await?;
            }
            if let Some(assignee) = assignee {
                client
                    .request(
                        reqwest::Method::POST,
                        &format!("/repos/{slug}/issues/{number}/assignees"),
                        Some(&json!({ "assignees": [assignee.trim_start_matches('@')] })),
                    )
                    .await?;
            }
            Ok(())
        }
        IssueBackend::Linear { api_key, api_url } => {
            let (id, _team) = linear_issue_id(api_url, api_key, key).await?;
            let mut input = serde_json::Map::new();
            if let Some(title) = title {
                input.insert("title".into(), json!(title));
            }
            if let Some(body) = body {
                input.insert("description".into(), json!(body));
            }
            if !add_labels.is_empty() {
                // labelIds REPLACES the set — union with the existing ones.
                let data = linear_graphql(
                    api_url,
                    api_key,
                    "query($id: String!) { issue(id: $id) { labels { nodes { id } } } }",
                    json!({ "id": id }),
                )
                .await?;
                let mut ids: Vec<String> = data["issue"]["labels"]["nodes"]
                    .as_array()
                    .map(|nodes| {
                        nodes
                            .iter()
                            .filter_map(|n| n["id"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                for new_id in linear_label_ids(api_url, api_key, add_labels).await? {
                    if !ids.contains(&new_id) {
                        ids.push(new_id);
                    }
                }
                input.insert("labelIds".into(), json!(ids));
            }
            if let Some(assignee) = assignee {
                input.insert(
                    "assigneeId".into(),
                    json!(linear_user_id(api_url, api_key, assignee).await?),
                );
            }
            if input.is_empty() {
                return Ok(());
            }
            linear_graphql(
                api_url,
                api_key,
                "mutation($id: String!, $input: IssueUpdateInput!) { \
                   issueUpdate(id: $id, input: $input) { success } }",
                json!({ "id": id, "input": Value::Object(input) }),
            )
            .await
            .map(|_| ())
        }
    }
}

/// Search labels by substring — so nobody has to know label names by heart.
/// An empty query lists them all (up to `limit`).
pub async fn search_labels(
    backend: &IssueBackend,
    root: &std::path::Path,
    query: &str,
    limit: u64,
) -> Result<Vec<LabelInfo>, String> {
    let limit = if limit == 0 { 30 } else { limit.min(100) };
    match backend {
        IssueBackend::GitHub => {
            let nodes = gh_json(
                "gh label list --limit 100 --json name,color,description".to_string(),
                root,
            )
            .await?;
            Ok(filter_labels(
                nodes
                    .as_array()
                    .map(|nodes| {
                        nodes
                            .iter()
                            .map(|n| LabelInfo {
                                name: n["name"].as_str().unwrap_or("").to_string(),
                                color: n["color"].as_str().map(str::to_string),
                                description: n["description"]
                                    .as_str()
                                    .filter(|d| !d.is_empty())
                                    .map(str::to_string),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                query,
                limit,
            ))
        }
        IssueBackend::GitHubApi { token, api_base } => {
            let client = GitHubRest::with_base(token, api_base);
            let slug = repo_slug(root).await?;
            let nodes = client
                .request(
                    reqwest::Method::GET,
                    &format!("/repos/{slug}/labels?per_page=100"),
                    None,
                )
                .await?;
            Ok(filter_labels(
                nodes
                    .as_array()
                    .map(|nodes| {
                        nodes
                            .iter()
                            .map(|n| LabelInfo {
                                name: n["name"].as_str().unwrap_or("").to_string(),
                                color: n["color"].as_str().map(str::to_string),
                                description: n["description"]
                                    .as_str()
                                    .filter(|d| !d.is_empty())
                                    .map(str::to_string),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                query,
                limit,
            ))
        }
        IssueBackend::Linear { api_key, api_url } => {
            let (graphql, variables) = if query.trim().is_empty() {
                (
                    "query($first: Int!) { issueLabels(first: $first) { \
                       nodes { name color description } } }",
                    json!({ "first": limit }),
                )
            } else {
                (
                    "query($term: String!, $first: Int!) { \
                       issueLabels(filter: { name: { containsIgnoreCase: $term } }, first: $first) { \
                         nodes { name color description } } }",
                    json!({ "term": query, "first": limit }),
                )
            };
            let data = linear_graphql(api_url, api_key, graphql, variables).await?;
            Ok(data["issueLabels"]["nodes"]
                .as_array()
                .map(|nodes| {
                    nodes
                        .iter()
                        .map(|n| LabelInfo {
                            name: n["name"].as_str().unwrap_or("").to_string(),
                            color: n["color"].as_str().map(str::to_string),
                            description: n["description"]
                                .as_str()
                                .filter(|d| !d.is_empty())
                                .map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default())
        }
    }
}

fn filter_labels(mut labels: Vec<LabelInfo>, query: &str, limit: u64) -> Vec<LabelInfo> {
    if !query.trim().is_empty() {
        labels.retain(|l| {
            contains_ci(&l.name, query)
                || l.description
                    .as_deref()
                    .is_some_and(|d| contains_ci(d, query))
        });
    }
    labels.truncate(limit as usize);
    labels
}

/// Search assignable people by substring (login, name, or email). An empty
/// query lists them all (up to `limit`).
pub async fn search_members(
    backend: &IssueBackend,
    root: &std::path::Path,
    query: &str,
    limit: u64,
) -> Result<Vec<MemberInfo>, String> {
    let limit = if limit == 0 { 30 } else { limit.min(100) };
    match backend {
        IssueBackend::GitHub => {
            // `gh api` resolves {owner}/{repo} from the workspace remote.
            let nodes = gh_json(
                "gh api 'repos/{owner}/{repo}/assignees?per_page=100'".to_string(),
                root,
            )
            .await?;
            Ok(filter_members(github_members(&nodes), query, limit))
        }
        IssueBackend::GitHubApi { token, api_base } => {
            let client = GitHubRest::with_base(token, api_base);
            let slug = repo_slug(root).await?;
            let nodes = client
                .request(
                    reqwest::Method::GET,
                    &format!("/repos/{slug}/assignees?per_page=100"),
                    None,
                )
                .await?;
            Ok(filter_members(github_members(&nodes), query, limit))
        }
        IssueBackend::Linear { api_key, api_url } => {
            let (graphql, variables) = if query.trim().is_empty() {
                (
                    "query($first: Int!) { users(first: $first) { \
                       nodes { name displayName email active } } }",
                    json!({ "first": limit }),
                )
            } else {
                (
                    "query($term: String!, $first: Int!) { users(filter: { or: [\
                       { email: { containsIgnoreCase: $term } },\
                       { name: { containsIgnoreCase: $term } },\
                       { displayName: { containsIgnoreCase: $term } }\
                     ] }, first: $first) { nodes { name displayName email active } } }",
                    json!({ "term": query, "first": limit }),
                )
            };
            let data = linear_graphql(api_url, api_key, graphql, variables).await?;
            Ok(data["users"]["nodes"]
                .as_array()
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter(|n| n["active"].as_bool().unwrap_or(true))
                        .map(|n| MemberInfo {
                            handle: n["email"].as_str().unwrap_or("").to_string(),
                            name: n["displayName"]
                                .as_str()
                                .or_else(|| n["name"].as_str())
                                .map(str::to_string),
                            email: n["email"].as_str().map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default())
        }
    }
}

fn github_members(nodes: &Value) -> Vec<MemberInfo> {
    nodes
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| n["login"].as_str())
                .map(|login| MemberInfo {
                    handle: format!("@{login}"),
                    name: None,
                    email: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn filter_members(mut members: Vec<MemberInfo>, query: &str, limit: u64) -> Vec<MemberInfo> {
    let query = query.trim().trim_start_matches('@');
    if !query.is_empty() {
        members.retain(|m| {
            contains_ci(&m.handle, query)
                || m.name.as_deref().is_some_and(|n| contains_ci(n, query))
                || m.email.as_deref().is_some_and(|e| contains_ci(e, query))
        });
    }
    members.truncate(limit as usize);
    members
}

fn urlencode(s: &str) -> String {
    // Minimal query-value escaping for label names (spaces, `&`, `#`, `+`).
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_default_and_clamp() {
        let filters = IssueFilters::default();
        assert_eq!(filters.limit(), 20);
        assert_eq!(filters.state(), "open");
        let filters = IssueFilters {
            limit: 500,
            state: Some("closed".into()),
            ..Default::default()
        };
        assert_eq!(filters.limit(), 100);
        assert_eq!(filters.state(), "closed");
    }

    #[test]
    fn github_summary_reads_both_rest_and_cli_shapes() {
        // gh CLI shape (camelCase url/updatedAt).
        let cli = json!({
            "number": 7, "title": "T", "state": "OPEN",
            "labels": [{ "name": "bug" }],
            "assignees": [{ "login": "octocat" }],
            "url": "https://github.com/o/r/issues/7",
            "updatedAt": "2026-07-18T00:00:00Z"
        });
        let summary = github_issue_summary(&cli);
        assert_eq!(summary.key, "#7");
        assert_eq!(summary.state, "open");
        assert_eq!(summary.assignee.as_deref(), Some("@octocat"));
        assert_eq!(summary.labels, vec!["bug"]);

        // REST shape (html_url/updated_at).
        let rest = json!({
            "number": 8, "title": "T", "state": "open",
            "labels": [], "assignees": [],
            "html_url": "https://github.com/o/r/issues/8",
            "updated_at": "2026-07-18T00:00:00Z"
        });
        let summary = github_issue_summary(&rest);
        assert_eq!(summary.url, "https://github.com/o/r/issues/8");
        assert_eq!(summary.updated_at.as_deref(), Some("2026-07-18T00:00:00Z"));
    }

    #[test]
    fn linear_status_words_map_to_state_types() {
        assert_eq!(linear_status_lookup("done").0, Some("completed"));
        assert_eq!(linear_status_lookup("In Progress").0, Some("started"));
        assert_eq!(linear_status_lookup("backlog").0, Some("backlog"));
        // Unknown words fall through to a name match.
        let (state_type, hint) = linear_status_lookup("In Review");
        assert_eq!(state_type, None);
        assert_eq!(hint, Some("In Review"));
    }

    #[test]
    fn label_and_member_filters_match_case_insensitively() {
        let labels = vec![
            LabelInfo {
                name: "Bug".into(),
                color: None,
                description: None,
            },
            LabelInfo {
                name: "infra".into(),
                color: None,
                description: Some("build & CI".into()),
            },
        ];
        let hits = filter_labels(labels.clone(), "bug", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Bug");
        // Description text matches too.
        assert_eq!(filter_labels(labels, "ci", 10).len(), 1);

        let members = vec![
            MemberInfo {
                handle: "@octocat".into(),
                name: None,
                email: None,
            },
            MemberInfo {
                handle: "mona@example.com".into(),
                name: Some("Mona Lisa".into()),
                email: Some("mona@example.com".into()),
            },
        ];
        assert_eq!(filter_members(members.clone(), "@octo", 10).len(), 1);
        assert_eq!(filter_members(members.clone(), "mona", 10).len(), 1);
        assert_eq!(filter_members(members, "", 1).len(), 1);
    }

    #[test]
    fn strip_ansi_rescues_force_colored_gh_json() {
        // The exact shape `gh issue list --json …` emits under
        // CLICOLOR_FORCE=1: every token wrapped in SGR sequences.
        let colored = "\u{1b}[1;37m[\u{1b}[m\u{1b}[1;37m]\u{1b}[m";
        assert_eq!(strip_ansi(colored), "[]");
        // OSC (BEL- and ST-terminated) and bare two-byte escapes go too;
        // JSON-encoded `` inside strings is untouched (no raw 0x1b).
        let mixed = "\u{1b}]0;title\u{7}{\"a\":\"\\u001b[1m\"}\u{1b}]8;;x\u{1b}\\\u{1b}M";
        assert_eq!(strip_ansi(mixed), "{\"a\":\"\\u001b[1m\"}");
        // Plain JSON passes through byte-identical.
        assert_eq!(strip_ansi("[{\"n\":1}]"), "[{\"n\":1}]");
    }

    #[test]
    fn urlencode_escapes_query_values() {
        assert_eq!(urlencode("good first issue"), "good%20first%20issue");
        assert_eq!(urlencode("a&b#c"), "a%26b%23c");
        assert_eq!(urlencode("plain-name_1.0~x"), "plain-name_1.0~x");
    }
}
