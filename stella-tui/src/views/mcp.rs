//! MCP tab — the management surface for external Model Context Protocol
//! servers: a live dashboard of configured servers (enabled/connected/health,
//! configured auth, per-tool call counts) plus in-tab registry search,
//! install, per-session enable/disable, auth, and remove.
//!
//! State lives entirely in [`McpTabState`] (a field on `DeckUi`); the driver
//! feeds it out-of-band snapshots ([`crate::Inbound::McpServers`] /
//! [`crate::Inbound::McpSearchResults`]) and services the actions the key
//! handler emits. The auth-value buffer is redacted in `Debug` so it never
//! reaches the deck's debug log.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use crate::deck::WorkspaceModel;
use crate::deck_ui::DeckUi;
use crate::envelope::{McpSearchOutcome, McpServerInfo};
use crate::theme;

/// Which sub-mode the MCP tab is in — browsing the configured list, typing a
/// registry search, or entering an auth credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpMode {
    #[default]
    Browse,
    Search,
    Auth,
}

/// The two steps of the in-tab auth prompt: name the credential, then enter its
/// (masked) value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthStep {
    #[default]
    Field,
    Value,
}

/// The in-progress auth prompt. `value`'s `Debug` is redacted so it never
/// appears in a log even though `DeckUi` derives `Debug`.
#[derive(Clone, Default)]
pub struct AuthPrompt {
    pub server: String,
    pub field: String,
    pub value: String,
    pub step: AuthStep,
}

impl std::fmt::Debug for AuthPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPrompt")
            .field("server", &self.server)
            .field("field", &self.field)
            .field("value", &"<redacted>")
            .field("step", &self.step)
            .finish()
    }
}

/// All MCP-tab view state.
#[derive(Debug, Clone, Default)]
pub struct McpTabState {
    /// The configured servers snapshot (out-of-band, from the driver).
    pub servers: Vec<McpServerInfo>,
    /// Highlighted row in the configured list.
    pub selected: usize,
    pub mode: McpMode,
    /// The registry-search query buffer (Search mode).
    pub query: String,
    /// The most recent search outcome (results or an error).
    pub search: Option<McpSearchOutcome>,
    /// Highlighted row among the search results.
    pub search_selected: usize,
    /// A search request is in flight (show a spinner-ish label).
    pub searching: bool,
    /// The in-progress auth prompt (Auth mode).
    pub auth: AuthPrompt,
    /// A transient one-line status/feedback message (cleared on next snapshot).
    pub status: Option<String>,
}

impl McpTabState {
    /// The currently-highlighted configured server, if any.
    pub fn selected_server(&self) -> Option<&McpServerInfo> {
        self.servers.get(self.selected)
    }

    /// The currently-highlighted search result name, if any.
    pub fn selected_search_name(&self) -> Option<&str> {
        self.search
            .as_ref()
            .and_then(|o| o.items.get(self.search_selected))
            .map(|i| i.name.as_str())
    }

    /// Whether the current search results match the current query (so a second
    /// Enter should install rather than re-search).
    pub fn results_match_query(&self) -> bool {
        self.search.as_ref().is_some_and(|o| {
            o.error.is_none() && o.query == self.query.trim() && !o.items.is_empty()
        })
    }
}

pub fn render(_model: &WorkspaceModel, ui: &mut DeckUi, area: Rect, buf: &mut Buffer) {
    let state = &ui.mcp;
    let connected = state.servers.iter().filter(|s| s.connected).count();
    let title = format!(
        " MCP — {} configured / {} connected ",
        state.servers.len(),
        connected
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(theme::rule());
    let inner = block.inner(area);
    block.render(area, buf);

    let mut lines: Vec<Line> = Vec::new();
    match state.mode {
        McpMode::Browse => render_browse(state, &mut lines),
        McpMode::Search => render_search(state, &mut lines),
        McpMode::Auth => render_auth(state, &mut lines),
    }

    // A transient status line (action feedback), then the keybind footer.
    lines.push(Line::default());
    if let Some(status) = &state.status {
        lines.push(Line::from(Span::styled(
            format!("  {status}"),
            Style::default().fg(theme::EMBER_GOLD),
        )));
    }
    lines.push(footer(state.mode));

    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(inner, buf);
}

fn render_browse(state: &McpTabState, lines: &mut Vec<Line<'static>>) {
    if state.servers.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No MCP servers configured.",
            theme::muted(),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Press s to search a registry, then Enter to install one.",
            theme::muted(),
        )));
        return;
    }
    for (i, server) in state.servers.iter().enumerate() {
        let selected = i == state.selected;
        let marker = if selected { "▸ " } else { "  " };
        // Enabled/disabled glyph.
        let (enabled_glyph, enabled_style) = if server.enabled {
            ("●", Style::default().fg(theme::SUCCESS_BRIGHT))
        } else {
            ("○", theme::muted())
        };
        let name_style = if selected {
            Style::default()
                .fg(theme::INK)
                .bg(theme::SELECT_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::INK)
        };
        // Connection / health.
        let conn = if !server.enabled {
            Span::styled("disabled", theme::muted())
        } else if server.connected {
            let label = server.health.clone().unwrap_or_else(|| "live".to_string());
            Span::styled(label, Style::default().fg(theme::SUCCESS))
        } else {
            Span::styled("not connected", Style::default().fg(theme::WARNING_BRIGHT))
        };

        let mut spans = vec![
            Span::raw(marker),
            Span::styled(enabled_glyph, enabled_style),
            Span::raw(" "),
            Span::styled(server.name.clone(), name_style),
            Span::styled(format!("  [{}]", server.kind), theme::muted()),
            Span::raw("  "),
            conn,
        ];
        if !server.auth_fields.is_empty() {
            spans.push(Span::styled(
                format!("  ⚿ {}", server.auth_fields.join(",")),
                Style::default().fg(theme::VIOLET),
            ));
        }
        // OAuth state for http servers: logged in (green) or available (`o`).
        match server.oauth {
            Some(true) => spans.push(Span::styled(
                "  ⚿ oauth ✓",
                Style::default().fg(theme::SUCCESS),
            )),
            Some(false) => spans.push(Span::styled("  ⚿ oauth: o to log in", theme::muted())),
            None => {}
        }
        spans.push(Span::styled(
            format!("  · {} tools", server.tool_count),
            theme::muted(),
        ));
        spans.push(Span::styled(
            format!("  · {}×", server.calls),
            Style::default().fg(theme::EMBER_GOLD),
        ));
        lines.push(Line::from(spans));
    }
}

fn render_search(state: &McpTabState, lines: &mut Vec<Line<'static>>) {
    let query_line = Line::from(vec![
        Span::styled("  search ", theme::accent()),
        Span::styled(state.query.clone(), Style::default().fg(theme::INK)),
        Span::styled("▏", Style::default().fg(theme::EMBER_GOLD)),
    ]);
    lines.push(query_line);
    lines.push(Line::default());

    if state.searching {
        lines.push(Line::from(Span::styled("  searching…", theme::accent())));
        return;
    }
    let Some(outcome) = &state.search else {
        lines.push(Line::from(Span::styled(
            "  Type a query and press Enter to search the registry.",
            theme::muted(),
        )));
        return;
    };
    if let Some(err) = &outcome.error {
        lines.push(Line::from(Span::styled(
            format!("  search failed: {err}"),
            Style::default().fg(theme::DANGER_BRIGHT),
        )));
        return;
    }
    if outcome.items.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  no servers matching “{}”", outcome.query),
            theme::muted(),
        )));
        return;
    }
    for (i, item) in outcome.items.iter().enumerate() {
        let selected = i == state.search_selected;
        let marker = if selected { "▸ " } else { "  " };
        let name_style = if selected {
            Style::default()
                .fg(theme::INK)
                .bg(theme::SELECT_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::INK)
        };
        let mut spans = vec![
            Span::raw(marker),
            Span::styled(item.name.clone(), name_style),
            Span::styled(format!("  [{}]", item.kinds), theme::muted()),
        ];
        if item.installed {
            spans.push(Span::styled(
                "  installed",
                Style::default().fg(theme::SUCCESS),
            ));
        }
        lines.push(Line::from(spans));
        if !item.description.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("      {}", item.description),
                theme::muted(),
            )));
        }
    }
    if outcome.has_more {
        lines.push(Line::from(Span::styled(
            "  … more results (refine the query)",
            theme::muted(),
        )));
    }
}

fn render_auth(state: &McpTabState, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::from(vec![
        Span::styled("  auth ", theme::accent()),
        Span::styled(state.auth.server.clone(), Style::default().fg(theme::INK)),
    ]));
    lines.push(Line::default());
    let field_active = state.auth.step == AuthStep::Field;
    lines.push(prompt_line(
        "  credential (env var / header): ",
        &state.auth.field,
        field_active,
        false,
    ));
    lines.push(prompt_line(
        "  value: ",
        &state.auth.value,
        !field_active,
        true,
    ));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  The value is stored in .stella/mcp.toml and never logged.",
        theme::muted(),
    )));
}

/// One field of the auth prompt. When `mask`, the value renders as bullets.
fn prompt_line(label: &str, value: &str, active: bool, mask: bool) -> Line<'static> {
    let shown = if mask {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    let value_style = Style::default().fg(theme::INK);
    let mut spans = vec![
        Span::styled(label.to_string(), theme::accent()),
        Span::styled(shown, value_style),
    ];
    if active {
        spans.push(Span::styled("▏", Style::default().fg(theme::EMBER_GOLD)));
    }
    Line::from(spans)
}

fn footer(mode: McpMode) -> Line<'static> {
    let pairs: &[(&str, &str)] = match mode {
        McpMode::Browse => &[
            ("↑↓", "select"),
            ("e/␣", "enable/disable"),
            ("s", "search registry (also /mcp-search)"),
            ("a", "auth"),
            ("o", "oauth login"),
            ("x", "remove"),
            ("r", "refresh"),
        ],
        McpMode::Search => &[
            ("type", "query"),
            ("↑↓", "results"),
            ("enter", "search/install"),
            ("esc", "back"),
        ],
        McpMode::Auth => &[("enter", "next/save"), ("esc", "cancel")],
    };
    let mut spans = vec![Span::raw("  ")];
    for (key, desc) in pairs {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(theme::VIOLET)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!("{desc}  "), theme::muted()));
    }
    Line::from(spans)
}
