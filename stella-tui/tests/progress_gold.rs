//! Witness: the run progress bar's fill must be GOLD.
//!
//! Goal under test: "color the progress bar gold". Today the determinate fill
//! rides the cool aurora gradient (cyan → azure → violet) — every stop is
//! blue-dominant (`b > r`). Gold is a warm hue (`r > b`). This test renders a
//! live (Running) bar through the public `progress::render` entry point and
//! asserts every painted fill cell (`█`) carries a gold foreground.
//!
//! It fails now (the fill is blue) and passes once the fill is recolored gold.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use stella_protocol::{AgentEvent, StageKind};
use stella_tui::theme::ColorMode;
use stella_tui::{AgentMeta, DeckUi, Inbound, WorkspaceModel};

/// A workspace with one lead agent mid-run (Execute stage → ~50% fill), so the
/// bar is in its `Running` state and paints a substantial gold-eligible fill.
fn running_model() -> WorkspaceModel {
    let mut m = WorkspaceModel::new();
    m.now_ms = 10_000;
    m.apply_inbound(&Inbound::Register(AgentMeta::new("lead", "goal", 0)));
    m.apply_inbound(&Inbound::Event {
        agent: "lead".into(),
        event: AgentEvent::Stage {
            name: StageKind::Execute,
        },
    });
    m
}

#[test]
fn progress_bar_fill_is_gold_not_blue() {
    let model = running_model();

    let mut ui = DeckUi::default();
    ui.focused = 0;
    // Truecolor is the one mode where the per-cell fill color is emitted
    // verbatim (lesser terminals collapse to a single solid token); freeze
    // motion so the frame is deterministic.
    ui.color_mode = ColorMode::Truecolor;
    ui.no_anim = true;

    let width: u16 = 60;
    let area = Rect::new(0, 0, width, 1);
    let mut buf = Buffer::empty(area);

    stella_tui::progress::render(&model, &ui, area, &mut buf);

    // Scan the row for the fill glyph. Only the determinate track paints `█`;
    // labels/telemetry/notches use other glyphs, so this isolates the fill.
    let mut fill_cells = 0usize;
    for x in 0..width {
        let Some(cell) = buf.cell((x, 0)) else {
            continue;
        };
        if cell.symbol() != "█" {
            continue;
        }
        fill_cells += 1;
        match cell.fg {
            Color::Rgb(r, _g, b) => {
                assert!(
                    i32::from(r) > i32::from(b),
                    "fill cell at x={x} is not gold: {:?} (gold is warm, r must exceed b)",
                    cell.fg
                );
            }
            other => panic!("fill cell at x={x} is not an RGB gold color: {other:?}"),
        }
    }

    assert!(
        fill_cells > 0,
        "expected the running progress bar to paint fill cells to inspect"
    );
}
