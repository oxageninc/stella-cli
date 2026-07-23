//! The self-improvement loop (user requirement): after every turn that did
//! real work — chat, `run`, `goal`, and the Command Deck alike, on success
//! AND on failure — the agent reflects on its own performance and records
//! improvement memories; before every turn, relevant memories and skills are
//! recalled into context; and when a lesson recurs enough times it is
//! automatically promoted to a durable skill (`.stella/skills/<slug>/SKILL.md`).
//! A failed turn is the highest-value learning signal, so it gets a
//! root-cause "why did this fail" reflection prompt (see [`reflect_on_turn`]).
//!
//! Data flow per turn:
//!
//! ```text
//! prompt ──> recall_block(): registry-routed recall (crate::contextgraph) + select_skills()
//!            └─ volatile message AFTER the byte-stable system prefix (L-E8)
//! turn runs …
//! outcome ─> reflect_and_record(): one cheap model call -> 0-3 lessons
//!            ├─ MemoryInput::reflection(...) -> context.db (domain-tagged)
//!            ├─ appended to .stella/private/reflections.jsonl (the mining log)
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
    self, AutoCreateConfig, AutoCreateDecision, SelectionConfig, Skill, SkillMineConfig,
    SkillObservation,
};
use stella_pipeline::{ContextRecallPort, RecalledFrame};
use stella_protocol::{CompletionMessage, MessageRole, Provider};

use crate::domains::Domains;

mod private_state;
mod projection;
#[cfg(test)]
mod quarantine_tests;
#[path = "memory/skills.rs"]
mod skill_files;
use private_state::resolve_context_db_path;
use projection::{is_quarantined_local_memory, project_recalled_frame};
#[cfg(test)]
pub(crate) use skill_files::load_workspace_skills;
pub(crate) use skill_files::{load_workspace_skills_with_authority, workspace_skills_dir};

/// Marker prefixing a recalled-context message so [`inject_recall_block`]
/// can find the newest one for dedup. Blocks land at the conversation
/// tail and stay in place as durable history (L-E8: the byte-stable
/// prefix — system prompt AND replayed turns — is never rewritten, which
/// is what preserves prompt-cache hits).
pub const RECALL_MARKER: &str = "[auto-recalled context]";

/// One reflection lesson as the model returns it and as persisted to the
/// mining log (`.stella/private/reflections.jsonl`, one JSON object per line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflectionLesson {
    pub lesson: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub occurred_at: u64,
}

mod reflection;
#[cfg(test)]
use reflection::parse_lessons;
pub use reflection::{
    ReflectionReport, reflect_on_turn, should_reflect_on, turn_warrants_reflection,
};

/// Session-scoped memory state: the context store, the CGP host that
/// routes every recall (workspace memory + code graph as in-process CGP
/// providers — see `crate::contextgraph`), the domain taxonomy, and the skills
/// auto-creation accounting.
pub struct SessionMemory {
    store: std::sync::Arc<ContextStore>,
    host: contextgraph_host::Host,
    domains: Domains,
    workspace_root: PathBuf,
    include_workspace_skills: bool,
    skills_created: usize,
    /// A/B recall control (Proposal 4): when true, recall is suppressed
    /// entirely on this turn so the outcome can be compared against recalled
    /// turns. Set by `maybe_suppress_recall()` from the turn counter below.
    ab_suppressed: bool,
    /// Count of turns that have consulted the A/B control, used to make
    /// every `rate`-th turn a deterministic control turn (see
    /// [`SessionMemory::maybe_suppress_recall`]).
    ab_turn: u32,
}

impl SessionMemory {
    /// Open the workspace's memory. `None` (with a one-line warning) when
    /// the store can't open — a session without memory beats no session.
    pub fn open(workspace_root: &Path, warn: bool) -> Option<Self> {
        Self::open_with_workspace_skills(workspace_root, warn, false)
    }

    /// Open memory with workspace skill injection governed by the session's
    /// immutable authority snapshot. Context recall itself remains evidence.
    pub fn open_with_authority(
        workspace_root: &Path,
        warn: bool,
        authority: &crate::settings::AuthorityPolicy,
    ) -> Option<Self> {
        Self::open_with_workspace_skills(workspace_root, warn, authority.project_prompts_allowed)
    }

    fn open_with_workspace_skills(
        workspace_root: &Path,
        warn: bool,
        include_workspace_skills: bool,
    ) -> Option<Self> {
        // Ephemeral benchmark trials must neither recall task/user-planted
        // learning state nor create or migrate a context database that can
        // perturb the task under test. Reflection is separately pinned off
        // by the launcher; this closes the pre-turn recall side of the same
        // boundary before the private-state resolver performs any I/O.
        if crate::settings::filesystem_settings_disabled() {
            return None;
        }
        let db_path = resolve_context_db_path(workspace_root, warn, |message| {
            eprintln!("  {} {message}", "!".yellow());
        })?;
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
                let host = crate::contextgraph::session_host(
                    store.clone(),
                    domains.names(),
                    workspace_root.to_path_buf(),
                );
                Some(Self {
                    store,
                    host,
                    domains,
                    workspace_root: workspace_root.to_path_buf(),
                    include_workspace_skills,
                    skills_created: 0,
                    ab_suppressed: false,
                    ab_turn: 0,
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
        workspace_skills_dir(&self.workspace_root)
    }

    /// Load the workspace's skills fresh (cheap — a handful of file reads;
    /// fresh so a just-installed or just-auto-created skill is live on the
    /// very next turn).
    pub fn load_skills(&self) -> Vec<Skill> {
        load_workspace_skills_with_authority(&self.workspace_root, self.include_workspace_skills)
            .skills
    }

    /// Build the volatile recalled-context block for a prompt: relevant
    /// memories (similarity + domain overlap + recency via the context
    /// store) and relevant skills (lexical + domain selection). `None` when
    /// nothing relevant surfaced — an empty block would only burn cache.
    ///
    /// **Quarantine filter (Proposal 3):** frames whose memory id (`nod_…`)
    /// appears in the session's `quarantined_ids` set are dropped before
    /// rendering. These are memories cited untruthful ≥ 2 times — surfacing
    /// them is active harm.
    ///
    /// **A/B control (Proposal 4):** when `ab_suppressed` is true (set by a
    /// deterministic coin flip), recall returns `None` so the turn runs
    /// without context — the outcome is then comparable to recalled turns.
    pub async fn recall_block(&self, prompt: &str) -> Option<String> {
        if self.ab_suppressed {
            return None;
        }
        let mut sections: Vec<String> = Vec::new();
        let frames = self.recalled_frames(prompt).await;
        if let Some(section) = render_context_section(&frames) {
            sections.push(section);
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

    /// A/B recall control (Proposal 4): suppress recall for this turn on a
    /// deterministic `1/rate` schedule, returning whether recall was
    /// suppressed. A rate of 0 (or 1) never suppresses. The caller records
    /// the outcome alongside this flag so `stella memory ab-report` can
    /// compare recalled vs control turns.
    ///
    /// Suppression is driven by a per-session **turn counter**, not a wall
    /// clock. A previous implementation seeded off `SystemTime` nanoseconds
    /// and tested `ns % rate == 0`; on any host whose realtime clock is
    /// coarser than nanoseconds (macOS keeps it in microseconds, so `ns` is
    /// always a multiple of 1000) that predicate is true on *every* turn for
    /// any `rate` dividing 1000 — silently disabling recall entirely. A plain
    /// counter makes exactly every `rate`-th turn a control turn, on every OS.
    pub fn maybe_suppress_recall(&mut self, rate: u32) -> bool {
        if rate == 0 || rate == 1 {
            self.ab_suppressed = false;
            return false;
        }
        self.ab_turn = self.ab_turn.wrapping_add(1);
        self.ab_suppressed = ab_control_turn(self.ab_turn, rate);
        self.ab_suppressed
    }

    /// Whether recall was suppressed this turn (for outcome attribution).
    pub fn recall_was_suppressed(&self) -> bool {
        self.ab_suppressed
    }
}

/// Is the `turn`-th turn (1-based) an A/B control turn at the given `rate`?
/// Every `rate`-th turn is a control turn; `rate` of 0 or 1 never controls.
/// Pure so the schedule is property-testable independent of the (heavy)
/// [`SessionMemory`] it lives on.
fn ab_control_turn(turn: u32, rate: u32) -> bool {
    rate > 1 && turn.is_multiple_of(rate)
}

impl SessionMemory {
    /// The skills recall would inject for `prompt`, as `(name, reason)` pairs
    /// for skill-version usage telemetry — `reason` is the matched
    /// domains/terms that selected it. Same enabled-filtered load + selection
    /// as [`Self::recall_block`], so this reports exactly what was applied.
    pub fn selected_skills(&self, prompt: &str) -> Vec<(String, String)> {
        skills::select_skills(
            &self.load_skills(),
            prompt,
            &self.domains.names(),
            &SelectionConfig::default(),
        )
        .into_iter()
        .map(|s| {
            let mut why: Vec<String> = Vec::new();
            if !s.matched_domains.is_empty() {
                why.push(format!("domains: {}", s.matched_domains.join(", ")));
            }
            if !s.matched_terms.is_empty() {
                why.push(format!("terms: {}", s.matched_terms.join(", ")));
            }
            (s.skill.name, why.join("; "))
        })
        .collect()
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
    /// Known limitation (deliberately deferred): `covers_path` *facts* are
    /// versioned (a moved path's old fact is superseded), but the File node's
    /// `node_domains` tags are insert-only — re-running `init` after a path
    /// moves from domain A to B adds the B tag without removing A. This does
    /// NOT break recall correctness: the session scopes recall to the *full
    /// current taxonomy*, so the node still passes the scope filter via B; the
    /// residual is only a domain-overlap ranking boost for A, and only while A
    /// itself remains a taxonomy domain.
    ///
    /// Two fixes were considered and both deferred:
    /// - Versioned node-domain associations (mirroring the fact model) — the
    ///   correct design, but a `stella-context` schema change (`node_domains`
    ///   gains validity columns, and every scope query must filter live rows).
    ///   Disproportionate to a ranking-edge, and higher-risk right after the
    ///   store's DuckDB→SQLite migration.
    /// - Retiring taxonomy-owned tags before re-adding (a `node_domains`
    ///   rewrite) — rejected as brittle: it relies on the unenforced invariant
    ///   that only the taxonomy ever tags File nodes, so it would silently wipe
    ///   a tag written by any future source.
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
    /// SKILL.md files. Best-effort throughout — a failed reflection must never
    /// fail the turn it describes. Returns a [`ReflectionReport`] so the caller
    /// can surface the outcome (a model-call error, or how many lessons landed)
    /// in whichever output format it speaks; the report distinguishes a genuine
    /// model-call failure from the common, correct "nothing worth recording."
    ///
    /// `succeeded` controls the reflection prompt template (Proposal 1):
    /// a failed turn gets a failure-analysis prompt that asks the model to
    /// identify the root cause — the highest-value learning signal in the
    /// system. A succeeded turn gets the conventional "what worked?" prompt.
    pub async fn reflect_and_record(
        &mut self,
        provider: &dyn Provider,
        model_hint: &str,
        transcript: &[CompletionMessage],
        quiet: bool,
        succeeded: bool,
        budget_limit: Option<f64>,
    ) -> ReflectionReport {
        let lessons = match reflect_on_turn(
            provider,
            model_hint,
            &self.workspace_root,
            transcript,
            &self.domains.names(),
            succeeded,
            budget_limit,
        )
        .await
        {
            Ok((lessons, cost_usd, events)) => (lessons, cost_usd, events),
            // The single reflection model call errored. Report it up so the
            // caller can warn (text) or emit an event (stream-json) — this
            // is the fix for the previously-silent reflection failure. Never
            // fatal: the turn already stands on its own.
            Err(model_error) => {
                return ReflectionReport {
                    recorded: 0,
                    model_error: Some(model_error.message),
                    cost_usd: model_error.cost_usd,
                    events: model_error.events,
                };
            }
        };
        let (lessons, reflection_cost_usd, reflection_events) = lessons;
        if lessons.is_empty() {
            return ReflectionReport {
                cost_usd: reflection_cost_usd,
                events: reflection_events,
                ..ReflectionReport::default()
            };
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
        // Count how many lessons actually reached the log so the message below
        // reports partial persistence accurately (some serialize/append writes
        // may fail while others succeed).
        let log_path =
            stella_store::workspace_private_state_path(&self.workspace_root, "reflections.jsonl")
                .ok();
        let mut logged_count = 0usize;
        for lesson in &lessons {
            if let Ok(line) = serde_json::to_string(lesson)
                && stella_store::append_workspace_private_line(
                    &self.workspace_root,
                    "reflections.jsonl",
                    &line,
                )
                .is_ok()
            {
                logged_count += 1;
            }
        }
        if let Some(log_path) = &log_path {
            self.auto_create_skills(log_path, quiet);
        }

        if !quiet {
            let n = lessons.len();
            if stored {
                println!(
                    "  {} remembered {n} lesson(s) from this turn",
                    "✦".magenta()
                );
            } else if logged_count == n {
                println!(
                    "  {} could not persist {n} lesson(s) to the context store \
                     (logged to reflections.jsonl only)",
                    "!".yellow()
                );
            } else if logged_count > 0 {
                println!(
                    "  {} could not persist {n} lesson(s) to the context store; \
                     {logged_count} of {n} reached reflections.jsonl",
                    "!".yellow()
                );
            } else {
                println!(
                    "  {} could not persist {n} lesson(s) — both the context store \
                     and reflections.jsonl writes failed",
                    "!".yellow()
                );
            }
        }
        ReflectionReport {
            recorded: if stored { lessons.len() } else { 0 },
            model_error: None,
            cost_usd: reflection_cost_usd,
            events: reflection_events,
        }
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
                                candidate.name.bright_magenta(),
                                path.display()
                            );
                        }
                    }
                }
                AutoCreateDecision::Skip { .. } => {}
            }
        }
    }

    /// Authoritative prompt and pipeline recall, including a fresh quarantine
    /// read so prior-turn feedback applies immediately.
    async fn recalled_frames(&self, goal: &str) -> Vec<RecalledFrame> {
        self.recalled_frames_reporting(goal, |message| eprintln!("  {} {message}", "!".yellow()))
            .await
    }
    /// Recall with an injectable diagnostic sink to avoid global stderr capture in tests.
    async fn recalled_frames_reporting(
        &self,
        goal: &str,
        mut report: impl FnMut(String),
    ) -> Vec<RecalledFrame> {
        if self.ab_suppressed {
            return Vec::new();
        }
        let query = ContextQuery {
            goal: goal.to_string(),
            query_text: Some(goal.to_string()),
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames: 5,
            max_tokens: 1200,
            as_of: None,
            representation_preferences: vec![],
        };
        let quarantined = match stella_store::Store::open(&self.workspace_root)
            .and_then(|store| store.quarantined_memory_ids())
        {
            Ok(ids) => ids,
            Err(error) => {
                report(format!(
                    "memory recall disabled: quarantine state unavailable: {error}"
                ));
                return Vec::new();
            }
        };
        crate::contextgraph::recall_via_host(&self.host, &query)
            .await
            .into_iter()
            .filter_map(project_recalled_frame)
            .filter(|frame| !is_quarantined_local_memory(frame, &quarantined))
            .collect()
    }
}

/// The pipeline's context-recall port over the workspace memory store: the
/// split-context planner (L-E6) receives the same durable lessons the
/// worker's injected recall block carries, as structured frames instead of a
/// rendered string. Frames without a citation label are dropped (L-C4), and
/// failed recall, including quarantine verification, degrades to no frames (L-C6).
#[async_trait::async_trait]
impl ContextRecallPort for SessionMemory {
    async fn recall(&self, goal: &str) -> Vec<RecalledFrame> {
        self.recalled_frames(goal).await
    }
}

/// Render recalled frames as the "Relevant context" section of the recall
/// block. Memory-kind frames carry their stable `[nod_…]` id inline — the
/// handle the `cite_memory` tool ties feedback to — and their presence
/// appends the citation instruction, so the model is asked to cite exactly
/// when there is something citable. Other frame kinds (code-graph hits,
/// episodes) keep the plain label form: they are grounding, not memories,
/// and never enter the citation → promotion loop. `None` when no frame has
/// a citation label (L-C4 filters the rest).
fn render_context_section(frames: &[RecalledFrame]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut citable = false;
    for f in frames {
        let label = &f.citation_label;
        if f.kind == "memory" {
            citable = true;
            if let Some(id) = &f.id {
                lines.push(format!("- [{id}] {label} — {}", f.content.trim()));
            }
        } else {
            lines.push(format!("- {} — {}", label, f.content.trim()));
        }
    }
    if lines.is_empty() {
        return None;
    }
    let mut section = format!("Relevant context:\n{}", lines.join("\n"));
    if citable {
        section.push_str(
            "\n\nWhen a memory above (a [nod_…]-tagged line) actually informs your work this \
             turn, call cite_memory with that id once you can judge it: useful_score 1-5 for \
             how much it helped the actual work, truthful for whether its content still holds \
             (verify against the workspace, don't assume), and a one-sentence remark. Cite \
             only memories you genuinely used — no courtesy citations.",
        );
    }
    Some(section)
}

/// Land the recalled-context message for this turn at the conversation
/// TAIL — just before the turn's prompt when the prompt is already present
/// (one-shot paths), appended otherwise (interactive paths push the prompt
/// right after) — leaving previous turns' blocks in place as durable
/// history. Rewriting or removing an early message every turn (the old
/// index-1 refresh) byte-changed the front of the replayed history, which
/// reduced the provider cache's reusable prefix to the system message
/// alone for the whole session — the exact full-rate re-bill L-E8 exists
/// to prevent. Durability's cost is bounded: an unchanged block is not
/// re-appended (the model already sees it), so only genuinely new recall
/// content adds tokens, and it rides the cached prefix from the next turn
/// on. `None` (nothing relevant, or an A/B-suppressed turn) adds nothing
/// and touches nothing.
pub fn inject_recall_block(messages: &mut Vec<CompletionMessage>, block: Option<String>) {
    let is_marker =
        |m: &CompletionMessage| m.role == MessageRole::User && m.content.starts_with(RECALL_MARKER);
    let Some(content) = block else { return };
    if messages
        .iter()
        .rev()
        .find(|m| is_marker(m))
        .is_some_and(|m| m.content == content)
    {
        return;
    }
    let message = CompletionMessage {
        role: MessageRole::User,
        content,
        tool_calls: vec![],
        tool_results: vec![],
        attachments: Vec::new(),
    };
    // Context precedes the question: when the turn's prompt is already the
    // final message, slot the block just before it.
    let at = match messages.last() {
        Some(last)
            if last.role == MessageRole::User
                && !is_marker(last)
                && last.tool_results.is_empty() =>
        {
            messages.len() - 1
        }
        _ => messages.len(),
    };
    messages.insert(at, message);
}

/// Seconds since the Unix epoch — the episode timestamps' primitive.
pub(crate) fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
            attachments: Vec::new(),
        }
    }

    #[test]
    fn ab_control_fires_exactly_once_per_rate_not_every_turn() {
        // The witness for the wall-clock bug: on a microsecond-resolution
        // realtime clock the old `ns % rate == 0` predicate was true on EVERY
        // turn, silently disabling recall. The turn-counter schedule must
        // suppress exactly turns rate, 2*rate, 3*rate — and no others.
        let rate = 10;
        let suppressed: Vec<u32> = (1..=30).filter(|&t| ab_control_turn(t, rate)).collect();
        assert_eq!(
            suppressed,
            vec![10, 20, 30],
            "exactly 1-in-{rate} turns is a control turn"
        );
        // The old bug would have suppressed all 30; guard against a regression
        // back to "always on".
        assert_eq!(
            (1..=30).filter(|&t| ab_control_turn(t, rate)).count(),
            3,
            "recall must be live on the other 27 of 30 turns"
        );
    }

    #[test]
    fn ab_control_disabled_for_rate_zero_and_one() {
        for rate in [0, 1] {
            assert!(
                (1..=50).all(|t| !ab_control_turn(t, rate)),
                "rate {rate} must never suppress"
            );
        }
    }

    #[test]
    fn inject_slots_the_block_before_an_already_present_prompt() {
        let mut messages = vec![
            msg(MessageRole::System, "sys"),
            msg(MessageRole::User, "do the thing"),
        ];
        inject_recall_block(&mut messages, Some(format!("{RECALL_MARKER}\nstuff")));
        assert_eq!(messages.len(), 3);
        assert!(messages[1].content.starts_with(RECALL_MARKER));
        assert_eq!(messages[0].content, "sys", "stable prefix untouched (L-E8)");
        assert_eq!(
            messages[2].content, "do the thing",
            "context precedes the question"
        );
    }

    /// The cache contract: a later turn's refresh may not rewrite, remove,
    /// or reorder anything already in history — the old index-1 refresh
    /// byte-changed the front of the replayed history every turn and cut
    /// the provider cache's reusable prefix to the system message alone.
    #[test]
    fn inject_appends_fresh_blocks_without_touching_history() {
        let mut messages = vec![msg(MessageRole::System, "sys")];
        inject_recall_block(&mut messages, Some(format!("{RECALL_MARKER}\nfirst")));
        messages.push(msg(MessageRole::User, "turn 1"));
        messages.push(msg(MessageRole::Assistant, "did it"));
        let history: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        inject_recall_block(&mut messages, Some(format!("{RECALL_MARKER}\nsecond")));
        // Fresh block at the tail; every prior message byte-identical.
        assert_eq!(messages.len(), history.len() + 1);
        assert!(messages.last().unwrap().content.contains("second"));
        for (i, prior) in history.iter().enumerate() {
            assert_eq!(&messages[i].content, prior, "history rewritten at {i}");
        }
    }

    #[test]
    fn inject_dedupes_an_unchanged_block() {
        let mut messages = vec![msg(MessageRole::System, "sys")];
        let block = format!("{RECALL_MARKER}\nstuff");
        inject_recall_block(&mut messages, Some(block.clone()));
        messages.push(msg(MessageRole::User, "turn 1"));
        messages.push(msg(MessageRole::Assistant, "did it"));
        inject_recall_block(&mut messages, Some(block));
        let markers = messages
            .iter()
            .filter(|m| m.content.starts_with(RECALL_MARKER))
            .count();
        assert_eq!(markers, 1, "an unchanged block is not re-appended");
    }

    #[test]
    fn inject_none_adds_nothing_and_touches_nothing() {
        let mut messages = vec![msg(MessageRole::System, "sys")];
        inject_recall_block(&mut messages, Some(format!("{RECALL_MARKER}\nstuff")));
        let before: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        inject_recall_block(&mut messages, None);
        let after: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        assert_eq!(before, after, "suppressed recall leaves history untouched");
    }

    fn frame(
        id: &str,
        kind: contextgraph_types::FrameKind,
        label: &str,
        content: &str,
    ) -> RecalledFrame {
        let kind = serde_json::to_value(kind)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        RecalledFrame {
            citation_label: label.into(),
            provider: "workspace-memory".into(),
            source: "stella-context".into(),
            kind,
            uri: None,
            method: None,
            content: content.into(),
            token_cost: 10,
            id: Some(id.into()),
        }
    }

    fn contextgraph_frame(
        id: &str,
        kind: contextgraph_types::FrameKind,
        label: &str,
        content: &str,
    ) -> contextgraph_types::ContextFrame {
        contextgraph_types::ContextFrame {
            id: id.into(),
            kind,
            title: label.into(),
            content: Some(content.into()),
            uri: None,
            score: 0.5,
            token_cost: 10,
            content_digest: None,
            representation: contextgraph_types::Representation::Full,
            content_fidelity: None,
            canonical_content_hash: None,
            content_ref: None,
            transform: None,
            minimum_content_fidelity: None,
            inline_content_requirement: None,
            canonical_token_cost: None,
            tokenizer_ref: None,
            valid_from: None,
            valid_to: None,
            recorded_at: None,
            provenance: vec![],
            citation_label: Some(label.into()),
            embedding: None,
            relations: vec![],
        }
    }

    #[test]
    fn recall_section_tags_memory_frames_with_ids_and_asks_for_citations() {
        let frames = vec![
            frame(
                "nod_0123456789abcdef01234567",
                contextgraph_types::FrameKind::Memory,
                "prefer rg",
                "prefer rg over grep here",
            ),
            frame(
                "nod_bbb",
                contextgraph_types::FrameKind::Snippet,
                "src/lib.rs",
                "fn main",
            ),
        ];
        let section = render_context_section(&frames).unwrap();
        assert!(
            section
                .contains("- [nod_0123456789abcdef01234567] prefer rg — prefer rg over grep here"),
            "memory frames carry the citable id: {section}"
        );
        assert!(
            section.contains("- src/lib.rs — fn main"),
            "non-memory frames keep the plain label form: {section}"
        );
        assert!(
            section.contains("cite_memory"),
            "instruction present: {section}"
        );
    }

    #[test]
    fn recall_section_without_memories_never_asks_for_citations() {
        let frames = vec![frame(
            "nod_ccc",
            contextgraph_types::FrameKind::Snippet,
            "src/lib.rs",
            "fn main",
        )];
        let section = render_context_section(&frames).unwrap();
        assert!(!section.contains("cite_memory"));

        // No labeled frames at all → no section (an empty block only burns
        // cache).
        assert!(render_context_section(&[]).is_none());
    }

    #[test]
    fn graph_frame_projection_preserves_provider_and_origin_provenance() {
        let mut graph = contextgraph_frame(
            "code-graph:sym:src/lib.rs:7:run",
            contextgraph_types::FrameKind::Symbol,
            "fn run (src/lib.rs:7)",
            "fn run() {}",
        );
        graph.uri = Some("file:///repo/src/lib.rs".into());
        graph.provenance = vec![
            contextgraph_types::Provenance {
                kind: "file".into(),
                uri: graph.uri.clone(),
                range: Some("L7-9".into()),
                digest: None,
                method: None,
                by: Some("git-worktree".into()),
            },
            contextgraph_types::Provenance {
                kind: "derivation".into(),
                uri: None,
                range: None,
                digest: None,
                method: Some("tree-sitter/symbol-extract".into()),
                by: Some("code-graph".into()),
            },
        ];

        let recalled = project_recalled_frame(crate::contextgraph::AttributedContextFrame {
            provider: "code-graph".into(),
            frame: graph,
        })
        .expect("labeled graph frame projects");

        assert_eq!(recalled.provider, "code-graph");
        assert_eq!(
            recalled.source, "git-worktree",
            "source is the earliest origin actor, not the latest derivation actor"
        );
        assert_eq!(recalled.kind, "symbol");
        assert_eq!(recalled.uri.as_deref(), Some("file:///repo/src/lib.rs"));
        assert_eq!(
            recalled.method.as_deref(),
            Some("tree-sitter/symbol-extract")
        );
    }

    #[test]
    fn quarantine_is_scoped_to_local_memory_provider_and_kind() {
        let quarantined = std::collections::HashSet::from(["shared-id".to_string()]);
        let mut local = frame(
            "shared-id",
            contextgraph_types::FrameKind::Memory,
            "local",
            "local memory",
        );
        assert!(is_quarantined_local_memory(&local, &quarantined));

        local.provider = "external-graph".into();
        assert!(
            !is_quarantined_local_memory(&local, &quarantined),
            "an external provider may reuse a local id"
        );
        local.provider = "workspace-memory".into();
        local.kind = "symbol".into();
        assert!(
            !is_quarantined_local_memory(&local, &quarantined),
            "only actual local memory frames participate in memory quarantine"
        );
    }

    #[tokio::test]
    async fn ab_control_suppresses_skills_before_any_recall_section_is_built() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".stella/skills/reviewer");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: reviewer\ndescription: database review\n---\nALWAYS_REVIEW_DATABASES",
        )
        .unwrap();
        let mut memory = SessionMemory::open_with_workspace_skills(dir.path(), false, true)
            .expect("session memory");
        memory.ab_suppressed = true;

        assert!(
            memory.recall_block("review the database").await.is_none(),
            "a control turn must suppress skills as well as context frames"
        );
    }

    #[tokio::test]
    async fn a_fresh_quarantine_filters_rendered_and_pipeline_recall_next_time() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".stella")).unwrap();
        let memory = SessionMemory::open(dir.path(), false).expect("session memory");
        let lesson = "always use the obsolete frobnicator for database migrations";
        memory
            .store
            .upsert(ContextDelta {
                memories: vec![MemoryInput::reflection(lesson, Vec::<String>::new())],
                ..ContextDelta::default()
            })
            .await
            .unwrap();

        let before = ContextRecallPort::recall(&memory, lesson).await;
        let memory_id = before
            .iter()
            .find(|frame| frame.content.contains("frobnicator"))
            .and_then(|frame| frame.id.clone())
            .expect("new memory is recallable before feedback");
        let rendered_before = memory.recall_block(lesson).await.expect("recall block");
        assert!(rendered_before.contains("frobnicator"));

        let feedback = stella_store::Store::open(dir.path()).unwrap();
        for turn in 0..2 {
            let execution = feedback
                .begin_execution("test", &format!("turn {turn}"), "local", "test")
                .unwrap();
            feedback
                .record_memory_citations(
                    execution,
                    &[stella_store::MemoryCitationRow {
                        memory_id: memory_id.clone(),
                        useful_score: 1,
                        truthful: false,
                        remark: "verified stale".into(),
                    }],
                )
                .unwrap();
        }

        let pipeline_after = ContextRecallPort::recall(&memory, lesson).await;
        assert!(
            pipeline_after
                .iter()
                .all(|frame| frame.id.as_deref() != Some(&memory_id)),
            "pipeline recall must apply quarantine written after session open"
        );
        let rendered_after = memory.recall_block(lesson).await;
        assert!(
            rendered_after
                .as_deref()
                .is_none_or(|block| !block.contains("frobnicator")),
            "rendered recall must use the same freshly quarantined frame set"
        );
    }

    #[cfg(unix)]
    #[test]
    fn context_database_is_private_inside_permissive_dot_stella() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let dot = dir.path().join(".stella");
        std::fs::create_dir_all(&dot).unwrap();
        std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o777)).unwrap();
        drop(SessionMemory::open(dir.path(), false).expect("memory opens"));

        let mode = |path: &Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&dot), 0o777, "mixed project directory is untouched");
        assert_eq!(mode(&dot.join("private")), 0o700);
        assert_eq!(mode(&dot.join("private/context.db")), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn context_database_symlink_is_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let dot = dir.path().join(".stella");
        std::fs::create_dir_all(&dot).unwrap();
        let target = dir.path().join("outside.db");
        let external = ContextStore::open(&target).unwrap();
        drop(external);
        let before = std::fs::read(&target).unwrap();
        std::fs::create_dir_all(dot.join("private")).unwrap();
        symlink(&target, dot.join("private/context.db")).unwrap();

        assert!(SessionMemory::open(dir.path(), false).is_none());
        assert_eq!(std::fs::read(&target).unwrap(), before);
    }

    #[test]
    fn untrusted_project_skill_bodies_are_absent_while_recalled_context_still_renders() {
        let _env = crate::test_env::lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(home.join(".config/stella/skills/user")).unwrap();
        std::fs::write(
            home.join(".config/stella/skills/user/SKILL.md"),
            "---\nname: user\ndescription: user skill\n---\nUSER_SKILL_BODY",
        )
        .unwrap();
        std::fs::create_dir_all(workspace.join(".stella/skills/project")).unwrap();
        std::fs::write(
            workspace.join(".stella/skills/project/SKILL.md"),
            "---\nname: project\ndescription: project skill\n---\nPROJECT_SKILL_BODY",
        )
        .unwrap();
        // SAFETY: serialized behind the binary-wide environment lock.
        let previous_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };
        let _test_home = crate::settings::test_user_home(home.clone());

        let skills = load_workspace_skills_with_authority(&workspace, false).skills;
        let trusted = load_workspace_skills_with_authority(&workspace, true).skills;

        match previous_home {
            Some(previous) => unsafe { std::env::set_var("HOME", previous) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, vec!["user"], "loaded skills: {names:?}");
        let trusted_names: Vec<&str> = trusted.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(trusted_names, vec!["user", "project"]);

        let ordinary = frame(
            "nod_context",
            contextgraph_types::FrameKind::Snippet,
            "src/lib.rs",
            "ordinary recalled evidence",
        );
        let section = render_context_section(&[ordinary]).expect("ordinary recall renders");
        assert!(section.contains("ordinary recalled evidence"), "{section}");
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

    /// End-to-end proof that the self-improvement write path works: a
    /// reflection model call returning lessons must land them in BOTH the
    /// mining log (`.stella/private/reflections.jsonl`) and the recallable context
    /// store. Uses a stub provider so the assertion is deterministic (the
    /// live model legitimately returns `[]` for trivial turns).
    #[tokio::test]
    async fn reflect_and_record_writes_lessons_to_log_and_store() {
        use async_trait::async_trait;
        use stella_protocol::{
            AgentEvent, CompletionRequest, CompletionResult, CompletionUsage, Provider,
            ProviderError,
        };

        struct StubProvider;
        #[async_trait]
        impl Provider for StubProvider {
            fn id(&self) -> &str {
                "stub"
            }
            async fn complete(
                &self,
                _req: CompletionRequest,
            ) -> Result<CompletionResult, ProviderError> {
                Ok(CompletionResult {
                    text: r#"[{"lesson": "prefer withTenantDb over raw db()", "domains": []}]"#
                        .into(),
                    tool_calls: vec![],
                    usage: CompletionUsage {
                        reported: true,
                        input_tokens: 1,
                        ..CompletionUsage::default()
                    },
                    model: "stub".into(),
                    cost_usd: 0.0,
                    finish_reason: None,
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".stella")).unwrap();
        let mut memory =
            SessionMemory::open(dir.path(), false).expect("open session memory in temp workspace");

        let transcript = vec![
            msg(MessageRole::User, "fix the tenancy leak"),
            msg(MessageRole::Assistant, "swapped db() for withTenantDb"),
        ];
        let report = memory
            .reflect_and_record(&StubProvider, "stub", &transcript, true, true, None)
            .await;

        assert_eq!(report.recorded, 1, "the lesson was stored");
        assert!(report.model_error.is_none());
        assert!(report.events.iter().any(|event| matches!(
            event,
            AgentEvent::StepUsage {
                role: stella_protocol::ModelCallRole::Reflection,
                ..
            }
        )));

        // The mining log now carries the lesson, one JSON object per line.
        let log = std::fs::read_to_string(dir.path().join(".stella/private/reflections.jsonl"))
            .expect("reflections.jsonl was written");
        assert!(
            log.contains("withTenantDb"),
            "the lesson reached the mining log: {log}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.path().join(".stella/private")), 0o700);
            assert_eq!(
                mode(&dir.path().join(".stella/private/reflections.jsonl")),
                0o600
            );
        }
    }

    #[tokio::test]
    async fn reflection_preserves_settled_cost_when_budget_rejects_model_output() {
        use async_trait::async_trait;
        use stella_protocol::{
            AgentEvent, CompletionRequest, CompletionResult, CompletionUsage, Provider,
            ProviderError,
        };

        struct PaidReflection;
        #[async_trait]
        impl Provider for PaidReflection {
            fn id(&self) -> &str {
                "paid-reflection"
            }

            async fn complete(
                &self,
                _request: CompletionRequest,
            ) -> Result<CompletionResult, ProviderError> {
                Ok(CompletionResult {
                    text: r#"[{"lesson":"must not apply","domains":[]}]"#.into(),
                    tool_calls: Vec::new(),
                    usage: CompletionUsage {
                        reported: true,
                        input_tokens: 8,
                        output_tokens: 2,
                        ..CompletionUsage::default()
                    },
                    model: "paid-reflection-model".into(),
                    cost_usd: 0.02,
                    finish_reason: None,
                })
            }
        }

        let dir = tempfile::tempdir().expect("root");
        let mut memory = SessionMemory::open(dir.path(), false).expect("memory");
        let report = memory
            .reflect_and_record(
                &PaidReflection,
                "paid-reflection-model",
                &[msg(MessageRole::User, "worked")],
                true,
                true,
                Some(0.001),
            )
            .await;
        assert_eq!(report.recorded, 0);
        assert_eq!(report.cost_usd, 0.02);
        assert!(report.model_error.is_some());
        assert!(report.events.iter().any(|event| matches!(
            event,
            AgentEvent::StepUsage {
                role: stella_protocol::ModelCallRole::Reflection,
                cost_usd,
                ..
            } if (*cost_usd - 0.02).abs() < f64::EPSILON
        )));
    }
}
