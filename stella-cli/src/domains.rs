//! Workspace domain inference — the `stella init` command.
//!
//! A **domain** is a semantic area of the workspace ("auth", "billing",
//! "cli", "ingestion") with the path prefixes that belong to it. Domains
//! are the tagging vocabulary for the whole context plane: memories
//! (including post-turn reflection lessons), code-graph nodes/edges, and
//! context facts all carry one or more domain tags, and recall uses domain
//! overlap as a relevance signal — so a lesson learned while touching
//! `stella-model` surfaces again when a future turn works in that
//! area.
//!
//! Inference is model-assisted with a deterministic fallback: `stella init`
//! summarizes the repo's shape (top-level structure + README head + key
//! manifests), asks the worker model for a domain taxonomy as structured
//! JSON (one bounded repair attempt on parse failure), and falls back to a
//! directory-name heuristic when no provider is configured or the call
//! fails — `init` always succeeds, offline included. Output is data on
//! disk (`.stella/domains.toml`), never code.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use stella_protocol::{CompletionMessage, CompletionRequest, Provider};

/// One inferred domain: a name, a one-line description, and the path
/// prefixes (workspace-relative, `/`-separated) that belong to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Domain {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

/// The `.stella/domains.toml` document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Domains {
    /// Format version — additive evolution only.
    #[serde(default = "default_version")]
    pub version: u32,
    /// How this taxonomy was produced: `"model"` or `"heuristic"`.
    #[serde(default)]
    pub inferred_by: String,
    #[serde(default, rename = "domain")]
    pub domains: Vec<Domain>,
}

fn default_version() -> u32 {
    1
}

impl Domains {
    pub fn path_for(workspace_root: &Path) -> PathBuf {
        workspace_root.join(".stella").join("domains.toml")
    }

    /// Load the workspace's domains, if `stella init` has run. `None` when
    /// the file is absent (callers treat "no domains yet" as an empty tag
    /// vocabulary, never an error). Consumed by `SessionMemory` (memory.rs)
    /// to scope reflection tagging and recall to the workspace's domains.
    pub fn load(workspace_root: &Path) -> Result<Option<Self>, String> {
        let path = Self::path_for(workspace_root);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map(Some)
                .map_err(|e| format!("{} is malformed: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, workspace_root: &Path) -> Result<PathBuf, String> {
        let path = Self::path_for(workspace_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// The bare domain-name vocabulary (what reflection tagging and recall
    /// filters consume). Consumed by `SessionMemory` (memory.rs) to scope
    /// `recall_scoped` and per-turn reflection to the workspace's domains.
    pub fn names(&self) -> Vec<String> {
        self.domains.iter().map(|d| d.name.clone()).collect()
    }

    /// Resolve the domains a workspace-relative path belongs to, by prefix
    /// match. A path matching nothing gets an empty set — untagged is
    /// valid, not an error. Consumed by memory write-back to tag episodes
    /// with their domain context.
    pub fn domains_for_path(&self, rel_path: &str) -> Vec<String> {
        let normalized = rel_path.trim_start_matches("./");
        self.domains
            .iter()
            .filter(|d| {
                d.paths.iter().any(|prefix| {
                    let prefix = prefix.trim_end_matches('/');
                    normalized == prefix || normalized.starts_with(&format!("{prefix}/"))
                })
            })
            .map(|d| d.name.clone())
            .collect()
    }
}

/// Build the repo-shape summary the inference prompt sees: top-level (and
/// one nested level of) directories, README head, and which manifest files
/// exist. Deliberately shallow and bounded — this is a prompt, not an
/// index.
pub fn summarize_repo(root: &Path) -> String {
    let mut lines = Vec::new();

    let mut dirs: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            if entry.path().is_dir() {
                dirs.push(name.clone());
                if let Ok(nested) = std::fs::read_dir(entry.path()) {
                    for sub in nested.flatten().take(20) {
                        if sub.path().is_dir() {
                            let sub_name = sub.file_name().to_string_lossy().to_string();
                            if !sub_name.starts_with('.') {
                                dirs.push(format!("{name}/{sub_name}"));
                            }
                        }
                    }
                }
            }
        }
    }
    dirs.sort();
    lines.push(format!("Directories:\n{}", dirs.join("\n")));

    for manifest in [
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
    ] {
        if root.join(manifest).exists() {
            lines.push(format!("Has manifest: {manifest}"));
        }
    }

    for readme in ["README.md", "readme.md", "README"] {
        if let Ok(text) = std::fs::read_to_string(root.join(readme)) {
            let head: String = text.chars().take(1500).collect();
            lines.push(format!("README head:\n{head}"));
            break;
        }
    }

    lines.join("\n\n")
}

/// Infer domains with the worker model; one bounded repair attempt on
/// unparseable output; heuristic fallback on any failure.
pub async fn infer_domains(provider: &dyn Provider, root: &Path) -> Domains {
    let summary = summarize_repo(root);
    let prompt = format!(
        "Analyze this repository's shape and infer its semantic DOMAINS — the 4-10 major \
         functional areas of the codebase (examples from other projects: auth, billing, \
         ingestion, cli, knowledge-graph, api, ui). For each domain give: name (short \
         kebab-case), description (one line), paths (the workspace-relative directory \
         prefixes that belong to it — only prefixes that actually appear in the listing \
         below).\n\nRespond with ONLY a JSON array, no prose:\n\
         [{{\"name\": \"...\", \"description\": \"...\", \"paths\": [\"...\"]}}]\n\n{summary}"
    );

    let mut messages = vec![
        CompletionMessage::system(
            "You infer domain taxonomies from repository structure. Respond with only valid JSON.",
        ),
        CompletionMessage::user(&prompt),
    ];

    for _attempt in 0..2 {
        let req = CompletionRequest {
            messages: messages.clone(),
            max_output_tokens: Some(2048),
            temperature: Some(0.0),
            effort: None,
            tools: vec![],
            reasoning: None,
            params: None,
        };
        match provider.complete(req).await {
            Ok(result) => match parse_domains_json(&result.text) {
                Ok(domains) if !domains.is_empty() => {
                    return Domains {
                        version: 1,
                        inferred_by: "model".into(),
                        domains,
                    };
                }
                Ok(_) | Err(_) => {
                    // Bounded repair: feed the failure back once.
                    messages.push(CompletionMessage {
                        role: stella_protocol::MessageRole::Assistant,
                        content: result.text.clone(),
                        tool_calls: vec![],
                        tool_results: vec![],
                        attachments: Vec::new(),
                    });
                    messages.push(CompletionMessage::user(
                        "That was not a valid non-empty JSON array of domains. Respond with \
                         ONLY the JSON array.",
                    ));
                }
            },
            Err(_) => break, // provider trouble → heuristic, don't hammer
        }
    }

    heuristic_domains(root)
}

/// Extract and parse the first JSON array in `text` (models love prose and
/// code fences; tolerate both).
fn parse_domains_json(text: &str) -> Result<Vec<Domain>, String> {
    let start = text.find('[').ok_or("no JSON array found")?;
    let end = text.rfind(']').ok_or("unterminated JSON array")?;
    if end <= start {
        return Err("malformed JSON array".into());
    }
    serde_json::from_str::<Vec<Domain>>(&text[start..=end]).map_err(|e| e.to_string())
}

/// Offline fallback: each meaningful top-level directory becomes a domain.
/// Crude but deterministic — and honestly labeled `inferred_by =
/// "heuristic"` so a later `stella init` with a key configured can upgrade
/// it.
pub fn heuristic_domains(root: &Path) -> Domains {
    let mut domains = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| {
                !n.starts_with('.')
                    && ![
                        "node_modules",
                        "target",
                        "dist",
                        "build",
                        "out",
                        "vendor",
                        "coverage",
                        "tmp",
                    ]
                    .contains(&n.as_str())
            })
            .collect();
        names.sort();
        for name in names {
            domains.push(Domain {
                name: name.to_lowercase().replace(['_', ' '], "-"),
                description: format!("code under {name}/"),
                paths: vec![name],
            });
        }
    }
    Domains {
        version: 1,
        inferred_by: "heuristic".into(),
        domains,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("stella-domains-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn save_load_round_trips() {
        let root = temp_root("roundtrip");
        let domains = Domains {
            version: 1,
            inferred_by: "model".into(),
            domains: vec![Domain {
                name: "llm".into(),
                description: "provider adapters".into(),
                paths: vec!["crates/stella-model".into()],
            }],
        };
        domains.save(&root).expect("save");
        let loaded = Domains::load(&root).expect("load").expect("present");
        assert_eq!(loaded, domains);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_absent_is_none_not_an_error() {
        let root = temp_root("absent");
        assert!(Domains::load(&root).expect("ok").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn domains_for_path_prefix_matches_and_unmatched_is_empty() {
        let domains = Domains {
            version: 1,
            inferred_by: "model".into(),
            domains: vec![
                Domain {
                    name: "llm".into(),
                    description: String::new(),
                    paths: vec!["crates/stella-model".into()],
                },
                Domain {
                    name: "cli".into(),
                    description: String::new(),
                    paths: vec!["crates/stella-cli".into(), "crates/stella-tui".into()],
                },
            ],
        };
        assert_eq!(
            domains.domains_for_path("crates/stella-model/src/zai.rs"),
            vec!["llm".to_string()]
        );
        assert_eq!(
            domains.domains_for_path("crates/stella-tui/src/lib.rs"),
            vec!["cli".to_string()]
        );
        // Prefix must be segment-aligned: stella-model-extras is NOT under
        // stella-model.
        assert!(
            domains
                .domains_for_path("crates/stella-model-extras/src/lib.rs")
                .is_empty()
        );
        assert!(domains.domains_for_path("docs/README.md").is_empty());
    }

    #[test]
    fn parse_tolerates_prose_and_code_fences() {
        let text = "Here you go:\n```json\n[{\"name\": \"api\", \"description\": \"routes\", \
                    \"paths\": [\"src/api\"]}]\n```\nHope that helps!";
        let parsed = parse_domains_json(text).expect("parses");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "api");
    }

    #[test]
    fn heuristic_fallback_derives_domains_from_directories() {
        let root = temp_root("heuristic");
        std::fs::create_dir_all(root.join("api")).expect("mkdir");
        std::fs::create_dir_all(root.join("web_app")).expect("mkdir");
        std::fs::create_dir_all(root.join("node_modules")).expect("mkdir");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir");

        let domains = heuristic_domains(&root);
        let names = domains.names();
        assert!(names.contains(&"api".to_string()));
        assert!(names.contains(&"web-app".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("node_modules")));
        assert_eq!(domains.inferred_by, "heuristic");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn summarize_repo_is_bounded_and_names_structure() {
        let root = temp_root("summary");
        std::fs::create_dir_all(root.join("src/routes")).expect("mkdir");
        std::fs::write(root.join("Cargo.toml"), "[package]").expect("write");
        std::fs::write(root.join("README.md"), "# My project\nDoes things.").expect("write");
        let summary = summarize_repo(&root);
        assert!(summary.contains("src/routes"));
        assert!(summary.contains("Has manifest: Cargo.toml"));
        assert!(summary.contains("My project"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
