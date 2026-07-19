//! Filesystem-derived views: skills, memories, rule files, distilled
//! lessons (`reflections.jsonl`), configured MCP servers (`mcp.toml`) and
//! the `settings.json` scope chain.
//!
//! Everything here is plain reads of files stella (or the user) already
//! wrote. Two invariants:
//!
//! 1. **Nothing is created or modified** — absent files and directories are
//!    states, rendered as empty sections.
//! 2. **Secrets never reach the browser.** `settings.json` and `mcp.toml`
//!    may carry credentials; [`redact`] scrubs any value under a
//!    sensitive-looking key (and every value under `env`/`headers` maps)
//!    before the JSON is serialized into a response.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

/// The user-scope stella config dir (`~/.config/stella`), the same path
/// `stella-cli`'s settings loader uses. `STELLA_CONFIG_DIR` overrides for
/// tests.
pub fn user_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("STELLA_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("stella"))
}

/// The org-managed settings path — mirrors `stella-cli`'s
/// `managed_settings_path` (override: `STELLA_MANAGED_SETTINGS`).
fn managed_settings_path() -> PathBuf {
    if let Some(p) = std::env::var_os("STELLA_MANAGED_SETTINGS") {
        return PathBuf::from(p);
    }
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/stella/settings.json")
    } else {
        PathBuf::from("/etc/stella/settings.json")
    }
}

/// Seconds since the epoch for a file's mtime, when the OS will say.
fn mtime_unix(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(secs).ok()
}

/// Pull `name:` and `description:` out of a `---` YAML frontmatter block
/// without a YAML dependency — stella's skill frontmatter is flat
/// `key: value` lines, which is all this reads.
fn frontmatter_fields(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let mut name = None;
    let mut description = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            match key.trim() {
                "name" if !value.is_empty() => name = Some(value.to_string()),
                "description" if !value.is_empty() => description = Some(value.to_string()),
                _ => {}
            }
        }
    }
    (name, description)
}

/// One skill entry from a markdown file.
fn skill_entry(path: &Path, scope: &str, learned: bool) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let (name, description) = frontmatter_fields(&text);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A directory skill's stem is `SKILL`; prefer the directory name then.
    let fallback = if stem.eq_ignore_ascii_case("skill") {
        path.parent()
            .and_then(Path::file_name)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(stem)
    } else {
        stem
    };
    Some(json!({
        "name": name.unwrap_or(fallback),
        "description": description.unwrap_or_default(),
        "scope": scope,
        "learned": learned,
        "path": path.display().to_string(),
        "modified_unix": mtime_unix(path),
    }))
}

/// Scan one skills root: directories holding `SKILL.md`, flat `*.md` files,
/// and the `learned/` subdirectory of self-extracted skills.
fn scan_skills_dir(dir: &Path, scope: &str, out: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name == "learned" {
                if let Ok(learned) = std::fs::read_dir(&path) {
                    for file in learned.flatten() {
                        let p = file.path();
                        if p.extension().is_some_and(|e| e == "md") {
                            out.extend(skill_entry(&p, scope, true));
                        }
                    }
                }
            } else {
                let manifest = path.join("SKILL.md");
                if manifest.is_file() {
                    out.extend(skill_entry(&manifest, scope, false));
                }
            }
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(skill_entry(&path, scope, false).unwrap_or_default());
        }
    }
}

/// Every skill visible to this workspace: project `.stella/skills` plus the
/// user-scope skills dir, with learned (self-extracted) skills flagged.
pub fn skills(workspace_root: &Path) -> Value {
    let mut rows = Vec::new();
    scan_skills_dir(&workspace_root.join(".stella/skills"), "project", &mut rows);
    if let Some(user) = user_config_dir() {
        scan_skills_dir(&user.join("skills"), "user", &mut rows);
    }
    rows.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                !v["learned"].as_bool().unwrap_or(false),
                v["name"].as_str().unwrap_or_default().to_lowercase(),
            )
        };
        key(a).cmp(&key(b))
    });
    json!(rows)
}

/// Collapse whitespace and cap for a card snippet, skipping any leading
/// `---` frontmatter block.
fn snippet(text: &str, max: usize) -> String {
    let body = match text.strip_prefix("---") {
        Some(rest) => rest.split_once("\n---").map(|(_, b)| b).unwrap_or(text),
        None => text,
    };
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max).collect();
    if collapsed.chars().count() > max {
        out.push('…');
    }
    out
}

/// Markdown files under one directory as `{name, snippet, bytes, modified}`.
fn markdown_cards(dir: &Path, snippet_len: usize) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<Value> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                return None;
            }
            let text = std::fs::read_to_string(&path).ok()?;
            Some(json!({
                "name": path.file_stem().map(|s| s.to_string_lossy().into_owned())?,
                "snippet": snippet(&text, snippet_len),
                "bytes": text.len(),
                "modified_unix": mtime_unix(&path),
            }))
        })
        .collect();
    rows.sort_by_key(|r| -r["modified_unix"].as_i64().unwrap_or(0));
    rows
}

/// Workspace memories: `.stella/memories/*.md`.
pub fn memories(workspace_root: &Path) -> Value {
    json!(markdown_cards(
        &workspace_root.join(".stella/memories"),
        400
    ))
}

/// Exploration maps from `.stella/explorations/*.json` with a per-map
/// freshness verdict computed by re-hashing each record's `path → sha256`
/// manifest against the working tree — the human-facing twin of the agents'
/// startup index (`docs/design/exploration-sharing.md` §4e). Records without
/// a manifest (pre-v2) report `"unknown"`.
pub fn explorations(workspace_root: &Path) -> Value {
    use sha2::{Digest, Sha256};
    let dir = workspace_root.join(".stella/explorations");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return json!([]);
    };
    let mut rows: Vec<Value> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(record) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        else {
            continue;
        };
        let manifest = record["manifest"].as_object().cloned().unwrap_or_default();
        let (mut changed, mut missing) = (Vec::new(), Vec::new());
        for (rel, saved) in &manifest {
            match std::fs::read(workspace_root.join(rel)) {
                Ok(bytes) => {
                    let mut hasher = Sha256::new();
                    hasher.update(&bytes);
                    let digest = hasher.finalize();
                    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                    if Some(hex.as_str()) != saved.as_str() {
                        changed.push(rel.clone());
                    }
                }
                Err(_) => missing.push(rel.clone()),
            }
        }
        let freshness = if manifest.is_empty() {
            "unknown"
        } else if changed.is_empty() && missing.is_empty() {
            "fresh"
        } else {
            "drifted"
        };
        rows.push(json!({
            "slice": record["slice"],
            "title": record["title"],
            "summary": record["summary"],
            "status": record["status"].as_str().unwrap_or("complete"),
            "pid": record["pid"],
            "created_at_ms": record["created_at_ms"],
            "git_head": record["git_head"],
            "manifest_files": manifest.len(),
            "freshness": freshness,
            "changed": changed,
            "missing": missing,
            "content_chars": record["content"].as_str().map(|s| s.chars().count()).unwrap_or(0),
        }));
    }
    rows.sort_by_key(|r| -(r["created_at_ms"].as_i64().unwrap_or(0)));
    json!(rows)
}

/// Rule files: `.stella/rules/*.md` (the db-promoted rules live in
/// [`crate::db::Observatory::memory`]'s payload).
pub fn rules_files(workspace_root: &Path) -> Value {
    json!(markdown_cards(&workspace_root.join(".stella/rules"), 400))
}

/// Distilled lessons from `.stella/reflections.jsonl` — one object per line,
/// `{lesson, domains, occurred_at}`. Unparseable lines are skipped.
pub fn lessons(workspace_root: &Path) -> Value {
    let path = workspace_root.join(".stella/reflections.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return json!([]);
    };
    let mut rows: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v["lesson"].is_string())
        .collect();
    rows.sort_by_key(|r| -r["occurred_at"].as_i64().unwrap_or(0));
    json!(rows)
}

/// Configured MCP servers from the project's `.stella/mcp.toml`. Env var
/// values and header values are never included — only their names.
pub fn mcp_servers(workspace_root: &Path) -> Value {
    let path = workspace_root.join(".stella/mcp.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return json!({ "path": path.display().to_string(), "servers": [] });
    };
    let parsed: toml::Table = match toml::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return json!({
                "path": path.display().to_string(),
                "servers": [],
                "parse_error": true,
            });
        }
    };
    let mut servers = Vec::new();
    if let Some(table) = parsed.get("servers").and_then(|s| s.as_table()) {
        for (name, def) in table {
            let transport = def
                .get("transport")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown");
            let target = match transport {
                "stdio" => {
                    let cmd = def.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
                    let args: Vec<&str> = def
                        .get("args")
                        .and_then(|a| a.as_array())
                        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                        .unwrap_or_default();
                    format!("{cmd} {}", args.join(" ")).trim().to_string()
                }
                "http" => def
                    .get("url")
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            };
            let key_names = |field: &str| -> Vec<String> {
                def.get(field)
                    .and_then(|e| e.as_table())
                    .map(|t| t.keys().cloned().collect())
                    .unwrap_or_default()
            };
            servers.push(json!({
                "name": name,
                "transport": transport,
                "target": target,
                "env_keys": key_names("env"),
                "header_keys": key_names("headers"),
            }));
        }
    }
    json!({ "path": path.display().to_string(), "servers": servers })
}

/// Does this (lowercased) key look like it holds a credential? Keys ending
/// `_env` are exempt — they name an environment variable, they don't hold
/// its value.
fn sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    if k.ends_with("_env") {
        return false;
    }
    ["key", "token", "secret", "password", "credential", "bearer"]
        .iter()
        .any(|marker| k.contains(marker))
        || k == "authorization"
}

/// Recursively scrub credentials from parsed settings before serving them.
/// Values under sensitive keys are replaced; `env` and `headers` maps have
/// every value replaced (their values are credentials by position).
fn redact(value: &mut Value, under_credential_map: bool) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                let is_credential_map = matches!(key.as_str(), "env" | "headers");
                if (under_credential_map || sensitive_key(key)) && v.is_string() {
                    *v = Value::String("<redacted>".into());
                } else {
                    redact(v, is_credential_map);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact(item, under_credential_map);
            }
        }
        _ => {}
    }
}

/// One scope of the settings chain: its path, whether it exists, and its
/// redacted contents.
fn settings_scope(scope: &str, path: PathBuf) -> Value {
    let exists = path.is_file();
    let mut body = Value::Null;
    let mut parse_error = false;
    if exists {
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        {
            Some(mut parsed) => {
                redact(&mut parsed, false);
                body = parsed;
            }
            None => parse_error = true,
        }
    }
    json!({
        "scope": scope,
        "path": path.display().to_string(),
        "exists": exists,
        "parse_error": parse_error,
        "settings": body,
    })
}

/// The full configuration picture: the settings scope chain (ascending
/// precedence: user → org-managed → project) plus where every store this
/// dashboard reads actually lives.
pub fn config(workspace_root: &Path) -> Value {
    let user = user_config_dir()
        .map(|d| d.join("settings.json"))
        .unwrap_or_else(|| PathBuf::from("settings.json"));
    let scopes = vec![
        settings_scope("user", user),
        settings_scope("managed", managed_settings_path()),
        settings_scope(
            "project",
            workspace_root.join(".stella").join("settings.json"),
        ),
    ];
    let dot = workspace_root.join(".stella");
    let store_path = |name: &str| {
        let p = dot.join(name);
        json!({ "path": p.display().to_string(), "exists": p.exists() })
    };
    let usage = crate::global::usage_db_path();
    json!({
        "scopes": scopes,
        "stores": {
            "store_db": store_path("store.db"),
            "fleet_db": store_path("fleet.db"),
            "codegraph_db": store_path("codegraph.db"),
            "context_db": store_path("context.db"),
            "usage_db": {
                "path": usage.display().to_string(),
                "exists": usage.exists(),
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_extracts_name_and_description() {
        let text = "---\nname: my-skill\ndescription: \"Does a thing\"\n---\n# Body";
        let (name, desc) = frontmatter_fields(text);
        assert_eq!(name.as_deref(), Some("my-skill"));
        assert_eq!(desc.as_deref(), Some("Does a thing"));
        assert_eq!(frontmatter_fields("no frontmatter"), (None, None));
    }

    #[test]
    fn redact_scrubs_credentials_but_keeps_env_var_names() {
        let mut v = serde_json::json!({
            "providers": { "zai": { "api_key": "sk-live-123", "api_key_env": "ZAI_KEY" } },
            "hooks": [ { "env": { "GITHUB_TOKEN": "ghp_abc" } } ],
            "mcp": { "headers": { "Authorization": "Bearer xyz" } },
            "model": "glm-5.2",
        });
        redact(&mut v, false);
        let s = v.to_string();
        assert!(!s.contains("sk-live-123"));
        assert!(!s.contains("ghp_abc"));
        assert!(!s.contains("Bearer xyz"));
        assert!(s.contains("ZAI_KEY"), "env var *names* survive");
        assert!(s.contains("glm-5.2"), "non-sensitive values survive");
    }
}
