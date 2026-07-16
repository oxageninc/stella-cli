//! The pure render model: a deterministic fold of the `AgentEvent` log into
//! the derived state every panel draws from (`09-lessons-learned.md` L-T1).
//!
//! [`SessionModel`] owns **only** state that is reconstructible by replaying
//! the event log from seq 1 — transcript lines, the files-touched map, HUD
//! numbers, and the pending scope-review. It has exactly one mutator,
//! [`SessionModel::apply`]; there is no other way to change it. Ephemeral
//! interaction state (scroll offset, composer buffer, panel focus) that is
//! *not* derived from events lives in [`crate::ui::UiState`], never here —
//! that boundary is what makes replay-from-seq-1 a supported debug mode and
//! what makes the panel panic boundary sound (render is a pure function over
//! `&SessionModel`, so a panicking panel can be caught and discarded without
//! leaving torn state — L-T7).
//!
//! Styling is deliberately *not* stored here: entries are semantic records,
//! and [`crate::render`] converts them to styled `ratatui` lines as a pure
//! function of the model. Determinism therefore extends all the way to the
//! backing cell buffer (the replay-determinism test in [`crate::render`]).

use stella_protocol::{
    AgentEvent, BudgetMode, FileChangeKind, MediaKind, PrStatus, ScopeProposal, StageKind,
    ToolOutput,
};

/// How many characters of a tool input / output summary we retain on a
/// transcript line before eliding — the full payload is never needed on the
/// one-line card (the diff panel and detail views carry the rest).
const SUMMARY_BUDGET: usize = 200;

/// The whole derived state of a session, folded from its `AgentEvent` log.
///
/// Every field is a pure function of the sequence of events applied so far;
/// two `SessionModel`s that have seen the same event vector are identical
/// (the L-T1 replay-determinism guarantee, exercised by tests here and in
/// [`crate::render`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionModel {
    /// The scrollback transcript, oldest first. Streaming `Text`/`Reasoning`
    /// deltas are accumulated into the trailing entry rather than producing
    /// one line per token.
    pub transcript: Vec<TranscriptEntry>,
    /// Files the agent touched, in first-touched order, each retaining the
    /// latest diff that rode its `FileChange` event (L-T5 — there is no
    /// second data path for diffs).
    pub files: Vec<FileState>,
    /// Live HUD numbers: spend/limit/mode, current stage, model.
    pub hud: Hud,
    /// A scope-review gate awaiting the user's decision (L-E5). Set by a
    /// `ScopeReview` event and cleared by the engine's follow-on event
    /// (a non-scope-review `Stage`, `Complete`, or `Error`) — so the pending
    /// state is itself purely event-derived and reconstructs on replay.
    pub pending_scope_review: Option<ScopeProposal>,
    /// An `ask_user` question awaiting the user's answer. Set by an `AskUser`
    /// event; cleared purely by events — the answer returns as the tool call's
    /// ordinary `ToolResult` (matched by `id`), so a `ToolResult` with the
    /// question's `call_id` clears it (also cleared on `Complete`/`Error`).
    pub pending_ask_user: Option<AskUserPrompt>,
}

/// A pending `ask_user` question. The renderer contract is binding: present
/// the structured `options` **and** always exactly one additional free-text
/// affordance so the user can answer in their own words on every question
/// (`stella_protocol::AgentEvent::AskUser`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserPrompt {
    /// Correlates the answer back to the question (the `ask_user` tool call's
    /// `call_id`).
    pub id: String,
    pub question: String,
    pub options: Vec<String>,
}

/// One semantic entry in the transcript. Rendering (colour, borders, glyphs)
/// is applied by [`crate::render`]; this type carries only content.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEntry {
    /// A user-submitted prompt. Not an `AgentEvent` — the deck driver pushes
    /// this when `PromptStarted` arrives so the user's message is visible
    /// inline in the transcript, matching the Crush-style conversational layout.
    User(String),
    /// A stage boundary marker (`triage`, `plan`, `execute`, …).
    Stage(StageKind),
    /// Accumulated assistant natural-language output.
    Text(String),
    /// Accumulated model reasoning (rendered dimmed).
    Reasoning(String),
    /// A tool invocation began.
    ToolStart {
        call_id: String,
        name: String,
        input: String,
        /// The workspace-relative path the call targets, parsed from its
        /// input's `path` field (every file tool uses that key). Retained so a
        /// mutating tool's `ToolResult` can be correlated back to the file it
        /// touched without re-parsing the elided input summary.
        path: Option<String>,
    },
    /// A tool invocation finished — `ok` is `false` for a typed tool error.
    ToolResult {
        call_id: String,
        ok: bool,
        summary: String,
        duration_ms: u64,
        /// For a *successful* file-mutating tool
        /// (`write_file`/`edit_file`/`delete_file`), the reference the
        /// renderer uses to show this call's diff inline. `None` for reads,
        /// non-file tools, and failed calls — which gates the inline diff to
        /// mutations that actually happened. The diff itself is never stored
        /// here (L-T5: one event-borne diff path).
        diff: Option<InlineDiffRef>,
    },
    /// A model call was retried (surfaced only once the step commits — the
    /// engine defers these, L-E10).
    Retry { attempt: u32, reason: String },
    /// A compaction pass ran.
    Compaction {
        before_tokens: u64,
        after_tokens: u64,
        evicted: usize,
        deduped: usize,
    },
    /// A spend tick (also folded into [`Hud`]); kept on the transcript as a
    /// dim one-liner so the money trail is visible in scrollback.
    BudgetTick {
        spent_usd: f64,
        limit_usd: Option<f64>,
        mode: BudgetMode,
    },
    /// A provider circuit breaker opened and the router fell back — never
    /// silent (L-M7).
    ProviderFallback {
        from: String,
        to: String,
        reason: String,
    },
    /// Context recall completed; frames are cited by human label, never raw
    /// id (L-C4).
    ContextRecall {
        frames: usize,
        tokens: u32,
        labels: Vec<String>,
    },
    /// Context write-back completed.
    ContextWrite {
        provider: String,
        upserts: u32,
        superseded: u32,
    },
    /// A media job changed state.
    MediaProgress {
        artifact_id: String,
        kind: MediaKind,
        state: String,
    },
    /// A media artifact landed on disk.
    MediaComplete {
        label: String,
        path: String,
        kind: MediaKind,
    },
    /// A verification verdict — `deterministic` distinguishes the flip-oracle
    /// ladder from a model judge (L-E11).
    JudgeVerdict {
        passed: bool,
        summary: String,
        deterministic: bool,
    },
    /// A scope-review gate was presented (the actionable card is driven off
    /// [`SessionModel::pending_scope_review`]; this line is the scrollback
    /// record of it).
    ScopeReview {
        summary: String,
        steps: usize,
        estimated_files: u32,
    },
    /// An `ask_user` question was presented (the actionable card is driven off
    /// [`SessionModel::pending_ask_user`]; this line is the scrollback record).
    AskUser { question: String, options: usize },
    /// A commit landed.
    Commit { sha: String, message: String },
    /// A pull request opened or changed status.
    Pr { url: String, status: PrStatus },
    /// An error event.
    Error { message: String, retryable: bool },
    /// The turn completed.
    Complete { model: String, cost_usd: f64 },
}

/// A mutating tool result's handle on the diff it may render inline: the
/// path into [`SessionModel::files`] plus the value of that file's `changes`
/// counter when the result folded. The renderer shows the inline diff only
/// while the counter still matches — a later mutation of the same path bumps
/// it, so a historical entry can never display a diff its call didn't
/// produce. Only the *reference* lives here; the diff bytes stay on the
/// single event-borne path (L-T5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineDiffRef {
    /// The key into [`SessionModel::files`].
    pub path: String,
    /// [`FileState::changes`] at fold time — stale (hidden) once it differs.
    pub seq: u32,
}

/// The state of one file in the files-touched panel. `latest_diff` is
/// literally the diff carried by the most recent `FileChange` for this path
/// — the single event-borne data path (L-T5).
#[derive(Debug, Clone, PartialEq)]
pub struct FileState {
    pub path: String,
    pub kind: FileChangeKind,
    pub latest_diff: Option<String>,
    /// How many `FileChange` events have touched this path.
    pub changes: u32,
}

/// Live HUD numbers, all folded from the event stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hud {
    pub spent_usd: f64,
    pub limit_usd: Option<f64>,
    pub budget_mode: Option<BudgetMode>,
    pub stage: Option<StageKind>,
    pub model: Option<String>,
    /// The final turn cost, set once a `Complete` event lands.
    pub final_cost_usd: Option<f64>,
    pub complete: bool,
}

impl SessionModel {
    /// A fresh, empty model — the seq-0 state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event into the model. This is the **only** mutator; every
    /// panel's state is a pure function of the sequence of `apply` calls, so
    /// replaying the same log yields an identical model (L-T1).
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Stage { name } => {
                self.hud.stage = Some(*name);
                // Any stage that isn't the scope-review gate itself means the
                // engine has moved past a pending gate (approved → execute,
                // or a later plan/verify stage) — clear it. Kept event-driven
                // so the pending state reconstructs on replay.
                if *name != StageKind::ScopeReview {
                    self.pending_scope_review = None;
                }
                self.transcript.push(TranscriptEntry::Stage(*name));
            }
            AgentEvent::Text { delta } => self.push_text(delta),
            AgentEvent::Reasoning { delta } => self.push_reasoning(delta),
            AgentEvent::ToolStart { call } => {
                self.transcript.push(TranscriptEntry::ToolStart {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    input: format_tool_input(&call.name, &call.input),
                    path: tool_input_path(&call.input),
                });
            }
            AgentEvent::ToolResult {
                call_id,
                output,
                duration_ms,
            } => {
                let (ok, summary) = match output {
                    ToolOutput::Ok { content } => (true, summarize(content)),
                    ToolOutput::Error { message } => (false, summarize(message)),
                };
                // Only a *successful* mutation gets an inline-diff reference —
                // a failed call produced no `FileChange`, and rendering the
                // path's previous diff under its ✗ would attribute a change
                // the call never made. The engine's `FileChangeTap` emits the
                // `FileChange` during the tool's execution, so by the time
                // this result folds, `files[path].changes` already counts this
                // call's own change — that value is the freshness tag.
                let diff = if ok {
                    self.mutated_path_for(call_id).map(|path| {
                        let seq = self
                            .files
                            .iter()
                            .find(|f| f.path == path)
                            .map_or(0, |f| f.changes);
                        InlineDiffRef { path, seq }
                    })
                } else {
                    None
                };
                self.transcript.push(TranscriptEntry::ToolResult {
                    call_id: call_id.clone(),
                    ok,
                    summary,
                    duration_ms: *duration_ms,
                    diff,
                });
                // The answer to an `ask_user` question comes back as this very
                // tool result (correlated by id) — there is no separate answer
                // event — so a matching result clears the pending question.
                if self
                    .pending_ask_user
                    .as_ref()
                    .is_some_and(|p| p.id == *call_id)
                {
                    self.pending_ask_user = None;
                }
            }
            AgentEvent::Retry { attempt, reason } => {
                self.transcript.push(TranscriptEntry::Retry {
                    attempt: *attempt,
                    reason: reason.clone(),
                });
            }
            AgentEvent::Compaction {
                before_tokens,
                after_tokens,
                evicted,
                deduped,
            } => {
                self.transcript.push(TranscriptEntry::Compaction {
                    before_tokens: *before_tokens,
                    after_tokens: *after_tokens,
                    evicted: *evicted,
                    deduped: *deduped,
                });
            }
            AgentEvent::BudgetTick {
                spent_usd,
                limit_usd,
                mode,
            } => {
                self.hud.spent_usd = *spent_usd;
                self.hud.limit_usd = *limit_usd;
                self.hud.budget_mode = Some(*mode);
                self.transcript.push(TranscriptEntry::BudgetTick {
                    spent_usd: *spent_usd,
                    limit_usd: *limit_usd,
                    mode: *mode,
                });
            }
            AgentEvent::ProviderFallback { from, to, reason } => {
                self.transcript.push(TranscriptEntry::ProviderFallback {
                    from: from.clone(),
                    to: to.clone(),
                    reason: reason.clone(),
                });
            }
            AgentEvent::FileChange { path, kind, diff } => self.touch_file(path, *kind, diff),
            AgentEvent::ContextRecall {
                frames,
                provider_mix: _,
                tokens,
            } => {
                let labels = frames.iter().map(|f| f.citation_label.clone()).collect();
                self.transcript.push(TranscriptEntry::ContextRecall {
                    frames: frames.len(),
                    tokens: *tokens,
                    labels,
                });
            }
            AgentEvent::ContextWrite {
                provider,
                upserts,
                superseded,
            } => {
                self.transcript.push(TranscriptEntry::ContextWrite {
                    provider: provider.clone(),
                    upserts: *upserts,
                    superseded: *superseded,
                });
            }
            AgentEvent::MediaProgress {
                artifact_id,
                kind,
                state,
            } => {
                self.transcript.push(TranscriptEntry::MediaProgress {
                    artifact_id: artifact_id.clone(),
                    kind: *kind,
                    state: media_state_label(state),
                });
            }
            AgentEvent::MediaComplete { artifact } => {
                self.transcript.push(TranscriptEntry::MediaComplete {
                    label: artifact.label.clone(),
                    path: artifact.path.clone(),
                    kind: artifact.kind,
                });
            }
            AgentEvent::JudgeVerdict { passed, evidence } => {
                self.transcript.push(TranscriptEntry::JudgeVerdict {
                    passed: *passed,
                    summary: evidence.summary.clone(),
                    deterministic: evidence.deterministic,
                });
            }
            AgentEvent::ScopeReview { proposal } => {
                self.transcript.push(TranscriptEntry::ScopeReview {
                    summary: proposal.summary.clone(),
                    steps: proposal.steps.len(),
                    estimated_files: proposal.estimated_files,
                });
                self.pending_scope_review = Some(proposal.clone());
            }
            AgentEvent::AskUser {
                id,
                question,
                options,
            } => {
                self.transcript.push(TranscriptEntry::AskUser {
                    question: question.clone(),
                    options: options.len(),
                });
                self.pending_ask_user = Some(AskUserPrompt {
                    id: id.clone(),
                    question: question.clone(),
                    options: options.clone(),
                });
            }
            AgentEvent::Commit { sha, message } => {
                self.transcript.push(TranscriptEntry::Commit {
                    sha: sha.clone(),
                    message: message.clone(),
                });
            }
            AgentEvent::Pr { url, status } => {
                self.transcript.push(TranscriptEntry::Pr {
                    url: url.clone(),
                    status: *status,
                });
            }
            // `StepUsage` is a metering/billing record consumed by
            // `stella-store`; the HUD's live spend is driven by `BudgetTick`,
            // so folding it here would double-count. `GoalVerdict`'s own
            // `cost_usd` is likewise already accounted against the budget when
            // it fires. Neither mutates TUI state today (a goal-verdict
            // transcript row is a display enhancement, tracked as follow-up).
            AgentEvent::StepUsage { .. } | AgentEvent::GoalVerdict { .. } => {}
            AgentEvent::Error { message, retryable } => {
                self.pending_scope_review = None;
                self.pending_ask_user = None;
                self.transcript.push(TranscriptEntry::Error {
                    message: message.clone(),
                    retryable: *retryable,
                });
            }
            AgentEvent::Complete { model, cost_usd } => {
                self.hud.stage = Some(StageKind::Complete);
                self.hud.model = Some(model.clone());
                self.hud.final_cost_usd = Some(*cost_usd);
                self.hud.complete = true;
                self.pending_scope_review = None;
                self.pending_ask_user = None;
                self.transcript.push(TranscriptEntry::Complete {
                    model: model.clone(),
                    cost_usd: *cost_usd,
                });
            }
        }
    }

    /// Fold an entire log at once — the replay entry point.
    pub fn replay(events: &[AgentEvent]) -> Self {
        let mut model = Self::new();
        for event in events {
            model.apply(event);
        }
        model
    }

    /// Append a streaming text delta, coalescing into the trailing `Text`
    /// entry when the last thing emitted was also assistant text.
    fn push_text(&mut self, delta: &str) {
        if let Some(TranscriptEntry::Text(buf)) = self.transcript.last_mut() {
            buf.push_str(delta);
        } else {
            self.transcript
                .push(TranscriptEntry::Text(delta.to_string()));
        }
    }

    /// Append a streaming reasoning delta, coalescing like [`Self::push_text`].
    fn push_reasoning(&mut self, delta: &str) {
        if let Some(TranscriptEntry::Reasoning(buf)) = self.transcript.last_mut() {
            buf.push_str(delta);
        } else {
            self.transcript
                .push(TranscriptEntry::Reasoning(delta.to_string()));
        }
    }

    /// Push a user-submitted prompt into the transcript. This is **not** an
    /// `AgentEvent` fold — the deck driver calls this when `PromptStarted`
    /// arrives so user messages appear inline in the conversational scrollback.
    pub fn push_user_prompt(&mut self, text: &str) {
        self.transcript
            .push(TranscriptEntry::User(text.to_string()));
    }

    /// If tool call `call_id` was a file mutation, the path it touched —
    /// recovered by correlating back to its `ToolStart` (which is already on
    /// the transcript by the time the result folds). `None` for reads and
    /// non-file tools, which is what gates the transcript's inline diff to
    /// mutations. The diff itself is *not* looked up here — the renderer reads
    /// it from [`SessionModel::files`] at draw time (L-T5).
    fn mutated_path_for(&self, call_id: &str) -> Option<String> {
        self.transcript
            .iter()
            .rev()
            .find_map(|entry| match entry {
                TranscriptEntry::ToolStart {
                    call_id: cid,
                    name,
                    path,
                    ..
                } if cid == call_id => Some((name.clone(), path.clone())),
                _ => None,
            })
            .and_then(|(name, path)| is_file_mutation(&name).then_some(path).flatten())
    }

    /// Record a file touch, retaining the latest diff for the path (L-T5).
    fn touch_file(&mut self, path: &str, kind: FileChangeKind, diff: &Option<String>) {
        if let Some(existing) = self.files.iter_mut().find(|f| f.path == path) {
            existing.kind = kind;
            existing.latest_diff = diff.clone();
            existing.changes += 1;
        } else {
            self.files.push(FileState {
                path: path.to_string(),
                kind,
                latest_diff: diff.clone(),
                changes: 1,
            });
        }
    }
}

/// Compact a tool-call input `Value` to a single-line JSON string. Falls back
/// to the empty string on the (impossible for `Value`) serialization error so
/// the model never panics on a tool card.
fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Format a tool-call input as a human-readable one-liner. Instead of raw
/// JSON, this extracts the most relevant field(s) per tool name so the
/// transcript reads naturally — `path` for file tools, `cmd` for shell, the
/// query for search tools, and so on.
fn format_tool_input(name: &str, input: &serde_json::Value) -> String {
    let str_field = |key: &str| -> Option<String> {
        input.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
    };

    // Primary field per tool — the one the user cares about at a glance.
    if let Some(p) = str_field("path").or_else(|| str_field("file_path")) {
        return match name {
            "edit_file" => {
                let old = str_field("old_string").map(|s| truncate_field(&s, 40));
                let new = str_field("new_string").map(|s| truncate_field(&s, 40));
                match (old, new) {
                    (Some(o), Some(n)) => format!("{p}  {o} → {n}"),
                    _ => p,
                }
            }
            "write_file" => {
                let lines = str_field("content").map(|c| c.lines().count()).unwrap_or(0);
                format!("{p}  ({lines} lines)")
            }
            _ => p,
        };
    }

    if let Some(cmd) = str_field("cmd").or_else(|| str_field("command")) {
        return truncate_field(&cmd, 120);
    }

    if let Some(query) = str_field("query").or_else(|| str_field("pattern"))
        .or_else(|| str_field("symbol"))
    {
        return truncate_field(&query, 80);
    }

    if let Some(prompt) = str_field("question").or_else(|| str_field("prompt")) {
        return truncate_field(&prompt, 80);
    }

    // Fallback: compact JSON, summarized.
    summarize(&compact_json(input))
}

/// Truncate a field value to `max` chars with an ellipsis.
fn truncate_field(s: &str, max: usize) -> String {
    let flat = s.replace(['\n', '\r'], " ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= max {
        return flat;
    }
    let head: String = chars[..max.saturating_sub(1)].iter().collect();
    format!("{head}…")
}

/// The workspace-relative path a file tool targets. Every built-in file tool
/// (`read_file`/`write_file`/`edit_file`/`delete_file`) takes its path under
/// the `path` key, and the engine emits `FileChange` for that same path — so
/// this is the join key between a tool result and its diff.
fn tool_input_path(input: &serde_json::Value) -> Option<String> {
    input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Whether a tool name is one of the file-*mutating* built-ins — the only
/// tools whose result should carry an inline diff (reads must not). Must
/// stay in lockstep with `file_change_of` in stella-cli's `command_deck.rs`,
/// the `FileChange` emitter that owns this list.
fn is_file_mutation(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "delete_file")
}

/// Truncate a summary to [`SUMMARY_BUDGET`] chars with a middle-out elision —
/// the head and tail both matter for a failing tool result (L-S3), so we keep
/// both rather than head-truncating away the error tail.
fn summarize(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= SUMMARY_BUDGET {
        return flat;
    }
    let keep = SUMMARY_BUDGET.saturating_sub(3);
    let head = keep / 2;
    let tail = keep - head;
    let head_str: String = chars[..head].iter().collect();
    let tail_str: String = chars[chars.len() - tail..].iter().collect();
    format!("{head_str}...{tail_str}")
}

/// A short display label for a media job state (the wire enum is tagged; the
/// TUI needs a flat human string).
fn media_state_label(state: &stella_protocol::MediaJobState) -> String {
    use stella_protocol::MediaJobState::*;
    match state {
        Queued => "queued".to_string(),
        Running => "running".to_string(),
        Succeeded => "succeeded".to_string(),
        Failed { reason } => format!("failed: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::{
        ContextFrameRef, JudgeEvidence, MediaArtifactRef, MediaJobState, ProviderShare, ToolCall,
    };

    fn text(delta: &str) -> AgentEvent {
        AgentEvent::Text {
            delta: delta.into(),
        }
    }

    #[test]
    fn streaming_text_deltas_coalesce_into_one_entry() {
        let mut model = SessionModel::new();
        model.apply(&text("Hel"));
        model.apply(&text("lo, "));
        model.apply(&text("world"));
        assert_eq!(model.transcript.len(), 1);
        match &model.transcript[0] {
            TranscriptEntry::Text(s) => assert_eq!(s, "Hello, world"),
            other => panic!("expected coalesced text, got {other:?}"),
        }
    }

    #[test]
    fn a_stage_between_text_deltas_breaks_coalescing() {
        let mut model = SessionModel::new();
        model.apply(&text("a"));
        model.apply(&AgentEvent::Stage {
            name: StageKind::Verify,
        });
        model.apply(&text("b"));
        // text, stage, text
        assert_eq!(model.transcript.len(), 3);
        assert!(matches!(model.transcript[0], TranscriptEntry::Text(_)));
        assert!(matches!(model.transcript[1], TranscriptEntry::Stage(_)));
        assert!(matches!(model.transcript[2], TranscriptEntry::Text(_)));
    }

    #[test]
    fn budget_tick_folds_into_hud_and_transcript() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::BudgetTick {
            spent_usd: 0.42,
            limit_usd: Some(2.0),
            mode: BudgetMode::Enforced,
        });
        assert_eq!(model.hud.spent_usd, 0.42);
        assert_eq!(model.hud.limit_usd, Some(2.0));
        assert_eq!(model.hud.budget_mode, Some(BudgetMode::Enforced));
        assert!(matches!(
            model.transcript.last(),
            Some(TranscriptEntry::BudgetTick { .. })
        ));
    }

    #[test]
    fn file_change_keeps_latest_diff_and_counts_touches() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::FileChange {
            path: "src/a.rs".into(),
            kind: FileChangeKind::Created,
            diff: Some("+first".into()),
        });
        model.apply(&AgentEvent::FileChange {
            path: "src/a.rs".into(),
            kind: FileChangeKind::Modified,
            diff: Some("+second".into()),
        });
        assert_eq!(model.files.len(), 1);
        let f = &model.files[0];
        assert_eq!(f.changes, 2);
        assert_eq!(f.kind, FileChangeKind::Modified);
        assert_eq!(f.latest_diff.as_deref(), Some("+second"));
    }

    #[test]
    fn files_are_kept_in_first_touched_order() {
        let mut model = SessionModel::new();
        for p in ["z.rs", "a.rs", "m.rs"] {
            model.apply(&AgentEvent::FileChange {
                path: p.into(),
                kind: FileChangeKind::Modified,
                diff: None,
            });
        }
        let order: Vec<&str> = model.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(order, vec!["z.rs", "a.rs", "m.rs"]);
    }

    #[test]
    fn scope_review_sets_then_clears_on_next_stage() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::ScopeReview {
            proposal: ScopeProposal {
                summary: "big refactor".into(),
                steps: vec!["s1".into(), "s2".into()],
                estimated_files: 12,
                estimated_cost_usd: Some(1.0),
            },
        });
        assert!(model.pending_scope_review.is_some());
        // The scope-review stage marker itself must NOT clear it.
        model.apply(&AgentEvent::Stage {
            name: StageKind::ScopeReview,
        });
        assert!(model.pending_scope_review.is_some());
        // The engine moving on to execute clears it.
        model.apply(&AgentEvent::Stage {
            name: StageKind::Execute,
        });
        assert!(model.pending_scope_review.is_none());
    }

    #[test]
    fn scope_review_clears_on_error_and_complete() {
        for terminal in [
            AgentEvent::Error {
                message: "aborted".into(),
                retryable: false,
            },
            AgentEvent::Complete {
                model: "glm".into(),
                cost_usd: 0.01,
            },
        ] {
            let mut model = SessionModel::new();
            model.apply(&AgentEvent::ScopeReview {
                proposal: ScopeProposal {
                    summary: "x".into(),
                    steps: vec![],
                    estimated_files: 1,
                    estimated_cost_usd: None,
                },
            });
            assert!(model.pending_scope_review.is_some());
            model.apply(&terminal);
            assert!(model.pending_scope_review.is_none());
        }
    }

    #[test]
    fn complete_populates_hud() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::Complete {
            model: "glm-5.2".into(),
            cost_usd: 0.033,
        });
        assert_eq!(model.hud.model.as_deref(), Some("glm-5.2"));
        assert_eq!(model.hud.final_cost_usd, Some(0.033));
        assert!(model.hud.complete);
        assert_eq!(model.hud.stage, Some(StageKind::Complete));
    }

    #[test]
    fn context_recall_cites_by_label_never_id() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::ContextRecall {
            frames: vec![ContextFrameRef {
                id: Some("913d6df1-uuid".into()),
                citation_label: "driver.rs step-driver".into(),
                source: "code-graph".into(),
                token_cost: 100,
            }],
            provider_mix: vec![ProviderShare {
                provider: "code-graph".into(),
                frames: 1,
            }],
            tokens: 100,
        });
        match model.transcript.last() {
            Some(TranscriptEntry::ContextRecall { labels, .. }) => {
                assert_eq!(labels, &vec!["driver.rs step-driver".to_string()]);
                assert!(!labels.iter().any(|l| l.contains("uuid")));
            }
            other => panic!("expected a context recall entry, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_summary_is_middle_out_truncated() {
        let mut model = SessionModel::new();
        let big = format!("HEAD{}TAIL", "x".repeat(500));
        model.apply(&AgentEvent::ToolResult {
            call_id: "c1".into(),
            output: ToolOutput::Ok { content: big },
            duration_ms: 5,
        });
        match model.transcript.last() {
            Some(TranscriptEntry::ToolResult { summary, .. }) => {
                assert!(summary.starts_with("HEAD"), "kept head: {summary}");
                assert!(summary.ends_with("TAIL"), "kept tail: {summary}");
                assert!(summary.contains("..."), "elided middle: {summary}");
                assert!(summary.chars().count() <= SUMMARY_BUDGET);
            }
            other => panic!("expected a tool result entry, got {other:?}"),
        }
    }

    #[test]
    fn media_and_judge_and_pr_events_land_on_the_transcript() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::MediaProgress {
            artifact_id: "a1".into(),
            kind: MediaKind::Video,
            state: MediaJobState::Failed {
                reason: "nsfw".into(),
            },
        });
        model.apply(&AgentEvent::MediaComplete {
            artifact: MediaArtifactRef {
                id: "a2".into(),
                kind: MediaKind::Image,
                path: ".stella/artifacts/a2.png".into(),
                label: "diagram".into(),
            },
        });
        model.apply(&AgentEvent::JudgeVerdict {
            passed: true,
            evidence: JudgeEvidence {
                summary: "flip oracle passed".into(),
                deterministic: true,
                evidence_refs: vec![],
            },
        });
        model.apply(&AgentEvent::Pr {
            url: "https://x/pr/1".into(),
            status: PrStatus::Open,
        });
        assert_eq!(model.transcript.len(), 4);
        assert!(matches!(
            model.transcript[0],
            TranscriptEntry::MediaProgress { .. }
        ));
        assert!(matches!(
            model.transcript[3],
            TranscriptEntry::Pr {
                status: PrStatus::Open,
                ..
            }
        ));
    }

    #[test]
    fn ask_user_sets_pending_and_the_matching_tool_result_clears_it() {
        let mut model = SessionModel::new();
        model.apply(&AgentEvent::AskUser {
            id: "call_ask_1".into(),
            question: "which database?".into(),
            options: vec!["postgres".into(), "sqlite".into()],
        });
        let pending = model.pending_ask_user.as_ref().expect("question pending");
        assert_eq!(pending.id, "call_ask_1");
        assert_eq!(pending.options.len(), 2);
        // An unrelated tool result must NOT clear it.
        model.apply(&AgentEvent::ToolResult {
            call_id: "call_other".into(),
            output: ToolOutput::Ok {
                content: "x".into(),
            },
            duration_ms: 1,
        });
        assert!(model.pending_ask_user.is_some());
        // The answer arrives as the ask_user tool's own result (matched by id).
        model.apply(&AgentEvent::ToolResult {
            call_id: "call_ask_1".into(),
            output: ToolOutput::Ok {
                content: "postgres".into(),
            },
            duration_ms: 1,
        });
        assert!(
            model.pending_ask_user.is_none(),
            "matching result clears it"
        );
    }

    #[test]
    fn replay_of_the_same_log_yields_identical_models() {
        let log = vec![
            AgentEvent::Stage {
                name: StageKind::Execute,
            },
            text("hi "),
            text("there"),
            AgentEvent::ToolStart {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "src/lib.rs"}),
                },
            },
            AgentEvent::FileChange {
                path: "src/lib.rs".into(),
                kind: FileChangeKind::Modified,
                diff: Some("@@\n-a\n+b".into()),
            },
            AgentEvent::Complete {
                model: "glm".into(),
                cost_usd: 0.01,
            },
        ];
        let a = SessionModel::replay(&log);
        let b = SessionModel::replay(&log);
        assert_eq!(a, b);
    }
}
