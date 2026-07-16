//! Full-deck render snapshot: fold the scripted demo scenario into a
//! `WorkspaceModel`, then render every tab through the real `render_deck`
//! entrypoint into a `TestBackend` and assert the expected content appears.
//! Also writes the rendered frames to `deck-snapshots.txt` at the repo root as
//! a human-readable artifact (a text "screenshot" — the honest headless
//! equivalent of a TTY capture).

use std::fmt::Write as _;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use stella_tui::scenario::{demo_graph, demo_inbound};
use stella_tui::{DeckTab, DeckUi, WorkspaceModel, render_deck};

fn folded_model() -> WorkspaceModel {
    let mut model = WorkspaceModel::new();
    model.now_ms = 312_000; // ~5:12 elapsed, so the dashboard timers read nicely
    for inbound in demo_inbound(0, std::process::id()) {
        model.apply_inbound(&inbound);
    }
    model
}

fn render_tab(model: &WorkspaceModel, tab: DeckTab, w: u16, h: u16) -> String {
    let mut ui = DeckUi::default();
    ui.splash.skip(); // past the splash so the tabs draw
    ui.tab = tab;
    ui.graph = Some(demo_graph());
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render_deck(model, &mut ui, f)).unwrap();
    let buf = terminal.backend().buffer();
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
fn deck_renders_every_tab_with_real_content() {
    let model = folded_model();
    assert_eq!(model.agents.len(), 3, "scenario registered 3 agents");

    let cases = [
        (DeckTab::Session, "lead"),
        (DeckTab::Agents, "sub:auth"),
        (DeckTab::Traces, "Which auth guard"),
        (DeckTab::Graph, "run_turn"),
        (DeckTab::Files, "automations"),
    ];
    for (tab, needle) in cases {
        let text = render_tab(&model, tab, 120, 36);
        assert!(
            text.contains(needle),
            "the {tab:?} tab should show {needle:?}, got:\n{text}"
        );
        // The comfy-tabs bar labels are always present — UPPERCASE by the
        // deck's tab-label convention.
        assert!(text.contains("AGENTS"), "tab bar should render on {tab:?}");
    }

    // Write all five tabs to a human-readable artifact at the repo root.
    let mut out = String::new();
    for tab in DeckTab::ALL {
        let _ = writeln!(out, "\n═══ {} tab ═══\n", tab.title());
        let _ = writeln!(out, "{}", render_tab(&model, tab, 150, 32));
    }
    // Best-effort: never fail the test on an artifact write.
    let _ = std::fs::write(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../deck-snapshots.txt"),
        out,
    );
}

#[test]
fn agents_dashboard_shows_status_and_spend_columns() {
    let model = folded_model();
    // The dashboard is dense (11 columns) — render it at a roomy width so the
    // rightmost columns aren't clipped (below ~150 cols the Table gracefully
    // clips its tail rather than panicking).
    let text = render_tab(&model, DeckTab::Agents, 160, 20);
    // Column headers and at least one agent's live status render.
    for needle in ["CPU%", "MEM", "In/Out", "Activity", "needs input"] {
        assert!(
            text.contains(needle),
            "dashboard missing {needle:?}:\n{text}"
        );
    }
}
