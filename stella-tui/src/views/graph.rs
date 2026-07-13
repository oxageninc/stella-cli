//! Graph tab — a visual code-graph neighborhood inspector.
//!
//! Renders exclusively from [`DeckUi::graph`], the out-of-band
//! [`GraphSnapshot`] (see `crate::graph` module docs — it is not folded from
//! the `AgentEvent` log, so this view does not touch `WorkspaceModel` at all).
//!
//! Layout: a selectable node list on the left, colored by [`GraphNode::kind`]
//! via [`theme::graph_kind_color`]; a detail panel on the right for the
//! cursor node showing its human `label` (the primary identifier — a raw id
//! is never shown, per project rule) plus its incident edges rendered as
//! human relations (`imports → serde`, `called by ← driver.rs`), citing the
//! *other* node's label, never an index or id. When there's room, a small
//! spatial node-edge sketch is drawn below the detail panel with
//! `ratatui::widgets::canvas`.

use std::f64::consts::{FRAC_PI_2, TAU};

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use crate::deck::WorkspaceModel;
use crate::deck_ui::DeckUi;
use crate::graph::{GraphNode, GraphSnapshot};
use crate::theme;

pub fn render(model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    // The graph snapshot is a labeled out-of-band read-model (set by the
    // caller/scenario), not folded from the model's event log — see
    // `crate::graph` module docs — so this view has nothing to read off
    // `model`. Keep the parameter to honor the frozen `render` signature
    // shared by every deck view.
    let _ = model;

    let Some(snapshot) = ui.graph.as_ref().filter(|g| !g.is_empty()) else {
        render_empty(area, buf);
        return;
    };

    // Defensive clamp: the deck's key handler (`handle_graph_key`) already
    // keeps `graph_cursor` in range on every keypress, but this view must
    // never index out of bounds regardless of how the cursor got here (a
    // fresh `DeckUi`, a test, a snapshot swapped out from under a stale
    // cursor).
    let cursor = ui.graph_cursor.min(snapshot.nodes.len() - 1);
    ui.graph_cursor = cursor;

    let cols =
        Layout::horizontal([Constraint::Percentage(34), Constraint::Percentage(66)]).split(area);
    render_node_list(snapshot, cursor, cols[0], buf);
    render_right(snapshot, cursor, cols[1], buf);
}

/// The "nothing loaded" state: a centered muted hint, no border chrome beyond
/// the tab's own frame.
fn render_empty(area: Rect, buf: &mut Buffer) {
    let block = Block::default().borders(Borders::ALL).title(" Graph ");
    let inner = block.inner(area);
    block.render(area, buf);

    let line = Line::from(Span::styled(
        "no neighborhood loaded — the code graph appears here",
        theme::muted(),
    ))
    .alignment(Alignment::Center);

    // Vertically center the single line (mirrors the splash's centering idiom
    // — this crate doesn't carry a generic `centered_rect` helper).
    let mid = inner.height / 2;
    let row = Rect {
        x: inner.x,
        y: inner.y + mid,
        width: inner.width,
        height: inner.height.saturating_sub(mid).max(1),
    };
    Paragraph::new(line).render(row, buf);
}

// ---------------------------------------------------------------------------
// Left: node list
// ---------------------------------------------------------------------------

fn render_node_list(snapshot: &GraphSnapshot, cursor: usize, area: Rect, buf: &mut Buffer) {
    let title = format!(" nodes · {} ", snapshot.nodes.len());
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Window the list around the cursor (centered when possible) so the
    // selection can never walk below the viewport into invisible rows — the
    // same keep-in-view slice the Files ledger uses. One row per node (no
    // wrap), so nodes map 1:1 to visible lines and the arithmetic is exact.
    let total = snapshot.nodes.len();
    let visible = inner.height as usize;
    let start = if total <= visible {
        0
    } else {
        cursor
            .saturating_sub(visible.saturating_sub(1) / 2)
            .min(total - visible)
    };
    let end = (start + visible).min(total);

    let lines: Vec<Line<'static>> = snapshot.nodes[start..end]
        .iter()
        .enumerate()
        .map(|(offset, node)| node_list_line(node, start + offset == cursor))
        .collect();
    Paragraph::new(Text::from(lines)).render(inner, buf);
}

fn node_list_line(node: &GraphNode, selected: bool) -> Line<'static> {
    let mut style = Style::new().fg(theme::graph_kind_color(&node.kind));
    if selected {
        style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    }
    let glyph = theme::graph_kind_glyph(&node.kind);
    Line::from(vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(node.label.clone(), style),
    ])
}

// ---------------------------------------------------------------------------
// Right: detail panel (+ bonus sketch when there's room)
// ---------------------------------------------------------------------------

/// Below this height (rows) + width (cols), the spatial sketch is skipped —
/// a cramped canvas reads as noise, not a diagram, and the list+detail view
/// already fully covers the cursor node.
const SKETCH_MIN_HEIGHT: u16 = 10;
const SKETCH_HEIGHT: u16 = 12;
const SKETCH_MIN_WIDTH: u16 = 24;

fn render_right(snapshot: &GraphSnapshot, cursor: usize, area: Rect, buf: &mut Buffer) {
    let show_sketch =
        area.height >= SKETCH_MIN_HEIGHT + SKETCH_HEIGHT && area.width >= SKETCH_MIN_WIDTH;
    if show_sketch {
        let rows =
            Layout::vertical([Constraint::Min(6), Constraint::Length(SKETCH_HEIGHT)]).split(area);
        render_detail(snapshot, cursor, rows[0], buf);
        render_sketch(snapshot, cursor, rows[1], buf);
    } else {
        render_detail(snapshot, cursor, area, buf);
    }
}

fn render_detail(snapshot: &GraphSnapshot, cursor: usize, area: Rect, buf: &mut Buffer) {
    let node = &snapshot.nodes[cursor];
    let title = format!(" {} ", snapshot.focus);

    let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
        node.label.clone(),
        theme::heading(),
    ))];

    let mut meta = vec![Span::styled(
        node.kind.clone(),
        Style::new().fg(theme::graph_kind_color(&node.kind)),
    )];
    if let Some(loc) = &node.location {
        meta.push(Span::styled("  ·  ", theme::muted()));
        meta.push(Span::styled(loc.clone(), theme::muted()));
    }
    lines.push(Line::from(meta));
    lines.push(Line::default());

    let degree = snapshot.degree(cursor);
    lines.push(Line::from(Span::styled(
        format!("relations · {degree}"),
        theme::heading(),
    )));
    if degree == 0 {
        lines.push(Line::from(Span::styled(
            "no known relations",
            theme::muted(),
        )));
    } else {
        for edge in &snapshot.edges {
            let outgoing = edge.from == cursor;
            let incoming = edge.to == cursor;
            if !outgoing && !incoming {
                continue;
            }
            // A self-loop (from == to == cursor) is cited once, outgoing,
            // pointing at the node's own label.
            let other_idx = if outgoing { edge.to } else { edge.from };
            let Some(other) = snapshot.nodes.get(other_idx) else {
                continue;
            };
            lines.push(relation_line(&edge.kind, outgoing, &other.label));
        }
    }

    let block = Block::default().borders(Borders::ALL).title(title);
    Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: true })
        .render(area, buf);
}

/// One relation line: `{kind} → {other}` outgoing, `{passive kind} ← {other}`
/// incoming — always citing the *other* node's human label, never an index.
fn relation_line(kind: &str, outgoing: bool, other_label: &str) -> Line<'static> {
    if outgoing {
        Line::from(Span::styled(
            format!("{kind} → {other_label}"),
            Style::new().fg(theme::OK),
        ))
    } else {
        Line::from(Span::styled(
            format!("{} ← {other_label}", passive(kind)),
            Style::new().fg(theme::RUN),
        ))
    }
}

/// Best-effort passive form of an edge `kind` for incoming relations, e.g.
/// `"imports"` → `"imported by"`, `"calls"` → `"called by"`, `"defines"` →
/// `"defined by"`, `"references"` → `"referenced by"`. Regular-verb heuristic
/// (strip a trailing `s`, add `"d"` if the stem ends in `e` else `"ed"`);
/// covers every documented `GraphEdge::kind` and degrades to a readable
/// (if not always grammatical) label for an unrecognized one.
fn passive(kind: &str) -> String {
    let stem = kind.strip_suffix('s').unwrap_or(kind);
    let past = if stem.ends_with('e') {
        format!("{stem}d")
    } else {
        format!("{stem}ed")
    };
    format!("{past} by")
}

// ---------------------------------------------------------------------------
// Bonus: a small spatial node-edge sketch
// ---------------------------------------------------------------------------

/// Roughly compensates for terminal cells being taller than they are wide, so
/// nodes placed on a unit circle read as a ring rather than a tall ellipse.
const X_BOUNDS: [f64; 2] = [-1.7, 1.7];
const Y_BOUNDS: [f64; 2] = [-1.1, 1.1];

fn render_sketch(snapshot: &GraphSnapshot, cursor: usize, area: Rect, buf: &mut Buffer) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" neighborhood ");
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.width < 6 || inner.height < 4 {
        return; // too small to sketch legibly; detail panel above still covers the cursor node
    }

    let positions = circle_positions(snapshot.nodes.len());
    let nodes = &snapshot.nodes;
    let edges = &snapshot.edges;

    let canvas = Canvas::default()
        .x_bounds(X_BOUNDS)
        .y_bounds(Y_BOUNDS)
        .marker(Marker::Dot)
        .paint(|ctx| {
            for edge in edges {
                if let (Some(&(x1, y1)), Some(&(x2, y2))) =
                    (positions.get(edge.from), positions.get(edge.to))
                {
                    let touches_cursor = edge.from == cursor || edge.to == cursor;
                    let color = if touches_cursor {
                        theme::AMBER_DEEP
                    } else {
                        theme::RULE
                    };
                    ctx.draw(&CanvasLine::new(x1, y1, x2, y2, color));
                }
            }
            for (i, node) in nodes.iter().enumerate() {
                if let Some(&(x, y)) = positions.get(i) {
                    ctx.print(x, y, node_glyph_line(node, i == cursor));
                }
            }
        });
    canvas.render(inner, buf);
}

/// The label printed at one node's position in the sketch: the cursor node is
/// bracketed and rendered in the brand accent so it stands out regardless of
/// its kind color; every other node keeps its kind glyph/color.
fn node_glyph_line(node: &GraphNode, is_cursor: bool) -> Line<'static> {
    let glyph = theme::graph_kind_glyph(&node.kind);
    if is_cursor {
        Line::from(Span::styled(
            format!("[{glyph}]"),
            Style::new().fg(theme::AMBER).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            glyph,
            Style::new().fg(theme::graph_kind_color(&node.kind)),
        ))
    }
}

/// Evenly space `n` points around a unit circle, starting at the top (12
/// o'clock) and proceeding clockwise. `n == 0` yields no points; `n == 1`
/// places the lone node at the origin.
fn circle_positions(n: usize) -> Vec<(f64, f64)> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![(0.0, 0.0)];
    }
    (0..n)
        .map(|i| {
            let angle = -FRAC_PI_2 + (i as f64) * (TAU / n as f64);
            (angle.cos(), angle.sin())
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::graph::{GraphEdge, GraphSnapshot};

    /// Flatten a `TestBackend` buffer to plain text (styling stripped — L-T6
    /// convention shared with `render.rs`'s tests: assert on content, not
    /// ANSI).
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

    fn small_snapshot() -> GraphSnapshot {
        // driver.rs --calls--> run() --imports--> serde
        GraphSnapshot {
            focus: "run".into(),
            nodes: vec![
                GraphNode {
                    label: "driver.rs".into(),
                    kind: "file".into(),
                    location: None,
                },
                GraphNode {
                    label: "run".into(),
                    kind: "function".into(),
                    location: Some("src/lib.rs:42".into()),
                },
                GraphNode {
                    label: "serde".into(),
                    kind: "module".into(),
                    location: None,
                },
            ],
            edges: vec![
                GraphEdge {
                    from: 0,
                    to: 1,
                    kind: "calls".into(),
                },
                GraphEdge {
                    from: 1,
                    to: 2,
                    kind: "imports".into(),
                },
            ],
        }
    }

    fn draw(ui: &mut DeckUi, w: u16, h: u16) -> String {
        let model = WorkspaceModel::new();
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render(&model, ui, area, f.buffer_mut());
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn empty_snapshot_shows_the_muted_hint() {
        let mut ui = DeckUi::default();
        let text = draw(&mut ui, 100, 24);
        assert!(
            text.contains("no neighborhood loaded"),
            "empty hint shown:\n{text}"
        );

        // Same for `Some(GraphSnapshot::default())` (present but empty).
        ui.graph = Some(GraphSnapshot::default());
        let text = draw(&mut ui, 100, 24);
        assert!(
            text.contains("no neighborhood loaded"),
            "empty-but-present snapshot also shows the hint:\n{text}"
        );
    }

    #[test]
    fn cursor_node_shows_label_kind_location_and_edges_as_human_relations() {
        let mut ui = DeckUi::default();
        ui.graph = Some(small_snapshot());
        ui.graph_cursor = 1; // the `run` function

        let text = draw(&mut ui, 100, 24);

        // The node list cites nodes by human label — never a raw id/index.
        assert!(text.contains("driver.rs"), "list shows driver.rs:\n{text}");
        assert!(text.contains("run"), "list shows run:\n{text}");
        assert!(text.contains("serde"), "list shows serde:\n{text}");

        // Detail panel: focus title, label, kind, location.
        assert!(text.contains("src/lib.rs:42"), "shows location:\n{text}");
        assert!(text.contains("function"), "shows kind:\n{text}");

        // Incident edges as human relations, citing the *other* node's label.
        assert!(
            text.contains("imports → serde"),
            "outgoing relation:\n{text}"
        );
        assert!(
            text.contains("called by ← driver.rs"),
            "incoming relation in passive form:\n{text}"
        );
    }

    #[test]
    fn cursor_clamps_to_the_node_range_instead_of_panicking() {
        let mut ui = DeckUi::default();
        ui.graph = Some(small_snapshot());
        ui.graph_cursor = 999; // stale/out-of-range cursor

        let text = draw(&mut ui, 100, 24);
        assert!(
            text.contains("serde"),
            "clamps to the last node (index 2) and renders it:\n{text}"
        );
        assert_eq!(
            ui.graph_cursor, 2,
            "render() writes the clamped cursor back"
        );
    }

    #[test]
    fn a_node_with_no_edges_says_so_instead_of_an_empty_list() {
        let mut ui = DeckUi::default();
        ui.graph = Some(small_snapshot());
        ui.graph_cursor = 0; // driver.rs has one outgoing edge; use a truly isolated node instead
        ui.graph = Some(GraphSnapshot {
            focus: "orphan".into(),
            nodes: vec![GraphNode {
                label: "orphan_fn".into(),
                kind: "function".into(),
                location: None,
            }],
            edges: vec![],
        });
        let text = draw(&mut ui, 100, 24);
        assert!(
            text.contains("no known relations"),
            "zero-degree node says so explicitly:\n{text}"
        );
    }

    #[test]
    fn node_list_windows_to_keep_the_cursor_visible() {
        // Far more nodes than a short terminal can list, cursor on the last
        // one: the window must slide so the selection stays on screen.
        let n = 40;
        let snapshot = GraphSnapshot {
            focus: "big".into(),
            nodes: (0..n)
                .map(|i| GraphNode {
                    label: format!("node_{i:02}"),
                    kind: "function".into(),
                    location: None,
                })
                .collect(),
            edges: vec![],
        };
        let mut ui = DeckUi::default();
        ui.graph = Some(snapshot);
        ui.graph_cursor = n - 1;

        let text = draw(&mut ui, 100, 12);
        assert!(
            text.contains("node_39"),
            "the cursor node scrolled into view:\n{text}"
        );
        assert!(
            !text.contains("node_00"),
            "the head of the list scrolled out of the window:\n{text}"
        );
    }

    #[test]
    fn passive_form_covers_the_documented_edge_kinds() {
        assert_eq!(passive("imports"), "imported by");
        assert_eq!(passive("calls"), "called by");
        assert_eq!(passive("defines"), "defined by");
        assert_eq!(passive("references"), "referenced by");
    }

    #[test]
    fn circle_positions_handles_zero_one_and_many_nodes_without_panicking() {
        assert!(circle_positions(0).is_empty());
        assert_eq!(circle_positions(1), vec![(0.0, 0.0)]);
        assert_eq!(circle_positions(5).len(), 5);
    }
}
