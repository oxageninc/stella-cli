//! `grep` — search file contents with a regex. Shells to `rg` when present
//! for speed; falls back to the system `grep -rn` otherwise.

use async_trait::async_trait;
use serde_json::Value;
use stella_protocol::tool::{ToolOutput, ToolSchema};
use tokio::process::Command;

use crate::registry::Tool;

const MAX_RESULTS: usize = 200;

/// Does this rg/grep stderr describe a broken *pattern* (bad regex or a flag
/// the pattern got mistaken for) rather than benign per-file walk warnings?
/// Both tools exit 2 either way, so the message text is the only signal:
/// pattern failures carry `regex parse error` / `error:` (rg) or
/// `invalid`/`unbalanced` (grep); per-path IO warnings read `rg: <path>:
/// <os reason>` with none of those.
fn is_pattern_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("regex parse error")
        || s.contains("error:")
        || s.contains("invalid")
        || s.contains("unbalanced")
        || s.contains("unmatched")
}

/// The most informative single line of an rg/grep error, for a compact tool
/// message (the full multi-line dump is noise to the model).
fn first_error_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("error") || l.contains("invalid") || l.contains("unbalanced")
        })
        .or_else(|| stderr.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("search failed")
        .to_string()
}

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "grep".into(),
            description: "Search file contents with a regex. Returns matching file:line:text. Shells to ripgrep when available.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression to search for" },
                    "path": { "type": "string", "description": "Subdirectory to search (default: workspace root)" },
                    "glob": { "type": "string", "description": "Restrict to files matching this glob (e.g. *.rs)" }
                },
                "required": ["pattern"]
            }),
            read_only: true,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let pattern = match input.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolOutput::Error {
                    message: "missing required field `pattern`".into(),
                };
            }
        };
        let search_path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let glob_filter = input.get("glob").and_then(|v| v.as_str());

        // Confine `path` to the workspace root — `Path::join` would otherwise
        // let an absolute or `../..` path escape it. `.`/empty resolve to the
        // root itself, so existing default-scoped calls keep working.
        let search_dir = match crate::resolve_within_root(root, search_path) {
            Some(p) => p,
            None => {
                return ToolOutput::Error {
                    message: format!("path `{search_path}` escapes workspace root"),
                };
            }
        };

        // Try ripgrep first — it's the fast path.
        let mut rg = Command::new("rg");
        rg.arg("--line-number")
            .arg("--no-heading")
            .arg("--color")
            .arg("never");
        rg.arg("--max-count").arg(MAX_RESULTS.to_string());
        if let Some(g) = glob_filter {
            rg.arg("--glob").arg(g);
        }
        // `-e <pattern>` (not a bare positional) so a pattern that begins
        // with `-` — the everyday `->`, `--flag`, `-n` — is treated as the
        // search string, not parsed as an rg option.
        rg.arg("-e").arg(pattern).arg(&search_dir);
        rg.stdout(std::process::Stdio::piped());
        rg.stderr(std::process::Stdio::piped());

        match rg.output().await {
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stdout);
                if !text.is_empty() {
                    // Matches (possibly partial — rg still exits 2 if some
                    // path in a recursive walk was unreadable). Never drop
                    // real results over a benign per-file walk warning.
                    let lines: Vec<&str> = text.lines().take(MAX_RESULTS).collect();
                    let mut result = lines.join("\n");
                    if lines.len() == MAX_RESULTS {
                        result.push_str(&format!("\n... (showing first {MAX_RESULTS} matches)"));
                    }
                    return ToolOutput::Ok { content: result };
                }
                // No results. Exit 2 with a *pattern* error (bad regex, bad
                // flag) must surface — the old code masked it as
                // "(no matches)". A plain exit 2 from unreadable directories
                // during the walk is benign and stays "(no matches)".
                let stderr = String::from_utf8_lossy(&output.stderr);
                if output.status.code() == Some(2) && is_pattern_error(&stderr) {
                    return ToolOutput::Error {
                        message: format!("rg error: {}", first_error_line(&stderr)),
                    };
                }
                ToolOutput::Ok {
                    content: "(no matches)".into(),
                }
            }
            Err(_) => {
                // rg not installed — fall back to grep
                let mut grep = Command::new("grep");
                grep.arg("-rn").arg("--color=never");
                if let Some(g) = glob_filter {
                    grep.arg("--include").arg(g);
                }
                // `-e <pattern>` for the same leading-`-` safety as rg above.
                grep.arg("-e").arg(pattern).arg(&search_dir);
                grep.stdout(std::process::Stdio::piped());
                grep.stderr(std::process::Stdio::piped());

                match grep.output().await {
                    Ok(output) => {
                        let text = String::from_utf8_lossy(&output.stdout);
                        if !text.is_empty() {
                            let lines: Vec<&str> = text.lines().take(MAX_RESULTS).collect();
                            return ToolOutput::Ok {
                                content: lines.join("\n"),
                            };
                        }
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if output.status.code() == Some(2) && is_pattern_error(&stderr) {
                            return ToolOutput::Error {
                                message: format!("grep error: {}", first_error_line(&stderr)),
                            };
                        }
                        ToolOutput::Ok {
                            content: "(no matches)".into(),
                        }
                    }
                    Err(e) => ToolOutput::Error {
                        message: format!("grep failed: {e}"),
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_matching_lines() {
        let dir = std::env::temp_dir();
        let path = format!("stella_grep_{}.txt", std::process::id());
        tokio::fs::write(dir.join(&path), "hello world\nfoo bar\nhello again\n")
            .await
            .unwrap();

        let result = Grep
            .execute(&serde_json::json!({"pattern": "hello", "path": path}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => {
                assert!(content.contains("hello world") || content.contains("hello"));
            }
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
        let _ = tokio::fs::remove_file(dir.join(&path)).await;
    }

    #[tokio::test]
    async fn no_matches_returns_ok_empty() {
        let dir = std::env::temp_dir();
        let result = Grep
            .execute(&serde_json::json!({"pattern": "zzz_not_found_xyz"}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => assert!(content.contains("no matches")),
            ToolOutput::Error { message } => panic!("expected ok, got: {message}"),
        }
    }

    /// A pattern that begins with `-` (the everyday `->`, `-n`, `--foo`)
    /// must be searched, not parsed by rg/grep as an option. Before `-e`
    /// this errored and was swallowed as "(no matches)".
    #[tokio::test]
    async fn a_leading_dash_pattern_is_searched_not_parsed_as_a_flag() {
        let dir = std::env::temp_dir().join(format!("stella_grep_dash_{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("code.rs"), "fn f() -> i32 { 0 }\n")
            .await
            .unwrap();

        let result = Grep
            .execute(&serde_json::json!({"pattern": "->"}), &dir)
            .await;
        match result {
            ToolOutput::Ok { content } => assert!(
                content.contains("->"),
                "the arrow pattern should match the fixture, got: {content}"
            ),
            ToolOutput::Error { message } => {
                panic!("a `->` search should succeed, got error: {message}")
            }
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// An invalid regex must surface as an error, not be swallowed as
    /// "(no matches)" — otherwise the agent silently believes its search
    /// found nothing when the query itself was broken.
    #[tokio::test]
    async fn an_invalid_regex_surfaces_an_error() {
        let dir = std::env::temp_dir().join(format!("stella_grep_badre_{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("f.txt"), "hello\n")
            .await
            .unwrap();

        // Unclosed character class — invalid for both rg and grep.
        let result = Grep
            .execute(&serde_json::json!({"pattern": "[unclosed"}), &dir)
            .await;
        assert!(
            matches!(result, ToolOutput::Error { .. }),
            "a broken regex must report an error, got: {result:?}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
