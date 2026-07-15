//! The self-improvement loop (user requirement): after every chat turn the
//! agent reflects on its own performance and records improvement memories;
//! before every turn, relevant memories and skills are recalled into
//! context; and when a lesson recurs enough times it is automatically
//! promoted to a durable skill (`.stella/skills/<slug>/SKILL.md`).
//!
//! Data flow per turn:
//!
//! ```text
//! prompt ──> recall_block(): store.recall_scoped(domains) + select_skills()
//!            └─ volatile message AFTER the byte-stable system prefix (L-E8)
//! turn runs …
//! outcome ─> reflect_and_record(): one cheap model call -> 0-3 lessons
//!            ├─ MemoryInput::reflection(...) -> context.db (domain-tagged)
//!            ├─ appended to .stella/reflections.jsonl (the mining log)
//!            └─ mine_skill_candidates over the log -> decide_auto_creation
//!               -> new SKILL.md files (capped per session, no-clobber)
//! ```
//!
//! Everything here is best-effort by contract: a failed reflection, a
//! malformed store, or a broken skills dir must NEVER fail or slow the
//! user's actual turn — degraded means "no memory this turn", not an error.

use std::path::{Path, PathBuf};

use colored::Colorize;
use serde::{Deserialize, Serialize};
use stella_context::{
    ContextDelta, ContextQuery, ContextStore, DomainInput, EpisodeInput, EpisodeOutcome,
    FactAssertion, HashEmbedder, MemoryInput, NodeInput, NodeKind, SystemClock, format_rfc3339,
};
use stella_core::skills::{
    self, AutoCreateConfig, AutoCreateDecision, LoadSkillsOptions, SelectionConfig, Skill,
    SkillMineConfig, SkillObservation, SkillSource,
};
use stella_pipeline::{ContextRecallPort, RecalledFrame};
use stella_protocol::{CompletionMessage, CompletionRequest, MessageRole, Provider};

use crate::domains::Domains;

/// Marker prefixing the volatile recalled-context message so it can be
/// found and refreshed in place each turn (index 1, right after the
/// byte-stable system prompt — L-E8: recalled frames ride as a volatile
/// message after the stable prefix, preserving prompt-cache hits).
pub const RECALL_MARKER: &str = "[auto-recalled context]";

/// One reflection lesson as the model returns it and as persisted to the
/// mining log (`.stella/reflections.jsonl`, one JSON object per line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflectionLesson {
    pub lesson: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub occurred_at: u64,
}

/// Session-scoped memory state: the context store, the OCP host that
/// routes every recall (workspace memory + code graph as in-process OCP
/// providers — see `crate::ocp`), the domain taxonomy, and the skills
/// auto-creation accounting.
pub struct SessionMemory {
    store: std::sync::Arc<ContextStore>,
    host: ocp_host::Host,
    domains: Domains,
    workspace_root: PathBuf,
    skills_created: usize,
}

/// Filesystem-backed [`SkillSource`] reading the workspace + user-global
/// skill directories.
struct FsSkillSource;

impl SkillSource for FsSkillSource {
    fn read_skill_files(&self, roots: &[String]) -> Vec<skills::SkillFile> {
        let mut files = Vec::new();
        for root in roots {
            // Flat layout: <root>/<slug>.md
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "md") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            files.push(skills::SkillFile {
                                path: path.display().to_string(),
                                content,
                            });
                        }
                    } else if path.is_dir() {
                        // Ecosystem layout: <root>/<slug>/SKILL.md
                        let nested = path.join("SKILL.md");
                        if let Ok(content) = std::fs::read_to_string(&nested) {
                            files.push(skills::SkillFile {
                                path: nested.display().to_string(),
                                content,
                            });
                        }
                    }
                }
            }
        }
        files
    }
}

impl SessionMemory {
    /// Open the workspace's memory. `None` (with a one-line warning) when
    /// the store can't open — a session without memory beats no session.
    pub fn open(workspace_root: &Path, warn: bool) -> Option<Self> {
        let db_path = workspace_root.join(".stella").join("context.db");
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match ContextStore::open_and_warm(
            &db_path,
            std::sync::Arc::new(HashEmbedder::default()),
            std::sync::Arc::new(SystemClock),
        ) {
            Ok(store) => {
                let domains = Domains::load(workspace_root)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let store = std::sync::Arc::new(store);
                let host = crate::ocp::session_host(
                    store.clone(),
                    domains.names(),
                    workspace_root.to_path_buf(),
                );
                Some(Self {
                    store,
                    host,
                    domains,
                    workspace_root: workspace_root.to_path_buf(),
                    skills_created: 0,
                })
            }
            Err(e) => {
                if warn {
                    eprintln!("  {} memory disabled this session: {e}", "!".yellow());
                }
                None
            }
        }
    }

    fn workspace_skills_dir(&self) -> String {
        self.workspace_root
            .join(".stella")
            .join("skills")
            .display()
            .to_string()
    }

    fn user_skills_dir(&self) -> String {
        std::env::var_os("HOME")
            .map(|home| {
                PathBuf::from(home)
                    .join(".config")
                    .join("stella")
                    .join("skills")
                    .display()
                    .to_string()
            })
            .unwrap_or_default()
    }

    /// Load the workspace's skills fresh (cheap — a handful of file reads;
    /// fresh so a just-installed or just-auto-created skill is live on the
    /// very next turn).
    pub fn load_skills(&self) -> Vec<Skill> {
        skills::load_skills(
            &FsSkillSource,
            &LoadSkillsOptions {
                workspace_skills_dir: self.workspace_skills_dir(),
                user_skills_dir: self.user_skills_dir(),
            },
        )
    }

    /// Build the volatile recalled-context block for a prompt: relevant
    /// memories (similarity + domain overlap + recency via the context
    /// store) and relevant skills (lexical + domain selection). `None` when
    /// nothing relevant surfaced — an empty block would only burn cache.
    pub async fn recall_block(&self, prompt: &str) -> Option<String> {
        let mut sections: Vec<String> = Vec::new();

        let query = ContextQuery {
            goal: prompt.to_string(),
            query_text: Some(prompt.to_string()),
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames: 5,
            max_tokens: 1200,
            as_of: None,
        };
        // Routed through the session's OCP host: workspace memory + the
        // code graph answer concurrently, isolated and budget-audited.
        let frames = crate::ocp::recall_via_host(&self.host, &query).await;
        let lines: Vec<String> = frames
            .iter()
            .filter_map(|f| {
                f.citation_label
                    .as_deref()
                    .map(|label| format!("- {} — {}", label, f.content.trim()))
            })
            .collect();
        if !lines.is_empty() {
            sections.push(format!("Relevant context:\n{}", lines.join("\n")));
        }

        let all_skills = self.load_skills();
        let selected = skills::select_skills(
            &all_skills,
            prompt,
            &self.domains.names(),
            &SelectionConfig::default(),
        );
        if !selected.is_empty() {
            sections.push(skills::render_skills_section(&selected));
        }

        if sections.is_empty() {
            None
        } else {
            Some(format!("{RECALL_MARKER}\n\n{}", sections.join("\n\n")))
        }
    }

    /// Record the turn that just finished as an episodic memory: a summary,
    /// the files it touched, and how it ended. Episodes become retrievable
    /// `Episode` nodes, so future recall can surface "we did something like
    /// this before" alongside reflections — the episodic half of the context
    /// plane (`stella-context` L-C3 neighborhood). Domain tags come from the
    /// touched files' taxonomy prefixes. Best-effort like everything here: a
    /// failed write must never fail the turn it describes.
    pub async fn record_episode(
        &self,
        prompt: &str,
        outcome: EpisodeOutcome,
        files_touched: &[(String, String)],
        started_unix_secs: i64,
    ) {
        let mut summary: String = prompt.chars().take(240).collect();
        if prompt.chars().count() > 240 {
            summary.push('…');
        }

        let mut domains: Vec<String> = Vec::new();
        for (path, _ops) in files_touched {
            for name in self.domains.domains_for_path(path) {
                if !domains.contains(&name) {
                    domains.push(name);
                }
            }
        }

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(started_unix_secs);
        let mut episode = EpisodeInput::new(
            summary,
            format_rfc3339(started_unix_secs),
            format_rfc3339(now_secs),
        )
        .with_domains(domains);
        episode.outcome = outcome;
        episode.files_touched = files_touched.iter().map(|(path, _)| path.clone()).collect();

        let delta = ContextDelta {
            episodes: vec![episode],
            ..Default::default()
        };
        let _ = self.store.upsert(delta).await;
    }

    /// Persist the `stella init` taxonomy into the context plane: each domain
    /// as a described domain record, and each of its path prefixes as a
    /// bi-temporal `covers_path` fact. Re-running `init` after the taxonomy
    /// shifts supersedes stale beliefs instead of deleting them, so
    /// "what did we believe at T1" still answers (L-C3).
    ///
    /// Known limitation: `covers_path` *facts* are versioned (a moved path's
    /// old fact is superseded), but the File node's `node_domains` tags are
    /// insert-only — re-running `init` after a path moves from domain A to B
    /// adds the B tag without removing A. This does NOT break recall
    /// correctness: the session scopes recall to the *full current taxonomy*,
    /// so the node still passes the scope filter via B; the only residual is a
    /// mild domain-overlap ranking boost for A. A correct fix is versioned
    /// node-domain associations (matching the fact model) — deliberately not a
    /// wholesale `node_domains` rewrite, which would depend on the unenforced
    /// invariant that only the taxonomy ever tags File nodes and would silently
    /// wipe tags from any future source.
    pub async fn record_taxonomy(&self, taxonomy: &crate::domains::Domains) {
        let domains = taxonomy
            .domains
            .iter()
            .map(|d| DomainInput {
                name: d.name.clone(),
                description: (!d.description.is_empty()).then(|| d.description.clone()),
            })
            .collect();
        let facts = taxonomy
            .domains
            .iter()
            .flat_map(|d| {
                d.paths.iter().map(|path| {
                    // Tag the nodes themselves, not just the edge — node-level
                    // tags are what `recall_scoped`'s domain filter and
                    // overlap boost read (`node_domains` rows come from the
                    // subject/object inputs, never from the fact's own tags).
                    let subject = NodeInput::new(NodeKind::Concept, &d.name)
                        .with_uri(format!("domain://{}", d.name))
                        .with_domains([d.name.clone()]);
                    let object = NodeInput::new(NodeKind::File, path)
                        .with_uri(format!("file://{path}"))
                        .with_domains([d.name.clone()]);
                    let mut fact = FactAssertion::new(subject, "covers_path", object)
                        .with_domains([d.name.clone()]);
                    // A domain legitimately covers several paths at once.
                    fact.multivalued = true;
                    fact
                })
            })
            .collect();
        let delta = ContextDelta {
            domains,
            facts,
            ..Default::default()
        };
        let _ = self.store.upsert(delta).await;
    }

    /// Post-turn self-reflection: one cheap model call producing 0-3
    /// durable lessons, stored as domain-tagged reflection memories AND
    /// appended to the skill-mining log; recurring lessons auto-promote to
    /// SKILL.md files. Best-effort throughout — returns how many lessons
    /// were recorded, and any failure degrades to 0 silently (a failed
    /// reflection must never fail the turn that just succeeded).
    pub async fn reflect_and_record(
        &mut self,
        provider: &dyn Provider,
        transcript: &[CompletionMessage],
        quiet: bool,
    ) -> usize {
        let lessons = reflect_on_turn(provider, transcript, &self.domains.names()).await;
        if lessons.is_empty() {
            return 0;
        }

        // 1. Store as recallable, domain-tagged reflection memories. Still
        // best-effort (a failed reflection never fails the turn), but the
        // outcome is kept so the "remembered" line below can't claim success
        // for lessons that never landed in the store.
        let delta = ContextDelta {
            memories: lessons
                .iter()
                .map(|l| MemoryInput::reflection(&l.lesson, l.domains.iter().cloned()))
                .collect(),
            ..Default::default()
        };
        let stored = self.store.upsert(delta).await.is_ok();

        // 2. Append to the mining log and mine for auto-creatable skills.
        // Track whether the log write actually succeeded so the message below
        // never claims a fallback that didn't happen.
        let log_path = self
            .workspace_root
            .join(".stella")
            .join("reflections.jsonl");
        let mut logged = true;
        for lesson in &lessons {
            match serde_json::to_string(lesson) {
                Ok(line) if append_line(&log_path, &line).is_ok() => {}
                _ => logged = false,
            }
        }
        self.auto_create_skills(&log_path, quiet);

        if !quiet {
            if stored {
                println!(
                    "  {} remembered {} lesson(s) from this turn",
                    "✦".magenta(),
                    lessons.len()
                );
            } else if logged {
                println!(
                    "  {} could not persist {} lesson(s) to the context store \
                     (logged to reflections.jsonl only)",
                    "!".yellow(),
                    lessons.len()
                );
            } else {
                println!(
                    "  {} could not persist {} lesson(s) — both the context store \
                     and reflections.jsonl writes failed",
                    "!".yellow(),
                    lessons.len()
                );
            }
        }
        if stored { lessons.len() } else { 0 }
    }

    /// Mine the whole reflection log for recurring lessons and auto-create
    /// skills for any that qualify (threshold + session cap + no-clobber
    /// enforced by `stella_core::skills`).
    fn auto_create_skills(&mut self, log_path: &Path, quiet: bool) {
        let Ok(log) = std::fs::read_to_string(log_path) else {
            return;
        };
        let observations: Vec<SkillObservation> = log
            .lines()
            .filter_map(|line| serde_json::from_str::<ReflectionLesson>(line).ok())
            .map(|l| SkillObservation {
                reference: format!("reflection:{}", l.occurred_at),
                text: l.lesson,
                domains: l.domains,
                occurred_at: l.occurred_at,
                salient: false,
            })
            .collect();
        if observations.is_empty() {
            return;
        }

        let existing = self.load_skills();
        let candidates =
            skills::mine_skill_candidates(observations, &existing, &SkillMineConfig::default());

        let skills_dir = self.workspace_skills_dir();
        let existing_paths: Vec<String> = existing.iter().map(|s| s.source_path.clone()).collect();
        let config = AutoCreateConfig::default();
        for candidate in candidates {
            match skills::decide_auto_creation(
                &candidate,
                &skills_dir,
                &existing_paths,
                self.skills_created,
                &config,
            ) {
                AutoCreateDecision::Create { path } => {
                    let markdown = skills::render_skill_markdown(&candidate);
                    let path = PathBuf::from(path);
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::write(&path, markdown).is_ok() {
                        self.skills_created += 1;
                        if !quiet {
                            println!(
                                "  {} new skill auto-created from recurring observations: {} ({})",
                                "✦".magenta().bold(),
                                candidate.name.bright_blue(),
                                path.display()
                            );
                        }
                    }
                }
                AutoCreateDecision::Skip { .. } => {}
            }
        }
    }
}

/// The pipeline's context-recall port over the workspace memory store: the
/// split-context planner (L-E6) receives the same durable lessons the
/// worker's injected recall block carries, as structured frames instead of a
/// rendered string. Frames without a citation label are dropped (L-C4), and
/// a failed recall degrades to no frames, never an error (L-C6).
#[async_trait::async_trait]
impl ContextRecallPort for SessionMemory {
    async fn recall(&self, goal: &str) -> Vec<RecalledFrame> {
        let query = ContextQuery {
            goal: goal.to_string(),
            query_text: Some(goal.to_string()),
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames: 5,
            max_tokens: 1200,
            as_of: None,
        };
        crate::ocp::recall_via_host(&self.host, &query)
            .await
            .into_iter()
            .filter_map(|f| {
                let citation_label = f.citation_label.clone()?;
                Some(RecalledFrame {
                    citation_label,
                    source: "memory".to_string(),
                    content: f.content.trim().to_string(),
                    token_cost: f.token_cost,
                    id: Some(f.id),
                })
            })
            .collect()
    }
}

/// Refresh (or insert) the volatile recalled-context message at index 1 —
/// immediately after the byte-stable system prompt, before all history
/// (L-E8). Replacing in place keeps exactly one recall block per
/// conversation no matter how many turns run.
pub fn inject_recall_block(messages: &mut Vec<CompletionMessage>, block: Option<String>) {
    let is_marker =
        |m: &CompletionMessage| m.role == MessageRole::User && m.content.starts_with(RECALL_MARKER);
    match block {
        Some(content) => {
            let message = CompletionMessage {
                role: MessageRole::User,
                content,
                tool_calls: vec![],
                tool_results: vec![],
            };
            if messages.len() > 1 && is_marker(&messages[1]) {
                messages[1] = message;
            } else {
                messages.insert(1.min(messages.len()), message);
            }
        }
        None => {
            if messages.len() > 1 && is_marker(&messages[1]) {
                messages.remove(1);
            }
        }
    }
}

/// Seconds since the Unix epoch — the episode timestamps' primitive.
pub(crate) fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether a completed turn is even worth a post-turn reflection model call.
///
/// Reflection ([`SessionMemory::reflect_and_record`]) mines lessons from WORK
/// — tool calls, file edits, multi-step problem solving. A turn that invoked
/// no tools produced no observable agent behavior to critique, and the
/// reflection prompt itself notes that "most turns have nothing worth
/// recording." Gating on tool use deterministically skips a model call (and
/// its latency and dollars) that would almost always return `[]`: the biggest
/// per-turn saving available, and the skipped turns are exactly the trivial
/// ones (greetings, quick questions, refusals). The trade is that a durable
/// preference revealed in pure conversation, with no tool call, is not
/// mined — an intentional bias toward determinism and cost over a rare,
/// speculative capture.
///
/// `turn_messages` must be ONLY the messages added during the turn being
/// judged (in the accumulating REPL transcript, the slice past the pre-turn
/// length) — otherwise a tool call from an earlier turn would keep
/// re-triggering reflection on every later tool-free turn.
pub fn turn_warrants_reflection(turn_messages: &[CompletionMessage]) -> bool {
    turn_messages.iter().any(|m| !m.tool_calls.is_empty())
}

/// One cheap reflection call (triage-tier discipline: single attempt, any
/// failure -> empty). The model critiques the completed turn and returns
/// 0-3 short forward-looking lessons tagged with domains FROM THE SUPPLIED
/// LIST only — invented domain names are dropped.
pub async fn reflect_on_turn(
    provider: &dyn Provider,
    transcript: &[CompletionMessage],
    domain_names: &[String],
) -> Vec<ReflectionLesson> {
    // Bounded transcript digest: last 12 messages, 300 chars each.
    let digest: String = transcript
        .iter()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|m| {
            let role = match m.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };
            let content: String = m.content.chars().take(300).collect();
            let tools = if m.tool_calls.is_empty() {
                String::new()
            } else {
                format!(
                    " [called: {}]",
                    m.tool_calls
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            format!("{role}: {content}{tools}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "Review this coding-agent turn transcript and reflect on the agent's performance. \
         Identify durable, forward-looking lessons that would improve FUTURE turns in this \
         workspace: wasted tool calls, wrong assumptions, user style preferences revealed, \
         repo conventions discovered. Most turns have nothing worth recording — an empty \
         list is the common, correct answer. Respond with ONLY a JSON array (max 3):\n\
         [{{\"lesson\": \"...\", \"domains\": [\"...\"]}}]\n\
         Allowed domain tags (use only these, or []): {}\n\nTranscript:\n{digest}",
        domain_names.join(", ")
    );

    let req = CompletionRequest {
        messages: vec![
            CompletionMessage::system(
                "You are a self-reflection module. Respond with only a JSON array.",
            ),
            CompletionMessage::user(&prompt),
        ],
        max_output_tokens: Some(512),
        temperature: Some(0.0),
        effort: None,
        tools: vec![],
    };

    let Ok(result) = provider.complete(req).await else {
        return Vec::new();
    };
    parse_lessons(&result.text, domain_names)
}

/// Extract the first JSON array from model output; drop invented domains;
/// cap at 3; stamp `occurred_at` with the current unix time.
pub fn parse_lessons(text: &str, allowed_domains: &[String]) -> Vec<ReflectionLesson> {
    let Some(start) = text.find('[') else {
        return Vec::new();
    };
    let Some(end) = text.rfind(']') else {
        return Vec::new();
    };
    if end <= start {
        return Vec::new();
    }
    let Ok(mut lessons) = serde_json::from_str::<Vec<ReflectionLesson>>(&text[start..=end]) else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    lessons.truncate(3);
    for lesson in &mut lessons {
        lesson.occurred_at = now;
        lesson
            .domains
            .retain(|d| allowed_domains.iter().any(|a| a.eq_ignore_ascii_case(d)));
    }
    lessons.retain(|l| !l.lesson.trim().is_empty());
    lessons
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: MessageRole, content: &str) -> CompletionMessage {
        CompletionMessage {
            role,
            content: content.into(),
            tool_calls: vec![],
            tool_results: vec![],
        }
    }

    #[test]
    fn inject_inserts_the_block_at_index_one_after_the_system_prefix() {
        let mut messages = vec![
            msg(MessageRole::System, "sys"),
            msg(MessageRole::User, "do the thing"),
        ];
        inject_recall_block(&mut messages, Some(format!("{RECALL_MARKER}\nstuff")));
        assert_eq!(messages.len(), 3);
        assert!(messages[1].content.starts_with(RECALL_MARKER));
        assert_eq!(messages[0].content, "sys", "stable prefix untouched (L-E8)");
        assert_eq!(messages[2].content, "do the thing");
    }

    #[test]
    fn inject_replaces_in_place_on_later_turns_never_accumulates() {
        let mut messages = vec![msg(MessageRole::System, "sys")];
        inject_recall_block(&mut messages, Some(format!("{RECALL_MARKER}\nfirst")));
        messages.push(msg(MessageRole::User, "turn 1"));
        inject_recall_block(&mut messages, Some(format!("{RECALL_MARKER}\nsecond")));
        let markers = messages
            .iter()
            .filter(|m| m.content.starts_with(RECALL_MARKER))
            .count();
        assert_eq!(markers, 1, "exactly one recall block, refreshed in place");
        assert!(messages[1].content.contains("second"));
    }

    #[test]
    fn inject_none_removes_a_stale_block() {
        let mut messages = vec![msg(MessageRole::System, "sys")];
        inject_recall_block(&mut messages, Some(format!("{RECALL_MARKER}\nstuff")));
        inject_recall_block(&mut messages, None);
        assert_eq!(messages.len(), 1, "nothing relevant -> no block at all");
    }

    #[test]
    fn parse_lessons_drops_invented_domains_and_caps_at_three() {
        let allowed = vec!["api".to_string(), "cli".to_string()];
        let text = r#"Sure! [
            {"lesson": "prefer tables", "domains": ["cli", "made-up"]},
            {"lesson": "b", "domains": []},
            {"lesson": "c", "domains": ["API"]},
            {"lesson": "d", "domains": []}
        ]"#;
        let lessons = parse_lessons(text, &allowed);
        assert_eq!(lessons.len(), 3, "capped at 3");
        assert_eq!(lessons[0].domains, vec!["cli"], "invented domain dropped");
        assert_eq!(
            lessons[2].domains,
            vec!["API"],
            "case-insensitive match kept"
        );
        assert!(lessons[0].occurred_at > 0);
    }

    #[test]
    fn parse_lessons_tolerates_garbage_and_empty_output() {
        assert!(parse_lessons("no json here", &[]).is_empty());
        assert!(parse_lessons("[]", &[]).is_empty());
        assert!(parse_lessons("[{\"lesson\": \"   \"}]", &[]).is_empty());
    }

    #[test]
    fn reflection_gate_fires_on_tool_use_and_skips_tool_free_turns() {
        use stella_protocol::ToolCall;

        // A pure conversational turn — no tool calls — is not worth a
        // reflection model call (the common, cheap-to-skip case).
        let chat_only = vec![
            msg(MessageRole::User, "what does this crate do?"),
            msg(MessageRole::Assistant, "it is a terminal coding agent"),
        ];
        assert!(!turn_warrants_reflection(&chat_only));

        // A turn where the assistant called a tool DID work worth mining.
        let mut worked = msg(MessageRole::Assistant, "reading the file first");
        worked.tool_calls = vec![ToolCall {
            call_id: "c1".into(),
            name: "read_file".into(),
            input: serde_json::json!({ "path": "src/main.rs" }),
        }];
        assert!(turn_warrants_reflection(&[worked]));

        // An empty turn slice (nothing happened) is trivially skippable.
        assert!(!turn_warrants_reflection(&[]));
    }
}
