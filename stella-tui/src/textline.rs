//! The shared event→text vocabulary — one lookup table for both rendering
//! surfaces (issue #66).
//!
//! Two independent renderers consume [`stella_protocol::AgentEvent`]s: the
//! plain `colored`+`println` surface in `stella-cli` (REPL and one-shot
//! modes) and this crate's ratatui transcript. Before this module each kept
//! its own event→string mapping, so every new `AgentEvent` variant had to be
//! worded twice. The contract now: **wording lives here, styling stays with
//! each surface.** A constructor per annotation variant yields an
//! [`EventLine`] of semantic pieces (glyph, tone, body, detail) that each
//! surface maps onto its own palette — `colored` codes on the plain surface,
//! `ratatui` styles on the deck.
//!
//! The wording is byte-load-bearing: the plain renderer's observable output
//! is composed as `"  {glyph} {body}"` (plus `" {detail}"` when present), and
//! the fixture tests at the bottom pin every line to the exact strings the
//! plain surface printed before the extraction. Change a string here and the
//! plain CLI's output changes with it — that is the point, but it must be
//! deliberate.
//!
//! Deliberately *not* here: streaming `Text`/`Reasoning` (accumulated, then
//! markdown-rendered or printed raw per surface), `Stage` transitions (the
//! deck draws rules, the plain surface prints only a "thinking…" cue), and
//! the `ToolStart`/`ToolResult` cards (the two surfaces present tool traffic
//! structurally differently — key=value cards vs an aligned label column —
//! and unifying them is a behavior change out of scope for #66).

use stella_protocol::{
    AgentEvent, BudgetMode, FileChangeKind, MediaJobState, MediaKind, PrStatus, ProviderShare,
    StageKind,
};

/// Semantic weight of an annotation line. Each surface owns the mapping to
/// its palette (e.g. plain maps `Muted` to ANSI dim, the deck to
/// `theme::MUTED`); no color name may appear in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Warn,
    Error,
    Muted,
}

/// One transcript annotation, split where the surfaces apply different
/// emphasis: a `glyph` carrying the line's `tone` (and `strong` for the few
/// glyphs both surfaces embolden), the `body` text, and an optional trailing
/// `detail` each surface de-emphasizes (dimmed / muted).
#[derive(Debug, Clone, PartialEq)]
pub struct EventLine {
    pub glyph: &'static str,
    pub tone: Tone,
    pub strong: bool,
    pub body: String,
    pub detail: Option<String>,
}

impl EventLine {
    /// The line's unstyled text, exactly as the plain surface prints it
    /// (minus its two-space indent) — what the fixture tests pin.
    pub fn text(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{} {} {}", self.glyph, self.body, detail),
            None => format!("{} {}", self.glyph, self.body),
        }
    }
}

// ── Per-variant constructors: the one place each line is worded ─────────────

pub fn retry(attempt: u32, reason: &str) -> EventLine {
    EventLine {
        glyph: "↻",
        tone: Tone::Warn,
        strong: false,
        body: format!("retry #{attempt}:"),
        detail: Some(reason.to_string()),
    }
}

pub fn compaction(
    before_tokens: u64,
    after_tokens: u64,
    evicted: usize,
    deduped: usize,
) -> EventLine {
    EventLine {
        glyph: "⤵",
        tone: Tone::Info,
        strong: false,
        body: format!(
            "compacted context: {before_tokens} → {after_tokens} tokens ({evicted} evicted, {deduped} deduped)"
        ),
        detail: None,
    }
}

/// The spend line. Visibility policy stays surface-side (the plain surface
/// suppresses ticks in `BudgetMode::Off`; the deck shows every tick and may
/// append the mode as detail).
pub fn budget_tick(spent_usd: f64, limit_usd: Option<f64>) -> EventLine {
    EventLine {
        glyph: "$",
        tone: Tone::Muted,
        strong: false,
        body: format!("spend: {}", spend_amount(spent_usd, limit_usd)),
        detail: None,
    }
}

pub fn provider_fallback(from: &str, to: &str, reason: &str) -> EventLine {
    EventLine {
        glyph: "⚠",
        tone: Tone::Warn,
        strong: true,
        body: format!("provider fallback {from} → {to}:"),
        detail: Some(reason.to_string()),
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the event's metering fields 1:1
pub fn step_usage(
    step: usize,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    cost_usd: f64,
    duration_ms: u64,
    retries: u32,
    tool_calls: usize,
) -> EventLine {
    let cached = if cached_input_tokens > 0 {
        format!(" ({} cached)", fmt_tokens(cached_input_tokens))
    } else {
        String::new()
    };
    let retried = if retries > 0 {
        format!(" · {retries} retry")
    } else {
        String::new()
    };
    let tools = if tool_calls > 0 {
        format!(
            " · {tool_calls} tool call{}",
            if tool_calls == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    EventLine {
        glyph: "·",
        tone: Tone::Muted,
        strong: false,
        body: format!(
            "step {} · {model} · {}{cached} in → {} out · {} · {:.1}s{retried}{tools}",
            step + 1,
            fmt_tokens(input_tokens),
            fmt_tokens(output_tokens),
            fmt_cost(cost_usd),
            duration_ms as f64 / 1000.0,
        ),
        detail: None,
    }
}

pub fn goal_verdict(round: usize, met: bool, reasoning: &str) -> EventLine {
    if met {
        EventLine {
            glyph: "✓",
            tone: Tone::Success,
            strong: true,
            body: format!("judge verdict (round {round}): goal met — {reasoning}"),
            detail: None,
        }
    } else {
        EventLine {
            glyph: "○",
            tone: Tone::Warn,
            strong: false,
            body: format!("judge verdict (round {round}): not yet met — {reasoning}"),
            detail: None,
        }
    }
}

pub fn file_change(path: &str, kind: FileChangeKind) -> EventLine {
    EventLine {
        glyph: "±",
        tone: Tone::Info,
        strong: false,
        body: format!("{} {path}", file_change_verb(kind)),
        detail: None,
    }
}

/// `cited` is the citation tail each surface owns the data for: the plain
/// surface passes the provider mix ([`provider_mix_label`]), the deck the
/// frames' human citation labels (L-C4).
pub fn context_recall(frames: usize, tokens: u32, cited: &str) -> EventLine {
    EventLine {
        glyph: "◈",
        tone: Tone::Info,
        strong: false,
        body: format!("recalled {frames} frames ({tokens} tokens: {cited})"),
        detail: None,
    }
}

pub fn context_write(provider: &str, upserts: u32, superseded: u32) -> EventLine {
    EventLine {
        glyph: "◈",
        tone: Tone::Muted,
        strong: false,
        body: format!(
            "context write-back via {provider}: {upserts} upserts, {superseded} superseded"
        ),
        detail: None,
    }
}

pub fn media_progress(kind: MediaKind, artifact_id: &str, state: &MediaJobState) -> EventLine {
    match state {
        MediaJobState::Failed { reason } => EventLine {
            glyph: "✗",
            tone: Tone::Error,
            strong: false,
            body: format!("{kind:?} job {artifact_id} failed: {reason}"),
            detail: None,
        },
        other => EventLine {
            glyph: "▣",
            tone: Tone::Info,
            strong: false,
            body: format!("{kind:?} job {artifact_id}: {}", media_state_label(other)),
            detail: None,
        },
    }
}

pub fn media_complete(label: &str, path: &str, kind: MediaKind) -> EventLine {
    EventLine {
        glyph: "▣",
        tone: Tone::Success,
        strong: false,
        body: format!("{label} ready: {path} ({})", media_kind_label(kind)),
        detail: None,
    }
}

pub fn judge_verdict(passed: bool, deterministic: bool, summary: &str) -> EventLine {
    let source = if deterministic {
        "deterministic"
    } else {
        "model judge"
    };
    EventLine {
        glyph: if passed { "✓" } else { "✗" },
        tone: if passed { Tone::Success } else { Tone::Error },
        strong: false,
        body: format!("verify ({source}):"),
        detail: Some(summary.to_string()),
    }
}

pub fn scope_review(
    summary: &str,
    steps: usize,
    estimated_files: u32,
    estimated_cost_usd: Option<f64>,
) -> EventLine {
    let cost = estimated_cost_usd
        .map(|c| format!(", ~${c:.2}"))
        .unwrap_or_default();
    EventLine {
        glyph: "⌾",
        tone: Tone::Warn,
        strong: true,
        body: format!("scope review: {summary} ({steps} steps, ~{estimated_files} files{cost})"),
        detail: None,
    }
}

/// The question line only. The structured options — and the binding
/// free-text affordance — are presented by each surface's own interaction
/// machinery (numbered stdin list vs the deck's answer card).
pub fn ask_user(question: &str) -> EventLine {
    EventLine {
        glyph: "?",
        tone: Tone::Warn,
        strong: true,
        body: question.to_string(),
        detail: None,
    }
}

pub fn commit(sha: &str, message: &str) -> EventLine {
    // `get(..8)` not a slice: a sha shorter than 8 bytes (or a non-ASCII
    // test fixture) must fall back whole rather than panic.
    let short = sha.get(..8).unwrap_or(sha);
    EventLine {
        glyph: "●",
        tone: Tone::Success,
        strong: false,
        body: format!("committed {short}"),
        detail: Some(message.to_string()),
    }
}

pub fn pr(url: &str, status: PrStatus) -> EventLine {
    EventLine {
        glyph: "⇡",
        tone: Tone::Info,
        strong: false,
        body: format!("PR {}: {url}", pr_status_label(status)),
        detail: None,
    }
}

/// Routing (stdout vs stderr, transcript row vs toast) stays surface-side.
pub fn error(message: &str, retryable: bool) -> EventLine {
    let label = if retryable { "warning" } else { "error" };
    EventLine {
        glyph: "✗",
        tone: Tone::Error,
        strong: false,
        body: format!("{label}: {message}"),
        detail: None,
    }
}

pub fn complete(model: &str, cost_usd: f64) -> EventLine {
    EventLine {
        glyph: "✓",
        tone: Tone::Success,
        strong: true,
        body: format!("complete · {model} · {}", fmt_cost(cost_usd)),
        detail: None,
    }
}

// ── Event dispatcher ─────────────────────────────────────────────────────────

/// The per-variant lookup table over a raw event stream. `None` for the
/// variants whose presentation is structural per surface (streamed
/// `Text`/`Reasoning`, `Stage`, and the tool cards — see the module doc); a
/// consumer must handle those (or deliberately skip them) itself, and gets
/// every annotation variant — including future ones — from this one table.
pub fn event_line(event: &AgentEvent) -> Option<EventLine> {
    match event {
        AgentEvent::Stage { .. }
        | AgentEvent::Text { .. }
        | AgentEvent::Reasoning { .. }
        | AgentEvent::ToolStart { .. }
        | AgentEvent::ToolResult { .. } => None,
        AgentEvent::Retry { attempt, reason } => Some(retry(*attempt, reason)),
        AgentEvent::Compaction {
            before_tokens,
            after_tokens,
            evicted,
            deduped,
        } => Some(compaction(
            *before_tokens,
            *after_tokens,
            *evicted,
            *deduped,
        )),
        AgentEvent::BudgetTick {
            spent_usd,
            limit_usd,
            ..
        } => Some(budget_tick(*spent_usd, *limit_usd)),
        AgentEvent::ProviderFallback { from, to, reason } => {
            Some(provider_fallback(from, to, reason))
        }
        AgentEvent::StepUsage {
            step,
            model,
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cost_usd,
            duration_ms,
            retries,
            tool_calls,
            // Estimator calibration feedback, not display material.
            ..
        } => Some(step_usage(
            *step,
            model,
            *input_tokens,
            *output_tokens,
            *cached_input_tokens,
            *cost_usd,
            *duration_ms,
            *retries,
            *tool_calls,
        )),
        AgentEvent::GoalVerdict {
            round,
            met,
            reasoning,
            ..
        } => Some(goal_verdict(*round, *met, reasoning)),
        AgentEvent::FileChange { path, kind, .. } => Some(file_change(path, *kind)),
        AgentEvent::ContextRecall {
            frames,
            provider_mix,
            tokens,
        } => Some(context_recall(
            frames.len(),
            *tokens,
            &provider_mix_label(provider_mix),
        )),
        AgentEvent::ContextWrite {
            provider,
            upserts,
            superseded,
        } => Some(context_write(provider, *upserts, *superseded)),
        AgentEvent::MediaProgress {
            artifact_id,
            kind,
            state,
        } => Some(media_progress(*kind, artifact_id, state)),
        AgentEvent::MediaComplete { artifact } => Some(media_complete(
            &artifact.label,
            &artifact.path,
            artifact.kind,
        )),
        AgentEvent::JudgeVerdict { passed, evidence } => Some(judge_verdict(
            *passed,
            evidence.deterministic,
            &evidence.summary,
        )),
        AgentEvent::ScopeReview { proposal } => Some(scope_review(
            &proposal.summary,
            proposal.steps.len(),
            proposal.estimated_files,
            proposal.estimated_cost_usd,
        )),
        AgentEvent::AskUser { question, .. } => Some(ask_user(question)),
        AgentEvent::Commit { sha, message } => Some(commit(sha, message)),
        AgentEvent::Pr { url, status } => Some(pr(url, *status)),
        AgentEvent::Error { message, retryable } => Some(error(message, *retryable)),
        AgentEvent::Complete { model, cost_usd } => Some(complete(model, *cost_usd)),
    }
}

// ── Enum label tables ────────────────────────────────────────────────────────

pub fn stage_label(stage: StageKind) -> &'static str {
    match stage {
        StageKind::Triage => "triage",
        StageKind::ContextRecall => "context recall",
        StageKind::Plan => "plan",
        StageKind::ScopeReview => "scope review",
        StageKind::Witness => "witness",
        StageKind::Execute => "execute",
        StageKind::Verify => "verify",
        StageKind::Judge => "judge",
        StageKind::Reflect => "reflect",
        StageKind::ContextWrite => "context write",
        StageKind::Complete => "complete",
    }
}

pub fn budget_mode_label(mode: BudgetMode) -> &'static str {
    match mode {
        BudgetMode::Off => "off",
        BudgetMode::Observed => "observed",
        BudgetMode::Enforced => "enforced",
    }
}

pub fn media_kind_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Svg => "svg",
        MediaKind::Video => "video",
    }
}

pub fn pr_status_label(status: PrStatus) -> &'static str {
    match status {
        PrStatus::Draft => "draft",
        PrStatus::Open => "open",
        PrStatus::Merged => "merged",
        PrStatus::Closed => "closed",
    }
}

/// A flat display label for a media job state (the wire enum is tagged).
pub fn media_state_label(state: &MediaJobState) -> String {
    match state {
        MediaJobState::Queued => "queued".to_string(),
        MediaJobState::Running => "running".to_string(),
        MediaJobState::Succeeded => "succeeded".to_string(),
        MediaJobState::Failed { reason } => format!("failed: {reason}"),
    }
}

/// Past-tense verb for a `FileChange` transcript line.
pub fn file_change_verb(kind: FileChangeKind) -> &'static str {
    match kind {
        FileChangeKind::Read => "read",
        FileChangeKind::Created => "created",
        FileChangeKind::Modified => "modified",
        FileChangeKind::Deleted => "deleted",
    }
}

/// The CRUD badge letter for a file-change kind — the vocabulary the
/// files-touched panels share with the plain CLI's registry ledger.
pub fn crud_letter(kind: FileChangeKind) -> &'static str {
    match kind {
        FileChangeKind::Read => "R",
        FileChangeKind::Created => "C",
        FileChangeKind::Modified => "U",
        FileChangeKind::Deleted => "D",
    }
}

// ── Number / amount formatting ───────────────────────────────────────────────

/// Render a token count compactly: `842`, `12.3k`, `1.2M`.
pub fn fmt_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// A USD cost at the 4-decimal precision every spend line uses.
pub fn fmt_cost(cost_usd: f64) -> String {
    format!("${cost_usd:.4}")
}

/// Spend against an optional limit — the HUD's spend gauge and the budget
/// tick line share this exact form.
pub fn spend_amount(spent_usd: f64, limit_usd: Option<f64>) -> String {
    match limit_usd {
        Some(limit) => format!("${spent_usd:.4} / ${limit:.2}"),
        None => format!("${spent_usd:.4}"),
    }
}

/// `2×code-graph, 1×memory` — a recall's provider mix as cited text.
pub fn provider_mix_label(mix: &[ProviderShare]) -> String {
    mix.iter()
        .map(|share| format!("{}×{}", share.frames, share.provider))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::{ContextFrameRef, JudgeEvidence, MediaArtifactRef, ScopeProposal};

    // ── Byte-exact fixtures ──────────────────────────────────────────────
    //
    // These strings are the plain CLI's pre-extraction output (issue #66):
    // `stella-cli` prints `"  {}"` around `EventLine::text()`, so each
    // fixture pins the visible line the extraction must not change.

    #[test]
    fn wording_matches_the_pre_extraction_plain_renderer_byte_for_byte() {
        assert_eq!(retry(2, "rate limited").text(), "↻ retry #2: rate limited");
        assert_eq!(
            compaction(10_000, 4_000, 3, 2).text(),
            "⤵ compacted context: 10000 → 4000 tokens (3 evicted, 2 deduped)"
        );
        assert_eq!(
            budget_tick(0.42, Some(2.5)).text(),
            "$ spend: $0.4200 / $2.50"
        );
        assert_eq!(budget_tick(0.42, None).text(), "$ spend: $0.4200");
        assert_eq!(
            provider_fallback("zai", "anthropic", "circuit open").text(),
            "⚠ provider fallback zai → anthropic: circuit open"
        );
        assert_eq!(
            step_usage(3, "glm-5.2", 12_000, 450, 9_000, 0.0042, 1_830, 1, 4).text(),
            "· step 4 · glm-5.2 · 12.0k (9.0k cached) in → 450 out · $0.0042 · 1.8s · 1 retry · 4 tool calls"
        );
        assert_eq!(
            step_usage(0, "glm-5.2", 842, 10, 0, 0.001, 500, 0, 1).text(),
            "· step 1 · glm-5.2 · 842 in → 10 out · $0.0010 · 0.5s · 1 tool call"
        );
        assert_eq!(
            goal_verdict(2, true, "tests pass").text(),
            "✓ judge verdict (round 2): goal met — tests pass"
        );
        assert_eq!(
            goal_verdict(1, false, "still failing").text(),
            "○ judge verdict (round 1): not yet met — still failing"
        );
        assert_eq!(
            file_change("src/lib.rs", FileChangeKind::Modified).text(),
            "± modified src/lib.rs"
        );
        assert_eq!(
            context_recall(2, 120, "2×code-graph").text(),
            "◈ recalled 2 frames (120 tokens: 2×code-graph)"
        );
        assert_eq!(
            context_write("mem0", 3, 1).text(),
            "◈ context write-back via mem0: 3 upserts, 1 superseded"
        );
        assert_eq!(
            media_progress(MediaKind::Image, "a1", &MediaJobState::Running).text(),
            "▣ Image job a1: running"
        );
        assert_eq!(
            media_progress(
                MediaKind::Video,
                "a2",
                &MediaJobState::Failed {
                    reason: "nsfw".into()
                }
            )
            .text(),
            "✗ Video job a2 failed: nsfw"
        );
        assert_eq!(
            media_complete("diagram", ".stella/artifacts/a2.png", MediaKind::Image).text(),
            "▣ diagram ready: .stella/artifacts/a2.png (image)"
        );
        assert_eq!(
            judge_verdict(true, true, "flip oracle passed").text(),
            "✓ verify (deterministic): flip oracle passed"
        );
        assert_eq!(
            judge_verdict(false, false, "inconclusive").text(),
            "✗ verify (model judge): inconclusive"
        );
        assert_eq!(
            scope_review("refactor auth", 2, 12, Some(1.25)).text(),
            "⌾ scope review: refactor auth (2 steps, ~12 files, ~$1.25)"
        );
        assert_eq!(
            scope_review("small fix", 1, 1, None).text(),
            "⌾ scope review: small fix (1 steps, ~1 files)"
        );
        assert_eq!(ask_user("which database?").text(), "? which database?");
        assert_eq!(
            commit("abc1234567", "feat: x").text(),
            "● committed abc12345 feat: x"
        );
        assert_eq!(
            commit("abc", "short sha").text(),
            "● committed abc short sha"
        );
        assert_eq!(
            pr("https://x/pr/1", PrStatus::Open).text(),
            "⇡ PR open: https://x/pr/1"
        );
        assert_eq!(error("boom", false).text(), "✗ error: boom");
        assert_eq!(error("blip", true).text(), "✗ warning: blip");
        assert_eq!(
            complete("glm-5.2", 0.0123).text(),
            "✓ complete · glm-5.2 · $0.0123"
        );
    }

    #[test]
    fn fmt_helpers_keep_their_exact_forms() {
        assert_eq!(fmt_tokens(842), "842");
        assert_eq!(fmt_tokens(12_300), "12.3k");
        assert_eq!(fmt_tokens(1_200_000), "1.2M");
        assert_eq!(fmt_cost(0.0042), "$0.0042");
        assert_eq!(spend_amount(0.42, Some(2.0)), "$0.4200 / $2.00");
        assert_eq!(spend_amount(0.42, None), "$0.4200");
        assert_eq!(
            provider_mix_label(&[
                ProviderShare {
                    provider: "code-graph".into(),
                    frames: 2,
                },
                ProviderShare {
                    provider: "memory".into(),
                    frames: 1,
                },
            ]),
            "2×code-graph, 1×memory"
        );
        assert_eq!(provider_mix_label(&[]), "");
    }

    #[test]
    fn label_tables_cover_every_variant() {
        assert_eq!(stage_label(StageKind::ContextRecall), "context recall");
        assert_eq!(budget_mode_label(BudgetMode::Enforced), "enforced");
        assert_eq!(media_kind_label(MediaKind::Svg), "svg");
        assert_eq!(pr_status_label(PrStatus::Merged), "merged");
        assert_eq!(
            media_state_label(&MediaJobState::Failed { reason: "x".into() }),
            "failed: x"
        );
        assert_eq!(file_change_verb(FileChangeKind::Created), "created");
        assert_eq!(crud_letter(FileChangeKind::Deleted), "D");
    }

    // ── Dispatch coverage ────────────────────────────────────────────────

    #[test]
    fn event_line_maps_every_annotation_variant_and_skips_the_structural_ones() {
        let annotations: Vec<AgentEvent> = vec![
            AgentEvent::Retry {
                attempt: 1,
                reason: "x".into(),
            },
            AgentEvent::Compaction {
                before_tokens: 2,
                after_tokens: 1,
                evicted: 1,
                deduped: 0,
            },
            AgentEvent::BudgetTick {
                spent_usd: 0.1,
                limit_usd: None,
                mode: BudgetMode::Observed,
            },
            AgentEvent::ProviderFallback {
                from: "a".into(),
                to: "b".into(),
                reason: "x".into(),
            },
            AgentEvent::StepUsage {
                step: 0,
                model: "m".into(),
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                estimated_input_tokens: 0,
                cost_usd: 0.0,
                duration_ms: 1,
                retries: 0,
                tool_calls: 0,
            },
            AgentEvent::GoalVerdict {
                round: 1,
                met: true,
                reasoning: "x".into(),
                cost_usd: 0.0,
            },
            AgentEvent::FileChange {
                path: "a.rs".into(),
                kind: FileChangeKind::Created,
                diff: None,
            },
            AgentEvent::ContextRecall {
                frames: vec![ContextFrameRef {
                    id: None,
                    citation_label: "l".into(),
                    source: "s".into(),
                    token_cost: 1,
                }],
                provider_mix: vec![],
                tokens: 1,
            },
            AgentEvent::ContextWrite {
                provider: "p".into(),
                upserts: 1,
                superseded: 0,
            },
            AgentEvent::MediaProgress {
                artifact_id: "a".into(),
                kind: MediaKind::Image,
                state: MediaJobState::Queued,
            },
            AgentEvent::MediaComplete {
                artifact: MediaArtifactRef {
                    id: "a".into(),
                    kind: MediaKind::Image,
                    path: "p".into(),
                    label: "l".into(),
                },
            },
            AgentEvent::JudgeVerdict {
                passed: true,
                evidence: JudgeEvidence {
                    summary: "s".into(),
                    deterministic: true,
                    evidence_refs: vec![],
                },
            },
            AgentEvent::ScopeReview {
                proposal: ScopeProposal {
                    summary: "s".into(),
                    steps: vec![],
                    estimated_files: 1,
                    estimated_cost_usd: None,
                },
            },
            AgentEvent::AskUser {
                id: "q".into(),
                question: "?".into(),
                options: vec![],
            },
            AgentEvent::Commit {
                sha: "abc".into(),
                message: "m".into(),
            },
            AgentEvent::Pr {
                url: "u".into(),
                status: PrStatus::Open,
            },
            AgentEvent::Error {
                message: "e".into(),
                retryable: false,
            },
            AgentEvent::Complete {
                model: "m".into(),
                cost_usd: 0.0,
            },
        ];
        for event in &annotations {
            assert!(
                event_line(event).is_some(),
                "annotation variant unmapped: {event:?}"
            );
        }

        use stella_protocol::{ToolCall, ToolOutput};
        let structural: Vec<AgentEvent> = vec![
            AgentEvent::Stage {
                name: StageKind::Execute,
            },
            AgentEvent::Text { delta: "t".into() },
            AgentEvent::Reasoning { delta: "r".into() },
            AgentEvent::ToolStart {
                call: ToolCall {
                    call_id: "c".into(),
                    name: "n".into(),
                    input: serde_json::Value::Null,
                },
            },
            AgentEvent::ToolResult {
                call_id: "c".into(),
                output: ToolOutput::Ok {
                    content: "o".into(),
                },
                duration_ms: 1,
                speculated: false,
            },
        ];
        for event in &structural {
            assert!(
                event_line(event).is_none(),
                "structural variant must stay surface-owned: {event:?}"
            );
        }
    }

    #[test]
    fn event_line_recall_cites_the_provider_mix_on_the_event_path() {
        let line = event_line(&AgentEvent::ContextRecall {
            frames: vec![ContextFrameRef {
                id: None,
                citation_label: "driver.rs".into(),
                source: "code-graph".into(),
                token_cost: 120,
            }],
            provider_mix: vec![ProviderShare {
                provider: "code-graph".into(),
                frames: 1,
            }],
            tokens: 120,
        })
        .expect("recall is an annotation");
        assert_eq!(
            line.text(),
            "◈ recalled 1 frames (120 tokens: 1×code-graph)"
        );
    }
}
