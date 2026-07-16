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
//! There is one global composer. Printable keys type into it from **any** tab
//! and it edits like a textarea: plain `⏎` inserts a line break (preserved
//! verbatim in the submitted prompt) and the `⌘⏎`/`⌃⏎` chord **always**
//! submits — enqueuing a new prompt without waiting on a busy agent — unless
//! the focused agent has a pending gate (scope review / ask-user), in which
//! case the chord/the gate keys answer it. (On legacy terminals that can't
//! report a modified Enter, plain `⏎` submits and `⌥⏎` is the line break —
//! see [`crate::composer::classify_enter`].) Tab hotkeys (`1`–`5`) and agent
//! controls (`p`/`s`/`r`) only fire when the composer is empty, so they never
//! eat a keystroke meant for a prompt (the same "quick-pick only when nothing
//! typed" gate `crate::ui` already uses).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::composer::{
    Composer, EnterAction, SlashCommand, SlashPopupOutcome, classify_enter, handle_edit_key,
    handle_slash_popup_key, slash_popup_matches,
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
    /// Legacy-terminal Enter semantics (see
    /// [`crate::composer::classify_enter`]): `false` when the kitty keyboard
    /// protocol is active (plain `⏎` = line break, `⌘⏎`/`⌃⏎` = submit),
    /// `true` where a modified Enter is unreportable (plain `⏎` = submit,
    /// `⌥⏎` = line break). The shell sets this from the terminal capability.
    pub enter_submits: bool,
    /// The transcript entry highlighted with ↑/↓ on the Session tab —
    /// `ctrl+o` toggles its expanded view. `None` = nothing selected.
    pub session_selected: Option<usize>,
    /// Set by a selection move; the session view consumes it (where visual
    /// row ranges are known) to scroll the selected entry into view.
    pub session_pending_scroll: Option<usize>,
    /// Per-agent set of entry indices expanded with `ctrl+o`.
    pub expanded: std::collections::HashMap<String, std::collections::HashSet<usize>>,
    /// Bumped on every expansion toggle so the fold cache invalidates.
    pub expanded_rev: u64,
    /// Per-agent eviction count last reconciled by [`ingest_inbound`] —
    /// front-eviction shifts every retained index, so when it advances that
    /// agent's `expanded` set is stale and must drop.
    pub evicted_seen: std::collections::HashMap<String, usize>,
    /// Armed by a `ctrl+o` pressed from the prompt (no selection); a second
    /// consecutive `ctrl+o` escalates to the all-thinking toggle. Any other
    /// key disarms.
    pub ctrl_o_primed: bool,
    /// The Session tab's incremental transcript fold cache.
    pub session_fold: crate::views::session::SessionFold,
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
            enter_submits: false,
            session_selected: None,
            session_pending_scroll: None,
            expanded: std::collections::HashMap::new(),
            expanded_rev: 0,
            evicted_seen: std::collections::HashMap::new(),
            ctrl_o_primed: false,
            session_fold: crate::views::session::SessionFold::default(),
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

    /// Point the deck at agent `idx`. A session-message selection indexes the
    /// *previously* focused agent's transcript, so it must not carry across;
    /// dropping it also re-arms tail-follow (the selection is what pinned the
    /// scroll window). Same-agent is a no-op.
    pub fn focus_agent(&mut self, idx: usize) {
        if idx == self.focused {
            return;
        }
        self.focused = idx;
        if self.session_selected.take().is_some() {
            self.session_pending_scroll = None;
            self.session_scroll.follow = true;
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
    // A refreshed graph snapshot or slash vocabulary is out-of-band view
    // state, not a model fold — apply it straight to the UI. Everything else
    // folds into the model, then selections are re-clamped.
    if let Inbound::GraphSnapshot(snapshot) = inbound {
        ui.graph = Some(snapshot.clone());
        return;
    }
    if let Inbound::SlashCommands(commands) = inbound {
        ui.slash_commands = commands.clone();
        ui.slash_selected = 0;
        return;
    }
    model.apply_inbound(inbound);
    clamp(model, ui);
}

/// Clamp selections to the current agent/file/queue counts.
fn clamp(model: &WorkspaceModel, ui: &mut DeckUi) {
    if model.agents.is_empty() {
        ui.focus_agent(0);
    } else {
        ui.focus_agent(ui.focused.min(model.agents.len() - 1));
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
    // Front-eviction shifts every retained index: a ctrl+o flag would
    // silently re-attach to whichever entry slid into its slot, so when an
    // agent's cumulative eviction count advances its expansion set drops
    // (bumping the rev invalidates the fold cache).
    for agent in &model.agents {
        let evicted = agent.model.evicted_entries();
        if evicted > ui.evicted_seen.get(&agent.meta.id).copied().unwrap_or(0) {
            ui.evicted_seen.insert(agent.meta.id.clone(), evicted);
            if ui.expanded.remove(&agent.meta.id).is_some() {
                ui.expanded_rev += 1;
            }
        }
    }
    // The ↑/↓ highlight must stay inside the retained window — eviction can
    // shrink the transcript below a selection taken before the pass.
    let entries = model
        .agents
        .get(ui.focused)
        .map(|a| a.model.transcript.len())
        .unwrap_or(0);
    ui.session_selected = ui
        .session_selected
        .and_then(|sel| (entries > 0).then(|| sel.min(entries - 1)));
}

/// Map one key to a [`DeckAction`]. Pure over `(key, model)`, mutating `ui`.
pub fn handle_deck_key(key: KeyEvent, model: &WorkspaceModel, ui: &mut DeckUi) -> DeckAction {
    if key.kind == KeyEventKind::Release {
        return DeckAction::Ignored;
    }

    let is_ctrl_o =
        key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('o'));
    if !is_ctrl_o {
        ui.ctrl_o_primed = false;
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

    // Ctrl-O: the expand/collapse verb. On a ↑/↓-highlighted message it
    // toggles that message; from the prompt it toggles the most recent
    // expandable message, and a second consecutive press escalates to the
    // all-thinking toggle (chain-of-thought everywhere).
    if is_ctrl_o {
        if let Some(sel) = ui.session_selected {
            // Only a genuinely expandable entry toggles — a no-op press must
            // not bump `expanded_rev` and invalidate the settled fold cache.
            if let Some(agent) = model.agents.get(ui.focused)
                && agent.model.transcript.get(sel).is_some_and(is_expandable)
            {
                let id = agent.meta.id.clone();
                toggle_expanded(ui, &id, sel);
            }
        } else if ui.ctrl_o_primed {
            ui.ctrl_o_primed = false;
            ui.thinking_expanded = !ui.thinking_expanded;
        } else {
            if let Some(agent) = model.agents.get(ui.focused)
                && let Some(idx) = last_expandable(&agent.model.transcript)
            {
                let id = agent.meta.id.clone();
                toggle_expanded(ui, &id, idx);
            }
            ui.ctrl_o_primed = true;
        }
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

    // Deck-global tab navigation (Tab / Shift-Tab only — digits never switch
    // tabs: they quick-pick ask-user answers and must be typeable as the
    // first character of a prompt). The slash popup claims Tab first —
    // completion beats tab-cycling while the menu is open.
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

    // Textarea editing beats per-tab navigation once the composer has
    // content: Enter breaks the line / the chord submits, and cursor motion
    // moves through the prompt instead of scrolling the active tab. A blank
    // composer leaves all of these to the tabs (handle_edit_key gates its
    // motion on the buffer internally).
    if !ui.composer.is_blank() {
        match classify_enter(&key, ui.enter_submits) {
            EnterAction::Submit => return dispatch_submission(ui),
            EnterAction::Newline => {
                ui.composer.insert_newline();
                return DeckAction::Handled;
            }
            EnterAction::NotEnter => {}
        }
    }
    if handle_edit_key(key, &mut ui.composer) {
        return DeckAction::Handled;
    }

    // Per-tab navigation for non-typing keys, then composer editing.
    match ui.tab {
        DeckTab::Agents => handle_agents_key(key, model, ui, composer_empty),
        DeckTab::Traces => handle_traces_key(key, model, ui, composer_empty),
        DeckTab::Graph => handle_graph_key(key, ui, composer_empty),
        DeckTab::Files => handle_files_key(key, model, ui, composer_empty),
        DeckTab::Session => handle_session_key(key, model, ui),
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
///
/// Deck-local commands (tab switches, the help overlay) are intercepted here
/// and act on the UI directly; everything else is enqueued for the driver,
/// which owns the session-level vocabulary (`/clear`, `/models`, `/init`, …).
fn handle_slash_key(key: KeyEvent, matches: &[String], ui: &mut DeckUi) -> Option<DeckAction> {
    match handle_slash_popup_key(key, matches, &mut ui.composer, &mut ui.slash_selected)? {
        SlashPopupOutcome::Handled => Some(DeckAction::Handled),
        SlashPopupOutcome::Submit(text) => Some(match text.as_str() {
            // Only the tab-switch commands are deck-local (they change view
            // state the driver has no say over). `/diff` opens the diff
            // viewer; `/files` shows the file tree, so it must also *close* a
            // diff left open from a prior view.
            "/files" | "/diff" => {
                ui.set_tab(DeckTab::Files);
                ui.files_diff_open = text == "/diff";
                DeckAction::Handled
            }
            "/graph" => {
                ui.set_tab(DeckTab::Graph);
                DeckAction::Handled
            }
            // Everything else — including `/help` — is enqueued for the
            // driver, which owns the session vocabulary and answers into the
            // transcript (a transient overlay would leave no record).
            _ => DeckAction::Send(WorkspaceInput::Enqueue { text }),
        }),
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
        KeyCode::Char('x') if ctrl => DeckAction::Send(WorkspaceInput::QueueRemove {
            index: ui.queue_sel,
        }),
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
            // The submit chord dispatches the typed free text as the answer.
            // A plain `⏎` is NOT claimed — it falls through to composer
            // editing, so the answer can span lines. A `!` line is a shell
            // command even while a question is pending — it must run
            // immediately, not be swallowed as the answer.
            KeyCode::Enter
                if classify_enter(&key, ui.enter_submits) == EnterAction::Submit
                    && !ui.composer.buffer().trim_start().starts_with('!') =>
            {
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
            ui.focus_agent(ui.focused.saturating_sub(1));
            Some(DeckAction::Handled)
        }
        KeyCode::Down => {
            if count > 0 {
                ui.focus_agent((ui.focused + 1).min(count - 1));
            }
            Some(DeckAction::Handled)
        }
        KeyCode::Enter if composer_empty && key.modifiers.is_empty() => {
            ui.set_tab(DeckTab::Session);
            Some(DeckAction::Handled)
        }
        // Agent controls — only when the composer is empty (else they type).
        // Only `s` (stop) is bound: the driver drops Pause/Restart as no-ops
        // (they need the fleet supervisor), and a key that visibly does
        // nothing erodes trust in the ones that work. Re-add `p`/`r` here
        // and in the help overlay when the driver honors them.
        KeyCode::Char('s') if composer_empty => model.agents.get(ui.focused).map(|entry| {
            DeckAction::Send(WorkspaceInput::Control {
                agent: entry.meta.id.clone(),
                control: AgentControl::Stop,
            })
        }),
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

fn handle_files_key(
    key: KeyEvent,
    model: &WorkspaceModel,
    ui: &mut DeckUi,
    composer_empty: bool,
) -> Option<DeckAction> {
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
        // Only an unmodified Enter with nothing typed toggles the diff — a
        // failed submit chord or a prompt in progress must never claim it.
        KeyCode::Enter if count > 0 && composer_empty && key.modifiers.is_empty() => {
            ui.files_diff_open = !ui.files_diff_open;
            ui.files_diff_scroll = ScrollState::default();
            Some(DeckAction::Handled)
        }
        _ => None,
    }
}

/// Flip entry `idx`'s expanded state for `agent`, invalidating the fold cache.
fn toggle_expanded(ui: &mut DeckUi, agent: &str, idx: usize) {
    let set = ui.expanded.entry(agent.to_string()).or_default();
    if !set.remove(&idx) {
        set.insert(idx);
    }
    ui.expanded_rev += 1;
}

/// Whether `ctrl+o` can meaningfully expand this entry — exactly the variants
/// whose [`crate::render::entry_lines`] rendering honors the expanded flag: a
/// tool call (full args), a tool result (full output), or a collapsed thought.
fn is_expandable(entry: &crate::model::TranscriptEntry) -> bool {
    use crate::model::TranscriptEntry as E;
    matches!(
        entry,
        E::ToolStart { .. } | E::ToolResult { .. } | E::Reasoning(_)
    )
}

/// The most recent transcript entry `ctrl+o` can meaningfully expand.
fn last_expandable(transcript: &[crate::model::TranscriptEntry]) -> Option<usize> {
    transcript.iter().rposition(is_expandable)
}

fn handle_session_key(
    key: KeyEvent,
    model: &WorkspaceModel,
    ui: &mut DeckUi,
) -> Option<DeckAction> {
    let (total, height) = (ui.metrics.session_total, ui.metrics.session_height);
    let entries = model
        .agents
        .get(ui.focused)
        .map(|a| a.model.transcript.len())
        .unwrap_or(0);
    match key.code {
        // ↑/↓ walk a message highlight through the transcript (the scroll
        // window follows the highlight); ↓ past the last message drops the
        // highlight and re-arms tail-follow. PgUp/PgDn stay pure scroll.
        KeyCode::Up if entries > 0 => {
            let sel = match ui.session_selected {
                None => entries - 1,
                Some(0) => 0,
                Some(i) => i - 1,
            };
            ui.session_selected = Some(sel);
            ui.session_pending_scroll = Some(sel);
            Some(DeckAction::Handled)
        }
        KeyCode::Down if entries > 0 => {
            match ui.session_selected {
                Some(i) if i + 1 < entries => {
                    ui.session_selected = Some(i + 1);
                    ui.session_pending_scroll = Some(i + 1);
                }
                Some(_) => {
                    ui.session_selected = None;
                    ui.session_scroll.follow = true;
                }
                None => ui.session_scroll.scroll_down(1, total, height),
            }
            Some(DeckAction::Handled)
        }
        KeyCode::Esc if ui.session_selected.is_some() => {
            ui.session_selected = None;
            Some(DeckAction::Handled)
        }
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

/// Dispatch the composer's content: a `!`-prefixed line is a shell command
/// that executes IMMEDIATELY, bypassing the prompt queue and any busy agent
/// entirely; any other prompt ALWAYS enqueues — never blocks on a busy agent.
fn dispatch_submission(ui: &mut DeckUi) -> DeckAction {
    match ui.composer.take_submission() {
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
        Some(text) => DeckAction::Send(WorkspaceInput::Enqueue { text }),
        None => DeckAction::Ignored,
    }
}

/// The always-available composer editing + non-blocking submit. (A non-blank
/// composer's Enter/motion keys were already handled by [`handle_deck_key`]'s
/// textarea interception; this fallback covers typing plus the blank-composer
/// Enter, which submits nothing and inserts nothing.)
fn handle_composer_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match classify_enter(&key, ui.enter_submits) {
        EnterAction::Submit => return dispatch_submission(ui),
        // No line breaks into a fully blank composer — a stray leading
        // newline is never what an empty ⏎ meant.
        EnterAction::Newline => {
            return if ui.composer.is_blank() {
                DeckAction::Ignored
            } else {
                ui.composer.insert_newline();
                DeckAction::Handled
            };
        }
        EnterAction::NotEnter => {}
    }
    match key.code {
        KeyCode::Backspace => {
            ui.composer.backspace();
            ui.slash_selected = 0;
            DeckAction::Handled
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META) =>
        {
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
    /// The submit chord — `⌘⏎` as the kitty keyboard protocol reports it.
    fn cmd_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SUPER)
    }
    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn model_with(ids: &[&str]) -> WorkspaceModel {
        let mut m = WorkspaceModel::new();
        for id in ids {
            m.apply_inbound(&Inbound::Register(AgentMeta::new(*id, *id, 0)));
        }
        m
    }

    /// Push one tool call + multi-line result onto `agent`'s transcript.
    fn with_tool_exchange(m: &mut WorkspaceModel, agent: &str) {
        use stella_protocol::{AgentEvent, ToolCall, ToolOutput};
        m.apply_inbound(&Inbound::Event {
            agent: agent.into(),
            event: AgentEvent::ToolStart {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({ "path": "src/main.rs" }),
                },
            },
        });
        m.apply_inbound(&Inbound::Event {
            agent: agent.into(),
            event: AgentEvent::ToolResult {
                call_id: "c1".into(),
                output: ToolOutput::Ok {
                    content: "line one\nline two\nline three".into(),
                },
                duration_ms: 7,
            },
        });
    }

    #[test]
    fn up_selects_the_last_message_and_ctrl_o_toggles_it() {
        let mut model = model_with(&["lead"]);
        with_tool_exchange(&mut model, "lead");
        let mut ui = ready_ui();

        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert_eq!(
            ui.session_selected,
            Some(1),
            "up highlights the newest message"
        );

        handle_deck_key(ctrl('o'), &model, &mut ui);
        assert!(
            ui.expanded.get("lead").is_some_and(|set| set.contains(&1)),
            "ctrl+o expands the highlighted message"
        );
        let rev = ui.expanded_rev;
        handle_deck_key(ctrl('o'), &model, &mut ui);
        assert!(
            ui.expanded.get("lead").is_none_or(|set| !set.contains(&1)),
            "a second ctrl+o collapses it again"
        );
        assert!(
            ui.expanded_rev > rev,
            "each toggle invalidates the fold cache"
        );
    }

    #[test]
    fn double_ctrl_o_from_the_prompt_toggles_all_thinking() {
        let mut model = model_with(&["lead"]);
        with_tool_exchange(&mut model, "lead");
        let mut ui = ready_ui();
        assert!(!ui.thinking_expanded);

        // First press (no selection): toggles the most recent expandable
        // message and arms the escalation…
        handle_deck_key(ctrl('o'), &model, &mut ui);
        assert!(ui.ctrl_o_primed);
        assert!(!ui.thinking_expanded);
        // …second consecutive press: the all-thinking toggle.
        handle_deck_key(ctrl('o'), &model, &mut ui);
        assert!(
            ui.thinking_expanded,
            "ctrl+o twice = chain-of-thought everywhere"
        );

        // Any other key disarms the escalation.
        handle_deck_key(ctrl('o'), &model, &mut ui);
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert!(
            !ui.ctrl_o_primed,
            "a non-ctrl+o key disarms the double-press"
        );
    }

    #[test]
    fn down_past_the_last_message_clears_selection_and_rearms_follow() {
        let mut model = model_with(&["lead"]);
        with_tool_exchange(&mut model, "lead");
        let mut ui = ready_ui();
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.session_selected, None, "down past the tail deselects");
        assert!(ui.session_scroll.follow, "…and re-arms tail-follow");
    }

    #[test]
    fn ctrl_o_on_a_non_expandable_selection_is_a_no_op() {
        let mut model = model_with(&["lead"]);
        // A plain text message — `entry_lines` ignores the expanded flag for
        // it, so ctrl+o has nothing to toggle.
        model.apply_inbound(&Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Text {
                delta: "hello".into(),
            },
        });
        let mut ui = ready_ui();
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert_eq!(ui.session_selected, Some(0));

        let rev = ui.expanded_rev;
        handle_deck_key(ctrl('o'), &model, &mut ui);
        assert!(
            ui.expanded.get("lead").is_none_or(|set| set.is_empty()),
            "nothing marked expanded"
        );
        assert_eq!(
            ui.expanded_rev, rev,
            "a no-op press must not invalidate the settled fold cache"
        );
    }

    #[test]
    fn switching_focus_drops_the_session_selection() {
        let mut model = model_with(&["lead", "sub"]);
        with_tool_exchange(&mut model, "lead");
        let mut ui = ready_ui();
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert!(ui.session_selected.is_some());

        // Focus another agent from the Agents tab: the selection indexes the
        // *previous* agent's transcript and must not carry across.
        ui.tab = DeckTab::Agents;
        handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        assert_eq!(ui.focused, 1);
        assert_eq!(
            ui.session_selected, None,
            "selection cleared on focus change"
        );
        assert!(ui.session_scroll.follow, "…and tail-follow re-arms");
    }

    fn ready_ui() -> DeckUi {
        let mut ui = DeckUi::default();
        ui.splash.skip(); // past the splash for interaction tests
        ui
    }

    #[test]
    fn eviction_clamps_the_selection_and_drops_stale_expansions() {
        use crate::model::MAX_TRANSCRIPT_ENTRIES;
        let mut model = model_with(&["lead"]);
        let mut ui = ready_ui();
        let retry = |i: usize| Inbound::Event {
            agent: "lead".into(),
            event: AgentEvent::Retry {
                attempt: i as u32,
                reason: "r".into(),
            },
        };
        // Grow to just under the cap, then highlight + expand near the tail.
        for i in 0..(MAX_TRANSCRIPT_ENTRIES - 1) {
            ingest_inbound(&retry(i), &mut model, &mut ui);
        }
        ui.session_selected = Some(MAX_TRANSCRIPT_ENTRIES - 2);
        toggle_expanded(&mut ui, "lead", MAX_TRANSCRIPT_ENTRIES - 2);
        let rev = ui.expanded_rev;

        // One more event crosses the cap: a chunk of the front evicts.
        ingest_inbound(&retry(MAX_TRANSCRIPT_ENTRIES), &mut model, &mut ui);
        let len = model.agents[0].model.transcript.len();
        assert!(len < MAX_TRANSCRIPT_ENTRIES, "a chunk was evicted");
        assert!(
            ui.session_selected.is_some_and(|sel| sel < len),
            "selection clamped into the retained window"
        );
        assert!(
            !ui.expanded.contains_key("lead"),
            "index-keyed expansions are stale once the front moved"
        );
        assert!(ui.expanded_rev > rev, "fold cache invalidated");
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
    fn only_tab_switches_tabs_and_digits_always_type() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        assert_eq!(ui.tab, DeckTab::Session);
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Agents);
        handle_deck_key(key(KeyCode::BackTab), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Session);
        // A digit with an empty composer starts the prompt — it never jumps
        // to a tab, so prompts can begin with 1–5.
        handle_deck_key(ch('3'), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Session, "digit typed, tab unchanged");
        handle_deck_key(ch('h'), &model, &mut ui);
        handle_deck_key(ch('2'), &model, &mut ui);
        assert_eq!(ui.composer.buffer(), "3h2");
    }

    #[test]
    fn submit_chord_always_enqueues_a_prompt_without_blocking() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        for c in "do the thing".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(cmd_enter(), &model, &mut ui);
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
    fn plain_enter_inserts_a_line_break_preserved_through_submit() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        for c in "line one".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert_eq!(
            handle_deck_key(key(KeyCode::Enter), &model, &mut ui),
            DeckAction::Handled,
            "plain ⏎ is a line break, not a submit"
        );
        for c in "line two".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(cmd_enter(), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::Enqueue {
                text: "line one\nline two".into()
            }),
            "the typed line break survives into the submitted prompt"
        );
    }

    #[test]
    fn plain_enter_on_a_blank_composer_inserts_nothing() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.tab = DeckTab::Graph; // a tab with no Enter binding of its own
        assert_eq!(
            handle_deck_key(key(KeyCode::Enter), &model, &mut ui),
            DeckAction::Ignored
        );
        assert!(ui.composer.buffer().is_empty(), "no stray leading newline");
    }

    #[test]
    fn alt_brackets_jump_the_cursor_to_start_and_end() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        for c in "abc".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        assert_eq!(ui.composer.cursor(), 3);
        assert_eq!(
            handle_deck_key(alt('['), &model, &mut ui),
            DeckAction::Handled
        );
        assert_eq!(ui.composer.cursor(), 0, "⌥[ → before the first character");
        assert_eq!(
            handle_deck_key(alt(']'), &model, &mut ui),
            DeckAction::Handled
        );
        assert_eq!(ui.composer.cursor(), 3, "⌥] → one past the last character");
    }

    #[test]
    fn legacy_terminals_fall_back_to_enter_submits_and_alt_enter_breaks() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.enter_submits = true; // no kitty keyboard protocol
        for c in "hi".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(
            handle_deck_key(alt_enter, &model, &mut ui),
            DeckAction::Handled,
            "⌥⏎ is the legacy line break"
        );
        assert_eq!(ui.composer.buffer(), "hi\n");
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::Enqueue {
                text: "hi\n".into()
            }),
            "plain ⏎ submits when the chord is unreportable"
        );
    }

    #[test]
    fn arrow_keys_edit_a_multiline_prompt_instead_of_scrolling() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        for c in "ab".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        for c in "cd".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        // ↑ moves the cursor into the first line (not the session scroll,
        // and NOT the queue editor — the composer is not empty).
        handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        assert_eq!(ui.composer.cursor(), 2, "column kept on the line above");
        handle_deck_key(key(KeyCode::Left), &model, &mut ui);
        handle_deck_key(ch('X'), &model, &mut ui);
        assert_eq!(ui.composer.buffer(), "aXb\ncd", "typed at the cursor");
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
        let action = handle_deck_key(cmd_enter(), &model, &mut ui);
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
        let action = handle_deck_key(cmd_enter(), &model, &mut ui);
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
        let action = handle_deck_key(cmd_enter(), &model, &mut ui);
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
        assert_eq!(
            handle_deck_key(ctrl('d'), &model, &mut ui),
            DeckAction::Handled
        );
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

    #[test]
    fn slash_files_switches_to_files_and_closes_an_open_diff() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.slash_commands = vec![SlashCommand::new("/files", "files")];
        ui.files_diff_open = true; // a diff was left open from a prior view
        for c in "/files".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let action = handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(action, DeckAction::Handled, "/files is consumed locally");
        assert_eq!(ui.tab, DeckTab::Files);
        assert!(!ui.files_diff_open, "/files shows the tree, not a diff");
    }

    #[test]
    fn slash_diff_switches_to_files_and_opens_the_diff() {
        let model = model_with(&["lead"]);
        let mut ui = ready_ui();
        ui.slash_commands = vec![SlashCommand::new("/diff", "diff")];
        for c in "/diff".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(ui.tab, DeckTab::Files);
        assert!(ui.files_diff_open, "/diff opens the viewer");
    }

    #[test]
    fn a_refreshed_graph_snapshot_updates_the_view_out_of_band() {
        use crate::graph::{GraphNode, GraphSnapshot};
        let mut model = model_with(&["lead"]);
        let mut ui = ready_ui();
        assert!(ui.graph.is_none());
        let snapshot = GraphSnapshot {
            focus: "src/lib.rs".into(),
            nodes: vec![GraphNode {
                label: "src/lib.rs".into(),
                kind: "file".into(),
                location: Some("src/lib.rs".into()),
            }],
            edges: vec![],
        };
        ingest_inbound(
            &Inbound::GraphSnapshot(snapshot.clone()),
            &mut model,
            &mut ui,
        );
        assert_eq!(ui.graph.as_ref(), Some(&snapshot));
    }
}
