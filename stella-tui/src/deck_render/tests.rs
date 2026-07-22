#![allow(clippy::field_reassign_with_default)]

use super::*;
use crate::envelope::{AgentMeta, Inbound};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use stella_protocol::{AgentEvent, StageKind};

/// End-to-end: render the whole deck frame (tab bar · view · progress bar ·
/// composer · footer · statline) and assert every §-level element is
/// present, the rocket/garble is gone, and nothing panics at 80 columns.
/// Run with `--nocapture` to eyeball the frame.
#[test]
fn full_deck_frame_composes_every_band_at_80_cols() {
    let mut model = running_model_with_queue();
    if let Some(a) = model.agents.first_mut() {
        a.tokens_out = 900;
        a.tokens_in = 42_000;
        a.cost_usd = 0.14;
        a.meta.started_ms = 0;
    }
    model.now_ms = 10_000;
    model.apply_inbound(&Inbound::Event {
        agent: "lead".into(),
        event: AgentEvent::Text {
            delta: "Found the root cause.".into(),
        },
    });

    let mut ui = DeckUi::default();
    ui.splash.skip(); // the splash owns the frame until done
    for c in "add nav".chars() {
        ui.composer.insert_char(c);
    }

    // 190 (was 160, before that 120) so the now-wider statline (CACHE +
    // PIPELINE, then SAVED + WARMTH for #267/#269) still has room for the
    // ethos chip, which is dropped first.
    for (w, h) in [(80u16, 24u16), (190, 40)] {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_deck(&model, &mut ui, f)).unwrap();
        let text = buffer_text(term.backend().buffer());
        if w == 80 {
            eprintln!("\n──── deck @ {w}×{h} ────\n{text}\n");
        }
        for needle in [
            ">>>",      // gold prompt prefix (§4)
            "add nav",  // typed prompt text
            "new line", // composer footer affordance (§4)
            "queue",    // footer / queue status
            "plan",     // progress stage labels (§3)
            "execute", "verify", "stella",  // statline brand (§5)
            "CONTEXT", // load-bearing token meter, kept at every width (§5)
        ] {
            assert!(
                text.contains(needle),
                "deck @{w}×{h} missing {needle:?}:\n{text}"
            );
        }
        // The ethos chip is chrome — it only needs to appear once the row is
        // wide enough (it is the first thing dropped on a narrow statline).
        if w >= 120 {
            assert!(
                text.contains("deterministic-first"),
                "deck @{w}×{h} missing ethos chip:\n{text}"
            );
        }
        // The killed rocket/garble leave no trace.
        assert!(
            !text.contains("}=>"),
            "rocket sprite still rendered:\n{text}"
        );
        assert!(!text.contains("<●>"), "UFO sprite still rendered:\n{text}");
    }
}

fn buffer_text(buf: &Buffer) -> String {
    let area = *buf.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn full_deck_frame_grows_a_third_statline_row_for_a_diagnosed_agent() {
    // The acceptance case: an opt-in provider (anthropic), 4 calls (past
    // MIN_TURNS=3), 0% hit rate, 0 cache writes — the marker never
    // engaged. `render_deck` must reserve the statline's third row and
    // `render_status_bar` must fill it with the full-sentence hint, not a
    // clipped fragment.
    let mut model = running_model_with_queue();
    for step in 1..=4usize {
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::StepUsage {
                step,
                role: stella_protocol::event::ModelCallRole::Worker,
                provider: "anthropic".into(),
                model: "claude-fable-5".into(),
                input_tokens: 10_000,
                output_tokens: 500,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                estimated_input_tokens: 0,
                cost_usd: 0.05,
                duration_ms: 100,
                retries: 0,
                tool_calls: 0,
                complete: true,
            },
        });
    }
    model.apply_inbound(&Inbound::CacheInsight {
        agent: "lead".into(),
        savings_usd_delta: 0.0,
        ttl_secs: 300,
        is_opt_in_provider: true,
    });

    let mut ui = DeckUi::default();
    ui.splash.skip();
    let mut term = Terminal::new(TestBackend::new(190, 40)).unwrap();
    term.draw(|f| render_deck(&model, &mut ui, f)).unwrap();
    let text = buffer_text(term.backend().buffer());
    assert!(
        text.contains("cache opt-in never engaged"),
        "diagnosis row missing from the full frame:\n{text}"
    );

    // A healthy session (no StepUsage at all) never grows the row or
    // shows the sentence — the common case stays the compact two rows.
    let healthy = running_model_with_queue();
    let mut healthy_ui = DeckUi::default();
    healthy_ui.splash.skip();
    let mut healthy_term = Terminal::new(TestBackend::new(190, 40)).unwrap();
    healthy_term
        .draw(|f| render_deck(&healthy, &mut healthy_ui, f))
        .unwrap();
    let healthy_text = buffer_text(healthy_term.backend().buffer());
    assert!(
        !healthy_text.contains("cache opt-in never engaged")
            && !healthy_text.contains("prompt prefix is unstable"),
        "a healthy session must not show a diagnosis:\n{healthy_text}"
    );
}

#[test]
fn trace_strip_shows_a_rule_and_the_newest_trace_entry() {
    let mut model = running_model_with_queue();
    model.apply_inbound(&Inbound::Event {
        agent: "lead".into(),
        event: AgentEvent::Text {
            delta: "Found the root cause.".into(),
        },
    });
    let area = Rect::new(0, 0, 60, 2);
    let mut buf = Buffer::empty(area);
    render_trace_strip(&model, area, &mut buf);
    let text = buffer_text(&buf);
    let rows: Vec<&str> = text.lines().collect();
    assert!(
        rows[0].starts_with("──"),
        "hairline rule on top: {:?}",
        rows[0]
    );
    assert!(rows[1].contains("text"), "kind label shown:\n{text}");
    assert!(
        rows[1].contains("Found the root cause."),
        "newest entry summarized:\n{text}"
    );

    // No trace yet → a quiet idle line, never a panic.
    let empty = WorkspaceModel::new();
    let mut buf = Buffer::empty(area);
    render_trace_strip(&empty, area, &mut buf);
    assert!(buffer_text(&buf).contains("no activity yet"));
}

fn running_model_with_queue() -> WorkspaceModel {
    let mut m = WorkspaceModel::new();
    m.now_ms = 1_000;
    m.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "goal", 0)));
    m.apply_inbound(&Inbound::Event {
        agent: "lead".into(),
        event: AgentEvent::Stage {
            name: StageKind::Execute,
        },
    });
    m.queue.enqueue("write the tests".into(), 1);
    m.queue.enqueue("open a pr".into(), 2);
    m
}

#[test]
fn empty_composer_is_a_single_gold_prompt_line_with_the_caret() {
    let ui = DeckUi::default(); // blank composer
    let layout = crate::composer::layout(&ui.composer, 40);
    let area = Rect::new(0, 0, 40, 4);
    let mut buf = Buffer::empty(area);
    render_composer(&ui, &layout, 0, area, &mut buf);
    let text = buffer_text(&buf);
    let rows: Vec<&str> = text.lines().collect();
    assert!(
        rows[0].starts_with(">>> "),
        "row 0 is the gold prompt: {:?}",
        rows[0]
    );
    // Exactly one prompt line — the rest of the box is empty.
    assert!(
        rows[1..].iter().all(|r| !r.contains(">>>")),
        "only one prompt line:\n{text}"
    );
    // The caret sits right after the prefix (a reversed cell at col 4).
    assert!(
        buf.cell((4, 0))
            .is_some_and(|c| c.modifier.contains(Modifier::REVERSED)),
        "caret right after the prefix"
    );
}

#[test]
fn a_multiline_paste_prefixes_every_row_and_scrolls_instead_of_chipping() {
    // 8 lines — well past the old 6-line chip threshold, but the deck's
    // composer keeps it inline (one `>>>` per line) and scrolls, rather
    // than collapsing it to a `[pasted: N lines]` chip (acceptance §4/#5).
    let mut ui = DeckUi::default();
    let paste: String = (1..=8).map(|n| format!("line{n}\n")).collect();
    ui.composer.paste(&paste);
    assert!(
        ui.composer.chips().is_empty(),
        "no chip — rendered per line"
    );

    let layout = crate::composer::layout(&ui.composer, 40);
    let area = Rect::new(0, 0, 40, 4); // capped at DECK_COMPOSER_MAX_ROWS
    let mut buf = Buffer::empty(area);
    render_composer(&ui, &layout, 0, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(!text.contains("[pasted"), "not chipped:\n{text}");
    for (i, row) in text.lines().enumerate() {
        assert!(row.starts_with(">>> "), "row {i} prefixed: {row:?}");
    }
    // Beyond 4 rows the box scrolls — the gutter shows a violet thumb.
    assert!(
        (0..area.height).any(|yy| buf
            .cell((area.width - 1, yy))
            .is_some_and(|c| c.symbol() == "▐")),
        "scroll indicator present:\n{text}"
    );
}

#[test]
fn composer_footer_shows_keys_line_counter_and_queue_status() {
    let model = running_model_with_queue();
    let ui = DeckUi::default();
    let layout = crate::composer::layout(&ui.composer, 40);
    let area = Rect::new(0, 0, 100, 1);
    let mut buf = Buffer::empty(area);
    render_composer_footer(&model, &ui, &layout, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(text.contains("new line"), "newline affordance:\n{text}");
    assert!(
        text.contains("queue (never blocks)"),
        "queue affordance:\n{text}"
    );
    assert!(text.contains("1 line"), "live line counter:\n{text}");
    assert!(
        text.contains("2 queued"),
        "queue status on the right:\n{text}"
    );
}

#[test]
fn composer_footer_reports_a_held_queue() {
    let model = running_model_with_queue();
    let mut ui = DeckUi::default();
    ui.dispatch_held = true;
    let layout = crate::composer::layout(&ui.composer, 40);
    let area = Rect::new(0, 0, 100, 1);
    let mut buf = Buffer::empty(area);
    render_composer_footer(&model, &ui, &layout, area, &mut buf);
    assert!(buffer_text(&buf).contains("2 held"), "held status shown");
}

#[test]
fn statline_shows_labeled_cells_and_the_ethos_chip() {
    // A wide terminal fits every cell plus the (chrome) ethos chip. 160
    // (was 120) leaves room for the CACHE + PIPELINE cells added to the row.
    let model = running_model_with_queue();
    let ui = DeckUi::default();
    let area = Rect::new(0, 0, 160, 2);
    let mut buf = Buffer::empty(area);
    render_status_bar(&model, &ui, area, &mut buf);
    let text = buffer_text(&buf);
    for needle in [
        "stella",
        "AGENT",
        "lead",
        "STAGE",
        "execute",
        "MODEL",
        "CPU",
        "CONTEXT",
        "SPEND",
        "CACHE",
        "ENGINE",
        "PIPELINE",
        "deterministic-first",
    ] {
        assert!(
            text.contains(needle),
            "statline missing {needle:?}:\n{text}"
        );
    }
}

#[test]
fn statline_keeps_the_context_meter_on_a_narrow_terminal() {
    // At 80 columns the row must still render without panicking and keep
    // the brand, the stage, and the load-bearing context meter — the ethos
    // chip (pure chrome) is what gives way, not the data.
    let model = running_model_with_queue();
    let ui = DeckUi::default();
    let area = Rect::new(0, 0, 80, 2);
    let mut buf = Buffer::empty(area);
    render_status_bar(&model, &ui, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(text.contains("stella"), "brand kept:\n{text}");
    assert!(text.contains("STAGE"), "stage kept:\n{text}");
    assert!(text.contains("CONTEXT"), "context meter kept:\n{text}");
}

/// One committed model call, carrying `input`/`cached` usage — the fold
/// that feeds the CACHE cell.
fn step_usage(input: u64, cached: u64) -> AgentEvent {
    AgentEvent::StepUsage {
        step: 1,
        role: stella_protocol::ModelCallRole::Worker,
        provider: "test".into(),
        model: "glm".into(),
        input_tokens: input,
        output_tokens: 0,
        cached_input_tokens: cached,
        cache_write_tokens: 0,
        estimated_input_tokens: 0,
        cost_usd: 0.0,
        duration_ms: 1,
        retries: 0,
        tool_calls: 0,
        complete: true,
    }
}

/// The running model plus one metered step with the given cache usage.
fn model_with_cache(input: u64, cached: u64) -> WorkspaceModel {
    let mut m = running_model_with_queue();
    m.apply_inbound(&Inbound::Event {
        agent: "lead".into(),
        event: step_usage(input, cached),
    });
    m
}

#[test]
fn statline_cache_box_sits_after_spend_and_before_engine() {
    let model = model_with_cache(1_000, 500);
    let ui = DeckUi::default();
    let area = Rect::new(0, 0, 200, 2);
    let mut buf = Buffer::empty(area);
    render_status_bar(&model, &ui, area, &mut buf);
    let text = buffer_text(&buf);
    let pos = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("missing {needle:?}:\n{text}"))
    };
    assert!(pos("SPEND") < pos("CACHE"), "CACHE after SPEND:\n{text}");
    assert!(pos("CACHE") < pos("ENGINE"), "CACHE before ENGINE:\n{text}");
    assert!(
        pos("ENGINE") < pos("PIPELINE"),
        "PIPELINE after ENGINE:\n{text}"
    );
}

#[test]
fn statline_cache_box_is_a_dash_before_any_usage() {
    // No StepUsage metered yet → the CACHE cell shows the no-data dash and
    // never divides by zero (the render below must not panic).
    let model = running_model_with_queue();
    let ui = DeckUi::default();
    let area = Rect::new(0, 0, 200, 2);
    let mut buf = Buffer::empty(area);
    render_status_bar(&model, &ui, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(text.contains("CACHE"), "cache label still present:\n{text}");
    assert!(
        !text.contains("tokens"),
        "no token counts before any usage:\n{text}"
    );
}

/// One `Pr` event folded onto the running model.
fn model_with_pr(number: Option<u64>, ci: Option<stella_protocol::CiStatus>) -> WorkspaceModel {
    let mut m = running_model_with_queue();
    m.apply_inbound(&Inbound::Event {
        agent: "lead".into(),
        event: AgentEvent::Pr {
            url: "https://github.com/x/y/pull/183".into(),
            status: stella_protocol::PrStatus::Open,
            number,
            ci,
        },
    });
    m
}

#[test]
fn statline_has_no_pr_cell_before_any_pr_event() {
    let model = running_model_with_queue();
    let ui = DeckUi::default();
    let area = Rect::new(0, 0, 200, 2);
    let mut buf = Buffer::empty(area);
    render_status_bar(&model, &ui, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(
        !text.contains("PR") && !text.contains('⇢'),
        "no PR cell before a Pr event:\n{text}"
    );
}

#[test]
fn statline_pr_cell_shows_number_status_and_a_bold_failing_cross() {
    let model = model_with_pr(Some(183), Some(stella_protocol::CiStatus::Failing));
    let ui = DeckUi::default();
    let area = Rect::new(0, 0, 200, 2);
    let mut buf = Buffer::empty(area);
    render_status_bar(&model, &ui, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(text.contains("PR"), "PR label present:\n{text}");
    assert!(
        text.contains("⇢ #183 open"),
        "number + status in the cell:\n{text}"
    );
    assert!(text.contains('✗'), "failing CI shows the cross:\n{text}");
}

#[test]
fn statline_pr_cell_falls_back_to_the_url_tail_without_a_number() {
    let model = model_with_pr(None, Some(stella_protocol::CiStatus::Passing));
    let ui = DeckUi::default();
    let area = Rect::new(0, 0, 200, 2);
    let mut buf = Buffer::empty(area);
    render_status_bar(&model, &ui, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(
        text.contains("⇢ 183 open"),
        "the URL tail identifies the PR when no number parsed:\n{text}"
    );
    assert!(text.contains('✓'), "passing CI shows the check:\n{text}");
}

#[test]
fn statline_pipeline_box_reads_on_or_off_with_the_mode() {
    let ui = DeckUi::default();
    let area = Rect::new(0, 0, 200, 2);

    let mut off = running_model_with_queue();
    off.pipeline = false;
    let mut buf = Buffer::empty(area);
    render_status_bar(&off, &ui, area, &mut buf);
    let off_text = buffer_text(&buf);
    assert!(off_text.contains("PIPELINE"), "pipeline label:\n{off_text}");
    assert!(
        off_text.contains("OFF"),
        "raw engine loop reads OFF:\n{off_text}"
    );

    let mut on = running_model_with_queue();
    on.pipeline = true;
    let mut buf = Buffer::empty(area);
    render_status_bar(&on, &ui, area, &mut buf);
    let on_text = buffer_text(&buf);
    assert!(
        !on_text.contains("OFF"),
        "staged pipeline is not OFF:\n{on_text}"
    );
    // The CONTEXT *label* also contains "ON", so scope the positive check
    // to the value row (the second rendered line).
    let values = on_text.lines().nth(1).expect("value row");
    assert!(
        values.contains("ON"),
        "staged pipeline reads ON:\n{on_text}"
    );
}

#[test]
fn queue_popup_lists_prompts_and_arms_the_clear_confirm() {
    let model = running_model_with_queue();
    let mut ui = DeckUi::default();
    ui.queue_open = true;
    ui.queue_sel = 1;
    let area = Rect::new(0, 0, 80, 20);
    let mut buf = Buffer::empty(area);
    render_queue_popup(&model, &ui, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(text.contains("queue · 2 pending"), "title:\n{text}");
    assert!(text.contains("write the tests"), "row 1:\n{text}");
    assert!(text.contains("▸ 2. open a pr"), "row 2 selected:\n{text}");
    assert!(text.contains("ctrl+x delete"), "legend:\n{text}");
    // Armed confirm swaps the legend for the warning.
    ui.queue_confirm_clear = true;
    let mut buf2 = Buffer::empty(area);
    render_queue_popup(&model, &ui, area, &mut buf2);
    let warned = buffer_text(&buf2);
    assert!(
        warned.contains("press ctrl+d again"),
        "confirm warning:\n{warned}"
    );
}

/// A `DeckUi` on the Graph tab with an `n`-file snapshot rooted on the
/// middle file, its picker open.
fn ui_with_graph_picker(n: usize) -> DeckUi {
    use crate::graph::{GraphNode, GraphSnapshot};
    let files: Vec<String> = (0..n).map(|i| format!("crate/mod_{i:02}.rs")).collect();
    let focus = files[n / 2].clone();
    let mut ui = DeckUi::default();
    ui.tab = DeckTab::Graph;
    ui.graph = Some(GraphSnapshot {
        focus: focus.clone(),
        nodes: vec![GraphNode {
            label: focus,
            kind: "file".into(),
            location: None,
        }],
        edges: vec![],
        files,
    });
    ui.graph_picker_open = true;
    ui
}

#[test]
fn graph_picker_lists_files_marks_the_current_and_shows_the_legend() {
    let mut ui = ui_with_graph_picker(4);
    ui.graph_picker_sel = 2; // the focus file (crate/mod_02.rs)
    let area = Rect::new(0, 0, 80, 24);
    let mut buf = Buffer::empty(area);
    render_graph_picker(&ui, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(text.contains("files · 4 indexed"), "title:\n{text}");
    assert!(text.contains("filter"), "filter query line:\n{text}");
    assert!(text.contains("crate/mod_00.rs"), "lists files:\n{text}");
    assert!(text.contains("· current"), "marks the rooted file:\n{text}");
    assert!(text.contains("type to filter"), "legend:\n{text}");
}

#[test]
fn graph_picker_windows_the_list_to_keep_the_selection_visible() {
    // Far more files than the popup's rows, selection at the end: the
    // window must slide (via the shared `scroll_window_start`) so the last
    // file shows and the first scrolls out.
    let mut ui = ui_with_graph_picker(60);
    ui.graph_picker_sel = 59;
    let area = Rect::new(0, 0, 80, 24);
    let mut buf = Buffer::empty(area);
    render_graph_picker(&ui, area, &mut buf);
    let text = buffer_text(&buf);
    assert!(
        text.contains("crate/mod_59.rs"),
        "selection scrolled in:\n{text}"
    );
    assert!(
        !text.contains("crate/mod_00.rs"),
        "head scrolled out:\n{text}"
    );
}

#[test]
fn graph_picker_narrows_to_the_filter_query() {
    let mut ui = ui_with_graph_picker(20);
    ui.graph_picker_query = "mod_1".to_string();
    let area = Rect::new(0, 0, 80, 24);
    let mut buf = Buffer::empty(area);
    render_graph_picker(&ui, area, &mut buf);
    let text = buffer_text(&buf);
    // mod_10..mod_19 match; mod_00..mod_09 (except none contain "mod_1")
    // do not.
    assert!(text.contains("crate/mod_10.rs"), "matches shown:\n{text}");
    assert!(
        !text.contains("crate/mod_02.rs"),
        "non-matches hidden:\n{text}"
    );
}
