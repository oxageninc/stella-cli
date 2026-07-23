use super::*;
use crate::composer::{Composer, SlashCommand};
use proptest::prelude::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use stella_protocol::{
    AgentEvent, BudgetMode, FileChangeKind, MediaJobState, MediaKind, ScopeProposal, StageKind,
    ToolCall, ToolOutput,
};

/// Flatten a `TestBackend` buffer to one `String` per row (styling
/// stripped — content is what we assert on, never raw ANSI, per L-T6).
fn buffer_rows(buf: &Buffer) -> Vec<String> {
    let area = *buf.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<String>()
        })
        .collect()
}

fn buffer_text(buf: &Buffer) -> String {
    buffer_rows(buf).join("\n")
}

fn draw(model: &SessionModel, ui: &mut UiState, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render(model, ui, f)).unwrap();
    buffer_text(terminal.backend().buffer())
}

#[test]
fn hud_and_transcript_render_the_event_content() {
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::Stage {
        name: StageKind::Execute,
    });
    model.apply(&AgentEvent::Text {
        delta: "building the thing".into(),
    });
    model.apply(&AgentEvent::Complete {
        model: "glm-5.2".into(),
        cost_usd: 0.0123,
    });
    let mut ui = UiState::default();
    let text = draw(&model, &mut ui, 100, 30);
    assert!(text.contains("glm-5.2"), "HUD shows the model:\n{text}");
    assert!(
        text.contains("building the thing"),
        "transcript shows text:\n{text}"
    );
    assert!(text.contains("complete"), "shows completion:\n{text}");
}

/// Every transcript entry renders through the shared label gutter: its
/// first line starts with a right-aligned `[label]: ` tag whose display
/// width is exactly `LABEL_COL`. Nothing prints at the left margin.
#[test]
fn every_transcript_entry_renders_in_the_label_gutter() {
    let samples = vec![
        TranscriptEntry::User("hi".into()),
        TranscriptEntry::Stage(StageKind::Execute),
        TranscriptEntry::Text("ok".into()),
        TranscriptEntry::Reasoning("hmm".into()),
        TranscriptEntry::ToolStart {
            call_id: "c1".into(),
            name: "bash".into(),
            input: "ls".into(),
            raw: "{}".into(),
            path: None,
        },
        TranscriptEntry::ToolResult {
            call_id: "c1".into(),
            name: "bash".into(),
            ok: true,
            summary: "done".into(),
            full: "done".into(),
            duration_ms: 3,
            speculated: false,
            diff: None,
        },
        TranscriptEntry::Retry {
            attempt: 1,
            reason: "rate limit".into(),
        },
        TranscriptEntry::Compaction {
            before_tokens: 10,
            after_tokens: 5,
            evicted: 1,
            deduped: 2,
        },
        TranscriptEntry::BudgetTick {
            spent_usd: 0.01,
            limit_usd: Some(1.0),
            mode: BudgetMode::Observed,
        },
        TranscriptEntry::ProviderFallback {
            from: "a".into(),
            to: "b".into(),
            reason: "down".into(),
        },
        TranscriptEntry::ContextRecall {
            frames: 2,
            tokens: 120,
            labels: vec!["adr".into()],
        },
        TranscriptEntry::ContextWrite {
            provider: "mem".into(),
            upserts: 2,
            superseded: 1,
        },
        TranscriptEntry::MediaProgress {
            artifact_id: "m1".into(),
            kind: MediaKind::Image,
            state: MediaJobState::Queued,
        },
        TranscriptEntry::MediaComplete {
            label: "logo".into(),
            path: "out.png".into(),
            kind: MediaKind::Image,
        },
        TranscriptEntry::JudgeVerdict {
            passed: true,
            summary: "ok".into(),
            deterministic: true,
        },
        TranscriptEntry::ScopeReview {
            summary: "auth".into(),
            steps: 2,
            estimated_files: 3,
        },
        TranscriptEntry::AskUser {
            question: "which db?".into(),
            options: 2,
        },
        TranscriptEntry::Commit {
            sha: "abc123def456".into(),
            message: "fix".into(),
        },
        TranscriptEntry::Pr {
            url: "https://example.test/pr/1".into(),
            status: PrStatus::Open,
            number: Some(1),
            ci: Some(CiStatus::Passing),
        },
        TranscriptEntry::TaskUpdate {
            done: 2,
            total: 5,
            active: Some("wire the task board".into()),
        },
        TranscriptEntry::Error {
            message: "boom".into(),
            retryable: false,
        },
        TranscriptEntry::Complete {
            model: "glm-5.2".into(),
            cost_usd: 0.1,
        },
    ];
    for entry in &samples {
        // Exhaustive on purpose: adding a `TranscriptEntry` variant fails
        // to compile here — add a sample above and render the new arm
        // through `push_labeled`/`push_labeled_block`.
        match entry {
            TranscriptEntry::User(_)
            | TranscriptEntry::Stage(_)
            | TranscriptEntry::Text(_)
            | TranscriptEntry::Reasoning(_)
            | TranscriptEntry::ToolStart { .. }
            | TranscriptEntry::ToolResult { .. }
            | TranscriptEntry::Retry { .. }
            | TranscriptEntry::Compaction { .. }
            // Not in `samples`: it deliberately renders as an untagged
            // system note, not a `[label]: ` line — see
            // `eviction_marker_renders_as_a_one_line_system_note`.
            | TranscriptEntry::Evicted { .. }
            | TranscriptEntry::BudgetTick { .. }
            | TranscriptEntry::ProviderFallback { .. }
            | TranscriptEntry::ContextRecall { .. }
            | TranscriptEntry::ContextWrite { .. }
            | TranscriptEntry::MediaProgress { .. }
            | TranscriptEntry::MediaComplete { .. }
            | TranscriptEntry::JudgeVerdict { .. }
            | TranscriptEntry::ScopeReview { .. }
            | TranscriptEntry::AskUser { .. }
            | TranscriptEntry::Commit { .. }
            | TranscriptEntry::Pr { .. }
            | TranscriptEntry::TaskUpdate { .. }
            | TranscriptEntry::Error { .. }
            | TranscriptEntry::Complete { .. } => {}
        }
        let mut lines = Vec::new();
        entry_lines(entry, &[], false, false, 0, &mut lines);
        let first = lines
            .first()
            .unwrap_or_else(|| panic!("{entry:?} renders no lines"));
        let tag = first.spans.first().expect("first span is the label tag");
        assert!(
            tag.content.ends_with("]: "),
            "{entry:?} must start with a `[label]: ` tag, got {:?}",
            tag.content
        );
        assert_eq!(
            UnicodeWidthStr::width(tag.content.as_ref()),
            LABEL_COL,
            "{entry:?} label tag must place content at LABEL_COL, got {:?}",
            tag.content
        );
    }
}

/// A wrapped continuation line begins flush at the content column: exactly
/// `LABEL_COL` leading spaces, never one more. Regression for the bug where
/// the wrap-boundary space was carried onto the next line, stacking on top
/// of the indent and drifting every wrapped row one column right of the
/// clean left edge (the "extra blank space after the colon on wrap" report).
#[test]
fn wrapped_continuation_starts_flush_at_the_content_column() {
    let content = "the quick brown fox jumps over the lazy dog and then keeps \
                   on running well past the right edge to force several wraps";
    let spans = vec![Span::raw(label_tag("agent")), Span::raw(content)];
    let mut out = Vec::new();
    // Narrow width so the content wraps several times.
    wrap_one_indent(Line::from(spans), 60, LABEL_COL, &mut out);

    assert!(
        out.len() > 1,
        "content must wrap into a continuation row, got {} row(s)",
        out.len()
    );
    for (i, line) in out.iter().enumerate().skip(1) {
        let text: String = line.spans.iter().flat_map(|s| s.content.chars()).collect();
        let leading = text.chars().take_while(|c| *c == ' ').count();
        assert_eq!(
            leading, LABEL_COL,
            "continuation row {i} must start exactly at the content column \
             (indent {LABEL_COL}, no carried wrap space); got {leading}: {text:?}",
        );
    }
}

#[test]
fn files_panel_lists_touched_files_by_label() {
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::FileChange {
        path: "src/driver.rs".into(),
        kind: FileChangeKind::Modified,
        diff: Some("@@\n-old\n+new".into()),
    });
    let mut ui = UiState::default();
    let text = draw(&model, &mut ui, 100, 20);
    assert!(text.contains("src/driver.rs"), "files panel:\n{text}");
    assert!(text.contains("files touched"), "panel title:\n{text}");
}

#[test]
fn diff_viewer_shows_the_selected_files_diff() {
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::FileChange {
        path: "a.rs".into(),
        kind: FileChangeKind::Modified,
        diff: Some("@@ -1 +1 @@\n-removed\n+added".into()),
    });
    let mut ui = UiState::default();
    ui.diff_open = true;
    let text = draw(&model, &mut ui, 100, 20);
    assert!(text.contains("removed"), "diff shows removals:\n{text}");
    assert!(text.contains("added"), "diff shows additions:\n{text}");
    // The PR-style chrome: the path rides the top rule, the line counts
    // ride the bottom rule.
    assert!(text.contains("a.rs"), "path in the header rule:\n{text}");
    assert!(
        text.contains("+1 addition") && text.contains("-1 removal"),
        "counts in the footer rule:\n{text}"
    );
}

#[test]
fn thinking_is_collapsed_by_default_and_expands_with_the_toggle() {
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::Reasoning {
        delta: "alpha\nbeta\ngamma".into(),
    });
    let mut ui = UiState::default();
    let collapsed = draw(&model, &mut ui, 100, 24);
    assert!(
        collapsed.contains("[⏵ thinking]: 3 lines"),
        "collapsed header:\n{collapsed}"
    );
    // Collapsed = header + 3 preview lines (all 3 fit within preview count).
    let c_lines = transcript_lines(&model, false, 0);
    assert_eq!(c_lines.len(), 4, "header + 3 preview lines");
    // Preview shows the reasoning content.
    let c_text: String = c_lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        c_text.contains("alpha"),
        "collapsed shows first line:\n{c_text}"
    );

    ui.thinking_expanded = true;
    let expanded = draw(&model, &mut ui, 100, 24);
    assert!(
        expanded.contains("alpha") && expanded.contains("gamma"),
        "expanded shows the full reasoning:\n{expanded}"
    );
    // Expanded = header + 3 content lines.
    assert_eq!(transcript_lines(&model, true, 0).len(), 4);
}

#[test]
fn collapsed_thinking_shows_preview_lines_not_the_full_wall() {
    let mut model = SessionModel::new();
    let long = format!("{}THE-TAIL", "reasoning noise ".repeat(20));
    model.apply(&AgentEvent::Reasoning { delta: long });
    let lines = transcript_lines(&model, false, 0);
    // 1 line of text → header + 1 preview = 2 lines.
    assert_eq!(lines.len(), 2);
    // The preview should be visible.
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("reasoning"), "preview shows content:\n{text}");
}

#[test]
fn collapsed_thinking_preview_updates_as_deltas_stream() {
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::Reasoning {
        delta: "planning the refactor".into(),
    });
    let before = transcript_lines(&model, false, 0);
    model.apply(&AgentEvent::Reasoning {
        delta: " now checking tests".into(),
    });
    let after = transcript_lines(&model, false, 0);
    // Both produce header + 1 preview = 2 lines.
    assert_eq!(before.len(), 2);
    assert_eq!(after.len(), 2, "still header + 1 preview line");
    // The preview line (index 1) visibly changes with each delta.
    assert_ne!(
        before[1], after[1],
        "the preview visibly changes with each delta"
    );
}

#[test]
fn scope_card_renders_the_decision_legend_when_unanswered() {
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::ScopeReview {
        proposal: ScopeProposal {
            summary: "refactor auth".into(),
            steps: vec!["s1".into(), "s2".into()],
            estimated_files: 9,
            estimated_cost_usd: Some(1.5),
        },
    });
    let mut ui = UiState::default();
    let text = draw(&model, &mut ui, 100, 30);
    assert!(text.contains("refactor auth"), "card summary:\n{text}");
    assert!(text.contains("pprove"), "shows approve legend:\n{text}");
    // Once answered, the legend flips to the awaiting message.
    ui.scope_answered = true;
    let text2 = draw(&model, &mut ui, 100, 30);
    assert!(text2.contains("awaiting"), "flips to awaiting:\n{text2}");
}

#[test]
fn ask_user_card_always_offers_a_free_text_affordance() {
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::AskUser {
        id: "q1".into(),
        question: "which database?".into(),
        options: vec!["postgres".into(), "sqlite".into()],
    });
    let mut ui = UiState::default();
    let text = draw(&model, &mut ui, 100, 30);
    assert!(text.contains("which database?"), "question:\n{text}");
    assert!(text.contains("postgres"), "option 1:\n{text}");
    assert!(text.contains("sqlite"), "option 2:\n{text}");
    // The binding renderer contract: a free-text affordance every time.
    assert!(
        text.contains("type your own answer"),
        "free-text option:\n{text}"
    );
}

#[test]
fn slash_menu_renders_filtered_commands() {
    let model = SessionModel::new();
    let mut composer = Composer::new();
    for c in "/di".chars() {
        composer.insert_char(c);
    }
    let mut ui = UiState::new(
        composer,
        vec![
            SlashCommand::new("/diff", "open the diff viewer"),
            SlashCommand::new("/files", "focus files"),
        ],
    );
    let text = draw(&model, &mut ui, 100, 30);
    assert!(text.contains("/diff"), "slash menu lists /diff:\n{text}");
    assert!(!text.contains("/files"), "filtered out /files:\n{text}");
}

#[test]
fn slash_popup_marks_the_selected_command() {
    let model = SessionModel::new();
    let mut composer = Composer::new();
    composer.insert_char('/');
    let mut ui = UiState::new(
        composer,
        vec![
            SlashCommand::new("/help", "show help"),
            SlashCommand::new("/diff", "open the diff viewer"),
        ],
    );
    ui.slash_selected = 1;
    let text = draw(&model, &mut ui, 100, 30);
    // The glyph is double-width: its trailing filler cell dumps as an
    // extra space before the explicit separator.
    assert!(
        text.contains("▸ 🔒  /diff"),
        "selection marker + builtin glyph on the second row:\n{text}"
    );
    assert!(
        text.contains("/ commands · 2"),
        "popup title with the match count:\n{text}"
    );
}

#[test]
fn slash_popup_glyphs_distinguish_builtin_from_custom_commands() {
    let model = SessionModel::new();
    let mut composer = Composer::new();
    composer.insert_char('/');
    let mut ui = UiState::new(
        composer,
        vec![
            SlashCommand::new("/help", "show help"),
            SlashCommand::custom("/fix-bug", "fix the named bug"),
        ],
    );
    let text = draw(&model, &mut ui, 100, 30);
    assert!(
        text.contains("🔒  /help"),
        "productized commands carry the lock glyph:\n{text}"
    );
    assert!(
        text.contains("⚡  /fix-bug"),
        "custom commands carry the lightning glyph:\n{text}"
    );
}

// ---- Slash-popup windowing ---------------------------------------------

#[test]
fn scroll_window_start_holds_still_until_the_selection_leaves_the_edge() {
    // Fits entirely: never scrolls.
    assert_eq!(scroll_window_start(5, 4, 8), 0);
    // Selection inside the first window: no movement.
    assert_eq!(scroll_window_start(20, 0, 8), 0);
    assert_eq!(scroll_window_start(20, 7, 8), 0);
    // One past the window's last row: scroll down by one.
    assert_eq!(scroll_window_start(20, 8, 8), 1);
    // The tail clamps so the final window is full, never blank-padded.
    assert_eq!(scroll_window_start(20, 19, 8), 12);
    // Selecting back at the top pulls the window all the way up.
    assert_eq!(scroll_window_start(20, 0, 8), 0);
    // A stale selection past the end (e.g. the filter just shrank the
    // match list) clamps to the last full window instead of panicking.
    assert_eq!(scroll_window_start(20, 999, 8), 12);
    // Degenerate inputs don't panic.
    assert_eq!(scroll_window_start(0, 0, 8), 0);
    assert_eq!(scroll_window_start(5, 0, 0), 0);
}

/// Rendering a slash popup taller than its window keeps the *selected*
/// row on screen and pushes the ones scrolled past off it — the concrete
/// symptom of the un-windowed version (selection navigable but invisible).
#[test]
fn slash_popup_windows_the_selection_into_view() {
    let cmds: Vec<SlashCommand> = (0..15)
        .map(|i| SlashCommand::new(format!("/cmd{i:02}"), "desc"))
        .collect();
    let menu = SlashMenu::filter(&cmds, "/");
    let area = Rect {
        x: 0,
        y: 0,
        width: 56,
        height: (SLASH_POPUP_MAX_ROWS as u16) + 3,
    };
    // Select the very last command: without windowing it renders off the
    // bottom of the popup box and never appears in the buffer.
    let mut buf = Buffer::empty(area);
    render_slash_popup(&menu, 14, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(text.contains("/cmd14"), "selected row is visible:\n{text}");
    assert!(
        !text.contains("/cmd00"),
        "the top rows scrolled out of view:\n{text}"
    );
    // The legend advertises the hidden rows above.
    assert!(text.contains('▲'), "scroll affordance shown:\n{text}");

    // Selecting the top shows the head and hides the tail instead.
    let mut buf = Buffer::empty(area);
    render_slash_popup(&menu, 0, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(text.contains("/cmd00"), "top row visible:\n{text}");
    assert!(!text.contains("/cmd14"), "tail hidden:\n{text}");
    assert!(text.contains('▼'), "hidden-below affordance shown:\n{text}");
}

/// A stale-high selection (the match list shrank under the cursor before
/// the upstream clamp caught up) must still render a sane, in-bounds
/// window rather than panic on the slice.
#[test]
fn slash_popup_survives_a_selection_past_the_filtered_end() {
    let cmds: Vec<SlashCommand> = (0..3)
        .map(|i| SlashCommand::new(format!("/cmd{i:02}"), "desc"))
        .collect();
    let menu = SlashMenu::filter(&cmds, "/");
    let area = Rect {
        x: 0,
        y: 0,
        width: 56,
        height: (SLASH_POPUP_MAX_ROWS as u16) + 3,
    };
    let mut buf = Buffer::empty(area);
    // selected far past the 3 matches — the render-side clamp keeps it in
    // view; all three rows fit so nothing scrolls.
    render_slash_popup(&menu, 99, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(text.contains("/cmd02"), "last row still shown:\n{text}");
    assert!(
        !text.contains('▲') && !text.contains('▼'),
        "short list shows no scroll affordance:\n{text}"
    );
}

#[test]
fn transcript_scrolls_line_exact_to_show_the_tail() {
    let mut model = SessionModel::new();
    for i in 0..200 {
        model.apply(&AgentEvent::Text {
            delta: format!("LINE{i:03}\n"),
        });
    }
    // The trailing streaming text is one entry with 200 embedded newlines
    // → 201 visual lines; following must land on the last of them.
    let mut ui = UiState::default();
    let text = draw(&model, &mut ui, 80, 20);
    assert!(
        text.contains("LINE199"),
        "tail is visible while following:\n{text}"
    );
    assert!(!text.contains("LINE000"), "head is scrolled off:\n{text}");
}

#[test]
fn ui_transcript_cache_invalidates_when_a_file_mutation_stales_an_inline_diff() {
    use stella_protocol::FileChangeKind as FK;
    // A FileChange appends NOTHING to the transcript — every older
    // fingerprint term (entry count, tail lengths, width) holds still —
    // yet it stales a settled tool result's inline diff. Only the
    // file-mutation term can catch it; without it the cache would keep
    // showing a diff the entry no longer owns.
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::ToolStart {
        call: ToolCall {
            call_id: "c1".into(),
            name: "edit_file".into(),
            input: serde_json::json!({"path": "src/x.rs"}),
        },
    });
    model.apply(&AgentEvent::FileChange {
        path: "src/x.rs".into(),
        kind: FK::Modified,
        diff: Some("@@ -1,1 +1,1 @@\n+first_diff_line".into()),
    });
    model.apply(&AgentEvent::ToolResult {
        call_id: "c1".into(),
        output: ToolOutput::Ok {
            content: "ok".into(),
        },
        duration_ms: 3,
        speculated: false,
    });
    let mut ui = UiState::default();
    ui.ensure_transcript_lines(&model, false, 120);
    let text = |lines: &[Line<'_>]| -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect()
    };
    assert!(
        text(ui.transcript_lines()).contains("first_diff_line"),
        "the fresh inline diff renders"
    );

    let len_before = model.transcript.len();
    model.apply(&AgentEvent::FileChange {
        path: "src/x.rs".into(),
        kind: FK::Modified,
        diff: Some("@@ -1,1 +1,1 @@\n+second_diff_line".into()),
    });
    assert_eq!(model.transcript.len(), len_before, "no transcript append");
    ui.ensure_transcript_lines(&model, false, 120);
    let after = text(ui.transcript_lines());
    assert!(
        !after.contains("first_diff_line") && !after.contains("second_diff_line"),
        "the stale diff is dropped, not misattributed: {after}"
    );
}

#[test]
fn ui_memoizes_transcript_lines_and_invalidates_on_a_streaming_delta() {
    // The transcript re-wrap is O(transcript) and ran EVERY frame — a
    // session redraws far more often than it changes. The UiState cache
    // must (a) reuse the parsed lines on an unchanged frame, and (b) still
    // invalidate when a streaming delta grows the trailing entry (its
    // length changes but the entry count does not), or new tokens would
    // never appear.
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::Text {
        delta: "a fairly long assistant message that wraps at a narrow width".into(),
    });
    let mut ui = UiState::default();

    // First frame populates the cache, and it matches a direct render.
    ui.ensure_transcript_lines(&model, false, 40);
    let first = ui.transcript_lines().to_vec();
    let ptr = ui.transcript_lines().as_ptr();
    assert_eq!(first, transcript_lines(&model, false, 40));

    // An unchanged frame reuses the SAME backing allocation — no re-wrap.
    ui.ensure_transcript_lines(&model, false, 40);
    assert_eq!(
        ui.transcript_lines().as_ptr(),
        ptr,
        "an unchanged frame must not rebuild the transcript"
    );

    // A streaming delta coalesces into the trailing Text entry: entry count
    // stays 1, but the tail grows. The cache must rebuild and show it.
    model.apply(&AgentEvent::Text {
        delta: " …and still more streamed text arriving token by token".into(),
    });
    assert_eq!(
        model.transcript.len(),
        1,
        "the delta coalesced, not appended"
    );
    ui.ensure_transcript_lines(&model, false, 40);
    assert_eq!(ui.transcript_lines(), transcript_lines(&model, false, 40));
    assert_ne!(
        ui.transcript_lines(),
        first.as_slice(),
        "a grown trailing entry must produce fresh lines"
    );

    // A width change (a resize) also invalidates.
    let wide = ui.transcript_lines().as_ptr();
    ui.ensure_transcript_lines(&model, false, 20);
    assert_ne!(
        ui.transcript_lines().as_ptr(),
        wide,
        "a wrap-width change must rebuild"
    );
    assert_eq!(ui.transcript_lines(), transcript_lines(&model, false, 20));
}

#[test]
fn streaming_preview_renders_live_and_the_authoritative_text_leaves_no_duplicate() {
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::TextDelta {
        text: "streamed toke".into(),
    });
    model.apply(&AgentEvent::TextDelta {
        text: "ns arriving".into(),
    });
    let mut ui = UiState::default();
    let text = draw(&model, &mut ui, 140, 12);
    assert!(
        text.contains("streamed tokens arriving"),
        "the preview is visible before any Text event lands:\n{text}"
    );

    // A further delta grows the preview WITHOUT changing the entry count
    // — the memoized lines must still invalidate and show it.
    model.apply(&AgentEvent::TextDelta {
        text: " token by token".into(),
    });
    let text = draw(&model, &mut ui, 140, 12);
    assert!(
        text.contains("arriving token by token"),
        "a grown preview must re-render:\n{text}"
    );

    // The step commits: bookkeeping lands, then the authoritative Text
    // replaces the preview — the answer must appear exactly once.
    model.apply(&AgentEvent::BudgetTick {
        spent_usd: 0.01,
        limit_usd: None,
        mode: BudgetMode::Observed,
        session_spent_usd: None,
        session_limit_usd: None,
    });
    model.apply(&AgentEvent::Text {
        delta: "streamed tokens arriving token by token".into(),
    });
    let text = draw(&model, &mut ui, 140, 12);
    assert_eq!(
        text.matches("streamed tokens arriving token by token")
            .count(),
        1,
        "replaced, never duplicated:\n{text}"
    );
}

#[test]
fn a_panicking_panel_becomes_an_error_card_and_input_stays_alive() {
    // L-T7: force a panel to panic via a panicking draw closure and prove
    // (a) it renders as a visible error card, (b) a sibling panel still
    // renders normally, and (c) the pure input path still processes keys.
    let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
    terminal
        .draw(|f| {
            let cols = Layout::horizontal([Constraint::Percentage(50); 2]).split(f.area());
            guarded_panel(f, cols[0], "boom", |_buf| panic!("kaboom in a panel"));
            guarded_panel(f, cols[1], "ok", |buf| {
                Paragraph::new("still-alive")
                    .block(Block::default().borders(Borders::ALL))
                    .render(cols[1], buf);
            });
        })
        .unwrap();
    let text = buffer_text(terminal.backend().buffer());
    assert!(text.contains("panicked"), "error card is visible:\n{text}");
    assert!(
        text.contains("kaboom"),
        "carries the panic message:\n{text}"
    );
    assert!(
        text.contains("still-alive"),
        "sibling panel unaffected:\n{text}"
    );

    // Input handling is entirely independent of rendering and keeps
    // working — the app did not die.
    use crate::ui::{ShellAction, handle_key};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let model = SessionModel::new();
    let mut ui = UiState::default();
    let action = handle_key(
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
        &model,
        &mut ui,
    );
    assert_eq!(action, ShellAction::Handled);
    assert_eq!(ui.composer.buffer(), "z");
}

#[test]
fn tool_cards_and_verdicts_style_content_deterministically() {
    let mut model = SessionModel::new();
    model.apply(&AgentEvent::ToolStart {
        call: ToolCall {
            call_id: "c1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "x"}),
        },
    });
    model.apply(&AgentEvent::ToolResult {
        call_id: "c1".into(),
        output: ToolOutput::Error {
            message: "not found".into(),
        },
        duration_ms: 12,
        speculated: false,
    });
    let lines = transcript_lines(&model, false, 0);
    let joined: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
        .collect();
    assert!(joined.contains("read_file"));
    assert!(joined.contains("not found"));
    assert!(joined.contains("12ms"));
    // The result row resolves its tool name from the call so the two
    // rows read as an aligned pair.
    assert!(
        joined.contains("[✗ read_file]"),
        "result labels itself with its tool: {joined}"
    );
}

/// Expanded (ctrl+o) detail rows — full tool output and pretty-printed
/// call args — align at the content column exactly like their parent
/// row's content, not at the left margin: the transcript's two-column
/// layout must hold for the hidden rows too.
#[test]
fn expanded_detail_rows_align_at_the_content_column() {
    let indent = " ".repeat(LABEL_COL);
    let line_text =
        |l: &Line<'_>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };

    let mut result_rows = Vec::new();
    entry_lines(
        &TranscriptEntry::ToolResult {
            call_id: "c1".into(),
            name: "grep".into(),
            ok: true,
            summary: "hit".into(),
            full: "src/a.rs:1: hit\nsrc/b.rs:2: hit".into(),
            duration_ms: 5,
            speculated: false,
            diff: None,
        },
        &[],
        false,
        true,
        120,
        &mut result_rows,
    );
    let details: Vec<String> = result_rows.iter().skip(1).map(line_text).collect();
    assert_eq!(details.len(), 2, "both output lines render");
    for d in &details {
        assert!(
            d.starts_with(&indent) && !d.starts_with(&format!("{indent} ")),
            "detail row starts exactly at LABEL_COL: {d:?}"
        );
    }

    let mut start_rows = Vec::new();
    entry_lines(
        &TranscriptEntry::ToolStart {
            call_id: "c1".into(),
            name: "grep".into(),
            input: "pattern".into(),
            raw: r#"{"pattern":"hit"}"#.into(),
            path: None,
        },
        &[],
        false,
        true,
        120,
        &mut start_rows,
    );
    assert!(
        start_rows
            .iter()
            .skip(1)
            .all(|l| line_text(l).starts_with(&indent)),
        "expanded call args align at the content column"
    );
}

// ---- Inline transcript diffs (mutating tool results) ----

/// A successful mutation's result entry, plus the file state its
/// [`InlineDiffRef`] resolves against: a 15-line Rust diff (1 hunk header
/// + 14 additions) whose freshness seq matches.
fn mutation_entry_and_files() -> (TranscriptEntry, Vec<FileState>) {
    let body: String = (1..=14).map(|i| format!("+let x{i} = {i};\n")).collect();
    let diff_text = format!("@@ -0,0 +1,14 @@\n{body}");
    let entry = TranscriptEntry::ToolResult {
        call_id: "c1".into(),
        name: "edit_file".into(),
        ok: true,
        summary: "ok".into(),
        full: "ok".into(),
        duration_ms: 7,
        speculated: false,
        diff: Some(InlineDiffRef {
            path: "src/x.rs".into(),
            seq: 1,
        }),
    };
    let files = vec![FileState {
        path: "src/x.rs".into(),
        kind: FileChangeKind::Modified,
        latest_diff: Some(diff_text),
        changes: 1,
        reads: 0,
    }];
    (entry, files)
}

fn flat_text(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn collapsed_tool_result_shows_a_capped_syntax_highlighted_inline_diff() {
    let (entry, files) = mutation_entry_and_files();
    let mut out = Vec::new();
    entry_lines(&entry, &files, false, false, 120, &mut out);
    let text = flat_text(&out);

    // The header rule carries the path; the footer carries the counts.
    assert!(text.contains("── src/x.rs"), "path rule present:\n{text}");
    assert!(text.contains("+14 additions"), "footer counts:\n{text}");
    // Capped at INLINE_DIFF_CAP styled diff lines: the hunk header plus
    // the first 9 additions — the 10th addition is folded away.
    assert!(text.contains("@@ -0,0 +1,14 @@"), "hunk header:\n{text}");
    assert!(text.contains("+let x9 = 9;"), "9th addition shown:\n{text}");
    assert!(
        !text.contains("+let x10 = 10;"),
        "the 11th diff line is folded behind ctrl+o:\n{text}"
    );
    assert!(
        text.contains("⋯ +5 more diff lines · ctrl+o opens the full diff"),
        "the fold names the hidden count and the key:\n{text}"
    );
    // Line-level standard: added lines are numbered on the new side.
    assert!(text.contains("   1 +let x1 = 1;"), "gutter number:\n{text}");
    // Syntax colors ride the path's language (`.rs` → Rust keywords).
    let kw = out
        .iter()
        .flat_map(|l| l.spans.iter())
        .find(|s| s.content == "let")
        .expect("`let` is its own syntax span");
    assert_eq!(kw.style.fg, Some(theme::SYNTAX_KEYWORD));
}

#[test]
fn expanded_tool_result_shows_the_full_inline_diff() {
    let (entry, files) = mutation_entry_and_files();
    let mut out = Vec::new();
    entry_lines(&entry, &files, false, true, 120, &mut out);
    let text = flat_text(&out);
    assert!(
        text.contains("+let x14 = 14;"),
        "ctrl+o reveals every diff line:\n{text}"
    );
    assert!(
        !text.contains("more diff lines"),
        "no fold hint once expanded:\n{text}"
    );
    assert!(
        text.contains("+14 additions"),
        "footer still closes:\n{text}"
    );
}

#[test]
fn a_stale_or_unresolvable_diff_ref_renders_no_inline_diff() {
    let (entry, mut files) = mutation_entry_and_files();
    // A later mutation bumped the path's change counter past the seq the
    // result recorded — showing the newer diff under this ✓ would
    // attribute a change the call never made.
    files[0].changes = 2;
    let mut out = Vec::new();
    entry_lines(&entry, &files, false, false, 120, &mut out);
    let text = flat_text(&out);
    assert!(
        !text.contains("── src/x.rs"),
        "stale ref hides the diff:\n{text}"
    );
    assert!(!text.contains("@@"), "no diff body either:\n{text}");

    // A ref whose path is no longer tracked resolves to nothing at all.
    let mut out = Vec::new();
    entry_lines(&entry, &[], false, false, 120, &mut out);
    assert!(
        !flat_text(&out).contains("@@"),
        "unknown path renders no diff"
    );
}

#[test]
fn eviction_marker_renders_as_a_one_line_system_note() {
    let mut out = Vec::new();
    entry_lines(
        &TranscriptEntry::Evicted { count: 1234 },
        &[],
        false,
        false,
        80,
        &mut out,
    );
    assert_eq!(out.len(), 1, "the marker costs exactly one visual row");
    let text: String = out[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "… 1234 earlier entries evicted");
}

// ---- Transcript prefix colors (ember gutter + violet user prompt) ----

/// The user prompt is the single exception to the ember gutter: its
/// `[user]:` tag AND every line of the prompt render in exactly the violet
/// used for the composer's keybind glyphs and the deterministic-first
/// chip — as one flat color, with no markdown tinting and no ember heat.
#[test]
fn user_prompt_entry_is_one_violet_color_end_to_end() {
    let mut out = Vec::new();
    // Markdown that WOULD tint under `markdown::render` (a code span goes
    // `theme::WARN`, a heading goes bold `INK`): proof none of it leaks.
    entry_lines(
        &TranscriptEntry::User("fix the `parser` bug\nand **ship** it".to_string()),
        &[],
        false,
        false,
        80,
        &mut out,
    );
    let mut saw_text = false;
    for line in &out {
        for span in &line.spans {
            // Skip pure-whitespace gutter/continuation indent (no fg).
            if span.content.trim().is_empty() {
                continue;
            }
            saw_text = true;
            assert_eq!(
                span.style.fg,
                Some(theme::VIOLET),
                "user entry span {:?} is not the violet accent",
                span.content
            );
            // Belt and suspenders: no ember heat anywhere on the entry.
            for banned in [theme::AURORA_CYAN, theme::AURORA_MAGENTA, theme::WARN] {
                assert_ne!(
                    span.style.fg,
                    Some(banned),
                    "user entry leaks an ember color: {:?}",
                    span.content
                );
            }
        }
    }
    assert!(
        saw_text,
        "the prompt rendered at least one styled text span"
    );
}

/// Every other entry's `[label]:` prefix stays inside the warm ember
/// family — failure crimson, success gold — so no raw ANSI cyan/blue/
/// magenta survives from before the ember theme landed.
#[test]
fn transcript_prefix_colors_stay_in_the_ember_family() {
    let prefix_fg = |entry: &TranscriptEntry| -> Option<Color> {
        let mut out = Vec::new();
        entry_lines(entry, &[], false, false, 80, &mut out);
        out[0].spans[0].style.fg
    };
    assert_eq!(
        prefix_fg(&TranscriptEntry::Error {
            message: "boom".into(),
            retryable: false,
        }),
        Some(theme::AURORA_MAGENTA),
        "error prefix is crimson",
    );
    assert_eq!(
        prefix_fg(&TranscriptEntry::ToolResult {
            call_id: "c1".into(),
            name: "read_file".into(),
            ok: true,
            summary: "ok".into(),
            full: "ok".into(),
            duration_ms: 3,
            speculated: false,
            diff: None,
        }),
        Some(theme::AURORA_CYAN),
        "successful tool-result prefix is gold",
    );
    assert_eq!(
        prefix_fg(&TranscriptEntry::ToolResult {
            call_id: "c2".into(),
            name: "read_file".into(),
            ok: false,
            summary: "no".into(),
            full: "no".into(),
            duration_ms: 3,
            speculated: false,
            diff: None,
        }),
        Some(theme::AURORA_MAGENTA),
        "failed tool-result prefix is crimson",
    );
    // The stage marker moved off raw cyan onto ember flame.
    assert_eq!(
        prefix_fg(&TranscriptEntry::Stage(StageKind::Execute)),
        Some(theme::AURORA_AZURE),
        "stage prefix is ember flame",
    );
}

// ---- Replay determinism (L-T1) ------------------------------------

/// A small event strategy over a representative spread of variants.
fn any_event() -> impl Strategy<Value = AgentEvent> {
    prop_oneof![
        "[a-z ]{0,12}".prop_map(|delta| AgentEvent::Text { delta }),
        "[a-z ]{0,12}".prop_map(|text| AgentEvent::TextDelta { text }),
        any::<u8>().prop_map(|n| AgentEvent::Stage {
            name: match n % 4 {
                0 => StageKind::Triage,
                1 => StageKind::Plan,
                2 => StageKind::Execute,
                _ => StageKind::Verify,
            },
        }),
        ("[a-z/.]{1,10}", any::<bool>()).prop_map(|(path, created)| AgentEvent::FileChange {
            path,
            kind: if created {
                FileChangeKind::Created
            } else {
                FileChangeKind::Modified
            },
            diff: Some("@@\n-a\n+b".into()),
        }),
        (any::<f64>(), any::<f64>()).prop_map(|(a, b)| AgentEvent::BudgetTick {
            spent_usd: a.abs() % 10.0,
            limit_usd: Some(b.abs() % 10.0),
            mode: BudgetMode::Observed,
            session_spent_usd: None,
            session_limit_usd: None,
        }),
        Just(AgentEvent::Complete {
            model: "glm".into(),
            cost_usd: 0.01,
        }),
    ]
}

proptest! {
    /// The core L-T1 guarantee: folding the same event vector into two
    /// fresh models and rendering both yields byte-identical backing cell
    /// buffers. State derived from the log cannot drift.
    #[test]
    fn replaying_a_log_renders_identical_buffers(events in prop::collection::vec(any_event(), 0..40)) {
        let mut a = UiState::default();
        let mut b = UiState::default();
        let model_a = SessionModel::replay(&events);
        let model_b = SessionModel::replay(&events);

        let mut ta = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let mut tb = Terminal::new(TestBackend::new(90, 24)).unwrap();
        ta.draw(|f| render(&model_a, &mut a, f)).unwrap();
        tb.draw(|f| render(&model_b, &mut b, f)).unwrap();

        prop_assert_eq!(
            buffer_rows(ta.backend().buffer()),
            buffer_rows(tb.backend().buffer())
        );
    }
}
