//! The ENGINE panel — the config editor the SETTINGS tab hosts full-width,
//! for `settings.json` → `agent_engine_config`: the global routing toggles
//! plus the per-agent model / prompt / sampling overrides for the four
//! pipeline agents (default · worker · judge · triage). Formerly the
//! `/engine` popup, then the AGENTS tab's right column; the same content now
//! lives on SETTINGS ([`crate::views::settings`]), the home of all config.
//!
//! Ownership mirrors the MCP and SKILLS surfaces: the **driver** owns the
//! settings files on disk and pushes [`crate::envelope::Inbound::EngineConfig`]
//! snapshots (at startup, after every save, and on request); the deck edits a
//! **working copy** in memory and sends it back whole via
//! [`WorkspaceInput::EngineConfigSave`] — the driver merges the
//! `agent_engine_config` object into the chosen scope's `settings.json`
//! (preserving every other key) and answers with a fresh snapshot whose
//! `status` carries the outcome. A `pristine` twin of the last adopted
//! snapshot gives the panel an honest "modified" marker and lets
//! [`ingest_config`] tell a benign refresh (safe to adopt) from one that
//! would clobber unsaved edits (kept until saved — or until focus leaves
//! the panel and the next snapshot arrives, which is the deliberate discard
//! path).
//!
//! Interaction follows the queue-editor contract: **modal while focused**
//! (`e` on the SETTINGS tab focuses; Esc hands the keyboard back to the
//! tab). Every key is claimed by [`handle_engine_key`] while focused, so the
//! letter verbs (`s`/`S`/`x`/`r`), the inline edit buffer, and the
//! model-picker filter can never leak a keystroke into the global composer.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use crate::deck::DeckTab;
use crate::deck_ui::{DeckAction, DeckUi};
use crate::envelope::{
    AgentScope, EngineAgentState, EngineConfigState, EngineRole, WorkspaceInput,
};
use crate::render::scroll_window_start;
use crate::theme;

/// The legal `effort` values, in cycle order (⏎ walks them, then wraps to
/// "provider default"). This is the FULL vocabulary — the fallback when
/// the selected model's own levels are unknown; [`effort_values_for`]
/// narrows it to what the model/provider pair actually supports.
const EFFORT_VALUES: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The effort levels `role`'s currently-selected model can act on, from
/// the driver-computed `model_efforts` map. Lookup tries the model string
/// verbatim (the picker writes `provider/slug`), then provider-qualified,
/// then any provider serving that bare slug. Unknown models keep the full
/// vocabulary — unknown must never restrict. `Some(vec![])` means effort
/// is genuinely not a knob for this model.
fn effort_values_for(state: &EngineConfigState, role: EngineRole) -> Vec<String> {
    let full = || EFFORT_VALUES.iter().map(|s| s.to_string()).collect();
    let Some(agent) = state.agent(role) else {
        return full();
    };
    let Some(model) = agent
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    else {
        return full();
    };
    let qualified = agent
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p}/{model}"));
    let suffix = format!("/{model}");
    let hit = state
        .model_efforts
        .get(model)
        .or_else(|| qualified.as_ref().and_then(|q| state.model_efforts.get(q)))
        .or_else(|| {
            state
                .model_efforts
                .iter()
                .find(|(spec, _)| spec.ends_with(&suffix))
                .map(|(_, levels)| levels)
        });
    match hit {
        Some(levels) => levels.clone(),
        None => full(),
    }
}
/// The legal `verbosity` values.
const VERBOSITY_VALUES: [&str; 3] = ["low", "medium", "high"];
/// The legal `service_tier` values.
const SERVICE_TIER_VALUES: [&str; 4] = ["auto", "default", "flex", "priority"];

/// Hint shown when an action needs the config snapshot the driver has not
/// delivered yet (a race right after startup, or a driver error).
const NO_SNAPSHOT_HINT: &str = "waiting for the engine config snapshot — r to reload";

/// Which tab of the overlay has the keyboard: the GLOBAL toggles or one of
/// the four per-agent override pages. GLOBAL comes first — the cross-agent
/// switches are what the panel usually opens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineTab {
    #[default]
    Global,
    Agent(EngineRole),
}

impl EngineTab {
    /// Display/cycle order: GLOBAL, then the agents in
    /// [`EngineRole::ALL`] order.
    pub const ALL: [EngineTab; 5] = [
        EngineTab::Global,
        EngineTab::Agent(EngineRole::Default),
        EngineTab::Agent(EngineRole::Worker),
        EngineTab::Agent(EngineRole::Judge),
        EngineTab::Agent(EngineRole::Triage),
    ];

    fn index(self) -> usize {
        EngineTab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    /// The next tab, wrapping past the last back to GLOBAL.
    pub fn next(self) -> Self {
        EngineTab::ALL[(self.index() + 1) % EngineTab::ALL.len()]
    }

    /// The previous tab, wrapping before GLOBAL to the last agent.
    pub fn prev(self) -> Self {
        let n = EngineTab::ALL.len();
        EngineTab::ALL[(self.index() + n - 1) % n]
    }

    /// The header label — the settings key for agents, `GLOBAL` otherwise.
    pub fn label(self) -> &'static str {
        match self {
            EngineTab::Global => "GLOBAL",
            EngineTab::Agent(role) => role.key(),
        }
    }
}

/// The GLOBAL tab's rows, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalRow {
    AutoMode,
    EffortAuto,
    ReasoningAuto,
    AllowedModels,
}

const GLOBAL_ROWS: [GlobalRow; 4] = [
    GlobalRow::AutoMode,
    GlobalRow::EffortAuto,
    GlobalRow::ReasoningAuto,
    GlobalRow::AllowedModels,
];

impl GlobalRow {
    fn label(self) -> &'static str {
        match self {
            GlobalRow::AutoMode => "auto_mode",
            GlobalRow::EffortAuto => "effort_auto",
            GlobalRow::ReasoningAuto => "reasoning_auto",
            GlobalRow::AllowedModels => "allowed_models",
        }
    }
}

/// One editable field of [`EngineAgentState`], in the struct's declaration
/// order — the agent tabs render exactly one row per variant, so the screen
/// order and the settings order can never drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentField {
    Model,
    Provider,
    Prompt,
    Effort,
    Reasoning,
    Temperature,
    TopP,
    TopK,
    FrequencyPenalty,
    PresencePenalty,
    RepetitionPenalty,
    MaxTokens,
    Seed,
    Verbosity,
    ServiceTier,
}

impl AgentField {
    pub const ALL: [AgentField; 15] = [
        AgentField::Model,
        AgentField::Provider,
        AgentField::Prompt,
        AgentField::Effort,
        AgentField::Reasoning,
        AgentField::Temperature,
        AgentField::TopP,
        AgentField::TopK,
        AgentField::FrequencyPenalty,
        AgentField::PresencePenalty,
        AgentField::RepetitionPenalty,
        AgentField::MaxTokens,
        AgentField::Seed,
        AgentField::Verbosity,
        AgentField::ServiceTier,
    ];

    /// The settings key — doubles as the row label so what the user reads is
    /// exactly what lands in `settings.json`.
    pub fn label(self) -> &'static str {
        match self {
            AgentField::Model => "model",
            AgentField::Provider => "provider",
            AgentField::Prompt => "prompt",
            AgentField::Effort => "effort",
            AgentField::Reasoning => "reasoning",
            AgentField::Temperature => "temperature",
            AgentField::TopP => "top_p",
            AgentField::TopK => "top_k",
            AgentField::FrequencyPenalty => "frequency_penalty",
            AgentField::PresencePenalty => "presence_penalty",
            AgentField::RepetitionPenalty => "repetition_penalty",
            AgentField::MaxTokens => "max_tokens",
            AgentField::Seed => "seed",
            AgentField::Verbosity => "verbosity",
            AgentField::ServiceTier => "service_tier",
        }
    }
}

/// An in-progress inline edit: which row it belongs to and the live buffer.
/// The buffer is committed on ⏎ (parsed per field type; a parse failure
/// keeps the edit alive with a hint rather than half-applying) and dropped
/// on Esc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineEdit {
    /// The row index (within the current tab) the buffer edits.
    pub row: usize,
    pub buffer: String,
}

/// The model-picker sub-overlay's state: a filter-as-you-type query over the
/// allowed models (falling back to the seed catalog when no restriction is
/// configured) — the graph tab's file-picker idiom.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelPicker {
    pub query: String,
    /// Selected row, indexing the *filtered* match list. Reset to 0 on every
    /// query edit (the match set changed under it).
    pub sel: usize,
}

/// All engine-panel view state (a field on [`DeckUi`]). The config itself
/// is driver-owned — `state` is the working copy being edited and `pristine`
/// the last snapshot adopted from the driver; everything else is ephemeral
/// interaction state. The panel is the full-width body of the SETTINGS tab
/// (no popup, no `/engine` command): `focused` is what routes the keyboard
/// to it.
#[derive(Debug, Clone, Default)]
pub struct EngineOverlay {
    /// Whether the panel owns the keyboard (modal while set, on the AGENT
    /// ENGINE tab only — `e` focuses, Esc returns focus to the left column).
    pub focused: bool,
    /// The working copy being edited. `None` until the first snapshot lands.
    pub state: Option<EngineConfigState>,
    /// The last driver snapshot adopted verbatim — `state != pristine` is
    /// the "modified" marker and the unsaved-edits guard in [`ingest_config`].
    pub pristine: Option<EngineConfigState>,
    pub tab: EngineTab,
    /// Selected row within the current tab, clamped on tab switches.
    pub row: usize,
    /// The active inline edit, if any (claims keys ahead of navigation).
    pub edit: Option<EngineEdit>,
    /// The model-picker sub-overlay, if open (claims keys ahead of the edit).
    pub picker: Option<ModelPicker>,
    /// One-line hint: driver save/refresh outcomes, local parse errors.
    pub status: Option<String>,
    /// A save/refresh is in flight driver-side — cleared when the next
    /// [`crate::envelope::Inbound::EngineConfig`] folds back.
    pub busy: bool,
}

impl EngineOverlay {
    /// Rows on the current tab (the ↑/↓ clamp bound).
    pub fn row_count(&self) -> usize {
        match self.tab {
            EngineTab::Global => GLOBAL_ROWS.len(),
            EngineTab::Agent(_) => AgentField::ALL.len(),
        }
    }

    /// Whether the working copy has unsaved local edits.
    pub fn dirty(&self) -> bool {
        self.state != self.pristine
    }

    /// The agent the current tab edits, if it is an agent tab.
    pub fn role(&self) -> Option<EngineRole> {
        match self.tab {
            EngineTab::Agent(role) => Some(role),
            EngineTab::Global => None,
        }
    }
}

// ── driver snapshot ingest ──────────────────────────────────────────────────

/// Fold one [`crate::envelope::Inbound::EngineConfig`] snapshot. Adopted as
/// both `pristine` + working copy unless the overlay is open with unsaved
/// edits — a background refresh must never eat what the user typed. The one
/// exception inside a dirty overlay is a snapshot that **equals** the working
/// copy (the echo of our own save): adopting it re-baselines `pristine`, so
/// the "modified" marker clears the moment the driver confirms the write.
/// `status` (save outcomes, errors) always lands, and `busy` always clears —
/// a snapshot is the completion signal for whatever op was in flight.
pub fn ingest_config(ui: &mut DeckUi, state: &EngineConfigState, status: &Option<String>) {
    let e = &mut ui.engine;
    let echoes_working = e.state.as_ref() == Some(state);
    if !e.focused || !e.dirty() || echoes_working {
        e.state = Some(state.clone());
        e.pristine = Some(state.clone());
    }
    if let Some(status) = status {
        e.status = Some(status.clone());
    }
    e.busy = false;
}

// ── focusers (`e` on the SETTINGS tab) ─────────────────────────────────────

/// Focus the engine panel (switching to the SETTINGS tab if needed) on
/// the GLOBAL tab, and ask the driver to re-read the settings chain so the
/// panel reflects disk truth (the reply is dirty-guarded by
/// [`ingest_config`], so refocusing over unsaved edits is safe).
pub fn focus_panel(ui: &mut DeckUi) -> DeckAction {
    ui.set_tab(DeckTab::Settings);
    let e = &mut ui.engine;
    e.focused = true;
    e.tab = EngineTab::Global;
    e.row = 0;
    e.edit = None;
    e.picker = None;
    e.busy = true;
    DeckAction::Send(WorkspaceInput::EngineConfigRefresh)
}

/// Open the model picker for `role`, pre-selecting the agent's current model
/// among the candidates (the graph picker's "start where you already are").
fn open_picker(e: &mut EngineOverlay, role: EngineRole) {
    let sel = e
        .state
        .as_ref()
        .and_then(|state| {
            let current = state.agent(role).and_then(|a| a.model.as_deref())?;
            picker_candidates(state)
                .iter()
                .position(|c| c.as_str() == current)
        })
        .unwrap_or(0);
    e.picker = Some(ModelPicker {
        query: String::new(),
        sel,
    });
}

/// The picker's vocabulary: `allowed_models` when a restriction is
/// configured, otherwise the whole seed catalog.
fn picker_candidates(state: &EngineConfigState) -> &[String] {
    if state.allowed_models.is_empty() {
        &state.catalog_models
    } else {
        &state.allowed_models
    }
}

/// Case-insensitive substring filter over the candidates — the exact
/// semantics of [`crate::graph::GraphSnapshot::matching_files`], so both
/// pickers feel identical.
fn picker_matches(state: &EngineConfigState, query: &str) -> Vec<String> {
    let needle = query.trim().to_lowercase();
    picker_candidates(state)
        .iter()
        .filter(|m| needle.is_empty() || m.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

// ── key handling ────────────────────────────────────────────────────────────

/// The overlay's modal key map, dispatched by [`crate::deck_ui::handle_deck_key`]
/// while `ui.engine.focused`. Precedence within the overlay: the picker owns the
/// keyboard when open, then an active inline edit, then navigation — the same
/// innermost-context-first ladder the deck itself uses.
pub fn handle_engine_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    if ui.engine.picker.is_some() {
        return handle_picker_key(key, ui);
    }
    if ui.engine.edit.is_some() {
        return handle_inline_edit_key(key, ui);
    }
    handle_nav_key(key, ui)
}

/// The model picker's keys: type to filter, ↑/↓ walk the matches, ⏎ applies
/// the picked slug to the current agent's `model`, Esc closes the picker
/// only (the overlay stays up).
fn handle_picker_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    // Snapshot the filtered matches up front so bounds and the picked slug
    // can never disagree with what render showed.
    let matches: Vec<String> = match (&ui.engine.state, &ui.engine.picker) {
        (Some(state), Some(picker)) => picker_matches(state, &picker.query),
        _ => Vec::new(),
    };
    let count = matches.len();
    match key.code {
        KeyCode::Esc => {
            ui.engine.picker = None;
            DeckAction::Handled
        }
        KeyCode::Up => {
            if let Some(p) = ui.engine.picker.as_mut() {
                p.sel = p.sel.saturating_sub(1);
            }
            DeckAction::Handled
        }
        KeyCode::Down => {
            if let Some(p) = ui.engine.picker.as_mut()
                && count > 0
            {
                p.sel = (p.sel + 1).min(count - 1);
            }
            DeckAction::Handled
        }
        KeyCode::Enter => {
            let sel = ui.engine.picker.as_ref().map(|p| p.sel).unwrap_or(0);
            let picked = matches.get(sel.min(count.saturating_sub(1))).cloned();
            ui.engine.picker = None;
            // The filter matched nothing → just close, like the graph picker.
            if let Some(slug) = picked
                && let EngineTab::Agent(role) = ui.engine.tab
                && let Some(state) = ui.engine.state.as_mut()
                && let Some(agent) = agent_mut(state, role)
            {
                agent.model = Some(slug);
            }
            DeckAction::Handled
        }
        KeyCode::Backspace => {
            if let Some(p) = ui.engine.picker.as_mut() {
                p.query.pop();
                p.sel = 0; // the match set changed — re-anchor
            }
            DeckAction::Handled
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META) =>
        {
            if let Some(p) = ui.engine.picker.as_mut() {
                p.query.push(c);
                p.sel = 0; // the match set changed — re-anchor
            }
            DeckAction::Handled
        }
        // Modal: swallow everything else so nothing leaks behind the popup.
        _ => DeckAction::Handled,
    }
}

/// The inline edit's keys: printable/backspace edit the buffer, ⏎ commits
/// (a parse failure keeps the edit alive with a hint), Esc cancels without
/// touching the field.
fn handle_inline_edit_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    match key.code {
        KeyCode::Esc => {
            ui.engine.edit = None;
            DeckAction::Handled
        }
        KeyCode::Enter => commit_inline(ui),
        KeyCode::Backspace => {
            if let Some(e) = ui.engine.edit.as_mut() {
                e.buffer.pop();
            }
            DeckAction::Handled
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META) =>
        {
            if let Some(e) = ui.engine.edit.as_mut() {
                e.buffer.push(c);
            }
            DeckAction::Handled
        }
        _ => DeckAction::Handled,
    }
}

/// Commit the inline buffer into the working copy. Empty clears the field to
/// "provider default" (`None`); numeric fields must parse or the edit stays
/// open with a hint — a half-applied number is worse than a visible error.
fn commit_inline(ui: &mut DeckUi) -> DeckAction {
    let Some(edit) = ui.engine.edit.clone() else {
        return DeckAction::Handled;
    };
    let tab = ui.engine.tab;
    let Some(state) = ui.engine.state.as_mut() else {
        ui.engine.edit = None;
        return DeckAction::Handled;
    };
    let result = match tab {
        // `allowed_models` is the only text-editable GLOBAL row: parse the
        // comma-joined display form back into slugs (empty → no restriction,
        // so the pickers fall back to the catalog).
        EngineTab::Global => {
            state.allowed_models = edit
                .buffer
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            Ok(())
        }
        EngineTab::Agent(role) => {
            let field = AgentField::ALL[edit.row.min(AgentField::ALL.len() - 1)];
            match agent_mut(state, role) {
                Some(agent) => set_field_from_text(agent, field, &edit.buffer),
                None => Ok(()),
            }
        }
    };
    match result {
        Ok(()) => ui.engine.edit = None,
        // Keep editing so the user can fix the buffer (or Esc out).
        Err(hint) => ui.engine.status = Some(hint),
    }
    DeckAction::Handled
}

/// The panel's navigation/verb keys (no picker, no edit active).
fn handle_nav_key(key: KeyEvent, ui: &mut DeckUi) -> DeckAction {
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META);
    match key.code {
        // Return focus to the tab's left column. The working copy stays in
        // memory (refocusing resumes the edits) until the next driver
        // snapshot replaces an unfocused panel's state — see `ingest_config`.
        KeyCode::Esc => {
            ui.engine.focused = false;
            DeckAction::Handled
        }
        KeyCode::Tab | KeyCode::Right => switch_tab(ui, true),
        KeyCode::BackTab | KeyCode::Left => switch_tab(ui, false),
        KeyCode::Up => {
            ui.engine.row = ui.engine.row.saturating_sub(1);
            DeckAction::Handled
        }
        KeyCode::Down => {
            let count = ui.engine.row_count();
            if count > 0 {
                ui.engine.row = (ui.engine.row + 1).min(count - 1);
            }
            DeckAction::Handled
        }
        KeyCode::Enter => activate_row(ui, false),
        // Space is the toggle chord only: it flips the on/off rows exactly
        // like ⏎ but deliberately does NOT open pickers/edits — a stray
        // space must not drop the user into an input they didn't ask for.
        KeyCode::Char(' ') if plain => activate_row(ui, true),
        KeyCode::Char('x') if plain => clear_row(ui),
        KeyCode::Char('s') if plain => save(ui, AgentScope::User),
        KeyCode::Char('S') if plain => save(ui, AgentScope::Project),
        KeyCode::Char('r') if plain => refresh(ui),
        // Modal: swallow everything else.
        _ => DeckAction::Handled,
    }
}

/// Cycle to the neighboring tab, keeping the row selection where possible
/// (clamped — the GLOBAL tab is shorter than the agent tabs).
fn switch_tab(ui: &mut DeckUi, forward: bool) -> DeckAction {
    let e = &mut ui.engine;
    e.tab = if forward { e.tab.next() } else { e.tab.prev() };
    e.row = e.row.min(e.row_count().saturating_sub(1));
    DeckAction::Handled
}

/// ⏎ (or, for toggles, space) on the selected row: flip toggles, cycle
/// enums, open the model picker, or start an inline edit — per the row's
/// nature.
fn activate_row(ui: &mut DeckUi, via_space: bool) -> DeckAction {
    if ui.engine.state.is_none() {
        ui.engine.status = Some(NO_SNAPSHOT_HINT.into());
        return DeckAction::Handled;
    }
    let row = ui.engine.row;
    match ui.engine.tab {
        EngineTab::Global => {
            let state = ui.engine.state.as_mut().expect("guarded above");
            match GLOBAL_ROWS[row.min(GLOBAL_ROWS.len() - 1)] {
                GlobalRow::AutoMode => state.auto_mode = !state.auto_mode,
                GlobalRow::EffortAuto => state.effort_auto = !state.effort_auto,
                GlobalRow::ReasoningAuto => state.reasoning_auto = !state.reasoning_auto,
                GlobalRow::AllowedModels => {
                    if via_space {
                        return DeckAction::Handled;
                    }
                    let buffer = state.allowed_models.join(", ");
                    ui.engine.edit = Some(EngineEdit { row, buffer });
                }
            }
        }
        EngineTab::Agent(role) => {
            let field = AgentField::ALL[row.min(AgentField::ALL.len() - 1)];
            match field {
                AgentField::Model => {
                    if via_space {
                        return DeckAction::Handled;
                    }
                    open_picker(&mut ui.engine, role);
                }
                // Reasoning is the tri-state toggle: provider default → on →
                // off → default. Space works here too — it's a toggle.
                AgentField::Reasoning => {
                    let state = ui.engine.state.as_mut().expect("guarded above");
                    if let Some(agent) = agent_mut(state, role) {
                        agent.reasoning = match agent.reasoning {
                            None => Some(true),
                            Some(true) => Some(false),
                            Some(false) => None,
                        };
                    }
                }
                AgentField::Effort | AgentField::Verbosity | AgentField::ServiceTier => {
                    if via_space {
                        return DeckAction::Handled;
                    }
                    let owned_values: Vec<String> = match field {
                        // Model-aware: only the levels this agent's selected
                        // model (as served by its provider) can act on.
                        AgentField::Effort => {
                            let state = ui.engine.state.as_ref().expect("guarded above");
                            effort_values_for(state, role)
                        }
                        AgentField::Verbosity => {
                            VERBOSITY_VALUES.iter().map(|s| s.to_string()).collect()
                        }
                        _ => SERVICE_TIER_VALUES.iter().map(|s| s.to_string()).collect(),
                    };
                    if owned_values.is_empty() {
                        // Effort on a model with no reasoning: explain
                        // instead of a keypress that visibly does nothing,
                        // and drop any stale level so a save can't carry it.
                        let state = ui.engine.state.as_mut().expect("guarded above");
                        if let Some(agent) = agent_mut(state, role) {
                            agent.effort = None;
                        }
                        ui.engine.status = Some(
                            "this model does not support reasoning — effort does not apply".into(),
                        );
                        return DeckAction::Handled;
                    }
                    let values: Vec<&str> = owned_values.iter().map(String::as_str).collect();
                    let state = ui.engine.state.as_mut().expect("guarded above");
                    if let Some(agent) = agent_mut(state, role) {
                        let slot = match field {
                            AgentField::Effort => &mut agent.effort,
                            AgentField::Verbosity => &mut agent.verbosity,
                            _ => &mut agent.service_tier,
                        };
                        cycle_enum(slot, &values);
                    }
                }
                AgentField::Provider => {
                    if via_space {
                        return DeckAction::Handled;
                    }
                    let providers = ui
                        .engine
                        .state
                        .as_ref()
                        .expect("guarded above")
                        .providers
                        .clone();
                    if providers.is_empty() {
                        // Nothing to cycle through — explain instead of a
                        // keypress that visibly does nothing.
                        ui.engine.status = Some(
                            "no providers configured — the driver's settings define them".into(),
                        );
                        return DeckAction::Handled;
                    }
                    let refs: Vec<&str> = providers.iter().map(String::as_str).collect();
                    let state = ui.engine.state.as_mut().expect("guarded above");
                    if let Some(agent) = agent_mut(state, role) {
                        cycle_enum(&mut agent.provider, &refs);
                    }
                }
                // Free-text / numeric rows: start the inline edit seeded
                // with the current value (None seeds empty — committing an
                // untouched empty buffer round-trips back to None).
                _ => {
                    if via_space {
                        return DeckAction::Handled;
                    }
                    let state = ui.engine.state.as_ref().expect("guarded above");
                    let buffer = state
                        .agent(role)
                        .and_then(|a| agent_field_value(a, field))
                        .unwrap_or_default();
                    ui.engine.edit = Some(EngineEdit { row, buffer });
                }
            }
        }
    }
    DeckAction::Handled
}

/// `x`: clear the selected row back to "provider default" (`None`). The
/// GLOBAL booleans have no `None` — clearing means "off" — and clearing
/// `allowed_models` lifts the restriction (pickers fall back to the catalog).
fn clear_row(ui: &mut DeckUi) -> DeckAction {
    let row = ui.engine.row;
    let tab = ui.engine.tab;
    let Some(state) = ui.engine.state.as_mut() else {
        ui.engine.status = Some(NO_SNAPSHOT_HINT.into());
        return DeckAction::Handled;
    };
    match tab {
        EngineTab::Global => match GLOBAL_ROWS[row.min(GLOBAL_ROWS.len() - 1)] {
            GlobalRow::AutoMode => state.auto_mode = false,
            GlobalRow::EffortAuto => state.effort_auto = false,
            GlobalRow::ReasoningAuto => state.reasoning_auto = false,
            GlobalRow::AllowedModels => state.allowed_models.clear(),
        },
        EngineTab::Agent(role) => {
            let field = AgentField::ALL[row.min(AgentField::ALL.len() - 1)];
            if let Some(agent) = agent_mut(state, role) {
                clear_agent_field(agent, field);
            }
        }
    }
    DeckAction::Handled
}

/// `s`/`S`: send the whole working copy to the driver for persistence at
/// `scope`. The request rides `pending_inputs` (drained by the shell after
/// this key) and the reply — a fresh snapshot with the outcome in `status` —
/// clears `busy` and re-baselines the modified marker via [`ingest_config`].
fn save(ui: &mut DeckUi, scope: AgentScope) -> DeckAction {
    let Some(state) = ui.engine.state.clone() else {
        ui.engine.status = Some(NO_SNAPSHOT_HINT.into());
        return DeckAction::Handled;
    };
    ui.engine.busy = true;
    ui.engine.status = Some(format!("saving to {} settings…", scope.label()));
    ui.pending_inputs
        .push(WorkspaceInput::EngineConfigSave { state, scope });
    DeckAction::Handled
}

/// `r`: ask the driver to re-read the settings chain. The reply is
/// dirty-guarded ([`ingest_config`]), so a reload can never eat unsaved
/// edits — save or close first to adopt disk truth over them.
fn refresh(ui: &mut DeckUi) -> DeckAction {
    ui.engine.busy = true;
    ui.engine.status = Some("reloading engine config…".into());
    ui.pending_inputs.push(WorkspaceInput::EngineConfigRefresh);
    DeckAction::Handled
}

// ── field access helpers ────────────────────────────────────────────────────

/// Mutable access to `role`'s slot in `state.agents`. The driver always
/// sends all four ([`EngineRole::ALL`] order), but a short vector (a hand-
/// built scenario, a driver bug) must not silently drop an edit — grow it
/// with defaults instead.
fn agent_mut(state: &mut EngineConfigState, role: EngineRole) -> Option<&mut EngineAgentState> {
    let idx = EngineRole::ALL.iter().position(|r| *r == role)?;
    while state.agents.len() <= idx {
        state.agents.push(EngineAgentState::default());
    }
    state.agents.get_mut(idx)
}

/// The display/edit-seed form of one agent field. `None` = unset (renders
/// dimmed as "(provider default)"; seeds an empty edit buffer).
fn agent_field_value(agent: &EngineAgentState, field: AgentField) -> Option<String> {
    match field {
        AgentField::Model => agent.model.clone(),
        AgentField::Provider => agent.provider.clone(),
        AgentField::Prompt => agent.prompt.clone(),
        AgentField::Effort => agent.effort.clone(),
        AgentField::Reasoning => agent
            .reasoning
            .map(|on| (if on { "on" } else { "off" }).to_string()),
        AgentField::Temperature => agent.temperature.map(|v| v.to_string()),
        AgentField::TopP => agent.top_p.map(|v| v.to_string()),
        AgentField::TopK => agent.top_k.map(|v| v.to_string()),
        AgentField::FrequencyPenalty => agent.frequency_penalty.map(|v| v.to_string()),
        AgentField::PresencePenalty => agent.presence_penalty.map(|v| v.to_string()),
        AgentField::RepetitionPenalty => agent.repetition_penalty.map(|v| v.to_string()),
        AgentField::MaxTokens => agent.max_tokens.map(|v| v.to_string()),
        AgentField::Seed => agent.seed.map(|v| v.to_string()),
        AgentField::Verbosity => agent.verbosity.clone(),
        AgentField::ServiceTier => agent.service_tier.clone(),
    }
}

/// Reset one agent field to "provider default".
fn clear_agent_field(agent: &mut EngineAgentState, field: AgentField) {
    match field {
        AgentField::Model => agent.model = None,
        AgentField::Provider => agent.provider = None,
        AgentField::Prompt => agent.prompt = None,
        AgentField::Effort => agent.effort = None,
        AgentField::Reasoning => agent.reasoning = None,
        AgentField::Temperature => agent.temperature = None,
        AgentField::TopP => agent.top_p = None,
        AgentField::TopK => agent.top_k = None,
        AgentField::FrequencyPenalty => agent.frequency_penalty = None,
        AgentField::PresencePenalty => agent.presence_penalty = None,
        AgentField::RepetitionPenalty => agent.repetition_penalty = None,
        AgentField::MaxTokens => agent.max_tokens = None,
        AgentField::Seed => agent.seed = None,
        AgentField::Verbosity => agent.verbosity = None,
        AgentField::ServiceTier => agent.service_tier = None,
    }
}

/// Apply a committed inline buffer to one field. Empty (after trimming)
/// clears to `None`; numeric fields must parse into their exact type.
fn set_field_from_text(
    agent: &mut EngineAgentState,
    field: AgentField,
    raw: &str,
) -> Result<(), String> {
    let t = raw.trim();
    let text = || {
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };
    match field {
        AgentField::Model => agent.model = text(),
        AgentField::Provider => agent.provider = text(),
        // The prompt keeps the buffer verbatim (minus a wholly-blank one):
        // its inline edit is a single line, and whether whitespace matters
        // inside a system prompt is not this layer's call.
        AgentField::Prompt => {
            agent.prompt = if t.is_empty() {
                None
            } else {
                Some(raw.to_string())
            }
        }
        AgentField::Effort => agent.effort = text(),
        AgentField::Verbosity => agent.verbosity = text(),
        AgentField::ServiceTier => agent.service_tier = text(),
        // ⏎ cycles reasoning in place — it never inline-edits, so a text
        // commit reaching here can only mean "leave it alone".
        AgentField::Reasoning => {}
        AgentField::Temperature => agent.temperature = parse_num::<f32>(t, "temperature")?,
        AgentField::TopP => agent.top_p = parse_num::<f32>(t, "top_p")?,
        AgentField::TopK => agent.top_k = parse_num::<u32>(t, "top_k")?,
        AgentField::FrequencyPenalty => {
            agent.frequency_penalty = parse_num::<f32>(t, "frequency_penalty")?
        }
        AgentField::PresencePenalty => {
            agent.presence_penalty = parse_num::<f32>(t, "presence_penalty")?
        }
        AgentField::RepetitionPenalty => {
            agent.repetition_penalty = parse_num::<f32>(t, "repetition_penalty")?
        }
        AgentField::MaxTokens => agent.max_tokens = parse_num::<u32>(t, "max_tokens")?,
        AgentField::Seed => agent.seed = parse_num::<u64>(t, "seed")?,
    }
    Ok(())
}

/// Parse one numeric buffer: empty → `None` (provider default), otherwise
/// the value or a keep-editing hint.
fn parse_num<T: std::str::FromStr>(t: &str, label: &str) -> Result<Option<T>, String> {
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<T>()
        .map(Some)
        .map_err(|_| format!("{label}: “{t}” does not parse — fix it or Esc to cancel"))
}

/// Cycle an enum-valued field through `values` and back to `None` (provider
/// default) past the end. An unrecognized stored value (hand-edited
/// settings) also wraps to `None` rather than guessing a position.
fn cycle_enum(current: &mut Option<String>, values: &[&str]) {
    let next = match current.as_deref() {
        None => values.first().map(|v| v.to_string()),
        Some(cur) => match values.iter().position(|v| v.eq_ignore_ascii_case(cur)) {
            Some(i) if i + 1 < values.len() => Some(values[i + 1].to_string()),
            _ => None,
        },
    };
    *current = next;
}

// ── render ──────────────────────────────────────────────────────────────────

/// Label column width — fits the longest key (`repetition_penalty`, 18).
const LABEL_W: usize = 19;

/// Render the ENGINE panel into the SETTINGS tab: an area-filling bordered
/// panel (accent border while it owns the keyboard, hairline otherwise),
/// windowed rows with the selection reversed, and the model-picker
/// sub-overlay centered over the panel when open. This is the exact content
/// the old `/engine` popup carried — a permanent config surface now, the
/// full-width body of the SETTINGS tab.
pub fn render_panel(ui: &DeckUi, area: Rect, buf: &mut Buffer) {
    let e = &ui.engine;
    let (w, h) = (area.width, area.height);
    if w < 4 || h < 4 {
        return; // no readable panel fits — draw nothing rather than garbage
    }
    let popup = area;

    let inner_h = (h as usize).saturating_sub(2);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header: GLOBAL + the four agents as tabs, selected one highlighted.
    let mut tabs: Vec<Span<'static>> = vec![Span::raw(" ")];
    for tab in EngineTab::ALL {
        let style = if tab == e.tab {
            theme::accent().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            theme::muted()
        };
        tabs.push(Span::styled(format!(" {} ", tab.label()), style));
        tabs.push(Span::raw(" "));
    }
    lines.push(Line::from(tabs));
    lines.push(Line::default());

    match &e.state {
        None => {
            lines.push(Line::from(Span::styled(
                format!("  {NO_SNAPSHOT_HINT}"),
                theme::muted(),
            )));
        }
        Some(state) => {
            let count = e.row_count();
            let sel = e.row.min(count.saturating_sub(1));
            // Header (1) + blank (1) + status (1) + footer (1) bracket the rows.
            let visible = inner_h.saturating_sub(4).max(1);
            let first = scroll_window_start(count, sel, visible);
            let last = (first + visible).min(count);
            for i in first..last {
                lines.push(render_row(e, state, i, i == sel, w as usize));
            }
        }
    }

    // Pad so the status + footer sit on the last two interior rows.
    while lines.len() < inner_h.saturating_sub(2) {
        lines.push(Line::default());
    }
    // The driver/local status line ("saved", parse errors), or the busy hint.
    let status = e
        .status
        .clone()
        .or_else(|| e.busy.then(|| "working…".to_string()));
    lines.push(match status {
        Some(s) => Line::from(Span::styled(
            format!(" {s}"),
            Style::default().fg(theme::ACCENT),
        )),
        None => Line::default(),
    });
    // The legend tracks focus: while the panel owns the keyboard it teaches
    // its verbs; otherwise it teaches the one key that grants focus.
    lines.push(Line::from(Span::styled(
        if e.focused {
            " tab agent · ⏎ edit · space toggle · x clear · s save user · S save project · r reload · esc done"
        } else {
            " e edit agents config"
        },
        theme::muted(),
    )));

    let title = format!(" agents{} ", if e.dirty() { " · modified" } else { "" });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if e.focused {
            theme::accent()
        } else {
            theme::rule()
        })
        .title(title);
    Paragraph::new(lines).block(block).render(popup, buf);

    if e.picker.is_some() {
        render_model_picker(e, area, buf);
    }
}

/// One config row: `▸ label  value`, the whole line reversed when selected.
/// `None` values render dimmed as "(provider default)"; an active inline
/// edit shows the live buffer with a caret instead of the stored value.
fn render_row(
    e: &EngineOverlay,
    state: &EngineConfigState,
    i: usize,
    is_sel: bool,
    popup_w: usize,
) -> Line<'static> {
    // Borders (2) + marker (2) + label column bound the value width.
    let value_w = popup_w.saturating_sub(2 + 2 + LABEL_W + 1).max(8);
    let (label, value): (&str, Option<String>) = match e.tab {
        EngineTab::Global => {
            let row = GLOBAL_ROWS[i.min(GLOBAL_ROWS.len() - 1)];
            let value = match row {
                GlobalRow::AutoMode => Some(on_off(state.auto_mode)),
                GlobalRow::EffortAuto => Some(on_off(state.effort_auto)),
                GlobalRow::ReasoningAuto => Some(on_off(state.reasoning_auto)),
                GlobalRow::AllowedModels => {
                    if state.allowed_models.is_empty() {
                        None // dimmed placeholder below
                    } else {
                        Some(state.allowed_models.join(", "))
                    }
                }
            };
            (row.label(), value)
        }
        EngineTab::Agent(role) => {
            let field = AgentField::ALL[i.min(AgentField::ALL.len() - 1)];
            let value = state.agent(role).and_then(|a| agent_field_value(a, field));
            (field.label(), value)
        }
    };

    let sel_mod = if is_sel {
        Modifier::REVERSED
    } else {
        Modifier::empty()
    };
    let marker = if is_sel { "▸ " } else { "  " };
    let mut spans = vec![
        Span::styled(
            marker.to_string(),
            Style::default().fg(theme::ACCENT).add_modifier(sel_mod),
        ),
        Span::styled(
            format!("{label:<LABEL_W$}"),
            theme::muted().add_modifier(sel_mod),
        ),
    ];

    if let Some(edit) = e.edit.as_ref().filter(|edit| edit.row == i) {
        // The live buffer, tail-windowed so the caret end stays visible on
        // long values (a prompt), with the violet edit caret.
        let shown: String = tail_chars(&edit.buffer, value_w.saturating_sub(1));
        spans.push(Span::styled(shown, theme::body().add_modifier(sel_mod)));
        spans.push(Span::styled(
            "▏",
            Style::default().fg(theme::VIOLET).add_modifier(sel_mod),
        ));
    } else {
        match value {
            Some(v) => spans.push(Span::styled(
                truncate_chars(&v, value_w),
                Style::default().fg(theme::INK).add_modifier(sel_mod),
            )),
            None => {
                let placeholder = match e.tab {
                    EngineTab::Global => "(none — pickers offer the catalog)",
                    EngineTab::Agent(_) => "(provider default)",
                };
                spans.push(Span::styled(
                    placeholder.to_string(),
                    theme::muted().add_modifier(sel_mod),
                ));
            }
        }
    }
    Line::from(spans)
}

/// The model-picker sub-overlay, centered over the main popup: the graph
/// file picker's exact idiom (filter line, windowed matches, legend).
fn render_model_picker(e: &EngineOverlay, area: Rect, buf: &mut Buffer) {
    let Some(picker) = &e.picker else { return };
    let w = area.width.min(56);
    let h = area.height.min(16);
    if w < 4 || h < 4 {
        return;
    }
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    Clear.render(popup, buf);

    let role_label = e.role().map(EngineRole::key).unwrap_or("agent");
    let current = e
        .role()
        .and_then(|role| e.state.as_ref().and_then(|s| s.agent(role)))
        .and_then(|a| a.model.clone());
    let matches = e
        .state
        .as_ref()
        .map(|s| picker_matches(s, &picker.query))
        .unwrap_or_default();

    let inner_h = (h as usize).saturating_sub(2);
    let visible = inner_h.saturating_sub(2).max(1);
    let sel = picker.sel.min(matches.len().saturating_sub(1));
    let first = scroll_window_start(matches.len(), sel, visible);
    let last = (first + visible).min(matches.len());

    let mut lines: Vec<Line<'static>> = vec![Line::from(vec![
        Span::styled("filter ", theme::muted()),
        Span::styled(picker.query.clone(), theme::body()),
        Span::styled("▏", Style::new().fg(theme::VIOLET)),
    ])];

    if e.state.is_none() {
        lines.push(Line::from(Span::styled(
            format!("  {NO_SNAPSHOT_HINT}"),
            theme::muted(),
        )));
    } else if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no models match — Backspace to widen",
            theme::muted(),
        )));
    }
    for (i, slug) in matches.iter().enumerate().take(last).skip(first) {
        let is_sel = i == sel;
        let marker = if is_sel { "▸ " } else { "  " };
        let mut style = theme::body();
        if is_sel {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let shown = truncate_chars(slug, (w as usize).saturating_sub(6));
        let mut spans = vec![
            Span::styled(marker.to_string(), style.fg(theme::ACCENT)),
            Span::styled(shown, style),
        ];
        if current.as_deref() == Some(slug.as_str()) {
            spans.push(Span::styled("  · current", theme::muted()));
        }
        lines.push(Line::from(spans));
    }

    // Pad so the legend sits on the last interior row regardless of matches.
    while lines.len() < inner_h.saturating_sub(1).max(1) {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        " type to filter · ↑/↓ select · ⏎ pick · esc back",
        theme::muted(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::accent())
        .title(format!(
            " model · {role_label} · {} available ",
            matches.len()
        ));
    Paragraph::new(lines).block(block).render(popup, buf);
}

/// `on` / `off` for the GLOBAL booleans.
fn on_off(v: bool) -> String {
    (if v { "on" } else { "off" }).to_string()
}

/// Char-safe prefix truncation with an ellipsis (long prompts, long model
/// lists must never wrap the row).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// The last `max_chars` of a buffer (edit rendering keeps the caret end in
/// view), with a leading ellipsis when the head is cut.
fn tail_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let tail: String = s
        .chars()
        .skip(count - max_chars.saturating_sub(1))
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::deck::WorkspaceModel;
    use crate::deck_ui::{DeckAction, DeckUi, handle_deck_key, ingest_inbound};
    use crate::envelope::Inbound;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn ready_ui() -> DeckUi {
        let mut ui = DeckUi::default();
        ui.splash.skip(); // past the splash for interaction tests
        ui
    }

    fn sample_state() -> EngineConfigState {
        EngineConfigState {
            auto_mode: false,
            effort_auto: false,
            reasoning_auto: false,
            allowed_models: vec!["anthropic/claude-fable-5".into(), "openai/gpt-6".into()],
            providers: vec!["anthropic".into(), "openrouter".into()],
            catalog_models: vec!["zai/glm-5".into()],
            model_efforts: Default::default(),
            agents: vec![EngineAgentState::default(); 4],
        }
    }

    /// A deck on the SETTINGS tab with the panel already focused over a
    /// loaded snapshot — the state most key tests start from.
    fn open_ui() -> (WorkspaceModel, DeckUi) {
        let model = WorkspaceModel::new();
        let mut ui = ready_ui();
        ui.set_tab(DeckTab::Settings);
        ui.engine.focused = true;
        ui.engine.state = Some(sample_state());
        ui.engine.pristine = Some(sample_state());
        (model, ui)
    }

    #[test]
    fn effort_vocabulary_follows_the_selected_model() {
        let mut state = sample_state();
        state.model_efforts.insert(
            "anthropic/claude-fable-5".into(),
            vec![
                "low".into(),
                "medium".into(),
                "high".into(),
                "xhigh".into(),
                "max".into(),
            ],
        );
        state
            .model_efforts
            .insert("openrouter/mistralai/mistral-7b-instruct".into(), vec![]);
        state.model_efforts.insert(
            "gemini/gemini-3-pro".into(),
            vec!["low".into(), "high".into()],
        );

        // Picker-written qualified spec → exact hit.
        state.agents[0].model = Some("anthropic/claude-fable-5".into());
        assert_eq!(effort_values_for(&state, EngineRole::Default).len(), 5);

        // A confirmed no-reasoning model → no levels at all.
        state.agents[0].model = Some("openrouter/mistralai/mistral-7b-instruct".into());
        assert!(effort_values_for(&state, EngineRole::Default).is_empty());

        // Provider pin + bare slug → provider-qualified lookup.
        state.agents[0].provider = Some("gemini".into());
        state.agents[0].model = Some("gemini-3-pro".into());
        assert_eq!(
            effort_values_for(&state, EngineRole::Default),
            vec!["low".to_string(), "high".to_string()]
        );

        // Unknown model (or no model at all) keeps the full vocabulary —
        // unknown never restricts.
        state.agents[0].provider = None;
        state.agents[0].model = Some("something-new".into());
        assert_eq!(effort_values_for(&state, EngineRole::Default).len(), 5);
        state.agents[0].model = None;
        assert_eq!(effort_values_for(&state, EngineRole::Default).len(), 5);
    }

    #[test]
    fn cycling_effort_on_a_non_reasoning_model_clears_and_explains() {
        let (_model, mut ui) = open_ui();
        let mut state = sample_state();
        state
            .model_efforts
            .insert("openrouter/mistralai/mistral-7b-instruct".into(), vec![]);
        state.agents[0].model = Some("openrouter/mistralai/mistral-7b-instruct".into());
        state.agents[0].effort = Some("xhigh".into()); // stale — model can't
        ui.engine.state = Some(state.clone());
        ui.engine.pristine = Some(state);
        ui.engine.tab = EngineTab::Agent(EngineRole::Default);
        // Row index of the Effort field on the agent tab.
        ui.engine.row = AgentField::ALL
            .iter()
            .position(|f| *f == AgentField::Effort)
            .expect("effort row exists");

        let action = handle_engine_key(key(KeyCode::Enter), &mut ui);
        assert_eq!(action, DeckAction::Handled);
        let agent = &ui.engine.state.as_ref().unwrap().agents[0];
        assert_eq!(agent.effort, None, "stale effort was dropped, not cycled");
        assert!(
            ui.engine
                .status
                .as_deref()
                .is_some_and(|s| s.contains("does not support reasoning")),
            "status explains why nothing cycled: {:?}",
            ui.engine.status
        );
    }

    #[test]
    fn e_on_the_settings_tab_focuses_the_panel_and_esc_unfocuses() {
        let model = WorkspaceModel::new();
        let mut ui = ready_ui();
        ui.set_tab(DeckTab::Settings);
        let action = handle_deck_key(ch('e'), &model, &mut ui);
        assert_eq!(
            action,
            DeckAction::Send(WorkspaceInput::EngineConfigRefresh),
            "focusing the panel asks the driver for a fresh snapshot"
        );
        assert!(ui.engine.focused);
        assert_eq!(ui.engine.tab, EngineTab::Global);
        assert!(ui.engine.picker.is_none());

        // Esc hands the keyboard back to the tab.
        handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert!(!ui.engine.focused, "esc hands the keyboard back to the tab");
    }

    #[test]
    fn tabs_cycle_and_rows_clamp() {
        let (model, mut ui) = open_ui();
        assert_eq!(ui.engine.tab, EngineTab::Global);

        // ↓ past the last GLOBAL row clamps; ↑ past the first clamps at 0.
        for _ in 0..10 {
            handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        }
        assert_eq!(ui.engine.row, GLOBAL_ROWS.len() - 1);
        for _ in 0..10 {
            handle_deck_key(key(KeyCode::Up), &model, &mut ui);
        }
        assert_eq!(ui.engine.row, 0);

        // Tab walks GLOBAL → the four agents → wraps back to GLOBAL.
        let mut seen = vec![ui.engine.tab];
        for _ in 0..5 {
            handle_deck_key(key(KeyCode::Tab), &model, &mut ui);
            seen.push(ui.engine.tab);
        }
        assert_eq!(seen.first(), seen.last(), "five presses wrap around");
        assert_eq!(ui.engine.tab, EngineTab::Global);

        // A deep agent-row selection clamps when returning to GLOBAL.
        handle_deck_key(key(KeyCode::Tab), &model, &mut ui); // → default
        for _ in 0..20 {
            handle_deck_key(key(KeyCode::Down), &model, &mut ui);
        }
        assert_eq!(ui.engine.row, AgentField::ALL.len() - 1);
        handle_deck_key(key(KeyCode::BackTab), &model, &mut ui); // → GLOBAL
        assert_eq!(ui.engine.tab, EngineTab::Global);
        assert_eq!(
            ui.engine.row,
            GLOBAL_ROWS.len() - 1,
            "row clamped into the shorter tab"
        );
    }

    #[test]
    fn enter_cycles_reasoning_none_on_off() {
        let (model, mut ui) = open_ui();
        ui.engine.tab = EngineTab::Agent(EngineRole::Default);
        ui.engine.row = 4; // reasoning (AgentField::ALL[4])

        let reasoning = |ui: &DeckUi| ui.engine.state.as_ref().unwrap().agents[0].reasoning;
        assert_eq!(reasoning(&ui), None);
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(reasoning(&ui), Some(true));
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(reasoning(&ui), Some(false));
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(reasoning(&ui), None, "the cycle wraps back to default");
        // Space is the same toggle.
        handle_deck_key(ch(' '), &model, &mut ui);
        assert_eq!(reasoning(&ui), Some(true));
        // `x` clears outright.
        handle_deck_key(ch('x'), &model, &mut ui);
        assert_eq!(reasoning(&ui), None);
    }

    #[test]
    fn inline_temperature_edit_commits_clears_and_rejects() {
        let (model, mut ui) = open_ui();
        ui.engine.tab = EngineTab::Agent(EngineRole::Default);
        ui.engine.row = 5; // temperature (AgentField::ALL[5])

        let temp = |ui: &DeckUi| ui.engine.state.as_ref().unwrap().agents[0].temperature;

        // ⏎ starts the edit seeded empty (the field is unset)…
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(
            ui.engine.edit,
            Some(EngineEdit {
                row: 5,
                buffer: String::new()
            })
        );
        // …"0.7" ⏎ commits Some(0.7).
        for c in "0.7".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(ui.engine.edit, None, "a clean parse ends the edit");
        assert_eq!(temp(&ui), Some(0.7));

        // Re-entering seeds the current value; emptying it clears to None.
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(ui.engine.edit.as_ref().unwrap().buffer, "0.7");
        for _ in 0..3 {
            handle_deck_key(key(KeyCode::Backspace), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(temp(&ui), None, "an empty commit means provider default");

        // Garbage sets a hint and keeps the edit alive — never half-applies.
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        for c in "abc".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert!(ui.engine.edit.is_some(), "still editing after a bad parse");
        assert!(
            ui.engine
                .status
                .as_deref()
                .is_some_and(|s| s.contains("temperature")),
            "the hint names the field: {:?}",
            ui.engine.status
        );
        assert_eq!(temp(&ui), None, "the field is untouched");
        // Esc abandons the bad buffer.
        handle_deck_key(key(KeyCode::Esc), &model, &mut ui);
        assert_eq!(ui.engine.edit, None);
        assert!(ui.engine.focused, "esc closed the edit, not the overlay");
    }

    #[test]
    fn s_saves_the_working_copy_at_user_scope() {
        let (model, mut ui) = open_ui();
        ui.engine.tab = EngineTab::Agent(EngineRole::Default);
        ui.engine.row = 5; // temperature
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        for c in "0.7".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);

        let action = handle_deck_key(ch('s'), &model, &mut ui);
        assert_eq!(action, DeckAction::Handled, "the save rides pending_inputs");
        let mut expected = sample_state();
        expected.agents[0].temperature = Some(0.7);
        assert_eq!(
            ui.pending_inputs,
            vec![WorkspaceInput::EngineConfigSave {
                state: expected.clone(),
                scope: AgentScope::User,
            }],
            "the edited working copy goes out whole, at user scope"
        );
        assert!(ui.engine.busy);

        // `S` targets the project scope with the same working copy.
        ui.pending_inputs.clear();
        handle_deck_key(ch('S'), &model, &mut ui);
        assert_eq!(
            ui.pending_inputs,
            vec![WorkspaceInput::EngineConfigSave {
                state: expected,
                scope: AgentScope::Project,
            }]
        );
    }

    #[test]
    fn picker_filters_by_substring_and_enter_sets_the_model() {
        let (model, mut ui) = open_ui();
        ui.engine.tab = EngineTab::Agent(EngineRole::Default);
        ui.engine.row = 0; // model

        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert!(ui.engine.picker.is_some(), "⏎ on the model row opens it");
        for c in "gpt".chars() {
            handle_deck_key(ch(c), &model, &mut ui);
        }
        let state = ui.engine.state.as_ref().unwrap();
        assert_eq!(
            picker_matches(state, "gpt"),
            vec!["openai/gpt-6".to_string()],
            "substring filter narrows the allowed models"
        );
        handle_deck_key(key(KeyCode::Enter), &model, &mut ui);
        assert_eq!(ui.engine.picker, None, "⏎ closes the picker");
        assert_eq!(
            ui.engine.state.as_ref().unwrap().agents[0].model.as_deref(),
            Some("openai/gpt-6")
        );
        assert!(ui.engine.dirty(), "the pick is a local edit until saved");
    }

    #[test]
    fn picker_falls_back_to_the_catalog_when_nothing_is_allowed() {
        let mut state = sample_state();
        state.allowed_models.clear();
        assert_eq!(
            picker_matches(&state, ""),
            vec!["zai/glm-5".to_string()],
            "an empty allow-list means the catalog vocabulary"
        );
    }

    #[test]
    fn ingest_applies_snapshot_and_status_without_clobbering_edits() {
        let mut model = WorkspaceModel::new();
        let mut ui = ready_ui();
        let snap = sample_state();

        // A first snapshot lands verbatim (working + pristine), with status.
        ingest_inbound(
            &Inbound::EngineConfig {
                state: snap.clone(),
                status: Some("loaded".into()),
            },
            &mut model,
            &mut ui,
        );
        assert_eq!(ui.engine.state.as_ref(), Some(&snap));
        assert_eq!(ui.engine.pristine.as_ref(), Some(&snap));
        assert_eq!(ui.engine.status.as_deref(), Some("loaded"));
        assert!(!ui.engine.busy);
        assert!(!ui.engine.dirty());

        // A refresh over an OPEN, dirty overlay must not eat local edits…
        ui.engine.focused = true;
        ui.engine.state.as_mut().unwrap().auto_mode = true;
        let mut other = sample_state();
        other.effort_auto = true;
        ingest_inbound(
            &Inbound::EngineConfig {
                state: other,
                status: None,
            },
            &mut model,
            &mut ui,
        );
        let state = ui.engine.state.as_ref().unwrap();
        assert!(state.auto_mode, "the local edit survives the refresh");
        assert!(!state.effort_auto, "the conflicting snapshot was held off");
        assert!(ui.engine.dirty());

        // …but the echo of our own save re-baselines pristine: modified clears.
        let echo = ui.engine.state.clone().unwrap();
        ingest_inbound(
            &Inbound::EngineConfig {
                state: echo,
                status: Some("saved to user settings".into()),
            },
            &mut model,
            &mut ui,
        );
        assert!(!ui.engine.dirty(), "the save echo clears the marker");
        assert_eq!(ui.engine.status.as_deref(), Some("saved to user settings"));

        // A CLOSED overlay always adopts the next snapshot (the deliberate
        // discard path for edits abandoned by closing).
        ui.engine.focused = false;
        ui.engine.state.as_mut().unwrap().auto_mode = false; // stale local edit
        let mut newest = sample_state();
        newest.reasoning_auto = true;
        ingest_inbound(
            &Inbound::EngineConfig {
                state: newest.clone(),
                status: None,
            },
            &mut model,
            &mut ui,
        );
        assert_eq!(ui.engine.state.as_ref(), Some(&newest));
    }

    #[test]
    fn render_smoke_covers_rows_and_the_picker() {
        fn buffer_text(buf: &Buffer) -> String {
            let area = buf.area();
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        let (_model, mut ui) = open_ui();
        ui.engine.tab = EngineTab::Agent(EngineRole::Default);
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_panel(&ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("agents"), "title drawn");
        assert!(text.contains("temperature"), "agent rows drawn");
        assert!(text.contains("(provider default)"), "unset renders dimmed");

        // The picker draws over the popup.
        ui.engine.picker = Some(ModelPicker::default());
        let mut buf = Buffer::empty(area);
        render_panel(&ui, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains("filter"), "picker filter line drawn");
        assert!(text.contains("claude-fable-5"), "allowed models listed");
    }
}
