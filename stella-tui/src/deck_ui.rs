//! Ephemeral deck state and the pure deck-level key→action mapping.
//!
//! Mirrors the single-session [`crate::ui`] split: [`DeckUi`] holds everything
//! *not* derived from the event log (active tab, the one global composer,
//! per-tab scroll/selection, the splash, the out-of-band graph snapshot), and
//! [`handle_deck_key`] is a pure function of `(key, model, &mut ui)` returning a
//! [`DeckAction`]. All deck interaction logic lives here, unit-tested, so
//! [`crate::deck_shell`] stays a near-logic-free event loop.
//!
//! ## Interaction model (the "never blocks input" contract)
//!
//! There is one global composer. Printable keys type into it from **any** tab;
//! `Enter` **always** submits — enqueuing a new prompt without waiting on a busy
//! agent — unless the focused agent has a pending gate (scope review /
//! ask-user), in which case `Enter`/the gate keys answer it. Tab hotkeys
//! (`1`–`5`) and agent controls (`p`/`s`/`r`) only fire when the composer is
//! empty, so they never eat a keystroke meant for a prompt (the same
//! "quick-pick only when nothing typed" gate `crate::ui` already uses).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::composer::{
    Composer, SlashCommand, SlashPopupOutcome, handle_slash_popup_key, slash_popup_matches,
};
use crate::deck::{DeckTab, WorkspaceModel};
use crate::envelope::{AgentControl, AgentId, Inbound, WorkspaceInput};
use crate::graph::GraphSnapshot;
use crate::input::{ScopeDecision, UserInput};
use crate::scroll::ScrollState;
use crate::splash::SplashState;

/// Viewport sizes recorded by the last render, so the pure key handler can do
/// line-exact scroll clamping without knowing the terminal size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeckMetrics {
    pub session_height: usize,
    pub session_total: usize,
    pub trace_height: usize,
    pub trace_total: usize,
    pub files_diff_height: usize,
    pub files_diff_total: usize,
}

/// All ephemeral view state for the deck.
#[derive(Debug, Clone)]
pub struct DeckUi {
    pub tab: DeckTab,
    /// The one global composer — typing works from any tab.
    pub composer: Composer,
    pub splash: SplashState,
    pub help_open: bool,
    /// Focused agent index (the Agents-tab highlight and the Session-tab target).
    pub focused: usize,
    pub session_scroll: ScrollState,
    pub trace_scroll: ScrollState,
    /// `None` = all agents; `Some(id)` = filter the traces to one agent.
    pub trace_filter: Option<AgentId>,
    pub graph_cursor: usize,
    /// The out-of-band code-graph snapshot (set by the caller/scenario).
    pub graph: Option<GraphSnapshot>,
    pub files_sel: usize,
    pub files_diff_open: bool,
    pub files_diff_scroll: ScrollState,
    pub metrics: DeckMetrics,
    /// When the active tab last changed — `deck_render` drives the
    /// [`crate::fx::tab_switch`] sweep from it, then clears it once the
    /// motion has played out. `None` = no sweep in flight.
    pub tab_switched_at: Option<std::time::Instant>,
    /// The slash-command vocabulary offered by the `/` popup (an input — the
    /// caller owns the real list, exactly like the single-session shell).
    pub slash_commands: Vec<SlashCommand>,
    /// Selected row in the slash popup (clamped to the matches at use time).
    pub slash_selected: usize,
    /// Whether reasoning renders in full on the Session tab. Off by default —
    /// collapsed thinking shows a one-line live tail; `ctrl+r` toggles.
    pub thinking_expanded: bool,
    /// Whether the queue editor popup is open (`ctrl+t`, or `↑` from an empty
    /// composer on the Session tab while prompts are queued).
    pub queue_open: bool,
    /// Selected row in the queue editor.
    pub queue_sel: usize,
    /// Armed by the first `ctrl+d` in the queue editor; the second one clears
    /// the whole queue. Any other key disarms.
    pub queue_confirm_clear: bool,
}

impl Default for DeckUi {
    fn default() -> Self {
        Self {
            tab: DeckTab::Session,
            composer: Composer::new(),
            splash: SplashState::new(),
            help_open: false,
            focused: 0,
            session_scroll: ScrollState::default(),
            trace_scroll: ScrollState::default(),
            trace_filter: None,
            graph_cursor: 0,
            graph: None,
            files_sel: 0,
            files_diff_open: false,
            files_diff_scroll: ScrollState::default(),
            metrics: DeckMetrics::default(),
            tab_switched_at: None,
            slash_commands: Vec::new(),
            slash_selected: 0,
            thinking_expanded: false,
            queue_open: false,
            queue_sel: 0,
            queue_confirm_clear: false,
        }
    }
}

impl DeckUi {
    pub fn new(composer: Composer) -> Self {
        Self {
            composer,
            ..Self::default()
        }
    }

    /// Switch the active tab, stamping the moment so the render layer can
    /// play the [`crate::fx::tab_switch`] sweep. Same-tab is a no-op — a
    /// re-press must not restart the motion.
    pub fn set_tab(&mut self, tab: DeckTab) {
        if tab != self.tab {
            self.tab = tab;
            self.tab_switched_at = Some(std::time::Instant::now());
        }
    }
}

/// The outcome of handling one deck key.
#[derive(Debug, Clone, PartialEq)]
pub enum DeckAction {
    Ignored,
    Handled,
    Send(WorkspaceInput),
    /// Run a `!`-prefixed shell command **immediately** — never enqueued, never
    /// gated on a busy agent. The shell executes it and feeds the output back
    /// as synthetic events for the local `shell` lane.
    Shell(String),
    Quit,
}

/// Fold one inbound envelope and keep the ephemeral UI in range.
pub fn ingest_inbound(inbound: &Inbound, model: &mut WorkspaceModel, ui: &mut DeckUi) {
    model.apply_inbound(inbound);
    clamp(model, ui);
}

/// Clamp selections to the current agent/file/queue counts.
fn clamp(model: &WorkspaceModel, ui: &mut DeckUi) {
    if model.agents.is_empty() {
        ui.focused = 0;
    } else {
        ui.focused = ui.focused.min(model.agents.len() - 1);
    }
    let files = model.ledger.records.len();
    ui.files_sel = if files == 0 {
        0
    } else {
        ui.files_sel.min(files - 1)
    };
    let queued = model.queue.pending();
    ui.queue_sel = if queued == 0 {
        0
    } else {
        ui.queue_sel.min(queued - 1)
    };
}

/// Map one key to a [`DeckAction`]. Pure over `(key, model)`, mutating `ui`.
pub fn handle_deck_key(key: KeyEvent, model: &WorkspaceModel, ui: &mut DeckUi) -> DeckAction {
    if key.kind == KeyEventKind::Release {
        return DeckAction::Ignored;
    }

    // Ctrl-C: clean cancel + quit, from anywhere.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return DeckAction::Quit;
    }

    // While the splash is up, any key dismisses it.
    if !ui.splash.is_done() {
        ui.splash.skip();
        return DeckAction::Handled;
    }

    // Help overlay is modal: any key closes it.
    if ui.help_open {
        ui.help_open = false;
        return DeckAction::Handled;
    }

    // Ctrl-R toggles the collapsed-thinking view from anywhere.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('r')) {
        ui.thinking_expanded = !ui.thinking_expanded;
        return DeckAction::Handled;
    }

    // Ctrl-T toggles the queue editor from anywhere.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('t')) {
        ui.queue_open = !ui.queue_open;
        ui.queue_confirm_clear = false;
        return DeckAction::Handled;
    }

    // The queue editor is modal while open: the queue is a *list* the user
    // navigates and edits, never a blob (↑/↓ select · Enter edit · ctrl+x
    // delete · ctrl+d twice clear all · Esc close).
    if ui.queue_open {
        return handle_queue_key(key, model, ui);
    }

    let composer_empty = ui.composer.buffer().is_empty();

    // Deck-global tab navigation (Tab / Shift-Tab always; digits when empty).
    // The slash popup claims Tab first — completion beats tab-cycling while
    // the menu is open.
    let slash = slash_matches(ui);
    if slash.is_empty() {
        match key.code {
            KeyCode::Tab => {
                ui.set_tab(ui.tab.next());
                return DeckAction::Handled;
            }
            KeyCode::BackTab => {
                ui.set_tab(ui.tab.prev());
                return DeckAction::Handled;
            }
            KeyCode::Char(d @ '1'..='5') if composer_empty => {
                ui.set_tab(DeckTab::from_index((d as usize) - ('1' as usize)));
                return DeckAction::Handled;
            }
            KeyCode::Char('?') if composer_empty => {
                ui.help_open = true;
                return DeckAction::Handled;
            }
            _ => {}
        }
    } else if let Some(action) = handle_slash_key(key, &slash, ui) {
        return action;
    }

    // `↑` from an empty composer on the Session tab opens the queue editor on
    // the newest prompt — the "press up to edit what I queued" affordance.
    // Gated on *full* composer emptiness (chips included, not just the live
    // buffer): editing a queued item loads it via `Composer::load`, which
    // clears any pasted chip still waiting to be sent, so this must not
    // trigger while a chip is attached even if nothing further was typed.
    if ui.tab == DeckTab::Session
        && ui.composer.is_empty()
        && model.queue.pending() > 0
        && matches!(key.code, KeyCode::Up)
    {
        ui.queue_open = true;
        ui.queue_sel = model.queue.pending() - 1;
        ui.queue_confirm_clear = false;
        return DeckAction::Handled;
    }

    // Focused-agent gates take precedence over normal composer editing, exactly
    // like the single-session shell — but they route to the focused agent.
    if let Some(agent) = focused_id(model, ui)
        && let Some(action) = handle_focused_gates(key, model, ui, &agent)
    {
        return action;
    }

    // Per-tab navigation for non-typing keys, then composer editing.
    match ui.tab {
        DeckTab::Agents => handle_agents_key(key, model, ui, composer_empty),
        DeckTab::Traces => handle_traces_key(key, model, ui, composer_empty),
        DeckTab::Graph => handle_graph_key(key, ui, composer_empty),
        DeckTab::Files => handle_files_key(key, model, ui),
        DeckTab::Session => handle_session_key(key, ui),
    }
    .unwrap_or_else(|| handle_composer_key(key, ui))
}

/// The names of the slash commands matching the composer, or empty when the
/// popup is inactive.
fn slash_matches(ui: &DeckUi) -> Vec<String> {
    slash_popup_matches(&ui.composer, &ui.slash_commands)
}

/// Slash-popup navigation: ↑/↓ choose, Tab completes into the buffer, Enter
/// dispatches the selection (as an enqueue, like any prompt), Esc dismisses.
/// Returns `None` for keys the popup doesn't claim. Shared with the
/// single-session REPL (`crate::ui`) via `crate::composer` so both surfaces
/// stay consistent by construction.
fn handle_slash_key(key: KeyEvent, matches: &[String], ui: &mut DeckUi) -> Option<DeckAction> {
    match handle_slash_popup_key(key, matches, &mut ui.composer, &mut ui.slash_selected)? {
        SlashPopupOutcome::Handled => Some(DeckAction::Handled),
        SlashPopupOutcome::Submit(text) => Some(DeckAction::Send(WorkspaceInput::Enqueue { text })),
    }
}

/// The queue editor's modal keys. Everything the design doc promises for the
/// queue-as-a-list: per-item delete, pull-back-to-edit, and an explicit
/// two-press clear-all.
fn handle_queue_key(key: KeyEvent, model: &WorkspaceModel, ui: &mut DeckUi) -> DeckAction {
    let count = model.queue.pending();
    if count == 0 {
        // Nothing left to edit — any key just closes the popup.
        ui.queue_open = false;
        ui.queue_confirm_clear = false;
        return DeckAction::Handled;
    }
    ui.queue_sel = ui.queue_sel.min(count - 1);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('d') if ctrl => {
            if ui.queue_confirm_clear {
                ui.queue_confirm_clear = false;
                ui.queue_open = false;
                return DeckAction::Send(WorkspaceInput::QueueClear);
            }
            ui.queue_confirm_clear = true;
            return DeckAction::Handled;
        }
        _ => ui.queue_confirm_clear = false,
    }
    match key.code {
        KeyCode::Up => {
            ui.queue_sel = ui.queue_sel.saturating_sub(1);
            DeckAction::Handled
        }
        KeyCode::Down => {
            ui.queue_sel = (ui.queue_sel + 1).min(count - 1);
            DeckAction::Handled
        }
        KeyCode::Char('x') if ctrl => {
            DeckAction::Send(WorkspaceInput::QueueRemove {
                index: ui.queue_sel,
            })
        }
        KeyCode::Enter => {
            // Pull the prompt out of the queue and into the composer to edit —
            // it is *removed*, not duplicated; re-submitting re-enqueues it.
            let index = ui.queue_sel;
            if let Some(item) = model.queue.items.get(index) {
                ui.composer.load(item.text.clone());
            }
            ui.queue_open = false;
            DeckAction::Send(WorkspaceInput::QueueRemove { index })
        }
        KeyCode::Esc => {
            ui.queue_open = false;
            DeckAction::Handled
        }
        _ => DeckAction::Ignored,
    }
}

/// The id of the focused agent, if any.
fn focused_id(model: &WorkspaceModel, ui: &DeckUi) -> Option<AgentId> {
    model.agents.get(ui.focused).map(|a| a.meta.id.clone())
}

/// Scope-review / ask-user gates for the focused agent. Returns `Some` to
/// short-circuit; `None` to fall through to normal editing.
fn handle_focused_gates(
    key: KeyEvent,
    model: &WorkspaceModel,
    ui: &mut DeckUi,
    agent: &AgentId,
) -> Option<DeckAction> {
    let entry = model.agents.get(ui.focused)?;
    let composer_empty = ui.composer.buffer().is_empty();

    // Scope review: a/t/x/Esc decide it (only when nothing is typed).
    if entry.model.pending_scope_review.is_some() && composer_empty {
        let decision = match key.code {
            KeyCode::Char('a') => Some(ScopeDecision::Approve),
            KeyCode::Char('t') => Some(ScopeDecision::Trim),
            KeyCode::Char('x') | KeyCode::Esc => Some(ScopeDecision::Abort),
            _ => None,
        };
        if let Some(decision) = decision {
            return Some(DeckAction::Send(WorkspaceInput::ToAgent {
                agent: agent.clone(),
                input: UserInput::ScopeDecision(decision),
            }));
        }
    }

    // Ask-user: digit quick-pick when nothing typed; Enter submits free text.
    if let Some(prompt) = &entry.model.pending_ask_user {
        match key.code {
            KeyCode::Char(d @ '1'..='9') if composer_empty => {
                let idx = (d as usize) - ('1' as usize);
                if let Some(option) = prompt.options.get(idx) {
                    return Some(DeckAction::Send(WorkspaceInput::ToAgent {
                        agent: agent.clone(),
                        input: UserInput::AskUserAnswer {
                            id: prompt.id.clone(),
                            answer: option.clone(),
                        },
                    }));
                }
            }
            // A `!` line is a shell command even while a question is pending —
            // it must run immediately, not be swallowed as the answer.
            KeyCode::Enter if !ui.composer.buffer().trim_start().starts_with('!') => {
                if let Some(answer) = ui.composer.take_submission() {
                    return Some(DeckAction::Send(WorkspaceInput::ToAgent {
                        agent: agent.clone(),
                        input: UserInput::AskUserAnswer {
                            id: prompt.id.clone(),
                            answer,
                        },
                    }));
                }
                return Some(DeckAction::Ignored); // force an explicit answer
            }
            _ => {}
        }
    }
    None
}

fn handle_agents_key(
    key: KeyEvent,
    model: &WorkspaceModel,
    ui: &mut DeckUi,
    composer_empty: bool,
) -> Option<DeckAction> {
    let count = model.agents.len();
    match key.code {
        KeyCode::Up => {
            ui.focused = ui.focused.saturating_sub(1);
            Some(DeckAction::Handled)
        }
        KeyCode::Down => {
            if count > 0 {
                ui.focused = (ui.focused + 1).min(count - 1);
            }
            Some(DeckAction::Handled)
        }
        KeyCode::Enter if composer_empty => {
            ui.set_tab(DeckTab::Session);
            Some(DeckAction::Handled)
        }
        // Agent controls — only when the composer is empty (else they type).
        KeyCode::Char('p') | KeyCode::Char('s') | KeyCode::Char('r') if composer_empty => {
            let control = match key.code {
                KeyCode::Char('p') => AgentControl::Pause,
                KeyCode::Char('s') => AgentControl::Stop,
                _ => AgentControl::Restart,
            };
            model.agents.get(ui.focused).map(|entry| {
                DeckAction::Send(WorkspaceInput::Control {
                    agent: entry.meta.id.clone(),
                    control,
                })
            })
        }
        _ => None,
    }
}

fn handle_traces_key(
    key: KeyEvent,
    model: &WorkspaceModel,
    ui: &mut DeckUi,
    composer_empty: bool,
) -> Option<DeckAction> {
    let (total, height) = (ui.metrics.trace_total, ui.metrics.trace_height);
    match key.code {
        KeyCode::Up => {
            ui.trace_scroll.scroll_up(1, total, height);
            Some(DeckAction::Handled)
        }
        KeyCode::Down => {
            ui.trace_scroll.scroll_down(1, total, height);
            Some(DeckAction::Handled)
        }
        KeyCode::PageUp => {
            ui.trace_scroll.page_up(total, height);
            Some(DeckAction::Handled)
        }
        KeyCode::PageDown => {
            ui.trace_scroll.page_down(total, height);
            Some(DeckAction::Handled)
        }
        // `f` cycles the per-agent filter (only when nothing is typed).
        KeyCode::Char('f') if composer_empty => {
            ui.trace_filter = cycle_filter(model, ui.trace_filter.as_deref());
            Some(DeckAction::Handled)
        }
        _ => None,
    }
}

fn handle_graph_key(key: KeyEvent, ui: &mut DeckUi, composer_empty: bool) -> Option<DeckAction> {
    let node_count = ui.graph.as_ref().map(|g| g.nodes.len()).unwrap_or(0);
    match key.code {
        KeyCode::Left | KeyCode::Up => {
            ui.graph_cursor = ui.graph_cursor.saturating_sub(1);
            Some(DeckAction::Handled)
        }
        KeyCode::Right | KeyCode::Down => {
            if node_count > 0 {
                ui.graph_cursor = (ui.graph_cursor + 1).min(node_count - 1);
            }
            Some(DeckAction::Handled)
        }
        // `/` search reserved (only when composer empty) — no-op stub for now.
        KeyCode::Char('/') if composer_empty => Some(DeckAction::Handled),
        _ => None,
    }
}

fn handle_files_key(key: KeyEvent, model: &WorkspaceModel, ui: &mut DeckUi) -> Option<DeckAction> {
    let count = model.ledger.records.len();
    if ui.files_diff_open {
        let (total, height) = (ui.metrics.files_diff_total, ui.metrics.files_diff_height);
        match key.code {
            KeyCode::Esc => {
                ui.files_diff_open = false;
                return Some(DeckAction::Handled);
            }
            KeyCode::Up => {
                ui.files_diff_scroll.scroll_up(1, total, height);
                return Some(DeckAction::Handled);
            }
            KeyCode::Down => {
                ui.files_diff_scroll.scroll_down(1, total, height);
                return Some(DeckAction::Handled);
            }
            _ => {}
        }
    }
    match key.code {
        KeyCode::Up => {
            ui.files_sel = ui.files_sel.saturating_sub(1);
            Some(DeckAction::Handled)
        }
        KeyCode::Down => {
            if count > 0 {
                ui.files_sel = (ui.files_sel + 1).min(count - 1);
            }
            Some(DeckAction::Handled)
        }
        KeyCode::Enter if count > 0 => {
            ui.files_diff_open = !ui.files_diff_open;
            ui.files_diff_scroll = ScrollState::default();
            Some(DeckAction::Handled)
        }
        _ => None,
    }
}

fn handle_session_key(key: KeyEvent, ui: &mut DeckUi) -> Option<DeckAction> {
    let (total, height) = (ui.metrics.session_total, ui.metrics.session_height);
    match key.code {
        KeyCode::Up => {
            ui.session_scroll.scroll_up(1, total, height);
            Some(DeckAction::Handled)
        }
        KeyCode::Down => {
            ui.session_scroll.scroll_down(1, total, height);
            Some(DeckAction::Handled)
        }
        KeyCode::PageUp => {
            ui.session_scroll.page_up(total, height);
            Some(DeckAction::Handled)
        }
        KeyCode::PageDown => {
            ui.session_scroll.page_down(total, height);
            Some(DeckAction::Handled)
        }
        _ => None,
    }
}

/// The always-available composer editing + non-blocking submit.
fn handle_composer_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match key.code {
        KeyCode::Enter => match ui.composer.take_submission() {
            // A `!`-prefixed line is a shell command: it executes IMMEDIATELY,
            // bypassing the prompt queue and any busy agent entirely.
            Some(text) if text.trim_start().starts_with('!') => {
                // Strip only the single leading `!` dispatch marker — not
                // every leading `!` — so a command whose own text starts
                // with `!` (e.g. `!!foo`, meant as the shell command `!foo`)
                // is not rewritten into something else.
                let leading = text.trim_start();
                let cmd = leading
                    .strip_prefix('!')
                    .unwrap_or(leading)
                    .trim()
                    .to_string();
                if cmd.is_empty() {
                    DeckAction::Ignored
                } else {
                    DeckAction::Shell(cmd)
                }
            }
            // Any other prompt ALWAYS enqueues — never blocks on a busy agent.
            Some(text) => DeckAction::Send(WorkspaceInput::Enqueue { text }),
            None => DeckAction::Ignored,
        },
        KeyCode::Backspace => {
            ui.composer.backspace();
            ui.slash_selected = 0;
            DeckAction::Handled
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            ui.composer.insert_char(c);
            ui.slash_selected = 0;
            DeckAction::Handled
        }
        _ => DeckAction::Ignored,
    }
}

/// Cycle the trace filter: None → agent[0] → agent[1] → … → None.
fn cycle_filter(model: &WorkspaceModel, current: Option<&str>) -> Option<AgentId> {
    if model.agents.is_empty() {
        return None;
    }
    match current {
        None => Some(model.agents[0].meta.id.clone()),
        Some(id) => {
            let idx = model.index_of(id).unwrap_or(0);
            model.agents.get(idx + 1).map(|a| a.meta.id.clone())
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::envelope::AgentMeta;
    use stella_protocol::AgentEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn model_with(ids: &[&str]) -> WorkspaceModel {
        let mut m = WorkspaceModel::new();
        for id in ids {
            m.apply_inbound(&Inbound::Register(AgentMeta::new(*id, *id, 0)));
        }
        m
    }
    fn ready_ui() -> DeckUi {
        let mut ui = DeckUi::default();
        ui.splash.skip(); // past the splash for interaction tests
        ui
    }

    #[test]
    fn any_key_dismisses_the_splash_first() {
        let model = model_with(&["lead"]);
        let mut ui = DeckUi::default(); // splash NOT skipped
        assert!(!ui.splash.is_done());
        assert_eq!(
            handle_deck_key(ch('a'), &model, &mut ui),
            DeckAction::Handled
        );
        assert!(ui.splash.is_done(), "first key skips the splash");
        assert!(ui.composer.buffer().is_empty(), "and does not type");
    }

    #[test]
    fn tab_and_digits_switch_tabs_only_when_composer_empty() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        assert_eq!(ui.tab, DeckTab::Session);
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Agents);
        handle_deck_key(ch('3'), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Traces);
        // Once typing, a digit types instead of switching.
        handle_deck_key(ch('h'), &model, &mut ui);
        handle_deck_key(ch('2'), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Traces, "digit typed, tab unchanged");
        assert_eq!(ui.composer.buffer(), "h2");
    }

    #[test]
    fn enter_always_enqueues_a_prompt_without_blocking() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        for c in "do the thing".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::Enqueue {
                text: "do the thing".into()
            })
        );
        assert!(
            ui.composer.buffer().is_empty(),
            "composer clears after submit"
        );
    }

    #[test]
    fn ctrl_c_quits_from_any_tab() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.tab = DeckTab::Graph;
        assert_eq!(
            handle_deck_key(ctrl('c'), &model, &mut ui),
            DeckAction::Quit
        );
    }

    #[test]
    fn agents_tab_controls_fire_only_when_composer_empty() {
        let model = model_with(&["lead", "sub"]);
        let mut ui = ready_ui();
        ui.tab = DeckTab::Agents;
        ui.focused = 1;
        // 's' with empty composer → Stop control for the focused agent.
        assert_eq!(
            handle_deck_key(ch('s'), &model, &mut ui),
            DeckAction::Send(WorkspaceInput::Control {
                agent: "sub".into(),
                control: AgentControl::Stop,
            })
        );
        // With text typed, 's' types instead.
        handle_deck_key(ch('h'), &model, &mut ui);
        handle_deck_key(ch('s'), &model, &mut ui);
        assert_eq!(ui.composer.buffer(), "hs");
    }

    #[test]
    fn agents_updown_moves_focus_and_enter_opens_session() {
        let model = model_with(&["a", "b", "c"]);
        let mut ui = ready_ui();
        ui.tab = DeckTab::Agents;
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.focused, 2);
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert_eq!(ui.focused, 1);
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Session);
    }

    #[test]
    fn focused_scope_gate_routes_decision_to_that_agent() {
        let mut model = model_with(&["lead"]);
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::ScopeReview {
                proposal: stella_protocol::ScopeProposal {
                    summary: "big".into(),
                    steps: vec![],
                    estimated_files: 3,
                    estimated_cost_usd: None,
                },
            },
        });
        let mut ui = ready_ui();
        let action = handle_deck_key(ch('a'), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::ToAgent {
                agent: "lead".into(),
                input: UserInput::ScopeDecision(ScopeDecision::Approve),
            })
        );
    }

    #[test]
    fn set_tab_stamps_the_switch_moment_only_on_change() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        assert!(ui.tab_switched_at.is_none(), "no motion before any switch");

        // The key path routes through set_tab and stamps the moment.
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Agents);
        let first = ui.tab_switched_at.expect("tab switch stamped");

        // Switching to the SAME tab must not restart the sweep.
        ui.set_tab(DeckTab::Agents);
        assert_eq!(ui.tab_switched_at, Some(first));
    }

    #[test]
    fn traces_filter_cycles_through_agents_and_back() {
        let model = model_with(&["a", "b"]);
        let mut ui = ready_ui();
        ui.tab = DeckTab::Traces;
        assert_eq!(ui.trace_filter, None);
        handle_deck_key(ch('f'), &model, &mut ui);
        assert_eq!(ui.trace_filter.as_deref(), Some("a"));
        handle_deck_key(ch('f'), &model, &mut ui);
        assert_eq!(ui.trace_filter.as_deref(), Some("b"));
        handle_deck_key(ch('f'), &model, &mut ui);
        assert_eq!(ui.trace_filter, None);
    }

    fn model_with_queue(prompts: &[&str]) -> WorkspaceModel {
        let mut m = model_with(&["lead"]);
        for (i, p) in prompts.iter().enumerate() {
            m.queue.enqueue((*p).to_string(), i as u64);
        }
        m
    }

    #[test]
    fn bang_prefix_runs_a_shell_command_immediately_never_enqueued() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        for c in "!cargo build".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(action, DeckAction::Shell("cargo build".into()));
    }

    #[test]
    fn bang_prefix_only_strips_the_single_dispatch_marker() {
        // The command text itself starts with `!` (e.g. `!important`), so
        // the typed line is `!!important` — only the first `!` is the
        // dispatch marker; the second belongs to the command.
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        for c in "!!important".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(action, DeckAction::Shell("!important".into()));
    }

    #[test]
    fn bang_prefix_beats_a_pending_ask_user_gate() {
        let mut model = model_with(&["lead"]);
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::AskUser {
                id: "q1".into(),
                question: "which db?".into(),
                options: vec!["sqlite".into()],
            },
        });
        let mut ui = ready_ui();
        for c in "!ls".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Shell("ls".into()),
            "a shell line is never swallowed as a gate answer"
        );
    }

    #[test]
    fn ctrl_t_toggles_the_queue_editor() {
        let model = model_with_queue(&["one"]);
        let mut ui = ready_ui();
        assert!(!ui.queue_open);
        handle_deck_key(ctrl('t'), &model, &mut ui);
        assert!(ui.queue_open);
        handle_deck_key(ctrl('t'), &model, &mut ui);
        assert!(!ui.queue_open);
    }

    #[test]
    fn up_arrow_on_session_opens_the_queue_editor_on_the_newest_prompt() {
        let model = model_with_queue(&["first", "second", "third"]);
        let mut ui = ready_ui();
        assert_eq!(ui.tab, DeckTab::Session);
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert!(ui.queue_open, "up from an empty composer opens the queue");
        assert_eq!(ui.queue_sel, 2, "newest prompt selected");
        // With no queue, Up falls back to transcript scrolling (no popup).
        let empty = model_with(&["lead"]);
        let mut ui2 = ready_ui();
        handle_deck_key(key(KeyCode::Up), &empty, &mut ui2);
        assert!(!ui2.queue_open);
    }

    #[test]
    fn up_arrow_does_not_open_the_queue_editor_over_a_pasted_chip() {
        // The live buffer is empty, but a pasted chip is still attached —
        // editing a queued item would `Composer::load` and silently drop it,
        // so the composer must not read as "empty" here.
        let model = model_with_queue(&["first"]);
        let mut ui = ready_ui();
        ui.composer
            .paste("line1\nline2\nline3\nline4\nline5\nline6");
        assert!(ui.composer.buffer().is_empty());
        assert!(
            !ui.composer.is_empty(),
            "the chip keeps the composer non-empty"
        );
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert!(
            !ui.queue_open,
            "opening the queue editor here would drop the pasted chip on next edit"
        );
    }

    #[test]
    fn queue_editor_navigates_deletes_and_edits_as_a_list() {
        let model = model_with_queue(&["first", "second", "third"]);
        let mut ui = ready_ui();
        handle_deck_key(ctrl('t'), &model, &mut ui);
        // Navigate to the second prompt.
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.queue_sel, 1);
        // ctrl+x deletes exactly the selected prompt.
        assert_eq!(
            handle_deck_key(ctrl('x'), &model, &mut ui),
            DeckAction::Send(WorkspaceInput::QueueRemove { index: 1 })
        );
        // Enter pulls the selected prompt back into the composer for editing.
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::QueueRemove { index: 1 })
        );
        assert_eq!(ui.composer.buffer(), "second", "prompt loaded for editing");
        assert!(!ui.queue_open, "editing returns to the composer");
    }

    #[test]
    fn queue_clear_requires_two_ctrl_d_presses() {
        let model = model_with_queue(&["a", "b"]);
        let mut ui = ready_ui();
        handle_deck_key(ctrl('t'), &model, &mut ui);
        // First press only arms the confirm.
        assert_eq!(handle_deck_key(ctrl('d'), &model, &mut ui), DeckAction::Handled);
        assert!(ui.queue_confirm_clear);
        // Any other key disarms it.
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert!(!ui.queue_confirm_clear, "other keys disarm the confirm");
        // Two consecutive presses clear.
        handle_deck_key(ctrl('d'), &model, &mut ui);
        assert_eq!(
            handle_deck_key(ctrl('d'), &model, &mut ui),
            DeckAction::Send(WorkspaceInput::QueueClear)
        );
        assert!(!ui.queue_open, "clearing closes the editor");
    }

    #[test]
    fn ctrl_r_toggles_thinking_from_any_tab() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        assert!(!ui.thinking_expanded, "collapsed by default");
        handle_deck_key(ctrl('r'), &model, &mut ui);
        assert!(ui.thinking_expanded);
    }

    #[test]
    fn deck_slash_popup_selects_completes_and_dispatches() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.slash_commands = vec![
            SlashCommand::new("/help", "show help"),
            SlashCommand::new("/models", "list models"),
        ];
        handle_deck_key(ch('/'), &model, &mut ui);
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.slash_selected, 1);
        // Tab completes into the buffer while the popup is open (it does NOT
        // cycle tabs).
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui);
        assert_eq!(ui.composer.buffer(), "/models");
        assert_eq!(ui.tab, DeckTab::Session, "tab did not cycle");
        // Enter dispatches the (still-matching) selection as a prompt.
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::Enqueue {
                text: "/models".into()
            })
        );
    }
}
